# Your vault in a browser, and for agents: `quire-cli serve`

Quire Sync is end-to-end encrypted, so the sync server can't show your
notes to a browser or to an AI agent. A **headless Quire** can: a copy of
the vault on a machine you control, kept in sync, serving

* a **web app** at `https://notes.example.com/` to read, search and lightly
  edit notes from any browser (a work laptop, a phone), with nothing
  installed and nothing downloaded there, and
* **MCP** at `https://notes.example.com/mcp`, so agents (Claude, an agent in
  a VM) use the vault through Quire's tools, zones and review, without a
  copy of the vault.

Both share one port and one process. The notes on the server are plain
Markdown files, like on every other device.

## What you need

* An always-on machine you trust with the plain text of your notes: a small
  VPS, a home server, a NAS. Unlike the sync server, this machine *can*
  read the notes it holds. Hold only the folders it needs (`--only`, below).
* `quire-cli` on it: the Linux archive from
  [Releases](https://github.com/BhanuNexus/quire/releases), or
  `cargo build --release -p quire-cli`. Quire Lite's `quire-cli` works too
  (math and diagrams then show as source).
* For use beyond the machine itself: a domain name and HTTPS in front
  (Caddy below, nginx, or a Cloudflare Tunnel). Quire doesn't do TLS itself.

## 1. Sync the vault onto the machine

On the server, make an empty folder and join the vault you already sync
from the app:

```sh
mkdir -p ~/vault
# Through a Quire Sync server (asks for the server token, then the passphrase):
quire-cli sync join ~/vault --server https://sync.example.com --device "home server"
# Or through a folder Syncthing/Dropbox keeps on this machine:
quire-cli sync join ~/vault --folder ~/Sync --device "home server"
```

* The sync passphrase comes from a prompt (not shown as you type),
  `QUIRE_SYNC_PASSPHRASE`, or `--passphrase-file FILE`; a server token from
  a prompt, `QUIRE_SYNC_TOKEN` or `--token-file FILE`. Neither is ever put
  on the command line or printed. The vault key and token are kept like the
  app keeps them: in the system keychain, or else in `secrets.toml` in
  Quire's config folder, readable only by you (`QUIRE_SECRETS_FILE` picks
  another file).
* With several sync vaults there, add `--space ID` (`join` lists them).
* `--create --recovery-key-file FILE` starts a new sync vault from this
  folder instead; the recovery key goes into `FILE` (readable only by you),
  never to the screen.

**Only some folders.** `--only Work --only Projects/2026` keeps just those
folders on the server; `--exclude Journal` leaves one out. Notes outside the
selection are never written there and are never sent as deleted: the other
devices keep them whatever happens on the server. A note moved into an
unselected folder on the server is sent as a move. Change it later with
`quire-cli sync scope ~/vault --only …` or `--all`; widening fetches the
notes now included. Folder names are compared exactly, letter case included.

Then:

```sh
quire-cli sync once ~/vault      # one pass
quire-cli sync watch ~/vault     # keep syncing (every minute, and at once when something changes)
quire-cli sync status ~/vault    # how it's set up and how the last pass went
```

Conflicts, history and "who wrote this" work as in the app: overlapping
edits keep both versions (`Note (conflict from laptop 2026-11-05).md`), an
edit beats a delete. `watch` stops on Ctrl+C or SIGTERM after finishing the
pass under way, so no file is left half-written.

## 2. Serve it

Set the web app's passphrase once (at least 10 characters; it is stored as
an Argon2id hash, and changing it signs everyone out):

```sh
quire-cli serve passwd
```

Then:

```sh
quire-cli serve ~/vault --sync --public-url https://notes.example.com
```

| Option | Meaning |
|---|---|
| `--port N` | Port (default 7311). |
| `--bind ADDR` | Address to listen on (default `127.0.0.1`). Anything else prints a warning; every address (`0.0.0.0`) also needs `--public-url`. |
| `--public-url URL` | The address people and apps use, behind your proxy. Needed for OAuth and for `Origin`/`Host` checks to accept it. |
| `--sync [--poll 60s]` | Run `sync watch` in the same process. |
| `--read-only` | No editing in the web app and no writes over MCP. |
| `--hide FOLDER` | Hide a folder from the web app (repeatable). |
| `--allow-private-urls` | Let MCP's `capture_url` fetch private and loopback addresses. |

To hide folders from the web app for every server of a vault, put them in
`.quire/shared/vault.toml`:

```toml
[web]
hide = ["Journal/", "Health"]
```

