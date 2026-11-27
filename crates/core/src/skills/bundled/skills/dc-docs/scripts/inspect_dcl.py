#!/usr/bin/env python3
"""Inspect DeepSeek Code (dc / dcl / dcd) sessions, stream-json runs, and LLM requests.

Standard library only. Resolves DATA_DIR by: --data-dir > DEEPSEEK_CODE_DATA_DIR > ~/.deepseek-code.

Examples:
  python inspect_dcl.py session <session-id-or-session.db>
  python inspect_dcl.py stream run.jsonl --tail 15
  python inspect_dcl.py requests <session-id> --show-tools 1
  python inspect_dcl.py requests <session-id> --prefix-diff 1 2
"""

from collections import Counter, defaultdict
from datetime import datetime
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import sys
import tempfile

CD_RE = re.compile(r"(?:^|[;&|]\s*)cd\s+([^\s;&|]+)")
SLEEP_RE = re.compile(r"\bsleep\b")
READ_VIA_BASH_TOKENS = ("sed -n", "grep ", "cat ", "head ", "tail ")


def resolve_data_dir(explicit=None):
    if explicit:
        return Path(explicit).expanduser()
    env = os.environ.get("DEEPSEEK_CODE_DATA_DIR")
    if env:
        return Path(env).expanduser()
    return Path.home() / ".deepseek-code"


def parse_iso_seconds(start_text, end_text):
    if not start_text or not end_text:
        return None
    try:
        s = datetime.fromisoformat(start_text.replace("Z", "+00:00"))
        e = datetime.fromisoformat(end_text.replace("Z", "+00:00"))
        return (e - s).total_seconds()
    except Exception:
        return None


