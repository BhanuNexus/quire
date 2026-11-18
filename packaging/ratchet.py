#!/usr/bin/env python3
"""The ratchet: counts of rules that cannot be at zero yet, and may only go down.

Reads the JSON that clippy prints (`--message-format=json`) when run with the
ratchet's own configuration (packaging/ratchet/clippy.toml, through
CLIPPY_CONF_DIR) over the non-test code of the workspace, counts each
`disallowed_methods` hit by rule, prints a table (and appends it to the file
named by --summary, for the job summary), and compares it with the committed
baseline, packaging/ratchet.toml.

  * a count above its baseline: exit 1 (a new use crept in);
  * a count below its baseline: exit 0, and says to lower the baseline (a
    one-line edit of packaging/ratchet.toml);
  * unusable input (no clippy output, a path that no rule owns): exit 2.

Usage: ratchet.py CLIPPY_JSON [--baseline FILE] [--summary FILE]
Run the clippy step yourself, as .github/workflows/ratchet.yml does:

  CLIPPY_CONF_DIR=packaging/ratchet cargo clippy --workspace --lib --bins --locked \\
    --message-format=json -- -A clippy::all -A clippy::pedantic -A clippy::nursery \\
    -W clippy::disallowed_methods > clippy.json
"""

import argparse
import json
import os
import re
import sys
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))

# Rule -> (what it counts, the method paths that count towards it). The paths
# are the ones in ratchet/clippy.toml, as clippy prints them in its message.
RULES = {
    "unwrap_or_default": ("Result::unwrap_or_default (a failed read becomes empty data)", ["core::result::Result::unwrap_or_default"]),
    "wall_clock": ("wall-clock calls (SystemTime::now, Utc::now, Local::now)", ["std::time::SystemTime::now", "chrono::Utc::now", "chrono::Local::now"]),
    "env_var": ("std::env::var", ["std::env::var"]),
}
RULE_OF = {path: rule for rule, (_, paths) in RULES.items() for path in paths}
MESSAGE = re.compile(r"^use of a disallowed method `([^`]+)`")


def die(message, code=2):
    print(f"ratchet: {message}", file=sys.stderr)
    sys.exit(code)


def count(lines):
    """({rule: {(file, line, column)}}, {rule: {crate: n}}, artifacts seen) from clippy's JSON lines."""
    hits = {rule: set() for rule in RULES}
    crate_of = {}
    artifacts = 0
    for raw in lines:
        raw = raw.strip()
        if not raw.startswith("{"):
            continue
        try:
            obj = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if obj.get("reason") == "compiler-artifact":
            artifacts += 1
            continue
        if obj.get("reason") != "compiler-message":
            continue
        msg = obj.get("message") or {}
        m = MESSAGE.match(msg.get("message", ""))
        if not m or (msg.get("code") or {}).get("code") != "clippy::disallowed_methods":
            continue
        path = m.group(1)
        if path not in RULE_OF:
            die(f"clippy reports `{path}`, which no rule in ratchet.py owns (is ratchet/clippy.toml out of step?)")
        span = next((s for s in msg.get("spans", []) if s.get("is_primary")), None)
        if span is None:
            continue
        # The same source line is seen again from each target that compiles it.
        key = (span["file_name"], span["line_start"], span["column_start"])
        rule = RULE_OF[path]
        if key not in hits[rule]:
            hits[rule].add(key)
            crate = re.match(r"((?:crates|apps)/[^/]+)/", span["file_name"])
            per = crate_of.setdefault(rule, {})
            per[crate.group(1) if crate else "(other)"] = per.get(crate.group(1) if crate else "(other)", 0) + 1
    return hits, crate_of, artifacts


def load_baseline(path):
    with open(path, "rb") as f:
        data = tomllib.load(f)
    base = data.get("baseline", {})
    missing = [r for r in RULES if not isinstance(base.get(r), int)]
    extra = [k for k in base if k not in RULES]
    if missing or extra:
        die(f"{path}: [baseline] needs an integer for each of {sorted(RULES)} and nothing else (missing {missing}, unknown {extra})")
    return {r: base[r] for r in RULES}


def report(counts, baseline, crate_of):
    """(markdown table, exit status, messages)."""
    rows = ["| Rule | Count | Baseline | |", "|---|---:|---:|---|"]
    status, notes = 0, []
    for rule, (what, _) in RULES.items():
        n, b = counts[rule], baseline[rule]
        if n > b:
            verdict = f"**{n - b} more than the baseline**"
            status = 1
            notes.append(f"{rule}: {n} > {b}. A new use crept in: remove it, or (if it is right) say why with #[expect] where it applies.")
        elif n < b:
            verdict = f"{b - n} fewer: lower the baseline"
            notes.append(f"{rule}: {n} < {b}. Lower `{rule}` to {n} in packaging/ratchet.toml (one line)." + (" It is 0: move the rule into clippy.toml." if n == 0 else ""))
        else:
            verdict = "at the baseline"
        rows.append(f"| {what} | {n} | {b} | {verdict} |")
    detail = []
    for rule in RULES:
        per = crate_of.get(rule, {})
        if per:
            top = ", ".join(f"{c} {k}" for c, k in sorted(per.items(), key=lambda kv: (-kv[1], kv[0])))
            detail.append(f"- `{rule}`: {top}")
    text = "\n".join(rows)
    if detail:
        text += "\n\nBy crate:\n" + "\n".join(detail)
    return text, status, notes


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("clippy_json")
    ap.add_argument("--baseline", default=os.path.join(HERE, "ratchet.toml"))
    ap.add_argument("--summary", help="append the table to this file (GITHUB_STEP_SUMMARY)")
    args = ap.parse_args(argv)
    try:
        with open(args.clippy_json, encoding="utf-8", errors="replace") as f:
            hits, crate_of, artifacts = count(f)
    except OSError as e:
        die(str(e))
    if artifacts == 0:
        die("no compiler-artifact lines in the clippy output: did the build run, with --message-format=json?")
    counts = {rule: len(keys) for rule, keys in hits.items()}
    text, status, notes = report(counts, load_baseline(args.baseline), crate_of)
    print(text)
    for n in notes:
        print(n)
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as f:
            f.write("## Ratchet\n\n" + text + "\n" + "".join(f"\n{n}\n" for n in notes))
    return status


if __name__ == "__main__":
    sys.exit(main())
