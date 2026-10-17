# 开发指南

## 环境

- Rust stable（edition 2021）。
- Python 3（仅 PTY 冒烟脚本需要）。
- 不需要真实 API key 即可跑测试；只有手动运行时需要先 `pi login` 或设置 `DEEPSEEK_API_KEY`。

## 常用命令

```bash
cargo build                     # 构建 workspace
cargo run -p pi-coding -- "解释这个仓库"   # 运行（需 key）
cargo test                      # 单元 + e2e（本地 mock SSE）
cargo clippy --all-targets      # 必须无告警
cargo fmt
```

PTY 冒烟（先 `cargo build`）：

```bash
mkdir -p /tmp/pi-smoke
python3 scripts/tui_smoke.py /tmp/pi-smoke target/debug/pi
python3 scripts/tui_chat_smoke.py /tmp/pi-smoke target/debug/pi
```

运行配置：

```bash
cargo run -p pi-coding -- login  # 隐藏输入，验证后写入 ~/.pi-rs/auth.json
# 或者：
cp .env.example .env            # 填入 DEEPSEEK_API_KEY
# 优先级：--api-key > 环境变量（含 .env）> auth.json；空值视为未设置
```

## 测试策略

| 层 | 怎么测 | 约束 |
|---|---|---|
| 单元 | 各 crate 内的 `#[cfg(test)]` | 纯逻辑，必要时用临时目录 |
| agent | `llm::mock::ScriptedProvider` + 假工具 | 不碰网络，验证事件顺序 |
| e2e | 起本地 TCP mock SSE/HTTP，跑真实 `pi` | 每次用全新临时 cwd + `PI_RS_HOME`，避免开发者 `.env` / auth.json 泄漏 |
| PTY | Python + 伪终端，跑真实二进制 | 验证 raw mode、流式渲染、干净退出 |
| 剪贴板 | 手动在 TUI 里 `Ctrl+V` | `cargo test` 不依赖系统剪贴板；单元测试只覆盖格式/尺寸/base64 |
| 压缩 | `ScriptedProvider` 按“摘要 → 最终回答”排脚本；会话测试覆盖 compaction 重放 | 不需要真实窗口；断言摘要请求、保留尾部与 session 记录 |

写测试的几条规矩：

- 新增行为先写失败测试；修 bug 先复现。
- 测试不依赖 `~/.pi-rs`、真实网络、真实 key 或当前工作目录。
- 涉及环境变量时用新进程/临时目录隔离，测完清理。
- 断言用户可见文本或协议字段，不断言内部实现细节。
- 测试名描述行为（`wheel_scrolls_transcript_and_resumes_follow_at_bottom`），不是方法名。

## 代码约定

- 错误边界：provider 用 `llm::Error`（thiserror）；工具失败是 `ToolResult::is_error`，绝不 `Err`；`coding` 用 anyhow。
- 消息只有 `llm::Message` 一种；不要加 wire/session 转换层。图片是用户消息上的 base64 字段，由 provider 在发送时展开成 content parts。
- 压缩摘要是一条普通 user 消息（`agent::summary_message`），会话里对应 `type:"compaction"` 记录；不要把摘要做成新的 message role。
- 切点只能是 user/assistant，永远不能把 assistant 的 tool_calls 和后面的 tool 结果分开；保留的消息不要改 reasoning。
- DeepSeek 的 `reasoning` 必须原样回传，任何清理都是 bug。
- 每个模块顶部用一句话说明职责；说不清就说明边界错了。
- 新增依赖前先看根 `AGENTS.md` 的依赖清单和 `docs/goals.md` 的预算原则。
- 注释解释“为什么”，不复述代码。

## 加一个工具

先确认是否真的需要：默认工具集固定为四个，新增属于产品变更，要同步 `docs/goals.md`。

1. 在 `crates/coding/src/tools/` 新建模块，实现 `agent::Tool`：

```rust
pub struct MyTool;

impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }
    fn description(&self) -> &str { "一句话说明何时使用" }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "arg": { "type": "string" } },
            "required": ["arg"]
        })
    }
    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        cancel: &'a Cancel,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            // 参数错误、文件不存在、命令失败 → ToolResult::err(...)
            ToolResult::ok("...")
        })
    }
}
```

2. 在 `crates/coding/src/tools/mod.rs` 注册：`mod`、`pub use`、`NAMES`、`all()`、`specified()`。
3. 为真实文件/进程行为写测试；错误路径也要覆盖。
4. 如果要让模型知道新工具，更新基础提示——但先读 `docs/goals.md`：提示词有预算。
5. 跑 `cargo test`、`cargo clippy --all-targets`，必要时更新架构文档的扩展点表。

