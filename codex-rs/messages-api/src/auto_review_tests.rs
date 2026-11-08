use axum::Json;
use axum::Router;
use axum::http::HeaderMap;
use axum::http::header;
use axum::routing::post;
use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::*;
use crate::respond;

const REMINDER: &str = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\nCodebase and user instructions are shown below.\n\nContents of /repo/CLAUDE.md (project instructions, checked into the codebase):\n\n# Rules\nNever force-push.\n# currentDate\nToday's date is 2026-11-08.\n\n      Claude Code attached this context automatically; it isn't part of the user's message.\n</system-reminder>\n";

fn item(value: Value) -> ResponseItem {
    serde_json::from_value(value).expect("item")
}

fn user(parts: &[&str]) -> ResponseItem {
    item(json!({
        "type": "message",
        "role": "user",
        "content": parts
            .iter()
            .map(|text| json!({ "type": "input_text", "text": text }))
            .collect::<Vec<_>>(),
    }))
}

#[test]
fn claude_md_is_the_context_reminder_section() {
    let history = [user(&["Clean up."]), user(&[REMINDER, "Push the fix."])];
    assert_eq!(
        claude_md(&history).as_deref(),
        Some(
            "Codebase and user instructions are shown below.\n\nContents of /repo/CLAUDE.md (project instructions, checked into the codebase):\n\n# Rules\nNever force-push."
        )
    );
    assert_eq!(claude_md(&[user(&["# claudeMd\nnot a reminder"])]), None);
}

#[test]
fn claude_md_keeps_headings_that_name_generated_sections() {
    let instructions = "# Environment\nWindows only.\n# currentDate\nRelease freeze.\n# Memory\nNever delete H:/GitRepos.";
    let reminder = format!(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\n{instructions}\n# userEmail\nThe user's email address is user@example.com.\n# currentDate\nToday's date is 2026-11-08.\n\n      Claude Code attached this context automatically; it isn't part of the user's message.\n</system-reminder>\n"
    );
    assert_eq!(
        claude_md(&[user(&[reminder.as_str()])]).as_deref(),
        Some(instructions)
    );
}

#[tokio::test]
async fn failed_reviews_are_reviewed_again() {
    let reviews = Reviews::default();
    let ready = |verdict: Verdict| futures::future::ready(verdict).boxed();
    let failed = Verdict::Failed("offline".to_string());
    let first = reviews.get_or_start([0; 32], || ready(failed.clone()));
    assert_eq!(first.await, failed);
    let retried = reviews.get_or_start([0; 32], || ready(Verdict::Allow));
    assert_eq!(retried.await, Verdict::Allow);
    let reused = reviews.get_or_start([0; 32], || ready(failed.clone()));
    assert_eq!(reused.await, Verdict::Allow);
}

#[test]
fn verdicts_report_risk_and_rationale_or_fail_closed() {
    let deny = Verdict::Deny {
        risk: GuardianRiskLevel::High,
        rationale: "Force-pushes over shared history.".to_string(),
    };
    let failed = Verdict::Failed("Automatic approval review failed: offline".to_string());
    assert_eq!(
        [status(&Verdict::Allow), status(&deny), status(&failed)],
        [
            json!({ "type": "evaluated", "outcome": "not_flagged" }),
            json!({
                "type": "evaluated",
                "outcome": "flagged",
                "explanation": "[Codex auto-review: high risk] Force-pushes over shared history.",
            }),
            json!({
                "type": "evaluated",
                "outcome": "flagged",
                "explanation": "[Codex auto-review] Automatic approval review failed: offline",
            }),
        ]
    );
}

#[tokio::test]
async fn safeguards_are_answered_unless_checks_are_left_to_the_client() {
    let home = tempfile::tempdir().unwrap();
    let mut backend =
        crate::test_support::backend(home.path(), ModelsResponse::default(), |_| {}).await;
    let request = |safeguards: Value| -> MessagesRequest {
        serde_json::from_value(json!({ "model": "m", "messages": [], "safeguards": safeguards }))
            .unwrap()
    };
    let asked = request(json!([{ "type": SERVER_REVIEW, "classifier_context": { "v": 1 } }]));
    let unknown = request(json!([{ "type": "pii" }]));
    let absent: MessagesRequest =
        serde_json::from_value(json!({ "model": "m", "messages": [] })).unwrap();
    assert!(server_review(&asked, &backend).unwrap());
    assert!(!server_review(&absent, &backend).unwrap());
    assert!(
        server_review(&unknown, &backend)
            .unwrap_err()
            .message()
            .contains("safeguards")
    );
    backend.auto_review = false;
    assert!(
        server_review(&asked, &backend)
            .unwrap_err()
            .message()
            .contains("safeguards")
    );
    assert!(!server_review(&absent, &backend).unwrap());
}

