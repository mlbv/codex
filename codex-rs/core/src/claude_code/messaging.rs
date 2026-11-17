//! Local Claude Code peer messaging, with one inbox process per Codex conversation.

#[cfg(test)]
#[path = "messaging_tests.rs"]
mod tests;

use super::events::EventSink;
use crate::CodexThread;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_tools::ResponsesApiTool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::ChildStdin;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_MESSAGE_CHARS: usize = 1024 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_FEATURE: &str = "notify_idle";
const IDLE_SUBSCRIPTION_LIMIT: usize = 32;
const IDLE_SUBSCRIPTION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const IDLE_DEBOUNCE: Duration = Duration::from_millis(750);

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    ListAgents,
    SendMessage,
}

pub(crate) fn specs(claude_delegation: bool) -> Vec<(Tool, ResponsesApiTool)> {
    let mut specs = vec![
        (
            Tool::ListAgents,
            super::tool(
                "list_agents",
                "List live Claude Code and Codex peer sessions on this machine, including their names, working directories, status, and reply addresses. Use an address from this listing to distinguish sessions with the same name.",
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            ),
        ),
        (
            Tool::SendMessage,
            super::tool(
                "send_message",
                "Send a plain-text message to a live Claude Code or Codex peer on this machine. Address it by its listed name, session ID, or uds: address. Incoming replies arrive automatically, including while idle. To hear when a session on this machine finishes what it is doing, pass notify_when_idle: true from the main conversation: exactly one automated notice arrives when it next goes idle or exits. Omit message for a pure subscription that costs that session nothing; include one to deliver it now and subscribe. Never poll list_agents in a loop or send are-you-done messages instead.",
                json!({
                    "type":"object",
                    "properties":{
                        "to":{"type":"string","description":"Recipient name, session ID, or exact uds: address from list_agents."},
                        "message":{"type":"string","minLength":1,"description":"Message text. Omit for a pure idle subscription. The first line is the recipient's visible preview."},
                        "notify_when_idle":{"type":"boolean","description":"Ask a session on this machine to send one notice when it next goes idle (finishes its turn with nothing queued) or exits. Main conversation only, opt-in, one-shot, no polling."}
                    },
                    "required":["to"],
                    "additionalProperties":false
                }),
            ),
        ),
    ];
    if claude_delegation {
        for (kind, spec) in &mut specs {
            match kind {
                Tool::ListAgents => {
                    spec.description.push_str(" Also lists agents in the current native team under native_agents, including their IDs, canonical task paths, current teammate names, and statuses.");
                }
                Tool::SendMessage => {
                    spec.description.push_str(" Native teammate names, agent IDs, and canonical or relative task paths route directly to this team, waking an idle recipient. Use main to wake the main conversation, or * to broadcast to other active named teammates. Native agents report completion automatically; notify_when_idle applies only to peer sessions.");
                    let properties = spec.parameters.properties.as_mut().unwrap();
                    properties.get_mut("to").unwrap().description = Some("Native teammate name, agent ID, task path, main, or *; otherwise a peer name, session ID, or exact uds: address from list_agents.".into());
                }
            }
        }
    }
    specs
}

#[derive(Deserialize)]
struct SendArguments {
    to: String,
    message: Option<String>,
    #[serde(default)]
    notify_when_idle: bool,
}

