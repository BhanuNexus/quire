// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use chrono::{DateTime, Local};

#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub title: String,
    pub now: DateTime<Local>,
}

/// Expand `{{title}}`, `{{date}}`, `{{time}}`, `{{date:FORMAT}}` (strftime) and
/// `{{cursor}}` (removed; its char offset is returned so the editor can put the
/// caret there).
pub fn expand_template(tpl: &str, vars: &TemplateVars) -> (String, Option<usize>) {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut cursor = None;
    let mut rest = tpl;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let key = after[..close].trim();
        match key {
            "title" => out.push_str(&vars.title),
            "date" => out.push_str(&vars.now.format("%Y-%m-%d").to_string()),
            "time" => out.push_str(&vars.now.format("%H:%M").to_string()),
            "cursor" => cursor = Some(out.chars().count()),
            k if k.starts_with("date:") => {
                use std::fmt::Write;
                let fmt = &k[5..];
                // chrono panics on invalid format strings at display time; validate first.
                let items: Vec<_> = chrono::format::StrftimeItems::new(fmt).collect();
                if items.iter().any(|i| matches!(i, chrono::format::Item::Error)) {
                    out.push_str(&after[..close]);
                } else {
                    let _ = write!(out, "{}", vars.now.format_with_items(items.into_iter()));
                }
            }
            _ => {
                out.push_str("{{");
                out.push_str(&after[..close]);
                out.push_str("}}");
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn expands_variables() {
        let vars = TemplateVars { title: "Standup".into(), now: Local.with_ymd_and_hms(2026, 11, 1, 9, 5, 0).unwrap() };
        let (s, cur) = expand_template("# {{title}}\n{{date}} {{time}} {{date:%A}}\n{{cursor}}\n{{unknown}}", &vars);
        assert_eq!(s, "# Standup\n2026-11-01 09:05 Sunday\n\n{{unknown}}");
        assert_eq!(cur, Some("# Standup\n2026-11-01 09:05 Sunday\n".chars().count()));
    }
}
