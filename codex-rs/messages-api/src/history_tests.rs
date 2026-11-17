use codex_protocol::models::FileReference;
use codex_protocol::models::ReasoningItemReasoningSummary;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn scoped_reminders_clear_after_any_user_message_and_effort_only_messages_add_no_text() {
    let conversation = conversation_items(
        messages(json!([
            {"role":"user", "content":"go"},
            {"role":"system", "content":"old", "clear_at":"next_user_message"},
            {"role":"assistant", "content":"working"},
            {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"c", "content":"ok"}]},
            {"role":"system", "content":[], "output_config":{"effort":"high"}},
            {"role":"system", "content":"current", "clear_at":"next_user_message"}
        ])),
        &ToolSet::default(),
        &HashMap::new(),
    )
    .unwrap();
    let serialized = serde_json::to_string(&conversation.items).unwrap();
    assert!(!serialized.contains("old"));
    assert!(serialized.contains("current"));
    assert_eq!(conversation.items.len(), 4);
}

fn messages(value: Value) -> Vec<InputMessage> {
    serde_json::from_value(value).expect("messages")
}

fn text(role: &str, text: &str) -> ResponseItem {
    let content = if role == "assistant" {
        ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: text.to_string(),
        }
    } else {
        ContentItem::InputText {
            text: text.to_string(),
        }
    };
    message_item(role, vec![content], /*phase*/ None)
}

fn with_phase(item: ResponseItem, phase: MessagePhase) -> ResponseItem {
    match item {
        ResponseItem::Message {
            encrypted_content,
            status,
            id,
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } => ResponseItem::Message {
            encrypted_content,
            status,
            id,
            role,
            content,
            phase: Some(phase),
            internal_chat_message_metadata_passthrough,
        },
        other => other,
    }
}

#[test]
fn claude_code_turn_becomes_codex_items() {
    let reasoning = ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: String::new(),
        }],
        content: None,
        encrypted_content: Some("opaque".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let signature = codec::encode_signature(&reasoning).expect("signature");
    let tools = ToolSet::from_request(&[json!({
        "name": "Bash",
        "description": "run",
        "input_schema": { "type": "object", "properties": {} },
    })])
    .expect("tools");
    let conversation = conversation_items(
        messages(json!([
            { "role": "user", "content": [
                { "type": "text", "text": "<system-reminder>ctx</system-reminder>" },
                { "type": "text", "text": "run it" },
            ]},
            { "role": "system", "content": "env" },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "", "signature": signature },
                { "type": "thinking", "thinking": "", "signature": "foreign" },
                { "type": "text", "text": "Running." },
                { "type": "tool_use", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "a.txt" },
                { "type": "text", "text": "and then?" },
            ]},
        ])),
        &tools,
        &HashMap::new(),
    )
    .expect("conversation");
    let mut output = FunctionCallOutputPayload::from_text("a.txt".to_string());
    output.success = Some(true);
    assert_eq!(
        conversation.items,
        vec![
            message_item(
                "user",
                vec![
                    ContentItem::InputText {
                        text: "<system-reminder>ctx</system-reminder>".to_string()
                    },
                    ContentItem::InputText {
                        text: "run it".to_string()
                    },
                ],
                /*phase*/ None,
            ),
            text("developer", "env"),
            reasoning,
            with_phase(text("assistant", "Running."), MessagePhase::Commentary),
            ResponseItem::FunctionCall {
                encrypted_content: None,
                status: None,
                id: None,
                name: "Bash".to_string(),
                namespace: None,
                arguments: r#"{"command":"ls"}"#.to_string(),
                encrypted_function_args: None,
                call_id: "call_1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call_1".to_string()),
                name: None,
                namespace: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            text("user", "and then?"),
        ]
    );
    assert_eq!(conversation.compaction, None);
}

#[test]
fn content_kinds_follow_codex_classification() {
    let item = text("user", "<system-reminder>ctx</system-reminder>");
    let kinds = match item {
        ResponseItem::Message {
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } => metadata.content_item_kinds,
        _ => None,
    };
    assert_eq!(
        kinds,
        Some(vec![ContentItemKind(
            "additional_content.system_reminder".to_string()
        )])
    );
}

#[test]
fn compaction_prompt_is_detected_and_stripped() {
    let prompt = format!(
        "{COMPACTION_PROMPT_START}\n\n- rules\n\n{}\n\n1. Primary Request",
        NATIVE_COMPACTION_TASKS[0]
    );
    let conversation = conversation_items(
        messages(json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello" },
            { "role": "user", "content": [{ "type": "text", "text": prompt }] },
            { "role": "system", "content": "<total_tokens>1 tokens left</total_tokens>" },
        ])),
        &ToolSet::default(),
        &HashMap::new(),
    )
    .expect("conversation");
    assert_eq!(
        conversation.items,
        vec![
            text("user", "hi"),
            with_phase(text("assistant", "hello"), MessagePhase::FinalAnswer),
            text("developer", "<total_tokens>1 tokens left</total_tokens>"),
        ]
    );
    assert_eq!(
        conversation.compaction,
        Some(CompactionRequest { mid_turn: false })
    );
}

