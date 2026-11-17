use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

use anyhow::Context;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::turn_input::TurnStartOptions;
use codex_tools::ResponsesApiTool;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::agent::api::AgentInfo;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;

use super::Runtime;
use super::arguments;
use super::error;
use super::runtime;
use super::tool;

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod tests;

type AgentUpdates =
    Pin<Box<dyn Stream<Item = codex_protocol::error::Result<AgentInfo>> + Send + 'static>>;

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    TaskCreate,
    TaskGet,
    TaskList,
    TaskUpdate,
    Shutdown,
}

pub(crate) fn specs() -> Vec<(Tool, ResponsesApiTool)> {
    vec![
        (
            Tool::TaskCreate,
            tool(
                "task_create",
                "Create a persistent task in this session's shared team task list. Idle teammates claim eligible unowned tasks automatically. Dependencies prevent a task from starting until those tasks complete.",
                json!({"type":"object","properties":{
                    "subject":{"type":"string"},"description":{"type":"string"},
                    "activeForm":{"type":"string"},"owner":{"type":"string"},
                    "blockedBy":{"type":"array","items":{"type":"string"}},
                    "metadata":{"type":"object"}
                },"required":["subject","description"],"additionalProperties":false}),
            ),
        ),
        (
            Tool::TaskGet,
            tool(
                "task_get",
                "Read a shared task, including its owner, status, dependencies and description.",
                json!({"type":"object","properties":{"taskId":{"type":"string"}},"required":["taskId"],"additionalProperties":false}),
            ),
        ),
        (
            Tool::TaskList,
            tool(
                "task_list",
                "List this session's shared tasks, with status, owner and dependencies.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
        ),
        (
            Tool::TaskUpdate,
            tool(
                "task_update",
                "Update a shared task. status is pending, in_progress, completed or deleted. Claiming in_progress is atomic and cannot steal another teammate's claim. owner assigns a teammate by name; an empty owner releases it. addBlocks/addBlockedBy add dependency edges. Mark work completed to release dependent tasks to idle teammates.",
                json!({"type":"object","properties":{
                    "taskId":{"type":"string"},"subject":{"type":"string"},
                    "description":{"type":"string"},"activeForm":{"type":"string"},
                    "owner":{"type":"string"},
                    "status":{"type":"string","enum":["pending","in_progress","completed","deleted"]},
                    "addBlocks":{"type":"array","items":{"type":"string"}},
                    "addBlockedBy":{"type":"array","items":{"type":"string"}},
                    "metadata":{"type":"object"}
                },"required":["taskId"],"additionalProperties":false}),
            ),
        ),
        (
            Tool::Shutdown,
            tool(
                "team_shutdown",
                "Request a teammate's cooperative shutdown. The teammate responds with the request_id and approve=true or false. Approval closes its native thread after acknowledging the response; rejection keeps it available.",
                json!({"type":"object","properties":{"target":{"type":"string"},"request_id":{"type":"string"},"approve":{"type":"boolean"},"reason":{"type":"string"}},"additionalProperties":false}),
            ),
        ),
    ]
}

#[derive(Clone, Serialize, Deserialize)]
struct Member {
    id: ThreadId,
    parent: ThreadId,
    name: Option<String>,
    path: String,
    shutdown: bool,
    run_in_background: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    id: String,
    subject: String,
    description: String,
    active_form: Option<String>,
    status: TaskStatus,
    owner: Option<String>,
    blocked_by: Vec<String>,
    metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ShutdownRequest {
    requester: ThreadId,
    recipient: ThreadId,
    reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct State {
    root: Option<ThreadId>,
    next_task_id: u64,
    members: BTreeMap<String, Member>,
    aliases: BTreeMap<String, String>,
    tasks: BTreeMap<String, Task>,
    shutdown_requests: BTreeMap<String, ShutdownRequest>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            root: None,
            next_task_id: 1,
            members: BTreeMap::new(),
            aliases: BTreeMap::new(),
            tasks: BTreeMap::new(),
            shutdown_requests: BTreeMap::new(),
        }
    }
}

pub(crate) struct Team {
    path: PathBuf,
    state: Mutex<State>,
    runtimes: Mutex<BTreeMap<String, Weak<Runtime>>>,
    watchers: Mutex<BTreeMap<String, Vec<JoinHandle<()>>>>,
    closing: Mutex<HashSet<ThreadId>>,
    dispatch: Mutex<()>,
    admissions: Arc<RwLock<()>>,
}

impl Team {
    pub(crate) fn new() -> Self {
        Self {
            path: PathBuf::new(),
            state: Mutex::new(State::default()),
            runtimes: Mutex::default(),
            watchers: Mutex::default(),
            closing: Mutex::default(),
            dispatch: Mutex::default(),
            admissions: Arc::default(),
        }
    }

    pub(crate) async fn shared(
        session_id: SessionId,
        codex_home: &Path,
    ) -> anyhow::Result<Arc<Self>> {
        static TEAMS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Team>>>> = OnceLock::new();
        let path = codex_home
            .join("claude-code")
            .join("teams")
            .join(session_id.to_string())
            .join("state.json");
        let mut teams = TEAMS.get_or_init(Mutex::default).lock().await;
        if let Some(team) = teams.get(&path).and_then(Weak::upgrade) {
            return Ok(team);
        }
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(err) => return Err(err.into()),
        };
        let team = Arc::new(Self {
            path: path.clone(),
            state: Mutex::new(state),
            runtimes: Mutex::default(),
            watchers: Mutex::default(),
            closing: Mutex::default(),
            dispatch: Mutex::default(),
            admissions: Arc::default(),
        });
        teams.insert(path, Arc::downgrade(&team));
        Ok(team)
    }

    async fn save(&self, state: &State) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(self.path.parent().unwrap()).await?;
        let temporary = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary, serde_json::to_vec_pretty(state)?).await?;
        tokio::fs::rename(temporary, &self.path).await?;
        Ok(())
    }

    pub(crate) async fn watch(&self, owner: ThreadId, watcher: JoinHandle<()>) {
        let mut watchers = self.watchers.lock().await;
        let watchers = watchers.entry(owner.to_string()).or_default();
        watchers.retain(|watcher| !watcher.is_finished());
        watchers.push(watcher);
    }

    pub(crate) async fn reports_result(&self, child: ThreadId) -> bool {
        let _admissions = self.admissions.write().await;
        self.state
            .lock()
            .await
            .members
            .get(&child.to_string())
            .is_none_or(|member| member.run_in_background)
    }

    pub(crate) async fn shutdown(&self) {
        let watchers = std::mem::take(&mut *self.watchers.lock().await);
        for watcher in watchers.into_values().flatten() {
            watcher.abort();
            let _ = watcher.await;
        }
    }
}

