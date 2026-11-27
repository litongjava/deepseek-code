#!/usr/bin/env python3
"""Probe a stdio MCP server: initialize, tools/list, optional tools/call.

Standard library only. Example:
  python probe_mcp.py --command D:/bin/aoci.exe --arg mcp
  python probe_mcp.py --command D:/bin/aoci.exe --arg mcp --call aoci_header
"""

import argparse
import json
import queue
import subprocess
import sys
import threading
import time


def reader(stream, events):
    for line in stream:
        events.put(line)
    events.put(None)


def send(proc, message):
    proc.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
    proc.stdin.flush()


def wait_for(events, request_id, timeout):
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"no response for id {request_id} within {timeout}s")
        try:
            line = events.get(timeout=remaining)
        except queue.Empty:
            raise TimeoutError(f"no response for id {request_id} within {timeout}s")
        if line is None:
            raise RuntimeError("server closed stdout")
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            print(f"non-JSON stdout: {line}", file=sys.stderr)
            continue
        if message.get("id") == request_id:
            return message


def text_content(result):
    return "".join(
        part.get("text", "")
        for part in result.get("content", [])
        if isinstance(part, dict)
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--command", required=True, help="server executable")
    parser.add_argument("--arg", action="append", default=[], help="argument; repeatable")
    parser.add_argument("--cwd", help="working directory for the server")
    parser.add_argument("--timeout", type=float, default=10.0, help="per-response timeout")
    parser.add_argument("--call", help="tool name to call after tools/list")
    parser.add_argument("--args", default="{}", help="JSON object for --call")
    options = parser.parse_args()

    command = [options.command, *options.arg]
    proc = subprocess.Popen(
        command,
        cwd=options.cwd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    events = queue.Queue()
    stderr_lines = []
    threading.Thread(target=reader, args=(proc.stdout, events), daemon=True).start()
    threading.Thread(
        target=lambda: stderr_lines.extend(proc.stderr), daemon=True
    ).start()

    try:
        send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "dc-docs-probe", "version": "1"},
                },
            },
        )
        initialized = wait_for(events, 1, options.timeout)
        if "error" in initialized:
            raise RuntimeError(f"initialize failed: {initialized['error']}")
        result = initialized.get("result", {})
        print(f"server: {result.get('serverInfo')}")
        print(f"protocol: {result.get('protocolVersion')}")

        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        listed = wait_for(events, 2, options.timeout)
        if "error" in listed:
            raise RuntimeError(f"tools/list failed: {listed['error']}")
        tools = listed.get("result", {}).get("tools", [])
        print(f"tools: {len(tools)}")
        for tool in tools:
            print(f"  - {tool.get('name')}")

        if options.call:
            arguments = json.loads(options.args)
            send(
                proc,
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": options.call, "arguments": arguments},
                },
            )
            called = wait_for(events, 3, options.timeout)
            if "error" in called:
                raise RuntimeError(f"tools/call failed: {called['error']}")
            result = called.get("result", {})
            print(f"call: {options.call} isError={result.get('isError')}")
            print(text_content(result))
    except (TimeoutError, RuntimeError) as error:
        print(f"probe failed: {error}", file=sys.stderr)
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        if stderr_lines:
            print("server stderr:", file=sys.stderr)
            sys.stderr.write("".join(stderr_lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
