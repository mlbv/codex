//! Codex threads behind client conversations, and the lanes that send their requests.
//!
//! A Claude Code session is one root thread; each of its subagents is a child thread. A lane
//! owns one WebSocket client and the history it last sent. A request takes the idle lane whose
//! history it repeats furthest; a new lane starts from the longest history it repeats, so
//! replays stay identical across lanes, agents and forked sessions.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;

use codex_core::ModelClient;
use codex_core::config::Config;
use codex_core::external_harness::ExternalThread;
use codex_core::external_harness::ExternalThreadRole;
use codex_core::external_harness::ExternalThreadState;
use codex_core::external_harness::ExternalTurn;
use codex_image_generation_extension::StandaloneImages;
use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_web_search_extension::StandaloneSearch;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::align::common_prefix;
use crate::align::is_model_output;
use crate::compactions;
use crate::error::ApiError;
use crate::mcp_connector::McpConnector;
use crate::persist;
use crate::persist::LaneKey;
use crate::persist::LoggedPrompt;
use crate::persist::Restored;
use crate::persist::RestoredLane;
use crate::persist::SessionLog;
use crate::respond::Identity;
use crate::watchdog::CacheStreak;
use crate::watchdog::Watchdog;

/// Sessions idle this long release their threads and sockets.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Period of idle-session eviction and session log pruning.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Lanes a thread keeps per instructions before a request repeating none of them takes the
/// least recently used idle one.
const LANES_PER_INSTRUCTIONS: usize = 4;

pub(crate) struct Backend {
    pub(crate) config: Arc<Config>,
    auth_manager: Arc<AuthManager>,
    pub(crate) models: SharedModelsManager,
    installation_id: String,
    pub(crate) originator: String,
    /// Codex's standalone search, when config and provider offer it.
    pub(crate) web_search: Option<StandaloneSearch>,
    /// Codex's Images API, when the provider offers it.
    pub(crate) images: Option<StandaloneImages>,
    pub(crate) mcp: McpConnector,
    git_root_discovery: Arc<GitRootDiscovery>,
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    pub(crate) watchdog: Arc<Watchdog>,
}

/// A request's conversation, as lanes are matched against it.
pub(crate) struct Probe<'a> {
    /// Instructions fingerprint: lanes never share histories across instructions.
    pub(crate) fingerprint: u64,
    pub(crate) items: &'a [ResponseItem],
}

struct Session {
    root: Arc<Thread>,
    agents: StdMutex<HashMap<String, Arc<Thread>>>,
    last_used: StdMutex<Instant>,
    log: Arc<SessionLog>,
}

pub(crate) struct Thread {
    pub(crate) external: Arc<ExternalThread>,
    agent: Option<String>,
    log: Arc<SessionLog>,
    lanes: StdMutex<Vec<Slot>>,
}

struct Slot {
    fingerprint: u64,
    index: usize,
    lane: Arc<Mutex<Lane>>,
    shared: Arc<StdMutex<Shared>>,
}

/// What other requests read of a lane, streaming or not.
struct Shared {
    /// The history as last recorded.
    history: Arc<Vec<ResponseItem>>,
    last_used: Instant,
}

pub(crate) struct Lane {
    pub(crate) client: ModelClient,
    /// Items last sent plus the outputs that answered them, as Codex history records them.
    pub(crate) history: Arc<Vec<ResponseItem>>,
    pub(crate) turn: Option<ExternalTurn>,
    /// Calls the last response made; their outputs continue the turn.
    pub(crate) pending_calls: Vec<String>,
    /// A mid-turn compaction keeps the turn open for the request that follows it.
    pub(crate) resume_turn: bool,
    pub(crate) prewarmed: bool,
    /// The last request's prompt, as the backend counted it.
    pub(crate) prompt: Option<SentPrompt>,
    pub(crate) cache: CacheStreak,
    key: LaneKey,
    /// Leading history items the session log holds.
    logged: usize,
    shared: Arc<StdMutex<Shared>>,
}

pub(crate) struct SentPrompt {
    /// Leading history items it held.
    pub(crate) items: usize,
    pub(crate) input_tokens: i64,
    /// `None` = restored from the session log: no resident cache chain to judge.
    pub(crate) sent: Option<Instant>,
}

