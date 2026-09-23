# Direct Computing 桌面增量传输开发方案

- 文档版本：0.1
- 状态：设计草案，供阶段 3 桌面优化实现使用
- 适用范围：Host 到 Viewer 的桌面图像传输
- 现有基础：QUIC/TLS 1.3、可靠关键帧、QUIC DATAGRAM 普通帧、H.264 NAL 分片、DXGI dirty/move rect、PackBits 区域更新、独立光标

## 1. 结论

建议将下一代桌面媒体协议实现为“有状态 framebuffer 增量协议”，而不是把 H.264
内部的 skip 宏块重新封装成自定义 slice：

```text
可靠媒体控制流
  └── 更新清单：基础状态、目标状态、变化区域、复制操作、载荷编号

QUIC DATAGRAM
  └── 变化载荷：区域像素或完整 H.264 access unit 的分片

可靠反馈流
  └── ACK / NACK / 状态摘要 / 区域刷新请求
```

每次更新只描述变化部分。清单中未出现的 tile 自动继承客户端已经确认的基础状态，
这就是协议层的 skip 语义，不需要逐个发送 skip 宏块或 skip 矩形。

第一阶段继续使用现有两种图像路径：

- 小范围、文字和 UI 变化：区域更新，使用 Raw 或 PackBits；
- 大范围、视频和动画：完整 H.264 picture，使用现有 NAL 分片；
- 窗口移动和滚动：增加 `CopyRect`，先复用客户端已有像素，再发送暴露出来的区域；
- 光标：继续使用独立可靠状态，不进入视频编码。

UDP 可靠性由应用层做选择性重传，但不假定“没有 NACK 就一定已经收到”。Host 缓存
已经编码和分片的数据；Viewer 对仍有显示价值的缺片发送 NACK，并低频发送累计 ACK 和
framebuffer 状态确认。过期数据不再重传，改为发送区域独立刷新或完整关键帧。

## 2. 目标与非目标

### 2.1 目标

- 静止桌面不发送图像载荷，只发送必要的保活和低频状态确认；
- 少量变化只传输变化区域，不为静态区域支付 H.264 skip、slice 和网络头开销；
- 丢失一个普通载荷时，后续无依赖区域仍能继续显示；
- 能够只重传缺失分片或只刷新失效区域；
- 避免 TCP 式跨媒体单元队头阻塞，同时保留 QUIC 的加密和拥塞控制；
- 明确维护 Host 与 Viewer 的 framebuffer 版本，避免长期画面漂移；
- 保持现有旧版整帧 H.264 路径可回退。

### 2.2 非目标

- 不修改 H.264 标准语法；
- 不从外部 skip 描述拼造一个声称完整的标准 H.264 picture；
- 不要求通用硬件解码器逐 slice 输出局部画面；
- 不保证所有旧更新最终送达，过期桌面更新允许被新状态替代；
- 不自行替代 QUIC 的拥塞控制。

## 3. 为什么不直接传输 H.264 skip slice

H.264 slice 是独立的语法和熵解码单元，不是独立的 framebuffer 补丁。一个 P slice
仍可能依赖历史参考图像、DPB、运动补偿和整张 picture 的解码状态。缺失 slice 时，标准
解码器只知道数据不完整，并不知道缺失区域本来是否全为 skip。

将所有 skip 宏块收集到同一个普通 slice 也通常不可行。宏块按照扫描顺序分布，skip 和
非 skip 往往交错；FMO 虽可改变宏块分组，但硬件支持差、压缩效率下降，不适合作为项目
主路径。

因此协议采用更直接的含义：

```text
未在本次更新中列出的区域 = 保留 base_state_id 中的现有像素
```

客户端直接保留 framebuffer 中的像素，不把这些区域送进 H.264 解码器。这比发送
“所有 skip 区域列表”更省，因为通常只需列出少数发生变化的区域。

## 4. 总体架构

```text
DXGI / ScreenCaptureKit / PipeWire
        │
        ├── dirty rect
        ├── move rect
        └── 完整 BGRA 帧
                │
        更新规划器 UpdatePlanner
        ├── 合并和切分区域
        ├── 选择 CopyRect / Fill / Patch / FullVideo
        ├── 分配 state_id、update_id、payload_id
        └── 选择编码和可靠性等级
                │
        ┌───────┴───────────────────┐
        │                           │
可靠媒体控制流                 QUIC DATAGRAM
UpdateManifest                 MediaFragment
StateAck/RefreshRequest        Patch/H.264 NAL payload
        │                           │
        └──────── Viewer ──────────┘
                    │
             更新暂存与校验
                    │
             原子提交 framebuffer
```

