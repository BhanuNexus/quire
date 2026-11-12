# Sync through S3-compatible storage or WebDAV

Quire 0.12 can sync a vault straight to a bucket you own (AWS S3, Cloudflare R2, Backblaze B2, Wasabi, MinIO, or anything else that speaks the S3 API) or to a WebDAV folder (Nextcloud, ownCloud, Fastmail Files, Synology, `rclone serve webdav`). Nothing else to run.

It is **end-to-end encrypted** like every Quire Sync: notes are encrypted on your device with a key wrapped by your sync passphrase, and the storage only ever holds ciphertext under random ids and numbers. It can't read note text, note or folder names, tags or links. It can see how many devices and revisions there are, roughly how big each one is, and when each device syncs. The full list is in [the design note](design/0.12-sync.md#2-what-the-storage-can-see).

Each device writes only its own files and never changes one once written, so two devices syncing at the same time can't overwrite each other, on any of these services. Concurrent edits to one note merge line by line, or keep both versions as a conflict copy, as with every Quire Sync.

## Set it up

**In the app:** Settings → Sync → *S3-compatible storage* or *WebDAV*. Pick a provider to fill in the endpoint, fill in the rest, press **Test connection**, then **Start syncing** (first device) or **Find vaults in the storage** and **Join and sync** (other devices).

**Headless** (a server, `quire-cli serve --sync`):

```sh
# S3: the secret comes from the environment (or --secret-file, or a prompt), never the command line
export QUIRE_S3_SECRET_ACCESS_KEY=…           # AWS_SECRET_ACCESS_KEY works too (the CLI says when it uses it)
quire-cli sync join ~/vault --s3 https://s3.eu-central-1.amazonaws.com --region eu-central-1 \
    --bucket my-notes --prefix quire --access-key-id AKIA… [--create --recovery-key-file ~/quire-recovery.txt]

# WebDAV: the password from QUIRE_WEBDAV_PASSWORD, --password-file or a prompt
quire-cli sync join ~/vault --webdav https://cloud.example.com/remote.php/dav/files/asha/Quire --user asha

quire-cli sync test ~/vault          # sign-in, write, read back, conditional writes
quire-cli sync status ~/vault        # "Syncs through S3 bucket my-notes/quire at https://…"
quire-cli sync credentials ~/vault   # new credentials (expired or rotated), checked first
```

`--allow-http` permits `http://` to a computer on your own network (MinIO on a NAS, `rclone serve`). Without it, only `https://` is accepted, and `http://` only to this computer. An `http://` address is always reached directly, never through a proxy set in `HTTP_PROXY` or `ALL_PROXY`. Quire never follows redirects: use the address the storage redirects to.

Put the user name and password in their own fields, not in the address: `https://asha:…@cloud.example.com/…` is refused.

Credentials are kept in the system keychain (or Quire's private secrets file), never in the vault.

## Providers

The access key should be able to reach **only Quire's folder in one bucket**. Quire needs to list, read and write objects there, and to delete the one-off probe of *Test connection* (`quire-sync/.probe-…`). Since 0.13 it also deletes old revisions once they are compacted (below): a device deletes only objects it wrote itself and old snapshots. It never needs to create or delete buckets. A key that may not delete still syncs; old revisions then stay, and the status says so.

### AWS S3

1. Create a bucket (any region). Leave *Block all public access* on. Turn on **versioning**: Quire never overwrites revisions itself, but with versioning one that someone else overwrites or deletes can be restored. Don't add a lifecycle rule that expires current objects under Quire's folder (one that expires *noncurrent* versions after a while is fine).
2. Create an IAM user with this policy (replace `my-notes` and the `quire` prefix; with no prefix, use `quire-sync/*`):

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       { "Effect": "Allow", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::my-notes",
         "Condition": { "StringLike": { "s3:prefix": ["quire/quire-sync/*"] } } },
       { "Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
         "Resource": "arn:aws:s3:::my-notes/quire/quire-sync/*" }
     ]
   }
   ```

   Turn on versioning (step 1) so an overwritten or deleted object can be brought back; a lifecycle rule that expires *noncurrent* versions after 30 days or so keeps compaction's savings. To keep revisions forever instead, leave `s3:DeleteObject` out except for `quire/quire-sync/.probe-*` (as in 0.12): sync works the same, nothing is compacted away. See [Credentials and devices](#credentials-and-devices) for a key per device.
3. Create an access key for that user. Endpoint: `https://s3.<region>.amazonaws.com`, region as created. Quire uses `bucket.s3.<region>.amazonaws.com` URLs for AWS automatically, except for bucket names with dots.