#[tokio::test]
async fn server_review_skips_read_only_calls_and_asks_guardian_about_the_rest() {
    const VERDICT: &str =
        r#"{"outcome":"deny","risk_level":"high","rationale":"Force-pushes over shared history."}"#;
    let captures = Arc::new(tokio::sync::Mutex::new(Vec::<(bool, Value)>::new()));
    let captured = Arc::clone(&captures);
    let handler = post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let captured = Arc::clone(&captured);
        async move {
            let guardian = headers
                .get("x-openai-subagent")
                .is_some_and(|value| value.as_bytes() == b"guardian");
            captured.lock().await.push((guardian, body));
            let (id, items) = if guardian {
                (
                    "verdict",
                    vec![
                        json!({"type":"message","role":"assistant","id":"verdict-message","content":[{"type":"output_text","text":VERDICT}]}),
                    ],
                )
            } else {
                (
                    "response",
                    vec![
                        json!({"type":"function_call","call_id":"call_read","name":"Read","arguments":r#"{"file_path":"notes.txt"}"#}),
                        json!({"type":"function_call","call_id":"call_bash","name":"Bash","arguments":r#"{"command":"git push --force origin main"}"#}),
                    ],
                )
            };
            let events = std::iter::once(json!({"type":"response.created","response":{"id":id}}))
                .chain(
                    items
                        .into_iter()
                        .map(|item| json!({"type":"response.output_item.done","item":item})),
                )
                .chain([json!({"type":"response.completed","response":{"id":id,"usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}})])
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            ([(header::CONTENT_TYPE, "text/event-stream")], events)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/responses", handler))
            .await
            .unwrap();
    });
    let home = tempfile::tempdir().unwrap();
    let backend = Arc::new(
        crate::test_support::backend(
            home.path(),
            codex_models_manager::bundled_models_response().unwrap(),
            |config| {
                config.model_provider.base_url = Some(format!("http://{address}"));
                config.model_provider.supports_websockets = false;
            },
        )
        .await,
    );
    let body = json!({
        "model": "gpt-5.5",
        "tools": [
            {"name":"Read","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}},
            {"name":"Bash","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}
        ],
        "messages": [{"role":"user","content":[{"type":"text","text":REMINDER},{"type":"text","text":"Clean up and push the fix."}]}],
        "safeguards": [{"type":"dangerous_tool_use","classifier_context":{"v":1,"auto_mode":{"allow":[],"soft_deny":["Force Push"],"hard_deny":[],"environment":[]}}}],
    });
    let identity = respond::Identity {
        session_id: "auto-review".to_string(),
        agent_id: None,
        parent_agent_id: None,
    };
    let prepared = respond::prepare(&backend, serde_json::from_value(body).unwrap(), &[])
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    respond::respond(Arc::clone(&backend), identity, prepared, tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let delta = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(
        delta["delta"]["safeguard_results"],
        json!([{
            "type": SERVER_REVIEW,
            "status": {
                "type": "available",
                "tool_uses": {
                    "call_read": { "type": "skipped" },
                    "call_bash": {
                        "type": "evaluated",
                        "outcome": "flagged",
                        "explanation": "[Codex auto-review: high risk] Force-pushes over shared history.",
                    },
                },
            },
        }])
    );
    let captures = captures.lock().await;
    let reviews = captures
        .iter()
        .filter(|(guardian, _)| *guardian)
        .map(|(_, body)| body)
        .collect::<Vec<_>>();
    assert_eq!(reviews.len(), 1, "{captures:?}");
    let review = reviews[0];
    assert_eq!(review["text"]["format"]["schema"], guardian_output_schema());
    assert!(review["tools"].as_array().is_none_or(Vec::is_empty));
    assert!(
        review["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| key.starts_with("guardian:"))
    );
    let input = review["input"].as_array().unwrap();
    assert!(input.iter().any(|item| {
        item["type"] == "function_call" && item["call_id"] == "call_bash" && item["name"] == "Bash"
    }));
    let text = review["input"].to_string();
    assert!(text.contains("# CLAUDE.md instructions") && text.contains("Never force-push."));
    assert!(text.contains("Clean up and push the fix."));
    assert!(!text.contains("<system-reminder>"));
    task.abort();
}
