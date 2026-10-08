#!/usr/bin/env bash
# 真实账号「最小流量」冒烟 + 低频复查计划
#
# 设计原则（见 docs/development.md 的教训）：
#   * 一次只用一个账号；
#   * 请求数压到最低：1 次 OpenAI 流式 + 1 次 Anthropic 流式 + 1 次 Responses（共 3 次）；
#   * 复查用登录接口（`account_check`，1 次登录/次），频率 ≤ 3 次/天，
#     不要用「20 分钟登录 3 次」这种异常行为；
#   * 账号可用性判定只认登录响应里的 `is_muted` / `biz_code`，不要依赖浏览器
#     localStorage（会给出假阴性）。
#
# 用法：
#   scripts/risk-experiment/probe-account.sh [-c config.toml] [-b 二进制] [-k api_key]
#                                            [--base http://127.0.0.1:PORT] [--dry-run]
#
#   --dry-run 只打印将要发出的请求与复查计划，不发任何请求（先用它自检）。
#
# 退出码：0 = 三个协议都通过；1 = 有失败；2 = 环境/参数问题。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG=""
BIN="target/debug/ds-free-api"
KEY=""
BASE=""
DRY_RUN=0

EMAIL=""
while [ $# -gt 0 ]; do
  case "$1" in
    -c) CONFIG="$2"; shift 2 ;;
    -b) BIN="$2"; shift 2 ;;
    -k) KEY="$2"; shift 2 ;;
    --email) EMAIL="$2"; shift 2 ;;
    --base) BASE="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

# ── 取配置里的第一个 api_key 与端口（只读，不打印密钥）──────────────────
if [ -n "$CONFIG" ]; then
  [ -f "$CONFIG" ] || { echo "找不到配置 $CONFIG" >&2; exit 2; }
  if [ -z "$KEY" ]; then
    KEY="$(python3 - "$CONFIG" <<'PY'
import re, sys
txt = open(sys.argv[1]).read()
m = re.search(r'\[\[api_keys\]\](.*?)(?=\n\[|\Z)', txt, re.S)
print(re.search(r'key\s*=\s*"([^"]+)"', m.group(1)).group(1) if m and re.search(r'key\s*=\s*"([^"]+)"', m.group(1)) else "")
PY
)"
  fi
  if [ -z "$EMAIL" ]; then
    EMAIL="$(sed -n 's/^email[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$CONFIG" | head -1)"
  fi
  if [ -z "$BASE" ]; then
    PORT="$(python3 - "$CONFIG" <<'PY'
import re, sys
txt = open(sys.argv[1]).read()
m = re.search(r'^port\s*=\s*(\d+)', txt, re.M)
print(m.group(1) if m else "22217")
PY
)"
    BASE="http://127.0.0.1:$PORT"
  fi
fi
[ -n "$BASE" ] || { echo "需要 --base 或 -c <config>" >&2; exit 2; }
[ -n "$KEY" ] || { echo "配置里没有 api_key，请用 -k 指定" >&2; exit 2; }

OPENAI_BODY='{"model":"deepseek-default","messages":[{"role":"user","content":"用一句话说明你是什么模型"}],"stream":true,"max_tokens":64}'
ANTHROPIC_BODY='{"model":"deepseek-default","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"用一句话说明你是什么模型"}]}'
RESPONSES_BODY='{"model":"deepseek-default","input":"用一句话说明你是什么模型","stream":true,"max_output_tokens":64}'

echo "== 代理地址: $BASE"
echo "== 本次将发出 3 次请求（1 OpenAI 流式 / 1 Anthropic 流式 / 1 Responses 流式）"
if [ "$DRY_RUN" = 1 ]; then
  echo "   (dry-run：不实际发送)"
  echo "   POST $BASE/v1/chat/completions"
  echo "   POST $BASE/anthropic/v1/messages"
  echo "   POST $BASE/v1/responses"
  echo
  echo "== 复查计划（登录接口，1 次/账号/次）"
  plan_args=()
  [ -n "$CONFIG" ] && plan_args+=(-c "$CONFIG")
  [ -n "$EMAIL" ] && plan_args+=(--email "$EMAIL")
  bash "$ROOT/scripts/risk-experiment/check-plan.sh" "${plan_args[@]+"${plan_args[@]}"}"
  exit 0
fi

fail=0
run() {
  local name="$1" url="$2" body="$3" expect="$4"
  local out
  out="$(curl -s --max-time 120 -X POST "$url" \
      -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
      -d "$body" || true)"
  if printf '%s' "$out" | grep -q "$expect"; then
    echo "✅ $name（收到 $expect）"
  else
    echo "❌ $name（未收到 $expect）"
    printf '%s\n' "$out" | head -5 | sed 's/^/     /'
    fail=1
  fi
}

run "OpenAI 流式" "$BASE/v1/chat/completions" "$OPENAI_BODY" "data: \[DONE\]"
run "Anthropic 流式" "$BASE/anthropic/v1/messages" "$ANTHROPIC_BODY" "message_stop"
run "Responses 流式" "$BASE/v1/responses" "$RESPONSES_BODY" "response.completed"

echo
echo "== 复查计划（登录接口，1 次/账号/次；请勿加密频率）"
plan_args=()
[ -n "$CONFIG" ] && plan_args+=(-c "$CONFIG")
[ -n "$EMAIL" ] && plan_args+=(--email "$EMAIL")
bash "$ROOT/scripts/risk-experiment/check-plan.sh" "${plan_args[@]+"${plan_args[@]}"}"

[ "$fail" = 0 ] || exit 1
echo
echo "✅ 三个协议均正常。接下来只做低频复查，不要为「确认账号还活着」额外发对话请求。"
