# Run your own Quire Sync server

*Status: beta. Your notes stay plain files on every device, so nothing is lost if sync misbehaves, but tell us about anything odd.*

> **You may not need a server.** Quire Sync also works through a folder that Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing already keeps on your devices: **Settings → Sync → A synced folder (free)**. It's free, encrypted the same way, and there's nothing to run. A server is worth it when you want changes to show up within a second, or don't use a file sync service.

Quire Sync keeps your vault the same on every device. The server only ever stores encrypted data: it can't read your notes, their names or your folder names. The design is in [design/sync.md](design/sync.md).

The server is one program with one database file. It's licensed under the AGPL like the rest of Quire.

## Self-hosting in order

1. **Pick a token** (step 1).
2. **Run the server**: Docker (2a) or the binary with systemd (2b). The release has the server for **Linux x86_64 only**; on ARM64, build it yourself (Docker does, on an ARM64 machine).
3. **Put HTTPS in front** (step 3) and check `https://…/health`.
4. **Make the first vault**: in the desktop app, or on a machine with only `quire-cli` (step 4).
5. **Join more devices** (step 4).
6. **Back up** the database file (Backups).
7. **Optional:** `quire-cli serve`, to read and edit in a browser and let agents in. It is a different program and holds your notes in plain text: [serve.md](serve.md).

## What you need

- A machine that's always on: a small VPS, a home server or a NAS.
- A domain name with HTTPS in front of the server. Caddy is the simplest option (below). Don't expose the server over plain HTTP on the internet. The contents are encrypted either way, but the token isn't.

## 1. Pick a token

Every device sends the same secret token to the server. Make it long and random:

```bash
openssl rand -hex 32
```

## 2a. Run with Docker

There is no prebuilt image: you build it from the source, which takes a few minutes. You need Docker and git.

```bash
git clone https://github.com/BhanuNexus/quire && cd quire
git checkout vX.Y.Z      # the release you want (see Releases); skip for the newest development version
docker build -f apps/quire-sync-server/Dockerfile -t quire-sync-server .
docker run -d --name quire-sync --restart unless-stopped -p 127.0.0.1:8787:8787 \
  -e QUIRE_SYNC_TOKEN=<your token> -v quire-sync:/data quire-sync-server
```

`-p 127.0.0.1:8787:8787` keeps the port on this machine, for the HTTPS proxy of step 3. Don't write it as `-p 8787:8787`: that publishes plain HTTP on every network interface. The token in `-e` shows in `docker inspect` and in your shell history; to avoid that, put `QUIRE_SYNC_TOKEN=<your token>` in a file readable only by you and use `--env-file <file>` instead. `docker ps` shows *healthy* once the server answers `/health`.

## 2b. Or run the binary

