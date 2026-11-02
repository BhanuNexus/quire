# Editor state: positions, sort order, icons, tag colours, git

Owner: WS-F (`quire_app::nav`, `quire_app::themes`, `quire_app::git`).

## session.json

`<config>/session.json` (device-local) gains `positions`: where you were in
the last 500 notes you opened, so a note opens there again even after its
tab was closed. Paths are vault-relative; the positions belong to the vault
named by `vault` in the same file and are dropped when another vault's
session is saved.

```json
{
  "vault": "/home/asha/Notes",
  "tabs": [{"path": "Projects/Plan.md", "caret": 812}],
  "positions": {
    "Projects/Plan.md": {"caret": 812, "scroll": 31.0, "at": 1793990000000}
  }
}
```

* `caret` is a character index, `scroll` the line at the top of the view
  (fractions are allowed), `at` when it was last updated (Unix ms; the
  oldest go first).
* Keys a version doesn't know are kept when the file is written.

## vault.toml sections

These travel with the vault (`.quire/shared/vault.toml`, see
[vault-prefs.md](vault-prefs.md)). Folder keys end in `/`; `"/"` is the
vault's top folder.

```toml
[sort]                       # how a folder's notes are ordered in the sidebar
"/" = "name"                 # name (default), modified, created or manual
"Projects/" = "manual"

[order]                      # manual order: file names, first to last
"Projects/" = ["Plan.md", "Budget.md", "Ideas.md"]

[icons]                      # folder icons (emoji); note icons are `icon:` in front matter
"Projects/" = "🗂"

[tags.colors]                # tag colours; `project` also colours `project/x`
project = "#4C8BF5"

[git]
auto_commit = false          # commit by itself after a quiet spell
commit_after_idle_s = 600    # how quiet (no edits, no outside changes)
message = "Quire: {{count}} notes"
pull_on_start = false        # off while Quire Sync is on for the vault
push_after_commit = false    # off while Quire Sync is on for the vault
```

* Notes missing from an `[order]` list come after the listed ones, by
  name. Names that no longer exist are ignored (and dropped the next time
  the order changes).
* Keys these sections don't know are kept (`[tags]` and `[git]` keep any
  other keys; the others are plain maps).
* Git: Quire commits notes, attachments and `.quire/shared/` (never the rest
  of `.quire/`), runs the system `git` with prompts off and a 60-second
  limit, and suggests a `.gitignore` of `.quire/*` with `!.quire/shared/`.
