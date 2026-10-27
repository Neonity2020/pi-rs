# 下一阶段计划与任务认领清单

基线：2026-10-27 扫描 `main`，HEAD `49d9ded`；扫描前工作区干净。已提交 agent 测试、格式化、进程管理、会话持久化和 CI 门禁。当前本地 `cargo test --workspace --locked`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 均通过；GitHub 远程 CI 状态未验证。

目标：先保证会话和进程的生命周期可靠，再交付一个可恢复会话、可验证的 OpenAI 兼容工作流。MCP、RPC 和其他 provider 保持后续阶段，避免同时扩大接口与风险面。

## 认领规则

- 任务均为待认领；认领者在对应项填写 Agent、开始时间和分支或工作目录，并先检查最新状态。一个任务仅由一名 Agent 负责。
- 每项交付包含代码、针对行为的测试、验证结果和遗留限制。不要修改其他任务的主要负责文件；确需跨界时先协调或提出接口变更。
- 共享工作区并行修改时不要运行全仓格式化或覆盖他人改动；集成者最后运行全部门禁。不要读取或输出 `.env` 凭据。
- 任务独立提交与推送按用户后续指令执行。完成后更新状态、提交或 diff 标识及测试结果。

## 第一批：可靠性与基础验证

### T1 — 会话关闭与失败恢复（P0）

- [x] 已完成（第二轮，见 git log `49d9ded` 与 handoff.md 第二/三轮记录）；Agent：主会话集成轮；开始：2026-10-27
- 范围：`crates/pi-coding-agent/src/core/agent_session.rs`、`session_manager.rs`。
- 核查并修复退出或通道关闭时仍有运行任务的生命周期；验证 panic 后运行状态能恢复。检查 `write_all` 部分失败、`sync_data` 失败后重试是否造成破损行或重复记录；按实际需要设计明确的持久化提交语义。
- 验收：退出、取消、失败注入和重试有自动化测试，证明消息顺序及重复/丢失行为；文档准确说明崩溃保证。避免在没有证据时宣称事务性持久化。
- 依赖：无。与 T3 协调会话恢复接口，T3 不修改上述核心文件，除非经协调。
- 结果：fsync 每条目 + 撕裂行自愈 + 写失败游标不推进有测试；持久化为有序、逐条落盘，非事务（模块文档如实标注）。

### T2 — 进程生命周期与跨平台验证（P1）

- [x] 已完成（第二轮，见 git log `49d9ded` 与 handoff.md 第二轮记录）；Agent：主会话集成轮；开始：2026-10-27
- 范围：`crates/pi-env/` 和 `crates/pi-coding-agent/src/core/tools/bash.rs`。
- 检查退出后管道静默宽限、输出截断、取消/超时的部分输出及进程组清理语义；确认 `ExecResult.timed_out` 与错误路径一致。检查 Unix 和非 Unix 的实际支持范围。
- 验收：有针对后代持有管道、持续输出、取消和超时的确定性测试；平台限制写清楚；在 Linux 上验证或明确记录未验证项。无需承诺清理主动脱离进程组的进程。
- 依赖：无；不修改会话核心文件。
- 结果：退出后 100ms 静默宽限（每块重计时）+ 持管道/排空/快速路径测试；进程组清理范围文档化（setsid 脱离组不覆盖，非 Unix 仅直接子进程）；`timed_out` 恒 false 的死代码已记录在 handoff 待清理项。Linux 验证：远程 CI test job 通过（第二轮）。

### T3 — 会话恢复 CLI（P1）

- [x] 已完成（第三轮并行冲刺，详见 handoff.md）；Agent：子 agent B；开始：2026-10-27
- 范围：`crates/pi-coding-agent/src/main.rs` 和恢复相关 CLI/模式代码；与 T1 协调 session 核心接口。
- 设计并实现明确的 `--continue` 或指定会话入口，恢复消息并继续追加原文件，处理不存在、损坏和空会话；明确与 `--no-session` 的冲突和当前工作目录策略。
- 验收：使用临时目录与 mock provider 的端到端测试，覆盖新建、退出、恢复后对话和再次恢复；不重复旧消息，不静默覆盖会话。更新 CLI 帮助和 README。
- 依赖：T1 的关闭及持久化语义确定后进行最终集成。
- 结果：`--continue`/`--resume <id|路径>`（精确/唯一前缀/完整路径），恢复前校验 header，坏会话不留半写；恢复后旧消息不重复落盘有测试；README 能力表已同步。

### T4 — Provider 本地 HTTP 契约测试（P1）

- [x] 已完成（第三轮并行冲刺，与 T5 一并由子 agent A 交付）；Agent：子 agent A；开始：2026-10-27
- 范围：`crates/pi-ai/src/providers/openai_completions.rs` 及其测试。
- 建立无需真实密钥或外网的本地 HTTP 测试，覆盖请求体、工具调用往返、错误状态、异常响应及取消。记录当前非流式行为和必要的兼容假设。
- 验收：自动测试验证实际 HTTP 请求与响应解析，测试不依赖环境变量中的真实密钥；发现的缺陷有修复或具体记录。
- 依赖：无；为 T5 提供基线。
- 结果：pi-ai/pi-agent 各一份本地 mock HTTP+SSE 集成测试（tokio TcpListener，无新依赖）；顺带修复 Error 事件 `tx.send` 缺 `.await` 的 bug。

## 第二批：功能扩展

### T5 — OpenAI 兼容增量流式输出（P2）

- [x] 已完成（第三轮并行冲刺）；Agent：子 agent A；开始：2026-10-27
- 范围：`pi-ai` 的 OpenAI 兼容 provider、必要的流事件测试。
- 在 T4 合并后实现 SSE 增量文本与工具参数组装，处理分片、结束原因、错误和取消，并明确 usage 的缺省情况。
- 验收：本地 HTTP 测试证明首个增量在响应结束前可见，分片工具调用最终还原正确，异常与取消正确终止；README 状态同步更新。
- 依赖：T4。不要与 T4 同时修改 provider 文件。
- 结果：真增量 SSE（文本 delta、tool call 按 index/id 分片累积、repairJson 移植、usage/finish_reason 收敛）；测试证明首增量先于响应结束到达；README 已同步。

### T6 — RPC 与 MCP 路线设计（P2）

- [x] 已被实现超越（第三轮并行冲刺直接交付了实现）；Agent：子 agent B（RPC）/ 子 agent C（MCP）
- 范围：只读调研 `crates/pi-mcp/`、`crates/pi-coding-agent/src/modes/rpc.rs` 和现有类型。
- 产出协议兼容范围、最小可交付切片、失败处理和测试方案；列出与会话接口的依赖。先不宣称协议兼容或实现完成。
- 验收：交付可审阅的方案与可独立认领任务，明确版本、命令和事件的证据来源；实现另行分派。
- 依赖：无；避免与 T1/T3 的实现冲突。
- 结果：RPC 10 命令 + 事件帧已实现（偏差见 rpc.rs 模块文档）；MCP stdio 传输、list/call tools、AgentTool 桥已实现（顺带修复协议类型缺 camelCase rename 的互通 bug）。剩余：MCP 接入 CLI 装配、真实 server 兼容性验证。

## 集成检查

- [x] 审查各任务的 diff 和跨模块接口，确认所有认领项状态及未解决问题。（2026-10-27 主会话集成轮）
- [x] 运行 `cargo test --workspace --locked`（124 项通过）、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 和 `git diff --check`。（2026-10-27）
- [x] 核对 README 功能状态（已同步第三轮能力）；远程 CI：第二轮 `49d9ded` 三 job 全过；第三轮改动推送后待确认。
