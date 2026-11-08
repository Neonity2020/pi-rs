# 扩展系统事件顺序与钩子语义审计（第四十三轮切片 C）

> 审计日期：2026-11-06。上游基线：本地快照 `f9daca3480732d0042b1df3a6e27221f597a620d`（`pi/` 浅克隆，与 compatibility-v1.4.md 冻结基线一致）；pi-rs 基线：`9eaf2f7`（main，工作区干净）。**只读审计**：不改任何代码；所有行号引用自该时点源码。
>
> 范围：上游 `pi/packages/coding-agent/src/core/extensions/runner.ts`、`core/agent-session.ts`、`core/extensions/types.ts`、`pi/packages/agent/src/agent-loop.ts`（钩子消费端）对照 Rust 侧 `crates/pi-coding-agent/src/core/extensions.rs`、`crates/pi-coding-agent/src/core/agent_session.rs`、`crates/pi-extension-host/host/host.mjs`、`crates/pi-agent/src/agent_loop.rs`。
>
> 严重度：**P1** = 会错行为（此前未声明）；**P2** = 有声明差异已记录、或字段/能力缺口可观察；**P3** = 纯记录、无行为影响或影响面极小。
>
> 结论先行：分发**顺序**（扩展加载序 → 扩展内注册序、扩展事件先于公开监听者、bridge 先于 mode 事件）与上游一致；偏差集中在三处 **P1**（tool_call 处理器异常 fail-open、terminate 批次 any/every、before_agent_start 每请求触发）与一批 **payload 字段缺失**（P2）。统计：P1 × 3、P2 × 12、P3 × 7（明细见 §9 总表）。

## 1. session_start 的时机与 reason

| 项 | 上游证据 | pi-rs 证据 | 判定 | 影响 | 修复建议 |
|---|---|---|---|---|---|
| 初始发射时机 | 运行时绑定完成即发射：`agent-session.ts:3339`（`bindExtensions` 尾部 `emit(this._sessionStartEvent)`），默认 `{reason:"startup"}`（`agent-session.ts:509`）；发射后接 `reportUnhandledMcpServers` + `extendResourcesFromExtensions`（`agent-session.ts:3340-3341`） | session task 首个循环 pass 前：`agent_session.rs:1004-1013`（`dispatch_event_all("session_start", {type,reason:"startup"})`），随后 `apply_extension_actions`（`agent_session.rs:1014`） | **一致** | — | — |
| reason 取值（替换） | new/resume/fork 由 runtime 重建时指定：`agent-session-runtime.ts:391-398`（resume，含 `previousSessionFile`）及同类 createRuntime 调用；reload 专用 `"reload"`（`agent-session.ts:3755`） | new→`"new"`（`agent_session.rs:1332`）、SwitchSession→`"resume"`（`:1362`）、Fork/Clone→`"fork"`（`:1392`、`:1409`）、RpcReplacement 映射 `:1403-1414` | **一致** | — | — |
| 会话替换：重建 vs 重发 | 替换 = 旧 runtime 先 `session_shutdown{reason,targetSessionFile}`（`agent-session-runtime.ts:167-178`）再**新建 ExtensionRunner**（扩展模块重新执行 factory，`agent-session.ts:3697-3703`），新 runner 上发 `session_start(reason)` | bridge host 存活、不重跑 factory，直接在原 host 上重发 `session_start{reason}`（`agent_session.rs:3194-3211`；hooks 在新 agent 上重装 `:3174-3178`） | **声明差异（P2-D1）** | 扩展的进程内状态（sessionState、注册表）跨会话存活；上游每次替换是全新实例。已在 `extensions.rs:21-23` 模块注释与 compatibility-v1.4.md 第十四轮增量声明 | 维持声明；文档补一句"factory 不重跑" |
| 替换前不 fire session_shutdown | 见上行：上游替换必先 shutdown 旧 runner | `agent_session.rs:3196-3198` 注释显式声明"no `session_shutdown` on the hosts here" | **声明差异（并入 P2-D1）** | 监听 `session_shutdown` 做清理的扩展在替换时收不到（仅 quit 时收到，见 §7） | 维持声明；若后续补发需带 `reason`+`targetSessionFile` |
| `/reload` 的 reason | 上游 reload：旧 runner `session_shutdown{reason:"reload"}`（`agent-session.ts:3723`）→ 重建 → 新 runner `session_start{reason:"reload"}`（`agent-session.ts:3755`） | pi-rs `/reload` 走替换机制（`interactive.rs:2452-2505`），`RpcReplacement::Switch` 映射 reason **`"resume"`**（`agent_session.rs:1405`） | **偏差（P2-D2，未声明）** | 扩展按 reason 分支时把 reload 当成 resume；扩展/skills 重载本身是 no-op（`interactive.rs:2453-2455` 已声明） | 若未来补齐扩展重载，需为 reload 传 `"reload"`；当前在 compatibility 头部补记 |
| `previousSessionFile` 字段 | `SessionStartEvent.previousSessionFile`（`types.ts:755-761`），new/resume/fork 时携带 | payload 仅 `{type,reason}`（`agent_session.rs:1010`、`:3206`） | **字段缺失（P2-F1）** | 扩展无法区分"从哪个会话切来" | payload 补字段（bridge 有源会话文件可用） |
| session_start 后的 resources_discover | `extendResourcesFromExtensions`（`agent-session.ts:3341,3344-3366`）：扩展可贡献 skills/prompts/themes | pi-rs 不派发 `resources_discover`（host 事件面无此条目；`extensions.rs` 模块注释未提） | **缺口（P2-D8 族）** | 扩展动态贡献资源不可用（静态发现已对齐 loader 语义） | 列入事件面缺口清单（§8），低优先 |

