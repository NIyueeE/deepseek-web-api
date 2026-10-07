# Testing Reproduction Guide

本文件描述如何**可复现地**验证「本代理是否触发上游风控」。
它是 2026-09-18 首版 runbook 的修订版：原版只比较「请求数 / 间隔」，
2026-10-07 的实测证明那样得不出结论，因此加入了**对照组**与**低频复查**两条硬要求。

## 结论先行（2026-10-07 实测）

| 客户端 | 流量 | 结果 |
|--------|------|------|
| 旧版（无 `x-hif-leim`） | 初始化 + 1 次对话 + 3 次登录复查 | ❌ +19min `USER_IS_BANNED` |
| 含 `x-hif-leim` + 按账号设备身份 | 初始化 + 1 次对话 | ❌ +37min `USER_IS_BANNED` |
| **仅官方浏览器（对照组）** | 登录 + 2 条消息，之后只打开页面 | ✅ **+3h 仍正常** |

**要点**：

1. **低请求量不能防止风控**（每账号 1~2 次请求一样会被停用）；
2. **必须有对照组**：同一天、同 IP、同类账号，只走官方浏览器 ——
   否则无法区分「账号历史处罚」与「客户端被识别」；
3. **不要高频登录复查**：账号 A 在 20 分钟内被登录复查 3 次，这本身不是正常用户行为，
   可能参与触发。复查优先用**官方浏览器打开页面**读取状态；
4. **已被处罚过的账号不能作为验证对象**（存在阶梯升级处罚：
   1 天 → 3 天 → 8 天 → 永久）。要验证请用**全新账号**。

## 前置条件

1. **2 个全新账号**（A 组跑本代理、B 组只用官方浏览器作对照）；
2. 每个账号各自的 `device_id`：用**独立浏览器配置文件**登录一次
   `https://chat.deepseek.com/sign_in`，从 `POST /api/v0/users/login` 请求体里复制
   （`device_id` 不能伪造；缺失会被 `RISK_DEVICE_DETECTED` 拒绝）；
3. 服务端：本仓库构建的二进制（**用发布产物更有意义**）。

## 配置

以 `config.example.testing.toml` 为起点：

```bash
cp config.example.testing.toml config.toml
# 填 A 组账号（独立 device_id）；hourly_request_quota = 10；emulation 用默认 okhttp4_12
```

A 组建议只放**一个**账号：这样「账号被封」与「客户端被识别」不会互相混淆。

## 步骤

### 1. 先探不需要账号的两项（零账号流量）

```bash
cargo run --example hif_probe                       # x-hif-leim 端点是否可达
cargo run -p ds_core --example identity_probe       # 身份/WAF 兼容性
```

### 2. 启动服务并确认初始化

```bash
RUST_LOG=info,ds_core::client=debug,ds_core::accounts=debug just serve
```

日志里应当能看到：

```
DEBUG ds_core::client  hif token refreshed: url=https://hif-leim.deepseek.com/query, ttl=570s
DEBUG ds_core::client  登录响应: code=0, … muted=Some(0)
DEBUG ds_core::client  attach x-hif-leim (73 chars)
INFO  ds_core::accounts Account … initialized successfully
```

### 3. 发 1~2 次真实请求

```bash
curl -s http://127.0.0.1:22217/v1/chat/completions \
  -H "Authorization: Bearer <你的 API Key>" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-default","messages":[{"role":"user","content":"你好，用一句话介绍杭州"}]}'
```

然后**停掉服务**（避免后台恢复任务产生额外请求）。

### 4. 对照组（B 组）

同一天用官方浏览器登录 B 组账号，发 2 条消息，之后只打开页面。

### 5. 低频复查（关键）

| 时间点 | A 组 | B 组 |
|--------|------|------|
| +30min | 浏览器打开 `chat.deepseek.com`，读账号状态 | 同 |
| +2h | 同 | 同 |
| +6h / +24h | 同 | 同 |

- 优先**只用浏览器打开页面**（`localStorage` 里
  `__appKit_@deepseek/chat_lastSessionValue` 的 `userIsMuted` 即状态，
  页面被跳转到 `sign_in` 通常意味着会话失效/账号异常）；
- 需要确认时，用浏览器**重新登录一次**即可看到官方提示
  （如「由于违规次数过多，你的账户已被临时停用」）——这比 API 登录更接近正常行为；
- **一天最多 3 次复查**。

## 需要记录的数据

- 每次请求的时间戳（UTC）与结果；
- 复查时间点、`is_muted` / 官方提示原文；
- 服务端日志中的 `hif token refreshed` / `attach x-hif-leim` / `muted=Some(…)`；
- A 组与 B 组的**差异**（这才是结论来源）。

## 预期日志（异常时）

```
WARN  ds_core::accounts Account X is muted until Some(…) (detected at login)
ERROR ds_core::accounts Account X 登录被终止性拒绝，已标记 Invalid 并停止重试: …
```

> 新版本对 `biz_code=2/5/10/11` 这类**终止性**错误会立即置 `Invalid` 并停止重试 ——
> 继续重试不会加速解禁，反而可能延长。

## Reporting Template

```markdown
## 环境
- 版本 / 部署方式：
- 代理：
- 账号数：A 组 __ 个（每个独立 device_id？是/否），B 组 __ 个

## A 组（本代理）
| 时间(UTC) | 动作 | 结果 |
|---|---|---|
| | 初始化 | |
| | 第 1 次请求 | |
| +30min | 浏览器复查 | |
| +2h | 浏览器复查 | |

## B 组（仅官方浏览器，对照）
| 时间(UTC) | 动作 | 结果 |
|---|---|---|

## 结论
- A/B 差异：
- 官方提示原文（如有）：
- 服务端日志片段（脱敏）：
```
