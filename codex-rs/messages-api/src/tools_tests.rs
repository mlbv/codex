use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("{name} tool"),
        "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
    })
}

#[test]
fn mcp_tools_join_codex_namespaces() {
    let set = ToolSet::from_request(&[
        tool("Read"),
        tool("mcp__intellij__search_text"),
        tool("mcp__intellij__analyze_calls"),
    ])
    .expect("tools");
    let names = set
        .specs
        .iter()
        .map(|spec| match spec {
            ToolSpec::Namespace(namespace) => format!(
                "{}[{}]",
                namespace.name,
                namespace
                    .tools
                    .iter()
                    .map(namespace_tool_name)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            other => other.name().to_string(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "Read".to_string(),
            "mcp__intellij__[analyze_calls,search_text]".to_string()
        ]
    );
    assert_eq!(
        set.anthropic_name(Some("mcp__intellij__"), "search_text"),
        "mcp__intellij__search_text"
    );
    assert_eq!(set.anthropic_name(Some("functions"), "Read"), "Read");
    assert_eq!(
        set.callable("mcp__intellij__search_text"),
        (
            Some("mcp__intellij__".to_string()),
            "search_text".to_string()
        )
    );
}

#[test]
fn web_search_server_tool_is_recorded() {
    let set = ToolSet::from_request(&[json!({
        "type": "web_search_20250305",
        "name": "web_search",
        "max_uses": 8,
        "allowed_domains": ["docs.rs"],
    })])
    .expect("tools");
    assert_eq!(set.specs.len(), 0);
    assert_eq!(
        set.web_search.map(|tool| tool.allowed_domains),
        Some(Some(vec!["docs.rs".to_string()]))
    );
}

#[test]
fn unsupported_server_tool_is_rejected() {
    let err = ToolSet::from_request(&[
        json!({ "type": "code_execution_20250825", "name": "code_execution" }),
    ])
    .expect_err("rejected");
    assert_eq!(
        err.body()["error"]["message"],
        "tools: tool type `code_execution_20250825` is not supported by the Codex backend"
    );
}

#[test]
fn anthropic_client_tools_become_functions() {
    let set = ToolSet::from_request(&[
        json!({ "type": "bash_20250124", "name": "bash" }),
        json!({ "type": "text_editor_20250728", "name": "str_replace_based_edit_tool", "max_characters": 10000 }),
        json!({ "type": "text_editor_20250124", "name": "str_replace_editor" }),
        json!({ "type": "memory_20250818", "name": "memory" }),
    ])
    .expect("tools");
    let properties = |name: &str| {
        let spec = set
            .specs
            .iter()
            .find(|spec| spec.name() == name)
            .expect("spec");
        let ToolSpec::Function(function) = spec else {
            panic!("function");
        };
        let schema = serde_json::to_value(&function.parameters).expect("schema");
        let mut properties = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        properties.sort();
        properties
    };
    assert_eq!(properties("bash"), vec!["command", "restart"]);
    assert!(properties("str_replace_based_edit_tool").contains(&"insert_text".to_string()));
    assert!(!properties("str_replace_editor").contains(&"insert_text".to_string()));
    assert_eq!(
        properties("memory"),
        vec![
            "command",
            "file_text",
            "insert_line",
            "insert_text",
            "new_path",
            "new_str",
            "old_path",
            "old_str",
            "path",
            "view_range",
        ]
    );
    assert_eq!(set.instructions.len(), 1);
    assert_eq!(set.callable("memory"), (None, "memory".to_string()));
}

#[test]
fn eager_input_streaming_is_tracked_per_tool() {
    let mut eager = tool("Write");
    eager["eager_input_streaming"] = json!(true);
    let set = ToolSet::from_request(&[eager, tool("Read")]).expect("tools");
    assert_eq!((set.is_eager("Write"), set.is_eager("Read")), (true, false));
}

#[test]
fn domain_allow_and_block_lists_are_exclusive() {
    let err = ToolSet::from_request(&[json!({
        "type": "web_search_20250305",
        "name": "web_search",
        "allowed_domains": ["docs.rs"],
        "blocked_domains": ["example.com"],
    })])
    .expect_err("rejected");
    assert_eq!(
        err.body()["error"]["message"],
        "tools.web_search: use either allowed_domains or blocked_domains, not both"
    );
}

#[test]
fn client_toolsets_become_namespaces_with_member_configs() {
    let set = ToolSet::from_request(&[
        json!({ "type": "computer_toolset_20260801" }),
        json!({ "type": "browser_toolset_20260801", "configs": { "javascript_exec": { "enabled": true }, "navigate": { "enabled": false } } }),
    ])
    .expect("tools");
    let members = |namespace: &str| {
        set.specs
            .iter()
            .find_map(|spec| match spec {
                ToolSpec::Namespace(candidate) if candidate.name == namespace => Some(
                    candidate
                        .tools
                        .iter()
                        .map(namespace_tool_name)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .expect("namespace")
    };
    assert_eq!(members("computer_toolset").len(), 17);
    let browser = members("browser_toolset");
    assert_eq!(browser.len(), 27);
    assert!(browser.contains(&"javascript_exec".to_string()));
    assert!(!browser.contains(&"navigate".to_string()));
    assert_eq!(set.toolset(Some("browser_toolset")), Some("browser"));
    assert_eq!(set.toolset(Some("mcp__x__")), None);
    assert_eq!(set.instructions.len(), 1);
}

#[test]
fn unknown_toolset_member_is_rejected() {
    let err = ToolSet::from_request(&[
        json!({ "type": "computer_toolset_20260801", "configs": { "teleport": { "enabled": true } } }),
    ])
    .expect_err("rejected");
    assert_eq!(
        err.body()["error"]["message"],
        "tools.computer_toolset_20260801.configs: unknown member `teleport`"
    );
}

#[test]
fn legacy_computer_tool_carries_the_display() {
    let set = ToolSet::from_request(&[json!({
        "type": "computer_20251124",
        "name": "computer",
        "display_width_px": 1280,
        "display_height_px": 800,
        "enable_zoom": true,
    })])
    .expect("tools");
    let [ToolSpec::Function(function)] = set.specs.as_slice() else {
        panic!("one function");
    };
    assert!(
        function
            .description
            .contains("The screen's resolution is 1280x800.")
    );
    let schema = serde_json::to_value(&function.parameters).expect("schema");
    assert!(
        schema["properties"]["action"]["enum"]
            .as_array()
            .expect("actions")
            .contains(&json!("zoom"))
    );
}

#[test]
fn deferred_tools_wait_for_tool_search() {
    let mut deferred = tool("get_weather");
    deferred["defer_loading"] = json!(true);
    let without_search = ToolSet::from_request(std::slice::from_ref(&deferred)).expect("tools");
    assert_eq!(
        (without_search.specs.len(), without_search.deferred.len()),
        (1, 0)
    );

    let with_search = ToolSet::from_request(&[
        json!({ "type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25" }),
        deferred,
        tool("Read"),
    ])
    .expect("tools");
    assert_eq!(
        (with_search.specs.len(), with_search.deferred.len()),
        (1, 1)
    );
    let search = with_search.tool_search.as_ref().expect("tool search");
    assert_eq!(
        (search.name.as_str(), search.regex),
        ("tool_search_tool_bm25", false)
    );
    assert_eq!(
        with_search.callable("get_weather"),
        (None, "get_weather".to_string())
    );
}
