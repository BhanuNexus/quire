// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Note templates: declarative placeholders, never code (`docs/design/0.9.md` §E.3, P7).
//!
//! * dates: `{{date}}`, `{{date:%A}}`, `{{date+7d:%Y-%m-%d}}`,
//!   `{{date:next monday}}`, `{{date-1w}}`; `{{time}}`, `{{time:%H:%M}}`;
//! * `{{title}}`, `{{folder}}`, `{{cursor}}` (where the caret goes);
//! * prompts, asked once per key: `{{prompt:Client}}`,
//!   `{{prompt:Client|ACME}}` (with a default), `{{select:Status|planned,active,done}}`.
//!
//! A template's front matter may hold `quire-template:` (`folder`, `name`,
//! `applies_to`); it is read with [`template_meta`] and left out of notes.
//! Expansion is two-phase: [`required_prompts`], then [`expand`].
//! Unknown placeholders are kept as written.

use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;

use quire_base::dates::{self, Date, Span, Weekday, Zoned};

#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub title: String,
    /// The moment `{{date}}` and `{{time}}` stand for, in the zone it is shown in.
    pub now: Zoned,
}

/// A question a template asks before the note is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// What's asked (`Client`); answers are keyed by it.
    pub key: String,
    /// Pre-filled answer (`ACME`, or a select's first option).
    pub default: String,
    /// For `{{select:…}}`: the choices.
    pub options: Vec<String>,
}

/// `quire-template:` settings in a template's front matter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TemplateMeta {
    /// Default folder for notes made from it.
    pub folder: Option<String>,
    /// Name pattern for new notes (`{{date}} {{prompt:Topic}}`).
    pub name: Option<String>,
    /// Folders where a new note uses this template.
    pub applies_to: Vec<String>,
}

/// The front matter key for template settings.
pub const META_KEY: &str = "quire-template";

/// Read `quire-template:` from a template.
pub fn template_meta(tpl: &str) -> TemplateMeta {
    let Some(fm) = quire_markdown::frontmatter::parse(tpl) else { return TemplateMeta::default() };
    let Some(m) = fm.get(META_KEY).and_then(quire_markdown::Value::as_map) else { return TemplateMeta::default() };
    let s = |k: &str| m.get(k).and_then(quire_markdown::Value::as_str).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    TemplateMeta {
        folder: s("folder").map(|f| f.trim_matches('/').to_string()),
        name: s("name"),
        applies_to: m
            .get("applies_to")
            .map(quire_markdown::Value::as_list)
            .unwrap_or_default()
            .into_iter()
            .map(|f| f.trim().trim_matches('/').to_string())
            .collect(),
    }
}

/// The template without its `quire-template:` settings (and without front
/// matter at all if that was its only field).
fn strip_meta(tpl: &str) -> String {
    match quire_markdown::frontmatter::remove_field(tpl, META_KEY) {
        Some(e) => e.apply(tpl),
        None => tpl.to_string(),
    }
}

/// Placeholders in order: (byte range including braces, inner text).
fn placeholders(tpl: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(open) = tpl[at..].find("{{").map(|o| at + o) {
        let Some(close) = tpl[open + 2..].find("}}").map(|c| open + 2 + c) else { break };
        // `{{a {{b}}`: start again at the inner `{{`.
        if let Some(inner) = tpl[open + 2..close].rfind("{{") {
            at = open + 2 + inner;
            continue;
        }
        out.push((open..close + 2, &tpl[open + 2..close]));
        at = close + 2;
    }
    out
}

