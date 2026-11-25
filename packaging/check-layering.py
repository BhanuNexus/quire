#!/usr/bin/env python3
"""Checks the workspace's crate layering. Builds nothing.

The ONE layer table is the crate table in ARCHITECTURE.md (section 3): its
"Layer" column gives every workspace crate a number, and the "Same-layer edges"
list under it names the only dependencies allowed between crates of one layer.
This script reads that table and compares it with the real dependency graph from
`cargo metadata --no-deps --format-version 1 --offline`. It fails when:

  1. a workspace crate is missing from the table, or the table lists a crate that
     does not exist (a PLANNED crate may be absent), or a path is wrong;
  2. a normal or build dependency points at the same or a higher layer, except
     a named same-layer edge or a TEMPORARY_EDGES entry;
  3. a crate other than the apps (`apps/*`) and `quire-app` turns on another
     workspace crate's optional features (except FEATURE_ALLOW);
  4. the table's "Depends on" column differs from the real graph;
  5. a workspace crate depends on a crate in BANNED_DEPS (dates are
     quire_base::dates since 0.14: docs/design/0.14-jiff.md);
  6. a shipped package (every crate under apps/) reaches a crate in BANNED_DEPS
     through any crate, a third-party one too, in either edition (default
     features and --no-default-features) on any target: `cargo tree -e
     normal,build -i <crate>` must find nothing. Skipped with --metadata (the
     self-tests fake cargo instead).

Dev-dependencies are exempt from 2 to 6 (chrono stays as a test oracle).

Usage: check-layering.py [--architecture ARCHITECTURE.md] [--metadata FILE]
Without --metadata it runs cargo metadata in the current directory.
Exit status: 0 ok, 1 violations, 2 unusable input.
"""

import argparse
import json
import os
import re
import subprocess
import sys

# Crates that the table may list before they exist (a branch is adding them).
# Edges to them are left out of the "Depends on" comparison, so the check
# passes before and after the crate lands. Remove the entry when it has landed
# and the table lists its edges.
PLANNED: set[str] = set()

# Outside crates no workspace crate may use in its code (normal and build
# dependencies), with what to use instead.
BANNED_DEPS: dict[str, str] = {"chrono": "quire_base::dates (jiff)", "chrono-tz": "quire_base::dates (jiff)"}

# Edges that break the layer rule today and are on their way out. The edge is
# optional in the "Depends on" comparison, so the check passes with or without it.
TEMPORARY_EDGES: set[tuple[str, str]] = set()

# (crate, dependency, feature): a library crate that turns on an optional
# feature of another workspace crate, kept until it is removed.
FEATURE_ALLOW = {
    # quire-assistant picks quire-ai's HTTP client; remove if the assistant gets
    # the client another way (its own feature, or quire-ai's default).
    ("quire-assistant", "quire-ai", "client"),
    # quire-base's `net` feature is its HTTP client builder: a crate turns it on
    # for its own network code. Which crates may have network code is cargo-deny's
    # ureq list (deny.toml), not this check.
    ("quire-import", "quire-base", "net"),
}

# Crates (besides apps/*) that may choose another crate's features.
FEATURE_CHOOSERS = {"quire-app"}


def fail_input(message):
    print(f"check-layering: {message}", file=sys.stderr)
    sys.exit(2)


def parse_architecture(text):
    """Returns (rows, same_layer) from ARCHITECTURE.md's text.

    rows: {crate: (layer, path, [deps])}; same_layer: {(a, b)}."""
    rows = {}
    for line in text.splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) != 5 or not cells[0].isdigit():
            continue
        layer, crate, path, _what, deps = cells
        crate = crate.strip("`")
        path = path.strip("`")
        deps = deps.strip()
        dep_list = [] if deps in ("none", "") else [d.strip().strip("`") for d in deps.split(",")]
        if crate in rows:
            fail_input(f"ARCHITECTURE.md lists {crate} twice")
        rows[crate] = (int(layer), path, dep_list)
    same = set()
    for m in re.finditer(r"^- `([a-z0-9-]+)` → `([a-z0-9-]+)`", text, re.M):
        same.add((m.group(1), m.group(2)))
    return rows, same


def load_graph(meta):
    """Returns (members, edges, features, paths).

    members: {name}; edges: {(a, b)} of normal and build deps between members;
    features: {(a, b, feature)} of explicitly requested non-default features."""
    root = meta["workspace_root"]
    pkgs = {p["name"]: p for p in meta["packages"]}
    members = set(pkgs)
    edges = set()
    feats = set()
    paths = {}
    for name, p in pkgs.items():
        folder = os.path.dirname(p["manifest_path"])
        paths[name] = os.path.relpath(folder, root).replace(os.sep, "/")
        for d in p["dependencies"]:
            if d["name"] not in members or d["kind"] == "dev":
                continue
            edges.add((name, d["name"]))
            target = pkgs[d["name"]]
            default = default_closure(target)
            for f in d.get("features") or []:
                if f not in default:
                    feats.add((name, d["name"], f))
    return members, edges, feats, paths


def default_closure(pkg):
    """The features that are on by default in `pkg` (transitively, own features)."""
    table = pkg.get("features", {})
    seen = set()
    todo = list(table.get("default", []))
    while todo:
        f = todo.pop()
        if f in seen:
            continue
        seen.add(f)
        todo.extend(x for x in table.get(f, []) if "/" not in x and not x.startswith("dep:"))
    return seen


