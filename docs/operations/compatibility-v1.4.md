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

## 差分测试首轮发现（2026-10-28）

框架：`difftest/`（场景脚本 + 回放服务器 + 归一化对比），上游 bundle `1.4.0`（快照 SHA 构建）vs `target/debug/pi-rs`。两个场景（单轮文本、bash 工具往返）双侧均跑通：exit 0、最终文本一致、轮数一致。规范视图（剥 id/时间戳/系统提示）下共 **31 处差异**，按类别归并：

| # | 差异 | 证据（artifacts/report.json） | 定性 |
|---|---|---|---|
| 1 | **默认工具集不同**：TS 请求带 4 个工具（bash/edit/read/write），Rust 带 7 个（另有 find/grep/ls） | `$.requests[0].toolNames` | 行为差异——上游默认只暴露 4 件套，`--tools` 才扩；Rust 固定全量 |
| 2 | **user 消息 wire 形状**：TS 发块数组 `[{"type":"text","text":…}]`，Rust 发纯字符串 | `$.requests[0].messages[1].content` | provider wire 差异（对多数 OpenAI 兼容后端等价，但与上游请求不可逐字节对齐） |
| 3 | **工具结果 wire 形状**：TS 发裸字符串（保留尾换行 `"hello world\n"`），Rust 发 `[{"type":"text","text":"hello world"}]`（剥尾换行） | `$.requests[1].messages[3].content` | 同上，且尾换行处理不同 |
| 4 | **会话文件结构完全不兼容**：TS 首行 `{type:"session",version:3,id,…}` + `model_change`/`thinking_level_change`/system checkpoint entry（6–8 entries）；Rust `{type:"header",version:1,…}` 扁平 message log（3–5 entries） | `$.session[*]` | 证实矩阵「会话格式整体不兼容」：双向互读不可行 |

意义：#1–#3 是 provider 请求形状/默认配置差异（修正成本中低），#4 是结构差异（需要实现上游 session 格式 + 迁移管线）。每轮扩面后更新本表与明细文档。

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
