# 项目质量改进交接

更新日期：2026-10-27（用户时区 Etc/GMT+7，第三轮）

## 目标与接手范围

pi-rs 是 pi coding agent 的 Rust 移植，README 定位为框架阶段。第一轮已完成连续 prompt 排队、快照补齐持久化、进程组清理、输出限幅与 CI 工作流（见 git log 与下方记录）。本轮接手后完成：严格 Clippy 门禁清零、rustfmt 基线、会话持久化加固、bash 管道滞留修复。验收围绕可靠的 agent、工具和会话关键路径，并准确说明尚未实现的能力。

工作目录为本仓库，分支 `main`。2026-10-27 再次扫描时 HEAD 为 `b4df566`，前几轮改动已分批提交；当前后续修复以 `git status` 为准。最新计划和认领状态见 `TASKS.md`；本文件保留上一轮实现记录。保留现有工作区内容，不要重置。仓库内 `pi/`、`pi-tutorial/`、`t3code/` 为独立或参考目录，不应修改。

## 本轮完成

- **质量门禁清零**：`cargo clippy --workspace --all-targets -- -D warnings` 通过。修复项：`pi-mcp` `TransportEvent::Message` 改 `Box<JsonRpcResponse>`；`pi-coding-agent` 的 `SessionEvent::Agent(Box<AgentEvent>)`、`SessionEntry::Message` Box 化、`param_str` 错误 Box 化（调用点 `return *e`）、若干 collapsible_if（用 let-chains）/`manual_clamp`/`items_after_test_module`；`pi-tui` markdown/fuzzy/preview 的机械性修复。
- **rustfmt 基线**：新增 `rustfmt.toml`（`hard_tabs = true`，匹配既有 tab 风格）；`cargo fmt --all` 全量格式化（51 文件，主要是空格→tab 归一，含新测试文件 `agent_loop.rs`）；`cargo fmt --all -- --check` 通过。
- **CI 门禁**：`.github/workflows/ci.yml` 三个 job：test / fmt --check / clippy `-D warnings`（均 `--locked`）。
- **会话持久化加固**（`session_manager.rs`）：
  - `append` 在 `write_all` 后 `sync_data()`：每条目返回即已落盘（进程/机器崩溃不丢已写条目；JSONL 仍非事务，模块文档已如实说明，勿宣称崩溃安全）。
  - 打开会话时若文件末尾无换行（崩溃撕裂的半行）自动补 `\n`，保证后续追加不粘连成无效行。
  - `count_entries` 跳过无法解析的行；新增 `#[cfg(test)] with_file` 注入失败 writer。
  - 新测试：打开缺失目录报错、重开续写保序、撕裂行不计数且追加自愈、只读句柄写失败不推进游标、in-memory 不写盘。
- **会话任务测试**（`agent_session.rs`）：3 个连续 prompt 落盘顺序断言；写失败发 Notice 且 run 不中断。
- **bash 管道滞留修复**（`pi-env/src/local.rs`）：移植上游 `waitForChildProcess` 语义——shell 退出后，若后代继承并持有 stdout/stderr，等待 EOF 改为「退出后 100ms 静默宽限，每个数据块重新计时」，静默即用已收输出收尾。`Capture` 重构为共享缓冲 + 后台任务。修复前 `sh -c 'sleep 5 & echo ok'` 会挂住 exec。
  - 清理保证范围已注释在 `ProcessGroup`：Unix 进程组 SIGKILL 覆盖同组后代；主动 setsid 脱离组的后代不覆盖；非 Unix 仅 kill 直接子进程。
  - 新测试：持管道及时返回、退出后输出继续排空、快速路径无宽限延迟。

## 验证记录（本轮实测）

| 检查 | 结果 |
| --- | --- |
| `cargo test --workspace --locked` | 通过，46 项（基线 36 + 新增 10），macOS arm64 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| GitHub Actions | test job 上一轮已写入；本轮加了 fmt/clippy job，均未在远程验证 |

## 第三轮：并行功能冲刺（2026-10-27，4 个子 agent 按crate 领地并行）

基线 `49d9ded`（第二轮已推送，远程 CI 三 job 全过）。四个互不重叠的任务包并行完成：

