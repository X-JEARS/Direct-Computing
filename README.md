# Direct Computing

Direct Computing is an early-stage, open-source remote desktop, file transfer, and remote
terminal project for direct connections over LAN, IPv6, or VPN networks.

The project is currently in **stage 3: file transfer foundations**. It intentionally does not include
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

Start a local host and connect a viewer with an address and password:

```sh
cargo run -p direct-computing -- --host 0.0.0.0:22100 '<password>'
cargo run -p direct-computing -- --connect 192.168.1.20:22100 '<password>'
```

The stage 2 path uses QUIC/TLS 1.3, protocol capability negotiation, Argon2id-derived challenge
authentication, and H.264 desktop packets on independent streams. The current host uses the real
Windows capture adapter on Windows and a synthetic source elsewhere; mouse/keyboard injection and
certificate TOFU are still tracked as stage 2 follow-up work.

Stage 3 now includes bounded file manifests, fixed-size chunks, SHA-256 checksums, resume-offset
validation, safe destination paths, `FileOffer`/`FileChunk`/`FileAck` protocol messages, and a
dedicated QUIC file-stream sender/receiver with temporary-file atomic delivery. The Host/Viewer
file-transfer CLI and persistent resume state are the next integration tasks.

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
window title and console report FPS, skipped frames, throughput, and capture/encode/decode/display
timings. OpenH264 rate-control skips are reported and do not terminate the preview.

See [`docs/WINDOWS_CAPTURE_TESTING.md`](docs/WINDOWS_CAPTURE_TESTING.md) for prerequisites and the
runtime test matrix.

See [`docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md`](docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md)
for the complete roadmap.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE).
