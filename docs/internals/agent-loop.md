# Agent Loop 核心解读

`pi-agent` 的核心是 `crates/pi-agent/src/agent_loop.rs`（下称 loop 文件）中的 `run_agent_loop`，移植自上游 `pi/packages/agent/src/agent-loop.ts` 的 `runAgentLoop` / `runAgentLoopContinue`。它是整个 coding agent 的「发动机」：一轮轮地执行「准备请求 → 流式调模型 → 执行工具 → 决定是否继续」，直到硬退出或无事可做。类型与契约定义在 `crates/pi-agent/src/types.rs`。

## 整体形态

循环体本身很薄（loop 文件 `run_agent_loop` 内的 `loop`），重活分两部分：**共享状态句柄**（`AgentLoopRun`）与**工具执行函数**。`Agent`（`agent.rs`）用 `Arc<RwLock/Mutex>` 把 transcript、streaming partial、两个消息队列、事件通道交给循环，循环只往里写——TS 版靠 class 引用共享状态，Rust 版靠共享句柄达到同样效果（interior mutability）。

```
run_agent_loop
 ├─ emit AgentStart → 逐条 append prompt_messages（带 MessageStart/End）
 └─ loop ──────────── 一轮 = 一个 Turn ────────────
     ├─ emit TurnStart
     ├─ prepare_request hook          可换 model / thinking_level
     ├─ 取 transcript（可选 convert_to_llm）→ stream_fn 流式请求
     ├─ 消费流事件 → 更新 streaming + MessageUpdate → Done/Error 收尾
     ├─ push assistant 消息 + MessageEnd
     ├─ 工具执行阶段（sequential 或 parallel）→ ToolResultMessage 入 transcript
     ├─ emit TurnEnd
     ├─ hard_exit 或 terminate_batch? → break
     ├─ prepare_next_turn hook        可换 model/thinking、追加消息
     ├─ steering poll                 有注入 → continue 下一轮
     ├─ finish_turn hook              End → break / Continue → 强制再一轮
     └─ follow-up poll（仅 tool-free turn）
 emit AgentEnd { 本 run 新增的全部消息 }
```

## 单轮 Turn 流水线

### 流式消费与协议兜底

流事件分两类（loop 文件「流消费」段）：`Start` / 各类 `*Start` / `Delta` / `End` 事件只把最新的 partial assistant message 写进 `run.streaming` 并发 `MessageUpdate`（附带原始 provider delta 的 JSON，供 UI 消费流式协议）；只有 `Done` / `Error` 落到 `final_message`。

协议兜底：流没发终结事件就结束（provider 违约），循环合成一条 `StopReason::Error` 的 assistant 消息继续走，永远不会挂起或 panic。

`duration_ms` 盖章用单调时钟，但跳过已带 duration 或时间戳早于本次流开始的消息——后者保护 deferred results（如复制的工具结果）不被错标。

### 工具调用识别与截断防御

- **识别靠 content block 而非 stop reason**：`stop` 结束但带 tool-call block 的消息照样执行工具；只有 `Error` / `Aborted` 是 hard exit，跳过工具阶段。
- **`length` 截断防御**：stop reason 为 `Length` 时整批工具调用全部不执行，每条给 error 结果提示模型重新发起——参数可能被 token 上限悄悄截断，即使能 parse 也不可信（对应上游 `failToolCallsFromTruncatedMessage`）。

## 工具执行：两条路径

`sequential` 判定：全局 `ToolExecutionMode::Sequential`，或批内**任一**工具声明 `sequential`，整个批走串行——工具级 override 会拖累全批（上游语义）。

### Sequential（`execute_tool_calls_sequential`）

按 source order 逐条：start → before hook → execute → after hook → end → append 结果，一条走完才走下一条。

### Parallel（`execute_tool_calls_parallel`）三段式

1. **Preflight（source order）**：逐条 emit `tool_execution_start`，跑 `before_tool_call` hook——被 block 的调用不 launch；未知工具立即 error 收尾。hook 只在工具存在时才运行（`run_before_hook`），未知工具跳过 hook，与上游一致。
2. **并发执行（JoinSet）**：可执行调用并发跑；**完成顺序** finalize（after hook）+ emit `tool_execution_end`；`ToolResultMessage` 最后统一按 **source order** append。事件流是完成序、transcript 是源序，各有各的顺序约定。panic 的任务由兜底 sweep 补一条 error 结果——保证每个 tool call 一定有配对的 ToolResultMessage（API 正确性要求，缺了下轮请求会 400）。
3. **terminate 传播**：preflight 里被 block 且 `terminate: true` 会停止 launch 本批后续调用（收到「was not executed」错误结果）。这是与上游的刻意偏离之一（上游只停 abort signal 上的 preflight）。

### update 回调的门闩

每个工具拿到的 `on_update` 回调由 `accepting: Arc<Mutex<bool>>` 包裹：execute 一 settle 就把 flag 翻 false。工具若保留回调（如长驻任务）事后不能再 emit——保证不会出现迟到的 `ToolExecutionUpdate` 排在 `tool_execution_end` 之后。锁用 `unwrap_or_else(|e| e.into_inner())` 容忍 poisoned lock。

### after hook 的覆盖语义

`after_tool_call` 返回字段级覆盖：提供的字段整体替换，缺省保留原值，无深合并。讲究点：只给 `content` 不给 `structured_content` 时后者被丢弃（可能不再匹配），要保留就两个都给（语义定义见 `types.rs` `AfterToolCallResult` 文档）。

