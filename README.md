# pi-rs

一个用 Rust 重写的极简 coding agent，遵循 [Pi](https://pi.dev/) 的哲学：

> 奥卡姆剃刀：如无必要，勿增实体。
> 如果我不需要它，它就不会被构建。

四把工具（`read` / `write` / `edit` / `bash`）、一段几百 token 的系统提示、一个只做一件事的 agent 循环、纯文本的会话记录。默认接 DeepSeek（`deepseek-flash`），任何 OpenAI 兼容端点都能替换。

## 层次

| 层级 | Crate | 职责 | 依赖 |
|---|---|---|---|
| 应用层 | `crates/coding` | CLI、TUI、会话 JSONL、系统提示、四个内建工具 | `agent`、`tui`、`llm` |
| 循环层 | `crates/agent` | 模型请求 → 工具执行 → 事件流（不知道终端和文件格式） | `llm` |
| 提供层 | `crates/llm` | 流式聊天、SSE 解析、DeepSeek / OpenAI 兼容 / MiniMax CN、模型与价格 | — |
| 渲染层 | `crates/tui` | 终端 UI 状态与渲染（不碰 I/O） | — |

依赖方向严格单向：`coding → agent → llm`，`tui` 是叶子。每层可以单独使用——`llm` 是完整的流式客户端，`agent` 是完整的 agent 循环。

## 快速开始

```bash
cargo install --path crates/coding      # 或：cargo run -p pi-coding
pi-rs                                   # 打开 TUI，输入 /login
```

`/login` 会提示粘贴 DeepSeek API key（输入不回显），先用 `GET /models` 验证，再把凭据写进 `~/.pi-rs/auth.json`（权限 0600）。验证失败不会保存。终端里也可以直接：

```bash
pi-rs login                 # 隐藏输入；也接受 --api-key 或管道
pi-rs logout                # 删除保存的凭据
```

凭据优先级：`--api-key` > 环境变量 / `.env` > `auth.json`。所以 CI 仍可注入 `DEEPSEEK_API_KEY`，不需要改用户的 auth 文件。

不想用登录的话，把 `DEEPSEEK_API_KEY=sk-...` 写进 `.env` 同样有效（模板：仓库里的 `.env.example`，`cp .env.example .env`）：启动时会从工作目录或最近上级目录加载，真实环境变量优先。

MiniMax 国内 M Plan 订阅内置为 `minimax-cn`（OpenAI 兼容接口）：

```bash
export MINIMAX_CN_API_KEY=sk-cp-...     # M Plan 订阅 Key
pi-rs --provider minimax-cn             # 默认模型 MiniMax-M3.1-Flash
pi-rs login --provider minimax-cn       # 或先验证并保存到 auth.json
```

订阅按套餐计费、不按 token 计价，所以成本显示 `-`；模型窗口仍参与自动压缩。`--model MiniMax-M3` 可切换当前另一个 1M 上下文模型。

MiniMax 的 M3.1 代际在 API 里的真实 ID 是 `MiniMax-M3.1-Flash`（直接发 `MiniMax-M3.1` 会被服务端以 `unknown model` 拒掉）。pi-rs 认得族名：`--model MiniMax-M3.1` 会解析成 `MiniMax-M3.1-Flash` 再发出去。

```
pi-rs                       # 交互式 TUI
pi-rs "解释这个仓库的结构"     # 单次执行，流式输出到 stdout
pi-rs -p < prompt.txt       # 从 stdin 读 prompt
pi-rs --json -p "..."       # 事件流以 JSONL 输出，可脚本化
pi-rs -c                    # 继续本目录最近一次会话
```

常用参数：

```
--model <id>          模型（默认 deepseek-flash，或 deepseek-v4-pro）
--provider <name>     deepseek | openai | minimax-cn
--base-url <url>      任意 OpenAI 兼容端点
--no-thinking         关闭 DeepSeek 思考模式
--effort <level>      推理强度 low | high | max
--tools <list>        限制工具集，如 --tools read,bash
```

TUI 内的斜杠命令：`/login` 登录（输入框切换为掩码）、`/logout` 删除保存的凭据、`/compact` 手动压缩上下文、`/model` 列出可用模型、`/model <id>` 切换模型、`/remember <text>` 记住一条项目事实、`/remember global <text>` 记住跨项目偏好、`/forget <text>` 删除匹配的项目记忆、`/forget global <text>` 删除匹配的全局记忆、`/memory` 查看记忆文件、`/lab` 开关实验模式、`/dream` 在实验模式里整理并遗忘长期记忆、`/help` 列出命令。输入 `/` 会弹出命令列表，`Up` / `Down` 选择、`Tab` 补全、`Esc` 关闭；只输入了命令前缀时按 `Enter` 也会先补全。没有凭据时 TUI 仍会启动，普通提问会被拦下并提示先 `/login`。

`/model` 列出所有 provider 的已知模型，`*` 标记当前模型。`/model <id>` 切换到指定模型，历史对话保留，状态栏同步更新。id 可以是族名（`MiniMax-M3.1`），会解析成 API 真实接受的 ID（`MiniMax-M3.1-Flash`）。切换到另一个 provider 的模型时需要该 provider 自己的凭据（`MINIMAX_CN_API_KEY` 或 `pi-rs login --provider minimax-cn`）；`--api-key` 和 `--base-url` 只属于启动时的 provider，不跟着走。切换只影响之后的请求，不改写会话文件里已记录的内容。 `/login` 和 `/logout` 始终操作当前 provider；切换成功后可直接继续聊天，登录验证期间输入框锁定。

`Up` / `Down` 在输入框里翻阅历史 prompt（跨会话，按工作目录保存于 `~/.pi-rs/history/<cwd 的 slug>`）；正在编辑的内容会先暂存，翻回最新一条后还原。

`Ctrl+V` 从剪贴板读一张图（macOS 用系统 `osascript`，Linux 用 `wl-paste` / `xclip`），显示在输入框下方；`Enter` 连同文字一起发送，`Esc` 删除最后一张。图片以 base64 存进会话，发送时按 OpenAI 多模态 `content` parts 展开，所以需要模型/端点支持图片输入；超过 10 MB 会被拒绝。

模型的推理和工具调用一起折叠成一行：推理进行时显示「正在思考…」，结束后变成「思考结果」。鼠标左键点这一行展开/收起，`Ctrl+T` 全局展开/收起。展开后能看到原始 reasoning、工具调用与输出预览。

接近模型上下文窗口时，pi-rs 会先压缩再发请求：把较早的对话交给当前模型总结成结构化 checkpoint，保留最近一段原文（默认约 20k token 的尾部），摘要作为一条 `compaction` 记录写进会话。压缩只发生在完整消息边界，不会拆开 tool call 与它的结果，也不会改写仍在上下文里的 `reasoning_content`。每次请求前检查；如果服务端先报了 context overflow，会压缩并重试一次。`/compact` 随时手动触发；未知窗口的 OpenAI 兼容端点不会自动压缩，但 `/compact` 和 overflow 恢复仍会尝试；窗口太小导致摘要本身放不下时，只能开新会话。

## 设计决定（对 Pi 的忠实翻译）

**奥卡姆剃刀是唯一的架构原则。** 每个依赖、运行模式、配置项、消息类型、常驻状态和抽象层都是一个“实体”，都要证明自己不可替代。四把工具够用，就不加第五把；`Message` 同时是线上格式和会话记录，就不加转换层；文件能保存状态，就不引入数据库。小不是省出来的，是删出来的。

**极短系统提示。** 基础提示约 300 token：模型已经知道什么是 coding agent，长篇说明只会在注意力里和你的代码竞争。个性化全部走 `AGENTS.md`（全局 `~/.pi-rs/AGENTS.md`，然后从文件系统根目录逐级合并到 cwd，深者优先），或用 `SYSTEM.md` 整体替换基础提示。

**只有四个工具。** 搜索、构建、测试、git 全部走 `bash`。

| 工具 | 作用 | 失败语义 |
|---|---|---|
| `read` | 读取文件（带行号） | `ToolResult::is_error`，不回滚循环 |
| `write` | 创建或覆盖文件 | `ToolResult::is_error`，不回滚循环 |
| `edit` | 精确字符串替换 | `ToolResult::is_error`，不回滚循环 |
| `bash` | 执行 shell 命令 | `ToolResult::is_error`，不回滚循环 |

工具失败不是异常，而是 fed back 给模型的结果文本（`is_error: true`）；参数 JSON 解析失败、未知工具名、panic 也一样——循环永不为工具而中断。

同一响应中的工具按调用顺序执行，每项结果保存后才开始下一项，支持先写文件再编辑等依赖操作。取消或结果保存失败后，剩余工具不会启动。Unix 下 bash 取消、超时或其 future 被丢弃时会终止原进程组内的子孙进程；主动脱离该组的进程不在此范围，其他平台目前只清理直接子进程。

`write` / `edit` 先写同目录临时文件，完整写入后才替换目标；写入失败或提交前取消时保留原文。保留已有文件的权限，跟随有效符号链接；拒绝悬空链接、只读文件和非普通文件。替换一个硬链接路径不会改动它的其他链接。

`read` / `bash` 的预览最多 64 KiB。`read` 按范围读取，超长行截断，预算满时给出继续读取的 offset；不会为了统计总行数读完整个文件。`bash` 保留 stdout/stderr 头尾并标注省略，仍会持续排空管道，让命令正常结束。

SSE 响应需要 `finish_reason` 才算完整；只收到部分正文或 `[DONE]` 就断开会报告错误、保留已生成正文并丢弃工具调用。已收到合法结束原因的兼容端点可以省略 `[DONE]`；带工具调用的响应必须以 `tool_calls` 结束，因长度等原因截断的调用不会执行。

连接超时为 10 秒，响应读取空闲超时为 120 秒；持续有数据的长流可以继续。SSE 单行超过 1 MiB 会报错，API 错误正文只保留前 2,000 字节。

**没有 max steps。** 循环到模型不再调用工具为止，或者被 `Ctrl+C`（取消令牌贯穿 HTTP 请求与工具进程）、或者失败。没有"步数上限"这种旋钮，因为从没用过。

**上下文是预算，不是终止条件。** 接近窗口上限时，旧消息被总结成结构化 checkpoint，最近一段保持原文；摘要作为 `compaction` 记录留在会话里，`-c` 能从这里继续。只丢完整的旧前缀，不拆 tool call 与结果，也不改写仍在上下文里的 reasoning——压缩是为了继续工作，不是悄悄改变历史。

**长期记忆也是文件。** 全局 `~/.pi-rs/MEMORY.md`、项目 `~/.pi-rs/memory/<cwd 的 slug>.md`，一行一条 Markdown bullet，启动时有界注入。没有向量库、没有后台抽取；模型只在用户明确要求时用现有四工具写入。

**YOLO 默认。** 没有权限弹窗。如果 agent 能写代码就能跑代码，确认对话框只是安全剧场。真正的安全边界是容器/沙箱，属于你的环境，不属于 harness。

**有意不做的事：** plan mode、sub-agent、MCP、内置 todo、后台 bash。计划写 `PLAN.md`，任务写 `TODO.md`——文件可以被 git、被 grep、跨会话存活；隐藏状态不行。需要子代理就开第二个 `pi-rs`。这些"拒绝"不是缺口，是产品决定：核心越小，你能看懂的部分越多。

**扩展点是 Rust trait。** `llm::Provider` 和 `agent::Tool` 就是全部插件接口。没有动态加载，没有热重载；加一个后端 = 实现一个 trait，加一个工具 = 实现一个 trait。

## 文档

维护者视角的文档在 [`docs/`](docs/README.md)：

- [目标](docs/goals.md) — 要什么、不要什么、成功标准
- [架构](docs/architecture.md) — 分层、数据流、不变量、失败模型
- [计划](docs/plan.md) — 里程碑、候选池、开放问题
- [开发指南](docs/development.md) — 测试、扩展工具/provider、提交检查

## 会话

每次会话一个 JSONL 文件：`~/.pi-rs/sessions/<cwd 的 slug>/<时间戳>-<id>.jsonl`。第一行是 header，之后每行一条记录（消息或 compaction），带 `id` + `parent` —— 文件是棵树而不是日志，目前读取时取活动分支（最后一条回溯到根）。assistant 的 `reasoning` 一并落盘，因为带 `tools` 的 DeepSeek 请求要求把历史 `reasoning_content` 原样带回（否则 API 返回 400），消息格式与线上格式共享同一个 `Message`；图片在发送时由 provider 展开为 content parts，会话里仍存 base64 的同一个 `Message`，所以不需要新的消息类型。

压缩不会删掉旧消息：文件里追加一条 `{"type":"compaction"}` 记录，保存摘要、`first_kept_id` 与压缩前后的 token 估算。恢复时从这条记录重建上下文：一条合成的 summary 用户消息 + 从 `first_kept_id` 开始的消息。旧消息仍在文件里，只是不再发给模型。

如果会话最后一条记录写到一半，`-c` 会先把原文件完整备份为同目录的 `.jsonl.<id>.bak`，再修复损坏尾部并提示备份路径；中间坏行、重复 ID 或损坏父链会明确报错。会话保存失败会中止当前回合：print 报错、JSON 返回 `outcome:"error"`，均非零退出；TUI 显示错误并阻止继续提问、压缩或 `/dream`。修复存储或文件后需要重启，已执行的工具操作不会回滚。

## 长期记忆

跨会话的记忆是两个普通 Markdown 文件，放在数据目录而不是仓库里：

- `~/.pi-rs/MEMORY.md` —— 全局偏好（回答语言、风格、跨项目习惯）；
- `~/.pi-rs/memory/<cwd 的 slug>.md` —— 当前目录的事实与决定（构建命令、项目约定）。

一行一条，例如 `- (2026-10-17) 这个仓库用 cargo test 验证`。可以直接编辑、grep、删除；没有数据库，也没有隐藏状态。启动时两个文件会被追加到系统提示末尾，总量限制在约 1,200 token（中文按字估算），超出时只放较新的条目并明确提示省略数量。

TUI 里用 `/remember <text>` 写项目记忆，`/remember global <text>` 写全局记忆，`/forget <text>` / `/forget global <text>` 按子串删除，`/memory` 查看路径和条数。在普通 prompt 里说“记住……”也可以，模型会用现有的 `read` / `edit` 工具完成。pi-rs **不会**自动从对话或仓库内容里抽取记忆；删除这两个文件就等于关闭该功能。

实验模式 `/lab` 提供一个 `/dream`：用当前模型跑一次隔离的整理回合，读两个记忆文件，合并重复/近重复、保留耐久事实、遗忘过期或琐碎条目，再按同一 bullet 格式写回。它只带 `read` / `write` / `edit`，不进入主会话，只把 token/成本计入统计；状态栏会显示 `LAB`。这是手动实验功能，不是后台睡眠计算。

## 成本

状态栏会显示输入/输出 token、缓存命中率与估算成本；`-p` 结尾显示 token 数与成本。DeepSeek 的定价分高峰/低谷（工作日 01:00–04:00 与 06:00–10:00 UTC），估算按 UTC 时间自动选择，但不考虑中国法定节假日，所以看成量级即可。`--provider openai`、MiniMax CN 订阅（按套餐计费）或未知模型时不估成本，显示 `-`。

## 测试

GitHub Actions 已配置 Linux/macOS 的格式、Clippy、锁定依赖测试与构建、PTY 冒烟，以及 Node 24 的网站构建；均不需要真实 API key。

```bash
cargo test          # 250 个测试，不需要 API key
```

覆盖：SSE 分块解析、工具 schema 与线格式（含 `reasoning_content` 回传与图片 content parts）、agent 循环（工具回环、顺序执行与逐项结果报告、中止、空流、context overflow 重试）、上下文压缩（token 估算、切点、摘要序列化、自动/手动压缩、摘要续写）、四个工具的真实文件/进程行为、会话读写、分支回溯与 compaction 重放、arg 解析、剪贴板图片的格式识别/尺寸解析/base64、提示词历史、长期记忆的解析/预算/写入/删除/计数、`/dream` 的提示词与工具子集、`/model` 的模型路由与切换、TUI 状态机、斜杠命令列表与补全、thinking 折叠与点击、换行与图片附件、Markdown 渲染（表格、代码块行号与语法高亮），以及 23 个端到端测试——起本地 mock SSE / auth 服务器，跑真实 `pi-rs` 二进制验证完整链路（含 `.env` 自动加载、真实环境变量优先、空值视为未设置、`pi-rs login` 保存与复用 auth.json、坏 key 不落盘、logout、MiniMax 族名解析成 API 真实 ID、context overflow 压缩重试与会话落盘）。

另外有三个真 pty 冒烟脚本（同样不需要真实 key）：

```bash
python3 scripts/tui_smoke.py <数据目录> target/debug/pi-rs     # 启动 / Ctrl+C 干净退出 / 恢复终端
python3 scripts/tui_chat_smoke.py <数据目录> target/debug/pi-rs   # 本地 mock API：输入 → 流式渲染 → 退出
python3 scripts/tui_model_smoke.py <数据目录> target/debug/pi-rs  # 模型切换后的真实请求、凭据、登录/退出与取消
```

## 安全

`bash` 工具以你的用户身份、无沙箱执行。和 Pi 一样：想要安全就用容器。

```bash
docker run --rm -it -e DEEPSEEK_API_KEY -v "$PWD:/work" -w /work your-image pi-rs
```
