use pretty_assertions::assert_eq;

use super::*;

#[test]
fn image_context_and_backend_request_identifiers_are_preserved() {
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-image-turn-id", HeaderValue::from_static("turn-1"));
    headers.insert(
        codex_core::X_CODEX_TURN_METADATA_HEADER,
        HeaderValue::from_static("{\"session_id\":\"s\"}"),
    );
    let (turn, metadata) = image_context(&headers).unwrap();
    assert_eq!(turn, "turn-1");
    assert_eq!(metadata, Some("{\"session_id\":\"s\"}"));
    let error = ApiError::api("failed")
        .with_image_request_id(Some("image-request-1".to_string()))
        .into_response();
    assert_eq!(
        error.headers()["x-codex-imagegen-request-id"],
        "image-request-1"
    );
}

fn request(body: Value) -> MessagesRequest {
    serde_json::from_value(body).expect("request")
}

#[test]
fn speed_header_fills_only_an_unset_speed() {
    let mut headers = HeaderMap::new();
    headers.insert(SPEED_HEADER, HeaderValue::from_static("fast"));
    let mut unset = request(json!({ "model": "m", "messages": [] }));
    default_speed(&mut unset, &headers).expect("ascii header");
    let mut standard = request(json!({ "model": "m", "messages": [], "speed": "standard" }));
    default_speed(&mut standard, &headers).expect("ascii header");
    assert_eq!(
        (unset.speed.as_deref(), standard.speed.as_deref()),
        (Some("fast"), Some("standard"))
    );
}

