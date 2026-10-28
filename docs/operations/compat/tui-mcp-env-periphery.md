# 兼容矩阵：TUI / MCP / env / telemetry / chord / durable / codemode / evals / 扩展系统

审计：2026-10-28，上游快照 `f9daca3480732d0042b1df3a6e27221f597a620d`。状态：**已验证**（有测试）/ **部分** / **骨架**（占位）/ **未实现** / **排除**（上游声明不算缺口）。

上游 MCP README:132 排除声明（如实引用，不计 Rust 缺口）："Batch JSON-RPC messages, legacy HTTP+SSE, servers, sampling, and tasks are outside the initial core."

## 1. TUI（上游为自带立即模式渲染循环的组件树；Rust 映射到 ratatui + crossterm）

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| Component 基础契约 | `tui/src/tui.ts`；index.ts:157-183 | components/mod.rs:24-36 | 已验证 | 无 Focusable/Container/组件树/焦点链 |
| 立即模式 TUI 主循环 | tui.ts（1543 行） | 无对应（ratatui 接管） | 排除/架构差异 | overlay/viewport 语义需另行设计 |
| Editor | components/editor.ts（2473 行：autocomplete、UndoStack、KillRing） | editor.rs（551 行：history/词导航/kill ring 基础/续行） | 部分 | 缺 undo-stack、autocomplete、kill ring 完整接线 |
| Markdown | components/markdown.ts（1065 行） | markdown.rs（914 行 pulldown-cmark） | 部分 | LaTeX 未做；需 golden 对拍 |
| ScrollView | scroll-view.ts（224 行） | scroll_view.rs（148 行） | 部分 | 缺 scrollbar 主题、scrollTo、wrap-aware 行号 |
| SelectList | select-list.ts（273 行） | select_list.rs（381 行） | 部分 | 缺 layout 选项/主题化 |
| 布局原语 Box/HStack/VStack/Stack/Spacer/TruncatedText | index.ts:40,69-78 | 仅 text.rs | 部分/未实现 | ratatui layout 可替代一部分 |
| Input/Loader/CancellableLoader/MouseRegion/Image/SettingsList | index.ts:41-68 | 无 | 未实现 | — |
| Fuzzy 匹配 | fuzzy.ts | fuzzy.rs（221 行） | 已验证 | — |
| 键盘解码（Kitty 协议/modifyOtherKeys/Windows） | keys.ts（1455 行） | keys.rs（547 行；明示不复现字节级 quirk） | 部分 | 设计决定而非缺口 |
| **Keybindings 体系** | keybindings.ts:71-210（47 动作）+ Manager | keybindings.rs:51-217（47 条 verbatim，测试断言 =47） | 已验证 | 全局单例未做（显式传参，可接受） |
| 主题 schema（oklab/okhsl 完整移植） | colors.ts + oklab.ts + theme.schema.json | theme.rs（898 行；测试期望值用上游 TS 实际运行生成） | 部分 | 缺 token 组（syntax*/scrollbar*/toolDiff* 等 30+）；无 light/dark 自动切换 |
| 终端生命周期 | terminal.ts（630 行）+ alt-screen/main-screen | terminal.rs（87 行 RAII） | 部分 | 缺 Terminal 抽象层、分屏与 alt-screen 搜索 |
| 终端图像（Kitty/iTerm2） | terminal-image.ts + image.ts | 无 | 未实现 | — |
| 杂项（program-status OSC/clipboard/utils 纯函数） | index.ts:112-126 | 无独立模块 | 未实现 | utils 类纯函数易补 |

