# Design notes: which to read

There is one note per piece of work, written when it was done. They are a record of why, so most of them are history by now. This page says which still describe the code, which are records of a release pass, and which are superseded. When a note and the code disagree, the code wins; for saved files, [`../formats/`](../formats/README.md) wins over any note.

Every note starts with a block under its title: `Status: current | record | superseded by <note>`, `Area:` and (for `current`) `Code:`. `packaging/check-design-notes.sh` checks the block, that each note is listed on this page, and that no note links outside the repository; CI blocks on it. A `record` note starts with "A record of a pass; not a description of today's code."

Status here follows what a note *is* (the design of a shipped feature, a record of a pass, or a build log). It was not re-checked line by line against the code.

## Read first

| Note | Why |
|---|---|
| [0.9.md](0.9.md) | §1 holds the invariants P1 to P10 that every change must keep (restated in [AGENTS.md](../../AGENTS.md)). §9 holds the speed budgets. The rest is the 0.9 design: current where the feature still works that way, history for plans and the size numbers (the size budgets in force are only in `packaging/check-size.sh`). |
| [../../ARCHITECTURE.md](../../ARCHITECTURE.md) | The layers and the table of all crates. |

## Current: the design of a shipped feature (read when you change that area)

| Area | Notes |
|---|---|
| Sync | [0.12-sync.md](0.12-sync.md) (S3 and WebDAV), [0.13-sync.md](0.13-sync.md) (compaction, device tokens, removing a device); the wire format is [../formats/sync-protocol.md](../formats/sync-protocol.md) |
| Agents, review, connections | [0.10-agentsync.md](0.10-agentsync.md), [0.11-secure.md](0.11-secure.md) (locked folders, connections); formats in `../formats/` |
| Headless and web | [0.10-serve.md](0.10-serve.md) (`quire-cli sync`, `quire-cli serve`) |
| AI | [0.10-ai.md](0.10-ai.md) (on-device AI, Ask, tool calls, voice notes) |
| Editor, commands, tasks | [0.11-editor.md](0.11-editor.md), [0.11-tasks.md](0.11-tasks.md), [0.10-human.md](0.10-human.md) (Vim, reminders, canvas cards, Bases) |
| Links | [0.12-links.md](0.12-links.md), [0.13-links.md](0.13-links.md) |
| Import | [0.12-switch.md](0.12-switch.md) |
| Drawings | [0.13-excalidraw.md](0.13-excalidraw.md) |
| Packaging | [0.13-pack.md](0.13-pack.md); the release pipeline since 0.13.2 is in [0.13.2-pipe.md](0.13.2-pipe.md) and [../../RELEASING.md](../../RELEASING.md) |

## History: records of a release pass or a build

These say what a pass found, changed and left. Read them for the reasoning behind a decision, not for the current state.

- 0.13.1, the hardening pass: [0.13.1-deps.md](0.13.1-deps.md), [0.13.1-parse.md](0.13.1-parse.md), [0.13.1-perf.md](0.13.1-perf.md), [0.13.1-qa.md](0.13.1-qa.md), [0.13.1-sec.md](0.13.1-sec.md). What each dependency ignore waits for is kept up to date in [../dependency-ignores.md](../dependency-ignores.md).
- 0.13.2, the data-safety pass: [0.13.2-store.md](0.13.2-store.md) (storage and locks), [0.13.2-sync.md](0.13.2-sync.md) (sync safety), [0.13.2-ui.md](0.13.2-ui.md) (saves and entry points). The pipeline note, [0.13.2-pipe.md](0.13.2-pipe.md), is listed above under Packaging.
- 0.14, the codebase-health pass: [0.14-health-docs.md](0.14-health-docs.md) (instructions and checks, current), [0.14-health-gates.md](0.14-health-gates.md) (rules that became tests, current), [0.14-health-saves.md](0.14-health-saves.md) (dropped results as lint errors, current), [0.14-health-ci.md](0.14-health-ci.md) (CI speed, scheduled checks, current) and the other teams' `0.14-health-*.md`, each with a `Status:` block.
- Earlier passes: [0.10-perf.md](0.10-perf.md), [0.12-loose.md](0.12-loose.md), [0.12-verify.md](0.12-verify.md).
- The 0.9 build: [F0-notes.md](F0-notes.md) and the workstream hand-offs of 2026-11-03: [ws-a](handoff/ws-a.md), [ws-b](handoff/ws-b.md), [ws-c](handoff/ws-c.md), [ws-d](handoff/ws-d.md), [ws-e](handoff/ws-e.md), [ws-f](handoff/ws-f.md), [ws-g](handoff/ws-g.md). Nothing in them is a rule for today's work.

## Superseded

- [sync.md](sync.md): the first Quire Sync design (beta in 0.5.0). The shipped protocol and behaviour are in the 0.12 and 0.13 sync notes and `../formats/sync-protocol.md`.
