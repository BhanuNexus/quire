// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reading JSON out of model replies, leniently: models wrap it in prose or
//! code fences, so take the first value that parses.

use serde_json::Value;

/// The first JSON value starting with `open` (`{` or `[`) that parses.
fn first(text: &str, open: char) -> Option<Value> {
    let mut from = 0;
    while let Some(i) = text[from..].find(open) {
        let at = from + i;
        let mut de = serde_json::Deserializer::from_str(&text[at..]).into_iter::<Value>();
        if let Some(Ok(v)) = de.next() {
            return Some(v);
        }
        from = at + open.len_utf8();
    }
    None
}

/// The first JSON object in `text`.
pub fn first_object(text: &str) -> Option<Value> {
    first(text, '{').filter(Value::is_object)
}

/// The first JSON array in `text`.
pub fn first_array(text: &str) -> Option<Vec<Value>> {
    match first(text, '[')? {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_json_in_prose() {
        assert_eq!(first_object("Here: {\"a\": 1} and {\"b\": 2}"), Some(serde_json::json!({"a": 1})));
        assert_eq!(first_object("{broken {\"ok\": true}"), Some(serde_json::json!({"ok": true})));
        assert_eq!(first_array("```json\n[1, 2]\n```"), Some(vec![serde_json::json!(1), serde_json::json!(2)]));
        assert_eq!(first_array("no json"), None);
        assert_eq!(first_object("[1]"), None);
    }
}