控制、键鼠、文件和桌面媒体继续使用不同的 QUIC stream/datagram lane。媒体控制流发生短暂
重传时，不得阻塞输入事件。

## 5. framebuffer 与 tile 模型

### 5.1 固定 tile 网格

建议默认使用 `64 x 64` tile，与当前 `DirtyRegionDetector` 一致。边缘 tile 可以小于固定
尺寸。tile 网格主要用于：

- 变化位图；
- 版本和校验摘要；
- NACK 后的局部刷新；
- 合并相邻变化区域；
- 限制单次错误的空间范围。

协议仍允许矩形区域跨越多个 tile，避免每个 tile 都产生独立网络头。

### 5.2 状态编号

每次可提交的桌面状态包含：

```text
epoch_id       捕获尺寸、像素格式或显示器布局变化时递增
base_state_id  本次更新依赖的客户端状态
state_id       应用本次更新后得到的状态
update_id      本次传输事务编号
```

规则：

- `epoch_id` 不一致时，Viewer 清空增量状态并请求完整刷新；
- Viewer 只有在本地拥有 `base_state_id` 时才能提交增量更新；
- `state_id` 只在整份更新成功提交后生效；
- 单个区域失败不能让 Viewer 宣称已经拥有整个 `state_id`；
- 初版同一会话只允许一条已确认的主状态链，避免分支参考链复杂化。

### 5.3 skip 的协议表达

`UpdateManifest` 只列出变化操作。未被任何操作覆盖的 tile 继承 `base_state_id`，无需发送
显式 skip 数据。

可选的变化 tile bitmap 只用于快速校验清单和统计，不应再列举未变化 tile。对于 1080p、
64 像素 tile，网格约为 `30 x 17 = 510` 个 tile，完整位图仅约 64 字节；是否携带由能力
协商决定。

## 6. 更新操作

首版支持以下操作：

```rust
enum DesktopOp {
    CopyRect {
        src: DesktopRect,
        dst_x: u32,
        dst_y: u32,
    },
    Fill {
        rect: DesktopRect,
        bgra: u32,
    },
    Patch {
        rect: DesktopRect,
        payload_id: u64,
        encoding: PatchEncoding,
        decoded_len: u32,
        checksum: u32,
    },
    FullVideo {
        payload_id: u64,
        codec: VideoCodec,
        keyframe: bool,
    },
}
```

操作应用顺序必须固定：

1. 读取 `base_state_id` 的不可变快照；
2. 执行所有 `CopyRect`；复制源始终来自基础快照，避免重叠复制产生顺序差异；
3. 执行 `Fill`；
4. 解码并覆盖 `Patch`；
5. `FullVideo` 替换整个 framebuffer；
6. 校验后原子发布 `state_id`；
7. 最后合成独立光标。

同一清单不允许 `FullVideo` 与其他像素操作混用。矩形越界、操作重叠规则冲突或解码长度
不符时，Viewer 必须拒绝整次更新并请求刷新。

### 6.1 Patch 编码

首版保留：

- `RawBgra`：高熵但非常小的区域；
- `PackBitsBgra`：纯色、文字背景和简单 UI；

后续根据真实数据评估：

- `ZstdBgra`：中等面积、无损区域；
- `Jpeg420` 或 WebP：允许有损的照片区域；
- 独立区域视频流：只在跨平台硬件解码和状态隔离得到验证后加入。

每个 Patch 的编码选择应比较最终 wire size，包括操作头和分片头，而不只比较压缩 payload。

### 6.2 H.264 的职责

H.264 继续负责大面积和高频变化画面。初版不将单个 slice 当成可直接覆盖 framebuffer 的
区域补丁：

- SPS/PPS 和恢复 IDR 走可靠媒体流；
- 普通 access unit 按现有 NAL 边界分片后走 Datagram；
- 编码器尽可能限制 slice/NAL 大小接近 Datagram payload；
- 禁用 B 帧和 lookahead，减少重排序与延迟；
- 普通帧丢失且参考链不可信时，请求新 IDR；
- NAL 分片可以选择性重传，但通用解码器仍按完整 picture 提交。

