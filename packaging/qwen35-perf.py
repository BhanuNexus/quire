#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""The Qwen 3.5 rows of the perf budgets (docs/design/0.14-local-ai.md section 2).

    python3 -I packaging/qwen35-perf.py <spike output> [--enforce]

Reads what `spike <dir> qwen35 4b|9b` printed (random weights in the released
shapes) and writes the speed and memory table to $GITHUB_STEP_SUMMARY (or
stdout). Memory has a budget: the engine's peak resident memory after a
2,000-token prompt, at most 3.5 GB for 4B and 7.5 GB for 9B (the frame's
section 5 rows). The rows report only until the frame adopts them; with
`--enforce` a row over its budget fails. Speed has no budget at all (the
owner's decision): it is recorded, never judged.
"""

import os
import sys

# spike prints MiB (VmHWM / 1024); the budgets are 3.5 GB and 7.5 GB.
BUDGET_MB = {"qwen3.5-4b": 3_500_000_000 // 2**20, "qwen3.5-9b": 7_500_000_000 // 2**20}


def rows(text):
    """(model, prompt tokens, reading tok/s, writing tok/s, RSS MB, peak MB) per measured line."""
    out = []
    for line in text.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) == 6 and cells[0] in BUDGET_MB:
            try:
                out.append((cells[0], int(cells[1]), float(cells[2]), float(cells[3]), int(cells[4]), int(cells[5])))
            except ValueError:
                continue
    return out


def report(text):
    machine = next((line for line in text.splitlines() if line.startswith("machine:")), "machine: not recorded")
    table = [machine, "", "| Model | Prompt | Reading tok/s | Writing tok/s | Peak RSS | Budget | |", "|---|---:|---:|---:|---:|---:|---|"]
    over = []
    for model, prompt, read, write, _rss, peak in rows(text):
        budget = BUDGET_MB[model] if prompt == 2000 else None
        ok = budget is None or peak <= budget
        if not ok:
            over.append(f"{model}: peak {peak} MiB after {prompt} tokens (budget {budget} MiB)")
        mark = "" if budget is None else ("ok" if ok else "**over**")
        table.append(f"| {model} | {prompt} | {read:.2f} | {write:.2f} | {peak} MiB | {'' if budget is None else f'{budget} MiB'} | {mark} |")
    return "\n".join(table) + "\n", over


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    text = open(args[0], encoding="utf-8").read()
    table, over = report(text)
    if not rows(text):
        sys.exit("no Qwen 3.5 rows in the spike output")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(table)
    print(table)
    for o in over:
        print(("::warning::" if os.environ.get("GITHUB_ACTIONS") else "warning: ") + o, file=sys.stderr)
    sys.exit(1 if over and "--enforce" in args else 0)


if __name__ == "__main__":
    main()
