# Quire Sync: design

*Status: shipped as a beta in 0.5.0 (server); folder sync follows. Tracks [#14](https://github.com/BhanuNexus/quire/issues/14).*

Quire Sync keeps a vault the same on every device. The server is open source (AGPL), runs as one binary anyone can host, and only ever stores encrypted data. The paid **managed sync** service runs the same server for people who don't want to host it.

## Goals

1. **End-to-end encrypted.** The server never sees note text, file names or folder names. A stolen database reveals nothing a user wrote.
2. **Never lose a word.** Any two devices can edit the same note offline. Every sync either merges cleanly or keeps both versions visibly. No last-writer-wins.
3. **Plain files stay the truth.** The vault on disk is still ordinary Markdown. Sync can be turned off at any time and nothing changes.
4. **Simple to self-host.** One binary with SQLite, or one Docker image. No external services needed.
5. **Cheap to run.** The managed service has to be profitable at a few dollars a month per user.

**Not in v1:** real-time co-editing (cursors and live typing between people), sharing a vault with other accounts, reading notes on the web. The design leaves room for these, but they aren't built first.

## Why not a CRDT

A CRDT (Automerge, Yjs) is the usual answer for offline editing. Quire doesn't need one yet:

* Notes are files that people also edit outside Quire, with other editors, scripts and Git. A CRDT has to be the source of truth and would turn every outside edit into a whole-document replacement anyway.
* A CRDT keeps its own history next to the text. That roughly doubles storage, and it would all have to be encrypted, uploaded and paid for.
* Quire already has a tested **three-way line merge** (`quire-text::merge`). It's what sync tools need: for most concurrent edits to one note it merges cleanly, and when it can't, it shows both versions.

So v1 syncs **file revisions** and merges them three-way. Real-time co-editing can later add a CRDT for open documents only, on top of the same revision log.

## How it works

### Data model

Each vault on the server is a **space**. A space holds an append-only log of encrypted **revisions**:

```
Revision {
  space_id    : uuid        -- plaintext, for routing
  file_id     : uuid        -- random, stable across renames
  rev_id      : uuid        -- random
  parents     : [rev_id]    -- 1 parent; 2 after a merge; 0 for a new file
  seq         : u64         -- assigned by the server, strictly increasing per space
  device_id   : uuid
  ciphertext  : bytes       -- encrypted { path, kind, content | blob refs, deleted }
  size_bucket : u32         -- padded length (see "What the server learns")
}
```

* **Renames** are a new revision of the same `file_id` with a new path inside the ciphertext. Links stay right because the client rewrites them (as it already does locally) and syncs those notes as edits.
* **Deletes** are a revision with `deleted: true`. The server keeps them, so a deleted note can be restored from history on any device.
* **Attachments** (images, PDFs) are stored as encrypted **blobs** in 4 MiB chunks. A revision lists its chunk ids. Chunk ids are `HMAC(blob_key, plaintext)`, so the same image added twice is stored once, without revealing its content.

### The sync loop

The server is a **per-file compare-and-swap log**:

1. **Pull:** `GET /spaces/{id}/changes?after={seq}` returns revisions newer than the device's cursor. The client decrypts them, applies each to disk (see below), and moves its cursor forward.
2. **Push:** when a note is saved, the client sends a new revision whose parent is the last revision it synced for that file. The server accepts it only if that parent is still the file's head. Otherwise it answers `409 Conflict` with the current head.
3. **Merge on conflict:** the client has the ancestor (the last synced text, kept in `.quire/sync/base/`), its own text and the head's text. It runs the same three-way merge the editor uses:
   * **Clean:** it pushes the merged text with two parents and writes it to disk. If the note is open, the existing "merged changes made elsewhere" path applies it as one undoable edit.
   * **Overlapping edits:** for an open note, the existing banner appears (Keep mine / Take theirs / Keep both). For a closed note, the other version is written as `Note (conflict from <device> <date>).md` next to it, which the existing **Resolve sync conflicts…** dialog already finds and merges.
4. **Notify:** a long-poll (`GET /spaces/{id}/wait?after={seq}`, upgradable to WebSocket later) wakes devices when another device pushes, so changes appear within a second or two. Devices also pull on start and every few minutes.

Edits made outside Quire are picked up by the existing file watcher and pushed like any save. A delete racing an edit resolves to **keep the edit** and tells the user.

### Encryption

| What | How |
|---|---|
| Vault key | 256-bit random key, made on the first device. Never leaves a device unwrapped |
| Revisions and blobs | XChaCha20-Poly1305 with a random 192-bit nonce. Associated data = `space_id ‖ file_id ‖ rev_id`, so the server can't swap ciphertexts between files or revisions |
| Subkeys | HKDF-SHA256 from the vault key: one for content, one for blob ids, one for padding |
| Joining a new device | The vault key is stored on the server **wrapped** by a key derived from the user's **sync passphrase** with Argon2id (64 MiB, 3 passes). A new device downloads the wrapped key and unwraps it locally. Later: scan a QR code from an existing device instead |
| Recovery | On setup, Quire shows a one-time **recovery key** (the vault key, base32, grouped). It's the only way back if the passphrase is forgotten. The server can't help, by design |
| Server login | Separate from encryption. Devices hold a revocable token. Self-hosted servers use invite tokens; the managed service uses email and password (Argon2id-hashed server side) |

Crypto comes from the RustCrypto crates (`chacha20poly1305`, `argon2`, `hkdf`, `hmac`, `sha2`): pure Rust, widely used, with a published audit of the AEAD crate. Nothing home-made.

### What the server learns

The server can see: how many files a vault has, roughly how big each revision is (lengths are padded up to the next size bucket), when each device syncs, and which revisions belong to the same file. It can't see: text, file or folder names, tags, links, or which files are attachments. This is the same trade-off Obsidian Sync and Standard Notes make, and it's written down so users can judge it.

## Sync through a folder (free)

Most people already have Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing. Quire Sync can use a folder one of those keeps on every device instead of a server. It's the default in Settings → Sync, costs nothing and needs nothing running.

Putting the vault itself in Dropbox already works, but a file sync tool can't merge: two devices editing one note produce a "conflicted copy", and Quire only cleans up afterwards. Folder sync keeps the merge in Quire, with the same encryption, by never letting two devices write the same file:

```
<chosen folder>/Quire Sync/<space>/
  space.json                          the wrapped vault key (written once, by the first device)
  devices/<device>/000001.jsonl       that device's revisions, one sealed revision per line
```

* **One writer per file.** A device only appends to its own log files. The sync tool copies them; it never sees two devices change one file, so it never makes conflicted copies.
* **The same order everywhere, without a server.** Each device reads every log and works out each note's current revision: start at the note's first revision and keep following its child. When two devices built on the same revision before seeing each other's, the child with the smaller revision id wins. That rule doesn't depend on which files arrived first, so every device agrees once it has the same files.
* **The losing device merges again.** The device whose revision lost hears about it when the winner's file arrives, which is like the server's `409 Conflict`, only later. It merges its changes onto the winner three-way, from the revision both share, and appends the result. Other devices just follow the winner. If two devices race to do this, the results are identical and the next round is a no-op. The engine change is one hook, `Transport::fork_base`, which servers don't need.
* **Half-synced files are fine.** A line is read only once it ends in a newline, so a log that's still downloading, or was cut short by a crash, is read up to its last whole line. A revision whose earlier revision hasn't arrived yet waits for it. A damaged line is skipped.
* **Changes are noticed by polling.** Every 2 seconds Quire checks the logs' sizes and times, which is cheap, and syncs when they change.

### Drawbacks, and what Quire does about them

| Drawback | Impact | Mitigation |
|---|---|---|
| **Slower.** Changes travel as fast as the sync tool: seconds for Dropbox and Syncthing, sometimes minutes for iCloud Drive, Google Drive and OneDrive | Concurrent edits are more likely, and other devices see changes later | Concurrent edits merge as they do with a server, so it's only a delay. A server stays available for near-instant sync |
| **Races resolve later.** With a server, the second device to push finds out at once. Here it finds out when the other file arrives | A third device may briefly show the winning version without the losing device's edits, until that device merges them back | The losing device always merges again; text is never dropped. If that device is never online again after the race, its last edit stays in the encrypted log but not in the note (restoring from the log comes with history, milestone 4) |
| **iCloud evicts files** to save space, leaving `.icloud` placeholders | Those logs can't be read until they download | Quire counts placeholders and shows *Waiting for files* with the fix: "Keep Downloaded" on the Quire Sync folder. Dropbox and OneDrive placeholders download when read |
| **Logs grow.** Every saved change is a line, and nothing is deleted yet | Roughly the size of your edits over time, padded and encrypted | Logs roll over at 1 MiB, so tools that re-upload a changed file whole only re-send the newest. Compaction (a snapshot, then dropping lines every device has read) is planned with history |
| **A copied vault** (including `.quire/`) on a second computer would make two computers write as one device | The two would append to the same log file | Quire notices lines it didn't write in its own log (or a sync tool's conflicted copy of it) and stops with "Another computer is syncing as this one". Turning sync off and on again on one of them fixes it |
| **The wrapped key is in the folder.** With a server it sits behind the server token; here anyone who can read the folder can try passphrases offline | A weak passphrase is the weak point | Argon2id with 64 MiB and 3 passes makes each guess slow, and the passphrase must be 8 characters or more. Use a long one, as for the server |
| **Several devices lose the same tie with overlapping edits** | Each has a version that didn't make it | The winner stays in the note everywhere, and each losing device writes only its *own* version beside it, so every version appears exactly once (tested with three devices) |
| **Polling** | A small amount of work every 2 seconds while the app is open | Only file sizes and times are read, not contents |

