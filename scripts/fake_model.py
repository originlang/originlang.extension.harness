#!/usr/bin/env python3
"""A stand-in chat-completions endpoint for end-to-end tests.

It answers in two deterministic shapes so the loop can be exercised without a
real model or a network:

1. First request: a single `add` tool call.
2. Every request after a `tool` message: a final prose answer.

That is enough to prove the three things a unit test cannot: the HTTP transport
works, the model's tool call is dispatched through the host, and the observation
comes back into the conversation.

    python scripts/fake_model.py --port 8765
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

REQUESTS = {"count": 0}


def tool_call_response(model: str) -> dict:
    return {
        "id": "chatcmpl-fake-1",
        "object": "chat.completion",
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": "call_add_1",
                            "type": "function",
                            "function": {
                                "name": "add",
                                "arguments": json.dumps({"a": 2, "b": 3}),
                            },
                        }
                    ],
                },
                "finish_reason": "tool_calls",
            }
        ],
        "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20},
    }


def answer_response(model: str, observation: str) -> dict:
    return {
        "id": "chatcmpl-fake-2",
        "object": "chat.completion",
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": f"The tool said {observation}, so the answer is 5.",
                },
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 30, "completion_tokens": 12, "total_tokens": 42},
    }


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 - required by BaseHTTPRequestHandler
        if self.path == "/health":
            self._send({"status": "ok", "requests": REQUESTS["count"]})
        else:
            self._send({"error": "not found"}, status=404)

    def do_POST(self) -> None:  # noqa: N802 - required by BaseHTTPRequestHandler
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            self._send({"error": {"message": "malformed JSON"}}, status=400)
            return

        REQUESTS["count"] += 1
        model = body.get("model", "fake")
        messages = body.get("messages", [])

        observation = ""
        for message in messages:
            if message.get("role") == "tool":
                observation = str(message.get("content", ""))
        # A tool observation anywhere in the history means the loop has already
        # dispatched, so the next turn must be the answer.
        payload = answer_response(model, observation) if observation else tool_call_response(model)
        print(
            f"[fake] request {REQUESTS['count']}: "
            f"{'answer' if observation else 'tool_call'}",
            flush=True,
        )
        self._send(payload)

    def _send(self, payload: dict, status: int = 200) -> None:
        data = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args) -> None:
        """Silences the default per-request log line."""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    server = HTTPServer((args.host, args.port), Handler)
    print(f"[fake] listening on http://{args.host}:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