/// `ExternalThread::new` or `ExternalThread::resume`.
type ExternalThreadConstructor<I> =
    fn(Arc<Config>, Arc<AuthManager>, String, String, Arc<GitRootDiscovery>, I) -> ExternalThread;

/// The thread a new session forks, and the history prefix its request repeats.
struct Fork {
    from: ExternalThreadState,
    prefix: Vec<ResponseItem>,
}

impl Backend {
    pub(crate) fn new(
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        models: SharedModelsManager,
        installation_id: String,
        originator: String,
    ) -> Self {
        Self {
            web_search: StandaloneSearch::from_config(&config, Arc::clone(&auth_manager)),
            images: StandaloneImages::from_config(
                &config,
                Arc::clone(&auth_manager),
                Some(originator.clone()),
            ),
            mcp: McpConnector::new(&config),
            watchdog: Arc::new(Watchdog::load(&config.codex_home)),
            config,
            auth_manager,
            models,
            installation_id,
            originator,
            git_root_discovery: Arc::new(GitRootDiscovery::default()),
            sessions: StdMutex::new(HashMap::new()),
        }
    }

    /// Effective metadata of a catalog model; other names are unknown to the backend.
    pub(crate) async fn model_info(&self, model: &str) -> Result<ModelInfo, ApiError> {
        if !self
            .models
            .get_remote_models()
            .await
            .iter()
            .any(|info| info.slug == model)
        {
            return Err(ApiError::not_found(format!("model: {model}")));
        }
        Ok(self
            .models
            .get_model_info(model, &self.config.to_models_manager_config())
            .await)
    }

