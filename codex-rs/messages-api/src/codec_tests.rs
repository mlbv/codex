use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::*;

fn reasoning() -> ResponseItem {
    ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server("rs_1".to_string())),
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "plan".to_string(),
        }],
        content: None,
        encrypted_content: Some("opaque".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn signature_round_trips_reasoning() {
    let signature = encode_signature(&reasoning()).expect("encode");
    assert_eq!(decode_signature(&signature), Some(reasoning()));
}

#[test]
fn foreign_signature_is_not_decoded() {
    assert_eq!(decode_signature("EqQBCkgIARABGAIiQL"), None);
}

#[test]
fn inline_marker_splits_surrounding_text() {
    let items = vec![
        ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "fix the build".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "state".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let text = format!("Summary:\n{}\n\nContinue.", inline_marker(&items));
    let Some(("Summary:\n", Marker::Inline(encoded), "\n\nContinue.")) = find_compaction(&text)
    else {
        panic!("inline marker not found in {text}");
    };
    assert_eq!(decode_items(encoded), Some(items));
}

#[test]
fn stored_marker_names_its_digest() {
    let digest = "0123456789abcdef".repeat(4);
    let text = format!("Summary:\n{}\nContinue.", compaction_marker(&digest));
    assert_eq!(
        find_compaction(&text),
        Some(("Summary:\n", Marker::Stored(&digest), "\nContinue."))
    );
}

#[test]
fn stored_marker_without_a_digest_is_text() {
    for payload in ["..\\..\\config", "0123456789ABCDEF", ""] {
        let text = format!("[[codex-compaction:v2:{payload}]]");
        assert_eq!(find_compaction(&text), None, "{payload}");
    }
}
