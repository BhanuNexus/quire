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

import json
import os
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
    failures += pinning()
    sys.exit(1 if failures else 0)


TWO_ROWS = """[[model]]
id = "qwen3.5-4b-q4km"
purpose = "llm"
repo = "unsloth/Qwen3.5-4B-GGUF"
revision = "main"
[[model.files]]
name = "model.gguf"
url = "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf"
[[model.files]]
name = "tokenizer.json"
url = "https://huggingface.co/Qwen/Qwen3.5-4B/resolve/main/tokenizer.json"

[[model]]
id = "ocrs"
purpose = "ocr"
repo = "robertknight/ocrs-models"
revision = "ocrs-0.13"
[[model.files]]
name = "text-detection.onnx"
url = "https://example.invalid/text-detection.onnx"
"""

RESOLVE = {
    "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf": [
        "https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/" + "c" * 40 + "/Qwen3.5-4B-Q4_K_M.gguf", SHA, 2740000000, "c" * 40],
    "https://huggingface.co/Qwen/Qwen3.5-4B/resolve/main/tokenizer.json": [
        "https://huggingface.co/Qwen/Qwen3.5-4B/resolve/" + "d" * 40 + "/tokenizer.json", "e" * 64, 12000000, "d" * 40],
    "https://example.invalid/text-detection.onnx": ["https://example.invalid/text-detection.onnx", "f" * 64, 2500000, None],
}


def pin_run(rec, *argv):
    """Pins a two-row catalog (Qwen 3.5 4B and OCR) with the URLs resolved by
    RESOLVE instead of Hugging Face; returns (exit, catalog after, stderr)."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "packaging" / "model-checks").mkdir(parents=True)
        (root / "crates" / "quire-ai" / "src").mkdir(parents=True)
        (root / "apps" / "quire-ai-engine" / "src").mkdir(parents=True)
        shutil.copy(HERE / "pin-models.py", root / "packaging" / "pin-models.py")
        (root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.14.0"\n')
        (root / "apps" / "quire-ai-engine" / "src" / "lib.rs").write_text("pub const ENGINE_COMPUTE_VERSION: u32 = 3;\n")
        catalog = root / "crates" / "quire-ai" / "src" / "catalog.toml"
        catalog.write_text(TWO_ROWS)
        if rec is not None:
            (root / "packaging" / "model-checks" / "qwen3.5-4b-q4km.toml").write_text(record(rec))
        resolve = root / "resolve.json"
        resolve.write_text(json.dumps(RESOLVE))
        env = {"PIN_MODELS_TEST_RESOLVE": str(resolve), "PATH": os.environ.get("PATH", "")}
        script = str(root / "packaging" / "pin-models.py")
        p = subprocess.run([sys.executable, "-I", script], capture_output=True, text=True, env=env)
        after = catalog.read_text()
        checks = [
            subprocess.run([sys.executable, "-I", script, "--check", *a], capture_output=True, text=True, env=env).returncode
            for a in argv
        ]
        return p.returncode, after, p.stderr, checks


def pinning():
    failures = 0

    def expect(name, ok, detail=""):
        nonlocal failures
        print(f"{'ok  ' if ok else 'FAIL'} {name}{'' if ok else ': ' + detail}")
        failures += not ok

    # No passing record for Qwen 3.5: OCR is pinned, Qwen 3.5 (every file) is not.
    code, after, err, checks = pin_run(None, [], ["--allow-unpinned", "qwen3.5-4b-q4km"], ["--allow-unpinned=other"])
    expect("a model without a reference check is left unpinned, the others are pinned", code == 0 and 'sha256 = "' + "f" * 64 + '"' in after, err)
    expect("…all of its files", after.count("sha256") == 1 and "resolve/main/Qwen3.5-4B-Q4_K_M.gguf" in after and "resolve/main/tokenizer.json" in after, after)
    expect("…with the reason printed", "not pinned: qwen3.5-4b-q4km: no reference check" in err and "left unpinned" in err, err)
    expect("--check fails while it is unpinned", checks[0] == 1)
    expect("--check --allow-unpinned names it: passes (OCR only)", checks[1] == 0)
    expect("--allow-unpinned for another row doesn't cover it", checks[2] == 1)
    # A passing record of the resolved file: both are pinned, URLs at their commits.
    code, after, err, checks = pin_run(GOOD, [])
    expect("with a passing record both are pinned", code == 0 and after.count("sha256") == 3 and "resolve/" + "c" * 40 in after, err + after)
    expect("…and --check passes", checks[0] == 0)
    return failures


if __name__ == "__main__":
    main()
