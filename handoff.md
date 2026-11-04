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

## 第八轮：RPC 33/33 + CLI json 模式（2026-10-29，主会话预研 + 2 个子 agent 并行 + 主会话集成）

基线 `b0a1ede`（P2 已推送，远程 CI 三 job 全过）。目标：RPC 补齐到上游 33 命令全集、CLI `--mode json`/`@file`、HTML 导出。

- **A（session_manager/agent_session/rpc.rs）**：会话树写入模型（叶指针 branch/reset_leaf、get_tree 森林投影、create_branched_session 重链 parentSession——新会话为同级目录属声明偏差）；会话替换命令（new_session/switch_session/fork/clone，任务内 abort→重建工具与 system prompt→原位换 SharedLog，rpc 探针自动跟随；cancelled 恒 false、MCP 不重建为声明偏差）；bash/abort_bash 透传（pi-env 执行、bash_execution_update 流帧、bashExecution message entry 对齐上游 messages.ts、excludeFromContext 只落盘不进上下文）；auto-retry（可重试错误判定移植、指数退避、auto_retry_start/end、agent_end.willRetry 经共享队列配对、与溢出恢复互斥）；get_session_stats/set_session_name/get_entries{since}/get_fork_messages/get_tree/get_state 12 字段。修掉真 bug：uuidv7 前缀目录名同毫秒碰撞→随机 v4 段。
- **B（json_event/export_html/print.rs/main.rs）**：`--mode json`（json-event.ts 逐字移植：message_update 剥 partial、toolcall_start 补 id/toolName；session header 首行；退出码 json 恒 0 照上游）；resolveAppMode 移植（--mode rpc/json/text、--rpc 保留旧入口）；`@file` 注入（file-processor.ts 文本分支、stdin→@file→prompt 单消息拼接、缺失 exit 1、图片走上游失败文本形态）；export_html.rs（上游实为客户端渲染——按同 DOM 结构服务端化：ansi-to-html 全移植 SGR/256/RGB、工具折叠块按 template.js 行数、HTML 转义防注入；dark 主题离线求值硬编码）。
- **主会话集成**：export_html 接 RPC（export_html 响应帧 `{path}`）；`SessionEvent::Settled{aborted}` 补上游 agent_settled.aborted；json 路径补 agent_end.willRetry；assistant 消息盖 `thinkingLevel`（agent-loop.ts:419）；toolcall_end 的 toolCall 加 `"type":"toolCall"` 标签；TurnEnd.toolResults 改带 role 的消息数组；**事件顺序对齐上游**——`agent_start → turn_start → 消息公告`（原先公告在 turn_start 前，steering/follow-up 消息入栈即时但公告推迟到续轮 turn_start 后，保证 mid-run 压缩 abort 时消息不丢）；`SessionManager::create` 丢失 header 字段导致 json 模式无 header 行的真 bug。
- **difftest 扩面**：新增 05-json-single-turn / 06-json-tool-call 两场景（canonicalJsonEvents 归一化：system 消息对丢弃同会话视图规则、message_start 内容为活引用时序伪影故只比 role、流中 usage 为本地单包回放伪影故只在 message_end/agent_end 比、agent_end 滤 system）。**六场景全部 IDENTICAL**（4 print + 2 json）。

**验证（本轮实测）**：`cargo test --workspace --locked` **521 项全绿**（444→521）、fmt/clippy -D warnings 干净；difftest 6/6 IDENTICAL。已知推迟：RPC session 替换后 MCP 工具表不重建（待扩展阶段）；retry 的 maxRetries/baseDelay 待 settings 扩键；RPC 模式差分场景（事件顺序已由 json 场景覆盖主要面）；`--mode json` 的 stdout 背压/信号处理；@file 图片附件。

改动已分批提交至 main（等用户指令推送）。

## 第九轮：TUI 会话选择器（2026-10-31，主会话单线）

基线 `fc8e942`（上游 `/resume` + `SessionSelectorComponent` + `session-selector-search.ts` 全量移植）：

- **会话列举**（`core/session_manager.rs`）：`SessionInfo`（path/id/cwd/name/parentSessionPath/created/modified/messageCount/firstMessage/allMessagesText，对照 session-manager.ts:229 `buildSessionInfo`）+ `list_sessions_in`/`list_sessions_progress`（每 10/100 个发布排序部分列表；pi-rs 单一 sessions 根目录，"当前文件夹"范围按 header cwd 匹配 = `paths_equivalent`，即上游自定义 dir 时的 `sessionCwdMatches` 语义）；`parse_iso_timestamp_ms`（`iso_timestamp` 的逆，含 `Z`/`±HH:MM` 偏移）+ `append_session_info_to_file`（任意会话文件追加 `session_info` 条目，空名清除——选择器 rename 路径，活动会话仍走 `SessionManager::append_session_info`）。
- **搜索与树**（新模块 `modes/session_selector.rs`）：`parse_search_query`（`re:` 正则大小写不敏感、`"phrase"` 短语、引号失衡回退空白分词）、分层评分（name 命中 > id/messages/cwd 文本命中 > 散乱 fuzzy，`TIER_WEIGHT=1e6`，复用 pi-tui `fuzzy_match`；小写语料随列表缓存）、`filter_sessions`（recent 只滤保序 / relevance 按分+最近平局）、`build_session_tree`/`flatten_session_tree`（parentSession 系缘树，子树最新活动排序，`│ ├ └` 前缀；canonicalize 缺失路径回退原串）。
- **选择器浮层**（同模块 `SessionSelectorOverlay`）：纯状态机 + 渲染（键处理返回 `SelectorAction`，文件变更与会话切换由 interactive 执行）。范围 Tab 切换（current/all，按需加载 + Loading n/t）、排序 ctrl+s 循环（Threaded/Recent/"Fuzzy"=relevance）、命名过滤 ctrl+n、路径显示 ctrl+p、删除确认 ctrl+d（活动会话拒绝；trash CLI 优先、失败回退 unlink、已消失视为成功——上游 `deleteSessionFile` 语义）、重命名 ctrl+r（预填现名）、ctrl+backspace 空查询快捷删除；↑↓/pgup/pgdn/enter/esc + 其余键进搜索行（与 model overlay 一致硬编码编辑键）；标题行（范围◉/○ + Name + Sort + 进度）、双 hint 行、状态行（删/改名结果 2s/3s 过期）、空态文案、`(i/n)` 滚动指示、选中行 REVERSED+粗体、命名行 warning 色（用 tool 色位）、当前会话 accent 色。
- **接线**（`modes/interactive.rs`）：`/resume` 命令 + `app.session.resume`（默认无键，可经 keybindings.json 绑定）经 `resolve_global_action` 打开浮层；列表加载走 `spawn_blocking` + 类型化 mpsc（`SessionListMsg::Progress/Loaded/Status`，过期范围消息丢弃）；选中 → `SessionCommand::SwitchSession`（复用 RPC 轮会话替换），`session → …` notice 到达时从 `live_agent` 快照重建聊天块（`blocks_from_messages`：assistant toolCall 与 toolResult 按 id 配对，孤立 call 空输出、孤立 result 跳过；usage 重算自消息）；TUI 探针全部改 `SessionHandle::live_agent()`（新 pub 方法，替换后 model/snapshot/tool_names 观察当前会话）+ `current_session_file()`/`session_name()`。删除/改名在 `spawn_blocking` 执行后重载当前范围。
- **声明偏差**：浮层浮于聊天区（上游全屏容器）；重命名/删除后整列表重载而非原地更新单行；relevance 排序的语料逐会话缓存于浮层（上游 WeakMap）；删除/改名结果无 `trash` 时直接 unlink（macOS 无预装 trash 常见）。

**验证**：576 项测试全绿（521→576，+55：查询解析/分层评分/树投影/浮层状态机 15 项、SessionInfo/ISO/列举/rename 4 项、块重建/usage/删除/键位分发 4 项等）；fmt/clippy -D warnings 干净。pty 端到端冒烟：`/resume` 打开浮层、行渲染（命名/首条消息/计数/时长）、搜索过滤、Down 光标移动（帧 diff 实证）、Enter 切换（`session → ` notice + 干净退出，连跑 4 次 exit 0）。注意：ratatui 光标寻址帧经 naive 去 escape 拼接会产生幻影词，冒烟断言要用帧 diff 或状态（会话文件/进程退出码）验证，勿用拼接文本。

