# Vegas+ 远程队列流管理方案记录

- 状态：设计候选 / 待验证
- 适用方向：Direct Computing 远程桌面、终端、文件传输和其他共享链路流量
- 参考文章：[Vegas+ 与 AIMD 的公平共存](https://zhuanlan.zhihu.com/p/2086123093186106637)
- 记录目的：保留一种“低延迟优先、被强势流压制时临时增强竞争力”的队列流管理思路，供后续协议和传输层设计参考

## 1. 设计目标

Vegas+ 的核心目标是在两种需求之间取得平衡：

1. 正常情况下保持较短队列和较低 RTT，避免远程输入、终端回显和桌面画面被大队列拖慢。
2. 与 Reno/CUBIC 等基于丢包或 AIMD 的竞争流共存时，不因主动避让而长期失去带宽。
3. 不依赖路由器显式协作，单个流可以根据自身 RTT、吞吐和丢包观测完成模式切换。
4. 媒体、控制、文件和终端流量可以共享连接，但不会因为某一类流量持续占满队列而让交互流失去响应。

该方案目前是控制器设计记录，不代表已经实现，也不代表已经证明适合公网、无线网络或所有现代拥塞控制算法。

## 2. 背景：Vegas 与 AIMD 的竞争差异

可以把瓶颈链路想象成收费站：

- cwnd 是允许同时在路上的数据包数量；
- 路由器队列是收费站前的车辆队列；
- RTT 是车辆往返耗时。

Reno/AIMD 主要等到丢包才认为拥塞，通常会持续增加窗口，直到队列接近溢出。Vegas 则观察 RTT 相对历史最小 RTT 的增量，在队列变长时提前降低发送速率。

因此混合运行时可能出现正反馈：

~~~
Reno 增大窗口
  -> 队列变长、RTT 增大
  -> Vegas 主动退让
  -> Reno 获得更多带宽并继续增大窗口
~~~

Vegas+ 的思路是：平时使用 Vegas 的低延迟控制；检测到自己被 AIMD 流突然抢占时，临时使用更激进的 AIMD 风格恢复竞争能力；恢复后再回到低延迟模式。

## 3. 观测量

每个 RTT 或发送控制周期维护以下状态：

| 量 | 含义 |
| --- | --- |
| cwnd | 拥塞窗口，单位可以是包或字节，但同一实现必须保持一致 |
| RTT | 当前测量到的往返时延 |
| base_rtt | 当前路径观测到的最小 RTT，用于近似无排队传播时延 |
| throughput | 发送窗口除以 RTT 的吞吐估计；实现中应优先使用实际 ACK/交付字节校正 |
| diff | 估计本流在队列中等待的包数或等价字节量 |
| fast_thru | 短时间尺度吞吐 EMA |
| slow_thru | 长时间尺度吞吐 EMA |
| loss_event | 当前周期是否检测到丢包、ECN 或等价拥塞信号 |

文章中的简化公式为：

~~~
diff = cwnd * (1 - base_rtt / RTT)
~~~

它等价于比较无排队吞吐 cwnd / base_rtt 和当前吞吐 cwnd / RTT。如果 RTT 被排队显著拉长，diff 就会增大。

## 4. 状态机

### 4.1 Normal

Normal 是默认模式，采用 Vegas 风格的连续队列反馈。一个候选更新式为：

~~~
cwnd += gamma * (target - diff) / cwnd
~~~

候选初始参数：

~~~
target = 2       # 目标排队包数，仅作为初始实验值
gamma  = 0.5
~~~

实现时应根据单位、MTU、ACK 频率和 RTT 对参数重新标定，不能直接把包数参数用于字节窗口。

### 4.2 Aggressive

Aggressive 是暂时的竞争恢复模式，采用 AIMD 风格：

~~~
无丢包：cwnd += additive / cwnd
有丢包：cwnd *= multiplicative_retention
~~~

文章给出的候选值为：

~~~
additive                 = 1.5
multiplicative_retention = 0.75
~~~

这不是标准 Reno 的精确复刻。它只是一个“偏激进的 AIMD 候选参数集”，需要与 Reno、CUBIC、AQM、ECN 和多流场景分别比较。

### 4.3 Transition / 防抖逻辑

采用两个时间尺度的 EMA：

~~~
fast_thru = 0.9  * fast_thru + 0.1  * throughput
slow_thru = 0.99 * slow_thru + 0.01 * throughput
~~~

可把它理解为大约 10 个 RTT 和 100 个 RTT 的两个记忆窗口。

候选切换条件：

~~~
Normal -> Aggressive:
    slow_thru > min_throughput
    fast_thru < enter_ratio * slow_thru

Aggressive -> Normal:
    fast_thru > exit_ratio * slow_thru
    持续 exit_hold_rtts 个 RTT
~~~

文章中的初始值为：

~~~
min_throughput = 10       # pkt/s，仅适用于对应仿真单位
enter_ratio    = 0.5
exit_ratio     = 0.8
exit_hold_rtts = 20
~~~

建议在实现中把 Transition 表达为显式状态或显式计时器，并记录每次切换的原因，便于诊断误触发和振荡。

## 5. 候选伪代码

~~~
state:
    mode = NORMAL
    base_rtt = +infinity
    fast_thru = 0
    slow_thru = 0
    recovery_hold = 0

on each control interval:
    rtt = measured_rtt()
    base_rtt = min(base_rtt, rtt)
    throughput = delivered_bytes / interval_duration
    diff = cwnd * (1 - base_rtt / rtt)

    fast_thru = 0.9  * fast_thru + 0.1  * throughput
    slow_thru = 0.99 * slow_thru + 0.01 * throughput

    if mode == NORMAL:
        if slow_thru > min_throughput and fast_thru < 0.5 * slow_thru:
            mode = AGGRESSIVE
            recovery_hold = 0
    else:
        if fast_thru > 0.8 * slow_thru:
            recovery_hold += 1
            if recovery_hold >= 20:
                mode = NORMAL
                recovery_hold = 0
        else:
            recovery_hold = 0

    if mode == NORMAL:
        cwnd += gamma * (target - diff) / max(cwnd, 1)
    else if loss_event:
        cwnd *= 0.75
    else:
        cwnd += 1.5 / max(cwnd, 1)

    cwnd = max(cwnd, minimum_cwnd)
~~~

生产实现还必须处理 ACK 聚合、重复 ACK、超时、ECN、应用限速、连接空闲和窗口单位换算；上面的代码只是控制器轮廓。

## 6. 与远程队列流管理的映射

Direct Computing 具有不同优先级的流量：

| 流量 | 主要目标 | 建议优先级 |
| --- | --- | --- |
| 鼠标、键盘、终端输入 | 极低排队延迟 | 最高，独立控制流 |
| 终端输出、交互式响应 | 低延迟和可靠性 | 高 |
| 最新桌面帧 / 普通视频帧 | 新鲜度，允许过期丢弃 | 中高 |
| 关键帧、状态刷新、协议控制 | 可靠到达 | 高，但需限速 |
| 文件传输、批量同步 | 吞吐 | 低，可使用剩余容量 |

Vegas+ 应首先作为“连接级或媒体级发送预算控制器”评估，而不是让每个小消息各自运行一个独立拥塞控制器。否则多个子流可能同时进入 Aggressive，产生反馈放大和队列振荡。

初步建议：

1. QUIC/TLS 和底层连接拥塞控制继续负责安全与基础拥塞约束。
2. Vegas+ 作为应用层发送调度器，控制媒体、文件和可丢弃帧的额外预算。
3. 输入、终端控制和恢复请求使用独立可靠通道，不等待普通媒体重传。
4. 普通桌面帧过期时优先丢弃旧帧，不为了追赶历史帧持续扩大队列。
5. Aggressive 模式设置总预算、最大持续时间和冷却时间，避免恢复逻辑变成长期队列填充器。

## 7. 已知限制与风险

以下问题在完成实现和测试前不能视为已解决：

- base_rtt 只单调取最小值，路径变化或初始测量受污染时可能产生错误基准；
- cwnd / RTT 只是吞吐近似，应用限速、ACK 压缩和发送空闲时会误导检测；
- 固定的吞吐阈值和 EMA 比例不适合所有带宽、RTT 和报文单位；
- 多个 Vegas+ 流可能同时进入 Aggressive，导致周期性队列振荡；
- “被压制”也可能是无线波动、路由变化、服务器负载或应用行为，而非 AIMD 竞争；
- 文章中的公平性推导假设相同传播时延、固定瓶颈和简单队列，不能直接推广到异构 RTT、AQM、CUBIC、BBR 或 ECN；
- 连续时间稳定性分析不能替代带反馈延迟的离散系统稳定性验证；
- Aggressive 参数不是标准 Reno 参数，不能直接声称“兼容 Reno 公平性”；
- Aggressive 模式改善公平性的代价可能是显著增加 RTT，因此必须同时看延迟和公平性。

## 8. 验证计划

实现前应先建立可重复的网络模拟和日志指标，至少覆盖：

### 场景

- 单个 Vegas+ 流；
- Vegas+ 与 Reno、CUBIC、BBR 混合；
- 多个 Vegas+ 流同时启动或同时恢复；
- 相同和不同基础 RTT；
- DropTail、RED/CoDel/PIE、ECN；
- 0%～5% 随机丢包、突发丢包和无线抖动；
- 交互流与大文件流并发；
- 应用限速和长时间空闲后恢复。

### 指标

- Jain 公平性指数；
- 链路利用率和有效吞吐；
- RTT 的平均值、P95/P99 和排队延迟；
- 丢包率、ECN 标记率和重传字节；
- 输入到达延迟、终端回显延迟和画面新鲜度；
- 模式切换次数、持续时间和切换原因；
- 队列长度、发送缓存占用和过期帧比例；
- 是否出现多流同步切换、振荡或恢复风暴。

### 通过标准

Vegas+ 不能只以“吞吐更公平”作为成功标准。候选实现至少应同时证明：

1. 混合流场景下不会长期饿死交互流；
2. RTT 的 P95/P99 在可接受范围内；
3. 多流不会持续同步进入和退出 Aggressive；
4. 反馈、缓存和重传都有明确上限；
5. 断网、路径变化和错误基准 RTT 下可以回到安全模式；
6. 在不适合该算法的场景中可以禁用或降级为底层 QUIC 的标准行为。

## 9. 当前结论

Vegas+ 值得作为远程队列流管理的候选方案研究，尤其适合“平时低延迟、偶尔需要与强势吞吐流竞争”的应用层调度问题。

但当前应把它视为：

~~~
低延迟 Vegas 控制
    + 吞吐骤降检测
    + 有限时间 AIMD 恢复
    + 明确的延迟/缓存/切换上限
~~~

而不是已经证明可靠的通用拥塞控制协议。后续实现应先在用户态模拟器和受控网络中验证，再考虑接入远程桌面或文件传输的真实发送路径。
