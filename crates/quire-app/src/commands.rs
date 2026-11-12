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
    QuickCapture,
    QuickOpen,
    Save,
    CloseTab,
    NextTab,
    PrevTab,
    RenameNote,
    DeleteNote,
    ExportHtml,
    CopyAsHtml,
    ExportPdf,
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
    ShowGraph,
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
    CopyParagraphLink,
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
    // ---- 0.9. Every variant below dispatches to `features::run`; the
    // workstream that owns it is noted. Handlers live in the owner's module.
    // WS-A: agent store, review, inbox
    OpenInbox,
    NextHunk,
    PrevHunk,
    AcceptHunk,
    AcceptAllInFile,
    RejectHunk,
    RejectAllInFile,
    ShowAgentActivity,
    UndoAgentChanges,
    CreateAgentInstructions,
    LintVault,
    AgentSettings,
    // WS-B: MCP, capture
    CaptureWebPage,
    ConnectAgent,
    // WS-C: trust tools
    CheckCitations,
    ThinkingTimeline,
    MarkReviewed,
    // WS-D: on-device AI, agent panel
    ToggleAgentPanel,
    IngestIntoWiki,
    AiSetup,
    ConnectExternalAgent,
    // 0.10 AI: voice notes
    RecordVoiceNote,
    // WS-E: planning and metadata
    ShowAgenda,
    ShowCalendar,
    OpenWeeklyNote,
    OpenMonthlyNote,
    OpenYearlyNote,
    NextPeriod,
    PrevPeriod,
    StartReview,
    ReviewThisNote,
    AddCardIds,
    RenameTag,
    RenameProperty,
    ConvertTaskToNote,
    ConvertToView,
    // 0.11 tasks: task notes and time tracking
    ConvertNoteToTask,
    NewTask,
    StartStopTimer,
    StartPomodoro,
    StopTimer,
    TimeReport,
    // WS-F: editor and UI
    ToggleVim,
    ChooseTheme,
    GitCommitNow,
    GitPull,
    GitPush,
    ToggleTwoPaneBrowser,
    OpenHomepage,
    // WS-G: canvas, PDF, citations
    NewCanvas,
    InsertSketch,
    InsertCitation,
    ImportBibliography,
    // 0.11 security: locked folders, agent connections
    LockFolder,
    UnlockFolder,
    LockNow,
    ChangeFolderPassphrase,
    StopLockingFolder,
    AgentConnections,
    ShowRecoveryKey,
    // 0.11 editor: the formatting toolbar and command customisation
    // (`format_cmds.rs`, `keymap.rs`).
    Heading4,
    Heading5,
    Heading6,
    BulletList,
    NumberedList,
    InsertMarkdownLink,
    InsertCallout,
    InsertTable,
    InlineMath,
    MathBlock,
    CodeBlock,
    InsertFootnote,
    InsertRule,
    InsertAttachment,
    ToggleToolbar,
    FocusToolbar,
    CustomizeCommands,
    // 0.12 typed links and hierarchy (`links_state.rs`)
    AddTypedLink,
    GoToParent,
    GoToNextSibling,
    GoToPrevSibling,
    ShowHierarchy,
    // 0.12 switching tools (`switch_state.rs`)
    LintNote,
    LintFolder,
    OpenHome,
    PinToHome,
    UndoImport,
    // 0.13 Excalidraw editing (`drawings.rs`, dispatched with the canvas commands)
    NewDrawing,
    EmbedNewDrawing,
}

/// Which workstream's module handles a 0.9 command (design §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    /// A 0.8 command, handled in `Workspace::run`.
    Core,
    Agent,
    Mcp,
    Trust,
    Assistant,
    Plan,
    Editor,
    Spatial,
    /// 0.11 security (`secure_state.rs`).
    Secure,
    /// 0.12 typed links and hierarchy (`links_state.rs`).
    Links,
    /// 0.12 switching tools (`switch_state.rs`).
    Switch,
}