## 加一个 provider

优先复用 `OpenAiCompat`：大多数端点只是 base URL、key 和少量额外参数不同。

1. 若协议兼容：在 `crates/coding/src/main.rs` 的 `build_provider` 里构造 `OpenAiCompat::custom(...)`，并决定默认模型。
2. 若协议不兼容：在 `crates/llm/` 新模块实现 `Provider::stream`，把厂商事件翻译成 `StreamEvent`。
3. 模型与价格：在对应注册表加 `Model`；未知模型必须继续可用，只是不估价。
4. 测试请求体与 SSE 解析；不要用真实 API 做单元测试。
5. 在 `docs/architecture.md` 的扩展点/失败模型里补一行（如有新失败模式）。

## 改会话格式

- 文件头有 `v`；改格式必须升版本，并保留对旧文件的可读性，或明确拒绝并给出错误。当前 `v: 2` 增加了 compaction 记录；`v: 1` 的消息文件仍按原样读取。
- 新增字段优先 `Option` + `#[serde(default)]`，避免旧会话无法加载。
- compaction 记录保存 `first_kept_id` 而不是复制消息；加载时重放，`first_kept_id` 坏了就退回完整历史。
- 保持每行一个 JSON、追加写；不要引入重写整文件的路径。
- 用 e2e 覆盖“旧文件仍能恢复”和“末行损坏仍能恢复”。

## 改 TUI

- `crates/tui` 不碰 I/O：需要外部信息时，让 `coding` 通过 `UiEvent` 送进来。剪贴板同理——`Ctrl+V` 只返回 `Action::PasteImage`。
- 渲染宽度以 `wrap::display_width` 为准（CJK 宽字符、tab）。
- Markdown 无法识别的语法按字面输出；宁可少样式，不可丢字。
- 改动渲染逻辑后跑单元测试 + 两个 PTY 冒烟脚本。

## 调试

| 想确认 | 手段 |
|---|---|
| 模型实际收到/返回什么 | `pi --json -p "..."` 看事件；会话 JSONL 看历史 |
| agent 循环顺序 | 单元测试里用 `ScriptedProvider`，检查事件序列 |
| 提示词内容 | 读 `crates/coding/src/prompt.rs` 的测试；临时打印 `build()` |
| TUI 是否干净退出 | `scripts/tui_smoke.py` |
| 图片是否随请求发出 | 会话 JSONL 的 `images` 字段；或临时 mock 服务器打印请求体 |
| 是否触发了压缩 | `--json` 事件里的 `compaction`；TUI 的 `context compacted` 提示；会话 JSONL 的 `type:"compaction"` |
| 压缩后保留了什么 | 恢复会话看消息；或读 `agent/compaction.rs` 的切点/序列化测试 |
| 流式解析边界 | `llm/openai.rs` 的 SSE 单元测试；构造分块输入 |

## 提交前检查清单

```bash
cargo fmt
cargo test
cargo clippy --all-targets
cargo build
python3 scripts/tui_smoke.py /tmp/pi-smoke target/debug/pi
python3 scripts/tui_chat_smoke.py /tmp/pi-smoke target/debug/pi
```

- 文档是否仍与行为一致？（尤其 `architecture.md`、`plan.md`）
- 是否新增依赖或配置项？理由写进提交信息。
- 是否违反 `docs/architecture.md` 的不变量？

## 文档维护

- 文档入口：[docs/README.md](README.md)。
- 架构/不变量变更：同一次提交更新 `architecture.md`。
- 计划增减：更新 `plan.md` 的里程碑或候选池。
- 工作流派生变化（命令、测试方式、扩展步骤）：更新本文件。
- 不复制结论；用链接。

## 常见问题

| 现象 | 排查 |
|---|---|
| `missing API key` | 环境变量、`.env`（工作目录或上级）、`--api-key`；空值视为未设置 |
| DeepSeek 返回 400 | 历史 assistant 的 `reasoning_content` 是否原样带回 |
| 恢复会话少消息 | 检查 JSONL 的 `id`/`parent` 链、末行是否损坏、`first_kept_id` 是否存在 |
| 压缩后行为变差 | 看摘要 prompt、tool 结果截断、保留尾部大小；必要时先 `/compact` 前导出会话 |
| TUI 花屏 | 是否走了 `TerminalGuard`；PTY 冒烟是否通过 |
| 成本显示 `-` | 模型不在注册表，或 provider 不提供价格 |
| e2e 读到本机 key | e2e 必须在全新临时 cwd 运行 |
