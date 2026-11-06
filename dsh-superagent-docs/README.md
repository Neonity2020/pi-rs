# dsh-superagent 架构文档体系

本目录是基于 DeepSeek 开源原型 **dsh-superagent-prototype**（2026-10-31 发布，MIT 许可）与论文
*Exploring Super-agent Design: Towards a More Autonomous Agent System* 整理的**完整中文架构文档体系**。

> **定位声明**：dsh-superagent 是 DeepSeek Harness 团队发布的**研究原型**，不是官方产品。
> 官方 README 明确说明：发布代码与内部版本、与论文描述均**不完全一致**，论文中部分设计属于未来工作。
> 本文体系以官方仓库 `docs/000–008` 为权威依据，用源码实现补充细节，并在说法存在分歧处标注来源。

## 一句话架构

**一个常驻在仓库旁的多智能体协调层**：主 Session（Router/Orchestrator）只做评估与派工，
每个授权工单（Ticket）由一个隔离的 Worker Session 在独立目录中执行并回交付物（通常是 PR），
事件驱动接入、双层授权门控、可恢复的持久副作用队列贯穿始终——
设计目标是**尽可能少占用人类注意力，尽可能多创造价值**。

## 文档地图

| 编号 | 文档 | 内容 | 对应官方文档 |
| --- | --- | --- | --- |
| [00](00-项目总览.md) | 项目总览 | 背景、产品形态、端到端走查、与论文的关系 | README / SETUP / 论文 |
| [01](01-设计哲学.md) | 设计哲学 | 注意力经济学、行为准则、设计准则、运营者责任 | docs/000 |
| [02](02-领域模型.md) | 领域模型 | 核心概念、聚合与不变式、被拒绝的概念、术语表 | docs/001 + `src/core/model.ts` |
| [03](03-总体架构.md) | 总体架构 | 分层结构、依赖规则、模块地图、持久化 | docs/007 + 全部 `src/` |
| [04](04-事件驱动与授权.md) | 事件驱动与授权 | Source 插件、Trigger、门控与准入表、Eval、Mandate | docs/002 + `src/sources/`、`src/policy.ts` |
| [05](05-执行与工单生命周期.md) | 执行与工单生命周期 | Ticket 状态机、Assignment、Worker、并发、恢复 | docs/003 + `src/executor/` |
| [06](06-可靠性与数据完整性.md) | 可靠性 | Effect 队列、幂等性、故障恢复矩阵、迁移 | docs/003 + `src/app/effects.ts`、`store.ts` |
| [07](07-提示词体系.md) | 提示词体系 | 组装顺序、优先级栈、团队可编辑部分、版本化 | docs/004 + `prompts/` |
| [08](08-面板与管理面.md) | 面板与管理面 | 面板三问、设计原则、管理 API、健康体系 | docs/005/006 + `src/app/http.ts` |
| [09](09-部署与运维.md) | 部署与运维 | 组件拓扑、安装、启停、巡检、升级 | docs/006 + SETUP + `scripts/` |
| [10](10-安全与信任边界.md) | 安全与信任边界 | 信任模型、注入风险、隔离建议 | docs/002/006 + 论文报道 |
| [11](11-扩展开发指南.md) | 扩展开发指南 | 新增 Source 的完整步骤与测试约定 | docs/008 |
| [12](12-关键设计决策.md) | 设计决策与评估 | ADR 风格决策记录、实验数据、局限与开放问题 | 论文报道 + docs/007 |

## 阅读路径

- **快速了解**（15 分钟）：00 → 01 → 02 中的流程图与术语表
- **实现者**：02 → 03 → 04 → 05 → 06，对照官方仓库 `docs/007-architecture.md` 的模块表查代码
- **运维者**：09 → 08 → 10
- **二次开发者**：02 → 04 → 11
- **架构研究者**：01 → 12 → 02（关注"被拒绝的概念"一节）

## 顶层结构总览

```mermaid
flowchart LR
  subgraph 接入层
    GH[GitHub 轮询] 
    FS[飞书事件/轮询]
  end
  subgraph 协调层（主 Session）
    EV[Eval 评估] --> DEC[Decision 决定]
  end
  subgraph 执行层
    W1[Worker Session 1 + 目录]
    W2[Worker Session N + 目录]
  end
  GH --> TR[Trigger] --> EV
  FS --> TR
  DEC --> TK[Ticket] --> W1 & W2
  W1 & W2 -->|report| TK
  TK -->|PR / 回复| GH
```

## 主要来源

- 官方仓库：[github.com/deepseek-ai/dsh-superagent-prototype](https://github.com/deepseek-ai/dsh-superagent-prototype)
- DeepSeek Harness：[deepseek.com/en/harness](https://www.deepseek.com/en/harness/)
- 论文：*Exploring Super-agent Design: Towards a More Autonomous Agent System*（随原型一同发布；论文与原型实现存在差异）
- 解读文章：[MindStudio](https://www.mindstudio.ai/blog/deepseek-super-agent-harness-design) ·
  [Pragmatic AI](https://pragmaticai.substack.com/p/deepseek-superagent-from-a-github) ·
  [MarkTechPost](https://www.marktechpost.com/2026/11/01/deepseek-dsh-superagent-router-worker-coding-agent) ·
  [知乎讨论](https://www.zhihu.com/question/2099984457524572950)
- 源码分析：本文体系中标注的 `src/**` 文件引用，均基于 2026-11-05 克隆的仓库快照（单 commit `main` 分支）。
