# Security policy

## Reporting a vulnerability

**Please don't open a public issue.** Report privately through GitHub: open the repository's **Security** tab, then **Report a vulnerability**. This creates a private advisory that only maintainers can see.

Please include the affected version, the steps to reproduce, and the impact you expect. We aim to acknowledge reports within 3 working days. Once a fix is released we'll publish an advisory and credit you, unless you'd rather stay anonymous.

## Supported versions

Only the latest release gets security fixes while Quire is pre-1.0.

## Scope notes

These areas matter most:

- **Notes are plain files** in the folder you choose. Quire never uploads them unless you configure an AI provider or embeddings endpoint in Settings.
- **API keys** are stored in the OS keychain. A 0600 file is the fallback when no keychain exists. Keys are never written to the settings file.
- **The MCP server** (`quire-cli mcp`) is read-only unless started with `--allow-write`. Every write it makes is versioned.
- **Importers** parse untrusted archives (zip, enex). Bugs that let an export write outside the target vault folder are in scope.
- **No code from your notes ever runs.** A note is text. Quire shows it; it never executes it. `<script>`, event handlers (`onclick=`), `javascript:` links, frames, forms and active `style` values are removed or shown as text in the preview, published sites, the web app, HTML export and copy as HTML. `dataviewjs` and Templater (`<% … %>`) blocks stay as text, and Quire has no script engine for them. Views, Dataview and Tasks queries are read by Quire's own parser, not run as code. What this does not cover: a link you click is opened by your system (browser or app), the AI agent command in Settings and the git program run what you configured, and Typst, which draws PDF and math, contains a WebAssembly interpreter for its plugins that note text cannot reach (note text is escaped into string literals). Bugs that make a note run code are in scope.
