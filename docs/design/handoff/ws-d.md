# WS-D hand-off: on-device AI, providers, assistant, onboarding, wiki loop

What WS-D built, what it touched outside its own files (and why), the
day-1 spike, what is left for a maintainer with network access, and
requests to other workstreams. Newest first.

## Before release (needs a machine that can reach Hugging Face)

1. **Pin the model catalog.** Hugging Face was blocked from the build
   machine (the proxy refused `huggingface.co`, `hf.co`,
   `cdn-lfs.huggingface.co` and `ocrs-models.s3-accelerate.amazonaws.com`),
   so `crates/quire-ai/src/catalog.toml` lists every model with its
   repository and file but no hashes. Unpinned files are never downloaded
   or installed; Settings says "Not available in this build". Run
   `python3 packaging/pin-models.py`, review the diff (revisions become
   commits, every file gets `sha256` and `size`), run
   `cargo test -p quire-ai`, and commit. The release workflow's `check` job
   runs `pin-models.py --check` on tags and fails while anything is unpinned.
   Two file names to confirm when pinning: `bartowski/Qwen_Qwen3-1.7B-GGUF`
   (`Qwen_Qwen3-1.7B-Q4_K_M.gguf`; Qwen's own 1.7B GGUF repository may only
   have Q8_0) and the ocrs `.onnx` names (taken from the ocrs 0.13 example's
   download script); the script stops with a clear error if a file isn't there.
2. **Run the real models once** (the spike below used random weights of the
   real shapes): `quire-cli ai download qwen3-1.7b-q4km`, then the agent
   panel on this computer, then meaning search with `bge-small-en-v1.5`.
   The chat template and tool-call parser are unit-tested against Qwen3's
   documented format, not yet against the real model's output.

## Day-1 spike (task 1)

Measured on the build VM (2 vCPUs, 7 GB RAM, no GPU, other builds running),
`cargo run --release -p quire-ai-engine --example spike`, candle 0.11.1
CPU, random weights with the real shapes (the arithmetic doesn't depend on
the values; real-model numbers to be re-checked per "Before release" 2):

| Model (format) | Load | Memory in use | Prompt (prefill) | Generation |
|---|---|---|---|---|
| Qwen3 1.7B (Q4_K_M) | 3.0 s | 2.3 GB after load, 3.7 GB after the first reply, 5.1 GB after a 256-token prompt | 8–25 tokens/s | 1.9–5.7 tokens/s |
| Qwen3 0.6B (Q8_0) | 1.3 s | 1.3 GB after load, 1.9–2.0 GB in use | 10–53 tokens/s | 4.3–5.8 tokens/s |
| BGE small (F32) | — | 0.65 GB for 64 × 128 tokens; 1.8 GB for 64 × 256 (now sent in batches of 16) | 5 passages/s at 128 tokens, 2/s at 256 | — |

CPU: Intel Xeon @ 2.1 GHz with AVX-512 VNNI, 2 threads, load average
4–5 from the parallel builds (so the speeds are pessimistic). Ranges are
across runs and prompt lengths.

Engine binary (Linux x86-64, release, stripped): **18.5 MB, 7.4 MB
gzipped** (budget: 25 MB compressed). It links candle, tokenizers, ocrs,
rten, hayro and image. Neither app binary links any of them.

**Findings and decision.**

* **Memory is the problem, more than speed.** With candle 0.11 the 1.7B
  model at Q4_K_M uses about 5 GB, not the 1.6 GB the design assumed:
  candle dequantizes the token-embedding table to f32 (1.2 GB for Qwen3's
  152k vocabulary) and, on x86 with AVX2/AVX-512, repacks Q4_K and Q6_K
  weights into expanded tiles on first use (a second, larger copy). There's
  no switch for the repacking. The catalog now records the measured memory
  (`ram_mb`) and recommends 8 GB of RAM for Small, 16 GB for Balanced and
  32 GB for Better (an estimate from the same breakdown); onboarding and
  Settings show those figures, and onboarding suggests Balanced only on
  computers with 16 GB or more (Small otherwise, and where the memory
  can't be read, which today is Windows).
* **Speed** on this VM (2 shared vCPUs) is under the design's 8 tokens/s
  go line for both sizes. This isn't the reference laptop (4 cores, not
  shared); generation is memory-bandwidth bound, so expect roughly 2–4×
  there. **Escalation (per §6.2 task 1):** measure on the reference laptop
  with the real files before release. If 1.7B is under 8 tokens/s or the
  memory is unacceptable, the options are, cheapest first: vendor candle's
  `quantized_qwen3` with an f16 embedding table (−0.6 GB for 1.7B); patch
  candle to skip repacking (memory for speed); or the mistral.rs fallback.
  The engine sits behind one protocol, so any of these stays inside
  `apps/quire-ai-engine`.
* Embedding the first time is slow on weak machines (minutes for a few
  thousand notes); it runs as background work and the f16 cache makes it a
  one-off.

**Tokenizers regex backend: pure Rust (`fancy-regex`).** Qwen3's
pre-tokenizer needs look-ahead (`\s+(?!\S)`). `tokenizers` 0.23 selects
fancy-regex when the `onig` feature is off and its (implicit)
`fancy-regex` feature is on; `unstable_wasm` isn't needed. The engine's
test `the_pure_rust_regex_backend_splits_like_qwen3_expects` checks the
Qwen pattern's splits (spaces before words, runs of spaces, newlines,
single digits, contractions in any case, CJK, special-token text). No C
library is built, and `esaxx_fast` (C++) stays off.

## What exists now

