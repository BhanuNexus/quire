#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""The design §9 speed budgets on an optimised build (the CI `perf` job).

    packaging/perf-check.py --cli target/perf/quire-cli --app target/perf/quire

`--app` is the desktop built with `--features bench`; it needs a display
(CI runs Xvfb). Each measure is a median: the CLI suites take the median of
`--runs` runs inside one process, the desktop runs are separate processes and
this takes the median of their results. A measure fails when it is over its
budget times MARGIN (CI runners are noisy and shared; see
docs/design/0.10-perf.md). Writes a Markdown table to $GITHUB_STEP_SUMMARY
when that is set, and exits 1 when anything is over.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile

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
    ("pdf", "text_per_page_p50_ms"): (50, "PDF text extraction per page (p50)"),
    ("pdf", "text_per_page_max_ms"): (None, "PDF text extraction per page (worst PDF)"),
    ("mcp-pdfs", "first_answer_200_pdfs_ms"): (500, "MCP start to first search answer, 200 new PDFs"),
    ("mcp-pdfs", "all_read_200_pdfs_ms"): (None, "MCP start until all 200 PDFs are searchable"),
    ("startup", "first_frame_ms"): (None, "Startup to first frame, 10k notes"),
    ("startup", "empty_first_frame_ms"): (None, "Startup to first frame, empty vault"),
    ("startup", "vault_cost_ms"): (20, "Startup: what 10k notes add before the first frame"),
    ("typing", "indexing_p50_ms"): (None, "Keystroke to frame while 10k notes are first indexed (p50)"),
    ("typing", "p50_ms"): (None, "Keystroke to frame, 100 KB note, jobs on (p50)"),
    ("typing", "p95_ms"): (12, "Keystroke to frame, 100 KB note, jobs on (p95)"),
    ("typing", "p99_ms"): (16, "Keystroke to frame, 100 KB note, jobs on (p99)"),
    ("canvas", "frame_p50_ms"): (8, "Canvas, 1,000 cards: pan/zoom frame (p50)"),
    ("canvas", "frame_max_ms"): (None, "Canvas, 1,000 cards: worst frame"),
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cli", required=True)
    ap.add_argument("--app")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--app-runs", type=int, default=5)
    ap.add_argument("--suites", default="all")
    ap.add_argument("--json", help="also write the results here")
    a = ap.parse_args()

    results = {}

    def add(rows):
        for r in rows:
            results.setdefault((r["suite"], r["metric"]), []).append(float(r["value"]))

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
    missing = [BUDGETS[k][1] for k in BUDGETS if BUDGETS[k][0] is not None and k not in med and (a.app or k[0] not in ("startup", "typing", "canvas"))]
    for m in missing:
        failed.append(f"{m}: not measured")
    report = "\n".join(table)
    print(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(f"## Speed budgets (design §9), median, fails above budget x {MARGIN}\n\n{report}\n")
    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            json.dump({f"{k[0]}.{k[1]}": v for k, v in med.items()}, f, indent=2)
    for f in failed:
        print(f"::error::{f}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
