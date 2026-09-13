# 固定音频夹具

`speech16.wav` 原样复制自 crates.io `webrtc-vad 0.4.0` 中的
`resources/libfvad/tests/data/audio_tiny16.wav`，上游为
[libfvad 测试数据](https://github.com/dpirch/libfvad/tree/master/tests/data)。
格式：单声道 PCM16 / 16kHz / 5.4 秒。

SHA-256：`551d3f0282c5d5e1e32e76da97bd3532c38e6f4bb02a32510caf562d7c06ae9c`。
保留上游 BSD 许可于 `LICENSE.libfvad`。本项目不修改或重新声称音频版权。

`wav-script.json` 仅用于证明真实 VAD 可以接入 Runtime；里面的文字是人为编写的
Fake ASR/LLM 测试数据，不是对 WAV 内容的识别结果。所有验收测试无需网络下载或麦克风。
