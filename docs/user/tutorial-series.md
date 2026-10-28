# pi-rs 系列教程：跟着构建进度上手一个 Rust coding agent

> 进度快照：2026-10-28（HEAD `08dddc3`）。本系列只讲**已实现并验证过**的能力；每一章末尾都会明确列出"还做不到的事"，与 [README 能力表](../../README.md)和 [docs/operations/compatibility-v1.4.md](../operations/compatibility-v1.4.md) 保持一致。跟随进度更新，请以仓库最新文档为准。

## 目录

| 章 | 主题 | 你将学会 |
|---|---|---|
| [第 1 章](#第-1-章认识-pi-rs) | 认识 pi-rs | 它是什么、与上游 pi 的关系、整体进度 |
| [第 2 章](#第-2-章构建与第一次对话) | 构建与第一次对话 | 零配置用 mock provider 演练 TUI 与 agent loop |
| [第 3 章](#第-3-章接入真实模型) | 接入真实模型 | MiMo / OpenAI 兼容 / Anthropic / 自定义 provider |
| [第 4 章](#第-4-章内置工具与-agent-loop) | 内置工具与 agent loop | 7 件套工具的参数、行为与安全边界 |
| [第 5 章](#第-5-章会话持久化恢复与压缩) | 会话：持久化、恢复与压缩 | `--continue`/`--resume`、`/compact`、自动压缩 |
| [第 6 章](#第-6-章tui-定制主题与键位) | TUI 定制 | `themes/*.json`、`keybindings.json`、`settings.json` |
| [第 7 章](#第-7-章mcp接入外部工具) | MCP 接入 | `mcp.json`、stdio/HTTP server、信任与环境变量展开 |
| [第 8 章](#第-8-章rpc-模式脚本化集成) | RPC 模式 | JSONL 帧协议、19 个命令、事件流 |
| [第 9 章](#第-9-章架构漫游与参与开发) | 架构漫游 | crate 分层、数据流、开发门禁与差分测试 |
| [附录 A](#附录-a速查表) | 速查表 | 键位、斜杠命令 |
| [附录 B](#附录-b当前能力边界) | 能力边界 | 已实现 / 未实现清单 |

---

## 第 1 章：认识 pi-rs

**pi-rs** 是 [pi](https://pi.dev)（[earendil-works/pi](https://github.com/earendil-works/pi)）coding agent 的 Rust 移植。上游是 TypeScript monorepo（`packages/coding-agent` 等），pi-rs 按 pi 根 `package.json` 的构建顺序把它映射成一组 cargo crate：

| crate | 对应 pi 包 | 状态 |
|---|---|---|
| `pi-tui` | `packages/tui` | **核心可用**（ratatui + crossterm 组件体系） |
| `pi-ai` | `packages/ai` | **部分**（OpenAI 兼容 + Anthropic Messages 两大 provider 已验证） |
| `pi-agent` | `packages/agent` | **核心可用**（agent loop、工具契约、steering 队列） |
| `pi-coding-agent` | `packages/coding-agent` | **框架完成**（bin `pi-rs`：7 个内置工具、interactive/print/rpc 三模式） |
| `pi-env` | `packages/env` | 本地实现完整（SSH 为桩） |
| `pi-mcp` | `packages/mcp` | stdio + streamable HTTP 客户端可用 |
| `pi-telemetry` / `pi-chord` / `pi-codemode` / `pi-durable` | 其余包 | 契约/占位/骨架 |

一句话概括现状：**分层结构、核心类型、agent loop、7 个内置工具、ratatui TUI、两大 provider 的增量流式、JSONL 会话持久化、压缩与恢复、MCP 客户端、RPC 协议帧都已就位**；扩展系统、SSH、会话分支树、OAuth 还在路上（见[附录 B](#附录-b当前能力边界)）。

所有模块注释都标注了对应的 TS 源文件路径，上游源码浅克隆在仓库的 `./pi/` 目录。教程采用"由用到改"的顺序：第 2–8 章把 pi-rs 当作用户工具学会，第 9 章切换到贡献者视角。

> 环境要求：Rust 工具链需支持 Edition 2024（建议 1.85+）。平台：macOS（Apple Silicon/Intel）、Linux。

---

## 第 2 章：构建与第一次对话

目标：不配任何密钥，用内置 mock provider 完整走一遍 TUI 和 agent loop。

### 2.1 构建

```bash
cargo build --release
```

产物在 `target/release/pi-rs`。直接 `cargo run -p pi-coding-agent` 也可以调试运行。

### 2.2 交互模式（TUI）

```bash
./target/release/pi-rs
```

默认进入 ratatui 交互界面。即使没有任何 API Key，内置的 mock 模型 `mock/mock-echo` 也能工作：

- 发送**普通文字**：模型逐段"打字"回显，演示流式渲染；
- 消息里包含 **"list" 或 "ls"**：触发一次 `ls` 工具调用往返——先发 toolUse、执行真实 `ls`、回传结果、再二轮总结。这是观察完整 agent loop 最直观的方式。

**试着输入：`list files in this directory`**，你会看到：一条折叠的工具调用卡片（`key=value` 参数 + 10 行输出预览 + 运行时长），随后是模型对结果的总结。

### 2.3 基本键位

| 键 | 作用 |
|---|---|
| `Enter` | 发送（`\` + `Enter` 换行续写） |
| `Esc` | 中止正在执行的生成或工具调用 |
| `Ctrl+C` | 退出 |
| `PgUp` / `PgDn` / 滚轮 | 翻动历史 |
| `Ctrl+O` | 展开选中工具的完整输出 |
| `Ctrl+T` | 折叠/展开思考过程 |

编辑器支持 readline 词导航：`Ctrl+←/→`、`Alt+B/F`、`Alt+Backspace`、`Ctrl+W/K/U/D`。完整键位见[附录 A](#附录-a速查表)。

底栏两行：左边是 cwd + git 分支，中间是 token 用量/费用/上下文占比（超过 70% 变黄、90% 变红），右侧对齐当前模型与 thinking 档位。

### 2.4 print 模式（非交互）

单条指令，直接输出 Markdown 结果后退出：

```bash
./target/release/pi-rs -p "list files"
./target/release/pi-rs -p "hello" -m mock/mock-echo
```

### 还做不到

- 会话选择器浮层（`/sessions` 目前是 stub，只列出命令名）；
- 图片粘贴、语法高亮、`Ctrl+G` 唤起外部编辑器。

### 源码导览

- TUI 组件体系：`crates/pi-tui/src/`（`Component` trait、`Editor`、`Markdown`、`SelectList`）
- 交互模式装配：`crates/pi-coding-agent/src/modes/interactive.rs`

---

## 第 3 章：接入真实模型

pi-rs 内置四个模型条目，通过 `-m provider/model-id` 选择：

| 模型标识 | API | 密钥环境变量 | 说明 |
|---|---|---|---|
| `mimo/mimo-v2.6-pro` | OpenAI 兼容 | `MIMO_API_KEY` | **默认模型**（国内 Token Plan） |
| `openai/gpt-5.2` | OpenAI 兼容 | `OPENAI_API_KEY` | 官方 OpenAI |
| `anthropic/claude-sonnet-4-6` | Anthropic Messages | `ANTHROPIC_API_KEY` | 支持 thinking 与 cache token 统计 |
| `mock/mock-echo` | mock | 无 | 第 2 章的演示模型 |

### 3.1 `.env` 与密钥加载

启动时自动加载**当前目录或其父目录**的 `.env`，已有进程环境变量优先，`.env` 已被 git 忽略：

```bash
# .env —— 国内默认（MiMo Token Plan）
MIMO_API_KEY=你的Token Plan密钥
# 可选：MIMO_BASE_URL=https://token-plan-cn.xiaomimimo.com/v1
```

注意：Token Plan 密钥和按量付费密钥不可混用，以订阅页面的凭证为准。

用 OpenAI / Anthropic 时同理：

```bash
# .env
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
```

```bash
./target/release/pi-rs -p "你好" -m openai/gpt-5.2
./target/release/pi-rs -p "你好" -m anthropic/claude-sonnet-4-6
```

### 3.2 自定义 OpenAI 兼容 provider

任何 OpenAI Chat Completions 兼容端点（DeepSeek、Qwen、vLLM、ollama 等）都可以写进 `~/.pi-rs/agent/models.json`：

```json
{
  "providers": [
    {
      "name": "custom-openai",
      "api": "openai-completions",
      "baseUrl": "https://api.your-endpoint.com/v1",
      "models": [
        { "id": "deepseek-coder", "contextWindow": 65536, "maxTokens": 4096 }
      ]
    }
  ]
}
```

然后 `-m custom-openai/deepseek-coder` 调起。自定义接口当前同样读取 `OPENAI_API_KEY`。

### 3.3 流式与 thinking

两大 HTTP provider 都支持**增量 SSE 流式**：文本与工具调用参数按分片增量到达并实时渲染。Anthropic 路径还映射了 thinking budget 与 cache token 用量。

thinking 档位共 7 级：`off / minimal / low / medium / high / xhigh / max`。在 TUI 里：

- `/thinking` 循环切换档位；
- `/model` 打开模糊搜索的模型选择浮层（↑↓ 选择、Enter 确认、Esc 取消）。

默认档位可在 `settings.json` 的 `defaultThinkingLevel` 固化（见第 6 章）。

### 还做不到

- Google Generative AI provider（上游还有 10+ 个 API）；
- 模型目录 hydrate 与 `auth.json`/OAuth 登录；
- 自定义 provider 的独立密钥字段（统一走 `OPENAI_API_KEY`）。

### 源码导览

- 模型注册表：`crates/pi-coding-agent/src/core/model_registry.rs`（内置模型就在 `builtin_models()`）
- Provider 适配器：`crates/pi-ai/src/providers/openai_completions.rs`、`anthropic.rs`

---

## 第 4 章：内置工具与 agent loop

pi-rs 实现了与上游相同的 7 件内置工具，全部实现 `pi_agent::AgentTool` trait。`/tools` 命令可随时列出当前激活的工具。

### 4.1 工具清单

| 工具 | 关键参数 | 行为要点 |
|---|---|---|
| `read` | `path`；可选 `offset`、`limit` | 带行号输出，大文件自动截断 |
| `write` | `path`、`content` | 覆盖/创建，自动建父目录，写入后 sync |
| `edit` | `path`、`oldText`、`newText` | 精确串替换；`oldText` 找不到或**出现多次都会拒绝**，防歧义损坏 |
| `bash` | `command`；可选 `timeout`（毫秒，默认 30000） | 见 4.2 |
| `grep` | `pattern`；可选 `path`、`include` | Rust 正则；自动跳过 `.git`/`node_modules`/`target` |
| `find` | `pattern`（glob）；可选 `path` | 文件名匹配，如 `*.rs` |
| `ls` | 可选 `path`、`depth` | 目录列表 |

### 4.2 bash 工具的工程细节

bash 是最复杂的一件，值得单独讲：

- **进程组隔离**：命令在独立 process group 中运行；退出或取消时向整组发 SIGKILL（主动 `setsid` 脱离组的后代不覆盖；非 Unix 只 kill 直接子进程）。
- **管道防挂死**：shell 退出后若有后代（如 `sleep 5 &`）持有 stdout/stderr，等待 EOF 改为"100ms 静默宽限、每收到一块输出重新计时"，避免主线程永远挂住。
- **会话环境注入**：命令可读到 `PI_RS_SESSION_ID`、`PI_RS_SESSION_FILE`、`PI_RS_PROVIDER`、`PI_RS_MODEL`、`PI_RS_REASONING_LEVEL` 五个变量。
- **可观测**：信号退出码按 `128+n` 报告；超时后全量输出落盘；TUI 中输出按节流实时预览。

在对话里让模型试一句：

```text
用 bash 打印 $PI_RS_MODEL 环境变量
```

### 4.3 agent loop 的形状

```
用户输入 ──▶ SessionCommand ──▶ AgentSession(后台任务) ──▶ Agent
                                │ 事件转发 + JSONL 持久化      │ run_agent_loop
                                ▼                            │  ├─ stream_fn(model, transcript)
                          EventBus<SessionEvent>             │  ├─ 工具执行（before/after hook 点位）
                                ▲                            │  ├─ steering / follow-up 队列轮询
                                └────────── InteractiveMode / print / rpc
```

一轮 run 里模型可以连续发起多次工具调用，工具结果回传后进入下一轮，直到产出终止消息。运行中你可以随时 `Esc` 中止，或输入新消息进 steering 队列（下一条消息会注入当前运行，而不是排队到下一轮）。

### 安全边界

工具经 `EnvConnection` 抽象执行（当前为本地实现，SSH 远程是桩）。**before/after hook 只有类型定义、权限门禁尚未接线**——运行模型生成的 shell 命令和文件修改时，请在可信环境中使用并检查结果。

### 还做不到

- 并行工具执行（上游用 JoinSet 并发跑多个工具，pi-rs 目前串行）；
- `prepare_request`/`finish_turn` 等 hook 的实际调用；
- bash 的 stdin transport 与 Windows 原生支持（上游 `bash-executor.ts`/`powershell.ts`）。

### 源码导览

- 工具实现：`crates/pi-coding-agent/src/core/tools/`（每件一个文件）
- 循环主体：`crates/pi-agent/src/agent.rs` 的 `run_agent_loop`（对应上游 `agent-loop.ts`）
- 进程语义：`crates/pi-env/src/local.rs`

---

## 第 5 章：会话：持久化、恢复与压缩

### 5.1 会话文件在哪

```
~/.pi-rs/agent/
└── sessions/<session-id>/session.jsonl
```

每行一个 JSON 条目：先是 `header`（会话元信息），后跟消息与事件条目。所有字段按 pi 的 camelCase wire 格式序列化（`role` tag、`stopReason`、`toolCallId`…），目标是与 TS pi 的 session transcript 互通。配置目录可用 `PI_RS_AGENT_DIR` 覆盖。

持久化的可靠性设计（第二、三轮加固）：每条目写入后 `sync_data()` 落盘，进程崩溃不丢已确认条目；打开会话时若文件末尾有崩溃撕裂的半行会自动补 `\n` 自愈，损坏行跳过计数；写入失败时游标不推进，避免重复追加。注意 JSONL 追加**不是事务**，文档如实如此声明。

### 5.2 恢复会话

```bash
./target/release/pi-rs --continue            # 恢复最近一次会话
./target/release/pi-rs --resume <id>         # 按 id（支持唯一前缀）
./target/release/pi-rs --resume /path/to/session.jsonl   # 按完整路径
```

行为保证：恢复前校验 header，坏会话直接报错**不留半写**；恢复后旧消息不会重复落盘；system prompt 按当前环境重新播种（对齐上游 `_rebuildSystemPrompt`）。`--continue` 与 `--no-session`（不记录会话）互斥。

### 5.3 压缩：手动与自动

上下文快满时把历史折叠成一段摘要：

```text
/compact
```

手动压缩会调用模型自身生成摘要并替换历史；模型停止原因是 error/length 或想再调工具时，压缩结果**拒绝落盘**。

自动压缩由 `settings.json` 控制（默认开启，保留最近 20k token）：

```json
{
  "compaction": {
    "enabled": true,
    "reserveTokens": 16384,
    "keepRecentTokens": 20000
  }
}
```

配置全局（`~/.pi-rs/agent/settings.json`）与项目（`.pi/settings.json`）两层，项目值优先。

**上下文溢出自动恢复**：请求超过模型窗口时，pi-rs 会剔除失败轮 → 压缩 → 重试一次。footer 上：压缩后 token 数不可精确时显示 `?`，自动压缩启用时显示 `(auto)` 指示符。

### 还做不到

- split-turn 与 mid-run 检查点压缩；
- 分支树投影（`/tree`）、fork/clone、HTML 导出；
- 按模型覆盖压缩参数（`compaction.modelOverrides`）。

### 源码导览

- 会话管理：`crates/pi-coding-agent/src/core/session_manager.rs`
- 压缩核心：`crates/pi-coding-agent/src/core/compaction.rs`（SUMMARIZATION_PROMPT 逐字移植自上游）
- 内部机制长文：[docs/internals/session-and-persistence.md](../internals/session-and-persistence.md)

---

## 第 6 章：TUI 定制：主题与键位

### 6.1 主题

pi-rs 沿用上游形态：主题是**独立文件** `~/.pi-rs/agent/themes/<name>.json`，再用 `settings.json` 的 `theme` 字段选择：

```json
// ~/.pi-rs/agent/settings.json
{ "theme": "my-dark" }
```

主题文件遵循上游 `theme.schema.json`：`vars` 变量引用（带环检测）、`#rgb`/`#rrggbb`、0–255 索引色、oklch/okhsl 颜色（完整移植了上游的 oklab 色彩空间实现，测试期望值直接用上游 TS 运行结果生成）。

坏文件的处理策略：**回退内置主题并给出 notice 提示**，不会白屏或 panic。键位文件同理。

### 6.2 键位

`~/.pi-rs/agent/keybindings.json` 可覆盖内置的 47 项默认键位（覆盖、禁用、冲突检测三种语义，对应上游 chord 体系）。interactive 模式键位统一经 `resolve_global_action` 纯函数分发——你在 keybindings.json 里改 `app.model.select` 之类的 action，重启即生效。

### 6.3 settings.json 全字段

```json
{
  "defaultProvider": "mimo",
  "defaultModel": "mimo-v2.6-pro",
  "defaultThinkingLevel": "medium",
  "theme": "my-dark",
  "externalEditor": "vim",
  "shellPath": "/bin/zsh",
  "compaction": { "enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000 }
}
```

### 还做不到

- 语法高亮、图片粘贴、`Ctrl+G` 外部编辑器（`externalEditor` 字段已存在，编辑器集成未实现）；
- 会话/设置选择器浮层。

### 源码导览

- 主题解析：`crates/pi-tui/src/theme.rs`；键位：`crates/pi-tui/src/keys.rs`、`keybindings.rs`
- 接线：`crates/pi-coding-agent/src/core/ui_config.rs`

---

## 第 7 章：MCP：接入外部工具

pi-rs 已接入 MCP 客户端，支持 **stdio** 和 **streamable HTTP** 两种 server。配置文件形状与其他 MCP 客户端（Claude Desktop 等）相同，可以直接拷过来用。

### 7.1 配置文件

全局：`~/.pi-rs/agent/mcp.json`；项目级：`<project>/.pi/mcp.json`（受信任的项目才会加载，首次在 TUI 中提示信任）。

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" }
    },
    "off": { "command": "uvx", "args": ["some-server"], "enabled": false }
  }
}
```

### 7.2 值展开

`env`/`headers` 的值支持 `${NAME}`、`$NAME` 引用环境变量，`!cmd` 执行 shell 命令取输出（tokio `sh -c`，10 秒超时）；`$$`、`$!` 转义字面量；`command`/`args`/`cwd` 不做 `${}` 展开，`cwd` 支持开头 `~`。某个值解析失败只丢弃该 server 条目（记为配置错误），不影响其他条目。

### 7.3 合并规则（全局 + 项目）

- 项目条目带 `command`、`url` 或 `type`：**整体替换**同名全局 server；
- 不带这些字段：只覆盖全局 server 的 `enabled` 开关；
- 覆盖不存在于全局的 server：报错。

### 7.4 运行行为

- 每个 server 启动预算 10 秒；单个 server 失败被**隔离**成一条 notice，不拖垮启动；
- 工具以 `mcp__<server>__<tool>` 命名注册进 agent，`/tools` 可见；
- 三条退出路径（TUI 返回、print 退出前、RPC 返回）都会执行 shutdown 清理子进程；
- 不支持的形态按非致命配置错误报告：HTTP `oauth`/`auth`（没有 OAuth 客户端）、legacy `sse` 类型；未知字段忽略，所以为其他客户端写的配置也能加载。

### 还做不到

- OAuth 授权（`oauth`/`auth` 字段会被拒绝）；
- streamable HTTP 的 POST SSE 续传；
- `/mcp` 管理 UI。

### 源码导览

- 配置读取与合并：`crates/pi-coding-agent/src/core/mcp_config.rs`（模块文档详细列出了与上游的逐条差异）
- 协议与传输：`crates/pi-mcp/src/`（JSON-RPC 类型、stdio/HTTP transport、`McpClient`）

---

## 第 8 章：RPC 模式：脚本化集成

`--rpc` 把 pi-rs 变成一个可被脚本/宿主程序驱动的 headless agent：stdin 收 JSON 命令（每行一个），stdout 出响应与事件（每行一个 JSON），LF 分帧。

```bash
./target/release/pi-rs --rpc -m mock/mock-echo
```

### 8.1 基本往返

```jsonc
// ↓ 你输入（stdin）
{"type": "get_state", "id": "1"}
// ← 它输出（stdout）
{"type": "response", "command": "get_state", "success": true, "data": { ... }}

{"type": "prompt", "id": "2", "message": "list files"}
// 立即回执（不等待 run 结束）：
{"type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}}
// 随后事件帧持续流出（Agent 事件，camelCase 字段）……

{"type": "get_last_assistant_text", "id": "3"}
```

`id` 会原样回显在响应里；`prompt` 的 `disposition` 是 `started`（立即开跑）或 `queued`（已有运行在排队）。

### 8.2 已实现的 19 个命令

`prompt`、`steer`、`follow_up`、`abort`、`clear_queue`、`set_model`、`cycle_model`、`set_thinking_level`、`cycle_thinking_level`、`get_state`、`get_messages`、`get_available_models`、`get_available_thinking_levels`、`set_steering_mode`、`set_follow_up_mode`、`compact`、`set_auto_compaction`、`get_last_assistant_text`、`get_commands`。

进程退出（EOF）时会先 abort 正在跑的 run 并等待会话落盘。

### 8.3 与上游 wire 的已知偏差

上游共 33 个命令。尚未实现的部分会以错误帧应答而不是静默忽略：session-tree 族（`fork`/`clone`/`get_tree`/`switch_session`/`new_session` 等）、`bash` 透传、`export_html`、重试控制（`set_auto_retry` 等）、扩展 UI 桥；`agent_settled` 事件也缺失。逐条偏差记录在 `crates/pi-coding-agent/src/modes/rpc.rs` 模块文档与 [docs/operations/compat/cli-rpc-sdk.md](../operations/compat/cli-rpc-sdk.md)——集成前请先读。

---

## 第 9 章：架构漫游与参与开发

### 9.1 分层与依赖方向

crate 依赖方向与上游 pi 的构建顺序一致，单向不回头：

```
telemetry → chord → tui → codemode → mcp → ai → durable → env → agent → coding-agent(bin)
```

- `pi-ai`：归一化消息模型、`AssistantMessageEvent` 流协议、provider 注册；
- `pi-agent`：`AgentTool` 契约、`AgentEvent`、steering/follow-up 队列、`run_agent_loop`；
- `pi-coding-agent`：cli/config、core（session/settings/system-prompt/model-registry/event-bus/slash-commands/compaction/mcp-config）、三种运行模式。

### 9.2 内部机制文档

`docs/internals/` 有五篇长文，比本章深得多：

- [overview.md](../internals/overview.md) — 全局总览
- [agent-loop.md](../internals/agent-loop.md) — 循环、队列、事件
- [session-and-persistence.md](../internals/session-and-persistence.md) — JSONL、恢复、压缩
- [tui-runtime.md](../internals/tui-runtime.md) — ratatui 运行时与组件
- [process-and-env.md](../internals/process-and-env.md) — bash、进程组、EnvConnection

### 9.3 差分测试

仓库带 `difftest/` 差分测试框架：对**固定版本的上游快照**做行为对比，用来发现 pi-rs 与 TS pi 的兼容性偏差。兼容矩阵与发现记录见 [docs/operations/compatibility-v1.4.md](../operations/compatibility-v1.4.md) 与 [baseline-v1.4.md](../operations/baseline-v1.4.md)。

### 9.4 开发门禁

每个改动都要过三道门（CI 三 job 全部 `--locked`）：

```bash
cargo test --workspace --locked
cargo fmt --all -- --check          # rustfmt.toml: hard_tabs = true
cargo clippy --workspace --all-targets --locked -- -D warnings
```

模块注释统一标注对应的 TS 文件路径——改一个模块前，先读它的上游对照。

### 9.5 路线图（接下来会实现什么）

1. agent loop 完整化：并行工具执行、hook 接线、错误自动重试；
2. providers 扩容：Google Generative AI、模型目录、OAuth；
3. session 分支树（`/tree`、fork）、HTML 导出；
4. MCP streamable HTTP 续传与 OAuth、`/mcp` UI；
5. TUI 剩余：语法高亮、外部编辑器、图片粘贴、会话选择器；
6. 扩展系统：skill/提示词模板、codemode 沙箱；
7. pi-durable、SSH 远程执行、pi-chord 多机。

---

## 附录 A：速查表

### 斜杠命令

| 命令 | 作用 |
|---|---|
| `/help` | 显示命令与键位 |
| `/model [query]` | 模型列表/模糊搜索浮层切换 |
| `/thinking` | 循环切换 thinking 档位（off→max 共 7 级） |
| `/tools` | 列出激活的工具（含 MCP 工具） |
| `/prompt` | 显示当前 system prompt |
| `/compact` | 手动压缩历史为摘要 |
| `/clear` | 清空聊天视图 |
| `/sessions` | 列出最近会话（stub） |
| `/exit`（`/quit`、`/q`） | 退出 |

### 编辑器与全局键位

`Enter` 发送 · `\`+`Enter` 换行 · `Esc` 中止 · `Ctrl+C` 退出 · `PgUp/PgDn`/滚轮 翻页 · `Ctrl+O` 展开工具输出 · `Ctrl+T` 思考折叠 · `Ctrl+←/→`、`Alt+B/F` 词导航 · `Ctrl+W/K/U/D` 词删除/行清空

### 环境变量

| 变量 | 作用 |
|---|---|
| `MIMO_API_KEY` / `MIMO_BASE_URL` | 默认 provider（MiMo Token Plan） |
| `OPENAI_API_KEY` | OpenAI 及自定义 OpenAI 兼容接口 |
| `ANTHROPIC_API_KEY` | Anthropic Messages |
| `PI_RS_AGENT_DIR` | 配置目录覆盖（默认 `~/.pi-rs/agent`） |
| `PI_RS_SESSION_ID` 等 | bash 工具内注入的会话上下文（只读） |

## 附录 B：当前能力边界

**已实现并验证**：7 内置工具 · OpenAI 兼容 + Anthropic 增量流式 · JSONL 会话（恢复/落盘加固/撕裂自愈） · `/compact` + 自动压缩 + 溢出恢复 · MCP（stdio/HTTP、信任、env 展开、失败隔离） · RPC 19 命令 · TUI 主题/键位文件加载 · bash 进程组/实时预览/`PI_RS_*` 注入

**未实现**：并行工具执行 · hook 权限门禁接线 · Google 等 provider、OAuth/auth.json · 会话分支树/HTML 导出 · MCP OAuth 与 SSE 续传、`/mcp` UI · 扩展系统（skill/codemode/tool-search） · SSH 远程 · pi-durable/chord · RPC 剩余 14 命令与 `agent_settled`

---

*本教程与代码同步演进：发现文档与行为不符时，以代码和 [README](../../README.md) 为准，并欢迎提 issue 修正教程。*
