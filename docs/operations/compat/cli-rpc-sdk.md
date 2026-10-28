# 兼容矩阵：CLI / 运行模式 / RPC / 退出码 / SDK

审计：2026-10-28，上游快照 `f9daca3480732d0042b1df3a6e27221f597a620d`。更正：Rust RPC 实际为 **19 个命令**（rpc.rs:66-106 `RpcCommand` 枚举），此前文档口径的「16 个」过时。

## 1. CLI 参数

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| `-p, --print` | `src/cli/args.ts:177-183`；docs/cli.md:45 | main.rs:41-43 | 已验证(语义部分) | 上游 `-p` 吸收非 flag 参数为 message 且可多条组合；Rust 无吸收逻辑 |
| `--mode text\|json\|rpc` | args.ts:96-110 | 无 `--mode`；等价物 `--rpc` | 部分 | **`--mode json` 完全缺失**（JSONL 事件一次性模式） |
| `-c, --continue` | args.ts:111-112；main.ts:432-434 | main.rs:53-55 + SessionSelector::Recent | 已验证(部分) | 缺 `-c` 短选项 |
| `-r, --resume`（无值，选择器）/ `--session <path\|id>` | args.ts:113-114,134-135；main.ts:391-430 | `--resume <ID\|PATH>` 带值（session_manager.rs:345-380） | 接受差异 | Rust `--resume` 实为上游 `--session` 语义；无选择器 UI、无跨项目 fork 提示 |
| `--session-id` / `--fork` / `--session-dir` / `-n --name` | args.ts:136-141,126-131 | 无 | 未实现 | |
| `--provider` / `--api-key` / `--models` | args.ts:115-120,142-146 | 无 | 未实现 | |
| `--model <pattern>`（provider/id、`:thinking` 后缀、fuzzy） | args.ts:117-118；main.ts:475-498 | main.rs:49-51 + registry find | 部分 | 未验证 `:thinking` 后缀；多加 `-m` 短选项（上游无） |
| `--thinking <level>` | args.ts:167-176 | 无 CLI flag（读 settings；非法值静默→off） | 部分 | 上游非法值 warning；Rust 静默 |
| `-t --tools`（含 +name/-name）/ `-xt` / `-nt` / `-nbt` | args.ts:147-166 | 无（工具集固定 create_tools） | 未实现 | |
| `--system-prompt` / `--append-system-prompt` | args.ts:121-125 | 无 | 未实现 | |
| `-e --extension` / `-ne --no-extensions` | args.ts:186-190 | 无 | 未实现 | |
| `--no-mcp` | args.ts:191-192 | 无（MCP 始终尝试连接） | 未实现 | |
| `--skill` / `--prompt-template` 系 | args.ts:193-198,210-213 | 无 | 未实现 | |
| `--theme` / `--use-theme` / `--no-themes` | args.ts:199-209,214-215 | 等价物 `--light`（上游无此 flag） | 接受差异 | |
| `-nc --no-context-files` | args.ts:216-217 | main.rs:66-68 | 已验证 | 缺 `-nc` 短选项 |
| `--list-models` / `--export` / `--tui-mode` / `--verbose` | args.ts:218-240 | 无 | 未实现 | |
| `-a --approve` / `-na` | args.ts:241-244；main.ts:720-750 | trust 仅作用于 .pi/mcp.json | 部分 | 无全局 resource trust |
| `--offline` | args.ts:245-246 | 无 | 未实现 | |
| `-h/--help`、`-v/--version` | args.ts:92-95；main.ts:881-888 | clap 自动 | 已验证 | 上游 `-v` vs clap `-V` |
| 未知长选项 → 交扩展 | args.ts:249-262 | clap 报错 exit 2 | 接受差异(暂) | 扩展系统落地前拒绝合理，但退出码 2 vs 上游 1 |
| `@file` 参数注入首条 prompt | args.ts:247-248；src/cli/file-processor.ts | 无（`@path` 当字面文本） | 未实现 | |
| 管道 stdin 内容前置到 prompt | main.ts:80-97,897-907 | **不读内容**（只判 is_terminal） | 未实现 | `echo x \| pi-rs -p` 语义丢失 |
| 模式解析规则（stdout 非 TTY 也进 print） | main.ts:112-123 | main.rs:224-226 | 部分 | 无 prompt + 非 TTY stdin 时 Rust 进 interactive（上游进 print） |
| 子命令 install/remove/update/list/config/auth/mcp | docs/cli.md:8-16,250-333 | 无 | 未实现 | |

**CLI 覆盖度**：上游 ~44 项 vs Rust 8 项（≈18%，语义完全一致 4 项）。

## 2. 退出码

| 场景 | 上游证据 | Rust 证据 | 状态 |
|---|---|---|---|
| 正常退出 0 | main.ts 多处 | print/rpc/interactive Ok | 已验证 |
| print error turn → 1 | print-mode.ts:145-147 | print.rs:31-32（Error→1） | 部分：**Aborted→Rust 0 vs 上游 1** |
| print length 截断 → 1 | print-mode.ts:154-157 | 无处理 → 0 | 未实现 |
| print 输出最后一条 assistant | print-mode.ts:139-159 | print.rs:28-41 输出**每条** | 部分 |
| 信号 SIGTERM/SIGHUP → 143/129 | print-mode.ts:50-66；rpc-mode.ts:359-375 | 无信号处理（rpc.rs:49-50 自认） | 未实现 |
| 诊断错误 → 1 | main.ts:624-631 | anyhow→1；clap 参数错→2 | 接受差异 |
| auth check 0/1/2 | main.ts:202,206 | 无 auth 子命令 | 未实现 |
| 会话选择取消 → 0 | main.ts:423-425 | --resume 失败→1 | 接受差异 |
| RPC stdin EOF → 0 | rpc-mode.ts:713-727 | rpc.rs:574-589（abort+settle+落盘） | 已验证 |

