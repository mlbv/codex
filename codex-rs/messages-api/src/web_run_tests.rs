use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::anthropic::WebSearchResult;

fn tools(definitions: Value) -> ToolSet {
    let Value::Array(definitions) = definitions else {
        panic!("tool definitions");
    };
    let mut tools = ToolSet::from_request(&definitions).expect("tools");
    tools.web_run = true;
    tools
}

#[test]
fn search_calls_echo_the_query_then_their_arguments() {
    let tools = tools(json!([{ "type": "web_search_20250305", "name": "web_search" }]));
    let arguments =
        r#"{"search_query":[{"q":"axum sse"},{"q":"tokio"}],"response_length":"short"}"#;
    let call = WebRunCall::classify("call_1".to_string(), arguments, &tools);
    assert_eq!(call.kind, ServerToolKind::Search);
    assert_eq!(
        call.input.to_string(),
        r#"{"query":"axum sse ...","search_query":[{"q":"axum sse"},{"q":"tokio"}],"response_length":"short"}"#
    );
    let ResponseItem::FunctionCall {
        namespace,
        name,
        arguments: replayed,
        call_id,
        ..
    } = replay_call("call_1".to_string(), WEB_SEARCH, call.input).expect("call")
    else {
        panic!("function call");
    };
    assert_eq!(
        (namespace.as_deref(), name.as_str(), call_id.as_str()),
        (Some("web"), "run", "call_1")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&replayed).expect("arguments"),
        serde_json::from_str::<Value>(arguments).expect("arguments")
    );
}

#[test]
fn one_url_open_is_a_fetch_when_web_fetch_is_enabled() {
    let both = tools(json!([
        { "type": "web_search_20250305", "name": "web_search" },
        { "type": "web_fetch_20250910", "name": "web_fetch" },
    ]));
    let open = r#"{"open":[{"ref_id":"https://docs.rs/axum"}]}"#;
    let call = WebRunCall::classify("call_1".to_string(), open, &both);
    assert_eq!(call.kind, ServerToolKind::Fetch);
    assert_eq!(call.input["url"], "https://docs.rs/axum");
    let by_reference = WebRunCall::classify(
        "call_2".to_string(),
        r#"{"open":[{"ref_id":"turn0search0"}]}"#,
        &both,
    );
    assert_eq!(by_reference.kind, ServerToolKind::Search);

    let fetch_only = tools(json!([{ "type": "web_fetch_20250910", "name": "web_fetch" }]));
    let search = WebRunCall::classify(
        "call_3".to_string(),
        r#"{"search_query":[{"q":"axum"}]}"#,
        &fetch_only,
    );
    let mut uses = ServerToolUses::default();
    let plans = plan(&[&search], &fetch_only, &mut uses, "");
    assert!(matches!(
        plans.as_slice(),
        [Plan::Refuse(_, "invalid_tool_input", _)]
    ));
}

#[test]
fn max_uses_refuses_calls_past_the_limit() {
    let tools =
        tools(json!([{ "type": "web_search_20250305", "name": "web_search", "max_uses": 1 }]));
    let calls = ["call_1", "call_2"]
        .map(|id| WebRunCall::classify(id.to_string(), r#"{"search_query":[{"q":"x"}]}"#, &tools));
    let mut uses = ServerToolUses::default();
    let plans = plan(&calls.iter().collect::<Vec<_>>(), &tools, &mut uses, "");
    let [Plan::Run(..), Plan::Refuse(call, code, message)] = plans.as_slice() else {
        panic!("one run, one refusal");
    };
    assert_eq!(uses.search, 1);
    let result = refusal(call, code, message);
    assert_eq!(
        result.block,
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": "call_2",
            "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" },
        })
    );
}

#[test]
fn first_search_result_carries_the_output() {
    let results = vec![
        json!({ "type": "text_result", "ref_id": "turn0search0", "title": "Axum", "url": "https://docs.rs/axum" }),
        json!({ "type": "text_result", "ref_id": "turn0search1", "url": "https://tokio.rs" }),
        json!({ "type": "time_result", "utc_offset": "+08:00" }),
    ];
    let blocks = search_results(&results, "citeturn0search0 output");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1]["title"], "https://tokio.rs");
    assert_eq!(blocks[1]["encrypted_content"], "turn0search1");
    let echoed =
        serde_json::from_value::<Vec<WebSearchResult>>(Value::Array(blocks)).expect("results");
    assert_eq!(
        replay_search_output(
            "call_1".to_string(),
            WebSearchResultContent::Results(echoed)
        ),
        output_item("call_1", "citeturn0search0 output".to_string())
    );
}

#[test]
fn foreign_search_calls_become_web_run_queries() {
    let ResponseItem::FunctionCall { arguments, .. } = replay_call(
        "srvtoolu_1".to_string(),
        WEB_SEARCH,
        json!({ "query": "claude shannon" }),
    )
    .expect("call") else {
        panic!("function call");
    };
    assert_eq!(arguments, r#"{"search_query":[{"q":"claude shannon"}]}"#);
    let foreign = vec![WebSearchResult {
        url: "https://en.wikipedia.org/wiki/Claude_Shannon".to_string(),
        title: "Claude Shannon".to_string(),
        encrypted_content: Some("EqgfCioIARgB".to_string()),
    }];
    assert_eq!(
        replay_search_output(
            "srvtoolu_1".to_string(),
            WebSearchResultContent::Results(foreign)
        ),
        output_item(
            "srvtoolu_1",
            "Claude Shannon (https://en.wikipedia.org/wiki/Claude_Shannon)".to_string()
        )
    );
}