## 2. prompt() 的事件顺序与 input 结果语义

上游 `prompt()`（`agent-session.ts:2092-2175`）分段 vs pi-rs `SessionCommand::Prompt` 分支（`agent_session.rs:1061-1132`）：

| 段 | 上游 | pi-rs | 判定 |
|---|---|---|---|
| ① settled 期 defer | `agent-session.ts:2093-2096`（`_deferredSettledActions`） | 会话任务串行处理命令，无重叠窗口；命令路径的 Settled 补发见 `agent_session.rs:1080-1088` | 等价（架构不同） |
| ② 扩展命令检查（最先，流式中也执行） | `expandPromptTemplates && text.startsWith("/")` → `_tryExecuteExtensionCommand`（`agent-session.ts:2101-2108`；实现 `:2180-2204`，解析 = 第一个空格切 name/args，错误走 `emitError(event:"command")` 且仍算 handled） | `text.starts_with('/')` + `bridge.match_command` + `execute_command`（`agent_session.rs:1066-1089`；`match_command` 同样首空格切分 `extensions.rs:561-576`；错误 → notice `:1072`） | **一致**（上游 `expandPromptTemplates` 门控在 pi-rs 的 Prompt 路径恒为展开模式，等价；重名命令上游改 `name:2` 调用名、裸名仍归第一个——pi-rs 注册表 first-wins 等价，`extensions.rs:1363-1371` 无去重但 `match_command` 取首个匹配） |
| ③ 压缩进行中 → throw | `agent-session.ts:2110-2114`（input 事件**之前**拒绝） | 无 throw；input 事件照发后排队（`agent_session.rs:1110-1116`） | **偏差（P3-R1）**：上游此时不触发 input handler，pi-rs 触发且输入进入 pending 队列。影响小（压缩窗口短），记录即可 |
| ④ input 事件（扩展可 transform/swallow） | `_runInputHandlers`（`agent-session.ts:2117-2127`，实现 `:2041-2059`）→ `emitInput`（`runner.ts:1525-1564`）：链式 transform（text/images 逐 handler 传递）、`handled` 短路整个 prompt、最终无变化回 `{action:"continue"}` | `dispatch_event_all("input", {type,text})`（`agent_session.rs:1095-1103`），**结果整体忽略**（注释声明 `:1090-1094`） | **声明差异（P2-D3）+ 字段缺失（P2-F1）**，见下两行 |
| ④a input 结果语义 | handler 可 `transform`（改写 text/images 后继续链）、`handled`（吞掉输入，prompt 直接返回） | 忽略：`handled` 不吞——提示词照常进入队列/起跑；`transform` 不生效 | **影响面**：输入审查/改写类扩展静默失效（输入原样上模型）。已在 compatibility-v1.4.md 头部声明"before_agent_start+input（只通知）"。**建议**：最小落地 handled+transform（结果已在返回值里，Rust 侧只是没读） |
| ④b input payload 字段 | `{type,text,images?,source,streamingBehavior?}`（`types.ts:1171-1181`；source 默认 interactive `agent-session.ts:2120`；streamingBehavior 仅流式时传 `:2121`） | `{type,text}`（`agent_session.rs:1099`、ExtensionPrompt 路径 `:1144`）——**缺 images/source/streamingBehavior**；注意 Prompt 命令本身带 images（`agent_session.rs:1061`），只是没放进 payload | 补字段是低成本修复 |
| ⑤ skill/模板展开 | `_expandSkillCommand` + `expandPromptTemplate`（`agent-session.ts:2130-2134`） | `expand_prompt_text`（`agent_session.rs:1109`，实现 `:720-766`，skill 先于模板，与上游同序） | **一致** |
| ⑥ 排队/起跑 | 流式中：必须 `streamingBehavior`，进 steering/followUp 队列（**当前 run 内消费**，`agent-session.ts:2137-2150`）；空闲：模型/auth 校验（`:2152-2170`）→ `_startRun`（`:2172`） | 流式/压缩/树导航中：进 `pending` 队列，**settle 后作为全新 run 重放**（`agent_session.rs:1110-1116`；消费点 `:1804-1811`、`:1838-1848`，重放 = `agent.prompt(...)` 新 run） | **偏差（P2-D4）**：队列语义不同（上游 steer 注入当前 run 的下一次请求；pi-rs pending 是 settle 后新 run）。这是会话层既有设计差异（非本轮引入），对扩展可见的后果：input 事件的 `streamingBehavior` 语义无法成立。已在队列相关轮次记录；此处补充其对扩展语义的影响 |
| ⑦ ExtensionPrompt（pi.sendUserMessage/sendMessage） | 上游 `prompt(text,{source:"extension",expandPromptTemplates:false})`：跳过扩展命令检查与展开，**input 事件仍发**（source:"extension"） | `SessionCommand::ExtensionPrompt`：input 事件发、无命令检查、无展开（`agent_session.rs:1134-1148`） | **一致**（顺序对齐；payload 同样缺 source 字段） |

