# 会话管理与持久化加固

会话管理逻辑位于 `crates/pi-coding-agent/src/core/session_manager.rs` 与 `agent_session.rs`。

## JSONL 存储格式

会话在磁盘上表示为一个单调追加的 JSONL 文件（`session.jsonl`），每行对应一个 `SessionEntry`：

```jsonl
{"type":"header","version":1,"id":"0192d6e3-f728-768a-b81b-90f75e9b9211","timestamp":"2026-10-27T06:00:00Z","cwd":"/repo"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"list files"}]}}
{"type":"message","message":{"role":"assistant","content":[{"type":"toolUse","id":"call_1","name":"ls","parameters":{}}]}}
{"type":"message","message":{"role":"toolResult","toolCallId":"call_1","content":[{"type":"text","text":"src\nCargo.toml"}]}}
```

## 持久化保证与边界说明

### 1. 落盘时机与 `sync_data()`
- 每次向会话追加条目（`append`）时，先完成底层文件句柄的 `write_all()`，紧接着调用操作系统的 `sync_data()`（即 Unix 系统的 `fdatasync`）。
- 保证：只要 `append()` 返回成功，已写入的条目即已刷入非易失性介质。即使此时主进程或机器突然断电，已写数据不会丢失。

### 2. 崩溃撕裂行自愈 (Torn Line Auto-healing)
- 若系统在写入半行且尚未刷入 `\n` 时发生硬崩溃，重启打开会话时：
  1. 扫描文件末尾字节；若末尾缺少换行符，自动补写 `\n`。
  2. 读取历史记录时（`count_entries` / `read_session_messages`），自动跳过损坏、不完整或无法解析的行，防止破损内容导致会话解析全面失败。
  3. 后续追加操作将无缝另起新行写入，避免粘连成非法 JSON 串。

### 3. 中断回合自愈 (Interrupted Turn Self-healing)
- 进程在「模型已发出 tool call、工具结果尚未落盘」之间被杀死时，会话文件里会留下一个没有对应结果的 tool call。
- Anthropic 系 provider 会直接拒绝这种历史（`tool call result does not follow tool call`），不带修复的续接必然失败。
- 重建模型上下文时（`project_entries`）校验 tool call 与 tool result 的配对：结果缺失、结果错位、结果没有对应调用，这些不完整的分组一律从上下文里剔除（`drop_incomplete_tool_groups`）。
- 会话文件本身不改写：该条目仍留在日志里（导出 HTML 时渲染为 pending），只是不会进入送给模型的 transcript。
- 配对完整的分组是正常路径，原样保留。

### 4. 非事务性明确声明
- 当前底层直接基于 append-only JSONL 文件，并非多版本并发事务系统（ACID DB）。多进程同时并发写入同一 session 文件不安全。
