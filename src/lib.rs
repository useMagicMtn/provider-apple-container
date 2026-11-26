//! `SandboxBackend` implementation backed by Apple Container
//! (<https://github.com/apple/container>), Apple's OCI container runtime
//! for macOS, driven through its `container` CLI.
//!
//! The `template` argument to [`SandboxBackend::create`] is a path to a
//! Containerfile/Dockerfile on the local machine (resolved by the service
//! layer from a template name). The provider builds an image from it
//! (using the Dockerfile's parent directory as the build context) and
//! starts a detached container. `image:<reference>|<UUID>` instead starts a
//! prebuilt image without host mounts. Prebuilt images use the default network.
//! Select another network or disable networking with `MAGICMTN_APPLE_CONTAINER_NETWORK`.
//!
//! Notes on the trait operations:
//! - `exec` maps to `container exec`.
//! - `ssh` has no direct equivalent; it is approximated by an interactive
//!   `container exec` shell attached to the current terminal.
//! - `scp` is implemented by streaming `tar` archives through
//!   `container exec`, so both `local` and `remote` are treated as
//!   directories.

/// Version of this independently released provider library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

use magicmtn_sdk::{
    BoxFuture, ExecResult, ProviderSandbox, SandboxBackend, SandboxState, ScpDirection,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

const CLI: &str = "container";

async fn capture_limited(
    mut input: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<(String, bool)> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        let count = input.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&chunk[..keep]);
        truncated |= keep < count;
    }
    Ok((String::from_utf8_lossy(&output).to_string(), truncated))
}

/// A `SandboxBackend` that runs sandboxes as local containers via the
/// Apple Container `container` CLI.
#[derive(Debug, Default, Clone)]
pub struct AppleContainerProvider;

impl AppleContainerProvider {
    pub fn new() -> Self {
        Self
    }

    /// Ensure the container system service is running.
    async fn ensure_system_running() -> Result<()> {
        match run(&["system", "status"]).await {
            Ok(_) => Ok(()),
            Err(_) => run(&["system", "start"]).await.map(|_| ()),
        }
    }
}

/// Run `container <args>` and return trimmed stdout, erroring on non-zero
/// exit with stderr included.
async fn run(args: &[&str]) -> Result<String> {
    run_cli(CLI, args).await
}

