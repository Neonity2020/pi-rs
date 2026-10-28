# Pi 1.4 基线记录

审计时间：2026-10-28。上游本地快照 `f9daca3480732d0042b1df3a6e27221f597a620d`，各包版本 1.4.0；本地没有与 HEAD 对应的 tag，因此未证明它与发行 tag 完全一致。Rust 起点 `c26d4b461caaf22b5c0310d10b050d0ba82913cb`，工作区包含阶段五的未提交预置改动。逐项范围和未验证部分见 [兼容矩阵](compatibility-v1.4.md)。

macOS 本地 Rust `1.98.0`、Cargo `1.98.0`，在上述工作区加工具更新事件改动后，`cargo test --workspace --locked`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 和 `git diff --check` 均通过。此结果不证明上游兼容，也不证明其他平台、真实 provider 或远程 CI 通过。

未提交预置包含自动压缩命令和状态的骨架、context usage 占位，以及工具更新事件接线。自动压缩和端到端工具实时输出在审计时尚未完成，不能按已交付能力宣传。实施中保留这些工作并按行为测试逐步完善。
