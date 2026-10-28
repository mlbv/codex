//! Versioned Messages request validation before typed decoding drops unused fields.

use serde_json::Value;

use crate::anthropic::MessagesRequest;
use crate::error::ApiError;

pub(crate) const API_VERSION: &str = "2026-10-21";

pub(crate) fn parse(
    body: &[u8],
    version: Option<&str>,
    betas: &[String],
) -> Result<MessagesRequest, ApiError> {
    let current = match version {
        None | Some(API_VERSION) => true,
        Some("2023-06-01") => false,
        Some(version) => {
            return Err(ApiError::invalid_request(format!(
                "anthropic-version: unsupported version `{version}`"
            )));
        }
    };
    let body: Value = serde_json::from_slice(body)
        .map_err(|error| ApiError::invalid_request(error.to_string()))?;
    if current {
        for name in ["temperature", "top_p", "top_k"] {
            if body.get(name).is_some() {
                return Err(ApiError::invalid_request(format!(
                    "{name}: removed in API version {API_VERSION}"
                )));
            }
        }
    }
    if let Some(messages) = body["messages"].as_array() {
        for (index, message) in messages.iter().enumerate() {
            let system = message["role"] == "system";
            let path = format!("messages.{index}");
            if let Some(clear_at) = message.get("clear_at") {
                if !system
                    || !betas
                        .iter()
                        .any(|beta| beta == "mid-conversation-system-clear-at-2026-08-21")
                {
                    return Err(ApiError::invalid_request(format!(
                        "{path}.clear_at: requires a system message and mid-conversation-system-clear-at-2026-08-21"
                    )));
                }
                if clear_at == "next_user_message" {
                    if message.get("output_config").is_some() {
                        return Err(ApiError::invalid_request(format!(
                            "{path}: output_config is not permitted on a turn-scoped system message"
                        )));
                    }
                    if let Some(blocks) = message["content"].as_array() {
                        for block in blocks {
                            if block["type"] != "text" || block.get("cache_control").is_some() {
                                return Err(ApiError::invalid_request(format!(
                                    "{path}: turn-scoped messages support text only, without cache_control"
                                )));
                            }
                        }
                    }
                }
            }
            if let Some(output) = message.get("output_config") {
                if !system
                    || !betas.iter().any(|beta| {
                        beta == "mid-conversation-output-config-2026-07-01"
                            || beta == "per-turn-control-2026-07-01"
                    })
                {
                    return Err(ApiError::invalid_request(format!(
                        "{path}.output_config: requires a system message and mid-conversation-output-config-2026-07-01"
                    )));
                }
                if output.get("format").is_some() {
                    return Err(ApiError::invalid_request(format!(
                        "{path}.output_config: only effort can change per message"
                    )));
                }
            }
            if let Some(blocks) = message["content"].as_array() {
                for block in blocks {
                    if matches!(
                        block["type"].as_str(),
                        Some("tool_addition" | "tool_removal")
                    ) {
                        if !system
                            || !betas.iter().any(|beta| {
                                beta == "inline-tools-2026-09-15"
                                    || beta == "mid-conversation-tool-changes-2026-07-01"
                            })
                        {
                            return Err(ApiError::invalid_request(format!(
                                "{path}: tool changes require a system message and a tool-changes beta"
                            )));
                        }
                        if block["tool"]["type"] == "tool_definition"
                            && !betas.iter().any(|beta| beta == "inline-tools-2026-09-15")
                        {
                            return Err(ApiError::invalid_request(format!(
                                "{path}: defining an inline tool requires inline-tools-2026-09-15"
                            )));
                        }
                    }
                    if current && message["role"] == "assistant" && block["type"] == "document" {
                        return Err(ApiError::invalid_request(format!(
                            "{path}: documents belong in user messages"
                        )));
                    }
                }
                if current && message["role"] == "user" {
                    reject_user_citations(blocks, &path)?;
                }
            }
        }
    }
    if body["thinking"]["type"] == "between_tools"
        && body["thinking"]
            .as_object()
            .is_some_and(|thinking| thinking.len() != 1)
    {
        return Err(ApiError::invalid_request(
            "thinking: between_tools accepts no other fields",
        ));
    }
    serde_json::from_value(body).map_err(|error| ApiError::invalid_request(error.to_string()))
}

fn reject_user_citations(blocks: &[Value], path: &str) -> Result<(), ApiError> {
    for block in blocks {
        if block["type"] == "text"
            && block["citations"]
                .as_array()
                .is_some_and(|citations| !citations.is_empty())
        {
            return Err(ApiError::invalid_request(format!(
                "{path}: user text citations are not accepted in API version {API_VERSION}"
            )));
        }
        if let Some(content) = block["content"].as_array() {
            reject_user_citations(content, path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
