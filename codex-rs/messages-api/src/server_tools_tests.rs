use codex_core::external_harness::ExternalToolSearch;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::anthropic::ToolReference;

fn tools_with_search() -> ToolSet {
    let mut tools = ToolSet::from_request(&[
        json!({ "type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex" }),
        json!({
            "name": "get_weather",
            "description": "Get the weather forecast for a city",
            "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } },
            "defer_loading": true,
        }),
        json!({
            "name": "search_files",
            "description": "Search files in the workspace",
            "input_schema": { "type": "object", "properties": { "query": { "type": "string" } } },
            "defer_loading": true,
        }),
    ])
    .expect("tools");
    tools.tool_search_index = Some(ToolSearchIndex(ExternalToolSearch::new(
        tools.deferred.clone(),
    )));
    tools
}

#[test]
fn tool_search_answers_with_references_and_replays_exactly() {
    let tools = tools_with_search();
    let Some(index) = &tools.tool_search_index else {
        panic!("index");
    };
    let result = tool_search_result(
        index,
        &tools,
        "call_1",
        json!({ "query": "weather forecast" }),
    );
    assert_eq!(
        result.block["content"]["tool_references"],
        json!([{ "type": "tool_reference", "tool_name": "get_weather" }])
    );
    let replayed = replay_tool_search_output(
        "call_1",
        ToolSearchResultContent::ToolSearchToolSearchResult {
            tool_references: vec![ToolReference {
                tool_name: "get_weather".to_string(),
            }],
        },
        &tools,
    );
    assert_eq!(replayed, result.output);
    let tool = tools.tool_search.as_ref().expect("tool search");
    assert_eq!(
        tool_search_input(tool, &json!({ "query": "weather", "limit": 3 })),
        json!({ "pattern": "weather", "limit": 3 })
    );
}

#[test]
fn empty_tool_search_is_an_error_result_with_no_tools() {
    let tools = tools_with_search();
    let Some(index) = &tools.tool_search_index else {
        panic!("index");
    };
    let result = tool_search_result(index, &tools, "call_1", json!({ "query": " " }));
    assert_eq!(result.block["content"]["error_code"], "invalid_tool_input");
    let ResponseItem::ToolSearchOutput { tools: loaded, .. } = result.output else {
        panic!("tool search output");
    };
    assert!(loaded.is_empty());
}

#[test]
fn mcp_results_map_both_ways() {
    let result = CallToolResult {
        content: vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "image", "mimeType": "image/png", "data": "iVBOR" }),
        ],
        structured_content: None,
        is_error: None,
        meta: None,
    };
    let mapped = mcp_result("mcptoolu_1", &result);
    assert_eq!(
        mapped.block,
        json!({
            "type": "mcp_tool_result",
            "tool_use_id": "mcptoolu_1",
            "is_error": false,
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBOR" } },
            ],
        })
    );
    let echoed: Vec<ToolResultBlock> =
        serde_json::from_value(mapped.block["content"].clone()).expect("blocks");
    assert_eq!(
        replay_mcp_output(
            "mcptoolu_1".to_string(),
            false,
            Some(ToolResultContent::Blocks(echoed))
        ),
        mapped.output
    );
}
