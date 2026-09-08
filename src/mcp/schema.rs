//! Making the generated tool schemas portable across MCP clients.
//!
//! `schemars` renders every `Option<T>` as `"type": ["string", "null"]`. That is legal JSON
//! Schema and says exactly what is meant, but a number of MCP clients read `type` as a
//! single string: they either refuse the tool or drop the constraint. The failure is the bad
//! kind -- nothing errors, the board is simply missing in that client, and the agent there
//! has no memory without anyone being told why.
//!
//! The equivalent single-type form is `anyOf` with one branch per type, which every client
//! reads. Rewriting on the way out keeps the Rust types honest (`Option<String>` stays an
//! `Option<String>`) instead of contorting eight parameter structs to satisfy other people's
//! parsers.

use rmcp::model::Tool;
use serde_json::{json, Value};

/// Every tool, with its schema made portable. This is what `list_tools` serves.
///
/// A function rather than a closure at the call site so a test can hold the before and the
/// after of the REAL registered tools, instead of re-deriving what the handler does and
/// proving only that the test agrees with itself.
pub fn portable_tools(tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            let mut schema = Value::Object((*tool.input_schema).clone());
            split_type_unions(&mut schema);
            if let Value::Object(map) = schema {
                tool.input_schema = std::sync::Arc::new(map);
            }
            tool
        })
        .collect()
}

/// Rewrites every `"type": [...]` array in a schema tree into single-type `anyOf` branches.
///
/// Walks the whole tree rather than a list of known field names. A parameter added later is
/// the case that matters: it inherits this without anyone remembering it exists, which is
/// the only version of this fix that stays true.
///
/// Sibling keywords stay where they are. A `format` or `minimum` next to the union applies
/// only to instances of the type it describes, so leaving them outside the `anyOf` neither
/// constrains `null` nor loses anything -- and hoisting them into one branch would be a
/// change of meaning made for cosmetic reasons.
pub fn split_type_unions(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if let Some(types) = map.get("type").and_then(Value::as_array).cloned() {
                // An `anyOf` already present was written deliberately; replacing it would
                // silently change what the tool accepts. Nothing here emits both today, so
                // this is a guard against a future generator rather than a live case.
                if !map.contains_key("anyOf") {
                    match types.len() {
                        // A one-element union is just that type. Emitting `anyOf` with a
                        // single branch would be correct and needlessly hard to read.
                        1 => {
                            map.insert("type".into(), types[0].clone());
                        }
                        _ => {
                            map.remove("type");
                            let branches =
                                types.into_iter().map(|t| json!({ "type": t })).collect();
                            map.insert("anyOf".into(), Value::Array(branches));
                        }
                    }
                }
            }
            for (_, child) in map.iter_mut() {
                split_type_unions(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                split_type_unions(item);
            }
        }
        _ => {}
    }
}

/// True when any `type` anywhere in the tree is still an array.
///
/// Exists for the test that asserts it of every registered tool, so a tool added later
/// cannot reintroduce the problem quietly.
pub fn has_type_union(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => {
            map.get("type").is_some_and(Value::is_array)
                || map.values().any(has_type_union)
        }
        Value::Array(items) => items.iter().any(has_type_union),
        _ => false,
    }
}
