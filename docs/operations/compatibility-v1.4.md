# Pi 1.4 兼容矩阵（P0 审计）

> 【CI 注记 2026-11-06】ubuntu-latest 镜像漂移（Python 3.14 + Linux 7.0 pty 行为）令未改动的 pty/node 测试在 CI 失败（基线 9cf57fa 复现）——workflow 已钉回 ubuntu-24.04；pty harness 移植后评估解钉。此前 2026-11-08 第十九轮增量（第四十五轮）：**MCP 登录交互流**——pi-mcp 新增 `oauth::sign_in`（上游 extensions/mcp/oauth.ts `signInMcpServer` 全编排：回调端口复用已注册 redirect 的阶梯、重登录 keep-client 判定、step-up 挑战 scope 合并、浏览器回调与手动粘贴重定向竞速、RFC 9207 iss 校验、完成页送达）；`mcp.json` HTTP 条目放开 `oauth` 配置键（clientId/clientSecret【`${}`/`!cmd` 解析同 headers】/callbackPort/callbackUrl/scope/clientName/authServerMetadataUrl，`clientRegistration: "cimd"` 声明推迟）；CLI 子命令 `pi-rs mcp add/remove/list/login/logout`（cli.ts 全量：add 选项装配与错误提示、remove 跨 scope 提示、list 文本/`--json` 与退出码语义、login 探针→登录→重连、logout）；会话内 `/mcp [login|logout|reconnect [server]]`（bare 出状态行、login 后全量重连并经 `set_tools` 声明公告刷新工具表）。声明差异：TUI 管理浮层（ui.ts showMcpManager）未做（bare /mcp 出状态）、TUI 粘贴回退仅 CLI（上游 ui.input）、per-server 重连→全量重连、`list` 无 resources/toolExposure 列、登录回调服务器完成页竞态修复（pi-ai 关页先于送页的缺陷顺带修复）。此前 2026-11-06 第十八轮增量（第四十四轮）：**RPC 扩展对话框转发**——`ctx.hasUI` 对 RPC 打开（对照上游）；对话框（select/confirm/input）经 `extension_ui_request{id,method,...}` 发往客户端、`extension_ui_response{id,value|confirmed|cancelled}` 在命令分发前截获并经 bridge 注册表解析（未知 id 静默消费）；notify/setStatus/setTitle/set_editor_text 为单向 request 帧（notify 以 ExtensionNotify 事件模式感知路由——interactive→notice、RPC→帧、无 UI 回退 notice 通道）；host shim 补回 setStatus 的 key（TUI 升为按键控状态、footer 按 key 排序）。声明差异：confirm 帧的 message 恒空串、timeout 选项缺失（shim 不传）、setWidget/editor 随 shim 面未桥接。此前 2026-11-06 第十七轮增量（第四十三轮 D）：**MCP streamable HTTP OAuth**——发现（挑战解析/受保护资源+AS 元数据/根回退/issuer 校验）、授权码+PKCE+RFC 7591 动态注册、刷新（轮换保留/scope 继承/invalid_grant 恢复）、存储 mcp-auth.json（legacy 键迁移、跨进程刷新锁）、transport 每请求 Bearer + 401 一次刷新重试；coding-agent `connect_one` 已接 `StoredTokenProvider`。声明推迟：交互式浏览器登录流（预置 token+刷新可用）、`oauth` 配置键仍拒（随登录流放开）、GET 流 401 恢复。此前 2026-11-06 第十六轮增量：**ui.* 面完整化 + jiti 兜底 + 事件顺序审计与 P1 修复**——`ui.setEditorText/setStatus/setTitle` 路由为 fire-and-forget TUI 事件（编辑器回填、footer 第三行状态项【host shim 丢 key 为单槽位】、OSC-0 终端标题；无 UI no-op 对照 runner.ts:337-348）；spawn 阶梯三环（原生剥离→strip 旗标→jiti 经 createRequire 从扩展目录向上解析，`PI_EXT_LOADER` 显式协议，帧协议零变化）——上游 loader.ts:570-584 无条件 jiti，strip-first 为移植等价物；事件顺序审计（extensions-event-order-audit.md：顺序一致，P1×3 已修复——tool_call 异常 fail-closed【throw→error 结果，agent-loop.ts:780-786】、terminate 批次 any→every【shouldTerminateToolBatch】、before_agent_start 每请求→每 run【_prepareRun】+payload 补 messageType/images；P2×12 字段缺失族与 P3×7 待认领）。此前 2026-11-06 第十五轮增量：**扩展对话框 TUI 路由 + sendUserMessage 提示词注入**——interactive 模式 `ui.select/confirm/input` 经 `SessionEvent::ExtensionDialog` 渲染 TUI 对话框（select 光标列表/confirm y-n/input 单行编辑，模态于全部浮层之上、多请求排队），经 bridge 注册表应答（`answer_dialog`，答案不走命令通道——agent 循环阻塞在 `tool_call` 钩子内时对话框也能解锁）；无 UI 模式（print/json/rpc，`has_ui:false`）按上游无 UI 缺省解析（select/input→null、confirm→false），并修掉此前未应答调用挂到超时的缺陷（现在显式回 null/false 帧）；`pi.sendUserMessage/sendMessage` 经 `SessionCommand::ExtensionPrompt` 注入提示词（上游 `prompt(text,{source:"extension"})`、expandPromptTemplates 缺省 false——扩展命令检查与 skill/模板展开跳过、input 事件仍发；host shim 只携带文本，custom-message 形状与 deliverAs 未建模为声明差异）。声明差异：对话框受分发预算约束（上游无界等待）。【第十六轮修订：ui.setStatus/setTitle/setEditorText 已在十六轮路由，见顶部增量】此前 2026-11-04 第十四轮增量：**扩展系统外部进程桥接入生产路径**——`-e/--extension`/`-ne`（-ne 语义对照上游：跳过发现目录、显式路径仍加载）、用户+受信任项目扩展目录发现（信任门与 mcp.json 共用决策）、每扩展一 node 进程（失败隔离 notice）；注册工具并入工具表（重名跳过）、命令进斜杠表（prompt() 顺序扩展命令最先，全模式）、事件面 session_start/shutdown/turn/agent 生命周期/tool_call 拦截（block/terminate）/tool_result/before_agent_start+input（只通知）；声明差异：ui.select 等对话框答复 null、无 package.json manifest、会话替换 host 存活重发 session_start。此前 2026-11-03 第十三轮：扩展宿主 PoC 6/6 成功标准实证（外部进程桥可行）；第十二轮：skills 与提示词模板全量移植。下方原始审计和统计保留当时快照，不代表当前逐项状态。2026-11-03 第十二轮增量：**skills 与提示词模板全量移植**——skills.ts/prompt-templates.ts/frontmatter.ts 逐字对照（发现规则/校验/冲突 first-wins/gitignore-aware/符号链接去重、`<available_skills>` XML 注入、`/skill:<name>` 与 `/模板` bash 风格参数替换展开、`--skill`/`--prompt-template`/`-ns`/`-np` CLI 面、settings `enableSkillCommands`）；SlashCommandSource 的 prompt/skill 变体首次通路。扩展宿主兼容实验设计定稿（docs/operations/extensions-host-experiment.md：推荐外部进程桥，PoC 6 条成功标准待执行）。此前增量（2026-11-03 第十一轮）：交互命令收尾 `/copy /settings /trust /hotkeys /changelog /reload /scoped-models`、CLI `-r` 选择器/跨项目 fork 提示（行为变化）/auth 子命令；2026-10-31 第十轮：CLI 旗标清零与会话树命令。下方原始审计和统计保留当时快照，不代表当前逐项状态。

