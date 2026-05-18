# OpenTeams 技术文档 v0.3.20

> 草稿版本 - 待确认

## 1. 项目概述

OpenTeams 是一个多智能体对话平台，支持多个 AI 代理（Claude Code、Gemini CLI、Codex 等）在共享会话中协作。

### 1.1 技术栈
- **后端**: Rust (Axum Web 框架, SQLx, Tokio)
- **前端**: React + TypeScript + Vite + Tailwind
- **数据库**: SQLite (SQLx)
- **实时通信**: WebSocket

---

## 2. 模块职责

### 2.1 核心模块 (`crates/`)

| 模块 | 职责 |
|------|------|
| `db/` | 数据库模型、SQLx 迁移、CRUD 操作 |
| `server/` | HTTP API 路由、WebSocket 处理 |
| `services/` | 业务逻辑层：40+ 服务 |
| `executors/` | 代理执行编排、进程管理 |
| `utils/` | 通用工具函数 |
| `git/` | Git 操作封装 |
| `deployment/` | 部署配置管理 |

### 2.2 Services 层主要服务

| 服务 | 路径 | 职责 |
|------|------|------|
| `ChatService` | `services/chat.rs` | 消息解析、@提及、附件处理 |
| `ChatRunner` | `services/chat_runner.rs` | 代理执行编排、WebSocket 流 |
| `SkillRegistry` | `services/skill_registry.rs` | 技能发现与管理 |
| `AnalyticsService` | `services/analytics.rs` | 使用分析追踪 |
| `EventsService` | `services/events.rs` | 事件流与补丁 |

---

## 3. 数据模型

### 3.1 核心实体

```
ChatSession (会话)
├── id: Uuid
├── title: Option<String>
├── status: ChatSessionStatus (active/archived)
├── team_protocol: Option<String>
├── team_protocol_enabled: bool
└── default_workspace_path: Option<String>

ChatAgent (代理)
├── id: Uuid
├── name: String
├── runner_type: String (codex/claude/gemini...)
├── system_prompt: String
└── tools_enabled: JsonValue

ChatSessionAgent (会话-代理关联)
├── id: Uuid
├── session_id: Uuid
├── agent_id: Uuid
├── state: ChatSessionAgentState (idle/running/waiting_approval/dead)
├── workspace_path: Option<String>
└── allowed_skill_ids: Vec<String>

ChatMessage (消息)
├── id: Uuid
├── session_id: Uuid
├── sender_type: ChatSenderType (user/agent/system)
├── content: String
├── mentions: Vec<String>
└── meta: JsonValue (attachments, reference)

ChatRun (运行记录)
├── id: Uuid
├── session_id: Uuid
├── session_agent_id: Uuid
├── run_index: i64
├── run_dir: String
├── log_state: ChatRunLogState
└── artifact_state: ChatRunArtifactState

ChatSkill (技能)
├── id: Uuid
├── name: String
├── trigger_type: String (always/keyword/manual)
├── trigger_keywords: Vec<String>
├── enabled: bool
└── compatible_agents: Vec<String>

ChatWorkItem (工作项)
├── id: Uuid
├── session_id: Uuid
├── run_id: Uuid
├── item_type: ChatWorkItemType (artifact/conclusion)
└── content: String
```

### 3.2 数据库迁移
- 位置: `crates/db/migrations/`
- 数量: 80+ 迁移文件
- 工具: SQLx CLI

---

## 4. API 接口

### 4.1 Chat 路由 (`/chat`)

| 方法 | 路径 | 描述 |
|------|------|------|
| GET | `/chat/sessions` | 获取会话列表 |
| POST | `/chat/sessions` | 创建会话 |
| GET | `/chat/sessions/{id}` | 获取会话详情 |
| PATCH | `/chat/sessions/{id}` | 更新会话 |
| DELETE | `/chat/sessions/{id}` | 删除会话 |
| POST | `/chat/sessions/{id}/archive` | 归档会话 |
| POST | `/chat/sessions/{id}/restore` | 恢复会话 |
| GET | `/chat/sessions/{id}/stream` | WebSocket 流 |
| GET | `/chat/sessions/{id}/agents` | 获取会话代理 |
| POST | `/chat/sessions/{id}/agents` | 添加代理到会话 |
| GET | `/chat/sessions/{id}/messages` | 获取消息 |
| POST | `/chat/sessions/{id}/messages` | 发送消息 |
| GET | `/chat/runs/{run_id}` | 获取运行详情 |
| GET | `/chat/skills` | 获取技能列表 |
| POST | `/chat/skills` | 创建技能 |
| POST | `/chat/skills/download` | 下载远程技能 |
| GET | `/chat/work_items` | 获取工作项 |

