# 环境安装与面试验收

所有项目命令均在包含 `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` 的根目录执行。接受完整 Git 仓库或源码压缩包；压缩包应保留隐藏目录 `.githooks/`，完整门禁的 hook 自检会读取它。仅运行或测试不要求配置 Git 用户名、安装项目 hook、申请模型账号或创建 `.env`。

```sh
git clone https://github.com/SihanTeng/voice-runtime.git
cd voice-runtime
```

## 1. 安装系统工具

已有 Python 3.9+ 时，优先使用项目入口：

```sh
./scripts/dev.sh doctor
./scripts/dev.sh setup
./scripts/dev.sh demo
./scripts/dev.sh                 # 保存修改后自动重编译、重启；Ctrl+C 优雅退出
```

脚本可从其他目录通过绝对路径调用；构建始终在仓库根目录运行，显式 `--config`/`--output` 相对路径按调用者目录解析。无需安装 cargo-watch、Node、pip 包或文件监听服务。Ubuntu/Debian、Fedora 和 Arch 分别提供 apt-get、dnf、pacman 安装命令；`setup --install-system` 才会执行系统安装，普通 setup 只负责固定 Rust 与依赖。macOS 的 Command Line Tools 可能需要等待系统安装窗口完成，然后重跑 setup；没有 Python 时 launcher 会输出安装指引。原生 Windows 请使用 WSL2 的 Linux 终端。

每次运行的日志放在 `output/dev/run-*/`，不会覆盖此前热重载证据。信号退出及重启会先等待当前 session 关闭和日志导出；开发模式没有热替换正在运行的 Rust 函数，也不跨重启保留上下文。