## 3. agent 生命周期事件

| 事件 | 上游 payload（types.ts）与时机 | pi-rs payload 与时机 | 判定 |
|---|---|---|---|
| `agent_start` | `{type}`（`types.ts:960-962`）；由 agent 事件驱动（`agent-session.ts:1304-1306`，`_turnIndex=0`），**先扩展后公开监听者**（`:1142-1144`），且"agent_start 在首个请求前被 await"（`:1104-1106` 注释） | `{type}`（`agent_session.rs:2055`）；bridge 派发先于 mode 事件（`:1969-1976`） | **payload 一致**；时序 **P3-R2**：pi-rs 的 agent loop 不等待 bridge 派发完成（session task 异步消费 broadcast，loop 继续跑首个请求）。因结果本就被忽略，实际影响仅为 handler 与请求并发 |
| `agent_end` | `{type, messages}`（`types.ts:965-968`） | `{type, messages}`（`agent_session.rs:2056-2059`，messages 走既有的 wire 形状——difftest 已对齐 role 标签） | **一致** |
| `turn_start` | `{type, turnIndex, timestamp}`（`types.ts:1058-1062`；turnIndex 从 0 计、`agent_end` 后不重置仅 `agent_start` 重置，`agent-session.ts:1305,1320`） | `{type}`（`agent_session.rs:2060`） | **字段缺失（P2-F1）**：缺 `turnIndex`/`timestamp` |
| `turn_end` | **边界事件** `{type, turnIndex, message, toolResults, messageEntryId, toolResultEntryIds, outcome, entries, continue, context}`（`types.ts:1065-1072` + BoundaryState `:1015-1020`）；经 `emitBoundary`（`runner.ts:1036-1085`）逐 handler 累积 entries/continue，每 handler 后重建 context 预览；handler 可追加条目（custom_message/context_edit/compaction 草稿）、可强制续跑；不可续时报告 `_reportInvalidBoundaryContinuation`（`agent-session.ts:879-881,1030-1037`） | `{type, message, toolResults}`（`agent_session.rs:2061-2067`），纯通知，无 entries/continue/context/entryId/outcome | **偏差（P2-D5）**：边界语义整体缺失（7 个字段 + 结果通道）。后果：清理/压缩类扩展无法在 turn 边界追加条目或拦截结算。此前未被单独声明（round-14 记录只说"turn/agent 生命周期"） |
| `agent_before_settle` | 边界事件，结算前最后通道（`agent-session.ts:2017-2039`，`emitBoundary` + `_commitBoundaryDrafts`），`continue` 或排队消息触发续跑 | **不派发**（`extension_agent_event_payload` 无此分支，`agent_session.rs:2053-2070`） | **缺口（并入 P2-D5）** |
| `agent_settled` | `{type, aborted}`（`types.ts:1033-1037`）；`_startRun` 的 finally 中发射（`agent-session.ts:1078-1100`）——**每个 run 恰一次**；bridge 先于 mode 事件（`:1084-1085`）；settled 期新 prompt 进 deferred 队列（`:2093-2096`） | `{type, aborted:abort_requested}`（`agent_session.rs:1040-1051`）；时机 = session task 的**每次 busy→idle 迁移**（`idle && agent_rx.is_empty()`，`:1026-1036`），且等待 agent_rx 排空后再发（注释 `:1030-1035`）；bridge 先于 mode 事件 ✓ | payload **一致**、bridge/mode 顺序**一致**；时机 **P3-R3**：压缩-only 或树导航-only 的 busy→idle 也发 `agent_settled`（上游这两类不产生 run、不发 settled）。次数偏多而非缺失，监听者按"空闲锚点"语义使用时无害 |
| 派发序（扩展 vs 监听者） | 每个事件先 `_emitExtensionEvent` 后 `_emit`（`agent-session.ts:1143-1144`） | bridge `dispatch_event_all` 后 `event_bus.emit`（`agent_session.rs:1969-1976`） | **一致** |