pub(crate) async fn call(
    tool: Tool,
    invocation: ToolInvocation,
) -> Result<Value, FunctionCallError> {
    let (method, arguments) = match tool {
        Tool::ListAgents => ("list", json!({})),
        Tool::SendMessage => {
            let arguments: SendArguments = super::arguments(&invocation)?;
            if arguments.notify_when_idle
                && matches!(&invocation.turn.session_source, SessionSource::SubAgent(_))
            {
                return Err(super::error(
                    "notify_when_idle is available from the main conversation only",
                ));
            }
            if arguments.notify_when_idle
                && super::delegation::native_message_targets(
                    &invocation.session,
                    &invocation.turn.session_source,
                    &arguments.to,
                )
                .await
                .is_some()
            {
                return Err(super::error(
                    "native agents report completion automatically; notify_when_idle applies only to peer sessions",
                ));
            }
            if let Some(message) = &arguments.message
                && let Some(result) = super::delegation::try_send_native_message(
                    &invocation,
                    &arguments.to,
                    message.clone(),
                )
                .await?
            {
                return Ok(result);
            }
            (
                "send",
                json!({"to":arguments.to,"message":arguments.message,"notify_when_idle":arguments.notify_when_idle}),
            )
        }
    };
    let runtime = super::runtime(&invocation);
    let messaging = runtime.messaging.as_ref().unwrap();
    messaging
        .configure(&invocation.turn.config)
        .await
        .map_err(super::error)?;
    let mut result = tokio::select! {
        result = messaging.request(method, arguments) => result.map_err(super::error)?,
        _ = invocation.cancellation_token.cancelled() => return Err(super::error("Claude Code messaging call cancelled")),
    };
    if matches!(tool, Tool::ListAgents)
        && invocation
            .session
            .enabled(codex_features::Feature::ClaudeDelegation)
    {
        let agents = super::delegation::list_native_agents(&invocation).await?;
        result
            .as_object_mut()
            .unwrap()
            .insert("native_agents".to_string(), json!(agents));
    }
    Ok(result)
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub(crate) struct Messaging {
    pub(crate) address: String,
    input: Arc<Mutex<Option<ChildStdin>>>,
    pending: Pending,
    next_id: AtomicU64,
    cancellation: CancellationToken,
    completion: watch::Receiver<Option<Result<(), String>>>,
    jobs: Mutex<Vec<JoinHandle<()>>>,
}

impl Messaging {
    pub(crate) async fn configure(&self, config: &Config) -> Result<()> {
        let permission_class = if config.permissions.approval_policy.value()
            == AskForApproval::Never
            && matches!(
                config.legacy_sandbox_policy(),
                SandboxPolicy::DangerFullAccess
            ) {
            "bypass"
        } else {
            "prompting"
        };
        self.request(
            "configure",
            json!({
                "cwd":config.cwd.as_path(),
                "trusted":config.active_project.is_trusted(),
                "permission_class":permission_class,
            }),
        )
        .await?;
        Ok(())
    }

    async fn request(&self, method: &str, arguments: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let request = json!({"id":id,"method":method,"arguments":arguments});
        let written = async {
            let mut input = self.input.lock().await;
            write_line(
                input.as_mut().context("Claude Code bridge has stopped")?,
                &request,
            )
            .await
        }
        .await;
        if let Err(error) = written {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        tokio::select! {
            response = receiver => response.context("Claude Code bridge disconnected")?.map_err(|error| anyhow!(error)),
            _ = self.cancellation.cancelled() => bail!("Claude Code bridge stopped"),
        }
    }

    pub(crate) async fn set_status(&self, status: &str) -> Result<()> {
        self.request("status", json!({"status":status})).await?;
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.cancellation.cancel();
        let mut completion = self.completion.clone();
        let result = completion.wait_for(Option::is_some).await?.clone().unwrap();
        let jobs = std::mem::take(&mut *self.jobs.lock().await);
        for job in jobs {
            job.await.context("Claude Code messaging job failed")?;
        }
        result.map_err(|error| anyhow!(error))
    }
}

impl Drop for Messaging {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// The hidden helper receives `<thread-id> <cwd> <name>` and exchanges JSON lines over stdio.
pub(crate) async fn start(
    thread: Arc<CodexThread>,
    events: EventSink,
    cancellation: CancellationToken,
) -> Result<Messaging> {
    let config = thread.config().await;
    let thread_id = thread.session.thread_id().to_string();
    let logs = config.codex_home.join("log");
    tokio::fs::create_dir_all(&logs).await?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join(format!("claude-code-bridge-{thread_id}.log")))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("claude-code-bridge")
        .arg(&thread_id)
        .arg(config.cwd.as_path())
        .arg(format!("codex-{thread_id}"))
        .current_dir(config.cwd.as_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .context("launch Claude Code messaging helper")?;
    let input = Arc::new(Mutex::new(Some(child.stdin.take().unwrap())));
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    #[derive(Deserialize)]
    struct Ready {
        address: String,
    }
    let ready: Ready = serde_json::from_str(
        &output
            .next_line()
            .await?
            .context("Claude Code messaging helper exited before publishing its inbox")?,
    )?;
    let pending: Pending = Arc::default();
    let cancellation = cancellation.child_token();
    let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<(&'static str, String)>();
    let forwarder_cancellation = cancellation.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = forwarder_cancellation.cancelled() => return,
                event = event_receiver.recv() => event,
            };
            let Some((kind, text)) = event else {
                return;
            };
            let result = tokio::select! {
                biased;
                _ = forwarder_cancellation.cancelled() => return,
                result = events.emit(kind, text) => result,
            };
            if let Err(error) = result {
                tracing::error!(%error, "Claude Code peer event delivery failed");
                forwarder_cancellation.cancel();
                return;
            }
        }
    });
    let reader_pending = Arc::clone(&pending);
    let reader_cancellation = cancellation.clone();
    let reader_thread = Arc::downgrade(&thread);
    let reader = tokio::spawn(async move {
        let result: Result<()> = async {
            loop {
                let line = tokio::select! {
                    line = output.next_line() => line?.context("Claude Code messaging helper closed its output")?,
                    _ = reader_cancellation.cancelled() => return Ok(()),
                };
                let frame: Value = serde_json::from_str(&line)?;
                if let Some(event) = frame.get("event") {
                    if event["type"] == "hold_notice" {
                        if let Some(thread) = reader_thread.upgrade() {
                            super::report_error(&thread.session, event["text"].as_str().unwrap()).await;
                        }
                        continue;
                    }
                    let (kind, text) = match event["type"].as_str() {
                        Some("user") => {
                            let mut text = "Claude Code peer message".to_owned();
                            if let Some(name) = event["name"].as_str() {
                                text.push_str(&format!(" from {name}"));
                            }
                            if let Some(from) = event["from"].as_str() {
                                text.push_str(&format!("\nSender/reply address: `{from}`\nReply with send_message using this exact address as `to`."));
                            }
                            text.push_str("\n\n");
                            text.push_str(event["message"]["content"].as_str().unwrap());
                            ("claude_code_message", text)
                        }
                        Some("idle_notice") => (
                            "claude_code_idle",
                            event["text"].as_str().unwrap().to_owned(),
                        ),
                        Some("control") => (
                            "claude_code_delivery",
                            format!("Claude Code delivery notice: {}", serde_json::to_string(event)?),
                        ),
                        _ => bail!("unrecognized Claude Code inbox frame: {event}"),
                    };
                    event_sender.send((kind, text))?;
                } else {
                    let id = frame["id"].as_u64().unwrap();
                    let response = match frame.get("error") {
                        Some(error) => Err(error.as_str().unwrap().to_owned()),
                        None => Ok(frame["result"].clone()),
                    };
                    let sender = reader_pending.lock().await.remove(&id).unwrap();
                    let _ = sender.send(response);
                }
            }
        }
        .await;
        if let Err(error) = &result {
            tracing::error!(%error, "Claude Code messaging bridge stopped");
        }
        let reason = result.err().map_or_else(
            || "Claude Code messaging bridge stopped".to_owned(),
            |error| error.to_string(),
        );
        for (_, sender) in reader_pending.lock().await.drain() {
            let _ = sender.send(Err(reason.clone()));
        }
        reader_cancellation.cancel();
    });
    let process_input = Arc::clone(&input);
    let process_cancellation = cancellation.clone();
    let (completion_sender, completion) = watch::channel(None);
    let process = tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = process_cancellation.cancelled() => {
                process_input.lock().await.take();
                child.wait().await
            }
        };
        let result = match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("Claude Code messaging helper exited with {status}")),
            Err(error) => Err(error.to_string()),
        };
        let _ = completion_sender.send(Some(result));
        process_cancellation.cancel();
    });
    let messaging = Messaging {
        address: ready.address,
        input,
        pending,
        next_id: AtomicU64::new(1),
        cancellation,
        completion,
        jobs: Mutex::new(vec![reader, forwarder, process]),
    };
    messaging.configure(&config).await?;
    Ok(messaging)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Peer {
    pid: u32,
    session_id: String,
    name: Option<String>,
    cwd: Option<String>,
    messaging_socket_path: Option<String>,
    proc_start: Option<String>,
    proc_start_ft: Option<String>,
    status: Option<String>,
    #[serde(default)]
    peer_features: Vec<String>,
}

impl Peer {
    fn socket(&self) -> &str {
        self.messaging_socket_path.as_deref().unwrap()
    }

    fn address(&self) -> String {
        format!("uds:{}", self.socket())
    }

    fn matches(&self, recipient: &str) -> bool {
        self.session_id == recipient
            || self.name.as_deref() == Some(recipient)
            || self.address() == recipient
    }
}

#[derive(Deserialize)]
struct BridgeRequest {
    id: u64,
    method: String,
    arguments: Value,
}

