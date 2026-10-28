# 兼容矩阵：会话格式 / 压缩 / 内置工具 / slash 命令 / 配置体系

审计：2026-10-28，上游快照 `f9daca3480732d0042b1df3a6e27221f597a620d`。状态：**已验证**（有测试）/ **部分** / **未实现** / **接受差异**（有意偏离，有注释）。

## 1. 会话格式与持久化

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| 存储布局 | `docs/session-format.md:11`：`sessions/--<path>--/<timestamp>_<id>.jsonl`（按 cwd 分组、单文件）；config.ts:601-655 | config.rs:17-32 + session_manager.rs:120-145：**每 session 一个目录** `<YYYYMMDD-HHMMSS>-<pid>/session.jsonl` | 接受差异 | 上游 `--session-dir`/settings/env 覆盖未移植 |
| header entry | `session-manager.ts:41-50`：`{type:"session",version:3,id,timestamp,cwd,parentSession?}` | session_manager.rs:30-35：`{type:"header",version:1,app,created,cwd}` | 接受差异(部分) | type 名/version/字段全不同；无 session id（用目录名）、无 parentSession |
| 版本迁移 v1→v3 | docs/session-format.md:24-30；session-manager.ts:287-347 | 无迁移逻辑 | 未实现 | **Rust 无法读上游 session 文件，反之亦然**（差分已实证） |
| 树结构（parentId 分支） | session-manager.ts:57-62,183-194；docs:209-235（buildContextEntries 走 leaf→root） | 扁平 log，无 parentId（文件头注释自认缩减版） | 未实现 | /tree /fork /clone、branch_summary 全依赖它；**最大结构性缺口** |
| 恢复 --continue/--resume/--session/--fork | docs/sessions.md:9-16,50-54 | SessionSelector::Recent（**无 cwd 过滤**）+ ByIdOrPath；28 个单测 | 部分已验证 | 无交互 picker、无 cwd 匹配、无 --fork |
| 持久化 durability | 上游 appendFileSync 无 fsync | 每 entry fsync、撕裂行修复、partial-write 回滚 | 已验证（有意偏离，强于上游） | — |
| entry：`usage` / `thinking_level_change` / `branch_summary` / `custom_message` / `label` / `session_info` | docs/session-format.md 各节 | 无 | 未实现 | thinking level 不持久化，重启丢失 |
| entry：`custom` | session-manager.ts:128-145 | Custom{kind,data}（:65-68），**字段名 kind ≠ 上游 customType** | 部分 | 命名不兼容 |
| entry：`context_edit` | docs:141-147 | session_manager.rs:59-64 形状兼容 | 已验证 | 仅溢出恢复一条路径使用 |
| system 消息 checkpoint（sections/tools patch 重放） | docs:80-87 | 无 | 未实现 | |

## 2. 压缩

| 能力项 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| 手动 /compact [instructions] | docs/compaction.md:41；compaction.ts:952-1057 | agent_session.rs:262-295（**无参数**；运行中拒绝而非 abort） | 部分 | 缺 custom instructions 透传 |
| 自动阈值公式 + 默认值（enabled/reserve=16384/keep=20000） | compaction.ts:267-270,126-130；settings-defaults.ts:9-13 | agent_session.rs:198-201,345-353 + settings_manager.rs:26-37（注释逐字对照） | 已验证 | 完全一致 |
| 触发时机 mid-run（prepareNextTurn） | docs:37 | agent_session.rs:15-18 明确未移植 | 未实现 | 长工具循环中间不检查；需 pi-agent 钩子 |
| 触发时机 prompt 前 / run 后 | docs:37 | agent_session.rs:190-212,311-358（有测试） | 已验证 | — |
| findCutPoint（永不切 toolResult、预算回退） | compaction.ts:446-501 | compaction.rs:101-138 + auto_compaction_cut 钳制到 user 边界 | 已验证（缩减） | 自动压缩比上游保守（永不切 assistant，split-turn 缺失的补偿） |
| split-turn（超长单 turn 切分 + 双摘要合并） | docs:100-127；compaction.ts:779-857,929-942,980-1019 | compaction.rs:16-18 明确未移植 | 未实现 | 超长单 turn 压不动 |
| 溢出恢复（overflow→省略失败轮→压缩→重试一次） | docs:37-39,85-98；compaction.ts:821-843 | agent_session.rs:311-358 + 27 条 OVERFLOW_PATTERNS（:532-560，对应 utils/overflow.ts） | 部分已验证 | 缺 turn_end/agent_end 顺序保证；恢复后 cut 前移规则（suffix advancement）未移植 |
| 重复压缩（previousSummary 迭代合并） | compaction.ts:540-579,714-728 | compaction.rs:191-225 + UPDATE prompt；有测试 | 已验证 | 语义等价 |
| 摘要格式与序列化（SUMMARIZATION_PROMPT、tool result 截 2000 字符） | compaction.ts:507-538；utils.ts | compaction.rs:42-60,294-349；8 个测试 | 已验证（缩减） | 角色 union 缺 bashExecution/custom/compactionSummary |
| branch-summarization | docs:169-232；branch-summarization.ts | 未移植 | 未实现 | 依赖会话树 |
| 文件跟踪（readFiles/modifiedFiles） | docs:200-204；compaction/utils.ts | 未移植 | 未实现 | — |
| per-model compaction overrides | settings-schema.ts:13-27 | deferred | 未实现 | — |
| 压缩请求 cacheRetention=none + retry | compaction.ts:618-638 | compaction.rs:248-272 无 retry 接线 | 部分 | 瞬时流错误直接导致压缩失败 |
| CompactionEntry 可选字段（details/usage/fromHook/systemMessage） | docs:134-137 | session_manager.rs:44-51 注释声明缺失 | 部分 | — |

