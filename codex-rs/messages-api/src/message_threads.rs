//! Ephemeral Messages API conversations addressed by the last completed message id.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::error::ApiError;
use crate::respond::Identity;

pub(crate) const BETA: &str = "message-threads-2026-08-12";
const TTL: Duration = Duration::from_secs(5 * 60);
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);
const FIXED_FIELDS: &[&str] = &[
    "model",
    "tool_choice",
    "thinking",
    "context_management",
    "output_config",
    "cache_control",
];

#[derive(Default)]
pub(crate) struct MessageThreads {
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    expires: Instant,
    state: State,
}

enum State {
    Ready(Snapshot),
    InFlight,
    Continued,
}

struct Snapshot {
    request: Value,
    betas: Vec<String>,
    identity: Identity,
    paused: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Create,
    Continue { previous_message_id: String },
}

pub(crate) struct Started {
    pub(crate) body: Value,
    pub(crate) identity: Identity,
    pub(crate) pending: Option<Pending>,
}

/// A continuation reserves its parent until completion or release on an unsuccessful request.
pub(crate) struct Pending {
    store: Arc<MessageThreads>,
    parent: Option<(String, Entry)>,
    snapshot: Option<Snapshot>,
}

impl MessageThreads {
    pub(crate) fn spawn_maintenance(self: &Arc<Self>) {
        let store = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PRUNE_INTERVAL).await;
                let Some(store) = store.upgrade() else {
                    break;
                };
                store
                    .entries
                    .lock()
                    .unwrap()
                    .retain(|_, entry| entry.expires > Instant::now());
            }
        });
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        mut body: Value,
        betas: &[String],
        mut identity: Identity,
    ) -> Result<Started, ApiError> {
        let operation = body.as_object_mut().and_then(|body| body.remove("thread"));
        let Some(operation) = operation.filter(|operation| !operation.is_null()) else {
            return Ok(Started {
                body,
                identity,
                pending: None,
            });
        };
        if !betas.iter().any(|beta| beta == BETA) {
            return Err(ApiError::invalid_request(format!(
                "thread: requires {BETA}"
            )));
        }
        let operation: Operation = serde_json::from_value(operation)
            .map_err(|error| ApiError::invalid_request(format!("thread: {error}")))?;
        let messages = body["messages"]
            .as_array()
            .ok_or_else(|| ApiError::invalid_request("messages: expected an array"))?;
        let mut betas = betas.to_vec();
        betas.sort();
        betas.dedup();
        let parent = match operation {
            Operation::Create => {
                check_supported(&body)?;
                if identity.session_id.is_empty() {
                    identity.session_id = format!("message-thread-{}", uuid::Uuid::new_v4());
                }
                None
            }
            Operation::Continue {
                previous_message_id,
            } => {
                for field in ["system", "tools"] {
                    if body.get(field).is_some() {
                        return Err(ApiError::invalid_request(format!(
                            "thread: {field} must be omitted on a continue request"
                        )));
                    }
                }
                let mut entries = self.entries.lock().unwrap();
                let now = Instant::now();
                entries.retain(|_, entry| entry.expires > now);
                let entry = entries.get_mut(&previous_message_id).ok_or_else(|| {
                    thread_error(
                        "thread_not_found",
                        "thread: previous message is unavailable",
                    )
                })?;
                let saved = match &entry.state {
                    State::Ready(saved) => saved,
                    State::InFlight => {
                        return Err(thread_error(
                            "thread_continue_in_flight",
                            "thread: another continuation is running",
                        ));
                    }
                    State::Continued => {
                        return Err(thread_error(
                            "thread_already_continued",
                            "thread: previous message has already been continued",
                        ));
                    }
                };
                let changed = changed_fields(&saved.request, &body, &saved.betas, &betas);
                if !changed.is_empty() {
                    return Err(thread_error(
                        "thread_fingerprint_mismatch",
                        format!("thread: fixed fields changed: {}", changed.join(", ")),
                    ));
                }
                if saved.paused != messages.is_empty() {
                    return Err(ApiError::invalid_request(if saved.paused {
                        "thread: a paused turn requires an empty messages array"
                    } else {
                        "thread: a continue request requires new messages"
                    }));
                }
                if messages
                    .first()
                    .is_some_and(|message| message["role"] == "assistant")
                {
                    return Err(ApiError::invalid_request(
                        "thread: do not resend the previous assistant response",
                    ));
                }
                for field in ["system", "tools"] {
                    if let Some(value) = saved.request.get(field) {
                        body[field] = value.clone();
                    }
                }
                check_supported(&body)?;
                let new_messages = std::mem::take(body["messages"].as_array_mut().unwrap());
                body["messages"] = Value::Array(
                    saved.request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .cloned()
                        .chain(new_messages)
                        .collect(),
                );
                identity = saved.identity.clone();
                let state = std::mem::replace(&mut entry.state, State::InFlight);
                Some((
                    previous_message_id,
                    Entry {
                        expires: entry.expires,
                        state,
                    },
                ))
            }
        };
        let snapshot = Snapshot {
            request: body.clone(),
            betas,
            identity: identity.clone(),
            paused: false,
        };
        Ok(Started {
            body,
            identity,
            pending: Some(Pending {
                store: Arc::clone(self),
                parent,
                snapshot: Some(snapshot),
            }),
        })
    }
}