第二阶段若实现“区域 H.264”，每个区域必须具有独立编码上下文或明确的帧内刷新边界，且
不能引用客户端未确认的其他区域状态。该能力单独协商，不能复用普通 `h264_nal_datagrams`
标志暗示支持。

## 7. 协议消息建议

以下是设计结构，不要求直接使用 Rust 内存布局作为 wire format。

### 7.1 能力协商

在 `Capabilities` 中新增独立标志：

```text
desktop_state_v1
desktop_copy_rect
desktop_selective_retransmit
desktop_state_digest
```

保留现有 `desktop_optimizations` 和 `h264_nal_datagrams` 作为旧路径兼容能力。双方只有在
对应能力均成立时才能启用新消息。

### 7.2 UpdateManifest

```text
UpdateManifest {
    epoch_id: u32,
    update_id: u64,
    base_state_id: u64,
    state_id: u64,
    timestamp_millis: u64,
    desktop_width: u32,
    desktop_height: u32,
    deadline_millis: u16,
    operations: Vec<DesktopOp>,
}
```

清单通过专用可靠媒体控制流发送。它通常很小，即使发生重传也不会阻塞键鼠控制流。
Datagram 载荷可能先到达，Viewer 可按 `update_id` 暂存到有界 orphan cache；超时后丢弃。

### 7.3 MediaFragment

```text
MediaFragment {
    epoch_id: u32,
    update_id: u64,
    payload_id: u64,
    fragment_index: u16,
    fragment_count: u16,
    payload_len: u32,
    flags: u8,
    data: bytes,
}
```

要求：

- 一个 Datagram 不超过协商后的最大大小；
- `(epoch_id, update_id, payload_id, fragment_index)` 唯一标识一个分片；
- 重传必须使用相同内容和标识；
- 重复分片可直接丢弃；
- 对计数、长度、总内存和在途事务数设置硬上限；
- payload 完成后校验长度和 checksum，再交给解码器。

### 7.4 反馈消息

```text
MediaNack {
    epoch_id: u32,
    update_id: u64,
    payload_id: u64,
    missing_ranges: Vec<FragmentRange>,
}

StateAck {
    epoch_id: u32,
    applied_state_id: u64,
    highest_seen_update_id: u64,
    receive_bitmap: u64,
}

RefreshRequest {
    epoch_id: u32,
    current_state_id: u64,
    regions: Vec<DesktopRect>,
    reason: RefreshReason,
}
```

`MediaNack` 请求具体缺片；`StateAck` 证明客户端实际提交到了哪个状态，而不是仅仅看到过
哪些包；`RefreshRequest` 用于缓存已过期、状态不匹配、校验失败或解码失败的情况。

## 8. 发送、接收与恢复状态机

### 8.1 Host 发送流程

```text
捕获新画面
  → 获得 dirty/move rect
  → 与客户端已确认 state 比较
  → 规划 CopyRect / Fill / Patch / FullVideo
  → 编码 payload
  → 缓存最终分片
  → 先发送 UpdateManifest
  → 发送 Datagram 分片
  → 继续捕获，不等待逐更新 ACK
```

Host 允许有限数量的未确认更新在途，但增量更新的 `base_state_id` 必须可恢复。首版建议：

- 最多 3 个未确认 state；
- 新更新优先基于最近已确认 state；
- 若使用未确认 state 作为 base，必须保留完整依赖链；
- 依赖链超出窗口时，合并为基于已确认 state 的新 Patch，或发送独立刷新。

为了降低首版复杂度，可以先严格要求所有增量更新都基于最近已确认 state。它会多编码少量
重复变化，但最容易保证正确性。

### 8.2 Viewer 接收流程

```text
收到 Manifest 或 Fragment
  → 按 update_id 建立有界暂存项
  → 检查 epoch/base_state/deadline
  → 收齐所有必要 payload
  → 校验 checksum
  → 在后台 framebuffer 上执行操作
  → 原子提交 state_id
  → 发送累计 StateAck
```

Viewer 不应该在 Patch 分片尚未完整时直接覆盖正在显示的 framebuffer，否则丢包会留下半块
新、半块旧的撕裂状态。可以对完整小区域立即提交，但必须以一个操作或一个 update 为原子
边界，并保证 state_id 只代表完整状态。