- **pi-ai + pi-agent（provider 流式）**：`openai_completions.rs` 重写为真增量 SSE 流式（对照上游 `openai-completions.ts`）：`SseDecoder` 增量帧解析（跨 chunk 断行、`[DONE]`）、文本 delta → MessageUpdate、tool call 按 index/id 分片累积并移植 TS `repairJson`（畸形参数记 `arguments_error` 不中断）、usage/finish_reason 收敛、错误路径携带 partial 内容、abort 即时生效。修复一处真实 bug：Error 事件的 `tx.send` 缺 `.await` 被静默吞掉。新增 2 个本地 mock HTTP+SSE 集成测试文件（证明首增量先于响应结束到达）。anthropic.rs 仍为 fail-fast 桩，留待后续。
- **pi-coding-agent（会话恢复 + RPC）**：`--continue`/`--resume <id|路径>`（`SessionSelector`：精确 id、唯一前缀、完整路径；`validate_session_dir` 校验 header，失败不留半写）；`restore_initial_messages` 过滤 system（恢复时由 Agent 按当前环境重新播种，对齐上游 `_rebuildSystemPrompt`）；恢复后追加不重复落盘有测试。RPC 从桩（49 行）到 ~700 行：prompt/steer/follow_up/abort/set_model/set_thinking/get_state/get_messages/get_available_models/get_available_thinking_levels + 事件帧（Agent 事件 serde camelCase、Notice 帧），EOF 时 abort 在跑的 run 并等落盘；`run_rpc(handle, registry)` 已接 main.rs；与上游 wire 的偏差列在模块文档。
- **pi-mcp（stdio + 工具桥）**：stdio transport 完整实现（行帧 16 MiB 上限、stderr 持续排空留 64 KiB 尾部、close 走 stdin→宽限→SIGTERM→SIGKILL 组、drop 守卫防孤儿）；`list_tools` 补 nextCursor 分页、`call_tool` 按上游 wire 形态；`McpTool` 桥实现 `pi_agent::AgentTool`（`mcp__<server>__<tool>` 命名、content 块映射、isError 传播）。**顺带修复框架期 bug：protocol 类型全部缺 camelCase rename（`nextCursor`/`inputSchema`/`isError` 等 snake_case 上 wire），与任何真实 MCP server 都无法互通**，已加 rename 并有真进程往返测试。
- **pi-tui（主题 + 键位，库级）**：`theme.rs` 按上游 theme.schema.json（vars 变量引用解析 + 环检测、`#rgb/#rrggbb`、0–255 索引、`""`→Reset、oklch/okhsl——完整移植 oklab.ts，测试期望值用上游 TS 实际运行生成）；`keys.rs`/`keybindings.rs` 移植 chord 解析与 47 项默认键位表、rebuild 覆盖/禁用/冲突语义。未接入 CLI。

