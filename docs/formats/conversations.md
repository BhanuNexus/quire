# Agent panel conversations

`.quire/agent/conversations/<id>.json`, device-local, format 1 (design
§D.5). Owner: WS-D. Types: `quire_assistant::conversation`.

```json
{ "format": 1, "id": "c_0J8…", "created": 1793000000000, "updated": 1793000100000,
  "title": "What do my notes say about ropes?",
  "model": "claude-sonnet-5-5",
  "messages": [ { "role": "user", "parts": [ { "type": "text", "text": "…" } ] } ],
  "items": [ { "kind": "user", "text": "…" },
             { "kind": "assistant", "text": "…" },
             { "kind": "tool", "id": "toolu_1", "summary": "Searched notes for *ropes* · 6 results", "detail": "…", "is_error": false },
             { "kind": "proposal", "proposal": "p_0J8…", "path": "Rope Notes.md" },
             { "kind": "applied", "path": "wiki/Ropes.md" },
             { "kind": "notice", "text": "Stopped.", "error": false } ] }
```

* `messages` are what the model sees (the app's chat form, with tool calls
  and results); `items` are what the panel shows.
* Written whole with a temporary file and rename after each message.
  Unknown keys are kept; a file from a newer Quire is never rewritten.
* Deleted 30 days after `updated` (checked when the list is opened).
* "Save as note" writes the transcript to `wiki/conversations/` (or
  `Conversations/` when the vault has no `wiki/`) with front matter
  `type: conversation`, `model`, `ai_written: true` and an `ai` callout.
