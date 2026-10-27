# 项目质量改进交接

更新日期：2026-10-27（用户时区 Etc/GMT+7，第三轮）

## 目标与接手范围

pi-rs 是 pi coding agent 的 Rust 移植，README 定位为框架阶段。第一轮已完成连续 prompt 排队、快照补齐持久化、进程组清理、输出限幅与 CI 工作流（见 git log 与下方记录）。本轮接手后完成：严格 Clippy 门禁清零、rustfmt 基线、会话持久化加固、bash 管道滞留修复。验收围绕可靠的 agent、工具和会话关键路径，并准确说明尚未实现的能力。

工作目录为本仓库，分支 `main`。2026-10-27 再次扫描时 HEAD 为 `49d9ded`，下述改动已分批提交，扫描前工作区干净。最新计划和认领状态见 `TASKS.md`；本文件保留上一轮实现记录。保留现有工作区内容，不要重置。仓库内 `pi/`、`pi-tutorial/`、`t3code/` 为独立或参考目录，不应修改。

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

**集成验证（主会话实测，全工作区）**：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked` 全部通过，**124 项测试**（第二轮 46 → 124）。改动未提交，等用户指令。README 能力表已同步更新。

**交接事项**：并行存在另一个会话在本工作区产出 `TASKS.md`（任务认领清单）、`docs/`（484 行文档树）并编辑了本文件指向 TASKS.md——非本轮 4 个 agent 所为；`docs/internals/session-and-persistence.md` 里 JSONL 示例首行写的是 `{"type":"session",...}`，实际实现是 `"header"`，若继续维护文档需修正。TASKS.md 的 T3/T4/T5（恢复 CLI、provider HTTP 契约测试、流式）已由本轮完成，认领状态未同步。

## 未完成与建议分工

1. **提交状态**：第二轮改动已推送至 `49d9ded`；**第三轮（并行冲刺）改动在工作区未提交**，建议按 crate 分四个提交（流式 / 会话恢复+RPC / MCP / TUI+文档），遵循用户指令执行。
2. **CI 远程验证**：第二轮三 job 已在 GitHub Actions 全过。第三轮改动 push 后需再确认（本轮新增的 mock HTTP 测试绑定本机随机端口，Linux 兼容；stdio 测试依赖 `/bin/sh`）。
3. **产品能力剩余**（README 路线图）：Anthropic Messages provider 完整实现（当前 fail-fast 桩）、会话压缩、MCP 工具接入 CLI 装配（`McpTool` 已就绪，缺配置加载与生命周期管理）、TUI 主题/键位接入配置加载、扩展系统。
4. **持久化下一步**：事务性同步持久化（崩溃安全）仍是有意不做；若产品需要，设计 WAL 或 double-buffer 方案。会话恢复 `read_session_messages` 已能跳过撕裂行。
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
