# Direct Computing

Direct Computing is an early-stage, open-source remote desktop, file transfer, and remote
terminal project for direct connections over LAN, IPv6, or VPN networks.

The project is currently in **stage 1: local media loopback**. It intentionally does not include
NAT traversal, a central account service, or a relay service.

## Workspace

- `apps/direct-computing`: unified host/viewer application entry point
- `apps/dc-cli`: command-line client entry point
- `crates/dc-common`: shared errors and logging
- `crates/dc-protocol`: protocol version and capability types
- `crates/*`: bounded components described by the development plan
- `docs`: architecture and development documents

## Development

Install the stable Rust toolchain, then run:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run the bootstrap binaries with:

```sh
cargo run -p direct-computing
cargo run -p dc-cli
```

Run the synthetic H.264 encode/decode loopback prototype with:

```sh
cargo run -p direct-computing -- --loopback 30
```

See [`docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md`](docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md)
for the complete roadmap.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE).
