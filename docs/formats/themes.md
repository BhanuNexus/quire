# Theme files: `<config>/themes/*.toml`

A theme sets Quire's colours for light and dark mode, and optionally its
fonts. Themes are data: colours and file names, no expressions, no imports,
nothing that runs. Quire comes with a few (Paper, Nord, Solarized, High
contrast); your own go in the `themes` folder of Quire's settings folder
(Settings → Appearance → Open themes folder). Owner: WS-F
(`quire_app::themes`, `apps/quire-desktop/src/theme.rs`).

```toml
format = 1

[light]
bg = "#FDFCFA"
text = "#26292F"
accent = "#2A67E0"
h1 = "#5B2E14"

[dark]
bg = "#15171B"
text = "#D9DCE1"

[fonts]           # TTF or OTF files in <config>/fonts/
ui = "Inter.ttf"          # menus, panels and dialogs
text = "Literata.ttf"     # the text of notes
mono = "Iosevka.ttf"      # code
```

* **Colours** are `#RGB`, `#RRGGBB` or `#RRGGBBAA`.
* **Tokens** (the same on both sides): `bg`, `panel`, `raised` (menus and
  dialogs), `text`, `strong` (bold text), `muted`, `faint`, `accent`, `link`,
  `tag`, `tag_bg`, `border`, `selection`, `hover`, `active_bg` (the chosen
  row), `mark_bg` (==highlights== and find matches), `code_bg`, `h1` to `h6`,
  `quote`, `conflict`, `warn`, `danger`, `added` (suggested text in review,
  added lines in diffs). A token a theme leaves out keeps
  the built-in colour; `link` follows `accent` when only `accent` is set.
  Settings → Appearance → New theme from these colours writes every token.
* **Forgiving.** Unknown tables and keys are ignored (a theme written for a
  newer Quire still works), a value that isn't a colour is skipped with a
  notice, and a file that can't be read at all leaves the built-in theme in
  place with a notice. A `format` above 1 is read as far as it goes.
* **Fonts** name files in `<config>/fonts/` (the extension may be left out).
  A font that can't be read is skipped. Bold and italic text keep the
  built-in faces. A `NotoEmoji-Regular.ttf` in the fonts folder is used for
  icons on systems without a monochrome emoji font.
* **Accent colour** (`[appearance] accent` in settings.toml) overrides the
  theme's accent, and tints links that follow it, selections and the chosen
  row.
* Which theme is in use is `[appearance] theme_file` in settings.toml: a
  bundled theme's name (`"Nord"`) or a file name (`"My theme.toml"`). Empty
  means the built-in colours.
