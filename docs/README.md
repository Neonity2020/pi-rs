# pi-rs 文档系统

pi-rs 是 [pi](https://pi.dev)（TypeScript）coding agent 的 Rust 移植版，当前处于框架阶段。本文档系统记录使用指南、系统内部架构设计与运维开发规范。

## 目录结构

```
docs/
├── README.md                          # 文档索引与导览（本文件）
├── user/                              # 用户指南
│   ├── quickstart.md                  # 构建、安装与运行模式
│   ├── configuration.md               # 环境变量、模型与设置配置
│   └── tools.md                       # 内置 7 大工具规格与安全边界
├── internals/                         # 架构与内部设计
│   ├── overview.md                    # 整体架构、分层与数据流
│   ├── agent-loop.md                  # 代理循环、消息流与 Steering 机制
│   ├── session-and-persistence.md     # 会话管理、JSONL 持久化与撕裂恢复
│   ├── process-and-env.md             # 进程组生命周期、管道排空与隔离
│   └── tui-runtime.md                 # TUI 组件模型、Markdown 渲染与事件循环
└── operations/                        # 运维与开发
    ├── development.md                 # 本地构建、门禁测试与 CI 规范
    └── upstream-mapping.md            # 上游 TS 包与 Rust Crate 对照表
```

## 阅读路径

1. **快速上手**：阅读 [user/quickstart.md](./user/quickstart.md) 与 [user/configuration.md](./user/configuration.md)。
2. **理解架构**：从 [internals/overview.md](./internals/overview.md) 开始，依次阅读 [internals/agent-loop.md](./internals/agent-loop.md) 和 [internals/session-and-persistence.md](./internals/session-and-persistence.md)。
3. **贡献代码**：参考 [operations/development.md](./operations/development.md) 了解代码格式化、Clippy 严格门禁与测试要求。