**交接事项**：设置选择器（settings-selector.ts）未做；树选择器（tree-selector.ts）未做；`--continue` 启动时 TUI 仍不回放历史（仅切换/替换后重建）；`fork`/`new_session` 尚无 UI 入口（RPC 已有）；上游 `/rename` 无独立斜杠命令，rename 仅在选择器内。

## 第十轮：外部编辑器 ctrl+g（2026-10-31，主会话单线）

基线 `c0ab58e`（上游 `external-editor.ts` + `getExternalEditorCommand` + `ui.stop()/start()` 生命周期移植）：

- **pi-tui**：`TerminalGuard::suspend/resume`——suspend 走 `restore_terminal`（退备屏、关 raw/鼠标/键盘 flags），resume 重进并 `Terminal::clear()` 强制全量重绘（备屏重进后是空白，不清屏会花屏）。
- **interactive**：`app.editor.external`（ctrl+g）→ `GlobalAction::EditorExternal` 只置 pending 标记；run() 循环在 draw 后处理：先停输入读取器（`spawn_term_reader` 可重启化——EventStream 存活会与编辑器抢键盘，select! 对 pause 通道退出即 drop 流），再 `guard.suspend()`，打两行提示，spawn 后台任务执行编辑器；结束帧经 `editor_rx` 回到主循环 resume + 重启读取器 + `editor.set_text`（失败不改内容，同上游）。`run_external_editor`：`pi-editor-<pid>-<nanos>` 临时目录写 prompt.md、空白分词命令 + stdio inherit、成功读回并 `strip_editor_text`（BOM + 恰一个尾换行）；命令不存在/非零退出/读不回都按失败处理（上游读失败会抛未捕获，移植降级为 failed）。命令解析 `resolve_external_editor`：settings `externalEditor` → `$VISUAL`/`$EDITOR` → nano（经 UiConfig.external_editor 传入，settings 管理器不进 interactive）。
- **声明偏差**：上游 Windows `shell:true` 未移植（直接 exec）；上游 spawn error 静默，移植同样静默不改内容。

**验证**：581 项测试全绿（576→581，+5：BOM/尾换行剥离、脚本编辑器往返、非零退出与坏命令失败、ctrl+g 分发、配置优先级）；fmt/clippy -D warnings 干净。pty 冒烟：EDITOR=脚本 → ctrl+g → 两行提示 → 编辑内容进 editor → 提交后会话文件含编辑后文本。教训延续：pty 冒烟用会话文件断言状态，勿用去 escape 拼接文本。

## 第十一轮：openai-responses provider（2026-10-31，主会话单线）

基线 `9b5bc57`（上游 `api/openai-responses.ts` 453 行 + `openai-responses-shared.ts` 891 行的 Rust 移植首片，路线图 providers 项）：

- **新模块 `pi-ai/src/providers/openai_responses.rs`**：POST `{base}/responses`（bearer + session_id/x-client-request-id 亲和头）；请求 wire（`input` 条目数组、`store:false`、`prompt_cache_key` 64 字符截断、`max_output_tokens` 最低 16 钳制、temperature、function tools 无 strict、`reasoning:{effort,summary:"auto"}` + `include:["reasoning.encrypted_content"]`、Off→`effort:"none"`）；消息转换（系统消息塌缩为单条头部条目——非 reasoning 模型 role=system、reasoning 模型 role=developer，工具集从全部 system 的 toolsAdded/Removed 解析 current；assistant 文本→`message` 条目（textSignature `{v:1,id,phase}` 重放 id/phase，回退 `msg_pi_N[_K]`，>64 截断）；thinking 带 signature→原始 reasoning item JSON 重放；toolCall→`function_call`（`call_id|fc_*` 复合 id，非 fc 前缀/外来 id 省略 item id 跳过配对校验）；toolResult→`function_call_output`（纯文本或 input_text/input_image 数组，按模型视觉能力））。
- **流处理**（`ResponsesSession`）：`output_index` 键控槽位（thinking/text/toolCall），事件映射 reasoning_summary_text/reasoning_text/summary_part.done(`\n\n`)/output_text/refusal delta、function_call_arguments delta/done（`ToolArgsStream`：lenient 部分预览 + replace 续传/重启 + finish 全量解析出 arguments_error，复用 json_repair）、output_item.done（reasoning: summary||content 文本 + 原始 item 作 thinkingSignature；message: 文本 + textSignature 编码；function_call: 替换+finish+namespace）、`phase:"final_answer"`→stop、response.completed/incomplete→usage（input 减 cached/cache_write）+ rawStopReason + mapStopReason（max_output_tokens→length，其余 incomplete→error，failed/cancelled→error，in_progress/queued→stop）+ 有 toolCall 时 stop 升级 toolUse；response.failed/error 事件→流失败；无 terminal event、toolUse 带未完成调用→流失败（上游 post-loop 守卫）。
- **接线**：`providers/mod.rs` 分发 `Api::OPENAI_RESPONSES`；model_registry 的 api 校验白名单加入；`SseDecoder` 改 pub(crate) 复用。
- **声明偏差（compat 门控路径，上游默认关闭，推迟）**：grammar/custom 工具（`custom_tool_call`）、additional_tools/tool_search 条目、configuration_update（thinkingLevelMap 也未建模）、ChatGPT 登录特判、copilot/xai 头、service-tier 定价、长缓存 retention/24h、OpenRouter 会话亲和；cost 计算沿用全库零值口径；`sanitizeSurrogates` 在 Rust UTF-8 下为 no-op；上游 Windows shell:true 未移植。

**验证**：597 项测试全绿（581→597，+16：wire/effort/off/重放/事件映射/incomplete/failed/未完成调用/视觉输出 9 项单测 + 6 项 mock HTTP+SSE 集成——增量 delta、429 错误体、response.failed、无 terminal 断连、abort、请求 wire 断言）；fmt/clippy -D warnings 干净。坑：mock 服务器 content-length 必须与 body 等长（reqwest 严格解码）；chunked body 需 `0\r\n\r\n` 终止才走干净 EOF 路径，否则 reqwest 报 decode error。

## 第十二轮：@file 图片附件（2026-10-31，主会话单线）

基线 `8351a39`（上游 `cli/file-processor.ts` 图片分支 + `utils/{mime,image-process,image-resize-core,exif-orientation}.ts` 的 Rust 移植）：

- **新模块 `core/image_process.rs`**（依赖 `image` crate 0.25，features png/jpeg/gif/webp/bmp；`base64`）：魔数嗅探 `detect_supported_image_mime_type`（jpeg 0xFFD8FF 非 F7、png 含 IHDR 校验 + APNG acTL 排除、GIF87a/89a、RIFF/WEBP、BMP 头校验——逐字移植 mime.ts，取前 4100 字节）；EXIF 方向（JPEG APP1/ WebP EXIF chunk → TIFF IFD 0x0112，8 种方向变换——image crate 无 transpose，5/7 用 rotate90/270+fliph 组合，与上游逐位一致）；`process_image`（限额内 2000×2000 且 base64 < 4.5MB：png/jpeg/gif/webp **原始字节透传**（上游 `resizeImageInProcess` 限额分支返回 inputBytes，EXIF 不烘焙——保真镜像），其余类型解码+定向+重编码 PNG 带转换提示；超限：Lanczos3 缩至 2000 内 → PNG + JPEG[80,85,70,55,40] 候选取第一个低于预算 → 0.75 递缩到 1×1，尺寸注记 `Multiply coordinates by …`）。
- **Prompt 链路**：`SessionCommand::Prompt` 增 `images: Vec<ImageContent>`；session 任务 pending 队列改存 `(text, images)` 元组，四条 pop_front 出队路径统一经 `user_message_with_images`（无图 → `Message::user(text)`，有图 → `UserContent::Blocks` 文本+图片块）；print 模式 `run_print` 增 `initial_images`（首条 prompt 携带，上游附在 messages[0]）；interactive 经 `UiConfig.initial_images` → `pending_images` 首次 send_prompt 携带；main.rs `process_file_arguments` 返回 `(text, images)`——图片按 `<file name="…">hints</file>` 文本引用 + 附件入列，失败走上游 `[Image omitted: …]` 文本形态。
- **声明偏差**：透传路径不烘焙 EXIF（上游同行为，旋转显示仅在重缩/转换时生效）；上游 Photon WASM 用原生 image crate 替代（Lanczos3 同款滤波）；上游保留 jpeg/gif/webp 原始容器的字节，端口同（不做容器归一化）；图片只进首条 prompt（上游同），steer/follow-up 仍纯文本。