### 8.3 丢包检测

Viewer 通过以下信息判断缺片：

- `fragment_count` 和接收位图；
- 后续 fragment/update 的序号；
- Manifest 中列出的 payload；
- 重组截止时间。

乱序等待时间建议从 RTT 动态计算：

```text
reorder_wait = clamp(smoothed_rtt / 4, 2 ms, 20 ms)
```

在 LAN 默认可从 5 ms 起步。不能在看到第一个序号缺口时立即 NACK，否则正常乱序会制造
重复流量。

### 8.4 选择性重传

Viewer 仅在以下条件全部满足时发送 NACK：

- 更新仍未过显示 deadline；
- 缺失 payload 仍是可提交状态链的一部分；
- 后续更新没有以独立内容完全覆盖该区域；
- 重试次数未超限。

建议最多发送两轮 NACK。Host 收到后：

```text
缓存仍存在且更新仍有价值 → 重发原分片
缓存不存在或已经过期       → 返回 RefreshRequired
状态链已经失效             → 发送区域独立刷新或可靠 IDR
```

重传的是缓存的最终分片，不重新捕获或重新编码。这样内容、校验和参考关系保持一致。

### 8.5 恢复层级

按成本从低到高恢复：

1. 重发缺失 Datagram 分片；
2. 对受影响矩形发送独立 Raw/PackBits/Zstd Patch；
3. 对完整 framebuffer 发送可靠 H.264 IDR；
4. epoch 变化时重新初始化桌面会话。

如果丢失的是 H.264 参考帧，且解码器无法证明参考链仍正确，不应继续喂入依赖帧等待错误
自行消失，应直接进入可靠 IDR 恢复。区域 Patch 与 H.264 DPB 是两套状态：Patch 不应推进
编码器或解码器的 DPB。只要两端仍停留在同一个 H.264 参考状态，之后的完整 P picture 可以
继续引用该状态并替换 framebuffer；如果编码器曾推进到 Viewer 没有收到的参考 picture，
或任何一端重建了不同的参考像素，则必须先发送新的可靠 IDR。

## 9. 缓存与淘汰

Host 缓存以下内容：

- 已编码 payload；
- 最终 Datagram 分片；
- Manifest；
- update/state 依赖关系；
- 区域覆盖范围和 deadline。

不要只缓存原始帧，也不要只按“经过多少帧”淘汰。帧率会变化，静止桌面甚至没有新帧。
使用时间、字节数和状态窗口的组合上限：

```text
cache_lifetime = clamp(max(3 * smoothed_rtt, 250 ms), 250 ms, 1500 ms)
max_cache_bytes = 按目标码率和内存预算计算，初始建议 8-32 MiB
max_inflight_updates = 8
max_unacked_states = 3
```

满足任一条件即可淘汰：

- `StateAck` 已确认且后续状态不再依赖；
- 超过重传有效期；
- 新操作完全覆盖旧区域，旧更新已无显示价值；
- 新 IDR 或 epoch 使旧参考链失效；
- 达到缓存内存上限，优先淘汰最旧且可被刷新替代的数据。

“连续若干帧没有收到 NACK 就删除”只能作为附加条件，不能作为收到确认的证明。NACK 本身
也可能丢失，因此必须保留低频 `StateAck` 或状态摘要。

## 10. ACK、NACK 与反馈流量

为了避免每包 ACK，使用混合反馈：

- 缺包时立即发送合并后的 NACK；
- 成功提交状态时延迟或累计 ACK，例如每 20 ms 或每 2-4 个 update；
- 静止桌面仍每 1-2 秒发送一次当前 `state_id`；
- NACK 可重复一次，并携带相同缺片范围；
- ACK/NACK 使用可靠反馈流；若请求后的媒体分片仍缺失，可在重传截止前发送更新后的 NACK，
  但不通过高频重复相同 NACK 来绕过 QUIC 自身的可靠传输；
- Host 根据 ACK 回收缓存，根据 NACK 重传，不根据“沉默”推断确定送达。

QUIC DATAGRAM 已受连接级拥塞控制约束。应用仍需限制重传流量，例如重传最多占当前媒体
预算的 10%-20%；拥塞持续时，应降低码率、帧率或改发最新区域刷新，而不是追补大量旧包。

