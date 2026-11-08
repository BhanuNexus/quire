// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task metadata written after a task's text.
//!
//! Dates, recurrence, priority and reminders, in the Obsidian Tasks emoji
//! format (`📅 2026-12-01`) or as Dataview fields (`[due:: 2026-12-01]`),
//! plus Day Planner time blocks at the start (`14:00 - 15:30 Write report`).
//! See design §E.2 and docs/formats/tasks.md.
//!
//! Fields are read from the end of the line, the way Obsidian Tasks reads
//! them: everything before the first field is the description. Fields Quire
//! doesn't use (`🆔`, `⛔`, `🏁`, `❌`, other `[key:: value]`) are skipped over
//! and kept as they are when a line is rewritten.
//!
//! [`extract`](crate::extract()) calls [`parse_meta`] for every task. Dates
//! stay strings here; `quire-plan` parses them.

use std::ops::Range;

use crate::Task;

/// A field Quire reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Due,
    Scheduled,
    Start,
    Done,
    Created,
    Recurrence,
    Priority,
    Reminder,
}

impl Field {
    pub const ALL: [Self; 8] =
        [Self::Priority, Self::Recurrence, Self::Created, Self::Start, Self::Scheduled, Self::Due, Self::Reminder, Self::Done];

    /// The emoji written for the field (priority has one per level).
    pub const fn emoji(self) -> &'static str {
        match self {
            Self::Due => "📅",
            Self::Scheduled => "⏳",
            Self::Start => "🛫",
            Self::Done => "✅",
            Self::Created => "➕",
            Self::Recurrence => "🔁",
            Self::Priority => "⏫",
            Self::Reminder => "⏰",
        }
    }

    /// The Dataview key written for the field.
    pub const fn dataview_key(self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::Scheduled => "scheduled",
            Self::Start => "start",
            Self::Done => "completion",
            Self::Created => "created",
            Self::Recurrence => "repeat",
            Self::Priority => "priority",
            Self::Reminder => "remind",
        }
    }

    const fn is_date(self) -> bool {
        matches!(self, Self::Due | Self::Scheduled | Self::Start | Self::Done | Self::Created)
    }
}

/// How fields are written: `📅 2026-12-01` or `[due:: 2026-12-01]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Emoji,
    Dataview,
}

impl Format {
    /// From the `tasks.format` setting.
    pub fn from_setting(s: &str) -> Self {
        if s.eq_ignore_ascii_case("dataview") { Self::Dataview } else { Self::Emoji }
    }
}

/// Priority emoji, highest first. `0` is 🔺 highest, `4` ⏬ lowest; no emoji is normal.
pub const PRIORITY_EMOJI: [&str; 5] = ["🔺", "⏫", "🔼", "🔽", "⏬"];
const PRIORITY_WORDS: [&str; 5] = ["highest", "high", "medium", "low", "lowest"];

/// Sort rank of a priority: highest 0 … normal 3 … lowest 5.
pub const fn priority_rank(p: Option<u8>) -> u8 {
    match p {
        Some(n @ 0..=2) => n,
        None => 3,
        Some(n) => n + 1,
    }
}

/// The word for a priority (`high`), or `normal`.
pub fn priority_word(p: Option<u8>) -> &'static str {
    p.and_then(|n| PRIORITY_WORDS.get(n as usize).copied()).unwrap_or("normal")
}

/// Priority from a word or emoji (`high`, `⏫`); `None` for `normal` and
/// for words that aren't priorities (see [`is_priority`]).
pub fn parse_priority(s: &str) -> Option<u8> {
    let s = s.trim().trim_end_matches('\u{fe0f}');
    PRIORITY_WORDS.iter().position(|w| w.eq_ignore_ascii_case(s)).or_else(|| PRIORITY_EMOJI.iter().position(|e| *e == s)).map(|i| i as u8)
}

/// Whether `s` names a priority, `normal` included.
pub fn is_priority(s: &str) -> bool {
    let t = s.trim();
    parse_priority(t).is_some() || t.eq_ignore_ascii_case("normal") || t.eq_ignore_ascii_case("none")
}

/// One field found in a task line. Ranges are bytes of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpan {
    /// `None` for fields Quire keeps but doesn't use, and for tags among the fields.
    pub field: Option<Field>,
    /// The whole field, `📅 2026-12-01` or `[due:: 2026-12-01]`.
    pub range: Range<usize>,
    pub value: String,
    pub format: Format,
}

