# pi-rs

[pi](https://pi.dev)（[earendil-works/pi](https://github.com/earendil-works/pi)）coding agent 的 Rust 移植。当前为**框架后期**：分层结构、核心类型、agent loop、内置工具与 ratatui TUI 已就位；默认使用 MiMo Token Plan 国内接口，OpenAI 兼容与 Anthropic 两大 HTTP provider 均支持增量流式。

上游 TS 源码浅克隆在 `./pi/`（.gitignore 已排除），所有模块注释均标注对应的 TS 文件路径。

## 当前能力边界

| 能力 | 当前状态 |
|---|---|
| 交互和 print 模式 | 可用于实验；内置工具调用、JSONL 会话记录（上游 v3 树格式，与 TS pi 会话文件双向互通）、`--continue`（cwd 过滤）/`--resume`、`/compact` 与阈值自动压缩、溢出自动恢复；print 支持多 prompt 顺序执行、管道 stdin 前置、`@file` 参数注入（文本文件包 `<file>` 标签）、上游退出码语义；`--mode text\|json`（json 事件流与上游逐事件对齐，差分场景验证） |
| 工具集选择 | 与上游同语义：默认激活 read/bash/edit/write（grep/find/ls 内置可选）、`-t/--tools`（裸名单替换或 `+name/-name` 修饰）、`-xt/--exclude-tools`（含 `mcp__*` 模式）、`-nt/--no-tools`、`-nbt/--no-builtin-tools`、settings `defaultTools`（两层合并）；消息 wire 与上游差分全场景一致（user/toolResult content 块数组） |
| OpenAI 兼容 Chat Completions | 增量流式（SSE）文本与工具调用、错误与取消路径；本地 HTTP mock 集成测试覆盖 |
| OpenAI Responses | 已实现：`input` 条目转换（assistant 重放带签名/复合 toolCall id）、`output_index` 槽位流处理、usage 缓存扣减、`max_output_tokens` 钳制；compat 门控路径推迟 |
| Anthropic Messages | 完整实现：SSE 流式（text/thinking/tool_use 分片）、thinking budget 映射、cache token 用量；本地 mock HTTP 测试覆盖 |
| Google Generative AI | 已实现：`streamGenerateContent?alt=sse`、thoughtSignature 重放与 thought 块切换、functionResponse 合并 user turn（Gemini 3+ 图片嵌套）、const→enum schema 重写、thinkingLevel/预算表；toolChoice/VALIDATED/重试推迟 |
| pi-messages | 已实现：`{model, context, options}` 直传 + 已序列化事件流（contentIndex 槽位、签名/redacted 重放）；gateway rewrite 诊断/debug 旗标/toolChoice 推迟 |
| Azure OpenAI Responses | 已实现：azure `api-key` 认证、URL 归一化（`/openai/v1`）、`AZURE_OPENAI_DEPLOYMENT_NAME_MAP` deployment 名解析；复用 responses 流处理 |
| Google Vertex AI | 已实现：express API key 与 ADC 双认证（service_account RS256 JWT / authorized_user refresh）、区域端点与自定义 baseUrl；metadata-server 凭据/toolChoice 推迟 |
| Mistral Conversations | 已实现：原生 Chat Completions wire（thinking 块数组、index 键控工具调用、`[tool error]` 文案、x-affinity 亲和、六拼写缓存命中）；toolChoice/reasoningEffort 推迟 |
| Amazon Bedrock | 已实现：SigV4 签名（纯 Rust，含 AWS 文档参考向量测试）与 bearer token 双认证、AWS 事件流二进制分帧解码（CRC 校验）、Converse wire（cachePoint 缓存、reasoningContent 重放/redactedContent、Claude adaptive/budget thinking、gpt-oss/GPT effort、错误前缀文案）；profile 文件凭据链/代理/HTTP2/toolChoice 推迟 |
| OpenAI Codex (ChatGPT 后端) | 已实现：SSE 路径（`{base}/codex/responses`，URL 归一化三分支）、ChatGPT OAuth JWT 认证（Bearer + JWT claim 提取 `chatgpt-account-id`）、`originator: pi`/`OpenAI-Beta`/session-id 头、instructions 承载系统提示、`text.verbosity`/`include`/`tool_choice`/`parallel_tool_calls` 固定面、response.done→completed 事件重写（end_turn/未知 status 剔除）；WebSocket 传输/zstd 压缩/重试循环/grammar 工具/service tier 推迟 |
| 其他 HTTP provider | mock provider 可用于本地演示；上游 10 个 wire API 至此全部移植 |
| 会话压缩与重试 | `/compact`、阈值自动压缩（保留最近 20k token，可关）、split-turn（超长单 turn 切分 + 双摘要合并）、mid-run 检查（`prepare_next_turn` 钩子内压缩）与上下文溢出自动恢复（先省略失败轮再选切点→压缩→重试一次）；错误自动重试（可重试错误判定 + 指数退避 + `auto_retry_start/end` 事件 + `agent_end.willRetry`，RPC 可开关/中止；支持 settings 的 `retry.enabled/maxRetries/baseDelayMs/maxAgentDelayMs`） |
| agent loop | 钩子全接线（prepare_request/finish_turn/prepare_next_turn/before-after_tool_call）、并行工具执行（start 源序→end 完成序→result 源序）、content 工具检测、length 整批失败、set_tools 公告、steering/follow-up 队列（per-queue clear/peek）；事件顺序对齐上游（turn_start 先于消息公告、assistant 盖 thinkingLevel、turn_end.toolResults 带 role） |
| 内置工具 | 与上游同参数面：edit（`edits:[{oldText,newText}]` 数组 + BOM/行尾处理）、bash（`timeout` 秒、尾部 2000 行/50KB 截断）、grep/find（`ignore` walker，gitignore-aware，参数面全齐）、ls/read（limit/50KB 截断）；read 行号前缀与 PI_RS_* 前缀为已声明差异 |
| MCP | 已接入 CLI：全局 `mcp.json` + 受信任项目的 `.pi/mcp.json`（首次 TUI 提示信任），stdio 与 streamable HTTP server，协议版本协商（LATEST 2025-11-25 + 旧版接受表）、30s 请求超时 + notifications/cancelled、ping；HTTP 服务器 OAuth（发现→PKCE 授权码/动态注册→刷新，token 存 `mcp-auth.json`，401 一次刷新重试；`oauth` 配置键；`pi-rs mcp add/remove/list/login/logout` 子命令；TUI `/mcp login|logout|reconnect` 登录流，粘贴回退仅 CLI，管理浮层/CIMD/per-server 重连推迟）；`--no-mcp` 禁用 MCP 加载和启动 |
| Skills 与提示词模板 | 已实现（对照上游 skills.ts/prompt-templates.ts）：`agentDir/skills` + 项目 `.pi/skills` + `--skill <路径>` 发现（SKILL.md 根不递归、根 .md 需 description、gitignore-aware、同名 first-wins、符号链接去重、name/description 校验 warning）、frontmatter YAML、`<available_skills>` XML 注入系统提示词（`disable-model-invocation` 排除）、`/skill:<name> [args]` 展开（settings `enableSkillCommands` 门控）；提示词模板 `agentDir/prompts` + `.pi/prompts` + `--prompt-template <路径>`（非递归 .md，description 回退首行截 60）、`/<模板> [args]` bash 风格参数替换（`$1`/`$@`/`${N:-默认}`/`${@:N:L}`）；`--no-skills`/`-ns`、`--no-prompt-templates`/`-np` 跳默认目录而显式路径仍加载 |
| 扩展系统 | 外部进程桥已接入生产路径：`-e/--extension <文件\|目录>` 可重复（`-ne` 跳过发现目录、显式 `-e` 仍加载，对照上游 noExtensions 语义）、用户 `~/.pi-rs/agent/extensions/` + 受信任项目 `.pi/extensions/` 目录发现（index.ts/index.js 入口）、每扩展一个 node 进程（Node ≥22.6，失败隔离成 notice）；注册工具并入工具表（重名跳过）、命令进斜杠表、事件面 session_start/shutdown、turn/agent 生命周期、tool_call 拦截（block/terminate）、tool_result、before_agent_start/input（只通知，结果忽略为声明差异）；扩展命令全模式执行。对话框路由：interactive 模式 `ui.select/confirm/input` 渲染 TUI 对话框并经 bridge 注册表应答（答案不走命令通道，agent 循环阻塞在钩子内也能解锁）；无 UI 模式按上游缺省解析（select/input→null、confirm→false）；`pi.sendUserMessage/sendMessage` 注入提示词（扩展命令检查与 skill/模板展开跳过、input 事件仍发）。ui.setEditorText/setStatus/setTitle 已路由（编辑器回填、按键控 footer 第三行状态项、OSC-0 终端标题；无 UI 模式 no-op；RPC 模式全部转发为 `extension_ui_request` 帧）；tool_call 处理器异常 fail-closed（对照上游 throw→error 结果）、terminate 批次 every 聚合、before_agent_start 每 run 一次（对照 _prepareRun）。声明差异：不支持 package.json manifest、sendMessage 的 custom-message 形状与 deliverAs 未建模、对话框受分发预算约束（上游无界等待）、会话替换 host 存活重发 session_start（上游重建运行时） |
| CLI 会话与资源旗标 | `--session-id`（校验/查重，存在即打开否则按该 id 新建）、`--fork <id\|路径>`（路径→本地精确→前缀→全局解析后分支新会话）、`--session-dir <dir>`（CLI > `PI_RS_SESSION_DIR`/`PI_SESSION_DIR` > settings `sessionDir`，交互 `/resume` 与选择器同根）、`--models <模式,…>`（glob/模糊作用域，picker 与 RPC 同步收窄）、`--theme <文件>` 可重复 / `--use-theme <名>` / `--no-themes`、`--export <id\|路径> [输出]`（恒为 HTML）、`--offline`、`-a/--approve` 与 `-na/--no-approve`（项目 `.pi` 资源信任覆盖）；`--tui-mode`/`--verbose` 未实现（TUI 恒全屏） |
| `--rpc` | **33/33 命令**（session 树族 new/switch/fork/clone/get_tree/get_entries/get_fork_messages/set_session_name、bash/abort_bash 透传、get_session_stats/export_html、set/abort_auto_retry）+ 事件帧含 `agent_settled{aborted}`、`bash_execution_update`、`auto_retry_start/end`、`session_info_changed`；扩展 UI 转发（`extension_ui_request` 对话框等客户端 `extension_ui_response` 应答，notify/setStatus/setTitle/set_editor_text 单向帧；`ctx.hasUI`=true 对照上游）；get_state 12 字段全集；无扩展取消钩子故 `cancelled` 恒 false；跨项目切换会关闭旧 MCP 连接并按目标项目的信任状态重连 |
| bash 工具 | 注入 `PI_RS_*` 会话环境变量、信号退出码 128+n、超限全量输出落盘、流式输出节播到 TUI 实时预览 |
| TUI 主题与键位 | 已接入配置加载：`themes/<name>.json`（settings `theme` 选择）与 `keybindings.json`，坏文件回退内置并提示；footer 统一上下文用量（压缩后显示 `?`、`(auto)` 指示符）；Markdown 围栏代码块支持 syntect 语法高亮（Sublime 语法包全覆盖，映射 Theme 的 `syntax*` 色位） |
| TUI 会话命令 | `/resume`（无参开选择器：范围/树形/搜索/删除/重命名；带 id/路径直接恢复）、`/export`（HTML 默认或 .jsonl 分支导出）、`/new`、`/name`、`/session`（上游 Session Info 面板）、`/hotkeys`（键位 markdown 表格）、`/fork`（用户消息选择器，选中后回填编辑器）与 `/fork <n>`（编号分叉）、`/clone`、`/tree`、`/import`、`/trust`（上游 TrustSelector：信任/信任父目录/不信任 + 重启提示）、`/copy`（平台剪贴板阶梯 + OSC 52）、`/settings`（settings 键选择器，全局层写盘，即时项热应用）、`/scoped-models`（模型多选作用域，会话级）、`/changelog`（内嵌 CHANGELOG 的 What's New）、`/reload`（重建键位/主题/模型目录与运行时）、`/thinking <level>`；ctrl+g 外部编辑器（TUI 挂起/恢复） |
| 剪贴板与 `@file` 图片 | 已实现：`ctrl+v` / `alt+v`（Windows）智能粘贴（macOS Finder 复制文件路径、系统截图/图片直接存入临时文件并将路径填入编辑器、普通文本回退）；魔数嗅探（png/jpeg/gif/webp/bmp/tiff + APNG 排除）、EXIF 方向、4.5MB base64 预算 Lanczos3 递缩；限额内原始字节透传 |

本项目仍在开发中，运行模型生成的 shell 命令和文件修改时，请在可信环境中使用并检查结果。

## 快速开始

```bash
cargo build --release
./target/release/pi-rs                 # 交互模式（ratatui TUI）
./target/release/pi-rs -p "list files" # print 模式（非交互）
./target/release/pi-rs --help
```

交互模式键位：`Enter` 发送（`\`+Enter 换行续写）· `Esc` 中止运行 · `Ctrl+C` 退出 · `PgUp/PgDn` / 滚轮 翻页 · `Ctrl+O` 展开工具完整输出 · `Ctrl+T` 折叠/展开思考 · 编辑器支持 readline 词导航（`Ctrl+←/→`、`Alt+B/F`、`Alt+Backspace`、`Ctrl+W/K/U/D`）。输入 `/` 弹出命令菜单（实时模糊过滤，↑↓ 选择、Enter 补全并执行、Tab 仅补全、Esc 关闭——补全优先于中止运行），`/model` 打开模糊搜索的模型选择浮层（↑↓ 选择、Enter 确认、Esc 取消）。

交互模式可使用 `/new` 新建会话、`/name <名称>` 命名当前会话、`/session` 查看当前会话、`/resume` 选择会话或 `/resume <ID或路径>` 恢复指定会话；选择器支持搜索会话名称、项目路径及用户和助手消息。`/help` 显示其他命令。恢复含 MCP 配置的未信任项目时，可以选择信任项目、跳过项目 MCP 或取消切换。跨项目切换会应用目标项目的 `defaultTools`，并保留启动时指定的工具选项。

会话树可用 `/fork`（选择一条用户消息，从其之前分支出全新会话，选中文本回填编辑器）、`/clone`（以当前叶为终点克隆新会话）、`/tree`（树形浏览条目，五种过滤模式，切换节点时可选 LLM 分支总结——三选一或自定义提示词，Esc 中止总结回树选择器）、`/import <path.jsonl>`（确认后复制进会话存储并切换）与 `/export [路径]`（`.jsonl` 导出原始分支，否则导出 HTML）。

启动时可用 `-n <名称>` 命名会话；`-r` 或无值 `--resume` 打开会话选择器（行式列表：mtime 降序，编号选择、文本过滤、空输入取消 exit 0；RPC 模式不可用）；带值 `--resume <ID或路径>` 或 `--session <ID或路径>` 直接恢复；`--session-id <id>` 指定会话 id；`--fork <id|路径>` 从既有会话分支。按 id/前缀解析到其他项目的会话时会提示 `Fork this session into current directory?`——确认则分支到当前项目，拒绝则退出；直接路径恢复保持会话原项目的设置与工作目录，原目录不存在时启动失败。

`pi-rs auth check [--provider <p>] [--model <m>] [--json] [--credentials] [--no-refresh]` 校验 provider 凭证可解析——auth.json 存储凭证优先（OAuth 凭证默认在 5 分钟窗口内自动刷新，`--no-refresh` 禁网），其次 models.json 的 `$ENV`/`!cmd` 形态与适配器环境变量；`auth print-api-key` 打印 API key（OAuth provider 报上游文案错误），`auth print-bearer-token [--min-expiry <duration>]` 打印 OAuth access token（要求刷新后剩余有效期 ≥ min-expiry，默认 30m）。OAuth 登录：TUI `/login`（provider 选择器 → 浏览器/device-code/copy-code 流，凭据落 `~/.pi-rs/agent/auth.json`）、`/logout` 移除。

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
| `pi-ai` | `packages/ai` | **部分（10 个 provider API 已实现，wire 层对齐）** | 归一化消息模型、`AssistantMessageEvent` 流协议、provider 适配器注册（anthropic-messages、openai-completions/responses、azure-openai-responses、google-generative-ai/vertex、mistral-conversations、bedrock-converse-stream、pi-messages、mock）；auth 模块（auth.json 存储锁写、PKCE/回调服务器/device-code 基础件、8 个 OAuth 流：anthropic/openai-chatgpt/openai-codex/github-copilot/openrouter/kimi-coding/xai/meta，radius 未移植）；模型目录生成物 hydration 未实现 |
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

已完成：anthropic-messages/openai-completions 增量流式、agent loop 完整化（并行工具、钩子、工具检测）、会话压缩全链路（阈值/split-turn/mid-run/溢出恢复）、MCP stdio+streamable HTTP 接入、TUI 主题/键位/语法高亮/剪贴板智能粘贴（图片/文件/文本）、工具集选择与消息 wire 对齐（差分六场景与上游快照逐字节一致）。

剩余：1. **providers** — wire 层与认证已全部完成（10 个 API + auth.json/OAuth 8 流 + `/login`/`/logout`；模型目录生成物 hydration、compat 门控路径推迟）。2. **RPC/SDK** — SDK 导出面。3. **MCP** — OAuth 登录流与 `/mcp` 命令已完成（TUI 管理浮层、CIMD、per-server 重连推迟）。4. **TUI** — 交互细节微调（会话选择器、外部编辑器 ctrl+g、设置选择器 `/settings` 与 `/scoped-models`、语法高亮、图片/文件直接粘贴已完成）。5. **扩展系统** — 外部进程桥已接入（-e/-ne、目录发现+信任门、工具/命令/事件面、TUI 对话框路由、sendUserMessage/sendMessage 提示词注入、ui.setEditorText/setStatus/setTitle/setWidget/editor、jiti 兜底非可擦除 TS）；事件顺序审计已做并修复全部 P1（docs/operations/extensions-event-order-audit.md，剩余 P2 payload 字段族）；剩余：codemode 沙箱、tool-search；`pi-rs package install/remove/list/update` 与包主题接入已完成（pi config 选择器推迟）。6. **SSH 远程环境**。
7. **rpc 模式** — 命令分发接 `SessionCommand`（协议帧已就位）。
8. **外围** — pi-durable（replay 策略）、pi-env SSH、pi-chord 多机。

## 开发

系统提示词可用 `--system-prompt <文本或文件>` 替换默认提示词，用可重复的 `--append-system-prompt <文本或文件>` 追加内容。项目上下文仍会加载；使用 `--no-context-files` 可关闭，包括切换或新建会话后。

```bash
cargo build          # 全工作区
cargo test           # 单元测试（session 时间戳等）
cargo run -p pi-coding-agent    # 调试运行
```
