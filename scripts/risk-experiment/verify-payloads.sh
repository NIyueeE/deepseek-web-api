#!/usr/bin/env bash
# 用假上游验证「请求形态」：零账号流量，不需要任何真实账号。
#
# 校验四个用例（对应 docs/development.md 的验证表）：
#   1. 首轮            → 新建会话、完整 prompt（含角色标签）、无 parent_message_id
#   2. 同一会话的延续   → 复用会话、只发新增用户消息（零标签）、parent = 上轮响应 id
#   3. 带 tools 的请求  → 新建会话、完整 prompt（工具脚手架不能拆）
#   4. 完全重复的请求   → 新建会话（不把历史重复灌进已有会话）
#
# 用法：scripts/risk-experiment/verify-payloads.sh [二进制路径]
# 退出码：0 = 全部符合预期；1 = 有断言失败（会打印假上游收到的原始请求）
set -euo pipefail

BIN="${1:-target/debug/ds-free-api}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
MOCK_PORT=8098
PROXY_PORT=22261

cleanup() {
  for pid in "${PROXY_PID:-}" "${MOCK_PID:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  sleep 1
}
trap cleanup EXIT

[ -x "$ROOT/$BIN" ] || [ -x "$BIN" ] || {
  echo "找不到二进制 $BIN（先 cargo build）" >&2
  exit 2
}
BIN_ABS="$([ -x "$ROOT/$BIN" ] && echo "$ROOT/$BIN" || echo "$BIN")"

cat > "$WORK/config.toml" <<CFG
[server]
host = "127.0.0.1"
port = $PROXY_PORT

[admin]
password_hash = ""
jwt_secret = ""

[ds_core]
api_base = "http://127.0.0.1:$MOCK_PORT/api/v0"
model_types = ["default"]
max_input_tokens = [64000]
max_output_tokens = [8000]
input_character_limits = [2621440]
model_aliases = [""]
hourly_request_quota = 0

[[ds_core.accounts]]
email = "mock@example.com"
mobile = ""
area_code = ""
password = "mock"
device_id = "11111111-2222-4333-8444-555555555555"

[[api_keys]]
key = "sk-mock"
name = "mock"
description = "payload verification"
CFG

python3 "$ROOT/scripts/risk-experiment/mock_upstream.py" "$MOCK_PORT" "$WORK/upstream.jsonl" \
  > "$WORK/mock.log" 2>&1 &
MOCK_PID=$!
sleep 1

DS_DATA_DIR="$WORK" "$BIN_ABS" -c "$WORK/config.toml" > "$WORK/server.log" 2>&1 &
PROXY_PID=$!

for _ in $(seq 1 40); do
  curl -sf --max-time 2 "http://127.0.0.1:$PROXY_PORT/health" >/dev/null && break
  sleep 0.5
done
curl -sf --max-time 3 "http://127.0.0.1:$PROXY_PORT/health" >/dev/null || {
  echo "代理未启动，日志：$WORK/server.log" >&2
  tail -20 "$WORK/server.log" >&2
  exit 2
}

post() {
  curl -s --max-time 90 -X POST "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
    -H "Authorization: Bearer sk-mock" -H "Content-Type: application/json" -d "$1" >/dev/null
}

post '{"model":"deepseek-default","messages":[{"role":"user","content":"第一轮问题"}],"stream":true}'
post '{"model":"deepseek-default","messages":[{"role":"user","content":"第一轮问题"},{"role":"assistant","content":"好的"},{"role":"user","content":"第二轮问题"}],"stream":true}'
post '{"model":"deepseek-default","messages":[{"role":"user","content":"查天气"}],"tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{}}}}],"stream":true}'
post '{"model":"deepseek-default","messages":[{"role":"user","content":"第一轮问题"},{"role":"assistant","content":"好的"},{"role":"user","content":"第二轮问题"}],"stream":true}'

python3 - "$WORK/upstream.jsonl" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    recs = [json.loads(line) for line in open(path)]
except FileNotFoundError:
    print("❌ 假上游没有收到任何 completion 请求")
    sys.exit(1)

if len(recs) != 4:
    print(f"❌ 期望 4 次 completion，实际 {len(recs)} 次")
    sys.exit(1)

first, second, tools, replay = (r["body"] for r in recs)
fails = []


def check(label, cond, detail=""):
    print(f"{'✅' if cond else '❌'} {label}{'' if cond else '  ' + detail}")
    if not cond:
        fails.append(label)


check(
    "首轮：完整 prompt 含角色标签、无 parent_message_id",
    first.get("prompt", "").count("<｜") >= 3 and "parent_message_id" not in first,
    f"prompt={first.get('prompt', '')[:60]!r} parent={first.get('parent_message_id')}",
)
check(
    "延续：复用同一会话",
    second.get("chat_session_id") == first.get("chat_session_id"),
    f"{first.get('chat_session_id')} -> {second.get('chat_session_id')}",
)
check(
    "延续：parent_message_id 指向上轮响应",
    second.get("parent_message_id") == recs[0]["response_message_id"],
    f"{second.get('parent_message_id')} != {recs[0]['response_message_id']}",
)
check(
    "延续：只发新增用户消息且零标签",
    second.get("prompt") == "第二轮问题",
    f"prompt={second.get('prompt', '')[:60]!r}",
)
check(
    "带工具：新建会话 + 完整 prompt（工具脚手架保留）",
    tools.get("chat_session_id") not in (None, first.get("chat_session_id"))
    and tools.get("prompt", "").count("<｜") >= 3,
    f"session={tools.get('chat_session_id')} tags={tools.get('prompt', '').count('<｜')}",
)
check(
    "重复请求：新建会话（不重复灌历史）",
    replay.get("chat_session_id") not in (None, second.get("chat_session_id")),
    f"session={replay.get('chat_session_id')}",
)
check(
    "所有请求都带 source=input 与风控令牌头",
    all(r["body"].get("source") == "input" for r in recs)
    and all(r["headers"].get("x-hif-leim") for r in recs),
)

if fails:
    print("\n假上游收到的原始请求：")
    for i, r in enumerate(recs, 1):
        b = r["body"]
        print(f"  {i}: session={b.get('chat_session_id')} parent={b.get('parent_message_id')} "
              f"prompt={b.get('prompt', '')[:70]!r}")
    sys.exit(1)
print("\n✅ 四个用例全部符合预期")
PY
