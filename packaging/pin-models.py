#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Pin the on-device model catalog (crates/quire-ai/src/catalog.toml).

For every file of every model:

* Hugging Face URLs (`https://huggingface.co/<repo>/resolve/<revision>/<file>`):
  the revision is resolved to its commit through the Hugging Face API and
  the URL is rewritten to that commit; the SHA-256 and size come from the
  API (LFS files) or from downloading the file (small files).
* Other URLs: the file is downloaded and hashed.

The model's `revision` becomes the commit of its first file's repository.
A language model (`purpose = "llm"`) from a repository that isn't Qwen's
own is pinned only with a passing reference check of the same file:
packaging/model-checks/<id>.toml with its `gguf_sha256` and `passed = true`,
written by packaging/check-model-reference.py (docs/design/0.14-local-ai.md
section 3.3); `--check` fails without one.

Comments and layout are kept. Run it where huggingface.co is reachable,
review the diff, and commit it. The release workflow checks for unpinned
entries (`--check`); while on-device AI ships disabled behind its
experimental switch (0.9.0) it only warns (`--check --warn`), see
REQUIRE_MODEL_PINS in .github/workflows/release.yml.

    python3 packaging/pin-models.py                  # pin what isn't pinned
    python3 packaging/pin-models.py --all            # re-resolve everything
    python3 packaging/pin-models.py --check          # exit 1 if anything is unpinned
    python3 packaging/pin-models.py --check --warn   # list them, warn, exit 0
    python3 packaging/pin-models.py --check --allow-unpinned qwen3-1.7b-q4km,whisper-base
                                                     # these rows may stay unpinned

A language model whose GGUF has no passing reference check is left unpinned,
all its files, with the reason printed; the others are pinned (so a release
can ship OCR only when Qwen 3.5 fails its check). `--allow-unpinned` names
rows that `--check` accepts unpinned: rows that stay unpinned can't be
downloaded, so they cost nothing but the feature. The experimental Qwen3 rows
need a passing check like any third-party language model if they are ever
pinned; the release lists them in `--allow-unpinned` instead.

