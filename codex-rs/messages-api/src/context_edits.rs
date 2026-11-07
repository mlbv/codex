//! Messages API context editing applied to the conversation before it is sent.
//!
//! Edits are recomputed from the full client history on every request, so the cleared prefix
//! stays identical across requests.

use std::collections::HashSet;

use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_utils_string::approx_token_count;
use serde_json::Value;
use serde_json::json;

use crate::anthropic::ContextEdit;
use crate::anthropic::KeepThinking;
use crate::tools::ToolSet;

const DEFAULT_KEEP_THINKING_TURNS: u64 = 1;
const DEFAULT_TOOL_TRIGGER_TOKENS: u64 = 100_000;
const DEFAULT_KEEP_TOOL_USES: u64 = 3;
const CLEARED_TOOL_RESULT: &str = "[Tool result cleared to save context]";
const CLEARED_TOOL_INPUT: &str = "{}";

/// Applies `edits` in order and returns their `applied_edits` reports.
pub(crate) fn apply(
    edits: &[ContextEdit],
    items: &mut Vec<ResponseItem>,
    tools: &ToolSet,
) -> Vec<Value> {
    let mut applied = Vec::new();
    for edit in edits {
        let report = match edit {
            ContextEdit::ClearThinking { keep } => match keep {
                Some(KeepThinking::All(_)) => None,
                Some(KeepThinking::Turns(threshold)) => clear_thinking(items, threshold.value),
                None => clear_thinking(items, DEFAULT_KEEP_THINKING_TURNS),
            },
            ContextEdit::ClearToolUses {
                trigger,
                keep,
                clear_at_least,
                exclude_tools,
                clear_tool_inputs,
            } => clear_tool_uses(
                items,
                tools,
                ToolUseLimits {
                    trigger: trigger.map_or(DEFAULT_TOOL_TRIGGER_TOKENS, |trigger| trigger.value),
                    keep: keep.map_or(DEFAULT_KEEP_TOOL_USES, |keep| keep.value),
                    clear_at_least: clear_at_least.map_or(0, |minimum| minimum.value),
                },
                exclude_tools,
                *clear_tool_inputs,
            ),
        };
        applied.extend(report);
    }
    applied
}

/// Drops reasoning outside the last `keep` model turns that reasoned.
fn clear_thinking(items: &mut Vec<ResponseItem>, keep: u64) -> Option<Value> {
    let mut drop = HashSet::new();
    let mut turns_with_reasoning = 0u64;
    let mut turn_counted = false;
    let mut cleared_turns = 0u64;
    let mut cleared_tokens = 0u64;
    for (index, item) in items.iter().enumerate().rev() {
        let is_model_output = match item {
            ResponseItem::Reasoning { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::WebSearchCall { .. } => true,
            ResponseItem::Message { role, .. } => role == "assistant",
            _ => false,
        };
        if !is_model_output {
            turn_counted = false;
            continue;
        }
        if !matches!(item, ResponseItem::Reasoning { .. }) {
            continue;
        }
        if !turn_counted {
            turn_counted = true;
            turns_with_reasoning += 1;
            cleared_turns += u64::from(turns_with_reasoning > keep);
        }
        if turns_with_reasoning > keep {
            cleared_tokens += tokens(item);
            drop.insert(index);
        }
    }
    if drop.is_empty() {
        return None;
    }
    let mut index = 0;
    items.retain(|_| {
        index += 1;
        !drop.contains(&(index - 1))
    });
    Some(json!({
        "type": "clear_thinking_20251015",
        "cleared_thinking_turns": cleared_turns,
        "cleared_input_tokens": cleared_tokens,
    }))
}

struct ToolUseLimits {
    trigger: u64,
    keep: u64,
    clear_at_least: u64,
}

/// Clears every eligible tool result older than the last `keep` tool uses, once the
/// conversation exceeds `trigger` and the clearing frees at least `clear_at_least`.
fn clear_tool_uses(
    items: &mut [ResponseItem],
    tools: &ToolSet,
    limits: ToolUseLimits,
    exclude_tools: &[String],
    clear_tool_inputs: bool,
) -> Option<Value> {
    if items.iter().map(tokens).sum::<u64>() <= limits.trigger {
        return None;
    }
    let calls = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                call_id,
                ..
            } if !exclude_tools.contains(&tools.anthropic_name(namespace.as_deref(), name)) => {
                Some((index, call_id.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let keep = usize::try_from(limits.keep).unwrap_or(usize::MAX);
    let cleared_calls = &calls[..calls.len().saturating_sub(keep)];
    let cleared_ids = cleared_calls
        .iter()
        .map(|(_, call_id)| call_id.as_str())
        .collect::<HashSet<_>>();
    let cleared = FunctionCallOutputPayload::from_text(CLEARED_TOOL_RESULT.to_string());
    let mut targets = Vec::new();
    let mut freed = 0u64;
    for (index, item) in items.iter().enumerate() {
        if let ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            output,
            ..
        } = item
            && cleared_ids.contains(call_id.as_str())
            && output.body != cleared.body
        {
            freed += tokens(item).saturating_sub(approx_token_count(CLEARED_TOOL_RESULT) as u64);
            targets.push(index);
        }
    }
    if targets.is_empty() || freed < limits.clear_at_least {
        return None;
    }
    for index in targets {
        if let ResponseItem::FunctionCallOutput { output, .. } = &mut items[index] {
            *output = cleared.clone();
        }
    }
    if clear_tool_inputs {
        for (index, _) in cleared_calls {
            if let ResponseItem::FunctionCall { arguments, .. } = &mut items[*index] {
                freed += approx_token_count(arguments) as u64;
                *arguments = CLEARED_TOOL_INPUT.to_string();
            }
        }
    }
    Some(json!({
        "type": "clear_tool_uses_20250919",
        "cleared_tool_uses": cleared_calls.len(),
        "cleared_input_tokens": freed,
    }))
}

fn tokens(item: &ResponseItem) -> u64 {
    approx_token_count(&serde_json::to_string(item).unwrap_or_default()) as u64
}

#[cfg(test)]
#[path = "context_edits_tests.rs"]
mod tests;
