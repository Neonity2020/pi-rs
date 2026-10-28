# 兼容矩阵：pi-ai / pi-agent（providers、消息模型、目录认证、agent loop、队列）

审计：2026-10-28，上游快照 `f9daca3480732d0042b1df3a6e27221f597a620d`（各包 1.4.0）。状态定义：**已验证** = Rust 侧有实际测试引用该行为；**部分** = 有实现但缺面/有偏差；**未实现**；**接受差异** = 有意偏离（有注释声明）。

## 一、Providers（`pi/packages/ai/src/api/`，41 个文件 = 10 聊天 API + 1 图像 API + 4 分类器 API + 12 共享/辅助 + 15 lazy 包装；另有 `src/providers/` 42 个 provider 工厂 + 模型目录）

### 1.1 聊天/流式 API（KnownApi 共 10 个，`ai/src/types.ts:64-74`）

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距/下一步 |
|---|---|---|---|---|
| `anthropic-messages`（lazy）：SSE 流式、工具、图片、thinking（budget/signature/redacted）、cache_control、refusal/pause_turn/sensitive 停止映射、streamSimple | `ai/src/api/anthropic-messages.ts`（58KB，stream:575 / streamSimple:956，mapStopReason:1664） | `crates/pi-ai/src/providers/anthropic.rs`（1312 行） | **已验证** | 集成测试 13 个（`crates/pi-ai/tests/anthropic_streaming.rs`）+ 单测 7 个（anthropic.rs:1141-1300，含 `request_body_matches_wire_shape`、`thinking_maps_reasoning_level_to_budget`、`stop_reason_mapping_matches_upstream`）。未移植：compat 覆盖、effort 标记、native tool changes/deferred 占位工具、`supportsMidConvoSystemMessages` 分支（恒 collapse，代码内已注明） |
| `openai-completions`（lazy）：增量 SSE、工具按 index 分片、reasoning_content→thinking、images(image_url)、repairJson、usage cache 拆分、streamSimple | `ai/src/api/openai-completions.ts`（63KB）；`ai/src/utils/streaming-json.ts` | `crates/pi-ai/src/providers/openai_completions.rs`（1051 行）+ `json_repair.rs` | **已验证** | 集成测试 6 个 + 单测 9 个。**未移植（真差距）**：请求体忽略 `tools_removed` 与 `sections`（openai_completions.rs:255-268 只回放 `tools_added`）、compat 兼容旗标体系（`providers/compat-schema.ts`）、`samplingParams`/`cacheRetention` 合并 |
| `openai-responses`（lazy）：Responses API SSE | `ai/src/api/openai-responses.ts:154,274` + `openai-responses-shared.ts`（36KB） | `Api::OPENAI_RESPONSES` 常量（model.rs:15），无适配器 | **未实现** | 优先级最高的第三方 API（GitHub Copilot/openai 官方默认） |
| `openai-codex-responses`（lazy）：Responses 变体 + **WebSocket 传输**、ChatGPT OAuth 头 | `ai/src/api/openai-codex-responses.ts:242,510`（WS 解析：879-943） | 无；`Api` 常量也缺失（缺 azure/codex 两个） | **未实现** | 需先补 `Api` 常量；P2 可只做 SSE 分支 |
| `azure-openai-responses`（lazy） | `ai/src/api/azure-openai-responses.ts` + `azure-openai-config.ts` | 无 | **未实现** | 依赖 openai-responses 先行 |
| `google-generative-ai`（lazy）：Gemini、thinkingLevel 解析、thoughtSignature | `ai/src/api/google-generative-ai.ts` + `google-shared.ts` | `Api::GOOGLE_GENERATIVE_AI` 常量存在，无适配器 | **未实现** | — |
| `google-vertex`（lazy）：ADC 认证 | `ai/src/api/google-vertex.ts` | 常量存在，无适配器 | **未实现** | — |
| `mistral-conversations`（lazy）：工具、图片、cacheRetention | `ai/src/api/mistral-conversations.ts` | 常量存在，无适配器 | **未实现** | — |
| `bedrock-converse-stream`（lazy）：AWS SDK、SigV4 | `ai/src/api/bedrock-converse-stream.ts` | 常量存在，无适配器 | **未实现** | 需 `aws-sdk-bedrockruntime` 或手签 SigV4 |
| `pi-messages`（lazy）：POST `{model, context, options}` → AssistantMessageEvent SSE | `ai/src/api/pi-messages.ts:35-56,364,444` | 常量存在，无适配器 | **未实现** | 事件模型与 Rust `AssistantMessageEvent` 同构，移植成本最低 |

### 1.2 图像 / 分类器 API