**验证**：605 项测试全绿（597→605，+8：嗅探/APNG/TIFF 方向读取/透传/重缩注记/auto-resize off/garbage/EXIF 变换 + main.rs 图片参数接线 2 项）；fmt/clippy -D warnings 干净。坑：mock 夹具三连——APNG 夹具漏 IHDR CRC、TIFF IFD entry 的 count 是 4 字节（SHORT 值在前 2 字节）、JPEG APP1 标记是 FF E1 两字节（u16 to_be_bytes 会写成 00 E1）。

## 第十三轮：google-generative-ai provider（2026-10-31，主会话单线）

基线 `4fb7db4`（上游 `api/google-generative-ai.ts` 466 行 + `google-shared.ts` 574 行的 Rust 移植首片）：

- **新模块 `pi-ai/src/providers/google_generative_ai.rs`**：POST `{base}/models/{id}:streamGenerateContent?alt=sse`（`x-goog-api-key` 头，base 缺省 v1beta，上游 baseUrl 已含版本路径）；请求 wire（`contents` 条目、leading system→`systemInstruction.parts`、工具→`functionDeclarations` + `parametersJsonSchema`（**const→单值 enum 重写**，name-map 旗标只作用当层键名）、Gemini 3+ 离散 `thinkingLevel` vs 2.5 系 `thinkingBudget` 表（pro/flash-lite/flash，无表 → -1 动态）、Off→`thinkingBudget:0`、`generationConfig` temperature/maxOutputTokens）。
- **消息转换**（`convert_contents`）：user text/inlineData；assistant 文本带 thoughtSignature 重放（仅同 provider+model 且合法 base64），空文本仅在有签名时保留（Gemini 签名回显规则）；同模型 thinking→`thought:true` 条目、跨模型→纯文本（防模仿）；functionCall 仅 `requiresToolCallId`（claude-/gpt-oss-/gemini≥3）带 id；toolResult→`functionResponse`（output/error 键）合并进单个 user turn，Gemini 3+ 图片嵌套 parts、<3 单独 user turn；`mapStopReason`（STOP→stop、MAX_TOKENS→length、其余安全/其他原因→error）+ 有 toolCall 时 stop 升级 toolUse。
- **流处理**（`GoogleSession`）：`thought:true` 驱动 text/thinking 块切换（块尾事件关闭）；thoughtSignature 首个非空保留（`retainThoughtSignature`）；functionCall 缺 id/重复 id → `{name}_{now}_{counter}` 合成；usage（prompt − cached、output + thoughts、reasoning=thoughts）；无 finishReason→流失败；safety 等 error 原因→`Provider stopped with: REASON`。
- **声明偏差**：toolChoice/constrained-sampling VALIDATED/useParameters（Cloud Code Assist）/请求重试/thinkingLevelMap（Xhigh/Max 钳到 High）推迟，均上游默认关闭或不可达；cost 沿用零值口径。

**验证**：621 项测试全绿（605→621，+16：wire/thinking 配置分族/工具结果合并/跨模型 thinking/签名重放/Gemini3 id/流切换/合成 id/usage/空文本回显/inlineData 11 项单测 + 5 项 mock HTTP+SSE 集成——跨 chunk 增量、429、无 finishReason、SAFETY→error、abort）；fmt/clippy -D warnings 干净。model_registry api 白名单扩入 google-generative-ai。

## 第十四轮：/export 命令（2026-10-31，主会话单线）

基线 `9397cf9`（上游 `interactive-mode.ts handleExportCommand` + `core/session-export.ts` 移植）：

- **新模块 `core/session_export.rs`**：`serialize_session_branch`（新导出头 `type:"session"` + 同 id/cwd + 分支条目 parentId 重链——对照上游 `serializeSessionBranch`，getBranch=build_session_path）、`export_session_to_jsonl`（默认名 `session-<ISO>.jsonl` 解析到 cwd、嵌套父目录创建；内存会话也可导出）。
- **interactive `/export [path]`**：`.jsonl`→JSONL 分支导出；其余（含无参默认路径）→HTML（`write_session_html`，内存会话报 "Cannot export in-memory session to HTML"）；参数支持成对引号剥离；成功/失败均走聊天通知（上游 showStatus/showError）。
- **验证**：623 项测试全绿（621→623，+2：分支重链序列化、默认/显式路径导出）；pty 冒烟：无参 HTML 导出落盘 + `/export out/session.jsonl` 嵌套路径导出成功；fmt/clippy -D warnings 干净。RPC `export_html` 命令与 CLI 共用 `write_session_html`，行为一致。

## 第十五轮：/new 命令（2026-10-31，主会话单线）

基线 `097ea20`（上游 `handleClearCommand` / `app.session.new` 的斜杠入口）：`/new` 发送 `SessionCommand::NewSession`（排队 prompt 由替换逻辑丢弃），"New session started" 通知 + "session → " 通知触发既有聊天重建；新会话文件落盘经 pty 冒烟实证。623 项测试维持全绿（无新增单测——命令入口由 pty 冒烟覆盖），fmt/clippy 干净。会话命令组现状：/new ✅、/resume 选择器 ✅、/export ✅、/tree 与 /fork 需树/消息选择器（推迟）。

## 第十六轮：/copy 命令（2026-10-31，主会话单线）

基线 `20c5674`（上游 `handleCopyCommand` + `utils/clipboard.ts` 首片）：

- **新模块 `core/clipboard.rs`**：平台命令阶梯（macOS pbcopy / Windows clip / linux 按 env：termux-clipboard-set、wl-copy、xclip、xsel）+ OSC 52 回退（SSH/mosh/PI_CLIPBOARD_OSC52 远程会话与 headless Linux，100k base64 上限）；WSL PowerShell 互操作与原生 FFI 写入推迟。`copy_to_clipboard` 错误文案对齐上游。
- **interactive**：`/copy` 命令 + `app.message.copy`（ctrl+x）→ 从 live agent 快照倒序找末条非空助手文本复制；无消息报 "No agent messages to copy yet."；成功通知。alt-screen 选区分支无对应 API（推迟）。实现时踩坑：slash_commands.rs 的 fmt 单行 info 使多行锚点替换静默失配——冒烟发现菜单缺命令后核对 git status 才补上。
- **验证**：626 项测试全绿（623→626，+3 clipboard 单测）；pty 冒烟错误路径（空会话提示），成功路径依赖 pbcopy 旁路（单测覆盖阶梯选择）；fmt/clippy 干净。

## 第十七轮：/name 命令（2026-10-31，主会话单线）

基线 `93ee2eb`（上游 `handleNameCommand`）：`/name` 显示当前显示名、`/name <name>` 经 `SessionCommand::SetSessionName` 设置（RPC 同款命令；清洗后的名字异步落盘，端口回显请求名并注明与上游同步读回的偏差）。626 项测试维持全绿，pty 冒烟 set+show 双路径，fmt/clippy 干净。

## 第十八轮：/thinking <level> 参数（2026-10-31，主会话单线）

基线 `6dee54a`（上游 `handleThinkingCommand`）：`/thinking <level>` 按模型能力（reasoning → off..max，否则仅 off）直接 `SessionCommand::SetThinking`，未知级别报 "Unknown thinking level ... Available levels: ..."（上游文案）；裸 `/thinking` 维持循环。626 项测试维持全绿，pty 冒烟 set（thinking → Low）+ 未知级别错误路径，fmt/clippy 干净。

## 第十九轮：/session 信息命令（2026-10-31，主会话单线）

