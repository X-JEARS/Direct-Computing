# Windows platform

The first Windows adapter is implemented in `crates/dc-platform/src/windows.rs` using D3D11 and
DXGI Desktop Duplication. It currently supports unrotated outputs on the default graphics adapter.

Runtime verification is tracked in `docs/WINDOWS_CAPTURE_TESTING.md`. Input injection, ConPTY, and
Windows Service integration belong to later development stages.
