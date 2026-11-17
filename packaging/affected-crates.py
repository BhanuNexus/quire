#!/usr/bin/env python3
"""Which workspace crates does a change affect? Builds nothing.

CI runs this on pull requests to decide what to test. It prints the crates whose
files changed, plus every workspace crate that depends on them (normal, build and
dev dependencies, so a change that breaks a dependent's tests is caught).

Rules:
  * A change under a crate's `tests/`, `benches/` or `examples/` counts for that
    crate alone: nothing depends on those files.
  * A file a crate reads from outside its own folder counts for that crate, found
    by scanning the sources: `include_str!`/`include_bytes!`/`include!` (the crate
    and its dependents) and string literals with `..` on a line that names
    `CARGO_MANIFEST_DIR` (the crate alone: those are test fixtures).
  * A Markdown file inside a crate (for example a nested `AGENTS.md`) affects
    nothing, unless a source file includes it or it is a test fixture (under
    `tests/`, `testdata/` or `fixtures/`).
  * The root `Cargo.toml`, `Cargo.lock`, `rust-toolchain`/`rust-toolchain.toml`,
    `.cargo/` and the workflow files (`.github/workflows/`) mean everything.
  * Everything else (docs, packaging, `.claude/`, ...) affects no crate.

Usage:
  affected-crates.py [--base REV --head REV | --files FILE] [--metadata FILE]
                     [--format lines|github]

  --base/--head  diff of `git diff --name-only BASE...HEAD` (merge-base form: what
                 the pull request changes)
  --files FILE   a list of changed paths, one per line ("-" for stdin)
  --metadata     a saved `cargo metadata --no-deps` JSON file; default: run cargo
  --format       `lines` (default): the crate names, one per line;
                 `github`: key=value lines for $GITHUB_OUTPUT (see below)

`github` outputs: everything, crates (space-separated names; empty for none),
rust (true when a crate is affected or the lint configuration changed, so the
lint and perf jobs have work), packaging (true when packaging/ or a workflow
changed), lite (quire-desktop or quire-cli affected), pdf (quire-pdf affected),
eval (quire-cli affected).
"""

import argparse
import json
import os
import re
import subprocess
import sys

EVERYTHING_FILES = {"Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml"}
EVERYTHING_PREFIXES = (".cargo/", ".github/workflows/")
# Change the lint and perf jobs' result without changing any crate.
LINT_CONFIG = {"clippy.toml", "rustfmt.toml", "packaging/perf-check.py"}
LOCAL_DIRS = ("tests", "benches", "examples")

INCLUDE_RE = re.compile(r'\binclude(?:_str|_bytes)?!\s*\(\s*"([^"\n]+)"')
LITERAL_RE = re.compile(r'"([^"\n]*\.\.[^"\n]*)"')


def die(message, code=2):
    print(f"affected-crates: {message}", file=sys.stderr)
    sys.exit(code)


def crate_dirs(meta):
    root = meta["workspace_root"]
    return {
        p["name"]: os.path.relpath(os.path.dirname(p["manifest_path"]), root).replace(os.sep, "/")
        for p in meta["packages"]
    }


def external_inputs(root, dirs):
    """Returns ({path: crate} for includes, {path: crate} for manifest-dir reads,
    {path} of files included from inside their own crate).

    The first two keep only paths outside the reading crate's own folder."""
    includes, reads, internal = {}, {}, set()
    for crate, d in dirs.items():
        base = os.path.join(root, d)
        for folder, _subdirs, files in os.walk(base):
            if os.sep + "target" + os.sep in folder + os.sep:
                continue
            for name in files:
                if not name.endswith(".rs"):
                    continue
                path = os.path.join(folder, name)
                try:
                    with open(path, encoding="utf-8", errors="replace") as f:
                        lines = f.read().splitlines()
                except OSError:
                    continue
                for line in lines:
                    for lit in INCLUDE_RE.findall(line):
                        target = os.path.normpath(os.path.join(folder, lit))
                        internal.add(os.path.relpath(target, root).replace(os.sep, "/"))
                        add(includes, root, d, target, crate)
                    if "CARGO_MANIFEST_DIR" in line:
                        for lit in LITERAL_RE.findall(line):
                            target = os.path.normpath(os.path.join(base, lit.lstrip("/")))
                            add(reads, root, d, target, crate)
    return includes, reads, internal


