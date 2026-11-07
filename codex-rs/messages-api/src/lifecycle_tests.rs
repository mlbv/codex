use super::*;
use serde_json::json;

fn request(value: Value) -> MessagesRequest {
    serde_json::from_value(value).unwrap()
}

#[test]
fn inline_definitions_replace_schemas_and_removed_tools_cannot_run() {
    let mut request = request(
        json!({"model":"m", "tools":[{"name":"Read", "input_schema":{"type":"object"}}], "messages":[
            {"role":"system", "content":[{"type":"tool_removal", "tool":{"type":"tool_reference", "name":"Read"}}, {"type":"tool_addition", "tool":{"type":"tool_definition", "definition":{"name":"Write", "input_schema":{"type":"object", "properties":{"path":{"type":"string"}}}}}}]}
        ]}),
    );
    let plan = apply(&mut request, true).unwrap();
    let mut tools = ToolSet::from_request(&request.tools).unwrap();
    let active = tools.subset(&plan.active).unwrap();
    tools.limit_to(&active);
    assert!(!tools.is_available(None, "Read"));
    assert!(tools.is_available(None, "Write"));
    assert_eq!(plan.initial[0]["name"], "Read");
    assert_eq!(plan.active.len(), 1);
    render(&mut request.messages, &tools, true).unwrap();
    let history = crate::history::conversation_items(
        request.messages,
        &tools,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(
        matches!(&history.items[1], codex_protocol::models::ResponseItem::AdditionalTools { tools, .. } if serde_json::to_string(tools).unwrap().contains("path"))
    );
}

#[test]
fn lite_schema_updates_preserve_earlier_declarations() {
    let mut request = request(json!({"model":"m", "tools":[], "messages":[
        {"role":"system", "content":[{"type":"tool_addition", "tool":{"type":"tool_definition", "definition":{"name":"Read", "input_schema":{"type":"object", "properties":{"old":{"type":"string"}}}}}}]},
        {"role":"assistant", "content":"ok"},
        {"role":"system", "content":[{"type":"tool_addition", "tool":{"type":"tool_definition", "definition":{"name":"Read", "input_schema":{"type":"object", "properties":{"new":{"type":"boolean"}}}}}}]}
    ]}));
    let plan = apply(&mut request, true).unwrap();
    let tools = ToolSet::from_request(&request.tools).unwrap();
    render(&mut request.messages, &tools, true).unwrap();
    let history = crate::history::conversation_items(
        request.messages,
        &tools,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    let first = serde_json::to_string(&history.items[0]).unwrap();
    let last = serde_json::to_string(history.items.last().unwrap()).unwrap();
    assert!(first.contains("old") && !first.contains("new"));
    assert!(last.contains("new") && !last.contains("old"));
    assert!(plan.initial.is_empty());
    assert!(
        plan.active[0]["input_schema"]["properties"]
            .get("new")
            .is_some()
    );
}

#[test]
fn deferred_references_and_mcp_member_changes_resolve_in_order() {
    let mut request = request(json!({"model":"m", "tools":[
        {"name":"Read", "defer_loading":true, "input_schema":{"type":"object"}},
        {"type":"mcp_toolset", "mcp_server_name":"docs"}
    ], "messages":[{"role":"system", "content":[
        {"type":"tool_addition", "tool":{"type":"tool_reference", "name":"Read"}},
        {"type":"tool_removal", "tool":{"type":"mcp_tool_reference", "server_name":"docs", "name":"delete"}}
    ]}]}));
    let plan = apply(&mut request, true).unwrap();
    assert_eq!(plan.initial.len(), 1);
    assert_eq!(
        plan.active
            .iter()
            .find(|tool| tool["name"] == "Read")
            .unwrap()["defer_loading"],
        false
    );
    assert_eq!(
        plan.active
            .iter()
            .find(|tool| tool["type"] == "mcp_toolset")
            .unwrap()["configs"]["delete"]["enabled"],
        false
    );
    let mut invalid = request;
    invalid.messages = serde_json::from_value(json!([{"role":"system", "content":[{"type":"tool_addition", "tool":{"type":"tool_reference", "name":"missing"}}]}])).unwrap();
    assert!(apply(&mut invalid, true).is_err());
}
