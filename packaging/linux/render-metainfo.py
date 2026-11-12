#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Quire contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Print the AppStream file for one version, to stdout.

    python3 -I packaging/linux/render-metainfo.py <version> [--date YYYY-MM-DD]

Fills in the template next to this script (io.github.bhanunexus.Quire.metainfo.xml):

* @VERSION@ is the version;
* @DATE@ is --date, else the date in CHANGELOG.md's "## [<version>] - <date>"
  heading, else SOURCE_DATE_EPOCH, else today (UTC);
* @RELEASE_NOTES@ is the release's <description>: the opening paragraph of
  that CHANGELOG.md section (the one-sentence summary), and a link to the
  full notes. A version without a section (a development build) has the
  link only.

Used by packaging/linux/package.sh (.deb, AppImage) and the Flatpak manifest.
"""

import datetime
import os
import re
import sys
from xml.sax.saxutils import escape

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
TEMPLATE = os.path.join(HERE, "io.github.bhanunexus.Quire.metainfo.xml")
CHANGELOG = os.path.join(ROOT, "CHANGELOG.md")


def changelog_section(version):
    """(date or None, opening paragraph or None) of CHANGELOG.md's section for `version`."""
    try:
        with open(CHANGELOG, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return None, None
    head = re.compile(r"^## \[" + re.escape(version) + r"\](?: - (\d{4}-\d{2}-\d{2}))?\s*$")
    for i, line in enumerate(lines):
        m = head.match(line)
        if not m:
            continue
        paragraph = []
        for body in lines[i + 1 :]:
            if body.startswith("## ") or body.startswith("### "):
                break
            if body.strip():
                paragraph.append(body.strip())
            elif paragraph:
                break
        return m.group(1), " ".join(paragraph) or None
    return None, None


def main(argv):
    args = argv[1:]
    date = None
    if "--date" in args:
        i = args.index("--date")
        date = args[i + 1]
        del args[i : i + 2]
    if len(args) != 1:
        sys.exit("usage: render-metainfo.py <version> [--date YYYY-MM-DD]")
    version = args[0]
    log_date, summary = changelog_section(version)
    if not date:
        date = log_date
    if not date:
        epoch = os.environ.get("SOURCE_DATE_EPOCH")
        when = datetime.datetime.fromtimestamp(int(epoch), datetime.timezone.utc) if epoch else datetime.datetime.now(datetime.timezone.utc)
        date = when.strftime("%Y-%m-%d")
    # AppStream's <p> takes text, not Markdown: drop the code ticks and link syntax.
    if summary:
        summary = summary.replace("`", "")
        summary = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", summary)
    notes = "<description>\n"
    if summary:
        notes += "          <p>" + escape(summary) + "</p>\n"
    else:
        notes += "          <p>Release notes are on the release page.</p>\n"
    notes += "        </description>"
    with open(TEMPLATE, encoding="utf-8") as f:
        text = f.read()
    text = text.replace("@RELEASE_NOTES@", notes).replace("@VERSION@", version).replace("@DATE@", date)
    sys.stdout.write(text)


if __name__ == "__main__":
    main(sys.argv)
