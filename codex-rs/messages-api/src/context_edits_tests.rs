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
    let mut conversation = Conversation {
        items: vec![user("a"), reasoning("r1"), user("b"), reasoning("r2")],
        turn_starts: vec![0, 2],
        compaction: None,
    };
    let before = conversation.items.clone();
    let applied = apply(
        &edits(json!([{ "type": "clear_thinking_20251015", "keep": "all" }])),
        &mut conversation,
        &ToolSet::default(),
    );
    assert_eq!((applied, conversation.items), (Vec::new(), before));
}

#[test]
fn thinking_outside_the_last_turns_is_dropped() {
    let mut conversation = Conversation {
        items: vec![user("a"), reasoning("r1"), user("b"), reasoning("r2")],
        turn_starts: vec![0, 2],
        compaction: None,
    };
    let applied = apply(
        &edits(json!([{ "type": "clear_thinking_20251015" }])),
        &mut conversation,
        &ToolSet::default(),
    );
    assert_eq!(
        conversation.items,
        vec![user("a"), user("b"), reasoning("r2")]
    );
    assert_eq!(conversation.turn_starts, vec![0, 1]);
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
}

fn request_conversation(messages: Value) -> Conversation {
    crate::history::conversation_items(
        serde_json::from_value(messages).unwrap(),
        &ToolSet::default(),
        &std::collections::HashMap::new(),
    )
    .unwrap()
}

fn thinking(tag: &str) -> Value {
    json!({
        "type": "thinking",
        "thinking": "",
        "signature": crate::codec::encode_signature(&reasoning(tag)).unwrap(),
    })
}

fn clear(keep: u64) -> Vec<ContextEdit> {
    edits(json!([{
        "type": "clear_thinking_20251015",
        "keep": { "type": "thinking_turns", "value": keep },
    }]))
}

#[test]
fn interleaved_tool_results_and_system_controls_do_not_end_the_turn() {
    let mut conversation = request_conversation(json!([
        { "role": "user", "content": "go" },
        { "role": "assistant", "content": [
            thinking("r1"),
            { "type": "tool_use", "id": "c1", "name": "Read", "input": {} },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "c1", "content": "one" },
        ] },
        { "role": "system", "content": "continue" },
        { "role": "assistant", "content": [
            thinking("r2"),
            { "type": "tool_use", "id": "c2", "name": "Read", "input": {} },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "c2", "content": "two" },
        ] },
    ]));
    let before = conversation.items.clone();
    let applied = apply(&clear(1), &mut conversation, &ToolSet::default());
    assert_eq!((applied, conversation.items), (Vec::new(), before));
}

#[test]
fn turns_without_thinking_and_the_pending_turn_count_toward_keep() {
    let mut conversation = request_conversation(json!([
        { "role": "user", "content": "first" },
        { "role": "assistant", "content": [thinking("r1")] },
        { "role": "user", "content": "second" },
        { "role": "assistant", "content": "done" },
        { "role": "user", "content": [] },
    ]));
    let applied = apply(&clear(2), &mut conversation, &ToolSet::default());
    assert!(
        !conversation
            .items
            .iter()
            .any(|item| matches!(item, ResponseItem::Reasoning { .. }))
    );
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
}

#[test]
fn mixed_user_content_starts_exactly_one_turn() {
    let mut conversation = request_conversation(json!([
        { "role": "user", "content": "first" },
        { "role": "assistant", "content": [
            thinking("r1"),
            { "type": "tool_use", "id": "c1", "name": "Read", "input": {} },
        ] },
        { "role": "user", "content": [
            { "type": "text", "text": "before" },
            { "type": "tool_result", "tool_use_id": "c1", "content": "one" },
            { "type": "text", "text": "after" },
        ] },
    ]));
    let before = conversation.items.clone();
    assert!(apply(&clear(2), &mut conversation, &ToolSet::default()).is_empty());
    assert_eq!(conversation.items, before);
    let applied = apply(&clear(1), &mut conversation, &ToolSet::default());
    assert!(!conversation.items.contains(&reasoning("r1")));
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
}

#[test]
fn repeated_edits_keep_turn_boundaries_and_count_cleared_turns_once() {
    let mut conversation = request_conversation(json!([
        { "role": "user", "content": "first" },
        { "role": "assistant", "content": [thinking("r1"), thinking("r2")] },
        { "role": "user", "content": "second" },
        { "role": "assistant", "content": [thinking("r3")] },
        { "role": "user", "content": "third" },
    ]));
    let mut strategies = clear(2);
    strategies.extend(clear(1));
    let applied = apply(&strategies, &mut conversation, &ToolSet::default());
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
    assert_eq!(applied[1]["cleared_thinking_turns"], 1);
    assert_eq!(conversation.turn_starts, vec![0, 1, 2]);
    assert!(
        !conversation
            .items
            .iter()
            .any(|item| matches!(item, ResponseItem::Reasoning { .. }))
    );
}

#[test]
fn restored_compaction_history_retains_its_turn_boundaries() {
    let replacement = vec![
        user("first"),
        reasoning("r1"),
        user("second"),
        reasoning("r2"),
    ];
    let marker = crate::codec::inline_marker(&replacement);
    let mut conversation = request_conversation(json!([
        { "role": "user", "content": marker },
    ]));
    let applied = apply(&clear(1), &mut conversation, &ToolSet::default());
    assert_eq!(
        conversation.items,
        vec![user("first"), user("second"), reasoning("r2")]
    );
    assert_eq!(applied[0]["cleared_thinking_turns"], 1);
}

#[test]
fn old_tool_results_clear_past_the_trigger() {
    let big = "x".repeat(400);
    let mut conversation = Conversation {
        items: vec![
            call("c1"),
            output("c1", &big),
            call("c2"),
            output("c2", &big),
            call("c3"),
            output("c3", &big),
        ],
        turn_starts: Vec::new(),
        compaction: None,
    };
    let applied = apply(
        &edits(json!([{
            "type": "clear_tool_uses_20250919",
            "trigger": { "type": "input_tokens", "value": 10 },
            "keep": { "type": "tool_uses", "value": 1 },
        }])),
        &mut conversation,
        &ToolSet::default(),
    );
    assert_eq!(
        conversation.items,
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