pub(crate) async fn on_ready(runtime: &Arc<Runtime>) -> anyhow::Result<()> {
    let thread = runtime
        .thread
        .upgrade()
        .context("thread stopped during team startup")?;
    let id = thread.session.thread_id;
    runtime
        .delegation
        .runtimes
        .lock()
        .await
        .insert(id.to_string(), Arc::downgrade(runtime));
    let mut state = runtime.delegation.state.lock().await;
    if state.root.is_none() {
        state.root = Some(id);
        runtime.delegation.save(&state).await?;
    }
    Ok(())
}

pub(crate) async fn on_stopped(runtime: &Arc<Runtime>) {
    let Some(thread) = runtime.thread.upgrade() else {
        return;
    };
    let id = thread.session.thread_id.to_string();
    runtime.delegation.runtimes.lock().await.remove(&id);
    let watchers = runtime.delegation.watchers.lock().await.remove(&id);
    for watcher in watchers.into_iter().flatten() {
        watcher.abort();
        let _ = watcher.await;
    }
}

pub(crate) async fn call(
    tool: Tool,
    invocation: ToolInvocation,
) -> Result<Value, FunctionCallError> {
    let runtime = runtime(&invocation);
    match tool {
        Tool::TaskCreate => task_create(&invocation, &runtime).await,
        Tool::TaskGet => {
            let args: TaskId = arguments(&invocation)?;
            let state = runtime.delegation.state.lock().await;
            let task = state
                .tasks
                .get(&args.task_id)
                .ok_or_else(|| error(format!("task {} does not exist", args.task_id)))?;
            Ok(task_value(&state, task))
        }
        Tool::TaskList => {
            let state = runtime.delegation.state.lock().await;
            Ok(
                json!({"tasks":state.tasks.values().map(|task|task_value(&state,task)).collect::<Vec<_>>()}),
            )
        }
        Tool::TaskUpdate => task_update(&invocation, &runtime).await,
        Tool::Shutdown => shutdown(&invocation, &runtime).await,
    }
}

pub(crate) struct NativeSpawn {
    runtime: Arc<Runtime>,
    name: Option<String>,
    run_in_background: bool,
    admission: tokio::sync::OwnedRwLockReadGuard<()>,
}

