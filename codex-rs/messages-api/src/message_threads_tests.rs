use super::*;
use axum::response::IntoResponse;

fn identity() -> Identity {
    Identity {
        session_id: "session".to_string(),
        agent_id: Some("agent".to_string()),
        parent_agent_id: None,
    }
}

fn begin(store: &Arc<MessageThreads>, body: Value) -> Result<Started, ApiError> {
    store.begin(body, &[BETA.to_string()], identity())
}

fn create() -> Value {
    json!({
        "model": "m", "max_tokens": 1024,
        "system": "instructions",
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "read it"}],
        "thread": {"type": "create"},
    })
}

fn next(id: &str) -> Value {
    json!({
        "model": "m", "max_tokens": 2048,
        "messages": [{"role": "user", "content": "next"}],
        "thread": {"type": "continue", "previous_message_id": id},
    })
}

fn response(id: &str) -> Value {
    json!({"id": id, "content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"})
}

fn error(result: Result<Started, ApiError>) -> Value {
    match result {
        Err(error) => error.body(),
        Ok(_) => panic!("expected a thread error"),
    }
}

fn error_code(result: Result<Started, ApiError>) -> Value {
    error(result)["error"]["details"]["error_code"].clone()
}

#[test]
fn replays_signed_thinking_and_tool_results_with_the_original_identity_and_tools() {
    let store = Arc::new(MessageThreads::default());
    let first = begin(&store, create()).unwrap();
    let mut answer = response("msg_first");
    answer["content"] = json!([
        {"type": "thinking", "thinking": "checking", "signature": "signed-reasoning"},
        {"type": "tool_use", "id": "call_read", "name": "Read", "input": {"path": "a"}},
    ]);
    answer["stop_reason"] = json!("tool_use");
    first.pending.unwrap().complete(answer.clone());
    let mut request = next("msg_first");
    let result = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_read", "content": "file"}]});
    request["messages"] = json!([result]);
    let continued = store
        .begin(
            request,
            &[BETA.to_string()],
            Identity {
                session_id: "different".to_string(),
                agent_id: None,
                parent_agent_id: None,
            },
        )
        .unwrap();
    assert_eq!(continued.identity.session_id, "session");
    assert_eq!(continued.identity.agent_id.as_deref(), Some("agent"));
    assert_eq!(continued.body["system"], "instructions");
    assert_eq!(continued.body["tools"], create()["tools"]);
    assert_eq!(continued.body["max_tokens"], 2048);
    assert_eq!(
        continued.body["messages"],
        json!([
            create()["messages"][0],
            {"role": "assistant", "content": answer["content"]},
            result,
        ])
    );
}

#[test]
fn reserves_once_releases_failed_requests_and_consumes_completed_continuations() {
    let store = Arc::new(MessageThreads::default());
    begin(&store, create())
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_first"));
    let continued = begin(&store, next("msg_first")).unwrap();
    assert_eq!(
        error_code(begin(&store, next("msg_first"))),
        "thread_continue_in_flight"
    );
    drop(continued);
    let retry = begin(&store, next("msg_first")).unwrap();
    retry.pending.unwrap().complete(response("msg_second"));
    assert_eq!(
        error_code(begin(&store, next("msg_first"))),
        "thread_already_continued"
    );
    let last = begin(&store, next("msg_second")).unwrap();
    assert_eq!(last.body["messages"].as_array().unwrap().len(), 5);
}

#[test]
fn fixed_parameter_errors_preserve_the_parent_and_mcp_credentials_can_rotate() {
    let store = Arc::new(MessageThreads::default());
    let mut initial = create();
    initial["mcp_servers"] = json!([{"type": "url", "name": "docs", "url": "https://example.com/mcp", "authorization_token": "old"}]);
    let betas = vec![BETA.to_string(), "other-beta".to_string()];
    store
        .begin(initial.clone(), &betas, identity())
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_first"));
    let mut current = next("msg_first");
    current["mcp_servers"] = initial["mcp_servers"].clone();
    current["mcp_servers"][0]["authorization_token"] = json!("new");
    for field in FIXED_FIELDS {
        let mut changed = current.clone();
        changed[*field] = json!("changed");
        let rejected = error(store.begin(changed, &betas, identity()));
        assert_eq!(
            rejected["error"]["details"]["error_code"],
            "thread_fingerprint_mismatch"
        );
        assert!(
            rejected["error"]["message"]
                .as_str()
                .unwrap()
                .contains(*field)
        );
    }
    assert_eq!(
        error_code(begin(&store, current.clone())),
        "thread_fingerprint_mismatch"
    );
    let reordered = vec!["other-beta".to_string(), BETA.to_string(), BETA.to_string()];
    let continued = store.begin(current, &reordered, identity()).unwrap();
    assert_eq!(
        continued.body["mcp_servers"][0]["authorization_token"],
        "new"
    );
}