### 4.2 WebSocket 事件 (`ChatStreamEvent`)

```typescript
type ChatStreamEvent =
  | { type: "message_new"; message: ChatMessage }
  | { type: "work_item_new"; work_item: ChatWorkItem }
  | { type: "agent_delta"; session_id: string; delta: string; is_final: boolean }
  | { type: "agent_state"; session_agent_id: string; state: ChatSessionAgentState }
  | { type: "mention_acknowledged"; status: MentionStatus }
  | { type: "compression_warning"; warning: CompressionWarning }
  | { type: "protocol_notice"; code: ChatProtocolNoticeCode }
```

---

## 5. 关键流程

### 5.1 消息发送流程

```
用户发送消息
    ↓
解析 @提及 → 提取 mentions
    ↓
创建 ChatMessage (DB)
    ↓
通过 WebSocket 广播 message_new 事件
    ↓
如果提及了代理 → 触发代理执行
```

### 5.2 代理执行流程 (ChatRunner)

```
1. 解析消息 mentions
2. 确定目标代理
3. 创建 ChatRun 记录
4. 启动执行器进程
5. 通过 WebSocket 流式输出 (agent_delta)
6. 捕获日志/diff
7. 更新 ChatSessionAgent 状态
8. 解析代理输出的 JSON protocol
```

### 5.3 Team Protocol

代理输出必须符合以下 JSON Schema:

```json
[
  { "type": "send", "to": "agent-name", "content": "...", "intent": "request|reply|notify|blocker|confirm" },
  { "type": "record", "content": "长期共享事实" },
  { "type": "artifact", "content": "交付物路径或输出" },
  { "type": "conclusion", "content": "当前轮次状态" }
]
```

---

## 6. 存储结构

### 6.1 运行时存储 (workspace-scoped)

```
<workspace>/
└── .openteams/
    ├── context/<session_id>/
    │   ├── messages.jsonl
    │   ├── shared_blackboard.jsonl  (record 类型)
    │   └── work_records.jsonl       (artifact/conclusion)
    └── runs/<session_id>/
        └── run_records/<run_id>/
            ├── context.jsonl
            └── logs/
```

### 6.2 日志管理

- 最大运行目录: 500MB
- 目标清理大小: 200MB
- 单运行最大日志: 8MB
- 工作区最大日志: 64MB

---

## 7. 仍待确认的问题

| # | 问题 | 状态 |
|---|------|------|
| 1 | Team Protocol 的 `intent` 枚举是否完整？当前: `request/reply/notify/blocker/confirm` | 待确认 |
| 2 | 压缩阈值 `DEFAULT_TOKEN_THRESHOLD = 50000` 是否适合所有模型？ | 待调整 |
| 3 | `ChatPermission` 的 capability/scope 粒度定义 | 待明确 |
| 4 | 跨会话代理状态同步机制 | 待设计 |
| 5 | 远程技能下载的缓存策略 | 待实现 |
| 6 | `archive_ref` 字段的具体使用场景 | 待确认 |

---

## 8. 配置与扩展

### 8.1 支持的代理类型
- `@anthropic-ai/claude-code`
- `@google/gemini-cli`
- `@openai/codex`
- `@qwen-code/qwen-code`
- Amp (内置)

### 8.2 环境变量
- `FRONTEND_PORT` - 前端端口
- `BACKEND_PORT` - 后端端口
- `HOST` - 主机地址
- `VK_ALLOWED_ORIGINS` - 允许的源

---

## 附录

- 类型定义: `shared/types.ts` (自动生成)
- 生成命令: `pnpm run generate-types`
- 类型生成源: `crates/server/src/bin/generate_types.rs`
