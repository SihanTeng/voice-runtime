# 这次 barge-in 跑出来什么

`trace.jsonl` 保留初版 v1 默认配置下场景 C 的完整日志，735 条事件；它同时作为向后兼容读取的测试 fixture。当前新运行生成 v2 日志，新增阶段事件，但默认 A–E 时序不变。`metrics.json` 汇总 A–E。`playback-truth.json` 是第一轮被打断的账本。时序图在 `sequence.md`。

这是模拟 sink 的消费记录，不是扬声器或真人听感。`played.wav` 是 Fake TTS 方波，用来看截断和间隙。

## C 按时间发生了什么

| 事件 | 时间 | 说明 |
|---|---:|---|
| 第一轮说完 | 800ms | 完整句，走 240ms endpoint |
| 开始播放 | 1180ms | endpoint 之后只等了 LLM 80ms + TTS 60ms |
| 用户开口 | 2380ms | 回复已经播了约 1.2s |
| 检测到首帧 | 2400ms | +20ms |
| 确认打断并停播 | 2500ms | 再 +100ms；停播本身是 0ms，撤销和清队列在同一次 owner 转换里做完 |
| 两个旧 TTS chunk | 2500ms、2505ms | cancel 之后仍到达，记 stale，没入队、没播放 |
| 第二轮首音频 | 3360ms | 新 generation |
| 关闭 | 4760ms | `active_tasks = 0`，`trace_complete = true` |

开口到停播 120ms，双方声音重叠也是 120ms。120ms 里没有设备 stop ACK，也不是 WebRTC VAD hangover，就是「检测 20ms + 连续语音确认 100ms」。

## 用户实际听到了什么

第一轮 LLM 生成了整句：

> Certainly I can arrange your booking for Wednesday afternoon and send you all the details once everything has been confirmed.

入队到 “Wednesday afternoon”。sink 只消费了 21120 sample（1320ms）。完整听到的词停在 “booking ”；`for ` 只播了 1920 / 3200 sample（60%），不算已听到，不进下一轮上下文。

这就是 Playback Truth 要分开记的原因：generated ≠ enqueued ≠ heard。

## A–E 数字

| 场景 | Endpoint | 说完 → 首音频 | 开口 → 停播 | stale 收到 / 播放 |
|---|---:|---:|---:|---:|
| A 完整句 | 240ms | 380ms | — | 0 / 0 |
| B 停 700ms 再续 | 最终结束后 240ms | 380ms | — | 0 / 0 |
| C 播放中打断 | 每轮 240ms | 每轮 380ms | 120ms | 2 / 0 |
| D 80ms 噪声 | 240ms | 380ms | 没有正式打断 | 0 / 0 |
| E 取消后迟到包 | 每轮 240ms | 每轮 380ms | 120ms | 2 / 0 |

默认跑里音频在途顶到 8000 sample（500ms），背压确实发生了。各场景误打断为 0，stale 播放为 0。B 没有在 700ms 停顿处提前播放。

`examples/late.json` 把两个旧 chunk 拖到新回复开播之后，仍然不入队、不播放。真实时间 C 另有测试，断言 ≤250ms。

## 额外两份日志

- `slow-vad/`：VAD 每帧 30ms。B 仍然只产生一轮完整回复，等输入追平后在 3010ms endpoint，没有把排队时间当成静音。
- `input-faults/`：固定 seed 的 jitter / 丢帧 / 乱序。C 在 126ms 停播，旧包收到 2、播放 0。输入真值不完整，所以 overlap 和误打断是 null。

G.711 的 μ-law / A-law 文件是从实际消费 WAV 编出来的，末包只补静音到 20ms，不补被取消的后缀。这是最小采样率适配，不是电话线路质量证明。

## ASR 修订与一次性澄清

`tests/revisions.rs` 复现标点/识别改口、final 与 partial 不同、多 segment，以及低 stability 下的 700ms 暂停。短附和保护区分及时识别的 mm-hmm/嗯与明确命令；识别尚未返回且短语音已结束时，也按开口后 220ms 的 timer 回退，不无限等 ASR。

`recovery/trace.jsonl` 是 schema v2 的中途 TTS 超时完整日志，对应 `examples/recovery.json`。旧回复先结算听到的前缀，新 generation 播放一次“回复被切断，请确认”的澄清；对比同目录 `playback-truth.json` 可见旧回复未播后缀没有被重播。关闭任务为 0，独立 replay 无违规。before-audio 超时、fallback 自身失败、旧任务错误迟到、重复取消和关闭另有自动回归；fallback 仍是本地方波 Fake。

## 固定种子尾延迟实验

`latency-evaluation.json` 来自两组各 100 次的 A 场景，seed 7–106，虚拟时钟。默认配置与 `examples/tail-latency.json` 对照；后者给 LLM/TTS 注入相关慢包及 jitter，两组都无失败，stale played 均为 0。

| 指标 | 默认 p50 / p95 / p99 | 突发延迟 p50 / p95 / p99 |
|---|---|---|
| 说完→首音频 | 380 / 380 / 380ms | 392 / 493 / 577ms |
| endpoint→首音频 | 140 / 140 / 140ms | 152 / 253 / 337ms |
| LLM TTFT | 80 / 80 / 80ms | 86 / 188 / 191ms |
| TTS 首音频 | 60 / 60 / 60ms | 65 / 144 / 149ms |
| 回复内累计播放间隙 | 0 / 0 / 0ms | 59 / 192 / 209ms |

这是故障注入敏感性实验，不是性能优化前后对比。LLM/TTS 的 p99 不能相加代表端到端 p99；文本队列的最老项等待 p99 达到 3035ms，也不是首音频额外排队，因为回复已在流式播放。样本分位数只描述该配置；失败与缺失观测必须一起看，不能当作生产 SLO。

复现并保留每次完整日志：

```sh
cargo run --locked --release -- evaluate --runs 100 --output output/evaluation-baseline
cargo run --locked --release -- evaluate --runs 100 --config examples/tail-latency.json --output output/evaluation-tail
cargo run --locked --release -- run --scenario A --config examples/recovery.json --output output/recovery
cargo run --locked --release -- replay sample-output/recovery/trace.jsonl --output output/recovery-replay
```
