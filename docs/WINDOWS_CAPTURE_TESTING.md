# Windows screen capture testing

This checklist validates the stage 1 DXGI Desktop Duplication prototype on a Windows 10 or
Windows 11 machine. The primary-display frame export and short live-preview checks have passed on
the environment recorded below; the extended runtime and hardware-matrix scenarios remain open.

## Prerequisites

- A current stable Rust toolchain (MSVC host)
- Visual Studio Build Tools with the Desktop development with C++ workload
- NASM available on `PATH` for the source-built OpenH264 dependency
- A local interactive desktop session; Desktop Duplication is not expected to work from a locked
  or disconnected session

## Build and automated checks

Open PowerShell in the repository root:

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Capture test

Capture 60 changed frames from the primary DXGI output, pass them through OpenH264, decode the
frames, and save the last decoded frame:

```powershell
cargo run -p direct-computing -- --capture-test 60 0 dc-capture-test.bmp
```

Open `dc-capture-test.bmp` in Windows Photos or Paint and verify:

- the expected display was captured;
- the image is not black;
- red and blue channels are not swapped;
- the image is upright;
- the logged frame, byte, and display values are plausible.

For another display, replace output index `0` with `1`, `2`, and so on. The current prototype
rejects rotated portrait outputs explicitly instead of returning an incorrectly oriented image.

## Live preview test

Capture the primary display, encode and decode it locally, and show the decoded frames for 60
seconds:

```powershell
cargo run -p direct-computing -- --preview 0 60
```

Use duration `0` to run until the window is closed or Escape is pressed:

```powershell
cargo run -p direct-computing -- --preview 0 0
```

Verify that:

- the preview is live, upright, and has correct red and blue channels;
- resizing the window preserves the desktop aspect ratio;
- closing the window and pressing Escape both exit cleanly;
- a mostly static desktop remains responsive when DXGI reports capture timeouts;
- the title and console update once per second with FPS, skipped-frame count, raw and encoded
  throughput, and average capture, encode, decode, and display time;
- the process exits after approximately the requested nonzero duration.

OpenH264 may intentionally skip a frame when its real-time rate control cannot encode it in time.
Skipped frames are reported by the `skipped` metric and must not terminate the preview or produce an
empty packet error.

The software H.264 path is not expected to sustain 4K/60 at this stage. Record the displayed
metrics so later bitrate, pacing, and hardware-encoder work has a baseline.

The network Host attempts the vendor-neutral Windows Media Foundation H.264 MFT first. Media
Foundation enumerates hardware encoders before synchronous software MFTs. Both synchronous and
asynchronous MFTs are supported; asynchronous transforms are unlocked and drained through their
Media Foundation output events. If initialization fails, the Host logs the stage and reason before
falling back to OpenH264. The current Media Foundation adapter accepts CPU BGRA frames and
converts them to NV12, so it is a hardware compression path but not yet a D3D11 zero-copy path.
Host logs report the selected backend and its hardware/zero-copy capability.

## Runtime scenarios

Run the capture command in these situations and record the exact command, console output, Windows
version, GPU model, and driver version for failures:

- Intel, AMD, and NVIDIA hardware when available
- one display and multiple displays
- 100%, 125%, and 150% display scaling
- active screen changes and a mostly static screen
- display resolution changes between separate runs
- laptop display only, external display only, and both displays

After the short scenarios pass, run `--preview 0 0` for 30 minutes. Record starting and ending
memory use, average CPU/GPU use, FPS range, skipped-frame count, and whether memory or latency grows
continuously. A clean short run does not replace this long-duration check.

Lock-screen, UAC secure-desktop, display hot-plug, rotation, and automatic recovery after
`DXGI_ERROR_ACCESS_LOST` are not stage 1 supported behaviors yet. They should fail visibly and must
not crash or hang the process.

## Validation record

The following results were recorded on 2026-09-21. They are evidence for the commands that were
actually run, not a substitute for the scenario matrix above.

- Environment: Windows 11 Home 64-bit, build 26200; AMD Radeon(TM) Graphics, driver
  `31.0.21924.61`; one physical output, `\\.\DISPLAY1`, at 1920x1200. The available display
  scaling was not changed for this run.
- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`: passed.
- `cargo run -p direct-computing -- --capture-test 60 0 dc-capture-test.bmp`: passed on the
  primary output; the exported 1920x1200 32-bit BMP was non-black, upright, and had correct color
  channels.
- `cargo run -p direct-computing -- --window-test 10`: passed and exited with code 0 after about
  10 seconds.
- `cargo run -p direct-computing -- --preview 0 130`: passed and exited with code 0 after about
  130 seconds, including the previous skipped-frame failure point. Skipped H.264 frames were
  reported and did not terminate the preview.
- `--capture-test 10 1 ...` was attempted and correctly failed because this machine has no DXGI
  output index `1`; this is not evidence of a second-display failure.
- A 30-minute `--preview 0 1800` run completed from a logged-in, unlocked Windows desktop:
  `elapsed=1800.28s`, `preview complete`, and no application error appeared in the run log. The
  808 interval reports ranged from 0.1 to 1.3 FPS (average 0.75 FPS), with 9 skipped frames in
  total. Average capture, encode, decode, and display times were 5.7 ms, 1450.5 ms, 353.1 ms,
  and 115.3 ms respectively. The log did not include process memory, CPU, GPU, or handle samples,
  so resource-growth and utilization conclusions still require a rerun with external monitoring.
- A supplementary monitored run used a PowerShell performance monitor with a single physical
  output and completed the 60-second smoke test (`elapsed=64.15s`, final 0.4 FPS, 1 skipped frame) and
  the 30-minute soak test (`elapsed=1800.36s`, final 0.6 FPS, 34 skipped frames) without errors.
  The 30-minute final interval reported 6.1 MB/s raw throughput, 14.0 KB/s H.264 throughput,
  4.8 ms capture, 1062.5 ms encode, 325.9 ms decode, and 115.4 ms display time. Across 326
  performance samples, process CPU averaged 5.98% (maximum 6%), working-set memory ranged from
  107.5 to 146.1 MB (145.4 MB at start and 131.7 MB at end), private memory ranged from 111.8
  to 132.9 MB (132.5 MB at start and 132.7 MB at end), and handle count stayed between 295 and
  299. These samples show no continuous memory or handle growth. The system CPU average was
  46.13% with a 91% maximum, which includes unrelated desktop activity; the per-process GPU
  engine sample averaged 0.11% and peaked at 0.52%. The requested one-second sampling interval
  was not achieved: effective intervals ranged from 4.6 to 8.3 seconds (median 5.2 seconds),
  so the monitor is suitable for resource-growth trends rather than one-second profiling.

Window resizing, Escape, and close-window behavior also require an interactive desktop window and
were not claimed as automated passes in this record. Do not mark those checks complete without
observing the actual preview window.