#[test]
fn recent_portion_compaction_stays_a_model_summary() {
    let prompt = format!(
        "{COMPACTION_PROMPT_START}\nYour task is to create a detailed summary of the RECENT portion of the conversation"
    );
    let conversation = conversation_items(
        messages(json!([{ "role": "user", "content": prompt }])),
        &ToolSet::default(),
        &HashMap::new(),
    )
    .expect("conversation");
    assert_eq!(conversation.compaction, None);
    assert_eq!(conversation.items, vec![text("user", &prompt)]);
}

fn replacement() -> Vec<ResponseItem> {
    vec![
        text("user", "fix the build"),
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "state".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

fn summary_conversation(
    marker: &str,
    stored: &HashMap<String, Vec<ResponseItem>>,
) -> Result<Conversation, ApiError> {
    let summary = format!(
        "This session is being continued.\n\nSummary:\nnote\n{marker}\n\nContinue the conversation."
    );
    conversation_items(
        messages(json!([{ "role": "user", "content": [
            { "type": "text", "text": "<system-reminder>ctx</system-reminder>" },
            { "type": "text", "text": summary },
        ]}])),
        &ToolSet::default(),
        stored,
    )
}

fn replayed_summary() -> Vec<ResponseItem> {
    let mut expected = vec![text("user", "<system-reminder>ctx</system-reminder>")];
    expected.extend(replacement());
    expected.push(text("user", "Continue the conversation."));
    expected
}

#[test]
fn stored_compaction_summary_replays_replacement_history() {
    let digest = "ab".repeat(32);
    let stored = HashMap::from([(digest.clone(), replacement())]);
    let conversation =
        summary_conversation(&codec::compaction_marker(&digest), &stored).expect("conversation");
    assert_eq!(conversation.items, replayed_summary());
}

#[test]
fn inline_compaction_summary_replays_replacement_history() {
    let conversation = summary_conversation(&codec::inline_marker(&replacement()), &HashMap::new())
        .expect("conversation");
    assert_eq!(conversation.items, replayed_summary());
}

#[test]
fn unstored_compaction_digest_is_rejected() {
    let digest = "cd".repeat(32);
    let error = summary_conversation(&codec::compaction_marker(&digest), &HashMap::new())
        .expect_err("missing digest");
    assert!(error.message().contains(&digest), "{}", error.message());
}

#[test]
fn pdf_and_url_documents_become_input_files() {
    let conversation = conversation_items(
        messages(json!([{ "role": "user", "content": [
            {
                "type": "document",
                "title": "Q3 report",
                "source": { "type": "base64", "media_type": "application/pdf", "data": "JVBERi0=" },
            },
            { "type": "document", "source": { "type": "url", "url": "https://example.com/a.pdf" } },
        ]}])),
        &ToolSet::default(),
        &HashMap::new(),
    )
    .expect("conversation");
    let [ResponseItem::Message { content, .. }] = conversation.items.as_slice() else {
        panic!("one message");
    };
    assert_eq!(
        content.as_slice(),
        [
            ContentItem::InputText {
                text: "Q3 report".to_string()
            },
            ContentItem::InputFile {
                file: FileReference::Inline {
                    filename: "Q3_report.pdf".to_string(),
                    file_data: "data:application/pdf;base64,JVBERi0=".to_string(),
                },
            },
            ContentItem::InputFile {
                file: FileReference::Url {
                    file_url: "https://example.com/a.pdf".to_string()
                },
            },
        ]
    );
}

#[test]
fn advisor_blocks_replay_as_a_function_call_and_its_advice() {
    let tools = ToolSet::from_request(&[
        json!({ "type": "advisor_20260301", "name": "advisor", "model": "gpt-6-astra" }),
    ])
    .expect("tools");
    let conversation = conversation_items(
        messages(json!([
            { "role": "user", "content": "plan it" },
            { "role": "assistant", "content": [
                { "type": "server_tool_use", "id": "srvtoolu_1", "name": "advisor", "input": {} },
                { "type": "advisor_tool_result", "tool_use_id": "srvtoolu_1", "content": { "type": "advisor_result", "text": "Start with the parser." } },
            ]},
        ])),
        &tools,
        &HashMap::new(),
    )
    .expect("conversation");
    assert_eq!(
        conversation.items[1..].to_vec(),
        vec![
            ResponseItem::FunctionCall {
                encrypted_content: None,
                status: None,
                id: None,
                name: "advisor".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                encrypted_function_args: None,
                call_id: "srvtoolu_1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            crate::advisor::advice_output("srvtoolu_1", "Start with the parser.".to_string()),
        ]
    );
}
