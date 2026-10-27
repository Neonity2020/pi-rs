# 快速上手指南

## 构建要求

- Rust 工具链：Edition 2024 支持（建议 Rust 1.85+ 或 nightly）
- 平台支持：macOS (Apple Silicon / Intel)、Linux (x86_64 / aarch64)

```bash
cargo build --release
```

编译产物位于 `target/release/pi-rs`。

## 运行模式

### 1. 交互模式 (Interactive TUI)

默认启动进入 ratatui 终端交互界面：

```bash
./target/release/pi-rs
```

常用快捷键：
- `Enter`：发送当前编辑内容。
- `\` + `Enter`：换行续写。
- `Esc`：中止正在执行的模型生成或工具调用。
- `Ctrl+C`：安全退出。
- `Ctrl+O`：展开 / 折叠选中的工具完整执行输出。
- `Ctrl+T`：折叠 / 展开模型思考过程 (Thinking)。
- `PgUp` / `PgDn` / 鼠标滚轮：上下翻动历史消息。
- `/model` 命令：唤起模糊匹配浮层，快速切换模型。
- 行内编辑：支持标准 readline 词导航（`Ctrl+←/→`、`Alt+B/F`、`Ctrl+W` 删除词、`Ctrl+U` 清行）。

### 2. Print 模式 (非交互命令行)

单次指令执行并直接在控制台输出 Markdown 格式结果后退出：

```bash
./target/release/pi-rs -p "查看当前目录下修改了哪些文件"
```

指定模型运行：

```bash
./target/release/pi-rs -p "hello" -m openai/gpt-5.2
```

### 3. RPC 模式

通过管道进行 JSON-RPC 帧通信（当前处理帧流，命令执行骨架就位）：

```bash
./target/release/pi-rs --rpc
```

### 4. Mock 模式演练

无网络或未配置 API Key 时，默认支持 mock provider：
- 发送普通文字：模拟打字回显。
- 消息中包含 "list" 或 "ls"：触发模拟 `ls` 工具调用、结果回传与第二轮总结，用于检验 agent loop 完整性。
