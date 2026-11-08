use codex_models_manager::bundled_models_response;
use pretty_assertions::assert_eq;

use super::*;

fn model(slug: &str) -> ModelInfo {
    bundled_models_response()
        .expect("catalog")
        .models
        .into_iter()
        .find(|model| model.slug == slug)
        .expect("model")
}

#[test]
fn unsupported_effort_steps_down_to_a_supported_one() {
    let model = model("gpt-5.5");
    assert_eq!(
        supported_effort(ReasoningEffort::Max, &model),
        ReasoningEffort::XHigh
    );
    assert_eq!(
        supported_effort(ReasoningEffort::None, &model),
        ReasoningEffort::Low
    );
    assert_eq!(
        supported_effort(ReasoningEffort::High, &model),
        ReasoningEffort::High
    );
}

#[test]
fn thinking_budgets_map_to_efforts() {
    assert_eq!(
        [1_024, 8_000, 20_000, 64_000].map(effort_for_budget),
        [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ]
    );
}

#[test]
fn message_effort_waits_for_user_input_and_survives_tool_loops() {
    let mut request: MessagesRequest = serde_json::from_value(
        serde_json::json!({"model":"m", "output_config":{"effort":"high"}, "messages":[
            {"role":"user", "content":"go"},
            {"role":"system", "content":[], "output_config":{"effort":"low"}},
            {"role":"assistant", "content":"working"},
            {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"c", "content":"ok"}]},
            {"role":"system", "content":[], "output_config":{"effort":"max"}}
        ]}),
    )
    .unwrap();
    assert_eq!(message_effort(&request), Some("low"));
    request.messages.push(
        serde_json::from_value(serde_json::json!({"role":"user", "content":"next"})).unwrap(),
    );
    assert_eq!(message_effort(&request), Some("max"));
}

#[test]
fn effort_changes_append_controls_without_changing_the_request_baseline_or_prefix() {
    let mut model = model("gpt-5.5");
    model.supports_reasoning_effort_updates = true;
    let user: ResponseItem = serde_json::from_value(serde_json::json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"go"}]})).unwrap();
    let reply: ResponseItem = serde_json::from_value(serde_json::json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]})).unwrap();
    let mut recorded = vec![user.clone()];
    assert_eq!(
        request_effort(&mut recorded, Some(ReasoningEffort::Low), &model, true),
        Some(ReasoningEffort::Low)
    );
    recorded.push(reply.clone());
    let (mut input, matched) = crate::align::align(&recorded, vec![user.clone(), reply, user]);
    assert_eq!(matched, recorded.len());
    assert_eq!(&input[..matched], recorded.as_slice());
    assert_eq!(
        request_effort(&mut input, Some(ReasoningEffort::High), &model, true),
        Some(ReasoningEffort::Low)
    );
    assert!(
        matches!(input.last(), Some(ResponseItem::ConfigurationUpdate { reasoning }) if reasoning.effort == ReasoningEffort::High)
    );
    let mut restored: Vec<ResponseItem> =
        serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
    let count = restored.len();
    assert_eq!(
        request_effort(&mut restored, Some(ReasoningEffort::High), &model, true),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(restored.len(), count);
    assert_eq!(
        request_effort(&mut restored, Some(ReasoningEffort::Medium), &model, false),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(restored.len(), count);
}
