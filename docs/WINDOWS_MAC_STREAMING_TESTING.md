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
media protocol=hybrid-v2 host_hybrid_video=true video transport=hybrid-keyframe-stream+quic-datagram ...
first encoded frame sequence=... keyframe=true ...
first decoded frame sequence=... non_zero=... dimensions=...x...
received video datagram index=1 bytes=...
received first complete inter frame after ... datagrams
presented=1 ...
```

Host 同时应记录：

```text
media protocol=hybrid-v2 peer hybrid_video=true
```

若 Host 记录 `hybrid_video=false`，说明连接到的是旧 Viewer；Host 会自动退回
Datagram 关键帧兼容路径。Viewer 等待可靠流超时也会明确记录 legacy fallback。
不要用旧 EXE 的 `video transport=quic-datagram` 日志判断新实现是否生效。

如果只有：

```text
no complete video frame for 2s; requested keyframe bitrate=... fps=... incomplete_datagrams=true
```

且 `incomplete_datagrams=true`，表示普通帧仍在 Datagram 到达或分片重组阶段丢失；Viewer
会降低码率并要求 Host 通过可靠媒体 Stream 发送恢复关键帧。Host 端的 `sent=N` 只表示
发送调用成功；还应观察：

```text
first video frame sequence=... bytes=... keyframe=true transport=reliable-stream
video datagrams sent=... oversized_dropped=... recovery_wait_dropped=... bitrate=... fps=... rtt_ms=...
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
- Initial and recovery keyframes use a dedicated reliable QUIC stream. Ordinary
  frames use QUIC DATAGRAM fragments and may be discarded when incomplete or stale.
  The Host waits for Datagram send-buffer capacity while sending the fragments
  of one frame. Quinn's immediate-send API evicts older queued Datagram payloads
  under pressure, which previously caused a large frame to discard its own
  leading fragments.
- In the legacy DCVD whole-frame compatibility path, a single lost Datagram
  fragment invalidates an ordinary frame. The Host limits those ordinary frames
  to 96 fragments; larger frames are dropped and converted into a reliable
  keyframe recovery instead of flooding a narrow link. Peers that negotiate the
  NAL Datagram mode packetize and reassemble each NAL independently instead.
- The default balanced target preserves the captured resolution and uses true 16-bit
  RGB565-style color quantization before H.264 encoding. Pointer coordinates therefore
  remain in the original desktop coordinate system. When Datagram fragments
  continue arriving without a complete frame, the Viewer sends a rate hint that
  steps down toward 384 Kbps / 6 FPS. The balanced profile starts at
  1 Mbps / 12 FPS and can rise to 4 Mbps / 30 FPS after stable delivery. The Host also reduces the target immediately
  when an inter frame exceeds the legacy DCVD 96-fragment budget. The negotiated
  NAL Datagram path does not apply that whole-frame guard. A static desktop with
  no incoming Datagram traffic is not treated as congestion.
  The Viewer starts at the profile's initial target (1 Mbps / 12 FPS for the
  balanced profile, 64 Kbps / 3 FPS for `--ultra-low`) and only raises the rate after a
  stable run of complete inter frames. On Windows, the first decoded frame
  recreates the placeholder window at native size when the remote dimensions
  plus window decoration fit the local primary display.
  The balanced profile uses RGB565 masks (5/6/5 bits), correcting the former
  3-bit-per-channel implementation that was described as 16-bit but was
  actually only 9-bit color. `--ultra-low` deliberately keeps 2 bits per channel.
  Spatial block averaging is intentionally disabled so text and small controls
  remain readable while the source dimensions are preserved.
  The Windows Host prefers a hardware Media Foundation H.264 MFT at every
  profile rate. Its portable media-type bitrate is supplemented, when the MFT
  exposes `ICodecAPI`, with CBR, mean/max bitrate, real-time, and low-latency
  controls. These controls are best-effort because vendor MFTs expose
  different subsets. A software-only MF MFT remains preferred when it is
  available, because it is generally faster than the OpenH264 fallback in this
  project. OpenH264 is used only when Media Foundation cannot initialize or
  configure an H.264 MFT, or when an active MFT fails at runtime. It must not be
  selected merely because the available MFT is software-only. The fallback
  uses QP 42..51 at the 64 Kbps floor and QP 36..51 for the other low-quality
  rates, together with bitrate-mode and low-complexity settings.
  Every MFT startup now logs `MFT codec controls` before and after media-type
  negotiation. Inspect the `accepted` and `rejected` lists rather than assuming
  that CBR was applied. The requested policy includes CBR, mean/max bitrate, a
  one-second VBV (bytes, not bits), low-latency/real-time operation, frame dropping,
  no B frames, the display-remoting scenario, a maximum QP, and a five-second keyframe
  distance. Force-keyframe capability is reported separately and is used when
  supported instead of rebuilding the encoder.
  To test an even lower-quality profile without changing the captured resolution,
  prepend `--ultra-low` to both commands. This profile starts at 64 Kbps / 3 FPS
  and can rise to 160 Kbps / 5 FPS; omit the flag for the balanced profile.
