// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Built-in prompts: `quire_ingest`, `quire_lint`, `quire_answer`, `quire_maintain_index`.
//!
//! The same texts drive the panel and are served over MCP, so external
//! agents follow Quire's conventions (design §4.3).

/// One prompt argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prompt {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub args: &'static [PromptArg],
    /// The prompt, with `{arg}` placeholders.
    pub text: &'static str,
}

/// Every built-in prompt, in the order `prompts/list` shows them. Empty
/// until WS-D writes them.
pub const PROMPTS: &[Prompt] = &[];

pub fn get(name: &str) -> Option<&'static Prompt> {
    PROMPTS.iter().find(|p| p.name == name)
}

/// The prompt's text with `{arg}` placeholders filled in.
pub fn render(p: &Prompt, args: &[(&str, &str)]) -> String {
    args.iter().fold(p.text.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_placeholders() {
        let p = Prompt { name: "x", title: "X", description: "", args: &[], text: "Ingest {source} now" };
        assert_eq!(render(&p, &[("source", "raw/a.md")]), "Ingest raw/a.md now");
        assert!(get("quire_ingest").is_none());
    }
}
