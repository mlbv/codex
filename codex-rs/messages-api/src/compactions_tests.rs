use std::time::Duration;

use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn items(text: &str) -> Vec<ResponseItem> {
    vec![
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
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "state".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

fn last_used(codex_home: &Path, digest: &str) -> SystemTime {
    std::fs::metadata(entry(codex_home, digest))
        .and_then(|metadata| metadata.modified())
        .expect("entry mtime")
}

fn age(codex_home: &Path, digest: &str, by: Duration) {
    std::fs::File::options()
        .write(true)
        .open(entry(codex_home, digest))
        .and_then(|file| file.set_modified(SystemTime::now() - by))
        .expect("backdate entry");
}

#[tokio::test]
async fn stored_items_load_under_their_digest() {
    let home = tempfile::tempdir().expect("tempdir");
    let digest = store(home.path(), &items("fix it")).await.expect("store");
    assert!(is_digest(&digest), "{digest}");
    assert_eq!(
        store(home.path(), &items("fix it")).await.expect("store"),
        digest
    );
    assert_ne!(
        store(home.path(), &items("other")).await.expect("store"),
        digest
    );
    assert_eq!(
        load(home.path(), &digest).await.expect("load"),
        Some(items("fix it"))
    );
    assert_eq!(
        load(home.path(), &"0".repeat(64)).await.expect("load"),
        None
    );
}

#[tokio::test]
async fn load_and_store_refresh_last_use() {
    let home = tempfile::tempdir().expect("tempdir");
    let digest = store(home.path(), &items("fix it")).await.expect("store");
    let day = Duration::from_secs(24 * 60 * 60);
    age(home.path(), &digest, day);
    load(home.path(), &digest).await.expect("load");
    assert!(last_used(home.path(), &digest).elapsed().expect("elapsed") < day);
    age(home.path(), &digest, day);
    store(home.path(), &items("fix it")).await.expect("store");
    assert!(last_used(home.path(), &digest).elapsed().expect("elapsed") < day);
}

#[tokio::test]
async fn prune_deletes_entries_unused_past_ttl() {
    let home = tempfile::tempdir().expect("tempdir");
    let stale = store(home.path(), &items("old")).await.expect("store");
    let fresh = store(home.path(), &items("new")).await.expect("store");
    age(home.path(), &stale, LOG_TTL + Duration::from_secs(60));
    prune(home.path()).await;
    assert_eq!(load(home.path(), &stale).await.expect("load"), None);
    assert_eq!(
        load(home.path(), &fresh).await.expect("load"),
        Some(items("new"))
    );
}

#[tokio::test]
async fn resolve_loads_user_stored_markers_only() {
    let home = tempfile::tempdir().expect("tempdir");
    let digest = store(home.path(), &items("fix it")).await.expect("store");
    let marker = codec::compaction_marker(&digest);
    let missing = codec::compaction_marker(&"0".repeat(64));
    let messages: Vec<InputMessage> = serde_json::from_value(json!([
        { "role": "user", "content": format!("Summary:\n{marker}") },
        { "role": "assistant", "content": [{ "type": "text", "text": missing }] },
        { "role": "user", "content": [
            { "type": "text", "text": missing },
            { "type": "text", "text": codec::inline_marker(&items("inline")) },
        ]},
    ]))
    .expect("messages");
    assert_eq!(
        resolve(home.path(), &messages).await.expect("resolve"),
        HashMap::from([(digest, items("fix it"))])
    );
}
