# Pi 1.4 兼容矩阵：初始审计

审计日期：2026-10-28。本地上游 `pi/` 的 HEAD 是 `f9daca3480732d0042b1df3a6e27221f597a620d`，各 `packages/*/package.json` 声明版本 `1.4.0`，工作树干净，HEAD 没有本地 tag。此 SHA 是可复现的源码基线；包版本本身不证明它恰好是发布 tag。Rust 基线 HEAD 为 `c26d4b4`，有未提交改动；以下状态是源码盘点，不代表该工作区已通过验证。

状态含义：**部分** = 存在相关实现但尚未证明完整兼容；**占位** = 明示桩或契约；**待审计** = 暂无充分证据。所有条目均须在固定 SHA 上用行为测试与差分结果逐项细化，不能将此表视作完整公开 API 清单或已验收结果。

| 能力 | 上游证据（相对于 `pi/`） | Rust 证据（相对于仓库根） | 初始状态与下一步 |
|---|---|---|---|
| Agent loop、事件、队列与钩子 | `packages/agent/src/agent-loop.ts`、`packages/agent/README.md` | `crates/pi-agent/src/agent_loop.rs`、`crates/pi-agent/tests/` | 部分；核对事件顺序、并发、取消、重试及钩子，工作区改动待审 |
| OpenAI Chat Completions / Anthropic Messages | `packages/ai/src/api/openai-completions.ts`、`anthropic-messages.ts` | `crates/pi-ai/src/providers/`、`crates/pi-ai/tests/` | 部分；已有流与 mock HTTP 测试，需差分所有字段和错误路径 |
| 其他模型 API、目录、认证与 OAuth | `packages/ai/src/api/`、`packages/ai/README.md` | `crates/pi-ai/src/model.rs`、`crates/pi-coding-agent/src/core/model_registry.rs` | 部分；逐个列出上游 API、模型及认证方案并验证，不能将自定义兼容接口等同全部 provider |
| 图片输入、图片生成、分类与跨 provider 迁移 | `packages/ai/README.md` 的 Image Input、Image Generation、Classification、Cross-Provider Handoffs | `crates/pi-ai/src/types.rs` | 待审计；按具体模型及传输建立测试 |
| CLI、print 与 SDK | `packages/coding-agent/docs/cli.md`、`sdk.md`；`packages/coding-agent/package.json` | `crates/pi-coding-agent/src/main.rs`、`src/modes/` | 部分；核对参数、退出码、SDK 导出和模式语义 |
| 会话与持久化、树和压缩 | `packages/coding-agent/docs/session-format.md`、`sessions.md`、`compaction.md` | `crates/pi-coding-agent/src/core/session_manager.rs`、`agent_session.rs`、`compaction.rs` | 部分；验证双向文件互读、分支、自动压缩、崩溃/故障恢复；未提交改动待审 |
| 内置工具及本地环境 | `packages/coding-agent/src/core/tools/`、`packages/env/` | `crates/pi-coding-agent/src/core/tools/`、`crates/pi-env/src/local.rs` | 部分；比较参数、输出截断、错误、取消、文件与进程行为 |
| 远程环境 | `packages/env/` | `crates/pi-env/src/remote.rs` | 部分/待审计；确认具体 SSH 支持及故障语义 |
| RPC 命令、事件及扩展 UI | `packages/coding-agent/docs/rpc-commands.md`、`rpc-extension-ui.md`、`src/modes/rpc/rpc-mode.ts` | `crates/pi-coding-agent/src/modes/rpc.rs` | 部分；逐条对照命令、消息帧、时序及错误；工作区改动待审 |
| MCP | `packages/mcp/README.md`、`packages/coding-agent/docs/mcp.md` | `crates/pi-mcp/src/`、`crates/pi-coding-agent/src/core/mcp_config.rs` | 部分；上游首选协议 2025-11-25 并接受三个旧版，逐项测试协商、分页、通知、取消、HTTP 重连、OAuth 和信任配置 |
| TUI、终端与配置 | `packages/tui/`、`packages/coding-agent/docs/tui.md`、`themes.md`、`keybindings.md` | `crates/pi-tui/`、`crates/pi-coding-agent/src/modes/interactive.rs`、`src/core/ui_config.rs` | 部分；PTY 覆盖渲染、输入、终端恢复、平台差异 |
| 扩展、skills、模板和包 | `packages/coding-agent/docs/extensions.md`、`skills.md`、`prompt-templates.md`、`packages.md` | `crates/pi-coding-agent/src/core/` | 待审计；清点扩展 API 及真实扩展兼容，明确宿主方案 |
| Codemode | `packages/codemode/`、`packages/coding-agent/docs/codemode.md` | `crates/pi-codemode/src/lib.rs` | 占位；实现运行时、工具桥和隔离并测试 |
| Durable | `packages/durable/` | `crates/pi-durable/src/lib.rs` | 占位；盘点存储、重放、任务和文档契约 |
| Chord | `packages/chord/` | `crates/pi-chord/src/lib.rs` | 占位；盘点 host、状态同步及 RPC 契约 |
| Telemetry | `packages/telemetry/` | `crates/pi-telemetry/src/lib.rs` | 占位；核对事件、采样和导出契约 |
| Evals | `packages/evals/` | 无对应 crate | 待审计；明确评测工具是否纳入发布物及如何对等验证 |

上游 MCP README 明确将批量 JSON-RPC、旧式 HTTP+SSE、server、sampling 和 tasks 排除在其初始核心之外，不能将这些项目直接计作上游客户端缺口。源码索引排除了部分目录，包括上游 env 与 coding-agent tools；后续盘点须直接核查这些目录，不能将索引缺项当作功能缺失。
