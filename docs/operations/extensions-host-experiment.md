# 扩展宿主兼容实验设计（P3 前置）

日期：2026-11-03。上游基线 `f9daca34`。本文是 build-plan-v1.4 P3「先用兼容性实验决定扩展宿主」的实验设计；**结论待 PoC 实证，本文不宣称任何兼容性**。

## 1. 上游扩展形态（盘点）

- 扩展 = TS/JS 模块，default export 为 `(pi: ExtensionAPI) => void|Promise` 工厂；`jiti` 加载（本地 TS 免编译）。位置：`~/.pi/agent/extensions/`、项目 `.pi/extensions/`、`--extension <path>`、包管理安装。
- `core/extensions/types.ts` 2308 行 + runner 1565 行。官方示例扩展 70+（`packages/coding-agent/examples/extensions/`）。
- 内置四个（codemode/tool-search/mcp/llama.cpp）是 **in-process inline factory**；pi-rs 的 MCP 已原生实现、llama/codemode/tool-search 尚无（本实验不覆盖内置扩展的 in-process 语义）。

## 2. API 使用分布（对 70+ 示例的实测统计）

逻辑面（可跨进程，JSONL 可序列化）：
| API | 用例数 | 说明 |
|---|---|---|
| `pi.on(<event>)` | 90 | session_start/shutdown、turn_start/end、before_agent_start、tool_call/result、input、session_tree/before_switch/before_fork/before_compact、user_bash、agent_* |
| `pi.registerCommand` | 56 | 名称+描述+handler(ctx) |
| `pi.registerTool` | 39 | 参数 schema、execute、(渲染器单独分类) |
| `pi.exec` | 19 | 进程执行原语 |
| `sendUserMessage`/`sendMessage` | 19 | 注入消息 |
| `setActiveTools`/`getActiveTools`/`getAllTools`/`appendEntry` | ~20 | 工具集与自定义 entry |
| `registerFlag`/`getFlag` | 8 | CLI 旗标面 |
| `setModel`/`setThinkingLevel`/`setSessionName`/`setLabel` | ~9 | 会话状态 |
| `registerProvider` | 2 | 自定义 provider（wire 级，可桥但工作量大） |

UI 面（分三类）：
- **可桥（对话框原语）**：`ctx.ui.notify`(138)、`select`(9)、`confirm`、`input`、`setEditorText`(6)、`setTitle`、`setStatus`(22)——文本进出，RPC 模式已有 hasUI 概念与 notice 帧先例。
- **不可桥（TUI 组件级，声明降级）**：`ctx.ui.custom`(30)、`setWidget`(5)、`setFooter`/`setHeader`、`setEditorComponent`、`registerMessageRenderer`/`registerEntryRenderer`/`registerShortcut`(2)、工具渲染器（renderCall/renderResult）。
- **宿主侧**：`ctx.ui.theme`(17) 读主题变量——host 侧由 pi-rs 推送当前主题色板即可。

## 3. 宿主选项对比

| 选项 | 上游扩展可运行性 | 依赖/维护成本 | 结论 |
|---|---|---|---|
| **A. 外部进程桥**：pi-rs 生成 `node`/`bun` 子进程跑宿主脚本，脚本内用 jiti 加载扩展原文件；ExtensionAPI 方法/事件经 stdio JSONL 双向转发 | 逻辑面+对话框原语可全兼容；TUI 组件级不可（进程边界无组件面）→ 按上游 `mode`/`hasUI` 声明降级 | 宿主脚本自研（单文件 JS，jiti 为独立 npm 包）；协议演进成本 | **推荐 PoC** |
| B. 嵌入 JS 运行时（deno_core/quickjs） | 理论上更贴近，但仍需复刻 jiti/TS 转译、Node 兼容层、全部 ctx 面 | 重量级依赖 + 长期维护负担；与上游行为偏差更难审计 | 不推荐（PoC 除非 A 证明不可行） |
| C. pi-rs 原生扩展 trait（编译期/dylib） | **不兼容上游扩展**（build-plan 明确不能宣称完整兼容） | 最低 | 仅作内部 API，不称「扩展系统」 |

## 4. PoC 设计（下一轮执行）

桥协议草图（复用 RPC 帧风格，独立通道不与 --mode rpc 混用）：

```
Rust → host:  {"type":"event","event":"tool_call","payload":{...}}
              {"type":"call","id":7,"method":"ui.select","params":{...}}     # 需要返回值的调用
host → Rust:  {"type":"return","id":7,"ok":true,"result":{...}}
              {"type":"action","method":"sendUserMessage","params":{...}}    # 无返回值动作
              {"type":"register","capability":"command","name":"hello",...}  # 工厂阶段声明
```

- 生命周期：工厂阶段同步收集 register* 声明 → Rust 侧建表；事件按上游 runner 顺序分发；`session_shutdown`→host 优雅退出（SIGKILL 兜底）。
- 成功标准（全部通过才可宣称「上游扩展可运行」，逐扩展记录证据）：
  1. `hello.ts`（纯命令+notify）行为一致；
  2. `notify.ts` 或 `session-name.ts`（session_start/shutdown 事件）时序一致；
  3. `question.ts`（ui.input/select 对话框）TUI 交互一致；
  4. `todo.ts` 或 `tools.ts`（registerTool：schema 校验、execute、tool_call 事件回路）一致；
  5. 一个 `--flag` 扩展（registerFlag/getFlag）；
  6. 失败隔离：host 崩溃→notice+扩展禁用，不拖垮主进程（对照上游单扩展错误隔离）。
- 显式不覆盖（首轮）：自定义 provider（wire 级）、codemode 沙箱（build-plan 要求单独验证）、TUI 组件级 API（降级声明+扩展可用 caps 探测）、包管理安装路径（包系统另轮）。

## 5. 信任与安全边界（如实声明）

上游声明扩展与 pi 同权限运行（读提示词/工具调用/凭据/会话历史）。桥形态下扩展进程权限相同（用户权限），信任门（project_trust 事件，仅个人/显式 --extension 参与）语义须在 Rust 侧保留：项目扩展仅在信任决策后加载。密钥不进桥日志/fixture。