/// Emoji of Obsidian Tasks fields Quire doesn't use but keeps.
const OTHER_EMOJI: [&str; 4] = ["🆔", "⛔", "🏁", "❌"];

fn emoji_field(e: &str) -> Option<Field> {
    Some(match e {
        "📅" | "📆" | "🗓" => Field::Due,
        "⏳" | "⌛" => Field::Scheduled,
        "🛫" => Field::Start,
        "✅" => Field::Done,
        "➕" => Field::Created,
        "🔁" => Field::Recurrence,
        "⏰" => Field::Reminder,
        _ => return None,
    })
}

fn dataview_field(key: &str) -> Option<Field> {
    Some(match key.trim().to_ascii_lowercase().as_str() {
        "due" => Field::Due,
        "scheduled" => Field::Scheduled,
        "start" => Field::Start,
        "completion" | "done" => Field::Done,
        "created" => Field::Created,
        "repeat" | "recurrence" => Field::Recurrence,
        "priority" => Field::Priority,
        "remind" | "reminder" => Field::Reminder,
        _ => return None,
    })
}

fn is_iso_date(s: &str) -> bool {
    crate::frontmatter::is_date(s) && s.len() == 10
}

/// `HH:MM` (one or two hour digits).
fn is_time(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else { return false };
    (1..=2).contains(&h.len())
        && m.len() == 2
        && h.bytes().all(|b| b.is_ascii_digit())
        && m.bytes().all(|b| b.is_ascii_digit())
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60)
}

/// A reminder value: a date, a date and time (`2026-11-30 09:00` or `…T09:00`).
fn is_reminder(s: &str) -> bool {
    if is_iso_date(s) {
        return true;
    }
    s.len() > 11 && is_iso_date(&s[..10]) && matches!(s.as_bytes()[10], b' ' | b'T') && is_time(&s[11..])
}

