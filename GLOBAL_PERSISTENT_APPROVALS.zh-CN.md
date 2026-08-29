# 全会话持久授权机制分析与改造方案

## 当前分支实施状态

OpenCodex 的 `freecodex` 分支已经完成前两个阶段的最小落地：

- 保留现有协议名称 `ApprovedForSession` / `AcceptForSession`，避免破坏现有客户端兼容性。
- 用户选择该范围后，Core 除了写入当前会话内存缓存，还会把规范化后的精确授权键写到 `$CODEX_HOME/approvals/tool/`。
- 每个授权键使用独立 JSON 文件并原子写入；新建、恢复、并行会话和应用重启后，都可以读取同一授权。
- 文件修改仍按 `environment_id + path` 精确匹配；命令仍包含环境、命令、工作目录、TTY、沙箱和附加权限；MCP 仍按 server、connector 和 tool 匹配。不同账号、工具、命令或路径不会因为已有授权被扩大放行。
- `Approved`（仅本次）不会落盘；拒绝、超时和取消不会落盘；管理员要求、沙箱策略和其他拒绝链路仍然优先执行。
- `request_permissions` 返回 `Session` 范围时，规范化后的动态网络/文件权限会写到 `$CODEX_HOME/approvals/permissions/`，其他本机会话在相同执行环境、cwd 和 workspace roots 下可直接复用。
- 动态权限每次仍与当前权限配置合并，持久授权不会绕过托管 deny、不可升级路径或 `strict_auto_review` 对 Session 授权的禁止。

为了兼容现有桌面客户端，OpenCodex 暂时复用协议中的 `Session` 名称，将它实现为“本机同一精确环境范围可持续复用”；没有新增会导致旧客户端不认识的枚举值。Browser / Computer Use actor、授权查看与撤销 API 仍属于后续阶段。

## 目标

用户要求的目标不是“默认批准某个本机 MCP”，而是：

> 用户对一个明确的业务操作范围完成一次授权后，同一授权在以后新建、恢复或切换的所有本机会话中继续有效；后续命中同一范围的操作不再重复确认，直到授权被撤销、过期或被更高优先级策略禁止。

这需要一套统一的、跨会话的持久授权系统。上游原始实现尚不存在这样的统一系统；`freecodex` 已开始按本文阶段方案补齐。

## 原始实现为什么做不到

### 授权被分散在多套系统中

| 操作类型 | 当前记忆范围 | 是否跨会话 | 当前持久化方式 |
| --- | --- | --- | --- |
| MCP / App 工具 | 单次、session、部分可永久 | 部分支持 | 把具体工具的 `approval_mode` 写成 `approve` |
| shell 命令 | 单次、session、命令前缀规则 | 支持特定前缀 | 写入 `$CODEX_HOME/rules/default.rules` |
| 网络访问 | 单次、session、具体 host 规则 | 支持具体 host | 写入 `$CODEX_HOME/rules/default.rules` |
| 文件修改 | 单次或 session | 不支持 | 仅内存 `ApprovalStore` |
| `request_permissions` | turn 或 session | OpenCodex 已支持 | `$CODEX_HOME/approvals/permissions/` 下的精确环境授权 |
| Browser Use Origin | 由桌面浏览器组件管理 | Rust Core 不负责持久化 | 本仓库只暴露策略与管理约束 |
| Computer Use 应用 | 由桌面组件管理 | Rust Core 不负责持久化 | 本仓库只暴露应用策略与管理约束 |
| 浏览器发布/发送确认 | 模型目录下发给 actor | 本地 Core 无统一覆盖 | `confirmation_policies` 元数据 |

### 通用审批缓存每次 Session 都是空的

`codex-rs/core/src/session/session.rs:1353-1379` 创建 Session 时执行：

```rust
tool_approvals: Mutex::new(ApprovalStore::default())
```

`ApprovalStore` 本身只是内存中的 `HashMap`，见 `codex-rs/core/src/tools/sandboxing.rs:39-62`。它没有磁盘加载、账户同步或跨 Session 恢复逻辑。

因此 `ApprovedForSession` 的真实含义就是当前 Session，不是所有会话。

### 文件修改只能选择 Session

App Server 协议的文件审批只有：

- `Accept`
- `AcceptForSession`
- `Decline`
- `Cancel`

见 `codex-rs/app-server-protocol/src/protocol/v2/item.rs:109-121`。协议中没有 `AcceptPersistently`。

文件审批缓存键是 `environment_id + path`，见 `codex-rs/core/src/tools/approvals.rs:252-265`，但该键只进入当前 Session 的内存缓存。

### 动态权限的上游实现最多保存到 Session

`PermissionGrantScope` 只有：

```rust
Turn,
Session,
```

见 `codex-rs/protocol/src/request_permissions.rs:10-16`。