基线 `0115b24`（上游 `handleSessionCommand` + `getSessionStats`）：统计逻辑从 rpc.rs 抽出为 `core/agent_session::compute_session_stats`（消息计数 + assistant/toolResult/usage 条目的用量求和），RPC `get_session_stats` 与 interactive `/session` 共用；`/session` 渲染 Name/File/ID、消息计数、Token 总量与 cached/uncached 拆分（上游 cache-warming 与按模型分解节无对应物，声明推迟）。**事故与恢复：用 `s.index("\n\tfn ")` 找方法边界时吞掉了 rpc.rs 后续方法与测试嵌套，靠 `git checkout -- rpc.rs` 还原后按真实方法边界（首个 `\n\t}\n`）重做手术式替换**——大块文本替换前先确认锚点唯一性。626 项测试维持全绿（RPC 45 项含 stats 聚合仍过），pty 冒烟渲染完整信息块，fmt/clippy 干净。

## 第二十轮：/hotkeys 命令（2026-10-31，主会话单线）

基线 `dc085d1`（上游 `handleHotkeysCommand`）：`/hotkeys` 渲染解析后的键位两列对齐表（Navigation/Editing/Other 三组 24 行，经 `keybinding_hint` 走 keybindings.json 覆盖；Notices 是纯文本行渲染故用对齐文本而非上游 markdown 表）。626 项测试维持全绿，pty 冒烟（表格 + 已解析键位 esc/ctrl+l），fmt/clippy 干净。

## 第二十一轮：/fork 首片（2026-10-31，主会话单线）

基线 `844a248`（上游 `showUserMessageSelector` 的最小可用替身）：`/fork` 无参列出 transcript 中的 user 消息（1 基编号 + 60 字符预览），`/fork <n>` 定位该消息的会话条目并发 `SessionCommand::Fork { position: Before }`（分支根在消息父节点，消息文本经替换 notice 回流供预填——上游 selectedText 语义）；交互式选择器 UI 推迟。**坑：/fork <n> 是 1 基索引，0 基取值会静默错位（pty 冒烟实证）。** 626 项测试维持全绿，pty 冒烟列表 + 分叉落盘（新分支会话文件），fmt/clippy 干净。

## 第二十二轮：retry settings 键（2026-10-31，主会话单线）

基线 `d207bd6`（第八轮遗留：maxRetries/baseDelay 进 settings.json）：`Settings` 增 `retry: Option<RetrySettings>`（camelCase maxRetries/baseDelayMs，字段级 project>global 合并，两层都无 → None → 引擎常量默认）；`RetryRuntime` 增 `max_retries`，自动重试引擎的上限与退避基数从 settings 读取（WireEvent max_attempts 同步）；`spawn_agent_session_with_settings` 增 RetrySettings 参数，main 传入 `settings.retry`。**事故：`shared.retry.lock().unwrap().attempt < shared.retry.lock().unwrap().max_retries` 同一 std Mutex 单表达式双锁死锁，cargo test 挂起——改单守卫块。** 627 项测试全绿（+1 解析/合并测试），fmt/clippy 干净。上游 retry.enabled 与 maxAgentDelayMs 仍是运行时开关（settings 键未接）。

## 第二十三轮：/sessions 升级（2026-10-31，主会话单线）

基线 `9e5b996`：`/sessions` 从 dir+条目计数 stub 升级为 SessionInfo 摘要（年龄右对齐 + 名字或首条消息预览 70 字符 + 消息计数，`list_sessions_in` 全量列举取最新 10；上游无此命令，pi-rs 扩展对齐新管线）；core/mod.rs 的未用 SessionManager 再导出移除。627 项测试全绿，pty 冒烟（名字/预览/时长——注意冒烟目录会积累每次运行的空会话，截断断言前先清理），fmt/clippy 干净。

## 第二十四轮：/import 命令（2026-10-31，主会话单线）

基线 `bcc6c07`（上游 `handleImportCommand`，去确认对话框）：`core/session_export.rs` 增 `import_session_from_jsonl`（校验首行 session 头 → 新建目录（新 session id 防双目录同 id——声明偏差，上游替换活会话）→ 逐条 append 线性重链、条目 id 保留；根/子路径错误均带文件名文案）；interactive `/import <path.jsonl>`（非 .jsonl 报 usage）导入即 `SwitchSession` 到新目录（"Session imported:" + "session → " 通知触发聊天重建，与导出/导入往返 pty 冒烟实证：2 个新目录、双通知）。628 项测试全绿（+1 导入往返），fmt/clippy 干净。坑：`items after a test module`（clippy）——追加函数落到 tests 模块后要移回前面；孤立 doc 注释残留会 parse fail。

## 第二十五轮：/trust 命令（2026-10-31，主会话单线）

基线 `a6bacef`（上游 `showTrustSelector` 首片）：`/trust` 在项目 `.pi/mcp.json` 存在且未信任时弹出两选项 SelectList 浮层（Trust / Don't trust，模型选择器同款面板）；Trust 落 trust.json（信任持久化）+ "restart to load its MCP servers"（MCP 装配在启动时已完成，与"MCP 不随会话替换重建"同口径）；无配置/已信任分别提示；Don't trust 提示保持跳过。628 项测试维持全绿，pty 冒烟（启动 y/N 拒绝 → /trust → 选 Trust → 通知），fmt/clippy 干净。

## 第二十六轮：/tree 只读视图（2026-10-31，主会话单线）

基线 `c14e9d1`（上游 `showTreeSelector` 只读首片）：`/tree` 渲染 get_tree 的条目树（`├─/└─/│` 前缀 + 每条目单行摘要：user/assistant 文本 48 字符、toolResult 工具名、其余为 type_name；多根依序）；叶切换选择器 UI 推迟。628 项测试维持全绿，pty 冒烟（--resume alpha-1 后 user/assistant 条目渲染），fmt/clippy 干净。

## 回归验证（2026-10-31，第九至二十六轮之后）

本会话 15 个 feat 全部落地后重跑差分套件：**六场景（单轮/ bash 往返/ 多轮工具/ 阈值压缩/ json 单轮/ json 工具调用）双侧 6/6 IDENTICAL（canonical view，上游快照 f9daca34）**——@file 图片、Prompt images 字段、会话导出/导入、retry settings 等改动未引入任何 wire 回归。

## 第二十七轮：/compact <instructions>（2026-10-31，主会话单线）

基线 `c14e9d1` 前后连推（上游 compaction.ts:714-716）：`SessionCommand::Compact` 增 `instructions: Option<String>`，`compact_prefix` 增第三参（None=全部既有路径），自定义指令在基础提示词后追加 `\n\nAdditional focus: …`（UPDATE 路径同）；interactive `/compact [focus]` 传参、RPC `compact` 传 None（wire 无字段）、阈值/mid-run/溢出路径均 None。629 项测试全绿（+1 提示词含 Additional focus 断言），pty 冒烟（空会话拒绝路径证明参数贯通），fmt/clippy 干净。

## 第二十八轮：RPC compact customInstructions（2026-10-31，主会话单线）

基线 `fbf056d`：`RpcCommand::Compact` 增可选 `customInstructions`（serde rename 到上游 camelCase 字段，缺省 None——additive，旧帧不受影响）并传入 SessionCommand；+1 wire 测试（"compact" 命令帧形状）。630 项测试全绿，fmt/clippy 干净。坑：字符串手术修补测试尾部两次失败后，按精确文本 replace + 手工核对括号结构修复（教训：测试插入用 Edit 工具锚定模块内位置，勿用 rindex 猜括号）。

## 第二十九轮：ctrl+p/alt+p 模型循环（2026-10-31，主会话单线）

基线 `dd953f5`（上游 `cycleModel` 的 available-models 变体）：`GlobalAction::ModelCycleForward/Backward`（ctrl+p / 平台相关 backward 弦——darwin shift+ctrl+p、其他 alt+p）接 resolve_global_action；interactive `cycle_model(±1)` 对 registry 模型列表环绕取下一个并发 `SetModel`，单模型提示 "Only one model available"；scoped-models 变体无对应（推迟）。**坑：MODEL_CYCLE_BACKWARD 是平台相关常量（darwin=shift+ctrl+p），测试用 alt+p 在 mac 上必挂；调试用 panic!() 测试用完必须删（fmt 会重排行导致正则删除失配，二次清理）。** 631 项测试全绿（+2 cycle 弦分发），pty 冒烟（单步 ctrl+p → "model → mimo/mimo-v2.6-flash" 通知），fmt/clippy 干净。