## 11. 编码与路径选择策略

更新规划器先按最终预计成本选择路径：

```text
无像素变化                       → 不发送图像更新
纯窗口移动/滚动                  → CopyRect + 暴露区 Patch
小于屏幕 10% 且压缩后 <= 256 KiB → Region Patch
大面积、视频或高频连续变化       → Full H.264
状态不一致                       → Region Refresh 或可靠 IDR
```

上述阈值应配置并通过测试调整，不作为 wire protocol 常量。规划器需要考虑：

- dirty area 比例；
- dirty rect 数量和合并后的浪费面积；
- 历史压缩比；
- RTT、丢包和可用码率；
- 编码耗时；
- 客户端确认状态；
- 本次更新在显示 deadline 前完成的概率。

高丢包时优先减少分片数；高 RTT 时优先发送可独立应用的最新 Patch；连续视频区域则保持完整
H.264，避免无损 BGRA Patch 占满链路。

## 12. 与当前代码的映射

当前模块可按以下方式演进：

| 当前位置 | 改造内容 |
| --- | --- |
| `crates/dc-protocol` | 新增能力位、Manifest、ACK/NACK、Refresh 消息和严格长度限制 |
| `crates/dc-desktop/src/region.rs` | 将 dirty detector 扩展为 tile 状态、CopyRect/Fill/Patch 规划器 |
| `crates/dc-desktop/src/video_datagram.rs` | 抽象通用 `MediaFragment`，增加 payload 级接收位图、NACK 与重传标识 |
| `crates/dc-media` | 保留完整 H.264 编解码；增加 Patch codec 抽象和 wire-size 估算 |
| `crates/dc-platform` | 保留 DXGI dirty/move rect，补齐 move rect 到 CopyRect 的转换 |
| `apps/direct-computing` Host | 增加发送缓存、依赖跟踪、更新规划和反馈处理 |
| `apps/direct-computing` Viewer | 增加 update 暂存、双 framebuffer、原子提交和状态 ACK |

当前 `DesktopUpdate` 可以作为兼容消息保留。新能力未协商时继续使用：

```text
可靠关键帧 + H.264 NAL Datagram + 现有 DesktopUpdate
```

新能力协商成功后才使用 stateful manifest。协议仍保持同一 major version，还是升级 major，
应在完成 wire-format 原型后决定；如果改变了同一消息编号的语义，必须升级 major。优先新增
消息编号和能力位，避免破坏旧实现。

## 13. 实施阶段

### 阶段 A：可观测性与基准

- [ ] 记录 dirty tile 数、变化面积、区域 wire size、H.264 wire size；
- [ ] 记录每帧 NAL/Datagram 数、丢片、乱序、重组时间和显示 deadline；
- [ ] 建立静止桌面、文字滚动、窗口拖动、网页滚动和视频播放数据集；
- [ ] 确认当前 PackBits 与完整 H.264 的切换阈值。

验收：能基于日志回答每类场景的字节数、P50/P95 延迟和恢复时间。

### 阶段 B：协议状态与可靠反馈

- [ ] 增加 `epoch_id/update_id/base_state_id/state_id`；
- [ ] 增加 `UpdateManifest`、`StateAck`、`RefreshRequest`；
- [ ] Viewer 使用后台 framebuffer 原子提交；
- [ ] Host 只以已确认状态作为首版增量 base；
- [ ] 实现状态不匹配时的可靠完整刷新。

验收：随机丢弃任意区域更新后，双方不会永久漂移，且能在限定时间内恢复。

### 阶段 C：分片 NACK 与发送缓存

- [ ] 将 Datagram 标识扩展到 update/payload/fragment；
- [ ] Viewer 生成合并的缺片范围；
- [ ] Host 缓存并原样重发最终分片；
- [ ] 增加基于 RTT、deadline、字节和状态窗口的淘汰；
- [ ] 缓存过期时降级为区域刷新或可靠 IDR；
- [ ] 重传带宽纳入码率预算。

验收：1%-5% 随机丢包下，大部分小区域更新无需完整 IDR 即可恢复，且延迟不持续累积。

### 阶段 D：CopyRect 与区域编码改进

