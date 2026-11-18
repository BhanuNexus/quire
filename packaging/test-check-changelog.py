#!/usr/bin/env python3
"""Tests for check-changelog.py: the verdict on made-up path lists, and the script
against a throwaway git repository (no network).

Run: python3 -I packaging/test-check-changelog.py
"""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "check-changelog.py")
spec = importlib.util.spec_from_file_location("cc", SCRIPT)
cc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cc)


class Verdict(unittest.TestCase):
    def ok(self, changed, labels=(), actor="someone"):
        return cc.verdict(changed, list(labels), actor)[0]

    def test_source_without_changelog_fails(self):
        self.assertFalse(self.ok(["crates/quire-sync/src/lib.rs"]))
        self.assertFalse(self.ok(["apps/quire-cli/src/main.rs", "docs/howto.md"]))
        self.assertFalse(self.ok(["crates/quire-sync/Cargo.toml"]))

    def test_source_with_changelog_passes(self):
        self.assertTrue(self.ok(["crates/quire-sync/src/lib.rs", "CHANGELOG.md"]))

    def test_label_passes(self):
        self.assertTrue(self.ok(["apps/quire-cli/src/main.rs"], ["no-changelog"]))
        self.assertFalse(self.ok(["apps/quire-cli/src/main.rs"], ["skip-changelog", "bug"]))

    def test_dependabot_passes(self):
        self.assertTrue(self.ok(["crates/quire-sync/Cargo.toml"], actor="dependabot[bot]"))
        self.assertFalse(self.ok(["crates/quire-sync/Cargo.toml"], actor="dependabot-fan"))

    def test_not_source(self):
        for p in [
            "crates/quire-sync/tests/remote.rs",
            "crates/quire-markdown/tests/props.proptest-regressions",
            "apps/quire-desktop/tests/layout/a.rs",
            "crates/quire-vault/AGENTS.md",
            "crates/quire-vault/README.md",
            "crates/quire-import/fixtures/obsidian/note.json",
            "crates/quire-import/src/fixtures/x.txt",
            "crates/quire-import/testdata/a.bin",
            "docs/design/0.14.md",
            ".github/workflows/ci.yml",
            "packaging/check-changelog.py",
            "fuzz/fuzz_targets/a.rs",
            "Cargo.lock",
            "crates",
        ]:
            self.assertFalse(cc.is_source(p), p)

    def test_source(self):
        for p in [
            "crates/quire-sync/src/lib.rs",
            "crates/quire-sync/src/tests.rs",  # a file, not a tests/ folder
            "crates/quire-sync/build.rs",
            "apps/quire-desktop/Cargo.toml",
            "apps/quire-desktop/assets/icon.png",
        ]:
            self.assertTrue(cc.is_source(p), p)

    def test_no_files(self):
        self.assertTrue(self.ok([]))


class Script(unittest.TestCase):
    def git(self, *args):
        subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
             "-c", "commit.gpgsign=false", *args],
            cwd=self.dir, check=True, capture_output=True,
        )

    def write(self, path, text):
        full = os.path.join(self.dir, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)

    def sha(self):
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.dir, check=True,
                              capture_output=True, text=True).stdout.strip()

    def run_check(self, base, head, labels=(), actor=""):
        return subprocess.run(
            [sys.executable, "-I", SCRIPT, "--base", base, "--head", head,
             "--labels", json.dumps(list(labels)), "--actor", actor],
            cwd=self.dir, capture_output=True, text=True,
        ).returncode

    def test_end_to_end(self):
        with tempfile.TemporaryDirectory() as d:
            self.dir = d
            self.git("init", "-q", "-b", "main")
            self.write("CHANGELOG.md", "# Changelog\n")
            self.write("crates/a/src/lib.rs", "// 1\n")
            self.git("add", "-A")
            self.git("commit", "-q", "-m", "base")
            base = self.sha()
            self.write("crates/a/src/lib.rs", "// 2\n")
            self.git("commit", "-q", "-am", "source only")
            src_only = self.sha()
            self.assertEqual(self.run_check(base, src_only), 1)
            self.assertEqual(self.run_check(base, src_only, ["no-changelog"]), 0)
            self.assertEqual(self.run_check(base, src_only, actor="dependabot[bot]"), 0)
            self.write("CHANGELOG.md", "# Changelog\n- x\n")
            self.git("commit", "-q", "-am", "changelog")
            self.assertEqual(self.run_check(base, self.sha()), 0)
            self.write("crates/a/tests/t.rs", "// t\n")
            self.git("add", "-A")
            self.git("commit", "-q", "-m", "test only")
            self.assertEqual(self.run_check(src_only, self.sha()), 0)
            self.assertEqual(self.run_check("0" * 40, self.sha()), 2)

    def test_bad_labels(self):
        r = subprocess.run(
            [sys.executable, "-I", SCRIPT, "--base", "a", "--head", "b", "--labels", "{"],
            capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
