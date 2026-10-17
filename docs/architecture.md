# 架构

## 总览

```
┌───────────────────────────────────────────────────────────┐
│ crates/coding — bin `pi`                                  │
│ CLI · 三种模式 · 系统提示 · 会话 JSONL · sinks · 四个工具   │
└───────────────┬───────────────────────────┬───────────────┘
                │                           │
                ▼                           ▼
┌───────────────────────────────┐   ┌───────────────────────┐
│ crates/agent — 循环           │   │ crates/tui — 状态/渲染 │
│ 请求 → 流式累积 → 工具 → 事件  │   │ App · markdown · wrap │
└───────────────┬───────────────┘   └───────────────────────┘
                │                     （叶子，无业务依赖）
                ▼
┌───────────────────────────────────────────────────────────┐
│ crates/llm — provider                                     │
│ Provider trait · SSE · DeepSeek/OpenAI 兼容 · 模型与定价    │
└───────────────────────────────────────────────────────────┘
```

依赖方向严格单向：`coding → agent → llm`，`coding → tui`。`tui` 不知道 agent/llm 的存在；`agent` 不知道终端、会话文件或提示词文件；`llm` 不知道工具语义、会话或 UI。

## 分层契约

| Crate | 职责 | 知道 | 不知道 |
|---|---|---|---|
| `llm` | 一次请求 → 一段 `StreamEvent` | HTTP、SSE、OpenAI 线格式、模型价格 | 工具怎么执行、会话、终端 |
| `agent` | 一轮对话 → 事件流 | provider、工具 trait、消息历史 | 会话落盘、系统提示来源、TUI |
| `tui` | `UiEvent` → 终端行；按键 → `Action` | ratatui/crossterm、Markdown、宽度 | 网络、文件、agent |
| `coding` | 把以上装成一个可执行程序 | 全部，以及磁盘与终端 | 无（它就是外壳） |

**反向依赖是边界泄漏。** 如果 `agent` 需要读会话文件，或 `tui` 需要发请求，先改分层，不要加 `cfg` 或回调打补丁。

## 一次 turn 的数据流

1. `coding::main` 解析参数，构造 provider、模型、工具集、系统提示、会话；按模式进入 `modes.rs`。
2. 模式层先把用户输入写入会话文件（`append_user`），交互模式同时发一个 `UiEvent::User`。
3. `Agent::run_turn(input, sink, cancel)` 把用户消息追加到内存历史，开始 step 循环。
4. 每个 step：
   - 发 `AgentEvent::Step { step }`；
   - 调 `Provider::stream(ChatRequest, tx, cancel)`；
   - 事件经 `UnboundedSender<StreamEvent>` 回到 agent，边收边发 `AgentEvent::{Text, Reasoning, Usage}` 给 sink，同时累积成一条 assistant `Message`；
   - 流结束后提交 `AgentEvent::Assistant`，把消息追加到历史。
5. 没有 tool calls → `Outcome::Completed`，turn 结束。
6. 有 tool calls → 先为每个调用发 `ToolStart`，并行执行（`JoinSet`），结果按调用顺序发 `ToolEnd`，再以 `Message::tool` 追加到历史，回到第 4 步。
7. `Ctrl+C` 触发 `Cancel`：HTTP 流中断，未完成的 tool calls 被丢弃（参数可能不完整），已产生的文本保留；返回 `Outcome::Aborted`。

没有步数上限。循环只由三件事终止：模型不再调工具、取消、错误。

## crates/llm