- [ ] 使用 DXGI move rect 生成 `CopyRect`；
- [ ] 正确处理重叠复制、暴露区域和多显示器边界；
- [ ] 增加纯色 `Fill` 检测；
- [ ] 评估 Zstd Patch，保留 Raw/PackBits 作为低开销路径；
- [ ] 合并相邻 rect，并限制单次操作数量。

验收：窗口拖动和文本滚动的发送字节显著少于完整 H.264，画面无残影。

### 阶段 E：高级视频区域实验

- [ ] 评估编码器 slice 大小控制在所有平台上的实际支持；
- [ ] 评估 region/tile 独立视频编码上下文的 CPU、码率和硬件兼容性；
- [ ] 验证逐区域解码输出接口，不将完整 picture API 误当局部输出；
- [ ] 仅在收益明确时加入新的协商能力。

该阶段不是首版增量协议的前置条件。

## 14. 测试计划

### 14.1 单元测试

- Manifest 编解码、边界和恶意长度；
- tile bitmap 与矩形映射；
- CopyRect 重叠和越界；
- Fragment 乱序、重复、缺失和冲突元数据；
- NACK range 合并；
- cache deadline 和字节上限淘汰；
- base state 不匹配；
- checksum 失败；
- update 原子提交；
- update_id/state_id 回绕策略。

### 14.2 确定性网络模拟

对每个场景注入：

- 0%、0.1%、1%、3%、5%、10% 丢包；
- 乱序、重复和突发丢包；
- 1/20/80/200 ms RTT；
- 1/5/20/100 Mbps 限速；
- NACK 丢失和 ACK 延迟；
- Manifest 后到、payload 先到；
- 缓存恰好在 NACK 到达前过期。

每次测试最终比较 Host 捕获 framebuffer 与 Viewer 已提交 framebuffer 的摘要。协议不能只以
“解码器没有报错”作为正确性标准。

### 14.3 场景测试

- 完全静止桌面 10 分钟；
- 光标持续移动但桌面不变；
- 终端文字滚动；
- 文档滚动；
- 窗口拖动、遮挡和最小化；
- 浏览器视频播放；
- 分辨率、旋转和显示器布局变化；
- Host/Viewer 暂停、断网并恢复；
- Windows Host 到 Windows/macOS Viewer。

### 14.4 初始性能指标

- 静止桌面图像 payload 接近 0；
- LAN 无丢包下输入不受媒体重传阻塞；
- 1% 随机丢包下 P95 区域恢复小于 2 个 RTT 或 150 ms，取较大者；
- 状态不一致在 2 秒内触发区域刷新或可靠 IDR；
- 发送缓存严格受字节数和时间限制；
- Viewer orphan/inflight 缓存严格受条目数和字节数限制；
- 任何丢包组合最终都能通过刷新恢复到一致画面。

## 15. 安全与健壮性要求

- 对矩形坐标、面积、stride、decoded length 使用 checked arithmetic；
- 限制每个 update 的操作数、payload 数、fragment 数和总声明字节数；
- 同一 payload 的元数据变化视为协议错误；
- 拒绝重叠且语义冲突的操作；
- checksum 只用于损坏和状态诊断，安全性仍由 QUIC AEAD 提供；
- 不允许远端通过大量孤立 fragment 或 NACK 造成无界缓存；
- NACK 只能引用仍在发送窗口内且属于当前会话的 payload；
- 限制重传次数和带宽，防止反馈放大；
- epoch 或分辨率改变时清空旧暂存和参考状态。

## 16. 决策摘要

1. 保留 QUIC，不切换为单一 TCP 视频流；可靠控制和不可靠媒体使用独立 lane。
2. skip 采用“未列出区域继承 base state”的协议语义，不发送 H.264 skip slice。
3. 小变化使用区域 Patch，大变化使用完整 H.264，窗口移动使用 CopyRect。
4. H.264 NAL/slice 分片是传输和错误隔离手段，不承诺局部 picture 输出。
5. 使用 Datagram + NACK 选择性重传，并辅以低频 StateAck；沉默不等于确认。
6. Host 缓存最终编码分片，按 RTT、deadline、状态依赖和内存上限淘汰。
7. 旧包过期或参考链断裂时，不追补历史，发送最新区域刷新或可靠 IDR。
8. 首版增量更新只基于 Viewer 已确认状态，正确性优先于最小字节数。
