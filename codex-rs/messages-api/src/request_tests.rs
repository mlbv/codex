use super::*;
use serde_json::json;

fn decode(body: Value, version: Option<&str>, betas: &[&str]) -> Result<MessagesRequest, ApiError> {
    parse(
        body,
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
fn unsupported_thinking_modes_are_rejected_even_with_effort_overrides() {
    for kind in ["disabled", "between_tools"] {
        for version in [None, Some(API_VERSION), Some("2023-06-01")] {
            for output in [Value::Null, json!({"effort":"high"})] {
                let mut body = json!({
                    "model":"gpt-5.5",
                    "messages":[],
                    "thinking":{"type":kind},
                });
                if !output.is_null() {
                    body["output_config"] = output;
                }
                let error = decode(body, version, &[]).unwrap_err();
                assert_eq!(
                    error.message(),
                    format!(
                        "thinking.type: `{kind}` is not supported by the Codex backend; use `adaptive` and output_config.effort"
                    )
                );
                assert_eq!(error.body()["error"]["type"], "invalid_request_error");
            }
        }
    }
}

#[test]
fn supported_thinking_modes_and_display_options_remain_accepted() {
    assert!(decode(json!({"model":"m", "messages":[]}), None, &[]).is_ok());
    for thinking in [
        json!({"type":"adaptive"}),
        json!({"type":"enabled", "budget_tokens":1024}),
    ] {
        let body = json!({"model":"m", "messages":[], "thinking":thinking});
        assert!(decode(body.clone(), None, &[]).is_ok());
        for display in ["summarized", "omitted", "updates"] {
            let mut body = body.clone();
            body["thinking"]["display"] = json!(display);
            assert!(decode(body, None, &[]).is_ok());
        }
    }
}
