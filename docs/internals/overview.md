# 架构概览与分层设计

pi-rs 采用 Cargo Workspace 组织，解耦模型接入、代理循环、环境执行和终端展现。

## Crate 依赖拓扑

构建顺序与依赖层级由底层至顶层严格分层：

```
pi-telemetry  (契约与观测 Span)
     │
pi-chord      (RPC 与状态同步占位)
     │
pi-tui        (Ratatui 终端渲染体系)
     │
pi-codemode   (沙箱脚本调用占位)
     │
pi-mcp        (Model Context Protocol 协议与传输)
     │
pi-ai         (统一模型抽象与 Provider 驱动)
     │
pi-durable    (持久化文档与重放占位)
     │
pi-env        (环境抽象：Local 本地执行与进程生命周期)
     │
pi-agent      (核心 Agent Loop、消息编排与工具契约)
     │
pi-coding-agent (CLI 交互主程序：交互模式、Print、会话管理与内置工具)
```

## 核心数据流

```
 用户交互 / 输入
       │
       ▼
 [SessionCommand] ──▶ AgentSession (Tokio 后台任务)
                            │
                            ├─▶ JSONL 存储 (`SessionManager`)
                            │     └─ 实时落盘 (sync_data)
                            │
                            ├─▶ Agent::run_agent_loop
                            │     ├─ 轮询 Follow-up / Steering 队列
                            │     ├─ 调用 pi-ai Stream 适配器
                            │     ├─ 驱动 AgentTool 执行
                            │     └─ 触发 AgentEvent 事件
                            │
                            ▼
                     [SessionEvent]
                            │
                            ▼
                  EventBus 广播总线
                            │
        ┌───────────────────┴───────────────────┐
        ▼                                       ▼
InteractiveMode (ratatui TUI)              PrintMode (stdout)
```

## 线格式兼容性 (Wire Format)

pi-rs 序列化规范与 TypeScript pi 保持 1:1 严格对齐：
- 字段名：全局采用 `camelCase`（如 `toolCallId`, `stopReason`, `maxTokens`）。
- 消息 Tag：采用内部打标枚举（`{"role": "user", "content": [...]}`，`{"role": "assistant", ...}`）。
- 会话文件：JSONL 格式支持直接导入导出并在两套运行时之间共享 transcript。
