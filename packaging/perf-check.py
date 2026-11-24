#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""The design §9 speed budgets on an optimised build (the CI `perf` job).

    packaging/perf-check.py --cli target/perf/quire-cli --app target/perf/quire \
        --server target/perf/quire-sync-server

`--app` is the desktop built with `--features bench`; it needs a display
(CI runs Xvfb). Each measure is a median: the CLI suites take the median of
`--runs` runs inside one process, the desktop runs are separate processes and
this takes the median of their results. A measure fails when it is over its
budget times MARGIN (CI runners are noisy and shared; see
docs/design/0.10-perf.md). Writes a Markdown table to $GITHUB_STEP_SUMMARY
when that is set, and exits 1 when anything is over.

`--server` adds the sync rows of docs/design/0.14-sync-speed.md §2 (Linux
only): a vault of `--sync-notes` notes from `quire-cli bench vault`, its
first sync to a local `quire-sync-server`, the pass right after it, idle
`sync once` runs (wall time, bytes read and written from /proc/<pid>/io,
peak memory), another device joining, the migration of a 0.13 state file,
and a first sync to a sync folder. Timings and memory only report until the
budgets are adopted; the exact count (requests of the first sync, through a
counting proxy) fails when over.
"""

import argparse
import http.client
import http.server
import json
import math
import os
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time

# Headroom for noise on shared CI runners: a measure fails above budget x 1.25.
MARGIN = 1.25

# (suite, metric) -> (budget in ms or None to only report, what it is)
BUDGETS = {
    ("index", "build_2000_ms"): (420, "Index build, 2,000 notes"),
    ("search", "bm25_p50_ms"): (10, "Search, 2,000 notes (p50)"),
    ("sections", "p50_ms"): (15, "Section search, 2,000 notes (p50)"),
    ("sections", "p95_ms"): (None, "Section search, 2,000 notes (p95)"),
    ("semantic", "scan_p50_ms"): (None, "Semantic scan alone (part of section search)"),
    ("semantic", "passages"): (None, "Passages scanned (count)"),
    ("mcp", "view_p50_ms"): (10, "MCP `view`, 10k notes (p50)"),
    ("mcp", "search_p95_ms"): (50, "MCP `search`, 10k notes (p95)"),
    ("mcp", "ask_p95_ms"): (None, "MCP `ask`, 10k notes (p95)"),
    ("mcp", "search_prefix_p95_ms"): (None, "MCP `search` for a one-character prefix, 10k notes (p95)"),
    ("mcp", "search_keyword_p95_ms"): (None, "MCP `search`, 10k notes, keywords only (p95)"),
    ("mcp", "ask_keyword_p95_ms"): (None, "MCP `ask`, 10k notes, keywords only (p95)"),
    # 0.14: an agent ranks through its view, here with 10% of notes denied (docs/design/0.14-agent-index.md G5-G7).
    ("mcp", "search_restricted_p95_ms"): (60, "MCP `search`, 10k notes, 10% denied (p95)"),
    # Report only: `ask` on this vault (every note has every word) takes 70-160 ms with keywords
    # only and no view at all, so G5's 60 ms is not the view's to meet; the view adds about 1.25x.
    ("mcp", "ask_restricted_p95_ms"): (None, "MCP `ask`, 10k notes, 10% denied (p95)"),
    ("mcp", "search_prefix_restricted_p95_ms"): (None, "MCP `search` for a one-character prefix, 10% denied (p95)"),
    ("mcp", "view_build_p50_ms"): (50, "An agent view's build, 10k notes (p50)"),
    ("mcp", "view_heap_bytes"): (None, "An agent view's memory, 10k notes (bytes)"),
    # 0.14: the listing tools filter with the call's Visibility (docs/design/0.14-agent-tools.md G5).
    ("mcp", "list_p50_ms"): (30, "MCP `list`, 10k notes (p50)"),
    ("mcp", "tags_p50_ms"): (30, "MCP `tags`, 10k notes (p50)"),
    ("mcp", "properties_p50_ms"): (30, "MCP `properties`, 10k notes (p50)"),
    ("mcp", "tasks_p50_ms"): (30, "MCP `tasks`, 10k notes (p50)"),
    ("mcp", "links_p50_ms"): (30, "MCP `links`, 10k notes (p50)"),
    ("mcp", "vault_info_p50_ms"): (30, "MCP `vault_info`, 10k notes (p50)"),
    ("mcp", "query_view_p50_ms"): (30, "MCP `query_view`, 10k notes (p50)"),
    ("pdf", "text_per_page_p50_ms"): (50, "PDF text extraction per page (p50)"),
    ("pdf", "text_per_page_max_ms"): (None, "PDF text extraction per page (worst PDF)"),
    ("mcp-pdfs", "first_answer_200_pdfs_ms"): (500, "MCP start to first search answer, 200 new PDFs"),
    ("mcp-pdfs", "all_read_200_pdfs_ms"): (None, "MCP start until all 200 PDFs are searchable"),
    ("startup", "first_frame_ms"): (None, "Startup to first frame, 10k notes"),
    ("startup", "empty_first_frame_ms"): (None, "Startup to first frame, empty vault"),
    ("startup", "vault_cost_ms"): (20, "Startup: what 10k notes add before the first frame"),
    ("typing", "indexing_p50_ms"): (None, "Keystroke to frame while 10k notes are first indexed (p50)"),
    ("typing", "p50_ms"): (None, "Keystroke to frame, 100 KB note, jobs on (p50)"),
    # 0.11: 12 -> 10 ms (measured 6.8-7.2 ms on the 2-core VM; docs/design/0.11-editor.md §6).
    ("typing", "p95_ms"): (10, "Keystroke to frame, 100 KB note, jobs on (p95)"),
    ("typing", "p99_ms"): (16, "Keystroke to frame, 100 KB note, jobs on (p99)"),
    ("canvas", "frame_p50_ms"): (8, "Canvas, 1,000 cards: pan/zoom frame (p50)"),
    ("canvas", "frame_max_ms"): (None, "Canvas, 1,000 cards: worst frame"),
}

# Storage calls of one listing call on the `mcp` suite's 10k notes in 50 folders
# (docs/design/0.14-agent-tools.md G4): exact, so any call over fails, with no
# margin (counts do not depend on the runner). Each is 3 per folder (a key file,
# an unlocking file, whether the folder is a symlink) plus the call's own: the
# policy file read twice, and for `links` the note it names, for `vault_info`
# the zones' folders.
COUNTS = {
    ("mcp", "list_storage_calls"): (152, "MCP `list`: storage calls"),
    ("mcp", "tags_storage_calls"): (152, "MCP `tags`: storage calls"),
    ("mcp", "properties_storage_calls"): (152, "MCP `properties`: storage calls"),
    ("mcp", "tasks_storage_calls"): (152, "MCP `tasks`: storage calls"),
    ("mcp", "links_storage_calls"): (157, "MCP `links`: storage calls"),
    ("mcp", "vault_info_storage_calls"): (159, "MCP `vault_info`: storage calls"),
    ("mcp", "query_view_storage_calls"): (152, "MCP `query_view`: storage calls"),
}

# docs/design/0.14-sync-speed.md §2, against a local server (and a folder):
# metric -> (budget at 10k notes, at 50k notes, unit, what it is). None: no
# budget yet. Report only until the budgets are adopted (wave 4).
SYNC_BUDGETS = {
    "first_sync_ms": (6000, 30000, "ms", "First sync to a local server"),
    "first_sync_requests": (None, None, "", "same: push requests (exact; fails above its bound)"),
    "first_sync_push_mb": (None, None, "MB", "same: request bytes pushed"),
    "pass_after_first_ms": (150, 600, "ms", "Pass right after the first sync"),
    "idle_wall_ms": (150, 600, "ms", "Idle `sync once` against a local server: wall (median)"),
    "idle_read_mb": (8, 40, "MB", "same: bytes read"),
    "idle_written_kb": (4, 4, "KB", "same: bytes written"),
    "idle_peak_mb": (60, 200, "MB", "same: peak memory"),
    "join_ms": (None, None, "ms", "Another device joining: its first pass"),
    "join_peak_mb": (None, None, "MB", "same: peak memory"),
    "join_fsyncs": (None, None, "", "same: fsync and fdatasync calls (with strace)"),
    "migration_ms": (None, 10000, "ms", "The pass that migrates a 0.13 state file"),
    "folder_first_sync_ms": (None, None, "ms", "First sync to a sync folder"),
    "folder_first_sync_fsyncs": (None, None, "", "same: fsync and fdatasync calls (with strace)"),
}

# Absolute regression guards for measures whose design budget is relative
# (startup is "0.8 + 20 ms": the 10k-note cost is checked above, this caps
# the whole).
ABSOLUTE = {("startup", "first_frame_ms"): 1500}


def lines(cmd, env=None, timeout=900):
    out = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=timeout, check=False)
    if out.returncode != 0:
        print(f"::error::{' '.join(cmd)} failed ({out.returncode}):\n{out.stderr[-3000:]}", file=sys.stderr)
        return []
    rows = []
    for line in out.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            rows.append(json.loads(line))
    return rows


class Measured:
    """One process run to its end: wall time, /proc/<pid>/io and peak memory."""

    def __init__(self, cmd, env, timeout=1800):
        with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
            start = time.monotonic()
            p = subprocess.Popen(cmd, env=env, stdout=out, stderr=err)
            # Wait without reaping, so /proc/<pid>/io still says what it read.
            deadline = start + timeout
            while True:
                got = os.waitid(os.P_PID, p.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                if got is not None:
                    break
                if time.monotonic() > deadline:
                    p.kill()
                time.sleep(0.005)
            self.wall_ms = (time.monotonic() - start) * 1000
            self.io = {}
            try:
                with open(f"/proc/{p.pid}/io", encoding="ascii") as f:
                    for line in f:
                        k, v = line.split(":")
                        self.io[k] = int(v)
            except OSError:
                pass
            _, status, usage = os.wait4(p.pid, 0)
            p.returncode = os.waitstatus_to_exitcode(status)
            self.peak_mb = usage.ru_maxrss * 1024 / 1e6
            self.returncode = p.returncode
            out.seek(0)
            err.seek(0)
            self.stdout = out.read().decode(errors="replace")
            self.stderr = err.read().decode(errors="replace")
        if self.returncode != 0:
            raise RuntimeError(f"{' '.join(cmd)} failed ({self.returncode}):\n{self.stderr[-3000:]}")


class Counter:
    """A proxy in front of the server that counts the requests pushing revisions."""

    def __init__(self, upstream_port):
        self.pushes = 0
        self.push_bytes = 0
        lock = threading.Lock()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def relay(self):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n) if n else None
                if self.command == "POST" and "/revisions" in self.path:
                    with lock:
                        outer.pushes += 1
                        outer.push_bytes += n
                if not hasattr(self, "up"):
                    self.up = http.client.HTTPConnection("127.0.0.1", upstream_port, timeout=600)
                skip = ("host", "connection", "content-length")
                headers = {k: v for k, v in self.headers.items() if k.lower() not in skip}
                self.up.request(self.command, self.path, body=body, headers=headers)
                r = self.up.getresponse()
                data = r.read()
                self.send_response(r.status)
                for k, v in r.getheaders():
                    if k.lower() not in ("content-length", "transfer-encoding", "connection", "date", "server"):
                        self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_DELETE = relay

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def fsyncs(cmd, env, tmp):
    """fsync and fdatasync calls of a run, when strace is there (else None)."""
    if not shutil.which("strace"):
        return None
    out = os.path.join(tmp, "strace.txt")
    Measured(["strace", "-f", "-c", "-o", out, "-e", "trace=fsync,fdatasync", *cmd], env)
    total = 0
    with open(out, encoding="utf-8") as f:
        for line in f:
            parts = line.split()
            # % time, seconds, usecs/call, calls, [errors], syscall
            if len(parts) >= 5 and parts[-1] in ("fsync", "fdatasync"):
                total += int(parts[3])
    return total


def sync_suite(a):
    """docs/design/0.14-sync-speed.md §2 against a local server: metric -> values."""
    res = {}
    n = a.sync_notes
    token = "perf-token-0123456789abcdefghij"
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ)
        for k in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
            env[k] = os.path.join(tmp, k.lower())
        env["HOME"] = tmp
        env["QUIRE_SECRETS_FILE"] = os.path.join(tmp, "secrets.json")
        env["QUIRE_SYNC_PASSPHRASE"] = "perf pass phrase"
        env["QUIRE_SYNC_TOKEN"] = token

        def vault(name):
            path = os.path.join(tmp, name)
            subprocess.run([a.cli, "bench", "vault", path, str(n)], check=True, capture_output=True)
            # An hour old, as a vault being set up is: files written within
            # 2 s of a scan are hashed again by the next one.
            old = time.time() - 3600
            for d, _, files in os.walk(path):
                for f in files:
                    os.utime(os.path.join(d, f), (old, old))
            return path

        def once(path):
            return Measured([a.cli, "sync", "once", path], env)

        def join(path, where, create):
            if where[0] == "--folder":
                os.makedirs(where[1], exist_ok=True)
            cmd = [a.cli, "sync", "join", path, *where, "--device", os.path.basename(path)]
            if create:
                cmd += ["--create", "--recovery-key-file", path + ".recovery"]
            os.makedirs(path, exist_ok=True)
            Measured(cmd, env)

        port = free_port()
        server = subprocess.Popen(
            [a.server],
            env={**env, "QUIRE_SYNC_ADDR": f"127.0.0.1:{port}", "QUIRE_SYNC_DB": os.path.join(tmp, "server.sqlite")},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        counter = None
        try:
            for _ in range(200):
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=1):
                        break
                except OSError:
                    time.sleep(0.05)
            counter = None if a.sync_direct else Counter(port)
            url = f"http://127.0.0.1:{counter.port if counter else port}"
            first = vault("a")
            join(first, ["--server", url], create=True)
            m = once(first)
            res["first_sync_ms"] = [m.wall_ms]
            if counter:
                res["first_sync_requests"] = [counter.pushes]
                res["first_sync_push_mb"] = [counter.push_bytes / 1e6]
            res["pass_after_first_ms"] = [once(first).wall_ms]
            for _ in range(a.runs):
                m = once(first)
                res.setdefault("idle_wall_ms", []).append(m.wall_ms)
                res.setdefault("idle_read_mb", []).append(m.io.get("rchar", 0) / 1e6)
                res.setdefault("idle_written_kb", []).append(m.io.get("wchar", 0) / 1e3)
                res.setdefault("idle_peak_mb", []).append(m.peak_mb)
            if "join" not in a.sync_without:
                other = os.path.join(tmp, "joined")
                join(other, ["--server", url], create=False)
                m = once(other)
                res["join_ms"] = [m.wall_ms]
                res["join_peak_mb"] = [m.peak_mb]
                shutil.rmtree(other, ignore_errors=True)
                third = os.path.join(tmp, "joined-traced")
                join(third, ["--server", url], create=False)
                f = fsyncs([a.cli, "sync", "once", third], env, tmp)
                if f is not None:
                    res["join_fsyncs"] = [f]
                shutil.rmtree(third, ignore_errors=True)
            # Needs a quire-cli built with `--features bench-hooks`.
            if "migration" not in a.sync_without:
                made = subprocess.run([a.cli, "bench", "sync-013", first], env=env, check=False, capture_output=True)
                if made.returncode == 0:
                    res["migration_ms"] = [once(first).wall_ms]
        finally:
            if counter:
                counter.close()
            server.terminate()
            server.wait(timeout=30)
        if "folder" in a.sync_without:
            return res
        # A sync folder: the same notes, a first sync.
        shutil.rmtree(first, ignore_errors=True)
        folder = vault("f")
        join(folder, ["--folder", os.path.join(tmp, "sync-folder")], create=True)
        res["folder_first_sync_ms"] = [once(folder).wall_ms]
        shutil.rmtree(os.path.join(tmp, "sync-folder"))
        shutil.rmtree(folder)
        traced = vault("ft")
        join(traced, ["--folder", os.path.join(tmp, "sync-folder-2")], create=True)
        f = fsyncs([a.cli, "sync", "once", traced], env, tmp)
        if f is not None:
            res["folder_first_sync_fsyncs"] = [f]
    return res


def sync_report(a, res):
    """The sync table, and what fails (the exact request bound only)."""
    col = 0 if a.sync_notes <= 10000 else 1
    med = {k: statistics.median(v) for k, v in res.items() if v}
    failed = []
    table = [
        f"| Measure ({a.sync_notes:,} notes) | Budget | Median | |",
        "|---|---:|---:|---|",
    ]
    for key, (b10, b50, unit, label) in SYNC_BUDGETS.items():
        if key not in med:
            table.append(f"| {label} | | not measured | |")
            continue
        v = med[key]
        budget = (b10, b50)[col]
        verdict = "report only"
        shown = "" if budget is None else f"{budget} {unit}"
        if key == "first_sync_requests":
            # docs/design/0.14-sync-speed.md §2: ceil(N / 1000) + ceil(bytes / 4 MiB) + 10.
            push_bytes = med.get("first_sync_push_mb", 0) * 1e6
            bound = math.ceil(a.sync_notes / 1000) + math.ceil(push_bytes / (4 * 1024 * 1024)) + 10
            shown = f"≤ {bound}"
            verdict = "ok" if v <= bound else "**over**"
            if v > bound:
                failed.append(f"{label}: {v:.0f} (bound {bound})")
        elif budget is not None and v > budget:
            verdict = "over (report only)"
        table.append(f"| {label} | {shown} | {v:,.2f} {unit} | {verdict} |")
    return "\n".join(table), failed, med


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cli", required=True)
    ap.add_argument("--app")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--app-runs", type=int, default=5)
    ap.add_argument("--suites", default="all")
    ap.add_argument("--json", help="also write the results here")
    ap.add_argument("--server", help="quire-sync-server: adds the sync rows")
    ap.add_argument("--sync-notes", type=int, default=10000)
    ap.add_argument("--sync-direct", action="store_true", help="no counting proxy (no request count)")
    ap.add_argument("--sync-without", default="", help="leave out: join, migration, folder (comma-separated)")
    a = ap.parse_args()

    results = {}

    def add(rows):
        for r in rows:
            results.setdefault((r["suite"], r["metric"]), []).append(float(r["value"]))

    if a.suites != "none":
        add(lines([a.cli, "bench", "--suite", a.suites, "--runs", str(a.runs)]))

    if a.app:
        with tempfile.TemporaryDirectory() as tmp:
            vault = os.path.join(tmp, "vault")
            empty = os.path.join(tmp, "empty")
            os.makedirs(empty)
            subprocess.run([a.cli, "bench", "vault", vault, "10000"], check=True, capture_output=True)
            env = dict(os.environ)
            env["XDG_CONFIG_HOME"] = os.path.join(tmp, "config")
            env["XDG_DATA_HOME"] = os.path.join(tmp, "data")
            env["XDG_CACHE_HOME"] = os.path.join(tmp, "cache")
            env["HOME"] = tmp
            env.pop("WAYLAND_DISPLAY", None)
            # Warm the page cache once (CI measures warm starts, like a person
            # opening the app a second time).
            lines([a.app, vault], env={**env, "QUIRE_BENCH": "startup"}, timeout=120)
            empty_runs = []
            for _ in range(a.app_runs):
                add(lines([a.app, vault], env={**env, "QUIRE_BENCH": "startup"}, timeout=120))
                rows = lines([a.app, empty], env={**env, "QUIRE_BENCH": "startup"}, timeout=120)
                empty_runs += [r["value"] for r in rows]
            if empty_runs:
                results[("startup", "empty_first_frame_ms")] = empty_runs
            for _ in range(max(1, a.app_runs // 2 + 1)):
                add(lines([a.app, vault], env={**env, "QUIRE_BENCH": "typing"}, timeout=300))
                add(lines([a.app], env={**env, "QUIRE_BENCH": "canvas"}, timeout=300))

    med = {k: statistics.median(v) for k, v in results.items() if v}
    full = med.get(("startup", "first_frame_ms"))
    empty = med.get(("startup", "empty_first_frame_ms"))
    if full is not None and empty is not None:
        med[("startup", "vault_cost_ms")] = max(0.0, full - empty)

    failed = []
    table = ["| Measure | Budget | Fails above | Median | |", "|---|---:|---:|---:|---|"]
    for key, (budget, label) in BUDGETS.items():
        if key not in med:
            continue
        v = med[key]
        cap = ABSOLUTE.get(key)
        limit = budget * MARGIN if budget is not None else cap
        ok = limit is None or v <= limit
        if not ok:
            failed.append(f"{label}: {v:.2f} ms (fails above {limit:.1f} ms)")
        unit = "" if key[1] == "passages" else " ms"
        table.append(
            f"| {label} | {'' if budget is None else f'{budget} ms'} | {'' if limit is None else f'{limit:.1f} ms'} "
            f"| {v:.2f}{unit} | {'ok' if ok else '**over**'} |"
        )
    for key, (budget, label) in COUNTS.items():
        if key not in med:
            continue
        v = med[key]
        ok = v <= budget
        if not ok:
            failed.append(f"{label}: {v:.0f} (budget {budget}, exact)")
        table.append(f"| {label} | {budget} | {budget} | {v:.0f} | {'ok' if ok else '**over**'} |")
    missing = [
        BUDGETS[k][1]
        for k in BUDGETS
        if a.suites != "none" and BUDGETS[k][0] is not None and k not in med and (a.app or k[0] not in ("startup", "typing", "canvas"))
    ]
    if a.suites != "none":
        missing += [COUNTS[k][1] for k in COUNTS if k not in med]
    for m in missing:
        failed.append(f"{m}: not measured")
    report = "\n".join(table)
    print(report)
    sync_table = None
    if a.server:
        try:
            sync_table, sync_failed, sync_med = sync_report(a, sync_suite(a))
        except (RuntimeError, OSError, subprocess.SubprocessError) as e:
            sync_table, sync_failed, sync_med = None, [f"sync rows: {e}"], {}
        failed += sync_failed
        med.update({("sync", k): v for k, v in sync_med.items()})
        if sync_table:
            print()
            print(sync_table)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(f"## Speed budgets (design §9), median, fails above budget x {MARGIN}\n\n{report}\n")
            if sync_table:
                f.write(f"\n## Sync (docs/design/0.14-sync-speed.md §2), report only but the request count\n\n{sync_table}\n")
    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            json.dump({f"{k[0]}.{k[1]}": v for k, v in med.items()}, f, indent=2)
    for f in failed:
        print(f"::error::{f}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
