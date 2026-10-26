# pi-rs

[pi](https://pi.dev)（[earendil-works/pi](https://github.com/earendil-works/pi)）coding agent 的 Rust 移植，完全继承 pi 的架构与逻辑。当前为**框架阶段**：分层结构、核心类型、agent loop、内置工具与 ratatui TUI 已就位并可端到端运行（默认走内置 mock provider），HTTP provider 适配器等在后续阶段补齐。

上游 TS 源码浅克隆在 `./pi/`（.gitignore 已排除），所有模块注释均标注对应的 TS 文件路径。

## 快速开始

```bash
cargo build --release
./target/release/pi-rs                 # 交互模式（ratatui TUI）
./target/release/pi-rs -p "list files" # print 模式（非交互）
./target/release/pi-rs --help
```

交互模式键位：`Enter` 发送 · `Esc` 中止运行 · `Ctrl+C` 退出 · `PgUp/PgDn` / 滚轮 翻页 · `/help` 查看命令（`/model` `/thinking` `/tools` `/prompt` `/compact` `/sessions` `/clear`…）。

mock provider 行为：普通消息回显（演示流式渲染）；包含 "list"/"ls" 的消息触发一次 `ls` 工具调用往返（演示完整 agent loop：toolUse → 执行 → 结果回传 → 二轮总结）。

## 结构映射（packages → crates）

依赖方向与 pi 根 `package.json` 的构建顺序一致：telemetry → chord → tui → codemode → mcp → ai → durable → env → agent → coding-agent。

| crate | 对应 pi 包 | 状态 | 职责 |
|---|---|---|---|
| `pi-telemetry` | `packages/telemetry` | 契约 | 遥测 span 契约（pi-ai 依赖） |
| `pi-chord` | `packages/chord` | 占位 | 插件宿主 / 复制状态 / JSON 链路 RPC |
| `pi-tui` | `packages/tui` | **可用的子集** | ratatui + crossterm 上的组件体系：`Component` trait、emacs 风格 `Editor`、`ScrollView`、`Markdown`、`Theme`、终端生命周期 |
| `pi-codemode` | `packages/codemode` | 占位 | 沙箱脚本执行，仅能调用注入的工具 |
| `pi-mcp` | `packages/mcp` | 骨架 | MCP 客户端：JSON-RPC 协议类型、transport trait（stdio/streamable-http 桩、in-memory 实现）、client |
| `pi-ai` | `packages/ai` | **核心完成** | 归一化消息模型、`AssistantMessageEvent` 流协议、模型目录、provider 适配器注册（anthropic/openai 桩 + 可用的 mock） |
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
5. **TUI 完善** — keybindings.json、主题文件（theme.schema.json）、markdown+cmark 渲染与语法高亮、模型选择器浮层、外部编辑器（ctrl+g）、图片粘贴。
6. **扩展系统** — skill/提示词模板目录加载（frontmatter）、codemode 沙箱、tool-search。
7. **rpc 模式** — 命令分发接 `SessionCommand`（协议帧已就位）。
8. **外围** — pi-durable（replay 策略）、pi-env SSH、pi-chord 多机。

## 开发

```bash
cargo build          # 全工作区
cargo test           # 单元测试（session 时间戳等）
cargo run -p pi-coding-agent    # 调试运行
```
