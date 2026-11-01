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