## 3. print 模式

| 项 | 上游证据 | Rust 证据 | 状态 |
|---|---|---|---|
| text 输出（纯文本、最后一条） | print-mode.ts:139-159 | print.rs:33-36（多条全输出） | 已验证(部分) |
| `--output` flag | 不存在 | 不存在 | 已验证（一致） |
| 非流式（等最终文本） | print-mode.ts:108-118 | 等message_end | 已验证 |
| `--mode json` 事件流（含 session header 首行） | print-mode.ts:122-127；json-event.ts:46-61 | 无 | 未实现 |
| 多 prompt 顺序执行（多轮） | main.ts:986-999 | positionals **join 成一条** | 接受差异 |
| `@file`/图片注入 | main.ts:211-230 | 无 | 未实现 |

## 4. RPC 命令（上游 33 / Rust 19，缺 14）

已验证（8）：abort、set_model、set_thinking_level、set_steering_mode、set_follow_up_mode、set_auto_compaction、get_available_models、get_messages、get_last_assistant_text、prompt/steer/follow_up（带字段偏差）。

**有形状偏差的 served 命令**（8）：

| 命令 | 偏差 |
|---|---|
| prompt | 无 `images`（静默丢弃）、无 `streamingBehavior`（流式中应报错，Rust 静默 queue）；disposition 本地推断、永无 `handled` |
| steer/follow_up | 无 images；恒答 queued |
| clear_queue | pi-agent 只有合并 peek，全部归入 steering，followUp 恒 [] |
| cycle_model | 复用裸 Model 响应形状（上游 `{model,thinkingLevel,isScoped}` 或 null）；无 isScoped |
| cycle_thinking_level / get_available_thinking_levels | **无视模型能力恒 7 级**（上游按模型裁剪、不支持时 null/["off"]） |
| compact | 答 `{disposition}` 而非 CompactionResult；`customInstructions` 静默忽略 |
| get_state | **缺 4 字段**：isCompacting、steeringMode、followUpMode、sessionName（rpc.rs:39-40 注释只承认 2 个，漏报 2 个） |
| get_commands | 仅内置命令，source 恒 "prompt"，无 sourceInfo |

**缺失命令（14）**：new_session、set_auto_retry、abort_retry、bash、abort_bash、get_session_stats、export_html、switch_session、fork、clone、get_fork_messages、get_entries、get_tree、set_session_name（均回 error frame）。

## 5. RPC 事件帧（上游 23 种 wire 事件 / Rust 10 + 自有 notice）

| 项 | 状态 | 差距 |
|---|---|---|
| 帧协议（stdin/stdout JSONL、LF 分帧） | 已验证 | 一致 |
| `id` 关联回显、parse 错误帧 | 已验证 | 一致 |
| agent_start/turn_start/turn_end/message_* /tool_execution_* | 已验证 | 一致（含 camelCase 补丁测试） |
| **`agent_settled`** | **未实现** | **P0 最大协议缺口**：上游客户端以它判定"不再自动继续"（重试/压缩恢复/队列后续）；Rust 只有 agent_end |
| agent_end 的 `willRetry` 字段 | 未实现 | — |
| queue_update / entry_appended / session_info_changed / thinking_level_changed | 未实现 | 部分以自有 `{type:"notice"}` 文本帧代替（上游客户端不识别） |
| compaction_start/end（reason/result/aborted/willRetry） | 未实现 | 压缩进度经 notice 文本 |
| auto_retry_* / summarization_retry_* / bash_execution_update / extension_error | 未实现 | — |
| extension_ui_request/response 子协议（9 方法） | 未实现 | docs/rpc-extension-ui.md 全集 |
| stdout 背压 / SIGTERM/SIGHUP | 未实现 | — |

## 6. SDK 导出（上游 ~150 符号 / Rust 0 正式导出）

上游 `src/index.ts:1-498` 导出 9 组（createAgentSession 工厂、AgentSession、SessionManager、SettingsManager、工具工厂、压缩 API、扩展系统 ~120 类型、运行模式+RpcClient、UI/主题）。Rust `pi-coding-agent` **无 lib target**（bin-only），内部有近似物（SessionHandle/SessionCommand/mpsc 事件、agent_session 工厂函数）但均非导出面，形状不同。覆盖 0%。

## 关键结论

1. **rpc.rs 模块注释有漏报**：get_state 实缺 4 字段（注释承认 2）；agent_settled 缺失注释只字未提。P0 矩阵以实际 wire 输出为准。
2. RPC 命令覆盖 58%（19/33），事件帧覆盖 ≈43%（10/23），SDK 0%。
3. CLI 参数覆盖 ≈18%；`--mode json`、管道 stdin、`@file` 是 print/JSON 集成场景的硬缺口。
4. 退出码：核心 0/1 一致；aborted/length/信号/参数错误四类不一致或缺失。
