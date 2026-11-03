# On-device AI engine protocol

The app talks to `quire-ai-engine` over the engine's stdin and stdout, one
JSON object per line (design §D.3). Owner: WS-D. Types: `quire_ai::protocol`.

* Request: `{"id": 1, "method": "hello", "params": {…}}`.
* Reply: `{"id": 1, "result": {…}}` or `{"id": 1, "error": {"code": 1, "message": "…"}}`.
  `generate` sends `{"id": 1, "event": {…}}` lines before its reply.
* `hello` comes first; anything else before it is refused. The app and
  engine versions must be equal, otherwise the error code is 1.
* Error codes: `-32601` unknown method, `-32602` bad params, `1` version
  mismatch, `2` model not loaded (the app loads it again and retries once),
  `3` cancelled, `4` the model couldn't be loaded, `5` inference failed.

| method | params | result / events |
|---|---|---|
| `hello` | `{app_version, protocol: 1}` | `{engine_version, protocol, devices: ["cpu", "metal"], threads}` |
| `load` | `{model, path (the model's folder), sha256, kind: "embed"\|"llm"\|"ocr", ctx, idle_unload_s, threads, gpu, options: {pooling, files}}` | `{loaded, mem_bytes}` |
| `embed` | `{model, texts (≤ 64), normalize, prefix}` | `{dims, vectors_f16_b64}` (row-major little-endian f16) |
| `generate` | `{model, system, messages, tools?, max_tokens, temperature, stop?, json}` | events `{"delta": "…"}` and `{"tool_call": {id, name, input}}`, then the reply `{done: true, usage: {input_tokens, output_tokens, stop: "end_turn"\|"tool_use"\|"max_tokens"\|"stop_sequence"}}` |
| `ocr` | `{model_set, image_path}` or `{model_set, pdf: {path, page (0-based), dpi}}` | `{lines: [{text, quad: [[x,y]×4], conf}]}` |
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

**Inside.** Qwen3 GGUF through candle's `quantized_qwen3`, with Qwen3's
chat template written out (non-thinking mode) and Hermes-style
`<tool_call>{"name", "arguments"}</tool_call>` blocks parsed from the
stream; BERT (BGE, E5) through candle's `bert` with CLS or mean pooling;
ocrs with rten for OCR, and hayro to render PDF pages. Tokenizers use the
pure-Rust `fancy-regex` backend.

`quire-ai-engine-fake` speaks the same protocol with tiny deterministic
models, for tests and UI checks; it is never shipped.
