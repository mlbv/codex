use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

async fn run(
    events: Vec<ResponseEvent>,
    stop_sequences: Vec<String>,
) -> (Vec<Value>, Vec<ResponseItem>) {
    run_with(&ToolSet::default(), events, stop_sequences).await
}

async fn run_with(
    tools: &ToolSet,
    events: Vec<ResponseEvent>,
    stop_sequences: Vec<String>,
) -> (Vec<Value>, Vec<ResponseItem>) {
    let (tx, mut rx) = mpsc::channel(256);
    let mut stream = MessageStream::new(
        &tx,
        tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ true,
        stop_sequences,
        Vec::new(),
    );
    stream.estimate_input(1000);
    for event in events {
        match stream.on_event(event).await.expect("send") {
            Flow::Continue | Flow::Completed { .. } => {}
            Flow::StopSequence | Flow::OutputLimit => break,
        }
    }
    stream.finish(/*refusal*/ None).await.expect("finish");
    let outputs = stream.outputs.clone();
    drop(stream);
    drop(tx);
    let mut sent = Vec::new();
    while let Some(event) = rx.recv().await {
        sent.push(event);
    }
    (sent, outputs)
}

fn without_message_start(events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .filter(|event| event["type"] != "message_start")
        .collect()
}

#[tokio::test]
async fn text_and_tool_call_stream_as_blocks() {
    let message = ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server("msg_1".to_string())),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: "Hi".to_string(),
        }],
        phase: Some(MessagePhase::Commentary),
        internal_chat_message_metadata_passthrough: None,
    };
    let call = ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server("fc_1".to_string())),
        name: "Bash".to_string(),
        namespace: None,
        arguments: r#"{"command":"ls"}"#.to_string(),
        encrypted_function_args: None,
        call_id: "call_1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let (events, outputs) = run(
        vec![
            ResponseEvent::OutputItemAdded(message.clone()),
            ResponseEvent::OutputTextDelta("H".to_string()),
            ResponseEvent::OutputTextDelta("i".to_string()),
            ResponseEvent::OutputItemDone(message),
            ResponseEvent::OutputItemAdded(call.clone()),
            ResponseEvent::OutputItemDone(call),
            ResponseEvent::Completed {
                response_id: "resp_1".to_string(),
                token_usage: Some(TokenUsage {
                    input_tokens: 100,
                    cached_input_tokens: 80,
                    output_tokens: 7,
                    ..Default::default()
                }),
                usage_metadata: None,
                end_turn: Some(true),
            },
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(
        without_message_start(events),
        vec![
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "H" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "i" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "call_1", "name": "Bash", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}" } }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                "usage": {
                    "input_tokens": 20,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 80,
                    "output_tokens": 7,
                },
            }),
            json!({ "type": "message_stop" }),
        ]
    );
    assert_eq!(
        outputs
            .iter()
            .map(ResponseItem::turn_id)
            .collect::<Vec<_>>(),
        vec![Some("turn-1"), Some("turn-1")]
    );
}

#[tokio::test]
async fn usage_starts_estimated_and_never_ends_on_a_zero_count() {
    let usage = |events: &[Value], kind: &str| {
        events
            .iter()
            .find(|event| event["type"] == kind)
            .map(|event| match kind {
                "message_start" => event["message"]["usage"].clone(),
                _ => event["usage"].clone(),
            })
    };
    let (cached, _) = run(
        vec![ResponseEvent::Completed {
            response_id: "resp_1".to_string(),
            token_usage: Some(TokenUsage {
                input_tokens: 2048,
                cached_input_tokens: 2048,
                output_tokens: 3,
                ..Default::default()
            }),
            usage_metadata: None,
            end_turn: Some(true),
        }],
        Vec::new(),
    )
    .await;
    let (unreported, _) = run(Vec::new(), Vec::new()).await;
    let counts = |input: i64, reads: i64, output: i64| {
        Some(json!({
            "input_tokens": input,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": reads,
            "output_tokens": output,
        }))
    };
    assert_eq!(
        (
            usage(&cached, "message_start"),
            usage(&cached, "message_delta"),
            usage(&unreported, "message_delta"),
        ),
        (counts(1000, 0, 0), counts(1, 2047, 3), counts(1000, 0, 0))
    );
}

