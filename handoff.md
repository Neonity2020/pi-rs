# 项目质量改进交接

更新日期：2026-10-27（用户时区 Etc/GMT+7，第二轮）

## 目标与接手范围

pi-rs 是 pi coding agent 的 Rust 移植，README 定位为框架阶段。第一轮已完成连续 prompt 排队、快照补齐持久化、进程组清理、输出限幅与 CI 工作流（见 git log 与下方记录）。本轮接手后完成：严格 Clippy 门禁清零、rustfmt 基线、会话持久化加固、bash 管道滞留修复。验收围绕可靠的 agent、工具和会话关键路径，并准确说明尚未实现的能力。

工作目录为本仓库，分支 `main`（HEAD 仍为 `1d4e0f1`，全部改动未提交）。保留现有工作区内容，不要重置。仓库内 `pi/`、`pi-tutorial/`、`t3code/` 为独立或参考目录，不应修改。

## 本轮完成

- **质量门禁清零**：`cargo clippy --workspace --all-targets -- -D warnings` 通过。修复项：`pi-mcp` `TransportEvent::Message` 改 `Box<JsonRpcResponse>`；`pi-coding-agent` 的 `SessionEvent::Agent(Box<AgentEvent>)`、`SessionEntry::Message` Box 化、`param_str` 错误 Box 化（调用点 `return *e`）、若干 collapsible_if（用 let-chains）/`manual_clamp`/`items_after_test_module`；`pi-tui` markdown/fuzzy/preview 的机械性修复。
- **rustfmt 基线**：新增 `rustfmt.toml`（`hard_tabs = true`，匹配既有 tab 风格）；`cargo fmt --all` 全量格式化（51 文件，主要是空格→tab 归一，含新测试文件 `agent_loop.rs`）；`cargo fmt --all -- --check` 通过。
- **CI 门禁**：`.github/workflows/ci.yml` 三个 job：test / fmt --check / clippy `-D warnings`（均 `--locked`）。
- **会话持久化加固**（`session_manager.rs`）：
  - `append` 在 `write_all` 后 `sync_data()`：每条目返回即已落盘（进程/机器崩溃不丢已写条目；JSONL 仍非事务，模块文档已如实说明，勿宣称崩溃安全）。
  - 打开会话时若文件末尾无换行（崩溃撕裂的半行）自动补 `\n`，保证后续追加不粘连成无效行。
  - `count_entries` 跳过无法解析的行；新增 `#[cfg(test)] with_file` 注入失败 writer。
  - 新测试：打开缺失目录报错、重开续写保序、撕裂行不计数且追加自愈、只读句柄写失败不推进游标、in-memory 不写盘。
- **会话任务测试**（`agent_session.rs`）：3 个连续 prompt 落盘顺序断言；写失败发 Notice 且 run 不中断。
- **bash 管道滞留修复**（`pi-env/src/local.rs`）：移植上游 `waitForChildProcess` 语义——shell 退出后，若后代继承并持有 stdout/stderr，等待 EOF 改为「退出后 100ms 静默宽限，每个数据块重新计时」，静默即用已收输出收尾。`Capture` 重构为共享缓冲 + 后台任务。修复前 `sh -c 'sleep 5 & echo ok'` 会挂住 exec。
  - 清理保证范围已注释在 `ProcessGroup`：Unix 进程组 SIGKILL 覆盖同组后代；主动 setsid 脱离组的后代不覆盖；非 Unix 仅 kill 直接子进程。
  - 新测试：持管道及时返回、退出后输出继续排空、快速路径无宽限延迟。

## 验证记录（本轮实测）

| 检查 | 结果 |
| --- | --- |
| `cargo test --workspace --locked` | 通过，46 项（基线 36 + 新增 10），macOS arm64 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| GitHub Actions | test job 上一轮已写入；本轮加了 fmt/clippy job，均未在远程验证 |

## 未完成与建议分工

1. **提交决策**：全部改动未提交。建议按「门禁修复 / 格式化 / 持久化 / 进程管道」分批提交，遵循用户指令执行。
2. **CI 远程验证**：push 后确认三个 job 在 ubuntu-latest 通过（注意 `exec_returns_while_descendant_holds_pipes` 等测试依赖 `/bin/sh` 与毫秒级 sleep，Linux 无虞；macOS 本地已过）。
3. **产品能力**（README 路线图）：真实流式响应、会话恢复接入 CLI、MCP 完成、RPC 命令实现（`--rpc` 目前全部返回未实现）。先以 mock/local HTTP 测 provider 工具调用与错误路径。
4. **持久化下一步**：事务性同步持久化（崩溃安全）仍是有意不做；若产品需要，设计 WAL 或 double-buffer 方案。会话恢复 `read_session_messages` 已能跳过撕裂行。
5. **进程生命周期边界**：setsid 脱离组的后代不会被清理（已文档化）；若产品需要，可考虑退出后扫描 reap。`ExecResult.timed_out` 恒为 false（超时走 `Err(Error::Timeout)`），bash 工具里的 `timed_out` 分支是防御性死代码，可在超时改为携带部分输出时一并清理。

## 恢复命令

```bash
git status --short
git diff --stat
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

不要读取或复制 `.env` 中的凭据到交接材料、日志或测试。后续提交、推送和部署需遵循用户指令；本次没有执行这些操作。
