use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::json;

fn request(value: Value) -> MessagesRequest {
    serde_json::from_value(value).unwrap()
}

fn client_search_request(messages: Value) -> MessagesRequest {
    request(json!({
        "model": "m",
        "tools": [
            {"name": "ToolSearch", "input_schema": {"type": "object"}},
            {
                "name": "SendMessage",
                "defer_loading": true,
                "input_schema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}}
                }
            },
            {"name": "Unused", "defer_loading": true, "input_schema": {"type": "object"}}
        ],
        "messages": messages
    }))
}

fn client_reference() -> Value {
    json!({"role": "user", "content": [{
        "type": "tool_result",
        "tool_use_id": "search",
        "content": [{"type": "tool_reference", "tool_name": "SendMessage"}]
    }]})
}

fn remove_send_message() -> Value {
    json!({"role": "system", "content": [{
        "type": "tool_removal",
        "tool": {"type": "tool_reference", "name": "SendMessage"}
    }]})
}

fn declarations(items: &[ResponseItem]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::AdditionalTools { tools, .. } => Some(tools[0]["tools"][0].clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn client_tool_search_activates_and_declares_only_referenced_tools_after_the_result() {
    let mut request = client_search_request(json!([
        {"role": "assistant", "content": [{
            "type": "tool_use", "id": "search", "name": "ToolSearch", "input": {}
        }]},
        {"role": "user", "content": [
            {"type": "text", "text": "before"},
            {"type": "tool_result", "tool_use_id": "search", "content": [
                {"type": "text", "text": "found"},
                {"type": "tool_reference", "tool_name": "SendMessage"},
                {"type": "tool_reference", "tool_name": "SendMessage"}
            ]},
            {"type": "text", "text": "after"}
        ]},
        {"role": "assistant", "content": [{
            "type": "tool_use", "id": "send", "name": "SendMessage",
            "input": {"message": "hello"}
        }]},
        {"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "send", "content": "sent"
        }]}
    ]));
    let plan = apply(&mut request, true).unwrap();
    assert_eq!(plan.initial.len(), 1);
    assert_eq!(plan.initial[0]["name"], "ToolSearch");
    assert_eq!(plan.active.len(), 2);
    assert_eq!(
        plan.active
            .iter()
            .find(|tool| tool["name"] == "SendMessage")
            .unwrap()["defer_loading"],
        false
    );

    let mut tools = ToolSet::from_request(&request.tools).unwrap();
    let initial = tools.subset(&plan.initial).unwrap();
    let active = tools.subset(&plan.active).unwrap();
    let initial_schemas =
        codex_tools::create_tools_json_for_responses_lite(&initial.specs).unwrap();
    assert_eq!(initial_schemas[0]["tools"].as_array().unwrap().len(), 1);
    let active_schemas = codex_tools::create_tools_json_for_responses_lite(&active.specs).unwrap();
    assert!(
        active_schemas[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| {
                tool["name"] == "SendMessage"
                    && tool["parameters"]["properties"]["message"]["type"] == "string"
            })
    );
    tools.limit_to(&active);
    assert!(tools.is_available(None, "SendMessage"));
    assert!(!tools.is_available(None, "Unused"));

    render(&mut request.messages, &tools, true).unwrap();
    let history = crate::history::conversation_items(
        request.messages,
        &tools,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(
        matches!(&history.items[0], ResponseItem::FunctionCall { name, .. } if name == "ToolSearch")
    );
    assert!(
        matches!(&history.items[1], ResponseItem::Message { role, content, .. }
        if role == "user" && content == &[ContentItem::InputText { text: "before".to_string() }])
    );
    assert!(
        matches!(&history.items[2], ResponseItem::FunctionCallOutput { call_id, .. }
        if call_id.as_deref() == Some("search"))
    );
    assert!(
        matches!(&history.items[3], ResponseItem::AdditionalTools { role, .. }
        if role == "developer")
    );
    assert!(
        matches!(&history.items[4], ResponseItem::Message { role, content, .. }
        if role == "user" && content == &[ContentItem::InputText { text: "after".to_string() }])
    );
    assert!(
        matches!(&history.items[5], ResponseItem::FunctionCall { name, .. } if name == "SendMessage")
    );
    let declarations = declarations(&history.items);
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0]["name"], "SendMessage");
    assert_eq!(
        declarations[0]["parameters"]["properties"]["message"]["type"],
        "string"
    );
}

#[test]
fn client_reference_activation_and_explicit_removal_follow_conversation_order() {
    for (messages, available) in [
        (json!([client_reference(), remove_send_message()]), false),
        (json!([remove_send_message(), client_reference()]), true),
        (
            json!([
                client_reference(),
                remove_send_message(),
                client_reference()
            ]),
            true,
        ),
        (
            json!([
                client_reference(),
                remove_send_message(),
                client_reference(),
                remove_send_message()
            ]),
            false,
        ),
    ] {
        for lite in [false, true] {
            let mut request = client_search_request(messages.clone());
            let plan = apply(&mut request, true).unwrap();
            let mut tools = ToolSet::from_request(&request.tools).unwrap();
            let active = tools.subset(&plan.active).unwrap();
            tools.limit_to(&active);
            assert_eq!(tools.is_available(None, "SendMessage"), available);
            assert!(!tools.is_available(None, "Unused"));
            assert_eq!(
                plan.active.iter().any(|tool| tool["name"] == "SendMessage"),
                available
            );
            render(&mut request.messages, &tools, lite).unwrap();
            crate::history::conversation_items(
                request.messages,
                &tools,
                &std::collections::HashMap::new(),
            )
            .unwrap();
        }
    }
}