#[derive(Debug, Serialize)]
pub(crate) struct NativeSpawnResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
}

pub(crate) async fn prepare_native_spawn(
    session: &Arc<crate::session::session::Session>,
    source: &codex_protocol::protocol::SessionSource,
    name: Option<String>,
    run_in_background: Option<bool>,
) -> Result<Option<NativeSpawn>, FunctionCallError> {
    if !session.enabled(codex_features::Feature::ClaudeDelegation) {
        if name.is_some() || run_in_background.is_some() {
            return Err(error(
                "name and run_in_background require claude_delegation",
            ));
        }
        return Ok(None);
    }
    let runtime = super::session_runtime(session).unwrap();
    if name.as_deref().is_some_and(|name| {
        name.trim().is_empty()
            || matches!(name, "main" | "team-lead" | "user" | "system" | "*")
            || ThreadId::from_string(name).is_ok()
    }) {
        return Err(error("this teammate name is reserved or empty"));
    }
    let run_in_background = run_in_background.unwrap_or(true);
    let registration_pending = source.parent_thread_id().is_some()
        && !runtime
            .delegation
            .state
            .lock()
            .await
            .members
            .contains_key(&session.thread_id.to_string());
    if registration_pending {
        // A child can invoke a tool before the parent's spawn call registers its name.
        drop(runtime.delegation.admissions.write().await);
    }
    let caller_is_teammate = runtime
        .delegation
        .state
        .lock()
        .await
        .members
        .get(&session.thread_id.to_string())
        .is_some_and(|member| member.name.is_some());
    if caller_is_teammate && run_in_background {
        return Err(error(
            "Teammates must use run_in_background=false for nested spawn_agent calls.",
        ));
    }
    let admission = Arc::clone(&runtime.delegation.admissions)
        .read_owned()
        .await;
    Ok(Some(NativeSpawn {
        runtime,
        name,
        run_in_background,
        admission,
    }))
}

impl NativeSpawn {
    pub(crate) fn decorate_prompt(&self, prompt: String) -> String {
        match self.name.as_deref() {
            Some(name) => format!(
                "You are teammate {name}. Communicate using the native agent messaging tools. Shared work is available through task_list, task_get and task_update. Mark completed tasks completed before finishing. You remain available for later messages and assignments.\n\n{prompt}"
            ),
            None => prompt,
        }
    }

    pub(crate) async fn finish(
        self,
        session: Arc<crate::session::session::Session>,
        cancellation: tokio_util::sync::CancellationToken,
        id: ThreadId,
        path: String,
        version: MultiAgentVersion,
    ) -> Result<NativeSpawnResult, FunctionCallError> {
        let Self {
            runtime,
            name,
            run_in_background,
            admission,
        } = self;
        let member = Member {
            id,
            parent: session.thread_id,
            name: name.clone(),
            path,
            shutdown: false,
            run_in_background,
        };
        {
            let mut state = runtime.delegation.state.lock().await;
            if let Some(name) = &name {
                state.aliases.insert(name.clone(), id.to_string());
            }
            state.members.insert(id.to_string(), member);
            runtime.delegation.save(&state).await.map_err(error)?;
        }
        drop(admission);
        // A short initial turn can finish before its teammate name is registered.
        wake_teammates(&runtime.delegation).await;
        if run_in_background {
            return Ok(NativeSpawnResult {
                name,
                status: "background",
                result: None,
            });
        }
        let updates = session
            .services
            .local_agent_runtime
            .control(session.session_id())
            .subscribe_status(id)
            .await
            .map_err(error)?;
        let owner = session.thread_id;
        let foreground_runtime = Arc::clone(&runtime);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let watcher = tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancellation.cancelled() => {
                    session.services.agent_control.interrupt(
                        owner,
                        AgentTarget::Id(id),
                        version,
                    ).await.map_err(error).and_then(|_| Err(error("foreground agent interrupted")))
                }
                result = terminal_result(updates, &foreground_runtime, id) => result.map_err(error),
            };
            let cleanup = stop_foreground_tasks(&foreground_runtime.delegation, id)
                .await
                .map_err(error);
            let _ = sender.send(cleanup.and(result));
        });
        runtime.delegation.watch(owner, watcher).await;
        Ok(NativeSpawnResult {
            name,
            status: "completed",
            result: Some(receiver.await.map_err(error)??),
        })
    }
}