#[tokio::test]
async fn reasoning_signature_carries_the_recorded_item() {
    let reasoning = ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server("rs_1".to_string())),
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "plan".to_string(),
        }],
        content: None,
        encrypted_content: Some("opaque".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let (events, outputs) = run(
        vec![
            ResponseEvent::OutputItemAdded(reasoning.clone()),
            ResponseEvent::ReasoningSummaryDelta {
                delta: "pl".to_string(),
                summary_index: 0,
            },
            ResponseEvent::ReasoningSummaryDelta {
                delta: "an".to_string(),
                summary_index: 1,
            },
            ResponseEvent::OutputItemDone(reasoning),
        ],
        Vec::new(),
    )
    .await;
    let signature = events
        .iter()
        .find(|event| event["delta"]["type"] == "signature_delta")
        .and_then(|event| event["delta"]["signature"].as_str())
        .expect("signature");
    assert_eq!(codec::decode_signature(signature).as_ref(), outputs.first());
    let thinking = events
        .iter()
        .filter(|event| event["delta"]["type"] == "thinking_delta")
        .map(|event| event["delta"]["thinking"].as_str().unwrap_or_default())
        .collect::<String>();
    assert_eq!(thinking, "pl\n\nan");
}

#[tokio::test]
async fn stop_sequence_truncates_and_records_visible_text() {
    let (events, outputs) = run(
        vec![ResponseEvent::OutputTextDelta("one END two".to_string())],
        vec!["END".to_string()],
    )
    .await;
    let text = events
        .iter()
        .filter(|event| event["delta"]["type"] == "text_delta")
        .map(|event| event["delta"]["text"].as_str().unwrap_or_default())
        .collect::<String>();
    assert_eq!(text, "one ");
    let stop = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .map(|event| event["delta"].clone());
    assert_eq!(
        stop,
        Some(json!({ "stop_reason": "stop_sequence", "stop_sequence": "END" }))
    );
    assert_eq!(outputs.len(), 1);
}

