# 差分测试框架（difftest）

对固定上游快照做行为差分：同一场景脚本喂给上游 TS pi 与 Rust pi-rs，各自对着同一个「脚本化回放服务器」，归一化后比较退出码、最终文本、provider 请求与会话转录。这是 build-plan-v1.4 P0 的差分测试基座。

- 上游基线 SHA 记录在 `upstream-sha.txt`（当前 `f9daca3480732d0042b1df3a6e27221f597a620d`，各包 1.4.0，领先 v1.4.0 tag 72 个提交）。变更基线须重跑全部场景并更新审计文档。

## 布局

```
difftest/
  upstream-sha.txt        固定上游 SHA（每次审计的锚点）
  upstream/pi/            上游副本 + 构建（gitignore，不入库）
  scenarios/*.json        共享 fixture：prompt、每轮的请求断言 + SSE 脚本、期望输出
  server/replay.mjs       零依赖回放服务器：按场景逐轮返回 SSE，记录收到的请求并断言
  lib/normalize.mjs       归一化规则：剥 id/时间戳/duration/系统提示等允许差异
  lib/compare.mjs         结构化 diff，输出路径级差异
  run.mjs                 编排器：--side rust|ts|both，--scenario <name>
  artifacts/              运行产物（gitignore）：requests/、session.jsonl、report.json
```

## 首次准备

```bash
# 1) 上游副本与依赖（保持仓库内 pi/ 只读，不动它）
rsync -a --exclude .git --exclude node_modules pi/ difftest/upstream/pi/
cd difftest/upstream/pi
npm ci --no-audit --no-fund
node packages/ai/scripts/generate-models.ts --data-only   # 模型目录数据可本地生成
npm run build:offline
# 产物：packages/coding-agent/dist/bundle/cli.js（--version 应输出 1.4.0）

# 2) Rust 侧二进制
cd ../../.. && cargo build -p pi-coding-agent   # target/debug/pi-rs
```

## 运行

```bash
cd difftest
node run.mjs                    # 全部场景，双侧运行 + 对比
node run.mjs --side rust        # 只跑一侧冒烟
node run.mjs --scenario 02-tool-call-bash
PI_RS_BIN=/path/to/pi-rs node run.mjs   # 指定 Rust 二进制
```

输出：每场景一行状态 + 路径级差异清单；完整产物在 `artifacts/<scenario>/{ts,rust}/`，汇总在 `artifacts/report.json`。

## 场景格式

`scenarios/<name>.json`：

- `prompt` / `model` / `fixtureFiles`：两侧共用的输入与临时 cwd 文件。
- `turns[]`：每轮 `assertRequest`（回放服务器对请求的断言：model、stream、末条消息角色）与 `sse`（按序回放的 OpenAI chunk，框架自动补 `data:` 帧与 `[DONE]`）。
- `expect`：`exitCode` 与 `finalText`（print 模式 stdout 全文，trim 后比较）。

## 归一化约定（P0 口径）

- 允许差异：会话 id/entry id/时间戳、duration、cost、cwd/git 分支等元数据。
- 系统提示当前**不比较**（实现自由度），但 user/assistant/toolResult 消息序列、内容、stopReason、usage、工具名集合必须一致。
- 会话文件投影为规范视图（role + content + stopReason + usage），非消息 entry 只比 `type` 与顺序——结构差异（如 TS 的 `model_change`/`thinking_level_change`）会被如实报出而不是被归一化吞掉。

## 首轮结果（2026-10-28）

两个场景双侧均可跑通（exit 0、最终文本一致、轮数一致），规范视图下共 31 处差异，已录入 `docs/operations/compatibility-v1.4.md` 的「差分发现」。新增场景即可扩充覆盖面；每轮扩面后更新兼容矩阵对应行。