#[test]
fn requires_the_beta_and_omitted_create_only_fields_including_null() {
    let store = Arc::new(MessageThreads::default());
    assert!(store.begin(create(), &[], identity()).is_err());
    let mut unthreaded = create();
    unthreaded.as_object_mut().unwrap().remove("thread");
    assert!(
        store
            .begin(unthreaded, &[], identity())
            .unwrap()
            .pending
            .is_none()
    );
    begin(&store, create())
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_first"));
    for field in ["system", "tools"] {
        for value in [Value::Null, json!([])] {
            let mut request = next("msg_first");
            request[field] = value;
            assert!(begin(&store, request).is_err());
        }
    }
    assert!(begin(&store, next("msg_first")).is_ok());
}

#[test]
fn expires_unavailable_ids_and_does_not_publish_refusals() {
    let store = Arc::new(MessageThreads::default());
    let mut refused = response("msg_refused");
    refused["stop_reason"] = json!("refusal");
    begin(&store, create())
        .unwrap()
        .pending
        .unwrap()
        .complete(refused);
    assert_eq!(
        error_code(begin(&store, next("msg_refused"))),
        "thread_not_found"
    );
    begin(&store, create())
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_expired"));
    store
        .entries
        .lock()
        .unwrap()
        .get_mut("msg_expired")
        .unwrap()
        .expires = Instant::now() - TTL;
    let rejected = begin(&store, next("msg_expired"))
        .err()
        .unwrap()
        .into_response();
    assert_eq!(rejected.status(), 404);
}

#[test]
fn only_counts_current_message_breakpoints_but_inherits_system_and_tool_breakpoints() {
    let store = Arc::new(MessageThreads::default());
    let marker = json!({"type": "ephemeral"});
    let mut initial = create();
    initial["system"] = json!([{"type": "text", "text": "instructions", "cache_control": marker}]);
    initial["tools"][0]["cache_control"] = marker.clone();
    initial["cache_control"] = marker.clone();
    initial["messages"][0]["content"] =
        json!([{"type": "text", "text": "first", "cache_control": marker}]);
    begin(&store, initial)
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_first"));
    let mut current = next("msg_first");
    current["cache_control"] = marker.clone();
    current["messages"][0]["content"] =
        json!([{"type": "text", "text": "next", "cache_control": marker}]);
    begin(&store, current.clone())
        .unwrap()
        .pending
        .unwrap()
        .complete(response("msg_second"));
    current["thread"]["previous_message_id"] = json!("msg_second");
    current["messages"][0]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "text", "text": "one too many", "cache_control": marker}));
    let rejected = error(begin(&store, current));
    assert_eq!(
        rejected["error"]["details"],
        json!({"error_code": "thread_unsupported_request", "unsupported": ["cache_control"]})
    );
}

#[test]
fn context_editing_and_compaction_are_rejected_but_preserved_thinking_is_allowed() {
    let store = Arc::new(MessageThreads::default());
    let mut request = create();
    request["context_management"] =
        json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]});
    assert!(begin(&store, request.clone()).is_ok());
    request["context_management"]["edits"][0]["keep"] = json!("none");
    assert_eq!(
        error_code(begin(&store, request)),
        "thread_unsupported_request"
    );
    let mut request = create();
    request["compaction"] = json!({"type": "summarize"});
    assert_eq!(
        error_code(begin(&store, request)),
        "thread_unsupported_request"
    );
}

#[test]
fn paused_turns_accept_only_empty_continuations() {
    let store = Arc::new(MessageThreads::default());
    let mut paused = response("msg_paused");
    paused["stop_reason"] = json!("pause_turn");
    begin(&store, create())
        .unwrap()
        .pending
        .unwrap()
        .complete(paused);
    assert!(begin(&store, next("msg_paused")).is_err());
    let mut resume = next("msg_paused");
    resume["messages"] = json!([]);
    assert!(begin(&store, resume).is_ok());
}