async fn run_cli(cli: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(cli)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to run `{CLI}` — is Apple Container installed?"))?;

    if !output.status.success() {
        bail!(
            "`{CLI} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn listed_state(body: &str, id: &str) -> Result<SandboxState> {
    let records: serde_json::Value = serde_json::from_str(body)?;
    let records = records
        .as_array()
        .context("invalid container list response")?;
    let mut state = None;
    for record in records {
        let record_id = record["id"]
            .as_str()
            .context("missing container identity")?;
        if record_id == id {
            anyhow::ensure!(state.is_none(), "duplicate container identity");
            state = Some(match record["status"]["state"].as_str() {
                Some("running") => SandboxState::Running,
                Some("stopped") => SandboxState::Stopped,
                _ => bail!("container runtime state is unknown or transitioning"),
            });
        }
    }
    Ok(state.unwrap_or(SandboxState::Missing))
}

async fn runtime_state(cli: &str, id: &str) -> Result<SandboxState> {
    // Absence is established by a successful full inventory, never by matching
    // an error string from inspect (which can also fail during an outage).
    run_cli(cli, &["system", "status"]).await?;
    listed_state(
        &run_cli(cli, &["list", "--all", "--format", "json"]).await?,
        id,
    )
}

async fn remove_container(cli: &str, id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty() && !id.starts_with('-'),
        "invalid container identity"
    );
    if runtime_state(cli, id).await? == SandboxState::Missing {
        return Ok(());
    }
    let _ = run_cli(cli, &["stop", id]).await;
    let deleted = run_cli(cli, &["delete", "--force", id]).await;
    // Another remover or a lost response may have already completed deletion.
    if runtime_state(cli, id).await? == SandboxState::Missing {
        return Ok(());
    }
    deleted?;
    bail!("container still exists after removal")
}

/// Single-quote a string for safe inclusion in a `sh -c` command line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a shell pipeline, erroring on non-zero exit.
async fn run_sh(script: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", script])
        .stdout(Stdio::null())
        .status()
        .await
        .context("failed to spawn `sh`")?;
    if !status.success() {
        bail!("command failed ({status}): {script}");
    }
    Ok(())
}

async fn inspect_isolated(
    id: &str,
    image: &str,
    identity: Option<&str>,
    network: &str,
) -> anyhow::Result<Option<String>> {
    let body = run(&["inspect", id]).await?;
    let records: serde_json::Value = serde_json::from_str(&body)?;
    let record = &records[0];
    let c = &record["configuration"];
    anyhow::ensure!(
        record["status"]["state"] == "running"
            && c["mounts"].as_array().is_some_and(Vec::is_empty)
            && matches_network(&c["networks"], network)
            && c["image"]["reference"]
                .as_str()
                .is_some_and(|r| r.ends_with(image)),
        "sandbox state/isolation/image changed"
    );
    let digest = c["image"]["descriptor"]["digest"].as_str();
    anyhow::ensure!(
        identity.is_none() || identity == digest,
        "sandbox image identity changed"
    );
    Ok(digest.map(str::to_owned))
}

fn matches_network(value: &serde_json::Value, network: &str) -> bool {
    value.as_array().is_some_and(|networks| {
        if network == "none" {
            networks.is_empty()
        } else {
            networks.len() == 1 && networks[0]["network"] == network
        }
    })
}

fn validate_network(network: &str) -> Result<()> {
    anyhow::ensure!(
        !network.is_empty()
            && network.len() <= 128
            && network
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            && !network.starts_with('-'),
        "invalid Apple Container network name"
    );
    Ok(())
}

impl SandboxBackend for AppleContainerProvider {
    fn prepare_image<'a>(
        &'a self,
        images: &'a std::collections::BTreeMap<String, magicmtn_sdk::image_registry::Image>,
    ) -> BoxFuture<'a, magicmtn_sdk::image_registry::PreparedImage> {
        Box::pin(async move {
            let platform = magicmtn_sdk::image_registry::local_platform()?;
            let image = images
                .get(platform)
                .context("image does not support this computer's platform")?;
            let reference = image.reference();
            anyhow::ensure!(
                image
                    .digest
                    .strip_prefix("sha256:")
                    .is_some_and(|hash| hash.len() == 64
                        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "invalid image digest"
            );
            Self::ensure_system_running().await?;
            run(&["image", "pull", "--platform", platform, &reference]).await?;
            run(&["run","--rm","--network","none","--entrypoint","/bin/sh",&reference,"-c",
                "test -x /usr/local/bin/magicmtn-sandbox-agent && test -w /workspace && python3 -c 'import os;assert os.getuid()==10001 and os.getgid()==10001' && pi --version >/dev/null"]).await.context("image must include the MagicMtn sandbox runtime, Pi, and a writable /workspace for sandbox user 10001")?;
            Ok(magicmtn_sdk::image_registry::PreparedImage {
                image: image.clone(),
                platform: platform.into(),
                artifact: reference,
                configuration: Default::default(),
            })
        })
    }
    fn status<'a>(&'a self, sandbox: &'a ProviderSandbox) -> BoxFuture<'a, SandboxState> {
        Box::pin(async move { runtime_state(CLI, &sandbox.provider_id).await })
    }
    fn create<'a>(&'a self, template: &'a str) -> BoxFuture<'a, ProviderSandbox> {
        Box::pin(async move {
            if let Some(spec) = template.strip_prefix("image:") {
                let (image, session) = spec
                    .rsplit_once('|')
                    .context("image template requires session identity")?;
                let session = Uuid::parse_str(session).context("invalid session identity")?;
                anyhow::ensure!(
                    !image.is_empty()
                        && !image.starts_with('-')
                        && !image.contains(char::is_whitespace),
                    "invalid image"
                );
                Self::ensure_system_running().await?;
                let name = format!("magicmtn-session-{session}");
                let network = std::env::var("MAGICMTN_APPLE_CONTAINER_NETWORK")
                    .unwrap_or_else(|_| "default".into());
                validate_network(&network)?;
                let mut args = vec![
                    "run",
                    "--detach",
                    "--name",
                    &name,
                    "--network",
                    &network,
                    "--cpus",
                    "1",
                    "--memory",
                    "512M",
                    "--ulimit",
                    "nproc=64:64",
                    image,
                    "/bin/sh",
                    "-c",
                    "while true; do sleep 3600; done",
                ];
                if network == "none" {
                    args.insert(6, "--no-dns");
                }
                run(&args).await?;
                let identity = inspect_isolated(&name, image, None, &network).await?;
                return Ok(ProviderSandbox {
                    provider_id: name,
                    status: Some("running".into()),
                    create_time_ms: Some(now_ms()),
                    metadata: Some(
                        json!({"provider":"apple-container","image":image,"image_identity":identity,"isolated":true,"network":network}),
                    ),
                });
            }
            let dockerfile = Path::new(template);
            if !dockerfile.is_file() {
                bail!("Dockerfile not found: {template}");
            }
            let context = dockerfile
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let dockerfile = dockerfile
                .canonicalize()
                .with_context(|| format!("failed to resolve Dockerfile path: {template}"))?;

            Self::ensure_system_running().await?;

            let short_id = Uuid::new_v4().simple().to_string();
            let short_id = &short_id[..12];
            let image = format!("magicmtn-{short_id}");
            let name = format!("magicmtn-{short_id}");

            run(&[
                "build",
                "--tag",
                &image,
                "--file",
                &dockerfile.to_string_lossy(),
                &context.to_string_lossy(),
            ])
            .await
            .context("image build failed")?;

            run(&["run", "--detach", "--name", &name, &image])
                .await
                .context("container run failed")?;

            Ok(ProviderSandbox {
                provider_id: name.clone(),
                status: Some("running".to_string()),
                create_time_ms: Some(now_ms()),
                metadata: Some(json!({
                    "provider": "apple-container",
                    "image": image,
                })),
            })
        })
    }

    fn ssh<'a>(&'a self, sandbox: &'a ProviderSandbox) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            anyhow::ensure!(
                !sandbox.provider_id.is_empty() && !sandbox.provider_id.starts_with('-'),
                "invalid container identity"
            );
            if let Some(metadata) = &sandbox.metadata
                && (metadata["isolated"] == true
                    || sandbox.provider_id.starts_with("magicmtn-session-"))
            {
                inspect_isolated(
                    &sandbox.provider_id,
                    metadata["image"]
                        .as_str()
                        .context("sandbox image missing")?,
                    metadata["image_identity"].as_str(),
                    metadata["network"].as_str().unwrap_or("none"),
                )
                .await?;
            }
            // Apple Container has no SSH daemon; approximate with an
            // interactive shell via `container exec`.
            // The provider's standard I/O carries the plugin protocol, so
            // attach the interactive session to the controlling terminal.
            let tty = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("interactive sandbox session requires a terminal")?;
            let tty_out = tty.try_clone()?;
            let tty_err = tty.try_clone()?;
            let status = Command::new(CLI)
                .args([
                    "exec",
                    "--interactive",
                    "--tty",
                    &sandbox.provider_id,
                    "/bin/sh",
                    "-l",
                ])
                .stdin(Stdio::from(tty))
                .stdout(Stdio::from(tty_out))
                .stderr(Stdio::from(tty_err))
                .status()
                .await
                .context("failed to spawn `container exec`")?;
            if !status.success() {
                bail!("interactive session exited with {status}");
            }
            Ok(())
        })
    }

    fn terminate<'a>(&'a self, sandbox: &'a ProviderSandbox) -> BoxFuture<'a, ()> {
        Box::pin(async move { remove_container(CLI, &sandbox.provider_id).await })
    }

    fn scp<'a>(
        &'a self,
        sandbox: &'a ProviderSandbox,
        local: &'a Path,
        remote: &'a Path,
        direction: ScpDirection,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let id = shell_quote(&sandbox.provider_id);
            let local = shell_quote(&local.to_string_lossy());
            let remote = shell_quote(&remote.to_string_lossy());
            let script = match direction {
                ScpDirection::ToSandbox => {
                    format!("tar -cf - -C {local} . | {CLI} exec -i {id} tar -xf - -C {remote}")
                }
                ScpDirection::FromSandbox => {
                    format!("{CLI} exec -i {id} tar -cf - -C {remote} . | tar -xf - -C {local}")
                }
            };
            run_sh(&script).await
        })
    }

    fn exec<'a>(
        &'a self,
        sandbox: &'a ProviderSandbox,
        command: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, String> {
        Box::pin(async move {
            if command.is_empty() {
                return Err(anyhow!("command must not be empty"));
            }
            let mut full = vec!["exec", sandbox.provider_id.as_str(), command];
            full.extend(args.iter().map(String::as_str));
            run(&full).await
        })
    }

    fn exec_bounded<'a>(
        &'a self,
        sandbox: &'a ProviderSandbox,
        command: &'a str,
        args: &'a [String],
        timeout_ms: u64,
        max_output_bytes: usize,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            anyhow::ensure!(
                (1..=30_000).contains(&timeout_ms) && (1..=16_384).contains(&max_output_bytes),
                "invalid exec limits"
            );
            anyhow::ensure!(
                !command.is_empty() && !command.contains('\0') && !command.starts_with('-'),
                "invalid program"
            );
            if let Some(metadata) = &sandbox.metadata
                && (metadata["isolated"] == true
                    || sandbox.provider_id.starts_with("magicmtn-session-"))
            {
                let image = metadata["image"]
                    .as_str()
                    .context("sandbox image missing")?;
                inspect_isolated(
                    &sandbox.provider_id,
                    image,
                    metadata["image_identity"].as_str(),
                    metadata["network"].as_str().unwrap_or("none"),
                )
                .await?;
            }
            let start = Instant::now();
            let mut child = Command::new(CLI)
                .arg("exec")
                .arg(&sandbox.provider_id)
                .arg(command)
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let stdout = child.stdout.take().context("stdout unavailable")?;
            let stderr = child.stderr.take().context("stderr unavailable")?;
            let output = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
                let (out, err, status) = tokio::join!(
                    capture_limited(stdout, max_output_bytes),
                    capture_limited(stderr, max_output_bytes),
                    child.wait()
                );
                Ok::<_, anyhow::Error>((out?, err?, status?))
            })
            .await;
            match output {
                Ok(result) => {
                    let ((stdout, a), (stderr, b), status) = result?;
                    Ok(ExecResult {
                        stdout,
                        stderr,
                        exit_code: status.code(),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        truncated: a || b,
                    })
                }
                Err(_) => {
                    let _ = child.kill().await;
                    Ok(ExecResult {
                        stdout: String::new(),
                        stderr: "command timed out".into(),
                        exit_code: None,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        timed_out: true,
                        truncated: false,
                    })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_policy_is_explicit_and_pinned() {
        assert!(matches_network(&json!([]), "none"));
        assert!(matches_network(&json!([{"network":"default"}]), "default"));
        assert!(!matches_network(&json!([{"network":"default"}]), "none"));
        assert!(!matches_network(&json!([]), "default"));
        assert!(!matches_network(&json!([{"network":"other"}]), "default"));
        for invalid in ["", "--host", "default,mac=foo", "default\n"] {
            assert!(validate_network(invalid).is_err());
        }
    }
    #[test]
    fn inventory_observations_require_a_valid_full_response() {
        assert_eq!(listed_state("[]", "test").unwrap(), SandboxState::Missing);
        assert_eq!(
            listed_state(r#"[{"id":"test","status":{"state":"running"}}]"#, "test").unwrap(),
            SandboxState::Running
        );
        assert_eq!(
            listed_state(r#"[{"id":"test","status":{"state":"stopped"}}]"#, "test").unwrap(),
            SandboxState::Stopped
        );
        for body in [
            "{}",
            "garbage",
            "[{}]",
            r#"[{"id":"test","status":{"state":"starting"}}]"#,
            r#"[{"id":"test","status":{"state":"running"}},{"id":"test","status":{"state":"stopped"}}]"#,
        ] {
            assert!(listed_state(body, "test").is_err(), "{body}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removal_recovers_a_lost_response_and_outages_are_not_absence() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir =
            Directory(std::env::temp_dir().join(format!("apple-lifecycle-{}", Uuid::new_v4())));
        std::fs::create_dir(&dir.0)?;
        let cli = dir.0.join("container");
        std::fs::write(
            &cli,
            r#"#!/bin/sh
base=$(dirname "$0")
case "$1" in
  system) exit 0 ;;
  list) cat "$base/inventory" ;;
  stop) exit 0 ;;
  delete) echo x >> "$base/deletes"; echo '[]' > "$base/inventory"; exit 1 ;;
esac
"#,
        )?;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))?;
        std::fs::write(
            dir.0.join("inventory"),
            r#"[{"id":"test","status":{"state":"running"}}]"#,
        )?;
        let cli_str = cli.to_str().unwrap();
        remove_container(cli_str, "test").await?;
        remove_container(cli_str, "test").await?;
        assert_eq!(std::fs::read_to_string(dir.0.join("deletes"))?, "x\n");
        std::fs::write(&cli, "#!/bin/sh\nexit 1\n")?;
        assert!(runtime_state(cli_str, "test").await.is_err());
        assert!(remove_container(cli_str, "test").await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn bounded_capture_drains_large_output() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = async {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&vec![b'x'; 16384]).await.unwrap();
            drop(writer);
        };
        let (_, result) = tokio::join!(write, capture_limited(reader, 64));
        let (output, truncated) = result.unwrap();
        assert_eq!(output, "x".repeat(64));
        assert!(truncated);
    }

    #[tokio::test]
    async fn preparation_rejects_missing_host_platform_without_runtime_calls() {
        let result = AppleContainerProvider::new()
            .prepare_image(&std::collections::BTreeMap::new())
            .await;
        assert!(result.unwrap_err().to_string().contains("platform"));
    }
}