#[tokio::test]
async fn image_http_path_forwards_identity_metadata_and_backend_identifiers() {
    use codex_protocol::openai_models::ModelsResponse;
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captures = Arc::clone(&captured);
    let stub = post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let captures = Arc::clone(&captures);
        async move {
            let failed = body["prompt"] == "fail";
            captures.lock().await.push((headers, body));
            (
                if failed {
                    axum::http::StatusCode::BAD_REQUEST
                } else {
                    axum::http::StatusCode::OK
                },
                [("x-codex-imagegen-request-id", "image-request-1")],
                Json(if failed {
                    json!({"error":{"message":"invalid image", "type":"invalid_request_error"}})
                } else {
                    json!({"created":1,"data":[{"b64_json":"png", "generation_id":"generation-1"}]})
                }),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/images/generations", stub.clone())
                .route("/images/edits", stub),
        )
        .await
        .unwrap()
    });
    let home = tempfile::tempdir().unwrap();
    let backend = crate::test_support::backend(home.path(), ModelsResponse::default(), |config| {
        config.model_provider.base_url = Some(format!("http://{address}"));
    })
    .await;
    let state = AppState {
        backend: Arc::new(backend),
        listen: address,
        message_threads: Arc::new(MessageThreads::default()),
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("x-codex-image-turn-id", HeaderValue::from_static("turn-1"));
    headers.insert(
        codex_core::X_CODEX_TURN_METADATA_HEADER,
        HeaderValue::from_static("{\"session_id\":\"s\"}"),
    );
    for (operation, body) in [
        (images::Operation::Generate, json!({"prompt":"cat"})),
        (
            images::Operation::Edit,
            json!({"prompt":"hat", "images":[{"image_url":"data:image/png;base64,AAAA"}]}),
        ),
        (images::Operation::Generate, json!({"prompt":"fail"})),
    ] {
        let failed = body["prompt"] == "fail";
        let response = images(
            state.clone(),
            headers.clone(),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
            operation,
        )
        .await;
        assert_eq!(response.status(), if failed { 400 } else { 200 });
        assert_eq!(
            response.headers()["x-codex-imagegen-request-id"],
            "image-request-1"
        );
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        if !failed {
            assert_eq!(body["data"][0]["generation_id"], "generation-1");
        }
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    let rejected = images(
        state,
        headers,
        Bytes::from_static(b"{\"prompt\":\"cat\"}"),
        images::Operation::Generate,
    )
    .await;
    assert_eq!(rejected.status(), 400);
    let captured = captured.lock().await;
    assert_eq!(captured.len(), 3);
    for (headers, body) in captured.iter() {
        assert_eq!(headers["x-codex-image-turn-id"], "turn-1");
        assert_eq!(
            headers[codex_core::X_CODEX_TURN_METADATA_HEADER],
            "{\"session_id\":\"s\"}"
        );
        assert!(
            headers[header::USER_AGENT]
                .to_str()
                .unwrap()
                .contains(&format!("/{} ", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(body["model"], codex_image_generation_extension::IMAGE_MODEL);
    }
    task.abort();
}

fn message_threads_begin(
    store: &Arc<MessageThreads>,
    thread: Value,
) -> crate::message_threads::Started {
    store.begin(
        json!({"model": "m", "messages": [{"role": "user", "content": "go"}], "thread": thread}),
        &[crate::message_threads::BETA.to_string()],
        Identity { session_id: "session".to_string(), agent_id: None, parent_agent_id: None },
    ).unwrap()
}

#[tokio::test]
async fn message_threads_publish_after_message_stop_and_replay_collected_tool_input() {
    let store = Arc::new(MessageThreads::default());
    let started = message_threads_begin(&store, json!({"type": "create"}));
    let (events, rx) = mpsc::channel(EVENT_BUFFER);
    let (tx, mut output) = mpsc::channel(EVENT_BUFFER);
    let task = tokio::spawn(forward_thread_response(started.pending.unwrap(), tx, rx));
    events.send(json!({"type": "message_start", "message": {
        "id": "msg_stream", "type": "message", "role": "assistant", "model": "m", "content": [], "usage": {}
    }})).await.unwrap();
    let first = output.recv().await.unwrap();
    let next = json!({"type": "continue", "previous_message_id": "msg_stream"});
    let unavailable = store.begin(
        json!({"model": "m", "messages": [{"role": "user", "content": "next"}], "thread": next}),
        &[crate::message_threads::BETA.to_string()],
        Identity { session_id: "session".to_string(), agent_id: None, parent_agent_id: None },
    ).err().unwrap();
    assert_eq!(
        unavailable.body()["error"]["details"]["error_code"],
        "thread_not_found"
    );
    for event in [
        json!({"type": "content_block_start", "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "checking"}}),
        json!({"type": "content_block_delta", "delta": {"type": "signature_delta", "signature": "signed"}}),
        json!({"type": "content_block_stop"}),
        json!({"type": "content_block_start", "content_block": {"type": "tool_use", "id": "call_read", "name": "Read", "input": {}}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "\"a\"}"}}),
        json!({"type": "content_block_stop"}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ] {
        events.send(event).await.unwrap();
    }
    drop(events);
    let collected = collect_message(first, output).await;
    task.await.unwrap();
    let response: Value = serde_json::from_slice(
        &axum::body::to_bytes(collected.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    let continued = message_threads_begin(&store, next);
    assert_eq!(response["content"][0]["signature"], "signed");
    assert_eq!(response["content"][1]["input"], json!({"path": "a"}));
    assert_eq!(
        continued.body["messages"][1]["content"],
        response["content"]
    );
    assert_eq!(response["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn message_threads_do_not_publish_an_error_after_message_start() {
    let store = Arc::new(MessageThreads::default());
    let started = message_threads_begin(&store, json!({"type": "create"}));
    let (events, rx) = mpsc::channel(EVENT_BUFFER);
    let (tx, mut output) = mpsc::channel(EVENT_BUFFER);
    let task = tokio::spawn(forward_thread_response(started.pending.unwrap(), tx, rx));
    events
        .send(json!({"type": "message_start", "message": {"id": "msg_failed", "content": []}}))
        .await
        .unwrap();
    events
        .send(ApiError::invalid_request("failed").body())
        .await
        .unwrap();
    drop(events);
    let first = output.recv().await.unwrap();
    let collected = collect_message(first, output).await;
    task.await.unwrap();
    assert_eq!(collected.status(), 400);
    let unavailable = store.begin(
        json!({"model": "m", "messages": [{"role": "user", "content": "next"}], "thread": {"type": "continue", "previous_message_id": "msg_failed"}}),
        &[crate::message_threads::BETA.to_string()],
        Identity { session_id: "session".to_string(), agent_id: None, parent_agent_id: None },
    ).err().unwrap();
    assert_eq!(
        unavailable.body()["error"]["details"]["error_code"],
        "thread_not_found"
    );
}

#[tokio::test]
async fn message_threads_are_rejected_by_token_counting_before_model_lookup() {
    let home = tempfile::tempdir().unwrap();
    let backend = crate::test_support::backend(
        home.path(),
        codex_protocol::openai_models::ModelsResponse::default(),
        |_| {},
    )
    .await;
    let state = AppState {
        backend: Arc::new(backend),
        listen: "127.0.0.1:41817".parse().unwrap(),
        message_threads: Arc::new(MessageThreads::default()),
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let response = count_tokens(
        State(state),
        headers,
        Bytes::from_static(
            b"{\"model\":\"missing\",\"messages\":[],\"thread\":{\"type\":\"create\"}}",
        ),
    )
    .await;
    assert_eq!(response.status(), 400);
    let response: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        response["error"]["message"],
        "thread: not supported by count_tokens"
    );
}