| 能力项 | 上游证据 | Rust 证据 | 状态 |
|---|---|---|---|
| 图像生成 `ProviderImages.generateImages`（openrouter-images） | `ai/src/types.ts:340-346`；`api/openrouter-images.ts:43`；`image-models.ts`、`images-api-registry.ts` | 无（`ImageModel`/`AssistantImages` 类型不存在） | **未实现** |
| 分类器 `ProviderClassifier.classify`（typesafe-system-one、cloudflare-system-one、llama-cpp-classify、openai-decisions） | `ai/src/types.ts:348-364`；四个 api 文件 + `system-one-shared.ts:189` | 无 | **未实现** |

### 1.3 API 共享/辅助模块

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| `lazyApi()`/`lazyStream()`：同步返回流、后台加载、setup 失败转 error | `ai/src/api/lazy.ts:60,98` | 静态注册表 `stream_fn_for`；不支持 API 返回 Error 流（providers/mod.rs:24-37） | **接受差异** | 无动态加载需求；无 onPayload/onResponse 观测器 |
| `simple-options.ts`：clampMaxTokensToContext、thinkingBudgetForLevel、clampReasoning、DEFAULT_THINKING_BUDGETS | `ai/src/api/simple-options.ts:18-93` | anthropic.rs:60,566-573（budget 映射 + xhigh→high + MIN_ANSWER_TOKENS 1024） | **部分** | 仅 anthropic 路径移植且有单测；`DEFAULT_THINKING_BUDGETS`、`clampMaxTokensToContext` 未移植 |
| `transform-messages.ts`：splitErrorToolResultImages、transformMessages | `ai/src/api/transform-messages.ts:31,91` | 无对应模块；anthropic.rs:362 内联处理工具结果图片 | **部分** | 通用整形未系统移植 |
| `constrained-sampling.ts`：strict json_schema / grammar | `ai/src/api/constrained-sampling.ts:127-301`；`types.ts:747-774` | 无；Rust `Tool` 只有 name/description/parameters（types.rs:232-237） | **未实现** | — |
| openai-prompt-cache / cloudflare.* / github-copilot-headers / azure-openai-config | `ai/src/api/` 各文件 | 无 | **未实现** | 随对应 provider 做 |
| 42 个 provider 工厂 + `.models.ts` 目录（KnownProvider 42 个） | `ai/src/types.ts:90-133`；`providers/all.ts:62-184`；`openai.ts:19-37` | 4 个手写内置模型 + models.json 自定义 provider（model_registry.rs:139-180） | **部分** | 无 provider 工厂概念；自定义 provider 仅 id/name/api/base_url/context_window/max_tokens 子集 |
| faux 测试 provider | `ai/src/providers/faux.ts` | `providers/mock.rs` | **接受差异** | mock 覆盖面小于 faux（不回放 thinking/tool 分片多样性） |

## 二、消息模型与流事件

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| 消息联合：system/user/assistant/toolResult，`role` tag wire | `ai/src/types.ts:561-659` | `crates/pi-ai/src/types.rs:265-462`（`#[serde(tag="role")]`） | **已验证** | wire 测试 + JSONL 往返测试覆盖 |
| 内容块：Text（textSignature）/Thinking（signature、redacted）/Image/ToolCall（thoughtSignature、namespace、argumentsError） | `ai/src/types.ts:422-457` | types.rs:38-100（字段齐备） | **已验证** | `namespace`/`thought_signature` 有字段无测试 |
| `AssistantMessage` 全字段（api/provider/model/responseModel/providerThinkingLevel/diagnostics/deferred…） | `ai/src/types.ts:585-616` | types.rs:311-340 | **部分** | 缺 `provider_thinking_level`、`diagnostics`、`deferred`；`duration_ms` 在但 provider 流从不写入 |
| `AssistantMessageEvent` 12 变体 | `ai/src/types.ts:821-837` | `crates/pi-ai/src/stream.rs:67-120`（齐备） | **部分** | Rust `ThinkingEnd` 缺 `content` 字段；Done/Error reason 未收窄类型 |
| `AssistantMessageEventStream`：背压队列、`durationMs` 单调钟盖章、`result()` | `ai/src/utils/event-stream.ts:43-128` | `Pin<Box<dyn Stream>>` 别名 + mpsc(64) 背压 | **接受差异** | `durationMs` 盖章与 `result()` 无对应物；Rust 助手消息耗时缺失 |
| `StopReason` 7 变体 | `ai/src/types.ts:489` | types.rs:216-227 | **已验证** | `Deferred` 有枚举无产生者 |
| `Usage` + cost 六字段 | `ai/src/types.ts:459-487` | types.rs:156-210 | **已验证** | Rust cost 字段全 Optional（上游必填数字） |
| `ThinkingLevel` off/minimal..max | `ai/src/providers/model-schema.ts:3-4` | types.rs:21-32 | **已验证** | `max` 档无测试（上游也仅部分模型支持） |
| SystemMessage：sections 增删、toolsAdded/toolsRemoved 回放 | `ai/src/types.ts:561-577`；`utils/transcript.ts:58-96` | types.rs:265-292 字段齐备；anthropic 回放（anthropic.rs:302-360） | **部分** | openai 路径**忽略 sections 与 tools_removed**；`declareToolChanges` 公告机制（agent-loop.ts:342-376）完全没有（agent.rs:388 TODO） |
| `normalizeContext`（保留后续 system） | `ai/src/utils/transcript.ts:30-34` | `stream.rs:36-58`（**丢弃**全部后续 system） | **接受差异**（有语义偏差） | Rust 行为等价于 `collapseSystemMessages`；当前无实害，接 pi-messages/mistral 会踩坑 |
| Transcript 工具状态 getToolStateChanges | `ai/src/utils/transcript.ts:144-263` | 无 | **未实现** | — |
| 流式 JSON 工具 repairJson | `ai/src/utils/streaming-json.ts`、`json-parse.ts` | `providers/json_repair.rs`（180 行） | **已验证** | 两 provider 集成测试 |