    /// Evicts idle sessions and prunes expired session logs and compactions, now and every
    /// `MAINTENANCE_INTERVAL`.
    pub(crate) fn spawn_maintenance(self: &Arc<Self>) {
        let backend = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let resident = {
                    let mut sessions = backend
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sessions.retain(|_, session| session.idle_for() < SESSION_IDLE_TTL);
                    sessions
                        .values()
                        .map(|session| session.log.path().to_path_buf())
                        .collect::<HashSet<_>>()
                };
                persist::prune(&backend.config.codex_home, &resident).await;
                compactions::prune(&backend.config.codex_home).await;
                tokio::time::sleep(MAINTENANCE_INTERVAL).await;
            }
        });
    }

    /// The thread answering a request, and the lane it sends on.
    pub(crate) async fn lane(
        &self,
        identity: &Identity,
        probe: &Probe<'_>,
    ) -> (Arc<Thread>, OwnedMutexGuard<Lane>) {
        let session = self.session(&identity.session_id, probe).await;
        *session
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        let thread = match &identity.agent_id {
            None => Arc::clone(&session.root),
            Some(agent_id) => self.agent(&session, agent_id, identity.parent_agent_id.as_deref()),
        };
        let (mut lane, matched) = match thread.select(probe) {
            Some(selected) => selected,
            None => (thread.add_lane(probe.fingerprint).await, 0),
        };
        if !lane.open() {
            session.seed(&mut lane, probe, matched);
        }
        (thread, lane)
    }

    /// `agent_id`'s thread, spawned under `parent_agent_id`'s when that agent is known, else
    /// under the root.
    fn agent(
        &self,
        session: &Session,
        agent_id: &str,
        parent_agent_id: Option<&str>,
    ) -> Arc<Thread> {
        let mut agents = session
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(thread) = agents.get(agent_id) {
            return Arc::clone(thread);
        }
        let parent = parent_agent_id
            .and_then(|parent| agents.get(parent))
            .unwrap_or(&session.root)
            .external
            .state();
        let thread = Arc::new(self.new_thread(
            &session.log,
            Some(agent_id),
            ExternalThreadRole::Subagent {
                session_id: parent.session_id(),
                parent_thread_id: parent.thread_id,
                depth: parent.depth() + 1,
            },
        ));
        session.log.thread(Some(agent_id), thread.external.state());
        agents.insert(agent_id.to_string(), Arc::clone(&thread));
        thread
    }

    async fn session(&self, session_id: &str, probe: &Probe<'_>) -> Arc<Session> {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
        {
            return Arc::clone(session);
        }
        let log = Arc::new(SessionLog::new(
            &self.config.codex_home,
            session_id,
            Arc::clone(&self.watchdog),
        ));
        let mut restored = log.restore().await;
        let (session, created) = match restored.threads.remove(&None) {
            Some(root) => (self.restore_session(log, root, restored), false),
            None => {
                let fork = self.fork(log.path(), probe).await;
                (self.new_session(log, fork, probe.fingerprint), true)
            }
        };
        let session = Arc::new(session);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match sessions.entry(session_id.to_string()) {
            Entry::Occupied(existing) => Arc::clone(existing.get()),
            Entry::Vacant(vacant) => {
                if created {
                    session.log.thread(None, session.root.external.state());
                }
                Arc::clone(vacant.insert(session))
            }
        }
    }

    fn new_session(&self, log: Arc<SessionLog>, fork: Option<Fork>, fingerprint: u64) -> Session {
        let role = match &fork {
            Some(fork) => ExternalThreadRole::Fork {
                session_id: fork.from.session_id(),
                forked_from: fork.from.thread_id,
            },
            None => ExternalThreadRole::Root,
        };
        let root = self.new_thread(&log, None, role);
        if let Some(fork) = fork {
            root.push_lane(
                fingerprint,
                RestoredLane {
                    history: fork.prefix,
                    ..RestoredLane::default()
                },
                /*restored*/ None,
            );
        }
        Session {
            root: Arc::new(root),
            agents: StdMutex::new(HashMap::new()),
            last_used: StdMutex::new(Instant::now()),
            log,
        }
    }

    fn restore_session(
        &self,
        log: Arc<SessionLog>,
        root: ExternalThreadState,
        restored: Restored,
    ) -> Session {
        let Restored { threads, lanes } = restored;
        let root = self.thread(&log, None, ExternalThread::resume, root);
        let agents = threads
            .into_iter()
            .filter_map(|(agent, state)| {
                let agent = agent?;
                let thread = self.thread(&log, Some(&agent), ExternalThread::resume, state);
                Some((agent, Arc::new(thread)))
            })
            .collect::<HashMap<_, _>>();
        for ((agent, fingerprint, index), lane) in lanes {
            let thread = match &agent {
                None => &root,
                Some(agent) => match agents.get(agent) {
                    Some(thread) => thread.as_ref(),
                    None => continue,
                },
            };
            thread.push_lane(fingerprint, lane, Some(index));
        }
        Session {
            root: Arc::new(root),
            agents: StdMutex::new(agents),
            last_used: StdMutex::new(Instant::now()),
            log,
        }
    }

    /// The thread whose recorded history a new session's first request repeats furthest,
    /// past a model output: that session forked it. Resident sessions first, then logs.
    async fn fork(&self, own_log: &Path, probe: &Probe<'_>) -> Option<Fork> {
        let first = probe.items.first()?;
        if !probe.items.iter().any(is_model_output) {
            return None;
        }
        let resident = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut best = resident
            .iter()
            .filter_map(|session| session.longest_prefix(probe))
            .max_by_key(|(_, prefix)| prefix.len());
        let mut skip = resident
            .iter()
            .map(|session| session.log.path().to_path_buf())
            .collect::<HashSet<PathBuf>>();
        skip.insert(own_log.to_path_buf());
        for path in
            persist::fork_candidates(&self.config.codex_home, &skip, probe.fingerprint, first).await
        {
            let restored = persist::restore(&path).await;
            for ((agent, fingerprint, _), lane) in &restored.lanes {
                let Some(state) = restored.threads.get(agent) else {
                    continue;
                };
                if *fingerprint != probe.fingerprint {
                    continue;
                }
                let matched = common_prefix(&lane.history, probe.items);
                if best
                    .as_ref()
                    .is_none_or(|(_, prefix)| matched > prefix.len())
                {
                    best = Some((*state, lane.history[..matched].to_vec()));
                }
            }
        }
        best.filter(|(_, prefix)| prefix.iter().any(is_model_output))
            .map(|(from, prefix)| Fork { from, prefix })
    }

    fn new_thread(
        &self,
        log: &Arc<SessionLog>,
        agent: Option<&str>,
        role: ExternalThreadRole,
    ) -> Thread {
        self.thread(log, agent, ExternalThread::new, role)
    }

    /// A thread whose Codex identity `external` builds from `identity`: `ExternalThread::new`
    /// with a role, `ExternalThread::resume` with a persisted state.
    fn thread<I>(
        &self,
        log: &Arc<SessionLog>,
        agent: Option<&str>,
        external: ExternalThreadConstructor<I>,
        identity: I,
    ) -> Thread {
        Thread {
            external: Arc::new(external(
                Arc::clone(&self.config),
                Arc::clone(&self.auth_manager),
                self.installation_id.clone(),
                self.originator.clone(),
                Arc::clone(&self.git_root_discovery),
                identity,
            )),
            agent: agent.map(str::to_string),
            log: Arc::clone(log),
            lanes: StdMutex::new(Vec::new()),
        }
    }
}

