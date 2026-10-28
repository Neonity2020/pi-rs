# Pi 1.4 基线记录

更新：2026-10-28（P0 审计完成）。上游本地快照 `f9daca3480732d0042b1df3a6e27221f597a620d`，各包版本 1.4.0。

**tag 核实（本轮完成）**：上游存在 `v1.4.0` tag（`4a080252f06caf63a525d7873c5d94d3c23887e0`），快照 SHA 领先该 tag 72 个提交、落后 0（tag 后下一开发周期的起点）。结论：以快照 SHA 为冻结基线；不宣称与 v1.4.0 发布构建完全兼容。

Rust 基线：`4106a9c`（第五轮冲刺 + 文档，已推送，远程 CI 三 job 全过），工作区干净。前几轮的"未提交改动"事项已闭环：阶段五改动已作为 `d8938e1` 提交并经 CI 验证。

## 门禁记录（2026-10-28 实测，macOS 26.6.2 arm64，rustc/cargo 1.98.0）

- `cargo test --workspace --locked`：288 通过 / 0 失败
- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过
- GitHub Actions（`4106a9c` push）：fmt 8s / clippy 31s / test 59s 三 job 全过
- 上游构建演练：快照副本（`difftest/upstream/pi/`，仓库内 `pi/` 保持只读）`npm ci` → `node packages/ai/scripts/generate-models.ts --data-only` → `npm run build:offline`，产出 coding-agent bundle `--version` = 1.4.0。注意 `build:offline` 依赖模型目录数据文件（gitignore 的生成物），fresh clone 必须先跑 generate-models。

## P0 交付物

1. **兼容矩阵**：[compatibility-v1.4.md](compatibility-v1.4.md)（索引 + 差分发现）+ [compat/](compat/) 四份明细（providers/agent、CLI/RPC/SDK、会话/工具/配置、TUI/MCP/env/外围），逐项链接上游与 Rust 源码及测试。
2. **差分测试框架**：[difftest/](../../difftest/)（构建配方、场景格式、归一化口径见其 README）；首轮两场景双侧跑通，31 处 wire/会话差异已录入矩阵。
3. **风险清单与 P1 候选**：见 compatibility-v1.4.md「高风险项」节。

## 定位更正

差分与盘点证实此前"框架后期/核心完成"的口径偏乐观：agent loop 钩子未接线、并行工具未实现、会话格式与上游不互通、models.json shape 不同、MCP 无版本协商/超时。当前诚实定位：**单 provider 工作流可用（实验级）；与上游 wire 兼容性处于已审计、未对齐状态**。逐项状态以兼容矩阵为准，不再维护相互矛盾的进度表（TASKS.md 保留为历史记录）。
