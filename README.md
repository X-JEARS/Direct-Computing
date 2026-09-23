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
# Confirm the first connection's printed fingerprint, then pin it on later connections:
cargo run -p direct-computing -- --connect 192.168.1.20:22100 '<password>' '<cert-sha256>'
```

The stage 2 path uses QUIC/TLS 1.3, protocol capability negotiation, Argon2id-derived challenge
authentication, and H.264 desktop packets on independent streams. The current host uses the real
Windows capture adapter on Windows and a synthetic source elsewhere. Viewer mouse/keyboard events
travel over the authenticated control stream and are injected with Windows `SendInput`. First
connections expose a SHA-256 certificate fingerprint and later connections can pin it with the
optional argument; `dc-transport` also provides a confirmed, persistent TOFU pin store.

Remote input uses stable physical-key identifiers and Windows Set-1 scan-code injection, including
distinct left/right modifiers, navigation keys, function keys, and numeric-keypad keys. Vertical
and horizontal wheel motion is transported at high resolution, and held inputs are released if the
control session closes unexpectedly.

On Windows, negotiated desktop optimization reads DXGI dirty/move rectangles, sends small changes
as bounded PackBits-compressed BGRA region updates, and transports the DXGI pointer separately for
Viewer-side composition. Cursor-only frames bypass H.264, and an unchanged desktop is reduced to a
five-second full-frame refresh interval. Larger or incompressible changes automatically fall back
to the normal H.264 path; sequence gaps request a reliable recovery keyframe.

Windows Host encoder preference is hardware Media Foundation H.264, then software Media
Foundation H.264. OpenH264 is retained only as a compatibility fallback when Media Foundation
cannot supply a usable encoder; it is not the preferred full-resolution interactive path because
measured software encode latency can reach hundreds of milliseconds or more. MFT startup logs show
which CBR/VBV/QP and low-latency controls the selected transform actually accepted.

Peers from this revision negotiate H.264 NAL Datagram transport. The Host parses each encoded
access unit into SPS/PPS/SEI/slice NAL units, packetizes each NAL independently, and the Viewer
reassembles completed NAL units without requiring one monolithic frame-fragment chain. A missing
NAL expires only its access unit and does not block a later frame. The current decoder adapters
still present complete pictures, so this is packet-level slice pipelining rather than partial-frame
display. The unit envelope carries an explicit codec identifier and picture/unit numbering so the
same framing can be extended with H.265 NAL and AV1 OBU packetizers without redesigning QUIC lanes.

An experimental libx264 Host backend is available behind the `x264` Cargo feature. It uses the
`veryfast` preset with zero-latency/fast-decode tuning, no delayed B-frame/lookahead queue, Annex-B
output, and a five-second GOP. It is opt-in because libx264 is a native GPL/commercial component:

```powershell
# Install libx264 headers/import library for the active MSVC target first. Set
# X264_INCLUDE_DIR and X264_LIB_DIR, or make x264 discoverable through pkg-config, then:
cargo build -p direct-computing --features x264
target\debug\direct-computing.exe --x264 --host 0.0.0.0:22100 '<password>'
```

The optional platform bridge configures `vbv-maxrate`, a half-second `vbv-bufsize`,
`slice-max-size`, `bframes=0`, `rc-lookahead=0`, `sync-lookahead=0`, repeated headers, and Annex-B
output through libx264's official API. A normal build does not require libx264. The backend remains
experimental until those settings are verified on the Windows test machines. The NAL-oriented
transport is codec-adapter infrastructure and does not pretend that an arbitrary single large
x264/MFT slice has become independently decodable merely because it was fragmented.

The default desktop profile is balanced for interaction: it starts at 1 Mbps / 12 FPS, uses true
RGB565 pre-quantization, can fall to 384 Kbps / 6 FPS under pressure, and can rise to 4 Mbps /
30 FPS after stable delivery. `--ultra-low` remains available for deliberately constrained links.
Video Datagram fragments wait for transport buffer space so a frame cannot evict its own earlier
fragments; the bounded one-frame producer queue still drops stale newer work instead of building
display latency.

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

To inspect incremental desktop updates, add `--show-dirty-regions` before `--connect` on the
Viewer. Each received dirty region is outlined in bright green without modifying the retained
desktop framebuffer or the pixels sent by the Host:

```powershell
.\direct-computing.exe --show-dirty-regions --connect <host>:22100 '<password>' '<cert-sha256>'
```

The overlay is disabled by default and is intended only for testing dirty-region detection and
transport.

Pass duration `0` to keep the preview open until the window is closed or Escape is pressed. The
window title and console report FPS, skipped frames, throughput, and capture/encode/decode/display
timings. OpenH264 rate-control skips are reported and do not terminate the preview.

See [`docs/WINDOWS_CAPTURE_TESTING.md`](docs/WINDOWS_CAPTURE_TESTING.md) for prerequisites and the
runtime test matrix.

The stateful dirty-region, selective retransmission, framebuffer versioning, and recovery design is
documented in
[`docs/DESKTOP_INCREMENTAL_TRANSPORT_PLAN.md`](docs/DESKTOP_INCREMENTAL_TRANSPORT_PLAN.md).

For the cross-platform Windows Host → macOS Viewer test procedure, see
[`docs/WINDOWS_MAC_STREAMING_TESTING.md`](docs/WINDOWS_MAC_STREAMING_TESTING.md).

See [`docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md`](docs/DIRECT_COMPUTING_DEVELOPMENT_PLAN.md)
for the complete roadmap.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE).
