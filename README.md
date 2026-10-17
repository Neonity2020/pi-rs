# pi-rs

一个用 Rust 重写的极简 coding agent，遵循 [Pi](https://pi.dev/) 的哲学：

> 如果我不需要它，它就不会被构建。

四把工具（`read` / `write` / `edit` / `bash`）、一段几百 token 的系统提示、一个只做一件事的 agent 循环、纯文本的会话记录。默认接 DeepSeek（`deepseek-flash`），任何 OpenAI 兼容端点都能替换。

## 层次

| 层级 | Crate | 职责 | 依赖 |
|---|---|---|---|
| 应用层 | `crates/coding` | CLI、TUI、会话 JSONL、系统提示、四个内建工具 | `agent`、`tui`、`llm` |
| 循环层 | `crates/agent` | 模型请求 → 工具执行 → 事件流（不知道终端和文件格式） | `llm` |
| 提供层 | `crates/llm` | 流式聊天、SSE 解析、DeepSeek / OpenAI 兼容、模型与价格 | — |
| 渲染层 | `crates/tui` | 终端 UI 状态与渲染（不碰 I/O） | — |

依赖方向严格单向：`coding → agent → llm`，`tui` 是叶子。每层可以单独使用——`llm` 是完整的流式客户端，`agent` 是完整的 agent 循环。

## 快速开始

```bash
cargo install --path crates/coding      # 或：cargo run -p pi-coding
pi                                      # 打开 TUI，输入 /login
```

`/login` 会提示粘贴 DeepSeek API key（输入不回显），先用 `GET /models` 验证，再把凭据写进 `~/.pi-rs/auth.json`（权限 0600）。验证失败不会保存。终端里也可以直接：

```bash
pi login                    # 隐藏输入；也接受 --api-key 或管道
pi logout                   # 删除保存的凭据
```

凭据优先级：`--api-key` > 环境变量 / `.env` > `auth.json`。所以 CI 仍可注入 `DEEPSEEK_API_KEY`，不需要改用户的 auth 文件。

不想用登录的话，把 `DEEPSEEK_API_KEY=sk-...` 写进 `.env` 同样有效（模板：仓库里的 `.env.example`，`cp .env.example .env`）：启动时会从工作目录或最近上级目录加载，真实环境变量优先。

```
pi                          # 交互式 TUI
pi "解释这个仓库的结构"        # 单次执行，流式输出到 stdout
pi -p < prompt.txt          # 从 stdin 读 prompt
pi --json -p "..."          # 事件流以 JSONL 输出，可脚本化
pi -c                       # 继续本目录最近一次会话
```

常用参数：

```
--model <id>          模型（默认 deepseek-flash，或 deepseek-v4-pro）
--provider <name>     deepseek | openai
--base-url <url>      任意 OpenAI 兼容端点
--no-thinking         关闭 DeepSeek 思考模式
--effort <level>      推理强度 low | high | max
--tools <list>        限制工具集，如 --tools read,bash
```

TUI 内的斜杠命令：`/login` 登录（输入框切换为掩码）、`/logout` 删除保存的凭据、`/help` 列出命令。没有凭据时 TUI 仍会启动，普通提问会被拦下并提示先 `/login`。

`Up` / `Down` 在输入框里翻阅历史 prompt（跨会话，按工作目录保存于 `~/.pi-rs/history/<cwd 的 slug>`）；正在编辑的内容会先暂存，翻回最新一条后还原。

`Ctrl+V` 从剪贴板读一张图（macOS 用系统 `osascript`，Linux 用 `wl-paste` / `xclip`），显示在输入框下方；`Enter` 连同文字一起发送，`Esc` 删除最后一张。图片以 base64 存进会话，发送时按 OpenAI 多模态 `content` parts 展开，所以需要模型/端点支持图片输入；超过 10 MB 会被拒绝。

模型的推理过程默认折叠成一行 `Thinking`；`Ctrl+T` 展开或收起全部推理内容。

## 设计决定（对 Pi 的忠实翻译）

**极短系统提示。** 基础提示约 300 token：模型已经知道什么是 coding agent，长篇说明只会在注意力里和你的代码竞争。个性化全部走 `AGENTS.md`（全局 `~/.pi-rs/AGENTS.md`，然后从文件系统根目录逐级合并到 cwd，深者优先），或用 `SYSTEM.md` 整体替换基础提示。

**只有四个工具。** 搜索、构建、测试、git 全部走 `bash`。

| 工具 | 作用 | 失败语义 |
|---|---|---|
| `read` | 读取文件（带行号） | `ToolResult::is_error`，不回滚循环 |
| `write` | 创建或覆盖文件 | `ToolResult::is_error`，不回滚循环 |
| `edit` | 精确字符串替换 | `ToolResult::is_error`，不回滚循环 |
| `bash` | 执行 shell 命令 | `ToolResult::is_error`，不回滚循环 |

工具失败不是异常，而是 fed back 给模型的结果文本（`is_error: true`）；参数 JSON 解析失败、未知工具名、panic 也一样——循环永不为工具而中断。

**没有 max steps。** 循环到模型不再调用工具为止，或者被 `Ctrl+C`（取消令牌贯穿 HTTP 请求与工具进程）、或者失败。没有"步数上限"这种旋钮，因为从没用过。

**YOLO 默认。** 没有权限弹窗。如果 agent 能写代码就能跑代码，确认对话框只是安全剧场。真正的安全边界是容器/沙箱，属于你的环境，不属于 harness。

**有意不做的事：** plan mode、sub-agent、MCP、内置 todo、后台 bash。计划写 `PLAN.md`，任务写 `TODO.md`——文件可以被 git、被 grep、跨会话存活；隐藏状态不行。需要子代理就开第二个 `pi`。这些"拒绝"不是缺口，是产品决定：核心越小，你能看懂的部分越多。

**扩展点是 Rust trait。** `llm::Provider` 和 `agent::Tool` 就是全部插件接口。没有动态加载，没有热重载；加一个后端 = 实现一个 trait，加一个工具 = 实现一个 trait。

## 文档

维护者视角的文档在 [`docs/`](docs/README.md)：

- [目标](docs/goals.md) — 要什么、不要什么、成功标准
- [架构](docs/architecture.md) — 分层、数据流、不变量、失败模型
- [计划](docs/plan.md) — 里程碑、候选池、开放问题
- [开发指南](docs/development.md) — 测试、扩展工具/provider、提交检查

## 会话

每次会话一个 JSONL 文件：`~/.pi-rs/sessions/<cwd 的 slug>/<时间戳>.jsonl`。第一行是 header，之后每行一条消息记录，带 `id` + `parent` —— 文件是棵树而不是日志，目前读取时取活动分支（最后一条回溯到根）。assistant 的 `reasoning` 一并落盘，因为带 `tools` 的 DeepSeek 请求要求把历史 `reasoning_content` 原样带回（否则 API 返回 400），消息格式与线上格式共享同一个 `Message`；图片在发送时由 provider 展开为 content parts，会话里仍存 base64 的同一个 `Message`，所以不需要新的消息类型。

## 成本

状态栏会显示输入/输出 token、缓存命中率与估算成本；`-p` 结尾显示 token 数与成本。DeepSeek 的定价分高峰/低谷（工作日 01:00–04:00 与 06:00–10:00 UTC），估算按 UTC 时间自动选择，但不考虑中国法定节假日，所以看成量级即可。`--provider openai` 或未知模型时不估成本，显示 `-`。

## 测试

```bash
cargo test          # 137 个测试，不需要 API key
```

覆盖：SSE 分块解析、工具 schema 与线格式（含 `reasoning_content` 回传与图片 content parts）、agent 循环（工具回环、并行批次顺序、中止、空流）、四个工具的真实文件/进程行为、会话读写与分支回溯、arg 解析、剪贴板图片的格式识别/尺寸解析/base64、提示词历史、TUI 状态机、换行与图片附件、Markdown 渲染（表格、代码块行号与语法高亮），以及 12 个端到端测试——起本地 mock SSE / auth 服务器，跑真实 `pi` 二进制验证完整链路（含 `.env` 自动加载、真实环境变量优先、空值视为未设置、`pi login` 保存与复用 auth.json、坏 key 不落盘、logout）。

另外有两个真 pty 冒烟脚本（同样不需要真实 key）：

```bash
python3 scripts/tui_smoke.py <数据目录> target/debug/pi        # 启动 / Ctrl+C 干净退出 / 恢复终端
python3 scripts/tui_chat_smoke.py <数据目录> target/debug/pi   # 本地 mock API：输入 → 流式渲染 → 退出
```

## 安全

`bash` 工具以你的用户身份、无沙箱执行。和 Pi 一样：想要安全就用容器。

```bash
docker run --rm -it -e DEEPSEEK_API_KEY -v "$PWD:/work" -w /work your-image pi
```