fn parse_prompt(inner: &str) -> Option<Prompt> {
    let inner = inner.trim();
    if let Some(rest) = inner.strip_prefix("prompt:") {
        let (key, default) = rest.split_once('|').unwrap_or((rest, ""));
        let key = key.trim();
        return (!key.is_empty()).then(|| Prompt { key: key.to_string(), default: default.trim().to_string(), options: Vec::new() });
    }
    if let Some(rest) = inner.strip_prefix("select:") {
        let (key, opts) = rest.split_once('|')?;
        let options: Vec<String> = opts.split(',').map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect();
        let key = key.trim();
        return (!key.is_empty() && !options.is_empty()).then(|| Prompt { key: key.to_string(), default: options[0].clone(), options });
    }
    None
}

/// The questions a template asks (its name pattern's too), each key once.
pub fn required_prompts(tpl: &str) -> Vec<Prompt> {
    let meta = template_meta(tpl);
    let body = strip_meta(tpl);
    let mut out: Vec<Prompt> = Vec::new();
    let name = meta.name.unwrap_or_default();
    for src in [name.as_str(), body.as_str()] {
        for (_, inner) in placeholders(src) {
            if let Some(p) = parse_prompt(inner)
                && !out.iter().any(|o| o.key.eq_ignore_ascii_case(&p.key))
            {
                out.push(p);
            }
        }
    }
    out
}

/// What [`expand`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    pub text: String,
    /// Char offset of `{{cursor}}`, if the template has one.
    pub cursor: Option<usize>,
}

/// Fill in a template. `answers` holds prompt answers by key; a prompt
/// without one gets its default. `folder` fills `{{folder}}`.
pub fn expand<S: BuildHasher>(tpl: &str, vars: &TemplateVars, folder: &str, answers: &HashMap<String, String, S>) -> Expanded {
    expand_text(&strip_meta(tpl), vars, folder, &with_defaults(tpl, answers), true)
}

/// A new note's name from a template's `name:` pattern (no `{{cursor}}`).
/// `tpl` is the whole template, for the prompts' defaults.
pub fn expand_name<S: BuildHasher>(
    tpl: &str,
    pattern: &str,
    vars: &TemplateVars,
    folder: &str,
    answers: &HashMap<String, String, S>,
) -> String {
    expand_text(pattern, vars, folder, &with_defaults(tpl, answers), false).text.trim().to_string()
}

/// The answers by prompt key, each unanswered prompt with its default (a key
/// repeated without a default takes it from its first use).
///
/// An answer's key matches a prompt's ignoring ASCII case (`Who` answers
/// `{{prompt:who}}`). When more than one answer matches the same prompt,
/// the one written exactly as the prompt's key wins, and otherwise the
/// first in byte order (`NAME`, then `Name`, then `name`), never the hash
/// order of `answers`. An answer that matches no prompt is kept under its
/// own key.
fn with_defaults<S: BuildHasher>(tpl: &str, answers: &HashMap<String, String, S>) -> BTreeMap<String, String> {
    let mut all = BTreeMap::new();
    for p in required_prompts(tpl) {
        let value = answer_for(answers, &p.key).map_or(p.default, Clone::clone);
        all.insert(p.key, value);
    }
    let given: BTreeMap<&String, &String> = answers.iter().collect();
    for (k, v) in given {
        if !all.keys().any(|key| key.eq_ignore_ascii_case(k)) {
            all.insert(k.clone(), v.clone());
        }
    }
    all
}

/// The answer for the prompt `key`: the one written exactly as `key`, else
/// the first in byte order that matches ignoring ASCII case. It never
/// depends on the hash order of `answers`.
pub fn answer_for<'a, S: BuildHasher>(answers: &'a HashMap<String, String, S>, key: &str) -> Option<&'a String> {
    answers.get(key).or_else(|| answers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).min_by(|a, b| a.0.cmp(b.0)).map(|(_, v)| v))
}

/// The answer for `key`: written the same, or else the first (in byte
/// order) that matches ignoring ASCII case.
fn answer<'a>(answers: &'a BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    answers.get(key).or_else(|| answers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v))
}

