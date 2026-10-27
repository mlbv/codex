//! HTTP surface: `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`, `/v1/images/*`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use codex_models_manager::manager::RefreshStrategy;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;

use crate::anthropic::MessagesRequest;
use crate::backend::Backend;
use crate::error::ApiError;
use crate::images;
use crate::respond;
use crate::respond::Identity;

const SESSION_HEADER: &str = "x-claude-code-session-id";
const AGENT_HEADER: &str = "x-claude-code-agent-id";
const PARENT_AGENT_HEADER: &str = "x-claude-code-parent-agent-id";
/// `speed` for requests whose body sets none.
const SPEED_HEADER: &str = "x-codex-speed";
const PING_INTERVAL: Duration = Duration::from_secs(15);
const EVENT_BUFFER: usize = 1024;
/// The Messages API's own request size limit.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    backend: Arc<Backend>,
    listen: SocketAddr,
}

pub(crate) fn router(backend: Arc<Backend>, listen: SocketAddr) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{id}", get(get_model))
        .route("/v1/images/generations", post(generations))
        .route("/v1/images/edits", post(edits))
        .route("/admin/status", get(status))
        .fallback(not_found)
        // Long conversations with media exceed axum's 2 MB default.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(axum::middleware::map_response(with_request_id))
        .with_state(AppState { backend, listen })
}

async fn status(State(state): State<AppState>) -> Response {
    Json(json!({ "lockout": state.backend.watchdog.lockout() })).into_response()
}

async fn not_found(uri: axum::http::Uri) -> Response {
    ApiError::not_found(format!("{} is not served", uri.path())).into_response()
}

async fn with_request_id(mut response: Response) -> Response {
    if let Ok(value) = HeaderValue::from_str(&format!("req_{}", uuid::Uuid::new_v4().simple())) {
        response.headers_mut().insert("request-id", value);
    }
    response
}

async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(reason) = state.backend.watchdog.lockout() {
        return ApiError::invalid_request(format!(
            "codex messages-api locked out: {reason}. Audit and fix, then delete {} and restart the proxy.",
            state.backend.watchdog.path().display()
        ))
        .into_response();
    }
    let request = match parse_request(&state, &headers, &body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    let identity = identity(&headers, &request);
    let streaming = request.stream;
    let prepared = match respond::prepare(&state.backend, request, &betas(&headers)).await {
        Ok(prepared) => prepared,
        Err(error) => return error.into_response(),
    };
    let (tx, mut rx) = mpsc::channel(EVENT_BUFFER);
    let task = tokio::spawn(respond::respond(
        Arc::clone(&state.backend),
        identity,
        prepared,
        tx,
    ));
    let Some(first) = rx.recv().await else {
        return match task.await {
            Ok(Err(error)) => error.into_response(),
            Ok(Ok(())) => ApiError::api("the response ended without events").into_response(),
            Err(err) => ApiError::api(format!("response task failed: {err}")).into_response(),
        };
    };
    if streaming {
        sse_response(first, rx)
    } else {
        collect_message(first, rx).await
    }
}