上游授权记录分别只写入 TurnState 或 SessionState，没有全局持久范围。OpenCodex 在保留该内存行为的同时，把 Session 范围同步到本机持久授权库，并在后续会话执行前读取合并。

### 命令和网络已经有各自的持久规则，但没有统一授权语义

命令批准可以写成 `prefix_rule`，网络批准可以写成 `network_rule`。文件写入入口位于：

- `codex-rs/execpolicy/src/amend.rs:65-124`
- `codex-rs/core/src/exec_policy.rs:453-494`
- `codex-rs/core/src/exec_policy.rs:497-534`

这些规则可以在新会话加载，但只能表达命令前缀或网络 host，不能表达“某个业务已经授权”。

### 浏览器确认存在仓库外的执行层

Core 会把模型目录中的 Browser Use / Computer Use `confirmation_policies` 原样发送给 `node_repl` / `cua_repl` actor：

`codex-rs/core/src/mcp_tool_call.rs:1239-1278`。

这意味着只修改 Core 的通用 `ApprovalStore` 仍不能保证浏览器发布不再询问；浏览器 actor 也必须接入同一套持久授权判断，或者接受 Core 传入的已匹配授权证明。

## “只要我授权了”的准确语义

不建议把一次授权解释成“从此所有操作都不确认”。合理语义应当是：

```text
授权主体 + 业务能力 + 操作 + 目标资源 + 账号 + 环境 + 有效期
```

例如：

```text
允许当前用户
通过 netease_uploader
对网易云音乐账号 A
执行 upload_audio、upload_cover、publish_song
目标为当前音乐项目
长期有效，直到撤销
```

它不应自动覆盖：

- 删除历史歌曲。
- 操作另一个账号。
- 购买、付款或订阅。
- 删除本地文件。
- 在未授权网站发布内容。

## 推荐的数据模型

建议新增统一的 `PersistentApprovalGrant`：

```rust
struct PersistentApprovalGrant {
    id: String,
    subject: ApprovalSubject,
    capability: ApprovalCapability,
    action: String,
    resource_scope: ApprovalResourceScope,
    account_scope: Option<String>,
    environment_scope: Option<String>,
    constraints: ApprovalConstraints,
    granted_at: i64,
    expires_at: Option<i64>,
    policy_version: u32,
}
```

能力类型至少包括：

```rust
enum ApprovalCapability {
    McpTool,
    ExecCommand,
    NetworkHost,
    FileSystemWrite,
    BrowserOrigin,
    BrowserAction,
    ComputerApplication,
}
```

授权键不能只使用自然语言。它必须由工具元数据、目标账号、Origin、路径或命令结构生成，确保不同会话得到相同且可验证的匹配结果。

## 推荐存储位置

### 同一台机器上的所有会话

可以在 `$CODEX_HOME` 下增加专用存储，例如：

```text
$CODEX_HOME/approvals/approvals.json
```

或放入已有本地 SQLite 基础设施。专用表更适合并发、撤销、过期、审计和迁移。

### 多设备、同一账户的所有会话

仅使用本地文件不够，需要账户侧同步服务。该服务不在本仓库中，必须由产品后端支持。

因此“所有会话”需要区分：

- 本机所有会话：本仓库可以实现。
- 所有设备上的账户会话：需要仓库外后端配合。

## 统一审批流程

建议把当前流程改成：

```text
工具准备执行
    ↓
生成规范化 ApprovalRequestDescriptor
    ↓
先应用管理员 requirements / deny 规则
    ↓
查询 PersistentApprovalStore
    ↓
命中且未过期 ─────────────→ 自动批准并记录审计
    ↓ 未命中
执行现有 Guardian / 用户确认
    ↓
用户选择：仅本次 / 当前会话 / 始终允许此范围
    ↓
“始终允许”写入 PersistentApprovalStore
    ↓
执行操作
```

管理员拒绝、组织策略、模型安全限制必须始终高于用户持久授权，不能被授权库覆盖。

## 协议和界面需要增加什么

### 统一范围枚举

建议把零散的 `Turn`、`Session` 扩展为：

```rust
enum ApprovalGrantScope {
    Once,
    Turn,
    Session,
    Persistent,
}
```

### 审批响应

命令、文件、权限、MCP、浏览器操作都应能返回统一的：

```text
accept + scope + normalized grant descriptor
```

而不是每种协议分别定义 `AcceptForSession`、policy amendment 或自定义 elicitation meta。

### 授权管理界面

必须提供：

- 查看所有长期授权。
- 查看授权的工具、账号、Origin、路径和有效期。
- 单项撤销。
- 一键撤销某个业务、连接器或网站的全部授权。
- 显示最近使用时间和审计记录。

没有撤销界面的永久授权不适合默认开放。