## 三、模型目录与认证

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| models.json wire 全字段（thinkingLevelMap、promptCache、compat、samplingParams、inputLimits、cost tiers） | `ai/src/providers/model-schema.ts:1-91`；`types.ts:839-915` | `crates/pi-ai/src/model.rs:37-117`（9 字段子集；ModelCost 无 tiers） | **部分** | `InputModality` 多出 Audio/Video（超集无害但非 wire 全等） |
| Models 门面（stream/streamSimple/calculateCost/clampThinkingLevel/refresh） | `ai/src/models.ts:258-1028`；`models-store.ts:22-28` | 无门面；model_registry.rs 只 load/find/default | **部分** | 无动态刷新/多 provider 注册 |
| generate-models 管线 | `ai/scripts/generate-models.ts`；`models.generated.ts` | 无 | **未实现** | 建议消费同一 JSON 而非移植抓取管线 |
| 认证类型（Credential/CredentialStore/AuthContext/ProviderAuth） | `ai/src/auth/types.ts:7-256`；`credential-store.ts:9`；`context.ts:23` | 无；仅 `StreamOptions.api_key`（stream.rs:149），适配器直读 env | **未实现** | P0 至少需 env-key 解析层 + resolveProviderAuth 骨架 |
| OAuth（13 个 provider + pkce/device-code/callback） | `ai/src/auth/oauth/`（14 文件）；`helpers.ts:40` | 无 | **未实现** | — |
| legacy 兼容层 | `ai/src/legacy-api-aliases.ts:29-105` | 无 | **接受差异** | — |

## 四、Agent loop 与事件

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| 入口 `agentLoop`/`runAgentLoop` + continue 变体 | `agent/src/agent-loop.ts:47-160` | `agent_loop.rs::run_agent_loop` + `Agent::prompt/continue_run` | **接受差异** | continue 前置校验等价；无独立低层 continue 公开函数 |
| `AgentLoopConfig`：convertToLlm、transformContext、getApiKey | `agent/src/types.ts:193-254` | `agent_loop.rs:35,40-55`（ConvertToLlmFn 存在但恒 None，agent.rs:443） | **部分** | transformContext/getApiKey 未实现 |
| `AgentEvent` 10 变体 | `agent/src/types.ts:516-539` | `crates/pi-agent/src/types.rs:197-259`（齐备） | **已验证** | 序列断言在 agent_loop.rs / anthropic_streaming.rs / openai_streaming.rs；差异：`assistant_message_event` 为 `Value` 且 serde snake_case（rpc.rs:469-481 运行时补 camelCase） |
| AgentState（pendingToolCalls、streamingMessage 等） | `agent/src/agent.ts:285-421` | `agent.rs:59-176`（AgentSnapshot） | **部分** | 缺 `pendingToolCalls`；多出 Rust 特有 `usage_valid_from` |
| `AgentTool` 契约（execute(signal,onUpdate)、replay、executionMode） | `agent/src/types.ts:466-499` | types.rs:76-121 | **已验证** | 缺 `prepareArguments` |
| `AgentToolResult`（content/details/structuredContent/usage/isError/terminate） | `agent/src/types.ts:424-446` | types.rs:43-73 | **已验证** | — |
| 工具执行：**parallel**（预检+并发、end 按完成序、result 按源序）、length→整批 fail、unknown tool→error | `agent/src/agent-loop.ts:518-700`、`types.ts:47` | `agent_loop.rs:295-413`（**仅顺序**）；unknown tool→error（:371） | **部分** | (1) parallel 未实现；(2) **工具检测更窄**：Rust 判 `stop_reason == ToolUse`（:287），上游按 content 过滤 toolCall 块（agent-loop.ts:268）——stop 结尾但带 tool_calls 的输出会被 Rust 丢掉；(3) length→整批失败未实现 |
| 钩子 `beforeToolCall`/`afterToolCall` | `agent/src/types.ts:326-341`；执行点 agent-loop.ts:718,696 | 类型已定义（types.rs:123-171）但 loop 从不调用 | **未实现** | agent_loop.rs:389/201 TODO；README「before/after hook 点位」只有点位没有钩子 |
| 钩子 `prepareRequest` / `finishTurn` / `prepareNextTurn` | `agent/src/types.ts:186-189,146-158,277-280` | 无 | **未实现** | — |
| 错误/中止路径 + 流无终态兜底 | `agent/src/agent-loop.ts:254-265,921` | `agent_loop.rs:270-292` + `handle_run_failure`（agent.rs:463-506） | **已验证** | panic 释放、abort、SSE 错误帧中段均有测试 |
| 流契约 + `setDefaultStreamFn` | `agent/src/types.ts:33-37`；`stream-fn.ts:10-25` | StreamFn 类型 + `stream_fn_for` 兜底 | **接受差异** | 组合替代全局单例 |
| `proxy.ts` 客户端代理流 | `agent/src/proxy.ts` | 无 | **未实现** | 与 RPC/远程 agent 阶段一起做 |