/// A `^block-id` at the end of the text, kept after the fields.
fn block_id_start(s: &str) -> Option<usize> {
    let t = s.trim_end();
    let caret = t.rfind('^')?;
    let id = &t[caret + 1..];
    let before_ok = caret == 0 || t[..caret].ends_with(' ');
    (before_ok && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then_some(caret)
}

/// The field ending at `end` in `line`, if one does.
fn field_ending_at(line: &str, start_limit: usize, end: usize) -> Option<FieldSpan> {
    let s = &line[start_limit..end];
    // Dataview `[key:: value]` or `(key:: value)`.
    if let Some(close) = s.chars().last().filter(|c| *c == ']' || *c == ')') {
        let open = if close == ']' { '[' } else { '(' };
        if let Some(o) = s.rfind(open) {
            let inner = &s[o + 1..s.len() - 1];
            if let Some((k, v)) = inner.split_once("::")
                && !k.trim().is_empty()
                && !k.contains(['[', ']', '(', ')'])
            {
                let field = dataview_field(k);
                let value = v.trim().to_string();
                let valid = match field {
                    Some(f) if f.is_date() => is_iso_date(&value),
                    Some(Field::Reminder) => is_reminder(&value),
                    Some(Field::Priority) => is_priority(&value),
                    _ => true,
                };
                if valid {
                    return Some(FieldSpan { field, range: start_limit + o..end, value, format: Format::Dataview });
                }
            }
        }
    }
    // A priority emoji alone.
    let trimmed = s.trim_end_matches('\u{fe0f}');
    for (i, e) in PRIORITY_EMOJI.iter().enumerate() {
        if trimmed.ends_with(e) {
            let at = start_limit + trimmed.len() - e.len();
            return Some(FieldSpan {
                field: Some(Field::Priority),
                range: at..end,
                value: PRIORITY_WORDS[i].into(),
                format: Format::Emoji,
            });
        }
    }
    // A tag among the fields.
    if let Some(sp) = s.rfind([' ', '\t'])
        && s[sp + 1..].starts_with('#')
        && s.len() > sp + 2
        && !s[sp + 1..].contains(['[', ']'])
    {
        return Some(FieldSpan { field: None, range: start_limit + sp + 1..end, value: s[sp + 2..].to_string(), format: Format::Emoji });
    }
    // An emoji field: find the last field emoji before `end`.
    let mut best: Option<(usize, &str)> = None;
    for e in ["📅", "📆", "🗓", "⏳", "⌛", "🛫", "✅", "➕", "🔁", "⏰"].into_iter().chain(OTHER_EMOJI) {
        if let Some(p) = s.rfind(e)
            && best.is_none_or(|(b, _)| p > b)
        {
            best = Some((p, e));
        }
    }
    let (p, e) = best?;
    let value = s[p + e.len()..].trim_start_matches('\u{fe0f}').trim().to_string();
    let field = emoji_field(e);
    let valid = match field {
        Some(f) if f.is_date() => is_iso_date(&value),
        Some(Field::Reminder) => is_reminder(&value),
        Some(Field::Recurrence) => !value.is_empty(),
        _ => true,
    };
    valid.then(|| FieldSpan { field, range: start_limit + p..end, value, format: Format::Emoji })
}

/// The fields at the end of a task line, in line order. `body` is where the
/// text after `- [ ] ` starts (fields are never read before it).
pub fn field_spans(line: &str, body: usize) -> Vec<FieldSpan> {
    let mut end = line.trim_end().len();
    if let Some(b) = block_id_start(&line[body.min(end)..end]) {
        end = line[..body + b].trim_end().len();
    }
    let mut out = Vec::new();
    while end > body {
        let Some(f) = field_ending_at(line, body, end) else { break };
        if f.range.start <= body && f.field.is_none() {
            break; // a lone tag is the description
        }
        end = line[..f.range.start].trim_end().len();
        out.push(f);
    }
    // Tags with no field before them belong to the description.
    while out.last().is_some_and(|f| f.field.is_none() && line[f.range.clone()].starts_with('#')) {
        out.pop();
    }
    out.reverse();
    out
}

/// Where the text after `- [ ] ` starts, given the `[` offset in the line.
fn body_start(line: &str, marker: usize) -> usize {
    let after = (marker + 3).min(line.len());
    after + line[after..].len() - line[after..].trim_start().len()
}

/// Fill `task`'s metadata fields from its source line.
///
/// `line` is the whole line the task's checkbox is on, `marker` the byte
/// offset of `[ ]` within it. The fields are taken out of `task.text` (tags
/// among them stay).
pub fn parse_meta(line: &str, marker: usize, task: &mut Task) {
    if marker + 3 > line.len() || !line.is_char_boundary(marker) {
        return;
    }
    let body = body_start(line, marker);
    // Day Planner time block at the start.
    let rest = &line[body..];
    if let Some((block, len)) = time_block(rest) {
        task.time_block = Some(block);
        if let Some(t) = task.text.strip_prefix(rest[..len].trim_end()) {
            task.text = t.trim_start().to_string();
        }
    }
    // A trailing `^block-id` isn't part of the description.
    let content = line.trim_end();
    if let Some(b) = block_id_start(&content[body.min(content.len())..]) {
        let id = &content[body + b..];
        if let Some(t) = task.text.trim_end().strip_suffix(id) {
            task.text = t.trim_end().to_string();
        }
    }
    let spans = field_spans(line, body);
    let Some(first) = spans.first() else { return };
    let line_start = task.offset - marker;
    let meta_end = spans.last().map_or(first.range.end, |l| l.range.end);
    task.meta_range = Some(line_start + first.range.start..line_start + meta_end);
    let mut tags = Vec::new();
    for s in &spans {
        match s.field {
            Some(Field::Due) => task.due = Some(s.value.clone()),
            Some(Field::Scheduled) => task.scheduled = Some(s.value.clone()),
            Some(Field::Start) => task.start = Some(s.value.clone()),
            Some(Field::Done) => task.done_on = Some(s.value.clone()),
            Some(Field::Created) => task.created = Some(s.value.clone()),
            Some(Field::Recurrence) => task.recurrence = Some(s.value.clone()),
            Some(Field::Priority) => task.priority = parse_priority(&s.value),
            Some(Field::Reminder) => task.reminder = Some(s.value.replacen('T', " ", 1)),
            None if s.format == Format::Emoji && line[s.range.clone()].starts_with('#') => tags.push(line[s.range.clone()].to_string()),
            None if span_key(line, s).is_some_and(|k| k.eq_ignore_ascii_case("time")) => task.time_spent = Some(s.value.clone()),
            None if span_key(line, s).is_some_and(|k| k.eq_ignore_ascii_case("estimate")) => task.estimate = Some(s.value.clone()),
            None => {}
        }
    }
    // The description is what's left of the text once the fields are out.
    let meta = &line[first.range.start..meta_end];
    let text = task.text.trim_end();
    let mut desc = if let Some(t) = text.strip_suffix(meta.trim_end()) {
        t.trim_end().to_string()
    } else {
        let mut t = text.to_string();
        for s in spans.iter().rev() {
            if let Some(p) = t.rfind(&line[s.range.clone()]) {
                t.replace_range(p..p + s.range.len(), "");
            }
        }
        t.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    for t in tags {
        if !desc.contains(&t) {
            desc.push(' ');
            desc.push_str(&t);
        }
    }
    task.text = desc;
}

/// The Day Planner time (`14:00 - 15:30`, `9:00`) at the start of a task's
/// text, as written.
pub fn time_block_text(s: &str) -> Option<&str> {
    time_block(s).map(|(_, len)| s[..len].trim_end())
}

/// `14:00 - 15:30 ` or `9:00 ` at the start of the text: (start, end) and
/// the bytes it takes. A lone time has an empty end.
fn time_block(s: &str) -> Option<((String, String), usize)> {
    let first = s.split_whitespace().next()?;
    // `14:00-15:30`
    if let Some((a, b)) = first.split_once('-')
        && is_time(a)
        && is_time(b)
    {
        let len = s.find(first)? + first.len();
        return Some(((a.into(), b.into()), len + s[len..].len() - s[len..].trim_start().len()));
    }
    if !is_time(first) {
        return None;
    }
    let after = &s[first.len()..];
    let t = after.trim_start();
    if let Some(r) = t.strip_prefix(['-', '–']) {
        let r = r.trim_start();
        let end = r.split_whitespace().next().filter(|e| is_time(e))?;
        let used = s.len() - r.len() + end.len();
        let used = used + s[used..].len() - s[used..].trim_start().len();
        return Some(((first.into(), end.into()), used));
    }
    // A lone time needs text after it.
    (!t.is_empty()).then(|| ((first.into(), String::new()), s.len() - t.len()))
}

/// The text of `field` as written in `format`.
pub fn format_field(field: Field, value: &str, format: Format) -> String {
    match (format, field) {
        (Format::Emoji, Field::Priority) => parse_priority(value).map_or_else(String::new, |p| PRIORITY_EMOJI[p as usize].to_string()),
        (Format::Emoji, f) => format!("{} {}", f.emoji(), value.trim()),
        (Format::Dataview, Field::Priority) => format!("[priority:: {}]", priority_word(parse_priority(value))),
        (Format::Dataview, f) => format!("[{}:: {}]", f.dataview_key(), value.trim()),
    }
}

/// Set, change or (with `None`) remove one field of a task line, keeping
/// every other byte.
///
/// An existing field keeps its style; a new one is written in `format`,
/// after the other fields (before a `^block-id`).
pub fn set_field(line: &str, marker: usize, field: Field, value: Option<&str>, format: Format) -> String {
    let body = body_start(line, marker);
    let spans = field_spans(line, body);
    let existing = spans.iter().find(|s| s.field == Some(field));
    let new_text = value.map(|v| format_field(field, v, existing.map_or(format, |e| e.format))).filter(|t| !t.is_empty());
    match (existing, new_text) {
        (Some(e), Some(t)) => format!("{}{t}{}", &line[..e.range.start], &line[e.range.end..]),
        (Some(e), None) => {
            // Take one space with it.
            let (mut a, mut b) = (e.range.start, e.range.end);
            if line[..a].ends_with(' ') {
                a -= 1;
            } else if line[b..].starts_with(' ') {
                b += 1;
            }
            format!("{}{}", &line[..a], &line[b..])
        }
        (None, Some(t)) => {
            let content_end = line.trim_end_matches(['\n', '\r']).len();
            let at = match (spans.last(), block_id_start(&line[body.min(content_end)..content_end])) {
                (Some(l), _) => l.range.end,
                (None, Some(b)) => line[..body + b].trim_end().len(),
                (None, None) => line[..content_end].trim_end().len(),
            };
            let sep = if at > 0 && !line[..at].ends_with(' ') { " " } else { "" };
            format!("{}{sep}{t}{}", &line[..at], &line[at..])
        }
        (None, None) => line.to_string(),
    }
}

/// The key of a Dataview span (`[time:: 1h]` → `time`).
fn span_key<'a>(line: &'a str, s: &FieldSpan) -> Option<&'a str> {
    if s.format != Format::Dataview {
        return None;
    }
    let inner = &line[s.range.start + 1..s.range.end - 1];
    inner.split_once("::").map(|(k, _)| k.trim())
}

