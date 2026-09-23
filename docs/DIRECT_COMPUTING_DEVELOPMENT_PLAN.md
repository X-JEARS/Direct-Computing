# Direct Computing 开发方案

- 版本：0.7（加入 QUIC 媒体通道、首帧诊断与低带宽恢复规划）
- 项目定位：局域网 / VPN 直连的跨平台远程桌面、文件传输与远程终端工具
- 实现语言：Rust
- 许可证：Apache-2.0；第三方代码继续按其各自许可证履行义务

## 1. 项目目标

Direct Computing（简称 DC）是一个面向局域网和 VPN 的远程控制工具，目标是替代传统 VNC，并补充类似 SSH 的远程终端能力。

核心使用方式：

```text
启动被控端服务
→ 客户端输入 IP / 域名 / VPN 地址和端口
→ 输入被控端密码
→ 选择远程桌面、终端或文件传输
```

项目默认不依赖：

- 设备识别码
- 账号系统
- 中央登录服务器
- rendezvous 服务
- 中继服务器
- NAT 穿透
- 云端设备列表

网络范围限定为：

- 局域网
- IPv6 直连
- VPN 网络
- 用户自行配置端口转发的网络

## 2. 功能范围

### 2.1 初期桌面版本必须具备

- Windows 主机和客户端
- IP/端口直连
- 密码认证
- TLS 1.3 加密
- 屏幕串流
- 鼠标键盘控制
- H.264 软件编码
- 基础自适应码率
- 文件发送和接收
- CLI 工具

### 2.2 后续功能

- 远程交互式终端
- 非交互式命令执行
- 基础 SSH 兼容端点
- macOS 支持
- Linux X11 支持
- Linux Wayland 支持
- H.264 硬件编码
- HEVC / AV1
- 多显示器
- 剪贴板同步
- 断点续传
- 局域网 mDNS 发现
- 无头服务器和虚拟显示器
- SFTP 兼容
- SSH Agent 转发（默认关闭）

### 2.3 明确不作为初期目标

- 公网 NAT 穿透
- 官方中继服务
- 账号和云端管理
- 移动端
- 音频重定向
- 打印机映射
- 远程摄像头
- 复杂企业策略
- 默认端口转发
- 隐蔽运行或隐蔽持久化

## 3. 总体架构

程序采用客户端与服务端一体的单一应用，可按角色启动：

```text
direct-computing --host
direct-computing --connect 192.168.1.20:22100
direct-computing --service
```

GUI、Host 和 Viewer 共用同一个核心库。

```text
Direct Computing
├── GUI
├── Host Service
├── Viewer
├── CLI
└── Shared Core
    ├── Address / Protocol
    ├── QUIC Transport
    ├── Authentication
    ├── Desktop Session
    ├── Terminal Session
    ├── Command Execution
    ├── File Transfer
    └── Platform Adapters
```

## 4. 代码仓库结构

建议使用一个 Rust Workspace，而不是为每个平台维护独立项目。

```text
direct-computing/
├── Cargo.toml
├── crates/
│   ├── dc-common/
│   ├── dc-protocol/
│   ├── dc-transport/
│   ├── dc-auth/
│   ├── dc-session/
│   ├── dc-desktop/
│   ├── dc-media/
│   ├── dc-terminal/
│   ├── dc-command/
│   ├── dc-file-transfer/
│   ├── dc-ssh/
│   ├── dc-platform/
│   └── dc-ui/
├── apps/
│   ├── direct-computing/
│   └── dc-cli/
├── platform/
│   ├── windows/
│   ├── macos/
│   └── linux/
├── tests/
├── docs/
└── packaging/
```

### 4.1 共享模块

以下模块应尽量保持跨平台：

- 地址解析
- 协议消息
- QUIC/TLS 连接
- 密码认证
- 会话状态机
- 文件分块和校验
- 码率控制
- CLI 参数
- 日志和错误处理
- 权限策略

### 4.2 平台模块

必须按平台实现：

- 屏幕采集
- 鼠标键盘注入
- 剪贴板
- PTY / ConPTY
- 显示器枚举
- 系统服务
- 开机启动
- 权限申请
- 硬件编码器

