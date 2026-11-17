#!/usr/bin/env python3
"""Tests for affected-crates.py on a made-up workspace in a temporary folder.

Run: python3 -I packaging/test-affected-crates.py
"""

import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("affected_crates", os.path.join(HERE, "affected-crates.py"))
ac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ac)

# text <- markdown <- vault <- app <- cli (bin); pdf <- app (dev-dep of cli too);
# typeset (reads a font from the cli's folder), canvas (own testdata, used by app tests)
CRATES = {
    "quire-text": ("crates/quire-text", []),
    "quire-markdown": ("crates/quire-markdown", ["quire-text"]),
    "quire-vault": ("crates/quire-vault", ["quire-markdown"]),
    "quire-pdf": ("crates/quire-pdf", ["quire-vault"]),
    "quire-canvas": ("crates/quire-canvas", []),
    "quire-typeset": ("crates/quire-typeset", []),
    "quire-app": ("crates/quire-app", ["quire-vault", "quire-pdf", "quire-canvas"]),
    "quire-cli": ("apps/quire-cli", ["quire-app"]),
    "quire-desktop": ("apps/quire-desktop", ["quire-app", "quire-typeset"]),
}

FILES = {
    "crates/quire-typeset/src/fonts.rs": 'static F: &[u8] = include_bytes!("../../../apps/quire-desktop/fonts/Inter.ttf");\n',
    "apps/quire-cli/src/agent.rs": 'const S: &str = include_str!("../../../skills/quire/SKILL.md");\n',
    "crates/quire-app/src/pdf_state.rs": 'let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../quire-pdf/tests/fixtures");\n',
    "crates/quire-app/src/own.rs": 'const O: &str = include_str!("../testdata/own.txt");\n',
    "crates/quire-vault/src/lib.rs": "// nothing\n",
    "crates/quire-text/src/lib.rs": '#![doc = include_str!("../README.md")]\n',
}


def make_meta(root):
    pkgs = []
    for name, (folder, deps) in CRATES.items():
        pkgs.append({
            "name": name,
            "manifest_path": f"{root}/{folder}/Cargo.toml",
            "dependencies": [{"name": d, "kind": None} for d in deps],
        })
    # a dev-dependency edge: quire-cli's tests use quire-pdf
    for p in pkgs:
        if p["name"] == "quire-cli":
            p["dependencies"].append({"name": "quire-pdf", "kind": "dev"})
        p["dependencies"].append({"name": "serde", "kind": None})  # not a workspace crate
    return {"workspace_root": root, "packages": pkgs}