## 4. tool_call 拦截

| 项 | 上游证据 | pi-rs 证据 | 判定 | 影响 | 修复建议 |
|---|---|---|---|---|---|
| 多扩展 handler 调用顺序 | `snapshotEventHandlers` 按扩展加载序、扩展内按注册序快照（`runner.ts:276-278`）；`emitToolCall` 顺序 await（`runner.ts:1249-1267`） | host 按 discovery 加载序排布（`extensions.rs:1238-1283` 入 Vec 序），`dispatch_event_inner` 顺序 await 各 host（`:1018-1056`）；host 内 `runEvent` 按注册序过滤并顺序 await（`host.mjs:268-279`） | **一致** | — | — |
| block 聚合 | 第一个返回 `block:true` 的 handler 赢，**整个 result 对象**原样返回（`runner.ts:1253-1263`）；loop 只看 `beforeResult?.block`，阻断时取 `reason \|\| "Tool execution was blocked"`、`terminate === true` 才置位（`packages/agent/src/agent-loop.ts:754-761`）；非 block 结果对 loop 无意义（最后非 block 结果虽被 runner 返回但 loop 不读） | `aggregate_tool_call_outcomes`：按 outcomes 序首个 `block:true` 赢，reason 取该 outcome 的非空白 reason（trim 后空 → None → pi-agent 默认文案），terminate 取同一 outcome（`extensions.rs:818-838`）；`blocked_result` 同上游默认文案与 terminate 逻辑（`agent_loop.rs:683-694`） | **一致**（trim 是无害细部，P3-R4） | — | — |
| handler 异常 | `emitToolCall` **无 try/catch**（`runner.ts:1249-1267`）→ `_beforeToolCall` 捕获后 rethrow（`agent-session.ts:667-681`）→ `prepareToolCall` 的 catch 把异常变成 immediate error 结果（**工具被阻断**，`agent-loop.ts:762-770`，非 block 文案而是异常消息） | host.mjs `runEvent` 逐 handler catch → `{__error}` 结果（`host.mjs:272-276`）；Rust 侧仅把 `{__error}` 转成 notice（`extensions.rs:704-711,1041-1045`），聚合看到无 block → **工具照常执行** | **偏差（P1-B1，未声明）** | 上游 fail-closed（扩展抛异常 = 阻断该工具调用并回显错误），pi-rs fail-open（异常只发通知、调用继续）。安全类/审批类扩展抛异常时行为相反 | 在 `aggregate_tool_call_outcomes`（或派发层）把 tool_call 的 `{__error}` 视为 block，reason 用上游文案 `Extension failed, blocking execution: <msg>`；host.mjs 的 `{__error}` 里已有消息可用 |
| terminate 批次聚合 | `shouldTerminateToolBatch` = **every**（全部 finalized 结果 `terminate===true` 才终止，`agent-loop.ts:700-702`；sequential/parallel 两路均用它 `:592,:668`）。类型注释同义（`types.ts:1458-1462` "every finalized tool result"） | sequential `terminate \|= entry.result.terminate`（`agent_loop.rs:868`）、parallel `.any(...)`（`:1013-1016`）——**any** | **偏差（P1-B2，未声明）** | 一个被 tool_call 钩子 block 且 terminate:true 的调用就会终止整个批次/续跑，上游需要批次内**所有**结果都带 terminate。pi-rs 的 parallel preflight 还会因 hook terminate 直接不给后续调用起跑（`agent_loop.rs:894-939`；该 preflight 行为本身已声明 `agent_loop.rs:16-18`，但 any/every 未声明） | 两处聚合改 `every`（空批次 false）；`agent_loop.rs` 头部偏差清单补记 |
| input 就地变更 | `event.input` 可变，后续 handler（含其他扩展）可见；钩子收到的是 **validateToolArguments 之后**的参数（`agent-loop.ts:727-753`） | payload 每 host clone（`extensions.rs:1038`）——同 host 内顺序 handler 共享对象（host.mjs 直接传同一 payload），**跨 host 不可见**；参数未做 schema 校验直传 | **部分（P3-R5）** | 跨扩展的参数改写链不成立（pi-rs 本就忽略参数变更——block 之外的变更上游也只在同批 handler 间可见，模型侧不改） | 记录即可 |
| 嵌套调用 parentToolCallId | `<parent id>/<n>` 嵌套调用走同一钩子并带 `parentToolCallId`（`agent-session.ts:730-763`；`types.ts:1193-1203`） | pi-rs 无嵌套工具执行（`ctx.executeTool` 未桥接），payload 无该字段（`extensions.rs:741-746`） | **字段缺失（P2-F1，随能力缺失）** | — | 随 executeTool 能力评估 |
| 超时 | 无界等待（上游进程内 await） | `EXT_TOOL_CALL_TIMEOUT` 120s（`extensions.rs:69`）；host 超时→notice/禁用，其余 host 继续 | **声明差异（P2-D6）** | 已在 `extensions.rs:56-66` 声明 | 维持 |

