# Fuzzing

Coverage-guided fuzzing (`cargo-fuzz`, libFuzzer) of the readers of bytes that
another device, a server or a file can send. This crate is **outside the
workspace** (its own `[workspace]` table, also listed in the root `exclude`):
`--workspace` commands, `cargo deny`, the layering check and releases never see
it, and the project stays on stable Rust. Running a target needs a nightly
compiler; `cargo check` here works on stable.

| Target | Reads | Checks |
|---|---|---|
| `front_matter` | front matter, `set_field`, `remove_field`, `set_field_path` | returns, range in bounds |
| `scan_inlines` | wikilinks, tags, highlights, citations, verbatim ranges | returns, ranges ordered and on boundaries |
| `obsidian_expr` | Bases expressions: parse and evaluate | returns within the limits of `expr::limits` |
| `sync_wire` | a revision and a snapshot manifest (JSON), folder and part names | returns; what is read writes back and reads again the same |
| `sync_xml` | S3 and WebDAV listings and error bodies | returns, never panics |

Not fuzzed: the sealed payload inside a revision (`Payload::open` needs the
vault's keys, so random bytes only fail the tag), the folder-mirror reader, the
canvas and Excalidraw readers, the importers' zip and tar readers, and the MCP
JSON-RPC body. Add a target by copying one in `fuzz_targets/` and adding a
`[[bin]]` entry to `Cargo.toml`.

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run front_matter -- -max_total_time=60   # from the repository root
cargo check --manifest-path fuzz/Cargo.toml --locked          # stable: does it still compile?
```

A crash is saved to `fuzz/artifacts/<target>/`. Reproduce it with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`, fix the bug,
and add the minimised input as a regression test next to the code. CI runs every
target for a few minutes each week (`.github/workflows/fuzz.yml`) and uploads
crashes as an artifact.
