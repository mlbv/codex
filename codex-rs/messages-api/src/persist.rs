//! Session state on disk, appended per request as Codex appends rollouts: each thread's id and
//! context window, and each lane's history. A restart resumes the same threads, so
//! `prompt_cache_key` and replays stay identical. `<session>.heads`: each root lane's first
//! item per rewrite from the start (first record, compaction); fork lookup reads it, not logs.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_core::external_harness::ExternalThreadRole;
use codex_core::external_harness::ExternalThreadState;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

use crate::align::common_prefix;
use crate::watchdog::LogRate;
use crate::watchdog::Watchdog;

const LOG_EXTENSION: &str = "jsonl";
const HEADS_EXTENSION: &str = "heads";
/// Logs last written this long ago are deleted with their heads.
pub(crate) const LOG_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A lane: its thread (`None` = root), instructions fingerprint, and index among such lanes.
pub(crate) type LaneKey = (Option<String>, u64, usize);

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Thread {
        agent: Option<String>,
        thread_id: ThreadId,
        role: ExternalThreadRole,
        window_number: u64,
        window_id: Uuid,
    },
    /// The lane's history is its first `keep` items, then `append`.
    Lane {
        agent: Option<String>,
        fingerprint: u64,
        lane: usize,
        /// The lane first takes this lane's history as logged so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<LaneKey>,
        keep: usize,
        append: Vec<ResponseItem>,
        pending_calls: Vec<String>,
        resume_turn: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<LoggedPrompt>,
    },
}

/// The lane's last prompt: its leading history items and the backend's token count of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoggedPrompt {
    pub(crate) items: usize,
    pub(crate) input_tokens: i64,
}

/// A root lane's first item, from a record rewriting the lane from its start.
#[derive(Serialize, Deserialize)]
struct Head {
    fingerprint: u64,
    lane: usize,
    first: ResponseItem,
}

#[derive(Default)]
pub(crate) struct Restored {
    pub(crate) threads: HashMap<Option<String>, ExternalThreadState>,
    pub(crate) lanes: HashMap<LaneKey, RestoredLane>,
}

#[derive(Default)]
pub(crate) struct RestoredLane {
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) pending_calls: Vec<String>,
    pub(crate) resume_turn: bool,
    pub(crate) prompt: Option<LoggedPrompt>,
}

pub(crate) struct SessionLog {
    path: PathBuf,
    /// Lines for the writer task, which owns the files.
    writer: mpsc::UnboundedSender<Write>,
    rate: Mutex<LogRate>,
    watchdog: Arc<Watchdog>,
}

enum Write {
    Line(Vec<u8>),
    Head(Vec<u8>),
    #[cfg(test)]
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl SessionLog {
    pub(crate) fn new(codex_home: &Path, session_id: &str, watchdog: Arc<Watchdog>) -> Self {
        let path = sessions_dir(codex_home)
            .join(file_stem(session_id))
            .with_extension(LOG_EXTENSION);
        let (writer, writes) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(path.clone(), writes));
        Self {
            path,
            writer,
            rate: Mutex::new(LogRate::default()),
            watchdog,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves once every earlier record is written.
    #[cfg(test)]
    pub(crate) async fn flush(&self) {
        let (done, written) = tokio::sync::oneshot::channel();
        if self.writer.send(Write::Flush(done)).is_ok() {
            let _ = written.await;
        }
    }

    pub(crate) async fn restore(&self) -> Restored {
        restore(&self.path).await
    }

    pub(crate) fn thread(&self, agent: Option<&str>, state: ExternalThreadState) {
        self.append(&Record::Thread {
            agent: agent.map(str::to_string),
            thread_id: state.thread_id,
            role: state.role,
            window_number: state.window_number,
            window_id: state.window_id,
        });
    }

    pub(crate) fn lane(
        &self,
        key: &LaneKey,
        keep: usize,
        append: &[ResponseItem],
        pending_calls: &[String],
        resume_turn: bool,
        prompt: Option<LoggedPrompt>,
    ) {
        let (agent, fingerprint, lane) = key;
        self.append(&Record::Lane {
            agent: agent.clone(),
            fingerprint: *fingerprint,
            lane: *lane,
            base: None,
            keep,
            append: append.to_vec(),
            pending_calls: pending_calls.to_vec(),
            resume_turn,
            prompt,
        });
        if keep == 0
            && let Some(first) = append.first()
        {
            self.head(key, first);
        }
    }

    /// `key`'s history becomes the first `keep` items `base` has logged.
    pub(crate) fn seed(&self, key: &LaneKey, base: &LaneKey, keep: usize, first: &ResponseItem) {
        let (agent, fingerprint, lane) = key;
        self.append(&Record::Lane {
            agent: agent.clone(),
            fingerprint: *fingerprint,
            lane: *lane,
            base: Some(base.clone()),
            keep,
            append: Vec::new(),
            pending_calls: Vec::new(),
            resume_turn: false,
            prompt: None,
        });
        self.head(key, first);
    }

    fn head(&self, (agent, fingerprint, lane): &LaneKey, first: &ResponseItem) {
        if agent.is_none()
            && let Some(line) = json_line(&Head {
                fingerprint: *fingerprint,
                lane: *lane,
                first: first.clone(),
            })
        {
            self.send(line, Write::Head);
        }
    }

    /// Persistence failures cost a restart its continuity, not the request.
    fn append(&self, record: &Record) {
        if let Some(line) = json_line(record) {
            self.send(line, Write::Line);
        }
    }

    fn send(&self, line: Vec<u8>, write: fn(Vec<u8>) -> Write) {
        if self
            .rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add(line.len())
        {
            self.watchdog.lock_out(format!(
                "session log {} grew past its 10-minute write budget",
                self.path.display()
            ));
        }
        let _ = self.writer.send(write(line));
    }
}

fn json_line(value: &impl Serialize) -> Option<Vec<u8>> {
    let mut line = serde_json::to_vec(value).ok()?;
    line.push(b'\n');
    Some(line)
}

/// The state the log at `path` records; unreadable lines end the replay.
pub(crate) async fn restore(path: &Path) -> Restored {
    let mut restored = Restored::default();
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return restored,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "session log unreadable");
            return restored;
        }
    };
    for line in text.lines() {
        match serde_json::from_str::<Record>(line) {
            Ok(Record::Thread {
                agent,
                thread_id,
                role,
                window_number,
                window_id,
            }) => {
                restored.threads.insert(
                    agent,
                    ExternalThreadState {
                        thread_id,
                        role,
                        window_number,
                        window_id,
                    },
                );
            }
            Ok(Record::Lane {
                agent,
                fingerprint,
                lane,
                base,
                keep,
                append,
                pending_calls,
                resume_turn,
                prompt,
            }) => {
                let base = base
                    .and_then(|base| restored.lanes.get(&base))
                    .map(|base| base.history[..keep.min(base.history.len())].to_vec());
                let state = restored
                    .lanes
                    .entry((agent, fingerprint, lane))
                    .or_default();
                if let Some(base) = base {
                    state.history = base;
                }
                state.history.truncate(keep);
                state.history.extend(append);
                state.pending_calls = pending_calls;
                state.resume_turn = resume_turn;
                state.prompt = prompt;
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "session log replay stopped");
                break;
            }
        }
    }
    restored
}