#[tokio::test]
async fn web_search_call_streams_server_tool_blocks() {
    let search = ResponseItem::WebSearchCall {
        encrypted_content: None,
        id: Some(ResponseItemId::from_server("ws_1".to_string())),
        status: Some("completed".to_string()),
        action: Some(WebSearchAction::Search {
            query: Some("axum sse".to_string()),
            queries: None,
        }),
        internal_chat_message_metadata_passthrough: None,
    };
    let (events, _) = run(
        vec![
            ResponseEvent::OutputItemAdded(search.clone()),
            ResponseEvent::OutputItemDone(search),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(
        without_message_start(events)[..5].to_vec(),
        vec![
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "ws_1", "name": "web_search", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"axum sse\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "web_search_tool_result", "tool_use_id": "ws_1", "content": [] } }),
            json!({ "type": "content_block_stop", "index": 1 }),
        ]
    );
}

fn function_call(id: &str, namespace: Option<&str>, name: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server(id.to_string())),
        name: name.to_string(),
        namespace: namespace.map(str::to_string),
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: format!("call_{id}"),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn eager_tool_input_streams_as_generated() {
    let tools = ToolSet::from_request(&[json!({
        "name": "Write",
        "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
        "eager_input_streaming": true,
    })])
    .expect("tools");
    let added = function_call("fc_1", None, "Write", "");
    let done = function_call("fc_1", None, "Write", r#"{"path":"a"}"#);
    let (events, _) = run_with(
        &tools,
        vec![
            ResponseEvent::OutputItemAdded(added),
            ResponseEvent::FunctionCallArgumentsDelta {
                item_id: "fc_1".to_string(),
                delta: r#"{"pa"#.to_string(),
            },
            ResponseEvent::FunctionCallArgumentsDelta {
                item_id: "fc_1".to_string(),
                delta: r#"th":"a"}"#.to_string(),
            },
            ResponseEvent::OutputItemDone(done),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(
        without_message_start(events)[..4].to_vec(),
        vec![
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "call_fc_1", "name": "Write", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"pa" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "th\":\"a\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
        ]
    );
}

#[tokio::test]
async fn web_run_calls_stream_as_server_tool_blocks() {
    let mut tools =
        ToolSet::from_request(&[json!({ "type": "web_search_20250305", "name": "web_search" })])
            .expect("tools");
    tools.web_run = true;
    let call = function_call(
        "fc_1",
        Some("web"),
        "run",
        r#"{"search_query":[{"q":"axum"}]}"#,
    );
    let (tx, mut rx) = mpsc::channel(64);
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ true,
        Vec::new(),
        Vec::new(),
    );
    for event in [
        ResponseEvent::OutputItemAdded(call.clone()),
        ResponseEvent::OutputItemDone(call),
    ] {
        stream.on_event(event).await.expect("send");
    }
    let calls = stream.take_server_calls();
    assert_eq!(stream.tool_calls().len(), 0);
    let mut uses = ServerToolUses::default();
    let web_calls = calls
        .iter()
        .filter_map(|call| match call {
            ServerCall::WebRun(call) => Some(call.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let plans = crate::web_run::plan(&web_calls, &tools, &mut uses, "");
    let [crate::web_run::Plan::Run(..)] = plans.as_slice() else {
        panic!("planned run");
    };
    stream.server_tool_uses = uses;
    let result = ServerResult {
        block: json!({ "type": "web_search_tool_result", "tool_use_id": "call_fc_1", "content": [] }),
        output: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call_fc_1".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(String::new()),
            internal_chat_message_metadata_passthrough: None,
        },
        urls: Vec::new(),
        citations: Vec::new(),
        failed: false,
    };
    stream.server_result(result).await.expect("send");
    stream.finish(/*refusal*/ None).await.expect("finish");
    assert_eq!(stream.outputs.len(), 2);
    drop(stream);
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let events = without_message_start(events);
    assert_eq!(
        events[..5].to_vec(),
        vec![
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "call_fc_1", "name": "web_search", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"axum\",\"search_query\":[{\"q\":\"axum\"}]}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "web_search_tool_result", "tool_use_id": "call_fc_1", "content": [] } }),
            json!({ "type": "content_block_stop", "index": 1 }),
        ]
    );
    assert_eq!(
        events[5]["usage"]["server_tool_use"],
        json!({ "web_search_requests": 1 })
    );
    assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn output_limit_ends_with_max_tokens_and_keeps_visible_text() {
    let (events, outputs) = run(
        vec![
            ResponseEvent::OutputTextDelta("partial answ".to_string()),
            messages_response_event(Err(CodexErr::new(CodexErrorDetails::ResponseIncomplete(
                Box::new(codex_protocol::error::ResponseIncompleteError {
                    response_id: Some("resp_1".to_string()),
                    token_usage: Some(TokenUsage {
                        input_tokens: 10,
                        output_tokens: 4,
                        ..Default::default()
                    }),
                    reason: "max_output_tokens".to_string(),
                }),
            ))))
            .expect("Messages API output-limit stop"),
        ],
        Vec::new(),
    )
    .await;
    let delta = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    assert_eq!(delta["usage"]["output_tokens"], 4);
    assert_eq!(
        outputs.iter().map(output_text_of).collect::<Vec<_>>(),
        vec!["partial answ".to_string()]
    );
}

fn output_text_of(item: &ResponseItem) -> String {
    match item {
        ResponseItem::Message { content, .. } => output_text(content),
        _ => String::new(),
    }
}

#[tokio::test]
async fn links_to_search_results_are_cited() {
    let tools =
        ToolSet::from_request(&[json!({ "type": "web_search_20250305", "name": "web_search" })])
            .expect("tools");
    let (tx, mut rx) = mpsc::channel(64);
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ true,
        Vec::new(),
        Vec::new(),
    );
    let citation = json!({ "type": "web_search_result_location", "url": "https://docs.rs/axum", "title": "axum", "encrypted_index": "turn0search0", "cited_text": "axum" });
    stream.cite(vec![("https://docs.rs/axum".to_string(), citation.clone())]);
    for delta in [
        "See [axum](https://docs",
        ".rs/axum) and [other](https://example.com).",
    ] {
        stream
            .on_event(ResponseEvent::OutputTextDelta(delta.to_string()))
            .await
            .expect("send");
    }
    stream.finish(/*refusal*/ None).await.expect("finish");
    drop(stream);
    drop(tx);
    let mut citations = Vec::new();
    while let Some(event) = rx.recv().await {
        if event["delta"]["type"] == "citations_delta" {
            citations.push(event["delta"]["citation"].clone());
        }
    }
    assert_eq!(citations, vec![citation]);
}

#[tokio::test]
async fn mcp_and_toolset_calls_stream_as_their_blocks() {
    let mut tools =
        ToolSet::from_request(&[json!({ "type": "computer_toolset_20260801" })]).expect("tools");
    tools
        .mcp_namespaces
        .insert("mcp__docs__".to_string(), "docs".to_string());
    let (events, _) = run_with(
        &tools,
        vec![
            ResponseEvent::OutputItemDone(function_call(
                "fc_1",
                Some("mcp__docs__"),
                "search",
                r#"{"q":"x"}"#,
            )),
            ResponseEvent::OutputItemDone(function_call(
                "fc_2",
                Some("computer_toolset"),
                "screenshot",
                "{}",
            )),
        ],
        Vec::new(),
    )
    .await;
    let starts = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| event["content_block"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![
            json!({ "type": "mcp_tool_use", "id": "call_fc_1", "name": "search", "server_name": "docs", "input": {} }),
            json!({ "type": "tool_use", "id": "call_fc_2", "name": "screenshot", "toolset_name": "computer", "input": {} }),
        ]
    );
}

#[tokio::test]
async fn advisor_calls_stream_as_server_tool_use() {
    let tools = ToolSet::from_request(&[
        json!({ "type": "advisor_20260301", "name": "advisor", "model": "gpt-6-astra", "max_uses": 2 }),
    ])
    .expect("tools");
    let mut tools = tools;
    tools.web_run = false;
    let (tx, mut rx) = mpsc::channel(64);
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ true,
        Vec::new(),
        Vec::new(),
    );
    stream
        .on_event(ResponseEvent::OutputItemDone(function_call(
            "fc_1", None, "advisor", "{}",
        )))
        .await
        .expect("send");
    let calls = stream.take_server_calls();
    assert!(
        matches!(calls.as_slice(), [ServerCall::Advisor { call_id }] if call_id == "call_fc_1")
    );
    assert!(stream.tool_calls().is_empty());
    drop(stream);
    drop(tx);
    let mut starts = Vec::new();
    while let Some(event) = rx.recv().await {
        if event["type"] == "content_block_start" {
            starts.push(event["content_block"].clone());
        }
    }
    assert_eq!(
        starts,
        vec![
            json!({ "type": "server_tool_use", "id": "call_fc_1", "name": "advisor", "input": {} })
        ]
    );
}

#[tokio::test]
async fn citation_markers_become_citations_and_leave_the_text() {
    let tools =
        ToolSet::from_request(&[json!({ "type": "web_search_20250305", "name": "web_search" })])
            .expect("tools");
    let (tx, mut rx) = mpsc::channel(64);
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ true,
        Vec::new(),
        Vec::new(),
    );
    let citation = json!({ "type": "web_search_result_location", "url": "https://github.com/x/releases", "title": "Releases", "encrypted_index": "turn0search1", "cited_text": "X5846" });
    stream.cite(vec![(
        "https://github.com/x/releases".to_string(),
        citation.clone(),
    )]);
    let marked = "Newest: X5846. \u{e200}cite\u{e202}turn0search1\u{e202}turn9search9\u{e201}";
    for delta in [
        "Newest: X5846. \u{e200}ci",
        "te\u{e202}turn0search1\u{e202}turn9search9\u{e201}",
    ] {
        stream
            .on_event(ResponseEvent::OutputTextDelta(delta.to_string()))
            .await
            .expect("send");
    }
    stream
        .on_event(ResponseEvent::OutputItemDone(ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: marked.to_string(),
                annotations: None,
                logprobs: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }))
        .await
        .expect("send");
    stream.finish(/*refusal*/ None).await.expect("finish");
    let recorded = stream
        .outputs
        .iter()
        .map(output_text_of)
        .collect::<Vec<_>>();
    drop(stream);
    drop(tx);
    let (mut text, mut citations) = (String::new(), Vec::new());
    while let Some(event) = rx.recv().await {
        match event["delta"]["type"].as_str() {
            Some("text_delta") => {
                text.push_str(event["delta"]["text"].as_str().unwrap_or_default())
            }
            Some("citations_delta") => citations.push(event["delta"]["citation"].clone()),
            _ => {}
        }
    }
    assert_eq!(text, "Newest: X5846. ");
    assert_eq!(citations, vec![citation]);
    assert_eq!(recorded, vec![marked.to_string()]);
}