struct Bridge {
    registry: PathBuf,
    record_path: PathBuf,
    key_path: PathBuf,
    record: Value,
    address: String,
    token: String,
    cancellation: CancellationToken,
    subscribers: Vec<IdleSubscriber>,
    outstanding: Vec<IdleSubscription>,
    idle_deadline: Option<Instant>,
    finished_at: Option<i64>,
    permission_class: Option<String>,
    settings_paths: Vec<PathBuf>,
    settings_subscriber: Option<FileWatcherSubscriber>,
    settings_registration: Option<WatchRegistration>,
    explicit_policy: Option<InboundPolicy>,
    held: Vec<IncomingFrame>,
    pending_events: Vec<Value>,
}

struct IncomingFrame {
    message: Value,
    self_sent: bool,
}

#[derive(Clone)]
struct ProcessIdentity {
    pid: u32,
    started: String,
}

struct PeerKey {
    token: String,
    owner: Option<ProcessIdentity>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InboundPolicy {
    Accept,
    Hold,
    Refuse,
}

#[derive(Clone)]
struct LocalTarget {
    socket: String,
    peer: Option<Peer>,
}

impl LocalTarget {
    fn registered(peer: Peer) -> Self {
        Self {
            socket: peer.socket().to_owned(),
            peer: Some(peer),
        }
    }

    fn address(&self) -> String {
        format!("uds:{}", self.socket)
    }

