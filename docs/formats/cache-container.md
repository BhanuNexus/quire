# Cache container: every file under `.quire/cache/` from 0.14

One header in front of every 0.14 cache file, so one rule decides when a cache
is thrown away. Design: [../design/0.14-architecture.md](../design/0.14-architecture.md) §3.
Owner: architecture (`quire_base::cache`). Caches written before 0.14
(`pdf-text/`, `claims/`, `served/`, `vectors-<model>.f16`) keep their own formats.

```
offset  size  field      value
0       8     magic      "QUIRECA2"
8       2     kind       u16, which cache (table below)
10      4     format     u32, the layout of this cache's body
14      4     builder    u32, the version of the code that derived the body
18      4     flags      u32, none defined: written 0, value ignored when read
22      8     body_len   u64, bytes after the header
30      32    hash       SHA-256 of bytes 0..30 of the header, then the body
62      …     body       private to the cache (its own format page)
```

All integers are little-endian. The bytes are the same from the full and the
Lite build.

The hash covers every header field but itself (magic through `body_len`,
flags included) and then the body, so a flipped bit in `kind`, `format`,
`builder` or `body_len` is caught as damage to the body is. One hash instead
of a separate header checksum: one field, one check. `QUIRECA1`, an
unreleased layout whose hash covered the body only, is a wrong magic: never
trusted, always replaced.

## Reading

`cache::read(path, kind, format, builder)` returns the body, or nothing for:
a missing or unreadable file, a wrong magic, another `kind`, another `format`
(older or newer), another `builder`, a file whose length is not exactly
62 + `body_len`, or a `hash` that is not the SHA-256 of the header's first
30 bytes and the body. **Any doubt is
"no cache"**: the caller builds the cache again from the notes. A cache is never
migrated; a change of layout or meaning bumps `format`, a change in the code
that derives the content bumps `builder`, and either discards the old file.

## Writing

`cache::write` replaces the whole file: a temporary file beside it, then a
rename (`quire_base::fs::write_atomic_parts` with `Options::STATE.lazy()`, not
flushed). A crash or power cut leaves the old file, no file, or the new file;
a body torn by a filesystem that reorders writes fails its hash. Nothing is
written in place or appended, so several processes on one vault (the app,
`quire-cli mcp`, `serve`) at worst replace each other's file, and the loser's
work is redone later.

If the file there holds the same `kind` in a **newer** `format`, and its
length is 62 + `body_len` and its `hash` is right, it is left
alone and the cache lives in memory only: a newer Quire on this device may
still be using it (P3). Anything else (an older format, another builder,
another kind, a newer format whose length or hash is wrong, junk) is
replaced, so a damaged file whose header only looks newer (a flipped bit in
`format`) does not stay forever: the hash covers `format`.

A cache that cannot be written (a read-only disk) is a notice once, then
in-memory operation; never an error that stops the app.

Deleting `.quire/cache/` (or one file in it) is always safe: it changes speed,
never results. A copy carried from another device by a file-sync tool is
validated against this device's files by each cache's own rules.

## Kinds

| `kind` | Cache | File | Body format |
|---|---|---|---|
| 1 | vault manifest | `.quire/cache/manifest` | [manifest.md](manifest.md) |
| 2 | index startup cache, row per note | `.quire/cache/index/records.idx` | [index-cache.md](index-cache.md) |
| 3 | index startup cache, segment | `.quire/cache/index/r-<16 hex>.seg` | [index-cache.md](index-cache.md) |
| 4 | passage cache, row per note | `.quire/cache/index/passages.idx` | reserved |
| 5 | passage cache, segment | `.quire/cache/index/p-<16 hex>.seg` | reserved |
| 6 | LSA model | `.quire/cache/index/lsa-<16 hex>.model` | reserved |

A new cache takes the next number in `quire_base::cache::Kind` and adds a row
here; a number is never reused.