审计日期：2026-10-28。上游基线：本地快照 `f9daca3480732d0042b1df3a6e27221f597a620d`，各包声明版本 1.4.0。**tag 核实结论**：上游仓库存在 `v1.4.0` 发布 tag（指向 `4a080252`），但快照 SHA **不是**该 tag 的提交——快照领先 v1.4.0 恰好 72 个提交、落后 0 个（tag 之后 "Add [Unreleased] section for next cycle" 的开发态）。因此：

- 本轮审计与差分测试以快照 SHA `f9daca34` 为冻结基线（GitHub 上可追溯）；
- **不宣称与 v1.4.0 发布构建兼容**，与发布 tag 的差距待后续以 compare/差分单独审计。

Rust 基线：`4106a9c`（第五轮冲刺已推送，远程 CI 三 job 全过），工作区干净。本矩阵是**源码盘点 + 差分实测**，全部条目链接源码与测试；状态含义：**已验证** = Rust 有行为测试背书；**部分** = 有实现但缺面/有偏差；**未实现**；**接受差异** = 有意偏离且已在代码注释声明；**排除** = 上游声明不算缺口。

## 明细文档

| 领地 | 文档 | 能力点 | 已验证 | 部分 | 未实现 | 接受差异/排除 |
|---|---|---|---|---|---|---|
| providers、消息模型、目录认证、agent loop、队列 | [compat/ai-agent.md](compat/ai-agent.md) | 46 | 13 | 12 | 14 | 7 |
| CLI、运行模式、RPC、退出码、SDK | [compat/cli-rpc-sdk.md](compat/cli-rpc-sdk.md) | ~44 CLI + 33 RPC 命令 + 23 事件 + ~150 SDK 符号 | CLI ≈18%、RPC 命令 58%、事件 ≈43%、SDK 0% | | | |
| 会话格式、压缩、内置工具、slash、配置 | [compat/session-tools-config.md](compat/session-tools-config.md) | 73 | 15 | 23 | 33 | 7 |
| TUI、MCP、env、外围包、扩展系统 | [compat/tui-mcp-env-periphery.md](compat/tui-mcp-env-periphery.md) | 67 | 10 | 16 | 35(+4 骨架) | 2 |

