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
  it (subfolders too) belongs to it. Locked folders don't nest.
* `quire-lock.unlocking.json` instead means the folder is being turned
  back into a plain one; until that finishes, a file may exist in both
  forms, and the plain one is current.
* A file `X` of the folder is stored as `X.qenc`. If both `X` and `X.qenc`
  exist, a conversion was interrupted and `X` is current.
* File and folder names are not encrypted.

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

### Getting the folder key

From the **passphrase**:

1. `wrap_key = Argon2id(password = passphrase as UTF-8, salt, m = memory_kib, t = iterations, p = parallelism, length 32)`.
2. `folder_key = XChaCha20-Poly1305-Open(key = wrap_key, nonce = wrapped_key[0..24], ciphertext = wrapped_key[24..], aad = "quire-lock key v1" ‖ folder_id bytes)`.
   A failure means a wrong passphrase.
3. Check that `check` matches (below); a mismatch means a tampered key file.

From the **recovery key** (shown once when the folder was locked): it *is*
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
  who can write the folder could swap two of its files (but read or change
  neither).
* There is no padding: a file's size shows roughly how big it is.
* A snapshot (`kind` 2) decrypts to the gzip of the note's text, as a plain
  `.md.gz` snapshot holds.

## Rules Quire follows

* Nothing plain is written into a locked folder: storage refuses it
  (`VaultError::Locked`), whoever writes (the app, the CLI, sync, agents).
  Only `*.qenc` files and the folder's own key file are written there.
* `Vault::notes()` never lists a note in a locked folder, so the vault index
  and everything built on it (and every cache) never holds one.
* Agents never see a locked folder, or any `*.qenc` or key file.
* Quire Sync carries the `.qenc` files and the key file as attachments
  (opaque bytes); they are never merged.

## Test vectors

`crates/quire-vault/src/lock/testdata/`: `quire-lock.json` (passphrase
`pass phrase`, folder key 32 bytes of `0x2a`, folder id 16 bytes of
`0x07`, test Argon2id settings) and `sealed.hex`, a `.qenc` file whose
plain text is `quire lock v1`. The recovery key of that folder key is
`58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E`.
