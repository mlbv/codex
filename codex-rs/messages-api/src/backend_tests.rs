use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

const INSTRUCTIONS: u64 = 7;

#[tokio::test]
async fn hidden_effort_updates_preserve_the_lane_and_open_tool_turn() {
    let home = tempfile::tempdir().unwrap();
    let backend = backend(home.path()).await;
    let initial = [
        user("one"),
        item(json!({"type":"configuration_update", "reasoning":{"effort":"low"}})),
    ];
    answer(&backend, "effort", None, &initial, call("c1"), &["c1"]).await;
    let echoed = [user("one"), call("c1"), output("c1")];
    let (_, lane) = backend
        .lane(&identity("effort", None), &probe(&echoed))
        .await;
    assert_eq!(lane.key.2, 0);
    assert!(lane.continued_by(3, &echoed));
    assert_eq!(lane.history.len(), 3);
}

async fn backend(home: &Path) -> Backend {
    crate::test_support::backend(home, ModelsResponse::default(), |_| {}).await
}

fn item(value: serde_json::Value) -> ResponseItem {
    serde_json::from_value(value).expect("item")
}

fn user(text: &str) -> ResponseItem {
    item(
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }),
    )
}

fn assistant(text: &str) -> ResponseItem {
    item(
        json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] }),
    )
}

fn call(call_id: &str) -> ResponseItem {
    item(json!({ "type": "function_call", "name": "Bash", "arguments": "{}", "call_id": call_id }))
}

fn output(call_id: &str) -> ResponseItem {
    item(json!({ "type": "function_call_output", "call_id": call_id, "output": "ok" }))
}

fn identity(session: &str, agent: Option<&str>) -> Identity {
    Identity {
        session_id: session.to_string(),
        agent_id: agent.map(str::to_string),
        parent_agent_id: None,
    }
}

fn forked_from(parent: ExternalThreadState) -> ExternalThreadRole {
    ExternalThreadRole::Fork {
        session_id: parent.session_id(),
        forked_from: parent.thread_id,
    }
}

fn probe(items: &[ResponseItem]) -> Probe<'_> {
    Probe {
        fingerprint: INSTRUCTIONS,
        items,
    }
}

/// Sends `items` on the lane they select and records `answer` as the model's reply.
async fn answer(
    backend: &Backend,
    session: &str,
    agent: Option<&str>,
    items: &[ResponseItem],
    answer: ResponseItem,
    pending_calls: &[&str],
) -> (Arc<Thread>, usize) {
    let (thread, mut lane) = backend.lane(&identity(session, agent), &probe(items)).await;
    let mut history = items.to_vec();
    history.push(answer);
    lane.history = Arc::new(history);
    lane.pending_calls = pending_calls.iter().map(ToString::to_string).collect();
    thread.record(&mut lane, 0);
    let index = lane.key.2;
    (thread, index)
}

async fn flush(backend: &Backend) {
    let sessions = backend
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for session in sessions {
        session.log.flush().await;
    }
}

#[tokio::test]
async fn requests_take_the_lane_they_repeat_and_new_lanes_start_from_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let backend = backend(home.path()).await;
    let first = [user("one")];
    answer(&backend, "s", None, &first, assistant("two"), &[]).await;
    let next = [user("one"), assistant("two"), user("three")];
    let (_, held) = backend.lane(&identity("s", None), &probe(&next)).await;
    assert_eq!((held.key.2, held.history.len()), (0, 2));
    let (_, concurrent) = backend.lane(&identity("s", None), &probe(&next)).await;
    assert_eq!(
        (
            concurrent.key.2,
            concurrent.history.as_slice(),
            concurrent.logged
        ),
        (1, &next[..2], 2)
    );
}

#[tokio::test]
async fn an_open_turn_is_left_to_the_request_continuing_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let backend = backend(home.path()).await;
    let first = [user("one")];
    answer(&backend, "s", None, &first, call("c1"), &["c1"]).await;
    let side = [user("one"), user("side question")];
    let (_, side_lane) = backend.lane(&identity("s", None), &probe(&side)).await;
    assert_eq!(
        (side_lane.key.2, side_lane.history.as_slice()),
        (1, &side[..1])
    );
    drop(side_lane);
    let follow_up = [user("one"), call("c1"), output("c1")];
    let (_, lane) = backend.lane(&identity("s", None), &probe(&follow_up)).await;
    assert_eq!(lane.key.2, 0);
    assert!(lane.continued_by(2, &follow_up));
}

#[tokio::test]
async fn agents_start_from_the_root_history_they_repeat() {
    let home = tempfile::tempdir().expect("tempdir");
    let backend = backend(home.path()).await;
    answer(&backend, "s", None, &[user("one")], assistant("two"), &[]).await;
    let forked = [user("one"), assistant("two"), user("summarize")];
    let (thread, lane) = backend
        .lane(&identity("s", Some("agent")), &probe(&forked))
        .await;
    assert_eq!(lane.history.as_slice(), &forked[..2]);
    let session = backend.session("s", &probe(&forked)).await;
    assert_eq!(
        thread.external.session_id(),
        session.root.external.session_id()
    );
}