def open_sqlite_readonly(db_path):
    """Open SQLite safely without contending with a live dcl writer."""
    db_path = Path(db_path)
    tmp_dir = tempfile.mkdtemp(prefix="inspect-dcl-")
    copied = Path(tmp_dir) / db_path.name
    shutil.copy2(db_path, copied)
    for suffix in ("-wal", "-shm"):
        side = Path(str(db_path) + suffix)
        if side.exists():
            shutil.copy2(side, Path(str(copied) + suffix))
    conn = sqlite3.connect(f"file:{copied}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn, tmp_dir


def classify_bash_commands(cmds):
    """cmds: list of (cmd_str, duration_ms, is_error, is_bg, has_cwd)."""
    cd_count = 0
    redundant_cd = 0
    prev_target = None
    cd_targets = Counter()
    sleep_count = 0
    sleep_ms = 0
    chained = 0
    read_via_bash = 0
    fg = 0
    bg = 0
    errors = 0
    with_cwd = 0

    for cmd, dur_ms, is_err, is_bg, has_cwd in cmds:
        if is_bg:
            bg += 1
        else:
            fg += 1
        if is_err:
            errors += 1
        if has_cwd:
            with_cwd += 1
        if any(op in cmd for op in ("&&", ";", "\n")):
            chained += 1
        if SLEEP_RE.search(cmd):
            sleep_count += 1
            sleep_ms += dur_ms or 0
        if (
            any(tok in cmd for tok in READ_VIA_BASH_TOKENS)
            and "cargo " not in cmd
            and "sleep " not in cmd
            and "git " not in cmd
        ):
            read_via_bash += 1
        match = CD_RE.search(cmd)
        if match:
            cd_count += 1
            target = match.group(1)
            cd_targets[target] += 1
            if target == prev_target:
                redundant_cd += 1
            prev_target = target

    return {
        "total": len(cmds),
        "fg": fg,
        "bg": bg,
        "errors": errors,
        "with_cwd": with_cwd,
        "cd_calls": cd_count,
        "redundant_same_cd": redundant_cd,
        "top_cd_targets": cd_targets.most_common(5),
        "sleep_calls": sleep_count,
        "sleep_seconds": round(sleep_ms / 1000.0, 1),
        "chained_calls": chained,
        "read_grep_via_bash": read_via_bash,
    }


def max_consecutive_run(names, target="Bash"):
    best = 0
    cur = 0
    for name in names:
        if name == target:
            cur += 1
            if cur > best:
                best = cur
        else:
            cur = 0
    return best


def cmd_session(args):
    data_dir = resolve_data_dir(args.data_dir)
    target = Path(args.target).expanduser()
    if target.is_file():
        db_path = target
        session_id = target.parent.name
    else:
        session_id = args.target
        db_path = data_dir / "data" / "sessions" / session_id / "session.db"

    if not db_path.is_file():
        print(f"Error: session database not found: {db_path}", file=sys.stderr)
        return 1

    conn, tmp_dir = open_sqlite_readonly(db_path)
    try:
        cur = conn.cursor()
        reqs = [dict(r) for r in cur.execute("SELECT * FROM model_requests ORDER BY rowid")]
        tcs = [
            dict(r)
            for r in cur.execute(
                """
                SELECT tc.*, tt.duration_ms AS tc_duration_ms
                FROM tool_calls tc
                LEFT JOIN tool_call_timings tt ON tc.id = tt.tool_call_id
                ORDER BY tc.rowid
                """
            )
        ]
        msgs = [
            dict(m)
            for m in cur.execute(
                "SELECT rowid, id, role, created_at, length(content) AS len, substr(content, 1, 100) AS preview FROM messages ORDER BY rowid"
            )
        ]
    finally:
        conn.close()
        shutil.rmtree(tmp_dir, ignore_errors=True)

    tot_model_ms = sum((r.get("total_duration_ms") or 0) for r in reqs)
    tot_ttft_ms = sum((r.get("ttft_ms") or 0) for r in reqs)
    tot_gen_ms = sum((r.get("generation_ms") or 0) for r in reqs)
    tot_tc_ms = sum((t.get("tc_duration_ms") or 0) for t in tcs)

    wall_s = None
    if reqs and reqs[0].get("started_at_ms") and reqs[-1].get("ended_at_ms"):
        wall_s = (reqs[-1]["ended_at_ms"] - reqs[0]["started_at_ms"]) / 1000.0

    tot_in = sum((r.get("input_tokens") or 0) for r in reqs)
    tot_cache = sum((r.get("cache_read_tokens") or 0) for r in reqs)
    tot_out = sum((r.get("output_tokens") or 0) for r in reqs)
    tot_reason = sum((r.get("reasoning_tokens") or 0) for r in reqs)

    cache_misses = [
        {
            "request_number": r.get("request_number"),
            "input_tokens": r.get("input_tokens"),
            "cache_read_tokens": r.get("cache_read_tokens"),
            "ttft_ms": r.get("ttft_ms"),
            "total_duration_ms": r.get("total_duration_ms"),
        }
        for r in reqs
        if (r.get("input_tokens") or 0) > 10000
        and (r.get("cache_read_tokens") or 0) < (r.get("input_tokens") or 0) * 0.5
    ]

    # Per-request tool call counts
    tc_counts = Counter()
    single_bash_reqs = 0
    single_bash_model_ms = 0
    for r in reqs:
        try:
            blocks = json.loads(r.get("response_blocks_json") or "[]")
        except Exception:
            blocks = []
        names = [b.get("name") for b in blocks if isinstance(b, dict) and b.get("type") == "tool_call"]
        tc_counts[len(names)] += 1
        if len(names) == 1 and names[0] == "Bash":
            single_bash_reqs += 1
            single_bash_model_ms += r.get("total_duration_ms") or 0

    tool_counts = Counter(t["name"] for t in tcs)
    tool_dur = defaultdict(int)
    for t in tcs:
        tool_dur[t["name"]] += t.get("tc_duration_ms") or 0

    bash_rows = []
    for t in tcs:
        if t["name"] != "Bash":
            continue
        try:
            inp = json.loads(t.get("input") or "{}")
        except Exception:
            inp = {}
        cmd = inp.get("command") or ""
        out = t.get("output") or ""
        has_cwd = '"cwd"' in out or "cwd:" in out
        is_err = bool(t.get("is_error")) or '"success":false' in out
        bash_rows.append((cmd, t.get("tc_duration_ms") or 0, is_err, bool(inp.get("run_in_background")), has_cwd))

    bash_stats = classify_bash_commands(bash_rows)
    bash_stats["max_consecutive_bash"] = max_consecutive_run([t["name"] for t in tcs], "Bash")

    cwd_file = data_dir / "state" / "shell-cwd" / session_id
    saved_cwd = cwd_file.read_text(encoding="utf-8", errors="replace").strip() if cwd_file.is_file() else None

    report = {
        "session_id": session_id,
        "db_path": str(db_path),
        "saved_shell_cwd": saved_cwd,
        "messages": len(msgs),
        "model_requests": len(reqs),
        "tool_calls": len(tcs),
        "models": dict(Counter(r.get("model") for r in reqs)),
        "efforts": dict(Counter(str(r.get("effort")) for r in reqs)),
        "timing": {
            "wall_clock_s": round(wall_s, 1) if wall_s is not None else None,
            "model_total_s": round(tot_model_ms / 1000.0, 1),
            "ttft_total_s": round(tot_ttft_ms / 1000.0, 1),
            "generation_total_s": round(tot_gen_ms / 1000.0, 1),
            "tool_total_s": round(tot_tc_ms / 1000.0, 1),
            "avg_model_s": round((tot_model_ms / max(1, len(reqs))) / 1000.0, 2),
        },
        "tokens": {
            "input": tot_in,
            "cache_read": tot_cache,
            "cache_hit_pct": round(100.0 * tot_cache / max(1, tot_in), 1),
            "output": tot_out,
            "reasoning": tot_reason,
            "reasoning_pct": round(100.0 * tot_reason / max(1, tot_out), 1),
            "cache_miss_spikes": len(cache_misses),
        },
        "batching": {
            "tools_per_request": dict(tc_counts.most_common()),
            "single_tool_requests": tc_counts.get(1, 0),
            "single_bash_requests": single_bash_reqs,
            "single_bash_model_s": round(single_bash_model_ms / 1000.0, 1),
        },
        "tools": {
            name: {
                "count": cnt,
                "total_s": round(tool_dur[name] / 1000.0, 1),
                "avg_ms": round(tool_dur[name] / cnt),
            }
            for name, cnt in tool_counts.most_common()
        },
        "bash": bash_stats,
    }

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0

    print(f"=== Session {session_id} ===")
    print(f"DB: {db_path}")
    if saved_cwd:
        print(f"Saved shell-cwd: {saved_cwd}")
    print(f"Models: {report['models']} | Efforts: {report['efforts']}")
    t = report["timing"]
    wall_str = f"{t['wall_clock_s']}s ({t['wall_clock_s']/60:.1f}m)" if t["wall_clock_s"] is not None else "n/a"
    print(
        f"Timing: wall={wall_str} | model={t['model_total_s']}s "
        f"(ttft={t['ttft_total_s']}s, gen={t['generation_total_s']}s, avg={t['avg_model_s']}s/req) "
        f"| tools={t['tool_total_s']}s"
    )
    tk = report["tokens"]
    print(
        f"Tokens: in={tk['input']:,} (cache_hit={tk['cache_hit_pct']}%, misses<50%={tk['cache_miss_spikes']}) "
        f"| out={tk['output']:,} (reasoning={tk['reasoning']:,}, {tk['reasoning_pct']}%)"
    )

    print(f"\n--- Tool Breakdown ({len(tcs)} calls across {len(reqs)} requests) ---")
    for name, info in report["tools"].items():
        print(f"  {name:24s}: {info['count']:4d} calls | total={info['total_s']:7.1f}s | avg={info['avg_ms']:6d}ms")

    b = report["batching"]
    print(
        f"\nBatching: tools/req={b['tools_per_request']} | "
        f"single-Bash reqs={b['single_bash_requests']}/{len(reqs)} ({b['single_bash_model_s']}s model time)"
    )

    bs = report["bash"]
    if bs["total"]:
        print(
            f"Bash ({bs['total']} total: fg={bs['fg']}, bg={bs['bg']}, err={bs['errors']}): "
            f"cd={bs['cd_calls']} (redundant_same={bs['redundant_same_cd']}), "
            f"sleep={bs['sleep_calls']} ({bs['sleep_seconds']}s), "
            f"chained={bs['chained_calls']}, read/grep_via_bash={bs['read_grep_via_bash']}, "
            f"with_cwd={bs['with_cwd']}/{bs['total']}, max_consecutive={bs['max_consecutive_bash']}"
        )
        if bs["top_cd_targets"]:
            print(f"  Top cd targets: {bs['top_cd_targets']}")

    if cache_misses:
        print(f"\n--- Cache Miss Spikes (<50% hit, >10k input): {len(cache_misses)} ---")
        for m in cache_misses[: args.top]:
            print(
                f"  req#{m['request_number']} in={m['input_tokens']} cache={m['cache_read_tokens']} "
                f"ttft={m['ttft_ms']}ms dur={m['total_duration_ms']}ms"
            )

    if args.top > 0 and tcs:
        print(f"\n--- Top {args.top} Slowest Tool Calls ---")
        for tc in sorted(tcs, key=lambda x: x.get("tc_duration_ms") or 0, reverse=True)[: args.top]:
            preview = (tc.get("input") or "").replace("\n", " ")[:120]
            print(f"  {(tc.get('tc_duration_ms') or 0)/1000:6.1f}s | {tc['name']:15s} | {preview}")

    if args.show_messages:
        print(f"\n--- Messages ({len(msgs)}) ---")
        for m in msgs:
            print(f"  #{m['rowid']:2d} {m['role']:9s} {m['created_at']} ({m['len']} chars) | {m['preview']!r}")

    if args.show_bash > 0 and bash_rows:
        n = args.show_bash
        print(f"\n--- Last {min(n, len(bash_rows))} Bash Commands ---")
        start = max(0, len(bash_rows) - n)
        for idx, (cmd, dur_ms, is_err, is_bg, has_cwd) in enumerate(bash_rows[start:], start=start):
            flags = ("BG " if is_bg else "FG ") + ("ERR" if is_err else "OK ")
            print(f"  [{idx:3d}] {dur_ms/1000:5.1f}s {flags} | {cmd.replace(chr(10), ' \\n ')[:140]}")

    return 0


def cmd_stream(args):
    path = Path(args.jsonl).expanduser()
    if not path.is_file():
        print(f"Error: file not found: {path}", file=sys.stderr)
        return 1

    events = []
    bad_lines = 0
    with path.open(encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                bad_lines += 1

    session_id = None
    model = None
    cwd = None
    reqs = []
    cur_req = None
    id_to_name = {}
    tool_results_by_id = {}
    req_attempts = []
    result_event = None
    turn_timings = []

    for e in events:
        if not session_id and e.get("session_id"):
            session_id = e.get("session_id")
        etype = e.get("type")
        if etype == "system":
            model = model or e.get("model")
            cwd = cwd or e.get("cwd")
        elif etype == "request_attempt":
            req_attempts.append(e)
        elif etype == "message_start":
            if cur_req is not None:
                reqs.append(cur_req)
            cur_req = {"tools": []}
        elif etype == "assistant":
            for b in ((e.get("message") or {}).get("content") or []):
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    if cur_req is None:
                        cur_req = {"tools": []}
                    tid = b.get("id")
                    name = b.get("name")
                    inp = b.get("input") or {}
                    cur_req["tools"].append((tid, name, inp))
                    if tid:
                        id_to_name[tid] = name
        elif etype == "tool_result":
            tid = e.get("id") or e.get("tool_use_id")
            if tid:
                tool_results_by_id[tid] = e
        elif etype == "turn_timing":
            turn_timings.append(e)
        elif etype == "result":
            result_event = e

    if cur_req is not None:
        reqs.append(cur_req)

    all_tools = [(i + 1, tid, name, inp) for i, r in enumerate(reqs) for tid, name, inp in r["tools"]]
    tool_counts = Counter(name for _, _, name, _ in all_tools)
    per_req_dist = Counter(len(r["tools"]) for r in reqs)
    single_bash_reqs = sum(1 for r in reqs if len(r["tools"]) == 1 and r["tools"][0][1] == "Bash")
    batched_reqs = sum(1 for r in reqs if len(r["tools"]) > 1)

    bash_rows = []
    for _, tid, name, inp in all_tools:
        if name != "Bash":
            continue
        res = tool_results_by_id.get(tid) or {}
        out = str(res.get("output") or res.get("content") or "")
        bash_rows.append(
            (
                inp.get("command") or "",
                0,
                bool(res.get("is_error")),
                bool(inp.get("run_in_background")),
                '"cwd"' in out or "cwd:" in out,
            )
        )

    bash_stats = classify_bash_commands(bash_rows)
    bash_stats["max_consecutive_bash"] = max_consecutive_run([name for _, _, name, _ in all_tools], "Bash")

    ttfb_sum = sum((a.get("elapsedMs") or 0) for a in req_attempts)
    retries = sum((a.get("retries") or 0) for a in req_attempts)

    status = "running"
    summary_result = {}
    if result_event:
        status = result_event.get("subtype") or ("error" if result_event.get("is_error") else "completed")
        usage = result_event.get("usage") or {}
        prompt_tok = usage.get("prompt_tokens") or usage.get("input_tokens") or 0
        cached_tok = (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or usage.get("cache_read_input_tokens") or 0
        comp_tok = usage.get("completion_tokens") or usage.get("output_tokens") or 0
        reason_tok = (usage.get("completion_tokens_details") or {}).get("reasoning_tokens") or 0
        summary_result = {
            "duration_s": round((result_event.get("duration_ms") or 0) / 1000.0, 1),
            "num_turns": result_event.get("num_turns"),
            "cost_usd": result_event.get("total_cost_usd"),
            "stop_reason": result_event.get("stop_reason"),
            "permission_denials": len(result_event.get("permission_denials") or []),
            "prompt_tokens": prompt_tok,
            "cached_tokens": cached_tok,
            "cache_hit_pct": round(100.0 * cached_tok / max(1, prompt_tok), 1) if prompt_tok else None,
            "completion_tokens": comp_tok,
            "reasoning_tokens": reason_tok,
        }

    report = {
        "file": str(path),
        "session_id": session_id,
        "model": model,
        "cwd": cwd,
        "status": status,
        "events": len(events),
        "parse_errors": bad_lines,
        "requests": len(reqs),
        "request_attempts": len(req_attempts),
        "avg_ttfb_ms": round(ttfb_sum / max(1, len(req_attempts))),
        "retries": retries,
        "tool_calls": len(all_tools),
        "tools": dict(tool_counts.most_common()),
        "tools_per_request": dict(per_req_dist.most_common()),
        "batched_requests": batched_reqs,
        "single_bash_requests": single_bash_reqs,
        "bash": bash_stats,
        "result": summary_result,
    }

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0

    print(f"=== Stream {path.name} ({status}) ===")
    print(f"Session: {session_id} | Model: {model} | CWD: {cwd}")
    print(
        f"Requests: {len(reqs)} (attempts={len(req_attempts)}, retries={retries}, "
        f"avg_ttfb={report['avg_ttfb_ms']}ms) | Tool calls: {len(all_tools)}"
    )
    print(f"Tools: {report['tools']}")
    print(
        f"Batching: tools/req={report['tools_per_request']} | "
        f"batched(>1)={batched_reqs}/{len(reqs)} | single-Bash={single_bash_reqs}/{len(reqs)}"
    )
    if bash_stats["total"]:
        print(
            f"Bash ({bash_stats['total']}): cd={bash_stats['cd_calls']} "
            f"(redundant={bash_stats['redundant_same_cd']}), sleep={bash_stats['sleep_calls']}, "
            f"chained={bash_stats['chained_calls']}, read/grep={bash_stats['read_grep_via_bash']}, "
            f"with_cwd={bash_stats['with_cwd']}/{bash_stats['total']}, "
            f"max_consecutive={bash_stats['max_consecutive_bash']}"
        )
    if summary_result:
        print(
            f"Result: duration={summary_result['duration_s']}s, turns={summary_result['num_turns']}, "
            f"cost=${summary_result['cost_usd']}, stop={summary_result['stop_reason']}, "
            f"denials={summary_result['permission_denials']}, "
            f"cache_hit={summary_result['cache_hit_pct']}%, "
            f"out={summary_result['completion_tokens']} (reasoning={summary_result['reasoning_tokens']})"
        )

    if args.tail > 0 and all_tools:
        print(f"\n--- Last {min(args.tail, len(all_tools))} Tool Calls ---")
        for req_num, tid, name, inp in all_tools[-args.tail :]:
            res = tool_results_by_id.get(tid)
            state = "PENDING" if res is None else ("ERR" if res.get("is_error") else "OK ")
            if name == "Bash":
                detail = (inp.get("command") or "").replace("\n", " ")[:110]
            else:
                detail = (
                    inp.get("file_path")
                    or inp.get("path")
                    or inp.get("pattern")
                    or json.dumps(inp, ensure_ascii=False)
                )[:110]
            print(f"  req#{req_num:3d} {state} {name:12s} | {detail}")

    if args.show_bash and bash_rows:
        print(f"\n--- All Bash Commands ({len(bash_rows)}) ---")
        for idx, (cmd, _, is_err, is_bg, _) in enumerate(bash_rows):
            flags = ("BG " if is_bg else "FG ") + ("ERR" if is_err else "OK ")
            print(f"  [{idx:2d}] {flags} | {cmd.replace(chr(10), ' \\n ')[:140]}")

    return 0


def load_request_records(rec_dir):
    metas = sorted(rec_dir.glob("*.meta.json"))
    records = []
    for meta_path in metas:
        base = meta_path.name[: -len(".meta.json")]
        seq = int(base.split("-", 1)[0]) if base.split("-", 1)[0].isdigit() else None
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8", errors="replace"))
        except Exception as exc:
            meta = {"_parse_error": str(exc)}
        req_path = rec_dir / f"{base}.request.json"
        sse_path = rec_dir / f"{base}.sse.txt"
        resp_path = rec_dir / f"{base}.response.json"
        headers_path = rec_dir / f"{base}.headers.json"
        records.append(
            {
                "seq": seq,
                "base": base,
                "meta": meta,
                "request_path": req_path if req_path.is_file() else None,
                "headers_path": headers_path if headers_path.is_file() else None,
                "sse_path": sse_path if sse_path.is_file() else None,
                "response_path": resp_path if resp_path.is_file() else None,
            }
        )
    return records


def extract_tool_names(req_body):
    tools = req_body.get("tools") or []
    names = []
    for item in tools:
        if not isinstance(item, dict):
            continue
        if "name" in item:
            names.append(item["name"])
        elif isinstance(item.get("function"), dict) and "name" in item["function"]:
            names.append(item["function"]["name"])
    return names


def extract_system_text(req_body):
    if "system" in req_body:
        return json.dumps(req_body["system"], ensure_ascii=False, sort_keys=True)
    if "instructions" in req_body:
        return json.dumps(req_body["instructions"], ensure_ascii=False, sort_keys=True)
    msgs = req_body.get("messages") or []
    sys_msgs = [m for m in msgs if isinstance(m, dict) and m.get("role") in ("system", "developer")]
    return json.dumps(sys_msgs, ensure_ascii=False, sort_keys=True)


def cmd_requests(args):
    data_dir = resolve_data_dir(args.data_dir)
    target = Path(args.target).expanduser()
    rec_dir = target if target.is_dir() else data_dir / "diagnostics" / "request-records" / args.target
    if not rec_dir.is_dir():
        print(f"Error: request-records directory not found: {rec_dir}", file=sys.stderr)
        return 1

    records = load_request_records(rec_dir)
    compactions = sorted(rec_dir.glob("compaction-*.json"))

    if args.show_tools is not None:
        match = next((r for r in records if r["seq"] == args.show_tools), None)
        if not match or not match["request_path"]:
            print(f"Error: request #{args.show_tools} not found in {rec_dir}", file=sys.stderr)
            return 1
        body = json.loads(match["request_path"].read_text(encoding="utf-8", errors="replace"))
        tools = extract_tool_names(body)
        sys_text = extract_system_text(body)
        msgs = body.get("messages") or body.get("input") or []
        print(f"=== Request #{match['seq']} ({match['base']}) ===")
        print(
            f"model={body.get('model')} | max_tokens={body.get('max_tokens') or body.get('max_completion_tokens') or body.get('max_output_tokens')} "
            f"| reasoning={body.get('reasoning') or body.get('reasoning_effort') or body.get('thinking')}"
        )
        print(f"system_chars={len(sys_text):,} | messages={len(msgs)} | tools={len(tools)}")
        print("tool_names:", ", ".join(tools))
        return 0

    if args.prefix_diff is not None:
        seq_a, seq_b = args.prefix_diff
        rec_a = next((r for r in records if r["seq"] == seq_a), None)
        rec_b = next((r for r in records if r["seq"] == seq_b), None)
        if not rec_a or not rec_b or not rec_a["request_path"] or not rec_b["request_path"]:
            print(f"Error: requests #{seq_a} and/or #{seq_b} not found", file=sys.stderr)
            return 1
        body_a = json.loads(rec_a["request_path"].read_text(encoding="utf-8", errors="replace"))
        body_b = json.loads(rec_b["request_path"].read_text(encoding="utf-8", errors="replace"))
        print(f"=== Prefix Diff: #{seq_a} vs #{seq_b} ===")
        sys_same = extract_system_text(body_a) == extract_system_text(body_b)
        tools_a = extract_tool_names(body_a)
        tools_b = extract_tool_names(body_b)
        tools_same = json.dumps(body_a.get("tools"), sort_keys=True) == json.dumps(body_b.get("tools"), sort_keys=True)
        print(f"system identical: {sys_same}")
        print(f"tools identical:  {tools_same} (count {len(tools_a)} -> {len(tools_b)})")
        if tools_a != tools_b:
            print(f"  added tools:   {[t for t in tools_b if t not in tools_a]}")
            print(f"  removed tools: {[t for t in tools_a if t not in tools_b]}")
        msgs_a = body_a.get("messages") or body_a.get("input") or []
        msgs_b = body_b.get("messages") or body_b.get("input") or []
        first_diff = None
        for idx in range(min(len(msgs_a), len(msgs_b))):
            if msgs_a[idx] != msgs_b[idx]:
                first_diff = idx
                break
        if first_diff is None:
            print(f"shared message prefix identical across all {min(len(msgs_a), len(msgs_b))} messages (len {len(msgs_a)} -> {len(msgs_b)})")
        else:
            print(f"FIRST MESSAGE DIVERGENCE at index {first_diff} (of {len(msgs_a)} -> {len(msgs_b)}):")
            print(f"  A[{first_diff}]: {json.dumps(msgs_a[first_diff], ensure_ascii=False)[:240]}")
            print(f"  B[{first_diff}]: {json.dumps(msgs_b[first_diff], ensure_ascii=False)[:240]}")
        return 0

    rows = []
    for r in records:
        m = r["meta"]
        dur_s = parse_iso_seconds(m.get("started_at"), m.get("ended_at"))
        rows.append(
            {
                "seq": r["seq"],
                "purpose": m.get("purpose"),
                "attempt": m.get("attempt"),
                "model": m.get("model"),
                "http_status": m.get("http_status"),
                "completed": m.get("stream_completed"),
                "duration_s": round(dur_s, 2) if dur_s is not None else None,
                "req_bytes": r["request_path"].stat().st_size if r["request_path"] else 0,
                "error": m.get("error") or m.get("recording_error"),
            }
        )

    if args.json:
        print(json.dumps({"dir": str(rec_dir), "requests": rows, "compactions": [c.name for c in compactions]}, ensure_ascii=False, indent=2))
        return 0

    print(f"=== Request Records: {rec_dir} ({len(rows)} attempts, {len(compactions)} compactions) ===")
    for row in rows:
        dur = f"{row['duration_s']:6.2f}s" if row["duration_s"] is not None else "    n/a"
        err = f" ERR={row['error']}" if row["error"] else ""
        print(
            f"  #{row['seq']:04d} a{row['attempt']} | {str(row['purpose']):10s} | "
            f"HTTP {str(row['http_status']):>3s} | ok={str(row['completed']):5s} | "
            f"{dur} | {row['req_bytes']:8d}B | {row['model']}{err}"
        )
    if compactions:
        print("Compaction files:", ", ".join(c.name for c in compactions))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p_sess = sub.add_parser("session", help="Audit a session.db for timings, cache hit rate, tools, and shell patterns")
    p_sess.add_argument("target", help="Session ID or path to session.db")
    p_sess.add_argument("--data-dir", help="Override DEEPSEEK_CODE_DATA_DIR")
    p_sess.add_argument("--top", type=int, default=10, help="Show top N slowest tool calls (default 10)")
    p_sess.add_argument("--show-messages", action="store_true", help="List user/assistant messages")
    p_sess.add_argument("--show-bash", type=int, default=0, help="Show last N Bash commands")
    p_sess.add_argument("--json", action="store_true", help="Emit JSON summary")
    p_sess.set_defaults(func=cmd_session)

    p_stream = sub.add_parser("stream", help="Inspect a stream-json run log (live or completed)")
    p_stream.add_argument("jsonl", help="Path to stream-json output file")
    p_stream.add_argument("--tail", type=int, default=15, help="Show last N tool calls (default 15)")
    p_stream.add_argument("--show-bash", action="store_true", help="List all Bash commands")
    p_stream.add_argument("--json", action="store_true", help="Emit JSON summary")
    p_stream.set_defaults(func=cmd_stream)

    p_req = sub.add_parser("requests", help="Inspect diagnostics/request-records for a session")
    p_req.add_argument("target", help="Session ID or path to request-records/<session> directory")
    p_req.add_argument("--data-dir", help="Override DEEPSEEK_CODE_DATA_DIR")
    p_req.add_argument("--show-tools", type=int, metavar="SEQ", help="Show model/system/tools of request #SEQ")
    p_req.add_argument("--prefix-diff", type=int, nargs=2, metavar=("A", "B"), help="Compare prompt prefix of requests #A and #B")
    p_req.add_argument("--json", action="store_true", help="Emit JSON summary")
    p_req.set_defaults(func=cmd_requests)

    args = parser.parse_args()
    sys.exit(args.func(args))


if __name__ == "__main__":
    main()
