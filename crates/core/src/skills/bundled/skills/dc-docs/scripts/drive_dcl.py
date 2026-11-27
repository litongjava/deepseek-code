#!/usr/bin/env python3
"""Drive the dcl REPL through a ConPTY and capture rendered screens.

Requires: pip install pywinpty pyte
"""

import argparse
import json
import os
import re
import sys
import threading
import time

try:
    import pyte
    import winpty
except ImportError as error:  # noqa: BLE001
    sys.exit(f"missing dependency: {error}\nRun: pip install pywinpty pyte")

# Win32 input mode: CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _
KEYS = {
    "enter": "\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_",
    "esc": "\x1b[27;1;27;1;0;1_\x1b[27;1;27;0;0;1_",
    "space": "\x1b[32;57;32;1;0;1_\x1b[32;57;32;0;0;1_",
    "tab": "\x1b[9;15;9;1;0;1_\x1b[9;15;9;0;0;1_",
    "right": "\x1b[39;77;0;1;0;1_\x1b[39;77;0;0;0;1_",
    "left": "\x1b[37;75;0;1;0;1_\x1b[37;75;0;0;0;1_",
}


def seed_trust(data_dir, cwd):
    folder = os.path.abspath(cwd)
    if not folder.startswith("\\\\?\\"):
        folder = "\\\\?\\" + folder
    path = os.path.join(data_dir, "startup-prompts.json")
    current = {}
    if os.path.exists(path):
        try:
            current = json.load(open(path, encoding="utf-8"))
        except (OSError, ValueError):
            current = {}
    folders = list(current.get("trusted_folders", []))
    if folder not in folders:
        folders.append(folder)
    current.update(
        {
            "trusted_folders": folders,
            "bypass_accepted": True,
            "full_access_accepted": True,
        }
    )
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(current, handle)


def render(buffer, rows, cols):
    screen = pyte.HistoryScreen(cols, rows, history=6000)
    pyte.Stream(screen).feed(buffer)
    lines = []
    for line in screen.history.top:
        lines.append("".join(line[column].data for column in range(cols)).rstrip())
    lines.extend(screen.display)
    return [line.rstrip() for line in lines]


def slug(text):
    cleaned = re.sub(r"[^A-Za-z0-9]+", "-", text.strip()).strip("-")
    return cleaned or "step"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True, help="dcl executable to drive")
    parser.add_argument("--cwd", default=".", help="workspace passed to dcl")
    parser.add_argument("--data-dir", help="throwaway data directory")
    parser.add_argument("--config", help="config.json copied into the data directory")
    parser.add_argument("--outdir", required=True, help="where screen captures are written")
    parser.add_argument("--steps", nargs="*", default=["/skills", "/plugins", "/tools"])
    parser.add_argument("--rows", type=int, default=42)
    parser.add_argument("--cols", type=int, default=150)
    parser.add_argument("--startup-timeout", type=float, default=40.0)
    parser.add_argument("--quiet", type=float, default=1.5)
    parser.add_argument("--no-seed-trust", action="store_true")
    args = parser.parse_args()

    cwd = os.path.abspath(args.cwd)
    os.makedirs(args.outdir, exist_ok=True)
    data_dir = args.data_dir
    if args.config and not data_dir:
        import tempfile

        data_dir = tempfile.mkdtemp(prefix="dcl-pty-")
    if data_dir:
        os.makedirs(data_dir, exist_ok=True)
    if args.config and data_dir:
        import shutil

        shutil.copy(args.config, os.path.join(data_dir, "config.json"))
    if data_dir and args.config and not args.no_seed_trust:
        seed_trust(data_dir, cwd)

    argv = [args.bin]
    if data_dir:
        argv += ["--data-dir", data_dir]

    process = winpty.PtyProcess.spawn(argv, cwd=cwd, dimensions=(args.rows, args.cols))
    chunks = []
    lock = threading.Lock()
    stop = False

    def reader():
        while not stop:
            try:
                data = process.read(8192)
            except EOFError:
                break
            except Exception:  # noqa: BLE001 - the PTY closes on exit
                break
            if not data:
                break
            with lock:
                chunks.append(data)

    threading.Thread(target=reader, daemon=True).start()

    def text():
        with lock:
            return "".join(chunks)

    def wait_for(needle, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in text():
                return True
            if not process.isalive():
                return False
            time.sleep(0.2)
        return False

    def wait_quiet(idle, timeout):
        deadline = time.time() + timeout
        last = len(text())
        quiet_since = time.time()
        while time.time() < deadline:
            time.sleep(0.3)
            now = len(text())
            if now != last:
                last = now
                quiet_since = time.time()
            elif time.time() - quiet_since >= idle:
                return True
            if not process.isalive():
                return False
        return False

    def send(data, wait=0.5):
        try:
            process.write(data)
        except Exception as error:  # noqa: BLE001
            print("write failed:", error)
        time.sleep(wait)

    if not wait_for("Type / to browse", args.startup_timeout):
        tail = "\n".join(render(text(), args.rows, args.cols)[-20:])
        print("startup failed; last screen:\n" + tail)
        if process.isalive():
            process.terminate()
        stop = True
        return 1

    failures = 0
    for index, step in enumerate(args.steps, start=1):
        if step.startswith("@"):
            name = step[1:].lower()
            if name not in KEYS:
                print(f"unknown key: {name}")
                failures += 1
                continue
            send(KEYS[name], 0.4)
        else:
            send(step, 0.4)
            send(KEYS["enter"], 0.4)
        wait_quiet(args.quiet, 30)
        lines = render(text(), args.rows, args.cols)
        tail = [line for line in lines if line.strip()][-args.rows:]
        path = os.path.join(args.outdir, f"{index:02d}-{slug(step)}.txt")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(tail))
        print(f"{path}: {len(tail)} lines")
        if not tail:
            failures += 1

    send(KEYS["esc"], 1.0)
    send("/exit", 0.3)
    send(KEYS["enter"], 2.5)
    if process.isalive():
        process.terminate()
    time.sleep(0.5)
    stop = True
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
