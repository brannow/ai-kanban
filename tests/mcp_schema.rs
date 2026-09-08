//! The tool schemas as other MCP clients will read them.
//!
//! `schemars` renders `Option<T>` as `"type": ["string", "null"]`. It is correct JSON Schema
//! and several MCP clients still mishandle it -- they refuse the tool or drop the constraint.
//! Nothing errors when that happens: the board is simply absent in that client, and an agent
//! there works with no memory. That is the failure these tests exist to keep out.

use ai_kanban::mcp::schema::{has_type_union, portable_tools, split_type_unions};
use ai_kanban::mcp::server::AiKanban;
use serde_json::{json, Value};

fn schema_of(tool: &rmcp::model::Tool) -> Value {
    Value::Object((*tool.input_schema).clone())
}

#[test]
fn no_tool_goes_out_with_a_type_union() {
    // The REAL registered tools, not a fixture. A tool added later is the case that matters:
    // it must inherit this without anyone remembering the problem exists, and only running
    // the actual list can show that.
    let raw = AiKanban::tool_router().list_all();
    assert!(!raw.is_empty(), "no tools registered -- this test would pass vacuously");

    // The generator still emits unions. If this stops being true the rewrite has become
    // dead code, and a test asserting the absence of something nothing produces is worse
    // than no test: it passes forever and guards nothing.
    let offenders: Vec<&str> = raw.iter()
        .filter(|t| has_type_union(&schema_of(t)))
        .map(|t| t.name.as_ref())
        .collect();
    assert!(!offenders.is_empty(), "schemars no longer emits type unions; re-check the rewrite");

    for tool in portable_tools(raw) {
        assert!(!has_type_union(&schema_of(&tool)),
            "tool `{}` still carries a `type` array: {}", tool.name, schema_of(&tool));
    }
}

#[test]
fn the_rewrite_preserves_what_the_schema_meant() {
    // The point of the rewrite is portability, so it must not quietly change the contract:
    // the same values validate before and after, and the text an agent reads survives.
    let mut s = json!({
        "type": "object",
        "properties": {
            "project":   { "type": ["string", "null"], "description": "Which board." },
            "limit":     { "type": ["integer", "null"], "format": "uint32", "minimum": 0 },
            "title":     { "type": "string" },
            "already":   { "anyOf": [{ "type": "string" }], "description": "hand written" },
            "single":    { "type": ["string"] }
        },
        "required": ["title"]
    });
    split_type_unions(&mut s);
    let p = &s["properties"];

    assert_eq!(p["project"]["anyOf"], json!([{ "type": "string" }, { "type": "null" }]));
    assert!(p["project"].get("type").is_none(), "the union must be gone, not merely joined");
    assert_eq!(p["project"]["description"], "Which board.",
        "the description is what the agent reads to decide whether to pass the argument");

    // Sibling constraints stay put. `format` and `minimum` apply only to instances of the
    // type they describe, so outside the anyOf they neither constrain null nor go missing.
    assert_eq!(p["limit"]["anyOf"], json!([{ "type": "integer" }, { "type": "null" }]));
    assert_eq!(p["limit"]["format"], "uint32");
    assert_eq!(p["limit"]["minimum"], 0);

    // A plain type was never the problem and must come through untouched.
    assert_eq!(p["title"]["type"], "string");
    assert_eq!(s["type"], "object");
    assert_eq!(s["required"], json!(["title"]));

    // An existing anyOf was written deliberately; overwriting it would change what the tool
    // accepts for the sake of a rule it already satisfies.
    assert_eq!(p["already"]["anyOf"], json!([{ "type": "string" }]));
    assert_eq!(p["already"]["description"], "hand written");

    // A one-element union is just that type. Wrapping it in anyOf would be correct and
    // needlessly hard to read.
    assert_eq!(p["single"]["type"], "string");
}
