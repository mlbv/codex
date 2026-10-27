use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

use super::*;

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
