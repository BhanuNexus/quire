# WS-A hand-off: requests and notes for other workstreams

What WS-A (agent store and review) needs from files it doesn't own, what it
worked around meanwhile, and what other workstreams can now use.

## Requests (one small change each)

1. **Editor banner slot (F0 / WS-F, `app.rs`).** Design §7.3 puts the
   review banner ("Claude Code suggests 4 changes · Why … · ◀ Prev Next ▶ ·
   Accept all · Reject all · Done") above the note. There is no hook for a
   panel above the editor, so the banner is the first card of the Inbox tab,
   and review commands open the Inbox. A registry function such as
   `registry::editor_banner(ui, ws, pal) -> PanelOut`, called where
   `panels::merge_banner` is shown, would let WS-A move it there
   (`inbox_ui::review_ui::review_card` is ready to be called from it).
2. **Reveal the caret after a feature command (F0, `app.rs`).** Moving to the
   next hunk sets the selection but `QuireApp::run` only scrolls to the caret
   for a fixed list of commands. Today the Inbox tab returns
   `PanelOut { reveal_only }` the next frame, which is why review commands
   return `UiRequest::Feature` (opening the Inbox). A `UiRequest::Reveal` or
   adding the `Review` category to `reveal_caret()` would remove that need.
3. **History dialog → activity (F0, `dialogs.rs`).** §A.9 asks history
   entries to link back to the activity timeline. `HistoryState` would need
   to show `Provenance` (it can read `<ms>.prov.json` through
   `quire_agent::Provenance::from_json(vault.read_provenance(s))`) and offer
   `Command::ShowAgentActivity`.
4. **Pinned agent instructions in the sidebar (WS-F, `panels.rs`).** §A.6
   shows `AGENTS.md`/`CLAUDE.md` as a pinned sidebar item with an "Agent
   instructions" badge. `quire_agent::instructions::find(vault)` names the
   file and `ws.features.agent.inbox.instructions` says whether it waits for
   approval (`!approved`), for a warning colour. Until then it is reached from
   Settings → Agents and the Inbox.
5. **Alt shortcuts also type their character (F0, `app.rs`).** egui sends
   the key's text with Alt held (Alt+] gives "]" on Linux and Windows, "‘"
   with Option on macOS), and `handle_shortcuts` leaves it for the editor,
   so Alt+I, Alt+], Alt+[ (and 0.8's Alt+D/S/T) insert a character into the
   note. WS-A works around it for its own keys (`agent_state::drop_typed_key`
   undoes a one-character insertion made in the shortcut's frame).
   Removing `Event::Text` from the input when an Alt-only shortcut fires
   would fix it for every key.
6. **Overlay target for feature commands (F0, `registry::UiTarget`).**
   "Undo an agent's changes…" would naturally be a dialog; with only tab
   targets it opens the Activity tab with its form expanded.

## For WS-B (MCP, CLI)

* **Unverified clients.** `Actor` has a new field `unverified: bool`
  (default false). Set it when the key is `mcp:<clientInfo.name>` because no
  `--client` key was given: the client's `[clients]` entry can then narrow
  but never widen what it may do (§A.2).
* **`include_pending` reads.** `AgentStore::pending_text(actor, path)` gives
  the text with the actor's own waiting proposal applied and its version.
  An edit sent with that version in `expected` replaces the pending text;
  `submit` accepts it without a version conflict.
* **Amend** is automatic: `SubmitOutcome::Proposed { amended: true }`.
* **CLI `agent accept|reject|revert`:**
  * accept/reject hunks of a closed note: `AgentStore::review(id, path,
    &disk_text)` for the hunk ids, then `accept_in_file(id, path,
    &decisions, None)` (writes the file with history and provenance) or
    `decide_checked` with rejections;
  * a whole proposal: `resolve_proposal(id, accept, &|_| false)` (returns
    `ResolveReport`; conflict hunks are never applied by accept);
  * revert: `revert_plan(&RevertSelector { actor, since, .. })` (dry run,
    `RevertStep::describe()` for each line), then `revert_apply(&plan,
    title)`; MCP `revert` → `propose_revert(&sel, &actor, title)` (always a
    proposal).
* **`agents-md init`** (task B.10): `AgentStore::create_instructions()`
  writes the starter `AGENTS.md` and approves it on this device
  (`quire_agent::instructions::starter(&policy)` for the text alone).
* **Instructions for MCP:** `AgentStore::instructions()` →
  `Instructions::served()` is the approved text or the "await review"
  notice; `PENDING_MESSAGE` is the notice.
* **Trust:** `AgentStore::trust_of(path, text)` for `structuredContent.trust`
  and the untrusted delimiter; `untrusted_source(&sources)`.
* **`last_actor(path)`** for hit metadata (`(Actor, at_ms)` from the ledger).
* **Read logging:** call `AgentStore::record_read(&actor, &path)` from read
  tools; it logs only when the device turned it on (Settings → Agents).
* **Errors:** new codes appear in `WriteError::to_json()` (`rate_limited`
  with `retry_after_ms`, `inbox_full`, `contract_violation` with
  `violations`), as in §4.4.
* The policy now needs device approval: a CLI or MCP process uses the same
  `<config>/vaults/<key>/state.toml` as the app (found by `AgentStore::open`
  for vaults on disk).

## For WS-C (trust findings in the Inbox)

The Inbox shows `Contradiction`, `Stale`, `CitationDrift`, `VerifyFailed` and
`JobFailed` items generically (a sentence, Open, Dismiss, Snooze). The
feature-specific actions in §C.5 (Open both, Mark one superseded, Ask agent
to reconcile) need a hook: WS-A can add a registry function for "actions of
an inbox item by kind" if WS-C wants to draw them; ask.

## For WS-D (ingest, panel)

* A change set is proposed as a whole if any change needs review. Ingest
  that writes `wiki/` pages and suggests links in the person's notes should
  submit two change sets with the same `run`; the Inbox groups them by run.
* `revert_plan(&RevertSelector { run: Some(run), .. })` + `revert_apply`
  restores the vault for "Revert run".