class Affected(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = cls.tmp.name
        for path, text in FILES.items():
            full = os.path.join(cls.root, path)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(text)
        cls.meta = make_meta(cls.root)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_affected(self, *changed):
        return ac.affected(self.meta, list(changed))

    def crates(self, *changed):
        return self.run_affected(*changed)[0]

    def test_docs_only_changes_nothing(self):
        for files in (["README.md"], ["docs/howto.md", "docs/design/0.9.md"], [".claude/skills/release/SKILL.md"], ["LICENSE"], []):
            crates, everything, lint, _ = self.run_affected(*files)
            self.assertEqual((crates, everything, lint), ([], False, False), files)

    def test_leaf_crate_changes_only_itself(self):
        self.assertEqual(self.crates("apps/quire-cli/src/main.rs"), ["quire-cli"])

    def test_dependents_follow(self):
        self.assertEqual(
            self.crates("crates/quire-markdown/src/lib.rs"),
            ["quire-app", "quire-cli", "quire-desktop", "quire-markdown", "quire-pdf", "quire-vault"],
        )

    def test_dev_dependency_edges_count(self):
        # quire-cli dev-depends on quire-pdf and also depends on it through quire-app
        self.assertIn("quire-cli", self.crates("crates/quire-pdf/src/lib.rs"))

    def test_tests_dir_counts_for_the_crate_alone(self):
        self.assertEqual(self.crates("crates/quire-markdown/tests/roundtrip.rs"), ["quire-markdown"])
        self.assertEqual(self.crates("crates/quire-markdown/benches/b.rs"), ["quire-markdown"])
        self.assertEqual(self.crates("crates/quire-markdown/examples/e.rs"), ["quire-markdown"])

    def test_src_and_tests_together_follow_the_src_change(self):
        got = self.crates("crates/quire-vault/tests/x.rs", "crates/quire-vault/src/lib.rs")
        self.assertIn("quire-app", got)

    def test_fixture_read_from_another_crate_counts_for_the_reader_alone(self):
        self.assertEqual(
            self.crates("crates/quire-pdf/tests/fixtures/cid.pdf"),
            ["quire-app", "quire-pdf"],
        )

    def test_included_file_counts_for_the_includer_and_its_dependents(self):
        self.assertEqual(self.crates("skills/quire/SKILL.md"), ["quire-cli"])
        got = self.crates("apps/quire-desktop/fonts/Inter.ttf")
        self.assertEqual(got, ["quire-desktop", "quire-typeset"])

    def test_include_of_a_file_in_the_same_crate_is_not_external(self):
        self.assertEqual(self.crates("crates/quire-app/testdata/own.txt"), ["quire-app", "quire-cli", "quire-desktop"])

    def test_markdown_inside_a_crate_is_docs_unless_included_or_a_fixture(self):
        self.assertEqual(self.crates("crates/quire-vault/AGENTS.md"), [])
        self.assertEqual(self.crates("apps/quire-desktop/AGENTS.md", "crates/quire-vault/docs/x.md"), [])
        self.assertIn("quire-text", self.crates("crates/quire-text/README.md"))  # included by lib.rs
        self.assertEqual(self.crates("crates/quire-vault/tests/fixtures/a.md"), ["quire-vault"])
        self.assertEqual(self.crates("crates/quire-canvas/testdata/x.md"), ["quire-app", "quire-canvas", "quire-cli", "quire-desktop"])

    def test_everything_triggers(self):
        for f in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", ".cargo/config.toml", ".github/workflows/ci.yml", ".github/workflows/release.yml"):
            crates, everything, _, _ = self.run_affected(f)
            self.assertTrue(everything, f)
            self.assertEqual(crates, sorted(CRATES), f)

    def test_a_crates_own_cargo_toml_is_not_everything(self):
        crates, everything, _, _ = self.run_affected("crates/quire-text/Cargo.toml")
        self.assertFalse(everything)
        self.assertIn("quire-text", crates)
        self.assertNotIn("quire-canvas", crates)

    def test_packaging_and_lint_config(self):
        crates, everything, lint, packaging = self.run_affected("packaging/check-layering.py")
        self.assertEqual((crates, everything, lint, packaging), ([], False, False, True))
        self.assertTrue(self.run_affected("clippy.toml")[2])
        self.assertTrue(self.run_affected("packaging/perf-check.py")[2])

    def test_unrelated_top_level_files(self):
        for f in ("deny.toml", "_typos.toml", "CHANGELOG.md", "ARCHITECTURE.md", "docs/design/README.md", "sample-vault/x.md"):
            self.assertEqual(self.crates(f), [], f)

    def test_github_output_format(self):
        files = os.path.join(self.root, "changed.txt")
        meta = os.path.join(self.root, "meta.json")
        import json
        with open(files, "w", encoding="utf-8") as f:
            f.write("crates/quire-pdf/src/lib.rs\ndocs/x.md\n")
        with open(meta, "w", encoding="utf-8") as f:
            json.dump(self.meta, f)
        import contextlib
        import io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = ac.main(["--files", files, "--metadata", meta, "--format", "github"])
        self.assertEqual(rc, 0)
        out = dict(line.split("=", 1) for line in buf.getvalue().splitlines())
        self.assertEqual(out["crates"], "quire-app quire-cli quire-desktop quire-pdf")
        self.assertEqual(
            (out["everything"], out["rust"], out["packaging"], out["lite"], out["pdf"], out["eval"]),
            ("false", "true", "false", "true", "true", "true"),
        )
        with open(files, "w", encoding="utf-8") as f:
            f.write("README.md\n")
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            ac.main(["--files", files, "--metadata", meta, "--format", "github"])
        out = dict(line.split("=", 1) for line in buf.getvalue().splitlines())
        self.assertEqual((out["crates"], out["rust"], out["lite"], out["pdf"]), ("", "false", "false", "false"))


if __name__ == "__main__":
    unittest.main()