## 5. tool_result

| 项 | 上游证据 | pi-rs 证据 | 判定 |
|---|---|---|---|
| 是否 await | **await**：`_afterToolCall`（`agent-session.ts:684-724`）在 `finalizeExecutedToolCall` 内于 ToolResultMessage 定稿/落盘**之前**执行（`agent-loop.ts:879-911`）；handler 对 content/details/structuredContent/isError/usage 的修改直接替换持久化结果与模型可见内容（合并规则 `runner.ts:1190-1247`：改 content 不带 structuredContent 则丢弃后者） | **fire-and-forget**：`hooks.fire("tool_result", …)` 不等结果、返回 `AfterToolCallResult::default()`（`extensions.rs:778-782`，注释显式声明"upstream awaits it; declared bridge deviation"） | **声明差异（P2-D7）** |
| 后果 a：结果改写 | 扩展可脱敏/标注结果 | 不可能（结果已定稿） | 已声明；影响面=改写类扩展静默失效 |
| 后果 b：handler 异常 | 异常把整个结果替换为 error 结果（`agent-loop.ts:912-914` catch → `createErrorToolResult`） | `{__error}` → notice，结果不变（`extensions.rs:1041-1045`） | **偏差（P2-D7 子项，未单独声明）**：上游"handler 崩溃 = 结果变错误"，pi-rs"崩溃 = 照常" |
| payload 字段 | `{type, toolCallId, toolName, parentToolCallId?, input, content, details, structuredContent?, isError, usage?}`（`types.ts:1267-1283`；组装 `agent-session.ts:690-701`） | `{type, toolCallId, toolName, input, content, isError, details}`（`extensions.rs:769-777`）——**缺 parentToolCallId/structuredContent/usage** | **字段缺失（P2-F1）** |
| 事件顺序 | 严格串行：tool_result 处理完成后才 append 结果消息 → 后续 turn_end/agent_end | dispatcher 线程串行保证"队列内 tool_result 先于下一个 tool_call"（`extensions.rs:399-452` 单线程通道），但与 session task 的异步派发（turn_end 等）**无跨线程顺序保证**：慢 handler 可能晚于下一轮事件到达 host | P3-R6（fire-and-forget 的自然后果；host 侧观察者若假设严格序会踩到） |

## 6. before_agent_start

| 项 | 上游证据 | pi-rs 证据 | 判定 |
|---|---|---|---|
| 触发点与频度 | **每个 run 一次**：`_startRun` → `_prepareRun`（`agent-session.ts:1873-1899,1926-1977`），在构造起始消息前发射；run 内的续轮（continue/steer）不再触发 | 挂在 agent 的 `prepare_request` 钩子（`extensions.rs:786-811`），该钩子**每次 LLM 请求前**执行（`agent_loop.rs:340-356`，loop 内逐请求调用）——一个 3 次 tool 轮的 run 会触发 **3 次** | **偏差（P1-B3，未声明）**：handler 副作用（通知、日志、外部调用）按请求次数放大。`extensions.rs:805-807` 只声明了"结果被忽略"，未声明频度 |
| payload | `{messageType:"user", prompt, images?, systemPrompt(live getter), systemPromptOptions}`（`runner.ts:1446-1453`；`types.ts:932-957`）；custom 起跑变体 `{messageType:"custom", message}` | `{type, prompt}`，prompt = 转录中最后一条 user 消息文本（`extensions.rs:789-797`）——**缺 messageType/images/systemPrompt/systemPromptOptions**；续轮时 prompt 语义漂移（可能是 steering 消息） | **字段缺失（P2-F1）** |
| 结果形状（上游实际支持） | `BeforeAgentStartEventResult {message?, systemPrompt?}`（`types.ts:1496-1500`）：`message`（customType/content/display/details）逐 handler 累积为起始消息后的 custom 消息（`agent-session.ts:1962-1972`）；`systemPrompt` 置 `forceSystemPrompt`，后续 handler 经 getter 观察到改写（`runner.ts:1456-1461`）；`_preparePromptAndToolLoadout` + 强制 system prompt 机制落地（`agent-session.ts:1973-1975,1773-1777`） | 结果整体忽略（`extensions.rs:805-807` 声明） | **声明差异（P2-D3 同族）**：注入消息/改写 system prompt 不可用。修复依赖请求构造可注入点，成本高；payload/频度先行 |

## 7. session_shutdown 与 teardown 顺序