建议通过 Trait 隔离平台代码：

```rust
trait ScreenCapturer {}
trait InputInjector {}
trait ClipboardProvider {}
trait TerminalBackend {}
trait PermissionManager {}
trait SystemService {}
```

## 5. 主协议设计

### 5.1 连接方式

客户端只使用地址作为连接目标：

```text
192.168.1.20:22100
10.8.0.5:22100
[2001:db8::20]:22100
server.local:22100
```

不定义全局设备 ID。服务端只需要配置：

- 监听地址
- 监听端口
- 密码派生值
- 本地证书或公钥
- 服务权限

### 5.2 握手流程

```text
客户端连接地址
    ↓
TLS 1.3 / QUIC 握手
    ↓
交换协议版本和能力
    ↓
密码认证
    ↓
创建会话
    ↓
选择 Desktop / Terminal / Command / File 服务
```

建议保留可选的公钥指纹确认，但它不承担设备发现、账号管理或服务器注册功能。

### 5.3 多服务模型

连接建立后不直接假定进入视频会话，而是协商服务能力：

```text
Capabilities {
    desktop: true,
    terminal: true,
    command_execution: true,
    file_transfer: true,
    clipboard: false,
    ssh_compatibility: true
}
```

服务类型：

```rust
enum ServiceKind {
    Desktop,
    Terminal,
    Command,
    FileTransfer,
    Clipboard,
}
```

每个服务使用独立的 QUIC Stream 或 Datagram，不允许文件传输阻塞键鼠和视频。

## 6. 桌面串流

### 6.1 媒体模式

支持两种策略：

1. 视频串流模式：适合视频、动画和高频变化画面。
2. 桌面优化模式：脏区域、鼠标分离、静止画面低帧率，适合文档和服务器管理。

### 6.2 编码器抽象

```text
Encoder
├── Software H.264
├── NVENC
├── Intel QSV
├── VA-API
├── AMD AMF
├── VideoToolbox
└── AV1 / HEVC（后续）
```

第一版以 OpenH264 作为通用软件基线；网络 Host 会在启动时按能力检测优先选择
通用 Windows Media Foundation H.264 MFT，并在不可用时回退到 OpenH264。厂商专用
后端（NVENC、QSV、VA-API、AMF 等）仍按平台逐步补齐。

### 6.3 传输策略：QUIC 控制面 + 自定义媒体分片

当前方案保留 QUIC 作为统一传输和安全层，但不把所有桌面帧放入可靠、有序的
QUIC Stream：

```text
QUIC/TLS 1.3
├── 可靠双向 Stream：认证、控制、键鼠、关键帧请求和服务信令
└── QUIC DATAGRAM：H.264 媒体帧
    └── DCVD 自定义分片头：序号、分片索引、分片总数、总长度
```

这样可以避免丢失一个旧视频包时阻塞后续输入和新画面。DCVD 只负责媒体帧的
分片、乱序重组、过期帧淘汰和统计，不替代 QUIC 的加密、拥塞控制或连接管理。
因此当前没有必要为了媒体协议而更换 QUIC；后续应通过实测比较 DATAGRAM、可靠
关键帧流和 FEC/重传策略，而不是直接改用另一套传输协议。

### 6.4 首帧、静止桌面和恢复策略

Windows Desktop Duplication 在桌面没有变化时可能持续超时。当前实现已经覆盖：

- Viewer 在首个解码帧到达前创建占位窗口；
- Viewer 在 1 秒没有完整帧、检测到序号间隔或解码失败时发送 `KeyframeRequest`；
- Host 对支持强制 IDR 的编码器直接请求关键帧；
- 不支持运行时强制 IDR 的编码器通过重建编码器重置 GOP；
- Desktop Duplication 超时时使用最近一帧生成恢复帧，并使用递增时间戳，避免编码器将其当作重复帧跳过；
- Viewer 记录 Datagram 接收、完整帧重组、解码和呈现指标，用于区分网络丢片、解码失败和窗口问题。

当前仍需完成的低带宽可靠性工作：

