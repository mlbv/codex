use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::*;
use crate::tools::ToolSet;

fn message_stream<'a>(
    tx: &'a mpsc::Sender<serde_json::Value>,
    tools: &'a ToolSet,
) -> MessageStream<'a> {
    MessageStream::new(
        tx,
        tools,
        "gpt-test".to_string(),
        "turn-1".to_string(),
        /*display_thinking*/ false,
        Vec::new(),
        Vec::new(),
    )
}

#[tokio::test]
async fn acceptance_opens_message_and_detects_abort() {
    let (tx, mut rx) = mpsc::channel(16);
    let tools = ToolSet::default();
    let mut stream = message_stream(&tx, &tools);
    let mut response = futures::stream::iter([Ok(ResponseEvent::Created { response_id: None })])
        .chain(futures::stream::pending());
    let compaction = collect(&mut response, &mut stream);
    tokio::pin!(compaction);
    let first = tokio::select! {
        _ = &mut compaction => panic!("compaction ended before completion"),
        event = rx.recv() => event.expect("message_start"),
    };
    assert_eq!(first["type"], "message_start");
    drop(rx);
    assert!(matches!(compaction.await, Err(Failure::Disconnected)));
}

#[tokio::test]
async fn failure_before_acceptance_leaves_message_unopened() {
    let (tx, _rx) = mpsc::channel(16);
    let tools = ToolSet::default();
    let mut stream = message_stream(&tx, &tools);
    let mut response = futures::stream::iter([Err(CodexErr::Stream("limit".to_string()))]);
    assert!(matches!(
        collect(&mut response, &mut stream).await,
        Ok(Err(_))
    ));
    assert!(!stream.started());
}

fn message(role: &str, parts: &[&str]) -> ResponseItem {
    let content = parts
        .iter()
        .map(|text| ContentItem::InputText {
            text: (*text).to_string(),
        })
        .collect::<Vec<_>>();
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        internal_chat_message_metadata_passthrough: Some(
            codex_protocol::models::InternalChatMessageMetadataPassthrough {
                content_item_kinds: content_kinds(role, &content),
                ..Default::default()
            },
        ),
        content,
        phase: None,
    }
}

#[test]
fn retained_messages_exclude_client_context() {
    let input = vec![
        message(
            "user",
            &["<system-reminder>CLAUDE.md</system-reminder>", "fix it"],
        ),
        message("developer", &["env"]),
        message("user", &["  <system-reminder>todo</system-reminder>"]),
        message("assistant", &["done"]),
        message("user", &["thanks"]),
    ];
    assert_eq!(
        user_messages(input),
        vec![message("user", &["fix it"]), message("user", &["thanks"])]
    );
}
