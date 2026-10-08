# 风控对照实验工具

拿到**全新账号**后按这里的步骤跑最小流量验证。工具本身不产生任何账号流量
（除 `probe-account.sh`，它明确只发 3 次请求）。

## 背景与前提（先读）

- 三个老账号（`l3366599051@163.com` / `1460183479@qq.com` / `n1yu3@proton.me`）已全部
  `biz_code=10 USER_IS_BANNED`，**有违规历史的账号不能用于验证**；
- 账号必须**全新**、无违规历史；每个账号用自己的 `device_id`（数美指纹）；
- 复查频率 ≤ 3 次/天，且**只用登录接口**；不要用对话请求探活；
- 判断账号状态只认登录响应的 `is_muted` / `biz_code` / `mute_until`，
  **不要**依赖浏览器 localStorage（会给出假阴性）；
- 观察窗口 ≥ 24h；封禁是**延迟**的（历史观测：+19min / +37min / ~2.5h / ≥6h）。

## 步骤

### 0. 零成本自检（不需要账号）

```bash
cargo build
bash scripts/risk-experiment/verify-payloads.sh target/debug/ds-free-api
```

用假上游校验请求形态：新建→复用→工具→重复四个用例，断言会打印每一条结论。
**改动请求链路后应先跑这个**，再考虑用真实账号。

### 1. 注册设备指纹（真实浏览器，一次）

用官方客户端登录一次并抓包（`cap/capture.js` 输出 `capture-*.json`），然后：

```bash
scripts/risk-experiment/extract-device.py capture-新账号.json <email>
```

把输出的 `[[ds_core.accounts]]` 块粘进 `config.toml`（填上密码）。
`X-Device-Id` 由该 `device_id` 自动派生，不用手填。

### 2. 最小流量冒烟（3 次请求）

```bash
scripts/risk-experiment/probe-account.sh -c config.toml            # 真实账号
scripts/risk-experiment/probe-account.sh -c config.toml --dry-run  # 只看计划
```

依次请求 OpenAI / Anthropic / Responses 三个流式端点并校验终止事件
（`data: [DONE]` / `message_stop` / `response.completed`），最后打印复查计划。

### 3. 低频复查

```bash
scripts/risk-experiment/check-plan.sh -c config.toml --email <email>
cargo run --example account_check -- -c config.toml <email>   # 每次 1 次登录
```

判读：`is_muted=0` 正常；`biz_code=5` + `mute_until` 被禁言；`biz_code=10` 已被停用
（登录即被拒，没有解封时间可读）。

## 对照组怎么设

| 组 | 账号 | 用法 |
|----|------|------|
| A | 全新账号 1 | 只跑本代理（步骤 2 的 3 次请求），之后停服，只做登录复查 |
| B | 全新账号 2 | 只用官方浏览器做**同等强度**使用（1~3 条消息），同样只做登录复查 |

只有「A 与 B 在 24h 内都没被处罚」时，才能说本代理的封号风险与官方客户端接近；
A 正常而 B 也正常 ≠ 证明代理不可检测，只说明这一档流量下未被处罚。

## 文件说明

| 文件 | 作用 |
|------|------|
| `mock_upstream.py` | 假 DeepSeek 后端（登录 / 建会话 / PoW / completion SSE），记录每次 completion 的请求体与关键头 |
| `verify-payloads.sh` | 用假上游断言四个请求形态用例（零账号流量） |
| `probe-account.sh` | 真实账号最小流量冒烟（3 次请求）+ 打印复查计划 |
| `check-plan.sh` | 打印 +30min / +2h / +6h / +24h 复查时间与命令 |
| `extract-device.py` | 从浏览器抓包提取数美 `device_id`，生成账号配置块 |

> PoW 注意：假上游必须复用**真实抓包的 challenge 四元组**
> （`salt` / `expire_at` / `challenge` / `signature` 配套）。实测伪造的 challenge
> 永远 `no solution` —— 详见 `ds_core/raw-api-reference.md` §3。
