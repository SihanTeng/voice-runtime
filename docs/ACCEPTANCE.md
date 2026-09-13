# 面试验收索引

本页将题目要求映射到可检查的代码、行为断言和运行产物。按题目允许的确定性 Fake Provider 与模拟 Playback 范围，核心要求及七类加分项均有证据；这是一份验收导航，不是面试官的最终评分或生产质量保证。

## 最短检查路径

不安装环境也能先看 [C 场景分析](../sample-output/ANALYSIS.md)、[A–E 指标](../sample-output/metrics.json)、[播放真值](../sample-output/playback-truth.json) 和 [时序图](../sample-output/sequence.md)。[GitHub Actions](https://github.com/SihanTeng/voice-runtime/actions/workflows/ci.yml) 的 Linux/macOS 作业运行完整门禁，并上传二进制和场景证据；请选择所审阅提交的成功运行。

本机需要 Git、C 编译器、Python 3.9+ 和 POSIX shell；macOS/Linux/WSL2 的准备步骤见 [SETUP](SETUP.md)。在仓库根目录执行：

```sh
./scripts/dev.sh setup                  # 准备固定 Rust 工具链与锁定依赖
./scripts/dev.sh test                   # 格式、Clippy、全部测试、release 构建、脚本自检
./scripts/dev.sh demo all --virtual     # 显示 A–E 指标与产物目录
```

需要固定输出路径和独立日志回放时，继续执行（首次门禁已生成下面的二进制）：

```sh
./target/release/voice-runtime run --scenario all --output output/review
./target/release/voice-runtime replay output/review/C/trace.jsonl --output output/review-replay
```

上述命令应返回 0；`output/review/C/lifecycle.json` 应包含 `active_tasks: 0`、`trace_complete: true`、`violations: []`。回放的 `audit.json` 应无违规，`metrics` 与原 C 的 `metrics.json` 一致。默认 Rust 测试共 34 项，全部 features 共 35 项（包含真实 VAD），另有 2 项 Python 开发脚本测试和 hook fixture 自检。

仅体验约 5 秒的真实时间打断用 `./scripts/dev.sh demo`；开发用 `./scripts/dev.sh`，保存修改后先关闭旧 session，再构建并启动新 session，Ctrl+C 会等待关闭和日志导出。Fake TTS 是方波，CLI 没有硬件音频输出；验收通过日志和实际模拟消费量进行。

2026-09-13 从公开仓库重新克隆提交 `9d057c7` 后，本地 macOS arm64 的 doctor、首次项目构建、虚拟时间 A–E 和独立 C 回放均通过；A–E 指标与已提交样例一致，运行后受跟踪文件无修改。新克隆不含构建产物，构建约 7.55 秒、A–E 运行 1.307 秒；本机已有 Rust 和依赖下载缓存，因此这不是全新系统安装耗时。该提交的 [Linux/macOS 托管完整门禁](https://github.com/SihanTeng/voice-runtime/actions/runs/34757247746) 也全部通过。

## 核心评分维度

| 评分维度 | 实现与说明 | 主要行为证据 |
|---|---|---|
| 状态模型与 Playback Truth · 25% | [Playback](../src/playback.rs) 区分生成、合成、入队、消费；记录范围、sample、截断和拒绝；[generation](../src/session/generation.rs) 只取已听到文本作为上下文 | [playback tests](../tests/playback.rs)：词中截断、UTF-8 边界、重复包；[scenarios tests](../tests/scenarios.rs)：下一轮不包含未播放后缀 |
| 并发、取消、迟到事件 · 25% | [生命周期 owner](../src/session/lifecycle.rs)、[transport](../src/transport.rs)、[有界队列](../src/queue.rs)；先撤销/停播/清队列，再通知取消，关闭后 join | [faults tests](../tests/faults.rs)：超时、panic、断开、主动关闭、sink/journal 故障、并发取消、关闭后 sink 写入探针；[scenarios tests](../tests/scenarios.rs)：旧包晚于新回复仍被丢弃 |
| 测试、可观测性与指标 · 20% | [事件 schema](../src/event.rs) 包含全部要求字段；[audit](../src/audit.rs) 按 trace 重建账本和指标；[样例](../sample-output/ANALYSIS.md) 逐事件解释结果 | [replay tests](../tests/replay.rs)：篡改日志可检测 stale 播放、缺事件、过量消费；[CLI tests](../tests/cli.rs)：在线/回放指标一致、故障非零退出仍保留证据 |
| Endpointing / Barge-in · 15% | [endpoint](../src/endpoint.rs) 使用 240/600/1000ms 动态静音阈值；[input](../src/session/input.rs) 检查 partial、ASR/VAD 进度和已观察静音；打断独立使用连续语音确认 | [scenarios tests](../tests/scenarios.rs)：A–E；[endpointing tests](../tests/endpointing.rs)：非关键词中英文犹豫及 VAD 积压；[CLI tests](../tests/cli.rs)：真实时钟停播 ≤250ms |
| 结构、可读性、资源管理 · 10% | 单 owner，私有模块按责任拆分，Clock/Provider/Sink 可替换；[配置](../src/session/config.rs) 校验队列、文本、任务、日志和 session 总量上限 | [完整门禁](../scripts/check.sh) 与 [双平台 CI](../.github/workflows/ci.yml)；取消路径没有独立失管播放任务 |
| 范围控制与沟通 · 5% | [README](../README.md) 提供环境、命令、架构、实际耗时、完成/未完成项与作者确认的 AI 声明；[DESIGN](../DESIGN.md) 解释状态、取舍、六问和三个生产风险 | 本页提供题目到证据的入口；运行不依赖真人、API key、模型或语音 Agent 编排框架 |

四条底线分别由 generation fencing、sink 消费账本、session 关闭/join、可注入时钟和行为测试落实。`stale_chunk_played_count == 0` 不只是默认值：回放测试会注入非法播放，验证计数和违规检测能发现它。

## A–E 的验收结果

以下为默认配置的虚拟时间结果；每项都在 [scenarios tests](../tests/scenarios.rs) 中有行为断言。

| 场景 | 应检查的行为 | 基准证据 |
|---|---|---|
| A | 完整句 endpoint 后流式启动，无等待整句合成的额外队列 | endpoint 240ms，语音结束→首音频 380ms；测试另覆盖四组 LLM/TTS 首包延迟 |
| B | 700ms 犹豫期间不 endpoint、不播放，续说结束后快速提交 | 最终结束后 240ms；外部验收预算 ≤400ms；慢 VAD 不把积压时间当静音 |
| C | 回复开始 1.2s 后用户开口，停播、清队列、取消旧轮并完成新轮 | 总停播延迟 120ms = 检测 20 + 确认 100 + owner 停播 0；消费区间与输入真值重叠 120ms |
| D | 80ms 高能噪声撤销语音候选，不正式取消回复 | 原 4 秒回复消费完整、误打断 0 |
| E | soft cancel 后两个旧包不能播放；即使新回复已开始也有效隔离 | stale 收到 2、播放 0；拒绝 chunk 在账本中标为未入队、零消费；关闭后无 sink 写入 |

各场景输出全部队列的 `capacity` 和 `peak`；断言要求容量大于 0 且 `peak <= capacity`。默认 TTS 在途音频达到 8000 sample 上限，确实触发背压，并非只声明容量而没有施压。

## 七类加分项

| 题目加分项 | 当前实现 | 证据入口 |
|---|---|---|
| 可注入虚拟时钟 | `Clock` 注入全部调度，Tokio paused time 可重复执行 | [clock](../src/clock.rs)、[scenarios tests](../tests/scenarios.rs) |
| 真实 WAV / 麦克风输入 | 真实 PCM16/16kHz WAV 输入与尾帧处理；采用 WAV 路径 | [wav](../src/wav.rs)、[夹具及许可](../tests/fixtures/README.md)、[wav tests](../tests/wav.rs) |
| 真实 VAD 或离线 ASR | 可选 WebRTC VAD 处理固定真实语音文件 | `cargo test --locked --features real-vad --test wav`；ASR 仍为脚本，不声称真实识别 |
| jitter / 丢帧 / 乱序 | 固定 seed Provider jitter 与有界输入故障器 | [impairment tests](../tests/impairment.rs)、[配置](../examples/input-faults.json)、[完整 trace](../sample-output/input-faults/trace.jsonl) |
| 属性测试或并发压测 | 32 个固定种子属性案例、8 个并发 session 及取消边界测试 | [faults tests](../tests/faults.rs)；验证任务交错，不声称多线程共享内存压测 |
| 8kHz G.711 电话适配 | PCMU/PCMA 双向转换，160 字节/20ms 包适配 Runtime | [g711](../src/g711.rs)、[g711 tests](../tests/g711.rs)：两种 law 的全部 65536 编码输入与 256 解码码字参考向量，以及 CLI 集成 |
| 从日志生成时序图 | run/replay 从事件生成 Mermaid 图，GitHub 可直接查看 | [sequence.md](../sample-output/sequence.md)、[audit](../src/audit.rs)、[replay tests](../tests/replay.rs) |

这里按题目中的“WAV/麦克风”“VAD 或离线 ASR”“属性测试或并发压测”为可选实现路径计覆盖。没有实现麦克风或真实 ASR；若评分者另行要求这些具体组件，需要按其附加标准评估，不能把替代路径描述成两者都实现。

## 面试时应说明的边界

- Playback Truth 是模拟 sink 的实际消费结果；`playback_stop_ms = 0` 表示同一个 owner 转换内完成，没有测量声卡缓冲、扬声器停声或真人感知。
- Endpoint 是可解释的简单规则。无标点、无不完整信号的未知文本可能使用 600ms fallback；慢于实时的 VAD 能避免积压引发提前 endpoint，但不能保证任意 Provider 配置仍满足 250ms。
- G.711 是文件/帧边界适配，采用简单采样率转换，不包含 RTP/SIP、抗混叠重采样和真实电话线路测量；缺少声学真值时 overlap/误打断返回 null。
- journal 有界驻留内存，受控关闭后写出；崩溃持久化恢复和不可 yield 的第三方阻塞代码隔离不在范围内。原生 Windows 未验证，使用 WSL2；双平台 CI 不等于全新系统安装实测。

这些限制已在设计中解释，不替代四条核心底线。是否获得满分仍取决于评分者对代码、测试充分性和现场讲解的判断。