## 3. 内置工具（上游 **8 个**含 powershell；Rust 7 个）

| 工具 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| read | tools/read.ts:14-18,96,133-152,179-183（2000 行/50KB、图片支持、无行号） | read.rs:38-48,85（2000 行+单行 2000 字符；**cat -n 行号前缀**；无图片；无 50KB） | 部分（未验证） | 行号前缀是额外偏差；0 测试 |
| bash — schema | tools/bash.ts:40-43（`timeout` **秒**、无默认） | bash.rs:115-124（`timeout_ms` 毫秒、默认 120000） | 部分（有意偏差） | **参数名+单位+默认值三者都不同** |
| bash — 截断 | bash.ts:255（尾部 2000 行/50KB；OutputAccumulator；structuredContent 1MB） | bash.rs:20-21（**60KB 单一字节上限**尾部保留 + spill 落盘） | 部分（已验证） | 截断提示文案不同 |
| bash — stdin transport | utils/shell.ts:6-28（**legacy WSL 走 `bash -s` + stdin**）；bash.ts:111-121 | 无 transport 概念 | 未实现 | 另缺 `shellCommandPrefix`、spawnHook |
| bash — PI_* env | bash.ts:193-211（PI_SESSION_ID 等） | bash.rs:40-85（**PI_RS_**_ 前缀，有测试） | 接受差异 | — |
| bash — 信号/退出码 | bash.ts:155-158（128+n） | bash.rs:243-251（同约定，有测试） | 已验证 | — |
| powershell | tools/powershell.ts:16-67 | 无 | 未实现 | Windows 路径 |
| edit — schema | tools/edit.ts:21-41（**`edits:[{oldText,newText}]` 数组**、prepareEditArguments 兼容、BOM/行尾处理、mutation queue、diff 输出） | edit.rs:37-48（**old_string/new_string/replace_all 单编辑**；无 BOM/行尾/queue/diff） | 部分（**schema 不兼容**） | 移植对话回放/上游提示词时 edit 调用会不匹配；0 测试 |
| write | tools/write.ts:11-14 | write.rs:34-43（行为一致；无 queue/abort 检查） | 部分已验证 | 0 测试 |
| grep | tools/grep.ts:21-33（pattern,path?,glob?,ignoreCase?,literal?,context?,limit?默认100；**rg 子进程** gitignore-aware；行截 500 字符/50KB） | grep.rs:17,45-54（仅 pattern,path?；walkdir+regex；200 上限；**不读 .gitignore**） | 部分（未验证） | 参数面缺口大；0 测试 |
| find | tools/find.ts:26-32（pattern,path?,limit?默认1000；**fd 子进程**） | find.rs:16,61-70（自实现 glob→regex；500 上限） | 部分（未验证） | 同上 |
| ls | tools/ls.ts:11-14（path?,limit?默认500；大小写不敏感排序；50KB 截断） | ls.rs:34-42（仅 path?；字典序；无上限） | 部分（未验证） | 大目录会爆上下文 |
| 工具级权限/hook | tool-definition-wrapper.ts；extensions/types.ts | 无 | 未实现 | — |

## 4. Slash 命令（上游 23 / Rust 9）

| 组 | 命令 | 状态 |
|---|---|---|
| 已验证 | /model /thinking /compact | 3 个（/thinking 为 cycle，上游可带参） |
| Rust 增设 | /help /exit /clear /tools /prompt /sessions | 接受差异（调试命令；/sessions 是 stub） |
| 未实现（会话管理） | /settings /tree /scoped-models /new /resume /name /session /fork /clone /import | 依赖会话树/管理 UI |
| 未实现（导出/分享） | /copy /export /share /bug | — |
| 未实现（资源/信任） | /login /logout /llama /trust /mcp /reload /hotkeys /changelog | trust 有底层 API 无命令入口 |
| SlashCommandSource | extension/prompt/skill 注册通路 | 部分（枚举已定义，无注册通路） |

