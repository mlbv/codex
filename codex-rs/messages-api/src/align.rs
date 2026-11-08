//! Request input rebuilt on what a lane already sent.
//!
//! Clients echo items back in Anthropic form; resolving them to the recorded originals keeps
//! replays identical, so prompt caching and WebSocket continuation see an unchanged prefix.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;

use crate::cite;

/// `recorded`'s longest prefix that `fresh` repeats, then the rest of `fresh`, and that
/// prefix's length.
pub(crate) fn align(
    recorded: &[ResponseItem],
    fresh: Vec<ResponseItem>,
) -> (Vec<ResponseItem>, usize) {
    let (matched, echoed) = matching_prefix(recorded, &fresh);
    let mut input = recorded[..matched].to_vec();
    input.extend(fresh.into_iter().skip(echoed));
    (input, matched)
}

/// How many leading items of `fresh` repeat `recorded`.
pub(crate) fn common_prefix(recorded: &[ResponseItem], fresh: &[ResponseItem]) -> usize {
    matching_prefix(recorded, fresh).0
}

fn matching_prefix(recorded: &[ResponseItem], fresh: &[ResponseItem]) -> (usize, usize) {
    let (mut matched, mut echoed) = (0, 0);
    while let Some(item) = recorded.get(matched) {
        // These controls are authored here; clients cannot echo them in Messages form.
        if matches!(item, ResponseItem::ConfigurationUpdate { .. })
            && !matches!(
                fresh.get(echoed),
                Some(ResponseItem::ConfigurationUpdate { .. })
            )
        {
            matched += 1;
        } else if fresh
            .get(echoed)
            .is_some_and(|fresh| same_item(item, fresh))
        {
            matched += 1;
            echoed += 1;
        } else {
            break;
        }
    }
    (matched, echoed)
}

/// Written by the model: two conversations repeating one share its origin.
pub(crate) fn is_model_output(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::Compaction { .. } => true,
        _ => false,
    }
}

/// Equal up to what the client cannot echo: ids, Codex metadata, argument formatting, citation markers.
fn same_item(recorded: &ResponseItem, fresh: &ResponseItem) -> bool {
    match (recorded, fresh) {
        (
            ResponseItem::ConfigurationUpdate { reasoning },
            ResponseItem::ConfigurationUpdate {
                reasoning: fresh_reasoning,
            },
        ) => reasoning == fresh_reasoning,
        (
            ResponseItem::AdditionalTools { tools, .. },
            ResponseItem::AdditionalTools {
                tools: fresh_tools, ..
            },
        ) => tools == fresh_tools,
        (
            ResponseItem::Message { role, content, .. },
            ResponseItem::Message {
                role: fresh_role,
                content: fresh_content,
                ..
            },
        ) => {
            role == fresh_role
                && content.len() == fresh_content.len()
                && content.iter().zip(fresh_content).all(|(recorded, fresh)| {
                    match (recorded, fresh) {
                        (
                            ContentItem::OutputText { text, .. },
                            ContentItem::OutputText {
                                text: fresh_text, ..
                            },
                        ) => text == fresh_text || cite::strip(text) == *fresh_text,
                        _ => recorded == fresh,
                    }
                })
        }
        (
            ResponseItem::Reasoning {
                encrypted_content, ..
            },
            ResponseItem::Reasoning {
                encrypted_content: fresh_encrypted_content,
                ..
            },
        ) => encrypted_content == fresh_encrypted_content,
        (
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            },
            ResponseItem::FunctionCall {
                name: fresh_name,
                arguments: fresh_arguments,
                call_id: fresh_call_id,
                ..
            },
        ) => {
            call_id == fresh_call_id
                && name == fresh_name
                && serde_json::from_str::<Value>(arguments).ok()
                    == serde_json::from_str::<Value>(fresh_arguments).ok()
        }
        (
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            },
            ResponseItem::FunctionCallOutput {
                call_id: fresh_call_id,
                output: fresh_output,
                ..
            },
        ) => call_id == fresh_call_id && output.body == fresh_output.body,
        (
            ResponseItem::WebSearchCall { id, .. },
            ResponseItem::WebSearchCall { id: fresh_id, .. },
        ) => id == fresh_id,
        (
            ResponseItem::ToolSearchCall {
                call_id, arguments, ..
            },
            ResponseItem::ToolSearchCall {
                call_id: fresh_call_id,
                arguments: fresh_arguments,
                ..
            },
        ) => call_id == fresh_call_id && arguments == fresh_arguments,
        (
            ResponseItem::ToolSearchOutput { call_id, tools, .. },
            ResponseItem::ToolSearchOutput {
                call_id: fresh_call_id,
                tools: fresh_tools,
                ..
            },
        ) => call_id == fresh_call_id && tools == fresh_tools,
        (
            ResponseItem::Compaction {
                encrypted_content, ..
            },
            ResponseItem::Compaction {
                encrypted_content: fresh_encrypted_content,
                ..
            },
        ) => encrypted_content == fresh_encrypted_content,
        _ => false,
    }
}

#[cfg(test)]
#[path = "align_tests.rs"]
mod tests;
