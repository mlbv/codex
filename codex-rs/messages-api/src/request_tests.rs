use super::*;
use serde_json::json;

fn decode(body: Value, version: Option<&str>, betas: &[&str]) -> Result<MessagesRequest, ApiError> {
    parse(
        &serde_json::to_vec(&body).unwrap(),
        version,
        &betas
            .iter()
            .map(|beta| beta.to_string())
            .collect::<Vec<_>>(),
    )
}

#[test]
fn new_version_rejects_removed_sampling_fields_even_when_null() {
    for field in ["temperature", "top_p", "top_k"] {
        for value in [json!(0), Value::Null] {
            let mut body = json!({"model":"m", "messages":[]});
            body[field] = value;
            assert!(decode(body.clone(), Some(API_VERSION), &[]).is_err());
            assert!(decode(body, Some("2023-06-01"), &[]).is_ok());
        }
    }
}

#[test]
fn user_citations_are_rejected_recursively_but_assistant_citations_replay() {
    let text = json!({"type":"text", "text":"answer", "citations":[{"type":"char_location"}]});
    for content in [
        json!([text]),
        json!([{"type":"search_result", "source":"s", "content":[text]}]),
        json!([{"type":"tool_result", "tool_use_id":"t", "content":[text]}]),
    ] {
        assert!(
            decode(
                json!({"model":"m", "messages":[{"role":"user", "content":content}]}),
                None,
                &[]
            )
            .is_err()
        );
    }
    assert!(
        decode(
            json!({"model":"m", "messages":[{"role":"assistant", "content":[text]}]}),
            None,
            &[]
        )
        .is_ok()
    );
}

#[test]
fn scoped_controls_and_inline_definitions_require_their_betas() {
    for (message, beta) in [
        (
            json!({"role":"system", "content":"reminder", "clear_at":"next_user_message"}),
            "mid-conversation-system-clear-at-2026-08-21",
        ),
        (
            json!({"role":"system", "content":[], "output_config":{"effort":"high"}}),
            "mid-conversation-output-config-2026-07-01",
        ),
        (
            json!({"role":"system", "content":[{"type":"tool_addition", "tool":{"type":"tool_definition", "definition":{"name":"Read", "input_schema":{"type":"object"}}}}]}),
            "inline-tools-2026-09-15",
        ),
    ] {
        let body = json!({"model":"m", "messages":[{"role":"user", "content":"go"}, message]});
        assert!(decode(body.clone(), None, &[]).is_err());
        assert!(decode(body, None, &[beta]).is_ok());
    }
    let body = json!({"model":"m", "messages":[{"role":"system", "content":[], "clear_at":"next_user_message", "output_config":{"effort":"high"}}]});
    assert!(
        decode(
            body,
            None,
            &[
                "mid-conversation-system-clear-at-2026-08-21",
                "mid-conversation-output-config-2026-07-01"
            ]
        )
        .is_err()
    );
}

#[test]
fn between_tools_is_accepted_without_claude_model_restrictions() {
    let mut body = json!({"model":"gpt-5.5", "messages":[], "thinking":{"type":"between_tools"}});
    assert!(decode(body.clone(), None, &[]).is_ok());
    body["thinking"]["display"] = json!("updates");
    assert!(decode(body, None, &[]).is_err());
}