## 第三十轮：app.session.tree/fork 键位分发（2026-10-31，主会话单线）

基线 `3e039e9`：`GlobalAction::SessionTree/SessionFork` 接 resolve_global_action（默认无键，keybindings.json 可绑——如 `super+t`/`super+f`）分别触发只读 /tree 视图与编号 /fork。app.session.* 动作组至此全接线（new/resume/tree/fork）。632 项测试全绿（+1 分发测试），fmt/clippy 干净。坑：chord 解析器不认 "ctrl+meta+"，meta 用 `super+` 前缀。

## 第三十一轮：退出恢复提示（2026-10-31，主会话单线）

基线 `88b062b`（上游 `formatResumeCommand` + 退出打印）：interactive 退出（终端还原后）打印 dim 色 `To resume this session: pi-rs --resume <目录名>`——端口指向会话目录名（`--resume` 按目录名前缀解析，pty 冒烟实证解析成功）；上游 `--session <id>` 旗标与 `--session-dir` 参数无对应（自定义 PI_RS_SESSION_DIR 需用户重导出，声明偏差）；内存会话不打印。632 项测试维持全绿，fmt/clippy 干净。

## 第三十二轮：--session-dir 旗标（2026-10-31，主会话单线）

基线 `249b0c0`（上游 `--session-dir`）：CLI 增 `--session-dir <DIR>`，解析后立即写 `PI_RS_SESSION_DIR` 环境变量（edition-2024 下 unsafe set_var，单线程早期调用；`config::sessions_dir()` 已读该变量）——自定义根下的会话落盘与退出恢复提示均实证。632 项测试维持全绿（测试 Cli 构造补 None），fmt/clippy 干净。上一轮声明的偏差（恢复提示的自定义根）就此闭合。

## 第三十三轮：!cmd bash 透传（2026-10-31，主会话单线）

基线 `1e09fce`（上游 bash mode：`!cmd` 入 transcript、`!!cmd` 排除上下文）：handle_submit 识别 `!`/`!!` 前缀 → `SessionCommand::ExecuteBash`（RPC 轮底层复用）；新增 `ChatBlock::Bash`（`bash: <cmd> (status)` 头 + 输出预览，运行中尾 20 行、完成后首 10 行 + done/exit N/cancelled/truncated 状态着色）——TUI select 循环新增 `session.wire_events` 臂，`BashExecutionUpdate` 追加增量、`BashExecutionDone` 终结（bash_running 标志置回，重复提交报上游同文案警告）。Esc 中止已接：Interrupt 分流 bash_running → AbortBash（优先于 agent abort）。632 项测试维持全绿，pty 冒烟（块渲染/输出/完成状态），fmt/clippy 干净。

## 第三十轮补充：pi-messages provider（2026-10-31，主会话单线）

基线 `88b062b` 前后（上游 `api/pi-messages.ts` 456 行全量移植，路线图 providers 项）：**新模块 `pi-ai/src/providers/pi_messages.rs`**——协议最薄的适配器：POST `{base}/messages` 带 `{model, context:{messages}, options:{temperature,maxTokens,reasoning,sessionId}}`（context 走 Message serde 原样序列化，转换在后端）；响应为已序列化的 assistant-message 事件流（`contentIndex` 寻址的 text/thinking/toolcall 槽位、contentSignature/redacted、toolCall 终态覆盖 + ToolArgsStream 容错增量——自 openai_responses 提为 pub(crate) 复用），终态 `done`/`error` 携 usage/responseId/responseModel；无终态事件→流失败；HTTP 错误体 `{error:{message,code}}` 格式化。model_registry 白名单扩入 pi-messages。声明推迟：gateway rewrite 诊断（无 sidecar 字段）、debug 旗标、toolChoice、identity 头。**坑：终态事件的 events 向量在提前 return 前忘记 forward，Done 静默丢失——pty/单测都抓到了（"done event, got [Start, Error]" 形状）**。635 项测试全绿（+4：wire/索引槽位流/无终态失败 + 既有），fmt/clippy 干净。providers 完成 5/10+。

## 第三十一轮补充：azure-openai-responses（2026-10-31，主会话单线）

基线 `7bd5096`（上游 `azure-openai-responses.ts` 255 行 + `azure-openai-config.ts`，复用 openai-responses 全部流处理）：openai_responses.rs 增 `Endpoint` 参数化（OpenAI/Azure）——auth 头 bearer vs `api-key`、key env OPENAI_API_KEY vs AZURE_OPENAI_API_KEY、URL 归一化（azure 主机 `.openai.azure.com/.cognitiveservices/.ai.azure.com` 路径归 `/openai/v1`）、deployment 名（AZURE_OPENAI_DEPLOYMENT_NAME_MAP `id=deployment,..` 按 model id 查找，未映射用 id）；body 的 model 字段用 deployment 名。`azure_stream_fn()` 注册 Api::AZURE_OPENAI_RESPONSES，model_registry 白名单扩入。声明推迟：grammar 工具、toolChoice、strict-mode 差异（与 openai-responses 首片同口径；上游 azure 的 compat 默认 supportsStrictMode=true 等——端口统一无 strict）。638 项测试全绿（+3 azure：URL 归一化/deployment 映射/body model 名），fmt/clippy 干净。providers 完成 6/10+。

## 第三十二轮补充：settings retry.enabled（2026-10-31，主会话单线）

基线 `bef0cd5`：`RetrySettings` 增 `enabled: Option<bool>`（字段级合并），spawn 时播种 auto_retry 原子量（缺省仍 RETRY_ENABLED_DEFAULT=true）——settings `{"retry":{"enabled":false}}` 可关闭自动重试（此前只有 RPC 开关）。638 项测试维持全绿，fmt/clippy 干净。剩余声明缺口：maxAgentDelayMs（上游 agent 级重试上限，与 provider 级 maxRetryDelayMs 分开）。

## 第三十三轮补充：google-vertex provider（2026-10-31，主会话单线）

基线 `93742de`（上游 `google-vertex.ts` 561 行首片，复用 google_generative_ai 的 `GoogleSession`/`request_body`——process_payload 加 `process_payload_streamed` 包装）：**新模块 `pi-ai/src/providers/google_vertex.rs`**——
- **Express API-key 模式**（options.api_key 为真实 key：非空、非 `gcp-vertex-credentials` 标记、非 `<placeholder>`）：POST `https://aiplatform.googleapis.com/v1/publishers/google/models/{id}:streamGenerateContent?alt=sse` + `x-goog-api-key`。
- **ADC 模式**（key 缺失/为标记）：要求 `GOOGLE_CLOUD_PROJECT`（或 GCLOUD_PROJECT）与 `GOOGLE_CLOUD_LOCATION`；URL `https://{location}-aiplatform.googleapis.com/v1/projects/{p}/locations/{l}/publishers/google/models/{id}:...`；`GOOGLE_APPLICATION_CREDENTIALS` 支持 `service_account`（RS256 JWT 签名换 token，rsa+sha2 纯 Rust）与 `authorized_user`（refresh_token grant）两种 JSON；token 进程内缓存至过期前 60s。
- 自定义 model.baseUrl 作区域前缀（`{location}` 占位符替换）。流处理与 generative-ai 完全同构（thought 块切换/签名保留/toolcall id 合成/finishReason 映射）。
- 声明推迟：metadata-server 凭据（GCE/GKE）、toolChoice、constrained sampling、请求重试、模型目录个性化（openrouter 同款）。
- **新依赖**：pi-ai 增 base64/rsa 0.9/sha2。
641 项测试全绿（+3：express URL、placeholder 判定、RS256 签名往返），fmt/clippy 干净。providers 完成 7/10+（剩 mistral-conversations、bedrock-converse-stream、openai-codex-responses）。

## 差分回归（2026-10-31，第二十九至三十三轮之后）

