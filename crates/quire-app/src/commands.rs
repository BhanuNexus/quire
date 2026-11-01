// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every user-visible action. Menus, the command palette and keyboard
//! shortcuts are all generated from [`COMMANDS`], so nothing can drift.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names, reason = "`CommandPalette` is the feature's name")]
pub enum Command {
    // Notes
    NewNote,
    NewFromTemplate,
    DailyNote,
    QuickOpen,
    Save,
    CloseTab,
    NextTab,
    PrevTab,
    RenameNote,
    DeleteNote,
    ExportHtml,
    OpenVault,
    Scratchpad,
    ShowHistory,
    NewProject,
    NewInFolder,
    ResolveSyncConflicts,
    MoveNote,
    ImportNotes,
    PublishSite,
    OpenSettings,
    // Search & navigation
    CommandPalette,
    ShowShortcuts,
    TakeTour,
    CheckForUpdates,
    SyncSettings,
    SyncNow,
    SearchVault,
    ShowTasks,
    // Formatting
    Bold,
    Italic,
    InlineCode,
    Strikethrough,
    Highlight,
    Heading1,
    Heading2,
    Heading3,
    Paragraph,
    ToggleTask,
    Quote,
    FormatTable,
    InsertLink,
    FollowLink,
    InsertDate,
    InsertView,
    Undo,
    Redo,
    SwitchUndoBranch,
    SelectAll,
    AddNextOccurrence,
    SelectAllOccurrences,
    AddCursorAbove,
    AddCursorBelow,
    ToggleFold,
    FoldAll,
    UnfoldAll,
    SplitEditor,
    Find,
    FindReplace,
    FindNext,
    FindPrev,
    VaultReplace,
    // View
    TogglePreview,
    ToggleSidebar,
    ToggleInsights,
    FocusMode,
    ToggleTheme,
    ToggleClassicEditor,
    ZoomIn,
    ZoomOut,
    // Intelligence
    Summarize,
    AskVault,
    EvaluateLine,
    LinkAllSuggestions,
}

/// Platform-neutral shortcut. `cmd` means Ctrl on Windows/Linux and ⌘ on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCombo {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    /// Key name understood by the frontend (`"N"`, `"Enter"`, `"F2"`, `"1"`…).
    pub key: &'static str,
}

const fn cmd(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: true, shift: false, alt: false, key }
}
const fn cmd_shift(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: true, shift: true, alt: false, key }
}
const fn alt(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: false, shift: false, alt: true, key }
}
const fn cmd_alt(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: true, shift: false, alt: true, key }
}
const fn shift(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: false, shift: true, alt: false, key }
}
const fn bare(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: false, shift: false, alt: false, key }
}