async fn count_tokens(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = match parse_request(&state, &headers, &body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    match respond::prepare(&state.backend, request, &betas(&headers)).await {
        Ok(prepared) => {
            Json(json!({ "input_tokens": respond::count_tokens(&prepared) })).into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn generations(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    images(state, headers, body, images::Operation::Generate).await
}

async fn edits(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    images(state, headers, body, images::Operation::Edit).await
}

async fn images(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    operation: images::Operation,
) -> Response {
    if let Err(error) = check_json_request(&state, &headers) {
        return error.into_response();
    }
    let request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => return ApiError::invalid_request(format!("body: {err}")).into_response(),
    };
    let image_context = match image_context(&headers) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    match images::run(
        &state.backend,
        operation,
        request,
        &image_context.0,
        image_context.1,
    )
    .await
    {
        Ok((body, request_id)) => {
            let mut response = Json(body).into_response();
            if let Some(request_id) = request_id {
                response.headers_mut().insert(
                    "x-codex-imagegen-request-id",
                    HeaderValue::from_str(&request_id)
                        .expect("upstream image request id is a header"),
                );
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

fn image_context(headers: &HeaderMap) -> Result<(String, Option<&str>), ApiError> {
    let value = |name| {
        headers
            .get(name)
            .map(HeaderValue::to_str)
            .transpose()
            .map_err(|_| ApiError::invalid_request(format!("{name}: not visible ASCII")))
    };
    let turn_id = value("x-codex-image-turn-id")?
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Ok((turn_id, value(codex_core::X_CODEX_TURN_METADATA_HEADER)?))
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = check_host(&state, &headers) {
        return error.into_response();
    }
    let models = model_entries(&state.backend).await;
    let first_id = models.first().map(|model| model["id"].clone());
    let last_id = models.last().map(|model| model["id"].clone());
    Json(json!({
        "data": models,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = check_host(&state, &headers) {
        return error.into_response();
    }
    match model_entries(&state.backend)
        .await
        .into_iter()
        .find(|model| model["id"] == id.as_str())
    {
        Some(model) => Json(model).into_response(),
        None => ApiError::not_found(format!("model: {id}")).into_response(),
    }
}

async fn model_entries(backend: &Backend) -> Vec<Value> {
    let presets = backend
        .models
        .list_models(
            RefreshStrategy::OnlineIfUncached,
            backend.config.http_client_factory(),
        )
        .await;
    let mut entries = Vec::new();
    for preset in presets {
        let info = backend
            .models
            .get_model_info(&preset.model, &backend.config.to_models_manager_config())
            .await;
        entries.push(json!({
            "type": "model",
            "id": preset.model,
            "display_name": preset.display_name,
            "description": preset.description,
            "created_at": "1970-01-01T00:00:00Z",
            "max_input_tokens": info.context_window,
            "auto_compact_token_limit": info.auto_compact_token_limit(),
        }));
    }
    entries
}

fn parse_request(
    state: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<MessagesRequest, ApiError> {
    check_json_request(state, headers)?;
    let version = headers
        .get("anthropic-version")
        .map(HeaderValue::to_str)
        .transpose()
        .map_err(|_| ApiError::invalid_request("anthropic-version: not visible ASCII"))?;
    let mut request = crate::request::parse(body, version, &betas(headers))?;
    default_speed(&mut request, headers)?;
    Ok(request)
}

fn check_json_request(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    check_host(state, headers)?;
    // A JSON content type forces a CORS preflight on browser requests, which this server denies.
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return Err(ApiError::invalid_request(
            "content-type must be application/json",
        ));
    }
    Ok(())
}

fn default_speed(request: &mut MessagesRequest, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(value) = headers
        .get(SPEED_HEADER)
        .filter(|_| request.speed.is_none())
    else {
        return Ok(());
    };
    let speed = value
        .to_str()
        .map_err(|_| ApiError::invalid_request(format!("{SPEED_HEADER}: not visible ASCII")))?;
    request.speed = Some(speed.to_string());
    Ok(())
}

/// Loopback names only, so rebinding a public hostname to 127.0.0.1 reaches nothing.
fn check_host(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let port = state.listen.port();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let allowed = ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|name| host == *name || host == format!("{name}:{port}"));
    if allowed {
        Ok(())
    } else {
        Err(ApiError::invalid_request(format!(
            "host `{host}` is not served"
        )))
    }
}

/// `anthropic-beta` flags, comma-separated across header values.
fn betas(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|beta| beta.trim().to_string())
        .filter(|beta| !beta.is_empty())
        .collect()
}

fn identity(headers: &HeaderMap, request: &MessagesRequest) -> Identity {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
            .map(str::to_string)
    };
    let session_id = header(SESSION_HEADER)
        .or_else(|| {
            let user_id = request.metadata.as_ref()?.user_id.as_deref()?;
            serde_json::from_str::<Value>(user_id)
                .ok()?
                .get("session_id")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default();
    Identity {
        session_id,
        agent_id: header(AGENT_HEADER),
        parent_agent_id: header(PARENT_AGENT_HEADER),
    }
}

fn sse_response(first: Value, rx: mpsc::Receiver<Value>) -> Response {
    let events = futures::stream::unfold((Some(first), rx), |(pending, mut rx)| async move {
        let event = match pending {
            Some(event) => Some(event),
            None => match tokio::time::timeout(PING_INTERVAL, rx.recv()).await {
                Ok(event) => event,
                Err(_) => Some(json!({ "type": "ping" })),
            },
        }?;
        let name = event["type"].as_str().unwrap_or("message").to_string();
        let frame = Bytes::from(format!("event: {name}\ndata: {event}\n\n"));
        Some((Ok::<_, Infallible>(frame), (None, rx)))
    });
    let mut response = Response::new(Body::from_stream(events));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Folds the event stream into the non-streaming `Message` response.
async fn collect_message(first: Value, mut rx: mpsc::Receiver<Value>) -> Response {
    let mut message = Value::Null;
    let mut blocks: Vec<Value> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut event = Some(first);
    while let Some(current) = event {
        match current["type"].as_str() {
            Some("message_start") => message = current["message"].clone(),
            Some("content_block_start") => {
                blocks.push(current["content_block"].clone());
                inputs.push(String::new());
            }
            Some("content_block_delta") => {
                let delta = &current["delta"];
                if let (Some(block), Some(input)) = (blocks.last_mut(), inputs.last_mut()) {
                    match delta["type"].as_str() {
                        Some("text_delta") => append(block, "text", &delta["text"]),
                        Some("thinking_delta") => append(block, "thinking", &delta["thinking"]),
                        Some("signature_delta") => block["signature"] = delta["signature"].clone(),
                        Some("citations_delta") => match block["citations"].as_array_mut() {
                            Some(citations) => citations.push(delta["citation"].clone()),
                            None => block["citations"] = json!([delta["citation"]]),
                        },
                        Some("input_json_delta") => {
                            input.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
            }
            Some("content_block_stop") => {
                if let (Some(block), Some(input)) = (blocks.last_mut(), inputs.last())
                    && !input.is_empty()
                {
                    block["input"] = serde_json::from_str(input).unwrap_or(Value::Null);
                }
            }
            Some("message_delta") => {
                for (key, value) in current["delta"].as_object().into_iter().flatten() {
                    message[key] = value.clone();
                }
                message["usage"] = current["usage"].clone();
            }
            Some("error") => return error_response(current),
            _ => {}
        }
        event = rx.recv().await;
    }
    message["content"] = Value::Array(blocks);
    Json(message).into_response()
}

fn append(block: &mut Value, field: &str, text: &Value) {
    let joined = format!(
        "{}{}",
        block[field].as_str().unwrap_or_default(),
        text.as_str().unwrap_or_default()
    );
    block[field] = Value::String(joined);
}

fn error_response(body: Value) -> Response {
    let status = match body["error"]["type"].as_str() {
        Some("invalid_request_error") => axum::http::StatusCode::BAD_REQUEST,
        Some("authentication_error") => axum::http::StatusCode::UNAUTHORIZED,
        Some("billing_error") => axum::http::StatusCode::PAYMENT_REQUIRED,
        Some("rate_limit_error") => axum::http::StatusCode::TOO_MANY_REQUESTS,
        Some("overloaded_error") => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