google-generative-ai 流处理重构（process_payload_streamed）、google-vertex、pi-messages、azure、Prompt images、retry settings、bash 透传等改动后重跑：**六场景双侧 6/6 IDENTICAL（canonical view，f9daca34）**——继续零 wire 回归。

## 第三十四轮：mistral-conversations provider（2026-10-31，主会话单线）

基线 `0fc097a`（上游 `mistral-conversations.ts` 924 行首片）：**新模块 `pi-ai/src/providers/mistral_conversations.rs`**——
- **请求**：`{base}/v1/chat/completions`（bearer + accept SSE + `x-affinity` 会话亲和）；messages（system 文本、user text/`image_url` 块（无视觉模型图片降级文案）、assistant `{content:[{type:text|thinking}],tool_calls:[{id,type,function,index:0}],prefix:false}`、tool `{tool_call_id,name,content:[text(+image)]}` 带 `[tool error]`/图片省略文案）；tools `function.strict:false`；`max_tokens`/`prompt_cache_key` snake_case；错误体 `Mistral API error (<code>): <4000 字符截断>`。
- **流处理**：`{data:{id,usage,choices[0]:{finish_reason,delta}}}`；delta.content 为 string 或 chunk 数组（text/thinking——GLM 空 content delta 不开新块）；tool_calls 按 `index`（无 index 用派生 id）键控累积（ToolArgsStream），收尾统一 finish() 出 arguments_error；usage 缓存命中六种字段拼写、input 扣 cached；finishReason：stop→stop、length/model_length→length、tool_calls→toolUse、error→"server error" 可重试文案、其余→error。
- **声明推迟**：toolChoice、reasoningEffort/promptMode（thinkingLevelMap 未建模）、60s header 超时、header 覆盖、`shortHash`（std hasher 替代——id 仅需会话内稳定，不需与上游一致）。
645 项测试全绿（+4：wire/历史重放/块切换+index 工具调用+缓存扣减/stop 映射），fmt/clippy 干净。**坑：Mistral 事件 JSON 外层是 `{data:{choices}}`，检查层级写错会全帧报 "Invalid Mistral streaming event"。** providers 完成 8/10+（剩 bedrock-converse-stream、openai-codex-responses）。

## 第三十五轮：/fork 交互选择器（2026-10-31，主会话单线）

基线 `891db01`（上游 `showUserMessageSelector` 的 SelectList 化）：裸 `/fork` 打开浮层（每条 user 消息一项，`n. 预览` 标签，value=条目 id），Enter 发 `Fork{position:Before}`、Esc 取消；`/fork <n>` 编号路径保留；trust 浮层同款面板与键位路由。顺手清理第二轮遗留的死代码桩（users 空向量）。645 项测试维持全绿，pty 冒烟（浮层/条目/分叉落盘/`session → ` 通知），fmt/clippy 干净。坑（重申）：pty 冒烟前必须重新 cargo build——上一轮二进制会让新 feat 看似失败。

## 合并轮：origin/main（provider 轮 43 提交）并入本地会话树轮（2026-11-03）

基线 `cc7530d`（本地会话树轮 6 提交：`410c617`/`3f5619e`/`e92deeb`/`f5502d9`/`56a7d76`/`cc7530d`）与 `62e0eb5`（远程 provider 轮 43 提交：google-generative-ai / google-vertex / pi-messages / azure-openai-responses / mistral-conversations + openai-responses、@file 图片、`/compact <instructions>`、retry settings、剪贴板、外部编辑器、会话选择器、`/fork` 选择器）合并，5 个文件冲突（README.md、core/settings_manager.rs、core/agent_session.rs、main.rs、modes/interactive.rs），两侧功能全部保留：

- **agent_session.rs**：`RetryRuntime` 取并集（`max_retries` + 上游 `maxAgentDelayMs` 派生的 `max_delay_ms`）；`SessionHandle` 并入两侧方法（`configure_tool_options`/`mcp_enabled`/`configure_mcp` 与 `live_agent`/`current_session_file`/`session_name`）；spawn 三态统一为 `spawn_agent_session` → `_with_settings(+retry)` → `_with_options(+prompt_options)`；`Prompt{text,images}` 队列改用元组（图片随首条消息），树导航的 `pending` 排空改映射回文本。
- **settings_manager.rs**：`RetrySettings` 两份定义合一（保留 `max_agent_delay_ms`），`effective()` 只留一个 `retry` 字段。
- **main.rs**：`--session-dir` 取本地的 `Option<String>`（CLI > `PI_RS_SESSION_DIR`/`PI_SESSION_DIR` > settings `sessionDir`，相对路径按启动 cwd 解析），解析结果回写 `PI_RS_SESSION_DIR` 让 `config::sessions_dir()` 的调用点一致；`@file` 图片与 `prompt_options` 并入同一条启动链；主题加载取本地 `load_with_cli_themes` + 远端的 `externalEditor`/`initialImages`。
- **interactive.rs**：导入、类型（`Dialog`/`ReplacementKind`/`PendingTreeNav` + `SlashMenu`）、结构体字段、按键分发、`SessionEvent` 分支、绘制函数全部取并集；命令分发按上游语义重排（`/resume` 无参开选择器、带参直恢复；`/fork` 选择器 + `/fork <n>`；`/tree` 走交互式树选择器；`/import` 确认后复制并切换；`/export`、`/name`、`/session`、`/trust`、`/copy`、`/hotkeys` 取远端实现）。
- **合并期修掉的真实缺陷**：① 斜杠菜单的 Enter 只补全不执行（上游 `applyCompletion` 斜杠分支会继续提交）——改为补全并执行，Tab 仅补全；② 远端「session → 」通知里的 `rebuild_from_live_session` 会把替换完成后的状态通知清空（PTY 冒烟抓到 `/import` 的「Session imported from:」丢失）——改由 `SessionReplaced` 事件统一恢复 transcript；③ `core/mod.rs` 的 `pub use session_manager::SessionManager` 被远端删除导致 `crate::core::SessionManager` 不可达——调用点改用模块路径；④ `RetrySettings` 重复定义与 `Cli`/`UiConfig` 初始化缺字段（图片与外部编辑器字段）一并补齐。

验证：`cargo test --workspace --locked` 731 项全绿（含 pty 冒烟：会话命令、信任与会话选择器）、`cargo fmt --all -- --check` 干净、`cargo clippy --workspace --all-targets --locked -- -D warnings` 零警告；差分框架六场景双侧 **6/6 IDENTICAL**（canonical view，f9daca34）。

## 第三十六轮：交互命令收尾 + CLI 收尾（2026-11-03，主会话调研 + 2 个子 agent 并行 + 主会话集成）

基线 `cd38b57`（合并轮后）。上游 `/mcp` 斜杠命令经核实不存在（登录流程的 MCP 提示而已），未发明。

- **A（interactive/slash_commands/settings_manager + 新 clipboard.rs/changelog.rs + 根 CHANGELOG.md）**：`/copy`（clipboard.ts 全移植：pbcopy/clip/termux/wl-copy/xclip/xsel 梯 5s 超时、OSC52 条件（SSH/MOSH/PI_CLIPBOARD_OSC52）+100k 上限、WSL PowerShell interop、上游错误文案）；`/settings`（12 键选择器，`SettingsManager::set_overrides`+`save` 写全局层，即时项热应用 thinking/theme/treeFilterMode，其余诚实提示重启）；`/trust`（TrustSelector 移植：Trust/Trust parent folder/Do not trust，父目录写+本目录 clear，"Saved trust decision: … Restart pi-rs…"）；`/hotkeys`（KeybindingsManager→markdown 表格，ChatBlock::Doc 渲染，上游第三节名 Other）；`/changelog`（parseChangelog 移植 + include_str! 内嵌根 CHANGELOG.md + 可执行旁回退，保留上游 .reverse() 怪癖）；`/reload`（流式/压缩拒绝文案对照，重建 ui_config/registry/外部编辑器 + 会话经替换机制 Switch 到自身路径，ReplacementKind::Reload；extensions/skills/prompts 无对应为声明差异）；`/scoped-models`（all_models 多选，Tab 应用 set_model_scope，会话级声明差异）。顺带修 PTY harness 既有脆弱点：quit 等待期不读 master 会因大 Doc 全屏重绘填满缓冲卡死——改为边等边排水。
- **B（main.rs/session_manager/model_registry + 新 cli_auth.rs）**：`-r` 纯 flag 与无值 `--resume`→行式选择器（stderr 渲染保持 stdout 机器可读；RPC 拒绝；print 模式吃 piped stdin 首行）；`--session` 恢复独立 flag；跨项目 id/前缀命中→fork 提示（y→fork_from_in，n/EOF→"Aborted." exit 0）——**行为变化**：修正第九轮的静默原项目 cwd 恢复（对照上游 main.ts:399-407 global 分支；path 直取不提示，main.ts:397 语义）；`auth check|print-api-key|print-bearer-token`（clap 前拦截 argv，exit 码矩阵 0/1/2 与 help 文案逐字，无 OAuth 为声明差异）。过滤字段剔除 cwd（macOS /var/folders 含 "older" 字面误匹配）。
- **集成（主会话）**：全工作区门禁 fmt/clippy -D warnings 干净、**768 项测试全绿**（627→768）；difftest 六场景 **6/6 IDENTICAL**；README/compat 三文档同步（含行为变化标注）。

