# Core 模块文档

Core 是项目的**纯技术/基础设施层**，不包含业务逻辑。所有模块均提供线程安全或异步安全的实现。

---

## 1. config.py - 配置管理

### 实现原理

使用 Python 标准库 `tomllib`（3.11+）或 `tomli` 解析 TOML 配置文件。模块加载时执行一次 `load_config()`，将配置缓存到全局 `CONFIG` 字典。

**配置文件结构**：

```toml
[account]           # 账号信息
email = "..."
password = "..."
token = "..."      # 登录后自动填充

[auth]             # 服务端鉴权（简化配置）
tokens = ["sk-...", "sk-..."]  # 字符串数组，非空则启用鉴权

[headers]          # HTTP 请求头（透传给 DeepSeek）
Host = "chat.deepseek.com"
User-Agent = "DeepSeek/1.3.0 Android/35"
...

[browser]          # 浏览器伪装
impersonate = "safari15_3"
```

### 导出方法

| 方法 | 说明 |
|------|------|
| `CONFIG` | 全局配置字典（模块级变量） |
| `load_config()` | 从 config.toml 重新加载配置 |
| `save_config(cfg)` | 将配置写回 config.toml |
| `CONFIG_PATH` | 配置文件路径，默认 `config.toml` |
| `DEEPSEEK_HOST` | DeepSeek 域名常量 |
| `DEEPSEEK_LOGIN_URL` | 登录 API 地址 |
| `DEEPSEEK_CREATE_POW_URL` | PoW 挑战 API 地址 |
| `BASE_HEADERS` | 从配置读取的 HTTP 请求头 |
| `DEFAULT_IMPERSONATE` | 浏览器伪装标识 |
| `WASM_PATH` | WASM 模块文件路径 |
| `get_auth_tokens()` | 读取 `auth.tokens` 字符串数组（非空则需要鉴权） |
| `get_pool_size()` | 读取 session pool 最大并发数（默认 10） |
| `get_pool_acquire_timeout()` | 读取等待 session 的超时秒数（默认 30.0） |
| `get_server_host()` | 读取 `[server].host` |
| `get_server_port()` | 读取 `[server].port` |
| `get_server_reload()` | 读取 `[server].reload` |
| `get_cors_origins()` | 读取 `[server].cors_origins` |
| `get_cors_allow_credentials()` | 读取 `[server].cors_allow_credentials` |
| `get_cors_allow_methods()` | 读取 `[server].cors_allow_methods` |
| `get_cors_allow_headers()` | 读取 `[server].cors_allow_headers` |

---

## 2. logger.py - 日志输出

### 实现原理

使用 Python 标准 `logging` 模块，配合自定义 `ColoredFormatter` 实现终端彩色输出。

**日志级别颜色映射**：
- DEBUG → 灰色
- INFO → 蓝色
- WARNING → 黄色
- ERROR → 红色

### 导出方法

| 方法 | 说明 |
|------|------|
| `setup_logger(name, level)` | 初始化带颜色的 logger |
| `logger` | 默认 logger 实例 |

---

## 3. auth.py - 身份认证

### 实现原理

**懒加载**：启动时不登录，token 在首次调用 `get_token()` 时才获取。

**token 持久化**：登录成功后自动保存到 config.toml，下次启动时从配置读取。

**自动刷新**：API 返回认证错误（code=40003）时，自动调用 `invalidate_token()` 使内存和配置文件中的 token 失效，下次请求时重新登录。

**线程安全**：使用 `_token_lock` 保护 token 的获取和设置，避免竞态条件。

```
流程：
1. get_token() 检查内存中的 _account["token"]
2. 若无，检查 CONFIG["account"]["token"]
3. 若无，调用 login() 获取新 token
4. login() 成功后调用 _save_token() 持久化
```

### 导出方法

| 方法 | 说明 |
|------|------|
| `init_single_account()` | 初始化账号配置（懒加载，不登录） |
| `login()` | 登录获取新 token，保存到配置 |
| `_save_token(token)` | 将 token 持久化到 config.toml |
| `invalidate_token()` | 使 token 失效（清除内存和配置） |
| `get_token()` | 获取 token，必要时自动登录 |
| `get_auth_headers()` | 获取带 Authorization 的完整请求头 |

---

## 4. pow.py - PoW 工作量证明

### 实现原理

DeepSeek API 要求每次请求携带 PoW 挑战答案。流程：

```
1. 调用 /api/v0/chat/create_pow_challenge 获取挑战
2. 使用 WASM 模块计算答案（DeepSeekHashV1 算法）
3. 将答案组装为特定格式，base64 编码后作为 x-ds-pow-response
```

**WASM 模块缓存**：
- 使用 `wasmtime` 加载 WASM 文件
- 首次加载后缓存到 `_wasm_cache`，避免重复初始化
- 线程安全：使用 `_cache_lock` 保护缓存写入

**WASM 计算过程**：
1. 分配 WASM 内存，写入 challenge 和 prefix（`{salt}_{expire_at}_`）
2. 调用 `wasm_solve()` 函数
3. 从返回地址读取 status（4字节）和 value（8字节）
4. status=0 表示计算失败，否则返回整数答案

**自动 token 刷新**：
- `get_pow_response()` 内部捕获 40003 错误
- 发现 token 无效时调用 `invalidate_token()` 并重试（最多2次）

### 导出方法

| 方法 | 说明 |
|------|------|
| `_get_cached_wasm(wasm_path)` | 获取/缓存 WASM 模块（线程安全） |
| `compute_pow_answer(algorithm, challenge_str, salt, difficulty, expire_at, wasm_path)` | 纯 WASM 计算 PoW 答案 |
| `get_pow_response(target_path)` | 获取完整 PoW 响应（API + 计算），支持 token 自动刷新 |

---

## 5. parent_msg_store.py - 会话状态管理

### 实现原理

DeepSeek API 要求连续对话时传递 `parent_message_id`。本模块维护内存映射表：

```
chat_session_id → parent_message_id
```

**单例模式**：使用双重检查锁定（double-checked locking）确保只创建一个实例。

**异步安全**：使用 `asyncio.Lock` 保护所有读写操作。

**数据流向**：
1. 创建 session → `acreate(session_id)`，parent_message_id = null
2. 收到响应 → `aupdate_parent_message_id(session_id, response_message_id)`
3. 下一轮对话 → `aget_parent_message_id(session_id)` 获取上次的 message_id

### 导出方法

| 方法 | 说明 |
|------|------|
| `get_instance()` | 获取单例（同步，用于模块导入） |
| `aget_instance()` | 获取单例（异步） |
| `acreate(session_id)` | 创建新 session，parent_message_id 初始化为 null |
| `aget_parent_message_id(session_id)` | 获取 session 对应的 parent_message_id |
| `aupdate_parent_message_id(session_id, message_id)` | 更新 parent_message_id |
| `adelete(session_id)` | 删除 session，返回是否存在 |
| `ahas(session_id)` | 检查 session 是否存在 |
| `aget_all()` | 获取所有 session ID 列表 |

> 注意：所有方法都是异步的，使用时需要 `await` 或 `asyncio.run()` 包装。

---

## 开发约定

1. **Core 层** - 只做技术实现，不包含业务逻辑
2. **API/Service 层** - 编排 Core 能力，实现具体业务功能
3. **Routes 层** - 薄路由，只做 HTTP 解析和响应封装