impl Command {
    /// The module that handles this command. 0.8 commands are `Core`.
    pub const fn owner(self) -> Owner {
        match self {
            Self::OpenInbox
            | Self::NextHunk
            | Self::PrevHunk
            | Self::AcceptHunk
            | Self::AcceptAllInFile
            | Self::RejectHunk
            | Self::RejectAllInFile
            | Self::ShowAgentActivity
            | Self::UndoAgentChanges
            | Self::CreateAgentInstructions
            | Self::LintVault
            | Self::AgentSettings => Owner::Agent,
            Self::CaptureWebPage | Self::ConnectAgent => Owner::Mcp,
            Self::CheckCitations | Self::ThinkingTimeline | Self::MarkReviewed => Owner::Trust,
            Self::ToggleAgentPanel | Self::IngestIntoWiki | Self::AiSetup | Self::ConnectExternalAgent | Self::RecordVoiceNote => {
                Owner::Assistant
            }
            Self::ShowAgenda
            | Self::ShowCalendar
            | Self::OpenWeeklyNote
            | Self::OpenMonthlyNote
            | Self::OpenYearlyNote
            | Self::NextPeriod
            | Self::PrevPeriod
            | Self::StartReview
            | Self::ReviewThisNote
            | Self::AddCardIds
            | Self::RenameTag
            | Self::RenameProperty
            | Self::ConvertTaskToNote
            | Self::ConvertToView
            | Self::ConvertNoteToTask
            | Self::NewTask
            | Self::StartStopTimer
            | Self::StartPomodoro
            | Self::StopTimer
            | Self::TimeReport => Owner::Plan,
            Self::ToggleVim
            | Self::ChooseTheme
            | Self::GitCommitNow
            | Self::GitPull
            | Self::GitPush
            | Self::ToggleTwoPaneBrowser
            | Self::OpenHomepage
            // 0.11 editor
            | Self::Heading4
            | Self::Heading5
            | Self::Heading6
            | Self::BulletList
            | Self::NumberedList
            | Self::InsertMarkdownLink
            | Self::InsertCallout
            | Self::InsertTable
            | Self::InlineMath
            | Self::MathBlock
            | Self::CodeBlock
            | Self::InsertFootnote
            | Self::InsertRule
            | Self::InsertAttachment
            | Self::ToggleToolbar
            | Self::FocusToolbar
            | Self::CustomizeCommands => Owner::Editor,
            Self::NewCanvas
            | Self::InsertSketch
            | Self::InsertCitation
            | Self::ImportBibliography
            | Self::NewDrawing
            | Self::EmbedNewDrawing => Owner::Spatial,
            Self::LockFolder
            | Self::UnlockFolder
            | Self::LockNow
            | Self::ChangeFolderPassphrase
            | Self::StopLockingFolder
            | Self::AgentConnections
            | Self::ShowRecoveryKey => Owner::Secure,
            Self::AddTypedLink | Self::GoToParent | Self::GoToNextSibling | Self::GoToPrevSibling | Self::ShowHierarchy => Owner::Links,
            Self::LintNote | Self::LintFolder | Self::OpenHome | Self::PinToHome | Self::UndoImport => Owner::Switch,
            _ => Owner::Core,
        }
    }
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
const fn cmd_alt_shift(key: &'static str) -> KeyCombo {
    KeyCombo { cmd: true, shift: true, alt: true, key }
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
            "Backspace" => "Backspace",
            "ArrowLeft" => "←",
            "ArrowUp" => "↑",
            "ArrowDown" => "↓",
            "ArrowRight" => "→",
            k => k,
        });
        s
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommandInfo {
    pub command: Command,
    /// Stable name for settings (`"Bold"`): the variant's name.
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub keys: &'static [KeyCombo],
    /// Requires an open document.
    pub needs_doc: bool,
}

macro_rules! commands {
    ($( $cmd:ident, $cat:literal, $title:literal, [$($k:expr),*], $doc:literal; )*) => {
        pub static COMMANDS: &[CommandInfo] = &[
            $( CommandInfo { command: Command::$cmd, id: stringify!($cmd), title: $title, category: $cat, keys: &[$($k),*], needs_doc: $doc }, )*
        ];
    };
}