- [ ] 关键帧可靠传输（可靠 QUIC Stream）或分片重传/FEC，避免关键帧任一分片丢失导致整帧不可用；
- [ ] 按 RTT、丢包、队列和吞吐动态调整初始码率、帧率、分辨率和关键帧间隔；
- [ ] 限制关键帧大小并避免在窄带链路上默认使用过高码率；
- [ ] 增加受控丢包、限速和高延迟网络测试，记录完整帧率、恢复时间和输入延迟。

### 6.5 自适应参数

根据以下指标调整码率和质量：

- RTT
- 丢包率
- 抖动
- 实际吞吐量
- 编码耗时
- 解码耗时
- CPU 占用
- 帧队列长度

优先降低帧率和码率，再降低分辨率；编码器切换只在重新连接或显式切换时发生。

## 7. 终端功能

终端功能分为三类，不能只实现一个“模拟键盘的终端”。

### 7.1 交互式终端

类似 SSH：

- 创建远端 shell
- stdin/stdout/stderr
- PTY 尺寸变化
- Ctrl+C、Ctrl+D 等控制字符
- ANSI 颜色和光标控制
- PowerShell、CMD、Bash、Zsh

协议消息可以包括：

```text
TerminalOpen { shell, columns, rows, environment }
TerminalInput { bytes }
TerminalOutput { stream, bytes }
TerminalResize { columns, rows }
TerminalClose { exit_code }
```

平台后端：

- Windows：ConPTY
- Linux/macOS：PTY、openpty 或 forkpty

### 7.2 非交互命令

面向脚本和 Agent：

```text
dc exec 192.168.1.20:22100 -- powershell -NoProfile -Command "Get-Service"
dc exec 10.8.0.5:22100 -- uname -a
```

返回：

- stdout
- stderr
- exit code
- timeout 状态
- 被终止原因

非交互命令必须是独立服务，不应通过解析终端画面获得结果。

### 7.3 原生终端显示

GUI 中点击“打开终端”时，可以启动本地 CLI：

```text
dc terminal 192.168.1.20:22100
```

由 CLI 运行在：

- Windows Terminal
- macOS Terminal / iTerm2
- Linux GNOME Terminal / Konsole 等

Direct Computing 不需要第一版自己实现完整终端模拟器。

## 8. SSH 兼容

SSH 兼容应作为独立端点存在，不与主 QUIC 协议强行混合。

### 8.1 端口建议

```text
22100：Direct Computing QUIC 协议
22122：可选 SSH 兼容端点
```

端口都应可配置。

### 8.2 SSH 兼容范围

第一阶段支持：

- 密码认证
- 远程 shell
- `ssh -t` PTY
- `ssh host command` 非交互执行
- 主机密钥
- 首次连接指纹确认

后续支持：

- SFTP
- SCP 兼容
- 端口转发（默认关闭）
- 子系统

默认不支持或关闭：

- SSH Agent 转发
- 任意端口转发
- X11 转发
- 反向隧道

这些功能会显著扩大攻击面，不应在早期默认开启。

### 8.3 用户名与密码

标准 SSH 客户端需要用户名，因此可以使用：

```text
ssh dc@192.168.1.20 -p 22122
```

其中 `dc` 可以是默认虚拟用户名，或者映射到被控端配置的本地系统用户。用户体验上仍然只需要输入被控端密码。

需要明确区分：

- Direct Computing 访问密码
- SSH 登录用户名
- 被控端真实系统用户

默认不应因为知道 DC 密码就获得 root、LocalSystem 或管理员权限。

### 8.4 SSH 实现策略

优先评估 Rust SSH 库，例如 `russh` 或同类实现。实现 SSH 兼容时，主机密钥必须在本地生成并保存，不能向任何中心服务上报。

如果未来需要复用系统 OpenSSH，也可以作为可选后端，但不应让 OpenSSH 成为跨平台主路径。

## 9. Agent 接入设计

Agent 优先使用结构化 CLI，而不是操纵 GUI：

```text
dc exec <address> -- <command>
dc terminal <address>
dc file-send <address> <local> <remote>
dc file-recv <address> <remote> <local>
```

建议支持：