What doesn't apply: **clock skew** (nothing depends on time; order comes from the revision graph), **deletes** (a delete is a revision, so it merges like an edit, and an edit still beats a delete), and **name clashes** (when two notes want the same name, every device gives the numbered name to the note with the larger id, whichever it saw first).

What the sync service learns is what a server would: how many notes, roughly how big each change is, when devices sync and how many devices there are. Not text, note or folder names.

**Tests.** The same convergence checks as the server run against real folders, with a simulated sync tool that copies files late, out of order and half-written, across 3 devices: 150 seeds per test on every CI run and 3,000 checked locally, plus an end-to-end test of two app workspaces sharing a folder.

## Components

| Piece | What it is |
|---|---|
| `quire-sync` crate | Protocol types, encryption, and the sync engine. No UI and no network code inside the engine: it talks to a `Transport` trait, so tests run many simulated devices against an in-memory server. `FolderTransport` syncs through a shared folder |
| `quire-sync-server` | axum + tokio + SQLite (rusqlite). Blobs on disk, with S3-compatible storage as an option later. One binary and a Docker image. AGPL |
| Desktop | **Settings → Sync**: server URL, create or join a vault, passphrase, recovery key, devices. A status-bar indicator (synced / syncing / offline / needs attention). It reuses the merge banner and the conflicts dialog |
| Managed service | The same server, run by us, plus accounts, quotas and billing. Because the server is AGPL, any changes we make to it are published. Billing glue can be a separate service |