commands! {
    NewNote,          "Note",   "New note",                       [cmd("N")], false;
    NewFromTemplate,  "Note",   "New note from template…",        [cmd_shift("N")], false;
    DailyNote,        "Note",   "Open today's daily note",        [alt("D")], false;
    QuickCapture,     "Note",   "Quick capture to today's note…", [], false;
    QuickOpen,        "Note",   "Quick open…",                    [cmd("P"), cmd("O")], false;
    Save,             "Note",   "Save",                           [cmd("S")], true;
    CloseTab,         "Note",   "Close tab",                      [cmd("W")], true;
    NextTab,          "Note",   "Next tab",                       [cmd("Tab")], false;
    PrevTab,          "Note",   "Previous tab",                   [cmd_shift("Tab")], false;
    RenameNote,       "Note",   "Rename note (updates links)…",   [bare("F2")], true;
    DeleteNote,       "Note",   "Move note to trash",             [], true;
    ExportHtml,       "Note",   "Export as HTML",                 [cmd_shift("E")], true;
    CopyAsHtml,       "Note",   "Copy as HTML",                   [], true;
    ExportPdf,        "Note",   "Export as PDF",                  [cmd_alt("E")], true;
    OpenVault,        "Note",   "Open vault folder…",             [cmd_shift("O")], false;
    Scratchpad,       "Note",   "Open scratchpad",                [alt("S")], false;
    ShowHistory,      "Note",   "Version history…",               [cmd_alt("H")], true;
    NewProject,       "Note",   "New project…",                   [cmd_alt("N")], false;
    NewInFolder,      "Note",   "New note in folder…",            [], false;
    ResolveSyncConflicts,"Note","Resolve sync conflicts…",        [], false;
    MoveNote,         "Note",   "Move to folder…",                [], true;
    ImportNotes,      "Note",   "Import notes…", [], false;
    PublishSite,      "Note",   "Publish as a website…",          [], false;
    OpenSettings,     "App",    "Settings…",                      [cmd("Comma")], false;
    SyncSettings,     "App",    "Sync settings…",                 [], false;
    SyncNow,          "App",    "Sync now",                       [], false;
    CommandPalette,   "Go",     "Command palette…",               [cmd_shift("P"), cmd("K")], false;
    ShowShortcuts,    "Help",   "Keyboard shortcuts",             [cmd("Slash")], false;
    TakeTour,         "Help",   "Take the tour",                  [], false;
    CheckForUpdates,  "Help",   "Check for updates",              [], false;
    SearchVault,      "Go",     "Search all notes…",              [cmd_shift("F")], false;
    ShowGraph,        "Go",     "Graph view",                     [cmd("G")], false;
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
    CopyParagraphLink,"Note",   "Copy link to this paragraph",    [cmd_alt("L")], true;
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
    // ---- 0.9: keys are final (design §7.3; changes recorded in §6.1).
    // Ctrl+Alt is AltGr on Windows: the desktop drops a Ctrl+Alt shortcut in
    // a frame where text was typed, so AltGr+C still types ć on a Polish
    // layout instead of making a canvas.
    OpenInbox,        "Agent",  "Inbox",                          [alt("I")], false;
    NextHunk,         "Review", "Next suggested change",          [alt("CloseBracket")], true;
    PrevHunk,         "Review", "Previous suggested change",      [alt("OpenBracket")], true;
    AcceptHunk,       "Review", "Accept suggested change",        [cmd_alt("Y")], true;
    AcceptAllInFile,  "Review", "Accept all suggested changes in this note", [cmd_alt_shift("Y")], true;
    RejectHunk,       "Review", "Reject suggested change",        [cmd_alt("Backspace")], true;
    RejectAllInFile,  "Review", "Reject all suggested changes in this note", [], true;
    ShowAgentActivity,"Agent",  "Agent activity",                 [], false;
    UndoAgentChanges, "Agent",  "Undo an agent's changes…",       [], false;
    CreateAgentInstructions,"Agent","Create agent instructions (AGENTS.md)", [], false;
    LintVault,        "Agent",  "Check the vault (lint)",         [], false;
    AgentSettings,    "Agent",  "Agent settings…",                [], false;
    CaptureWebPage,   "Note",   "Capture web page…",              [], false;
    ConnectAgent,     "Agent",  "Connect an AI agent (MCP)…",     [], false;
    CheckCitations,   "Assist", "Check citations in this note",   [], true;
    ThinkingTimeline, "Assist", "Thinking timeline…",             [], false;
    MarkReviewed,     "Note",   "Mark note as reviewed",          [], true;
    ToggleAgentPanel, "Agent",  "Agent panel",                    [cmd_shift("J")], false;
    IngestIntoWiki,   "Agent",  "Ingest into wiki…",              [], false;
    AiSetup,          "App",    "Set up AI…",                     [], false;
    ConnectExternalAgent,"Agent","Connect an external agent (ACP)…", [], false;
    RecordVoiceNote,  "Note",   "Record voice note…",             [], false;
    ShowAgenda,       "Go",     "Agenda",                         [alt("A")], false;
    ShowCalendar,     "Go",     "Calendar",                       [], false;
    OpenWeeklyNote,   "Note",   "Open this week's note",          [], false;
    OpenMonthlyNote,  "Note",   "Open this month's note",         [], false;
    OpenYearlyNote,   "Note",   "Open this year's note",          [], false;
    NextPeriod,       "Go",     "Next daily/weekly/monthly note", [], true;
    PrevPeriod,       "Go",     "Previous daily/weekly/monthly note", [], true;
    StartReview,      "Study",  "Review flashcards",              [cmd_alt("R")], false;
    ReviewThisNote,   "Study",  "Review this note's flashcards",  [], true;
    AddCardIds,       "Study",  "Add ids to this note's flashcards", [], true;
    RenameTag,        "Note",   "Rename tag…",                    [], false;
    RenameProperty,   "Note",   "Rename property…",               [], false;
    ConvertTaskToNote,"Format", "Convert task to note",           [], true;
    ConvertToView,    "Format", "Convert to quire-view",          [], true;
    ConvertNoteToTask,"Format", "Convert task note back to a task line", [], true;
    NewTask,          "Note",   "New task…",                      [cmd_shift("K")], false;
    StartStopTimer,   "Go",     "Start or stop the timer on this task", [cmd_shift("M")], false;
    StartPomodoro,    "Go",     "Start a pomodoro on this task",  [], false;
    StopTimer,        "Go",     "Stop the task timer",            [], false;
    TimeReport,       "Go",     "Time this week",                 [], false;
    ToggleVim,        "Edit",   "Vim mode",                       [], false;
    ChooseTheme,      "View",   "Theme…",                         [], false;
    GitCommitNow,     "App",    "Commit now (git)",               [], false;
    GitPull,          "App",    "Pull (git)",                     [], false;
    GitPush,          "App",    "Push (git)",                     [], false;
    ToggleTwoPaneBrowser,"View","Folders and notes side by side", [], false;
    OpenHomepage,     "Go",     "Open homepage note",             [], false;
    NewCanvas,        "Note",   "New canvas",                     [cmd_alt("C")], false;
    InsertSketch,     "Format", "Insert sketch",                  [], true;
    InsertCitation,   "Format", "Insert citation…",               [], true;
    ImportBibliography,"Note",  "Import bibliography…",           [], false;
    // ---- 0.11 security
    LockFolder,       "Security","Lock a folder (encrypt it)…",   [], false;
    UnlockFolder,     "Security","Unlock a locked folder…",       [], false;
    LockNow,          "Security","Lock locked folders now",       [cmd_shift("B")], false;
    ChangeFolderPassphrase,"Security","Change a locked folder's passphrase…", [], false;
    StopLockingFolder,"Security","Stop locking a folder (decrypt it)…", [], false;
    AgentConnections, "Agent",  "Agent connections…",             [], false;
    ShowRecoveryKey,  "Security","Show a locked folder's recovery key…", [], false;
    // ---- 0.11 editor: new commands have no default keys (Settings → Commands binds them).
    Heading4,         "Format", "Heading 4",                      [], true;
    Heading5,         "Format", "Heading 5",                      [], true;
    Heading6,         "Format", "Heading 6",                      [], true;
    BulletList,       "Format", "Toggle bulleted list",           [], true;
    NumberedList,     "Format", "Toggle numbered list",           [], true;
    InsertMarkdownLink,"Format","Insert link [text](url)",        [], true;
    InsertCallout,    "Format", "Insert callout",                 [], true;
    InsertTable,      "Format", "Insert table",                   [], true;
    InlineMath,       "Format", "Toggle inline math",             [], true;
    MathBlock,        "Format", "Toggle math block",              [], true;
    CodeBlock,        "Format", "Toggle code block",              [], true;
    InsertFootnote,   "Format", "Insert footnote",                [], true;
    InsertRule,       "Format", "Insert horizontal rule",         [], true;
    InsertAttachment, "Format", "Insert image or attachment…",    [], true;
    ToggleToolbar,    "View",   "Formatting toolbar",             [], false;
    FocusToolbar,     "View",   "Move focus to the formatting toolbar", [bare("F10")], true;
    CustomizeCommands,"App",    "Customize commands and shortcuts…", [], false;
    // ---- 0.12 typed links: no default keys (Settings → Commands binds them).
    AddTypedLink,     "Note",   "Add typed link…",                [], true;
    GoToParent,       "Go",     "Go to parent",                   [], false;
    GoToNextSibling,  "Go",     "Go to next sibling",             [], false;
    GoToPrevSibling,  "Go",     "Go to previous sibling",         [], false;
    ShowHierarchy,    "View",   "Show hierarchy",                 [], false;
    // ---- 0.12 switching tools: no default keys (Settings → Commands binds them).
    LintNote,         "Note",   "Tidy this note (lint)",          [], true;
    LintFolder,       "Note",   "Tidy notes in a folder…",        [], false;
    OpenHome,         "Go",     "Home",                           [], false;
    PinToHome,        "Note",   "Pin to Home (or unpin)",         [], true;
    UndoImport,       "Note",   "Undo the last import",           [], false;
    // ---- 0.13 Excalidraw: no default keys (Settings → Commands binds them).
    NewDrawing,       "Note",   "New drawing (Excalidraw)",       [], false;
    EmbedNewDrawing,  "Format", "Embed a new drawing in this note", [], true;
}

