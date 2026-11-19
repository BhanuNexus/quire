# Vault manifest: `.quire/cache/manifest`

A table of every visible file of the vault: its size, time and SHA-256, and the
generation in which it last changed. Sync, the index and agent views ask it
"what changed?" instead of reading every note. It is a cache: deleting it costs
one scan that reads every file, never a result. Design:
[../design/0.14-architecture.md](../design/0.14-architecture.md) §2.
Owner: architecture (`quire_vault::changes`).

The file is one [cache container](cache-container.md): `kind` 1, `format` 1,
`builder` 1. The body, all integers little-endian, varints unsigned LEB128:

```
origin    32 bytes  SHA-256 of the absolute path of this file, as this device names it
epoch     16 bytes  the writing process's epoch; not read back (a new one on every open)
gen       u64       the current generation
floor     u64       tombstones of this generation and older were dropped
count     u64       rows that follow, sorted by path (byte order), no two equal
row:
  path_len  varint
  path      UTF-8, vault-relative, `/`-separated, as the storage names it
  size      varint  bytes on disk when hashed
  mtime_ns  u64     nanoseconds since 1970 as the listing gave it; 0 = unknown
  hash      32 bytes SHA-256 of the bytes as stored (no BOM stripping)
  gen       varint  the generation in which the row last changed
  flags     u8      1 = gone (a tombstone), 2 = racy; other bits must be 0
```

About 87 bytes a row with typical paths (`Folder 12/Sub 3/A note with a title 1234.md`):
870 KB at 10,000 files, 4.4 MB at 50,000.

## Rules

* **Which files.** What `Storage::list` lists: regular files outside hidden
  folders (no `.quire/`, `.git/`, `.trash/`, `node_modules`), never a symlink or
  anything under a symlinked folder. In a locked folder that is the stored
  file, so the row is `Journal/Day.md.qenc` and its hash is of the encrypted
  bytes; no plain text or plain-text hash of a locked note is kept.
* **Stat is a hint, hash is the fact.** A scan lists the vault once with sizes
  and times. A row whose size and time equal the listing's, whose time is not 0
  and that is not racy is kept without reading the file. Any other file is read
  and hashed; its row changes, and gets a new `gen`, only when the hash
  changed. A verify pass reads every file, or, from `verify_where`, every
  file a consumer tracks (sync: its notes, not every video). A file is hashed
  in 64 KiB pieces where the storage can, never read into memory whole.
* **Racy.** A row whose time is less than 2 s before its scan, or after it, is
  hashed again by the next scan (coarse clocks, an edit in the same tick as the
  scan). A row Quire recorded for its own write has time 0 and is racy.
* **Own writes and observed reads.** Bytes Quire wrote itself
  (`note_written`) update the row at once and are not sent to subscribers:
  the writer tells others itself. Bytes Quire read but did not write
  (`observed`: sync or the editor reading a note another program changed)
  update the row the same way and, when the hash is new, are sent to every
  subscriber as a delta of that one row; the next scan, finding the same
  bytes, sends nothing more.
* **Subscribers.** A subscriber hears only what happens after it subscribed,
  and never own writes. At start it subscribes, then reconciles with
  `since(gen)` (or every row, when it knows no `gen` or `since` says the
  tombstones are gone), comparing by hash.
* **Generations.** `gen` goes up by one for each scan, or each own write or
  removal, that changed at least one row. A generation is meaningful only
  within one process: the epoch is new on every open (the `epoch` stored in
  the file is not read back), because after a crash, or with a file another
  process wrote, the same `gen` can come back naming other rows. A
  consumer's in-process cursor is a `gen`; a consumer that persists what it
  saw compares by hash on its next start, which reads no note.
* **Tombstones** keep the last hash of a removed file. When there are more than
  1,024 and more than a quarter of the live rows, the oldest go and `floor`
  rises.
* **Never a delete.** A listing error leaves every row as it was. A file that
  is listed but can't be read (locked by another program, no permission, an
  I/O error) keeps its row, marked racy, is never made a tombstone, and is
  named in the scan's `unread`; the other files are updated as usual. Sync never
  takes a delete from the manifest's tombstones or cached rows (`0.14-sync-speed.md` §3.6).

## Reading and writing

* Any doubt is no table: a container that fails (see its page), another
  `origin` (a copy made for another vault, or for this vault at another place,
  as a file-sync tool may carry it), a path that is empty, not in normal form or
  leaves the vault, rows out of order, an unknown flag, a row `gen` above the
  header's, or bytes left over. The table then starts empty with a new epoch.
* Written whole, atomically and not flushed, when its owner flushes it (the
  app: when quiet for 30 s, at most every 5 minutes while busy, and on clean
  exit). A newer `format` on disk is left alone and the table stays in memory
  (P3). Two processes on one vault may replace each other's file; either is a
  valid start for the next scan.
* A storage with no place on disk (in-memory vaults, a single file opened
  outside a vault) keeps the table in memory only.

## Where 0.14 departs from the frame (`0.14-architecture.md` §2)

* The field and method are `generation`, not `gen`: Rust 2024 reserves `gen`.
  The file keeps the frame's names.
* `Manifest` is a shared handle: every method takes `&self` and clones share
  one table, so own writes never wait on a scan (P9) and the index and sync use
  one scanner. `scan()` and `verify()` take no vault: the manifest scans the
  storage it was opened on.
* `since(generation)` returns owned rows, or `None` once tombstones that old
  were dropped (the frame does not say how tombstones end).
* The `origin` field (not in the frame) keeps a copy made for another vault
  from being trusted.
* Known limit: on a disk with a coarse clock, a file renamed over a deleted one
  of the same size and the same time is found only by a verify pass.
