# Vault preferences: `.quire/shared/vault.toml`

Preferences that belong to the vault rather than the device: periodic
notes, sort order, icons, tag colours, git, lint, staleness, citations. The
file travels with the vault and may be edited by hand, so Quire rewrites it
with `toml_edit`: comments, key order and sections it doesn't know survive.
Framework owner: F0. Each section's owner: the feature that uses it.

```toml
# Shared settings for this vault
format = 1

[periodic]        # WS-E (quire_plan::periodic::PeriodicPrefs)
[periodic.weekly]
folder = "Weekly"

[sort]            # WS-F: sort mode per folder (editor-state.md)
[order]           # WS-F: manual sort order per folder
[icons]           # WS-F
[tags.colors]     # WS-F
[git]             # WS-F
[lint]            # WS-A
[staleness]       # WS-C
[citations]       # WS-G
[links]           # 0.12 typed links; 0.13 Settings → Links writes it
```

## Copying to another vault

`quire-cli prefs-copy <from-vault> <to-vault> [section]...` replaces sections
of the destination's `vault.toml` with the source's. With no section named it
copies the ones that make sense in any vault: `periodic`, `tags`, `git`,
`lint`, `staleness`, `citations`, `links`, `tasks`. Three `[git]` keys are
never copied, even when `git` is named: `auto_commit`, `pull_on_start` and
`push_after_commit` act on a remote, and copying them could start sending
notes somewhere nobody chose. The destination keeps its own values. `sort`, `order`, `icons`
and `web` are about one vault's own folders and notes: name them to copy
them. Other sections and comments in the destination stay. A destination
written by a newer Quire, or a source that can't be parsed, is left as it is.
Device-wide settings ([settings.md](settings.md)) are not per vault, so there
is nothing to copy for them.

## `[periodic]` (WS-E)

```toml
[periodic.daily]          # also where "Open today's daily note" goes
folder = "Daily"          # default
format = "%Y-%m-%d"       # strftime; default
template = "Daily.md"     # in the templates folder; default
[periodic.weekly]         # defaults: Weekly, %G-W%V (ISO week), no template
[periodic.monthly]        # defaults: Monthly, %Y-%m
[periodic.yearly]         # defaults: Yearly, %Y
```

Every key is optional. A format chrono can't print falls back to the
default. A note's period is found from its folder and name (formats chrono
can't parse back are matched against the 30 years around today).

* Read and write a section with `VaultPrefs::section::<T>("name")` and
  `VaultPrefs::set_section("name", &value)`, where `T` is the feature's own
  serde type with `#[serde(default)]` and `#[serde(flatten)] other:
  toml::Table`. A section that doesn't match its type gives the type's
  defaults; the file is never changed by reading.
* `set_section` keeps the position and comments of keys that stay, and
  removes keys the type doesn't serialize (so keep unknown keys in `other`).
* A file with `format` above 1, or one that can't be parsed, is read as far
  as possible and never saved over (`PrefsError::NewerFormat`,
  `PrefsError::Unreadable`).

## `[links]` (0.12 typed links)

Which link fields make the hierarchy, whether they imply their reverse and
when the breadcrumbs trail shows; since 0.13 also the colour of free-form
types (`[links.types.<name>]`) and Dendron-style names (`[links.dendron]`).
Settings → Links writes it with `VaultPrefs::set_section` and keeps comments,
unknown keys and every other byte of the file; a key still at its default is
not added to a hand-written section. See [links.md](links.md#hierarchy).

## `[lint]` and `[lint.format]` (0.12 switching tools)

`[lint]` holds the vault check's keys (`exclude`, a level per check).
`[lint.format]` holds the rules of the Markdown linter. Quire reads both and
writes `[lint.format]` from Settings → Linter, keeping the other keys. See
[lint-format.md](lint-format.md).

## `[home]` (0.12 switching tools)

The Home page: its sections, pinned notes, the view, the daily word goal and
whether it opens at start. Read, and written by Settings → Linter & Home and
"Pin to Home". See [home.md](home.md).

## `[citations]` (WS-G)

```toml
[citations]
library = "references.bib"        # or a list: ["references.bib", "refs/thesis.bib"]
style = "apa"                     # PDF export: "apa", "chicago-author-date" or "ieee"
watch = "~/Zotero/My Library.bib" # a file outside the vault, merged in when it changes
```

Every key is optional: the library defaults to `references.bib` when it
exists, the style to APA, and nothing is watched. Only Typst's built-in
styles are accepted (no style files). `watch` is checked every few seconds
while Quire runs; on a device where the path doesn't exist nothing happens.
See [citations.md](citations.md).
