//! Per-request model settings: Anthropic thinking and effort controls as Codex reasoning.

use codex_core::config::Config;
use codex_core::external_harness::turn_service_tier;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;

use crate::anthropic::MessagesRequest;
use crate::anthropic::ThinkingConfig;
use crate::anthropic::ThinkingDisplay;
use crate::error::ApiError;

const EFFORT_ORDER: [ReasoningEffort; 7] = [
    ReasoningEffort::None,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

#[derive(Debug)]
pub(crate) struct RequestSettings {
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) summary: ReasoningSummary,
    pub(crate) service_tier: Option<String>,
    pub(crate) display_thinking: bool,
}

pub(crate) fn resolve(
    request: &MessagesRequest,
    model_info: &ModelInfo,
    config: &Config,
) -> Result<RequestSettings, ApiError> {
    let requested = message_effort(request).or_else(|| {
        request
            .output_config
            .as_ref()
            .and_then(|output| output.effort.as_deref())
    });
    let effort =
        match (requested, &request.thinking) {
            (Some(effort), _) => Some(effort.parse::<ReasoningEffort>().map_err(|err| {
                ApiError::invalid_request(format!("output_config.effort: {err}"))
            })?),
            (None, Some(ThinkingConfig::Enabled { budget_tokens, .. })) => {
                Some(effort_for_budget(*budget_tokens))
            }
            (None, Some(ThinkingConfig::Disabled | ThinkingConfig::BetweenTools)) => {
                Some(ReasoningEffort::None)
            }
            (None, Some(ThinkingConfig::Adaptive { .. }) | None) => {
                config.model_reasoning_effort.clone()
            }
        };
    let display = match &request.thinking {
        Some(ThinkingConfig::Enabled { display, .. } | ThinkingConfig::Adaptive { display }) => {
            *display
        }
        Some(ThinkingConfig::Disabled | ThinkingConfig::BetweenTools) | None => None,
    };
    let speed = match request.speed.as_deref() {
        None | Some("standard") => None,
        Some("fast") => Some("priority".to_string()),
        Some(speed) => {
            return Err(ApiError::invalid_request(format!(
                "speed: unknown speed `{speed}`"
            )));
        }
    };
    let tier = match request.service_tier.as_deref() {
        None | Some("auto") => speed,
        Some("standard_only") => Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()),
        Some(tier) => {
            return Err(ApiError::invalid_request(format!(
                "service_tier: unknown tier `{tier}`"
            )));
        }
    };
    Ok(RequestSettings {
        effort: effort.map(|effort| supported_effort(effort, model_info)),
        summary: config
            .model_reasoning_summary
            .unwrap_or(model_info.default_reasoning_summary),
        service_tier: turn_service_tier(config, tier, model_info),
        display_thinking: !matches!(display, Some(ThinkingDisplay::Omitted)),
    })
}

/// Effort changes after a tool-result-only message wait for the next user input.
fn message_effort(request: &MessagesRequest) -> Option<&str> {
    use crate::anthropic::{ContentBlock, MessageContent, Role};
    let mut selected = None;
    let mut pending = None;
    let mut after_user_input = false;
    for message in &request.messages {
        match message.role {
            Role::User => {
                after_user_input = match &message.content {
                    MessageContent::Text(text) => !text.is_empty(),
                    MessageContent::Blocks(blocks) => blocks
                        .iter()
                        .any(|block| !matches!(block, ContentBlock::ToolResult { .. })),
                };
                if after_user_input && pending.is_some() {
                    selected = pending.take();
                }
            }
            Role::Assistant => after_user_input = false,
            Role::System => {
                if let Some(effort) = message
                    .output_config
                    .as_ref()
                    .and_then(|output| output.effort.as_deref())
                {
                    if after_user_input {
                        selected = Some(effort);
                        pending = None;
                    } else {
                        pending = Some(effort);
                    }
                }
            }
        }
    }
    selected
}

/// Keep the first request effort while updates in durable history select later efforts.
pub(crate) fn request_effort(
    input: &mut Vec<ResponseItem>,
    selected: Option<ReasoningEffort>,
    model: &ModelInfo,
    updates_enabled: bool,
) -> Option<ReasoningEffort> {
    if !updates_enabled {
        return selected;
    }
    let effort = model.resolve_reasoning_effort(
        selected
            .clone()
            .or_else(|| model.default_reasoning_level.clone())?,
    );
    if matches!(&effort, ReasoningEffort::Custom(value) if value != "disabled") {
        return selected;
    }
    let mut updates = input.iter().filter_map(|item| match item {
        ResponseItem::ConfigurationUpdate { reasoning } => Some(&reasoning.effort),
        _ => None,
    });
    let baseline = updates.next().cloned().unwrap_or_else(|| effort.clone());
    let latest = updates
        .next_back()
        .cloned()
        .unwrap_or_else(|| baseline.clone());
    if latest != effort
        || !input
            .iter()
            .any(|item| matches!(item, ResponseItem::ConfigurationUpdate { .. }))
    {
        input.push(ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning { effort },
        });
    }
    Some(baseline)
}

fn effort_for_budget(budget_tokens: u64) -> ReasoningEffort {
    match budget_tokens {
        0..4_096 => ReasoningEffort::Low,
        4_096..16_384 => ReasoningEffort::Medium,
        16_384..32_768 => ReasoningEffort::High,
        _ => ReasoningEffort::XHigh,
    }
}

/// The strongest supported effort not above `effort`; else the model's weakest.
fn supported_effort(effort: ReasoningEffort, model_info: &ModelInfo) -> ReasoningEffort {
    let supported = |candidate: &ReasoningEffort| {
        model_info
            .supported_reasoning_levels
            .iter()
            .any(|preset| &preset.effort == candidate)
    };
    if supported(&effort) {
        return effort;
    }
    let Some(rank) = EFFORT_ORDER.iter().position(|level| level == &effort) else {
        return effort;
    };
    EFFORT_ORDER[..rank]
        .iter()
        .rev()
        .find(|level| supported(level))
        .or_else(|| EFFORT_ORDER[rank..].iter().find(|level| supported(level)))
        .cloned()
        .unwrap_or(effort)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
