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
Comments and layout are kept. Run it where huggingface.co is reachable,
review the diff, and commit it; the release workflow refuses to build
with unpinned entries (`--check`).

    python3 packaging/pin-models.py            # pin what isn't pinned
    python3 packaging/pin-models.py --all      # re-resolve everything
    python3 packaging/pin-models.py --check    # exit 1 if anything is unpinned
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


def main():
    args = set(sys.argv[1:])
    text = CATALOG.read_text(encoding="utf-8")
    catalog = tomllib.loads(text)
    todo = [(m, f) for m in catalog.get("model", []) for f in m.get("files", []) if "--all" in args or not pinned(f)]
    if "--check" in args:
        for m, f in todo:
            print(f"unpinned: {m['id']} / {f['name']}")
        if todo:
            msg = (
                f"{len(todo)} model file(s) in crates/quire-ai/src/catalog.toml have no sha256 and size, "
                "so the on-device models can't be downloaded from this release. On a machine that can reach "
                "huggingface.co, run `python3 packaging/pin-models.py`, review the diff, run "
                "`cargo test -p quire-ai` and commit it before tagging."
            )
            # GitHub Actions shows `::error::` lines on the run's summary page.
            print(f"::error::{msg}" if os.environ.get("GITHUB_ACTIONS") else msg, file=sys.stderr)
        sys.exit(1 if todo else 0)
    results = {}
    revisions = {}
    for m, f in todo:
        print(f"pinning {m['id']} / {f['name']} …", file=sys.stderr)
        new_url, sha, size, commit = pin(f["url"])
        results[f["url"]] = (new_url, sha, size)
        if commit and f is m["files"][0]:
            revisions[m["id"]] = commit
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


if __name__ == "__main__":
    main()