## 分阶段落地

### 第一阶段：Core 精确授权缓存持久化（已实现）

本分支已完成：

1. 把现有 `ApprovalStore` 改为可选磁盘后端，并在生产 Session 中启用。
2. 每个序列化授权键生成稳定文件 ID，独立原子持久化，避免并行会话覆盖彼此授权。
3. 通用工具缓存与 MCP session-remember 读取同一本机持久存储。
4. 保留旧 `config.toml`、MCP 永久批准和 `default.rules` 的兼容读取。
5. 增加精确范围、仅本次不落盘、跨 Session 复用的测试。

为了控制第一阶段改动规模，本阶段复用了现有精确缓存键，尚未引入统一的公开 `PersistentApprovalStore` trait 和完整 `ApprovalRequestDescriptor` 协议；这些会与撤销 API 一起在后续阶段抽象。

### 第二阶段：动态权限跨会话复用（已实现）

本分支已完成：

1. `request_permissions` 的 Session 授权以独立 JSON 文件原子写入 `$CODEX_HOME/approvals/permissions/`。
2. 持久键绑定 `environment_id + cwd + workspace_roots`，workspace roots 会排序去重，避免不同项目之间误复用。
3. 同一环境下的多个精确授权分别保存，读取时合并；并行会话不会因为覆盖同一总文件而丢失其他授权。
4. 当前会话内授权与磁盘授权在执行前合并，因此新建会话和已打开的并行会话都能看到后来写入的授权。
5. 保留 Turn 仅本轮、不落盘；空授权、拒绝、取消、超时及 `strict_auto_review` Session 响应均不生成长期权限。
6. 增加持久存储重载、工作区隔离及跨本地 Session 复用测试。

本阶段没有扩大单个文件修改审批的范围：第一阶段已经按既有精确文件键持久化 `AcceptForSession`。后续若要支持“整个规范化目录”授权，应当新增显式范围和撤销界面，不能把一次文件确认静默扩大为目录权限。

### 第三阶段：Browser / Computer Use

1. 给 actor 传递不可伪造的授权 ID 或已匹配授权描述。
2. Browser Use 按 Origin + action + account 匹配。
3. Computer Use 按应用身份 + action 匹配。
4. actor 的 confirmation policy 在命中有效用户授权时跳过重复确认；组织强制确认仍保留。

### 第四阶段：授权管理与同步

1. App Server 增加授权 list / revoke API。
2. Desktop 增加授权管理页面。
3. 如果要求跨设备，再接入账户侧同步和冲突处理。

## 预计修改位置

主要涉及：

- `codex-rs/core/src/tools/sandboxing.rs`
- `codex-rs/core/src/tools/approvals.rs`
- `codex-rs/core/src/session/session.rs`
- `codex-rs/core/src/session/mod.rs`
- `codex-rs/core/src/mcp_tool_call.rs`
- `codex-rs/core/src/exec_policy.rs`
- `codex-rs/protocol/src/request_permissions.rs`
- `codex-rs/app-server-protocol/src/protocol/v2/item.rs`
- `codex-rs/app-server-protocol/src/protocol/v2/permissions.rs`
- App Server 的审批请求/响应处理和 schema fixtures
- Desktop Browser/Computer Use actor；该实现不完整地包含在本仓库中

这不是适合一次提交的大改。按仓库的 800 行变更限制，应拆成至少三个可独立评审阶段。

## 测试要求

至少需要覆盖：

1. 会话 A 永久批准后，会话 B 对完全相同范围不再询问。
2. 不同工具、账号、Origin、路径或环境不能错误复用授权。
3. 授权撤销后，新旧会话下一次操作都重新询问。
4. 授权过期后重新询问。
5. 管理 deny 规则优先于用户持久授权。
6. 工具元数据或策略版本变化时，旧授权按规则失效。
7. 并发会话同时写授权时不丢数据、不产生损坏。
8. 从现有 MCP `approve`、exec prefix 和 network host 规则兼容迁移。

## 最终判断

用户要求的“授权一次，所有会话不再重复确认”在产品逻辑上是可实现的，但当前仓库只实现了若干互不统一的局部持久化能力。

正确的改造不是全局关闭确认，也不是统一把所有工具设为 `approve`，而是建立一个跨 Session 的、范围明确、可撤销、可过期且服从管理员上限的 `PersistentApprovalStore`，并让 MCP、命令、网络、文件权限以及 Browser/Computer Use 在提示前统一查询它。

官方 OpenAI 文档建议为外部写入、破坏性或扩展范围的操作明确设置授权边界。持久授权应当把这个边界编码成可验证的数据，而不是仅依赖对话文本：<https://developers.openai.com/api/docs/guides/latest-model#define-autonomy-and-approval-boundaries>。