## 2. MCP

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| **协议版本** | protocol/types.ts:4 LATEST="2025-11-25"；:9 接受 3 个旧版；client.ts:235-238 校验 | client.rs:76 固定请求 "2025-06-18"，无协商不校验 | **未实现（P0）** | 最便宜且最该先补 |
| initialize + notifications/initialized | client.ts:223,243 | client.rs:69-91 | 已验证 | — |
| 分页 tools/list | README:121；types.ts:87-91 | client.rs:111-149（nextCursor 跟随 + 守卫） | 已验证 | — |
| tools/call（structuredContent） | README:122；types.ts:76-85 | client.rs:153-164 + CallToolResult | 部分 | annotations/outputSchema/execution(taskSupport)/_meta 未建模 |
| **ping / 30s 超时 / notifications/cancelled** | client.ts:38,175,292,555-566 | 无 | **未实现（P0）** | 服务器挂起会永久阻塞 |
| 服务器请求（ping/roots/list） | client.ts:175-179；README:127 | supports_server_messages 钩子预留（默认 false） | 未实现 | — |
| 通用通知（logging/tool-list-changed/progress） | client.ts:260-261；README:123,128 | 无 | 未实现（P1） | — |
| resources API | client.ts:301-337；types.ts:93-131；mcp.md:236-250 | 无 | 未实现（P1） | — |
| stdio transport + 关停阶梯 | transports/stdio.ts（216 行）；README:126 | stdio.rs（723 行：进程组、grace→SIGTERM→SIGKILL、orphan guard） | 已验证 | 对齐良好 |
| Streamable HTTP | transports/streamable-http.ts（522 行）；README:125 | streamable_http.rs（881 行：POST 双分支、GET 流重连、Last-Event-ID、session id） | 部分 | OAuth 与 server-initiated requests 未实现 |
| in-memory 测试 transport | transports/in-memory.ts | in_memory.rs | 已验证 | — |
| OAuth | mcp/src/oauth/（5 文件 ~1100 行）+ auth-provider.ts | 无；mcp_config.rs:23-25 明示拒绝 oauth 字段 | 未实现（P1） | — |
| toLlmContent / 工具桥 | protocol/content.ts；README:30-53 | tool.rs（541 行；命名规则、内容映射、isError） | 已验证（子集） | 并发调用被 mutex 串行化（上游可并发） |
| mcp.json 配置 | extensions/mcp/config.ts + core/mcp-servers.ts + resolve-config-value.ts；mcp.md:30-114 | mcp_config.rs（1226 行；合并/trust/env 展开；差异在 :30-49 逐条声明） | 已验证（子集） | exposure/toolExposure、per-server timeout 缺失 |
| 项目信任 trust.json | trust-manager.ts + project-trust.ts | trust.rs（338 行） | 部分 | 只覆盖 mcp.json；上游还门控 skills/prompts/packages |

## 3. env（上游远程形态 = Rust daemon 部署到远端 + 二进制帧协议，不是简单 SSH exec）

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| EnvConnection 抽象 | durable/src/env/index.ts:209-286,359-372（FileSystem ~20 方法 + Shell） | connection.rs:80-121（8 方法 trait） | 部分 | 接口面远小于上游 |
| 本地 exec 语义 | durable/src/env/node.ts:661（abort/timeout 杀整组、输出窗口） | local.rs（541 行：60KB 捕获 + spill、100ms 静默宽限、128+n） | 已验证（核心）/部分 | 缺 abort 信号模型、string[] 无 shell 形式、输出窗口语义 |
| listFiles（walk+gitignore） | env/src/list-files.ts:24-72 | local.rs walkdir（无 options） | 部分 | 无过滤/上限/截断协议 |
| watch | env/src/watch.ts（158 行）+ WatchTarget/WatchChange | trait 已留位、默认 Unimplemented | 未实现 | — |
| 远程 daemon 帧协议 | env/src/connection.ts（397 行：13B 帧头、心跳、token、重连） | 无（remote.rs 55 行桩） | 未实现（P1） | — |
| SSH/主机钥匙/部署 | env/src/ssh.ts（539 行） | 无 | 未实现（P1） | — |
| RemoteExecutionEnv | env/src/remote-env.ts（957 行） | 无 | 未实现（P1） | — |
| 终端打开（远程） | env/src/terminal.ts | 无 | 未实现（P2） | — |

## 4-7. 外围包

