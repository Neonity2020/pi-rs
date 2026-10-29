# Pi 1.4 兼容矩阵（P0 审计）

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
