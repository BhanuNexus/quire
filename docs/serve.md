# Your vault in a browser, and for agents: `quire-cli serve`

> **Beta, for self-hosters.** `quire-cli serve` is your agents and your
> browser, on a server you own. The server reads your notes as plain files,
> so it holds a **decrypted copy** of every folder it serves or syncs,
> unlike Quire Sync, whose server only ever sees encrypted data. Run it
> only on a machine you trust. It is not "Quire on the web": an end-to-end
> encrypted app that runs in the browser is planned separately. Expect
> rough edges, and tell us about them.

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
`quire-cli sync scope ~/vault --only …` or `--all`, while nothing syncs the
vault on this machine: `scope` refuses while `sync watch`, `serve --sync` or
the app runs (stop it, change the folders, start it again). Narrowing
deletes nothing: the files left out stay on this disk, and nothing done to
them is sent while they're left out. Widening fetches the notes now
included as they are on the other devices; a copy here that was changed
meanwhile is merged with them (or kept beside them if the changes overlap),
and an unchanged old copy of a note that moved or was deleted elsewhere goes
to the vault's `.trash/`. Folder names are compared exactly, letter case
included.

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

**One sync per vault on a machine.** Only one process syncs a vault folder
at a time (a lock on `.quire/sync/lock`): two would each send a new note as a
note of its own, and every device would get it twice. If the Quire app on
the same machine already syncs `~/vault`, `sync watch` and `serve --sync`
say "Quire is already syncing this vault (the app or `quire-cli serve`)",
wait, and take over when it stops; the app does the same next to a running
`serve --sync` (Settings → Sync shows "Synced by another Quire"). Either way
the files are synced once, and each sees the other's changes on disk.
`sync once` and `sync scope` refuse instead.

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
| `--sync [--poll 60s]` | Run `sync watch` in the same process (it waits while the app or another `quire-cli` syncs this vault here). |
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
callouts, links between notes, embedded images and PDFs, Excalidraw
drawings as pictures, voice notes and other recordings with a player, tags,
backlinks, code with colours, and (full edition) math and Mermaid diagrams. The sun
button switches light and dark; the layout works on a phone.

**Editing** is a plain Markdown text box with a preview, plus "New note".
Saving checks that the note hasn't changed since you opened it. If it has
(another device, an agent, sync), nothing is overwritten: when the changes
don't overlap you get both combined to check and save; when they do, you
see what differs and can save yours as a copy. Every save is written
atomically and kept in the note's history as "You · via web · home server",
with the version before it, so it can be undone in the app. The check that
the note is still the version you started from is made at the moment of
writing, as one step with the write, so a sync pass or an agent writing at
that instant is never overwritten. "Save mine as a copy" picks the copy's
name in the same step: two copies saved at once get two names.

**Sessions** end after 8 hours without use and 7 days after signing in, or
when you sign out or change the passphrase. After 5 wrong passphrases, each
further try waits (1 second, doubling, up to 15 minutes) for everyone; a
correct one clears it. After signing in you go back to the page you
asked for, only ever a page of this server. Restarting the server signs
everyone out.

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

Agents' changes outside agent zones arrive as suggestions. The web app
doesn't review them yet: you review them in the Quire app, below.

### Agents on the server, review on your desktop

An agent connected to `serve` writes on the server, so its suggestions and
its run history are made there. To review them in the app on your desktop
(and undo a run from there), turn on **"Also sync vault settings and review
history"**: Settings → Sync in the app, or on the server

```sh
quire-cli sync share ~/vault on
```

It needs Quire 0.9 or later on every device (through a Quire Sync server,
older devices are then refused; through a sync folder, 0.8 devices keep
syncing notes only). Then, with `serve --sync` (or `sync watch`) running:

* **Suggestions** made on the server appear in the desktop's Inbox after
  the next sync, marked "on home server". Review them hunk by hunk, Accept
  all or Reject all, as usual. What you decide goes back to the server:
  the note you changed syncs like any edit, and the suggestion is resolved
  there too, so it's never applied twice. If you and someone at the server
  decide the same suggestion at the same time, both devices end with the
  same outcome.
* **Rejected stays rejected.** If the agent on the server writes again and
  brings back a line you rejected (in other letter case or spacing, with
  other closing punctuation, or with the lines around it rewritten), it
  isn't shown again. A line with any word, number or date changed is a new
  suggestion and is asked about: a corrected date is never hidden.
* **Activity** shows the server's runs on their own lanes, under the
  server's name, and **Revert run** works from the desktop: the server
  carries the versions an undo needs, and your edits made since are kept
  (overlapping ones become a suggestion to review).

Without it, suggestions stay on the device where the agent ran, and the
Inbox says "Suggestions from other devices appear here when 'Also sync
vault settings and review history' is on". The shared review history is
small (a few hundred KB for most vaults; nothing over 4 MB per file),
kept for 30 days after a suggestion is resolved and three months for runs,
and kept out of git. Details: [design/0.10-agentsync.md](design/0.10-agentsync.md).

## Locked folders

Folders locked in the Quire app ([design](design/0.11-secure.md)) don't
exist for `serve`: the web app doesn't list, search, show or accept new
notes in them, and agents never see them. `serve` holds no key, so it
can't decrypt them, and nothing plain can be written into them. They still
sync as their encrypted files (`--sync`), so the server keeps an encrypted
copy. Unlocking in the browser isn't offered: it would put the key on the
server; the planned end-to-end encrypted Quire Web is where it belongs.

## Security, in short

* Every request needs the web passphrase (a session) or an MCP token, on
  loopback too.
* Session cookies are `HttpOnly` and `SameSite=Strict`, and `Secure` (named
  `__Host-…`) when the public URL is https. Every form that changes
  something carries a per-session token, and must come from this server's
  own address (`Origin`). Requests for another `Host` are refused
  (DNS rebinding).
* Pages may run only this server's own scripts. HTML inside a note (a
  captured web page, an agent's text, a note from another device) is
  cleaned to an allow-list before it is shown, here and on published sites:
  formatting, tables, callouts, `details`, images and audio stay;
  `<meta http-equiv>` (a refresh to another site), `<base>`, forms,
  frames, `object`, `embed`, SVG and MathML in the note, scripts and
  every `on…` attribute go, and links may only be `http(s)`, `mailto`,
  `tel` or relative. The page's policy (CSP) also sets `base-uri 'none'`,
  `form-action 'self'` and `frame-ancestors 'none'`. (CSP's `navigate-to`,
  which would limit where a page goes, was dropped from the standard and no
  browser supports it, so cleaning the HTML is what stops it.) Files from
  the vault are sent as downloads or in a sandbox (PDFs excepted, so the
  browser's viewer works).
* An Excalidraw drawing's pictures pass the same path check as every
  other file: a drawing can't pull in a file from a hidden folder, a
  folder hidden from the web, `.quire`, or outside the vault through a
  symlink.
* Wrong passphrases are limited (see Sessions above). Each try is counted
  before the passphrase is checked, so tries sent at once don't get extra
  guesses, and at most two are checked at a time (each check takes about
  19 MB); others are asked to wait a second.
* Pages and vault files are sent with `Cache-Control: no-store`, which tells browsers
  and proxies not to keep a copy (only the app's own scripts and styles are cached).
* What's not protected: anyone who controls the server, or gets the web
  passphrase, can read and change the notes held there. Pick a long
  passphrase and hold only the folders you need.
