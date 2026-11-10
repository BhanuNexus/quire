# Run your own Quire Sync server

*Status: beta. Your notes stay plain files on every device, so nothing is lost if sync misbehaves, but tell us about anything odd.*

> **You may not need a server.** Quire Sync also works through a folder that Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing already keeps on your devices: **Settings → Sync → A synced folder (free)**. It's free, encrypted the same way, and there's nothing to run. A server is worth it when you want changes to show up within a second, or don't use a file sync service.

Quire Sync keeps your vault the same on every device. The server only ever stores encrypted data: it can't read your notes, their names or your folder names. The design is in [design/sync.md](design/sync.md).

The server is one program with one database file. It's licensed under the AGPL like the rest of Quire.

## What you need

- A machine that's always on: a small VPS, a home server or a NAS.
- A domain name with HTTPS in front of the server. Caddy is the simplest option (below). Don't expose the server over plain HTTP on the internet. The contents are encrypted either way, but the token isn't.

## 1. Pick a token

Every device sends the same secret token to the server. Make it long and random:

```bash
openssl rand -hex 32
```

## 2a. Run with Docker

```bash
docker build -f apps/quire-sync-server/Dockerfile -t quire-sync-server .
docker run -d --name quire-sync --restart unless-stopped -p 127.0.0.1:8787:8787 \
  -e QUIRE_SYNC_TOKEN=<your token> -v quire-sync:/data quire-sync-server
```

## 2b. Or run the binary

Download `quire-sync-server-<version>-linux-x86_64.tar.gz` from [Releases](https://github.com/BhanuNexus/quire/releases) (or build it with `cargo build --release -p quire-sync-server`), then:

```bash
QUIRE_SYNC_TOKEN=<your token> QUIRE_SYNC_DB=/var/lib/quire/sync.sqlite ./quire-sync-server
```

| Variable | Default | Meaning |
|---|---|---|
| `QUIRE_SYNC_TOKEN` | (required) | The shared secret (24 characters at least) |
| `QUIRE_SYNC_TOKEN_FILE` | | Read the token from a file instead, e.g. a Docker or systemd secret |
| `QUIRE_SYNC_ADDR` | `127.0.0.1:8787` | Where to listen |
| `QUIRE_SYNC_DB` | `quire-sync.sqlite` | The database file |

## 3. Put HTTPS in front

With [Caddy](https://caddyserver.com), this `Caddyfile` gets a certificate automatically:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Check it: `curl https://sync.example.com/health` prints `ok`.

## 4. Connect Quire

On the first device: **Settings → Sync**, enter the server address and token, choose **Start a new sync vault** and pick a passphrase. Quire shows a recovery key once; keep it somewhere safe.

On every other device: open the folder you want to sync (an empty one is fine), **Settings → Sync**, enter the address and token, choose **Join from another device**, **Find vaults on the server**, pick the vault and enter the passphrase (or the recovery key).

The status bar shows *Synced*, *Syncing…*, *Offline* or *Sync needs attention*; click it to open Settings → Sync.

## Backups

Back up the database file (with the server stopped, or with `sqlite3 quire-sync.sqlite ".backup backup.sqlite"`). A backup is still encrypted, so it's safe to keep offsite. Your notes are also plain files on each device, so the server is never the only copy.

## What the server can see

It can see how many notes a vault has, roughly how big each change is (sizes are padded), when devices sync, and which changes belong to the same note. It can't see text, note or folder names, tags or links. Someone who steals the database learns nothing you wrote.

## Limits of this preview

- One token per server, so the server is meant for one person or one household. Accounts come with the managed service.
- Attachments (images, PDFs and other files) sync up to 10 MB each; bigger files stay on the device they're on.
- Version history stays on each device; restoring old versions from the server comes later.