Conditional writes: supported.

### Cloudflare R2

1. R2 → Create bucket. Then *Manage R2 API tokens* → *Create API token* → **Object Read & Write**, limited to that bucket.
2. Endpoint: `https://<ACCOUNT_ID>.r2.cloudflarestorage.com` (shown on the bucket's page), region `auto`. Use the token's *Access Key ID* and *Secret Access Key*.

R2 tokens are per bucket, not per prefix: use a bucket for Quire. Conditional writes: supported.

### Backblaze B2

1. Create a **private** bucket. *Application Keys* → *Add a New Application Key*: allow access to that bucket only, *Read and Write*, and optionally a *File name prefix* (`quire/quire-sync/`).
2. Endpoint: the bucket's *S3 Endpoint* with `https://` (for example `https://s3.us-west-004.backblazeb2.com`), region the middle part (`us-west-004`). Access key ID = *keyID*, secret = *applicationKey*.

B2 refuses `If-None-Match: *` with an error. Quire notices and works without it; nothing about sync changes ([why](design/0.12-sync.md#what-two-devices-at-once-can-and-cant-do)). *Test connection* says "no conditional writes".

### Wasabi

1. Create a bucket. Create a user with programmatic access and a policy like the AWS one above (Wasabi uses the same IAM policy language).
2. Endpoint `https://s3.<region>.wasabisys.com` (for example `https://s3.eu-central-1.wasabisys.com`), region as created.

We couldn't confirm whether Wasabi honours conditional writes; *Test connection* tells you, and Quire works either way.

### MinIO (self-hosted)

1. `mc mb local/my-notes`, then a user and policy limited to the bucket (the AWS policy above works as a MinIO policy).
2. Endpoint `https://minio.example.com` (or `http://127.0.0.1:9000` on the same machine; `--allow-http` / *Allow http://* for another machine on your network), region `us-east-1` unless you set another. Quire uses path-style URLs for everything except AWS.

Conditional writes: supported.

### Nextcloud and ownCloud

1. Create a folder for Quire (say `Quire`). In *Settings → Security*, create an **app password** (also needed when two-factor sign-in is on).
2. URL: `https://cloud.example.com/remote.php/dav/files/<user>/Quire` (Nextcloud shows the WebDAV address in *Files → Settings*). ownCloud: `https://owncloud.example.com/remote.php/webdav/Quire`.

Don't let the Nextcloud desktop client also sync that folder into your vault: it holds only Quire's encrypted files.

### Fastmail

*Settings → Privacy & Security → App passwords*: a new one with **Files (WebDAV)** access. URL `https://myfiles.fastmail.com/Quire` (create the folder in Fastmail's Files first), user your Fastmail address.

### Synology

Install **WebDAV Server** in Package Center and turn on HTTPS (port 5006 by default). Create a shared folder or a folder for Quire and a user that can write only there. URL `https://nas.example.com:5006/<shared folder>/Quire`.

### rclone

`rclone serve webdav remote:path --addr 127.0.0.1:8080 --user asha --pass …` turns anything rclone reaches into a WebDAV folder. Put HTTPS in front (`--cert`/`--key`, or a reverse proxy), or use `--allow-http` on your own network only. `rclone serve s3` should work through the S3 option as well (not tested).

## Credentials and devices

Every device uses the storage credential you give it. With one shared key, any device's copy of it (on a lost or retired laptop, say) can overwrite or, if the policy allows, delete every device's objects. Quire copes with a lost or damaged object (devices skip it, say so, and its writer uploads it again), but a key that can delete everything can stop sync for everyone. So:

* **Removing a device** (0.13) has two halves. **Remove it from the vault** (Settings → Sync → Devices and sync format → *Remove this device*, or `quire-cli sync remove-device`): the other devices get a new vault key, so it can't read anything written from then on, and no device takes what it writes from then on. **And take away its storage access**, because until you do, the key it holds can still delete or overwrite any object in the vault (which can stop sync and destroy the sync copy of history, though not the notes on each device): make a new key, give it to every other device (`quire-cli sync credentials`, or *Update credentials…*), and delete the old key. With a key per device (below), deleting the removed device's key is enough. What removal does and doesn't protect: [design 0.13 §2](design/0.13-sync.md#2-removing-a-device).
* **A key per device** (AWS, Wasabi, MinIO: anything with IAM-style policies) limits what a lost device can do. Each device only ever writes its own objects, so its key needs write access only to those. After a device joins, `quire-cli sync status` shows the vault id and this device's id; give that device's key this policy (fill in the bucket, the prefix, the vault id `SPACE` and the device id `DEVICE`):

  ```json
  {
    "Version": "2012-10-17",
    "Statement": [
      { "Effect": "Allow", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::my-notes",
        "Condition": { "StringLike": { "s3:prefix": ["quire/quire-sync/*"] } } },
      { "Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::my-notes/quire/quire-sync/*" },
      { "Effect": "Allow", "Action": ["s3:PutObject", "s3:DeleteObject"], "Resource": [
          "arn:aws:s3:::my-notes/quire/quire-sync/SPACE/devices/DEVICE/*",
          "arn:aws:s3:::my-notes/quire/quire-sync/SPACE/tips/DEVICE",
          "arn:aws:s3:::my-notes/quire/quire-sync/SPACE/snapshots/*-DEVICE/*",
          "arn:aws:s3:::my-notes/quire/quire-sync/.probe-*" ] },
      { "Effect": "Allow", "Action": "s3:PutObject", "Resource":
          "arn:aws:s3:::my-notes/quire/quire-sync/SPACE/keys/*-DEVICE.json" }
    ]
  }
  ```

  Join with the shared key first (joining reads `space.json`; making the vault and upgrading it write it), then switch that device to its own key with `sync credentials`. A lost device's key can then only touch its own revisions, snapshots and key records, and you can delete that key without touching the others. Old snapshots made by other devices then stay until their maker deletes them.
* **Versioning or object lock** (S3, B2, Wasabi, MinIO) keeps earlier versions of every object, so even a key that can delete can't destroy history for good. R2 has no versioning: use per-bucket tokens and keep the bucket for Quire alone.

## What it costs

Storage holds every note's current version, the last 90 days of changes and the newest 10 versions of each note, once the vault uses the 0.13 sync format and has been compacted (a snapshot replaces older revisions; each device deletes its own a week later). Before that, every revision stays: roughly the size of your edits over time, each padded to at least 256 bytes, plus each version of each attachment. A typical text vault holds a few MB.

Requests: each saved change is one upload, plus one small upload (the device's "tip") per sync pass. Each other device then downloads it once. While the app is open, it lists the devices' tips **once a minute** to notice changes, and runs a full pass every 2 minutes (one more listing; on WebDAV also one tiny download per device). That listing is what dominates costs where listings are billed.

Rough monthly costs for **one device with the app open 8 hours a day** and 50 changes a day (list prices at the time of writing; check the provider's page):

| Provider | Storage | Listings (~22 000) | Uploads (~3 000) | Downloads | Total, about |
|---|---|---|---|---|---|
| AWS S3 Standard | $0.023/GB | $0.005 per 1 000 → $0.11 | $0.005 per 1 000 → $0.02 | $0.0004 per 1 000, plus data out | **$0.15** per device |
| Cloudflare R2 | $0.015/GB, first 10 GB free | Class A, 1 million free | Class A | Class B, 10 million free; no egress fees | **$0** for most people |
| Backblaze B2 | about $6–7 per TB, first 10 GB free | Class C, 2 500 a day free | free | Class B, 2 500 a day free | **$0** for one or two devices |
| Wasabi | $6.99 per TB, billed as at least 1 TB, 90-day minimum | free | free | free | **$6.99** (the minimum) |
| MinIO, Nextcloud, rclone… | your disk | — | — | — | **$0** |

Several devices multiply the listing count. A device with the app closed costs nothing; `quire-cli sync watch --poll 5m` lists every 5 minutes instead of every minute.

## Moving a vault from one place to another

`quire-cli sync move ~/vault --s3 … --recovery-key-file F` (or `--webdav`, `--folder`, `--server`) runs one last pass where the vault syncs now, then makes a new sync vault in the new place from this device's notes, with a new passphrase and recovery key. The old setup is removed only once the new vault exists, so a move that fails leaves sync as it was. If changes made on this device haven't been uploaded to the old S3 or WebDAV storage yet, the move stops and says so; `--force` moves anyway (the changes are in your notes and go to the new place, but devices still on the old place won't get them). On each other device, run its last sync, then `quire-cli sync join` the new vault (or Settings → Sync → Stop syncing, then Join): notes already there are recognised, not copied. History stays in the old place until you delete it.

## When something's wrong

| Message | What to do |
|---|---|
| "The secret access key is wrong" / "doesn't know this access key ID" | Check the key; for R2 and B2 use the S3-compatible key pair |
| "The storage credentials have expired" | Make a new key, then *Update credentials…* or `quire-cli sync credentials` |
| "This computer's clock is N minutes behind…" | Turn on automatic time; S3 refuses requests more than 15 minutes off |
| "The region is wrong for this bucket" | Use the region the error names |
| "There's no bucket … at this address" / "That folder doesn't exist on the WebDAV server" | Create it, or fix the name or URL |
| "The storage redirected to … ; Quire doesn't follow redirects" | Use that address |
| "…certificate isn't trusted on this computer" | Install your server's certificate in the system, or use a certificate from a public authority |
| "N change(s) saved here, not uploaded yet" | Offline; they upload on the next pass that reaches the storage |
| "Waiting for N device(s)' latest changes…" | Another device's upload is still finishing |
| "Still downloading other devices' changes." | A large first sync: it continues on its own |
| "Skipped N revision(s) missing or damaged in the storage (device 1a2b3c4d revision 7…)" | Something deleted or damaged those objects (a lifecycle rule, a tidy-up, a server that kept part of an upload). Everything after them is synced. When that device next syncs it uploads deleted or cut-short ones again and they arrive; restore a corrupted one from the bucket's versions if you have them |
| "Revision N of device 1a2b3c4d is missing from the storage" | The newest revision a device announced isn't there after 10 minutes. It's uploaded again when that device next syncs |
| "This device's tip in the storage says it uploaded N revisions, but…" | This device's mirror was lost and the storage holds far fewer of its revisions than its tip says: turn sync off and on again on this device |
| "Another computer is syncing as this one" | A copied vault folder: turn sync off and on again on one of them |
| "This vault was made by a newer Quire; update Quire on this device" | The vault uses the 0.13 sync format: update Quire on this device |
| "This device was removed from the sync vault on another device" | Join again with the passphrase (Settings → Sync), if it should sync |
| "Old revisions stay in the storage: this device's key can't delete them" | Give the key `s3:DeleteObject` on Quire's folder (policy above), or leave it: sync works either way |
| "Some changes use the vault's new key, which this device doesn't have yet" | Another device removed a device; this one takes the key at its next pass. If it stays, this device wasn't given the key: join again |

## Testing against your own server

The integration tests run against in-process mock servers by default. To run the two-devices-at-once check against real servers:

```sh
python3 -m venv /tmp/q && /tmp/q/bin/pip install 'moto[server]' wsgidav cheroot
QUIRE_SYNC_REAL_SERVERS=/tmp/q/bin/python cargo test -p quire-sync --test remote real_servers -- --nocapture
```
