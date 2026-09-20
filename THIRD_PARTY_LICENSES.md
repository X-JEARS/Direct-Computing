# Third-party licenses

Versions below are locked by `Cargo.lock`. The authoritative license texts are included in each
crate's source distribution.

| Package | Version | License |
| --- | --- | --- |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| cc | 1.4.7 | MIT OR Apache-2.0 |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| find-msvc-tools | 0.1.13 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| jobserver | 0.1.35 | MIT OR Apache-2.0 |
| libc | 0.2.189 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| minifb | 0.28.0 | MIT OR Apache-2.0 |
| nasm-rs | 0.3.2 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| openh264 | 0.9.8 | BSD-2-Clause |
| openh264-sys2 | 0.9.8 | BSD-2-Clause |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| raw-window-handle | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| safe_arch | 1.2.0 | Zlib OR Apache-2.0 OR MIT |
| same-file | 1.0.6 | Unlicense OR MIT |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| walkdir | 2.5.0 | Unlicense OR MIT |
| wide | 1.7.1 | Zlib OR Apache-2.0 OR MIT |
| winapi | 0.3.9 | MIT OR Apache-2.0 |
| winapi-util | 0.1.11 | Unlicense OR MIT |
| windows | 0.62.2 | MIT OR Apache-2.0 |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 |
| windows-core | 0.62.2 | MIT OR Apache-2.0 |
| windows-future | 0.3.2 | MIT OR Apache-2.0 |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 |
| windows-result | 0.4.1 | MIT OR Apache-2.0 |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 |
| x11-dl | 2.21.0 | MIT |

## OpenH264 distribution note

The stage 1 prototype builds Cisco OpenH264 from source through `openh264-sys2`. OpenH264's source
code is BSD-2-Clause licensed, but H.264 implementations and distribution may be subject to patent
licensing requirements in some jurisdictions. Before distributing binaries, the project must make
an explicit codec-distribution decision and complete a legal review. Cisco's separately distributed
prebuilt binaries have their own patent-license terms; this project does not currently download or
redistribute those binaries.