## 五、队列与钩子语义（steering / follow-up）

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| `PendingMessageQueue` + QueueMode（all/one-at-a-time） | `agent/src/agent.ts:149-180,255-256` | types.rs:261-309 + QueueMode（:26-35） | **部分** | 实现完整但分桶行为**无测试** |
| steering 注入时序 | `agent/src/agent.ts:308-310`；agent-loop.ts:185,213-215,304 | `agent.rs:267-273` + `agent_loop.rs:427-439` | **已验证**（follow-up 有测试；steering drain 无直接测试，严格可标部分） | — |
| `skipInitialSteeringPoll` | `agent/src/agent.ts:406,476-507` | `agent.rs:230-247` + `agent_loop.rs:191,428-433` | **部分** | 机制在、skip 分支无直接测试 |
| follow-up 注入 | `agent/src/agent.ts:312-315`；agent-loop.ts:310-317 | `agent.rs:275-282` + `agent_loop.rs:444-453` | **已验证** | `queued_follow_up_runs_after_first_answer` |
| 队列管理 API（per-queue clear/peek） | `agent/src/agent.ts:317-342` | `agent.rs:284-301` | **部分** | 无 per-queue clear（handoff 已记录） |
| run 生命周期约束（streaming 中 prompt 拒绝、abort） | `agent/src/agent.ts:382-389,349-352` | `agent.rs:197-211,412-424` | **已验证** | — |
| `reset()` | `agent/src/agent.ts:363-377` | `replace_messages`（agent.rs:343） | **接受差异** | 泛化实现，有单测 |
| 工具集变更公告 declareToolChanges | `agent/src/agent-loop.ts:332-376,377-389` | `set_tools`（agent.rs:388-390，TODO） | **未实现** | — |

## 计数汇总

| 状态 | 项数 |
|---|---|
| 已验证 | 13 |
| 部分 | 12 |
| 未实现 | 14 |
| 接受差异 | 7（其中 normalizeContext 带语义偏差注记） |

## 文档与现状不符的偏差（需要修正宣传口径）

1. README「pi-ai 核心完成 / 模型目录」——实际 model.rs 只有 9 字段子集，无目录存储、42 provider 目录、generate-models 管线。
2. README/handoff「pi-agent 核心完成」——4 个钩子只有类型定义、loop 零调用；parallel 工具未实现。
3. handoff「Agent 事件 serde camelCase」不准确——derive 是 snake_case，wire 由 rpc.rs:469-481 运行时逐字段补丁。
4. agent loop 工具检测比上游窄（stop_reason==ToolUse vs content 过滤），文档未提。
5. `normalize_context` 丢弃后续 system 消息（上游保留），当前无实害但属语义偏差。
6. openai-completions 忽略 `tools_removed`/`sections`——运行中移除工具会残留旧声明。
7. `ThinkingEnd` 事件缺 `content` 字段；助手消息 `duration_ms` 无人写入。
8. README 两条 provider 流式声明与测试相符（无虚报）。