pub(crate) async fn resolve_alias(
    session: &crate::session::session::Session,
    target: &str,
) -> Option<ThreadId> {
    if !session.enabled(codex_features::Feature::ClaudeDelegation) {
        return None;
    }
    let runtime = super::session_runtime(session).unwrap();
    let state = runtime.delegation.state.lock().await;
    if matches!(target, "main" | "team-lead") {
        return state.root;
    }
    state
        .aliases
        .get(target)
        .and_then(|id| state.members.get(id))
        .map(|member| member.id)
}

pub(crate) async fn native_followup(
    session: &crate::session::session::Session,
    id: ThreadId,
) -> Result<(), FunctionCallError> {
    update_native_member(session, id, false, true).await
}

pub(crate) async fn native_closed(
    session: &crate::session::session::Session,
    id: ThreadId,
) -> Result<(), FunctionCallError> {
    update_native_member(session, id, true, true).await
}

async fn update_native_member(
    session: &crate::session::session::Session,
    id: ThreadId,
    shutdown: bool,
    run_in_background: bool,
) -> Result<(), FunctionCallError> {
    if !session.enabled(codex_features::Feature::ClaudeDelegation) {
        return Ok(());
    }
    let runtime = super::session_runtime(session).unwrap();
    let mut state = runtime.delegation.state.lock().await;
    if let Some(member) = state.members.get_mut(&id.to_string()) {
        member.shutdown = shutdown;
        member.run_in_background = run_in_background;
        runtime.delegation.save(&state).await.map_err(error)?;
    }
    Ok(())
}

async fn stop_foreground_tasks(team: &Team, child: ThreadId) -> anyhow::Result<()> {
    let runtime = team
        .runtimes
        .lock()
        .await
        .get(&child.to_string())
        .and_then(Weak::upgrade);
    if let Some(runtime) = runtime {
        runtime.tasks.shutdown_quiet().await?;
    }
    Ok(())
}

pub(crate) async fn settled_status(
    session: &crate::session::session::Session,
    child: ThreadId,
    status: AgentStatus,
) -> Result<AgentStatus, FunctionCallError> {
    if !session.enabled(codex_features::Feature::ClaudeDelegation)
        || !matches!(status, AgentStatus::Completed(_))
    {
        return Ok(status);
    }
    let runtime = super::session_runtime(session).unwrap();
    let child_runtime = runtime
        .delegation
        .runtimes
        .lock()
        .await
        .get(&child.to_string())
        .and_then(Weak::upgrade);
    match child_runtime {
        Some(child_runtime) => {
            super::events::wait_until_idle(&child_runtime, &runtime.cancellation)
                .await
                .map_err(error)
        }
        None => Ok(status),
    }
}

async fn terminal_result(
    mut updates: AgentUpdates,
    runtime: &Runtime,
    child: ThreadId,
) -> anyhow::Result<Value> {
    loop {
        let next = tokio::select! {
            _ = runtime.cancellation.cancelled() => anyhow::bail!("agent observation cancelled"),
            next = updates.next() => next.context("agent status subscription closed")??,
        };
        let mut status = next.status().cloned();
        if matches!(status, Some(AgentStatus::Completed(_))) {
            let child_runtime = runtime
                .delegation
                .runtimes
                .lock()
                .await
                .get(&child.to_string())
                .and_then(Weak::upgrade);
            if let Some(child_runtime) = child_runtime {
                status = Some(
                    super::events::wait_until_idle(&child_runtime, &runtime.cancellation).await?,
                );
            }
        }
        match status {
            Some(AgentStatus::Completed(text)) => return Ok(json!({"text":text})),
            Some(AgentStatus::Errored(message)) => anyhow::bail!("{message}"),
            Some(AgentStatus::Interrupted) => anyhow::bail!("agent interrupted"),
            Some(AgentStatus::Shutdown | AgentStatus::NotFound) => anyhow::bail!("agent stopped"),
            _ => {}
        }
    }
}

async fn member(team: &Team, target: &str) -> Result<Member, FunctionCallError> {
    let state = team.state.lock().await;
    let id = state
        .aliases
        .get(target)
        .map(String::as_str)
        .unwrap_or(target);
    let member = state
        .members
        .get(id)
        .or_else(|| state.members.values().find(|member| member.path == target))
        .ok_or_else(|| error(format!("teammate {target} does not exist")))?;
    if member.shutdown {
        return Err(error(format!("teammate {target} has shut down")));
    }
    Ok(member.clone())
}