| 项 | 上游证据 | pi-rs 证据 | 判定 |
|---|---|---|---|
| 发射门控 | `emitSessionShutdownEvent` 仅在有 handler 时发射（`runner.ts:265-274`） | `dispatch_event_all` 跳过未订阅 host（`extensions.rs:597-604`） | 一致 |
| reason 取值与发射点 | quit：runtime `dispose()` → `{reason:"quit"}`（`agent-session-runtime.ts:404-411`）；替换：`teardownCurrent(reason,targetSessionFile)`，**先 abort 结算在途回合**再发（`:167-178`）；reload：`{reason:"reload"}`（`agent-session.ts:3723`） | 仅 quit 路径：main 收尾 `extension_bridge.shutdown()`（`main.rs:1587,1644,1707`）→ `dispatch_event_all("session_shutdown", {"type":"session_shutdown"})` → 逐 host 优雅关闭阶梯（ask→3s→kill，`extensions.rs:715-733`）。**payload 无 reason/targetSessionFile**；替换路径不 fire（声明，§1） | **字段缺失（P2-F1）+ 声明差异（P2-D1）** |
| teardown 顺序 | shutdown handlers → `beforeSessionInvalidate`（UI 分离）→ `session.dispose()`（`agent-session-runtime.ts:171-177`） | session task 先退出（run/compaction 中止、transcript 落盘，`agent_session.rs:1989-2021`），**之后** main 才派发 session_shutdown 并关 host；随后关 MCP | **P3-R7**：顺序镜像合理（扩展先于进程退出收到事件），差异在"扩展收到事件时 session task 已不在"——handler 若回调 pi.sendUserMessage 会落空（router 已随任务结束，`extensions.rs:964-975` answer null）。上游 quit 时 session 仍存活。低危，记录 |
| host 关闭阶梯 | 无对应物（进程内运行，dispose 即完成） | ask → 3s grace → kill（`extensions.rs:715-733` + host 实现注释） | N/A（桥架构固有） |

## 8. 事件面缺口清单（上游有、pi-rs bridge 不派发）

已在 round-14 记录中圈定事件面范围（compatibility-v1.4.md 头部："事件面 session_start/shutdown/turn/agent 生命周期/tool_call 拦截（block/terminate）/tool_result/before_agent_start+input（只通知）"），下表逐项落实并补代码内声明：

| 事件族 | 上游证据 | pi-rs 现状 | 判定 |
|---|---|---|---|
| `message_start`/`message_update`/`message_end` | `agent-session.ts:1321-1352`；**message_end 有结果语义**（handler 可同 role 替换消息，`runner.ts:1151-1188`，就地替换 `agent-session.ts:1286-1351`） | 不派发，代码内声明（`agent_session.rs:2050-2052` "message deltas would be per-chunk noise"） | **P2-D8**（声明在代码、未入 compatibility 头部；message_end 的替换语义随能力缺失） |
| `tool_execution_start/update/end` | `agent-session.ts:1353-1380` | 不派发（同上声明） | **P2-D8** |
| `model_select`/`thinking_level_select` | `agent-session.ts:2527-2533,2692-2697` | SetModel/SetThinking 命令不触及 bridge | **P2-D8** |
| `session_info_changed` | `agent-session.ts:4004-4006`（setSessionName 回发扩展） | pi.setSessionName 动作帧被消费并写日志（`agent_session.rs:2029-2048`），但不向扩展回发该事件 | **P3-R8**（自环通知，丢失影响小） |
| `user_bash` | `types.ts:1153-1161`（`!`/`!!` 前缀） | bash 透传（`run_bash_job`）不派发 | **P2-D8** |
| `ui_prompt_start/end` | `runner.ts:589-622` | 对话框路由（`answer_host_call`）不发 UI 提示事件 | **P3-R9** |
| `resources_discover` | §1 末行 | 不派发 | **P2-D8** |
| `session_before_switch/fork/compact/tree`、`session_compact(_failed)`、`session_tree` | `runner.ts:1087-1125`（cancel 语义）+ `agent-session.ts:2857,2922,3185,3248` 等 | 替换/压缩/树导航流程不咨询扩展 | **P2-D8**（cancel/自定义压缩能力缺失，随替换机制声明覆盖一部分） |
| `context`/`context_with_system`、`before_provider_request/headers`、`after_provider_response`、`provider_stream_event`、`cache_warming_decision` | `runner.ts:1305-1425` 等 | 无请求期变换通道（`prepare_request` 只用于 before_agent_start 通知） | **P2-D8**（与 D3 同根：结果通道缺失） |
| `project_trust` 事件 | `runner.ts:302-329`（首个 yes/no 赢） | 不派发；信任决策由本地门控替代（`extensions.rs:40-41` 声明） | **声明差异（P2-D9）** |
| `mcp_servers_change` | `runner.ts:463-468` | N/A：pi-rs MCP 为原生实现，不经扩展注册 | **排除** |

## 9. 偏差总表与计数

