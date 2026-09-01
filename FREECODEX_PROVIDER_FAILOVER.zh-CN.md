# FreeCodex 模型 Provider 自动故障转移

FreeCodex 可以在主 ChatGPT/Codex 推理额度耗尽后，将当前本地 session 自动切换到一个用户配置的 OpenAI Responses API 兼容 provider。

## 目标

- 平时继续使用 ChatGPT/Codex 登录和官方模型。
- 仅在明确的额度/套餐耗尽错误时自动切换备用 provider。
- 普通 429 限速、网络故障、401、服务过载不会触发永久切换。
- 切换后，本 session 后续模型请求继续使用备用 provider，避免每一次请求都重新撞官方额度。
- 不改变 shell、文件、MCP、Browser/Computer Use 的审批和安全策略。

## 配置

在 `~/.codex/config.toml` 中定义备用 provider，并声明故障转移目标：

```toml
[model_provider_fallback]
provider = "backup"
model = "your-backup-model"

[model_providers.backup]
name = "Backup OpenAI Compatible"
base_url = "https://api.example.com/v1"
env_key = "FREECODEX_BACKUP_API_KEY"
wire_api = "responses"
```

然后在启动 FreeCodex 的环境中设置：

```bash
export FREECODEX_BACKUP_API_KEY="..."
```

Windows PowerShell：

```powershell
$env:FREECODEX_BACKUP_API_KEY="..."
```

API Key 不应直接写入 `config.toml`；`env_key` 填的是保存 API Key 的环境变量名。

## 兼容要求

备用服务必须兼容 OpenAI Responses API（`/v1/responses`）。当前 Codex 已不再支持 `wire_api = "chat"`，因此只有 Chat Completions `/v1/chat/completions` 而没有 Responses API 的服务不能直接作为本功能的备用 provider。

备用模型应至少支持 FreeCodex 当前任务使用到的 function/tool calling 能力。FreeCodex 在切换到第三方 provider 后会关闭 OpenAI 内部 Responses Lite、ChatGPT attestation、Codex WebSocket 路由和 service tier 等第一方专用行为，尽量使用标准 Responses API 请求。

## 触发条件

V1 仅在以下 Codex 错误分类时触发：

- `UsageLimitReached`
- `QuotaExceeded`
- `UsageNotIncluded`

不会因为以下情况自动永久切换：

- 普通 `RateLimitExceeded`（临时 429）
- `Unauthorized` / 401
- `ServerOverloaded`
- 网络断开或超时

这是为了避免临时故障把整个 session 错误地切到备用收费服务。

## Session 行为

自动故障转移是 session 级状态：一旦当前 session 成功触发切换，后续模型调用直接使用备用 provider。新启动的 session 仍首先使用主 provider；如果主额度仍未恢复，则首次命中额度错误后再次切换。

这不会改写用户的 `model_provider` 默认配置，也不会把备用 provider 永久设成主 provider。