Download `quire-sync-server-<version>-linux-x86_64.tar.gz` from [Releases](https://github.com/BhanuNexus/quire/releases) (x86_64 only; on ARM64 build it with `cargo build --release -p quire-sync-server`). To try it (the database file is made in the current folder):

```bash
tar -xzf quire-sync-server-<version>-linux-x86_64.tar.gz
cd quire-sync-server-<version>-linux-x86_64
QUIRE_SYNC_TOKEN=<your token> ./quire-sync-server
```

To keep it running, install it and run it with systemd. The token goes in a file only the server can read, not on a command line:

```bash
# from the folder that holds the downloaded .tar.gz (skip the tar line if you just extracted it)
tar -xzf quire-sync-server-<version>-linux-x86_64.tar.gz
sudo install -m 755 quire-sync-server-<version>-linux-x86_64/quire-sync-server /usr/local/bin/
sudo useradd --system --home-dir /var/lib/quire-sync --create-home --shell /usr/sbin/nologin quire-sync
sudo install -d -m 755 /etc/quire-sync
sudo install -m 640 -o root -g quire-sync /dev/null /etc/quire-sync/token    # an empty file with its final permissions
openssl rand -hex 32 | sudo tee /etc/quire-sync/token > /dev/null            # or put the token from step 1 in this file
```

`/etc/systemd/system/quire-sync.service`:

```ini
[Unit]
Description=Quire Sync server
After=network-online.target
Wants=network-online.target

[Service]
User=quire-sync
Environment=QUIRE_SYNC_ADDR=127.0.0.1:8787
Environment=QUIRE_SYNC_DB=/var/lib/quire-sync/sync.sqlite
Environment=QUIRE_SYNC_TOKEN_FILE=/etc/quire-sync/token
ExecStart=/usr/local/bin/quire-sync-server
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now quire-sync
journalctl -u quire-sync -f     # "listening on 127.0.0.1:8787"
```

| Variable | Default | Meaning |
|---|---|---|
| `QUIRE_SYNC_TOKEN` | (required) | The shared secret (24 characters at least) |
| `QUIRE_SYNC_TOKEN_FILE` | | Read the token from a file instead, e.g. a Docker or systemd secret |
| `QUIRE_SYNC_ADDR` | `127.0.0.1:8787` | Where to listen |
| `QUIRE_SYNC_DB` | `quire-sync.sqlite` | The database file |
| `QUIRE_SYNC_KEEP_DAYS` | (keep everything) | 0.13: once a day, drop revisions replaced more than this many days ago. Each note's current version and its newest `QUIRE_SYNC_KEEP_VERSIONS` stay, and so does a deleted note's marker; a device offline for longer still catches up |
| `QUIRE_SYNC_KEEP_VERSIONS` | `20` | The versions of each note compaction always keeps (at least 2, so a deleted note keeps its last text) |

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

**No desktop on the first machine?** A headless machine with only `quire-cli` can start the vault from a folder of notes, and keeps the recovery key out of the terminal:

```bash
mkdir -p ~/vault     # the folder must exist; put your notes in it, or leave it empty
quire-cli sync join ~/vault --server https://sync.example.com --create --recovery-key-file ~/vault-recovery-key.txt --device "home server"
```

It asks for the server token and a passphrase (or read them from `--token-file` and `--passphrase-file`). The other options, and serving the vault to a browser, are in [serve.md](serve.md#1-sync-the-vault-onto-the-machine).

On every other device: open the folder you want to sync (an empty one is fine), **Settings → Sync**, enter the address and token, choose **Join from another device**, **Find vaults on the server**, pick the vault and enter the passphrase (or the recovery key).

The status bar shows *Synced*, *Syncing…*, *Offline* or *Sync needs attention*; click it to open Settings → Sync.

**Device tokens (0.13).** The server's token is only for setting devices up: at its first sync, each 0.13 device trades it for a token of its own and keeps only that. Removing a device (Settings → Sync → *Devices and sync format*) makes the server refuse its token, and the other devices get a new vault key. A vault in the 0.13 sync format (new vaults, or after *Upgrade sync format…*) takes device tokens only, and tells Quire 0.12 and older to update. If a removed device might know the server's own token (it was set up with Quire 0.12, or the token is written down somewhere), change `QUIRE_SYNC_TOKEN`: devices with their own tokens don't need it, only new devices joining.

## Backups

Back up the database file (with the server stopped, or with `sqlite3 quire-sync.sqlite ".backup backup.sqlite"`). A backup is still encrypted, so it's safe to keep offsite. Your notes are also plain files on each device, so the server is never the only copy.

## What the server can see

It can see how many notes a vault has, roughly how big each change is (sizes are padded), when devices sync, and which changes belong to the same note. It can't see text, note or folder names, tags or links. Someone who steals the database learns nothing you wrote.

## Limits of this preview

- One server token, so the server is meant for one person or one household (each device then has its own token). Accounts come with the managed service.
- Attachments (images, PDFs and other files) sync up to 10 MB each; bigger files stay on the device they're on.
- Version history stays on each device; restoring old versions from the server comes later.
