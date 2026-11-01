# Quire — Competitive analysis & roadmap

*Prepared 1 November 2026. Competitor facts come from their changelogs and recent reviews (sources at the end); prices change, so re-check before quoting them publicly.*

---

## 1. Where Quire stands today

**Strengths**

* Native Rust, no Electron. The editor stays smooth on a 10 MB, 100k-line note (median 0.5 ms of UI work per frame).
* Plain Markdown files in a folder. It opens Obsidian vaults as they are.
* Useful intelligence with **no network and no AI credits**: related notes, link and tag suggestions, search by meaning, Ask-your-notes with citations, inline calculator, natural dates.
* Wikilinks, backlinks, rename-with-link-rewrite, multi-cursor, regex find & replace across the vault, image paste, templates, daily notes, CLI.

**Gaps that block adoption**

* No mobile app and no sync of its own.
* No structured views over notes (tables, kanban, calendar).
* No importers.
* No plugins.
* The fast editor isn't accessible to screen readers yet.
* Neural search is not shipped; the semantic index is LSA.

---

## 2. Competitive landscape

| App | Positioning | Storage | Notable in 2026 | AI | Price | Weak spots |
|---|---|---|---|---|---|---|
| **Obsidian** | Power users' local-first PKM | Markdown folder | **Bases**: database views over notes, with table/cards/list and now **kanban** layouts. **Command-line interface** (1.12). Colored highlights, image resize | Plugins only | Free app; Sync $4/mo; Publish $8/site/mo; commercial licence now optional | Electron; plugin quality varies; setup burden |
| **Notion** | All-in-one team workspace | Cloud database | **Notion Agent** for multi-step tasks; **Custom Agents** on schedules or triggers, billed in credits | Central | Plus $10, Business $20 /member/mo; agent credits extra | AI pricing feels steep and unpredictable; some users find AI intrusive; weak offline |
| **Logseq** | Outliner + daily notes | Split in 2026: **Logseq OG** (Markdown, maintenance-only) and **Logseq** (database, main line) | E2E-encrypted sync; real-time collaboration planned; headless CLI | Limited | Free; sync currently via sponsor invites | Users of the Markdown version are stranded on a frozen branch |
| **Bear** | Beautiful Apple-only writing | Proprietary SQLite | **CLI + MCP server** (2.8) so AI tools can read and edit notes. OCR in images and PDFs, note encryption | Via MCP | $29.99/yr | Apple-only; no plain files; the MCP/CLI shipped **enabled by default**, which drew privacy criticism |
| **Apple Notes** | Free default | iCloud | iOS 26 adds **Markdown import/export**, but doesn't style Markdown | Apple Intelligence | Free | Not a Markdown editor; Apple-only |
| **AFFiNE** | Open-source Notion + Miro | Local-first + optional server | **Importers** for OneNote, Obsidian, Bear and Notion; calendar views; **bring-your-own AI key**; whiteboards | BYOK (experimental) | Free / self-host | Breaking sync changes between versions |
| **Capacities** | Object-based PKM | Cloud | Custom object types, smart queries, tasks, calendar | Pro | Free; Pro ~$9.99/mo | Reviews mention slowness on large notes and mobile issues |
| **Tana** | Supertags + meetings | Cloud | **Botless meeting capture**; agents that draft issues and CRM updates during calls; MCP + API | Central | Pro ~$20–30/user/mo | Expensive; cloud-only; steep learning curve |
| **Heptabase** | Visual thinking on whiteboards | Cloud + offline | Cards on whiteboards, PDF highlights → cards, AI tutor and research agent, Readwise/Zotero, **MCP + CLI**, BYOK | Central | $8.99–53.99/mo | Price; proprietary format |
| **Reflect** | Daily-notes + AI | **Reflect Open**: local Markdown, iCloud or Git sync | Free Mac app; BYOK AI that answers with links to sources | BYOK | Mac free; mobile paid | Mac-first; E2E encryption only in the legacy app |

### Trends that matter for Quire

1. **AI tools want access to your notes.** Bear, Heptabase, Tana and Obsidian (CLI) all expose notes to agents. Bear's backlash shows it must be **opt-in and scoped**.
2. **Structure on top of notes.** Obsidian Bases, Tana supertags and Capacities objects all add queries and views over front matter. Obsidian keeps them in plain-text `.base` files.
3. **Bring-your-own AI key replaces bundled credits** (Reflect, AFFiNE, Heptabase). Notion's credit model is a common complaint.
4. **The local-first Markdown crowd is restless.** Logseq froze its Markdown version. Reflect went open-source on local files. Apple Notes added Markdown export. Each one sends users looking for a new home.
5. **Everyone has mobile.** It's the single biggest table-stakes gap.
6. **Visual thinking** (whiteboards, canvases) is a growing segment (Heptabase, AFFiNE, Obsidian Canvas).

---

## 3. Positioning

> **Quire is the fastest, most private home for your Markdown notes. You get Obsidian's open files and Bear's feel, with intelligence that runs on your machine, no Electron, and no AI credits.**

