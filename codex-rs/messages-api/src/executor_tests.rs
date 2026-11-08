use super::*;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;

fn pending_state(round_count: usize) -> ExecutorState {
    let rounds = (0..round_count)
        .map(|index| Round {
            content: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                text: format!("{index:08}"),
            }],
            calls: vec![
                serde_json::from_value(json!({
                    "type": "function_call", "call_id": format!("round-{index:08}"),
                    "name": "Echo", "arguments": "{}",
                }))
                .unwrap(),
            ],
            results: (index + 1 < round_count).then(|| vec![json!("completed")]),
        })
        .collect::<Vec<_>>();
    ExecutorState {
        cells: vec![Cell {
            at: Some(0),
            call: exec("delta", "for (;;) { text(await tools.echo({})); }"),
            request: Some(Operation::Exec(ExecArgs {
                tool_call_id: "delta".to_string(),
                enabled_tools: ToolCatalog::new(tools()),
                source: "for (;;) { text(await tools.echo({})); }".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            })),
            rounds,
            response: None,
            output: None,
        }],
    }
}

fn assert_state(actual: &ExecutorState, expected: &ExecutorState) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn pending_cell_delta_size_is_independent_of_recorded_round_count() {
    let mut sizes = Vec::new();
    for count in [1, 32, 1024] {
        let mut previous = pending_state(count);
        // Neither a large prior frontier nor the results the history holds are repeated.
        previous.cells[0].rounds.last_mut().unwrap().content[0] =
            codex_code_mode::FunctionCallOutputContentItem::InputText {
                text: "1".repeat(64_000),
            };
        let answered = &previous.cells[0].rounds.last().unwrap().calls[0];
        let history = [result(answered, &"2".repeat(4096))];
        let mut current = previous.clone();
        current.cells[0].rounds.last_mut().unwrap().results = Some(vec![json!("2".repeat(4096))]);
        let next = pending_state(count + 1).cells[0]
            .rounds
            .last()
            .unwrap()
            .clone();
        current.cells[0].rounds.push(next);
        let update = current.update(&previous);
        assert!(update.append.is_empty());
        assert!(update.amend.as_ref().unwrap().results.is_some());
        let encoded = serde_json::to_vec(&update).unwrap();
        assert!(encoded.len() < 1000);
        sizes.push(encoded.len());
        let update = serde_json::from_slice(&encoded).unwrap();
        previous.apply(update, &history).unwrap();
        assert_state(&previous, &current);
    }
    assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() < 16);
}

#[test]
fn cell_deltas_round_trip_result_clearing_round_truncation_and_completion() {
    let previous = pending_state(4);
    let history = previous.cells[0]
        .rounds
        .iter()
        .take(2)
        .flat_map(|round| round.calls.iter().cloned())
        .collect::<Vec<_>>();
    let prefix = previous.prefix(&history);
    assert_eq!(prefix.cells[0].rounds.len(), 1);
    assert!(prefix.cells[0].rounds[0].results.is_none());
    let mut applied = previous.clone();
    applied
        .apply(
            serde_json::from_value(serde_json::to_value(prefix.update(&applied)).unwrap()).unwrap(),
            &history,
        )
        .unwrap();
    assert_state(&applied, &prefix);
    let mut complete = prefix.clone();
    complete.cells[0].rounds[0].results = Some(vec![json!("answer")]);
    complete.cells[0].response = Some(RuntimeResponse::Result {
        cell_id: CellId::new("cell".to_string()),
        content_items: Vec::new(),
        error_text: None,
        result_buffered_for: std::time::Duration::ZERO,
        code_mode_host_duration: None,
    });
    complete.cells[0].output = Some(result(&complete.cells[0].call, "completed"));
    let answered = [
        history.clone(),
        vec![result(&complete.cells[0].rounds[0].calls[0], "answer")],
    ]
    .concat();
    applied.apply(complete.update(&applied), &answered).unwrap();
    assert_state(&applied, &complete);
    applied.apply(prefix.update(&applied), &history).unwrap();
    assert_state(&applied, &prefix);
}

