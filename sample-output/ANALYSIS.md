# 本次运行分析

`trace.jsonl` 是默认配置下 C 场景的完整记录，733 个事件；`metrics.json` 汇总 A–E。`manifest.json` 保存生成配置，`validation.json` 记录源码 commit、实际检查结果和执行时间。两次 release 运行的 A–E trace 均逐字节相同；`sequence.md` 可直接在支持 Mermaid 的 Markdown 阅读器中查看。

| 事件序号 | 相对时间 | 发生了什么 |
|---:|---:|---|
| 105 | 1040ms | 第一轮语音在 800ms 结束，完整 partial 对应 240ms endpoint 阈值 |
| 121 | 1180ms | 开始播放；endpoint 后只等待 LLM 80ms + TTS 60ms |
| 398 | 2500ms | 用户于 2380ms 开口，2400ms 检测首帧，2500ms 达到连续语音确认条件 |
| 400 | 2500ms | 旧 generation 已停播并取消，未播放队列清空 |
| 402、405 | 2500、2505ms | 两个取消后的旧 TTS chunk 到达，均记录 stale drop |
| 492、508 | 3220、3360ms | 第二轮 endpoint 提交及新回复首音频 |
| 732 | 4760ms | 新回复完整播放后关闭，所有受监督任务已回收 |

第一轮完整生成了 “Certainly I can arrange your booking for Wednesday afternoon and send you all the details once everything has been confirmed.”，入队文本截至 “Wednesday afternoon”。实际消费仅 21120 sample（1320ms），完整听到的词截至 “booking ”；`for ` 的文本字节范围是 `[37,41)`，仅消费对应 3200 sample 的 1920（60%），因此不作为完整词加入下一轮上下文。`playback-truth.json` 保存逐 chunk 证据；`played.wav` 从这些消费记录导出，保留间隙、排除取消后缀，内容为 Fake TTS 方波。

检测/确认/停播三段延迟为 20/100/0ms，总计 120ms；输入语音真值与消费区间求交也得到 120ms 重叠。音频在途上限实际触及 8000 sample（500ms），说明测试确实施加了背压。各场景 stale played 为 0，D 的噪声没有正式取消，B 的 700ms 停顿没有提前 endpoint；最终句结束后的 endpoint 仍为 240ms。

另外，`examples/late.json` 与自动化测试把两个旧 chunk 延迟到新回复开始之后，仍验证不入队、不播放。真实时间 C 测试独立断言 ≤250ms；真实 VAD 使用固定 WAV 验证语音检测和恢复静音，缺少人工真值的 WAV 指标明确返回 null。样例为模拟播放器的消费真值，不代表硬件扬声器或真人感知测量。

评审修订补充了非词表省略号的犹豫测试、配置推导的首包预算与真实 UTF-8 非法范围测试，并拆分 session 内部职责。重构前后 A–E 的六类产物全部逐字节相同，因此这里的数值及事件序号保持有效；比较范围、哈希和新增门禁结果见 `review-validation.json`。`playback_stop_ms = 0` 仅表示 owner 同步撤销和结算，不包括实际设备 stop ACK；120ms 的输入确认是连续语音防抖，不是 WebRTC VAD 的尾部 hangover。
