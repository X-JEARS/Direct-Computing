# Platform implementations

OS-specific implementations live below this directory and implement the traits exposed by
`dc-platform`.

- `windows`: screen capture, input injection, ConPTY, service integration
- `macos`: ScreenCaptureKit, CGEvent, permissions, PTY, service integration
- `linux`: X11/Wayland capture, input integration, PTY, systemd integration

Platform code is introduced only when its corresponding development stage begins.
