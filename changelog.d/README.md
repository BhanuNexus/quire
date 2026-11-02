# Changelog fragments

While 0.9 is built in parallel, nobody edits `CHANGELOG.md` directly. Each
workstream writes its user-visible changes as fragments here, and the
fragments are merged into `CHANGELOG.md` at release (design 0.9 §6.3).

* One file per workstream or topic: `ws-a.md`, `ws-b.md`, …, `f0.md`. Add to
  your own file; never edit another workstream's.
* Use the CHANGELOG's headings (`### Added`, `### Changed`, `### Fixed`) and
  its style: what the user notices, in plain words, one bullet per change.
* Write nothing for internal changes. If a change alters behaviour on
  purpose (for example MCP `--allow-write` making proposals), say so under
  `### Changed`.
* At release the fragments are merged under the version's heading, grouped by
  heading, and this folder is emptied except for this file.
