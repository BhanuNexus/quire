# Dependency ignores

Two places tell a tool to leave a dependency alone: the advisory ignores in `deny.toml` and the `ignore` list in `.github/dependabot.yml`. This page says, for each one, why it is there and what lifts it. The reasons themselves are in the comments of those files and in `docs/design/0.13.1-deps.md`.

`cargo deny check advisories` runs on every push and pull request (CI's `deny` job) and every Monday (`.github/workflows/audit.yml`), so a new advisory is seen within a week even when nobody pushes.

To see what holds a crate: `cargo tree -i <crate>`. When a crate has left the tree, delete its ignore.

## `deny.toml` advisory ignores (13)

| Advisory | Crate | Why it is ignored | Lift it when |
|---|---|---|---|
| RUSTSEC-2026-0370, -0371, -0372 | `zvariant` and `zbus_names` (three advisories) | Merged into zbus 6 (not abandoned, no vulnerability). Reached through eframe, egui-winit, accesskit (the Linux screen-reader bridge), which still uses zbus 5. | accesskit moves to zbus 6. Dependabot will raise that upgrade. When `cargo tree -i zvariant` shows only zbus 6 users (or nothing), the three lines go. |
| RUSTSEC-2026-0194, -0195 | `quick-xml` 0.38 | Denial of service. Reached only through typst, hayagriva and citationberg's CSL parser. Quire gives Typst a style name from a fixed list, never a CSL file. | A Typst release that takes a citationberg using `quick-xml` 0.41 or newer. Until then re-read the argument in `docs/design/0.13.1-deps.md` if the PDF export ever accepts a user-supplied style. |
| RUSTSEC-2026-0235 | `rkyv` 0.7 | Out-of-bounds read, reached through `mitex-spec`. The archive that is read is mitex's own build-time default, never user data (Quire passes no spec). | mitex moves to rkyv 0.8. |
| RUSTSEC-2025-0141 | `bincode` 1 | Unmaintained. Via typst and syntect. | Typst or syntect stops using it. |
| RUSTSEC-2025-0057 | `fxhash` | Unmaintained. Via mitex. | mitex stops using it. |
| RUSTSEC-2024-0436 | `paste` | Unmaintained. Via typst, hayagriva and biblatex. | The parent stops using it. |
| RUSTSEC-2026-0206 | `rustybuzz` | Unmaintained. Via typst-layout and krilla. | Typst or krilla moves to its replacement. |
| RUSTSEC-2026-0192 | `ttf-parser` | Unmaintained. Via typst, fontdb and mermaid-rs-renderer. | The parents move off it. |
| RUSTSEC-2024-0449 | `unsafe-libyaml` | Unmaintained. Via typst, hayagriva and serde_yaml; hayagriva YAML bibliographies are not offered. | hayagriva moves off serde_yaml. |
| RUSTSEC-2024-0320 | `yaml-rust` | Unmaintained. Via typst, syntect and two-face; the syntax definitions are embedded at build time, no user YAML. | syntect or two-face moves off it. |

The ten rows after the zbus family come from the Typst and mitex stack and lift the same way: a release of the parent (Typst, mitex or the crate named) drops the dependency, and the Dependabot "rendering" group raises it. Typst 0.15.1 is the newest release `docs/design/0.13.1-deps.md` checked (2026-11-15). Nothing here is a vulnerability Quire reaches with user input: each row's argument is in `deny.toml`.

If a new advisory is a real vulnerability that Quire reaches, do not add it here: fix or replace the crate.

## Dependabot `ignore` (4)

| Crate | Why | Lift it when |
|---|---|---|
| `hayro`, `hayro-interpret` | Typst 0.15 pins hayro 0.7. A newer copy next to Typst's puts two PDF stacks in the full edition (about 3 MiB over its size budget, `packaging/check-size.sh`). | A Typst release moves to hayro 0.8 or later. Remove the ignore, bump `hayro`, `hayro-interpret`, `resvg` and `usvg` together, and run the release dry run: its size check is the test. |
| `resvg`, `usvg` | Typst 0.15 pins resvg and usvg 0.47. Same reason. | A Typst release moves to resvg 0.49 or later (same change as above). |

The 0.13.1 attempt to take these bumps (#57, #58, #60) was rolled back for the size reason above. Dependabot pull requests that bump them are the ones to close until Typst moves.