impl Pending {
    /// Publish before forwarding `message_stop`, so its recipient can immediately continue.
    pub(crate) fn complete(mut self, response: Value) {
        let mut snapshot = self.snapshot.take().unwrap();
        let now = Instant::now();
        let mut entries = self.store.entries.lock().unwrap();
        if let Some((id, _)) = self.parent.take() {
            entries.insert(
                id,
                Entry {
                    expires: now + TTL,
                    state: State::Continued,
                },
            );
        }
        if response["stop_reason"] == "refusal" {
            return;
        }
        let id = response["id"].as_str().unwrap().to_string();
        snapshot.request["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "role": "assistant", "content": response["content"],
            }));
        snapshot.paused = response["stop_reason"] == "pause_turn";
        entries.insert(
            id,
            Entry {
                expires: now + TTL,
                state: State::Ready(snapshot),
            },
        );
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some((id, entry)) = self.parent.take() {
            self.store.entries.lock().unwrap().insert(id, entry);
        }
    }
}

fn changed_fields(
    previous: &Value,
    current: &Value,
    previous_betas: &[String],
    current_betas: &[String],
) -> Vec<&'static str> {
    let mut changed = FIXED_FIELDS
        .iter()
        .copied()
        .filter(|field| previous[*field] != current[*field])
        .collect::<Vec<_>>();
    if mcp_fingerprint(previous) != mcp_fingerprint(current) {
        changed.push("mcp_servers");
    }
    if previous["container"]["skills"] != current["container"]["skills"] {
        changed.push("container.skills");
    }
    if previous_betas != current_betas {
        changed.push("anthropic-beta");
    }
    changed
}

fn mcp_fingerprint(request: &Value) -> Value {
    let mut servers = request
        .get("mcp_servers")
        .cloned()
        .unwrap_or_else(|| json!([]));
    if let Some(servers) = servers.as_array_mut() {
        for server in servers {
            if let Some(server) = server.as_object_mut() {
                server.remove("authorization_token");
            }
        }
    }
    servers
}

fn check_supported(body: &Value) -> Result<(), ApiError> {
    let edited = body["context_management"]["edits"]
        .as_array()
        .is_some_and(|edits| {
            edits
                .iter()
                .any(|edit| edit["type"] != "clear_thinking_20251015" || edit["keep"] != "all")
        });
    let compacted = body["messages"].as_array().unwrap().iter().any(|message| {
        message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "compaction"))
    });
    if edited || compacted || body.get("compaction").is_some_and(|value| !value.is_null()) {
        return Err(unsupported(
            "context_management",
            "thread: context editing and compaction are unavailable",
        ));
    }
    let breakpoints = usize::from(
        body.get("cache_control")
            .is_some_and(|value| !value.is_null()),
    ) + cache_controls(&body["system"])
        + cache_controls(&body["tools"])
        + cache_controls(&body["messages"]);
    if breakpoints > 4 {
        return Err(unsupported(
            "cache_control",
            "thread: at most 4 cache_control breakpoints are allowed",
        ));
    }
    Ok(())
}

fn cache_controls(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(cache_controls).sum(),
        Value::Object(object) => {
            usize::from(
                object
                    .get("cache_control")
                    .is_some_and(|value| !value.is_null()),
            ) + object.get("content").map_or(0, cache_controls)
        }
        _ => 0,
    }
}

fn thread_error(code: &'static str, message: impl Into<String>) -> ApiError {
    let error = if code == "thread_not_found" {
        ApiError::not_found(message)
    } else {
        ApiError::invalid_request(message)
    };
    error.with_details(json!({ "error_code": code }))
}

fn unsupported(feature: &str, message: &str) -> ApiError {
    ApiError::invalid_request(message).with_details(json!({
        "error_code": "thread_unsupported_request", "unsupported": [feature],
    }))
}

#[cfg(test)]
#[path = "message_threads_tests.rs"]
mod tests;
