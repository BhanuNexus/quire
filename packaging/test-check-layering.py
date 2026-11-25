#!/usr/bin/env python3
"""Tests for check-layering.py, with made-up metadata and tables (no cargo).

Run: python3 -I packaging/test-check-layering.py
"""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("check_layering", os.path.join(HERE, "check-layering.py"))
cl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cl)

ROOT = "/w"


def dep(name, kind=None, features=(), default=True):
    return {"name": name, "kind": kind, "features": list(features), "uses_default_features": default}


def pkg(name, folder, deps=(), features=None):
    return {
        "name": name,
        "manifest_path": f"{ROOT}/{folder}/Cargo.toml",
        "dependencies": list(deps),
        "features": features or {},
    }


def meta(*pkgs):
    return {"workspace_root": ROOT, "packages": list(pkgs)}


def table(rows, same=()):
    """rows: (layer, crate, path, deps)."""
    out = ["| Layer | Crate | Path | What | Depends on |", "|---|---|---|---|---|"]
    for layer, crate, path, deps in rows:
        out.append(f"| {layer} | `{crate}` | `{path}` | something | {', '.join(deps) or 'none'} |")
    out.append("")
    for a, b in same:
        out.append(f"- `{a}` → `{b}`")
    return "\n".join(out)


# A small good world: text(0) <- markdown(1) <- vault(2), sync(2); mcp(3) -> app(3 is a sibling)...
def base_world():
    packages = [
        pkg("quire-text", "crates/quire-text"),
        pkg("quire-markdown", "crates/quire-markdown", [dep("quire-text")]),
        pkg("quire-sync", "crates/quire-sync", [dep("quire-text")], {"default": [], "http": []}),
        pkg("quire-vault", "crates/quire-vault", [dep("quire-markdown")]),
        pkg("quire-app", "crates/quire-app", [dep("quire-vault"), dep("quire-sync", features=["http"])]),
        pkg("quire-mcp", "crates/quire-mcp", [dep("quire-app")]),
        pkg("quire-web", "crates/quire-web", [dep("quire-app"), dep("quire-mcp")]),
        pkg("quire-cli", "apps/quire-cli", [dep("quire-web"), dep("quire-sync", features=["http"])]),
    ]
    rows = [
        (0, "quire-text", "crates/quire-text", []),
        (1, "quire-markdown", "crates/quire-markdown", ["quire-text"]),
        (2, "quire-sync", "crates/quire-sync", ["quire-text"]),
        (2, "quire-vault", "crates/quire-vault", ["quire-markdown"]),
        (3, "quire-app", "crates/quire-app", ["quire-sync", "quire-vault"]),
        (4, "quire-mcp", "crates/quire-mcp", ["quire-app"]),
        (4, "quire-web", "crates/quire-web", ["quire-app", "quire-mcp"]),
        (5, "quire-cli", "apps/quire-cli", ["quire-sync", "quire-web"]),
    ]
    return packages, rows, [("quire-web", "quire-mcp")]