- In an earlier 96 Kbps test, the software-only Media Foundation MFT produced
  inter frames of roughly 117–269 KB (103–237 Datagram fragments). Those frames
  exceeded the 96-fragment guard, so the Host discarded them and requested a new
  reliable keyframe. Initial frame presentation and keyframe ACK timing are now
  acceptable, but this repeated oversized-inter-frame cycle remains the main
  MFT rate-control issue to verify with the per-control log. The run did not
  show connection loss or decoder errors.
- A later 1440x900 / 96 Kbps run forced the Host onto OpenH264. It reduced the
  initial keyframe to about 91 KB and avoided the Host's oversized-frame guard,
  but encoding became the dominant latency: ordinary observations included
  roughly 240–306 ms and pathological frames took about 1.69–1.91 seconds.
  OpenH264 is therefore a functional compatibility path, not an acceptable
  full-resolution interactive encoder on that machine. Do not trade away MFT
  encoding throughput solely to obtain smaller packets.
- A subsequent software-MFT run showed normal encode averages near 20–60 ms and
  Viewer decode/present near 10–25 ms / 2–4 ms, while complete frames were still
  separated by repeated two-second recoveries. Control input remained responsive.
  This isolated the delay to Datagram self-eviction plus the policy that discarded
  every complete packet after a sequence gap. Datagram submission is now paced,
  and an isolated gap is passed to decoder concealment; recovery is requested only
  after an actual decode failure or sustained lack of complete media.
- The preferred next backend work, if a particular MFT cannot enforce its rate
  controls, is a native hardware path with explicit VBV/QP control: Intel
  oneVPL/Quick Sync, NVIDIA NVENC, or AMD AMF. An optional libx264/FFmpeg path
  may provide strong software rate control, but its CPU cost, binary deployment,
  and licensing must be evaluated before it can replace the system MFT. Until
  one of those backends is implemented, dirty-region updates should carry small
  desktop changes and MFT should carry full-frame changes.
- New peers negotiate `hybrid-v4-h264-nal-regions`: ordinary H.264 access units
  are parsed into NAL units and each NAL is independently fragmented and
  reassembled. This removes the old whole-frame fragment guard from the new
  path and prevents one incomplete old access unit from blocking a later one.
  Decoder output is still picture-oriented; partial-frame display is not yet
  claimed. OpenH264 fallback additionally constrains VCL slice NAL sizes inside
  the encoder based on the negotiated Datagram MTU.
- An experimental x264 Host can be built with `--features x264` and selected by
  placing `--x264` before `--host`. It requires native libx264 headers and an
  import library, discovered through `X264_INCLUDE_DIR` / `X264_LIB_DIR` or
  pkg-config, and is excluded from normal builds. The native bridge explicitly applies and
  logs VBV and `slice-max-size` controls; treat the backend as experimental
  until the Host log and parsed NAL sizes confirm that the installed libx264
  accepted them.
- On Windows, a failed Media Foundation Viewer decoder probe is cached for the
  lifetime of the stream; the Viewer then stays on OpenH264 instead of retrying
  the unavailable MFT for every frame.
- H.264 input is already 4:2:0 on both Windows Media Foundation and OpenH264 paths:
  Media Foundation receives NV12, while OpenH264 converts BGRA/RGB input into its
  internal YUV 4:2:0 representation. The 16-bit color-depth step is an additional
  pre-quantization and does not replace chroma subsampling.
- Reliable keyframe requests are coalesced until the Viewer acknowledges that the
  current keyframe was decoded and presented. This prevents multiple large IDR frames
  from accumulating behind one another on a narrow reliable stream.
- Natural periodic IDRs from the MFT also use the reliable media stream, but they do
  not create an acknowledgement barrier or pause capture. Only startup, explicit
  recovery, and encoder reconfiguration frames pause capture. This matters for the
  tested software MFT, which rejected keyframe-distance control and emitted an IDR
  roughly every four frames.
- The Viewer records the receive timestamp after a QUIC message has arrived, rather
  than before waiting for the next message; this keeps `receive_to_present_avg` from
  including idle time between frames. The Host also drops encoded frames that became
  stale while a reliable keyframe was crossing a narrow link, so recovery resumes with
  the newest available desktop state.
- Capture sequence numbers are not used as transport sequence numbers. The Host
  assigns a contiguous sequence only to packets that the encoder actually emits;
  this prevents OpenH264/MFT frame skipping from being misclassified as network
  loss by the Viewer.
- While a hybrid reliable keyframe is queued or in flight, the Host pauses new
  capture and encoding. It resumes only after the Viewer has decoded, presented,
  and acknowledged that keyframe; ordinary frames therefore cannot accumulate
  behind a recovery anchor.
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
Datagram count, oversized-frame drops, completed/incomplete ordinary frames, keyframe request
rate, and time from a request to the next reliably received keyframe. Test at least one
rate-limited and one lossy condition before considering the recovery path complete.

This test demonstrates cross-platform stage 2 interoperability. It does not replace the remaining
Windows ↔ Windows long-duration benchmark, multi-monitor validation, or a future native macOS Host
capture implementation.