/// Logs in `codex_home` outside `resident` with a root lane under `fingerprint` now opening
/// with `first`.
pub(crate) async fn fork_candidates(
    codex_home: &Path,
    resident: &HashSet<PathBuf>,
    fingerprint: u64,
    first: &ResponseItem,
) -> Vec<PathBuf> {
    let Ok(mut entries) = tokio::fs::read_dir(sessions_dir(codex_home)).await else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let heads = entry.path();
        let log = heads.with_extension(LOG_EXTENSION);
        if heads
            .extension()
            .is_some_and(|extension| extension == HEADS_EXTENSION)
            && !resident.contains(&log)
            && opens_with(&heads, fingerprint, first).await
        {
            candidates.push(log);
        }
    }
    candidates
}

async fn opens_with(heads: &Path, fingerprint: u64, first: &ResponseItem) -> bool {
    let Ok(text) = tokio::fs::read_to_string(heads).await else {
        return false;
    };
    let mut current = HashMap::new();
    for head in text
        .lines()
        .map_while(|line| serde_json::from_str::<Head>(line).ok())
        .filter(|head| head.fingerprint == fingerprint)
    {
        current.insert(head.lane, head.first);
    }
    current
        .values()
        .any(|head| common_prefix(std::slice::from_ref(head), std::slice::from_ref(first)) == 1)
}

/// Deletes logs under `codex_home` outside `resident` last written over `LOG_TTL` ago, and
/// their heads.
pub(crate) async fn prune(codex_home: &Path, resident: &HashSet<PathBuf>) {
    let Ok(mut entries) = tokio::fs::read_dir(sessions_dir(codex_home)).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let log = path.with_extension(LOG_EXTENSION);
        if !path
            .extension()
            .is_some_and(|extension| extension == LOG_EXTENSION || extension == HEADS_EXTENSION)
            || resident.contains(&log)
        {
            continue;
        }
        // Heads age with their log; orphaned heads with themselves.
        let written = match tokio::fs::metadata(&log).await {
            Ok(metadata) => metadata.modified(),
            Err(_) => entry
                .metadata()
                .await
                .and_then(|metadata| metadata.modified()),
        };
        if written
            .ok()
            .and_then(|written| written.elapsed().ok())
            .is_some_and(|age| age > LOG_TTL)
            && let Err(err) = tokio::fs::remove_file(&path).await
        {
            warn!(path = %path.display(), error = %err, "expired session log not deleted");
        }
    }
}

async fn write_lines(log: PathBuf, mut writes: mpsc::UnboundedReceiver<Write>) {
    let heads = log.with_extension(HEADS_EXTENSION);
    let mut log_file = None;
    let mut heads_file = None;
    while let Some(write) = writes.recv().await {
        let (path, file, line) = match write {
            Write::Line(line) => (&log, &mut log_file, line),
            Write::Head(line) => (&heads, &mut heads_file, line),
            #[cfg(test)]
            Write::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        if file.is_none() {
            *file = open_log(path).await;
        }
        if let Some(opened) = file.as_mut()
            && let Err(err) = opened.write_all(&line).await
        {
            warn!(path = %path.display(), error = %err, "session log write failed");
        }
    }
}

async fn open_log(path: &Path) -> Option<tokio::fs::File> {
    if let Some(parent) = path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        warn!(path = %parent.display(), error = %err, "session log directory unavailable");
        return None;
    }
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .inspect_err(|err| warn!(path = %path.display(), error = %err, "session log unavailable"))
        .ok()
}

fn sessions_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("messages-api").join("sessions")
}

/// Client session ids are untrusted: anything beyond `[A-Za-z0-9_-]` becomes its hash.
fn file_stem(session_id: &str) -> String {
    if !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return session_id.to_string();
    }
    format!("h{:016x}", fnv1a(session_id.as_bytes()))
}

/// FNV-1a: stable across processes, unlike `DefaultHasher`.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
#[path = "persist_tests.rs"]
mod tests;
