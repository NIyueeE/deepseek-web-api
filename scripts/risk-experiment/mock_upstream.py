#!/usr/bin/env python3
"""假上游：模拟 DeepSeek 后端的登录 / 建会话 / PoW / completion SSE。

用途：**零账号流量**地验证请求形态（会话复用、增量 prompt、工具注入、身份头等）。
每次 `/api/v0/chat/completion` 的请求体都会追加写入记录文件，供断言或人工比对。

用法：
    python3 mock_upstream.py <port> <记录文件> [--wasm-port-file <path>]

PoW 说明（2026-10-07 实测）：`challenge` 与 `salt`/`expire_at` 之间存在可验证关系，
**伪造的 challenge 永远 no solution**。这里直接复用真实抓包的样例四元组
（`ds_core/raw-api-reference.md` §3），wasm 不校验 `expire_at` 是否过期。
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8098
LOG = sys.argv[2] if len(sys.argv) > 2 else "/tmp/mock_upstream.jsonl"

# 真实下发的样例（salt / expire_at / challenge / signature 必须配套）
REAL_CHALLENGE = {
    "algorithm": "DeepSeekHashV1",
    "challenge": "7ffc9d19b6eed96a6fca68f8ffe30ee61035d4959e4180f187bf85b356016a96",
    "salt": "3bde54628ea8413fee87",
    "signature": "ce4678cf7a1290c2a7ac88c4195a5b8497e5fc4b0e8044e804f5a6f3af6fe462",
    "difficulty": 144000,
    "expire_after": 300000,
    "expire_at": 1775380966945,
}

SESSIONS = {"n": 0}
RESPONSE_MSG_ID = {"n": 1000}


def envelope(biz_data):
    return {
        "code": 0,
        "msg": "",
        "data": {"biz_code": 0, "biz_msg": "", "biz_data": biz_data},
    }


def envelope_null():
    """biz_data = null（delete_session 等返回 unit 的端点）"""
    return {"code": 0, "msg": "", "data": {"biz_code": 0, "biz_msg": "", "biz_data": None}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse_body(self, text="好的"):
        rid = RESPONSE_MSG_ID["n"] = RESPONSE_MSG_ID["n"] + 1
        frames = [
            "event: ready\ndata: "
            + json.dumps({"request_message_id": rid - 1, "response_message_id": rid})
            + "\n\n",
            'event: update_session\ndata: {"p":"response/status","o":"SET","v":"WIP"}\n\n',
            'data: {"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":"'
            + text
            + '"}]}\n\n',
            'data: {"p":"response/status","o":"SET","v":"FINISHED"}\n\n',
            "event: close\ndata: {}\n\n",
        ]
        return rid, "".join(frames).encode()

    def do_GET(self):
        path = self.path.split("?")[0]
        if path.endswith("/client/settings"):
            return self._json(
                envelope(
                    {
                        "version": 88,
                        "settings": {
                            "model_types": [
                                {
                                    "model_type": "default",
                                    "name": "DeepSeek",
                                    "enabled": True,
                                    "switchable": True,
                                    "input_character_limit": 2621440,
                                }
                            ]
                        },
                    }
                )
            )
        return self._json(envelope({}))

    def do_POST(self):
        path = self.path.split("?")[0]
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            body = {"_raw": raw.decode("utf-8", "replace")}

        if path.endswith("/users/login"):
            return self._json(
                envelope(
                    {
                        "code": 0,
                        "msg": "",
                        "user": {
                            "id": "mock-user",
                            "token": "MOCK-TOKEN",
                            "email": body.get("email"),
                            "chat": {"is_muted": 0},
                        },
                    }
                )
            )
        if path.endswith("/users/auth_token/check_device"):
            return self._json(envelope({"rotate": None}))
        if path.endswith("/chat_session/create"):
            SESSIONS["n"] += 1
            return self._json(
                envelope({"chat_session": {"id": f"mock-session-{SESSIONS['n']}"}})
            )
        if path.endswith("/chat_session/delete"):
            return self._json(envelope_null())
        if path.endswith("/chat/create_pow_challenge"):
            challenge = dict(REAL_CHALLENGE)
            challenge["target_path"] = body.get("target_path", "/api/v0/chat/completion")
            return self._json(envelope({"challenge": challenge}))
        if path.endswith("/chat/completion"):
            rid, sse = self._sse_body()
            with open(LOG, "a") as f:
                f.write(
                    json.dumps(
                        {
                            "t": time.strftime("%Y-%m-%dT%H:%M:%S"),
                            "path": path,
                            "headers": {
                                "x-hif-leim": self.headers.get("x-hif-leim"),
                                "x-hif-dliq": self.headers.get("x-hif-dliq"),
                                "x-client-platform": self.headers.get("x-client-platform"),
                                "sec-fetch-dest": self.headers.get("sec-fetch-dest"),
                                "sec-ch-ua-platform": self.headers.get("sec-ch-ua-platform"),
                                "accept": self.headers.get("accept"),
                                "referer": self.headers.get("referer"),
                                "sec-fetch-site": self.headers.get("sec-fetch-site"),
                            },
                            "body": body,
                            "response_message_id": rid,
                        },
                        ensure_ascii=False,
                    )
                    + "\n"
                )
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(sse)))
            self.end_headers()
            self.wfile.write(sse)
            return
        return self._json(envelope({}))


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