本地验证平台是 macOS Apple Silicon；Linux/macOS 托管门禁的结果与产物见 [GitHub Actions](https://github.com/SihanTeng/voice-runtime/actions/workflows/ci.yml)。CI 验证不等于全新系统安装流程实测，原生 Windows 的 shell/hook 流程未验证，可在已有 WSL Ubuntu 中按 Linux 步骤操作。

**macOS：** 安装 Apple Command Line Tools，提供编译器、链接器和 Git；若已安装可跳过。

```sh
xcode-select --install
```

等待系统安装窗口完成，再检查：

```sh
git --version
cc --version
python3 --version
```

仅构建、运行和 Rust 测试不需要 Python。完整 `scripts/check.sh` 的 hook 自检需要 Python ≥3.9；若没有可用的 `python3`，从 [Python 官方 macOS 下载页](https://www.python.org/downloads/macos/) 安装后重新打开终端。

**Ubuntu/Debian（apt）：** 系统编译/链接工具也供可选的 `webrtc-vad` C 代码使用。

```sh
sudo apt-get update
sudo apt-get install -y build-essential curl ca-certificates git python3
python3 --version
```

`python3` 须为 3.9 或更新版本。使用较老发行版时先升级 Python；无需 pip、虚拟环境或第三方 Python 包。

## 2. 安装固定 Rust 工具链

已安装 rustup 的机器直接跳到下一段。否则使用 [Rust 官方安装方式](https://rust-lang.org/tools/install/)；这里只安装 rustup，不额外安装随时间变化的默认 stable：

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
```

进入拿到的项目根目录后安装固定版本；不需要修改全局默认工具链：

```sh
rustup toolchain install 1.96.1 --profile minimal --component rustfmt --component clippy
rustup show active-toolchain
rustc --version
cargo --version
```

`rustc --version` 应包含 `1.96.1`。仓库的 [`rust-toolchain.toml`](../rust-toolchain.toml) 声明版本与组件，rustup 的[目录工具链规则](https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file)会在此目录选择它；`Cargo.lock` 固定依赖，后续始终使用 `--locked`。首次安装/编译需要能访问 Rust 分发服务器和 crates.io；这是工具链与依赖下载，不是运行时调用模型服务。

## 3. 按顺序验收

最短流程与 [README](../README.md#面试官快速验收) 相同：

```sh
cargo test --locked
cargo run --locked --release -- run --scenario all --output output
cargo run --locked --release -- replay output/C/trace.jsonl --output output/replay
```

测试应全部成功，默认共 34 项（Cargo 分测试二进制分别输出结果）；32 个属性案例和 8 个并发 session 已包含在这些测试内部。默认使用虚拟时间，完整句 A 的 endpoint 延迟是 240ms，C 的开口→模拟停播是 120ms，所有场景 stale played 为 0。终端命令都应返回 0；可以紧接命令运行 `echo $?` 查看。

| 产物 | 阅读用途 |
|---|---|
| `output/metrics.json` | A–E 指标汇总 |
| `output/C/trace.jsonl` | 按顺序解释打断、清队列、迟到丢弃、下一轮和关闭 |
| `output/C/playback-truth.json` | 对比生成、入队、完整听到文本和部分词 |
| `output/C/lifecycle.json` | 确认 `active_tasks: 0`、`trace_complete: true`、`violations: []` |
| `output/C/sequence.mmd` | Mermaid 时序图；运行或验收不依赖 Mermaid 安装 |
| `output/replay/audit.json` | 从 trace 独立重建的指标在 `metrics` 中，违规列表在 `violations` 中；同目录另含账本与 WAV |

`played.wav` 是确定性 Fake TTS 方波，不是可懂的合成语音。CLI 不连接硬件播放器；没有声音不是安装失败。重复执行会覆盖所指定输出目录中的同名产物，需要保留结果时换一个 `--output` 目录。

完整质量门禁及真实 VAD：

```sh
sh scripts/check.sh
cargo run --locked --release --features real-vad -- wav tests/fixtures/speech16.wav \
  --script tests/fixtures/wav-script.json --output output/wav
```

门禁依次检查 rustfmt、Clippy warnings-as-errors、默认 34 项/全部 features 35 项 Rust 测试、release 构建、隔离 Git 仓库中的 hook 自检及 2 项开发脚本测试；自检忽略系统/全局 Git 配置，不依赖个人签名密钥，不会安装本项目 hook，也不会修改项目暂存内容。开发脚本 fixture 使用含空格路径，真实启动子进程，验证重启前回收、编译失败恢复和 Ctrl+C。WAV 与脚本随项目提供；ASR/LLM/TTS 仍是脚本。首次编译耗时取决于机器与下载速度，README 的 1–2 秒只指编译后的虚拟场景。

## 4. 常见问题

| 现象 | 处理 |
|---|---|
| `cargo` / `rustup: command not found` | 重新打开终端，或执行 `. "$HOME/.cargo/env"`，检查 `~/.cargo/bin` 在 PATH 中。 |
| 工具链不是 1.96.1，或报告不支持 edition 2024 | 确认在项目根目录并运行 `rustup show`；检查是否有 `RUSTUP_TOOLCHAIN` 环境变量、目录 override 或命令行 `+stable` 覆盖了仓库配置，不要删除版本锁定文件。 |
| `linker cc not found` / C 编译失败 | 完成上面的 Command Line Tools 或 `build-essential` 安装；在 macOS 可用 `xcode-select -p` 确认开发工具路径。 |
| 下载超时、证书错误、找不到 crate | 检查到 Rust 分发服务器和 crates.io 的网络/代理与系统 CA；保留锁文件。依赖预先缓存后可用 `cargo test --locked --offline`，空缓存无法离线首次编译。 |
| `python3` 缺失或版本过低 | 安装 Python ≥3.9 后重跑完整门禁；Rust 核心测试可先用 `cargo test --locked`。 |
| WAV 提示缺少 real-vad | 使用上述带 `--features real-vad` 的 Cargo 命令，避免误运行之前默认 features 编译的二进制。 |
| `could not find Cargo.toml` / fixture 文件不存在 | 切换到完整源码根目录；确认 `tests/fixtures/` 随源码交付。 |
| `not a git repository`（安装 hook 时） | 源码压缩包无需安装 hook，直接运行/测试；仅在准备提交的 Git checkout 中运行 `scripts/install-hooks.sh`。 |
| `examples/timeout.json` 返回非零 | 这是故意注入 Provider 超时的预期结果；查看其输出 trace，普通 A–E 应返回 0。 |

无需权限或安装条件时，可以直接阅读已提交的 [样例分析](../sample-output/ANALYSIS.md)、[设计说明](../DESIGN.md)和 [README 的快速验收步骤](../README.md#面试官快速验收)。不应将模拟消费的停播时间当作真实设备或声学测量。