**交接事项（明确推迟）**：/share /bug（pi.dev/Radius/gist 服务）；/login /logout（OAuth）；包管理子命令 install/remove/update/list/config（P3 扩展/包系统，package-manager.ts 2791 行）；`--tui-mode`/`--verbose`/quietStartup；上游 settings 选择器未覆盖的键（steeringMode/followUpMode/quietStartup 等约 40 键中 pi-rs 未有者）；扩展系统兼容实验（P3 首任务）。改动未提交，等用户指令。

## 第三十七轮：skills 与提示词模板 + 扩展宿主实验设计（2026-11-03，主会话调研 + 1 个子 agent 纵向切片 + 主会话 spike）

基线 `5f7186a`（第十一轮后）。

- **skills + 提示词模板（子 agent 全量移植，一个纵向切片）**：`core/skills.rs`（skills.ts 逐字：SKILL.md 根不递归/根 .md 需 description/gitignore-aware 规则前缀拼接/点文件与 node_modules 跳过/同名 first-wins + collision diagnostic/canonicalize 符号链接去重/name 与 description 校验 warning 但仍加载/disable-model-invocation/formatSkillsForPrompt XML 逐字含转义顺序与头部文案）、`core/prompt_templates.rs`（非递归 .md/description 回退首行截 60/argument-hint/parseCommandArgs bash 引号/substituteArgs 全占位形态单遍不递归/expand_prompt_template）、`core/frontmatter.rs`（BOM/CRLF 归一/--- YAML --- 提取，serde_yaml 0.9 入 workspace）。通路：`SessionShared.resources` + spawn 第 7 参、Prompt 分支先 `/skill:` 后模板展开（展开文本即用户消息）、system_prompt `<skills>` 节（project context 后 cwd 前）、`dynamic_slash_commands()` 合并点（BUILTIN 静态表不动，u/p/t 源标记）、settings `enableSkillCommands`、CLI `--skill/--prompt-template/--no-skills/-ns/--no-prompt-templates/-np`。测试：单测覆盖发现/校验/XML/占位替换全形态 + `tests/skills_prompts_cli.rs` 6 条真子进程（json 模式断言展开文本/--no-skills 显式路径仍加载/坏 frontmatter warning 不崩）。声明差异：扩展命令与 input handlers 通路未移植（无 extension runner）、SourceInfo 简化为作用域标记、目录项排序确定性 first-wins（Node 为 OS 序）。
- **扩展宿主实验设计（主会话 spike，docs/operations/extensions-host-experiment.md）**：对上游 70+ 官方示例扩展实测 API 使用分布（事件 90/命令 56/工具 39/消息注入 19/UI notify 138/custom 组件 30）；宿主三选项对比，**推荐外部进程桥**（node/bun + jiti 宿主脚本，JSONL 双向转发；逻辑面+对话框原语可兼容，TUI 组件级声明降级）；桥协议草图 + PoC 6 条成功标准（hello/notify/question/todo/flag 扩展 + 失败隔离）+ 信任边界声明。**结论待 PoC 实证，不宣称兼容**。
- **集成（主会话）**：全工作区门禁 fmt/clippy -D warnings 干净、**809 项测试全绿**（768→809）；difftest 六场景 **6/6 IDENTICAL**。

**交接事项（下一轮候选）**：扩展宿主 PoC（按实验设计执行，跑通上游 6 个代表性扩展才算证明）；包管理子命令（install/remove/update/list/config，依赖 PoC 结论）；`-e/--extension`/`-ne` 旗标（PoC 后接桥）；OAuth 登录；/share /bug；`--tui-mode`/`--verbose`；codemode 沙箱与 tool-search（上游为内置扩展，pi-rs 需决定原生实现或桥接）。改动未提交，等用户指令。

## 第三十八轮：扩展宿主 PoC 通过（2026-11-03，主会话单线）

基线 `03b0f0f`。按实验设计（docs/operations/extensions-host-experiment.md §4）执行 PoC，**6/6 成功标准全部实证，外部进程桥可行**。