async fn send_native(
    invocation: &ToolInvocation,
    recipient: ThreadId,
    message: String,
    wake: bool,
) -> Result<(), FunctionCallError> {
    invocation
        .session
        .services
        .agent_control
        .send(SendRequest {
            caller: invocation.session.thread_id,
            target: AgentTarget::Id(recipient),
            resume_config: build_agent_resume_config(&invocation.turn).map_err(error)?,
            input: AgentInput::Message {
                message: AgentMessage::Plaintext(message),
                mode: if wake {
                    MessageDeliveryMode::TriggerTurn
                } else {
                    MessageDeliveryMode::QueueOnly
                },
            },
            start_options: TurnStartOptions::default(),
        })
        .await
        .map_err(error)?;
    Ok(())
}

pub(crate) async fn native_message_targets(
    session: &crate::session::session::Session,
    source: &codex_protocol::protocol::SessionSource,
    target: &str,
) -> Option<Vec<ThreadId>> {
    if !session.enabled(codex_features::Feature::ClaudeDelegation) {
        return None;
    }
    let runtime = super::session_runtime(session).unwrap();
    let state = runtime.delegation.state.lock().await;
    let targets = state.message_targets(
        session.thread_id,
        &source
            .get_agent_path()
            .unwrap_or_else(codex_protocol::AgentPath::root),
        target,
    );
    drop(state);
    if targets.is_some() {
        return targets;
    }
    let native = &session.services.local_agent_runtime;
    if let Ok(id) = ThreadId::from_string(target)
        && native.ensure_agent_known(id).is_ok()
    {
        return Some(vec![id]);
    }
    native
        .resolve_agent_reference(session.thread_id, source, target)
        .await
        .ok()
        .map(|id| vec![id])
}

impl State {
    fn message_targets(
        &self,
        caller: ThreadId,
        caller_path: &codex_protocol::AgentPath,
        target: &str,
    ) -> Option<Vec<ThreadId>> {
        if target == "*" {
            return Some(
                self.aliases
                    .values()
                    .filter_map(|id| self.members.get(id))
                    .filter(|member| member.id != caller && !member.shutdown)
                    .map(|member| member.id)
                    .collect(),
            );
        }
        let root = self.root.unwrap();
        if matches!(target, "main" | "team-lead" | "/root") || target == root.to_string() {
            return Some(vec![root]);
        }
        let id = self
            .aliases
            .get(target)
            .map(String::as_str)
            .unwrap_or(target);
        let relative_path = caller_path.join(target).ok();
        self.members
            .get(id)
            .or_else(|| {
                self.members.values().find(|member| {
                    member.path == target
                        || relative_path
                            .as_ref()
                            .is_some_and(|path| path.as_str() == member.path)
                })
            })
            .map(|member| vec![member.id])
    }
}

/// Returns None for a peer address outside this native agent tree.
pub(crate) async fn try_send_native_message(
    invocation: &ToolInvocation,
    target: &str,
    message: String,
) -> Result<Option<Value>, FunctionCallError> {
    let Some(recipients) =
        native_message_targets(&invocation.session, &invocation.turn.session_source, target).await
    else {
        return Ok(None);
    };
    if message.trim().is_empty() {
        return Err(error("message must not be empty"));
    }
    let runtime = runtime(invocation);
    let root = runtime.delegation.state.lock().await.root.unwrap();
    for recipient in &recipients {
        if *recipient == root && invocation.turn.multi_agent_version != MultiAgentVersion::V2 {
            let recipient = runtime
                .delegation
                .runtimes
                .lock()
                .await
                .get(&root.to_string())
                .and_then(Weak::upgrade)
                .ok_or_else(|| error("main conversation is not loaded"))?;
            recipient
                .events
                .emit("teammate-message", message.clone())
                .await
                .map_err(error)?;
        } else {
            native_followup(&invocation.session, *recipient).await?;
            send_native(invocation, *recipient, message.clone(), true).await?;
        }
    }
    Ok(Some(json!({"delivered":true,"recipients":recipients})))
}

