# pi-rs

[pi](https://pi.dev)（[earendil-works/pi](https://github.com/earendil-works/pi)）coding agent 的 Rust 移植。当前为**框架阶段**：分层结构、核心类型、agent loop、内置工具与 ratatui TUI 已就位；默认使用 MiMo Token Plan 国内接口，OpenAI 兼容 Chat Completions 支持请求和工具调用，其他 HTTP provider 仍在开发中。

上游 TS 源码浅克隆在 `./pi/`（.gitignore 已排除），所有模块注释均标注对应的 TS 文件路径。

## 当前能力边界

| 能力 | 当前状态 |
|---|---|
| 交互和 print 模式 | 可用于实验；支持内置工具调用、本地 JSONL 会话记录与 `--continue`/`--resume` 会话恢复 |
| OpenAI 兼容 Chat Completions | 支持增量流式（SSE）文本与工具调用、错误与取消路径；本地 HTTP mock 集成测试覆盖 |
| 其他 HTTP provider | Anthropic 为 fail-fast 桩；mock provider 可用于本地演示 |
| 会话压缩 | 尚未实现（会话恢复已接入 CLI） |
| MCP | stdio 传输、客户端（list/call tools）与 AgentTool 桥已完成并通过测试；尚未接入 CLI 工具装配 |
| SSH、扩展系统 | 接口或骨架阶段，尚不可用于完整工作流 |
| `--rpc` | 已实现 prompt/steer/follow_up/abort/set_model/set_thinking/get_state/get_messages 等命令与事件帧；与上游 wire 形态的偏差见 rpc.rs 模块文档 |
| TUI 主题与键位 | 主题文件解析（含 OKHSL 色值）与键位管理器已库级移植并测试；尚未接入 CLI 配置加载 |

本项目仍在开发中，运行模型生成的 shell 命令和文件修改时，请在可信环境中使用并检查结果。

## 快速开始

```bash
cargo build --release
./target/release/pi-rs                 # 交互模式（ratatui TUI）
./target/release/pi-rs -p "list files" # print 模式（非交互）
./target/release/pi-rs --help
```

交互模式键位：`Enter` 发送（`\`+Enter 换行续写）· `Esc` 中止运行 · `Ctrl+C` 退出 · `PgUp/PgDn` / 滚轮 翻页 · `Ctrl+O` 展开工具完整输出 · `Ctrl+T` 折叠/展开思考 · 编辑器支持 readline 词导航（`Ctrl+←/→`、`Alt+B/F`、`Alt+Backspace`、`Ctrl+W/K/U/D`）。`/model` 打开模糊搜索的模型选择浮层（↑↓ 选择、Enter 确认、Esc 取消）。

渲染按 pi 的组件语义移植：助手/用户消息以 markdown 渲染（标题、粗斜体、行内代码、围栏代码块、列表、引用、表格、链接），工具调用折叠显示 `key=value` 参数与 10 行输出预览（含运行时长），底栏两行显示 cwd+git 分支与 token/费用/上下文占比（>70% 变黄、>90% 变红），右侧对齐当前模型与 thinking 档位。

mock provider 行为：普通消息回显（演示流式渲染）；包含 "list"/"ls" 的消息触发一次 `ls` 工具调用往返（演示完整 agent loop：toolUse → 执行 → 结果回传 → 二轮总结）。

## 使用 OpenAI 兼容接口

默认模型为 `mimo/mimo-v2.6-pro`。在 `.env` 中设置 `MIMO_API_KEY=你的Token Plan密钥` 后即可运行。默认地址为 `https://token-plan-cn.xiaomimimo.com/v1`；如订阅页面提供不同地址，可设置 `MIMO_BASE_URL`。Token Plan 密钥和按量付费密钥不可混用，请以订阅页面的凭证为准。

设置 `OPENAI_API_KEY` 后运行 `cargo run -p pi-coding-agent -- -p "你好" -m openai/gpt-5.2`。也可在 `~/.pi-rs/agent/models.json` 中使用 `providers` 数组配置 `name`、`api`（`openai-completions`）、`base_url` 和 `models`（含 `id`），再通过 `-m provider/model-id` 选择。自定义接口当前同样读取 `OPENAI_API_KEY`。响应当前在完整接收后显示，尚不支持增量流式输出。

也可在当前目录或其父目录的 `.env` 中设置 `OPENAI_API_KEY=你的密钥`。启动时自动加载，已有环境变量优先；`.env` 已加入 Git 忽略列表。

## 结构映射（packages → crates）

依赖方向与 pi 根 `package.json` 的构建顺序一致：telemetry → chord → tui → codemode → mcp → ai → durable → env → agent → coding-agent。

| crate | 对应 pi 包 | 状态 | 职责 |
|---|---|---|---|
| `pi-telemetry` | `packages/telemetry` | 契约 | 遥测 span 契约（pi-ai 依赖） |
| `pi-chord` | `packages/chord` | 占位 | 插件宿主 / 复制状态 / JSON 链路 RPC |
| `pi-tui` | `packages/tui` | **核心可用** | ratatui + crossterm 上的组件体系：`Component` trait、多行 emacs 风格 `Editor`（词导航/kill/`\` 续行）、`ScrollView`、pulldown-cmark `Markdown`（对照 markdown.ts）、fuzzy 匹配 + `SelectList`、`Theme`（含 md 配色）、终端生命周期 |
| `pi-codemode` | `packages/codemode` | 占位 | 沙箱脚本执行，仅能调用注入的工具 |
| `pi-mcp` | `packages/mcp` | 骨架 | MCP 客户端：JSON-RPC 协议类型、transport trait（stdio/streamable-http 桩、in-memory 实现）、client |
| `pi-ai` | `packages/ai` | **核心完成** | 归一化消息模型、`AssistantMessageEvent` 流协议、模型目录、provider 适配器注册（OpenAI Chat Completions、mock；Anthropic 为桩） |
| `pi-durable` | `packages/durable` | 占位 | 持久会话 / 任务 / 文档运行时 |
| `pi-env` | `packages/env` | **本地实现** | `EnvConnection` 抽象（exec/read/write/list/watch），本地实现完整，SSH 远程为桩 |
| `pi-agent` | `packages/agent` | **核心完成** | `AgentTool` 契约、`AgentEvent`、steering/follow-up 队列、agent loop（`run_agent_loop`，对应 `agent-loop.ts`）、`Agent` |
| `pi-coding-agent` | `packages/coding-agent` | **框架完成** | bin `pi-rs`：cli/config、core（session/settings/system-prompt/model-registry/event-bus/slash-commands）、7 个内置工具、interactive（ratatui）/print/rpc 三种模式 |

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

## 路线图

1. **providers** — anthropic-messages（SSE）、openai-completions/responses（工具调用增量、reasoning content、compat 覆盖）、google-generative-ai；models 目录 hydrate 与 pi models.json 完全互通；auth.json/OAuth。
2. **agent loop 完整化** — 并行工具执行（JoinSet 预检/并发）、`prepare_request`/`finish_turn`/`prepare_next_turn`/`after_tool_call` hook、错误自动重试。
3. **session/compaction** — resume（`--continue`/`--session`）、分支树投影（`/tree`）、compaction（token 预算 + 摘要）、HTML 导出。
4. **MCP** — stdio/streamable-http transport 实装、OAuth、`mcp__<server>__<tool>` 注册进 agent、`/mcp` UI。
5. **TUI 完善** — 已完成 markdown+cmark 渲染、模型选择器浮层、footer、工具展开；剩余：keybindings.json、主题文件（theme.schema.json）、语法高亮、外部编辑器（ctrl+g）、图片粘贴、会话/设置选择器。
6. **扩展系统** — skill/提示词模板目录加载（frontmatter）、codemode 沙箱、tool-search。
7. **rpc 模式** — 命令分发接 `SessionCommand`（协议帧已就位）。
8. **外围** — pi-durable（replay 策略）、pi-env SSH、pi-chord 多机。

## 开发

```bash
cargo build          # 全工作区
cargo test           # 单元测试（session 时间戳等）
cargo run -p pi-coding-agent    # 调试运行
```