### 边界

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn stream<'a>(
        &'a self,
        request: ChatRequest<'a>,
        sink: UnboundedSender<StreamEvent>,
        cancel: Cancel,
    ) -> BoxFuture<'a, Result<(), Error>>;
}
```

- 事件走 channel，返回的 future 只表示成功/失败。`Err(Cancelled)` 或直接丢弃 future 都表示中止。
- `StreamEvent` 全是增量：`Text`、`Reasoning`、`ToolCallStart`、`ToolCallDelta`、`Usage`、`Stop`。**累积是 agent 的职责**，provider 不保存状态。
- `ChatRequest` 借用模型、系统提示、消息与工具 schema；长对话不复制。

### 实现

- `OpenAiCompat` 同时服务 DeepSeek 与任意 OpenAI 兼容端点：`deepseek(...)` 额外携带 `thinking` / `reasoning_effort` 参数，`custom(...)` 不带。
- SSE 逐行解析 `data:`；`[DONE]` 结束；忽略注释与空行；非法 JSON chunk 变成 `Error::Stream`，HTTP 非 2xx 变成 `Error::Api`（含截断的响应体）。
- tool call 增量按 `index` 归位；没有 id 的调用在 agent 侧补 `call_{i}`。
- `Message` 是唯一消息类型：线上格式、agent 历史、会话记录三位一体。`reasoning` 必须随 assistant 消息原样回传（DeepSeek + tools 的硬要求）。
- `deepseek.rs` 维护模型注册表（上下文窗口、最大输出、峰谷价格）；未知模型照常工作，但成本显示为 `-`。
- `mock.rs` 的 `ScriptedProvider` 让 agent 测试无需网络。

## crates/agent

`Agent` 持有 provider、模型、系统提示、工具、工具 schema 与消息历史。

| 关注点 | 行为 |
|---|---|
| 取消 | `tokio::select!` 以 cancel 为最高优先级；不完整 tool calls 丢弃，保留文本 |
| 空流 | provider 一个事件都没发 → `Error::Stream("provider returned an empty stream")` |
| 工具批 | 相互独立的调用并行执行；结果按调用顺序返回 |
| 失败 | 未知工具、参数 JSON 坏、panic、取消 → `ToolResult::err`，继续循环 |
| panic 隔离 | 每个工具再套一层 `tokio::spawn`，panic 变成错误结果 |
| 历史 | assistant 消息与 tool 结果都进入 `Agent::messages`，下一 step 原样回传 |

`AgentEvent` 是唯一的对外事实来源：

```
Step · Text · Reasoning · Assistant · ToolStart · ToolEnd · Usage
```

`Sink` 是同步、内联调用的消费者；因此实现者不应做长时间阻塞操作。`coding` 的 sinks 在此处落盘并转发 UI。

## crates/coding

### 启动顺序

1. 加载 `.env`（工作目录或最近上级；真实环境变量优先；坏文件只警告）。
2. 解析参数（手写，不引 clap）。`--json` 必须有 prompt。
3. 取得 cwd，构造 provider 与模型（缺 key 时给出可操作的错误）。
4. 选择工具集：默认四个，`--tools read,bash` 取子集，未知名字直接失败。
5. 组装系统提示。
6. 新建会话，或 `-c` 载入最近会话的活动分支。
7. 进入模式；三种模式共用 `Shared`（会话 + 统计 + 模型）。

### 三种模式

| 模式 | 入口 | 输出去向 | 结束行为 |
|---|---|---|---|
| Interactive | 无 prompt | TUI（alternate screen） | `q`/`Ctrl+C` 退出并打印会话路径 |
| Print | prompt / `-p` / stdin | 正文 stdout，工具活动 stderr | 打印 token/成本摘要；取消退出 130 |
| Json | `--json` + prompt | 每行一个 JSON 事件 | 最后一行 `result`；失败退出 1 |

所有模式共享同一套 `Agent`、工具、系统提示与会话；差异只在 sink 和事件循环。

### 系统提示

```
SYSTEM.md（cwd，其次 $PI_RS_HOME）    存在则整体替换基础提示
  +
基础提示（四工具、规则、cwd/平台/日期）  约 300–400 token
  +
$PI_RS_HOME/AGENTS.md（若存在）
  +