impl Session {
    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }

    /// The longest recorded history prefix `probe` repeats, across the session's threads.
    fn longest_prefix(
        &self,
        probe: &Probe<'_>,
    ) -> Option<(ExternalThreadState, Vec<ResponseItem>)> {
        let (thread, donor) = self.donor(probe)?;
        let history = Arc::clone(
            &donor
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .history,
        );
        let matched = common_prefix(&history, probe.items);
        Some((thread.external.state(), history[..matched].to_vec()))
    }

    /// Restarts `lane` from the longest recorded history `probe` repeats, when that goes past
    /// the `matched` items it repeats of `lane`. The log references the donor lane, read and
    /// referenced under its snapshot lock so both name the same history.
    fn seed(&self, lane: &mut Lane, probe: &Probe<'_>, matched: usize) {
        let Some((_, donor)) = self
            .donor(probe)
            .filter(|(_, donor)| donor.matched > matched)
        else {
            return;
        };
        let shared = donor
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seed = common_prefix(&shared.history, probe.items);
        if seed <= matched {
            return;
        }
        lane.history = Arc::new(shared.history[..seed].to_vec());
        self.log.seed(&lane.key, &donor.key, seed, &lane.history[0]);
        lane.logged = seed;
        lane.prompt = None;
    }

    /// The lane whose recorded history `probe` repeats furthest, across the session's threads.
    fn donor(&self, probe: &Probe<'_>) -> Option<(Arc<Thread>, Donor)> {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        std::iter::once(Arc::clone(&self.root))
            .chain(agents)
            .filter_map(|thread| {
                let donor = thread.donor(probe)?;
                Some((thread, donor))
            })
            .max_by_key(|(_, donor)| donor.matched)
    }
}

/// A lane other lanes may start from.
struct Donor {
    key: LaneKey,
    shared: Arc<StdMutex<Shared>>,
    /// Leading items of its recorded history the request repeats.
    matched: usize,
}

impl Thread {
    /// The idle lane `probe` should continue, and how much of its history it repeats: one
    /// whose open turn it continues, else the one it repeats furthest. `None` = a new lane.
    fn select(&self, probe: &Probe<'_>) -> Option<(OwnedMutexGuard<Lane>, usize)> {
        let slots = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut best: Option<((bool, usize), OwnedMutexGuard<Lane>)> = None;
        // Idle lanes `probe` repeats nothing of, by last use.
        let mut oldest: Option<(Instant, OwnedMutexGuard<Lane>)> = None;
        let mut count = 0;
        for slot in slots
            .iter()
            .filter(|slot| slot.fingerprint == probe.fingerprint)
        {
            count += 1;
            let Ok(lane) = Arc::clone(&slot.lane).try_lock_owned() else {
                continue;
            };
            let matched = common_prefix(&lane.history, probe.items);
            let continues = lane.continued_by(matched, probe.items);
            if lane.open() && !continues {
                continue;
            }
            if continues || matched > 0 {
                let rank = (continues, matched);
                if best.as_ref().is_none_or(|(best, _)| rank > *best) {
                    best = Some((rank, lane));
                }
            } else {
                let used = lane.last_used();
                if oldest.as_ref().is_none_or(|(oldest, _)| used < *oldest) {
                    oldest = Some((used, lane));
                }
            }
        }
        let (lane, matched) = match best {
            Some(((_, matched), lane)) => (lane, matched),
            None if count >= LANES_PER_INSTRUCTIONS => (oldest?.1, 0),
            None => return None,
        };
        lane.touch();
        Some((lane, matched))
    }