def add(table, root, crate_dir, target, crate):
    rel = os.path.relpath(target, root).replace(os.sep, "/")
    if rel.startswith("..") or rel == crate_dir or rel.startswith(crate_dir + "/"):
        return
    table.setdefault(rel, set()).add(crate)


def under(path, prefix):
    return path == prefix or path.startswith(prefix + "/")


def affected(meta, changed, root=None):
    """Returns (crates, everything, lint_config, packaging)."""
    root = root or meta["workspace_root"]
    dirs = crate_dirs(meta)
    members = set(dirs)
    deps = {
        p["name"]: {d["name"] for d in p["dependencies"] if d["name"] in members}
        for p in meta["packages"]
    }
    changed = sorted({c.strip().strip('"') for c in changed if c.strip()})
    everything = any(c in EVERYTHING_FILES or c.startswith(EVERYTHING_PREFIXES) for c in changed)
    lint_config = any(c in LINT_CONFIG for c in changed)
    packaging = any(c.startswith("packaging/") or c.startswith(".github/workflows/") for c in changed)
    if everything:
        return sorted(members), True, True, True

    includes, reads, internal = external_inputs(root, dirs)
    hit, local = set(), set()
    for c in changed:
        owner = None
        for crate, d in dirs.items():
            if under(c, d) and (owner is None or len(d) > len(dirs[owner])):
                owner = crate
        if owner:
            parts = c[len(dirs[owner]) + 1 :].split("/")
            doc_only = (
                c.endswith(".md")
                and c not in internal
                and parts[0] not in LOCAL_DIRS
                and "testdata" not in parts
                and "fixtures" not in parts
            )
            if not doc_only:
                (local if parts[0] in LOCAL_DIRS else hit).add(owner)
        for prefix, crates in includes.items():
            if under(c, prefix):
                hit |= crates
        for prefix, crates in reads.items():
            if under(c, prefix):
                local |= crates
    grow = True
    while grow:
        grow = False
        for name in members - hit:
            if deps[name] & hit:
                hit.add(name)
                grow = True
    return sorted(hit | local), False, lint_config, packaging


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base")
    ap.add_argument("--head")
    ap.add_argument("--files")
    ap.add_argument("--metadata")
    ap.add_argument("--format", choices=["lines", "github"], default="lines")
    args = ap.parse_args(argv)

    if args.files:
        if args.files == "-":
            text = sys.stdin.read()
        else:
            with open(args.files, encoding="utf-8") as f:
                text = f.read()
        changed = text.splitlines()
    elif args.base and args.head:
        out = subprocess.run(
            ["git", "diff", "--name-only", f"{args.base}...{args.head}"],
            check=False, capture_output=True, text=True,
        )
        if out.returncode != 0:
            die(f"git diff failed: {out.stderr.strip()}")
        changed = out.stdout.splitlines()
    else:
        die("give --base and --head, or --files")

    try:
        if args.metadata:
            with open(args.metadata, encoding="utf-8") as f:
                meta = json.load(f)
        else:
            out = subprocess.run(
                ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
                check=True, capture_output=True, text=True,
            )
            meta = json.loads(out.stdout)
    except (OSError, ValueError, subprocess.CalledProcessError) as e:
        die(f"cannot get cargo metadata: {e} {getattr(e, 'stderr', '') or ''}")

    crates, everything, lint_config, packaging = affected(meta, changed)
    if args.format == "lines":
        print("\n".join(crates))
        return 0
    flag = lambda b: "true" if b else "false"  # noqa: E731
    print(f"everything={flag(everything)}")
    print(f"crates={' '.join(crates)}")
    print(f"rust={flag(bool(crates) or lint_config)}")
    print(f"packaging={flag(packaging)}")
    print(f"lite={flag('quire-desktop' in crates or 'quire-cli' in crates)}")
    print(f"pdf={flag('quire-pdf' in crates)}")
    print(f"eval={flag('quire-cli' in crates)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
