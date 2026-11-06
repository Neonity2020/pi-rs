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

## 6. PoC 执行结果（2026-11-03）

**结论：外部进程桥可行，6/6 成功标准全部实证**（`crates/pi-extension-host`，7 个集成测试全过；fixture 为上游原样扩展 + 3 个 PoC 自制）。

| 标准 | 验证 | 证据 |
|---|---|---|
| 1. 纯命令+notify（hello/session-name） | ✅ | 命令注册、handler 回路、notify 对话框往返 |
| 2. session_start/shutdown 事件（notify/session-name） | ✅ | agent_settled 分发；OSC 777 转义经 stderr 转发完好 |
| 3. 对话框扩展（permission-gate） | ✅ | tool_call 事件 → ui.select 往返 → block/allow 决策回流 |
| 4. registerTool 回路（hello.ts 原样） | ✅ | schema 以 JSON 注册、execute 返回与上游扩展体逐字一致 |
| 5. --flag 扩展 | ✅ | registerFlag 注册 + getFlag 经 env 注入取值 |
| 6. 失败隔离 | ✅ | 工厂抛错→硬错误上报 exit；handler 抛错→单次调用报错、host 存活续服务 |

实现要点（全部实测发现，非推测）：

- **零依赖 host 可行**：Node ≥ 22.6 原生 TS 类型剥离直接加载上游扩展（本轮 fixtures 均为可擦除语法）；`jiti` 对含枚举等不可擦除语法的真实扩展仍可能需要（生产桥应支持，PoC 未覆盖）。
- **包解析**：宿主运行时物化 shim `node_modules` + `module.register` resolve 钩子，把 `@earendil-works/pi-coding-agent`（defineTool 恒等）、`@earendil-works/pi-ai`/`typebox`（最小 Type 构造器）、`@earendil-works/pi-tui`（抛错的声明性桩）映射到 shim；扩展文件保持原路径加载。
- **同步/异步分界是协议关键**：上游同步 API（getFlag/appendEntry/set-getSessionName/get-setActiveTools/getAllTools/get-setThinkingLevel）无法 await stdio 往返——host 本地镜像状态 + 单向 `action` 帧；异步 API（ui.*/sendMessage/setModel/exec）走 id 匹配调用。
- **扩展 stdout 必须重定向**：上游扩展直接写 stdout（OSC 777 通知等），会破坏协议帧流——host 把非帧 stdout 全部转发 stderr，Rust 侧按原始字节收尾桶（OSC 无换行）。
- **进程内调用与请求返回交错**：Rust 侧共享状态（Arc<Mutex>）使 call future 与 host 反向调用（对话框）可同时进行。

生产桥差距（下一轮接线清单，按依赖排序）：接入 pi-coding-agent（`-e/--extension`/`-ne`、扩展目录发现、信任门：项目扩展仅信任决策后加载）→ 桥产物入会话（工具注册进 registry、命令进斜杠表、事件接 agent/session 生命周期）→ jiti 兜底非可擦除语法 → sessionManager 读面与 input/session_tree/before_* 事件补齐 → 对照上游 runner 的事件顺序审计。

## 7. 生产桥增量（2026-11-06，第四十三轮）

- **jiti 兜底已接线**（PoC 差距清单第 3 项闭环）：spawn 阶梯三环——原生剥离（无旗标）→ `--experimental-strip-types` → jiti（`createRequire(extensionPath).resolve("jiti")` 从扩展目录向上解析，未命中保持原始 strip 错误）；`PI_EXT_LOADER` 每次 spawn 显式设置（`""`/`"jiti"`）防环境继承翻转；帧协议零变化，仅失败 `stage` 区分（`import` vs `jiti`）。jiti `alias` 承载 `@earendil-works` shim 映射（jiti 有独立解析管线，不走全局 loader hooks）。上游对照：loader.ts:570-584 无条件经 jiti 加载，strip-first 阶梯是移植侧等价物。测试用最小假 jiti 包（CJS 入口、仅 enum 变换）——真实 jiti 未入库，其行为未经测试验证（声明）。
- **事件顺序审计完成**：见 [extensions-event-order-audit.md](extensions-event-order-audit.md)——分发顺序与上游一致；3 个未声明 P1（tool_call 异常 fail-open、terminate 批次 any-vs-every、before_agent_start 每请求触发）已在同轮修复；P2 字段缺失族与修复顺序在文档末尾。