**P1（会错行为，未声明）— 3 项**

| # | 偏差 | 证据锚点 | 修复入口 |
|---|---|---|---|
| B1 | tool_call handler 异常 fail-open（上游 fail-closed：异常→阻断该调用） | 上 `runner.ts:1249-1267` + `agent-session.ts:667-681` + `agent-loop.ts:762-770`；下 `host.mjs:272-276` + `extensions.rs:704-711,818-838` | `aggregate_tool_call_outcomes` 把 `{__error}` 视为 block |
| B2 | terminate 批次 any vs 上游 every | 上 `agent-loop.ts:700-702,592,668`；下 `agent_loop.rs:868,1013-1016` | 两处聚合改 every + 头部偏差清单补记 |
| B3 | before_agent_start 每请求触发（上游每 run 一次） | 上 `agent-session.ts:1935`；下 `extensions.rs:786-811` + `agent_loop.rs:340-356` | 派发点上移到 session task 起 run 前（或 gate 首轮），payload 补 messageType/images |

**P2（声明差异已记录 / 可观察缺口）— 12 项**

| # | 偏差 | 声明位置 |
|---|---|---|
| D1 | 会话替换不 fire session_shutdown、host 存活不重建 factory | `extensions.rs:21-23`、`agent_session.rs:3194-3198`、compatibility-v1.4.md 头部 |
| D2 | ~~`/reload` 重发 session_start reason="resume"（上游 "reload"）~~ **【第四十六轮已修】**：RpcReplacement::Reload 专变体映射 reason="reload"（§11） | 已修复 |
| D3 | input/before_agent_start 结果被忽略（无法 transform/swallow、无法注入消息/改写 prompt） | `agent_session.rs:1090-1094`、`extensions.rs:805-807`、compatibility 头部"只通知" |
| D4 | 流式中排队语义：pending 队列 settle 后新 run（上游 steer/followUp 当前 run 内消费） | 会话层既有差异；对扩展表现为 streamingBehavior 语义缺位 |
| D5 | turn_end/agent_before_settle 边界语义缺失（entries/continue/context/entryId/outcome 7 字段 + 结果通道） | 未单独声明（本轮新增） |
| D6 | 派发预算（事件 30s / tool_call 120s）vs 上游无界 | `extensions.rs:56-76` |
| D7 | tool_result fire-and-forget（上游 await 且异常→结果转 error） | `extensions.rs:778-782`；异常子项已声明（第四十六轮：pi-rs 对 `{__error}` 报 notice、结果照常——compatibility 头部） |
| D8 | 事件面缺口（§8 各行：message_*/tool_execution_*/model_select/thinking_level_select/user_bash/resources_discover/session_before_*/session_compact*/context 族） | `agent_session.rs:2050-2052`（部分）+ compatibility 头部事件面圈定 |
| D9 | project_trust 事件不派发（本地门控替代） | `extensions.rs:40-41` |
| F1 | payload 字段缺失族：input（images/source/streamingBehavior）、tool_call（parentToolCallId）、tool_result（parentToolCallId/structuredContent/usage）、turn_start（turnIndex/timestamp）、turn_end（7 字段，并入 D5）、session_start（previousSessionFile）、session_shutdown（reason/targetSessionFile）、before_agent_start（messageType/images/systemPrompt/systemPromptOptions）。**【第四十六轮已修大半】**input 三字段、tool_result structuredContent/usage、turn_start turnIndex/timestamp、turn_end turnIndex、session_start previousSessionFile、session_shutdown reason 已补（§11）；messageType/images 已随 P1-B3 修；仍缺：parentToolCallId（随 executeTool 能力）、before_agent_start systemPrompt/systemPromptOptions getter、turn_end 边界字段族（D5） | 本轮逐字段盘点（各节表格）+ §11 修复记录 |
| D10 | has_ui 语义：print/json/rpc 恒 false（上游 RPC 为 true、有 RPC 对话框转发） | `extensions.rs:1188-1191` 声明 |
| D11 | pi.sendUserMessage/sendMessage 只携带文本（custom-message 形状与 deliverAs 未建模） | `extensions.rs:32-36` 声明 |

**P3（纯记录）— 7 项**：R1 压缩中上游先拒绝后 input、pi-rs 先 input 后排队（§2③）；R2 agent_start 不被 loop await（§3）；R3 agent_settled 在压缩/树导航后额外发射（§3）；R4 reason trim 细部（§4）；R5 tool_call input 跨 host 不可见变更 + 参数未校验（§4）；R6 tool_result 与后续异步派发无跨线程顺序保证（§5）；R7 quit 时 session task 已退出（§7）；R8 session_info_changed 不回发（§8）；R9 ui_prompt_start/end 不发（§8）。（R8/R9 并入 D8 亦可，此处单列为观察项。）

