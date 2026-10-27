# 上游 TypeScript 对应关系与移植进度

pi-rs 旨在完整忠实地将上游 [earendil-works/pi](https://github.com/earendil-works/pi) 移植到 Rust 生态。

## 模块映射对照表

| Rust Crate | 上游 TS 对应包 (`packages/`) | 当前状态 | 职责与能力边界 |
|---|---|---|---|
| `pi-telemetry` | `packages/telemetry` | 契约占位 | 分布式跟踪与 Span 契约定义 |
| `pi-chord` | `packages/chord` | 契约占位 | 插件宿主、CRDT 状态同步、JSON 链路 RPC |
| `pi-tui` | `packages/tui` | **核心可用** | ratatui 驱动的 TUI 终端组件体系、Markdown 解析排版、多行编辑器 |
| `pi-codemode` | `packages/codemode` | 契约占位 | 沙箱代码执行引擎，限制仅能调用注入的工具 |
| `pi-mcp` | `packages/mcp` | 骨架阶段 | Model Context Protocol 协议类型、JSON-RPC、客户端与传输接口 |
| `pi-ai` | `packages/ai` | **核心完成** | 统一消息类型、`AssistantMessageEvent` 流、OpenAI / MiMo / Mock 驱动 |
| `pi-durable` | `packages/durable` | 契约占位 | 持久化会话、任务流与重放引擎 |
| `pi-env` | `packages/env` | **本地完成** | 环境抽象 (`EnvConnection`)，本地进程组隔离与管道排空已实装，SSH 远端占位 |
| `pi-agent` | `packages/agent` | **核心完成** | `AgentTool` 抽象、`AgentEvent` 事件、Steering/Follow-up 队列管理、Agent Loop |
| `pi-coding-agent`| `packages/coding-agent` | **框架完成** | 二进制入口 `pi-rs`：会话管理、配置加载、7 大内置工具、交互/Print/RPC 模式 |

## 移植规范与命名习惯

1. **注释对齐**：每个 Rust 模块头注释中均标明其对应的 upstream TS 文件相对路径。
2. **序列化协议**：对外暴露和持久化的 JSON 结构体严格遵循 camelCase，与 TS pi 生成的 session 文件无损互认。
3. **异步模型**：使用 `tokio` 异步运行时，网络请求基于 `reqwest`，事件分发通过 `tokio::sync::broadcast`。
