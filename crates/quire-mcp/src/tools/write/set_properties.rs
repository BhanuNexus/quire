// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `set_properties`, which sets front-matter properties.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "set_properties",
        "Set properties",
        "Set or remove front-matter properties. Values may be strings, numbers, booleans, lists or nested objects; null removes. Doing it twice changes nothing. Example: {\"path\": \"Rope Notes.md\", \"set\": {\"status\": \"done\", \"tags\": [\"cs\", \"ropes\"]}, \"remove\": [\"draft\"]}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "set": {"type": "object"}, "remove": {"type": "array", "items": {"type": "string"}},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path"]}),
        o(),
        Annotations::WRITE_IDEMPOTENT,
        Access::Write,
        set_properties,
    )
}

fn set_properties(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let mut set = Vec::new();
    let mut remove = list(a, "remove");
    if let Some(Value::Object(m)) = a.get("set") {
        for (k, v) in m {
            match fm_value(v) {
                Some(v) => set.push((k.clone(), v)),
                None => remove.push(k.clone()),
            }
        }
    }
    if set.is_empty() && remove.is_empty() {
        return ToolOutput::error("invalid_args", "Give `set` (an object) or `remove` (a list of keys).");
    }
    edit(cx, ext, a, "set_properties", &EditOp::Properties { set, remove })
}

/// A JSON value as a front-matter value (`None` for null: remove).
fn fm_value(v: &Value) -> Option<FmValue> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(FmValue::Str(s.clone())),
        Value::Bool(b) => Some(FmValue::Str(b.to_string())),
        Value::Number(n) => Some(FmValue::Str(n.to_string())),
        Value::Array(items) => Some(FmValue::List(
            items
                .iter()
                .map(|i| match i {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        )),
        Value::Object(m) => Some(FmValue::Map(m.iter().filter_map(|(k, v)| Some((k.clone(), fm_value(v)?))).collect())),
    }
}
