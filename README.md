# pi-rs

[pi](https://pi.dev)（[earendil-works/pi](https://github.com/earendil-works/pi)）coding agent 的 Rust 移植。当前为**框架后期**：分层结构、核心类型、agent loop、内置工具与 ratatui TUI 已就位；默认使用 MiMo Token Plan 国内接口，OpenAI 兼容与 Anthropic 两大 HTTP provider 均支持增量流式。

上游 TS 源码浅克隆在 `./pi/`（.gitignore 已排除），所有模块注释均标注对应的 TS 文件路径。

## 当前能力边界

| 能力 | 当前状态 |
|---|---|
| 交互和 print 模式 | 可用于实验；内置工具调用、JSONL 会话记录（上游 v3 树格式，与 TS pi 会话文件双向互通）、`--continue`（cwd 过滤）/`--resume`、`/compact` 与阈值自动压缩、溢出自动恢复；print 支持多 prompt 顺序执行、管道 stdin 前置、`@file` 参数注入（文本文件包 `<file>` 标签）、上游退出码语义；`--mode text\|json`（json 事件流与上游逐事件对齐，差分场景验证） |
| 工具集选择 | 与上游同语义：默认激活 read/bash/edit/write（grep/find/ls 内置可选）、`-t/--tools`（裸名单替换或 `+name/-name` 修饰）、`-xt/--exclude-tools`（含 `mcp__*` 模式）、`-nt/--no-tools`、`-nbt/--no-builtin-tools`、settings `defaultTools`（两层合并）；消息 wire 与上游差分全场景一致（user/toolResult content 块数组） |
| OpenAI 兼容 Chat Completions | 增量流式（SSE）文本与工具调用、错误与取消路径；本地 HTTP mock 集成测试覆盖 |
| Anthropic Messages | 完整实现：SSE 流式（text/thinking/tool_use 分片）、thinking budget 映射、cache token 用量；本地 mock HTTP 测试覆盖 |
| 其他 HTTP provider | mock provider 可用于本地演示 |
| 会话压缩与重试 | `/compact`、阈值自动压缩（保留最近 20k token，可关）、split-turn（超长单 turn 切分 + 双摘要合并）、mid-run 检查（`prepare_next_turn` 钩子内压缩）与上下文溢出自动恢复（先省略失败轮再选切点→压缩→重试一次）；错误自动重试（可重试错误判定 + 指数退避 + `auto_retry_start/end` 事件 + `agent_end.willRetry`，RPC 可开关/中止；settings 键待接入） |
| agent loop | 钩子全接线（prepare_request/finish_turn/prepare_next_turn/before-after_tool_call）、并行工具执行（start 源序→end 完成序→result 源序）、content 工具检测、length 整批失败、set_tools 公告、steering/follow-up 队列（per-queue clear/peek）；事件顺序对齐上游（turn_start 先于消息公告、assistant 盖 thinkingLevel、turn_end.toolResults 带 role） |
| 内置工具 | 与上游同参数面：edit（`edits:[{oldText,newText}]` 数组 + BOM/行尾处理）、bash（`timeout` 秒、尾部 2000 行/50KB 截断）、grep/find（`ignore` walker，gitignore-aware，参数面全齐）、ls/read（limit/50KB 截断）；read 行号前缀与 PI_RS_* 前缀为已声明差异 |
| MCP | 已接入 CLI：全局 `mcp.json` + 受信任项目的 `.pi/mcp.json`（首次 TUI 提示信任），stdio 与 streamable HTTP server，协议版本协商（LATEST 2025-11-25 + 旧版接受表）、30s 请求超时 + notifications/cancelled、ping；OAuth 未实现 |
| SSH、扩展系统 | 接口或骨架阶段，尚不可用于完整工作流 |
| `--rpc` | **33/33 命令**（session 树族 new/switch/fork/clone/get_tree/get_entries/get_fork_messages/set_session_name、bash/abort_bash 透传、get_session_stats/export_html、set/abort_auto_retry）+ 事件帧含 `agent_settled{aborted}`、`bash_execution_update`、`auto_retry_start/end`、`session_info_changed`；get_state 12 字段全集；无扩展取消钩子故 `cancelled` 恒 false、MCP 工具不随会话替换重建（已声明偏差），逐条见 rpc.rs 模块文档 |
| bash 工具 | 注入 `PI_RS_*` 会话环境变量、信号退出码 128+n、超限全量输出落盘、流式输出节播到 TUI 实时预览 |
| TUI 主题与键位 | 已接入配置加载：`themes/<name>.json`（settings `theme` 选择）与 `keybindings.json`，坏文件回退内置并提示；footer 统一上下文用量（压缩后显示 `?`、`(auto)` 指示符） |

本项目仍在开发中，运行模型生成的 shell 命令和文件修改时，请在可信环境中使用并检查结果。

## 快速开始

```bash
cargo build --release
./target/release/pi-rs                 # 交互模式（ratatui TUI）
./target/release/pi-rs -p "list files" # print 模式（非交互）
./target/release/pi-rs --help
```

交互模式键位：`Enter` 发送（`\`+Enter 换行续写）· `Esc` 中止运行 · `Ctrl+C` 退出 · `PgUp/PgDn` / 滚轮 翻页 · `Ctrl+O` 展开工具完整输出 · `Ctrl+T` 折叠/展开思考 · 编辑器支持 readline 词导航（`Ctrl+←/→`、`Alt+B/F`、`Alt+Backspace`、`Ctrl+W/K/U/D`）。`/model` 打开模糊搜索的模型选择浮层（↑↓ 选择、Enter 确认、Esc 取消）。

浅色终端可使用 `cargo run -p pi-coding-agent -- --light`，或在 settings.json 中设置 `"theme": "light"`。如果终端设置了 `COLORFGBG`，程序也会按它声明的背景颜色选择默认主题；显式主题设置优先。

渲染按 pi 的组件语义移植：助手/用户消息以 markdown 渲染（标题、粗斜体、行内代码、围栏代码块、列表、引用、表格、链接），工具调用折叠显示 `key=value` 参数与 10 行输出预览（含运行时长），底栏两行显示 cwd+git 分支与 token/费用/上下文占比（>70% 变黄、>90% 变红），右侧对齐当前模型与 thinking 档位。

mock provider 行为：普通消息回显（演示流式渲染）；包含 "list"/"ls" 的消息触发一次 `ls` 工具调用往返（演示完整 agent loop：toolUse → 执行 → 结果回传 → 二轮总结）。

## 使用 OpenAI 兼容接口

在交互界面输入 `/login`（或 `/provider`），用方向键选择 MiMo 中国区 Token Plan、OpenAI、Anthropic 或自定义服务。预设服务只需输入 API key；自定义服务还需填写 ID、API 类型、Base URL 和模型 ID。保存后自动切换到该服务的模型；下次启动默认仍为 MiMo（除非 settings 指定了其他默认模型）。密钥输入时会隐藏，并以明文写入用户配置目录下的 `models.json`（Unix 文件权限为 0600）。预设服务可重新输入密钥；已有同名自定义服务不会被覆盖。

默认模型为 `mimo/mimo-v2.6-pro`。在 `.env` 中设置 `MIMO_API_KEY=你的Token Plan密钥` 后即可运行。默认地址为 `https://token-plan-cn.xiaomimimo.com/v1`；如订阅页面提供不同地址，可设置 `MIMO_BASE_URL`。Token Plan 密钥和按量付费密钥不可混用，请以订阅页面的凭证为准。

设置 `OPENAI_API_KEY` 后运行 `cargo run -p pi-coding-agent -- -p "你好" -m openai/gpt-5.2`。也可在 `~/.pi-rs/agent/models.json` 中按上游格式配置自定义 provider（`providers` 为对象，键是 provider id；`apiKey` 支持 `$ENV` 展开与 `!cmd` 命令），再通过 `-m provider/model-id` 选择：

```json
{
	"providers": {
		"my-proxy": {
			"baseUrl": "https://example.com/v1",
			"api": "openai-completions",
			"apiKey": "$MY_PROXY_KEY",
			"models": [{ "id": "my-model", "contextWindow": 128000 }]
		}
	}
}
```

旧版 Rust 数组格式仍可读取（过渡期）；`maxTokens` 默认 16384，`contextWindow` 默认 128000。响应支持增量流式输出。

也可在当前目录或其父目录的 `.env` 中设置 `OPENAI_API_KEY=你的密钥`。启动时自动加载，已有环境变量优先；`.env` 已加入 Git 忽略列表。

## 结构映射（packages → crates）

依赖方向与 pi 根 `package.json` 的构建顺序一致：telemetry → chord → tui → codemode → mcp → ai → durable → env → agent → coding-agent。

| crate | 对应 pi 包 | 状态 | 职责 |
|---|---|---|---|
| `pi-telemetry` | `packages/telemetry` | 契约 | 遥测 span 契约（pi-ai 依赖） |
| `pi-chord` | `packages/chord` | 占位 | 插件宿主 / 复制状态 / JSON 链路 RPC |
| `pi-tui` | `packages/tui` | **核心可用** | ratatui + crossterm 上的组件体系：`Component` trait、多行 emacs 风格 `Editor`（词导航/kill/`\` 续行）、`ScrollView`、pulldown-cmark `Markdown`（对照 markdown.ts）、fuzzy 匹配 + `SelectList`、`Theme`（含 md 配色）、终端生命周期 |
| `pi-codemode` | `packages/codemode` | 占位 | 沙箱脚本执行，仅能调用注入的工具 |
| `pi-mcp` | `packages/mcp` | 骨架 | MCP 客户端：JSON-RPC 协议类型、transport trait（stdio、streamable-http 和 in-memory 实现）、client |
| `pi-ai` | `packages/ai` | **部分（2 provider 已验证）** | 归一化消息模型、`AssistantMessageEvent` 流协议、provider 适配器注册（OpenAI Chat Completions、Anthropic Messages 和 mock）；上游 10+ 个 API 与模型目录/认证未实现 |
| `pi-durable` | `packages/durable` | 占位 | 持久会话 / 任务 / 文档运行时 |
| `pi-env` | `packages/env` | **本地实现** | `EnvConnection` 抽象（exec/read/write/list/watch），本地实现完整，SSH 远程为桩 |
| `pi-agent` | `packages/agent` | **核心可用** | `AgentTool` 契约、`AgentEvent`、steering/follow-up 队列、agent loop（`run_agent_loop`，对应 `agent-loop.ts`）、`Agent`；钩子全接线、并行工具执行、content 工具检测 |
| `pi-coding-agent` | `packages/coding-agent` | **框架完成** | bin `pi-rs`：cli/config、core（session/settings/system-prompt/model-registry/event-bus/slash-commands/tool-selection）、7 个内置工具（默认激活 4，`--tools` 可选全量）、interactive（ratatui）/print/rpc 三种模式 |

## 数据流

```
用户输入 ──▶ SessionCommand ──▶ AgentSession(后台任务) ──▶ Agent
                                │ 事件转发 + JSONL 持久化      │ run_agent_loop
                                ▼                            │  ├─ stream_fn(model, transcript)
                          EventBus<SessionEvent>             │  ├─ 工具执行（before/after hook 点位）
                                ▲                            │  ├─ steering / follow-up 队列轮询
                                └────────── ratatui InteractiveMode / print / rpc
```

JSON 兼容性：消息、模型、session 文件全部按 pi 的 camelCase wire 格式序列化（`role` tag、`type` tag、`stopReason`、`toolCallId`…），目标是与 TS pi 的 session  transcript 互通。

配置目录：`~/.pi-rs/agent/`（`PI_RS_AGENT_DIR` 覆盖），含 `settings.json`、`models.json`、`sessions/<id>/session.jsonl`。与 pi 本体的 `~/.pi` 隔离，两者可共存。

## 内置工具（core/tools）

与 pi 相同的 7 件套：`read`（行号+截断）、`write`、`edit`（精确串替换+唯一性检查）、`bash`（经 pi-env，超时+abort 感知）、`grep`（regex，跳过 .git/node_modules/target）、`find`（glob）、`ls`。每个工具实现 `pi_agent::AgentTool` trait；后续 before/after hook 接上权限门禁。

默认激活上游同款 4 件（`read`/`bash`/`edit`/`write`，`tool_selection.rs` 对照 `settings-manager.ts` + `sdk.ts`）；grep/find/ls 随 `-t bash,grep`（裸名单替换）、`-t +grep`（修饰默认集）、`-xt`（排除，支持 `mcp__*` 模式）、`-nt`/`-nbt` 或 settings `defaultTools` 激活。

## 路线图

已完成：anthropic-messages/openai-completions 增量流式、agent loop 完整化（并行工具、钩子、工具检测）、会话压缩全链路（阈值/split-turn/mid-run/溢出恢复）、MCP stdio+streamable HTTP 接入、TUI 主题/键位、工具集选择与消息 wire 对齐（差分四场景与上游快照逐字节一致）。

剩余：1. **providers** — openai-responses、google-generative-ai 等 8 个 API；模型目录/认证（auth.json/OAuth）。2. **RPC/SDK** — session-tree 族（fork/clone/get_tree/get_entries）、bash 透传、export_html、SDK 导出面。3. **MCP** — OAuth、`/mcp` UI。4. **TUI** — 语法高亮、外部编辑器（ctrl+g）、图片粘贴、会话/设置选择器。5. **扩展系统** — skill/提示词模板目录加载（frontmatter）、codemode 沙箱、tool-search。6. **SSH 远程环境**。
7. **rpc 模式** — 命令分发接 `SessionCommand`（协议帧已就位）。
8. **外围** — pi-durable（replay 策略）、pi-env SSH、pi-chord 多机。

## 开发

```bash
cargo build          # 全工作区
cargo test           # 单元测试（session 时间戳等）
cargo run -p pi-coding-agent    # 调试运行
```
