# Agent state on this device: `<config>/vaults/<key>/`

What this device approved for one vault, and its per-vault preferences
(design §A.2, P7). `<key>` is the vault key: 16 hex of FNV-1a over the
lower-cased canonical vault root. Moving a vault folder resets approvals,
which is the safe direction. Owner: WS-A. Types:
`quire_agent::approval`.

## `state.toml`

```toml
format = 1
approved_policy = "sha256:…"        # hash of the approved .quire/shared/agent-policy.toml
approved_instructions = "sha256:…"  # hash of the approved AGENTS.md / CLAUDE.md
log_reads = false                   # log which notes agents read (ledger `read` events)

[snoozed]                           # inbox fingerprint → Unix ms
3fa1c0d2b4e59a71 = 1793086400000
```

* Unknown keys are kept. A file with `format` above 1 is read for what this
  version knows and never rewritten.
* Written atomically (temporary file, then rename).

## Copies of what was approved

* `approved-policy.toml`: the approved policy file's text. It is the
  baseline while a changed file waits for approval (see
  [policy.md](policy.md)), and what the change is compared with.
* `approved-instructions.md`: the approved instructions' text, to show
  what changed.

A copy is used only when its hash matches `state.toml`.

## Agent instructions

The first of `AGENTS.md`, `CLAUDE.md` at the vault root (any case) is the
vault's instructions. While its hash isn't approved here, MCP serves
"Vault instructions changed outside Quire on this device and await review"
instead of its text, and the Inbox asks for approval. Approving happens in
the Inbox or Settings → Agents; saving the file from Quire's editor, or
creating it with "Create agent instructions", approves it too.
