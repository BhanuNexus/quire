# On-device AI engine protocol

The app talks to `quire-ai-engine` over the engine's stdin and stdout, one
JSON object per line (design §D.3). Owner: WS-D. Types: `quire_ai::protocol`.

* Request: `{"id": 1, "method": "hello", "params": {…}}`.
* Reply: `{"id": 1, "result": {…}}` or `{"id": 1, "error": {"code": 1, "message": "…"}}`.
  `generate` sends `{"id": 1, "event": {…}}` lines before its reply.
* `hello` comes first; anything else before it is refused. The app and
  engine versions must be equal, otherwise the error code is 1.
* **Protocol 2** (0.10) added `transcribe` and the `transcribe` model kind,
  and changed nothing else. An engine accepts `protocol` 1 or 2 in `hello`
  and answers with its own (2); the app asks to transcribe only an engine
  that said 2, and an engine without the method answers `-32601`, which the
  app shows as "this engine can't transcribe".
* Error codes: `-32601` unknown method, `-32602` bad params, `1` version
  mismatch, `2` model not loaded (the app loads it again and retries once),
  `3` cancelled, `4` the model couldn't be loaded, `5` inference failed.

| method | params | result / events |
|---|---|---|
| `hello` | `{app_version, protocol: 2}` (1 is accepted) | `{engine_version, protocol, devices: ["cpu", "metal"], threads}` |
| `load` | `{model, path (the model's folder), sha256, kind: "embed"\|"llm"\|"ocr"\|"transcribe", ctx, idle_unload_s, threads, gpu, options: {pooling, files, repack}}` | `{loaded, mem_bytes}` |
| `embed` | `{model, texts (≤ 64), normalize, prefix}` | `{dims, vectors_f16_b64}` (row-major little-endian f16) |
| `generate` | `{model, system, messages, tools?, max_tokens, temperature, stop?, json}` | events `{"delta": "…"}` and `{"tool_call": {id, name, input}}`, then the reply `{done: true, usage: {input_tokens, output_tokens, stop: "end_turn"\|"tool_use"\|"max_tokens"\|"stop_sequence"}}` |
| `ocr` | `{model_set, image_path}` or `{model_set, pdf: {path, page (0-based), dpi}}` | `{lines: [{text, quad: [[x,y]×4], conf}]}` |
| `transcribe` | `{model, audio_path (a WAV file), language ("en", or "" to detect)}` | `{text, language, seconds}` |
| `status` | `{}` | `{loaded: [{model, kind, mem_bytes}], rss_bytes}` |
| `cancel` | `{id}` (of the request to stop) | `{}`; the stopped request ends with error `3` |
| `unload` | `{model}` | `{}` |
| `shutdown` | `{}` | `{}`, then the engine exits |

`messages` use the app's chat form: `{"role": "user"|"assistant"|"tool"|"system",
"parts": [{"type": "text", "text"} | {"type": "tool_use", "id", "name", "input"} |
{"type": "tool_result", "id", "content", "is_error"}]}`. `tools` are
`{name, description, input_schema}`.

**Behaviour.** Model work runs on one worker thread, so `cancel` and
`status` are answered while a model runs. A model is unloaded after
`idle_unload_s` without use (300 s when not given), and the engine exits
after ten minutes with nothing loaded; the app starts it again when needed.
A panic in model code ends that request with error `5`, not the engine.
The app starts the engine with `nice -n 10` on Unix (when `nice` exists)
and below-normal priority on Windows, restarts it at most twice after a
crash, and kills it when a request goes quiet for two minutes.

**Inside.** Qwen3 GGUF through the engine's own copy of candle's
`quantized_qwen3` (`apps/quire-ai-engine/src/qwen3.rs`: the token table
stays quantized, K-quant weights stay in their GGUF blocks, and only
`Q8_0` weights are repacked into candle's x86 tiles unless `options.repack`
is `"all"` (Settings → AI → Advanced → "Faster prompts, more memory") or
`"never"`; see model-catalog.md for the measurements), with Qwen3's chat
template written out (non-thinking mode) and Hermes-style
`<tool_call>{"name", "arguments"}</tool_call>` blocks read from the stream.

**Tool calls from small models** (0.10). The reply parser repairs what
small models get wrong and nothing more (`quire_ai::toolcall`): single
quotes, a code fence inside the tags, trailing commas, `True`/`None`, bare
keys, words after the object, two calls in one block or a JSON array,
`arguments` as a JSON string or spread beside `name`, a missing closing
`</tool_call>` before the next call, a stray `</think>`, and a reply that is
only a JSON call (bare or fenced) when it names an offered tool. Missing
closing brackets are added only when the model closed the block itself.
Text after a `<tool_response>` the model writes itself is dropped. A block
that still isn't a whole call is sent as `{"tool_call": {"id", "name": "",
"input": {"_unparsed": <raw>, "_error": <why>}}}`; the app never runs it
(nor anything else in that turn) and sends the model one short correction;
a second unreadable call ends the run with a notice.
Fixtures: `apps/quire-ai-engine/tests/fixtures/qwen3-tool-calls.jsonl`.

Whisper (safetensors) through candle's `whisper` for `transcribe`: 16 kHz
mono (other rates are resampled), 30-second windows, greedy decoding
without timestamps, the language given or detected from the first window,
windows the model marks as silence skipped. The mel filter bank is
computed in the engine (Slaney scale, matching librosa's).

BERT (BGE, E5) through candle's `bert` with CLS or mean pooling;
ocrs with rten for OCR, and hayro to render PDF pages. Tokenizers use the
pure-Rust `fancy-regex` backend.

`quire-ai-engine-fake` speaks the same protocol with tiny deterministic
models, for tests and UI checks; it is never shipped.
