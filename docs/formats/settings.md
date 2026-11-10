# Settings: `<config>/settings.toml` and `device.toml`

Device-local, never in the vault. `<config>` is the platform's config folder
for Quire (`directories::ProjectDirs("dev", "Quire", "Quire")`).

## settings.toml

The 0.8 keys stay at the top level. Each 0.9 area is a table in its own
source file (`crates/quire-app/src/settings/<table>.rs`), owned by its
workstream; every key defaults to off or unchanged.

| Table | Owner | Keys |
|---|---|---|
| `[onboarding]` | WS-D | `ai_done`, `ai_choice`, `upgrade_card_done` |
| `[ai_local]` | WS-D | `experimental` (off in 0.9.0: every other key is ignored until it is on), `engine`, `embeddings`, `llm`, `ocr`, `embed_model`, `llm_model`, `threads`, `gpu`, `idle_unload_s`, `mirror_url` |
| `[agent_panel]` | WS-D | `enabled`, `backend`, `model` (`""`, `"provider"` or `"device"`), `max_steps`, `acp_command`, `acp_args`, `acp_risk_ack` |
| `[trust]` | WS-C | `contradictions`, `claims_daily_cap`, `ask_before_tell`, `staleness`, `verify_citations` |
| `[mcp_http]` | WS-B | `enabled`, `bind`, `port`, `public_url` |
| `[editing]` | WS-F | `vim`, `vim_ctrl_scroll` (0.12, default on: with Vim keys, Ctrl+B, F, D and U scroll in Normal and Visual mode instead of running Quire's commands; not on macOS, where those are ⌘ keys), `tab_mode` (`"reuse"` or `"new"`), `display_titles`, `homepage` (a note's name or path), `two_pane`, `new_note_folder` (`"current"` or `"root"`) |
| `[tasks]` | WS-E | `reminders`, `new_occurrence`, `format` |
| `[appearance]` | WS-F | `theme_file` (a bundled theme's name or a file in `<config>/themes/`, [themes.md](themes.md)), `accent` (`#RRGGBB`) |
| `[jobs]` | F0 | `pause_on_typing_ms`, `max_threads` |
| `[security]` | 0.11 security | `lock_after_idle_minutes` (default 15; 0 never): unlocked locked folders lock after this long without input |
| `[toolbar]` | 0.11 editor | `show`, `floating`, `[[toolbar.items]]` (the toolbar above the editor) and, from 0.12, `[[toolbar.floating_items]]` (the toolbar next to a selection): each `command` (a command id, `macro:<name>`, `\|` for a separator, `menu:headings`), `icon`, `label`. A missing list is the default buttons; an empty one (`floating_items = []`) none. Design: [0.11-editor.md](../design/0.11-editor.md#12-the-toolbar-toolbar_uirs) |

* Keys and tables a version doesn't know are kept (`other` at the top level
  and in every table), from 0.9 on.
* Quire 0.8.0 reads the file but drops every key it doesn't know when it
  saves. So 0.9 writes `written_by = "0.9"` as the first line, and with every
  save also writes `settings.v09.toml` next to the file (atomically): every
  table and key that isn't a 0.8 key. When `settings.toml` has no
  `written_by` (or an older one), 0.8 saved it last: tables and keys it lacks
  are taken from the sidecar. A file 0.9 wrote is never mixed with the
  sidecar. `settings/compat.rs` tests this against 0.8.0's exact struct.
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