impl Command {
    pub fn info(self) -> &'static CommandInfo {
        COMMANDS.iter().find(|c| c.command == self).expect("every command is registered")
    }

    /// The stable name settings use for this command (`"Bold"`).
    pub fn id(self) -> &'static str {
        self.info().id
    }

    /// The command named `id` in settings, if this version has it.
    pub fn from_id(id: &str) -> Option<Self> {
        COMMANDS.iter().find(|c| c.id == id).map(|c| c.command)
    }
}

/// Key names a shortcut can use, as the frontend names them, with other
/// spellings people type (`"="`, `"Up"`).
const KEY_NAMES: &[(&str, &[&str])] = &[
    ("ArrowUp", &["Up", "⏶"]),
    ("ArrowDown", &["Down", "⏷"]),
    ("ArrowLeft", &["Left", "←", "⏴"]),
    ("ArrowRight", &["Right", "→", "⏵"]),
    ("Escape", &["Esc"]),
    ("Tab", &[]),
    ("Backspace", &[]),
    ("Enter", &["Return"]),
    ("Space", &[" "]),
    ("Insert", &[]),
    ("Delete", &["Del"]),
    ("Home", &[]),
    ("End", &[]),
    ("PageUp", &[]),
    ("PageDown", &[]),
    ("Comma", &[","]),
    ("Minus", &["-"]),
    ("Period", &["."]),
    ("Plus", &["+"]),
    ("Equals", &["=", "Equal"]),
    ("Semicolon", &[";"]),
    ("Colon", &[":"]),
    ("Backslash", &["\\"]),
    ("Slash", &["/"]),
    ("OpenBracket", &["[", "BracketLeft"]),
    ("CloseBracket", &["]", "BracketRight"]),
    ("Backtick", &["`", "Grave", "Backquote"]),
    ("Quote", &["'"]),
    ("A", &[]),
    ("B", &[]),
    ("C", &[]),
    ("D", &[]),
    ("E", &[]),
    ("F", &[]),
    ("G", &[]),
    ("H", &[]),
    ("I", &[]),
    ("J", &[]),
    ("K", &[]),
    ("L", &[]),
    ("M", &[]),
    ("N", &[]),
    ("O", &[]),
    ("P", &[]),
    ("Q", &[]),
    ("R", &[]),
    ("S", &[]),
    ("T", &[]),
    ("U", &[]),
    ("V", &[]),
    ("W", &[]),
    ("X", &[]),
    ("Y", &[]),
    ("Z", &[]),
    ("0", &[]),
    ("1", &[]),
    ("2", &[]),
    ("3", &[]),
    ("4", &[]),
    ("5", &[]),
    ("6", &[]),
    ("7", &[]),
    ("8", &[]),
    ("9", &[]),
    ("F1", &[]),
    ("F2", &[]),
    ("F3", &[]),
    ("F4", &[]),
    ("F5", &[]),
    ("F6", &[]),
    ("F7", &[]),
    ("F8", &[]),
    ("F9", &[]),
    ("F10", &[]),
    ("F11", &[]),
    ("F12", &[]),
    ("F13", &[]),
    ("F14", &[]),
    ("F15", &[]),
    ("F16", &[]),
    ("F17", &[]),
    ("F18", &[]),
    ("F19", &[]),
    ("F20", &[]),
];

