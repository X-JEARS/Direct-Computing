# Windows Host → macOS Viewer testing

This checklist validates the cross-platform stage 2 path: a Windows Host captures its real
desktop, while a macOS Viewer receives, decodes, and displays the H.264 stream. The macOS side is
currently a Viewer only; macOS Host capture is not implemented yet.

## Prerequisites

- Windows 10/11 with an interactive, unlocked desktop session and the MSVC Rust toolchain.
- macOS with a current stable Rust toolchain and permission to open the preview window.
- Both machines reachable over LAN or VPN. QUIC uses UDP port `22100`; allow it through the
  Windows firewall for the selected network profile.

## Test procedure

On Windows, start the Host:

```powershell
cargo run --release -p direct-computing -- --host 0.0.0.0:22100 '<password>'
```

Record the printed certificate SHA-256 fingerprint. On macOS, connect using the Windows address:

```sh
cargo run --release -p direct-computing -- --connect <windows-ip>:22100 '<password>'
```

For a Windows Viewer, use the same `--connect` command. The Viewer lazily
initializes a Windows Media Foundation H.264 decoder after the first packet,
preferring a hardware decoder MFT and falling back to OpenH264 when no usable
MFT is available. The log identifies the selected backend and reports whether
`hardware=true`.

At startup, confirm that the Viewer reports:

```text
decoder backend=apple-videotoolbox-h264 hardware=true zero_copy=false low_latency=true
present backend=metal
```

On the Windows Host, the corresponding successful hardware path should report:

```text
encoder backend=windows-media-foundation-h264 hardware=true zero_copy=false low_latency=true
```

If the Host reports `openh264-h264 hardware=false`, inspect the preceding Media Foundation
warning. A configuration-stage HRESULT identifies why the MFT was rejected.

If VideoToolbox cannot accept or decode the stream, the Viewer logs the failure and switches to
OpenH264. Treat fallback as a functional pass but not as a hardware-acceleration performance pass.

The first connection reports the server fingerprint. After confirming it out of band, repeat with
the optional fingerprint argument to exercise certificate pinning:

```sh
cargo run --release -p direct-computing -- --connect <windows-ip>:22100 '<password>' '<cert-sha256>'
```

## Acceptance checks

- Authentication succeeds with the correct password and fails with an incorrect password.
- The macOS window shows the Windows desktop with correct orientation and colors.
- On macOS, the Viewer requests VideoToolbox BGRA output and reports the Metal presentation
  backend; `zero_copy=false` is expected because the current cross-platform frame model still
  copies the CVPixelBuffer before the Metal upload.
- On Windows, a successful Media Foundation Viewer path reports
  `decoder backend=windows-media-foundation-h264-decoder`; NV12 output is copied
  into the shared BGRA frame model, so `zero_copy=false` is expected.
- Video packets remain ordered under decode backpressure. Replacing pending
  packets with the newest packet would discard H.264 reference frames and make
  both hardware and software decoders fail.
- The stream remains active for at least 15 minutes without QUIC, decode, or application errors.
- Moving the mouse and pressing/releasing keys in the macOS Viewer produces the expected input on
  Windows. Verify only on a disposable test desktop; remote input is intentionally enabled by the
  `control_input` permission.
- A changed certificate fingerprint is rejected when the pinned value is supplied.
- Stop the Host during streaming and verify that the Viewer reports the video-stream closure and
  exits instead of waiting indefinitely; a Host-side encoder failure should close the QUIC session.
- Record both machines' OS versions, Rust versions, network type, resolution, FPS, skipped frames,
  decoder backend, decode/present/receive-to-present timings, and any firewall or reconnect behavior.

This test demonstrates cross-platform stage 2 interoperability. It does not replace the remaining
Windows ↔ Windows long-duration benchmark, multi-monitor validation, or a future native macOS Host
capture implementation.
