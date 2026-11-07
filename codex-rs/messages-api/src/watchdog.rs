//! Locks serving out once usage is being wasted: a session log growing past any normal rate
//! (rewrite loops), or a lane's prompt cache missing request after request (a prefix the client
//! or this proxy no longer keeps stable). The lockout outlives restarts; deleting
//! `$CODEX_HOME/messages-api/lockout.json`, after an audit, lifts it.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use tracing::error;

/// One session log's appends within `LOG_WINDOW` past which serving locks out. Measured: heavy
/// five-agent use 2.7 MB; the full-history rewrite loop 90 MB.
const LOG_BYTES_PER_WINDOW: usize = 50 * 1024 * 1024;
const LOG_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Consecutive misses that lock out; one-off misses (tool list change, eviction) recover first.
const MISS_STREAK: u32 = 3;
/// A request judges its cache only when the lane's previous request is this recent.
const CACHE_FRESH: Duration = Duration::from_secs(5 * 60);
/// A miss: more uncached tokens than this, and than a tenth of the expected prefix.
const MISS_MIN_TOKENS: i64 = 32_000;

pub(crate) struct Watchdog {
    path: PathBuf,
    lockout: Mutex<Option<String>>,
}

#[derive(Serialize, Deserialize)]
struct Lockout {
    reason: String,
    at: String,
}

impl Watchdog {
    /// Locked out when `codex_home` holds a lockout, or one that cannot be read.
    pub(crate) fn load(codex_home: &Path) -> Self {
        let path = codex_home.join("messages-api").join("lockout.json");
        let lockout = match std::fs::read_to_string(&path) {
            Ok(text) => Some(
                serde_json::from_str::<Lockout>(&text).map_or(text, |lockout| {
                    format!("{} (at {})", lockout.reason, lockout.at)
                }),
            ),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => Some(format!("lockout unreadable: {err}")),
        };
        Self {
            path,
            lockout: Mutex::new(lockout),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn lockout(&self) -> Option<String> {
        self.lockout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Locks serving out for good; the first reason stands.
    pub(crate) fn lock_out(&self, reason: String) {
        let mut lockout = self
            .lockout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lockout.is_some() {
            return;
        }
        error!(%reason, path = %self.path.display(), "messages-api locked out");
        let record = Lockout {
            reason: reason.clone(),
            at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(err) = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(
                    &self.path,
                    serde_json::to_vec_pretty(&record).unwrap_or_default(),
                )
            })
        {
            error!(error = %err, "lockout not persisted; it lasts until restart");
        }
        *lockout = Some(reason);
    }
}

/// One session log's recent appends.
#[derive(Default)]
pub(crate) struct LogRate {
    appends: VecDeque<(Instant, usize)>,
    bytes: usize,
}

impl LogRate {
    /// Records an append; `true` while the window holds more than the limit.
    pub(crate) fn add(&mut self, bytes: usize) -> bool {
        let now = Instant::now();
        while let Some((at, old)) = self.appends.front()
            && now.duration_since(*at) > LOG_WINDOW
        {
            self.bytes -= old;
            self.appends.pop_front();
        }
        self.appends.push_back((now, bytes));
        self.bytes += bytes;
        self.bytes > LOG_BYTES_PER_WINDOW
    }
}

/// A lane's run of consecutive cache misses.
#[derive(Default)]
pub(crate) struct CacheStreak {
    misses: u32,
}

impl CacheStreak {
    /// Judges a request reading `cached` of its `input` tokens, after the lane's previous request
    /// counted `previous` input tokens `since` ago. `Some` = the streak that locks out.
    pub(crate) fn judge(
        &mut self,
        previous: i64,
        since: Duration,
        input: i64,
        cached: i64,
    ) -> Option<String> {
        if since > CACHE_FRESH {
            return None;
        }
        let expected = previous.min(input);
        let missed = expected - cached;
        if missed <= MISS_MIN_TOKENS.max(expected / 10) {
            self.misses = 0;
            return None;
        }
        self.misses += 1;
        (self.misses >= MISS_STREAK).then(|| {
            format!(
                "{} consecutive prompt cache misses on one lane; last read {cached} of {expected} cached tokens",
                self.misses
            )
        })
    }
}

#[cfg(test)]
#[path = "watchdog_tests.rs"]
mod tests;
