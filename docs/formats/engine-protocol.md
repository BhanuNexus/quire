# On-device AI engine protocol

The app talks to `quire-ai-engine` over the engine's stdin and stdout, one
JSON object per line. Owner: WS-D. Types: `quire_ai::protocol`.

* Request: `{"id": 1, "method": "hello", "params": {…}}`.
* Reply: `{"id": 1, "result": {…}}` or `{"id": 1, "error": {"code": 1, "message": "…"}}`.
  Streams (`generate`) send `{"id": 1, "event": {…}}` lines first.
* `hello` comes first: `{"app_version": "0.9.0", "protocol": 1}` →
  `{"engine_version", "protocol", "devices": ["cpu"], "threads"}`. The app
  and engine versions must be equal; otherwise the error code is 1.
* Error codes: `-32601` method not in this build, `-32602` bad params,
  `1` version mismatch, `2` model not loaded, `3` cancelled.

| method | params | result / events |
|---|---|---|
| `hello` | `{app_version, protocol}` | `{engine_version, protocol, devices, threads}` |
| `load` | `{model, path, sha256, kind: "embed"\|"llm"\|"ocr", ctx}` | `{loaded, mem_bytes}` |
| `embed` | `{model, texts (≤ 64), normalize, prefix}` | `{dims, vectors_f16_b64}` |
| `generate` | `{model, messages, tools?, max_tokens, temperature, stop?, json?}` | events `{delta}`, `{tool_call}`, then `{done, usage}` |
| `ocr` | `{model_set, image_path \| pdf: {path, page, dpi}}` | `{lines: [{text, quad, conf}]}` |
| `cancel` / `unload` / `shutdown` | `{id}` / `{model}` / `{}` | `{}` |

Today the engine answers `hello` and `shutdown`; everything else is `-32601`.