    fn label(&self) -> String {
        self.peer
            .as_ref()
            .map(peer_label)
            .unwrap_or_else(|| idle_line(&self.address()))
    }
}

struct IdleSubscriber {
    target: LocalTarget,
    orig_msg_id: String,
    requested_at: Instant,
    retried: bool,
}

struct IdleSubscription {
    target: LocalTarget,
    msg_id: String,
    requested_at: Instant,
}

/// Runs before the ordinary CLI starts, so stdout contains only bridge protocol frames.
pub async fn run_bridge(arguments: Vec<String>) -> Result<()> {
    let [thread_id, cwd, name]: [String; 3] = arguments
        .try_into()
        .map_err(|_| anyhow!("claude-code-bridge requires <thread-id> <cwd> <name>"))?;
    let registry = claude_config_dir().join("sessions");
    create_private_dir(&registry)?;
    let socket = inbox_path()?;
    let listener = Listener::bind(&socket).await?;
    let token = Uuid::new_v4().simple().to_string();
    let pid = std::process::id();
    let process_start = process_start(pid)
        .await?
        .context("bridge process identity is unavailable")?;
    let identity_field = if cfg!(windows) {
        "procStartFt"
    } else {
        "procStart"
    };
    let key_path = key_path(&registry, pid, &socket);
    let record_path = registry.join(format!("{pid}.json"));
    let key = json!({"peerToken":token,identity_field:process_start});
    write_private_json(&key_path, &key)?;
    let now = chrono::Utc::now().timestamp_millis();
    let record = json!({
        "pid":pid,
        "sessionId":thread_id,
        "cwd":cwd,
        "name":name,
        "nameSource":"user",
        "nameSince":now,
        "startedAt":now,
        "updatedAt":now,
        "statusUpdatedAt":now,
        "status":"idle",
        "kind":"interactive",
        "agent":"Codex",
        "entrypoint":"codex",
        "version":env!("CARGO_PKG_VERSION"),
        "messagingSocketPath":socket,
        "peerProtocol":1,
        "peerFeatures":[IDLE_FEATURE],
        identity_field:process_start
    });
    write_private_json(&record_path, &record)?;
    let cancellation = CancellationToken::new();
    let file_watcher = Arc::new(FileWatcher::new()?);
    let (settings_subscriber, mut settings_changes) = file_watcher.add_subscriber();
    let mut bridge = Bridge {
        registry,
        record_path,
        key_path,
        record,
        address: format!("uds:{socket}"),
        token,
        cancellation,
        subscribers: Vec::new(),
        outstanding: Vec::new(),
        idle_deadline: None,
        finished_at: None,
        permission_class: None,
        settings_paths: Vec::new(),
        settings_subscriber: Some(settings_subscriber),
        settings_registration: None,
        explicit_policy: None,
        held: Vec::new(),
        pending_events: Vec::new(),
    };
    let (sender, mut inbox) = mpsc::unbounded_channel();
    let accept = tokio::spawn(accept_messages(
        listener,
        bridge.token.clone(),
        sender,
        bridge.cancellation.clone(),
    ));
    let result: Result<()> = async {
        let mut output = tokio::io::stdout();
        write_line(&mut output, &json!({"address":bridge.address})).await?;
        let mut input = BufReader::new(tokio::io::stdin()).lines();
        loop {
            let deadline = bridge.next_deadline();
            let timer = async move {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(timer);
            tokio::select! {
                line = input.next_line() => {
                    let Some(line) = line? else { break; };
                    let request: BridgeRequest = serde_json::from_str(&line)?;
                    let response = match bridge.handle(&request).await {
                        Ok(result) => json!({"id":request.id,"result":result}),
                        Err(error) => json!({"id":request.id,"error":error.to_string()}),
                    };
                    write_line(&mut output, &response).await?;
                    for event in std::mem::take(&mut bridge.pending_events) {
                        write_line(&mut output, &json!({"event":event})).await?;
                    }
                }
                message = inbox.recv() => {
                    let message = message.context("Claude Code inbox closed")?;
                    match bridge.receive_frame(message).await {
                        Ok(Some(event)) => write_line(&mut output, &json!({"event":event})).await?,
                        Ok(None) => {},
                        Err(error) => tracing::warn!(%error, "Claude Code peer control frame rejected"),
                    }
                    for event in std::mem::take(&mut bridge.pending_events) {
                        write_line(&mut output, &json!({"event":event})).await?;
                    }
                }
                _ = &mut timer => {
                    for event in bridge.tick().await? {
                        write_line(&mut output, &json!({"event":event})).await?;
                    }
                }
                change = settings_changes.recv() => {
                    change.context("Claude Code settings watcher stopped")?;
                    bridge.refresh_inbound_policy().await?;
                    for event in std::mem::take(&mut bridge.pending_events) {
                        write_line(&mut output, &json!({"event":event})).await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    bridge.cancellation.cancel();
    bridge.expire_held().await;
    bridge
        .flush_subscribers(if bridge.record["status"] == "idle" {
            "idle"
        } else {
            "exited"
        })
        .await;
    let accept_result = accept.await?;
    std::fs::remove_file(&bridge.record_path)?;
    std::fs::remove_file(&bridge.key_path)?;
    #[cfg(unix)]
    std::fs::remove_file(socket)?;
    result?;
    accept_result
}

impl Bridge {
    async fn handle(&mut self, request: &BridgeRequest) -> Result<Value> {
        match request.method.as_str() {
            "configure" => {
                let cwd = Path::new(
                    request.arguments["cwd"]
                        .as_str()
                        .context("missing peer settings directory")?,
                );
                self.permission_class = Some(
                    request.arguments["permission_class"]
                        .as_str()
                        .context("missing permission class")?
                        .to_owned(),
                );
                self.settings_paths = vec![
                    super::memory::managed_settings(),
                    claude_config_dir().join("settings.json"),
                ];
                if request.arguments["trusted"] == true {
                    for filename in ["settings.local.json", "settings.json"] {
                        self.settings_paths.extend(
                            cwd.ancestors()
                                .map(|directory| directory.join(".claude").join(filename)),
                        );
                    }
                }
                if let Some(subscriber) = &self.settings_subscriber {
                    let registration = subscriber.register_paths(
                        self.settings_paths
                            .iter()
                            .map(|path| WatchPath {
                                path: path.clone(),
                                recursive: false,
                            })
                            .collect(),
                    );
                    ensure!(
                        registration.registration_succeeded(),
                        "could not watch Claude Code inbound settings"
                    );
                    self.settings_registration = Some(registration);
                }
                self.refresh_inbound_policy().await?;
                Ok(json!({}))
            }
            "list" => {
                let peers = self.peers().await?;
                let agents = peers
                    .iter()
                    .map(|peer| {
                        json!({
                            "name":peer.name,
                            "session_id":peer.session_id,
                            "address":peer.address(),
                            "cwd":peer.cwd,
                            "status":peer.status
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(
                    json!({"self":{"name":self.record["name"],"address":self.address},"agents":agents}),
                )
            }
            "send" => {
                let arguments: SendArguments = serde_json::from_value(request.arguments.clone())?;
                let plain_message = arguments.message.as_deref().unwrap_or_default();
                ensure!(
                    !plain_message.trim().is_empty() || arguments.notify_when_idle,
                    "message must not be empty unless notify_when_idle is true"
                );
                let peers = self.peers().await?;
                let matches = peers
                    .iter()
                    .filter(|peer| peer.matches(&arguments.to))
                    .collect::<Vec<_>>();
                let target = match matches.as_slice() {
                    [peer] => LocalTarget::registered((*peer).clone()),
                    [] if arguments.to.starts_with("uds:") => {
                        self.reply_target(&arguments.to, peers).await?
                    }
                    [] => bail!("no live peer named or addressed '{}'", arguments.to),
                    _ => bail!(
                        "multiple peers match '{}'; use an exact uds: address from list_agents",
                        arguments.to
                    ),
                };
                ensure!(
                    canonical_socket(&target.socket)
                        != canonical_socket(self.record["messagingSocketPath"].as_str().unwrap()),
                    "the recipient is this session"
                );
                if arguments.notify_when_idle {
                    ensure!(
                        self.inbound_policy(None) != InboundPolicy::Refuse,
                        "crossSessionInbound=refuse: this session cannot receive an idle notice"
                    );
                    ensure!(
                        target.peer.as_ref().is_none_or(|peer| peer
                            .peer_features
                            .iter()
                            .any(|feature| feature == IDLE_FEATURE)),
                        "the recipient does not support idle notices"
                    );
                    ensure!(self.outstanding.iter().filter(|subscription| subscription.target.address() != target.address()).count() < IDLE_SUBSCRIPTION_LIMIT,
                        "this session already holds 32 pending idle subscriptions");
                }
                let mut result = json!({"success":true,"to":target.address()});
                if !plain_message.trim().is_empty() {
                    let message_id = Uuid::new_v4().to_string();
                    let body = plain_message
                        .replace("<cross-session-message", "<\\cross-session-message")
                        .replace("</cross-session-message", "<\\/cross-session-message");
                    let content = format!(
                        "<cross-session-message from=\"{}\" from-session=\"{}\" from-name=\"{}\">\n{}\n</cross-session-message>",
                        self.address,
                        self.record["sessionId"].as_str().unwrap(),
                        self.record["name"].as_str().unwrap(),
                        body,
                    );
                    self.send_frame(
                        &target,
                        &json!({
                            "msgV":1,"msg_id":message_id,"type":"user",
                            "message":{"role":"user","content":content},
                            "priority":"next","from":self.address,"from_mode":self.permission_class
                        }),
                    )
                    .await?;
                    result["msg_id"] = json!(message_id);
                    result["message"] = json!(
                        "Message sent to the peer's inbox. Replies and delivery notices arrive automatically."
                    );
                }
                if arguments.notify_when_idle {
                    let message_id = Uuid::new_v4().to_string();
                    let prior = self
                        .outstanding
                        .iter()
                        .enumerate()
                        .filter(|(_, subscription)| {
                            subscription.target.address() == target.address()
                        })
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    for index in prior
                        .iter()
                        .skip(1)
                        .take(prior.len().saturating_sub(3))
                        .rev()
                    {
                        self.outstanding.remove(*index);
                    }
                    self.outstanding.push(IdleSubscription {
                        target: target.clone(),
                        msg_id: message_id.clone(),
                        requested_at: Instant::now(),
                    });
                    self.send_control(
                        &target,
                        json!({
                            "action":"notify_when_idle","from":self.address,"msg_id":message_id
                        }),
                    )
                    .await?;
                    result["idle_subscription_id"] = json!(message_id);
                    result["notify_when_idle"] = json!(true);
                    result["peer_known_capable"] = json!(target.peer.is_some());
                    result["message"] = json!(
                        "Subscribed: one notice will arrive here when the recipient is next idle or exits. Carry on; do not poll or wait for it."
                    );
                }
                Ok(result)
            }
            "status" => {
                self.refresh_inbound_policy().await?;
                let was_idle = self.record["status"] == "idle";
                let idle = request.arguments["status"] == "idle";
                self.record["status"] = request.arguments["status"].clone();
                if idle && !was_idle {
                    self.finished_at = Some(chrono::Utc::now().timestamp_millis());
                }
                self.idle_deadline =
                    (idle && !self.subscribers.is_empty()).then(|| Instant::now() + IDLE_DEBOUNCE);
                self.record["statusUpdatedAt"] = json!(chrono::Utc::now().timestamp_millis());
                self.record["updatedAt"] = self.record["statusUpdatedAt"].clone();
                write_private_json(&self.record_path, &self.record)?;
                Ok(json!({}))
            }
            method => bail!("unknown Claude Code bridge method '{method}'"),
        }
    }

    fn inbound_policy(&self, sender_class: Option<&str>) -> InboundPolicy {
        self.inbound_policy_for(sender_class, false)
    }

    fn inbound_policy_for(&self, sender_class: Option<&str>, self_sent: bool) -> InboundPolicy {
        if let Some(policy) = self.explicit_policy {
            return policy;
        }
        if self_sent {
            return InboundPolicy::Accept;
        }
        match (self.permission_class.as_deref(), sender_class) {
            (Some("prompting"), None | Some("prompting")) => InboundPolicy::Accept,
            (Some("bypass"), Some("bypass")) => InboundPolicy::Accept,
            _ => InboundPolicy::Hold,
        }
    }

    async fn refresh_inbound_policy(&mut self) -> Result<()> {
        let paths = self.settings_paths.clone();
        self.explicit_policy = tokio::task::spawn_blocking(move || {
            let mut base = None;
            let mut repository = None;
            for (index, path) in paths.iter().enumerate() {
                let Some(content) = super::memory::read_optional(path)
                    .map_err(|error| anyhow!(error.to_string()))?
                else {
                    continue;
                };
                let settings: Value = match serde_json::from_str(&content) {
                    Ok(settings) => settings,
                    Err(_) => {
                        repository = Some(InboundPolicy::Hold);
                        continue;
                    }
                };
                let Some(value) = settings.get("crossSessionInbound") else {
                    continue;
                };
                let policy = match value.as_str() {
                    Some("accept") => InboundPolicy::Accept,
                    Some("refuse") => InboundPolicy::Refuse,
                    _ => InboundPolicy::Hold,
                };
                if index < 2 {
                    if base.is_none() {
                        base = Some(policy);
                    }
                } else {
                    repository = Some(
                        repository.map_or(policy, |previous: InboundPolicy| previous.max(policy)),
                    );
                }
            }
            Ok::<_, anyhow::Error>(match (base, repository) {
                (Some(base), Some(repository)) => Some(base.max(repository)),
                (Some(base), None) => Some(base),
                (None, Some(repository)) if repository != InboundPolicy::Accept => Some(repository),
                _ => None,
            })
        })
        .await??;
        let held = std::mem::take(&mut self.held);
        for frame in held {
            match self.inbound_policy_for(frame.message["from_mode"].as_str(), frame.self_sent) {
                InboundPolicy::Accept => {
                    self.peer_receipt(&frame.message, "delivered").await;
                    self.pending_events.push(frame.message);
                }
                InboundPolicy::Hold => self.held.push(frame),
                InboundPolicy::Refuse => self.peer_receipt(&frame.message, "refused").await,
            }
        }
        Ok(())
    }

    async fn peer_receipt(&self, message: &Value, status: &str) {
        if message["type"] != "user" {
            return;
        }
        let receipt = async {
            let from = message["from"]
                .as_str()
                .context("peer message has no reply address")?;
            let target = self.reply_target(from, self.peers().await?).await?;
            self.send_control(
                &target,
                json!({
                    "action":"peer_message_status", "status":status,
                    "orig_msg_id":message["msg_id"], "from":self.address,
                }),
            )
            .await
        }
        .await;
        if let Err(error) = receipt {
            tracing::warn!(%error, "Claude Code peer receipt delivery failed");
        }
    }

    async fn hold_frame(&mut self, frame: IncomingFrame) {
        if self.held.len() == 100 {
            let expired = self.held.remove(0);
            self.peer_receipt(&expired.message, "expired").await;
        }
        self.held.push(frame);
    }

    async fn expire_held(&mut self) {
        let held = std::mem::take(&mut self.held);
        let bridge = &*self;
        let receipts = futures::future::join_all(
            held.iter()
                .map(|message| bridge.peer_receipt(&message.message, "expired")),
        );
        let _ = tokio::time::timeout(IDLE_DEBOUNCE, receipts).await;
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.idle_deadline
            .into_iter()
            .chain(
                self.outstanding
                    .iter()
                    .map(|subscription| subscription.requested_at + IDLE_SUBSCRIPTION_LIFETIME),
            )
            .min()
    }

    async fn reply_target(&self, address: &str, peers: Vec<Peer>) -> Result<LocalTarget> {
        let socket = address
            .strip_prefix("uds:")
            .context("idle reply target must be a uds: address")?;
        ensure!(
            local_reply_socket(socket, self.record["messagingSocketPath"].as_str().unwrap()),
            "reply target is outside this session's local socket namespace"
        );
        let canonical = canonical_socket(socket);
        let peer = peers
            .into_iter()
            .find(|peer| canonical_socket(peer.socket()) == canonical);
        Ok(LocalTarget {
            socket: socket.to_owned(),
            peer,
        })
    }

    async fn peer_token(&self, target: &LocalTarget) -> Result<Option<PeerKey>> {
        let suffix = format!(".{}.key", socket_digest(&target.socket));
        let mut entries = tokio::fs::read_dir(&self.registry).await?;
        let mut selected = None;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(pid) = name
                .strip_suffix(&suffix)
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            let contents = match tokio::fs::read(entry.path()).await {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let key: Value = serde_json::from_slice(&contents)?;
            let token = key["peerToken"].as_str().context("peer key has no token")?;
            let expected = key[if cfg!(windows) {
                "procStartFt"
            } else {
                "procStart"
            }]
            .as_str();
            let actual = process_start(pid).await?;
            let rank = match (actual.as_deref(), expected) {
                (Some(actual), Some(expected)) if actual == expected => 2,
                (Some(_), None) => 1,
                _ => 0,
            };
            if selected
                .as_ref()
                .is_none_or(|(selected_rank, _)| rank > *selected_rank)
            {
                selected = Some((
                    rank,
                    PeerKey {
                        token: token.to_owned(),
                        owner: actual.map(|started| ProcessIdentity { pid, started }),
                    },
                ));
            }
        }
        match selected {
            Some((rank, token)) => {
                ensure!(
                    !cfg!(windows) || rank != 0,
                    "the peer inbox key has no live owner"
                );
                Ok(Some(token))
            }
            None => {
                ensure!(
                    !cfg!(windows) || target.peer.is_some(),
                    "the peer pipe has neither an inbox key nor a live registered owner"
                );
                Ok(None)
            }
        }
    }

    async fn send_frame(&self, target: &LocalTarget, message: &Value) -> Result<()> {
        if let Some(peer) = &target.peer {
            let expected_start = if cfg!(windows) {
                &peer.proc_start_ft
            } else {
                &peer.proc_start
            };
            ensure!(
                process_start(peer.pid).await?.as_ref() == expected_start.as_ref(),
                "the peer process has exited"
            );
        }
        #[cfg(unix)]
        ensure!(
            !tokio::fs::symlink_metadata(&target.socket)
                .await?
                .is_symlink(),
            "reply target is a symlink"
        );
        let key = self.peer_token(target).await?;
        let auth = key
            .as_ref()
            .map(|key| json!({"type":"auth","token":key.token}));
        let size = auth
            .as_ref()
            .map(|auth| serde_json::to_string(auth).unwrap().encode_utf16().count() + 1)
            .unwrap_or(0)
            + serde_json::to_string(message)?.encode_utf16().count()
            + 1;
        ensure!(
            size <= MAX_MESSAGE_CHARS,
            "message exceeds Claude Code's 1 MiB inbox frame limit"
        );
        tokio::time::timeout(SEND_TIMEOUT, async {
            let mut connection = connect(&target.socket).await?;
            let actual = connected_server_identity(&connection).await?;
            let expected = target
                .peer
                .as_ref()
                .map(|peer| ProcessIdentity {
                    pid: peer.pid,
                    started: if cfg!(windows) {
                        &peer.proc_start_ft
                    } else {
                        &peer.proc_start
                    }
                    .as_ref()
                    .unwrap()
                    .clone(),
                })
                .or_else(|| key.as_ref().and_then(|key| key.owner.clone()));
            if let Some(expected) = expected {
                let actual = actual.context("connected peer identity is unavailable")?;
                ensure!(
                    actual.pid == expected.pid && actual.started == expected.started,
                    "connected peer is not the process owning the inbox"
                );
            }
            if let Some(auth) = &auth {
                write_line(&mut connection, auth).await?;
            }
            write_line(&mut connection, message).await?;
            connection.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("timed out sending to Claude Code peer")??;
        Ok(())
    }

    async fn send_control(&self, target: &LocalTarget, mut control: Value) -> Result<()> {
        control["type"] = json!("control");
        control["msgV"] = json!(1);
        if let Some(permission_class) = &self.permission_class {
            control["from_mode"] = json!(permission_class);
        }
        if control.get("msg_id").is_none() {
            control["msg_id"] = json!(Uuid::new_v4().to_string());
        }
        self.send_frame(target, &control).await
    }

    #[cfg(test)]
    async fn receive(&mut self, message: Value) -> Result<Option<Value>> {
        self.receive_frame(IncomingFrame {
            message,
            self_sent: false,
        })
        .await
    }

    async fn receive_frame(&mut self, frame: IncomingFrame) -> Result<Option<Value>> {
        let IncomingFrame { message, self_sent } = frame;

        if message["type"] == "user" {
            self.refresh_inbound_policy().await?;
            return match self.inbound_policy_for(message["from_mode"].as_str(), self_sent) {
                InboundPolicy::Accept => {
                    self.peer_receipt(&message, "delivered").await;
                    Ok(Some(message))
                }
                InboundPolicy::Refuse => {
                    self.peer_receipt(&message, "refused").await;
                    Ok(None)
                }
                InboundPolicy::Hold => {
                    let reason = match self.explicit_policy {
                        Some(InboundPolicy::Hold) => "crossSessionInbound=hold",
                        _ if self.permission_class.is_none() => {
                            "permission settings are still being configured"
                        }
                        _ if message["from_mode"].is_null() => {
                            "the sender has no permission class assertion and this session bypasses prompts"
                        }
                        _ => "the sender's permission class differs from this session's",
                    };
                    self.peer_receipt(&message, "held").await;
                    let text = format!(
                        "Peer message held for user review ({reason}). To release held messages, set crossSessionInbound to accept in the applicable Claude settings. Settings changes are applied automatically.\n\n{}",
                        message["message"]["content"].as_str().unwrap_or_default(),
                    );
                    self.hold_frame(IncomingFrame { message, self_sent }).await;
                    Ok(Some(json!({"type":"hold_notice","text":text})))
                }
            };
        }
        if message["type"] != "control" {
            return Ok(Some(message));
        }
        match message["action"].as_str() {
            Some("notify_when_idle") => {
                self.refresh_inbound_policy().await?;
                if self.inbound_policy(None) == InboundPolicy::Refuse {
                    return Ok(None);
                }
                let from = message["from"]
                    .as_str()
                    .context("idle subscription is missing its reply address")?;
                let orig_msg_id = message["msg_id"]
                    .as_str()
                    .context("idle subscription is missing its message ID")?;
                let peers = self.peers().await?;
                let target = self.reply_target(from, peers).await?;
                ensure!(
                    canonical_socket(&target.socket)
                        != canonical_socket(self.record["messagingSocketPath"].as_str().unwrap()),
                    "idle reply target is this session"
                );
                self.subscribers.retain(|subscription| {
                    subscription.requested_at.elapsed() < IDLE_SUBSCRIPTION_LIFETIME
                });
                if let Some(subscription) = self.subscribers.iter_mut().find(|subscription| {
                    canonical_socket(&subscription.target.socket)
                        == canonical_socket(&target.socket)
                }) {
                    subscription.orig_msg_id = orig_msg_id.to_owned();
                    subscription.requested_at = Instant::now();
                    subscription.retried = false;
                } else if self.subscribers.len() == IDLE_SUBSCRIPTION_LIMIT {
                    self.send_control(&target, json!({"action":"peer_idle_notice","orig_msg_id":orig_msg_id,"state":"unavailable","from":self.address})).await?;
                } else {
                    self.subscribers.push(IdleSubscriber {
                        target,
                        orig_msg_id: orig_msg_id.to_owned(),
                        requested_at: Instant::now(),
                        retried: false,
                    });
                }
                if self.record["status"] == "idle" && self.idle_deadline.is_none() {
                    self.idle_deadline = Some(Instant::now() + IDLE_DEBOUNCE);
                }
                Ok(None)
            }
            Some("peer_idle_notice") => {
                let Some(index) = self.outstanding.iter().position(|subscription| {
                    message["orig_msg_id"].as_str() == Some(subscription.msg_id.as_str())
                }) else {
                    return Ok(None);
                };
                if message["from"]
                    .as_str()
                    .is_some_and(|from| from != self.outstanding[index].target.address())
                {
                    return Ok(None);
                }
                let subscription = self.outstanding.remove(index);
                if matches!(message["state"].as_str(), Some("idle" | "exited")) {
                    self.outstanding.retain(|outstanding| {
                        outstanding.target.address() != subscription.target.address()
                    });
                }
                let state = match message["state"].as_str() {
                    Some("idle") => "idle",
                    Some("exited") => "exited",
                    _ => "unavailable",
                };
                let mut text = format!(
                    "[Cross-session idle notice] {} {}.",
                    subscription.target.label(),
                    match state {
                        "idle" => "is idle now",
                        "exited" => "has exited before going idle",
                        _ => "is not holding the idle subscription",
                    }
                );
                if state == "idle"
                    && let Some(detail) = message["detail"].as_str()
                {
                    text.push_str(&format!(" Its harness reports: {}.", idle_line(detail)));
                }
                text.push_str(" This is an automated notice from that session's harness. Act on it insofar as the user's earlier request calls for it.");
                match self.inbound_policy_for(message["from_mode"].as_str(), self_sent) {
                    InboundPolicy::Accept => Ok(Some(json!({"type":"idle_notice","text":text}))),
                    InboundPolicy::Hold => {
                        self.hold_frame(IncomingFrame {
                            message: json!({"type":"idle_notice","text":text,"from_mode":message["from_mode"]}),
                            self_sent,
                        })
                        .await;
                        Ok(Some(json!({"type":"hold_notice","text":text})))
                    }
                    InboundPolicy::Refuse => Ok(None),
                }
            }
            Some("peer_message_status") => Ok(Some(json!({
                "type":"hold_notice", "text":format!("Peer delivery status for {}: {}", message["orig_msg_id"], message["status"])
            }))),
            _ => Ok(Some(message)),
        }
    }

    async fn tick(&mut self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        let now = Instant::now();
        let mut expired = HashMap::new();
        self.outstanding.retain(|subscription| {
            if now.duration_since(subscription.requested_at) < IDLE_SUBSCRIPTION_LIFETIME {
                return true;
            }
            expired.insert(subscription.target.address(), subscription.target.clone());
            false
        });
        for (address, target) in expired {
            if self
                .outstanding
                .iter()
                .any(|subscription| subscription.target.address() == address)
            {
                continue;
            }
            events.push(json!({"type":"idle_notice","text":format!("[Cross-session idle notice] No idle signal arrived from {} within 12 hours; the subscription has expired. Continue according to the user's earlier request.", target.label())}));
        }
        if self.idle_deadline.is_some_and(|deadline| deadline <= now) {
            self.idle_deadline = None;
            if self.record["status"] == "idle" {
                self.flush_subscribers("idle").await;
            }
        }
        Ok(events)
    }

    async fn flush_subscribers(&mut self, state: &str) {
        let subscribers = std::mem::take(&mut self.subscribers);
        if self.inbound_policy(None) == InboundPolicy::Refuse {
            return;
        }
        let bridge = &*self;
        let deliveries = futures::future::join_all(subscribers.into_iter().map(|subscription| async move {
            if subscription.requested_at.elapsed() >= IDLE_SUBSCRIPTION_LIFETIME {
                return (subscription, Ok(()));
            }
            let mut notice = json!({"action":"peer_idle_notice","orig_msg_id":subscription.orig_msg_id,"state":state,"from":bridge.address});
            if state == "exited" {
                notice["finished_at"] = json!(chrono::Utc::now().timestamp_millis());
            } else if let Some(finished_at) = bridge.finished_at {
                notice["finished_at"] = json!(finished_at);
            }
            let result = bridge.send_control(&subscription.target, notice).await;
            (subscription, result)
        })).await;
        for (mut subscription, result) in deliveries {
            if let Err(error) = result {
                if state == "idle" && !subscription.retried {
                    subscription.retried = true;
                    self.subscribers.push(subscription);
                    self.idle_deadline = Some(Instant::now() + IDLE_DEBOUNCE);
                } else {
                    tracing::warn!(%error, "Claude Code peer idle notice delivery failed");
                }
            }
        }
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        let mut entries = tokio::fs::read_dir(&self.registry).await?;
        let mut peers = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_str().unwrap();
            let Some(pid) = name
                .strip_suffix(".json")
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            let contents = match tokio::fs::read(entry.path()).await {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let peer: Peer = serde_json::from_slice(&contents)?;
            if peer
                .messaging_socket_path
                .as_deref()
                .is_none_or(str::is_empty)
            {
                continue;
            }
            let expected = if cfg!(windows) {
                peer.proc_start_ft.as_ref()
            } else {
                peer.proc_start.as_ref()
            };
            if process_start(peer.pid).await?.as_ref() == expected && expected.is_some() {
                peers.push(peer);
            }
        }
        Ok(peers)
    }
}

fn peer_label(peer: &Peer) -> String {
    idle_line(peer.name.as_deref().unwrap_or(&peer.session_id))
}

fn idle_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '<' | '>' | '«' | '»' | '"' | '[' | ']')
            {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

async fn write_line<W: AsyncWrite + Unpin>(output: &mut W, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".claude"))
}

fn canonical_socket(socket: &str) -> String {
    #[cfg(windows)]
    return socket.replace('/', "\\").to_ascii_lowercase();
    #[cfg(unix)]
    return socket.to_owned();
}

fn socket_digest(socket: &str) -> String {
    format!("{:x}", Sha256::digest(canonical_socket(socket).as_bytes()))
}

fn key_path(registry: &Path, pid: u32, socket: &str) -> PathBuf {
    registry.join(format!("{pid}.{}.key", socket_digest(socket)))
}

fn local_reply_socket(socket: &str, own_socket: &str) -> bool {
    #[cfg(windows)]
    {
        let socket = canonical_socket(socket);
        let tail = socket
            .strip_prefix("\\\\.\\pipe\\local\\cc-msg-")
            .or_else(|| socket.strip_prefix("\\\\.\\pipe\\cc-msg-"));
        tail.is_some_and(|tail| {
            tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }
    #[cfg(unix)]
    {
        let socket = Path::new(socket);
        socket.is_absolute()
            && socket.parent() == Path::new(own_socket).parent()
            && socket
                .extension()
                .is_some_and(|extension| extension == "sock")
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    super::write_atomic(path, &serde_json::to_vec(value)?)?;
    Ok(())
}

#[cfg(test)]
async fn receive<S: AsyncRead + AsyncWrite + Unpin>(
    connection: S,
    token: &str,
    inbox: &mpsc::UnboundedSender<Value>,
    cancellation: &CancellationToken,
) -> Result<()> {
    read_frames(connection, token, cancellation, |message| {
        inbox.send(message)?;
        Ok(())
    })
    .await
}

async fn read_frames<S, F>(
    connection: S,
    token: &str,
    cancellation: &CancellationToken,
    mut admit: F,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(Value) -> Result<()>,
{
    let mut lines = BufReader::new(connection).lines();
    let first = tokio::select! {
        line = lines.next_line() => line?,
        _ = cancellation.cancelled() => return Ok(()),
    };
    let Some(first) = first else {
        return Ok(());
    };
    let first: Value = serde_json::from_str(&first)?;
    if first["type"] == "auth" {
        ensure!(first["token"] == token, "invalid Claude Code inbox token");
    } else {
        #[cfg(windows)]
        bail!("Claude Code inbox requires an authentication frame");
        #[cfg(unix)]
        admit(first)?;
    }
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line?,
            _ = cancellation.cancelled() => return Ok(()),
        };
        let Some(line) = line else {
            return Ok(());
        };
        ensure!(
            line.encode_utf16().count() <= MAX_MESSAGE_CHARS,
            "Claude Code inbox frame exceeds 1 MiB"
        );
        admit(serde_json::from_str(&line)?)?;
    }
}

async fn accept_messages(
    mut listener: Listener,
    token: String,
    inbox: mpsc::UnboundedSender<IncomingFrame>,
    cancellation: CancellationToken,
) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let connection = connection?;
                let token = token.clone();
                let inbox = inbox.clone();
                let cancellation = cancellation.clone();
                connections.spawn(async move {
                    let result = async {
                        let identity = connected_client_identity(&connection).await?;
                        let self_sent = match identity {
                            Some(identity) => helper_descendant(&identity).await?,
                            None => false,
                        };
                        read_frames(connection, &token, &cancellation, |message| {
                            inbox.send(IncomingFrame { message, self_sent })?;
                            Ok(())
                        }).await
                    }.await;
                    if let Err(error) = result {
                        tracing::warn!(%error, "Claude Code inbox connection closed");
                    }
                });
            }
            Some(result) = connections.join_next() => { result?; }
            _ = cancellation.cancelled() => break,
        }
    }
    while let Some(result) = connections.join_next().await {
        result?;
    }
    Ok(())
}

#[cfg(unix)]
fn inbox_path() -> Result<String> {
    let uid = unsafe { libc::geteuid() };
    let directory = Path::new("/tmp").join(format!("cc-socks-{uid}"));
    create_private_dir(&directory)?;
    Ok(directory
        .join(format!("{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned())
}

#[cfg(windows)]
fn inbox_path() -> Result<String> {
    Ok(format!(
        r"\\.\pipe\LOCAL\cc-msg-{}",
        Uuid::new_v4().simple()
    ))
}

#[cfg(unix)]
struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
impl Listener {
    async fn bind(path: &str) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let listener = tokio::net::UnixListener::bind(path)?;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        Ok(Self(listener))
    }

    async fn accept(&mut self) -> Result<tokio::net::UnixStream> {
        Ok(self.0.accept().await?.0)
    }
}

#[cfg(unix)]
async fn connect(path: &str) -> Result<tokio::net::UnixStream> {
    Ok(tokio::net::UnixStream::connect(path).await?)
}

#[cfg(windows)]
struct Listener {
    path: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    async fn bind(path: &str) -> Result<Self> {
        let next = tokio::net::windows::named_pipe::ServerOptions::new().create(path)?;
        Ok(Self {
            path: path.to_owned(),
            next,
        })
    }

    async fn accept(&mut self) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        self.next.connect().await?;
        let next = tokio::net::windows::named_pipe::ServerOptions::new().create(&self.path)?;
        Ok(std::mem::replace(&mut self.next, next))
    }
}

#[cfg(windows)]
async fn connect(path: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    Ok(tokio::net::windows::named_pipe::ClientOptions::new().open(path)?)
}

async fn identity_for_pid(pid: u32) -> Result<Option<ProcessIdentity>> {
    Ok(process_start(pid)
        .await?
        .map(|started| ProcessIdentity { pid, started }))
}

#[cfg(unix)]
async fn connected_server_identity(
    connection: &tokio::net::UnixStream,
) -> Result<Option<ProcessIdentity>> {
    let credentials = connection.peer_cred()?;
    ensure!(
        credentials.uid() == unsafe { libc::geteuid() },
        "connected peer belongs to another user"
    );
    match credentials.pid() {
        Some(pid) => identity_for_pid(u32::try_from(pid)?).await,
        None => Ok(None),
    }
}

#[cfg(unix)]
async fn connected_client_identity(
    connection: &tokio::net::UnixStream,
) -> Result<Option<ProcessIdentity>> {
    connected_server_identity(connection).await
}

#[cfg(windows)]
async fn connected_server_identity(
    connection: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> Result<Option<ProcessIdentity>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    let mut pid = 0;
    ensure!(
        unsafe { GetNamedPipeServerProcessId(connection.as_raw_handle(), &mut pid) } != 0,
        "could not read connected pipe server identity: {}",
        std::io::Error::last_os_error()
    );
    identity_for_pid(pid).await
}

#[cfg(windows)]
async fn connected_client_identity(
    connection: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> Result<Option<ProcessIdentity>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    let mut pid = 0;
    ensure!(
        unsafe { GetNamedPipeClientProcessId(connection.as_raw_handle(), &mut pid) } != 0,
        "could not read connected pipe client identity: {}",
        std::io::Error::last_os_error()
    );
    identity_for_pid(pid).await
}

async fn helper_descendant(identity: &ProcessIdentity) -> Result<bool> {
    // The helper is the per-conversation transport owner. The shared daemon cannot be an
    // ancestry root: other conversations and their commands also descend from that process.
    let helper = std::process::id();
    if helper == 1 {
        return Ok(false);
    }
    let mut pid = identity.pid;
    for _ in 0..32 {
        if pid == helper {
            return Ok(
                process_start(identity.pid).await?.as_deref() == Some(identity.started.as_str())
            );
        }
        let Some(parent) = parent_process(pid).await? else {
            return Ok(false);
        };
        if parent == pid || parent <= 1 {
            return Ok(false);
        }
        pid = parent;
    }
    Ok(false)
}

#[cfg(windows)]
async fn parent_process(pid: u32) -> Result<Option<u32>> {
    tokio::task::spawn_blocking(move || {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::Process32NextW;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            ensure!(
                snapshot != INVALID_HANDLE_VALUE,
                "could not read process ancestry: {}",
                std::io::Error::last_os_error()
            );
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut parent = None;
            let mut available = Process32FirstW(snapshot, &mut entry) != 0;
            while available {
                if entry.th32ProcessID == pid {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                available = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            Ok(parent)
        }
    })
    .await?
}

#[cfg(unix)]
async fn parent_process(pid: u32) -> Result<Option<u32>> {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?.trim().parse()?))
}

#[cfg(unix)]
async fn process_start(pid: u32) -> Result<Option<String>> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .await?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned()))
}

#[cfg(windows)]
async fn process_start(pid: u32) -> Result<Option<String>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Ok(None);
        }
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let result = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(process);
        Ok((result != 0).then(|| {
            ((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
                .to_string()
        }))
    }
}