* `quire-ai` (`client`): catalog, downloader (Range resume, pause, cancel,
  hash check with one retry, mirror, install from file, manifest), the
  engine process manager (lazy start, `nice`/below-normal, restart at most
  twice then "gave up" until Settings restarts it, a quiet-request watchdog,
  cancel, status), `LocalEmbedder` (f16 cache), `LocalChat`, `LocalOcr`.
  `build.rs` embeds `engine.sha256.<target>`.
* `quire-ai-engine`: the protocol loop (worker thread, cancel, idle unload,
  exit after ten minutes empty, panics contained), candle Qwen3 + BERT,
  ocrs + hayro, Qwen3 template and tool-call parser; and
  `quire-ai-engine-fake` (tiny deterministic models) for tests and UI checks.
* `quire-intel::llm`: Anthropic and OpenAI-compatible as `ChatModel` with
  tools and SSE streaming (and servers that answer without streaming);
  `LanguageModel` rides on the same call. `chat::ChatLanguageModel` adapts
  any `ChatModel` to the old port (additive; trait signatures unchanged).
* `quire-assistant`: prompts (MCP `PromptDef`s via `prompts::mcp_defs()`),
  the agent loop (`runner`), conversations, ACP bridge, ingest (`wiki`).
* `quire-app`: `ai` (settings → models, downloads, engine status, OCR job,
  `chat_model`/`panel_chat`/`provider_chat` constructors), `onboarding`,
  `assistant_state` (commands, the wiki index/log job, JobCx chat model).
* Desktop: first-run dialog and upgrade card, Settings → AI, the Agent tab,
  the ACP warning and permission dialogs, the Ingest dialog. CLI: `ai`,
  `ingest`.

## Touched outside WS-D's files (smallest possible additions)

| File | Change | Why |
|---|---|---|
| `apps/quire-cli/src/commands/mcp.rs` | `prompts` and `chat` in both `ServerConfig`s | WS-B's request: "map `quire_assistant::prompts::PROMPTS` into it (five lines each)" and "a constructor from settings" (`quire_app::ai::chat_model`) |
| `apps/quire-desktop/src/mcp_ui.rs` | the same in the HTTP host factory (reads settings, since the factory gets no workspace) | same |
| `apps/quire-desktop/src/main.rs` | `mod ai_settings_ui;`, `mod onboarding_ui;` | the new files F0-notes lists for WS-D (`acp_ui.rs` and the new `ingest_ui.rs` are included by `agent_panel_ui.rs` with `#[path]`, as WS-A does for `review_ui.rs`) |
| `apps/quire-desktop/src/registry.rs` | WS-D blocks: `FeatureUi::ai_settings`, Settings tab `ai`, banner `ai-upgrade`, `ui_target` for `AiSetup` (now the first-run dialog), `IngestIntoWiki`, `ConnectExternalAgent`, ten `layout_samples` | registration points |
| `Cargo.toml` markers in quire-app, desktop, CLI | `quire-ai` (client), `quire-assistant` | the markers F0 left |
| `.github/workflows/release.yml` | an `engine` job per target; app jobs download the pins first; publish attests the engine assets and drops the pin files | design §D.3 distribution |
| `Cargo.lock` | candle, tokenizers, ocrs, rten, hayro, image for the engine | the workspace already listed them; nothing added to `[workspace.dependencies]` |
| `docs/formats/README.md`, `settings.md` | rows for WS-D's files and keys | format rules |

`crates/quire-ai/.gitignore` keeps the release workflow's pin files out of
git. `packaging/pin-models.py` is new.

## Deviations from the design (and why)

* **ACP types**: a hand-written JSON-RPC loop over `serde_json::Value`
  instead of `agent-client-protocol-schema`: the bridge uses ten fields,
  and the crate (not in the lock) would add size to both editions.
* **`index.md` while open and dirty**: the job waits until the note is
  saved instead of making a proposal from `job:index` (the watcher's
  three-way merge then takes the update in like any external change; a
  proposal of a generated block would only add review work).
* **"Use for the agent panel"** is the one per-feature choice when both a
  provider and an on-device model are set (`[agent_panel] model`); the
  assist features and jobs use the provider when there is one.
* **Upgrade card** is a bar above the note (`EDITOR_BANNERS`) with Choose…
  and ✕, shown when the tour was done before this version started.
* **OCR of PDFs** reads the pages WS-G's PDF text job found empty (it needs
  their `AttachmentText { by: Embedded }` first); images are read directly.

## Requests

### WS-C (retrieval and trust)

* `ChatModel` and `ScriptedChat` are unchanged. Real providers:
  `quire_app::ai::chat_model(&settings)` (provider, else on-device), and
  `JobCx::chat` is now set from it when a vault opens and when AI settings
  change.
* `LocalEmbedder` sends search queries with the catalog's `query_prefix`
  (`embed`) and passages with `passage_prefix` (`embed_batch`); the vectors
  are normalized.
* `LocalOcr` results go to `Index::upsert_attachment` with
  `TextSource::Ocr` and `source_hash` = SHA-256 of the file.
* Settings → AI → Features draws the `[trust]` keys (Ask style,
  contradictions with the daily cap, staleness, citation checks); nothing
  else of WS-C's is touched.

### WS-G (PDF)

* For scanned PDFs, please send `AttachmentText { by: Embedded, pages }`
  with empty strings for pages without text; WS-D's OCR job fills those
  pages (and re-sends with `by: Ocr`) when OCR is on.

### Integrator

* CI: `cargo test --workspace` now builds candle for `quire-ai-engine`
  (a few minutes more). The engine's own tests use the fake engine.
* Please keep `crates/quire-ai/engine.sha256.*` out of commits (the
  `.gitignore` there does it).
