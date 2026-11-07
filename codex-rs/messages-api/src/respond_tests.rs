use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[tokio::test]
async fn lite_inline_tools_and_per_message_effort_keep_the_initial_prompt_catalog() {
    let home = tempfile::tempdir().unwrap();
    let mut models = codex_models_manager::bundled_models_response().unwrap();
    let model = models
        .models
        .iter_mut()
        .find(|model| model.slug == "gpt-5.5")
        .unwrap();
    model.use_responses_lite = true;
    model.supports_reasoning_effort_updates = true;
    let backend = crate::test_support::backend(home.path(), models, |_| {}).await;
    let mut body = json!({"model":"gpt-5.5", "output_config":{"effort":"low"}, "tools":[{"name":"Read", "input_schema":{"type":"object"}}], "messages":[{"role":"user", "content":"go"}]});
    let first = prepare(&backend, serde_json::from_value(body.clone()).unwrap(), &[])
        .await
        .unwrap();
    body["messages"] = json!([
        {"role":"user", "content":"go"}, {"role":"assistant", "content":"done"}, {"role":"user", "content":"next"},
        {"role":"system", "content":[], "output_config":{"effort":"high"}},
        {"role":"system", "content":[{"type":"tool_addition", "tool":{"type":"tool_definition", "definition":{"name":"Write", "input_schema":{"type":"object", "properties":{"path":{"type":"string"}}}}}}]}
    ]);
    let next = prepare(
        &backend,
        serde_json::from_value(body).unwrap(),
        &["inline-tools-2026-09-15".to_string()],
    )
    .await
    .unwrap();
    assert!(next.model_info.use_responses_lite);
    assert_eq!(
        serde_json::to_value(&first.parts.tools).unwrap(),
        serde_json::to_value(&next.parts.tools).unwrap()
    );
    assert_eq!(first.parts.instructions.text, next.parts.instructions.text);
    assert_eq!(
        next.settings.effort,
        Some(codex_protocol::openai_models::ReasoningEffort::High)
    );
    assert!(next.tools.is_available(None, "Write"));
    assert!(next.conversation.items.iter().any(|item| matches!(item, ResponseItem::AdditionalTools { tools, .. } if serde_json::to_string(tools).unwrap().contains("Write"))));
}

#[test]
fn forced_tools_use_a_function_choice_or_a_narrowed_namespace() {
    let tools = ToolSet::from_request(&[
        json!({ "name": "Read", "input_schema": { "type": "object" } }),
        json!({ "name": "mcp__docs__search", "input_schema": { "type": "object" } }),
        json!({ "name": "mcp__docs__fetch", "input_schema": { "type": "object" } }),
    ])
    .expect("tools");
    let top_level = forced_tool("Read", &tools, &tools.specs).expect("forced");
    assert_eq!(
        (top_level.tool_choice, top_level.tools.is_none()),
        (
            CodexToolChoice::Function {
                name: "Read".to_string()
            },
            true
        )
    );
    let namespaced = forced_tool("mcp__docs__search", &tools, &tools.specs).expect("forced");
    let Some([ToolSpec::Namespace(namespace)]) = namespaced.tools.as_deref() else {
        panic!("one namespace");
    };
    assert_eq!(
        (namespaced.tool_choice, namespace.tools.len()),
        (CodexToolChoice::Required, 1)
    );
    assert!(forced_tool("Write", &tools, &tools.specs).is_err());
}

#[test]
fn commentary_section_runs_to_the_next_heading_of_its_level_or_above() {
    let template = "# Working\n\n## Intermediate commentary\n\nUpdate often.\n\n### Detail\n\nKeep.\n\n## Final answer\n\nShort.";
    assert_eq!(
        commentary_section(template),
        Some("## Intermediate commentary\n\nUpdate often.\n\n### Detail\n\nKeep.")
    );
    assert_eq!(
        commentary_section("## Intermediate commentary\n\nLast."),
        Some("## Intermediate commentary\n\nLast.")
    );
    assert_eq!(commentary_section("# Working\n\nNothing."), None);
}
