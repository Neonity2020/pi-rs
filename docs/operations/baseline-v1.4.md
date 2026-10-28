# Pi 1.4 基线记录

审计时间：2026-10-28。上游本地快照 `f9daca3480732d0042b1df3a6e27221f597a620d`，各包版本 1.4.0；本地没有与 HEAD 对应的 tag，因此未证明它与发行 tag 完全一致。Rust 起点 `c26d4b461caaf22b5c0310d10b050d0ba82913cb`，工作区包含阶段五的未提交预置改动。逐项范围和未验证部分见 [兼容矩阵](compatibility-v1.4.md)。

macOS 本地 Rust `1.98.0`、Cargo `1.98.0`，在上述工作区加工具更新事件改动后，`cargo test --workspace --locked`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 和 `git diff --check` 均通过。此结果不证明上游兼容，也不证明其他平台、真实 provider 或远程 CI 通过。

未提交预置包含自动压缩命令和状态的骨架、context usage 占位，以及工具更新事件接线。自动压缩和端到端工具实时输出在审计时尚未完成，不能按已交付能力宣传。实施中保留这些工作并按行为测试逐步完善。

## 本轮实现与限制

已增加自动阈值压缩、保留尾部和恢复、压缩提交的并发与失败保护；溢出自动重试和上游完整压缩语义仍待实现。工具更新经 agent 传递至交互界面，bash 提供节流的实时输出和取消时的部分输出。RPC 已补增量事件及部分字段对齐，命令确认语义仍有差异。MCP HTTP 支持 JSON/SSE POST、GET 重连和会话管理，并有本地端到端测试；POST SSE 续传、OAuth、服务端请求及完整 conformance 尚未完成，GET 致命错误目前不能中断正在等待响应的 POST。各能力仍属部分兼容，需继续对照固定上游验证。

2026-10-28 集成门禁：`cargo test --workspace --locked`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo fmt --all -- --check` 和 `git diff --check` 本地通过；未验证真实服务及所有目标平台。