源码盘点之外，差分测试（见下）提供了 wire 级行为证据。

## 差分测试发现（P2 收敛，2026-10-29）

框架：`difftest/`（场景脚本 + 回放服务器 + 归一化对比），上游 bundle `1.4.0`（快照 SHA 构建）vs `target/debug/pi-rs`。**四个场景全部双侧通过，且规范视图逐字节一致（4/4 IDENTICAL）**——P0 首轮 31 处 → P1 集成 39 处（两类结构差异）→ **P2 清零**：

| # | 原差异 | 处数 | P2 修复 |
|---|---|---|---|
| 1 | 默认工具集：TS 默认 4 工具（bash/edit/read/write），Rust 固定 7 个 | 24 | 默认激活集对齐上游 `DEFAULT_TOOL_NAMES`（`core/tool_selection.rs` 移植 `settings-manager.ts` + `sdk.ts` 解析）；`-t/--tools`（裸名单替换 / `+name/-name` 修饰，不可混用、修饰符禁通配）、`-xt/--exclude-tools`（`*` 通配）、`-nt/--no-tools`（连 MCP 一并过滤）、`-nbt/--no-builtin-tools`（仅清内置）、settings `defaultTools`（两层合并：裸名单替换、纯修饰追加）全部按上游语义落地 |
| 2 | user/toolResult 消息 content 形状：TS 块数组（toolResult 保留尾换行），Rust 纯字符串 | 15 | `UserMessage::text` 改产上游 CLI 形状 `[{type:"text",text}]`（prompt/steer/follow-up/压缩摘要全部覆盖，对照 `agent-session.ts` `_queueSteer`/`_queueFollowUp` 与 prompt 组装）；`UserContent` 保持上游 `string \| blocks` 联合（untagged 双向反序列化，`UserMessage::raw` 保留字符串半边），会话条目与 provider 请求自然对齐 |

P1 交付在差分中的实证（保留）：SUMMARIZATION_PROMPT 与压缩 summary 前缀**逐字一致**；会话格式 v3 树互通后 `type:"session"`/entry 链对齐（含启动时 `model_change`/`thinking_level_change`）；归一化规则中两项等价处理有据：system entry 不持久化（Rust 恢复时重建 system prompt，已知差异）与 usage.reasoning 缺省视为 0（TS 恒序列化 0）。toolResult 的 content 形状在 P1 时已一致（Rust `ToolResultMessage.content` 本就是 `Vec<TextOrImage>`）。

历史首轮发现（P0 时点，现已修复的不再列出）：默认工具集（上表 #1）、消息 content 形状（上表 #2）、会话文件完全不兼容（已由会话格式 v3 落地修复）。

## 第八轮扩面（2026-10-29）：json 模式差分 + RPC 33/33

