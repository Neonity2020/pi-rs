# 架构

## 总览

```
┌───────────────────────────────────────────────────────────┐
│ crates/coding — bin `pi-rs`                               │
│ CLI · 三种模式 · 系统提示 · 会话 JSONL · 长期记忆 · 四工具  │
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
2. 模式层先把用户输入写成一条 `Message` 并追加到会话文件（`append_message`），交互模式同时发一个 `UiEvent::User`（只带图片标签，不带字节）。
3. `Agent::run_turn(message, sink, cancel)` 把用户消息追加到内存历史，开始 step 循环。图片随消息进入历史。
4. 每个 step：
   - 先估算上下文；超过模型窗口减去预留时，压缩旧消息（见“上下文压缩”），发 `AgentEvent::Compaction`；
   - 发 `AgentEvent::Step { step }`；
   - 调 `Provider::stream(ChatRequest, tx, cancel)`；
   - 事件经 `UnboundedSender<StreamEvent>` 回到 agent，边收边发 `AgentEvent::{Text, Reasoning, Usage}` 给 sink，同时累积成一条 assistant `Message`；
   - 流结束后提交 `AgentEvent::Assistant`，把消息追加到历史。
5. 没有 tool calls → `Outcome::Completed`，turn 结束。
6. 有 tool calls → 按模型调用顺序逐个执行：发 `ToolStart`，执行当前工具，发 `ToolEnd` 并以 `Message::tool` 追加历史，再处理下一项。每个结果先交给 sink 落盘，因此落盘失败的取消可以阻止同批后续工具；被跳过的调用仍补匹配的取消结果，保持 tool 对完整。完成后回到第 4 步。
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

- `OpenAiCompat` 同时服务 DeepSeek、MiniMax CN 与任意 OpenAI 兼容端点：`deepseek(...)` 额外携带 `thinking` / `reasoning_effort` 参数；`minimax_cn(...)` 携带 `reasoning_split: true`，把思考内容拆到 `reasoning_content`，与其它端点一样原样回传；`custom(...)` 两者都不带。
- SSE 按字节缓存再逐行解析 `data:`，支持跨分块的 UTF-8 和无末尾换行的记录；忽略注释与空行。必须收到 `finish_reason` 才算完整；`[DONE]` 或 EOF 前缺少该字段会报 `Error::Stream`。兼容已收到 `finish_reason` 但省略 `[DONE]` 的端点，且继续收集末尾 usage。带 tool call 的响应必须以 `tool_calls` 结束，`length` 等其他原因视为不完整工具响应。非法 JSON 或 UTF-8 也会报错；agent 保留已有正文，丢弃该响应的工具调用。HTTP 非 2xx 变成 `Error::Api`（含截断的响应体）。
- 网络与缓冲有固定边界：连接（含 TLS 握手）10 秒，单次读取空闲 120 秒，正常持续输出没有总时长上限；登录验证仍有 15 秒总超时。SSE 逐个行片段检查 1 MiB 上限再追加，不把整个传输 chunk 复制到缓冲。API 错误响应只读取前 2,000 字节并标注限制，达到上限立即放弃剩余正文；错误正文读取同样可取消。
- tool call 增量按 `index` 归位；没有 id 的调用在 agent 侧补 `call_{i}`。
- `Message` 是唯一消息类型：agent 历史、会话记录、线上格式三位一体。`reasoning` 必须随 assistant 消息原样回传（DeepSeek + tools 的硬要求）。用户消息可带 base64 `Image`；文本消息继续用字符串 `content`，带图消息在发送时展开成 OpenAI 的 `[{type:"text"},…,{type:"image_url"}]` parts，会话文件里仍是同一个 `Message`。
- `deepseek.rs` / `minimax_cn.rs` 维护模型注册表（上下文窗口、最大输出；DeepSeek 另有峰谷价格）。MiniMax M Plan 按套餐计费、不按 token 计价，所以其模型 `cost: None`，成本显示 `-`；未知模型照常工作，但成本同样显示 `-`。
- `llm::provider_models` / `provider_model` / `provider_of_model` 是注册表的统一入口：前者列模型，第二个把 id 解析成 `Model`（含族名别名），第三个给出该 id 属于哪个 provider。模型 id 在各注册表间不重名，所以它就是 `/model` 的路由表。
- 厂商的族名不一定等于 API 接受的 ID：MiniMax 的 M3.1 代际实际是 `MiniMax-M3.1-Flash`，直接发 `MiniMax-M3.1` 会被服务端以 `unknown model` 拒掉。`minimax_cn::canonical_id` 负责把族名解析成真实 ID，注册表与 wire 都用解析后的名字。
- `mock.rs` 的 `ScriptedProvider` 让 agent 测试无需网络。

## crates/agent

`Agent` 持有 provider、模型、系统提示、工具、工具 schema 与消息历史。

| 关注点 | 行为 |
|---|---|
| 取消 | `tokio::select!` 以 cancel 为最高优先级；不完整 tool calls 丢弃，保留文本 |
| 空流 | provider 一个事件都没发 → `Error::Stream("provider returned an empty stream")` |
| 工具批 | 按模型顺序执行；每项结果报告并写入历史后才执行下一项，避免依赖操作与同文件修改竞态 |
| 失败 | 未知工具、参数 JSON 坏、panic、取消 → `ToolResult::err`，继续循环 |
| panic 隔离 | 每个工具由一个受 `JoinSet` 管理的任务运行，panic 变成错误结果；取消时中止并等待任务销毁，丢弃 agent future 也不会留下脱离管理的内层任务 |
| 历史 | assistant 消息与 tool 结果都进入 `Agent::messages`，下一 step 原样回传 |
| 压缩 | 超阈值时总结旧前缀，替换为 summary + 最近尾部；只丢完整前缀，不拆 tool 对 |
| overflow | provider 报 context 超限时压缩并重试一次；仍失败才返回错误 |

`AgentEvent` 是唯一的对外事实来源：

```
Step · Text · Reasoning · Assistant · ToolStart · ToolEnd · Compaction · CompactionFailed · Usage
```

`Sink` 是同步、内联调用的消费者；因此实现者不应做长时间阻塞操作。`coding` 的 sinks 在此处落盘并转发 UI。

## 上下文压缩

目标是让长会话继续，而不是缩短回答。核心算法参考 Pi 的 `core/compaction`（阈值、overflow、结构化摘要、保留尾部），去掉了 extensions、branch summary 与独立的 turn-prefix 摘要；切在 turn 中间时只用一句提示告诉摘要器后续消息会原样保留。

**估算。** 优先用上一次 assistant usage 的实际上下文 token（输入 + 输出），之后新增的消息再按字符数 / 4 估算；没有 usage（例如刚恢复会话）时全部按字符估算。图片按固定 4800 字符估算（base64 字节数与图片 token 不成比例）。system prompt 与 tool schema 也计入。`context_window == 0` 的未知模型不自动压缩。

**阈值。** `tokens > context_window - reserve`，默认 `reserve = 16k`、保留尾部 `keep = 20k`；窗口较小时按 `window/4`、`window/2` 收紧。未知窗口用手动/overflow 模式，使用默认值。

**切点。** 从最新往旧累计，落在 user 或 assistant 消息上，**永不落在 tool 结果上**。切在 assistant 时，它后面的 tool 结果天然被保留，因此 tool call / result 不会被拆开。若第一条旧消息本身超过预算，则退到下一个合法切点，而不是声称“无可压缩”。

**摘要请求。** 使用当前 provider/模型，独立 system prompt + 一条包含 `<conversation>` 的 user 消息；不传工具，`max_tokens` 上限 8k。旧消息序列化为文本：assistant 的 reasoning 省略，tool 结果截到 2000 字符，图片只留类型标记；总字符数按窗口限制（最多 400k）。已有摘要时用 update prompt，把旧摘要放进 `<previous-summary>`。切点落在 assistant（同一 turn 中间）时，额外提示摘要器后续消息会原样保留。摘要末尾附 `<read-files>` / `<modified-files>`。

**替换。** 内存历史变成 `summary user message + 旧历史[cut..]`。summary 仍是普通 `Message::user`，线上/会话格式不变；`AgentEvent::Compaction` 带 `first_kept_index`，由 sink 落盘。**旧消息不删除**，只是不再发送。

**恢复。** 会话里的 `{"type":"compaction"}` 记录摘要、`first_kept_id`、压缩前后估算。加载时取最新一条，重放出 summary 消息 + 保留尾部。坏掉的 `first_kept_id` 回退到完整历史，避免静默丢消息。

**触发。** 每个 step 前自动检查；`/compact` 手动触发（忙时拒绝）；provider 返回 context overflow 时强制压缩并重试一次。自动压缩失败只发 `CompactionFailed`，不中止循环；overflow 恢复失败才返回原始错误。

## crates/coding

### 启动顺序

1. 加载 `.env`（工作目录或最近上级；真实环境变量优先；坏文件只警告）。
2. 解析参数（手写，不引 clap）。`--json` 必须有 prompt；`login` / `logout` 是子命令。
3. 取得 cwd，解析模型，再按 `--api-key` > 环境变量 > `auth.json` 取 key，构造 provider。
   交互模式允许没有 key：用占位 provider 启动，输入框提示 `/login`；print/json
   模式直接给出可操作的错误。`logout` 删除 auth.json 里的条目后退出。
4. 选择工具集：默认四个，`--tools read,bash` 取子集，未知名字直接失败。
5. 组装系统提示：`SYSTEM.md`（或基础提示）+ 逐级 `AGENTS.md` + 有界长期记忆。
6. 新建会话，或 `-c` 载入最近会话的活动分支。
7. 进入模式；三种模式共用 `Shared`（会话 + 统计 + 模型）与 `modes::Turn`。

### 认证（auth.json）

凭据是 `$PI_RS_HOME/auth.json`（默认 `~/.pi-rs`），格式与 Pi 一致——顶层按
provider 分键：

```json
{
  "deepseek": {
    "type": "api_key",
    "key": "sk-...",
    "base_url": "https://api.deepseek.com",
    "updated_at": "2026-10-17T22:10:00Z"
  }
}
```

- 写入只发生在 `pi-rs login` / TUI `/login`：先 `GET {base_url}/models` 验证 key，
  成功后才落盘（临时文件 + rename，文件 0600，目录 0700）。401/403 直接报错，
  不会留下坏凭据。
- 读取优先级：`--api-key` > 环境变量（含 `.env`）> `auth.json`。`base_url`
  优先级：`--base-url` > auth.json > provider 默认值。
- 交互模式的 `/login` 把输入框换成掩码输入；验证与保存跑在后台任务里，成功后
  通过 `Agent::set_provider` 换掉当前模型所属的 provider，会话历史不丢。验证期间
  掩码输入框屏蔽其他命令，不能被新的模型切换抢先覆盖。`/logout` 只删除当前
  provider 的 auth.json 条目；如果环境变量或启动参数仍供给它的 key，则重新构建
  provider 继续使用该 key，否则清除运行时凭据并标记为未登录。
- 损坏的 auth.json 是硬错误（提示修好或删除），避免静默覆盖别的 provider。
- `pi login` 的 key 来源：`--api-key` > provider 环境变量 > stdin（管道）>
  TTY 隐藏输入。TUI 内的 `/login` 总是要求重新输入。

### 提示词历史

TUI 里 `Up` / `Down` 翻阅历史 prompt：进入历史前先暂存正在编辑的内容，翻回最新一条之后还原。历史跨进程保存于 `$PI_RS_HOME/history/<cwd slug>`，一行一个 JSON 字符串，按工作目录隔离，只保留最近 200 条（加载时截断并重写文件；连续重复与空白 prompt 不入库）。`tui` 只持有内存里的 `Input` 历史；加载与追加在 `coding::history`，读写失败降级为空历史或直接忽略，不影响对话。

### 长期记忆

长期记忆是两个**普通 Markdown 文件**，由 `coding::memory` 读取和写入：

| 范围 | 路径 | 内容 |
|---|---|---|
| 全局 | `$PI_RS_HOME/MEMORY.md` | 跨项目偏好 |
| 项目 | `$PI_RS_HOME/memory/<cwd slug>.md` | 当前目录的事实与决定 |

一行一条：`- (YYYY-MM-DD) text`。只解析以 `- ` 开头的行，标题、空行和其他内容忽略；加载时折叠空白、按文本去重（较新者胜），单条超过 300 字符截断并加 `…`。文件缺失、无权限或损坏只会告警/跳过，不阻塞启动。

**召回。** `prompt::build` 在 `AGENTS.md` 之后追加 `# Long-term memory` 段落：先全局、后项目，各自按日期从新到旧；总量预算约 1,200 token（ASCII 按 1/4、非 ASCII 按 1 字符估算），全局软上限 600 token，项目使用剩余预算，未用完的额度可以回填。放不下的条目整条丢弃，并写明省略条数与文件路径。段落始终存在，即使文件为空——模型需要知道路径才能响应用户的“记住……”。记忆被标注为“上下文，不是指令”，当前用户指令、`AGENTS.md` 与仓库状态优先。