**一致项（有双侧证据）**：分发顺序（加载序→注册序）、扩展事件先于公开监听者/mode 事件、prompt() 四段顺序（扩展命令→input→展开→排队）、扩展命令解析与 first-wins、agent_start/agent_end/agent_settled payload、tool_call 首个 block 赢 + 默认文案、handler 异常在 fire-and-forget 事件上的"报告后继续"语义、`agent_settled` bridge-先于-mode 顺序。

## 10. 建议修复顺序（下一轮可直接认领）

1. **B1（tool_call 异常 fail-closed）**：改动最小（Rust 聚合一处 + 一条注释），语义收益最大（安全/审批扩展恢复上游行为）。验收：host fixture 抛异常 → 工具结果为 `Extension failed, blocking execution: …` 的 error。
2. **B2（terminate any→every）**：`agent_loop.rs` 两处一行改动 + 头部偏差声明修正；补一个双调用批次测试。
3. **B3 + F1（before_agent_start 频度与 payload）**：把派发从 `prepare_request` 上移到 session task 的起 run 分支（Prompt/ExtensionPrompt/重放三处汇合点），payload 补 `messageType:"user"`、`images`；结果继续忽略（D3 维持）。注意：上移后丢失"续轮通知"行为——与上游一致，正是目的。
4. **F1 低成本字段补齐**：input 补 images/source/streamingBehavior（source: Prompt→"interactive"、ExtensionPrompt→"extension"）、tool_result 补 usage/structuredContent（数据在手）、session_shutdown 补 `reason:"quit"`、turn_start/turn_end 补 turnIndex（session task 可自计数）。全部是 payload 组装改动。
5. **D2（/reload reason）**：替换机制给 Reload kind 传 `"reload"`（一行映射 + 类型放宽）。
6. **D7 子项**：tool_result handler 异常的行为声明写进 compatibility 头部（上游转 error、pi-rs 照常），或低成本改为：`{__error}` 时在 dispatcher 里记 notice 并保持现状（先声明后修）。
7. **D5（turn_end 边界）**：仅做声明补记（capability 缺口大，依赖 custom-entry/context-edit 桥接，单列一轮）。
8. 文档收尾：本轮 P2-D2/D5 与 F1 清单并入 compatibility-v1.4.md 头部声明段。

## 11. 修复记录（第四十六轮，2026-11-08）

按 §10 顺序执行，4/5/6/8 已落地，7（D5 边界语义）按计划仅声明：

- **F1 字段补齐**：`input` 补 `images`（有图才带）/`source`（Prompt→"interactive"、ExtensionPrompt→"extension"）/`streamingBehavior`（run/streaming 活跃时映射 `"followUp"`——pi-rs 的 pending 队列即 settle 后新 run，D4 声明不变）；`tool_result` 补 `structuredContent`/`usage`（工具提供时在场，否则缺省不置 null——`tool_result_payload` 抽为纯函数）；`turn_start` 补 `turnIndex`/`timestamp`（task 内计数，agent_start 重置、agent_end 不重置，对照 agent-session.ts:1305,1320）、`turn_end` 补 `turnIndex`（取本次 turn 的 index）；`session_start` 补 `previousSessionFile`（替换前捕获 outgoing 文件；in-memory 为缺省）；`session_shutdown` 补 `reason`（pi-rs 仅 quit 时发 → `"quit"`，D1 不变）。
- **D2**：`RpcReplacement::Reload{path}` 专变体 → `session_start reason="reload"`；interactive `/reload` 改发该变体。
- **D7 子项声明**：compatibility 头部声明「tool_result handler 异常：上游转 error 结果、pi-rs 报 notice 且结果照常」。
- **D5 声明**：compatibility 头部声明 turn_end/agent_before_settle 边界语义（entries/continue/context/entryId/outcome + 结果通道）整体缺位，依赖 custom-entry/context-edit 桥接，单列一轮。
- 测试：`payload-fields.ts` fixture + 会话级断言（input source/streamingBehavior 缺省、turnstart:0:number、turnend:0、start:new:prev=<路径>、shutdown:quit）+ `tool_result_payload` 单测（可选字段在场/缺省两态、usage camelCase、无 parentToolCallId）。

## 附：方法与限制

- 静态源码对照，未运行差分（约束：不跑 cargo/测试）。行号为审计时点快照，后续提交可能漂移。
- 上游钩子消费端语义（block/reason/terminate/await 位置）取自 `pi/packages/agent/src/agent-loop.ts`（上游 monorepo 的 agent 包），与 `coding-agent` 的 runner/session 分层：runner 聚合 handler 结果，agent-loop 消费聚合结果。
- pi-rs 的 `Message`/`ToolResultMessage` JSON 形状（agent_end.messages、turn_end.message/toolResults）未逐字节复核，依赖既有 difftest 结论（compatibility-v1.4.md 第八轮：turn_end.toolResults role 标签等已按上游形状补齐）。
