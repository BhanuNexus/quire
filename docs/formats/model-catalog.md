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
purpose = "embed"            # embed | llm | ocr (the engine is added at build time)
label = "English"            # what Settings calls the choice
model_name = "BGE small (English)"
licence = "MIT"
attribution = ""             # shown next to the model (CC BY-SA asks for it)
ram_mb = 250                 # memory in use when loaded
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
