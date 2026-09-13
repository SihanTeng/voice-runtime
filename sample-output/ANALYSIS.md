# 本次运行分析

`trace.jsonl` 是默认配置下 C 场景的完整记录，735 个事件；`metrics.json` 汇总 A–E。`manifest.json` 保存生成配置，`completion-validation.json` 记录核心补齐阶段的工作区版本和产物哈希，`dev-validation.json` 记录随后开发入口与信号退出的验证；这些历史记录中的路径和哈希对应当时版本，旧材料可从 Git 历史查阅，不表示仍存在于当前工作区。两次 release 运行的 A–E trace 均逐字节相同；`sequence.md` 可直接在支持 Mermaid 的 Markdown 阅读器中查看。

| 事件序号 | 相对时间 | 发生了什么 |
|---:|---:|---|
| 106 | 1040ms | 第一轮语音在 800ms 结束，完整 partial 对应 240ms endpoint 阈值 |
| 122 | 1180ms | 开始播放；endpoint 后只等待 LLM 80ms + TTS 60ms |
| 399 | 2500ms | 用户于 2380ms 开口，2400ms 检测首帧，2500ms 达到连续语音确认条件 |
| 401 | 2500ms | 旧 generation 已停播并取消，未播放队列清空 |
| 403、406 | 2500、2505ms | 两个取消后的旧 TTS chunk 到达，均记录 stale drop |
| 494、510 | 3220、3360ms | 第二轮 endpoint 提交及新回复首音频 |
| 734 | 4760ms | 新回复完整播放后关闭，所有受监督任务已回收 |

第一轮完整生成了 “Certainly I can arrange your booking for Wednesday afternoon and send you all the details once everything has been confirmed.”，入队文本截至 “Wednesday afternoon”。实际消费仅 21120 sample（1320ms），完整听到的词截至 “booking ”；`for ` 的文本字节范围是 `[37,41)`，仅消费对应 3200 sample 的 1920（60%），因此不作为完整词加入下一轮上下文。`playback-truth.json` 保存逐 chunk 证据；`played.wav` 从这些消费记录导出，保留间隙、排除取消后缀，内容为 Fake TTS 方波。

检测/确认/停播三段延迟为 20/100/0ms，总计 120ms；输入语音真值与消费区间求交也得到 120ms 重叠。音频在途上限实际触及 8000 sample（500ms），说明测试确实施加了背压。各场景 stale played 为 0，D 的噪声没有正式取消，B 的 700ms 停顿没有提前 endpoint；最终句结束后的 endpoint 仍为 240ms。

另外，`examples/late.json` 与自动化测试把两个旧 chunk 延迟到新回复开始之后，仍验证不入队、不播放。真实时间 C 测试独立断言 ≤250ms；真实 VAD 使用固定 WAV 验证语音检测和恢复静音，缺少人工真值的 WAV 指标明确返回 null。样例为模拟播放器的消费真值，不代表硬件扬声器或真人感知测量。

本轮 endpoint 事件增加 VAD/输入进度与已观察静音样本证据，消费时长和默认指标不变。旧 generation 的两个迟到 chunk 现在也进入账本，明确为未入队、零播放量和 `stale_generation`；`synthesized_samples` 与实际播放量分别汇总。`played.mulaw` 与 `played.alaw` 编码自实际消费 WAV，并保留静音间隙；只对文件末包补静音到 20ms，不补播取消的回复后缀。

额外验收见 `bonus-metrics.json`：30ms/帧慢 VAD 下 B 只产生一轮完整回复，等待输入追平后于 3010ms endpoint，没有抢话；固定 seed 的输入 jitter/丢帧/乱序下 C 在 126ms 停播，旧包收到 2、播放 0，关闭后任务为 0。对应完整日志及配置分别保存在 `slow-vad/`、`input-faults/`，可独立 replay。有丢失输入时 overlap/误打断真值不完整，明确返回 null。两种 G.711 编码的真实语音文件均经 WebRTC VAD 跑通，使用脚本 ASR/LLM 和 Fake TTS。

`playback_stop_ms = 0` 表示 owner 同步撤销和结算，不包括设备 stop ACK；120ms 是连续语音确认，不是 WebRTC VAD hangover。两种电话格式使用最小采样率转换，未验证真实电话线路或生产级声学质量。
