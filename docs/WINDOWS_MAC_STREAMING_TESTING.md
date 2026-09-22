# Windows Host → macOS Viewer testing

This checklist validates the cross-platform stage 2 path: a Windows Host captures its real
desktop, while a macOS Viewer receives, decodes, and displays the H.264 stream. The macOS side is
currently a Viewer only; macOS Host capture is not implemented yet.

## Prerequisites

- Windows 10/11 with an interactive, unlocked desktop session and the MSVC Rust toolchain.
- macOS with a current stable Rust toolchain and permission to open the preview window.
- Both machines reachable over LAN or VPN. QUIC uses UDP port `22100`; allow it through the
  Windows firewall for the selected network profile.

## Windows Host deployment

For the current release build, the Host does not require the Rust workspace, `target` directory,
Cargo files, or a separate OpenH264 DLL. OpenH264 is compiled into the executable, while Media
Foundation, Desktop Duplication, Winsock and the Windows graphics/input libraries are provided by
Windows. A minimal deployment can therefore contain:

```text
direct-computing.exe
```

The `.pdb` file is optional and is only useful for crash debugging. If Windows reports a missing
runtime component on a clean machine, install the matching Microsoft Visual C++ x64 runtime;
do not copy development build artifacts as a substitute.

The Host still requires an interactive, unlocked Windows desktop session and permission to use
Desktop Duplication. Configure an inbound UDP firewall rule for the listening port (default
`22100`). Start it with the password as an argument, for example:

```powershell
.\direct-computing.exe --host 0.0.0.0:22100 '<password>'
```

The current implementation generates a self-signed certificate at process startup and does not
persist it to a certificate file. Consequently, the printed certificate fingerprint can change
after every Host restart; certificate pinning should be re-confirmed after a restart until
persistent certificate storage is implemented. No separate password or configuration file is
currently read by the executable.

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

The Host also reports the first captured frame before encoding:

```text
first captured frame sequence=0 sample_sum=... non_zero=... dimensions=...x...
```

If the Viewer reports `first decoded ... non_zero=0`, compare this Host value. A zero or near-zero
Host value points to the Desktop Duplication/session/capture path; a non-zero Host value with a
zero Viewer value points to the encoder bitstream or decoder path.

If the Host reports `openh264-h264 hardware=false`, inspect the preceding Media Foundation
warning. A configuration-stage HRESULT identifies why the MFT was rejected.

If VideoToolbox cannot accept or decode the stream, the Viewer logs the failure and switches to
OpenH264. Treat fallback as a functional pass but not as a hardware-acceleration performance pass.

### 首帧和网络诊断日志

Viewer 启动后会先创建占位窗口。正常收到并重组首帧时，应依次看到类似日志：

```text
received video datagram index=1 bytes=...
received complete video frame after ... datagrams
first encoded frame sequence=... keyframe=true ...
first decoded frame sequence=... non_zero=... dimensions=...x...
presented=1 ...
```

如果只有：

```text
no complete video frame received for 1s; requested keyframe
```

则问题仍在 Datagram 到达或分片重组阶段，不应先从窗口像素转换排查。Host 端的
`sent=N` 只表示发送调用成功；还应记录：

```text
first video frame sequence=... fragments=... datagram_size=...
```

用来与 Viewer 的 `received video datagram` 数量对照。Host 在静止桌面下可能记录：

```text
desktop unchanged; synthesized recovery frame sequence=... timestamp_ms=...
```

这表示 Desktop Duplication 没有新变化，Host 正在用最近一帧生成新的恢复帧。

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
- Video data uses QUIC DATAGRAM fragments. Missing or stale fragments may be
  discarded; when a sequence gap or decode error is observed, the Viewer sends
  a keyframe request and the Host resumes from a fresh IDR when supported.
- A single lost fragment invalidates the whole encoded frame. For low-bandwidth or
  lossy links, record the number of received fragments and completed frames; the
  current DATAGRAM path repeats keyframe fragments once as a lightweight loss
  mitigation, but it is not yet a reliable keyframe path. Reliable keyframe
  delivery or FEC/fragment retransmission remains a planned improvement.
- Do not treat `presented=0` as a rendering failure until the Viewer has logged
  `received complete video frame`. If a complete frame is decoded but the window
  remains black, compare `non_zero` decoded bytes and then inspect pixel conversion
  and the minifb presentation backend.
- The stream remains active for at least 15 minutes without QUIC, decode, or application errors.
- Moving the mouse and pressing/releasing keys in the macOS Viewer produces the expected input on
  Windows. Verify only on a disposable test desktop; remote input is intentionally enabled by the
  `control_input` permission.
- A changed certificate fingerprint is rejected when the pinned value is supplied.
- Stop the Host during streaming and verify that the Viewer reports the video-stream closure and
  exits instead of waiting indefinitely; a Host-side encoder failure should close the QUIC session.
- Record both machines' OS versions, Rust versions, network type, resolution, FPS, skipped frames,
  decoder backend, decode/present/receive-to-present timings, and any firewall or reconnect behavior.

For low-bandwidth tests, also record the configured bitrate, frame rate, maximum Datagram size,
fragment count per keyframe, completed/incomplete frame counts, keyframe request rate, and time
from a request to the next successfully presented frame. Test at least one rate-limited and one
lossy condition before considering the recovery path complete.

This test demonstrates cross-platform stage 2 interoperability. It does not replace the remaining
Windows ↔ Windows long-duration benchmark, multi-monitor validation, or a future native macOS Host
capture implementation.