fn expand_text(tpl: &str, vars: &TemplateVars, folder: &str, answers: &BTreeMap<String, String>, cursor_ok: bool) -> Expanded {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut cursor = None;
    let mut at = 0;
    for (range, inner) in placeholders(tpl) {
        out.push_str(&tpl[at..range.start]);
        at = range.end;
        let key = inner.trim();
        if let Some(p) = parse_prompt(key) {
            out.push_str(answer(answers, &p.key).map_or(p.default.as_str(), String::as_str));
            continue;
        }
        match key {
            "title" => out.push_str(&vars.title),
            "folder" => out.push_str(folder),
            "cursor" if cursor_ok => cursor = Some(out.chars().count()),
            "cursor" => {}
            k => match render_date(k, &vars.now) {
                Some(s) => out.push_str(&s),
                None => out.push_str(&tpl[range]),
            },
        }
    }
    out.push_str(&tpl[at..]);
    Expanded { text: out, cursor }
}

/// `date`, `date:%A`, `date+7d:%F`, `date:next monday`, `time`, `time:%H`.
fn render_date(key: &str, now: &Zoned) -> Option<String> {
    let (head, arg) = key.split_once(':').map_or((key, None), |(h, a)| (h, Some(a.trim())));
    let (base, default_fmt) = match head.trim() {
        h if h.starts_with("date") => (&h[4..], "%Y-%m-%d"),
        h if h.starts_with("time") => (&h[4..], "%H:%M"),
        _ => return None,
    };
    let mut date = if base.is_empty() { now.clone() } else { offset(now, base)? };
    // The argument is a format (has `%`), a phrase, or `phrase:%format`.
    let (phrase, fmt): (Option<&str>, &str) = match arg {
        None => (None, default_fmt),
        Some(a) => match a.find(":%") {
            Some(i) => (Some(a[..i].trim()), &a[i + 1..]),
            None if a.contains('%') => (None, a),
            None => (Some(a), default_fmt),
        },
    };
    if let Some(p) = phrase {
        let day = relative_day(p, date.date())?;
        date = dates::single_in(date.time_zone(), day.to_datetime(date.time())).unwrap_or(date);
    }
    // A pattern that isn't one, or can't be written (`%#z`, which used to
    // panic), leaves the placeholder as typed.
    dates::format(fmt, &dates::local_moment(&date)).ok()
}

/// `+7d`, `-1w`, `+2m`, `+1y` from now. Days and weeks are 24 hours each
/// (across a clock change the hour moves, as it always did); months and
/// years move the calendar date and keep the clock time, none when that
/// time doesn't exist that day or exists twice.
fn offset(now: &Zoned, s: &str) -> Option<Zoned> {
    let s = s.trim();
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1i64, &s[1..]),
        b'-' => (-1i64, &s[1..]),
        _ => return None,
    };
    let unit = rest.chars().last()?;
    let n: i64 = rest[..rest.len() - unit.len_utf8()].trim().parse().ok()?;
    let hours = |days: i64| {
        let at = now.timestamp().checked_add(Span::new().try_hours(days.checked_mul(24)?).ok()?).ok()?;
        Some(at.to_zoned(now.time_zone().clone()))
    };
    match unit {
        'd' => hours(sign * n),
        'w' => hours(sign * n * 7),
        'm' | 'y' => {
            let months = sign * n.checked_mul(if unit == 'y' { 12 } else { 1 })?;
            let moved = now.datetime().checked_add(Span::new().try_months(months).ok()?).ok()?;
            dates::single_in(now.time_zone(), moved)
        }
        _ => None,
    }
}

const WEEKDAY_NAMES: [(&str, Weekday); 7] = [
    ("monday", Weekday::Monday),
    ("tuesday", Weekday::Tuesday),
    ("wednesday", Weekday::Wednesday),
    ("thursday", Weekday::Thursday),
    ("friday", Weekday::Friday),
    ("saturday", Weekday::Saturday),
    ("sunday", Weekday::Sunday),
];

