#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""A scripted model server for agent sessions without a real LLM.

It speaks the two wire formats the agents use: Anthropic Messages
(`/v1/messages`, Claude Code and pi) and OpenAI Responses (`/v1/responses`,
Codex). Every request is appended to the JSON-lines log FAKE_LLM_LOG. The
script is fixed: FAKE_LLM_COMMANDS holds one shell command per line; a
request that offers a shell tool and whose conversation carries fewer tool
results than there are commands gets a call running the next command, and
every other request gets the final text FAKE_LLM_DONE. The log is the
evidence: what each agent sent the model (system prompt, hook context, tool
results).
"""
import itertools
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COMMANDS = [c for c in os.environ.get("FAKE_LLM_COMMANDS", "grep -rn helper_1 src").splitlines() if c]
DONE = "FAKE_LLM_DONE"
LOG = os.environ.get("FAKE_LLM_LOG", "/evidence/llm-requests.jsonl")
ids = itertools.count(1)


def anthropic_turn(body):
    """Return (blocks, stop_reason) for one Messages request."""
    step = sum(1 for m in body.get("messages", []) if isinstance(m.get("content"), list)
               for part in m["content"] if part.get("type") == "tool_result")
    shell = next((t for t in body.get("tools", []) if t.get("name") in ("Bash", "bash")), None)
    if shell and step < len(COMMANDS):
        return [{"type": "tool_use", "id": f"toolu_fake_{next(ids)}", "name": shell["name"],
                 "input": {"command": COMMANDS[step]}}], "tool_use"
    return [{"type": "text", "text": DONE}], "end_turn"


def shell_arguments(tool, command):
    """Arguments for whichever shell tool shape this Codex version offers."""
    props = tool.get("parameters", {}).get("properties", {})
    if "cmd" in props:
        return {"cmd": command}
    if props.get("command", {}).get("type") == "array":
        return {"command": ["bash", "-lc", command]}
    return {"command": command}


def responses_turn(body):
    """Return the output items for one Responses request."""
    items = body.get("input", [])
    step = sum(1 for i in items if isinstance(i, dict) and i.get("type") == "function_call_output")
    shell = next((t for t in body.get("tools", [])
                  if t.get("type") == "function"
                  and t.get("name") in ("shell", "exec_command", "shell_command", "local_shell")), None)
    if shell and step < len(COMMANDS):
        return [{"type": "function_call", "id": f"fc_fake_{next(ids)}", "call_id": f"call_fake_{next(ids)}",
                 "name": shell["name"], "arguments": json.dumps(shell_arguments(shell, COMMANDS[step])),
                 "status": "completed"}]
    return [{"type": "message", "id": f"msg_fake_{next(ids)}", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": DONE, "annotations": []}]}]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # the JSON log replaces stderr lines
        pass

    def record(self, body):
        headers = {k.lower(): v for k, v in self.headers.items()
                   if k.lower() not in ("authorization", "x-api-key")}
        with open(LOG, "a") as log:
            log.write(json.dumps({"method": self.command, "path": self.path,
                                  "headers": headers, "body": body}) + "\n")

    def send_json(self, payload, status=200):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_events(self, events):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        for name, payload in events:
            self.wfile.write(f"event: {name}\ndata: {json.dumps(payload)}\n\n".encode())
            self.wfile.flush()
        self.close_connection = True

    def do_GET(self):
        self.record(None)
        if self.path.rstrip("/").endswith("/models"):
            self.send_json({"data": [{"id": "fake-model", "type": "model", "object": "model"}],
                            "object": "list", "has_more": False})
        else:
            self.send_json({})

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw) if raw else {}
        except ValueError:
            body = {"unparsed": raw.decode(errors="replace")}
        self.record(body)
        path = self.path.split("?")[0].rstrip("/")
        if path.endswith("/messages/count_tokens"):
            self.send_json({"input_tokens": 1})
        elif path.endswith("/messages"):
            self.answer_messages(body)
        elif path.endswith("/responses"):
            self.answer_responses(body)
        else:
            self.send_json({"error": {"type": "not_found", "message": path}}, 404)

    def answer_messages(self, body):
        blocks, stop = anthropic_turn(body)
        model = body.get("model", "fake-model")
        usage = {"input_tokens": 1, "output_tokens": 1,
                 "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        message = {"id": f"msg_fake_{next(ids)}", "type": "message", "role": "assistant", "model": model,
                   "content": blocks, "stop_reason": stop, "stop_sequence": None, "usage": usage}
        if not body.get("stream"):
            self.send_json(message)
            return
        events = [("message_start", {"type": "message_start", "message": dict(message, content=[],
                                                                           stop_reason=None)})]
        for index, block in enumerate(blocks):
            if block["type"] == "text":
                start, delta = {"type": "text", "text": ""}, {"type": "text_delta", "text": block["text"]}
            else:
                start = dict(block, input={})
                delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
            events += [("content_block_start", {"type": "content_block_start", "index": index,
                                                "content_block": start}),
                       ("content_block_delta", {"type": "content_block_delta", "index": index, "delta": delta}),
                       ("content_block_stop", {"type": "content_block_stop", "index": index})]
        events += [("message_delta", {"type": "message_delta",
                                      "delta": {"stop_reason": stop, "stop_sequence": None},
                                      "usage": {"output_tokens": 1}}),
                   ("message_stop", {"type": "message_stop"})]
        self.send_events(events)

    def answer_responses(self, body):
        output = responses_turn(body)
        rid = f"resp_fake_{next(ids)}"
        usage = {"input_tokens": 1, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 1,
                 "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 2}
        response = {"id": rid, "object": "response", "status": "completed", "model": body.get("model"),
                    "output": output, "usage": usage}
        if not body.get("stream"):
            self.send_json(response)
            return
        events = [("response.created", {"type": "response.created",
                                        "response": dict(response, status="in_progress", output=[])})]
        for index, item in enumerate(output):
            events.append(("response.output_item.added", {"type": "response.output_item.added",
                                                          "output_index": index, "item": item}))
            if item["type"] == "message":
                events.append(("response.output_text.delta", {"type": "response.output_text.delta",
                                                              "item_id": item["id"], "output_index": index,
                                                              "content_index": 0, "delta": DONE}))
            events.append(("response.output_item.done", {"type": "response.output_item.done",
                                                         "output_index": index, "item": item}))
        events.append(("response.completed", {"type": "response.completed", "response": response}))
        self.send_events(events)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
