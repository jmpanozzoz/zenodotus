#!/usr/bin/env python3
"""A stand-in for the programs the evaluation harness launches, used by its tests.

- With a `scrape` or `add` argument it acts as an indexing command: it logs its arguments and exits with
  STUB_PREPARE_EXIT (default 0), printing "stub prepare failed" to stderr when that is not 0. A `scrape` prints
  "Successfully scraped N pages" with N from STUB_SCRAPED_PAGES (default 2); the value "none" prints nothing.
- With a `serve` or `mcp` argument it is an MCP server on stdio. `tools/call` logs the call, then answers with the
  text of $STUB_DIR/<tool>.txt and the JSON of $STUB_DIR/<tool>.json when they exist. An argument value "fail"
  makes the call return a tool error; "crash" makes the server exit without answering. `tools/list` lists the comma-separated
  names in STUB_TOOLS, by default `resolve_library` and `query_docs`; STUB_TOOLS=error makes it fail.
- Anything else exits with status 2.

Every log line goes to $STUB_DIR/calls.log as JSON; without STUB_DIR nothing is logged and no response is canned.
"""

import json
import os
import sys

STUB_DIR = os.environ.get("STUB_DIR")


def log(entry):
    if STUB_DIR is None:
        return
    with open(os.path.join(STUB_DIR, "calls.log"), "a", encoding="utf-8") as calls:
        calls.write(json.dumps(entry) + "\n")


def read(name):
    if STUB_DIR is None:
        return None
    path = os.path.join(STUB_DIR, name)
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as file:
        return file.read()


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def call_tool(params):
    name = params["name"]
    arguments = params.get("arguments") or {}
    log({"tool": name, "arguments": arguments, "env": {k: v for k, v in os.environ.items() if k.startswith("STUB_")}})
    values = list(arguments.values())
    if "crash" in values:
        sys.exit(1)
    if "fail" in values:
        return {"content": [{"type": "text", "text": "stub failure"}], "isError": True}
    result = {"content": [{"type": "text", "text": read(name + ".txt") or ""}]}
    structured = read(name + ".json")
    if structured is not None:
        result["structuredContent"] = json.loads(structured)
    return result


def serve():
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if "id" not in message:
            continue
        if method == "initialize":
            result = {
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stub", "version": "0"},
            }
        elif method == "tools/list" and os.environ.get("STUB_TOOLS") == "error":
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": "stub listing failure"}})
            continue
        elif method == "tools/list":
            names = os.environ.get("STUB_TOOLS", "resolve_library,query_docs").split(",")
            result = {"tools": [{"name": name, "inputSchema": {"type": "object"}} for name in names]}
        elif method == "tools/call":
            result = call_tool(message["params"])
        else:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": method}})
            continue
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})


def main():
    arguments = sys.argv[1:]
    if "scrape" in arguments or "add" in arguments:
        log({"argv": arguments, "env": {k: v for k, v in os.environ.items() if k in ("HOME", "npm_config_cache") or k.startswith("DOCS_MCP_")}})
        pages = os.environ.get("STUB_SCRAPED_PAGES", "2")
        if "scrape" in arguments and pages != "none":
            sys.stdout.write(f"Successfully scraped {pages} pages\n")
        code = int(os.environ.get("STUB_PREPARE_EXIT", "0"))
        if code:
            sys.stderr.write("stub prepare failed\n")
        sys.exit(code)
    if "serve" in arguments or "mcp" in arguments:
        log({"argv": arguments})
        serve()
        return
    sys.exit(2)


main()
