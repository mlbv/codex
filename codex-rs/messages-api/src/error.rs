//! Codex failures as Messages API error envelopes.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use serde_json::Value;
use serde_json::json;

/// HTTP 529, Anthropic's overloaded status.
const OVERLOADED: u16 = 529;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
    retry_after_secs: Option<u64>,
    should_retry: bool,
    image_request_id: Option<String>,
}

impl ApiError {
    pub(crate) fn with_image_request_id(mut self, request_id: Option<String>) -> Self {
        self.image_request_id = request_id;
        self
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after_secs: None,
            should_retry: status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
            image_request_id: None,
        }
    }

    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found_error", message)
    }

    pub(crate) fn api(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", message)
    }

    /// `prompt_tokens > context_window` is the form clients parse to trigger compaction.
    pub(crate) fn from_codex(err: &CodexErr, prompt_tokens: u64, context_window: u64) -> Self {
        let message = err.to_string();
        match err.details() {
            CodexErrorDetails::ContextWindowExceeded => Self::invalid_request(format!(
                "prompt is too long: {} tokens > {context_window} maximum",
                prompt_tokens.max(context_window.saturating_add(1)),
            )),
            CodexErrorDetails::UsageLimitReached(limit) => Self {
                retry_after_secs: limit.resets_at.map(|resets_at| {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |now| now.as_secs());
                    u64::try_from(resets_at.timestamp())
                        .map_or(0, |resets_at| resets_at.saturating_sub(now))
                }),
                should_retry: false,
                ..Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message)
            },
            CodexErrorDetails::RateLimitExceeded(_) => Self {
                retry_after_secs: err.server_retry_delay().map(|delay| delay.as_secs()),
                ..Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message)
            },
            CodexErrorDetails::QuotaExceeded | CodexErrorDetails::UsageNotIncluded => {
                Self::new(StatusCode::PAYMENT_REQUIRED, "billing_error", message)
            }
            CodexErrorDetails::RefreshTokenFailed(_) => {
                Self::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
            }
            CodexErrorDetails::ServerOverloaded | CodexErrorDetails::FlexUnavailable => Self::new(
                StatusCode::from_u16(OVERLOADED).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                "overloaded_error",
                message,
            ),
            CodexErrorDetails::ModelNotFound { .. } => Self::not_found(message),
            CodexErrorDetails::InvalidRequest(_)
            | CodexErrorDetails::InvalidPrompt { .. }
            | CodexErrorDetails::InvalidImageRequest()
            | CodexErrorDetails::ToolCollision(_)
            | CodexErrorDetails::UnsupportedOperation(_) => Self::invalid_request(message),
            CodexErrorDetails::UnexpectedStatus(response) if response.status.is_client_error() => {
                Self::new(response.status, "invalid_request_error", message)
            }
            CodexErrorDetails::Timeout | CodexErrorDetails::RequestTimeout => {
                Self::new(StatusCode::GATEWAY_TIMEOUT, "timeout_error", message)
            }
            _ => Self::api(message),
        }
    }

    pub(crate) fn body(&self) -> Value {
        json!({
            "type": "error",
            "error": { "type": self.kind, "message": self.message },
        })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, axum::Json(self.body())).into_response();
        let headers = response.headers_mut();
        if let Some(request_id) = self.image_request_id {
            headers.insert(
                "x-codex-imagegen-request-id",
                HeaderValue::from_str(&request_id).expect("upstream image request id is a header"),
            );
        }
        if let Some(seconds) = self.retry_after_secs {
            headers.insert("retry-after", HeaderValue::from(seconds));
        }
        headers.insert(
            "x-should-retry",
            HeaderValue::from_static(if self.should_retry { "true" } else { "false" }),
        );
        response
    }
}
