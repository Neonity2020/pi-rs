# 开发与质量门禁规范

pi-rs 对代码风格、静态检查和自动化测试执行严格的持续集成（CI）门禁。

## 本地常用验证命令

在提交代码前，必须保证以下三项命令全部零警告通过：

```bash
# 1. 全工作区单元与集成测试（锁定依赖版本）
cargo test --workspace --locked

# 2. 严格 Clippy 静态分析（警告视为错误）
cargo clippy --workspace --all-targets --locked -- -D warnings

# 3. 代码格式化校验（必须符合 rustfmt.toml 规则）
cargo fmt --all -- --check
```

## 格式化规范 (`rustfmt.toml`)

仓库根目录下维护了统一定制的 `rustfmt.toml`：

```toml
hard_tabs = true
tab_spaces = 4
```

- 代码统一使用 Hard Tab 缩进，禁止混用空格。
- 提交前请运行 `cargo fmt --all` 自动格式化。

## GitHub Actions CI 矩阵

`.github/workflows/ci.yml` 包含三个前置拦截 Job：
1. **test**：在 Ubuntu 与 macOS 环境下执行 `cargo test --workspace --locked`。
2. **clippy**：严格审查全工作区目标与测试代码的警告。
3. **fmt**：检查全局 Tab 缩进与换行风格。

## 凭据安全准则

严禁将 `.env`、API Key 或任何私人凭证硬编码至代码仓库、提交说明或单元测试用例中。测试 mock 请求必须使用假密钥。
