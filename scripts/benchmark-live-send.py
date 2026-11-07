#!/usr/bin/env python3
"""Measure real AoE key delivery and preview echo in an isolated tmux session."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time


ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-2A-Z]|[\x00-\x08\x0b-\x1f\x7f]")
ROOT = Path(__file__).resolve().parents[1]


def summary(values):
    ordered = sorted(values)
    return {
        "median_ms": round(statistics.median(values), 2),
        "p95_ms": round(ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))], 2),
        "max_ms": round(max(values), 2),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/aoe")
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--width", type=int, default=160)
    parser.add_argument("--height", type=int, default=48)
    parser.add_argument("--gap-ms", type=int, default=40)
    parser.add_argument("--idle-sessions", type=int, default=0,
                        help="Add stopped sessions to exercise background maintenance")
    parser.add_argument("--capture-fallback", action="store_true")
    parser.add_argument("--debug-namespace", action="store_true")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--trace-out", type=Path,
                        help="Save render timing traces from the isolated app")
    parser.add_argument("--assert-responsive", action="store_true")
    args = parser.parse_args()
    if args.samples < 1 or args.gap_ms < 0 or args.idle_sessions < 0:
        parser.error("samples must be positive; gap-ms and idle-sessions must be nonnegative")
    binary = str(args.binary.resolve())
    tmux = shutil.which("tmux")
    if not tmux:
        parser.error("tmux is required")
    version = subprocess.check_output([binary, "--version"], text=True).strip().split()[-1]

    with tempfile.TemporaryDirectory(prefix="aoe-lag-", dir="/private/tmp" if sys.platform == "darwin" else "/tmp") as directory:
        fixture = Path(directory)
        socket = str(fixture / "tmux.sock")
        child_env = os.environ.copy()
        for key in list(child_env):
            if key.startswith(("AOE_", "AGENT_OF_EMPIRES_")) or key in ("TMUX", "TMUX_PANE"):
                child_env.pop(key)
        child_env.update({
            "HOME": str(fixture), "XDG_CONFIG_HOME": str(fixture / ".config"),
            "XDG_DATA_HOME": str(fixture / ".local/share"),
            "AOE_TMUX_SOCKET": socket, "TERM": "xterm-256color",
            "PATH": str(fixture / "bin") + os.pathsep + os.environ.get("PATH", ""),
        })
        app_name = "agent-of-empires-dev" if args.debug_namespace else "agent-of-empires"
        app = fixture / ".config" / app_name
        profile = app / "profiles" / "default"
        profile.mkdir(parents=True)
        (app / "config.toml").write_text(f'''[updates]
update_check_mode = "off"
[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
has_acknowledged_agent_hooks = true
last_seen_version = "{version}"
[session]
default_attach_mode = "live_send"
[tmux]
vt_live = {"false" if args.capture_fallback else "true"}
''')
        if args.trace_out:
            with (app / "config.toml").open("a") as config:
                config.write('\n[logging]\ndefault_level = "warn"\noutput = "file"\nfile_path = "benchmark.log"\n[logging.targets]\n"tui.render" = "trace"\n')
        instance = {"id": "12345678-bench", "title": "latency", "project_path": directory,
                    "group_path": "", "command": "claude", "tool": "claude", "yolo_mode": False,
                    "status": "idle", "created_at": "2026-01-01T00:00:00Z"}
        instances = [instance]
        for index in range(args.idle_sessions):
            instances.append({**instance, "id": f"{index + 1:08x}-idle",
                              "title": f"stopped-{index:04d}", "status": "stopped"})
        (profile / "sessions.json").write_text(json.dumps(instances))
        (fixture / "bin").mkdir()
        agent = fixture / "bin" / "claude"
        receipt = fixture / "received.jsonl"
        agent.write_text("#!" + sys.executable + "\n" + '''import json, os, sys, time, tty
tty.setraw(0)
os.write(1, b"BENCH_READY\\r\\n")
receipt = open(sys.argv[1], "a", buffering=1)
while True:
    data = os.read(0, 4096)
    if not data:
        break
    for byte in data:
        receipt.write(json.dumps({"key": byte, "at": time.clock_gettime_ns(time.CLOCK_MONOTONIC)}) + "\\n")
        if 32 <= byte < 127:
            os.write(1, b"\\x1b[2J\\x1b[H" + bytes([byte]) * 24 + b"\\r\\n")
''')
        agent.chmod(0o755)
        prefix = "aoe_dev_" if args.debug_namespace else "aoe_"
        name = prefix + "latency_12345678"

        def tmux_command(*values, check=True):
            return subprocess.run([tmux, "-S", socket, *values], env=child_env,
                                  capture_output=True, text=True, check=check)

        pid = master = None
        transcript = bytearray()
        try:
            tmux_command("-f", "/dev/null", "new-session", "-d", "-x", str(args.width), "-y", str(args.height),
                         "-s", name, shlex.join([str(agent), str(receipt)]))
            pid, master = pty.fork()
            if pid == 0:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", args.height, args.width, 0, 0))
                os.execve(binary, [binary], child_env)
            os.set_blocking(master, False)

            def read_output(wait):
                if not select.select([master], [], [], wait)[0]:
                    return b""
                try:
                    data = os.read(master, 1024 * 1024)
                except OSError as error:
                    raise RuntimeError("AoE exited during benchmark") from error
                transcript.extend(data)
                # Answer a cursor-position query if a terminal backend asks.
                if b"\x1b[6n" in data:
                    os.write(master, b"\x1b[1;1R")
                return data

            def wait_for(marker, timeout=15):
                start = time.monotonic()
                output = bytearray()
                while time.monotonic() - start < timeout:
                    output.extend(read_output(0.02))
                    if marker in ANSI.sub(b"", output):
                        return
                tail = ANSI.sub(b"", transcript[-12000:]).decode("utf8", "replace")
                raise RuntimeError(f"Timed out waiting for {marker!r}. Isolated TUI output:\n{tail}")

            wait_for(b"BENCH_READY")
            os.write(master, b"\r")
            wait_for(b"Ctrl+Q")
            # Let entry resize/ownership work settle before measuring typing.
            settle = time.monotonic() + 1
            while time.monotonic() < settle:
                read_output(0.02)
            delivery, echo = [], []
            for index in range(args.samples):
                key = ord("a") + index % 26
                while read_output(0):
                    pass
                began = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
                os.write(master, bytes([key]))
                wait_for(bytes([key]) * 24, timeout=5)
                painted = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
                records = [json.loads(line) for line in receipt.read_text().splitlines()]
                received = next(row["at"] for row in reversed(records) if row["key"] == key)
                assert received >= began, "Matched a stale echo instead of the key just sent"
                delivery.append((received - began) / 1_000_000)
                echo.append((painted - began) / 1_000_000)
                until = time.monotonic() + args.gap_ms / 1000
                while time.monotonic() < until:
                    read_output(max(0, min(0.01, until - time.monotonic())))

            result = {"binary": binary, "version": version, "capture_fallback": args.capture_fallback,
                      "idle_sessions": args.idle_sessions, "gap_ms": args.gap_ms,
                      "dimensions": [args.width, args.height], "samples": args.samples,
                      "key_delivery": summary(delivery), "visible_echo": summary(echo),
                      "delivery_ms": [round(n, 2) for n in delivery], "echo_ms": [round(n, 2) for n in echo]}
            print(json.dumps({k: v for k, v in result.items() if k not in ("delivery_ms", "echo_ms")}, indent=2))
            if args.out:
                args.out.write_text(json.dumps(result, indent=2) + "\n")
            if args.assert_responsive:
                assert result["visible_echo"]["p95_ms"] < 50, "Typing echo exceeded the 50 ms p95 budget"
                assert result["visible_echo"]["max_ms"] < 100, "Typing stalled for 100 ms or longer"
        finally:
            if pid:
                try:
                    os.kill(pid, signal.SIGTERM)
                    deadline = time.monotonic() + 3
                    while time.monotonic() < deadline:
                        if os.waitpid(pid, os.WNOHANG)[0]:
                            break
                        if master is not None:
                            try:
                                if select.select([master], [], [], 0.01)[0]:
                                    os.read(master, 1024 * 1024)
                            except OSError:
                                pass
                    else:
                        os.kill(pid, signal.SIGKILL)
                        os.waitpid(pid, 0)
                except (ProcessLookupError, ChildProcessError):
                    pass
            if master is not None:
                os.close(master)
            tmux_command("kill-server", check=False)
            if args.trace_out and (app / "benchmark.log").exists():
                shutil.copyfile(app / "benchmark.log", args.trace_out)


if __name__ == "__main__":
    main()
