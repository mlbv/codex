use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

use super::*;

fn executor_state(cells: usize, description: &str) -> ExecutorState {
    let tools = vec![codex_code_mode::ToolDefinition {
        name: "echo".to_string(),
        tool_name: codex_protocol::ToolName {
            name: "Echo".to_string(),
            namespace: None,
        },
        description: description.to_string(),
        kind: codex_code_mode::CodeModeToolKind::Function,
        input_schema: Some(serde_json::json!({"type": "object"})),
        input_schema_max_bytes: None,
        output_schema: None,
    }];
    serde_json::from_value(serde_json::json!({
        "cells": (0..cells).map(|index| serde_json::json!({
            "at": 0,
            "call": {"type": "custom_tool_call", "name": "exec",
                "call_id": format!("exec-{index}"), "input": "text(1);"},
            "request": {"Exec": {
                "tool_call_id": format!("exec-{index}"), "enabled_tools": tools,
                "source": "text(1);", "yield_time_ms": null, "max_output_tokens": null,
            }},
            "rounds": [], "response": null, "output": null,
        })).collect::<Vec<_>>(),
    }))
    .unwrap()
}

#[tokio::test]
async fn catalogs_are_logged_once_across_cells_lanes_and_restarts() {
    let home = tempfile::tempdir().unwrap();
    let key = (None, 7, 0);
    let agent = (Some("agent".to_string()), 7, 0);
    let state = executor_state(32, &"1".repeat(64_000));
    let log = SessionLog::new(home.path(), "catalogs", watchdog(home.path()));
    log.lane_with_executor(
        &key,
        0,
        0,
        &[message("one")],
        &[],
        false,
        None,
        state.update(&ExecutorState::default()),
    );
    log.lane_with_executor(
        &agent,
        0,
        0,
        &[message("two")],
        &[],
        false,
        None,
        state.update(&ExecutorState::default()),
    );
    log.flush().await;
    let path = log.path().to_path_buf();
    let first_size = tokio::fs::metadata(&path).await.unwrap().len();
    assert!(first_size < 100_000);
    drop(log);
    let log = SessionLog::new(home.path(), "catalogs", watchdog(home.path()));
    let restored = log.restore().await;
    assert!(restored.lanes[&key].executor == state);
    assert!(restored.lanes[&agent].executor == state);
    let next = executor_state(33, &"1".repeat(64_000));
    log.lane_with_executor(&key, 1, 0, &[], &[], false, None, next.update(&state));
    log.flush().await;
    let text = tokio::fs::read_to_string(&path).await.unwrap();
    let records = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "tool_catalog")
            .count(),
        1
    );
    assert!(
        records
            .iter()
            .filter(|record| record["type"] == "lane")
            .flat_map(|record| record["executor"]["append"].as_array().unwrap())
            .all(|cell| cell["request"]["Exec"]["enabled_tools"].is_string())
    );
    assert!(text.len() as u64 - first_size < 2000);
    assert!(restore(&path).await.lanes[&key].executor == next);
}

#[tokio::test]
async fn legacy_inline_catalogs_and_whole_cell_updates_can_be_continued_with_references() {
    let home = tempfile::tempdir().unwrap();
    let key = (None, 7, 0);
    let state = executor_state(2, &"1".repeat(4096));
    let log = SessionLog::new(home.path(), "legacy", watchdog(home.path()));
    // Writing directly retains the pre-interning whole-cell representation.
    log.append(&Record::Lane {
        agent: None,
        fingerprint: 7,
        lane: 0,
        base: None,
        keep: 0,
        append: vec![message("one")],
        pending_calls: Vec::new(),
        resume_turn: false,
        prompt: None,
        executor: Some(state.update(&ExecutorState::default())),
    });
    log.flush().await;
    let path = log.path().to_path_buf();
    let original = tokio::fs::read_to_string(&path).await.unwrap();
    let record: serde_json::Value = serde_json::from_str(original.trim()).unwrap();
    assert!(record["executor"]["append"][0]["request"]["Exec"]["enabled_tools"].is_array());
    assert!(record["executor"].get("amend").is_none());
    assert!(record["executor"].get("compact").is_none());
    drop(log);
    let log = SessionLog::new(home.path(), "legacy", watchdog(home.path()));
    let restored = log.restore().await;
    assert!(restored.lanes[&key].executor == state);
    let next = executor_state(3, &"1".repeat(4096));
    log.lane_with_executor(&key, 1, 0, &[], &[], false, None, next.update(&state));
    log.flush().await;
    let text = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(text.starts_with(&original));
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert!(last["executor"]["append"][0]["request"]["Exec"]["enabled_tools"].is_string());
    assert!(restore(&path).await.lanes[&key].executor == next);
}

#[tokio::test]
async fn only_relogged_items_and_cells_count_toward_the_watchdog() {
    let home = tempfile::tempdir().unwrap();
    let key = (None, 7, 0);
    let log = SessionLog::new(home.path(), "relogged", watchdog(home.path()));
    let relogged = || log.rate.lock().unwrap().bytes();
    let state = executor_state(2, "first");
    log.lane_with_executor(
        &key,
        0,
        0,
        &[message("one"), message("two")],
        &[],
        false,
        None,
        state.update(&ExecutorState::default()),
    );
    log.lane_with_executor(
        &key,
        2,
        0,
        &[message("three")],
        &[],
        false,
        None,
        state.update(&state),
    );
    assert_eq!(relogged(), 0);
    let replacement = [message("rewritten")];
    log.lane_with_executor(
        &key,
        1,
        1,
        &replacement,
        &[],
        false,
        None,
        state.update(&state),
    );
    let items = serde_json::to_vec(&replacement).unwrap().len();
    assert_eq!(relogged(), items);
    let changed = executor_state(2, "second");
    log.lane_with_executor(&key, 2, 0, &[], &[], false, None, changed.update(&state));
    assert!(relogged() > items);
}

