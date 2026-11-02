# Agent policy: `.quire/shared/agent-policy.toml`

What agents may do where. It travels with the vault, so a change made
elsewhere only takes effect on a device after the person approves it there
(design §A.2, P7). Owner: WS-A. Types: `quire_agent::policy`.

```toml
format = 1
default = "propose"                     # deny | read | propose | create-only | direct
[[zone]]
path = "wiki/"                          # prefix, or glob with * and **
role = "agent"                          # agent | raw | human | private
mode = "direct"
[[zone]]
path = "raw/"
role = "raw"
mode = "create-only"
[limits]
writes_per_minute = 30
max_write_bytes = 262144
max_files_per_change = 50
max_pending_per_client = 200
[clients."mcp:claude-code"]
default = "propose"
zones = { "wiki/" = "direct" }
approve_in_client = false
[[contract]]
name = "concept pages"
required = ["type", "sources"]
```

Without the file, the defaults are the zones above. How a mode is worked
out (`AgentStore::effective_mode`):

1. People are never limited. For agents:
2. the zone that matches most specifically (longest literal prefix; at equal
   length a literal beats a glob; matching ignores case);
3. hard rules: `.quire/**` is `deny`; root `AGENTS.md` and `CLAUDE.md`,
   any `SKILL.md` and the templates folder are at most `propose`;
4. the caller's grant (stdio without `--allow-write` is `read`).

The most restrictive outcome wins. **Today** (F0) the policy file can only
narrow the defaults: until device approval exists, the stricter of the
file's mode and the default mode applies. Client overrides and contracts are
parsed and kept but not applied yet. `create-only` lets agents add files;
editing, renaming or deleting one is refused with `raw_is_immutable`.
