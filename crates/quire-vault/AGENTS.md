# AGENTS.md: quire-vault

The vault: a folder of Markdown files behind the `Storage` port, with notes, templates, refactors, version history and
locked (encrypted) folders. It never draws, never uses the network and never indexes (P6, P9).
Depends on: quire-base, quire-markdown. Used by: quire-agent, quire-app, quire-import, quire-index, quire-intel,
quire-pdf, quire-spell. Edition: both (P5).

## Invariants
- A write changes only the bytes it means to (P1). [review only; round-trip tests sit beside each editor in quire-markdown]
- Every vault file is written through `Storage`: `write_bytes_atomic` for new content, `write_bytes_if` (compare-and-swap)
  whenever the new bytes depend on what you read (`Vault::write_if` wraps it). [lint] `clippy.toml` bans `fs::write`,
  `fs::rename`, `File::create` and `persist`; `storage.rs` is the one module with an `#![expect]` for them.
- A listing that fails is an error, never an empty vault: sync turns notes it cannot see into deletes. `LocalFs::list`
  walks again up to `LIST_TRIES` (3) times, then fails naming the folder.
  [test] `a_listing_that_lost_a_folder_is_retried_then_refused` (`storage.rs`)
- Notes in a locked folder are never in `Vault::notes`, never written as plain text, and their keys live in memory only.
  The header of `lock/mod.rs` has the rules. [test] `lock/tests.rs`, for example
  `deleting_the_key_file_never_turns_a_locked_folder_plain`
- A key change always leaves a way in: `begin_change_key` returns the new recovery key before any file changes, and a new
  change first finishes files left under the previous key. [test] `a_key_change_that_stops_anywhere_always_leaves_a_way_in`,
  `a_new_key_change_first_finishes_files_left_under_the_previous_key`
- Saves racing a conversion are kept: lock and unlock write over the copy they read, by compare-and-swap.
  [test] `a_save_during_locking_is_kept`
- `trash` and `trash_if` move a note to `.trash/` and never replace a file trashed in the same second.
  [test] `the_trash_never_replaces_a_file_trashed_in_the_same_second`. `remove` is for app data (pruned history), never notes.
- A snapshot written by someone other than the person typing has a provenance file and a typing save never replaces it
  (`history.rs` header). Format: `docs/formats/provenance.md`. [review only]
- A path leaving the root is refused by `LocalFs::abs` (`quire_base::path::Rule::InsideRoot`). The hidden-folder test is
  `storage::is_hidden`, the one copy; do not write another. [test] `local_fs_refuses_drive_and_rooted_paths`

## Do not
- Call `std::fs::write`, `File::create` or `fs::rename` on a vault or `.quire/` file. On Windows a rename over a file others
  have open fails: use the `Storage` methods, or `quire_base::fs::replace_file` for a temporary file you made.
- Call `Vault::open_local` outside `quire_app::secure_state::open_vault`: a locked folder whose key file is missing would be
  treated as plain. [lint] `clippy.toml`
- Write to storage inside the `still` closure of `write_bytes_if` or `trash_if` (it runs under the write lock).
- Add a `Storage` method whose default silently ignores locked folders (`set_locked_folders` is a no-op by default on purpose).

## Where things go
- A new storage operation: a `Storage` method with a default, then `LocalFs`, `MemFs` and `SingleFile` in `storage.rs`, then
  the test doubles that implement the trait: `lock/tests.rs` (`Crash`, `Hook`, `HeldFile`) and `Gate` in
  `crates/quire-app/src/secure_state/tests.rs`.
- Locked-folder format or conversion: `lock/` (`format.rs`, `convert.rs`, `ring.rs`, `known.rs`); update
  `docs/formats/locked-folders.md` and keep old key files readable (P3).

## Before you finish
- `cargo test -p quire-vault --locked`; `cargo clippy -p quire-vault --all-targets --locked -- -D warnings`. The Windows
  retry code (`retry_unlocked`, `replacing_a_file_that_is_open_for_reading`) runs only in CI on Windows.
- Anything under `.quire/` changes: `docs/formats/`, and old data stays loadable.

## Read first (only when changing that area)
- Locked folders: `docs/formats/locked-folders.md`, `docs/design/0.11-secure.md` (current).
- Trash, Windows replace, key changes, history: `docs/design/0.13.2-store.md` (record: the code wins).
