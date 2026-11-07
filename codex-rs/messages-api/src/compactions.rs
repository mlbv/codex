//! Replacement histories of remote compactions, stored by content digest; summaries carry only the
//! digest.

use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use codex_protocol::models::ResponseItem;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;
use uuid::Uuid;

use crate::anthropic::ContentBlock;
use crate::anthropic::InputMessage;
use crate::anthropic::MessageContent;
use crate::anthropic::Role;
use crate::codec;
use crate::codec::Marker;
use crate::error::ApiError;
use crate::persist::LOG_TTL;

const EXTENSION: &str = "json";
const DIGEST_LEN: usize = 64;

/// Lowercase hex SHA-256 of the serialized items.
pub(crate) fn is_digest(text: &str) -> bool {
    text.len() == DIGEST_LEN
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Stores `items` and returns their digest; an existing entry only has its last use refreshed.
pub(crate) async fn store(codex_home: &Path, items: &[ResponseItem]) -> io::Result<String> {
    let bytes = serde_json::to_vec(items)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let path = entry(codex_home, &digest);
    match touch(path.clone()).await {
        Ok(()) => return Ok(digest),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    tokio::fs::create_dir_all(dir(codex_home)).await?;
    // Readers never see a partial entry.
    let partial = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    tokio::fs::write(&partial, &bytes).await?;
    tokio::fs::rename(&partial, &path).await?;
    Ok(digest)
}

/// The items stored under `digest`, refreshing its last use; `None` = not stored.
pub(crate) async fn load(codex_home: &Path, digest: &str) -> io::Result<Option<Vec<ResponseItem>>> {
    let path = entry(codex_home, digest);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    touch(path).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// The stored histories that the compaction markers in `messages` name.
pub(crate) async fn resolve(
    codex_home: &Path,
    messages: &[InputMessage],
) -> Result<HashMap<String, Vec<ResponseItem>>, ApiError> {
    let mut digests = Vec::new();
    for message in messages.iter().filter(|message| message.role == Role::User) {
        match &message.content {
            MessageContent::Text(text) => digests.extend(stored_digest(text)),
            MessageContent::Blocks(blocks) => {
                digests.extend(blocks.iter().filter_map(|block| match block {
                    ContentBlock::Text { text } => stored_digest(text),
                    _ => None,
                }));
            }
        }
    }
    let mut stored = HashMap::new();
    for digest in digests {
        if stored.contains_key(digest) {
            continue;
        }
        let loaded = load(codex_home, digest)
            .await
            .map_err(|err| ApiError::api(format!("compaction {digest}: {err}")))?;
        if let Some(items) = loaded {
            stored.insert(digest.to_string(), items);
        }
    }
    Ok(stored)
}

/// Deletes entries last stored or loaded over `LOG_TTL` ago.
pub(crate) async fn prune(codex_home: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir(codex_home)).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if entry
            .metadata()
            .await
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|used| used.elapsed().ok())
            .is_some_and(|age| age > LOG_TTL)
            && let Err(err) = tokio::fs::remove_file(&path).await
        {
            warn!(path = %path.display(), error = %err, "expired compaction not deleted");
        }
    }
}

fn stored_digest(text: &str) -> Option<&str> {
    match codec::find_compaction(text)? {
        (_, Marker::Stored(digest), _) => Some(digest),
        (_, Marker::Inline(_), _) => None,
    }
}

async fn touch(path: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .write(true)
            .open(path)?
            .set_modified(SystemTime::now())
    })
    .await
    .map_err(io::Error::other)?
}

fn entry(codex_home: &Path, digest: &str) -> PathBuf {
    dir(codex_home).join(digest).with_extension(EXTENSION)
}

fn dir(codex_home: &Path) -> PathBuf {
    codex_home.join("messages-api").join("compactions")
}

#[cfg(test)]
#[path = "compactions_tests.rs"]
mod tests;