impl KeyCombo {
    pub fn label(&self, mac: bool) -> String {
        let mut s = String::new();
        if self.cmd {
            s.push_str(if mac { "⌘" } else { "Ctrl+" });
        }
        if self.alt {
            s.push_str(if mac { "⌥" } else { "Alt+" });
        }
        if self.shift {
            s.push_str(if mac { "⇧" } else { "Shift+" });
        }
        s.push_str(match self.key {
            "Backslash" => "\\",
            "OpenBracket" => "[",
            "CloseBracket" => "]",
            "Backtick" => "`",
            "Period" => ".",
            "Equals" => "=",
            "Comma" => ",",
            "Slash" => "/",
            "Minus" => "-",
            k => k,
        });
        s
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommandInfo {
    pub command: Command,
    pub title: &'static str,
    pub category: &'static str,
    pub keys: &'static [KeyCombo],
    /// Requires an open document.
    pub needs_doc: bool,
}

macro_rules! commands {
    ($( $cmd:ident, $cat:literal, $title:literal, [$($k:expr),*], $doc:literal; )*) => {
        pub static COMMANDS: &[CommandInfo] = &[
            $( CommandInfo { command: Command::$cmd, title: $title, category: $cat, keys: &[$($k),*], needs_doc: $doc }, )*
        ];
    };
}

commands! {
    NewNote,          "Note",   "New note",                       [cmd("N")], false;
    NewFromTemplate,  "Note",   "New note from template…",        [cmd_shift("N")], false;
    DailyNote,        "Note",   "Open today's daily note",        [alt("D")], false;
    QuickOpen,        "Note",   "Quick open…",                    [cmd("P"), cmd("O")], false;
    Save,             "Note",   "Save",                           [cmd("S")], true;
    CloseTab,         "Note",   "Close tab",                      [cmd("W")], true;
    NextTab,          "Note",   "Next tab",                       [cmd("Tab")], false;
    PrevTab,          "Note",   "Previous tab",                   [cmd_shift("Tab")], false;
    RenameNote,       "Note",   "Rename note (updates links)…",   [bare("F2")], true;
    DeleteNote,       "Note",   "Move note to trash",             [], true;
    ExportHtml,       "Note",   "Export as HTML",                 [cmd_shift("E")], true;
    OpenVault,        "Note",   "Open vault folder…",             [cmd_shift("O")], false;
    Scratchpad,       "Note",   "Open scratchpad",                [alt("S")], false;
    ShowHistory,      "Note",   "Version history…",               [cmd_alt("H")], true;
    NewProject,       "Note",   "New project…",                   [cmd_alt("N")], false;
    NewInFolder,      "Note",   "New note in folder…",            [], false;
    ResolveSyncConflicts,"Note","Resolve sync conflicts…",        [], false;
    MoveNote,         "Note",   "Move to folder…",                [], true;
    ImportNotes,      "Note",   "Import notes (Logseq, Notion, Bear, Evernote)…", [], false;
    PublishSite,      "Note",   "Publish as a website…",          [], false;
    OpenSettings,     "App",    "Settings…",                      [cmd("Comma")], false;
    SyncSettings,     "App",    "Sync settings…",                 [], false;
    SyncNow,          "App",    "Sync now",                       [], false;
    CommandPalette,   "Go",     "Command palette…",               [cmd_shift("P"), cmd("K")], false;
    ShowShortcuts,    "Help",   "Keyboard shortcuts",             [cmd("Slash")], false;
    TakeTour,         "Help",   "Take the tour",                  [], false;
    CheckForUpdates,  "Help",   "Check for updates",              [], false;
    SearchVault,      "Go",     "Search all notes…",              [cmd_shift("F")], false;
    ShowTasks,        "Go",     "Show open tasks",                [alt("T")], false;
    Bold,             "Format", "Bold",                           [cmd("B")], true;
    Italic,           "Format", "Italic",                         [cmd("I")], true;
    InlineCode,       "Format", "Inline code",                    [cmd("Backtick")], true;
    Strikethrough,    "Format", "Strikethrough",                  [cmd_shift("X")], true;
    Highlight,        "Format", "Highlight",                      [cmd_shift("H")], true;
    Heading1,         "Format", "Heading 1",                      [cmd("1")], true;
    Heading2,         "Format", "Heading 2",                      [cmd("2")], true;
    Heading3,         "Format", "Heading 3",                      [cmd("3")], true;
    Paragraph,        "Format", "Paragraph",                      [cmd("0")], true;
    ToggleTask,       "Format", "Toggle checklist item",          [cmd("L")], true;
    Quote,            "Format", "Toggle quote",                   [cmd_shift("Period")], true;
    FormatTable,      "Format", "Format table",                   [cmd_shift("T")], true;
    InsertLink,       "Format", "Insert [[link]]",                [cmd("Backslash")], true;
    FollowLink,       "Go",     "Follow link under caret",        [alt("Enter")], true;
    InsertDate,       "Format", "Insert today's date",            [cmd_shift("D")], true;
    InsertView,       "Format", "Insert view (table, board, calendar)", [], true;
    Undo,             "Edit",   "Undo",                           [cmd("Z")], true;
    Redo,             "Edit",   "Redo",                           [cmd_shift("Z"), cmd("Y")], true;
    SwitchUndoBranch, "Edit",   "Switch undo branch",             [], true;
    SelectAll,        "Edit",   "Select all",                     [cmd("A")], true;
    AddNextOccurrence,"Edit",   "Add next occurrence to selection",[cmd("D")], true;
    SelectAllOccurrences,"Edit","Select all occurrences",         [cmd_shift("L")], true;
    AddCursorAbove,   "Edit",   "Add cursor above",               [cmd_alt("ArrowUp")], true;
    AddCursorBelow,   "Edit",   "Add cursor below",               [cmd_alt("ArrowDown")], true;
    ToggleFold,       "View",   "Fold or unfold this section",    [cmd_shift("OpenBracket")], true;
    FoldAll,          "View",   "Fold all sections",              [cmd_alt("OpenBracket")], true;
    UnfoldAll,        "View",   "Unfold all sections",            [cmd_alt("CloseBracket")], true;
    SplitEditor,      "View",   "Split editor (two notes side by side)", [cmd_alt("Backslash")], true;
    Find,             "Find",   "Find in note",                   [cmd("F")], true;
    FindReplace,      "Find",   "Find and replace in note",       [cmd("H")], true;
    FindNext,         "Find",   "Find next",                      [bare("F3")], true;
    FindPrev,         "Find",   "Find previous",                  [shift("F3")], true;
    VaultReplace,     "Find",   "Find and replace in all notes…", [cmd_shift("R")], false;
    TogglePreview,    "View",   "Toggle preview",                 [cmd("E")], false;
    ToggleSidebar,    "View",   "Toggle sidebar",                 [cmd_shift("Backslash")], false;
    ToggleInsights,   "View",   "Toggle insights panel",          [cmd_shift("I")], false;
    FocusMode,        "View",   "Focus mode",                     [bare("F11"), cmd_shift("Enter")], false;
    ToggleTheme,      "View",   "Toggle light/dark theme",        [], false;
    ToggleClassicEditor,"View", "Switch between fast and classic editor", [], false;
    ZoomIn,           "View",   "Zoom in",                        [cmd("Equals")], false;
    ZoomOut,          "View",   "Zoom out",                       [cmd("Minus")], false;
    Summarize,        "Assist", "Summarize note",                 [cmd_shift("S")], true;
    AskVault,         "Assist", "Ask your notes…",                [cmd_shift("A")], false;
    EvaluateLine,     "Assist", "Evaluate math on this line",     [cmd("Enter")], true;
    LinkAllSuggestions,"Assist","Link all suggested mentions",    [], true;
}

impl Command {
    pub fn info(self) -> &'static CommandInfo {
        COMMANDS.iter().find(|c| c.command == self).expect("every command is registered")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            for k in c.keys {
                assert!(seen.insert((k.cmd, k.shift, k.alt, k.key)), "duplicate shortcut {k:?} on {}", c.title);
            }
        }
        assert_eq!(Command::Bold.info().keys[0].label(false), "Ctrl+B");
        assert_eq!(Command::Quote.info().keys[0].label(true), "⌘⇧.");
    }
}