- `--json` 结构化输出
- `--timeout`
- `--cwd`
- `--env KEY=VALUE`
- `--shell`
- `--no-pty`
- 明确的退出码
- stderr 与 stdout 分离
- 输出大小限制
- 命令超时

标准 SSH 客户端也可以供 Agent 使用：

```text
ssh -p 22122 dc@host command args...
```

Direct Computing 自己的 CLI 适合更稳定地返回结构化结果；SSH 兼容则适合接入现有 Agent、IDE 和自动化工具。

## 10. 权限模型

认证成功后，不应自动获得所有能力。建议定义会话权限：

```text
Permissions {
    view_desktop: bool,
    control_input: bool,
    open_terminal: bool,
    execute_command: bool,
    transfer_files: bool,
    ssh_access: bool
}
```

早期版本可以使用一个密码，但权限字段必须从第一版存在。后续可以配置：

- 普通密码：桌面查看、文件接收
- 控制密码：桌面控制和文件传输
- 管理密码：终端和命令执行

远程终端默认以明确配置的本地用户身份运行。提权必须继续经过系统自身的 UAC、sudo 或授权机制。

## 11. 安全要求

- TLS 1.3
- Argon2id 保存密码派生值
- PAKE 或挑战认证，避免明文密码
- 连接失败限速
- 可选 IP 白名单
- 可选仅绑定局域网/VPN 网卡
- 文件传输单独确认
- 命令执行日志
- 会话列表和一键断开
- 命令超时和输出上限
- 默认关闭 SSH 转发
- 默认关闭匿名访问
- 默认不启用隐藏持久化
- 不默认执行任何外部网络请求

## 12. 跨平台计划

### Windows 优先

- Desktop Duplication / Windows Graphics Capture
- SendInput
- ConPTY
- Windows Service
- UAC 基础处理
- H.264 软件编码

### Linux X11

- X11 屏幕采集
- XTest 输入
- PTY
- systemd 服务
- 软件编码

### macOS

- ScreenCaptureKit
- CGEvent
- 辅助功能权限
- 屏幕录制权限
- PTY
- LaunchAgent/LaunchDaemon

### Linux Wayland

- PipeWire
- Desktop Portal
- Wayland 输入限制
- 不同桌面环境兼容

Wayland 放在 X11 之后实现。

## 13. 开发阶段

### 阶段 0：工程初始化

- 建立 Rust Workspace
- 建立 crate 结构
- 定义错误类型和日志
- 定义协议版本
- 建立 Windows/Linux/macOS 编译任务
- 选择许可证

### 阶段 1：本地回环媒体原型

```text
屏幕采集 → H.264 软件编码 → 本地解码 → 显示
```

暂不实现网络和远程输入。

当前进度（2026-09-21）：

- [x] 定义帧尺寸、像素格式、步长、序号和时间戳模型
- [x] 定义编码包、关键帧和编解码器类型
- [x] 定义捕获、编码、解码和显示端接口
- [x] 建立可重复的 BGRA 合成画面源
- [x] 建立无损 Raw 回环，用于隔离管线问题
- [x] 接入 OpenH264 软件编码和解码
- [x] 提供 `direct-computing --loopback [frame-count]` 验证入口
- [x] 实现 Windows DXGI Desktop Duplication 采集器并通过 Windows 目标交叉编译
- [x] 提供 `direct-computing --capture-test` 真实采集与解码帧导出入口
- [x] 在 Windows 真机完成主显示器采集、颜色和方向的基础验证
- [x] 接入本地图形窗口显示解码帧，并支持缩放、关闭和 Escape 退出
- [x] 输出帧率、吞吐量及采集、编码、解码、显示的阶段耗时
- [ ] 在 Windows 真机验证实时预览、多显示器和静态桌面超时恢复
- [ ] 建立 30 分钟持续运行和资源占用基准

阶段 1 验收标准：真实屏幕帧经过 H.264 软件编码和本地解码后可在窗口持续显示；
分辨率、时间戳和帧序号正确；在目标平台连续运行 30 分钟无崩溃和明显资源增长。

### 阶段 1.5：实时串流基础（前置性能闸门）

