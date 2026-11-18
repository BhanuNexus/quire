#!/usr/bin/env python3
"""Fails when a pull request changes source under crates/ or apps/ and not CHANGELOG.md.

Run by CI's `changelog` job (pull requests only):
  python3 -I packaging/check-changelog.py --base SHA --head SHA [--labels JSON] [--actor NAME]

Not counted as source: anything under a `tests/`, `fixtures/` or `testdata/` folder,
`*.md` files (so also every `AGENTS.md`). Passes without a changelog line when the
pull request carries the label `no-changelog` or the actor is `dependabot[bot]`.
Needs the base and head commits in the clone (fetch-depth 0). Tests:
packaging/test-check-changelog.py.
"""

import argparse
import json
import subprocess
import sys

EXEMPT_LABEL = "no-changelog"
EXEMPT_ACTORS = {"dependabot[bot]"}
EXEMPT_DIRS = {"tests", "fixtures", "testdata"}


def is_source(path):
    parts = path.split("/")
    if parts[0] not in ("crates", "apps") or len(parts) < 2:
        return False
    if path.endswith(".md"):
        return False
    return not EXEMPT_DIRS.intersection(parts[:-1])


def verdict(changed, labels, actor):
    """Returns (ok, message) for the changed paths, label names and actor."""
    if actor in EXEMPT_ACTORS:
        return True, f"{actor}: no changelog line needed."
    sources = [p for p in changed if is_source(p)]
    if not sources:
        return True, "No source change under crates/ or apps/: no changelog line needed."
    if EXEMPT_LABEL in labels:
        return True, f"Labelled {EXEMPT_LABEL}."
    if "CHANGELOG.md" in changed:
        return True, "CHANGELOG.md is changed."
    shown = "\n".join(f"  {p}" for p in sources[:10])
    more = f"\n  ... and {len(sources) - 10} more" if len(sources) > 10 else ""
    return False, (
        "This pull request changes source under crates/ or apps/ but not CHANGELOG.md:\n"
        f"{shown}{more}\n"
        "Add one bullet under [Unreleased] (what a person sees, at most two sentences; "
        f"details go in the design note), or label the pull request `{EXEMPT_LABEL}` "
        "and re-run this job (adding a label does not re-run CI). See AGENTS.md."
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--head", required=True)
    ap.add_argument("--labels", default="[]", help="JSON array of label names")
    ap.add_argument("--actor", default="")
    a = ap.parse_args()
    try:
        labels = json.loads(a.labels or "[]")
        if not isinstance(labels, list):
            raise ValueError("not a list")
    except ValueError as e:
        print(f"error: --labels is not a JSON array: {e}", file=sys.stderr)
        return 2
    out = subprocess.run(
        ["git", "diff", "--name-only", f"{a.base}...{a.head}"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        print(f"error: git diff failed (is the history fetched?): {out.stderr}", file=sys.stderr)
        return 2
    ok, msg = verdict(out.stdout.splitlines(), labels, a.actor)
    if ok:
        print(msg)
        return 0
    print(f"::warning title=No changelog line::{msg.splitlines()[0]}")
    print(msg)
    return 1


if __name__ == "__main__":
    sys.exit(main())