Hidden folders don't exist for the web app: not in the file list, search,
quick open, tags, backlinks or links, and their addresses answer "not
found" (letter case doesn't matter). Agents follow the vault's agent
policy instead: a `private` zone (`mode = "deny"`) is invisible to them,
but still visible to you on the web unless you hide it there too.
`.quire/`, `.trash/` and other hidden files are never shown.

## 3. Put HTTPS in front (Caddy)

```caddyfile
notes.example.com {
    reverse_proxy 127.0.0.1:7311
}
```

Caddy gets a certificate by itself. With nginx, proxy to
`http://127.0.0.1:7311`, pass the `Host` header unchanged
(`proxy_set_header Host $host;`) and turn buffering off for `/mcp`
(`proxy_buffering off;`) so event streams arrive at once. A Cloudflare
Tunnel works the same way (`cloudflared tunnel --url http://127.0.0.1:7311`,
with the tunnel's hostname as `--public-url`).

Don't expose port 7311 itself to the internet: passphrases and tokens would
travel unencrypted.

## 4. Keep it running (systemd)

`~/.config/systemd/user/quire-serve.service`:

```ini
[Unit]
Description=Quire: web app, MCP and sync for ~/vault
After=network-online.target

[Service]
ExecStart=%h/.local/bin/quire-cli serve %h/vault --sync --public-url https://notes.example.com
Restart=on-failure
# SIGTERM lets the current sync pass finish.
KillSignal=SIGTERM
TimeoutStopSec=60

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now quire-serve
loginctl enable-linger "$USER"     # keep it running when you're logged out
journalctl --user -u quire-serve -f
```

On a server without a keychain, the sync key is in the secrets file; the
service finds it as long as it runs as the same user.

## 5. Use it

**In a browser,** open `https://notes.example.com/` and sign in with the web
passphrase. The left side lists files; `Ctrl+K` opens a note by name; the
search box finds words in any note (accents and letter case don't matter,
`"exact phrase"` and `#tag` work). Notes show as in a published site:
callouts, links between notes, embedded images and PDFs, tags, backlinks,
code with colours, and (full edition) math and Mermaid diagrams. The sun
button switches light and dark; the layout works on a phone.

**Editing** is a plain Markdown text box with a preview, plus "New note".
Saving checks that the note hasn't changed since you opened it. If it has
(another device, an agent, sync), nothing is overwritten: when the changes
don't overlap you get both combined to check and save; when they do, you
see what differs and can save yours as a copy. Every save is written
atomically and kept in the note's history as "You · via web · home server",
with the version before it, so it can be undone in the app.

**Sessions** end after 8 hours without use and 7 days after signing in, or
when you sign out or change the passphrase. After 5 wrong passphrases, each
further try waits (1 second, doubling, up to 15 minutes) for everyone; a
correct one clears it. Restarting the server signs everyone out.

**Agents** connect to `https://notes.example.com/mcp`, exactly as with
`quire-cli mcp serve-http` ([agents.md](agents.md)):

* Apps that support MCP sign-in (Claude, Claude Code, …) find Quire's OAuth
  server by themselves. Approve the request in the web app (the sign-in page
  links to it, and `https://notes.example.com/consent` lists waiting
  requests) or with `quire-cli mcp approve <id>` on the server. Check that
  the code matches. Only the browser window that started the
  sign-in receives the result.
* For anything else, make a token: `quire-cli mcp token create --name vm
  --scope notes:read,notes:propose` and give it to the agent as a bearer
  token.
* Claude Code: `claude mcp add --transport http quire https://notes.example.com/mcp`.

Agents' changes outside agent zones arrive as suggestions; you review them
in the Quire app on any device once they sync (the web app doesn't review
suggestions yet).

## Security, in short

* Every request needs the web passphrase (a session) or an MCP token, on
  loopback too.
* Session cookies are `HttpOnly` and `SameSite=Strict`, and `Secure` (named
  `__Host-…`) when the public URL is https. Every form that changes
  something carries a per-session token, and must come from this server's
  own address (`Origin`). Requests for another `Host` are refused
  (DNS rebinding).
* Pages may run only this server's own scripts; HTML inside a note (a
  captured web page, an agent's text) can't run scripts, load anything from
  elsewhere, or be framed. Files from the vault are sent as downloads or in
  a sandbox (PDFs excepted, so the browser's viewer works).
* Pages and vault files are sent with `Cache-Control: no-store`, which tells browsers
  and proxies not to keep a copy (only the app's own scripts and styles are cached).
* What's not protected: anyone who controls the server, or gets the web
  passphrase, can read and change the notes held there. Pick a long
  passphrase and hold only the folders you need.
