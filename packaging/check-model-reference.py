#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Check a third-party Qwen 3.5 GGUF against Qwen's own weights.

docs/design/0.14-local-ai.md section 3.3: a file's hash proves nothing about
what is in it, so before a pin the engine's next-token distributions on the
GGUF are compared with transformers' on Qwen's bf16 safetensors, over a fixed
text of 4,096 tokens.

    python3 packaging/check-model-reference.py --id qwen3.5-4b-q4km \\
        --gguf-dir <dir with model.gguf> --reference <dir with Qwen's snapshot> \\
        --reference-repo Qwen/Qwen3.5-4B --reference-commit <commit> \\
        --spike target/release/examples/spike [--out packaging/model-checks]

Needs torch, transformers and safetensors, the engine's `spike` example built
in release, and memory for the bf16 model (about 10 GB for 4B, 24 GB for 9B).
It downloads nothing: fetch both snapshots first (the `Qwen 3.5 record`
workflow does).

Method. The text is ARCHITECTURE.md then docs/howto.md, tokenized by the
reference tokenizer and cut to 4,096 tokens; its SHA-256 is recorded. For each
position transformers gives the 40 most likely next tokens (log-softmax in
f32); the engine (`spike qwen35-logits`) gives its log-probabilities for those
40 ids and its own most likely token. KL divergence is estimated on the 40 ids
plus one bucket for the rest of the vocabulary, which can only be lower than
the KL over the whole vocabulary; top-1 agreement is exact.

Pass: mean KL <= 0.05 nats and top-1 agreement >= 90% (fixed in the design
note before any run). The result is written to <out>/<id>.toml either way;
pin-models.py accepts a third-party file only with a passing record of the
same sha256.
"""

import argparse
import datetime
import hashlib
import json
import math
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOKENS = 4096
TOP = 40
MAX_KL = 0.05
MIN_TOP1 = 0.90


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def engine_version():
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
    return cargo["workspace"]["package"]["version"]


def reference_top(ref_dir, ids):
    """transformers' 40 most likely ids and their log-probabilities, per position."""
    import torch
    from transformers import AutoModelForCausalLM

    model = AutoModelForCausalLM.from_pretrained(ref_dir, torch_dtype=torch.bfloat16)
    model.eval()
    out = []
    with torch.no_grad():
        # The text decoder alone (a full forward would hold 4,096 x 248,320 logits).
        hidden = model.get_decoder()(input_ids=torch.tensor([ids])).last_hidden_state[0]
        head = model.get_output_embeddings().weight.float()
        for start in range(0, len(ids), 256):
            logits = hidden[start : start + 256].float() @ head.T
            lp = torch.log_softmax(logits, dim=-1)
            vals, idx = torch.topk(lp, TOP, dim=-1)
            out.extend(zip(idx.tolist(), vals.tolist()))
    return out


def compare(ref, engine):
    kls, agree = [], 0
    for (ids, ref_lp), row in zip(ref, engine):
        p = [math.exp(x) for x in ref_lp]
        q = [math.exp(x) for x in row["want"]]
        rp, rq = max(1.0 - sum(p), 0.0), max(1.0 - sum(q), 1e-12)
        kl = sum(pi * (math.log(pi) - math.log(max(qi, 1e-30))) for pi, qi in zip(p, q) if pi > 0)
        if rp > 0:
            kl += rp * (math.log(rp) - math.log(rq))
        kls.append(kl)
        agree += row["top"][0][0] == ids[0]
    return sum(kls) / len(kls), agree / len(kls)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--id", required=True, help="the catalog row, e.g. qwen3.5-4b-q4km")
    ap.add_argument("--gguf-dir", required=True, type=Path)
    ap.add_argument("--reference", required=True, type=Path, help="local snapshot of Qwen's repository")
    ap.add_argument("--reference-repo", required=True)
    ap.add_argument("--reference-commit", required=True)
    ap.add_argument("--spike", required=True, type=Path, help="the release build of the spike example")
    ap.add_argument("--out", type=Path, default=ROOT / "packaging" / "model-checks")
    args = ap.parse_args()

    from transformers import AutoTokenizer

    text = (ROOT / "ARCHITECTURE.md").read_text() + "\n" + (ROOT / "docs" / "howto.md").read_text()
    tok = AutoTokenizer.from_pretrained(args.reference)
    ids = tok(text)["input_ids"][:TOKENS]
    if len(ids) < TOKENS:
        sys.exit(f"the text has only {len(ids)} tokens")
    used = tok.decode(ids)

    ref = reference_top(args.reference, ids)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "ids.json").write_text(json.dumps(ids))
        (tmp / "want.jsonl").write_text("".join(json.dumps(i) + "\n" for i, _ in ref))
        subprocess.run(
            [str(args.spike), str(args.gguf_dir), "qwen35-logits", str(tmp / "ids.json"), str(tmp / "out.jsonl"), str(tmp / "want.jsonl")],
            check=True,
        )
        engine = [json.loads(line) for line in (tmp / "out.jsonl").read_text().splitlines()]
    if len(engine) != len(ref):
        sys.exit(f"the engine gave {len(engine)} positions for {len(ref)}")
    kl, top1 = compare(ref, engine)
    passed = kl <= MAX_KL and top1 >= MIN_TOP1
    args.out.mkdir(parents=True, exist_ok=True)
    record = (
        f'# Reference check of {args.id} (docs/design/0.14-local-ai.md section 3.3).\n'
        f'gguf_sha256 = "{sha256(args.gguf_dir / "model.gguf")}"\n'
        f'reference_repo = "{args.reference_repo}"\n'
        f'reference_commit = "{args.reference_commit}"\n'
        f'text_sha256 = "{hashlib.sha256(used.encode()).hexdigest()}"\n'
        f"tokens = {len(ids)}\n"
        f"mean_kl_nats = {kl:.5f}\n"
        f"top1_agreement = {top1:.4f}\n"
        f"max_kl_nats = {MAX_KL}\n"
        f"min_top1_agreement = {MIN_TOP1}\n"
        f"passed = {'true' if passed else 'false'}\n"
        f'date = "{datetime.date.today().isoformat()}"\n'
        f'engine_version = "{engine_version()}"\n'
    )
    (args.out / f"{args.id}.toml").write_text(record)
    print(record)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
