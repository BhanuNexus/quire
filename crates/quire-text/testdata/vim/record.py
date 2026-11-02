#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Record what real Vim does for each case in cases.in, into cases.tsv.

Each case is a text (with `|` where the cursor starts) and keys in Vim
notation. Vim runs in a pseudo-terminal with the options below (the ones
Quire's Vim mode follows), types the keys, and dumps the buffer and cursor.
The Rust test table (src/vim/tests.rs) checks the state machine against the
recorded results; it needs no Vim. Re-record after adding cases:

    python3 crates/quire-text/testdata/vim/record.py

Lines in cases.in: `text<TAB>keys`, with `\\n` for line breaks. Lines starting
with `#` are comments. A third column overrides Vim's result where Quire
deliberately differs (say why in a comment above it).
"""

import json
import os
import pty
import select
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
VIMRC = r"""
set nocompatible
set autoindent nojoinspaces shiftwidth=2 softtabstop=2 expandtab
set whichwrap= belloff=all noshowmode nomore shortmess+=I
set undolevels=1000 ttimeout ttimeoutlen=5
function! Dump(out) range
  call writefile([json_encode({'lines': getline(1, '$'), 'cur': getcursorcharpos()})], a:out)
  qa!
endfunction
"""

NAMES = {"Esc": "\x1b", "CR": "\r", "BS": "\x08", "Tab": "\t", "lt": "<", "Del": "\x1b[3~",
         "Left": "\x1bOD", "Right": "\x1bOC", "Up": "\x1bOA", "Down": "\x1bOB"}


def keys_to_bytes(keys):
    out, i = "", 0
    while i < len(keys):
        if keys[i] == "<":
            j = keys.find(">", i)
            name = keys[i + 1:j] if j > i + 1 else ""
            if name in NAMES:
                out += NAMES[name]
                i = j + 1
                continue
            if len(name) == 3 and name.startswith("C-"):
                c = name[2].lower()
                out += "\x1b" if c == "[" else chr(ord(c) - 96)
                i = j + 1
                continue
        out += keys[i]
        i += 1
    return out


def split_keys(keys):
    """Keys one by one: `<Esc>` and friends stay whole."""
    out, i = [], 0
    while i < len(keys):
        j = keys.find(">", i) if keys[i] == "<" else -1
        if j > i + 1 and (keys[i + 1:j] in NAMES or keys[i + 1:j].startswith("C-")):
            out.append(keys[i:j + 1])
            i = j + 1
        else:
            out.append(keys[i])
            i += 1
    return out


def drain(fd):
    while select.select([fd], [], [], 0)[0]:
        try:
            os.read(fd, 65536)
        except OSError:
            return


def unescape(s):
    return s.replace("\\\\", "\0").replace("\\n", "\n").replace("\\t", "\t").replace("\0", "\\")


def escape(s):
    return s.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t")


def run(text_with_cursor, keys, tmp):
    cur = text_with_cursor.index("|")
    text = text_with_cursor[:cur] + text_with_cursor[cur + 1:]
    line = text[:cur].count("\n") + 1
    col = cur - (text[:cur].rfind("\n") + 1) + 1
    src, out, rc = (os.path.join(tmp, n) for n in ("in.txt", "out.json", "vimrc"))
    with open(src, "w") as f:
        f.write(text + "\n")
    with open(rc, "w") as f:
        f.write(VIMRC)
    if os.path.exists(out):
        os.remove(out)
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("vim", ["vim", "-u", rc, "-i", "NONE", "-n",
                          "+call setcursorcharpos(%d, %d)" % (line, col), src])
    # Type the keys one at a time, as a person would: Vim splits undo steps
    # between typed commands (keys from `-s` would all be one step).
    time.sleep(0.15)
    for k in split_keys(keys) + ["<Esc>"] + [f":call Dump('{out}')\r"]:
        drain(fd)
        os.write(fd, keys_to_bytes(k).encode())
        time.sleep(0.02)
    deadline = time.time() + 5
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                os.read(fd, 65536)
            except OSError:
                break
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
    else:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
        raise RuntimeError(f"vim hung on {text_with_cursor!r} {keys!r}")
    with open(out) as f:
        d = json.load(f)
    lines, (_, lnum, ccol, _, _) = d["lines"], d["cur"]
    res = "\n".join(lines)
    pos = sum(len(l) + 1 for l in lines[:lnum - 1]) + ccol - 1
    return res[:pos] + "|" + res[pos:]


def main():
    cases = []
    with open(os.path.join(HERE, "cases.in")) as f:
        for raw in f:
            raw = raw.rstrip("\n")
            if not raw or raw.startswith("#"):
                continue
            cases.append(raw.split("\t"))
    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        for c in cases:
            text, keys = unescape(c[0]), c[1]
            got = unescape(c[2]) if len(c) > 2 else run(text, keys, tmp)
            rows.append(f"{escape(text)}\t{keys}\t{escape(got)}")
    with open(os.path.join(HERE, "cases.tsv"), "w") as f:
        f.write("# Recorded from Vim by record.py: text, keys, result (| is the cursor).\n")
        f.write("\n".join(rows) + "\n")
    print(f"{len(rows)} cases", file=sys.stderr)


if __name__ == "__main__":
    main()