impl KeyCombo {
    /// A key name in the spelling shortcuts use, or `None` for a key that
    /// can't be part of one.
    pub fn key_name(name: &str) -> Option<&'static str> {
        KEY_NAMES
            .iter()
            .find(|(k, alts)| k.eq_ignore_ascii_case(name) || alts.iter().any(|a| a.eq_ignore_ascii_case(name)))
            .map(|(k, _)| *k)
    }

    /// Read a shortcut as settings write it: `"Ctrl+Shift+B"`. `Ctrl`,
    /// `Cmd`, `⌘` and `Mod` all mean Ctrl (⌘ on macOS); `Alt` and `Option`
    /// are the same key.
    pub fn parse(spec: &str) -> Option<Self> {
        let mut k = Self { cmd: false, shift: false, alt: false, key: "" };
        let spec = spec.trim();
        // "Ctrl++" is Ctrl and the plus key.
        let (mods, key) = match spec.strip_suffix("++") {
            Some(m) => (m, "+"),
            None => spec.rsplit_once('+').unwrap_or(("", spec)),
        };
        for m in mods.split('+').map(str::trim).filter(|m| !m.is_empty()) {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" | "mod" | "⌘" | "super" => k.cmd = true,
                "shift" | "⇧" => k.shift = true,
                "alt" | "option" | "opt" | "⌥" => k.alt = true,
                _ => return None,
            }
        }
        k.key = Self::key_name(key.trim())?;
        Some(k)
    }

    /// The shortcut as settings store it (`parse` reads it back).
    pub fn spec(&self) -> String {
        let mut s = String::new();
        if self.cmd {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(self.key);
        s
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

    #[test]
    fn the_0_9_keys_are_the_documented_ones() {
        let label = |c: Command| c.info().keys.first().map(|k| k.label(false)).unwrap_or_default();
        for (c, keys) in [
            (Command::NextHunk, "Alt+]"),
            (Command::PrevHunk, "Alt+["),
            (Command::AcceptHunk, "Ctrl+Alt+Y"),
            (Command::AcceptAllInFile, "Ctrl+Alt+Shift+Y"),
            (Command::RejectHunk, "Ctrl+Alt+Backspace"),
            (Command::ToggleAgentPanel, "Ctrl+Shift+J"),
            (Command::NewCanvas, "Ctrl+Alt+C"),
            (Command::StartReview, "Ctrl+Alt+R"),
            // Ctrl+Shift+I (insights) and Ctrl+Shift+D (insert date) were taken in 0.8.
            (Command::OpenInbox, "Alt+I"),
            (Command::ShowAgenda, "Alt+A"),
        ] {
            assert_eq!(label(c), keys, "{c:?}");
        }
        // 0.8 bindings are unchanged.
        assert_eq!(label(Command::ToggleInsights), "Ctrl+Shift+I");
        assert_eq!(label(Command::InsertDate), "Ctrl+Shift+D");
    }

    #[test]
    fn commands_have_stable_ids_and_shortcuts_read_back() {
        assert_eq!(Command::Bold.id(), "Bold");
        assert_eq!(Command::from_id("InsertFootnote"), Some(Command::InsertFootnote));
        assert_eq!(Command::from_id("NoSuchCommand"), None);
        for c in COMMANDS {
            assert_eq!(Command::from_id(c.id), Some(c.command));
            for k in c.keys {
                assert_eq!(KeyCombo::parse(&k.spec()), Some(*k), "{}", k.spec());
            }
        }
        assert_eq!(KeyCombo::parse("cmd+shift+="), Some(KeyCombo { cmd: true, shift: true, alt: false, key: "Equals" }));
        assert_eq!(KeyCombo::parse("Ctrl++").map(|k| k.key), Some("Plus"));
        assert_eq!(KeyCombo::parse("Option+Up").map(|k| k.label(true)), Some("⌥↑".into()));
        assert_eq!(KeyCombo::parse("Hyper+B"), None);
        assert_eq!(KeyCombo::parse("Ctrl+NotAKey"), None);
    }

    #[test]
    fn every_command_is_registered_once_with_an_owner() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.command), "{:?} registered twice", c.command);
            assert!(!c.title.is_empty());
        }
        assert_eq!(Command::OpenInbox.owner(), Owner::Agent);
        assert_eq!(Command::Bold.owner(), Owner::Core);
        assert!(COMMANDS.iter().filter(|c| c.command.owner() != Owner::Core).count() >= 40);
    }
}
