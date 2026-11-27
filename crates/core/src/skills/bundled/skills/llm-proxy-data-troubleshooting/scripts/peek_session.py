#!/usr/bin/env python3
"""Compact timeline of one java-llm-proxy session.

Usage: python peek_session.py <proxy.db> <session_id>
Read-only; never prints headers or credentials.
"""
import json
import sqlite3
import sys


def frames(text):
    for block in (text or "").split("\n\n"):
        event, data = None, []
        for line in block.split("\n"):
            if line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())
        if not data or data == ["[DONE]"]:
            continue
        try:
            yield event, json.loads("\n".join(data))
        except ValueError:
            continue


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 1
    db, session = sys.argv[1], sys.argv[2]
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = con.execute(
        "select id, timestamp, status, model, provider, input_tokens, output_tokens, "
        "reasoning_tokens, body from requests where session_id=? order by timestamp",
        (session,),
    ).fetchall()
    print(f"session {session}: {len(rows)} request(s)")
    for rid, ts, status, model, provider, it, ot, rt, body in rows:
        record = json.loads(body)
        up = record.get("upstreamResponseBody") or ""
        client = record.get("responseBody") or ""
        up_reasoning = up.count('"reasoning_content"')
        client_reasoning = client.count("response.reasoning_summary_text.delta")
        client = client.replace(" ", "")
        empty_message = '"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":""' in client
        print(
            f"  {rid} status={status} model={model} provider={provider} "
            f"in={it} out={ot} reasoning={rt} upstreamThinking={up_reasoning} "
            f"clientThinking={client_reasoning} emptyMessage={empty_message}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