## 5. 配置体系

| 配置 | 上游证据 | Rust 证据 | 状态 | 差距 |
|---|---|---|---|---|
| 配置根 | `~/.pi/agent/`（PI_CODING_AGENT_DIR）+ 项目 `.pi/` | `~/.pi-rs/agent/`（PI_RS_AGENT_DIR）+ 项目 `.pi/` | 接受差异 | 有意隔离 |
| settings.json 键面 | settings-schema.ts:206-454（**约 40 键**） | settings_manager.rs:14-24（**7 键**；项目>全局合并有测试） | 部分 | 缺 steeringMode/followUpMode/retry/sessionDir/defaultTools 等 |
| settings compaction 子树 | settings-schema.ts:18-27 | settings_manager.rs:31-53（前三者） | 部分已验证 | 缺 modelOverrides |
| **models.json 格式** | docs/models-configuration.md:11-24：`providers` 为**对象**（键=provider id）；字段 apiKey(`$ENV`/`!cmd`)/oauth/headers/compat/models/modelOverrides；model 条目含 reasoning/thinkingLevelMap/input/inputLimits/cost/promptCache/contextWindow(默认128000)/**maxTokens(默认16384)** | model_registry.rs:19-45：`providers` 为**数组**；仅 5 字段；max_tokens 默认 **8192**；0 测试 | 部分（**shape 不兼容**） | 另有默认模型硬编码 mimo/mimo-v2.6-pro（:78-80）非上游行为 |
| keybindings.json | docs/keybindings.md；keybindings-schema.ts:7-21 | ui_config.rs（44 个 app 级定义；坏文件 notice；16 测试） | 部分已验证 | 键位 id 集合需逐条对照 |
| themes | docs/themes.md；theme.schema.json；上游有 system 主题 | ui_config.rs:11-21,231-337（无 system 主题，注释声明偏离；14 测试） | 部分已验证 | 无 schema 等价物 |
| mcp.json 布局/合并/信任 | docs/mcp.md:32；mcp-servers.ts:248-305 | mcp_config.rs（同布局、trust 门控、合并规则移植；23 测试） | 已验证 | — |
| mcp.json 字段面 | exposure(codemode/deferred/direct/hidden)+toolExposure+description+timeout(默认60s)；http oauth/auth.provider | 容忍但不生效 exposure/toolExposure；无 timeout/oauth/auth.provider | 部分（已验证连通面） | — |
| 信任机制 | project-trust.ts + trust-manager.ts（门控 skills/prompts/packages/mcp）+ settings defaultProjectTrust | trust.rs（只覆盖 mcp.json 一个消费方；8 测试） | 已验证（缩减） | 无 defaultProjectTrust 联动、无 /trust 命令 |
| auth.json | auth-storage.ts:64-65（文件锁 + OAuth 凭据） | 路径已定义，无实现 | 未实现 | — |

## 关键偏差（P0 风险排序）

1. **会话格式整体不兼容**（差分已实证）：无树、header 形状不同、缺 6 种 entry、custom.kind 字段名不兼容。双向都读不了对方的文件。
2. **split-turn 压缩缺失** + 自动压缩钳制到 user 边界（保守补偿）。
3. **mid-run 压缩检查缺失**（依赖 pi-agent prepareNextTurn 钩子）。
4. **edit 工具 schema 不兼容**（edits 数组 vs old_string/new_string）；bash timeout 参数名/单位/默认值三不同。
5. **grep/find/ls 自实现**而非 rg/fd 托管：不读 .gitignore、参数面缩水、上限不同。
6. **models.json shape 不兼容**（数组 vs 对象）+ maxTokens 默认 8192 vs 16384。
7. settings 键面 7/~40；slash 9/23；powershell、auth.json、/tree /fork /clone 全缺。
8. 有意接受且已声明：配置根隔离、PI_RS_* 前缀、session 目录化、fsync（强于上游）、read 行号前缀、bash 60KB 截断。

## 计数汇总

| 小节 | 能力点 | 已验证 | 部分 | 未实现 | 接受差异 |
|---|---|---|---|---|---|
| 会话格式 | 13 | 2 | 3 | 7 | 3 |
| 压缩 | 14 | 5 | 5 | 4 | 0 |
| 内置工具 | 12 | 2 | 9 | 1 | 2 |
| Slash 命令 | 24 | 3 | 1 | 19 | 1 |
| 配置体系 | 10 | 3 | 5 | 2 | 1 |
| **合计** | **73** | **15** | **23** | **33** | **7** |
