use anyhow::Result;
use magicmtn_provider_apple_container::AppleContainerProvider;
use magicmtn_sdk::plugin::Capabilities;
use magicmtn_sdk::plugin::SandboxOperation::*;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    magicmtn_provider_framework::run_sandbox(
        "apple-container",
        env!("CARGO_PKG_VERSION"),
        Capabilities::sandbox(vec![
            PrepareImage,
            Create,
            Status,
            Ssh,
            Terminate,
            Exec,
            ExecBounded,
        ]),
        AppleContainerProvider::new(),
    )
    .await
}
