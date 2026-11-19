# Index startup cache: `.quire/cache/index/`

The index's records and the text of every plain note, so a start reads only
the notes that changed since the last one. It is a cache: deleting the
folder costs one start that reads and parses every note, never a result.
Design: [../design/0.14-index-startup.md](../design/0.14-index-startup.md) §3.6.
Owner: index (`quire_index::store`).

| File | Container `kind` | Written |
|---|---|---|
| `writer.lock` | none (empty file) | held with a non-blocking `try_lock` by the one process that writes |
| `records.idx` | 2, records index | whole file, at each flush |
| `r-<16 hex>.seg` | 3, records segment | once; never changed in place |

Each file but the lock is one [cache container](cache-container.md) with
`format` 2 and `builder` = `quire_index::store::DERIVE_VERSION` (1): the
version of what derives a record from a note (`extract`, the tokenizer and
stemmer, the BM25 weights). A golden test (`crates/quire-index/tests/derive_version.rs`)
fails when the derivation changes until the version is bumped; any other
`builder` or `format` discards the file (`format` 1 held records as JSON
during development and was never released). Varints are unsigned LEB128;
a string is a varint byte length and UTF-8 bytes; an option is a byte 0
(none) or 1 followed by the value; a list is a varint count and its items;
an `f32` is its bits, 4 bytes little-endian.

## `records.idx`

```
count      varint
row:       (sorted by path, byte order, no two equal)
  path     string   vault-relative, `/`-separated, the note's path
  hash     32 bytes SHA-256 of the note's bytes when its record was made
                    (the manifest's hash: `docs/formats/manifest.md`)
  segment  u64 LE   the segment file: `r-<segment as 16 lower-case hex>.seg`
  slot     varint   the note's place in the segment, below 512
  modified varint   zigzag; when the note last changed, Unix ms
```

## `r-<16 hex>.seg`

```
terms      varint, then that many strings: the segment's term table, sorted
notes      varint, 1 to 512
section A  varint inflated length, then a string of raw deflate (level 1)
section B  varint inflated length, then a string of raw deflate (level 1)
```

Section A is, per note in slot order, the record below, then a varint: the
byte length of the note's text in section B. Section B is the notes' texts
one after another, UTF-8.

```
path, name, title         strings
aliases                   list of strings
headings                  list of: level u8, text string, offset varint, anchor string
links                     list of: kind u8 (0 wiki, 1 embed, 2 relative, 3 external),
                          target string, heading option string, display string,
                          range start varint and end varint, field option string
tags                      list of strings
tasks                     list of: done u8 (0 or 1), text string, offset varint,
                          due, scheduled, start, done_on, created, recurrence
                          (each an option string), priority option u8,
                          reminder option string, time_block option (two strings),
                          meta_range option (start and end varints),
                          time_spent option string, estimate option string
stats                     words, chars, lines, reading_minutes: varints
properties                list of: key string, value
                          (value: 0 string | 1 list of strings | 2 list of key string and value;
                          at most 64 levels deep)
plain                     string
content                   list of: term varint (into the segment's table), count varint
bm25                      list of: term varint, weight f32
len                       f32, the document length
```

Every field of `NoteRecord` and of `quire_markdown`'s `Heading`, `Link`,
`Task` and `Stats` is listed: the encoder destructures each type without
`..`, so a new field does not compile until it has a place here (and
`format` is bumped).

## Rules

* **The cache never decides alone.** A row is used only when its `hash`
  equals the manifest's hash of the note now; any other note is read and
  parsed. A copy from another vault or device only costs parses.
* **Any doubt is no cache.** A container that fails (its page), rows out of
  order, a path that is empty, absolute, hidden or has `.`/`..`, a slot of
  512 or more, a section that inflates to another length, a record that is
  not that JSON, a term index past the table, a text that is not UTF-8,
  a record whose path is not its row's, or bytes left over: the notes
  concerned (or all, for `records.idx`) are parsed, and the next flush
  writes them again.
* **One writer.** Only the process holding `writer.lock` writes segments
  and `records.idx` and deletes segments; others read at load and never
  write. Nobody waits for the lock: a process that does not get it tries
  again at its next flush, so when the app quits a running `quire-cli mcp`
  can take over.
* **Flushes.** When the index has been quiet for 30 s, at most every 5
  minutes while it stays busy, within 60 s of removing a note, and on clean
  exit (given up after 2 s). A flush writes the changed notes into new
  segments, writes again without them every segment that held a removed
  or replaced note, replaces `records.idx`, then deletes every segment
  `records.idx` does not reference. Above 256 segments it writes them all
  anew.
* **The cache holds text only of notes that are in the vault now, within
  one flush.** A removed note, a note moved away (including into a locked
  folder, by sync from another device), and the previous version of an
  edited note leave every segment at the next flush, without waiting for any
  threshold. At load, rows for paths that are no longer notes count as
  removed, so the first flush after start removes them.
* **No locked text.** Only notes of `Vault::notes()` (never one in a locked
  folder) that the manifest lists are cached. Every write checks its paths
  against the locked folders just before and just after; if one is locked,
  the file just written and every other cache file are deleted. A load that
  finds a path in a locked folder in `records.idx` deletes every cache file
  before reading a segment. Locking a folder deletes `index/` (with the other
  caches) and tells the index worker to drop its rows.
* **Crashes.** Each file is written atomically and not flushed
  (`quire_base::cache::write`). A crash leaves an old `records.idx` with
  segments that are still whole (unreferenced new ones are deleted by the
  next flush) or a temporary file, which the next start sweeps.
* **Newer format.** A `records.idx` of a newer `format` is left alone and
  the index runs without the cache (P3).
* **Edition.** Records do not depend on the edition (P5).
