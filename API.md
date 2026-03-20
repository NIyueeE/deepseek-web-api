# DeepSeek Web API 文档

## 概述

本服务是 DeepSeek Chat API 的透明代理，提供自动认证和 PoW 计算。

**Base URL**: `http://localhost:5001/v0/chat`

**设计原则**: 透传 + 最小包装，只在必要时添加认证和 PoW。

---

## 端点列表

| 端点 | 方法 | 说明 |
|------|------|------|
| `/` | GET | 服务健康检查 |
| `/completion` | POST | 发送对话，透传 SSE |
| `/delete` | POST | 删除 session |
| `/upload_file` | POST | 上传文件 |
| `/fetch_files` | GET | 查询文件状态 |
| `/history_messages` | GET | 获取历史消息 |
| `/create_session` | POST | 创建新 session |

---

## 0. GET /

服务健康检查。

### 响应

```json
{"status":"ok","service":"deepseek-web-api"}
```

---

## 1. POST /completion

发送对话请求，自动管理会话状态。

### 请求体

```json
{
  "prompt": "你好",
  "chat_session_id": "可选，用于多轮对话",
  "search_enabled": true,
  "thinking_enabled": true,
  "ref_file_ids": []
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `prompt` | string | 是 | 对话内容 |
| `chat_session_id` | string | 否 | 会话 ID，不提供则自动创建 |
| `search_enabled` | boolean | 否 | 是否启用搜索，默认 true |
| `thinking_enabled` | boolean | 否 | 是否启用思考，默认 true |
| `ref_file_ids` | array | 否 | 引用文件 ID 列表 |

### 响应

SSE 流，每个 event 类型：

**ready 事件**（连接建立）：
```
event: ready
data: {"request_message_id":1,"response_message_id":2}
```

**update_session 事件**（会话更新）：
```
event: update_session
data: {"updated_at":1774012749.15355}
```

**消息内容事件**（逐字输出）：
```
data: {"v":{"response":{"message_id":2,"parent_id":1,"model":"","role":"ASSISTANT","thinking_enabled":true,"ban_edit":false,"ban_regenerate":false,"status":"WIP","accumulated_token_usage":0,"files":[],"feedback":null,"inserted_at":1774012749.149245,"search_enabled":true,"content":"","thinking_content":null,"thinking_elapsed_secs":null,"search_status":null,"search_results":null,"tips":[]}}
```

**内容片段事件**（流式文本）：
```
data: {"p":"response/thinking_content","v":"嗯"}
data: {"o":"APPEND","v":"，"}
data: {"v":"用户"}
data: {"v":"问"}
data: {"v":"了一个"}
data: {"v":"非常"}
data: {"v":"基础的"}
...
```

**完成事件**（最终状态）：
```
data: {"p":"response/status","v":"FINISHED"}
```

**finish 事件**（流结束）：
```
event: finish
data: {}
```

**title 事件**（会话标题）：
```
event: title
data: {"content":"会话标题"}
```

**close 事件**（连接关闭）：
```
event: close
data: {"click_behavior":"none","auto_resume":false}
```

**新会话响应头**（首次请求无 chat_session_id 时）：
```
x-chat-session-id: <chat_session_id>
```

> **注意**：连续对话通过 `parent_message_id` 实现，response_message_id 会作为下一次请求的 parent_message_id。

### 包装逻辑

1. **无 `chat_session_id`**：调用 `POST /api/v0/chat_session/create` 创建 session，记录到本地映射表
2. **有 `chat_session_id`**：从本地映射表获取 `parent_message_id`，添加到请求体
3. **添加 Header**：Authorization、Bearer Token、x-ds-pow-response

### 发送至 DeepSeek 的 payload

```json
{
  "chat_session_id": "xxx",
  "parent_message_id": null 或 2 或 4...",
  "preempt": false,
  "prompt": "你好",
  "ref_file_ids": [],
  "search_enabled": true,
  "thinking_enabled": true
}
```

---

## 2. POST /delete

删除指定的 session。

### 请求体

```json
{
  "chat_session_id": "需要删除的 session ID"
}
```

### 响应

```json
{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}
```

### 包装逻辑

1. 从本地映射表删除记录
2. 转发请求至 `POST /api/v0/chat_session/delete`

---

## 3. POST /upload_file

上传文件到 DeepSeek。

### 请求体

`Content-Type: multipart/form-data`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `file` | binary | 是 | 文件内容 |

### 响应

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "id": "file-d3983c30-0679-425c-a3c0-6596940052f9",
      "status": "PENDING",
      "file_name": "README.md",
      "previewable": false,
      "file_size": 3154,
      "token_usage": 0,
      "error_code": null,
      "inserted_at": 1774017375.563,
      "updated_at": 1774017375.563
    }
  }
}
```

### 包装逻辑

添加 Authorization header、x-ds-pow-response、x-file-size，转发至 `POST /api/v0/file/upload_file`

---

## 4. GET /fetch_files

查询文件解析状态。

### Query 参数

```
GET /fetch_files?file_ids=file-xxx,file-yyy
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `file_ids` | string | 是 | 逗号分隔的文件 ID 列表 |

### 响应

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "files": [
        {
          "id": "file-xxx",
          "status": "SUCCESS",
          "file_name": "README.md",
          "previewable": true,
          "file_size": 3154,
          "token_usage": 817,
          "error_code": null,
          "inserted_at": 1774017375.563,
          "updated_at": 1774017639.0
        }
      ]
    }
  }
}
```

> **status 说明**：`PENDING` = 解析中，`SUCCESS` = 解析完成，`FAILED` = 解析失败

### 包装逻辑

添加 Authorization header，转发至 `GET /api/v0/file/fetch_files`

---

## 5. GET /history_messages

获取会话的历史消息。

### Query 参数

```
GET /history_messages?chat_session_id=xxx&offset=0&limit=20
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `chat_session_id` | string | 是 | 会话 ID |
| `offset` | integer | 否 | 消息偏移，默认 0 |
| `limit` | integer | 否 | 消息数量，默认 20 |

### 响应

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "chat_session": {
        "id": "acd61ee0-ceaa-426c-aaf2-5e91f6e8792c",
        "title": null,
        "title_type": "WIP",
        "pinned": false,
        "updated_at": 1774012756.515,
        "seq_id": 196175956,
        "agent": "chat",
        "version": 0,
        "current_message_id": null,
        "inserted_at": 1774012756.515
      },
      "chat_messages": [],
      "cache_valid": false,
      "route_id": null
    }
  }
}
```

### 包装逻辑

添加 Authorization header，转发至 `GET /api/v0/chat/history_messages`

---

## 6. POST /create_session

手动创建新 session。

### 请求体

```json
{
  "agent": "chat"
}
```

### 响应

```json
{
  "code": 0,
  "msg": "",
  "data": {
    "biz_code": 0,
    "biz_msg": "",
    "biz_data": {
      "id": "acd61ee0-ceaa-426c-aaf2-5e91f6e8792c",
      "seq_id": 196175956,
      "agent": "chat",
      "model_type": "DEFAULT",
      "title": null,
      "title_type": "WIP",
      "version": 0,
      "current_message_id": null,
      "pinned": false,
      "inserted_at": 1774012756.515,
      "updated_at": 1774012756.515
    }
  },
  "chat_session_id": "acd61ee0-ceaa-426c-aaf2-5e91f6e8792c"
}
```

### 包装逻辑

添加 Authorization header，转发至 `POST /api/v0/chat_session/create`

---

## 会话状态管理

服务端维护内部映射表：

```
chat_session_id → last_response_message_id
```

### 规则

1. 首次请求（无 `chat_session_id`）：`parent_message_id = null`
2. 后续请求：自动获取上次的 `response_message_id`（通过 SSE 解析）作为 `parent_message_id`

---

## 认证

服务端使用配置的单一账号自动认证，无需客户端提供 Authorization header。

---

## DeepSeek 原始端点映射

| 本服务 | DeepSeek 原始端点 |
|--------|------------------|
| `/completion` | `/api/v0/chat/completion` |
| `/delete` | `/api/v0/chat_session/delete` |
| `/upload_file` | `/api/v0/file/upload_file` |
| `/fetch_files` | `/api/v0/file/fetch_files` |
| `/history_messages` | `/api/v0/chat/history_messages` |
| `/create_session` | `/api/v0/chat_session/create` |
