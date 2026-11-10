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
  "undone_ms": null
}
```

| Key | Meaning |
|---|---|
| `id` | `i_` and the Unix milliseconds; the file's name |
| `source`, `origin` | what was imported, and the export's file or folder *name* (never its full path) |
| `folder` | the vault folder the import wrote into (`""` for the root) |
| `notes`, `files` | every file the import created, with the content hash it wrote |
| `undone_ms` | when it was undone |

**Undo** moves to the trash the files whose content still has the recorded
hash. A file edited since stays and is reported. Nothing outside the record
is touched, and a record can't reach outside its own `folder`, into hidden
folders (`.quire`, `.trash`) or through `..`: such entries are ignored.
Existing notes are never overwritten by an import (clashes get a new name,
and the record holds the name used).

Each imported note also gets a history snapshot with provenance
("Imported (Joplin)", format in [provenance.md](provenance.md)). Records
are device-independent and travel with the vault; a reader ignores keys and
`format` values it doesn't know and never rewrites a record it can't read.