/// The value of any Dataview field among a task line's fields
/// (`[time:: 1h25m]`), by key, ignoring case.
pub fn inline_field(line: &str, marker: usize, key: &str) -> Option<String> {
    let body = body_start(line, marker);
    field_spans(line, body).into_iter().find(|s| span_key(line, s).is_some_and(|k| k.eq_ignore_ascii_case(key))).map(|s| s.value)
}

/// Set or (with `None`) remove a Dataview field `[key:: value]` on a task
/// line, keeping every other byte. A new field goes after the others
/// (before a `^block-id`).
pub fn set_inline_field(line: &str, marker: usize, key: &str, value: Option<&str>) -> String {
    let body = body_start(line, marker);
    let spans = field_spans(line, body);
    let existing = spans.iter().find(|s| span_key(line, s).is_some_and(|k| k.eq_ignore_ascii_case(key)));
    match (existing, value) {
        (Some(e), Some(v)) => {
            let k = span_key(line, e).unwrap_or(key);
            let (open, close) = if line[e.range.clone()].starts_with('(') { ('(', ')') } else { ('[', ']') };
            format!("{}{open}{k}:: {}{close}{}", &line[..e.range.start], v.trim(), &line[e.range.end..])
        }
        (Some(e), None) => {
            let (mut a, mut b) = (e.range.start, e.range.end);
            if line[..a].ends_with(' ') {
                a -= 1;
            } else if line[b..].starts_with(' ') {
                b += 1;
            }
            format!("{}{}", &line[..a], &line[b..])
        }
        (None, Some(v)) => {
            let content_end = line.trim_end_matches(['\n', '\r']).len();
            let at = match (spans.last(), block_id_start(&line[body.min(content_end)..content_end])) {
                (Some(l), _) => l.range.end,
                (None, Some(b)) => line[..body + b].trim_end().len(),
                (None, None) => line[..content_end].trim_end().len(),
            };
            let sep = if at > 0 && !line[..at].ends_with(' ') { " " } else { "" };
            let after = &line[at..];
            let gap = if after.starts_with(' ') || after.trim_end_matches(['\n', '\r']).is_empty() { "" } else { " " };
            format!("{}{sep}[{key}:: {}]{gap}{after}", &line[..at], v.trim())
        }
        (None, None) => line.to_string(),
    }
}

