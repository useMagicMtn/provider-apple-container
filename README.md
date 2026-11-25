<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/logo-dark.svg">
    <img src=".github/assets/logo-light.svg" alt="MagicMtn logo" width="180" height="180">
  </picture>
</p>

# Apple Container provider for MagicMtn

Run MagicMtn sandboxes in lightweight Linux virtual machines on your Mac.

## Install

Use an Apple silicon Mac with macOS 26 or later. Install Apple Container from its signed release package or with `brew install container`. [Apple Container setup](https://github.com/apple/container#get-started) · [Release packages](https://github.com/apple/container/releases)

Start the system service and verify that it is running.

```sh
container system start
container system status
```

Install the provider from the MagicMtn Registry.

```sh
magic provider install apple-container
```

## Run a sandbox

Run `magic server start` in one terminal. In a second terminal, prepare Canonical and create a sandbox. Substitute its returned ID in the remaining commands.

```sh
magic --local images prepare canonical --provider apple-container
magic --local computer new
magic --local computer exec SANDBOX_ID -- /bin/echo hello
magic --local computer terminate SANDBOX_ID --yes
```

Open a local sandbox shell with `magic --local computer ssh SANDBOX_ID`. Type `exit` to return to your Mac. The sandbox keeps running.

## Network access

Prebuilt `image:` sandboxes default to network `none` with DNS disabled. To enable outbound access, set this variable in the provider connection environment.

```text
MAGICMTN_APPLE_CONTAINER_NETWORK=default
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for source builds, tests, and packaging.

## License

[Apache License 2.0](LICENSE).