阶段 1 的软件编码回环用于验证媒体模型，不作为远程桌面的最终性能方案。阶段 2
之前必须先完成以下与具体硬件后端无关的低延迟基础：

- [x] 将 Host 的采集、编码移出异步运行时线程
- [x] 使用有界有序视频队列和背压，避免 H.264 参考帧被覆盖丢弃
- [x] 合并同一 UI tick 内的鼠标移动事件，按键事件保持顺序
- [x] 记录 capture/encode/send/receive/decode/present 的阶段耗时和接收至显示延迟
- [x] 接入通用 Windows Media Foundation H.264 MFT 后端（优先硬件、软件回退）
- [x] Windows Viewer 接入 Media Foundation H.264 解码（优先硬件、OpenH264 回退）
- [x] macOS Viewer 接入 VideoToolbox H.264 解码并保留 OpenH264 运行时回退
- [x] macOS Viewer 保留 VideoToolbox 原生 BGRA 输出并通过 Metal 窗口呈现，跳过 RGB24 颜色转换
- [x] Viewer 首帧占位窗口、首帧编码/解码/非零像素诊断和呈现耗时日志
- [x] 静止桌面超时后的恢复帧路径；恢复帧时间戳保持单调递增
- [ ] 在 1080p/30 下完成端到端 P95 延迟基准

当前编码器和 Viewer 解码器会在启动或首个视频包到达时报告 backend、hardware、zero-copy
和 low-latency 能力；网络 Host/Viewer 每秒报告采集、编码、发送背压、解码、显示和接收至
显示耗时。接收至显示耗时不等于跨主机端到端帧年龄，后者还需要同步的
捕获时钟字段和双机基准测试。

Windows Viewer 优先使用 Media Foundation H.264 Decoder MFT，并在初始化或运行失败时切换到
OpenH264；macOS Viewer 优先使用系统 VideoToolbox，并保留同样的 OpenH264 回退路径。
VideoToolbox 会请求原生 BGRA 输出；macOS 的 minifb 窗口使用 Metal 上传和呈现该缓冲区，
因此不再执行逐像素 RGB24 颜色转换。CVPixelBuffer 直接交给 Metal 的完全零拷贝显示仍属于
后续渲染优化。

该阶段只提前硬件 H.264 的最小可用后端和流水线基础；QSV、VA-API、NVENC、
VideoToolbox 的完整能力矩阵和零拷贝渲染、HEVC/AV1 以及安装包仍保留在阶段 7。

### 阶段 2：局域网桌面直连

当前实现进度（2026-09-21）：

- [x] QUIC/TLS 1.3 端点和独立双向流
- [x] QUIC DATAGRAM 视频通道、MTU 分片和过期帧丢弃；控制/输入继续使用可靠流
- [x] 自定义 DCVD Datagram 分片重组，支持乱序、重复分片过滤和过期帧淘汰
- [x] IP 字面量和 DNS 地址解析
- [x] 协议版本、能力协商和长度受限的二进制消息
- [x] Argon2id 密码派生值和一次性挑战认证
- [x] 权限字段和桌面查看权限门控
- [x] H.264/Raw 视频包跨网络序列化与解码
- [x] 丢帧后的关键帧请求与 OpenH264 强制 IDR 恢复路径
- [x] 不支持强制 IDR 的编码器通过重建编码器恢复 GOP；静止桌面使用最近帧生成递增时间戳的恢复帧
- [x] Viewer 在无完整帧、序号间隔或解码失败时自动请求关键帧
- [x] 关键帧 Datagram 分片进行一次冗余发送，降低单个分片丢失导致首帧无法重组的概率
- [x] Viewer 使用有界解码队列保留恢复关键帧、丢弃过期依赖帧；序号断档后等待新关键帧并重建解码器
- [x] macOS Viewer 使用标题栏下方的内容布局呈现画面，鼠标只按实际视频矩形映射并忽略黑边
- [ ] 关键帧可靠传输或 FEC/分片重传，适配高丢包和低带宽链路
- [ ] 完成 QUIC DATAGRAM 接收计数、分片丢失率和恢复时间的双机实测
- [x] RTT/丢包/队列/编码耗时驱动的基础码率控制器
- [x] Windows SendInput 鼠标键盘注入和 GUI 事件采集
- [x] 证书 SHA-256 指纹固定、TOFU pin store 与首次连接指纹提示
- [ ] 真机 Windows ↔ Windows 长时间串流基准

