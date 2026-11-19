# Sync state: `.quire/sync/state.d/`

What sync did last on this device: for every synced file, its revision,
path and a print of its synced text, and the texts that three-way merges
start from. Device-local; never synced, never needed on another device
(losing it costs conflict copies, never text). Design:
[../design/0.14-sync-speed.md](../design/0.14-sync-speed.md) §3.2, §3.3, §3.7.
Owner: sync (`crates/quire-app/src/vault_sync/journal.rs`).

One folder per engine state: `state.d/` (the whole vault),
`state-partial.d/` (while a folder selection is set) and `state-shared.d/`
(a sync folder's settings lane). Quire 0.13 kept each as one JSON file,
`state.json`, `state-partial.json`, `state-shared.json`, migrated once (below).

```
state.d/
  checkpoint-000007.json   the whole state, without texts
  journal-000007.jsonl     what changed since that checkpoint, one line a commit
  bases-000003.pack        merge-base texts, at most 16 MiB a pack
  bases-000004.pack        the pack being appended to
```

Only the process holding `.quire/sync/lock` writes here.

## Checkpoint: `checkpoint-<6 digits>.json`

One line, then a newline:

```
{"format":1,"h":"<SHA-256 of d, 64 lower-case hex>","d":<body>}
```

`d` is the body's JSON exactly as written (the hash is of those bytes):

| Field | Meaning |
|---|---|
| `state` | the engine state (`quire_sync::SyncState`): `cursor`, `files` (by file id: `path`, `synced_path`, `head`, `head_seq`, `base`, `deleted`, and when set `outside`, `absorbed`, `kept`, `up`, `disk`), `left`, `skipped`, `deletes_sent`. `base` (and `kept.base`, `left.*.base`) is a **print**: 64 hex of SHA-256 of the synced text (for an attachment, of its base64, the hash inside 0.13's `sha256:` fingerprint). `disk` is the vault manifest's hash of the bytes last known equal to the base. |
| `verified_at` | Unix ms of the last pass that read every note again; 0: never |
| `packs` | `{"<pack number>": <bytes the record names>}` |
| `places` | `{"<print>": [pack, offset, length]}`: where each note base's record is |
| `migrated_from` | SHA-256 of the 0.13 file it was made from, when it was |
| `older_runs` | how many times an older Quire's file replaced this record (absent: 0) |

A `format` newer than this build knows is an error ("written by a newer
Quire; update Quire on this device"), never a fresh start.

## Journal: `journal-<6 digits>.jsonl`

The changes since the checkpoint of the same number, one line a commit:

```
{"h":"<SHA-256 of d>","d":{"files":{"<id>":<entry>|null},"cursor":7,"places":{"<print>":[1,0,90]|null},…}}
```

`files` and `places` hold only the entries that changed (`null`: removed);
`cursor`, `left`, `skipped`, `deletes_sent`, `verified_at` and `packs` are
there, whole, only when they changed. A commit that changes nothing writes
nothing. Once the journal passes a quarter of the checkpoint (at least
256 KiB), it is folded into the next checkpoint.

## Packs: `bases-<6 digits>.pack`

Records one after another, no header:

```
u32 length (little-endian, of `bytes`) | 32-byte print | u8 codec | bytes
```

Codec 0: the text as UTF-8; 1: raw deflate (level 1), used when smaller. A
record is good when its print is the SHA-256 of its text. Only note texts
are kept (attachments, settings files and encrypted files are never
merged); an empty text needs no record. A pack is appended to until it
passes 16 MiB.

## Writing

* **A commit**, at the end of a sync pass: flush the packs appended to
  (`sync_data`, and the folder when a file was made), then append the
  journal line and flush it. Data first, then what names it.
* **Compaction**: a pack (other than the one appended to) whose named
  records are under half its bytes has them copied to the active pack; the
  commit names their new places and drops the pack from `packs`, then it
  is deleted. On disk the packs stay under twice the live bytes plus one pack.
* **Locked folders**: when a base is cleared because its note is locked
  now, every pack with any unnamed bytes, the active one included, is
  compacted in that same commit, so no version of that text stays in a pack
  (`docs/design/0.11-secure.md`).
* **A checkpoint** is written whole with `quire_base::fs::write_atomic`
  (flushed, then renamed); then the older checkpoint and journal are
  deleted.

## Opening

* The highest checkpoint that parses and passes its hash; then its
  journal's lines in order while each one ends in a newline and passes
  its hash.
* Only once those were accepted: the journal is cut after its last good
  line, each pack is cut to the bytes `packs` names, and checkpoints,
  journals and packs the record doesn't name (and temporary files) are
  deleted.
* **Can't be read now** (listing the folder, or reading a checkpoint or
  the journal fails for another reason than "not found"): an error; the
  pass doesn't run and nothing is changed.
* **No checkpoint parses**: the folder is renamed whole to
  `state.d.unreadable` (`-2`, `-3`… if taken), sync starts afresh and says
  so once.
* **The highest checkpoint is damaged, a lower one parses**: the lower one
  is used (it is complete: a checkpoint is written before the older pair is
  deleted), the damaged file is renamed to `….unreadable`, and a notice says so.
* **A torn last line** (no newline, or its hash fails, with no good line
  after it): a crash's leftover, cut silently.
* **A bad line followed by good ones**: damage. The state up to that line
  is used and saved as a new checkpoint; the journal is renamed to
  `….jsonl.unreadable`; a notice says so.
* **A pack record that is missing, short or fails its print**: that base
  is missing. A merge that needs it keeps both versions (the other device's
  as a conflict copy); a notice says so once. A pack that can't be read now
  fails the pass that needs it.
* A folder with no checkpoint at all (a first commit or a migration that
  stopped before its checkpoint) names nothing: it is deleted.
* `sync status`, history and setup read the record without changing it.

## From Quire 0.13, and back

At the first 0.14 session over a vault with `state.json` (likewise the
other two):

1. It is read as 0.13 did (a damaged file is copied to `state.json.unreadable`
   and sync starts afresh, saying so).
2. Each note's text goes into a pack and each base becomes a print. No
   `disk` is known yet, so the first pass reads each note once.
3. The packs are flushed and `checkpoint-000001.json` is written with
   `migrated_from`.
4. `state.json` is deleted and the folder flushed.

A `state.json` beside a folder whose `migrated_from` matches it is the
leftover of a crash after step 3: it is deleted. One that doesn't match was
written later by an older Quire: it wins, the folder is deleted and the
migration runs again, carrying `older_runs` + 1. The first such time a
notice says sync went on from the older Quire's record; from the second,
it names the cause (an older Quire syncing the same vault on this device).

Going back to 0.13: it finds no `state.json`, starts afresh and adopts the
notes already there; a note changed here since the last 0.14 pass is kept
beside the synced version as a copy. 0.13 ignores the folders.

Changing the folder selection renames `state.d` ↔ `state-partial.d` (and a
not yet migrated `state.json` ↔ `state-partial.json`). Stopping sync removes
both forms.
