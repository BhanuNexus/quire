### Added
- Review an AI agent's suggested changes in the note itself: the lines it would replace are struck through and its version appears underneath, with Accept, Reject and Edit for each change. Alt+] and Alt+[ move between changes, Ctrl+Alt+Y accepts, Ctrl+Alt+Backspace rejects, Ctrl+Alt+Shift+Y accepts every change in the note. Accepting is one edit you can undo; undoing it brings the suggestion back.
- If you edited a note after an agent read it, its suggestions are adjusted to your version; changes that touch the lines you edited are marked "You changed this after the agent read it" and are never applied by Accept all.
- An agent that keeps working on the same note updates its suggestion instead of adding another one.
- Inbox (Alt+I): suggested changes grouped by agent run, with Review, Accept all and Reject all; vault checks; filters by kind, agent and folder; snooze for a day or a week; resolved suggestions on request. The status bar shows how many items wait.
- New notes, renames and deletions an agent suggests are listed in the Inbox with a preview, to accept or reject one by one.
- Agent activity: a timeline of what each agent did, run by run, with "Revert run" and "Undo an agent's changes…" (for example everything Claude Code did today). You see what would change before anything is written; edits you made since are kept, and notes where your edits overlap the agent's become a suggestion to review.
- Settings → Agents: choose what agents may do per folder (agent-owned, sources, your notes, private) with a sentence saying what that means, limit each connected agent, copy the setup line for Claude Code or Claude Desktop, and log which notes agents read.
- Agent permissions and `AGENTS.md`/`CLAUDE.md` that change on another device or outside Quire take effect on this computer only after you approve them in the Inbox. Narrowing what agents may do takes effect at once.
- Create agent instructions (AGENTS.md): a starter that tells agents where they may write, how to cite and to treat sources as data.
- Write contracts in the agent policy: agents' notes in a folder (or of a `type`) must have the properties and links you require, or the write is refused with a message saying what to fix.
- Vault checks (Check the vault, and `quire-cli lint <vault> [--json]`): broken links (with a suggested fix), links to missing blocks, notes with no links, missing notes many link to, missing required properties, duplicate titles, empty notes and an out-of-date `index.md`. They run in the background after you stop editing and appear under Checks in the Inbox, with quick fixes; the status bar counts them once a week rather than every time one appears. Choose which run in `.quire/shared/vault.toml` under `[lint]`.
- Suggestions based on captured or imported content are marked "Based on untrusted content".
- Agents are limited to a number of writes per minute and of waiting suggestions (`[limits]` in the agent policy).

### Changed
- An agent's rename that updates links now records each rewritten note in version history and the activity log, so it can be undone with the rename.