/// `today`, `tomorrow`, `yesterday`, `monday`, `next friday`, `last monday`,
/// `start of week`, `end of month`, `start of year`.
pub fn relative_day(phrase: &str, today: Date) -> Option<Date> {
    let p = phrase.trim().to_ascii_lowercase();
    let words: Vec<&str> = p.split_whitespace().collect();
    let weekday = |w: &str| WEEKDAY_NAMES.iter().find(|(n, _)| n.starts_with(w) && w.len() >= 3).map(|(_, d)| *d);
    let from_monday = |d: Weekday| i64::from(d.to_monday_zero_offset());
    let plus = |days: i64| dates::add_days(today, days);
    let next = |d: Weekday| {
        let ahead = (7 + from_monday(d) - from_monday(today.weekday())) % 7;
        plus(if ahead == 0 { 7 } else { ahead })
    };
    let last = |d: Weekday| {
        let back = (7 + from_monday(today.weekday()) - from_monday(d)) % 7;
        plus(-(if back == 0 { 7 } else { back }))
    };
    match words.as_slice() {
        ["today"] => Some(today),
        ["tomorrow"] => plus(1),
        ["yesterday"] => plus(-1),
        [w] => {
            // This week's day: today or the next one.
            let d = weekday(w)?;
            plus((7 + from_monday(d) - from_monday(today.weekday())) % 7)
        }
        ["next", "week"] => plus(7),
        ["last", "week"] => plus(-7),
        ["next", w] => next(weekday(w)?),
        ["last", w] => last(weekday(w)?),
        ["start", "of", "week"] => plus(-from_monday(today.weekday())),
        ["end", "of", "week"] => plus(6 - from_monday(today.weekday())),
        ["start", "of", "month"] => Some(today.first_of_month()),
        ["end", "of", "month"] => Some(today.last_of_month()),
        ["start", "of", "year"] => Some(today.first_of_year()),
        ["end", "of", "year"] => Some(today.last_of_year()),
        _ => None,
    }
}

/// Expand in one go, without asking.
///
/// Prompts take their defaults and are otherwise left as written, so
/// nothing a template asks is lost silently. Kept for callers that can't
/// ask (0.8 API). Returns the text and the `{{cursor}}` char offset.
pub fn expand_template(tpl: &str, vars: &TemplateVars) -> (String, Option<usize>) {
    let e = expand_deferred(tpl, vars, "");
    (e.text, e.cursor)
}

/// Like [`expand`] without answers, but prompts without a default stay as
/// `{{prompt:…}}`, to be filled in later ([`pending_prompts`]).
pub fn expand_deferred(tpl: &str, vars: &TemplateVars, folder: &str) -> Expanded {
    let body = strip_meta(tpl);
    let defaults: HashMap<String, String> = required_prompts(tpl).into_iter().map(|p| (p.key.to_lowercase(), p.default)).collect();
    // Prompts without a default are hidden from expansion, then put back.
    let mut keep = String::with_capacity(body.len());
    let mut at = 0;
    for (range, inner) in placeholders(&body) {
        keep.push_str(&body[at..range.start]);
        at = range.end;
        match parse_prompt(inner) {
            Some(p) if defaults.get(&p.key.to_lowercase()).is_none_or(String::is_empty) => {
                keep.push('\u{0}');
                keep.push_str(inner);
                keep.push('\u{1}');
            }
            _ => keep.push_str(&body[range]),
        }
    }
    keep.push_str(&body[at..]);
    let e = expand_text(&keep, vars, folder, &with_defaults(tpl, &HashMap::new()), true);
    // Each marker becomes two characters: move the caret past them.
    let cursor = e.cursor.map(|c| c + e.text.chars().take(c).filter(|ch| matches!(ch, '\u{0}' | '\u{1}')).count());
    Expanded { text: e.text.replace('\u{0}', "{{").replace('\u{1}', "}}"), cursor }
}