pub(crate) async fn list_native_agents(
    invocation: &ToolInvocation,
) -> Result<Vec<Value>, FunctionCallError> {
    if !invocation
        .session
        .enabled(codex_features::Feature::ClaudeDelegation)
    {
        return Ok(Vec::new());
    }
    let agents = invocation
        .session
        .services
        .agent_control
        .list(
            invocation.session.thread_id,
            invocation.turn.parent_thread_id,
            &invocation.turn.session_source,
            None,
        )
        .await
        .map_err(error)?;
    let runtime = runtime(invocation);
    let state = runtime.delegation.state.lock().await;
    Ok(agents.into_iter().map(|agent| {
        let name = state.members.get(&agent.thread_id.to_string())
            .and_then(|member| member.name.as_ref())
            .filter(|name| state.aliases.get(*name) == Some(&agent.thread_id.to_string()));
        json!({"agent_id":agent.thread_id,"agent_name":agent.metadata.agent_path,"name":name,"agent_status":agent.status})
    }).collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskId {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTask {
    subject: String,
    description: String,
    active_form: Option<String>,
    owner: Option<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    metadata: BTreeMap<String, Value>,
}

async fn task_create(
    invocation: &ToolInvocation,
    runtime: &Arc<Runtime>,
) -> Result<Value, FunctionCallError> {
    let args: CreateTask = arguments(invocation)?;
    let mut state = runtime.delegation.state.lock().await;
    let mut next = state.clone();
    let id = state.next_task_id.to_string();
    let task = Task {
        id: id.clone(),
        subject: args.subject,
        description: args.description,
        active_form: args.active_form,
        status: TaskStatus::Pending,
        owner: args.owner.filter(|owner| !owner.is_empty()),
        blocked_by: args.blocked_by,
        metadata: args.metadata,
    };
    for dependency in &task.blocked_by {
        if !state.tasks.contains_key(dependency) {
            return Err(error(format!(
                "dependency task {dependency} does not exist"
            )));
        }
    }
    next.next_task_id += 1;
    next.tasks.insert(id, task.clone());
    runtime.delegation.save(&next).await.map_err(error)?;
    let result = json!({"task":task_value(&next,&task)});
    *state = next;
    drop(state);
    wake_teammates(&runtime.delegation).await;
    Ok(result)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateTask {
    task_id: String,
    subject: Option<String>,
    description: Option<String>,
    active_form: Option<String>,
    owner: Option<String>,
    status: Option<String>,
    #[serde(default)]
    add_blocks: Vec<String>,
    #[serde(default)]
    add_blocked_by: Vec<String>,
    metadata: Option<BTreeMap<String, Value>>,
}

async fn task_update(
    invocation: &ToolInvocation,
    runtime: &Arc<Runtime>,
) -> Result<Value, FunctionCallError> {
    let args: UpdateTask = arguments(invocation)?;
    let mut state = runtime.delegation.state.lock().await;
    let mut next = state.clone();
    if args.status.as_deref() == Some("deleted") {
        next.tasks
            .remove(&args.task_id)
            .ok_or_else(|| error(format!("task {} does not exist", args.task_id)))?;
        for task in next.tasks.values_mut() {
            task.blocked_by
                .retain(|dependency| dependency != &args.task_id);
        }
    } else {
        let actor = next
            .members
            .get(&invocation.session.thread_id.to_string())
            .and_then(|member| member.name.clone())
            .unwrap_or_else(|| "main".to_string());
        if args.status.as_deref() == Some("in_progress") {
            let task = next
                .tasks
                .get(&args.task_id)
                .ok_or_else(|| error(format!("task {} does not exist", args.task_id)))?;
            if task
                .blocked_by
                .iter()
                .any(|dependency| next.tasks[dependency].status != TaskStatus::Completed)
            {
                return Err(error(format!(
                    "task {} has incomplete dependencies",
                    task.id
                )));
            }
        }
        let task = next
            .tasks
            .get_mut(&args.task_id)
            .ok_or_else(|| error(format!("task {} does not exist", args.task_id)))?;
        if args.status.as_deref() == Some("in_progress") {
            let owner = args
                .owner
                .as_deref()
                .filter(|owner| !owner.is_empty())
                .unwrap_or(&actor);
            if task.status == TaskStatus::InProgress
                && task
                    .owner
                    .as_deref()
                    .is_some_and(|current| current != owner)
            {
                return Err(error(format!(
                    "task {} is claimed by {}",
                    task.id,
                    task.owner.as_deref().unwrap()
                )));
            }
            task.owner = Some(owner.to_string());
        }
        if let Some(subject) = args.subject {
            task.subject = subject;
        }
        if let Some(description) = args.description {
            task.description = description;
        }
        if let Some(active_form) = args.active_form {
            task.active_form = Some(active_form);
        }
        if let Some(owner) = args.owner
            && args.status.as_deref() != Some("in_progress")
        {
            task.owner = (!owner.is_empty()).then_some(owner);
        }
        if let Some(status) = args.status {
            task.status = serde_json::from_value(json!(status)).map_err(error)?;
        }
        if let Some(metadata) = args.metadata {
            for (key, value) in metadata {
                if value.is_null() {
                    task.metadata.remove(&key);
                } else {
                    task.metadata.insert(key, value);
                }
            }
        }
        for dependency in args.add_blocked_by {
            if !task.blocked_by.contains(&dependency) {
                task.blocked_by.push(dependency);
            }
        }
        for dependent in args.add_blocks {
            let task = next
                .tasks
                .get_mut(&dependent)
                .ok_or_else(|| error(format!("task {dependent} does not exist")))?;
            if !task.blocked_by.contains(&args.task_id) {
                task.blocked_by.push(args.task_id.clone());
            }
        }
        validate_dependencies(&next.tasks)?;
    }
    runtime.delegation.save(&next).await.map_err(error)?;
    let result = json!({"task":next.tasks.get(&args.task_id).map(|task|task_value(&next,task)),"deleted":!next.tasks.contains_key(&args.task_id)});
    *state = next;
    drop(state);
    wake_teammates(&runtime.delegation).await;
    Ok(result)
}

fn task_value(state: &State, task: &Task) -> Value {
    let mut value = json!(task);
    value["blockedBy"] = json!(
        task.blocked_by
            .iter()
            .filter(|dependency| state.tasks[*dependency].status != TaskStatus::Completed)
            .collect::<Vec<_>>()
    );
    value["blocks"] = json!(
        state
            .tasks
            .values()
            .filter(|dependent| dependent.blocked_by.contains(&task.id))
            .map(|dependent| &dependent.id)
            .collect::<Vec<_>>()
    );
    value
}

fn validate_dependencies(tasks: &BTreeMap<String, Task>) -> Result<(), FunctionCallError> {
    fn visit<'a>(
        tasks: &'a BTreeMap<String, Task>,
        id: &'a str,
        visited: &mut BTreeMap<&'a str, bool>,
    ) -> Result<(), FunctionCallError> {
        match visited.get(id) {
            Some(true) => return Ok(()),
            Some(false) => return Err(error(format!("task dependency cycle includes {id}"))),
            None => {}
        }
        let task = tasks
            .get(id)
            .ok_or_else(|| error(format!("dependency task {id} does not exist")))?;
        visited.insert(id, false);
        for dependency in &task.blocked_by {
            visit(tasks, dependency, visited)?;
        }
        visited.insert(id, true);
        Ok(())
    }
    let mut visited = BTreeMap::new();
    for id in tasks.keys() {
        visit(tasks, id, &mut visited)?;
    }
    Ok(())
}

async fn wake_teammates(team: &Arc<Team>) {
    let runtimes = team
        .runtimes
        .lock()
        .await
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for runtime in runtimes {
        let Some(thread) = runtime.thread.upgrade() else {
            continue;
        };
        let owner = thread.session.thread_id;
        let watcher = tokio::spawn(async move {
            let result = tokio::select! {
                _ = runtime.cancellation.cancelled() => return,
                result = on_idle(&runtime) => result,
            };
            if let Err(err) = result {
                if let Err(delivery) = runtime.events.emit("team-error", err.to_string()).await {
                    tracing::error!(%delivery,"team dispatch error delivery failed");
                }
            }
        });
        team.watch(owner, watcher).await;
    }
}

pub(crate) async fn on_idle(runtime: &Arc<Runtime>) -> anyhow::Result<()> {
    let _dispatch = runtime.delegation.dispatch.lock().await;
    let Some(thread) = runtime.thread.upgrade() else {
        return Ok(());
    };
    if !matches!(thread.agent_status().await, AgentStatus::Completed(_)) {
        return Ok(());
    }
    if thread.session.active_turn.lock().await.is_some()
        || thread
            .session
            .input_queue
            .has_trigger_turn_mailbox_items()
            .await
    {
        return Ok(());
    }
    let id = thread.session.thread_id;
    let mut state = runtime.delegation.state.lock().await;
    let Some(member) = state.members.get(&id.to_string()).cloned() else {
        return Ok(());
    };
    if member.shutdown {
        let root = state.root.unwrap();
        drop(state);
        if runtime.delegation.closing.lock().await.insert(id) {
            let control = thread
                .session
                .services
                .local_agent_runtime
                .control(thread.session.session_id());
            let team = Arc::clone(&runtime.delegation);
            let close = tokio::spawn(async move {
                if let Err(err) = control.close_agent(id).await {
                    team.closing.lock().await.remove(&id);
                    tracing::error!(%err,"approved teammate shutdown failed");
                }
            });
            runtime.delegation.watch(root, close).await;
        }
        return Ok(());
    }
    if runtime.tasks.has_pending_work() || runtime.events.has_pending() {
        return Ok(());
    }
    let Some(name) = member.name else {
        return Ok(());
    };
    if state.aliases.get(&name) != Some(&id.to_string()) {
        return Ok(());
    }
    let candidate = state
        .tasks
        .values()
        .filter(|task| {
            task.status != TaskStatus::Completed
                && task.owner.as_ref().is_none_or(|owner| owner == &name)
                && task
                    .blocked_by
                    .iter()
                    .all(|dependency| state.tasks[dependency].status == TaskStatus::Completed)
        })
        .min_by_key(|task| {
            (
                task.status != TaskStatus::InProgress,
                task.id.parse::<u64>().unwrap(),
            )
        })
        .cloned();
    let Some(mut task) = candidate else {
        return Ok(());
    };
    let previous = task.clone();
    task.owner = Some(name.clone());
    task.status = TaskStatus::InProgress;
    state.tasks.insert(task.id.clone(), task.clone());
    runtime.delegation.save(&state).await?;
    // Keep the claim locked until native admission accepts this task.
    let turn = thread.session.new_default_turn().await;
    let result = thread.session.services.agent_control.send(SendRequest {
        caller: state.root.unwrap(), target: AgentTarget::Id(id),
        resume_config: build_agent_resume_config(&turn).map_err(anyhow::Error::msg)?,
        input: AgentInput::Message {
            message: AgentMessage::Plaintext(format!("You claimed shared task #{}: {}\n\n{}\n\nComplete the work, then call task_update with taskId={} and status=completed. If blocked, update the task and notify main using the native agent messaging tools.",task.id,task.subject,task.description,task.id)),
            mode: MessageDeliveryMode::TriggerTurn,
        },
        start_options: TurnStartOptions::default(),
    }).await;
    if let Err(err) = result {
        state.tasks.insert(previous.id.clone(), previous);
        runtime.delegation.save(&state).await?;
        return Err(err.into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct ShutdownArgs {
    target: Option<String>,
    request_id: Option<String>,
    approve: Option<bool>,
    reason: Option<String>,
}

async fn shutdown(
    invocation: &ToolInvocation,
    runtime: &Arc<Runtime>,
) -> Result<Value, FunctionCallError> {
    let args: ShutdownArgs = arguments(invocation)?;
    if let Some(request_id) = args.request_id {
        let mut state = runtime.delegation.state.lock().await;
        let request = state
            .shutdown_requests
            .get(&request_id)
            .ok_or_else(|| error("shutdown request does not exist"))?
            .clone();
        if request.recipient != invocation.session.thread_id {
            return Err(error("this shutdown request belongs to another teammate"));
        }
        let approve = args
            .approve
            .ok_or_else(|| error("approve is required for a shutdown response"))?;
        state.shutdown_requests.remove(&request_id);
        if approve {
            state
                .members
                .get_mut(&request.recipient.to_string())
                .unwrap()
                .shutdown = true;
        }
        runtime.delegation.save(&state).await.map_err(error)?;
        drop(state);
        let requester = runtime
            .delegation
            .runtimes
            .lock()
            .await
            .get(&request.requester.to_string())
            .and_then(Weak::upgrade);
        if let Some(requester) = requester {
            requester.events.emit("teammate-shutdown",json!({"request_id":request_id,"approved":approve,"agent_id":request.recipient,"reason":args.reason}).to_string()).await.map_err(error)?;
        }
        Ok(json!({"request_id":request_id,"approved":approve}))
    } else {
        let target = args
            .target
            .ok_or_else(|| error("target is required for a shutdown request"))?;
        let recipient = member(&runtime.delegation, &target).await?;
        let request_id = Uuid::new_v4().to_string();
        let request = ShutdownRequest {
            requester: invocation.session.thread_id,
            recipient: recipient.id,
            reason: args.reason,
        };
        let mut state = runtime.delegation.state.lock().await;
        state
            .shutdown_requests
            .insert(request_id.clone(), request.clone());
        runtime.delegation.save(&state).await.map_err(error)?;
        drop(state);
        send_native(invocation,recipient.id,format!("Shutdown requested. Reason: {}. Finish or explain why you need to continue, then call team_shutdown with request_id={} and approve=true or false.",request.reason.as_deref().unwrap_or("work is complete"),request_id),true).await?;
        Ok(json!({"request_id":request_id,"target":target}))
    }
}
