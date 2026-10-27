# 配置指南

## 目录布局

pi-rs 运行时状态与配置默认存放于用户主目录：

```
~/.pi-rs/agent/
├── settings.json       # 全局运行时偏好设置
├── models.json         # 自定义 Provider 与模型端点定义
└── sessions/           # 持久化会话目录
    └── <session-id>/
        └── session.jsonl
```

可通过环境变量覆盖默认路径：

```bash
export PI_RS_AGENT_DIR="/custom/path/to/agent"
```

注意：pi-rs 配置目录与上游 TypeScript pi 的 `~/.pi/` 相互隔离，两者可共存互不干扰。

## 环境变量配置

支持在工作目录或其父级目录的 `.env` 中声明凭据。启动时自动加载，已存在的进程环境变量优先。

### MiMo Token Plan（国内默认）

默认使用小米 MiMo Token Plan 接口：

```bash
# .env
MIMO_API_KEY="你的Token Plan密钥"
# 可选自定义端点（默认: https://token-plan-cn.xiaomimimo.com/v1）
MIMO_BASE_URL="https://token-plan-cn.xiaomimimo.com/v1"
```

默认模型标识为 `mimo/mimo-v2.6-pro`。

### OpenAI 兼容接口

```bash
# .env
OPENAI_API_KEY="sk-..."
```

命令行通过 `-m` 参数指定模型：

```bash
./target/release/pi-rs -m openai/gpt-5.2
```

## 自定义模型配置 (`models.json`)

在 `~/.pi-rs/agent/models.json` 中可声明第三方 OpenAI 兼容 API：

```json
{
  "providers": [
    {
      "name": "custom-openai",
      "api": "openai-completions",
      "baseUrl": "https://api.your-endpoint.com/v1",
      "models": [
        {
          "id": "deepseek-coder",
          "contextWindow": 65536,
          "maxTokens": 4096
        }
      ]
    }
  ]
}
```

声明后可通过 `-m custom-openai/deepseek-coder` 调起。
