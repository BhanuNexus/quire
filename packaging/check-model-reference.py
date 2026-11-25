#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Check a third-party Qwen 3.5 GGUF against Qwen's own weights.

docs/design/0.14-local-ai.md section 3.3: a file's hash proves nothing about
what is in it, so before a pin the engine's next-token distributions on the
GGUF are compared with transformers' on Qwen's bf16 safetensors.

    python3 packaging/check-model-reference.py --id qwen3.5-4b-q4km \\
        --gguf-dir <dir with model.gguf and tokenizer.json> \\
        --reference <dir with Qwen's snapshot> \\
        --reference-repo Qwen/Qwen3.5-4B --reference-commit <commit> \\
        --spike target/release/examples/spike [--out packaging/model-checks]

Needs torch, transformers and safetensors, the engine's `spike` example built
in release, and memory for the bf16 model (about 10 GB for 4B, 24 GB for 9B).
It downloads nothing: fetch both snapshots first (the `Qwen 3.5 record`
workflow does). `tokenizer.json` in the GGUF directory is the file the engine
will use (Qwen's own, as the catalog row pins it).

Two texts:

* docs: ARCHITECTURE.md then docs/howto.md, tokenized by the reference
  tokenizer and cut to 4,096 tokens.
* chat: packaging/model-checks/reference-chat.json (a tool call, a tool
  result, German, Japanese and Hindi), rendered by transformers'
  `apply_chat_template` and, separately, by the engine's own template.

For each text the engine's token ids must equal the reference tokenizer's
(the engine's template and tokenizer are part of what is checked), then per
position transformers gives the 40 most likely next tokens (log-softmax in
f32) and the engine (`spike qwen35-logits`) its log-probabilities for those
40 ids and its own most likely token. KL divergence is estimated on the 40
ids plus one bucket for the rest of the vocabulary, which can only be lower
than the KL over the whole vocabulary; the record also gives the reference's
probability mass inside its top 40 (how much that bucket holds) and the 99th
percentile of the per-position KL. Top-1 agreement is exact.

Pass, for both texts: identical token ids, mean KL <= 0.05 nats and top-1
agreement >= 90% (fixed in the design note before any run). The result is
written to <out>/<id>.toml either way; pin-models.py accepts a third-party file
only with a passing record of the same sha256 and engine version.
"""

import argparse
import datetime
import hashlib
import importlib.util
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


def versions():
    out = {}
    for name in ("torch", "transformers", "huggingface_hub", "safetensors"):
        try:
            out[name] = __import__(name).__version__
        except ImportError:
            out[name] = "not installed"
    return out


def template_form(conv):
    """The engine's conversation as transformers takes it (same mapping as the template fixtures)."""
    path = ROOT / "packaging" / "qwen35-template-fixtures.py"
    spec = importlib.util.spec_from_file_location("qwen35_template_fixtures", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.to_template_form(conv)


def reference_top(model, ids):
    """transformers' 40 most likely ids and their log-probabilities, per position."""
    import torch

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
    kls, mass, agree = [], [], 0
    for (ids, ref_lp), row in zip(ref, engine):
        p = [math.exp(x) for x in ref_lp]
        q = [math.exp(x) for x in row["want"]]
        rp, rq = max(1.0 - sum(p), 0.0), max(1.0 - sum(q), 1e-12)
        kl = sum(pi * (math.log(pi) - math.log(max(qi, 1e-30))) for pi, qi in zip(p, q) if pi > 0)
        if rp > 0:
            kl += rp * (math.log(rp) - math.log(rq))
        kls.append(kl)
        mass.append(sum(p))
        agree += row["top"][0][0] == ids[0]
    ordered = sorted(kls)
    p99 = ordered[min(len(ordered) - 1, math.ceil(0.99 * len(ordered)) - 1)]
    return {
        "mean_kl_nats": sum(kls) / len(kls),
        "p99_kl_nats": p99,
        "top1_agreement": agree / len(kls),
        "mean_top40_mass": sum(mass) / len(mass),
        "min_top40_mass": min(mass),
    }


def spike(args, *rest):
    subprocess.run([str(args.spike), str(args.gguf_dir), *map(str, rest)], check=True)


def check_text(args, model, name, ref_ids, engine_ids, tmp):
    result = {"tokens": len(ref_ids), "same_ids": engine_ids == ref_ids}
    if not result["same_ids"]:
        first = next((i for i, (a, b) in enumerate(zip(ref_ids, engine_ids)) if a != b), min(len(ref_ids), len(engine_ids)))
        print(f"{name}: the engine's ids differ from the reference tokenizer's at position {first}", file=sys.stderr)
        result["first_difference"] = first
        return result
    ref = reference_top(model, ref_ids)
    (tmp / f"{name}-ids.json").write_text(json.dumps(ref_ids))
    (tmp / f"{name}-want.jsonl").write_text("".join(json.dumps(i) + "\n" for i, _ in ref))
    spike(args, "qwen35-logits", tmp / f"{name}-ids.json", tmp / f"{name}-out.jsonl", tmp / f"{name}-want.jsonl")
    engine = [json.loads(line) for line in (tmp / f"{name}-out.jsonl").read_text().splitlines()]
    if len(engine) != len(ref):
        sys.exit(f"{name}: the engine gave {len(engine)} positions for {len(ref)}")
    result.update(compare(ref, engine))
    return result


def passed(r):
    return r["same_ids"] and r.get("mean_kl_nats", 1e9) <= MAX_KL and r.get("top1_agreement", 0) >= MIN_TOP1


def toml_table(name, r):
    lines = [f"[{name}]"]
    for k, v in r.items():
        if isinstance(v, bool):
            lines.append(f"{k} = {'true' if v else 'false'}")
        elif isinstance(v, float):
            lines.append(f"{k} = {v:.5f}")
        else:
            lines.append(f"{k} = {v}")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--id", required=True, help="the catalog row, e.g. qwen3.5-4b-q4km")
    ap.add_argument("--gguf-dir", required=True, type=Path)
    ap.add_argument("--gguf-url", default="", help="where the file was fetched (recorded)")
    ap.add_argument("--reference", required=True, type=Path, help="local snapshot of Qwen's repository")
    ap.add_argument("--reference-repo", required=True)
    ap.add_argument("--reference-commit", required=True)
    ap.add_argument("--spike", required=True, type=Path, help="the release build of the spike example")
    ap.add_argument("--out", type=Path, default=ROOT / "packaging" / "model-checks")
    args = ap.parse_args()

    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    tok = AutoTokenizer.from_pretrained(args.reference)
    model = AutoModelForCausalLM.from_pretrained(args.reference, torch_dtype=torch.bfloat16)
    model.eval()

    docs = (ROOT / "ARCHITECTURE.md").read_text() + "\n" + (ROOT / "docs" / "howto.md").read_text()
    doc_ids = tok(docs, add_special_tokens=False)["input_ids"][:TOKENS]
    if len(doc_ids) < TOKENS:
        sys.exit(f"the text has only {len(doc_ids)} tokens")
    doc_text = tok.decode(doc_ids)
    # The cut text as both sides read it (decoding and encoding again may
    # differ from the cut ids at the very end).
    doc_ids = tok(doc_text, add_special_tokens=False)["input_ids"]
    conv_path = ROOT / "packaging" / "model-checks" / "reference-chat.json"
    conv = json.loads(conv_path.read_text())
    messages, tools = template_form(conv)
    chat_text = tok.apply_chat_template(messages, tools=tools, tokenize=False, add_generation_prompt=True, enable_thinking=False)
    chat_ids = tok(chat_text, add_special_tokens=False)["input_ids"]

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "docs.txt").write_text(doc_text)
        spike(args, "qwen35-tokenize", "text", tmp / "docs.txt", tmp / "docs-engine.json")
        spike(args, "qwen35-tokenize", "chat", conv_path, tmp / "chat-engine.json")
        docs_r = check_text(args, model, "docs", doc_ids, json.loads((tmp / "docs-engine.json").read_text()), tmp)
        chat_r = check_text(args, model, "chat", chat_ids, json.loads((tmp / "chat-engine.json").read_text()), tmp)

    ok = passed(docs_r) and passed(chat_r)
    lib = versions()
    args.out.mkdir(parents=True, exist_ok=True)
    record = (
        f"# Reference check of {args.id} (docs/design/0.14-local-ai.md section 3.3).\n"
        f'gguf_sha256 = "{sha256(args.gguf_dir / "model.gguf")}"\n'
        f'gguf_url = "{args.gguf_url}"\n'
        f'tokenizer_sha256 = "{sha256(args.gguf_dir / "tokenizer.json")}"\n'
        f'reference_repo = "{args.reference_repo}"\n'
        f'reference_commit = "{args.reference_commit}"\n'
        f'docs_text_sha256 = "{hashlib.sha256(doc_text.encode()).hexdigest()}"\n'
        f'chat_text_sha256 = "{hashlib.sha256(chat_text.encode()).hexdigest()}"\n'
        f"max_kl_nats = {MAX_KL}\n"
        f"min_top1_agreement = {MIN_TOP1}\n"
        f"passed = {'true' if ok else 'false'}\n"
        f'date = "{datetime.date.today().isoformat()}"\n'
        f'engine_version = "{engine_version()}"\n'
        + "".join(f'{k}_version = "{v}"\n' for k, v in lib.items())
        + "\n"
        + toml_table("docs", docs_r)
        + "\n"
        + toml_table("chat", chat_r)
    )
    (args.out / f"{args.id}.toml").write_text(record)
    print(record)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