Windows Host → macOS Viewer 可以先作为跨平台集成测试：Windows 使用 DXGI 采集，macOS 使用
通用 Viewer 窗口解码并回传输入。该测试不替代 Windows ↔ Windows 长时间基准；macOS Host
原生屏幕采集仍属于后续跨平台阶段。

2026-09-22 的 Windows Host → macOS Viewer 联调已确认新 QUIC DATAGRAM 路径能够持续接收并
呈现数据，窗口缩放后的鼠标指向与 Host 一致；标题栏和宽高比产生的黑边不参与远端坐标映射。
高丢包恢复时间、15 分钟以上稳定性和 1080p/30 P95 延迟仍需按专项测试清单记录。

当前可通过 `direct-computing --host [addr] <password>` 启动 Host，通过
`direct-computing --connect <addr> <password> [cert-sha256]` 启动 Viewer；首次连接会显示
服务端证书 SHA-256 指纹，后续把该值作为可选参数传入即可启用证书固定。非 Windows Host
使用合成画面，Windows Host 使用阶段 1 的桌面采集器。Viewer 窗口产生的鼠标/键盘事件
经过独立控制流发送，Host 仅在认证权限包含 `control_input` 时调用 Windows `SendInput`。

远程终端和命令执行不属于阶段 2 的桌面串流交付范围。协议继续保留
`terminal`、`command_execution` 能力字段和独立服务类型；在对应服务实现前，Host/Viewer
必须通过能力协商明确报告不支持，不能把终端数据复用到桌面控制流中。

阶段 2 本机验证记录（2026-09-21）：

- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`、
  `cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo test --workspace -j 1`
  均通过；工作区单元测试和文档测试没有失败项。
- 在 Windows 本机启动 `--host 127.0.0.1:22100`，使用正确密码启动
  `--connect 127.0.0.1:22100`；两个进程持续运行 15 秒，未报告 QUIC/TLS、认证或解码错误，
  随后正常清理进程。
- 使用错误密码连接时，Host 记录 `authentication failed`，Viewer 收到连接关闭；认证拒绝路径
  符合预期。
- 本记录验证的是本机进程间连接和协议路径，不替代 Windows ↔ Windows 真机的交互和长时间
  串流基准；该项仍保持未完成状态。
- Windows Host → macOS Viewer 的操作步骤和验收项见
  `docs/WINDOWS_MAC_STREAMING_TESTING.md`；尚未在真实双机环境执行，等待 Windows/macOS
  设备测试结果回填。

### 阶段 3：文件传输和桌面优化

- 当前实现进度（2026-09-21）：

- [x] 文件清单和固定大小分块描述
- [x] SHA-256 文件和分块校验
- [x] 断点续传偏移量校验
- [x] 文件块流式读取和协议消息转换
- [x] 接收路径安全校验，拒绝绝对路径和父目录穿越
- [x] 独立 QUIC 文件流发送/接收服务，支持初始断点 ACK、临时文件和原子落盘
- [ ] Host/Viewer 文件发送和接收 CLI
- [ ] 断点状态持久化和中断恢复
- 脏区域
- 光标分离
- 多显示器基础支持

### 阶段 4：Direct Computing 终端（桌面和文件功能稳定后）

阶段 4 在阶段 2 的桌面串流验收、阶段 3 的文件传输 CLI/恢复能力完成后开始。终端服务
复用已经稳定的 QUIC、TLS、认证和权限模型，但使用独立服务流和独立权限，不影响已有桌面
串流协议。

- Windows ConPTY
- Linux/macOS PTY
- `dc terminal`
- `dc exec`
- stdout/stderr/exit code
- 终端尺寸调整

### 阶段 5：SSH 兼容

- 独立 SSH 端口
- 密码认证
- 主机密钥
- shell
- exec
- PTY
- 首次指纹确认

### 阶段 6：跨平台

1. Linux X11
2. macOS
3. Linux Wayland
4. 无头服务器

### 阶段 7：硬件编码和发布

- QSV
- VA-API
- NVENC
- VideoToolbox
- HEVC/AV1
- 安装包
- 后台服务
- 文档和发布流程

## 14. 测试方案

### 单元测试

- 地址解析
- 协议版本
- 密码认证
- 能力协商
- 权限判断
- 文件分块
- 路径安全
- 命令超时
- 退出码处理
- 帧布局和缓冲区长度
- 合成画面序号和时间戳
- Raw 与 H.264 本地回环

### 集成测试

- Host ↔ Viewer
- Host ↔ Host
- Desktop 与 Terminal 并发
- 文件传输与视频并发
- Windows ↔ Windows
- Windows ↔ Linux
- macOS ↔ Windows
- VPN 地址连接
- 限速、延迟和丢包
- 连接中断后恢复

### Agent 测试

- `dc exec --json`
- stdout/stderr 分离
- 命令退出码
- PTY 交互
- 终端 resize
- 超时和取消
- SSH `-t`
- SSH 远程命令
- SSH 主机密钥变更提示

## 15. 开源与依赖策略

Direct Computing 本身直接开源。复用 RustDesk、Sunshine、FFmpeg 或其他项目时，必须：

- 保留版权和许可证
- 记录第三方依赖
- 发布所需源代码和修改
- 核查 GPL/AGPL 义务
- 核查编解码器专利和分发限制
- 不把参考代码误标为原创

项目文档中应维护：

```text
THIRD_PARTY_LICENSES.md
NOTICE.md
```

## 16. 版本里程碑与完成标准

### 桌面优先版本

阶段 0 至阶段 3 完成后，可交付一个以远程桌面为核心的局域网版本：

```text
电脑 A 启动 Host
电脑 B 输入电脑 A 的 IP:端口
输入密码
看到远程桌面
可以控制鼠标键盘
可以发送和接收文件
```

该版本不承诺远程终端、非交互式命令执行或 SSH 兼容；这些能力在后续阶段实现。

### 完整首个可用版本

阶段 0 至阶段 5 完成后，首个可用版本的定义：

```text
电脑 A 启动 Host
电脑 B 输入电脑 A 的 IP:端口
输入密码
看到远程桌面
可以控制鼠标键盘
可以启动远程终端
可以执行命令并得到退出码
可以使用标准 SSH 客户端连接兼容端口
```

性能方面先以“局域网内稳定可用”为目标，不追求 4K/60 或极限压缩率。

## 17. 下一步开发入口

阶段 0 已于 2026-09-20 完成。阶段 1 已完成媒体模型、合成画面、真实 H.264
编解码回环、Windows DXGI 采集和本地图形窗口显示。Windows 主显示器的帧导出已于
2026-09-21 在真机验证通过；实时预览入口和阶段耗时指标已实现。

下一次开发从阶段 1 的剩余实时预览验收和稳定性基准继续。2026-09-21 已记录主显示器
短时预览、H.264 跳帧恢复和 30 分钟连续运行结果；由于长测日志没有外部内存、CPU、GPU
采样，资源占用基准仍需在登录且未锁定的交互式 Windows 桌面补采：

> 按 `docs/WINDOWS_CAPTURE_TESTING.md` 在 Windows 10/11 真机运行 `--preview`，验证实时画面的
> 颜色、方向、缩放、关闭行为和静态桌面超时恢复；随后覆盖多显示器场景，并建立 30 分钟连续
> 运行的帧率、内存、CPU 和 GPU 基准。根据结果修正显示器枚举、旋转和访问丢失处理。

完成阶段 1/2 的桌面验收后，先推进阶段 3 的文件传输 CLI、断点恢复和桌面优化；阶段 4
远程终端排在这些桌面功能稳定之后，阶段 5 SSH 兼容再依赖阶段 4 的命令与 PTY 能力。

后续按照阶段 1 到阶段 7 逐步推进，不提前实现公网 NAT、账号系统或复杂企业功能。