* **Primary user:** writers, engineers and researchers with a few hundred to tens of thousands of Markdown notes who care about speed, ownership and privacy.
* **Switching moments to target:**
  * Logseq OG users (Markdown, frozen branch).
  * Obsidian users tired of Electron or plugin sprawl.
  * Bear users who want cross-platform plain files.
  * Apple Notes users who now have a Markdown export.
* **What we won't chase:** team wikis, Notion-style databases as storage, meeting bots, credit-metered AI.

---

## 4. Gap analysis

| Capability | Obsidian | Bear | Logseq | Notion | Quire now | Priority |
|---|---|---|---|---|---|---|
| Mobile apps | ✓ | ✓ | ✓ | ✓ | ✗ | **P0** (v4) |
| Own sync (E2E) | ✓ paid | iCloud | ✓ | cloud | ✗ (works with iCloud/Dropbox/Git folders) | **P0** (v4) |
| Version history | File recovery | ✗ | ✗ | ✓ | undo tree only | **P1** (v3) |
| Importers | community | — | — | ✓ | ✗ | **P1** (v3) |
| Views over properties (table/kanban/calendar) | Bases | ✗ | queries | ✓ | ✗ | **P1** (v3) |
| AI access via MCP / CLI | CLI | ✓ | CLI | API | CLI only | **P1** (v3) |
| Bring-your-own AI key / local LLM | plugins | — | — | ✗ | Anthropic only | **P1** (v3) |
| Neural semantic search | plugins | ✗ | ✗ | ✓ | LSA | **P1** (v3) |
| Accessibility (screen readers) | ✓ | ✓ | partial | ✓ | classic editor only | **P1** (v3) |
| Web clipper | ✓ | ✓ | — | ✓ | ✗ | P2 (v4) |
| Publish to web | ✓ paid | — | ✓ | ✓ | HTML export | P2 (v4) |
| Note / folder encryption | ✗ | ✓ | sync only | ✗ | ✗ | P2 (v4) |
| Canvas / whiteboard | ✓ | ✗ | whiteboards | ✓ | ✗ | P2 (v5) |
| Graph view | ✓ | ✗ | ✓ | ✗ | data only | P2 (v5) |
| Plugins | ✓ huge | ✗ | ✓ | integrations | ✗ | P2 (v5) |
| PDF annotation | plugin | OCR | ✓ | ✗ | ✗ | P3 (v5) |
| Voice notes + local transcription | plugin | ✗ | ✗ | meeting notes | ✗ | P3 (v5) |
| Real-time collaboration | ✗ | ✗ | planned | ✓ | ✗ | P3 (later) |

**Already ahead of most competitors:** large-file speed, offline intelligence with citations, vault-wide regex replace with preview, inline calculator and natural dates, multi-cursor editing.

---

## 5. Roadmap

Effort: **S** ≤ 1 week · **M** 1–3 weeks · **L** 1–2 months · **XL** quarter+ (one engineer).

### v3 — "Trust & reach" ✅ shipped (see ARCHITECTURE.md §12)

All seven items are built and tested. Deviations from the sketch below: neural embeddings come from an OpenAI-compatible endpoint (Ollama etc.) instead of a bundled candle model; Apple Notes is covered by the generic Markdown-folder importer; the MCP server is scoped by being opt-in per vault and read-only by default rather than by a token.

Goal: make switching safe and make Quire useful from AI tools, without touching the network by default.