/// The line with its checkbox set to done (`[x]`) or open (`[ ]`).
pub fn set_done(line: &str, marker: usize, done: bool) -> String {
    if marker + 3 > line.len() || !line.is_char_boundary(marker) || !line[marker..].starts_with('[') {
        return line.to_string();
    }
    format!("{}[{}]{}", &line[..marker], if done { 'x' } else { ' ' }, &line[marker + 3..])
}

/// Byte offset of `[` in a task line (`- [ ]`, `* [x]`, `1. [ ]`, `> - [ ]`).
pub fn marker_in_line(line: &str) -> Option<usize> {
    let mut rest = line;
    let mut at = 0;
    loop {
        let t = rest.trim_start();
        at += rest.len() - t.len();
        if let Some(r) = t.strip_prefix('>') {
            at += 1;
            rest = r;
            continue;
        }
        rest = t;
        break;
    }
    let bullet = if rest.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !rest[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    };
    let after = &rest[bullet..];
    let t = after.trim_start_matches(' ');
    if t.len() == after.len() || !t.starts_with('[') || t.len() < 3 || t.as_bytes()[2] != b']' {
        return None;
    }
    Some(at + bullet + (after.len() - t.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Task {
        let marker = marker_in_line(line).expect("a task line");
        let text = line[marker + 3..].trim().to_string();
        let mut t = Task { text, offset: 100 + marker, ..Task::default() };
        parse_meta(line, marker, &mut t);
        t
    }

    #[test]
    fn emoji_fields() {
        let t = parse("- [ ] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-11-28 📅 2026-12-01 ⏰ 2026-11-30 09:00 ✅ 2026-12-01");
        assert_eq!(t.text, "Pay rent");
        assert_eq!(t.priority, Some(1));
        assert_eq!(t.recurrence.as_deref(), Some("every month on the 1st"));
        assert_eq!(t.scheduled.as_deref(), Some("2026-11-28"));
        assert_eq!(t.due.as_deref(), Some("2026-12-01"));
        assert_eq!(t.reminder.as_deref(), Some("2026-11-30 09:00"));
        assert_eq!(t.done_on.as_deref(), Some("2026-12-01"));
        assert_eq!(
            t.meta_range,
            Some(
                100 + 15
                    ..100
                        + "- [ ] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-11-28 📅 2026-12-01 ⏰ 2026-11-30 09:00 ✅ 2026-12-01".len()
            )
        );
        let t = parse("- [ ] Plan 🔺 🛫 2026-11-01 ➕ 2026-10-30 ⌛️ 2026-11-02");
        assert_eq!(
            (t.priority, t.start.as_deref(), t.created.as_deref(), t.scheduled.as_deref()),
            (Some(0), Some("2026-11-01"), Some("2026-10-30"), Some("2026-11-02"))
        );
    }

    #[test]
    fn dataview_fields_and_unknowns() {
        let t = parse("- [x] Ship it [due:: 2026-12-01] (priority:: low) [repeat:: every week] [remind:: 2026-11-30T08:15] [owner:: Ada]");
        assert_eq!(t.text, "Ship it");
        assert_eq!((t.due.as_deref(), t.priority, t.recurrence.as_deref()), (Some("2026-12-01"), Some(3), Some("every week")));
        assert_eq!(t.reminder.as_deref(), Some("2026-11-30 08:15"));
        // Unused Obsidian Tasks fields and tags between fields are skipped over.
        let t = parse("- [ ] Call Bo #work 📅 2026-12-01 🆔 abc #home ⛔ xyz ^blk-1");
        assert_eq!((t.text.as_str(), t.due.as_deref()), ("Call Bo #work #home", Some("2026-12-01")));
        // An emoji in the description with no valid value stays text.
        let t = parse("- [ ] Buy a 📅 calendar");
        assert_eq!((t.text.as_str(), t.due, t.meta_range), ("Buy a 📅 calendar", None, None));
        let t = parse("- [ ] Read #book");
        assert_eq!((t.text.as_str(), t.meta_range), ("Read #book", None));
        let t = parse("- [ ] Bad date 📅 2026-13");
        assert_eq!(t.due, None);
    }

    #[test]
    fn time_blocks() {
        let t = parse("- [ ] 14:00 - 15:30 Write report 📅 2026-11-02");
        assert_eq!(t.time_block, Some(("14:00".into(), "15:30".into())));
        assert_eq!((t.text.as_str(), t.due.as_deref()), ("Write report", Some("2026-11-02")));
        let t = parse("- [ ] 9:05-10:00 Standup");
        assert_eq!((t.time_block, t.text.as_str()), (Some(("9:05".into(), "10:00".into())), "Standup"));
        let t = parse("- [ ] 08:30 Gym");
        assert_eq!((t.time_block, t.text.as_str()), (Some(("08:30".into(), String::new())), "Gym"));
        assert_eq!(parse("- [ ] 25:00 Nope").time_block, None);
        assert_eq!(parse("- [ ] 10:00").time_block, None);
    }

    #[test]
    fn writes_fields_in_place() {
        let line = "- [ ] Pay rent 🔁 every month 📅 2026-12-01 ^r1";
        let m = marker_in_line(line).unwrap();
        assert_eq!(set_field(line, m, Field::Due, Some("2027-01-01"), Format::Dataview), "- [ ] Pay rent 🔁 every month 📅 2027-01-01 ^r1");
        assert_eq!(
            set_field(line, m, Field::Done, Some("2026-12-02"), Format::Emoji),
            "- [ ] Pay rent 🔁 every month 📅 2026-12-01 ✅ 2026-12-02 ^r1"
        );
        assert_eq!(set_field(line, m, Field::Recurrence, None, Format::Emoji), "- [ ] Pay rent 📅 2026-12-01 ^r1");
        assert_eq!(set_field(line, m, Field::Priority, Some("high"), Format::Emoji), "- [ ] Pay rent 🔁 every month 📅 2026-12-01 ⏫ ^r1");
        let plain = "  * [x] Thing ^id";
        let m = marker_in_line(plain).unwrap();
        assert_eq!(set_field(plain, m, Field::Done, Some("2026-11-02"), Format::Dataview), "  * [x] Thing [completion:: 2026-11-02] ^id");
        let dv = "- [ ] T [due:: 2026-12-01]";
        assert_eq!(
            set_field(dv, 2, Field::Due, Some("2026-12-05"), Format::Emoji),
            "- [ ] T [due:: 2026-12-05]",
            "keeps the field's style"
        );
        assert_eq!(set_done(dv, 2, true), "- [x] T [due:: 2026-12-01]");
        assert_eq!(marker_in_line("> 1. [ ] quoted"), Some(5));
        assert_eq!(marker_in_line("- not a task"), None);
        assert_eq!(marker_in_line("-[ ] no space"), None);
        // Writing a field then reading it back.
        for f in Field::ALL {
            let v = match f {
                Field::Recurrence => "every week",
                Field::Priority => "low",
                Field::Reminder => "2026-11-30 09:00",
                _ => "2026-11-30",
            };
            for fmt in [Format::Emoji, Format::Dataview] {
                let out = set_field("- [ ] Task #x", 2, f, Some(v), fmt);
                let t = parse(&out);
                assert_eq!(t.text, "Task #x", "{out}");
                let got = match f {
                    Field::Due => t.due,
                    Field::Scheduled => t.scheduled,
                    Field::Start => t.start,
                    Field::Done => t.done_on,
                    Field::Created => t.created,
                    Field::Recurrence => t.recurrence,
                    Field::Priority => Some(priority_word(t.priority).to_string()),
                    Field::Reminder => t.reminder,
                };
                assert_eq!(got.as_deref(), Some(v), "{out}");
            }
        }
    }

    #[test]
    fn priorities_rank() {
        let mut v = [Some(4), None, Some(0), Some(2), Some(3), Some(1)];
        v.sort_by_key(|p| priority_rank(*p));
        assert_eq!(v, [Some(0), Some(1), Some(2), None, Some(3), Some(4)]);
        assert_eq!(parse_priority("⏬"), Some(4));
        assert!(parse_priority("Normal").is_none() && is_priority("Normal"));
        assert!(parse_priority("urgent").is_none() && !is_priority("urgent"));
    }

    #[test]
    fn inline_fields_by_key() {
        let line = "- [ ] Write report 📅 2026-11-20 [time:: 25m] ^rep";
        assert_eq!(inline_field(line, 2, "time").as_deref(), Some("25m"));
        assert_eq!(inline_field(line, 2, "estimate"), None);
        assert_eq!(set_inline_field(line, 2, "time", Some("1h5m")), "- [ ] Write report 📅 2026-11-20 [time:: 1h5m] ^rep");
        assert_eq!(set_inline_field(line, 2, "time", None), "- [ ] Write report 📅 2026-11-20 ^rep");
        assert_eq!(set_inline_field("- [ ] Plain", 2, "time", Some("5m")), "- [ ] Plain [time:: 5m]");
        assert_eq!(set_inline_field("- [ ] Id ^x1", 2, "time", Some("5m")), "- [ ] Id [time:: 5m] ^x1");
        assert_eq!(set_inline_field("- [ ] Dates 📅 2026-11-20", 2, "time", Some("5m")), "- [ ] Dates 📅 2026-11-20 [time:: 5m]");
        // The parsed task is unchanged apart from the field.
        let t = parse("- [ ] Dates 📅 2026-11-20 [time:: 5m] [estimate:: 1h]");
        assert_eq!((t.text.as_str(), t.due.as_deref()), ("Dates", Some("2026-11-20")));
        assert_eq!((t.time_spent.as_deref(), t.estimate.as_deref()), (Some("5m"), Some("1h")));
        assert_eq!(set_inline_field("- [ ] A (time:: 1m)", 2, "Time", Some("2m")), "- [ ] A (time:: 2m)");
    }
}