**集成验证（主会话实测，全工作区）**：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked` 全部通过，**124 项测试**（第二轮 46 → 124）。第三轮改动已分批提交至 `b4df566`。README 能力表已同步更新。

**交接事项**：并行存在另一个会话在本工作区产出 `TASKS.md`（任务认领清单）、`docs/`（484 行文档树）并编辑了本文件指向 TASKS.md——非本轮 4 个 agent 所为；JSONL 示例已改为 `header`；任务清单状态以 `TASKS.md` 为准。

## 第四轮：下一阶段冲刺（2026-10-27，4 个子 agent 按 crate 领地并行）

基线 `b4df566`（第三轮已推送，远程 CI 三 job 全过）。四个互不重叠的任务包并行完成：

- **Anthropic Messages provider 完整实现**（`crates/pi-ai/src/providers/anthropic.rs`，对照上游 `anthropic-messages.ts` + `simple-options.ts`）：/v1/messages 请求 wire（system 顶层折叠、tools 有序回放、thinking budget 按 ThinkingLevel 映射、ephemeral cache breakpoint、x-api-key/anthropic-version 头、key 走运行时 env）；专用 SseDecoder 处理 message_start/content_block_delta（text/thinking/input_json）/message_delta/stop 全事件；tool_use 按 block index 累积（open_blocks map，交错 index 正确）；stopReason/usage（cache_read/cache_creation/ephemeral_1h/thinking_tokens）忠实移植 mapStopReason；错误路径全带 partial、abort 即时生效。`json_repair.rs` 从 openai_completions 提取为共享模块。17 个新测试（13 provider 集成 + 4 agent loop 集成，本地 mock HTTP+SSE）。
- **Agent 历史替换 API + 会话压缩**（`pi-agent/src/agent.rs` + `pi-coding-agent/src/core/compaction.rs` 新模块）：`Agent::replace_messages`（运行中拒绝，对齐上游"只压缩空闲 agent"）与 `Agent::stream_fn`；压缩核心对照上游 compaction.ts——SUMMARIZATION_PROMPT 逐字移植、总结调用用 agent 自身 model+stream_fn、error/length 停止原因拒绝落盘、tool call 拒绝；`SessionEntry::Compaction`（wire `{type:"compaction", summary, firstKeptEntryId, tokensBefore, id}`，对齐上游 keep-nothing）；接线 /compact（运行中拒绝、压缩中 prompt 排队）；恢复时 compaction entry 重建 summary 为普通消息。测试：prompt→压缩→再 prompt→恢复全往返。未做：自动阈值触发、branch-summarization。
- **MCP 接入 CLI**（`core/mcp_config.rs` 新 + `core/tools/mod.rs` + main.rs）：`~/.pi-rs/agent/mcp.json` 装配（逐项校验镜像上游、坏文件非致命）、10s/server 预算、失败隔离成 notice、`McpAssembly::shutdown` 在三条退出路径执行（rpc 返回/print exit 前/interactive 返回后）；pi-mcp 补 `McpClient::close()`。冒烟：好 server 连接 + 坏 server 隔离 + 正常回包。
- **TUI 主题/键位接线**（`core/ui_config.rs` 新 + modes/interactive.rs + pi-tui 小补充）：上游实际形态为 `themes/<name>.json` + settings `theme` 字段（非 theme.json，已对齐上游）；keybindings.json 覆盖喂 `KeybindingsManager`（上游静默回退，pi-rs 改 notice 送达，注释标明分歧）；interactive 硬编码键位改为 `resolve_global_action` 纯函数分发（app.model.select ctrl+l 等接通），overlay 列表键走 `handle_key_with`；pty 冒烟验证主题色值/键位覆盖/坏文件回退全链路。

**跨 agent 集成（主会话）**：MCP/resume 启动 notice 统一按模式分发（interactive 进 `UiConfig.notices` 聊天通道，print/rpc 走 stderr）。**全工作区门禁：fmt/clippy -D warnings/test 全过，199 项测试**（124 → 199）。第四轮改动未提交，等用户指令。README 能力表已同步。

**交接事项**：`bash-executor.ts`/`powershell.ts`（上游 bash 工具的 stdin transport 与 Windows 路径）未移植；MCP 剩余：streamable HTTP/OAuth、项目级 `.pi/mcp.json`、env 展开；压缩剩余：自动阈值、branch-summarization。

## 第五轮：阶段五冲刺（2026-10-28，4 个子 agent 并行：压缩完整化/MCP 完整化/RPC 补齐/TUI 渲染）

基线 `c26d4b4`（第四轮已推送，远程 CI 三 job 全过）。主会话先预置跨包接口（`SessionCommand::SetAutoCompaction`、`SessionHandle.auto_compaction`、`core/context_usage.rs`、agent_loop 的 `ToolExecutionUpdate` 接线——此前 on_update 恒 None），再并行派出。注：上一轮派出时因并发限额看似失败的任务实际已启动，与本轮新批次在同一批文件相遇；两批 agent 均按"不覆盖、在现状之上补齐"收敛，最终由新批次收尾（见 A/B 报告的协作记录）。

- **压缩完整化**（pi-ai types + core/{compaction,agent_session,settings_manager,context_usage}.rs）：`Usage::context_tokens()`（totalTokens 优先）；`find_cut_point`（对照 findCutPoint：user/assistant 边界、预算回退、尾部 toolResult 保护）+ `UPDATE_SUMMARIZATION_PROMPT` 增量合并 previous-summary；阈值自动压缩（默认 reserve 16384/keep 20000，settings.compaction 可配）挂钩 prompt 接受前与 agent_end 后；**溢出恢复**——`is_context_overflow`（27 个 provider 超限片段 + HTTP 413 + 静默超窗）与 `is_recoverable_length` 移植，error/length 结尾 run → ContextEdit 墓碑 + `replace_messages` 剔除失败轮 → 压缩 → 重试恰一次（日志不重复持久化）；`context_usage::compute` 统一（usage 优先 + trailing 估算，压缩后无新 usage 返回 None）。mid-run 检查与 split-turn 推迟（模块注释列明）。
- **MCP 完整化**（pi-mcp + core/{mcp_config,trust}.rs + tools/mod.rs + main.rs）：streamable HTTP transport（POST JSON/SSE 双分支、批量数组、`notifications/initialized` 后 GET 流经 mpsc 喂 pull 模型、Mcp-Session-Id 捕获回带 + MCP-Protocol-Version 自学习、404 会话过期、1s/×2/30s/5 次重连 + Last-Event-ID + retry: 字段、close DELETE）；env 展开（`${VAR}`/`$VAR`/`$$`/`$!`/`!cmd` 10s 超时 + per-load 缓存，fail-fast 应用到 stdio env 与 HTTP headers）；项目级 `.pi/mcp.json` + 信任机制（core/trust.rs：trust.json 规范化路径记录、向上就近查找、TUI y/n 提示持久化、print/rpc 非 TTY 跳过 + notice）；合并规则照上游（整体替换/仅 enabled 覆盖/无基座报错/命名冲突跨文件）。
- **RPC 补齐 + bash 增强**（modes/rpc.rs + core/tools/bash.rs + pi-env）：新命令 clear_queue/cycle_model/cycle_thinking_level/compact/set_auto_compaction/get_commands，get_state 补 autoCompactionEnabled（现有 22 个 RPC 测试）；bash 注入 `PI_RS_SESSION_ID/SESSION_FILE/PROVIDER/MODEL/REASONING_LEVEL`（main.rs 已接线 SessionToolContext）、信号退出码 128+n、超限全量输出 spill 到临时文件（修掉尾部切片写错 chunk[keep..] 的真 bug）、pi-env 增 `exec_streaming`（stdout 逐 chunk）驱动 bash 500ms 节播（首 chunk 立即）→ TUI 实时预览；现有 exec 语义（宽限/超时/进程组/60k）全保持。
- **TUI 渲染**（modes/interactive.rs）：footer 分子改调 `context_usage::compute`（None → `?/{window}`），`(auto)` 指示符，纯函数化带 5 个单测；`ToolExecutionUpdate` 追加到 running 块 output（兼容旧 content 形态），running 预览 20 行尾 + live 时长（对照上游 PREVIEW_LINES）。

**跨 agent 集成（主会话）**：main.rs 接线 BashTool 会话上下文；全工作区门禁 fmt/clippy -D warnings/test 全过，**288 项测试**（199 → 288）。冒烟：print 模式、RPC 新命令（get_state/set_auto_compaction/get_commands）端到端帧往返。第五轮改动未提交，等用户指令。README 能力表已同步（框架后期定位）。

**交接事项（明确推迟）**：扩展系统（建议换形为外部进程 + JSONL 协议，RPC 面已较全，可作下一阶段）；MCP OAuth（上游约 1900 行 TS 且依赖 pi.dev 端点）；RPC session-tree 族（fork/clone/get_tree/get_entries/switch/new_session）、bash/abort_bash 透传、export_html/get_session_stats、prompt images/streamingBehavior；压缩 split-turn 与 mid-run 检查；bash stdin transport 与 powershell（仅 Windows/legacy WSL）；clear_queue 的 per-queue 归属与 isCompacting（待 pi-agent 暴露 per-queue peek / compacting 标志）。

## 未完成与建议分工

1. **提交状态**：第二轮 `49d9ded`、第三轮 `b4df566`、第四轮 `c26d4b4` 均已推送；**第五轮（压缩完整化/MCP 完整化/RPC 补齐/TUI 渲染）改动在工作区未提交**，建议按任务包分批提交，遵循用户指令执行。
2. **CI 远程验证**：第四轮三 job 已全过。第五轮改动 push 后需再确认。
3. **产品能力剩余**（README 路线图）：扩展系统（外部进程 + JSONL 协议形态）、MCP OAuth、RPC session-tree 族与 bash 透传、压缩 split-turn/mid-run 检查、SSH 远程环境。
4. **持久化下一步**：事务性同步持久化（崩溃安全）仍是有意不做；若产品需要，设计 WAL 或 double-buffer 方案。会话恢复 `read_session_messages` 已能跳过撕裂行、处理 compaction 与 ContextEdit 墓碑 entry。
5. **进程生命周期边界**：setsid 脱离组的后代不会被清理（已文档化）；若产品需要，可考虑退出后扫描 reap。`ExecResult.timed_out` 恒为 false（超时走 `Err(Error::Timeout)`），bash 工具里的 `timed_out` 分支是防御性死代码，可在超时改为携带部分输出时一并清理。

## 恢复命令

```bash
git status --short
git diff --stat
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