    async fn add_lane(&self, fingerprint: u64) -> OwnedMutexGuard<Lane> {
        self.push_lane(fingerprint, RestoredLane::default(), /*restored*/ None)
            .lock_owned()
            .await
    }

    /// Adds a lane holding `state`; `restored` = its index in the session log, which holds
    /// its history.
    fn push_lane(
        &self,
        fingerprint: u64,
        state: RestoredLane,
        restored: Option<usize>,
    ) -> Arc<Mutex<Lane>> {
        let mut slots = self
            .lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = restored.unwrap_or_else(|| {
            slots
                .iter()
                .filter(|slot| slot.fingerprint == fingerprint)
                .map(|slot| slot.index + 1)
                .max()
                .unwrap_or(0)
        });
        let history = Arc::new(state.history);
        let shared = Arc::new(StdMutex::new(Shared {
            history: Arc::clone(&history),
            last_used: Instant::now(),
        }));
        let lane = Arc::new(Mutex::new(Lane {
            client: self.external.model_client(),
            logged: if restored.is_some() { history.len() } else { 0 },
            history,
            turn: None,
            pending_calls: state.pending_calls,
            resume_turn: state.resume_turn,
            prewarmed: false,
            prompt: state.prompt.map(|prompt| SentPrompt {
                items: prompt.items,
                input_tokens: prompt.input_tokens,
                sent: None,
            }),
            cache: CacheStreak::default(),
            key: (self.agent.clone(), fingerprint, index),
            shared: Arc::clone(&shared),
        }));
        slots.push(Slot {
            fingerprint,
            index,
            lane: Arc::clone(&lane),
            shared,
        });
        lane
    }

    /// This thread's lane whose recorded history `probe` repeats furthest.
    fn donor(&self, probe: &Probe<'_>) -> Option<Donor> {
        self.lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|slot| slot.fingerprint == probe.fingerprint)
            .map(|slot| {
                let history = Arc::clone(
                    &slot
                        .shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .history,
                );
                Donor {
                    key: (self.agent.clone(), slot.fingerprint, slot.index),
                    shared: Arc::clone(&slot.shared),
                    matched: common_prefix(&history, probe.items),
                }
            })
            .filter(|donor| donor.matched > 0)
            .max_by_key(|donor| donor.matched)
    }

    /// Persists the lane: its first `keep` items stand, the rest of its history follows.
    /// Logged and published under the snapshot lock, so a lane seeded from this one
    /// references the history it copied.
    pub(crate) fn record(&self, lane: &mut Lane, keep: usize) {
        let keep = keep.min(lane.logged).min(lane.history.len());
        let shared = Arc::clone(&lane.shared);
        let mut shared = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.log.lane(
            &lane.key,
            keep,
            &lane.history[keep..],
            &lane.pending_calls,
            lane.resume_turn,
            lane.prompt.as_ref().map(|prompt| LoggedPrompt {
                items: prompt.items,
                input_tokens: prompt.input_tokens,
            }),
        );
        lane.logged = lane.history.len();
        shared.history = Arc::clone(&lane.history);
    }

    /// Persists the context window a compaction started.
    pub(crate) fn record_window(&self) {
        self.log
            .thread(self.agent.as_deref(), self.external.state());
    }
}

impl Lane {
    /// Whether a request repeating `matched` recorded items, then `items[matched..]`, continues
    /// this lane's open turn. Repeating all of it is required: forks (agent summaries) answer
    /// the pending calls too.
    pub(crate) fn continued_by(&self, matched: usize, items: &[ResponseItem]) -> bool {
        self.resume_turn
            || (matched == self.history.len()
                && !self.pending_calls.is_empty()
                && self.pending_calls.iter().all(|call_id| {
                    items.iter().any(|item| {
                        matches!(item, ResponseItem::FunctionCallOutput { call_id: Some(id), .. } if id == call_id)
                    })
                }))
    }

    pub(crate) fn index(&self) -> usize {
        self.key.2
    }

    fn open(&self) -> bool {
        self.resume_turn || !self.pending_calls.is_empty()
    }

    fn last_used(&self) -> Instant {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_used
    }

    fn touch(&self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_used = Instant::now();
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