#[test]
fn client_references_preserve_schema_versions_across_updates_and_removal() {
    let mut request = client_search_request(json!([
        client_reference(),
        {"role": "system", "content": [{
            "type": "tool_addition",
            "tool": {"type": "tool_definition", "definition": {
                "name": "SendMessage",
                "input_schema": {
                    "type": "object", "properties": {"recipient": {"type": "string"}}
                }
            }}
        }]},
        remove_send_message(),
        client_reference()
    ]));
    let plan = apply(&mut request, true).unwrap();
    let mut tools = ToolSet::from_request(&request.tools).unwrap();
    let active = tools.subset(&plan.active).unwrap();
    tools.limit_to(&active);
    assert!(tools.is_available(None, "SendMessage"));
    let schema = &plan
        .active
        .iter()
        .find(|tool| tool["name"] == "SendMessage")
        .unwrap()["input_schema"];
    assert!(schema["properties"].get("message").is_none());
    assert_eq!(schema["properties"]["recipient"]["type"], "string");

    render(&mut request.messages, &tools, true).unwrap();
    let history = crate::history::conversation_items(
        request.messages,
        &tools,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    let declarations = declarations(&history.items);
    assert_eq!(declarations.len(), 3);
    assert_eq!(
        declarations[0]["parameters"]["properties"]["message"]["type"],
        "string"
    );
    assert!(
        declarations[0]["parameters"]["properties"]
            .get("recipient")
            .is_none()
    );
    assert_eq!(declarations[1], declarations[2]);
    assert_eq!(
        declarations[2]["parameters"]["properties"]["recipient"]["type"],
        "string"
    );
    assert!(
        declarations[2]["parameters"]["properties"]
            .get("message")
            .is_none()
    );
}

#[test]
fn references_to_already_loaded_tools_do_not_append_declarations() {
    for (inline, deferred) in [(false, true), (true, false)] {
        let mut request = client_search_request(json!([client_reference(), client_reference()]));
        request.tools[1]["defer_loading"] = json!(deferred);
        let plan = apply(&mut request, inline).unwrap();
        let tools = ToolSet::from_request(&request.tools).unwrap();
        render(&mut request.messages, &tools, true).unwrap();
        let history = crate::history::conversation_items(
            request.messages,
            &tools,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(
            plan.initial
                .iter()
                .any(|tool| tool["name"] == "SendMessage")
        );
        assert!(declarations(&history.items).is_empty());
    }
}

#[test]
fn client_references_also_load_tools_from_a_server_search_catalog() {
    let mut request = client_search_request(json!([client_reference()]));
    request.tools.push(json!({
        "type": "tool_search_tool_regex_20251119", "name": "server_search"
    }));
    let plan = apply(&mut request, true).unwrap();
    let tools = ToolSet::from_request(&request.tools).unwrap();
    let initial = tools.subset(&plan.initial).unwrap();
    let active = tools.subset(&plan.active).unwrap();
    assert_eq!(initial.deferred.len(), 2);
    assert_eq!(active.deferred.len(), 1);
    let schemas = codex_tools::create_tools_json_for_responses_lite(&active.specs).unwrap();
    assert!(schemas[0]["tools"].as_array().unwrap().iter().any(|tool| {
        tool["name"] == "SendMessage"
            && tool["parameters"]["properties"]["message"]["type"] == "string"
    }));
    render(&mut request.messages, &tools, true).unwrap();
    let history = crate::history::conversation_items(
        request.messages,
        &tools,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert_eq!(declarations(&history.items).len(), 1);
}

#[test]
fn failed_client_tool_results_do_not_activate_references() {
    let mut result = client_reference();
    result["content"][0]["is_error"] = json!(true);
    let mut request = client_search_request(json!([result]));
    let plan = apply(&mut request, true).unwrap();
    assert_eq!(plan.initial, plan.active);
}

#[test]
fn client_references_require_a_definition_known_at_that_point() {
    let mut result = client_reference();
    result["content"][0]["content"][0]["tool_name"] = json!("missing");
    let mut request = client_search_request(json!([
        result,
        {"role": "system", "content": [{
            "type": "tool_addition",
            "tool": {"type": "tool_definition", "definition": {
                "name": "missing", "input_schema": {"type": "object"}
            }}
        }]}
    ]));
    let error = apply(&mut request, true)
        .err()
        .expect("unresolved reference");
    assert!(
        error
            .message()
            .contains("unresolved tool reference `missing`")
    );
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
