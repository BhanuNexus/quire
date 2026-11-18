#!/usr/bin/env python3
"""Tests for ratchet.py, with made-up clippy output (no cargo).

Run: python3 -I packaging/test-ratchet.py
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ratchet", os.path.join(HERE, "ratchet.py"))
rt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rt)


def artifact():
    return json.dumps({"reason": "compiler-artifact", "target": {"name": "x"}})


def hit(path, file, line=1, col=1, code="clippy::disallowed_methods", text=None):
    msg = text or f"use of a disallowed method `{path}`"
    return json.dumps({
        "reason": "compiler-message",
        "message": {
            "message": msg,
            "code": {"code": code},
            "spans": [{"file_name": file, "line_start": line, "column_start": col, "is_primary": True}],
        },
    })


def run(lines, baseline):
    """(exit status, stdout, summary text) of ratchet.main on these JSON lines and baseline."""
    with tempfile.TemporaryDirectory() as d:
        j, b, s = (os.path.join(d, n) for n in ("c.json", "r.toml", "s.md"))
        with open(j, "w") as f:
            f.write("\n".join(lines))
        with open(b, "w") as f:
            f.write("[baseline]\n" + "".join(f"{k} = {v}\n" for k, v in baseline.items()))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = rt.main([j, "--baseline", b, "--summary", s])
        return status, out.getvalue(), open(s).read()


BASE = {"unwrap_or_default": 2, "wall_clock": 1, "env_var": 1}
UOD = "core::result::Result::unwrap_or_default"


class Ratchet(unittest.TestCase):
    def test_at_the_baseline_passes(self):
        lines = [artifact(), hit(UOD, "crates/a/src/x.rs", 1), hit(UOD, "crates/b/src/y.rs", 2),
                 hit("chrono::Utc::now", "crates/a/src/x.rs", 9), hit("std::env::var", "apps/c/src/main.rs")]
        status, out, summary = run(lines, BASE)
        self.assertEqual(status, 0, out)
        self.assertIn("at the baseline", out)
        self.assertIn("## Ratchet", summary)
        self.assertIn("`unwrap_or_default`: crates/a 1, crates/b 1", out)

    def test_above_the_baseline_fails(self):
        lines = [artifact(), hit("std::env::var", "crates/a/src/x.rs", 1), hit("std::env::var", "crates/a/src/x.rs", 2)]
        status, out, _ = run(lines, BASE)
        self.assertEqual(status, 1)
        self.assertIn("**1 more than the baseline**", out)

    def test_all_three_clock_calls_count_together(self):
        paths = ["std::time::SystemTime::now", "chrono::Utc::now", "chrono::Local::now"]
        lines = [artifact()] + [hit(p, "crates/a/src/x.rs", i + 1) for i, p in enumerate(paths)]
        status, out, _ = run(lines, {**BASE, "wall_clock": 3, "unwrap_or_default": 0, "env_var": 0})
        self.assertEqual(status, 0, out)

    def test_below_the_baseline_passes_and_says_to_lower_it(self):
        status, out, _ = run([artifact()], {"unwrap_or_default": 0, "wall_clock": 0, "env_var": 1})
        self.assertEqual(status, 0)
        self.assertIn("1 fewer: lower the baseline", out)
        self.assertIn("Lower `env_var` to 0", out)

    def test_the_same_line_seen_twice_counts_once(self):
        lines = [artifact(), hit(UOD, "crates/a/src/x.rs", 5, 7), hit(UOD, "crates/a/src/x.rs", 5, 7)]
        status, out, _ = run(lines, {"unwrap_or_default": 1, "wall_clock": 0, "env_var": 0})
        self.assertEqual(status, 0, out)

    def test_other_clippy_messages_do_not_count(self):
        lines = [artifact(), hit(UOD, "crates/a/src/x.rs", code="clippy::unwrap_used"),
                 hit(UOD, "crates/a/src/x.rs", 3, text="`chrono::Utc::now` does not refer to an existing function"),
                 "not json", "", json.dumps({"reason": "build-finished", "success": True})]
        status, out, _ = run(lines, {"unwrap_or_default": 0, "wall_clock": 0, "env_var": 0})
        self.assertEqual(status, 0, out)

    def test_unusable_input_exits_2(self):
        for lines in ([], ["warning: not json output"]):
            with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()):
                run(lines, BASE)
            self.assertEqual(e.exception.code, 2)
        with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()):
            run([artifact(), hit("std::fs::write", "crates/a/src/x.rs")], BASE)
        self.assertEqual(e.exception.code, 2)

    def test_a_baseline_with_a_missing_or_unknown_rule_exits_2(self):
        for bad in ({"unwrap_or_default": 1}, {**BASE, "extra": 1}):
            with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()):
                run([artifact()], bad)
            self.assertEqual(e.exception.code, 2)

    def test_the_committed_files_agree(self):
        """ratchet/clippy.toml lists exactly the paths ratchet.py owns, and ratchet.toml names every rule."""
        import tomllib
        with open(os.path.join(HERE, "ratchet", "clippy.toml"), "rb") as f:
            paths = {m["path"] for m in tomllib.load(f)["disallowed-methods"]}
        self.assertEqual(paths, set(rt.RULE_OF))
        self.assertEqual(set(rt.load_baseline(os.path.join(HERE, "ratchet.toml"))), set(rt.RULES))


if __name__ == "__main__":
    unittest.main()
