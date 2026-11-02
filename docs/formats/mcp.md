# MCP over HTTP: tokens, connection requests, approval keys

Files in the config folder (`<config>`: `~/.config/Quire` on Linux,
`~/Library/Application Support/dev.Quire.Quire` on macOS,
`%APPDATA%\Quire\Quire\config` on Windows) that the MCP server over HTTP
uses (design 0.9 §4.5, §4.6). None of them is in a vault, and none is
needed on another device. Owner: WS-B.

## `<config>/mcp/tokens.json`

Personal tokens (`quire-cli mcp token create`, Settings → Connections) and
the OAuth tokens Quire's authorization server issued. Written atomically
with mode 0600, under the lock file `<config>/mcp/tokens.lock` so the app
and `quire-cli` can share it. **Tokens themselves are never stored**, only
their SHA-256.

```json
{
  "format": 1,
  "tokens": [
    { "hash": "9f2c…(64 hex)", "kind": "personal", "name": "laptop-script", "client": "",
      "actor": "token:laptop-script", "scopes": ["notes:read", "notes:propose"],
      "resource": "", "created": 1793577600000, "last_used": 1793577700000,
      "expires": 1801353600000, "family": "x8Kq…", "used": false },
    { "hash": "…", "kind": "access", "name": "Claude", "client": "https://claude.ai/oauth/mcp.json",
      "actor": "oauth:claude.ai/oauth/mcp.json", "scopes": ["notes:read", "notes:propose"],
      "resource": "http://127.0.0.1:7311/mcp", "created": …, "expires": …, "family": "…" },
    { "hash": "…", "kind": "refresh", … , "used": false }
  ]
}
```

| Field | Meaning |
|---|---|
| `kind` | `personal`, `access` (1 hour) or `refresh` (30 days, works once) |
| `name` | the token's name, or the app's `client_name` |
| `client` | the OAuth `client_id` (a URL); empty for personal tokens |
| `actor` | who the agent acts as in provenance, the ledger and policy |
| `scopes` | `notes:read`, `notes:propose`, `notes:write`, `notes:ask` |
| `resource` | the MCP endpoint an OAuth token is bound to (audience) |
| `family` | tokens issued and rotated together; revoking one revokes the family |
| `used` | a refresh token already exchanged; presenting it again revokes the family |

Tokens look like `qmcp_` + 43 base64url characters (256 random bits).
Expired and used entries are dropped when the file is next written.

## `<config>/mcp/consent/<id>.json`

A request from an app to connect over HTTP, waiting for the person (the
app's dialog, or `quire-cli mcp approve <id>` / `deny <id>`). Written by the
server process; decided by whichever process the person uses. Mode 0600.
Expired after 10 minutes, deleted after a day.

```json
{ "format": 1, "id": "aSgMgXohZO0HCuan", "created": 1793643007543, "expires": 1793643607543,
  "client_id": "https://app.example/client.json", "client_name": "Example Agent", "client_host": "app.example",
  "redirect_uri": "http://127.0.0.1:54321/callback", "scopes": ["notes:read", "notes:propose"],
  "state": "pending", "decided_at": null, "resource": "http://127.0.0.1:7311/mcp",
  "code_challenge": "…", "oauth_state": "…", "actor": "oauth:app.example/client.json" }
```

`state` is `pending`, `approved`, `denied` or `used` (the authorization code
was issued; an approval yields one code). Nothing here is secret: the code
itself lives only in the server's memory for 60 seconds.

## `<config>/vaults/<vault-key>/mrtr.key`

32 random bytes (mode 0600): the per-vault key that seals the
`requestState` of in-client approvals (XChaCha20-Poly1305). Deleting it only
invalidates approvals in flight. The `<vault-key>` is defined in
[README.md](README.md).

## `<config>/vaults/<vault-key>/state.toml` (read)

The MCP server reads `approved_instructions = "sha256:…"` (whether
`AGENTS.md` may be served) and `[agent] log_reads` (whether reads are
logged; on unless `false`). The file and its writer belong to WS-A.

## `.quire/cache/served/<hex>.md.gz` (vault, cache)

The texts served to agents, gzipped and named by their content hash, so an
edit made against an older version can be merged three-way. At most 2,000
entries or 64 MB, 7 days each; safe to delete.
