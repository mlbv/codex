//! Codex remote compaction for clients that compact by asking for a summary.
//!
//! The summary answered to the client names the replacement history Codex builds; replaying that
//! summary restores the compaction item in place of the summarized conversation.

use std::path::Path;

use codex_core::ResponseEvent;
use codex_core::external_harness::CompactionPhase;
use codex_core::external_harness::CompactionReason;
use codex_core::external_harness::CompactionTrigger;
use codex_core::external_harness::ExternalRequest;
use codex_core::external_harness::ExternalRequestKind;
use codex_core::external_harness::ExternalTurn;
use codex_core::external_harness::StreamRetries;
use codex_core::external_harness::record_history_item;
use codex_core::external_harness::retain_messages_for_compaction;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use futures::Stream;
use futures::StreamExt;

use crate::codec;
use crate::compactions;
use crate::history::CLIENT_CONTEXT_PREFIX;
use crate::history::content_kinds;
use crate::respond::Failure;
use crate::respond::PromptParts;
use crate::stream::MessageStream;

const SUMMARY_NOTE: &str =
    "Codex compacted the earlier conversation; the proxy stores its state under the digest below.";

/// Compacts `input`, stores the replacement history and answers the client with its digest.
pub(crate) async fn run(
    turn: &mut ExternalTurn,
    parts: &PromptParts,
    input: Vec<ResponseItem>,
    mid_turn: bool,
    request: &mut ExternalRequest<'_>,
    stream: &mut MessageStream<'_>,
    codex_home: &Path,
) -> Result<Vec<ResponseItem>, Failure> {
    request.kind = ExternalRequestKind::Compaction {
        trigger: CompactionTrigger::Auto,
        reason: CompactionReason::ContextLimit,
        phase: if mid_turn {
            CompactionPhase::MidTurn
        } else {
            CompactionPhase::PreTurn
        },
    };
    let mut prompt_input = input.clone();
    prompt_input.push(ResponseItem::CompactionTrigger {});
    let prompt = parts.prompt(prompt_input);
    let mut retries = StreamRetries::default();
    let (mut compaction, usage) = loop {
        let attempt = match turn.stream(&prompt, request).await {
            Ok(mut response) => collect(&mut response, stream).await?,
            Err(err) => Err(err),
        };
        match attempt {
            Ok((mut compactions, usage)) if compactions.len() == 1 => {
                break (compactions.remove(0), usage);
            }
            Ok((compactions, _)) => {
                return Err(CodexErr::Fatal(format!(
                    "remote compaction v2 expected exactly one compaction output item, got {}",
                    compactions.len()
                ))
                .into());
            }
            Err(err) => turn.recover(err, request, &mut retries).await?,
        }
    };
    record_history_item(&mut compaction, turn.turn_id());
    let mut replacement = retain_messages_for_compaction(user_messages(input));
    replacement.push(compaction);

    let digest = compactions::store(codex_home, &replacement)
        .await
        .map_err(CodexErr::Io)?;
    let marker = codec::compaction_marker(&digest);
    stream
        .summary(
            &format!("<summary>\n{SUMMARY_NOTE}\n{marker}\n</summary>"),
            usage,
        )
        .await?;
    Ok(replacement)
}

/// One compaction response's compaction items and usage, or the error ending it.
async fn collect(
    response: &mut (impl Stream<Item = Result<ResponseEvent, CodexErr>> + Unpin),
    stream: &mut MessageStream<'_>,
) -> Result<Result<(Vec<ResponseItem>, Option<TokenUsage>), CodexErr>, Failure> {
    let mut compactions = Vec::new();
    loop {
        let event = tokio::select! {
            event = response.next() => event,
            () = stream.closed() => return Err(Failure::Disconnected),
        };
        match event {
            // Compaction streams nothing for minutes, past Claude Code's first-byte window; a
            // client abort goes unnoticed until bytes flow. Failures before acceptance stay HTTP.
            Some(Ok(ResponseEvent::Created { .. })) => stream.start().await?,
            Some(Ok(ResponseEvent::OutputItemDone(item @ ResponseItem::Compaction { .. }))) => {
                compactions.push(item);
            }
            Some(Ok(ResponseEvent::Completed { token_usage, .. })) => {
                return Ok(Ok((compactions, token_usage)));
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Ok(Err(err)),
            None => {
                return Ok(Err(CodexErr::Stream(
                    "remote compaction v2 stream closed before response.completed".to_string(),
                )));
            }
        }
    }
}

/// What the user wrote, without the context the client injected around it.
fn user_messages(input: Vec<ResponseItem>) -> Vec<ResponseItem> {
    input
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::Message {
                encrypted_content,
                status,
                id,
                role,
                content,
                phase,
                internal_chat_message_metadata_passthrough,
            } if role == "user" => {
                let content = content
                    .into_iter()
                    .filter(|part| {
                        !matches!(part, ContentItem::InputText { text }
                            if text.trim_start().starts_with(CLIENT_CONTEXT_PREFIX))
                    })
                    .collect::<Vec<_>>();
                // Kinds stay aligned with the parts that remain.
                let internal_chat_message_metadata_passthrough =
                    internal_chat_message_metadata_passthrough.map(|mut metadata| {
                        metadata.content_item_kinds = content_kinds(&role, &content);
                        metadata
                    });
                (!content.is_empty()).then_some(ResponseItem::Message {
                    encrypted_content,
                    status,
                    id,
                    role,
                    content,
                    phase,
                    internal_chat_message_metadata_passthrough,
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
