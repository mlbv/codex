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