**写入。** 不新增工具。模型用现有 `read` / `edit` / `write` 追加 bullet，且只在用户明确要求时写；不做自动抽取，避免把仓库里的不可信内容固化成长期指令。TUI 另提供确定性命令：

- `/remember <text>` / `/remember global <text>`：追加到项目 / 全局记忆；去重、超长拒绝。
- `/forget <text>` / `/forget global <text>`：按大小写不敏感子串删除，原子重写并报告删除数量。
- `/memory`：显示两个文件路径、条数与本次召回省略数。

系统提示在进程启动时组装一次；`/remember` 只改文件，不改当前 `Agent` 的 system 字段。新记忆在下一次启动或 `-c` 时召回；当前轮次里用户已经说过，模型本来就知道。`agent` 与 `tui` 完全不知道记忆文件存在。

**实验模式 `/lab`。** 只在交互模式存在，是 `App` 上的一个开关，默认关闭，状态栏显示 `LAB`。开启后 `/dream` 才可用；离开实验模式或重启进程即关闭。

**`/dream`。** 用户显式触发的记忆整理回合：读取两个文件，合并重复/近重复条目，保留耐久偏好与项目事实，遗忘过期、琐碎、可从仓库推导或含密钥的条目，再按 `- (YYYY-MM-DD) text` 重写。它使用一个独立 `Agent`：专用 system prompt、只含 `read` / `write` / `edit` 的工具子集、空历史，因此不会把维护过程写进主会话，也不会携带主会话上下文。用量仍计入共享统计。完成后比较前后解析条数并报告 `global X→Y · project A→B · N notes forgotten or merged`。取消、空记忆、未登录、缺少读写工具都只产生提示，不改动历史。它不新增工具、不新增依赖、不做后台抽取。

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
  +