fn watchdog(home: &Path) -> Arc<Watchdog> {
    Arc::new(Watchdog::load(home))
}

fn message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn lane_records_replay_to_the_last_history() {
    let home = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::new(home.path(), "session/../1", watchdog(home.path()));
    let state = ExternalThreadState {
        thread_id: ThreadId::new(),
        role: ExternalThreadRole::Subagent {
            session_id: ThreadId::new(),
            parent_thread_id: ThreadId::new(),
            depth: 2,
        },
        window_number: 2,
        window_id: Uuid::now_v7(),
    };
    log.thread(None, state);
    let key = (None, 7, 0);
    let prompt = LoggedPrompt {
        items: 2,
        input_tokens: 900,
        context: Some(crate::tokens::ContextUsage {
            visible_items: 2,
            items: 2,
            tokens: 950,
        }),
    };
    log.lane(&key, 0, &[message("a"), message("b")], &[], false, None);
    log.lane(
        &key,
        1,
        &[message("c")],
        &["call_1".to_string()],
        true,
        Some(prompt),
    );
    log.flush().await;
    let restored = SessionLog::new(home.path(), "session/../1", watchdog(home.path()))
        .restore()
        .await;
    assert_eq!(restored.threads.get(&None), Some(&state));
    let lane = restored.lanes.get(&key).expect("lane");
    assert_eq!(lane.history, vec![message("a"), message("c")]);
    assert_eq!(lane.pending_calls, vec!["call_1".to_string()]);
    assert!(lane.resume_turn);
    assert_eq!(lane.prompt, Some(prompt));
    assert!(
        std::fs::read_dir(home.path().join("messages-api").join("sessions"))
            .expect("sessions")
            .all(|entry| entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with('h'))
    );
}

#[tokio::test]
async fn fork_candidates_open_with_the_current_first_item() {
    let home = tempfile::tempdir().expect("tempdir");
    for (session, first, fingerprint) in [("a", "one", 7), ("b", "other", 7), ("c", "one", 8)] {
        let log = SessionLog::new(home.path(), session, watchdog(home.path()));
        log.lane(
            &(None, fingerprint, 0),
            0,
            &[message(first)],
            &[],
            false,
            None,
        );
        log.lane(
            &(None, fingerprint, 0),
            1,
            &[message("two")],
            &[],
            false,
            None,
        );
        log.flush().await;
    }
    let compacted = SessionLog::new(home.path(), "d", watchdog(home.path()));
    compacted.lane(&(None, 7, 0), 0, &[message("one")], &[], false, None);
    compacted.lane(&(None, 7, 0), 0, &[message("summary")], &[], false, None);
    compacted.flush().await;
    let dir = sessions_dir(home.path());
    let none = HashSet::new();
    let resident = HashSet::from([dir.join("a.jsonl")]);
    assert_eq!(
        (
            fork_candidates(home.path(), &none, 7, &message("one")).await,
            fork_candidates(home.path(), &none, 7, &message("summary")).await,
            fork_candidates(home.path(), &resident, 7, &message("one")).await,
        ),
        (
            vec![dir.join("a.jsonl")],
            vec![dir.join("d.jsonl")],
            Vec::new()
        )
    );
}

#[tokio::test]
async fn expired_logs_and_their_heads_are_pruned() {
    let home = tempfile::tempdir().expect("tempdir");
    for session in ["old", "new", "resident"] {
        let log = SessionLog::new(home.path(), session, watchdog(home.path()));
        log.lane(&(None, 7, 0), 0, &[message("one")], &[], false, None);
        log.flush().await;
    }
    let dir = sessions_dir(home.path());
    let expired = std::time::SystemTime::now() - LOG_TTL - Duration::from_secs(60);
    for name in ["old.jsonl", "old.heads", "resident.jsonl", "new.heads"] {
        std::fs::File::options()
            .write(true)
            .open(dir.join(name))
            .and_then(|file| file.set_modified(expired))
            .expect("age file");
    }
    prune(home.path(), &HashSet::from([dir.join("resident.jsonl")])).await;
    let mut left = std::fs::read_dir(&dir)
        .expect("sessions")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    left.sort();
    assert_eq!(
        left,
        ["new.heads", "new.jsonl", "resident.heads", "resident.jsonl"]
    );
}

#[tokio::test]
async fn seeded_lanes_replay_their_base_as_logged_then() {
    let home = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::new(home.path(), "s", watchdog(home.path()));
    let (donor, seeded) = ((None, 7, 0), (None, 7, 1));
    log.lane(
        &donor,
        0,
        &[message("a"), message("b"), message("c")],
        &[],
        false,
        None,
    );
    log.seed(&seeded, &donor, 2, &message("a"));
    log.lane(&seeded, 2, &[message("d")], &[], false, None);
    log.lane(&donor, 0, &[message("x")], &[], false, None);
    log.flush().await;
    let restored = log.restore().await;
    assert_eq!(
        (
            &restored.lanes[&seeded].history,
            &restored.lanes[&donor].history
        ),
        (
            &vec![message("a"), message("b"), message("d")],
            &vec![message("x")]
        )
    );
}