- 新 crate `crates/pi-extension-host`：Rust 侧 JSONL 协议（spawn/注册收集/事件分发/id 匹配调用/动作帧/失败隔离/stderr 尾桶）；零依赖 Node 宿主 `host/host.mjs`（stdout 协议化+扩展 stdout 转 stderr、运行时物化 shim node_modules + resolve 钩子映射 @earendil-works/* 与 typebox、最小 Type 构造器、pi-tui 抛错桩、Node ≥22.6 原生 TS 剥离 + `--experimental-strip-types` 重试、`pi.exec` 走 host 内 child_process）。
- 测试 7 个（tests/poc.rs）：上游原样 hello.ts（registerTool schema+execute 回路）、session-name.ts（命令+同步 API 镜像）、notify.ts（agent_settled+OSC777 stderr 转发）、permission-gate.ts（tool_call→ui.select 对话框→block/allow）+ 自制 flags/factory-throw/handler-throw（flag 面、工厂硬错误、handler 级隔离）；node 过旧自动跳过。
- 实测关键发现（写进实验文档 §6）：同步/异步 API 分界决定协议形态（同步=本地镜像+单向 action 帧）；扩展 stdout 必须重定向；不可桥 API（registerProvider/渲染器/custom UI）调用时抛声明性错误。
- 集成：全工作区 **816 项测试**（809→816，+7 PoC）、fmt/clippy -D warnings 干净、difftest 6/6 IDENTICAL。

**交接事项**：生产桥接线清单在实验文档 §6 末尾（-e 旗标/扩展目录/信任门→桥产物入会话→jiti 兜底→事件面补齐→runner 顺序审计）。改动未提交（本条 handoff 与代码一起提交），等用户指令。

## 第三十九轮：扩展桥生产接线（2026-11-04，主会话调研 + 1 个子 agent 纵向切片 + 主会话集成）

基线 `e590940`（PoC 通过并推送，CI 全绿）。

- **子 agent 纵向切片**：`core/extensions.rs`（发现顺序 项目→用户→CLI、index 入口、信任门 resolve_project_trust_gate 与 mcp.json 共用、ExtensionAssembly 逐扩展 spawn 20s 预算/失败隔离 notice/node 缺失整体禁用/teardown 三退出路径）；`-ne` 语义**纠正**：上游 noExtensions 跳过发现目录、显式 -e 仍加载（resource-loader.ts 实证）；ExtensionTool 实现 AgentTool（content/details/isError 映射、重名跳过）；before_tool_call 同步钩子经**专用 dispatcher 线程**（tokio worker 内 block_on 会 panic——实测）聚合 block/terminate；事件面 session_start(reason)/shutdown/turn/agent 生命周期/agent_settled/tool_result/before_agent_start+input（只通知，声明差异）；SessionCommand::ExtensionCommand + prompt() 顺序扩展命令最先（全模式）；ui.notify→Notice；interactive 斜杠菜单路由。
- **修掉的两个真 bug**：桥持强 EventBus 令 print/rpc 在 session task 退出后等不到通道关闭而挂起（改 weak）；handler 不 await notify 时请求完成后帧滞留队列（with_host_answers + try_next_host_call drain）。
- **集成（主会话）**：全工作区 **836 项测试**（816→836）、fmt/clippy -D warnings 干净、difftest 六场景 **6/6 IDENTICAL**（空 Assembly 不涉场景）。
- 声明差异（代码注释）：ui.select/confirm/input 答复 null（TUI 对话框路由为下轮增量）、无 package.json manifest、会话替换 host 存活重发 session_start（上游重建运行时）、tool_result 不 await、每扩展一进程 spawn 延迟 20s 预算。

**交接事项**：TUI 对话框路由 + sendMessage→prompt 注入（下轮自然增量）；jiti 兜底；包管理子命令；对照上游 runner 的事件顺序审计。改动未提交，等用户指令。

## 第四十轮：bedrock-converse-stream provider（2026-11-04，主会话单线）

基线 `62e0eb5`（上游 `api/bedrock-converse-stream.ts` 1424 行首片）：**新模块 `pi-ai/src/providers/bedrock_converse_stream.rs`**——与其他 adapter 最大的不同：不走 SSE，而是 AWS 事件流二进制分帧 + SigV4 签名，全部纯 Rust 手写（与 google_vertex 手写 RS256 同口径），新依赖仅 hmac 0.12 / crc32fast 1.4。
- **SigV4**（`SigV4::sign_request`）：canonical request（host/x-amz-date/可选 x-amz-security-token）+ HMAC-SHA256 密钥链，**AWS 文档参考向量测试钉死**（iam 2015-08-30 例，含 content-type 头）；bearer 路径（options.api_key / `AWS_BEARER_TOKEN_BEDROCK`）与 `AWS_BEDROCK_SKIP_AUTH=1` 跳过签名。
- **事件流解码**（`EsDecoder`）：prelude/headers/payload/CRC-32 校验、7 类头值类型、`:message-type`（event/exception/error）路由、`:exception-type`/`:error-message`；16MB 帧上限，尾部残帧报错。
- **区域/端点**：ARN 内嵌区域 > AWS_REGION/AWS_DEFAULT_REGION > 标准端点主机（含 -fips/.cn）> us-east-1；自定义 baseUrl 非 Bedrock 主机原样使用。modelId 路径段按 AWS SDK extendedEncodeURIComponent 严格百分号编码（`:`→%3A、`/`→%2F）。
- **Converse wire**：`/model/{id}/converse-stream`；cachePoint（系统块+末条 user，PI_CACHE_RETENTION long 加 ttl:1h，claude 判定同上游 + AWS_BEDROCK_FORCE_CACHE）；toolResult 连续合并为一条 user（claude 错误结果图片挪到整轮尾部带 caption）；assistant thinking 重放（签名→reasoningText.signature、缺失→纯文本、redacted→redactedContent base64 原样回传）；`<empty>` 占位；sanitizeBedrockDocument 滤空键。
- **additionalModelRequestFields**：claude adaptive（opus-4-6+/sonnet-4-6+/5 系，block_binding 仅 4-7+）vs budget（默认预算表 1024/2048/8192/16384 + interleaved-thinking beta）、effort 映射（xhigh 仅 4-7+/5 系原生）、GovCloud 省 display/block_binding；gpt-oss 平铺 reasoning_effort、gpt- 嵌套 reasoning.effort。
- **声明推迟**：HTTP/2（reqwest 走 HTTP/1.1，Bedrock 接受）、代理 agent、profile 文件/SSO/IMDS 凭据链（缺环境变量对即显式报错）、toolChoice、strict 模式、requestMetadata、onPayload/onResponse 钩子、calculateCost（与其他 adapter 一致置零）、sanitizeSurrogates（Rust String 无孤代理）。
646 项测试全绿（+22：SigV4 参考向量/session-token 头/路径编码/UTC 日期/区域解析/事件流分帧+CRC/session 事件全链/redacted 累积/stop 映射/错误前缀+保留模式提示/wire 断言×5/mock HTTP 全链路——本地 TcpListener 回放二进制帧，断言请求行+auth 头+事件序列+usage），fmt/clippy 干净。**坑：①测试 JSON payload 手写括号层数（4 层闭合）极易数错，改用 raw string 或 json! 宏；②model_match_candidates 同时匹配 id 与 name，测试改 id 不改 name 会仍走 adaptive 分支；③AWS SigV4 参考向量的 canonical request 带 content-type 头，漏掉则签名对不上。** providers 完成 9/10+（仅剩 openai-codex）。差分回归：六场景 6/6 IDENTICAL（f9daca34）。

## 第三十七轮：openai-codex-responses provider（2026-11-04，主会话单线）

基线 `4185ced`（上游 `api/openai-codex-responses.ts` 1719 行 SSE 首片，WebSocket 传输推迟）：**新模块 `pi-ai/src/providers/openai_codex_responses.rs`**，wire 层至此全部完成（10/10 API）。
- **openai_responses.rs 重构**：`request_body` 的消息转换抽为 `pub(crate) convert_transcript -> TranscriptParts{system_parts, tools, input}`（input 不含 system item，openai-responses 侧自行插入 developer/system 头——输出逐字节不变，既有测试全绿）；`ResponsesSession`（struct/message 字段/new/partial/into_message/process_payload/finish）、`SseDecoder`、`effort_string`、`forward`、`clamp_prompt_cache_key` 提为 pub(crate)。
- **Codex wire**：`{base}/codex/responses`（URL 归一化三分支：以 /codex/responses 结尾原样、/codex 补 /responses、其余拼接；空 base 回退 chatgpt.com/backend-api）；认证 = ChatGPT OAuth JWT——`Authorization: Bearer` + 从 JWT payload claim `https://api.openai.com/auth` 提取 `chatgpt_account_id` 作 `chatgpt-account-id` 头（base64url 无/有填充双试，失败即上游同文案）；`originator: pi`、`OpenAI-Beta: responses=experimental`、`accept: text/event-stream`、session-id/x-client-request-id。
- **请求体**：system 文本进顶层 `instructions`（空回退 "You are a helpful assistant."），input 不含 system item；固定 `store:false`/`stream:true`/`text:{verbosity:"low"}`/`include:["reasoning.encrypted_content"]`/`tool_choice:"auto"`/`parallel_tool_calls:true`；prompt_cache_key 64 字符钳制；reasoning：显式 level→effort+summary auto，Off/None→推理模型 effort none、非推理省略；**不发 max_output_tokens**（上游 codex 就不发）。
- **事件映射**（mapCodexEvents）：`error`→Err("Codex error: {message||code||event}"，message/code 取顶层或嵌套 error)；`response.failed`→Err(response.error.message||"Codex response failed")；`response.done/completed/incomplete`→重写 type 为 response.completed（end_turn 布尔写入 session、status 不在六值集则剔除）后交共享 ResponsesSession 处理；其余原样。尾部检查复用 finish()（终态事件必须到达、工具调用必须闭合）。
- **声明推迟**：WebSocket 传输及会话/continuation 缓存（走上游的 SSE 回退路径）、zstd 请求压缩（后端也收明文 JSON）、重试循环（上游 DEFAULT_MAX_RETRIES=0）、maxRetries/timeoutMs、grammar 工具、service tier 定价、onPayload/onResponse、identity 头（仅 originator）。
677 项测试全绿（+10：请求体固定面/系统映射/推理档位/缓存键钳制/URL 三分支/JWT 提取+两种坏 token/error 映射两种/response.failed/response.done 重写+end_turn/未知 status 剔除/mock HTTP 全链路——断言请求行、account/originator/beta 头、SSE 全程到 Done+usage+end_turn），fmt/clippy 严格干净。**坑：①normalize_context(None,..) 会丢弃传入 messages 里已有的 system 消息（只折叠 system_prompt 参数）——构造 TranscriptContext 直传即可；②reqwest/http crate 把请求头名规范成小写，mock 服务器断言要写 `openai-beta` 而非 `OpenAI-Beta`（HTTP 头名大小写不敏感，行为等价）。** 差分回归：六场景 6/6 IDENTICAL（f9daca34）。