差分框架扩到 **6 场景全部 IDENTICAL**（原 4 个 print 场景 + 新增 `05-json-single-turn`/`06-json-tool-call`，对比 `--mode json` 的逐事件流）。归一化新增规则（均有据）：system 消息 start/end 对与 agent_end.messages 中的 system 丢弃（同会话视图规则）；`message_start` 只比 role（上游 spread 活引用、内容为消费时序伪影）；流中 `message_update.usage` 不比（本地单包回放下上游解析器跑在消费者前面，真实网络两侧一致于末帧）；`agent_settled.aborted`、`agent_end.willRetry`、`thinkingLevel`、`turn_end.toolResults` role 标签、`toolcall_end.toolCall` type 标签已按上游形状补齐。事件顺序对齐：`agent_start → turn_start → 消息公告`（steering/follow-up 入栈即时、公告随续轮 turn_start）。

RPC 补齐到 **33/33**：session 树族（new_session/switch_session/fork/clone/get_tree/get_entries/get_fork_messages/set_session_name，底层叶指针写入模型 + create_branched_session 重链）、bash/abort_bash 透传（bash_execution_update 流帧 + bashExecution entry）、get_session_stats、export_html、set/abort_auto_retry（错误重试引擎：可重试判定 + 指数退避 + willRetry，与溢出恢复互斥）。声明偏差：无扩展取消钩子故 cancelled 恒 false；新分支会话目录布局为同级目录（上游平铺同目录）。跨项目切换时 MCP 按目标项目的信任状态重连，未信任的项目配置跳过。CLI 补 `--mode text|json|rpc` 与 `@file` 注入。

## 源码盘点发现的高风险项（P1 候选，按风险排序）

1. **agent loop 钩子未接线**：prepare_request/finish_turn/prepare_next_turn/before-after_tool_call 只有类型定义，loop 零调用；parallel 工具执行未实现；工具检测比上游窄（`stop_reason==ToolUse` vs content 过滤）。README/handoff 的「核心完成」表述需修正。
2. **会话树缺失**（parentId/branch//tree//fork//clone）——会话能力与上游差距的结构性根源；split-turn 压缩、mid-run 压缩检查、branch-summary 均被它阻塞。
3. **MCP 协议版本固定 2025-06-18**（上游首选 2025-11-25 + 接受表 + 版本校验）且**请求无超时/取消**——服务器挂起会永久阻塞。
4. **RPC 缺 `agent_settled` 事件**（上游客户端生命周期锚点）；33 命令缺 14，served 8 个有形状偏差；rpc.rs 模块注释漏报 2 个 get_state 字段。
5. **edit 工具 schema 不兼容**（edits 数组 vs old_string/new_string）；bash `timeout` 秒 vs `timeout_ms` 毫秒；grep/find/ls 参数面缩水且不读 .gitignore。
6. **models.json shape 不兼容**（数组 vs 上游对象）+ maxTokens 默认 8192 vs 16384；settings 键面 7/~40；CLI 参数覆盖 ≈18%；`--mode json` 缺失；管道 stdin 不读内容。
7. **文档口径修正**：README pi-ai「核心完成」→ 2/10+ provider 已验证；pi-agent「核心完成」→ 钩子未接线；handoff「Agent 事件 serde camelCase」→ 实为 rpc.rs 运行时补丁。

## 门禁与可复现性（2026-10-28 实测）

| 检查 | 结果 |
|---|---|
| `cargo test --workspace --locked` | 288 通过 / 0 失败（macOS 26.6.2 arm64，rustc/cargo 1.98.0） |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| 远程 CI（GitHub Actions，`4106a9c`） | fmt/clippy/test 三 job 全过 |
| 上游构建（快照副本） | `npm ci` + `generate-models --data-only` + `build:offline` 成功，bundle `--version` = 1.4.0 |
| 差分框架 | `difftest/` 双侧跑通，首轮 31 处发现 |

此结果不证明其他平台、真实 provider 或完整兼容；按矩阵逐项收敛。

## 阻塞项与下一步（负责人待用户指派）

1. 修正 README/handoff 能力口径（直接可做）。
2. P1 任务包候选（矩阵已给出证据与文件锚点）：agent loop 钩子+并行工具+工具检测、会话树与上游 session 格式、MCP 版本协商+超时取消、RPC agent_settled+缺命令、工具 schema 对齐（edit/bash/grep/find/ls）、models.json 对象格式。
3. 差分框架扩面：RPC 模式场景（事件顺序）、`--mode json`、anthropic-messages provider 场景、溢出/压缩场景。