PIN_MODELS_TEST_RESOLVE names a JSON file mapping each URL to
[pinned url, sha256, size, commit], used instead of Hugging Face (the
self-test, packaging/test-pin-models.py).
"""

import hashlib
import json
import os
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

CATALOG = Path(__file__).resolve().parent.parent / "crates" / "quire-ai" / "src" / "catalog.toml"
CHECKS = Path(__file__).resolve().parent / "model-checks"
ROOT = Path(__file__).resolve().parent.parent
# The thresholds of docs/design/0.14-local-ai.md section 3.3, checked here
# again rather than trusting the record's own `passed`.
MAX_KL = 0.05
MIN_TOP1 = 0.90


def engine_compute_version():
    """`ENGINE_COMPUTE_VERSION` of apps/quire-ai-engine/src/lib.rs: what the engine computes
    from a model file, bumped only when those numbers change (not with the release)."""
    src = (ROOT / "apps" / "quire-ai-engine" / "src" / "lib.rs").read_text(encoding="utf-8")
    m = re.search(r"^pub const ENGINE_COMPUTE_VERSION: u32 = (\d+);$", src, re.M)
    if not m:
        raise SystemExit("ENGINE_COMPUTE_VERSION not found in apps/quire-ai-engine/src/lib.rs")
    return int(m.group(1))
HF = re.compile(r"^https://huggingface\.co/(?P<repo>[^/]+/[^/]+)/resolve/(?P<rev>[^/]+)/(?P<file>.+)$")
UA = {"User-Agent": "quire-pin-models"}


def get_json(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60) as r:
        return json.load(r)


def hash_url(url):
    h = hashlib.sha256()
    n = 0
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=120) as r:
        while chunk := r.read(1 << 20):
            h.update(chunk)
            n += len(chunk)
    return h.hexdigest(), n


_repos = {}


def repo_info(repo, rev):
    key = (repo, rev)
    if key not in _repos:
        _repos[key] = get_json(f"https://huggingface.co/api/models/{repo}/revision/{rev}?blobs=true")
    return _repos[key]


def pin(url):
    """(pinned url, sha256, size, commit or None)"""
    if test := os.environ.get("PIN_MODELS_TEST_RESOLVE"):
        new_url, sha, size, commit = json.loads(Path(test).read_text(encoding="utf-8"))[url]
        return new_url, sha, size, commit
    m = HF.match(url)
    if not m:
        sha, size = hash_url(url)
        return url, sha, size, None
    info = repo_info(m["repo"], m["rev"])
    commit = info["sha"]
    new_url = f"https://huggingface.co/{m['repo']}/resolve/{commit}/{m['file']}"
    sib = next((s for s in info.get("siblings", []) if s["rfilename"] == m["file"]), None)
    if sib is None:
        raise SystemExit(f"{m['file']} isn't in {m['repo']} at {m['rev']}")
    lfs = sib.get("lfs")
    if lfs and len(lfs.get("sha256", "")) == 64:
        return new_url, lfs["sha256"], lfs["size"], commit
    sha, size = hash_url(new_url)
    return new_url, sha, size, commit


def pinned(f):
    return len(f.get("sha256", "")) == 64 and f.get("size", 0) > 0


def needs_reference(m):
    """A third-party language model: its file must pass the reference check."""
    return m.get("purpose") == "llm" and not m.get("repo", "").startswith("Qwen/")


def reference_problem(m, sha):
    """Why the model's GGUF (sha256 `sha`) can't be pinned, or None."""
    record = CHECKS / f"{m['id']}.toml"
    if not record.exists():
        return f"{m['id']}: no reference check ({record.relative_to(CATALOG.parents[3])}); run packaging/check-model-reference.py"
    r = tomllib.loads(record.read_text(encoding="utf-8"))
    if r.get("gguf_sha256") != sha:
        return f"{m['id']}: the reference check is of another file ({r.get('gguf_sha256')}, not {sha})"
    if r.get("passed") is not True:
        return f"{m['id']}: the file failed its reference check"
    for text in ("docs", "chat"):
        t = r.get(text, {})
        if t.get("same_ids") is not True:
            return f"{m['id']}: the engine's {text} token ids differ from the reference's"
        kl, top1 = t.get("mean_kl_nats"), t.get("top1_agreement")
        if not isinstance(kl, (int, float)) or not isinstance(top1, (int, float)) or kl > MAX_KL or top1 < MIN_TOP1:
            return f"{m['id']}: {text}: mean KL {kl} (at most {MAX_KL}), top-1 {top1} (at least {MIN_TOP1})"
    if r.get("engine_compute_version") != engine_compute_version():
        return (
            f"{m['id']}: the reference check ran on engine compute version {r.get('engine_compute_version')}, "
            f"this engine is {engine_compute_version()} (its numbers changed); run it again"
        )
    return None


def gguf(m):
    return next((f for f in m.get("files", []) if f["name"].endswith(".gguf")), None)


def allowed_unpinned(argv):
    """The ids after `--allow-unpinned` (comma-separated; the flag may repeat)."""
    out = set()
    for i, a in enumerate(argv):
        if a == "--allow-unpinned" and i + 1 < len(argv):
            out |= {x.strip() for x in argv[i + 1].split(",") if x.strip()}
        elif a.startswith("--allow-unpinned="):
            out |= {x.strip() for x in a.split("=", 1)[1].split(",") if x.strip()}
    return out


def main():
    args = set(sys.argv[1:])
    allowed = allowed_unpinned(sys.argv[1:])
    text = CATALOG.read_text(encoding="utf-8")
    catalog = tomllib.loads(text)
    todo = [(m, f) for m in catalog.get("model", []) for f in m.get("files", []) if "--all" in args or not pinned(f)]
    if "--check" in args:
        for m, f in todo:
            print(f"unpinned{' (allowed)' if m['id'] in allowed else ''}: {m['id']} / {f['name']}")
        todo = [(m, f) for m, f in todo if m["id"] not in allowed]
        warn = "--warn" in args
        unchecked = [p for m in catalog.get("model", []) if needs_reference(m) and (g := gguf(m)) and pinned(g) and (p := reference_problem(m, g["sha256"]))]
        for p in unchecked:
            print(f"::error::{p}" if os.environ.get("GITHUB_ACTIONS") else f"error: {p}", file=sys.stderr)
        if unchecked:
            sys.exit(1)
        if todo and warn:
            msg = (
                f"{len(todo)} model file(s) in crates/quire-ai/src/catalog.toml have no sha256 and size, so their "
                "models show as not available in this build and download nothing. Pin them with "
                "`python3 packaging/pin-models.py` (a third-party language model needs a passing reference check "
                "first), list the rows that may stay unpinned in MODELS_ALLOWED_UNPINNED, and set "
                "REQUIRE_MODEL_PINS to \"true\" in .github/workflows/release.yml to make this an error."
            )
            print(f"::warning::{msg}" if os.environ.get("GITHUB_ACTIONS") else f"warning: {msg}", file=sys.stderr)
        elif todo:
            msg = (
                f"{len(todo)} model file(s) in crates/quire-ai/src/catalog.toml have no sha256 and size, "
                "so the on-device models can't be downloaded from this release. On a machine that can reach "
                "huggingface.co, run `python3 packaging/pin-models.py`, review the diff, run "
                "`cargo test -p quire-ai` and commit it before tagging."
            )
            # GitHub Actions shows `::error::` lines on the run's summary page.
            print(f"::error::{msg}" if os.environ.get("GITHUB_ACTIONS") else msg, file=sys.stderr)
        sys.exit(1 if todo and not warn else 0)
    results = {}
    revisions = {}
    skipped = []
    # Model by model: a language model whose GGUF has no passing reference
    # check is left unpinned, every file of it, and the rest go on.
    for m in catalog.get("model", []):
        files = [f for mm, f in todo if mm is m]
        if not files:
            continue
        found, commit0 = {}, None
        for f in files:
            print(f"pinning {m['id']} / {f['name']} …", file=sys.stderr)
            new_url, sha, size, commit = pin(f["url"])
            found[f["url"]] = (new_url, sha, size)
            if commit and f is m["files"][0]:
                commit0 = commit
        g = gguf(m)
        g_sha = found[g["url"]][1] if g and g["url"] in found else (g or {}).get("sha256", "")
        if needs_reference(m) and g and (problem := reference_problem(m, g_sha)):
            print(f"not pinned: {problem}", file=sys.stderr)
            skipped.append(m["id"])
            continue
        results.update(found)
        if commit0:
            revisions[m["id"]] = commit0
    out = []
    model_id = None
    lines = text.splitlines(keepends=True)
    i = 0
    while i < len(lines):
        line = lines[i]
        s = line.strip()
        if s.startswith("id = "):
            model_id = s.split("=", 1)[1].strip().strip('"')
        if s.startswith("revision = ") and model_id in revisions:
            line = f'revision = "{revisions[model_id]}"\n'
        if s.startswith("url = "):
            url = s.split("=", 1)[1].strip().strip('"')
            if url in results:
                new_url, sha, size = results[url]
                out.append(f'url = "{new_url}"\n')
                out.append(f'sha256 = "{sha}"\n')
                out.append(f"size = {size}\n")
                i += 1
                # Drop the old pin lines that followed.
                while i < len(lines) and lines[i].strip().startswith(("sha256 = ", "size = ")):
                    i += 1
                continue
        out.append(line)
        i += 1
    CATALOG.write_text("".join(out), encoding="utf-8")
    print(f"pinned {len(results)} file(s); review `git diff {CATALOG}`", file=sys.stderr)
    if skipped:
        print(f"left unpinned (no passing reference check): {', '.join(skipped)}", file=sys.stderr)


if __name__ == "__main__":
    main()
