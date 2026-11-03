# Claims, contradictions, staleness and the timeline (`.quire/cache`)

Everything here is device-local and rebuildable: deleting it costs model
requests or history, never note text. Owner: WS-C. Code:
`quire_intel::{claims, contradict, timeline}`, `quire_app::trust_jobs`.

## `.quire/cache/claims/<extractor>/<h2>/<passage-hash>.json`

The claims an AI model listed for one passage (design §C.5).

```json
{"format": 1, "extractor": "claude-sonnet-4@p1",
 "claims": [{"id": "c_1a2b3c4d5e6f", "text": "The launch is in June 2027.", "span": [12, 48],
             "subject": "launch", "polarity": "positive", "time": "June 2027", "kind": "model"}]}
```

* `extractor` is the model name (characters other than letters, digits,
  `-`, `.`, `_` become `_`) plus `@p` and the prompt version: a new model or
  prompt has its own folder, so nothing stale is reused.
* `passage-hash` is the 32 hex of the content hash of the passage text
  (title and heading line, then the passage), `h2` its first two characters.
* `id` is `c_` + 12 hex of the hash of the claim text (whitespace collapsed,
  lowercased): equal texts are the same claim. `span` is the byte range of the
  exact source text in the passage. `kind` is `model` or `heuristic`.
* An empty `claims` list means the model found none (or replied twice with
  something unreadable); the passage isn't sent again.

## `.quire/cache/contradict/<judge>.jsonl`

One line per judged pair of claims:

```json
{"v": 1, "a": "c_1a2b…", "b": "c_9f8e…", "relation": "contradiction", "confidence": 0.9, "why": "Different launch dates."}
```

`a` ≤ `b` (the pair is sorted, so it is judged once), `relation` is
`contradiction`, `tension`, `compatible` or `unrelated`. `judge` is the
model's extractor key with `@j1p1` (judge prompt version). Readers skip lines
they can't read or whose `v` is newer.

## `.quire/cache/claims/seen.jsonl`

When a passage's claims change, the claims job appends what appeared and what
went (the thinking timeline reads it; design §C.7):

```json
{"v": 1, "claim": "c_1a2b…", "path": "Plan.md", "event": "added", "at": 1793577600000, "text": "The launch is in June 2027."}
```

`event` is `added`, `removed` or `contradicted`. `text` keeps a taken-out
claim readable.

## `.quire/cache/stale.json`

Which version of each note the staleness job last raised, so a dismissed
"Getting stale" item stays away until the note changes:

```json
{"format": 1, "notes": {"Projects/Old plan.md": "1a2b3c…(32 hex)"}}
```

## `[staleness]` in `.quire/shared/vault.toml`

```toml
[staleness]
flag_at = 0.6                 # flag notes scoring at least this
exclude = ["raw/", "Archive"] # never flagged (daily notes and templates never are)
[staleness.folders]
"Projects/" = "90d"           # fresh for 90 days (d, w, m, y)
```

Per note, `review_every: 90d` and `reviewed: 2026-11-03` in front matter win
over the folder; `valid_until: <date>` in the past or `superseded_by: [[Note]]`
flag it at once.