def check(meta, architecture_text):
    rows, same = parse_architecture(architecture_text)
    if not rows:
        fail_input("no crate table found in ARCHITECTURE.md (rows: | layer | crate | path | what | depends on |)")
    members, edges, feats, paths = load_graph(meta)
    errors = []

    for crate in sorted(members - set(rows)):
        errors.append(f"{crate} is a workspace member but has no row in ARCHITECTURE.md's layer table")
    for crate in sorted(set(rows) - members - PLANNED):
        errors.append(f"ARCHITECTURE.md lists {crate}, which is not a workspace member")
    for crate in sorted(members & set(rows)):
        if rows[crate][1] != paths[crate]:
            errors.append(f"{crate}: ARCHITECTURE.md says path {rows[crate][1]}, the manifest is in {paths[crate]}")

    layer = {c: r[0] for c, r in rows.items()}
    for a, b in sorted(edges):
        if a not in layer or b not in layer:
            continue  # already reported
        if layer[b] < layer[a]:
            continue
        if layer[b] == layer[a] and (a, b) in same:
            continue
        if (a, b) in TEMPORARY_EDGES:
            continue
        kind = "the same layer" if layer[b] == layer[a] else "a higher layer"
        errors.append(
            f"{a} (layer {layer[a]}) depends on {b} (layer {layer[b]}), {kind}: "
            "a crate may depend only on lower layers. Move the code down, or "
            "(for a same-layer edge that is really needed) name it under the table in ARCHITECTURE.md"
        )
    for a, b in sorted(same):
        if a in layer and b in layer and layer[a] != layer[b]:
            errors.append(f"same-layer edge {a} -> {b} in ARCHITECTURE.md joins layers {layer[a]} and {layer[b]}")

    for a, b, f in sorted(feats):
        is_app = paths.get(a, "").startswith("apps/")
        if is_app or a in FEATURE_CHOOSERS or (a, b, f) in FEATURE_ALLOW:
            continue
        errors.append(
            f"{a} turns on feature `{f}` of {b}: only apps/* and quire-app choose another "
            "crate's optional features. Make it a feature of the app, or default in the crate"
        )

    def comparable(deps_of):
        out = {}
        for a, deps in deps_of.items():
            out[a] = sorted(
                d for d in deps if d not in PLANNED and (a, d) not in TEMPORARY_EDGES
            )
        return out

    real = {c: [] for c in members}
    for a, b in edges:
        real[a].append(b)
    table = {c: rows[c][2] for c in members & set(rows)}
    real_c = comparable({c: real[c] for c in table})
    table_c = comparable(table)
    for crate in sorted(table):
        if real_c[crate] != table_c[crate]:
            missing = sorted(set(real_c[crate]) - set(table_c[crate]))
            extra = sorted(set(table_c[crate]) - set(real_c[crate]))
            parts = []
            if missing:
                parts.append("missing from the table: " + ", ".join(missing))
            if extra:
                parts.append("in the table but not in Cargo.toml: " + ", ".join(extra))
            errors.append(f"{crate}: ARCHITECTURE.md's 'Depends on' differs from cargo metadata ({'; '.join(parts)})")

    for pkg in meta.get("packages", []):
        if pkg.get("name") not in members:
            continue
        for dep in pkg.get("dependencies", []):
            if dep.get("name") in BANNED_DEPS and dep.get("kind") != "dev":
                errors.append(f"{pkg['name']} depends on {dep['name']}: use {BANNED_DEPS[dep['name']]} (a dev-dependency may stay, for an oracle test)")
    return errors


def shipped(meta):
    """The workspace packages under apps/: what Quire ships."""
    root = meta.get("workspace_root", "").rstrip("/") + "/apps/"
    return sorted(p["name"] for p in meta.get("packages", []) if p.get("manifest_path", "").startswith(root))


def run_cargo(args):
    out = subprocess.run(["cargo", *args], capture_output=True, text=True)
    return out.returncode, out.stdout, out.stderr


def tree_violations(packages, run=run_cargo):
    """Rule 6: `cargo tree -i` of every banned crate, per shipped package and edition."""
    errors = []
    for pkg in packages:
        for edition in ([], ["--no-default-features"]):
            for banned in BANNED_DEPS:
                args = ["tree", "--locked", "--target", "all", "-e", "normal,build", "-p", pkg, *edition, "-i", banned]
                code, out, err = run(args)
                label = f"{pkg} ({edition[0]})" if edition else pkg
                if code == 0 and out.strip():
                    errors.append(f"{label} reaches {banned}: use {BANNED_DEPS[banned]}. The path:\n{out.strip()}")
                elif code != 0 and "did not match any packages" not in err:
                    errors.append(f"cargo tree failed for {label} and {banned}: {err.strip()}")
    return errors


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--architecture", default="ARCHITECTURE.md")
    ap.add_argument("--metadata", help="a saved `cargo metadata --no-deps` JSON file (default: run cargo)")
    args = ap.parse_args(argv)
    try:
        with open(args.architecture, encoding="utf-8") as f:
            text = f.read()
    except OSError as e:
        fail_input(f"cannot read {args.architecture}: {e}")
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
        detail = getattr(e, "stderr", "") or ""
        fail_input(f"cannot get cargo metadata: {e} {detail}")
    errors = check(meta, text)
    if not args.metadata:
        errors += tree_violations(shipped(meta))
    for e in errors:
        print(f"error: {e}")
    if errors:
        print(f"check-layering: {len(errors)} problem(s). The layer table is in ARCHITECTURE.md section 3.")
        return 1
    print("check-layering: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
