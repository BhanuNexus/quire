// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Format commands and the toolbar's buttons (0.11).
//!
//! One table from command to Markdown-aware edit (`quire_markdown::format`),
//! used for the note's selection here and for every cursor by the desktop's
//! fast editor. Each edit is one undo step.

use quire_markdown::format::{self, Edit, Inline, List};
use quire_markdown::table;
use quire_text::{Buffer, EditKind, Selection};

use crate::commands::Command;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// Whether `cmd` is a formatting edit [`edit`] knows.
pub const fn handles(cmd: Command) -> bool {
    use Command as C;
    matches!(
        cmd,
        C::Bold
            | C::Italic
            | C::InlineCode
            | C::Strikethrough
            | C::Highlight
            | C::Heading1
            | C::Heading2
            | C::Heading3
            | C::Heading4
            | C::Heading5
            | C::Heading6
            | C::Paragraph
            | C::ToggleTask
            | C::Quote
            | C::InsertLink
            | C::InsertMarkdownLink
            | C::BulletList
            | C::NumberedList
            | C::InsertCallout
            | C::InsertTable
            | C::InlineMath
            | C::MathBlock
            | C::CodeBlock
            | C::InsertFootnote
            | C::InsertRule
    )
}

/// The edit `cmd` makes on `sels` (all cursors), or `None` when it has
/// nothing to do there.
pub fn edit(cmd: Command, buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    use Command as C;
    let heading = |level| format::set_heading(buf, sels, level);
    match cmd {
        C::Bold => format::toggle_inline(buf, sels, Inline::Bold),
        C::Italic => format::toggle_inline(buf, sels, Inline::Italic),
        C::InlineCode => format::toggle_inline(buf, sels, Inline::Code),
        C::Strikethrough => format::toggle_inline(buf, sels, Inline::Strike),
        C::Highlight => format::toggle_inline(buf, sels, Inline::Highlight),
        C::InlineMath => format::toggle_inline(buf, sels, Inline::Math),
        C::Heading1 => heading(1),
        C::Heading2 => heading(2),
        C::Heading3 => heading(3),
        C::Heading4 => heading(4),
        C::Heading5 => heading(5),
        C::Heading6 => heading(6),
        C::Paragraph => heading(0),
        C::ToggleTask => format::toggle_task(buf, sels),
        C::Quote => format::toggle_quote(buf, sels),
        C::InsertLink => format::toggle_wikilink(buf, sels),
        C::InsertMarkdownLink => format::toggle_link(buf, sels),
        C::BulletList => format::toggle_list(buf, sels, List::Bullet),
        C::NumberedList => format::toggle_list(buf, sels, List::Numbered),
        C::InsertCallout => format::toggle_callout(buf, sels, "note"),
        C::InsertTable => {
            // In a table already: tidy it (Format table) instead.
            let primary = *sels.last()?;
            if buf.line(buf.char_to_line(primary.head)).trim_start().starts_with('|') {
                let tx = table::format_at(buf, primary)?;
                let after = tx.selection().unwrap_or(primary);
                return Some((tx, vec![after]));
            }
            format::insert_table(buf, sels)
        }
        C::MathBlock => format::toggle_math_block(buf, sels),
        C::CodeBlock => format::toggle_code_block(buf, sels),
        C::InsertFootnote => format::insert_footnote(buf, sels),
        C::InsertRule => format::insert_rule(buf, sels),
        _ => None,
    }
}

/// Run a formatting command on the active note's selection. The desktop
/// runs them on every cursor itself (`FastEditor::format`).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    let doc = ws.active_doc_mut()?;
    let sel = doc.selection();
    match edit(cmd, doc.buffer(), &[sel]) {
        Some((tx, after)) => {
            let head = tx.map_pos(sel.head, quire_text::Assoc::After);
            let primary = after.iter().copied().min_by_key(|s| s.head.abs_diff(head)).unwrap_or(sel);
            let _ = doc.apply(tx.with_selection(primary), EditKind::Other);
        }
        None if cmd == Command::InsertTable => ws.notify(NoticeLevel::Info, "The table is tidy already"),
        None => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;

    #[test]
    fn format_commands_edit_the_note_and_undo_in_one_step() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("A.md", "make this bold")]));
        ws.open_note(&"A.md".into());
        ws.active_doc_mut().unwrap().set_selection(Selection::new(5, 9));
        ws.run(Command::Bold);
        assert_eq!(ws.active_doc().unwrap().text().to_string(), "make **this** bold");
        ws.run(Command::Bold);
        assert_eq!(ws.active_doc().unwrap().text().to_string(), "make this bold", "toggled off");
        ws.run(Command::Undo);
        assert_eq!(ws.active_doc().unwrap().text().to_string(), "make **this** bold");
        ws.run(Command::BulletList);
        assert_eq!(ws.active_doc().unwrap().text().to_string(), "- make **this** bold");
        ws.run(Command::Undo);
        ws.run(Command::Undo);
        assert_eq!(ws.active_doc().unwrap().text().to_string(), "make this bold", "each button press is one step");
        for c in crate::COMMANDS.iter().filter(|c| handles(c.command)) {
            assert_eq!(c.category, "Format", "{} is a Format command", c.title);
        }
    }
}
