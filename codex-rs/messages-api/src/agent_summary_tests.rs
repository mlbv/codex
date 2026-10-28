use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn item(value: serde_json::Value) -> ResponseItem {
    serde_json::from_value(value).expect("item")
}

fn user(text: &str) -> ResponseItem {
    item(
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }),
    )
}

fn call(arguments: serde_json::Value) -> ResponseItem {
    item(
        json!({ "type": "function_call", "name": "Bash", "arguments": arguments.to_string(), "call_id": "c1" }),
    )
}

#[test]
fn summaries_are_answered_from_the_latest_tool_call() {
    let prompt = format!(
        "{PROMPT_START} using present tense (-ing).\n\nPrevious: \"Reading main.rs\" — say something NEW."
    );
    let reminder = item(
        json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "<total_tokens>" }] }),
    );
    let with_call = [
        user("task"),
        call(json!({ "command": "cargo test\n--all", "description": "Run proxy tests" })),
        user(&prompt),
        reminder.clone(),
    ];
    let without_call = [user("task"), user(&prompt), reminder.clone()];
    let after_reminder = [
        user("task"),
        call(json!({ "file_path": "src/main.rs" })),
        item(json!({ "type": "message", "role": "user", "content": [
            { "type": "input_text", "text": "<system-reminder>
The coordinator sent a message" },
            { "type": "input_text", "text": prompt },
        ] })),
        reminder,
    ];
    assert_eq!(
        (
            answer(&with_call),
            answer(&without_call),
            answer(&after_reminder),
            answer(&[user("task"), call(json!({ "command": "ls" }))]),
            answer(&[item(
                json!({ "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": prompt },
                { "type": "input_text", "text": "and then fix the build" },
            ] })
            )]),
        ),
        (
            Some("Bash: Run proxy tests".to_string()),
            Some("Reading main.rs".to_string()),
            Some("Bash: src/main.rs".to_string()),
            None,
            None,
        )
    );
}
