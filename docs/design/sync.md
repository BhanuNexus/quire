# Quire Sync: design

*Status: proposal, for review. Tracks [#14](https://github.com/BhanuNexus/quire/issues/14).*

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

## Components

| Piece | What it is |
|---|---|
| `quire-sync` crate | Protocol types, encryption, and the sync engine. No UI and no network code inside the engine: it talks to a `Transport` trait, so tests run many simulated devices against an in-memory server |
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
