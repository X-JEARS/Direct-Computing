# Windows screen capture testing

This checklist validates the stage 1 DXGI Desktop Duplication prototype on a Windows 10 or
Windows 11 machine.

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

## Runtime scenarios

Run the capture command in these situations and record the exact command, console output, Windows
version, GPU model, and driver version for failures:

- Intel, AMD, and NVIDIA hardware when available
- one display and multiple displays
- 100%, 125%, and 150% display scaling
- active screen changes and a mostly static screen
- display resolution changes between separate runs
- laptop display only, external display only, and both displays

Lock-screen, UAC secure-desktop, display hot-plug, rotation, and automatic recovery after
`DXGI_ERROR_ACCESS_LOST` are not stage 1 supported behaviors yet. They should fail visibly and must
not crash or hang the process.
