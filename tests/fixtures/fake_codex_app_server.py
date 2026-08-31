#!/usr/bin/env python3
from __future__ import annotations

import json
import sys


thread = {"id": "thread-origin", "status": {"type": "idle"}, "turns": []}
version_label = sys.argv[1] if len(sys.argv) > 1 else "0.150.1"
missing_thread_read = len(sys.argv) > 2 and sys.argv[2] == "no-thread-read"


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if request_id is None:
        continue
    if method == "initialize":
        result = {"userAgent": f"codex-cli/{version_label}", "platformFamily": "unix"}
    elif method == "thread/resume":
        result = {"thread": thread}
    elif method == "thread/read" and not missing_thread_read:
        result = {"thread": thread}
    elif method == "turn/start":
        turn = {
            "id": "turn-fake",
            "status": "inProgress",
            "items": [{"type": "userMessage", "content": params["input"]}],
        }
        thread["status"] = {"type": "active"}
        thread["turns"].append(turn)
        result = {"turn": turn}
    elif method == "turn/steer":
        thread["turns"][-1]["items"].append(
            {"type": "userMessage", "content": params["input"]}
        )
        result = {"turnId": params["expectedTurnId"]}
    else:
        print(json.dumps({"id": request_id, "error": {"code": -32601, "message": method}}), flush=True)
        continue
    print(json.dumps({"id": request_id, "result": result}), flush=True)
