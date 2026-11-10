# Import records: `.quire/imports/<id>.json`

Importing writes a record of what it created, so the import can be undone.
Owner: 0.12 switching tools. Code: `quire_app::import_run`.

```json
{
  "format": 1,
  "id": "i_1793527200000",
  "at_ms": 1793527200000,
  "source": "Joplin",
  "origin": "joplin-export.jex",
  "folder": "Imported",
  "notes": [{ "path": "Imported/Work/Plan.md", "hash": "sha256:5f2c…" }],
  "files": [{ "path": "Imported/attachments/diagram.png", "hash": "sha256:91ab…" }],
  "undone_ms": null,
  "in_progress": false
}
```

| Key | Meaning |
|---|---|
| `id` | `i_` and the Unix milliseconds; the file's name |
| `source`, `origin` | what was imported, and the export's file or folder *name* (never its full path) |
| `folder` | the vault folder the import wrote into (`""` for the root) |
| `notes`, `files` | every file the import created, with the hash of the bytes it wrote, as stored on disk |
| `undone_ms` | when it was undone |
| `in_progress` | `true` while the import runs. A record that still has it was left by an import that failed or was killed: what it lists was written and can be undone |

The record is created before the first file is written and saved again every
20 files, at the end, and when the import fails or is cancelled. An import
that is refused before writing anything (a hidden target folder, a locked
folder that is closed) leaves no record. Two imports in the same millisecond
get different ids.

**Locked folders.** The hash is of the stored bytes: for a note or picture in
a locked folder that is the encrypted file (`Day.md.qenc`), so the record
never holds a hash of a locked note's text. A locked file's name is not
secret, only its content. Editing the note seals it again with a new nonce,
so an edited locked note no longer matches and stays, like any edited note.
Unlocking a folder for good (decrypting it) also changes the bytes: its
imported notes then stay on undo.

**Undo** moves to the trash the files whose stored bytes still have the
recorded hash. A file edited since stays and is reported. A file that isn't
where the import put it (renamed, moved or deleted) is not touched and is
listed as missing; while any are missing the import is **not** marked undone,
and what did go to the trash leaves the record, so undo can be run again once
they are back. The desktop saves open notes first, so typing that autosave
hasn't written yet keeps its note and tab. Nothing outside the record is
touched, and a record can't reach outside its own `folder`, into hidden
folders (`.quire`, `.trash`) or through `..`: such entries are ignored.
Nothing is overwritten by an import: every file is created only if its name
is free (a name that appears meanwhile is never replaced). A note whose name
is taken gets a new one (`Name 1.md`), and the record holds the name used. An
attachment whose name is taken by a *different* file gets a new name too, and
the imported notes' `[[links]]` to it are rewritten; one that is
byte-identical to the file already there is not copied again.

Each imported note also gets a history snapshot with provenance
("Imported (Joplin)", format in [provenance.md](provenance.md)). Records
are device-independent and travel with the vault; a reader ignores keys and
`format` values it doesn't know and never rewrites a record it can't read.
