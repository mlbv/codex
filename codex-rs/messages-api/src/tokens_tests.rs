use super::*;
use crate::executor::Executor;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ToolDefinition;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use serde_json::json;

const MODEL: &str = "gpt-test";

fn item(value: serde_json::Value) -> ResponseItem {
    serde_json::from_value(value).unwrap()
}

fn user(text: &str) -> ResponseItem {
    item(json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}))
}

fn reasoning(bytes: usize) -> ResponseItem {
    item(json!({"type":"reasoning","summary":[],"encrypted_content":"x".repeat(bytes)}))
}

#[test]
fn measured_context_does_not_recount_encrypted_history_after_a_tool_only_response() {
    let mut history = vec![user("work"), reasoning(1_000_000)];
    let usage = TokenUsage {
        input_tokens: 278_646,
        output_tokens: 1_259,
        total_tokens: 279_905,
        ..Default::default()
    };
    let context = ContextUsage::measured(&usage, None, 1, &history, 2, MODEL);
    assert_eq!(context.estimate(&history, MODEL), 279_905);
    history.push(user("next"));
    let restored: ContextUsage =
        serde_json::from_value(serde_json::to_value(context).unwrap()).unwrap();
    assert_eq!(restored.estimate(&history, MODEL), 279_906);
    history.push(user("more"));
    assert_eq!(restored.estimate(&history, MODEL), 279_907);
}

#[test]
fn excluded_reasoning_is_explicit_and_tied_to_its_measured_boundary() {
    let mut history = vec![user("old"), reasoning(2_000), user("new"), reasoning(3_000)];
    let usage = TokenUsage {
        total_tokens: 1_000,
        ..Default::default()
    };
    for (included, expected) in [(None, 1_000), (Some(true), 1_000), (Some(false), 1_213)] {
        let context = ContextUsage::measured(&usage, included, 3, &history, 4, MODEL);
        assert_eq!(context.estimate(&history, MODEL), expected);
        history.push(user("next"));
        assert_eq!(context.estimate(&history, MODEL), expected + 1);
        let replacement =
            ContextUsage::measured(&usage, None, history.len(), &history, history.len(), MODEL);
        assert_eq!(replacement.estimate(&history, MODEL), 1_000);
        history.pop();
    }
}

#[test]
fn generated_exec_calls_are_covered_when_their_projection_is_installed() {
    let mut history = vec![user("work")];
    let usage = TokenUsage {
        total_tokens: 1_000,
        ..Default::default()
    };
    let mut context = ContextUsage::measured(&usage, None, 1, &history, 1, MODEL);
    context.cover_execution(&history, 1, MODEL);
    history.push(item(
        json!({"type":"custom_tool_call","name":"exec","call_id":"a","input":"x".repeat(10_000)}),
    ));
    assert_eq!(context.estimate(&history, MODEL), 1_000);
    history.push(item(
        json!({"type":"custom_tool_call_output","call_id":"a","output":"1234567"}),
    ));
    assert_eq!(context.estimate(&history, MODEL), 1_002);
    context.cover_execution(&history, 1, MODEL);
    history.push(item(
        json!({"type":"custom_tool_call","name":"exec","call_id":"b","input":"y".repeat(10_000)}),
    ));
    assert_eq!(context.estimate(&history, MODEL), 1_002);
}

#[tokio::test]
async fn serial_executor_waves_keep_measured_context_through_restart() {
    let mut executor = Executor::default();
    let mut history = vec![user("work"), reasoning(1_000_000)];
    let call = item(
        json!({"type":"custom_tool_call","call_id":"exec-1","name":"exec","input":"await tools.echo({}); await tools.echo({}); await tools.echo({}); text('done');"}),
    );
    let definitions = vec![ToolDefinition {
        name: "echo".to_string(),
        tool_name: ToolName {
            name: "Echo".to_string(),
            namespace: None,
        },
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: Some(json!({"type":"object"})),
        input_schema_max_bytes: None,
        output_schema: None,
    }];
    let first = executor
        .start(call, definitions, history.len())
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    history.extend(first);
    let context = ContextUsage::measured(
        &TokenUsage {
            input_tokens: 278_646,
            output_tokens: 1_259,
            total_tokens: 279_905,
            ..Default::default()
        },
        None,
        1,
        &executor.project(&history),
        2,
        MODEL,
    );
    for wave in 0..3 {
        assert_eq!(
            context.estimate(&executor.project(&history), MODEL),
            279_905
        );
        let ResponseItem::FunctionCall { call_id, .. } = history.last().unwrap() else {
            panic!("client call")
        };
        let result = item(
            json!({"type":"function_call_output","call_id":call_id,"output":"a".repeat(100_000)}),
        );
        history.push(result);
        executor = Executor::restore(
            serde_json::from_slice(&serde_json::to_vec(executor.state()).unwrap()).unwrap(),
        );
        let next = executor.resume(&history).await.unwrap();
        assert_eq!(next.len(), usize::from(wave < 2));
        history.extend(next);
    }
    assert!(!executor.pending());
    let tokens = context.estimate(&executor.project(&history), MODEL);
    assert!(
        (279_905..280_005).contains(&tokens),
        "context after bounded exec output: {tokens}"
    );
}

#[test]
fn old_prompt_records_load_without_inferred_reasoning_coverage() {
    let saved: crate::persist::LoggedPrompt =
        serde_json::from_value(json!({"items":10,"input_tokens":278_646})).unwrap();
    assert_eq!(saved.context, None);
}
