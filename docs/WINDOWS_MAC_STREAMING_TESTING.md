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
cargo run -p direct-computing -- --host 0.0.0.0:22100 '<password>'
```

Record the printed certificate SHA-256 fingerprint. On macOS, connect using the Windows address:

```sh
cargo run -p direct-computing -- --connect <windows-ip>:22100 '<password>'
```

The first connection reports the server fingerprint. After confirming it out of band, repeat with
the optional fingerprint argument to exercise certificate pinning:

```sh
cargo run -p direct-computing -- --connect <windows-ip>:22100 '<password>' '<cert-sha256>'
```

## Acceptance checks

- Authentication succeeds with the correct password and fails with an incorrect password.
- The macOS window shows the Windows desktop with correct orientation and colors.
- The stream remains active for at least 15 minutes without QUIC, decode, or application errors.
- Moving the mouse and pressing/releasing keys in the macOS Viewer produces the expected input on
  Windows. Verify only on a disposable test desktop; remote input is intentionally enabled by the
  `control_input` permission.
- A changed certificate fingerprint is rejected when the pinned value is supplied.
- Record both machines' OS versions, Rust versions, network type, resolution, FPS, skipped frames,
  and any firewall or reconnect behavior.

This test demonstrates cross-platform stage 2 interoperability. It does not replace the remaining
Windows ↔ Windows long-duration benchmark, multi-monitor validation, or a future native macOS Host
capture implementation.