#[tokio::test]
async fn a_new_session_repeating_model_output_forks_into_its_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let backend = backend(home.path()).await;
    let (parent, _) = answer(
        &backend,
        "parent",
        None,
        &[user("one")],
        assistant("two"),
        &[],
    )
    .await;
    let fresh = [user("one")];
    let (unrelated, _) = backend
        .lane(&identity("unrelated", None), &probe(&fresh))
        .await;
    assert_eq!(unrelated.external.state().role, ExternalThreadRole::Root);
    let forked = [user("one"), assistant("two"), user("other path")];
    let (fork, lane) = backend.lane(&identity("fork", None), &probe(&forked)).await;
    let (parent, fork) = (parent.external.state(), fork.external.state());
    assert_eq!(
        (fork.role, lane.history.as_slice()),
        (forked_from(parent), &forked[..2])
    );
    assert_ne!(fork.thread_id, parent.thread_id);
}

#[tokio::test]
async fn forks_of_logged_sessions_and_their_identity_survive_restarts() {
    let home = tempfile::tempdir().expect("tempdir");
    let parent = {
        let backend = backend(home.path()).await;
        let (parent, _) = answer(
            &backend,
            "parent",
            None,
            &[user("one")],
            assistant("two"),
            &[],
        )
        .await;
        flush(&backend).await;
        parent.external.state()
    };
    let forked = [user("one"), assistant("two"), user("other path")];
    let fork = {
        let backend = backend(home.path()).await;
        let (fork, _) = answer(&backend, "fork", None, &forked, assistant("three"), &[]).await;
        flush(&backend).await;
        fork.external.state()
    };
    assert_eq!(fork.role, forked_from(parent));
    let backend = backend(home.path()).await;
    let next = [&forked[..], &[assistant("three"), user("four")]].concat();
    let (thread, lane) = backend.lane(&identity("fork", None), &probe(&next)).await;
    assert_eq!(
        (thread.external.state(), lane.history.len(), lane.logged),
        (fork, 4, 4)
    );
}

#[tokio::test]
async fn forks_find_logged_parents_by_their_current_first_item() {
    let home = tempfile::tempdir().expect("tempdir");
    let parent = {
        let backend = backend(home.path()).await;
        answer(
            &backend,
            "parent",
            None,
            &[user("one")],
            assistant("two"),
            &[],
        )
        .await;
        let (parent, _) = answer(
            &backend,
            "parent",
            None,
            &[user("summary")],
            assistant("after"),
            &[],
        )
        .await;
        flush(&backend).await;
        parent.external.state()
    };
    let backend = backend(home.path()).await;
    let forked = [user("summary"), assistant("after"), user("other path")];
    let (fork, lane) = backend.lane(&identity("fork", None), &probe(&forked)).await;
    assert_eq!(
        (fork.external.state().role, lane.history.as_slice()),
        (forked_from(parent), &forked[..2])
    );
}

#[tokio::test]
async fn nested_agents_sit_under_their_parent_across_restarts() {
    let home = tempfile::tempdir().expect("tempdir");
    let nested = Identity {
        parent_agent_id: Some("outer".to_string()),
        ..identity("s", Some("inner"))
    };
    let (root, outer, inner) = {
        let backend = backend(home.path()).await;
        let (root, _) = answer(&backend, "s", None, &[user("one")], assistant("two"), &[]).await;
        let (outer, _) = answer(
            &backend,
            "s",
            Some("outer"),
            &[user("task")],
            assistant("done"),
            &[],
        )
        .await;
        let (inner, _) = backend.lane(&nested, &probe(&[user("subtask")])).await;
        flush(&backend).await;
        (
            root.external.state(),
            outer.external.state(),
            inner.external.state(),
        )
    };
    assert_eq!(
        (outer.role, inner.role),
        (
            ExternalThreadRole::Subagent {
                session_id: root.thread_id,
                parent_thread_id: root.thread_id,
                depth: 1,
            },
            ExternalThreadRole::Subagent {
                session_id: root.thread_id,
                parent_thread_id: outer.thread_id,
                depth: 2,
            }
        )
    );
    let backend = backend(home.path()).await;
    let (restored, _) = backend.lane(&nested, &probe(&[user("subtask")])).await;
    assert_eq!(restored.external.state(), inner);
}

#[tokio::test]
async fn seeded_lanes_log_a_reference_to_their_donor() {
    let home = tempfile::tempdir().expect("tempdir");
    let backend = backend(home.path()).await;
    let session = identity("s", None);
    answer(&backend, "s", None, &[user("one")], assistant("two"), &[]).await;
    let main = [user("one"), assistant("two"), user("three")];
    let (thread, mut held) = backend.lane(&session, &probe(&main)).await;
    held.history = Arc::new([&main[..], &[assistant("four")]].concat());
    thread.record(&mut held, 2);
    let side = [
        user("one"),
        assistant("two"),
        user("three"),
        assistant("four"),
        user("side"),
    ];
    let (_, mut side_lane) = backend.lane(&session, &probe(&side)).await;
    assert_eq!(
        (side_lane.key.2, side_lane.history.len(), side_lane.logged),
        (1, 4, 4)
    );
    side_lane.history = Arc::new([&side[..], &[assistant("noted")]].concat());
    thread.record(&mut side_lane, 4);
    let expected = side_lane.history.to_vec();
    drop((held, side_lane));
    flush(&backend).await;
    let log = backend
        .session("s", &probe(&side))
        .await
        .log
        .path()
        .to_path_buf();
    let restored = persist::restore(&log).await;
    assert_eq!(restored.lanes[&(None, INSTRUCTIONS, 1)].history, expected);
}
