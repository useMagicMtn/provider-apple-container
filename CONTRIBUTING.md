# Contributing

Build with a current stable Rust toolchain and a sibling `magicmtn` checkout. Create an ignored `.cargo/config.toml` with these SDK paths. Adjust them for your checkout layout.

```toml
[patch.crates-io]
magicmtn-sdk = { path = "../magicmtn/sdk/rust/magicmtn-sdk" }
magicmtn-provider-framework = { path = "../magicmtn/sdk/rust/magicmtn-provider-framework" }
```

Run the checks from the provider checkout.

```sh
cargo fmt --check
cargo test --locked
cargo check --locked --lib --no-default-features
```

To verify a runtime change on an Apple silicon Mac, start Apple Container and run the sandbox lifecycle in the [README](README.md#run-a-sandbox). Check that termination removes the workload.

Build the native provider package with Python 3.11 or later and `./package.sh`. The archive contains the executable, its protocol version and checksum manifest, and the Apache 2.0 license.