不要读取或复制 `.env` 中的凭据到交接材料、日志或测试。后续提交、推送和部署需遵循用户指令；本次没有执行这些操作。

## 第六轮：P1 核心运行时与持久化（2026-10-28，主会话预置 + 4 个子 agent 并行 + 主会话集成）

基线 `08dddc3`（P0 审计已推送，远程 CI 三 job 全过）。按 build-plan-v1.4 的 P1 执行：主会话先预置跨包接口（pi-agent 的 async `PrepareNextTurnFn` + `Agent::set_prepare_next_turn` + `queue_modes()`；agent_session 的 `SessionEvent::Settled` + `SessionHandle.compacting`），再派 4 个按领地并行的子 agent：

- **A（pi-agent + pi-ai）**：4 钩子接线（prepare_request/finish_turn/before-after_tool_call，类型此前只有定义）、并行工具执行（JoinSet：start 源序→end 完成序→result 源序、terminate 语义）、工具检测放宽（content 过滤替代 stop_reason==ToolUse）、length 整批失败（上游文案）、set_tools 公告（toolsAdded/toolsRemoved system 消息）、per-queue clear/peek、`duration_ms` 单调钟盖章、`ThinkingEnd.content` 字段（pi-ai 类型 + 两 provider 发送点，顺带修 openai 路径丢弃 finish() 事件的真 bug）。测试 56→75。
- **B（session/compaction/agent_session）**：**会话格式上游 v3 兼容**——header `type:"session"` + uuidv7 + parentId 树 + 11 种 entry（custom 字段名改 customType）+ `buildContextEntries` 投影 + **读上游文件**（v3 直读、v1→v2→v3 迁移管线逐字移植）；旧 Rust 格式明确报错不迁移；`--continue` cwd 过滤；split-turn 压缩（findProjectedCutPoint + TURN_PREFIX_SUMMARIZATION_PROMPT + 双摘要合并，移除 user 边界钳制）；mid-run 压缩（MidRunHook 装在预置钩子上：阈值→压缩→entry 先落盘→abort→transcript 交换→continue_run，`SharedLog` Arc<Mutex> 防竞争、EventBusWeak 防泄漏）；溢出恢复改"先省略后选切点"（等价上游 suffix advancement，顺带修掉恢复永不触发的真 bug）。测试 50→72，modes/main.rs 零适配行。
- **C（tools + model_registry）**：七工具参数面对齐上游——edit 改 `edits:[{oldText,newText}]` 数组（prepareEditArguments 三形态兼容、BOM/CRLF、diff/patch details）、bash `timeout` 秒（无默认）+ 尾部 2000 行/50KB 截断 + structuredContent 1MB、grep（ignore crate + globset：gitignore-aware、glob/ignoreCase/literal/context/limit 默认 100）、find（fd 语义 full-path/limit 1000）、ls（limit 500/排序）、read（50KB + 续读/bash fallback 提示）；models.json 改**上游对象格式**（键=provider id、apiKey `$ENV`/`!cmd` 展开、modelOverrides、maxTokens 默认 16384，旧数组过渡兼容）；新增依赖 ignore/globset/uuid。测试 149→230。
- **D（pi-mcp + rpc.rs）**：MCP 协议版本协商（LATEST 2025-11-25 + SUPPORTED 表 + 校验）、30s 请求超时 + `notifications/cancelled`、ping 双向（TransportEvent 拆 Request/Notification）、Tool 透传建模（annotations/title/outputSchema/_meta）；RPC get_state 补 isCompacting/steeringMode/followUpMode、clear_queue 接 per-queue API、thinking 级别按模型能力裁剪、**发现并修复预置 agent_settled 的时序缺陷**（settle 需等 agent_rx 排空，避免先于 agent_end/插入下一 run）。测试 pi-mcp 30→39、rpc 24→31。

