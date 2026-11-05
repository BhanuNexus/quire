# Model catalog, downloads and the local caches

On-device AI (design §D.2). Owner: WS-D. Types: `quire_ai::catalog`,
`quire_ai::download`, `quire_ai::local`.

## The catalog (compiled in)

`crates/quire-ai/src/catalog.toml`, format 1. A catalog with a newer
`format` is refused; keys Quire doesn't know are kept on each entry.

```toml
format = 1
[[model]]
id = "bge-small-en-v1.5"
purpose = "embed"            # embed | llm | ocr | transcribe (the engine is added at build time)
label = "English"            # what Settings calls the choice
model_name = "BGE small (English)"
licence = "MIT"
attribution = ""             # shown next to the model (CC BY-SA asks for it)
ram_mb = 250                 # memory in use (language models: with a prompt of about 2,000 tokens)
min_ram_gb = 0               # language models: recommended computer memory
dims = 384                   # embedders
tier = "balanced"            # language models: small | balanced | better (what the agent panel allows)
pooling = "cls"              # embedders: cls | mean
query_prefix = ""            # embedders: put before search queries ("query: ")
passage_prefix = ""          # … and before passages ("passage: ")
repo = "BAAI/bge-small-en-v1.5"
revision = "<commit>"        # the URLs are pinned to it
[[model.files]]
name = "model.safetensors"   # the file name under <data_dir>/models/<id>/
url = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/<commit>/model.safetensors"
sha256 = "<64 hex>"          # empty: not pinned, so never downloaded or installed
size = 133466304             # bytes; 0 when not pinned
```

