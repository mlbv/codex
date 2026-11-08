use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;

use super::*;

fn message(role: &str, text: &str, id: Option<&str>, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: id.map(|id| ResponseItemId::from_server(id.to_string())),
        role: role.to_string(),
        content: vec![if role == "assistant" {
            ContentItem::OutputText {
                text: text.to_string(),
                annotations: None,
                logprobs: None,
            }
        } else {
            ContentItem::InputText {
                text: text.to_string(),
            }
        }],
        phase,
        internal_chat_message_metadata_passthrough: id.map(|_| {
            InternalChatMessageMetadataPassthrough {
                turn_id: Some("turn".to_string()),
                ..Default::default()
            }
        }),
    }
}

fn call(arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: "Bash".to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: "call_1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn echoed_items_resolve_to_recorded_originals() {
    let mut recorded = vec![
        message("user", "hi", Some("msg_1"), None),
        message(
            "assistant",
            "hello",
            Some("msg_2"),
            Some(MessagePhase::Commentary),
        ),
        call(r#"{"command": "ls"}"#),
    ];
    let ResponseItem::Message {
        encrypted_content,
        status,
        content,
        ..
    } = &mut recorded[1]
    else {
        unreachable!("recorded assistant message")
    };
    *encrypted_content = Some("opaque-output".to_string());
    *status = Some("completed".to_string());
    let ContentItem::OutputText {
        annotations,
        logprobs,
        ..
    } = &mut content[0]
    else {
        unreachable!("assistant output text")
    };
    *annotations = Some(Vec::new());
    *logprobs = Some(Vec::new());
    let fresh = vec![
        message("user", "hi", None, None),
        message("assistant", "hello", None, Some(MessagePhase::FinalAnswer)),
        call(r#"{"command":"ls"}"#),
        message("user", "next", None, None),
    ];
    let (input, matched) = align(&recorded, fresh);
    assert_eq!(matched, 3);
    let mut expected = recorded;
    expected.push(message("user", "next", None, None));
    assert_eq!(input, expected);
}

#[test]
fn divergence_keeps_fresh_items_after_it() {
    let recorded = vec![
        message("user", "hi", Some("msg_1"), None),
        message("assistant", "hello", Some("msg_2"), None),
    ];
    let fresh = vec![
        message("user", "hi", None, None),
        message("assistant", "edited", None, None),
    ];
    let (input, matched) = align(&recorded, fresh.clone());
    assert_eq!(matched, 1);
    assert_eq!(input, vec![recorded[0].clone(), fresh[1].clone()]);
}

#[test]
fn echoed_text_without_citation_markers_resolves_to_the_marked_original() {
    let original = message(
        "assistant",
        "Tag X5846.\u{e200}cite\u{e202}turn0search1\u{e201}",
        Some("msg_2"),
        None,
    );
    let recorded = vec![message("user", "hi", Some("msg_1"), None), original.clone()];
    let fresh = vec![
        message("user", "hi", None, None),
        message("assistant", "Tag X5846.", None, None),
        message("user", "next", None, None),
    ];
    let (input, matched) = align(&recorded, fresh);
    assert_eq!(matched, 2);
    assert_eq!(input[1], original);
}