/// Prompts left in a note made without asking (`{{prompt:Client}}`), for
/// filling in afterwards: (byte range, prompt).
pub fn pending_prompts(text: &str) -> Vec<(std::ops::Range<usize>, Prompt)> {
    placeholders(text).into_iter().filter_map(|(r, inner)| parse_prompt(inner).map(|p| (r, p))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i16, m: i8, d: i8, h: i8, mi: i8, tz: &str) -> Zoned {
        Date::new(y, m, d).unwrap().at(h, mi, 0, 0).to_zoned(dates::TimeZone::get(tz).unwrap()).unwrap()
    }

    fn vars() -> TemplateVars {
        TemplateVars { title: "Standup".into(), now: at(2026, 11, 1, 9, 5, "UTC") }
    }

    #[test]
    fn expands_variables() {
        let (s, cur) = expand_template("# {{title}}\n{{date}} {{time}} {{date:%A}}\n{{cursor}}\n{{unknown}}", &vars());
        assert_eq!(s, "# Standup\n2026-11-01 09:05 Sunday\n\n{{unknown}}");
        assert_eq!(cur, Some("# Standup\n2026-11-01 09:05 Sunday\n".chars().count()));
    }

    #[test]
    fn date_math_and_phrases() {
        let e = |t: &str| expand(t, &vars(), "Meetings", &HashMap::new()).text;
        assert_eq!(e("{{date+7d:%Y-%m-%d}}"), "2026-11-08");
        assert_eq!(e("{{date-1w}}"), "2026-10-25");
        assert_eq!(e("{{date+1m}} {{date+1y:%Y}}"), "2026-12-01 2027");
        assert_eq!(e("{{date:next monday}}"), "2026-11-02");
        assert_eq!(e("{{date:last friday:%a %d}}"), "Fri 30");
        assert_eq!(e("{{date:tomorrow}} {{date:end of month}}"), "2026-11-02 2026-11-30");
        assert_eq!(e("{{time:%H}}h in {{folder}}"), "09h in Meetings");
        assert_eq!(e("{{date:%Q}} {{date+3x}} {{date:someday}}"), "{{date:%Q}} {{date+3x}} {{date:someday}}", "bad ones stay as written");
        // Parses, but can't be written (it used to panic): stays as typed.
        assert_eq!(e("{{date:%#z}} {{time:%#z}}"), "{{date:%#z}} {{time:%#z}}");
        let sunday = Date::new(2026, 11, 1).unwrap();
        assert_eq!(relative_day("sunday", sunday), Some(sunday));
        assert_eq!(relative_day("next sunday", sunday), Some(Date::new(2026, 11, 8).unwrap()));
        assert_eq!(relative_day("start of week", sunday), Some(Date::new(2026, 10, 26).unwrap()));
        assert_eq!(relative_day("last sunday", sunday), Some(Date::new(2026, 10, 25).unwrap()));
        assert_eq!(relative_day("end of year", sunday), Some(Date::new(2026, 12, 31).unwrap()));
        assert_eq!(relative_day("end of month", Date::new(2028, 2, 3).unwrap()), Some(Date::new(2028, 2, 29).unwrap()));
    }

    /// Across a clock change, as with chrono: `+1d` is 24 hours later, `+1m`
    /// keeps the clock time; `%Z` is the offset.
    #[test]
    fn date_math_across_a_clock_change() {
        let Ok(_) = dates::TimeZone::get("Europe/Berlin") else { return };
        let v = TemplateVars { title: "T".into(), now: at(2026, 3, 28, 12, 0, "Europe/Berlin") };
        let e = |t: &str| expand(t, &v, "", &HashMap::new()).text;
        assert_eq!(e("{{date+1d:%Y-%m-%d %H.%M %Z}}"), "2026-03-29 13.00 +02:00");
        assert_eq!(e("{{date+1m:%Y-%m-%d %H.%M %z}}"), "2026-04-28 12.00 +0200");
        assert_eq!(e("{{date:%Z %z}}"), "+01:00 +0100");
        let jan31 = TemplateVars { title: "T".into(), now: at(2026, 1, 31, 9, 0, "UTC") };
        assert_eq!(expand("{{date+1m}}", &jan31, "", &HashMap::new()).text, "2026-02-28");
    }

    const MEETING: &str = "---\nquire-template:\n  folder: Meetings/\n  name: \"{{date}} {{prompt:Topic}}\"\n  applies_to: [Meetings/, Calls]\ntags: [meeting]\n---\n# {{prompt:Topic}} with {{prompt:Client|ACME}}\nStatus: {{select:Status|planned, active, done}}\nAgain: {{prompt:client}}\n";

    #[test]
    fn prompts_and_settings() {
        let meta = template_meta(MEETING);
        assert_eq!(
            meta,
            TemplateMeta {
                folder: Some("Meetings".into()),
                name: Some("{{date}} {{prompt:Topic}}".into()),
                applies_to: vec!["Meetings".into(), "Calls".into()]
            }
        );
        let p = required_prompts(MEETING);
        assert_eq!(
            p.iter().map(|p| (p.key.as_str(), p.default.as_str())).collect::<Vec<_>>(),
            [("Topic", ""), ("Client", "ACME"), ("Status", "planned")]
        );
        assert_eq!(p[2].options, ["planned", "active", "done"]);
        let answers = HashMap::from([("Topic".to_string(), "Roadmap".to_string()), ("Status".to_string(), "active".to_string())]);
        let e = expand(MEETING, &vars(), "Meetings", &answers);
        assert_eq!(e.text, "---\ntags: [meeting]\n---\n# Roadmap with ACME\nStatus: active\nAgain: ACME\n");
        assert_eq!(expand_name(MEETING, &meta.name.unwrap(), &vars(), "", &answers), "2026-11-01 Roadmap");
        // Without asking, prompts without a default stay for later.
        let (s, _) = expand_template(MEETING, &vars());
        assert_eq!(s, "---\ntags: [meeting]\n---\n# {{prompt:Topic}} with ACME\nStatus: planned\nAgain: ACME\n");
        assert_eq!(pending_prompts(&s).len(), 1);
        let e = expand_deferred("{{prompt:A}} {{cursor}}x", &vars(), "");
        assert_eq!((e.text.as_str(), e.cursor), ("{{prompt:A}} x", Some(13)));
        // A template whose only front matter is the settings loses the block.
        let (s, _) = expand_template("---\nquire-template:\n  folder: X\n---\nBody {{title}}\n", &vars());
        assert_eq!(s, "Body Standup\n");
    }

    #[test]
    fn answers_that_differ_only_in_case_give_one_result() {
        let tpl = "{{prompt:Client}} / {{prompt:client}} / {{prompt:Topic}} / {{prompt:Extra|-}}";
        let run = |pairs: &[(&str, &str)]| {
            // A new map each time: each has its own hash order.
            let answers: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
            expand(tpl, &vars(), "", &answers).text
        };
        // The key written as the prompt's wins; otherwise the first in byte order.
        let both = [("client", "lower"), ("Client", "exact"), ("TOPIC", "upper"), ("topic", "lower")];
        for _ in 0..64 {
            assert_eq!(run(&both), "exact / exact / upper / -");
        }
        assert_eq!(run(&[("CLIENT", "a"), ("client", "b")]), "a / a /  / -");
        let answers: HashMap<String, String> =
            [("name", "lower"), ("NAME", "upper"), ("Name", "exact")].map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(answer_for(&answers, "Name").map(String::as_str), Some("exact"));
        assert_eq!(answer_for(&answers, "nAme").map(String::as_str), Some("upper"));
        assert_eq!(answer_for(&answers, "other"), None);
    }
}