**Pinning.** A file downloads only when its `sha256` (64 hex) and `size`
are set. `packaging/pin-models.py` resolves each repository's revision to a
commit through the Hugging Face API, rewrites the URLs to that commit, and
fills in hashes and sizes (hashing files that aren't stored in LFS). The
release workflow refuses to build with unpinned entries.

**The engine** is one more entry, `id = "engine"`, made at build time:
`https://github.com/BhanuNexus/quire/releases/download/v<version>/quire-ai-engine-<target>.gz`.
Its pin comes from `crates/quire-ai/engine.sha256.<target>`, written by the
release workflow before the app is built: `"<sha256 of the .gz> <size> <sha256 of the binary>"`.
Development builds have no pin and use `QUIRE_AI_ENGINE=/path` or an engine
next to the app instead.

## Memory and speed (0.10)

Measured on the build VM (2 shared vCPUs, Intel Xeon 2.1 GHz with AVX-512
VNNI, 8 GB, other builds running: load average 4–6, so speeds vary by about
±30% between runs), `cargo run --release -p quire-ai-engine --example spike
-- <dir> qwen3|qwen3-small|whisper <variant>`, random weights with the real
shapes (WS-D's method: the arithmetic doesn't depend on the values). RSS is
the engine's resident memory; tokens/s for prompts of 16, 256 and 1,024
tokens, then 24 generated tokens.

**Qwen3 1.7B Q4_K_M** (Balanced):

| | candle 0.11 `quantized_qwen3` (0.9) | engine, as shipped (0.10) | engine, everything repacked |
|---|---|---|---|
| Load | 4.7 s | 2.0–2.2 s | 1.5–2.9 s |
| RSS after load | 2,353 MB | 1,191 MB | 1,193 MB |
| after a 16-token prompt | 3,759 MB | 1,193–1,243 MB | 2,600 MB |
| after 256 | 5,149 MB | 1,207–1,257 MB | 3,868 MB |
| after 1,024 (peak) | killed by the OOM killer | 1,652 MB | 4,328 MB |
| Prompt, tokens/s (16 / 256 / 1,024) | 3.1 / 24.8 / — | 3.9–9.1 / 12.3–12.4 / 5.9–8.3 | 3.4–3.5 / 20.3 / 14.3 |
| Generation, tokens/s (after 16 / 256 / 1,024) | 1.82 / 2.46 / — | 1.47–2.32 / 1.34–1.60 / 1.19 | 1.64–1.75 / 1.70 / 1.36 |

Peak memory with a 256-token prompt fell from 5.1 GB to 1.3 GB (a quarter),
from two changes in the engine's copy of candle's model
(`apps/quire-ai-engine/src/qwen3.rs`):

* **The token table stays quantized** and rows are dequantized when looked
  up; Qwen3 ties the output projection to it, so one copy serves both
  (candle kept a 1.2 GB f32 table plus the quantized copy).
* **K-quant weights stay in their GGUF blocks.** Candle repacks Q4_K and
  Q6_K weights into wider x86 tiles on first use, a second copy larger than
  the first (+1.4 GB after the first prompt, +2.7 GB after a long one).
  Without it, prompts are read at about 60% of the speed (12 vs 20 tokens/s
  at 256 tokens) and replies are written at about the same speed (within the
  noise of this VM). "Faster prompts, more memory" (Settings → AI →
  Advanced, `[ai_local] fast_prompts`) brings the repacking back.

The KV cache is kept once (candle kept a second, growing copy for prompts):
about 0.23 MB a token for 0.6B and 1.7B, so an 8,192-token context adds up
to 1.9 GB.

**Qwen3 0.6B Q8_0** (Small):

| | candle 0.11 (0.9) | engine, nothing repacked | engine, as shipped (Q8_0 repacked) |
|---|---|---|---|
| RSS after load | 1,279 MB | 685 MB | 683 MB |
| after 16 / 256 / 1,024 | 1,878 / 2,001 / 2,345 MB | 686 / 744 / 1,256 MB | 1,282 / 1,347 / 1,861 MB |
| Prompt, tokens/s (16 / 256 / 1,024) | 8.0 / 45.0 / 16.8 | 8.6 / 12.2 / 7.9 | 7.3 / 43.2 / 18.4 |
| Generation, tokens/s | 3.61 / 2.75 / 2.76 | 2.04 / 2.03 / 1.55 | 2.63 / 2.48 / 2.03 |

`Q8_0` tiles are the same size as the blocks, and on x86 they read prompts
3–4× and write about 1.3× faster, so the engine still repacks `Q8_0`
weights (the Small model) by default.

**Whisper** (random weights; one 30-second window decoded to the 224-token
limit, which random weights always reach, so this is the worst case; real
speech of 30 seconds is about 60–100 tokens):

| | Load | RSS loaded | Peak RSS | 30 s of audio |
|---|---|---|---|---|
| tiny | 0.4 s | 148 MB | 629 MB | 46 s |
| base | 0.8 s | 279 MB | 1,200 MB | 102 s |
| small | 2.9 s | 921 MB | 3,575 MB | 311 s |

Candle's Whisper decoder re-reads the whole token sequence at each step, so
decoding time grows with the square of the transcript length.

**Size suggestion.** Onboarding suggests Balanced from 8 GB of memory
(rounded; 7 GB reads as 8) and Small below that or when the memory can't be
read. On Windows Quire reads it through PowerShell
(`Get-CimInstance Win32_ComputerSystem`), then `wmic`, without unsafe code.
The catalog's `min_ram_gb`: Small 6, Balanced 8, Better 16.

## Downloads: `<data_dir>/models/`

* `<id>/<name>.part` while downloading (resumed with `Range: bytes=<len>-`;
  a `200` answer restarts it), then fsynced and renamed to `<id>/<name>`
  once its SHA-256 matches. A mismatch deletes it and downloads once more,
  then fails with "checksum mismatch". Pause keeps the `.part`; Cancel
  deletes it.
* The engine's `.gz` is unpacked to `engine/quire-ai-engine` (`.exe` on
  Windows), checked against the binary hash and made executable. Before
  the first start in each session the app hashes the binary again; a
  changed file is not run.
* A mirror URL replaces the scheme and host and keeps the path and hash.
* "Install from file…" hashes each picked file, matches it to a catalog
  file (or the engine binary) and copies it in.
* `installed.toml`, format 1, rewritten whole (temporary file and rename);
  one from a newer Quire is never rewritten:

```toml
format = 1
[models."bge-small-en-v1.5"]
verified = 1793000000000              # Unix ms of the check
[models."bge-small-en-v1.5".files]
"model.safetensors" = "<sha256>"      # what each file was verified against
```

A model counts as installed when every file of its catalog entry is on
disk and recorded here with the hash this build pins (so a catalog with new
hashes asks for a new download).

## Caches in the vault (`.quire/cache/`, rebuildable)

* `vectors-<model>.f16`: on-device embeddings, little-endian. `QVF1`, then
  `dims` (u32), the model id's length (u32) and its UTF-8 bytes, then
  records of a u64 key (the first 8 bytes of SHA-256 over
  `prefix ‖ 0x00 ‖ text`) and `dims` f16 values. A file for another model
  is ignored. Vectors not used during a full rebuild are dropped.
* `ocr/<sha256 of the file>.json`: `{"format": 1, "model": "ocrs", "pages": ["…"]}`.
  A result from another model is ignored.
