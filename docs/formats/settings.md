# Settings: `<config>/settings.toml` and `device.toml`

Device-local, never in the vault. `<config>` is the platform's config folder
for Quire (`directories::ProjectDirs("dev", "Quire", "Quire")`).

## settings.toml

The 0.8 keys stay at the top level. Each 0.9 area is a table in its own
source file (`crates/quire-app/src/settings/<table>.rs`), owned by its
workstream; every key defaults to off or unchanged.

| Table | Owner | Keys |
|---|---|---|
| `[onboarding]` | WS-D | `ai_done`, `ai_choice` |
| `[ai_local]` | WS-D | `engine`, `embeddings`, `llm`, `ocr`, `embed_model`, `llm_model`, `threads`, `gpu`, `idle_unload_s`, `mirror_url` |
| `[agent_panel]` | WS-D | `enabled`, `backend`, `max_steps`, `acp_command`, `acp_args`, `acp_risk_ack` |
| `[trust]` | WS-C | `contradictions`, `claims_daily_cap`, `ask_before_tell`, `staleness`, `verify_citations` |
| `[mcp_http]` | WS-B | `enabled`, `bind`, `port`, `public_url` |
| `[editing]` | WS-F | `vim`, `tab_mode` (`"reuse"` or `"new"`), `display_titles`, `homepage` (a note's name or path), `two_pane` |
| `[tasks]` | WS-E | `reminders`, `new_occurrence`, `format` |
| `[appearance]` | WS-F | `theme_file` (a bundled theme's name or a file in `<config>/themes/`, [themes.md](themes.md)), `accent` (`#RRGGBB`) |
| `[jobs]` | F0 | `pause_on_typing_ms`, `max_threads` |

* Keys and tables a version doesn't know are kept (`other` at the top level
  and in every table), so 0.8 and 0.9 can share the file in both directions;
  `settings/compat.rs` tests exactly that against the 0.8 struct.
* A new table name must not collide with a top-level key or alias: `editor`
  (fast or classic) and `assistant` (old name of `ai_provider`) are taken.
  A collision would make the file unreadable to older versions. New
  settings go in a table, never at the top level.
* `embeddings` gains the value `"local"`; 0.8 treats it as LSA.

## device.toml

```toml
format = 1
id = "d_7K2M9Q4W8XHZ"
name = "Asha's laptop"
```

Created on first use (`quire_app::device`). The id is random and never
reused; it appears in provenance and the ledger. A file that can't be read
is kept as `device.toml.unreadable` and replaced; one from a newer Quire is
used as it is and not rewritten. Tests never write it.
