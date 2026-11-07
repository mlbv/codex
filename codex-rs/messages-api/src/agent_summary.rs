//! Claude Code's agent summaries: every 30 s per running subagent, the whole subagent context
//! plus a "describe your most recent action" prompt, meant to read the subagent's prompt cache.
//! Measured 2026-10-05: on a sibling connection they read 384000-423936 of 431k-490k cached,
//! each paying the rest; a 672k prompt left nothing for a second connection, nor for its own
//! resend; a connection keeps only its latest response, so a summary there evicts the
//! subagent's. Summaries are answered here, from the subagent's latest tool call.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;

const PROMPT_START: &str = "Describe your most recent action in 3-5 words";
const PREVIOUS_START: &str = "Previous: \"";
/// Arguments naming what a tool call acts on, in preference order.
const SUBJECT_ARGUMENTS: [&str; 7] = [
    "description",
    "file_path",
    "pattern",
    "query",
    "url",
    "command",
    "prompt",
];
const MAX_LABEL_CHARS: usize = 60;

/// The label answering an agent-summary request; `None` = not one.
pub(crate) fn answer(items: &[ResponseItem]) -> Option<String> {
    let prompt = items
        .iter()
        .rposition(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))?;
    let ResponseItem::Message { content, .. } = &items[prompt] else {
        return None;
    };
    // Claude Code merges queued reminders into the prompt's message, ahead of it.
    let text = content.iter().rev().find_map(|part| match part {
        ContentItem::InputText { text } => Some(text),
        _ => None,
    })?;
    if !text.starts_with(PROMPT_START) {
        return None;
    }
    let label = items[..prompt]
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => Some(call_label(name, arguments)),
            _ => None,
        })
        .or_else(|| previous(text))
        .unwrap_or_else(|| "Working".to_string());
    Some(label.chars().take(MAX_LABEL_CHARS).collect())
}

fn call_label(name: &str, arguments: &str) -> String {
    let arguments = serde_json::from_str::<Value>(arguments).unwrap_or_default();
    match SUBJECT_ARGUMENTS
        .iter()
        .find_map(|key| arguments.get(key)?.as_str())
        .and_then(|subject| subject.lines().next())
    {
        Some(subject) => format!("{name}: {subject}"),
        None => name.to_string(),
    }
}

fn previous(prompt: &str) -> Option<String> {
    let start = prompt.find(PREVIOUS_START)? + PREVIOUS_START.len();
    let end = prompt[start..].find('"')?;
    Some(prompt[start..start + end].to_string())
}

#[cfg(test)]
#[path = "agent_summary_tests.rs"]
mod tests;