**主会话集成**：print 模式对齐上游（多 prompt 顺序执行、管道 stdin 前置、只打最后一条 assistant、error/aborted/length→exit 1、模式选择含 stdout 非 TTY）；新会话启动写初始 `model_change`/`thinking_level_change` entry；差分框架扩到 4 场景（多轮工具、阈值压缩，runner 支持 prompts 数组 + models.json 对象格式），归一化规则新增 system entry 丢弃与 usage.reasoning 缺省=0（均有据）。

**验证（本轮实测）**：`cargo test --workspace --locked` **426 项全绿**（288→426）、fmt/clippy -D warnings 干净；差分四场景双侧通过，规范视图差异 88→**39 且全部为两个 P2 结构类别**（默认工具集 4v7、user/toolResult content 形状），压缩 SUMMARIZATION_PROMPT 逐字一致、会话格式互通实证。

**交接事项（明确推迟）**：user/toolResult 消息 content 块数组化（pi-ai 类型层 wire 对齐，P2 首选）；CLI `--tools`/`--mode json`/`@file`；RPC session-tree 族与 bash 透传；MCP OAuth；扩展系统；assistant thinking_end 帧的 `content` 现含完整思考文本（wire additive 变化）。改动未提交，等用户指令。

## 第七轮：P2 wire 对齐清零（2026-10-29，主会话单线，worktree 分支 `p2-wire-cli`）