## Testing

Data loss is the failure that matters, so the engine is tested hardest:

* **Convergence property test:** 3–5 simulated devices make random edits, renames, deletes and attachment changes, go offline and online in random orders, and sync through an in-memory server. Afterwards every device has the same files, and every line any device wrote is either in a file or in a conflict copy. Thousands of seeded runs in CI.
* **Crypto tests:** a tampered ciphertext, a swapped revision or a wrong passphrase must fail loudly. Fixed test vectors for the format, so later versions can still read old data.
* **Server tests:** compare-and-swap under concurrent pushes, auth, quotas, and a crash in the middle of a write.
* **End-to-end:** the real server binary and two desktop workspaces on a temporary vault, in CI.

## Milestones

1. **Engine:** `quire-sync` with crypto, the revision log, the merge on conflict and the in-memory transport, plus the convergence tests. Nothing user-visible yet.
2. **Server:** `quire-sync-server` with SQLite, token auth, the long-poll, a Docker image and self-hosting docs.
3. **Desktop:** Settings → Sync, status indicator, conflict handling wired to the existing UI. Ships as **beta** for self-hosters.
4. **Hardening:** attachment chunks, history and restore from the server, device list and revocation, backpressure and quotas, and an export of a whole space.
5. **Managed service:** accounts, billing and the hosted deployment. The paid product launches here.

The iOS app (#13) and the web clipper (#12) build on milestones 1–3.

## Decisions to confirm

1. **Separate sync passphrase**, not derived from the account password. It's safer: a leaked account password doesn't expose notes. It costs one more secret to remember, softened by the recovery key. *(Recommended.)*
2. **Revision log with three-way merge for v1**, CRDT later only for live co-editing. *(Recommended.)*
3. **axum + SQLite** for the server, so self-hosting is one binary. Postgres can come later for the managed service if needed. *(Recommended.)*
4. **History retention** on the server: keep everything by default for self-hosted; the managed plan sets a window (for example 12 months) to bound storage costs.