class Layering(unittest.TestCase):
    def run_check(self, packages, rows, same):
        return cl.check(meta(*packages), table(rows, same))

    def assertOneError(self, errors, *needles):
        self.assertEqual(len(errors), 1, errors)
        for n in needles:
            self.assertIn(n, errors[0])

    def test_good_world_passes(self):
        self.assertEqual(self.run_check(*base_world()), [])

    def test_upward_edge_fails(self):
        packages, rows, same = base_world()
        packages[1]["dependencies"].append(dep("quire-vault"))  # markdown -> vault
        rows[1] = (1, "quire-markdown", "crates/quire-markdown", ["quire-text", "quire-vault"])
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-markdown", "quire-vault", "higher layer")

    def test_same_layer_edge_must_be_named(self):
        packages, rows, same = base_world()
        packages[3]["dependencies"].append(dep("quire-sync"))  # vault -> sync, both layer 2
        rows[3] = (2, "quire-vault", "crates/quire-vault", ["quire-markdown", "quire-sync"])
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-vault", "quire-sync", "same layer")

    def test_named_same_layer_edge_passes(self):
        # quire-web -> quire-mcp is named in base_world and passes there.
        self.assertEqual(self.run_check(*base_world()), [])
        packages, rows, _ = base_world()
        errors = self.run_check(packages, rows, [])  # unname it
        self.assertOneError(errors, "quire-web", "quire-mcp")

    def test_missing_row_fails(self):
        packages, rows, same = base_world()
        packages.append(pkg("quire-new", "crates/quire-new"))
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-new", "no row")

    def test_row_without_crate_fails(self):
        packages, rows, same = base_world()
        rows.append((1, "quire-ghost", "crates/quire-ghost", []))
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-ghost", "not a workspace member")

    def test_wrong_path_fails(self):
        packages, rows, same = base_world()
        rows[0] = (0, "quire-text", "crates/elsewhere", [])
        self.assertOneError(self.run_check(packages, rows, same), "quire-text", "crates/elsewhere")

    def test_table_drift_fails_both_ways(self):
        packages, rows, same = base_world()
        rows[1] = (1, "quire-markdown", "crates/quire-markdown", [])  # table forgot an edge
        self.assertOneError(self.run_check(packages, rows, same), "missing from the table: quire-text")
        packages, rows, same = base_world()
        rows[0] = (0, "quire-text", "crates/quire-text", ["quire-markdown"])  # table invents one
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "in the table but not in Cargo.toml: quire-markdown")

    def test_dev_dependencies_are_exempt(self):
        packages, rows, same = base_world()
        packages[0]["dependencies"].append(dep("quire-cli", kind="dev"))  # text dev-depends on the app
        packages[1]["dependencies"].append(dep("quire-sync", kind="dev", features=["http"]))
        self.assertEqual(self.run_check(packages, rows, same), [])

    def test_chrono_only_as_a_dev_dependency(self):
        packages, rows, same = base_world()
        packages[1]["dependencies"].append(dep("chrono", kind="dev"))  # an oracle test
        self.assertEqual(self.run_check(packages, rows, same), [])
        packages[2]["dependencies"].append(dep("chrono"))
        self.assertOneError(self.run_check(packages, rows, same), "quire-sync depends on chrono", "quire_base::dates")

    def test_chrono_tz_is_banned_too(self):
        packages, rows, same = base_world()
        packages[2]["dependencies"].append(dep("chrono-tz"))
        self.assertOneError(self.run_check(packages, rows, same), "quire-sync depends on chrono-tz")

    def test_build_dependencies_count(self):
        packages, rows, same = base_world()
        packages[0]["dependencies"].append(dep("quire-cli", kind="build"))
        rows[0] = (0, "quire-text", "crates/quire-text", ["quire-cli"])
        self.assertOneError(self.run_check(packages, rows, same), "quire-text", "higher layer")

    def test_feature_rules(self):
        packages, rows, same = base_world()
        # a library crate (not quire-app) turns on a feature
        packages[5]["dependencies"].append(dep("quire-sync", features=["http"]))  # mcp -> sync
        rows[5] = (4, "quire-mcp", "crates/quire-mcp", ["quire-app", "quire-sync"])
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-mcp", "feature `http`", "only apps/* and quire-app")

    def test_default_feature_is_not_a_choice(self):
        packages, rows, same = base_world()
        packages[2]["features"] = {"default": ["http"], "http": []}
        packages[4]["dependencies"][1] = dep("quire-sync", features=["http"])
        packages[5]["dependencies"].append(dep("quire-sync", features=["http"]))
        rows[5] = (4, "quire-mcp", "crates/quire-mcp", ["quire-app", "quire-sync"])
        self.assertEqual(self.run_check(packages, rows, same), [])

    def test_feature_allow_list(self):
        packages, rows, same = base_world()
        packages.append(pkg("quire-ai", "crates/quire-ai", [dep("quire-text")], {"client": []}))
        packages.append(pkg("quire-assistant", "crates/quire-assistant", [dep("quire-ai", features=["client"])]))
        rows += [(4, "quire-ai", "crates/quire-ai", ["quire-text"]), (5, "quire-assistant", "crates/quire-assistant", ["quire-ai"])]
        self.assertEqual(self.run_check(packages, rows, same), [])

    def test_base_is_the_bottom_layer(self):
        packages, rows, same = base_world()
        packages.insert(0, pkg("quire-base", "crates/quire-base", features={"default": [], "net": []}))
        rows = [(0, "quire-base", "crates/quire-base", [])] + [(r[0] + 1, r[1], r[2], r[3]) for r in rows]
        packages[2]["dependencies"].append(dep("quire-base"))  # markdown -> base
        rows[2] = (2, "quire-markdown", "crates/quire-markdown", ["quire-base", "quire-text"])
        self.assertEqual(self.run_check(packages, rows, same), [])
        packages[0]["dependencies"].append(dep("quire-markdown"))  # base -> markdown: upward
        rows[0] = (0, "quire-base", "crates/quire-base", ["quire-markdown"])
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "quire-base", "higher layer")

    def test_same_layer_name_across_layers_is_flagged(self):
        packages, rows, same = base_world()
        same = same + [("quire-web", "quire-text")]
        errors = self.run_check(packages, rows, same)
        self.assertOneError(errors, "joins layers")

    def test_parse_table(self):
        rows, same = cl.parse_architecture(table([(1, "quire-a", "crates/quire-a", ["quire-b", "quire-c"])], [("quire-a", "quire-b")]))
        self.assertEqual(rows, {"quire-a": (1, "crates/quire-a", ["quire-b", "quire-c"])})
        self.assertEqual(same, {("quire-a", "quire-b")})

    def test_the_real_table_parses(self):
        with open(os.path.join(HERE, "..", "ARCHITECTURE.md"), encoding="utf-8") as f:
            rows, same = cl.parse_architecture(f.read())
        self.assertGreaterEqual(len(rows), 22)
        self.assertIn(("quire-web", "quire-mcp"), same)


class Trees(unittest.TestCase):
    """The shipped apps' full graphs (`cargo tree -i`), both editions, faked."""

    def fake(self, found):
        """A `cargo tree` that finds `found` = {(package, edition, crate): text}."""
        calls = []

        def run(args):
            calls.append(args)
            pkg = args[args.index("-p") + 1]
            edition = "lite" if "--no-default-features" in args else "full"
            crate = args[args.index("-i") + 1]
            text = found.get((pkg, edition, crate))
            if text is None:
                return 101, "", f"error: package ID specification `{crate}` did not match any packages"
            return 0, text, ""

        return run, calls

    def test_clean_graphs_pass_and_every_edition_is_asked(self):
        run, calls = self.fake({})
        self.assertEqual(cl.tree_violations(["quire-cli", "quire-desktop"], run), [])
        self.assertEqual(len(calls), 2 * 2 * len(cl.BANNED_DEPS))
        self.assertTrue(all("normal,build" in c and "--target" in c and "all" in c for c in calls))

    def test_a_crate_brought_in_by_a_third_party_fails(self):
        run, _ = self.fake({("quire-desktop", "lite", "chrono-tz"): "chrono-tz v0.10.4\n\u2514\u2500\u2500 some-crate v1.0.0\n"})
        errors = cl.tree_violations(["quire-cli", "quire-desktop"], run)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("quire-desktop (--no-default-features)", errors[0])
        self.assertIn("chrono-tz", errors[0])
        self.assertIn("some-crate", errors[0])

    def test_cargo_failing_otherwise_is_an_error(self):
        errors = cl.tree_violations(["quire-cli"], lambda args: (101, "", "error: no lock file"))
        self.assertTrue(errors and all("cargo tree" in e for e in errors), errors)

    def test_the_apps_are_the_shipped_packages(self):
        packages, _, _ = base_world()
        packages.append(pkg("quire-desktop", "apps/quire-desktop"))
        self.assertEqual(cl.shipped(meta(*packages)), ["quire-cli", "quire-desktop"])


if __name__ == "__main__":
    unittest.main()
