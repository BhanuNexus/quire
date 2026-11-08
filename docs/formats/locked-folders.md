# Locked folders: `quire-lock.json` and `*.qenc`

A **locked folder** (Quire 0.11) keeps its notes and attachments encrypted
on disk. This page is enough to decrypt one without Quire. Design and
threat model: [../design/0.11-secure.md](../design/0.11-secure.md).
Owner: 0.11 security (`quire_vault::lock`).

```
Journal/                          the locked folder (any depth below the vault root)
  quire-lock.json                 its key, wrapped by the passphrase
  2026-11-06.md.qenc              the note 2026-11-06.md, encrypted
  Trips/Lisbon.md.qenc            subfolders belong to the locked folder
  photo.jpg.qenc                  an attachment
.quire/history/<fnv>/<ms>.md.gz.qenc   a history snapshot of one of its notes
```

* A folder is locked when it contains `quire-lock.json`. Everything below
  it (subfolders too, hidden ones included) belongs to it. Locked folders
  don't nest.
* A device that has seen a folder locked remembers that apart from the key
  file ([below](#remembered-locked-folders)): a missing key file never
  makes the folder plain again.
* `quire-lock.unlocking.json` instead means the folder is being turned
  back into a plain one by the device doing it; until that finishes, a file
  may exist in both forms, and the plain one is current. A device that
  remembers the folder as locked and isn't running that conversion treats
  the folder as locked (the file could have been renamed by anyone who can
  write the folder).
* A file `X` of the folder is stored as `X.qenc`. If both `X` and `X.qenc`
  exist in a locked folder, **`X.qenc` is the file**: the plain `X` is a
  conflict (an interrupted conversion, or a file another program or a
  device without locked folders put there). Quire never reads it in place
  of `X.qenc`, never saves over it and never encrypts it over `X.qenc`; it
  reports it and offers to keep both (the plain one is encrypted under a
  new name, `X (plain copy)`). A plain `X` with no `X.qenc` is a file a
  conversion hasn't reached yet: running "Lock a folder" again encrypts it.
* File and folder names are not encrypted, and the number of files and
  their approximate sizes show.

## The key file: `quire-lock.json`

JSON, `format` 1. Unknown fields are kept when Quire rewrites it.

```json
{
  "format": 1,
  "type": "quire-locked-folder",
  "folder_id": "07070707070707070707070707070707",
  "created": 1793000000000,
  "kdf": { "name": "argon2id", "memory_kib": 65536, "iterations": 3, "parallelism": 1 },
  "salt": "0d0db6ff4263b17beae7e7f916a7b184",
  "wrapped_key": "ea4c1ef2…",
  "check": "1d924365e74f4bd2f3b70c4d26090006"
}
```

| Field | Meaning |
|---|---|
| `type` | always `quire-locked-folder` |
| `folder_id` | 16 random bytes, hex; also in every file's header |
| `created` | Unix ms |
| `kdf` | Argon2id (version 0x13) settings: memory in KiB, passes, lanes. Output 32 bytes |
| `salt` | 16 random bytes, hex: the Argon2id salt |
| `wrapped_key` | hex of `nonce (24 bytes) ‖ ciphertext (32 bytes) ‖ tag (16 bytes)`: the folder key sealed with XChaCha20-Poly1305 |
| `check` | hex of the first 16 bytes of `HKDF(folder key, "check")`: tells whether a key (from the recovery key) is this folder's |
| `previous_key` | optional, after "Change the folder's key": hex of `nonce (24) ‖ ciphertext (32) ‖ tag (16)`, the previous folder key sealed with XChaCha20-Poly1305 under `HKDF(folder key, "previous")`, associated data `"quire-lock previous key v1" ‖ folder_id`. Opens files not encrypted again yet, or written meanwhile by a device that still had the old key |
| `rotating` | optional, `true` while a key change is under way (some files may still be under the previous key); running it again finishes it |

Quire accepts Argon2id settings from 65536 KiB and 3 passes (its default)
up to 4 GiB, 64 passes and 16 lanes, and refuses a key file outside that
range ("someone may have changed the file"): lower settings would make a
passphrase cheap to guess, higher ones would make unlocking hang. A new
passphrase is always wrapped with at least the default settings.

### Getting the folder key

From the **passphrase**:

1. `wrap_key = Argon2id(password = passphrase as UTF-8, salt, m = memory_kib, t = iterations, p = parallelism, length 32)`.
2. `folder_key = XChaCha20-Poly1305-Open(key = wrap_key, nonce = wrapped_key[0..24], ciphertext = wrapped_key[24..], aad = "quire-lock key v1" ‖ folder_id bytes)`.
   A failure means a wrong passphrase.
3. Check that `check` matches (below); a mismatch means a tampered key file.

From the **recovery key** (shown when the folder was locked, and again on
request after the passphrase): it *is*
the folder key, in the same form as Quire Sync's recovery key: 14 groups
of four characters from the Crockford alphabet
`0123456789ABCDEFGHJKMNPQRSTVWXYZ` (case-insensitive; `O` reads as `0`,
`I` and `L` as `1`; dashes and spaces are ignored). The first 52
characters are the 32 key bytes as base32 (5 bits each, most significant
first, the last 4 bits padding); the last 4 characters are the low 5 bits
of the first 4 bytes of SHA-256(key), a typo check.

### Subkeys

HKDF-SHA256 (RFC 5869) with salt `"quire-lock v1"` and input key material
the folder key; one 32-byte block of output:

* `file_key = HKDF(info = "file")`: encrypts `.qenc` files.
* `check = HKDF(info = "check")[0..16]`, hex in the key file.

## Encrypted files: `*.qenc`

```
offset  size  field
0       4     magic "QENC"
4       1     version = 1
5       1     kind: 1 = a file of the folder (its bytes), 2 = a history snapshot (gzip of the note text)
6       16    folder_id
22      24    nonce (random)
46      n+16  XChaCha20-Poly1305 ciphertext and tag
```

`plain = XChaCha20-Poly1305-Open(key = file_key, nonce, ciphertext, aad = bytes 0..22)`.

* The header up to the nonce is the associated data, so a file moved from
  another locked folder, or a history snapshot put in place of a note, fails
  to decrypt (and Quire says so; it never shows such a file as empty).
* The file name isn't bound: renaming needs no re-encryption, and someone
  who can write the folder could swap two of its files, or delete one (but
  read or change neither, or make a new one that opens). A plain file put
  next to an encrypted one is never taken for it (see above).
* There is no padding: a file's size shows roughly how big it is.
* A snapshot (`kind` 2) decrypts to the gzip of the note's text, as a plain
  `.md.gz` snapshot holds.

## Remembered locked folders

Whether a folder is locked can't hang on its key file alone: anyone who
can write the folder could delete it. So each device that sees a locked
folder (locks it, opens a vault with it, or receives its key file through
sync) records it, by folder id, in two files of the same format:

* `.quire/shared/locked-folders.json`, which travels with the vault (sync,
  git), so another device knows of the folder even before its key file
  arrives;
* `<config>/vaults/<vault key>/locked-folders.json`, this device's own
  (outside the vault: nobody who only reaches the vault can change it).

```json
{
  "format": 1,
  "folders": [{ "folder": "Journal", "id": "07070707070707070707070707070707", "check": "1d924365…" }]
}
```

`check` is the key file's check value, so a recovery key can be tested
even when the key file and every encrypted file are gone. A remembered
folder whose key file is missing (or renamed to the `unlocking` name by
someone else) stays locked: plain writes there are refused, and Quire
says to restore the key file or set a new passphrase with the recovery
key. A new key file is written only for a recovery key that opens one of
the folder's encrypted files, or, with none left, matches `check`. A
folder is forgotten only when nothing of it is left: no key file under
either name, no longer in the vault's record (the device that unlocked it
for good removed it there), and no `.qenc` file in it.

## What a conversion keeps on this device

* `.quire/lock/<folder id>.json`, while "Lock a folder" runs: for each file
  it encrypts, the plain path and a keyed fingerprint (first 16 bytes of
  HMAC-SHA256 under `HKDF(folder key, "journal")`, hex) of the bytes
  encrypted. A resumed run removes a plain file only if it still has the
  recorded fingerprint; one that has an encrypted copy with other content
  is then the stale original (the encrypted copy is newer, from another
  device) and goes, while a plain file the journal doesn't know is kept
  as a conflict. Deleted when the run finishes.
* `.quire/history/paths.jsonl`: one `{"dir": "<fnv>", "path": "…"}` line
  per history folder when it gets its first snapshot, so that locking a
  folder also encrypts the history of notes deleted or renamed before.

## Rules Quire follows

* Nothing plain is written into a locked folder: storage refuses it
  (`VaultError::Locked`), whoever writes (the app, the CLI, sync, agents),
  including into a remembered folder whose key file is missing, and a
  plain `X` wherever `X.qenc` exists. Only `*.qenc` files and the folder's
  own key file are written there.
* `Vault::notes()` never lists a note in a locked folder, so the vault index
  and everything built on it (and every cache) never holds one.
* Agents never see a locked folder, or any `*.qenc` or key file.
* Quire Sync carries the `.qenc` files and the key file as attachments
  (opaque bytes); they are never merged.

## Test vectors

`crates/quire-vault/src/lock/testdata/`: `quire-lock.json` (passphrase
`pass phrase`, folder key 32 bytes of `0x2a`, folder id 16 bytes of
`0x07`, test Argon2id settings below the accepted range: only Quire's own
tests read it) and `sealed.hex`, a `.qenc` file whose
plain text is `quire lock v1`. The recovery key of that folder key is
`58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E`.
