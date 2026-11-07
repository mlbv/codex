use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn reasoning(tag: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content: None,
        encrypted_content: Some(tag.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: "Read".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn output(call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn edits(value: Value) -> Vec<ContextEdit> {
    serde_json::from_value(value).expect("edits")
}

#[test]
fn keep_all_thinking_is_a_no_op() {
    let mut items = vec![user("a"), reasoning("r1"), user("b"), reasoning("r2")];
    let before = items.clone();
    let applied = apply(
        &edits(json!([{ "type": "clear_thinking_20251015", "keep": "all" }])),
        &mut items,
        &ToolSet::default(),
    );
    assert_eq!((applied, items), (Vec::new(), before));
}

#[test]
fn thinking_outside_the_last_turns_is_dropped() {
    let mut items = vec![user("a"), reasoning("r1"), user("b"), reasoning("r2")];
    let applied = apply(
        &edits(json!([{ "type": "clear_thinking_20251015" }])),
        &mut items,
        &ToolSet::default(),
    );
    assert_eq!(items, vec![user("a"), user("b"), reasoning("r2")]);
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
}

#[test]
fn old_tool_results_clear_past_the_trigger() {
    let big = "x".repeat(400);
    let mut items = vec![
        call("c1"),
        output("c1", &big),
        call("c2"),
        output("c2", &big),
        call("c3"),
        output("c3", &big),
    ];
    let applied = apply(
        &edits(json!([{
            "type": "clear_tool_uses_20250919",
            "trigger": { "type": "input_tokens", "value": 10 },
            "keep": { "type": "tool_uses", "value": 1 },
        }])),
        &mut items,
        &ToolSet::default(),
    );
    assert_eq!(
        items,
        vec![
            call("c1"),
            output("c1", CLEARED_TOOL_RESULT),
            call("c2"),
            output("c2", CLEARED_TOOL_RESULT),
            call("c3"),
            output("c3", &big),
        ]
    );
    assert_eq!(applied[0]["cleared_tool_uses"], 2);
}
