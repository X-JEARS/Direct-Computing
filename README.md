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

Open a cross-platform synthetic decoded-frame window for 10 seconds with:

```sh
cargo run -p direct-computing -- --window-test 10
```

On Windows, capture 60 desktop frames through DXGI and the H.264 loopback, then save the last
decoded frame as a BMP image:

```powershell
cargo run -p direct-computing -- --capture-test 60 0 dc-capture-test.bmp
```

Run a 60-second live preview of the primary display with:

```powershell
cargo run -p direct-computing -- --preview 0 60
```

Pass duration `0` to keep the preview open until the window is closed or Escape is pressed. The
window title and console report FPS, throughput, and capture/encode/decode/display timings.

See [`docs/WINDOWS_CAPTURE_TESTING.md`](docs/WINDOWS_CAPTURE_TESTING.md) for prerequisites and the
runtime test matrix.

See [`docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md`](docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md)
for the complete roadmap.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE).