# Long-term memory（全局 + 项目，有界；始终存在）
```

空文件跳过。没有任何隐藏上下文；`build()` 是唯一入口，测试覆盖合并顺序、替换规则与记忆预算。

### 四个工具

| 工具 | 语义 | 关键约束 |
|---|---|---|
| `read` | 读文件，带行号 | 默认 2000 行，支持 `offset`/`limit`；单行显示最多 2000 字符，总输出最多 64 KiB，给出继续读取的 offset |
| `write` | 创建/覆盖 | 自动创建父目录 |
| `edit` | 精确字符串替换 | 要求唯一匹配，或显式 `replace_all` |
| `bash` | 执行 shell 命令 | 前台进程、可取消；默认 120s 超时；持续排空 stdout/stderr，各保留至多 16 KiB 原始头尾，总结果最多 64 KiB |

`write` / `edit` 共用 `tools::atomic`：同目录临时文件完整写入并同步后 rename，失败或提交前取消则清理临时文件并保留原文。文件提交使用同步 I/O，在工具任务返回后没有后台写入继续修改目标。保留已有普通文件的权限；有效符号链接解析到目标，保留链接本身；拒绝悬空链接、只读文件、目录和设备等特殊文件。原子替换只更新指定目录项，其他硬链接保留旧内容。不承诺并发外部修改检测或断电后的目录持久性。

`read` 用有界缓冲扫描到请求范围，跳过的行不保留内容，超长行仅保留足够展示的 UTF-8 前缀；不再预读整份文件或构造全部行索引。达到行数上限时仅探测是否还有内容，报告 `more lines` 和下一次 offset，避免为总行数继续扫描。达到字节预算时保留整行并给出第一条未展示行的 offset。只校验显示前缀的 UTF-8，范围外内容不影响已请求的范围。

`bash` 的管道采集不会在预算满后停读，否则子进程会卡在满管道上。采集仅保留每路头尾和省略字节数，之后才做行数与最终 UTF-8 字节预算裁剪；不会先把全部输出放进内存再截断。

Unix 下 `bash` 以独立进程组启动。取消、超时或读取输出失败时，向该组发送 SIGKILL 并等待直接子进程回收；future 被丢弃时，由 guard 发出同样的组信号，直接子进程仍由 tokio 的 `kill_on_drop` 清理。覆盖仍处于该组的子孙进程；主动脱离进程组的进程不在这一机制范围内。非 Unix 平台保留直接子进程终止与回收，不宣称进程树清理。为此 `coding` 在 Unix 上直接声明已有依赖树中的 `libc`，使用系统提供的 `killpg` 和信号常量，避免外部命令和手写平台常量。

注册点只有一处：`tools::NAMES`、`tools::all()`、`tools::specified()`。**第五个工具是产品变更**，先改 `docs/goals.md`。

工具失败不是 `Err`，而是：

```rust
ToolResult { content: String, is_error: bool }
```

### 会话

路径：`$PI_RS_HOME/sessions/<cwd slug>/<timestamp>-<id>.jsonl`（默认 `~/.pi-rs`）。每行是一条 message 或 compaction 记录。

```jsonl
{"type":"session","v":2,"id":"...","cwd":"/work","model":"deepseek-flash","created":"2026-..."}
{"id":"...","parent":null,"ts":"...","type":"message","message":{"role":"user","content":"..."}}
{"id":"...","parent":"...","ts":"...","type":"message","message":{"role":"assistant","content":"...","reasoning":"..."}}
{"id":"...","parent":"...","ts":"...","type":"message","message":{"role":"tool","content":"...","tool_call_id":"..."}}
{"id":"...","parent":"...","ts":"...","type":"compaction","compaction":{"summary":"## Goal...","first_kept_id":"...","tokens_before":984000,"tokens_after":21000}}
```

- 每行一个 JSON，追加写；`id` + `parent` 使文件在格式上是一棵树。
- compaction 行只描述“从哪里开始保留 + 摘要”，不复制或删除旧消息；加载时重放最新一条。
- 恢复时从最后一条沿 `parent` 回溯到根，得到活动分支。
- 最后一条非空行损坏（包括 UTF-8 字符写到一半）时，先将原始字节完整备份到同目录 `<session>.jsonl.<id>.bak`，再截掉坏尾部并告警；备份失败则拒绝修复。合法末行没有换行时补上换行，确保后续追加不会拼接到上一条记录。
- 中间坏行、重复 ID、缺失父节点或父链环路直接报错，保持文件原样；加载时建立 ID 索引，父链校验与分支重建均为线性遍历。
- 当前没有分支导航 UI；需要分支时再实现，不重写历史格式。

### Sinks 与统计

`Shared` 持有会话、统计、模型、当前回合的取消令牌和首个持久化错误。用户输入先经 `Shared::append_message` 落盘；每个 sink 先 `record(event)`：

| 事件 | 落盘/统计 |
|---|---|
| `Assistant` | 会话追加 assistant 消息 |
| `ToolEnd` | 会话追加 tool 消息 |
| `Compaction` | 追加 compaction 记录，更新会话上下文映射 |
| `Usage` | 累加 token 与估价（含摘要请求） |

之后 print/json/tui sink 再做各自的展示。TUI 状态栏的缓存命中率按 `cached_tokens / input_tokens` 四舍五入，尚未收到 usage 或输入为 0 时显示 `cache -`。`stats` 只在进程内累计，不落盘；`-c` 恢复历史消息，但 token/成本从 0 重新计。成本按模型注册表估算；DeepSeek 用峰谷价（UTC 工作日 01–04、06–10 点为峰时，不含中国法定节假日），MiniMax M Plan 订阅不估价。

任一消息或 compaction 写入失败，`Shared` 保留首个错误并同步取消当前回合，停止后续会话写入、模型请求和剩余工具调用。assistant 消息未落盘时，其工具不会执行；已经执行的工具不回滚。print 模式在 stderr 报错并以非零状态退出；JSON 最终输出 `outcome:"error"` 并非零退出；TUI 显示错误并结束忙碌态，拒绝继续提问、`/compact` 和 `/dream`，退出时返回失败。即使之后路径恢复可写，也必须修复文件并重启，避免继续使用与磁盘不一致的内存历史。这里复用现有取消机制，`agent` 无需知道文件或会话语义。

### UI 接线

- 独立线程跑 `crossterm::event::read`，把按键/滚轮/左键点击/尺寸变化送进 channel；左键带着坐标，由 `App::on_click` 命中 thinking header。
- 主循环 `select!` 在输入、`UiEvent`、120ms ticker 之间调度；只有 `dirty` 时重绘。
- `tui::commands` 是斜杠命令的唯一表格（名字、参数提示、一行说明、是否 lab-only）；`/help` 文本和补全菜单都从它生成，避免两处漂移。输入框内容是一个不以空白结尾的 `/` 前缀时 `App::completion` 打开，`Up` / `Down` 改选、`Tab` 补全、`Esc` 关闭；前缀尚未等于完整命令名时 `Enter` 先补全，不把半截命令发给应用层。补全只改输入框，不产生 `Action`。
- `Action::Submit` 先看首 token：`/login`、`/logout`、`/compact`、`/model`、`/remember`、`/forget`、`/memory`、`/lab`、`/dream`、`/help` 由应用层处理，其余才触发 `spawn_turn`。未知斜杠命令只报错，不发给模型。斜杠命令是纯文本的：已附图片会退回输入框，留给下一条消息。`/remember` / `/forget` / `/memory` 只读写 `coding::memory` 的文件，不打断当前 turn。
- `/model` 无参时列出所有 provider 的注册模型（`*` 标记当前）；带 id 时切换模型。模型 id 到 provider 的映射由 `llm::provider_of_model` 给出，族名先经 `llm::provider_model` 解析成 API 真实接受的 ID。同 provider 切换只改模型；跨 provider 切换还要解析那个 provider 自己的凭据并重建 provider（`--api-key` / `--base-url` 只属于启动时的 provider，不跟着走）。切换不中断对话，不改写会话文件里已记录的内容；忙时直接拒绝。跨 provider 切换成功同步更新登录状态，启动时未登录也可切到已有凭据的 provider 后直接聊天。`/login` / `/logout` 读取共享模型的当前 provider；启动时的参数只对启动 provider 有效。
- `/lab` 只翻转 `App::lab`；`/dream` 在 lab 关闭、忙碌、未登录、记忆为空或工具不足时直接拒绝/提示。满足条件时把 `Cancel` 交给隔离的 dream 任务，`TuiSink::ephemeral` 展示思考与工具活动、累计用量，但不追加会话记录；结束统一发 `Busy(false)`。
- `/compact` 在空闲时拿 agent 锁执行一次强制压缩；`TuiSink` 负责落盘并把 `Compaction` / `CompactionFailed` 翻成提示。忙时直接拒绝，不打断当前 turn。
- `Ctrl+V` 在 `tui` 里只产生 `Action::PasteImage`；`coding::clipboard` 读系统剪贴板（macOS `osascript`、Linux `wl-paste`/`xclip`、Windows PowerShell），把图片元数据装进 `UiEvent::ImageAttached`。字节留在 `App` 的待发送附件里，提交时随 `Message` 进会话。
- 未登录时普通提交被拦下并提示 `/login`，不产生模型请求；已附的图片会退回输入框，避免静默丢失。
- `/login` 的掩码输入属于 `App`（`Action::Login(key)`）；验证、写盘、换 provider 都在后台任务，结果以 `UiEvent::LoginDone` 回到主循环（关闭输入框、显示成功/失败）。
- `Action::Submit` 触发 `spawn_turn`：后台拿 agent 锁跑一轮，`TuiSink` 把 `AgentEvent` 翻成 `UiEvent`，主循环消费并更新 `App`。
- `TerminalGuard` 在 `Drop` 中恢复 raw mode 与主屏幕；即使出错也不把终端留在 alt screen。

## crates/tui

**无 I/O。** 输入是 `UiEvent` 与按键，输出是 `Action` 和渲染结果；终端由 `coding` 驱动。

- `App`：消息项、输入框、忙碌态、滚动/跟随、状态栏、spinner、版本号。
- `UiEvent`：用户消息、assistant 增量/最终文本、reasoning 增量、工具开始/结束、忙闲、状态刷新；reasoning 与工具事件都聚合成 `Item::Thinking`。
- `Action`：`Submit(Submission)` / `PasteImage` / `Interrupt` / `Quit` / `None`；`Submission` 带文本和待发送图片，`PasteImage` 只请求应用层读剪贴板（TUI 不做 I/O）。
- 输入框在有附件时多一行：`[1] PNG 640×480 · 148 B`；`Esc` 删除最后一张，`Ctrl+C` 依次清空文本、附件、退出。
- `Up` / `Down` 调用 prompt 历史：`coding` 启动时把 `$PI_RS_HOME/history/<cwd slug>` 读进 `Input`，每次提交再追加回该文件；`tui` 不做 I/O。
- 推理和工具调用折叠为 `Item::Thinking`。折叠时一行 `▸ 思考结果 · N 行推理 · N 个工具 · 点击展开`（reasoning 还在流式时是 `⠋ 正在思考…`；首个 answer token 或工具调用到达即结束该状态）；展开后显示原始 reasoning、工具名/参数/状态/输出预览。左键点 header 切换单块，`Ctrl+T` 全局切换并清空单块覆盖。
- 渲染：transcript（Markdown）→ 状态栏 → 输入框。渲染时刷新 `viewport` / `max_scroll` 与 thinking header 的行号；滚轮脱离跟随，回到底部自动恢复跟随。
- 鼠标左键点击 thinking header：`coding` 把 `MouseEventKind::Down(Left)` 作为坐标送进 `App::on_click`，用渲染时记录的 header 行号命中并切换该块。
- `markdown.rs`：围栏代码（行号 + 语法高亮）、标题、列表、引用、表格、分隔线、行内代码/强调/链接；无法识别的内容按字面输出，**绝不丢字**。
- `wrap.rs`：按显示宽度换行（CJK 宽字符、tab 展开、长词硬断）。

## 不变量

1. 依赖方向只允许 `coding → agent → llm` 与 `coding → tui`。
2. 消息只有一种格式，不做 wire/session 转换；唯一例外是带图消息在发送时由 provider 展开为 OpenAI content parts，会话仍存同一个 `Message`。
3. assistant 的 `reasoning` 原样保留并回传。
4. 工具失败是 `ToolResult::is_error`，不是 `Err`，不终止循环。
5. 工具集合是产品决定：默认且仅四个。
6. 没有 max steps。
7. 基础系统提示保持在约 400 token 以内。
8. 会话文件正常运行时只追加；每条记录有 `id` 与 `parent`。唯一截断例外是恢复时已完整备份的损坏尾部，不重写保留的记录。
9. `tui` 不碰 I/O；终端生命周期属于 `coding`。
10. provider 不知道会话、提示词文件或终端。
11. 取消令牌贯穿 HTTP 请求与工具；工具任务在取消时中止并等待销毁，Unix bash 的组信号由清理 guard 保证。
12. 预览/渲染失败不能吞掉内容。
13. 压缩只丢完整的旧前缀；切点不落在 tool 结果上，不拆 tool call/result，不改写仍在上下文里的 reasoning。摘要是一条普通 user 消息，不新增消息类型。

违反任意一条，视为架构变更：同时更新本文件与测试。

## 失败模型

| 故障 | 表现 | 恢复方式 |
|---|---|---|
| 缺 API key（print/json） | 启动即失败，提示 `pi login` / 环境变量 / `.env` / `--api-key` | 登录或配置后重试 |
| 缺 API key（TUI） | 正常启动，输入框提示未登录；普通提问被拦下 | `/login` |
| 登录 key 被拒 | 不写 auth.json，登录框关闭并显示错误 | 重新 `/login` |
| auth.json 损坏 | 启动报错并给出文件路径 | 修复或删除该文件 |
| HTTP 非 2xx | `llm::Error::Api { status, body }` | 修正 key/端点/模型 |
| 网络中断 | `llm::Error::Http` | 重试 |
| SSE chunk 坏 / 空流 | `Error::Stream` | 重试；持续则查 provider |
| 连接或响应读取停滞 | `Error::Http` 超时，保留已有正文，丢弃该响应的工具调用 | 检查网络或端点后重试 |
| SSE 单行超过 1 MiB | `Error::Stream`，超限行不进入事件流 | 缩小单次响应或修复端点 |
| SSE 缺少 finish_reason / 工具调用以 length 等原因截断 | `Error::Stream`，保留已有正文，丢弃工具调用；JSON 最终为 error | 检查端点协议或重新提问 |
| 工具参数不是 JSON | `ToolResult::err`，回给模型 | 模型自我修正 |
| 工具 panic | 捕获为错误结果 | 循环继续 |
| 用户取消 | `Outcome::Aborted`，保留已产出文本 | 重新提问 |
| 会话末行损坏 | 备份原文件、告警并移除损坏尾部 | 自动恢复；备份失败则报错 |
| 会话中间坏行 / ID 或父链损坏 | 明确报错，文件保持原样 | 依据错误路径手动修复后重启 |
| 会话写入失败 | 取消当前回合，保留首个错误，拒绝后续模型操作；三种模式均报告失败 | 修复存储或文件后重启 |
| `first_kept_id` 丢失 | 回退到完整历史，不静默丢消息 | 继续对话；必要时重新 `/compact` |
| 上下文超过窗口 | 自动/手动压缩；provider 报 overflow 时压缩并重试一次 | 仍失败则开新会话或减小输入 |
| 摘要请求失败 / 返回工具调用 / 被长度截断 | 自动/overflow 发 `CompactionFailed`；手动压缩返回错误；历史不变 | 自动压缩继续发原请求；手动压缩重试 |
| 历史 prompt 文件损坏 | 跳过坏行；读不出就是空历史 | 继续使用，新 prompt 照常追加 |
| 剪贴板没有图片 | 输入框上方提示，不产生消息 | 复制图片后重试 |
| 剪贴板图片过大/格式不支持 | 错误提示，不进入会话 | 压缩或转成 PNG/JPEG |
| TUI 出错 | `TerminalGuard::drop` 恢复终端 | 检查 stderr |
| `/dream` 失败 / 取消 | 显示错误或 `dream interrupted`；记忆文件保持不变或保留已写入部分 | 重新 `/dream`；先检查记忆文件 |
| `/dream` 缺少读写工具 | 启动时按 `--tools` 选择的子集执行；不足则提示 | 用默认四工具重启 |

## 扩展点

| 想加什么 | 改哪里 | 是否动内核 |
|---|---|---|
| 新 provider | 实现 `llm::Provider`；在 `coding::build_provider` 接线 | 否 |
| 新工具 | 实现 `agent::Tool`；在 `coding/tools` 注册 | 是（需先改目标） |
| 新展示方式 | 实现 `agent::Sink` | 否 |
| 新模型/价格 | `llm/deepseek.rs`、`llm/minimax_cn.rs` 或 provider 自己的注册表 | 否 |
| 新运行模式 | `coding/modes.rs` + `cli::Mode` | 否 |

步骤与代码骨架见 [development.md](development.md)。

## 测试地图

| 层 | 位置 | 保护什么 |
|---|---|---|
| llm 单元 | `crates/llm/src/*` | SSE 解析、请求体、reasoning 回传、峰谷价 |
| agent 单元 | `crates/agent/src/*` | 工具回环、顺序执行与逐项结果报告、取消后跳过剩余调用、非协作工具销毁、空流、panic、token 估算、切点、摘要序列化、自动/手动/overflow 压缩 |
| coding 单元 | `crates/coding/src/*` | CLI、会话树、compaction 重放、提示词合并、长期记忆的解析/预算/写入/删除/计数、`/dream` 的提示词与工具子集、工具行为、统计、auth.json、提示词历史、剪贴板图片的格式/尺寸/base64、斜杠命令解析 |
| tui 单元 | `crates/tui/src/*` | 状态机、滚动、历史召回、斜杠命令列表/补全、thinking 折叠/点击、换行、图片附件、Markdown/表格/高亮 |
| e2e | `crates/coding/tests/e2e.rs` | 真实二进制 + 本地 mock SSE/HTTP：三种模式、`.env`、`pi-rs login`/`logout`、key 优先级、错误路径、context overflow 压缩重试与会话落盘 |
| PTY 冒烟 | `scripts/tui_*.py` | 真实终端：启动、输入、流式渲染、干净退出 |

## 改代码前的定位表

| 症状 | 先看 |
|---|---|
| 模型返回空 / 流断了 | `llm/openai.rs` 的 SSE 循环 |
| tool call 丢参数 / 顺序乱 | `agent::Acc`、`execute_tools` |
| 恢复会话少了消息 | `coding/session.rs` 的 parent 回溯与 compaction 重放 |
| 压缩后模型忘了目标 / 摘要质量差 | `agent/compaction.rs` 的摘要 prompt、序列化截断、保留尾部 |
| 压缩后 tool call 与结果对不上 | `agent/compaction.rs` 的 `find_cut_index`；切点不能是 tool 结果 |
| Ctrl+V 没有附上图片 | `coding/clipboard.rs` 的平台工具、`tui::Action::PasteImage` 的接线 |
| 提示词里没有项目规则 | `coding/prompt.rs` 的搜索顺序 |
| TUI 花屏 / 宽度错 | `tui/render.rs`、`tui/wrap.rs` |
| 代码块高亮错位 | `tui/markdown.rs` 的 `highlight` / 行号槽 |
| 成本不对 | `llm/model.rs`、模型注册表、`llm/deepseek.rs` 的峰谷判断 |