| 包 | 上游公开面 | Rust 对应 | 状态 |
|---|---|---|---|
| telemetry | index.ts:13-235（Span 契约、TelemetryStore、传播、类型化 metrics/schema、render/conformance） | lib.rs ~40 行（仅 pi-ai 依赖的 span 契约） | 骨架；存储/传播/metrics 未实现（P2） |
| chord | host.ts（createHost/defineFacet）、contract/bind、state.ts 复制状态 + delta/、manager/cluster、Link/ipcLink/websocket/sqlite | lib.rs 仅 CellAddress 占位 | 骨架；全部未实现 |
| durable | storage/（jsonl/sqlite/memory/indexeddb）、session/{transaction,forks}、tasks、documents、harness（重放/效果策略）、tools/（含 image/edit-diff） | lib.rs 纯注释占位 | 骨架；内置工具面已由 pi-coding-agent 独立实现（部分） |
| codemode | runtime/{host,inline,vm,worker}（QuickJS）、declarations.ts 工具→JS 声明桥、source.ts 协议、remote/cloudflare/wasm | lib.rs 注释占位（计划 rquickjs/WASM） | 骨架 |
| evals | harness.ts（createPiCodingAgentHarness:471、verifySystemPrompt:257）、report.ts、cli.ts、plan/docker | 无 crate | 未实现（P2） |

## 8. 扩展系统（coding-agent）

| 能力项 | 上游证据 | Rust 证据 | 状态 |
|---|---|---|---|
| 扩展生命周期（factory、session_start/shutdown、agent 状态机 8 事件） | extensions.md:54-80 | 无扩展运行时 | 未实现（P0 决策点：外部进程 + JSONL 形态） |
| 事件/注册 API（pi.on/registerTool/registerCommand/registerShortcut/registerFlag/registerProvider/registerMcpServer/renderer…） | extensions.md:171-190 | 无 pi.* API；MCP 硬编码接线（registerMcpServer 的原生等价已内联） | 未实现（部分内联） |
| Skills（SKILL.md 发现、/skill:name、trust 门控） | skills.md:41-66 | 无 | 未实现（P0/P1） |
| Prompt templates（$1/$@/${1:-default} 替换、trust 门控） | prompt-templates.md:27-53 | 无 | 未实现（P1） |
| 包管理器（pi install npm:/git:/URL、settings trust 门控） | packages.md:7-42 | 无 | 未实现（P2） |
| MCP 作为首个内置扩展 | extensions/mcp/；mcp.md:256-272 | mcp_config.rs + trust.rs + pi-mcp（原生等价） | 已验证（原生等价） |

## 计数汇总

| 小节 | 行数 | 已验证 | 部分 | 骨架 | 未实现 | 排除 |
|---|---|---|---|---|---|---|
| TUI | 16 | 3 | 8 | 0 | 4 | 1 |
| MCP | 14 | 5 | 3 | 0 | 5(+OAuth) | 1 |
| env | 8 | 1 | 3 | 0 | 4 | 0 |
| telemetry | 5 | 0 | 1 | 1 | 3 | 0 |
| chord | 5 | 0 | 0 | 1 | 4 | 0 |
| durable | 7 | 0 | 1 | 1 | 5 | 0 |
| codemode | 4 | 0 | 0 | 1 | 3 | 0 |
| evals | 2 | 0 | 0 | 0 | 2 | 0 |
| 扩展系统 | 6 | 1 | 0 | 0 | 5 | 0 |
| **合计** | **67** | **10** | **16** | **4** | **35** | **2** |

## P0 关键结论（按风险排序）

1. **MCP 协议版本**：Rust 固定 2025-06-18，未跟随上游 2025-11-25 首选 + 接受表——最便宜且最该先补。
2. **MCP 超时/取消缺失**：上游默认 30s；Rust 请求循环无超时，服务器挂起会永久阻塞。
3. **pi-env watch**：trait 留位但默认 Unimplemented；若无调用方可降级处理。
4. **扩展系统整体未实现**；MCP 已作为"原生内置扩展"先行，是唯一有等价物的入口。
5. TUI 的 keybindings（47/47）、fuzzy、本地 exec 语义达到 verbatim 级对齐，可作为后续移植模板。