基线 `fdadd3e`（docs 提交，P1 已推送）。本轮在独立 worktree `/Users/andi/Documents/ChatGPT/pi-rs-p2`（分支 `p2-wire-cli`）执行，目标是清掉差分报告中仅剩的两个结构性差异类别（24 处工具集 + 15 处 content 形状 = 39 处）：

- **bash 截断 spill 文件名防碰撞**（`core/tools/bash.rs`，带入主工作区一个未提交修复）：`pid + 纳秒时间戳 + 进程内计数器` 取代 `pid + now_ms`——并行工具执行下两笔截断同毫秒落盘会互相覆盖「Full output」路径。
- **user 消息 content 块数组化**（`crates/pi-ai/src/types.rs`）：`UserMessage::text` 改产 `[{type:"text",text}]`（上游 CLI 的 prompt/steer/follow-up/压缩摘要统一形状，`agent-session.ts` 实证）；`UserContent` 保持上游 `string | blocks` untagged 联合，新增 `UserMessage::raw` 承接字符串半边（上游文件往返）。会话条目序列化与两个 provider 的请求体随类型自然对齐；toolResult 本就是 `Vec<TextOrImage>` 无需动。差分 15 处 content 差异清零。顺带修正 anthropic wire 测试旧断言（user content 裸字符串 → 块数组，上游 Messages API 本要求块数组）。
- **工具集选择**（`core/tool_selection.rs` 新模块 + `settings_manager.rs` + `main.rs`）：默认激活集改上游 `DEFAULT_TOOL_NAMES = ["read","bash","edit","write"]`（7 件套定义保留、按选择激活）；移植 `isToolModifier`/`getToolListError`/`applyToolModifiers`/`mergeDefaultTools`/`resolveDefaultTools`/`createToolNameMatcher`（`*` 通配、其余字面量）与 `createAgentSession` 的解析块；CLI 新增 `-t/--tools`、`-xt/--exclude-tools`、`-nt/--no-tools`、`-nbt/--no-builtin-tools`（三个上游组合短旗标经 argv 预处理接 clap），settings 新增 `defaultTools`（两层合并）；无效 `--tools` → 诊断错误 exit 1。MCP 侧按上游 `_allowlistFiltersMcp` 语义：allowlist 为空或含 `mcp__` 条目时才过滤 MCP 工具，`-xt` 恒作用于 MCP。差分 24 处 toolNames 差异清零。

**验证（本轮实测）**：`cargo test --workspace --locked` **444 项全绿**（426→444，+18）、fmt/clippy -D warnings 干净；`difftest` 四场景（单轮/ bash 往返/多轮工具/阈值压缩）规范视图 **4/4 IDENTICAL**——与上游快照 `f9daca34` 的 wire 级语义差异清零（system entry 不持久化与 usage.reasoning 缺省两项归一化等价处理保留在案）。CLI 冒烟：`-t "bash,+grep"` 报错 exit 1、`-nt` 别名生效、`--help` 旗标齐。注意：`cargo test` 不刷新 `target/debug/pi-rs`，跑差分前先 `cargo build -p pi-coding-agent`。

**交接事项（明确推迟）**：CLI `--mode json`/`@file`；RPC session-tree 族与 bash 透传、export_html/get_session_stats；MCP OAuth；扩展系统；SSH 远程。worktree 分支 `p2-wire-cli` 未合并、未推送，等用户指令。主工作区 `main` 上同款 bash.rs 修复仍以未提交状态存在——合并或 rebase 前先处理（内容与本轮 `094826d` 相同，可 stash drop）。