#[test]
fn compaction_delta_does_not_repeat_kept_cells_and_can_append_a_new_cell() {
    for count in [1, 1024] {
        let cell = pending_state(1).cells.pop().unwrap();
        let previous = ExecutorState {
            cells: vec![cell; count],
        };
        let mut executor = Executor::restore(previous.clone());
        executor.compact();
        let update = executor.state().update(&previous);
        assert_eq!(update.keep, count);
        assert_eq!(update.compact, count);
        assert!(update.append.is_empty());
        assert!(update.amend.is_none());
        assert!(serde_json::to_vec(&update).unwrap().len() < 80);
        let mut applied = previous.clone();
        applied.apply(update, &[]).unwrap();
        assert_state(&applied, executor.state());
        executor.state.cells.extend(pending_state(1).cells);
        applied = previous;
        applied
            .apply(executor.state().update(&applied), &[])
            .unwrap();
        assert_state(&applied, executor.state());
    }
}

#[test]
fn repeated_catalogs_share_memory_across_cells_and_snapshots() {
    let mut state = pending_state(1);
    let catalog = state.intern_catalog(tools());
    let Some(Operation::Exec(request)) = &mut state.cells[0].request else {
        panic!("exec")
    };
    assert!(Arc::ptr_eq(
        catalog.tools.as_ref().unwrap(),
        request.enabled_tools.tools.as_ref().unwrap()
    ));
    let snapshot = state.clone();
    let Some(Operation::Exec(cloned)) = &snapshot.cells[0].request else {
        panic!("exec")
    };
    assert!(Arc::ptr_eq(
        catalog.tools.as_ref().unwrap(),
        cloned.enabled_tools.tools.as_ref().unwrap()
    ));
}

fn exec(id: &str, source: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "custom_tool_call", "call_id": id, "name": "exec", "input": source,
    }))
    .unwrap()
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "echo".to_string(),
        tool_name: ToolName {
            name: "Echo".to_string(),
            namespace: Some("client".to_string()),
        },
        description: "Echoes the input".to_string(),
        kind: CodeModeToolKind::Function,
        input_schema: Some(json!({"type": "object"})),
        input_schema_max_bytes: None,
        output_schema: None,
    }]
}

fn wait(id: &str, cell_id: &str, yield_time_ms: u64, terminate: bool) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "function_call", "call_id": id, "name": "wait",
        "arguments": json!({"cell_id": cell_id, "yield_time_ms": yield_time_ms, "terminate": terminate}).to_string(),
    })).unwrap()
}

fn yielded_id(executor: &Executor) -> String {
    let Some(RuntimeResponse::Yielded { cell_id, .. }) =
        &executor.state.cells.last().unwrap().response
    else {
        panic!("expected yielded script")
    };
    cell_id.to_string()
}

fn result(call: &ResponseItem, text: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "function_call_output", "call_id": item_call_id(call).unwrap(), "output": text,
    }))
    .unwrap()
}

fn output_text(executor: &Executor) -> String {
    let output = executor
        .state
        .cells
        .last()
        .unwrap()
        .output
        .as_ref()
        .unwrap();
    let payload = match output {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => panic!("expected exec output"),
    };
    payload.body.to_text().unwrap()
}

fn restored(executor: &Executor) -> Executor {
    Executor::restore(
        serde_json::from_slice(&serde_json::to_vec(executor.state()).unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn timed_native_observation_preserves_unanswered_tool_ids_for_the_frontier() {
    let session = InProcessCodeModeSession::new();
    let delegate = Arc::new(Delegate::default());
    let started = session
        .execute(
            ExecuteRequest {
                tool_call_id: "timed-frontier".to_string(),
                enabled_tools: tools(),
                source: "await Promise.all([tools.echo({n: 1}), tools.echo({n: 2})]);".to_string(),
                yield_time_ms: Some(1),
                max_output_tokens: None,
            },
            delegate.clone(),
            None,
        )
        .await
        .unwrap();
    let cell_id = started.cell_id.clone();
    assert!(matches!(
        started.initial_response().await.unwrap(),
        RuntimeResponse::Yielded { .. }
    ));
    let outcome = session
        .wait_to_pending(WaitToPendingRequest {
            cell_id: cell_id.clone(),
        })
        .await
        .unwrap();
    let WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
        pending_tool_call_ids,
        ..
    }) = outcome
    else {
        panic!("expected a live pending frontier: {outcome:?}")
    };
    assert_eq!(pending_tool_call_ids, ["tool-1", "tool-2"]);
    let calls = delegate.take(&pending_tool_call_ids).await;
    assert_eq!(calls.len(), 2);
    for call in calls {
        call.result.send(Ok(json!("done"))).unwrap();
    }
    let response: RuntimeResponse = session
        .wait(
            WaitRequest {
                cell_id,
                yield_time_ms: 1000,
                max_output_tokens: None,
            },
            None,
        )
        .await
        .unwrap()
        .into();
    assert!(matches!(
        response,
        RuntimeResponse::Result {
            error_text: None,
            ..
        }
    ));
}

#[tokio::test]
async fn native_promise_all_exposes_entire_frontier_and_projects_original_exchange() {
    let mut executor = Executor::default();
    let call = exec(
        "parallel",
        "text(await Promise.all(Array.from({length: 24}, (_, n) => tools.echo({n}))));",
    );
    let frontier = executor.start(call.clone(), tools(), 0).await.unwrap();
    assert_eq!(frontier.len(), 24);
    for (n, call) in frontier.iter().enumerate() {
        let ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            ..
        } = call
        else {
            panic!("function call")
        };
        assert_eq!(name, "Echo");
        assert_eq!(namespace.as_deref(), Some("client"));
        assert_eq!(
            serde_json::from_str::<Value>(arguments).unwrap(),
            json!({"n": n})
        );
    }
    let mut history = frontier.clone();
    history.extend(
        frontier
            .iter()
            .enumerate()
            .map(|(n, call)| result(call, &n.to_string())),
    );
    assert!(executor.resume(&history).await.unwrap().is_empty());
    assert!(!executor.pending());
    let projected = executor.project(&history);
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0], call);
    assert!(output_text(&executor).contains("[\"0\",\"1\",\"2\""));
}