| # | Feature | Why now | Design sketch | Effort |
|---|---|---|---|---|
| 1 | **Version history per note** | Trust is the main reason people hesitate to switch | Store snapshots in `.quire/history/` on save (deduplicated, compressed, pruned by age). Timeline UI with diff and restore. Restore is a Transaction, so it's undoable | M |
| 2 | **Importers**: Logseq OG, Bear, Apple Notes Markdown, Notion export, Evernote `.enex` | Ride the switching wave (Logseq split, Apple's new Markdown export) | `quire-import` crate with one adapter per source. Converts block refs, properties, tags and attachments. Dry-run report before writing | M–L |
| 3 | **Properties panel + Views** | Bases and supertags show demand for structure | Front-matter property editor. ` ```quire-view ` blocks (YAML: filter, sort, group, layout = table / list / kanban / calendar) render live and stay plain text | L |
| 4 | **MCP server (opt-in, scoped)** | Every AI-forward competitor ships one; Bear shows how not to | `quire-cli mcp` with tools for search, read, ask, create and append. **Off by default**, per-vault token, read-only unless allowed, writes appear in version history | S–M |
| 5 | **Bring-your-own AI + local LLM** | Answers the frustration with Notion-style credits | Providers: Anthropic, OpenAI-compatible endpoint (covers Ollama, LM Studio, llama.cpp server). Keys kept in the OS keychain | S |
| 6 | **Neural embeddings (optional)** | Better search-by-meaning for small vaults | `Embedder` implementation using a small multilingual model via candle. One-click model download with checksum. LSA stays the zero-download default | M |
| 7 | **Fast-editor accessibility** | Required for a mainstream release | AccessKit text nodes for visible lines and the selection, plus reflowed IME pre-edit text | M |

Success measures: an import of a 5k-note Logseq or Bear library finishes with zero data loss; under 1% of sessions switch to the classic editor; MCP is used by early adopters without security reports.

### v4 — "Everywhere" (next quarter)

| # | Feature | Design sketch | Effort |
|---|---|---|---|
| 1 | **Folder-sync hardening** (iCloud/Dropbox/Syncthing/Git) | Detect conflicting copies and merge them three-way using the last saved version as the base. Show a conflict UI instead of a silent overwrite | M |
| 2 | **Quire Sync (E2E, optional paid)** | Changes to a note sync as small encrypted edits rather than whole files, merging automatically when two devices edit at once. The server only ever sees encrypted data | XL |
| 3 | **Mobile apps** (iOS first, then Android) | Rust core shared through bindings, with a native UI for capture, edit, search and Ask. The editor is the hardest part | XL |
| 4 | **Web clipper** | Browser extension turns a page into Markdown and sends it to the local app (via MCP/CLI) or to sync | M |
| 5 | **Publish** | Static site generated from a folder or tag, keeping wikilinks and the graph | M |
| 6 | **Encrypted notes/folders** | Encrypted on disk with a password or key | M |

### v5 — "Think visually & extend"

| # | Feature | Design sketch | Effort |
|---|---|---|---|
| 1 | **Canvas** | Store canvases in the open **JSON Canvas** format so they also open in Obsidian | L |
| 2 | **Graph view** | Built from data Quire already computes; filter by tag or folder; local graph per note | M |
| 3 | **Plugins (WASM)** | Plugins run in a sandbox and must ask for each permission. They can add commands and views | L |
| 4 | **PDF reader + highlights → notes** | Highlights become linked notes with page anchors | L |
| 5 | **Voice notes** | Local transcription (Whisper) into the daily note | M |

### Later / maybe

Real-time collaboration (needs Quire Sync first), team vaults, tasks with recurrence and calendar sync.

---

## 6. Prioritisation (RICE-style)

Scores are 1–5; effort is in weeks. Score = Reach × Impact × Confidence ÷ Effort.

| Feature | Reach | Impact | Confidence | Effort | Score |
|---|---|---|---|---|---|
| Bring-your-own AI + local LLM | 3 | 3 | 5 | 1 | **45** |
| Version history | 5 | 4 | 5 | 2.5 | **40** |
| MCP server (opt-in) | 3 | 3 | 4 | 1.5 | **24** |
| Importers | 4 | 5 | 4 | 5 | **16** |
| Accessibility | 2 | 5 | 4 | 3 | **13** |
| Neural embeddings | 3 | 3 | 3 | 2.5 | **11** |
| Properties + Views | 4 | 4 | 3 | 6 | **8** |
| Mobile (iOS) | 5 | 5 | 3 | 13 | **6** |
| Quire Sync | 5 | 5 | 3 | 13 | **6** |

Mobile and sync score low only because they're big. They're still the biggest growth levers, which is why they get a whole quarter in v4 instead of competing with v3 items.

---

## 7. Decisions needed

1. **Platform order:** desktop polish first (this plan), or start mobile in parallel?
2. **Sync strategy:** build Quire Sync (revenue, control) or stay "bring your own folder sync" longer?
3. **Business model:** ✅ decided: everything is open source; managed sync will be an optional paid service.
4. **Open source:** ✅ decided: AGPL-3.0-or-later for the whole project (app, CLI, importers and the future sync server), developed in public at github.com/BhanuNexus/quire.
5. **AI default:** keep "offline by default, opt-in providers" as a brand promise?

---

## Sources

* Obsidian changelog — [1.12.0 desktop](https://obsidian.md/changelog/2026-02-10-desktop-v1.12.0), [1.14.0 desktop](https://obsidian.md/changelog/2026-09-02-desktop-v1.14.0/)
* [Getting started with Obsidian Bases](https://obsidian.rocks/getting-started-with-obsidian-bases/)
* [Obsidian pricing 2026 (eesel)](https://www.eesel.ai/blog/obsidian-pricing)
* [Notion AI review 2026 (eesel)](https://eesel.ai/blog/notion-ai-review)
* [Logseq: splitting into two versions](https://logseq.io/p/e3YDyX5AYr)
* [Bear research summary (rywalker)](https://rywalker.com/research/bear-app)
* [Everything new in Apple Notes in iOS 26 (9to5Mac)](https://9to5mac.com/2025/10/16/heres-everything-new-for-apple-notes-in-ios-26/)
* [AFFiNE 0.27 release overview (Elestio)](https://blog.elest.io/affine-0-27-new-importers-calendar-view-and-a-sync-warning/)
* [Capacities plans (CostBench)](https://www.costbench.com/software/note-taking/capacities/)
* [Tana review (Tooliverse)](https://tooliverse.ai/tools/tana)
* [Heptabase review (Tooliverse)](https://tooliverse.ai/tools/heptabase)
* [Reflect review (Tooliverse)](https://tooliverse.ai/tools/reflect)
