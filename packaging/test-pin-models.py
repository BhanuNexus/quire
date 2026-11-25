#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Tests of pin-models.py's reference-check rule (no network).

    python3 -I packaging/test-pin-models.py

A pinned third-party language model passes `--check` only with a record in
packaging/model-checks/<id>.toml of the same sha256 and engine compute version whose
numbers meet the thresholds; Qwen's own repositories and unpinned rows need
none. Each case runs a copy of the script in a temporary tree.
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
SHA = "a" * 64

ROW = """[[model]]
id = "qwen3.5-4b-q4km"
purpose = "llm"
repo = "{repo}"
[[model.files]]
name = "model.gguf"
url = "https://example.invalid/x.gguf"
{pin}
"""

GOOD = {
    "gguf_sha256": f'"{SHA}"',
    "passed": "true",
    "engine_compute_version": "3",
    "docs": {"same_ids": "true", "mean_kl_nats": "0.01", "top1_agreement": "0.97"},
    "chat": {"same_ids": "true", "mean_kl_nats": "0.02", "top1_agreement": "0.95"},
}


def record(r):
    top = "".join(f"{k} = {v}\n" for k, v in r.items() if not isinstance(v, dict))
    tables = "".join(f"[{k}]\n" + "".join(f"{a} = {b}\n" for a, b in v.items()) for k, v in r.items() if isinstance(v, dict))
    return top + tables


def run(repo="unsloth/Qwen3.5-4B-GGUF", pinned=True, rec=None, version="0.14.0"):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "packaging" / "model-checks").mkdir(parents=True)
        (root / "crates" / "quire-ai" / "src").mkdir(parents=True)
        shutil.copy(HERE / "pin-models.py", root / "packaging" / "pin-models.py")
        (root / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
        (root / "apps" / "quire-ai-engine" / "src").mkdir(parents=True)
        (root / "apps" / "quire-ai-engine" / "src" / "lib.rs").write_text("pub const ENGINE_COMPUTE_VERSION: u32 = 3;\n")
        pin = f'sha256 = "{SHA}"\nsize = 1' if pinned else ""
        (root / "crates" / "quire-ai" / "src" / "catalog.toml").write_text(ROW.format(repo=repo, pin=pin))
        if rec is not None:
            (root / "packaging" / "model-checks" / "qwen3.5-4b-q4km.toml").write_text(record(rec))
        p = subprocess.run([sys.executable, "-I", str(root / "packaging" / "pin-models.py"), "--check", "--warn"], capture_output=True, text=True)
        return p.returncode, p.stderr


def changed(**kw):
    r = {k: (dict(v) if isinstance(v, dict) else v) for k, v in GOOD.items()}
    for k, v in kw.items():
        if "." in k:
            t, f = k.split(".")
            r[t][f] = v
        else:
            r[k] = v
    return r


def main():
    cases = [
        ("a passing record", dict(rec=GOOD), 0, ""),
        ("no record", dict(), 1, "no reference check"),
        ("Qwen's own repository needs none", dict(repo="Qwen/Qwen3.5-4B"), 0, ""),
        ("an unpinned row needs none", dict(pinned=False), 0, ""),
        ("another file", dict(rec=changed(gguf_sha256=f'"{"b" * 64}"')), 1, "another file"),
        ("a failed record", dict(rec=changed(passed="false")), 1, "failed"),
        ("passed, but the numbers miss the KL threshold", dict(rec=changed(**{"docs.mean_kl_nats": "0.2"})), 1, "mean KL"),
        ("passed, but top-1 is too low on the chat", dict(rec=changed(**{"chat.top1_agreement": "0.5"})), 1, "top-1"),
        ("different token ids", dict(rec=changed(**{"chat.same_ids": "false"})), 1, "token ids"),
        ("another engine compute version", dict(rec=changed(engine_compute_version="2")), 1, "compute version"),
        ("a release bump alone changes nothing", dict(rec=GOOD, version="0.15.0"), 0, ""),
    ]
    failures = 0
    for name, kw, want, text in cases:
        code, err = run(**kw)
        if code != want or text not in err:
            print(f"FAIL {name}: exit {code}, stderr {err!r}")
            failures += 1
        else:
            print(f"ok   {name}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