#[tokio::test]
async fn resumes_multiple_native_frontiers_after_restart_without_reissuing_results() {
    let mut executor = Executor::default();
    let call = exec(
        "waves",
        "const first = await tools.echo({wave: 1}); text(first); text(await Promise.all([tools.echo({wave: 2}), tools.echo({wave: 3})]));",
    );
    let first = executor.start(call.clone(), tools(), 0).await.unwrap();
    assert_eq!(first.len(), 1);
    let mut history = first.clone();
    history.push(result(&first[0], "first-result"));
    let mut executor = restored(&executor);
    let second = executor.resume(&history).await.unwrap();
    assert_eq!(second.len(), 2);
    history.extend(second.iter().cloned());
    history.extend(second.iter().map(|call| result(call, "later-result")));
    let mut executor = restored(&executor);
    assert!(executor.resume(&history).await.unwrap().is_empty());
    let text = output_text(&executor);
    assert!(text.contains("first-result"));
    assert!(text.contains("[\"later-result\",\"later-result\"]"));
    assert_eq!(executor.project(&history).len(), 2);
    assert_eq!(executor.project(&history)[0], call);
}

#[tokio::test]
async fn replay_preserves_store_across_completed_cells_and_compaction() {
    let mut executor = Executor::default();
    let first = exec("store", "store('saved', await tools.echo({}));");
    let frontier = executor.start(first, tools(), 0).await.unwrap();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "remembered"));
    executor.resume(&history).await.unwrap();
    executor.compact();
    let mut executor = restored(&executor);
    let second = exec("load", "text(load('saved'));");
    assert!(
        executor
            .start(second.clone(), tools(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(output_text(&executor).contains("remembered"));
    assert_eq!(executor.project(&[]).len(), 2);
    assert_eq!(executor.project(&[])[0], second);
}

#[tokio::test]
async fn fork_at_an_earlier_frontier_replays_only_its_completed_external_results() {
    let mut executor = Executor::default();
    let call = exec(
        "fork",
        "text(await tools.echo({wave: 1})); text(await tools.echo({wave: 2}));",
    );
    let first = executor.start(call, tools(), 0).await.unwrap();
    let mut first_history = first.clone();
    first_history.push(result(&first[0], "first"));
    let second = executor.resume(&first_history).await.unwrap();
    let mut second_history = first_history.clone();
    second_history.extend(second.iter().cloned());
    second_history.push(result(&second[0], "original"));
    executor.resume(&second_history).await.unwrap();
    let mut fork = Executor::restore(executor.state().prefix(&first_history));
    let replayed_second = fork.resume(&first_history).await.unwrap();
    assert_eq!(replayed_second, second);
    first_history.extend(replayed_second.iter().cloned());
    first_history.push(result(&replayed_second[0], "forked"));
    fork.resume(&first_history).await.unwrap();
    assert!(output_text(&fork).contains("forked"));
    assert!(!output_text(&fork).contains("original"));
    assert!(output_text(&fork).contains("first"));
}

#[tokio::test]
async fn native_argument_errors_reject_in_javascript_without_client_calls() {
    let mut executor = Executor::default();
    let call = exec(
        "invalid",
        "try { await tools.echo(1); } catch (error) { text(String(error)); }",
    );
    assert!(executor.start(call, tools(), 0).await.unwrap().is_empty());
    assert!(output_text(&executor).contains("expects a JSON object for arguments"));
    assert!(!executor.pending());
}

#[tokio::test]
async fn script_errors_and_parse_errors_are_upstream_tool_outputs() {
    let mut executor = Executor::default();
    assert!(
        executor
            .start(
                exec("error", "throw new Error('script failed');"),
                tools(),
                0
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(output_text(&executor).contains("script failed"));
    let ResponseItem::CustomToolCallOutput { output, .. } =
        executor.state.cells[0].output.as_ref().unwrap()
    else {
        panic!("custom output")
    };
    assert_eq!(output.success, Some(false));
    assert!(
        executor
            .start(exec("parse", ""), tools(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(output_text(&executor).contains("raw JavaScript"));
}

#[tokio::test]
async fn image_results_can_be_forwarded_through_native_image_helper() {
    let mut executor = Executor::default();
    let call = exec(
        "image",
        "const result = await tools.echo({}); image(result.content[1]); text(result.content[0].text);",
    );
    let frontier = executor.start(call, tools(), 0).await.unwrap();
    let image = "data:image/png;base64,aGVsbG8=";
    let output = output_item(
        &exec(item_call_id(&frontier[0]).unwrap(), "unused"),
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "caption".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: image.to_string(),
                },
                detail: Some(ImageDetail::High),
            },
        ],
        true,
    );
    let mut history = frontier;
    history.push(output);
    executor.resume(&history).await.unwrap();
    let ResponseItem::CustomToolCallOutput { output, .. } =
        executor.state.cells[0].output.as_ref().unwrap()
    else {
        panic!("custom output")
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        panic!("content items")
    };
    assert!(items.iter().any(|item| matches!(item, FunctionCallOutputContentItem::InputImage { image: ImageReference::Inline { image_url }, .. } if image_url == image)));
    assert!(output_text(&executor).contains("caption"));
}

#[tokio::test]
async fn logged_prefix_truncation_clears_results_and_replays_the_retained_frontier() {
    use crate::persist::SessionLog;
    use crate::watchdog::Watchdog;

    let home = tempfile::tempdir().unwrap();
    let log = SessionLog::new(
        home.path(),
        "truncated-executor",
        Arc::new(Watchdog::load(home.path())),
    );
    let key = (None, 7, 0);
    let mut executor = Executor::default();
    let first = executor.start(exec("truncate", "const first = await tools.echo({n:1}); const second = await tools.echo({n:2}); text([first, second]);"), tools(), 0).await.unwrap();
    let mut history = first.clone();
    history.push(result(&first[0], "original"));
    let second = executor.resume(&history).await.unwrap();
    history.extend(second.iter().cloned());
    let previous = executor.state().clone();
    log.lane_with_executor(
        &key,
        0,
        0,
        &history,
        &[item_call_id(&second[0]).unwrap().to_string()],
        false,
        None,
        previous.update(&ExecutorState::default()),
    );
    executor.retain_prefix(&first);
    log.lane_with_executor(
        &key,
        first.len(),
        0,
        &[],
        &[item_call_id(&first[0]).unwrap().to_string()],
        false,
        None,
        executor.state().update(&previous),
    );
    log.flush().await;
    let mut restored = log.restore().await;
    let state = restored.lanes.remove(&key).unwrap();
    assert_eq!(state.history, first);
    assert_state(&state.executor, executor.state());
    let mut executor = Executor::restore(state.executor);
    let mut history = first.clone();
    history.push(result(&first[0], "rewritten"));
    let replayed = executor.resume(&history).await.unwrap();
    assert_eq!(replayed, second);
    history.extend(replayed);
    history.push(result(&second[0], "final"));
    assert!(executor.resume(&history).await.unwrap().is_empty());
    assert!(output_text(&executor).contains("rewritten"));
    assert!(output_text(&executor).contains("final"));
    assert!(!output_text(&executor).contains("original"));
}

#[tokio::test]
async fn lane_log_restores_executor_deltas_and_seeded_pending_cells() {
    use crate::persist::SessionLog;
    use crate::watchdog::Watchdog;

    let home = tempfile::tempdir().unwrap();
    let log = SessionLog::new(
        home.path(),
        "executor-log",
        Arc::new(Watchdog::load(home.path())),
    );
    let key = (None, 7, 0);
    let seeded = (None, 7, 1);
    let mut executor = Executor::default();
    let call = exec("persisted", "store('saved', await tools.echo({}));");
    let frontier = executor.start(call, tools(), 0).await.unwrap();
    log.lane_with_executor(
        &key,
        0,
        0,
        &frontier,
        &[item_call_id(&frontier[0]).unwrap().to_string()],
        false,
        None,
        executor.state().update(&ExecutorState::default()),
    );
    log.seed(&seeded, &key, frontier.len(), &frontier[0]);
    let previous = executor.state().clone();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "stored"));
    executor.resume(&history).await.unwrap();
    log.lane_with_executor(
        &key,
        frontier.len(),
        0,
        &history[frontier.len()..],
        &[],
        false,
        None,
        executor.state().update(&previous),
    );
    log.flush().await;
    let mut restored = log.restore().await;
    let donor = restored.lanes.remove(&key).unwrap();
    assert_eq!(
        serde_json::to_value(&donor.executor).unwrap(),
        serde_json::to_value(executor.state()).unwrap()
    );
    let mut donor = Executor::restore(donor.executor);
    assert!(
        donor
            .start(
                exec("loaded-donor", "text(load('saved'));"),
                tools(),
                history.len()
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(output_text(&donor).contains("stored"));
    let fork = restored.lanes.remove(&seeded).unwrap();
    let mut fork = Executor::restore(fork.executor);
    assert!(fork.pending());
    fork.resume(&history).await.unwrap();
    assert!(!fork.pending());
    assert!(
        fork.start(
            exec("loaded", "text(load('saved'));"),
            tools(),
            history.len()
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(output_text(&fork).contains("stored"));
}

const STORE_AND_PRINT: &str =
    "const value = await tools.echo({}); store('saved', value); text(value);";

fn failure(call: &ResponseItem, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(item_call_id(call).unwrap().to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success: Some(false),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn session_log_leaves_reproducible_results_to_the_lane_history() {
    use crate::persist::SessionLog;
    use crate::persist::restore;
    use crate::watchdog::Watchdog;

    let home = tempfile::tempdir().unwrap();
    let log = SessionLog::new(
        home.path(),
        "deduplicated",
        Arc::new(Watchdog::load(home.path())),
    );
    let key = (None, 7, 0);
    let marker = "unique-result-text";
    let mut executor = Executor::default();
    let frontier = executor
        .start(
            exec(
                "dedup",
                "const [value, failed] = await Promise.all([tools.echo({}), tools.echo({})]);                  store('saved', value); text(value); text(failed.content[0].text);",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    log.lane_with_executor(
        &key,
        0,
        0,
        &frontier,
        &frontier
            .iter()
            .map(|call| item_call_id(call).unwrap().to_string())
            .collect::<Vec<_>>(),
        false,
        None,
        executor.state().update(&ExecutorState::default()),
    );
    let pending = executor.state().clone();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], marker));
    history.push(failure(&frontier[1], "failed-result"));
    executor.resume(&history).await.unwrap();
    // The log's history drops `success`, which only the error result reflects.
    let failed = json!({"content": [{"type": "text", "text": "failed-result"}], "isError": true});
    log.lane_with_executor(
        &key,
        frontier.len(),
        0,
        &history[frontier.len()..],
        &[],
        false,
        None,
        executor.state().update(&pending),
    );
    log.flush().await;
    let text = tokio::fs::read_to_string(log.path()).await.unwrap();
    // The client's tool result and the model's exec output.
    assert_eq!(text.matches(marker).count(), 2);
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(
        last["executor"]["amend"]["results"]["value"],
        json!([null, failed])
    );
    assert_state(
        &restore(log.path()).await.lanes[&key].executor,
        executor.state(),
    );

    // A fork's first record holds compacted cells, whose outputs left the history.
    executor.compact();
    let fork_key = (Some("fork".to_string()), 7, 0);
    log.lane_with_executor(
        &fork_key,
        0,
        0,
        &[],
        &[],
        false,
        None,
        executor.state().update(&ExecutorState::default()),
    );
    log.flush().await;
    let text = tokio::fs::read_to_string(log.path()).await.unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(
        last["executor"]["append"][0]["rounds"][0]["results"],
        json!([marker, failed])
    );
    let fork = restore(log.path()).await.lanes.remove(&fork_key).unwrap();
    assert_state(&fork.executor, executor.state());
    let mut fork = Executor::restore(fork.executor);
    fork.start(exec("load", "text(load('saved'));"), tools(), 0)
        .await
        .unwrap();
    assert!(output_text(&fork).contains(marker));
}

// TODO(0.168): drop with session logs that repeat results, pending content and terminal output.
#[tokio::test]
async fn legacy_executor_records_resume_and_continue() {
    use crate::persist::SessionLog;
    use crate::persist::restore;
    use crate::watchdog::Watchdog;

    let home = tempfile::tempdir().unwrap();
    let log = SessionLog::new(
        home.path(),
        "legacy-executor",
        Arc::new(Watchdog::load(home.path())),
    );
    let key = (None, 7, 0);
    let marker = "legacy-result-text";
    let mut executor = Executor::default();
    let frontier = executor
        .start(exec("legacy", STORE_AND_PRINT), tools(), 0)
        .await
        .unwrap();
    let pending = executor.state().clone();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], marker));
    executor.resume(&history).await.unwrap();
    let mut started = serde_json::to_value(pending.update(&ExecutorState::default())).unwrap();
    started["append"][0]["content"] = json!([]);
    let mut completed = serde_json::to_value(executor.state().update(&pending)).unwrap();
    completed["amend"]["results"]["value"] = json!([marker]);
    completed["amend"]["content"] = json!({"keep": 0, "append": []});
    completed["amend"]["response"]["Result"]["content_items"] =
        json!([{"type": "input_text", "text": marker}]);
    let lines = [
        json!({
            "type": "lane", "agent": null, "fingerprint": 7, "lane": 0, "keep": 0,
            "append": frontier, "pending_calls": [item_call_id(&frontier[0]).unwrap()],
            "resume_turn": false, "executor": started
        }),
        json!({
            "type": "lane", "agent": null, "fingerprint": 7, "lane": 0, "keep": frontier.len(),
            "append": [history.last().unwrap()], "pending_calls": [], "resume_turn": false,
            "executor": completed
        }),
    ];
    std::fs::create_dir_all(log.path().parent().unwrap()).unwrap();
    std::fs::write(
        log.path(),
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let state = log.restore().await.lanes.remove(&key).unwrap();
    assert_eq!(state.history, history);
    assert_state(&state.executor, executor.state());
    let mut resumed = Executor::restore(state.executor);
    resumed
        .start(exec("load", "text(load('saved'));"), tools(), history.len())
        .await
        .unwrap();
    assert!(output_text(&resumed).contains(marker));
    log.lane_with_executor(
        &key,
        history.len(),
        0,
        &[],
        &[],
        false,
        None,
        resumed.state().update(executor.state()),
    );
    log.flush().await;
    let text = tokio::fs::read_to_string(log.path()).await.unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(last["executor"]["keep"], 1);
    assert_eq!(last["executor"]["append"].as_array().unwrap().len(), 1);
    assert_state(
        &restore(log.path()).await.lanes[&key].executor,
        resumed.state(),
    );
}

#[tokio::test]
async fn native_tool_search_outputs_resume_and_project_after_client_execution_mapping() {
    let mut executor = Executor::default();
    let call = exec(
        "search",
        "text((await tools.echo({query:'find'}))[0].name);",
    );
    let frontier = executor.start(call.clone(), tools(), 0).await.unwrap();
    let call_id = item_call_id(&frontier[0]).unwrap();
    let history = vec![
        serde_json::from_value(json!({
            "type":"tool_search_call", "call_id":call_id, "execution":"client", "arguments":{"query":"find"},
        })).unwrap(),
        serde_json::from_value(json!({
            "type":"tool_search_output", "call_id":call_id, "execution":"client", "status":"completed", "tools":[{"name":"found"}],
        })).unwrap(),
    ];
    let mut executor = restored(&executor);
    executor.resume(&history).await.unwrap();
    assert!(output_text(&executor).contains("found"));
    let projected = executor.project(&history);
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0], call);
}

#[tokio::test]
async fn native_timer_yields_and_wait_returns_only_subsequent_output_after_restart() {
    let mut executor = Executor::default();
    let call = exec(
        "timer",
        "// @exec: {\"yield_time_ms\": 25}\ntext('before'); await yield_control(); await new Promise(done => setTimeout(done, 250)); text('after');",
    );
    assert!(
        executor
            .start(call.clone(), tools(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!executor.pending());
    let cell_id = yielded_id(&executor);
    assert!(output_text(&executor).contains("before"));
    assert!(!output_text(&executor).contains("after"));
    let mut executor = restored(&executor);
    let wait = wait("wait-timer", &cell_id, 1000, false);
    assert!(
        executor
            .start(wait.clone(), Vec::new(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!executor.pending());
    assert!(output_text(&executor).contains("Script completed"));
    assert!(output_text(&executor).contains("after"));
    assert!(!output_text(&executor).contains("before"));
    let projected = executor.project(&[]);
    assert_eq!(projected.len(), 4);
    assert_eq!(projected[0], call);
    assert_eq!(projected[2], wait);
}

#[tokio::test]
async fn quiescent_cells_yield_and_native_terminate_finishes_the_cell() {
    let mut executor = Executor::default();
    assert!(
        executor
            .start(
                exec(
                    "quiescent",
                    "// @exec: {\"yield_time_ms\": 5}\nawait new Promise(() => {});"
                ),
                tools(),
                0
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!executor.pending());
    let cell_id = yielded_id(&executor);
    let mut executor = restored(&executor);
    assert!(
        executor
            .start(wait("terminate", &cell_id, 5, true), Vec::new(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(output_text(&executor).contains("Script terminated"));
    assert!(!executor.pending());
    assert!(
        executor
            .start(wait("missing", &cell_id, 5, false), Vec::new(), 0)
            .await
            .unwrap()
            .is_empty()
    );
    let ResponseItem::FunctionCallOutput { output, .. } = executor
        .state
        .cells
        .last()
        .unwrap()
        .output
        .as_ref()
        .unwrap()
    else {
        panic!("wait output")
    };
    assert_eq!(output.success, Some(false));
}

#[tokio::test]
async fn multiple_yielded_cells_keep_their_ids_across_interleaved_waits_and_restart() {
    let mut executor = Executor::default();
    executor
        .start(
            exec(
                "first-cell",
                "// @exec: {\"yield_time_ms\": 5}\nawait new Promise(() => {});",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let first = yielded_id(&executor);
    executor
        .start(
            exec(
                "second-cell",
                "// @exec: {\"yield_time_ms\": 5}\nawait new Promise(() => {});",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let second = yielded_id(&executor);
    assert_ne!(first, second);
    let mut executor = restored(&executor);
    executor
        .start(wait("first-wait", &first, 5, false), Vec::new(), 0)
        .await
        .unwrap();
    assert_eq!(yielded_id(&executor), first);
    executor
        .start(wait("second-stop", &second, 5, true), Vec::new(), 0)
        .await
        .unwrap();
    assert!(output_text(&executor).contains("Script terminated"));
    executor
        .start(wait("first-stop", &first, 5, true), Vec::new(), 0)
        .await
        .unwrap();
    assert!(output_text(&executor).contains("Script terminated"));
    assert!(!executor.pending());
}

#[tokio::test]
async fn wait_surfaces_tools_from_a_yielded_cell_and_resumes_the_same_wait_operation() {
    let mut executor = Executor::default();
    executor.start(exec("yield-before-tools", "// @exec: {\"yield_time_ms\": 25}\nawait new Promise(done => setTimeout(done, 250)); text(await Promise.all([tools.echo({n:1}), tools.echo({n:2})]));"), tools(), 0).await.unwrap();
    let cell_id = yielded_id(&executor);
    let wait = wait("wait-tools", &cell_id, 1000, false);
    let mut executor = restored(&executor);
    let frontier = executor.start(wait.clone(), Vec::new(), 0).await.unwrap();
    assert_eq!(frontier.len(), 2);
    assert!(executor.pending());
    let mut history = frontier.clone();
    history.extend(frontier.iter().map(|call| result(call, "tool-result")));
    let mut executor = restored(&executor);
    assert!(executor.resume(&history).await.unwrap().is_empty());
    assert!(!executor.pending());
    assert!(output_text(&executor).contains("[\"tool-result\",\"tool-result\"]"));
    let projected = executor.project(&history);
    assert_eq!(projected.len(), 4);
    assert_eq!(projected[2], wait);
}

#[tokio::test]
async fn native_yielded_cell_and_independent_client_background_task_can_interleave() {
    let mut executor = Executor::default();
    executor
        .start(
            exec(
                "waiting-cell",
                "// @exec: {\"yield_time_ms\": 5}\nawait new Promise(() => {});",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let cell_id = yielded_id(&executor);
    let frontier = executor
        .start(
            exec(
                "background-shell",
                "text(await tools.echo({command:'work', run_in_background:true}));",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    assert_eq!(frontier.len(), 1);
    let ResponseItem::FunctionCall { arguments, .. } = &frontier[0] else {
        panic!("client shell call")
    };
    assert_eq!(
        serde_json::from_str::<Value>(arguments).unwrap()["run_in_background"],
        true
    );
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "background-task-id"));
    executor.resume(&history).await.unwrap();
    assert!(output_text(&executor).contains("background-task-id"));
    executor
        .start(
            wait("stop-waiting-cell", &cell_id, 5, true),
            Vec::new(),
            history.len(),
        )
        .await
        .unwrap();
    assert!(output_text(&executor).contains("Script terminated"));
}

#[tokio::test]
async fn replay_preserves_recorded_diagnostic_output_and_stored_values() {
    let mut executor = Executor::default();
    executor
        .start(
            exec(
                "timing",
                "text(Math.random()); store('saved', 'remembered');",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let recorded = restored(&executor).state;
    let mut executor = restored(&executor);
    executor
        .start(exec("next", "text(load('saved'));"), tools(), 0)
        .await
        .unwrap();
    assert_eq!(executor.state.cells[0].response, recorded.cells[0].response);
    assert_eq!(executor.state.cells[0].output, recorded.cells[0].output);
    assert!(output_text(&executor).contains("remembered"));
}

#[tokio::test]
async fn replay_preserves_diagnostics_across_completed_tool_frontiers() {
    let mut executor = Executor::default();
    let frontier = executor
        .start(
            exec(
                "timing",
                "text(Math.random()); text(await tools.echo({value:1}));",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "done"));
    executor.resume(&history).await.unwrap();
    let recorded = restored(&executor).state.cells[0].output.clone();
    let mut executor = restored(&executor);
    executor
        .start(exec("next", "text('next');"), tools(), history.len())
        .await
        .unwrap();
    assert_eq!(executor.state.cells[0].output, recorded);
}

#[tokio::test]
async fn replay_rejects_a_changed_output_boundary_before_waiting_on_a_yielded_cell() {
    let mut executor = Executor::default();
    executor
        .start(
            exec(
                "yielded",
                "// @exec: {\"yield_time_ms\": 5}\ntext('first'); await new Promise(() => {});",
            ),
            tools(),
            0,
        )
        .await
        .unwrap();
    let cell_id = yielded_id(&executor);
    let Some(Operation::Exec(request)) = &mut executor.state.cells[0].request else {
        panic!("exec request")
    };
    request.source = "text('first'); text('second'); await new Promise(() => {});".to_string();
    let mut executor = restored(&executor);
    let error = executor
        .start(wait("next", &cell_id, 5, false), Vec::new(), 0)
        .await
        .unwrap_err();
    assert!(error.contains("recorded cell lifecycle"));
}

#[tokio::test]
async fn replay_rejects_a_changed_output_boundary_at_a_pending_tool_frontier() {
    let mut executor = Executor::default();
    let frontier = executor
        .start(
            exec("pending", "text('first'); await tools.echo({});"),
            tools(),
            0,
        )
        .await
        .unwrap();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "done"));
    let Some(Operation::Exec(request)) = &mut executor.state.cells[0].request else {
        panic!("exec request")
    };
    request.source = "text('first'); text('second'); await tools.echo({});".to_string();
    let mut executor = restored(&executor);
    let error = executor.resume(&history).await.unwrap_err();
    assert!(error.contains("recorded tool frontier"));
}

#[tokio::test]
async fn replay_rejects_a_changed_script_before_exposing_any_new_external_call() {
    let mut executor = Executor::default();
    let frontier = executor
        .start(exec("recorded", "await tools.echo({value:1});"), tools(), 0)
        .await
        .unwrap();
    let mut history = frontier.clone();
    history.push(result(&frontier[0], "done"));
    executor.resume(&history).await.unwrap();
    let Some(Operation::Exec(request)) = &mut executor.state.cells[0].request else {
        panic!("exec request")
    };
    request.source = "await tools.echo({value:2});".to_string();
    let mut executor = restored(&executor);
    let error = executor
        .start(exec("next", "text('next');"), tools(), history.len())
        .await
        .unwrap_err();
    assert!(error.contains("recorded tool frontier"));
}