## 退出条件与调度

五个出口，按优先级：

1. `signal.is_cancelled()`——循环顶部检查（`CancellationToken`）。
2. `hard_exit`——stop reason 为 `Error` 或 `Aborted`（`is_hard_exit`）。
3. `terminate_batch`——本批任何工具结果带 `terminate: true`（before hook block、工具结果、after hook 覆盖三条路径都能置位）。
4. `finish_turn` hook 返回 `End`——即使 follow-up 队列有货也 break。
5. 自然结束——tool-free turn 且 steering 空、follow-up 空、无 `explicit_continuation`。

### Steering 与 Follow-up 的语义差别

设计核心：二者都是 `PendingMessageQueue`（`QueueMode::All` / `OneAtATime` 排空策略），但 poll 时机不同。

- **Steering 在每轮 Turn 结束后 poll**（有工具没工具都 poll）：用户可在模型干活中途插话，注入后直接 `continue` 开下一轮。整批工具执行完才生效——不是单个工具调用结束后。
- **Follow-up 只在 tool-free turn 之后 drain**：语义是「任务做完了再排下一个任务」。

`Agent::steer()` / `Agent::follow_up()` 只是无锁 enqueue，与循环靠 Mutex 同步。`Agent::continue_run()` 自己排空队列后以 `skip_initial_steering_poll` 启动循环，避免重复消费首轮 steering。

### explicit_continuation

上游 `explicitContinuation` 的移植：`finish_turn` 返回 `Continue` 时保证还有一次 provider 请求，即使没有任何自然调度者。注入 steering 或出现工具调用都会清掉它（它们自己保证了下一轮）；follow-up 为空时它触发一次「context-only turn」——不带新消息再请求一次，然后重置归零，不会死循环。

## Hook 一览

| Hook | 时机 | 同步性 | 能做什么 |
|---|---|---|---|
| `convert_to_llm` | 每次请求前转换 transcript | 同步 | 过滤/改写发给 LLM 的消息 |
| `prepare_request` | 每次 provider 请求前 | async | 换 model / thinking_level |
| `prepare_next_turn` | turn_end 后、下轮请求前 | async | 换 model/thinking + 追加消息（mid-run compaction 挂这里） |
| `before_tool_call` | 工具 launch 前 | 同步 | block（附 reason）/ terminate |
| `after_tool_call` | 工具执行完 | 同步 | 字段级覆盖 content/details/usage/is_error/terminate |
| `finish_turn` | steering poll 后 | async | End / Continue |

async 与同步的区分是有意的：`prepare_request` / `prepare_next_turn` / `finish_turn` 预期跑长任务（如 mid-run compaction），`before/after_tool_call` 要求快速返回。

## 事件机制（`AgentEvent`）

通过 tokio broadcast channel 分发，`Agent::subscribe()` 取 receiver；无 receiver 时 send 静默成功，循环不依赖 UI 存在。事件名与 TS pi 的 `type` 字符串对齐（snake_case serde tag）：

- `agent_start` / `agent_end { messages }`——run 边界，`agent_end` 携带本 run 新增的全部消息。
- `turn_start` / `turn_end { message, tool_results }`——turn 边界。
- `message_start` / `message_update` / `message_end`——消息生命周期；`message_update` 仅流式中的 assistant 消息，附原始 provider delta（`assistant_message_event`）。
- `tool_execution_start { tool_call_id, tool_name, args }`。
- `tool_execution_update { ..., partial_result }`——工具执行中的 partial 上报。
- `tool_execution_end { ..., result, is_error, duration_ms? }`——`duration_ms` 缺省表示工具根本没跑（被 block / 未知工具）。

## 与上游 TS 的三处刻意偏离

loop 文件头注释明确记录：

1. 工具执行用单层循环，替代 TS 的嵌套 inner/outer 结构，`explicitContinuation` 语义保留。
2. **`finish_turn` 移到 steering poll 之后**（上游在之前）——注入了 steering 的 turn 不触发这个 hook；硬退出与 terminate batch 同样在 hook 点之前 break。
3. 并行模式下 `before_tool_call` block + terminate 会停止 launch 本批后续调用（上游只停 abort signal 上的 preflight）。

## 并发与同步设计要点

- **CancellationToken 贯穿全程**：clone 进 `StreamOptions` 和每个 `tool.execute`，abort 即全链路可取消；已 launch 的工具调用总是跑完（结果照常入 transcript）。
- **std Mutex 的使用边界**：所有锁持有不跨 `.await`（drain / snapshot 均为同步操作），因此用 std Mutex 而非 tokio Mutex。
- **transcript 是唯一事实源**：hook 收到的 `messages`、`AgentEnd` 的增量、持久化层读的都是同一个 `Arc<RwLock<Vec<Message>>>`。
- **每轮 transcript 快照三次 clone**：`prepare_request` context 一次、发给 stream_fn 的 transcript 一次、`hook_messages` 一次（没装 hook 时前两处照付）。TS 版是引用传递，这里是 O(会话长度) 的每轮常数开销——长会话高频 turn 时会显形。优化方向是 `Arc<Message>` 化或 Cow 语义，但会传染所有 hook 签名；当前优先级（先对齐 wire 行为）下暂不处理。