从文件系统根到 cwd 的每一级 AGENTS.md（深者最后，优先）
```

空文件跳过。没有任何隐藏上下文；`build()` 是唯一入口，测试覆盖合并顺序与替换规则。

### 四个工具

| 工具 | 语义 | 关键约束 |
|---|---|---|
| `read` | 读文件，带行号 | 默认 2000 行，支持 `offset`/`limit`；单行超长截断 |
| `write` | 创建/覆盖 | 自动创建父目录 |
| `edit` | 精确字符串替换 | 要求唯一匹配，或显式 `replace_all` |
| `bash` | 执行 shell 命令 | 前台进程、可取消；默认 120s 超时；输出头尾截断 |

注册点只有一处：`tools::NAMES`、`tools::all()`、`tools::specified()`。**第五个工具是产品变更**，先改 `docs/goals.md`。

工具失败不是 `Err`，而是：

```rust
ToolResult { content: String, is_error: bool }
```

### 会话

路径：`$PI_RS_HOME/sessions/<cwd slug>/<timestamp>-<id>.jsonl`（默认 `~/.pi-rs`）。

```jsonl
{"type":"session","v":1,"id":"...","cwd":"/work","model":"deepseek-flash","created":"2026-..."}
{"id":"...","parent":null,"ts":"...","type":"message","message":{"role":"user","content":"..."}}
{"id":"...","parent":"...","ts":"...","type":"message","message":{"role":"assistant","content":"...","reasoning":"..."}}
{"id":"...","parent":"...","ts":"...","type":"message","message":{"role":"tool","content":"...","tool_call_id":"..."}}
```

- 每行一个 JSON，追加写；`id` + `parent` 使文件在格式上是一棵树。
- 恢复时从最后一条沿 `parent` 回溯到根，得到活动分支。
- 最后一行损坏（写到一半崩溃）会被跳过，不影响恢复。
- 当前没有分支导航 UI；需要分支时再实现，不重写历史格式。

### Sinks 与统计

`Shared` 持有会话、统计和模型。每个 sink 先 `record(event)`：

| 事件 | 落盘/统计 |
|---|---|
| `Assistant` | 会话追加 assistant 消息 |
| `ToolEnd` | 会话追加 tool 消息 |
| `Usage` | 累加 token 与估价 |

之后 print/json/tui sink 再做各自的展示。`stats` 只在进程内累计，不落盘；`-c` 恢复历史消息，但 token/成本从 0 重新计。成本按 DeepSeek 峰谷价估算（UTC 工作日 01–04、06–10 点为峰时，不含中国法定节假日）。

### UI 接线

- 独立线程跑 `crossterm::event::read`，把按键/滚轮/尺寸变化送进 channel。
- 主循环 `select!` 在输入、`UiEvent`、120ms ticker 之间调度；只有 `dirty` 时重绘。
- `Action::Submit` 触发 `spawn_turn`：后台拿 agent 锁跑一轮，`TuiSink` 把 `AgentEvent` 翻成 `UiEvent`，主循环消费并更新 `App`。
- `TerminalGuard` 在 `Drop` 中恢复 raw mode 与主屏幕；即使出错也不把终端留在 alt screen。

## crates/tui

**无 I/O。** 输入是 `UiEvent` 与按键，输出是 `Action` 和渲染结果；终端由 `coding` 驱动。

- `App`：消息项、输入框、忙碌态、滚动/跟随、状态栏、spinner、版本号。
- `UiEvent`：用户消息、assistant 增量/最终文本、工具开始/结束、忙闲、状态刷新。
- `Action`：`Submit` / `Interrupt` / `Quit` / `None`。
- 渲染：transcript（Markdown）→ 状态栏 → 输入框。渲染时刷新 `viewport` / `max_scroll`；滚轮脱离跟随，回到底部自动恢复跟随。
- `markdown.rs`：围栏代码（行号 + 语法高亮）、标题、列表、引用、表格、分隔线、行内代码/强调/链接；无法识别的内容按字面输出，**绝不丢字**。
- `wrap.rs`：按显示宽度换行（CJK 宽字符、tab 展开、长词硬断）。

## 不变量

1. 依赖方向只允许 `coding → agent → llm` 与 `coding → tui`。
2. 消息只有一种格式，不做 wire/session 转换。
3. assistant 的 `reasoning` 原样保留并回传。
4. 工具失败是 `ToolResult::is_error`，不是 `Err`，不终止循环。
5. 工具集合是产品决定：默认且仅四个。
6. 没有 max steps。
7. 基础系统提示保持在约 400 token 以内。
8. 会话文件只追加；每条记录有 `id` 与 `parent`。
9. `tui` 不碰 I/O；终端生命周期属于 `coding`。
10. provider 不知道会话、提示词文件或终端。
11. 取消是协作式的，贯穿 HTTP 请求与工具子进程。
12. 预览/渲染失败不能吞掉内容。

违反任意一条，视为架构变更：同时更新本文件与测试。

## 失败模型

| 故障 | 表现 | 恢复方式 |
|---|---|---|
| 缺 API key | 启动即失败，提示环境变量 / `.env` / `--api-key` | 配置后重试 |
| HTTP 非 2xx | `llm::Error::Api { status, body }` | 修正 key/端点/模型 |
| 网络中断 | `llm::Error::Http` | 重试 |
| SSE chunk 坏 / 空流 | `Error::Stream` | 重试；持续则查 provider |
| 工具参数不是 JSON | `ToolResult::err`，回给模型 | 模型自我修正 |
| 工具 panic | 捕获为错误结果 | 循环继续 |
| 用户取消 | `Outcome::Aborted`，保留已产出文本 | 重新提问 |
| 会话末行损坏 | 跳过该行 | 自动恢复 |
| TUI 出错 | `TerminalGuard::drop` 恢复终端 | 检查 stderr |

## 扩展点

| 想加什么 | 改哪里 | 是否动内核 |
|---|---|---|
| 新 provider | 实现 `llm::Provider`；在 `coding::build_provider` 接线 | 否 |
| 新工具 | 实现 `agent::Tool`；在 `coding/tools` 注册 | 是（需先改目标） |
| 新展示方式 | 实现 `agent::Sink` | 否 |
| 新模型/价格 | `llm/deepseek.rs` 或 provider 自己的注册表 | 否 |
| 新运行模式 | `coding/modes.rs` + `cli::Mode` | 否 |

步骤与代码骨架见 [development.md](development.md)。

## 测试地图

| 层 | 位置 | 保护什么 |
|---|---|---|
| llm 单元 | `crates/llm/src/*` | SSE 解析、请求体、reasoning 回传、峰谷价 |
| agent 单元 | `crates/agent/src/agent.rs` | 工具回环、并行顺序、中止、空流、panic |
| coding 单元 | `crates/coding/src/*` | CLI、会话树、提示词合并、工具行为、统计 |
| tui 单元 | `crates/tui/src/*` | 状态机、滚动、换行、Markdown/表格/高亮 |
| e2e | `crates/coding/tests/e2e.rs` | 真实二进制 + 本地 mock SSE：三种模式、`.env`、错误路径 |
| PTY 冒烟 | `scripts/tui_*.py` | 真实终端：启动、输入、流式渲染、干净退出 |

## 改代码前的定位表

| 症状 | 先看 |
|---|---|
| 模型返回空 / 流断了 | `llm/openai.rs` 的 SSE 循环 |
| tool call 丢参数 / 顺序乱 | `agent::Acc`、`execute_tools` |
| 恢复会话少了消息 | `coding/session.rs` 的 parent 回溯 |
| 提示词里没有项目规则 | `coding/prompt.rs` 的搜索顺序 |
| TUI 花屏 / 宽度错 | `tui/render.rs`、`tui/wrap.rs` |
| 代码块高亮错位 | `tui/markdown.rs` 的 `highlight` / 行号槽 |
| 成本不对 | `llm/model.rs`、`llm/deepseek.rs` 的峰谷判断 |
