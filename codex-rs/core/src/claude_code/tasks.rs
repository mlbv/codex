//! Session-owned shell tasks and push monitors.

use super::Runtime;
use super::arguments;
use super::error;
use super::runtime;
use super::tool;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::unified_exec::ExecCommandHandler;
use codex_exec_server::ExecOutputStream;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_websocket_client::WebSocketConnector;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const OUTPUT_BYTES: usize = 1024 * 1024;
const OUTPUT_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const BACKGROUND_DEFAULT_MS: u64 = 1_800_000;
const BACKGROUND_MAX_MS: u64 = 7_200_000;
const MONITOR_DEFAULT_MS: u64 = 300_000;
const MONITOR_INTERACTIVE_MAX_MS: u64 = 1_800_000;
const MONITOR_EXEC_MAX_MS: u64 = 600_000;

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    Output,
    List,
    Stop,
    Monitor,
}

pub(crate) fn specs() -> Vec<(Tool, ResponsesApiTool)> {
    vec![
        (
            Tool::Output,
            tool(
                "task_output",
                "Retrieve a background task's status and retained output. block waits for completion, up to timeout milliseconds. Completion and monitor events arrive automatically.",
                json!({
                    "type":"object", "properties":{
                        "task_id":{"type":"string"}, "block":{"type":"boolean","default":true},
                        "timeout":{"type":"integer","minimum":0,"maximum":600000,"default":30000}
                    }, "required":["task_id"], "additionalProperties":false
                }),
            ),
        ),
        (
            Tool::List,
            tool(
                "background_tasks",
                "List this session's background shell tasks and monitors, including their task IDs and current status.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
        ),
        (
            Tool::Stop,
            tool(
                "task_stop",
                "Stop a background shell task or monitor by task_id.",
                json!({
                    "type":"object", "properties":{"task_id":{"type":"string"}},
                    "required":["task_id"], "additionalProperties":false
                }),
            ),
        ),
        (
            Tool::Monitor,
            tool(
                "monitor",
                "Start a background monitor. Supply exactly one of command or ws. Each stdout line or WebSocket text frame becomes an event delivered to the conversation while you keep working. Stderr is retained in output_file. Filter the source to useful success and failure signals. The default deadline is 5 minutes, capped to 30 minutes for interactive sessions and 10 minutes for noninteractive sessions. Expiry sends the delivered event count. Use task_stop to cancel and task_output to inspect output; polling is unnecessary.",
                json!({
                    "type":"object", "properties":{
                        "command":{"type":"string"},
                        "ws":{"type":"object","properties":{"url":{"type":"string"},"protocols":{"type":"array","items":{"type":"string"}}},"required":["url"],"additionalProperties":false},
                        "description":{"type":"string"},
                        "timeout_ms":{"type":"integer","minimum":1000,"maximum":3600000,"default":300000},
                        "headers":{"type":"object","additionalProperties":{"type":"string"},"description":"Headers for a WebSocket source. Host is set by the connector."},
                        "workdir":{"type":"string"}, "shell":{"type":"string"}
                    }, "required":["description"], "additionalProperties":false
                }),
            ),
        ),
    ]
}

#[derive(Default)]
pub(crate) struct TaskRegistry {
    tasks: Mutex<BTreeMap<String, Arc<Task>>>,
    jobs: Mutex<Vec<JoinHandle<()>>>,
    changed: Arc<Notify>,
}

impl TaskRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .values()
            .any(|task| task.running())
    }

    pub(crate) async fn wait_for_settled(&self, cancellation: &CancellationToken) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.has_pending_work() {
                return;
            }
            tokio::select! {
                _ = changed => {},
                _ = cancellation.cancelled() => return,
            }
        }
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.jobs.lock().unwrap().push(tokio::spawn(future));
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown_quiet().await
    }

    pub(crate) async fn shutdown_quiet(&self) -> anyhow::Result<()> {
        for task in self.tasks.lock().unwrap().values() {
            task.quiet.store(true, Ordering::Release);
            task.notify_completion.store(false, Ordering::Release);
            task.cancellation.cancel();
        }
        let jobs = std::mem::take(&mut *self.jobs.lock().unwrap());
        for result in futures::future::join_all(jobs).await {
            result?;
        }
        Ok(())
    }

    pub(crate) fn observer(&self, call_id: &str) -> Option<TaskObserver> {
        self.tasks
            .lock()
            .unwrap()
            .values()
            .find(|task| task.call_id == call_id)
            .map(|task| TaskObserver(Arc::clone(task)))
    }

    pub(crate) fn observe(&self, _runtime: &Arc<Runtime>, event: &EventMsg) {
        match event {
            EventMsg::ExecCommandBegin(event) => {
                if let Some(observer) = self.observer(&event.call_id)
                    && let Some(process_id) = &event.process_id
                {
                    observer.started(process_id.parse().unwrap());
                }
            }
            EventMsg::ExecCommandEnd(event) => {
                if let Some(observer) = self.observer(&event.call_id) {
                    observer.completed(event.exit_code);
                }
            }
            _ => {}
        }
    }

    fn get(&self, task_id: &str) -> Result<Arc<Task>, FunctionCallError> {
        self.tasks
            .lock()
            .unwrap()
            .get(task_id)
            .cloned()
            .ok_or_else(|| error(format!("unknown task: {task_id}")))
    }

    fn list(&self) -> Value {
        Value::Array(
            self.tasks
                .lock()
                .unwrap()
                .values()
                .map(|task| task.snapshot(false))
                .collect(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct TaskObserver(Arc<Task>);

impl std::fmt::Debug for TaskObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("TaskObserver")
            .field(&self.0.id)
            .finish()
    }
}

impl TaskObserver {
    pub(crate) fn is_background(&self) -> bool {
        self.0.notify_completion.load(Ordering::Acquire)
    }

    pub(crate) fn started(&self, process_id: i32) {
        self.0.state.lock().unwrap().process_id = Some(process_id);
        self.0.changed.notify_waiters();
    }

    /// Returns whether cancellation arrived before the process became stoppable.
    pub(crate) fn admitted(&self) -> bool {
        let now = Instant::now();
        {
            let mut state = self.0.state.lock().unwrap();
            state.status = "running";
            state.admitted_at = Some(now);
            state.deadline = state.timeout.map(|timeout| now + timeout);
        }
        let _ = self.0.events.send(TaskEvent::Wake);
        self.0.changed.notify_waiters();
        self.0.cancellation.is_cancelled()
    }

    pub(crate) async fn output(&self, stream: ExecOutputStream, bytes: &[u8]) {
        let _ = self
            .0
            .output
            .send(TaskOutput {
                stream,
                bytes: bytes.to_vec(),
                frame: false,
            })
            .await;
    }

    pub(crate) fn output_started(&self) {
        self.0.state.lock().unwrap().output_streams += 1;
    }

    pub(crate) fn output_closed(&self) {
        self.0.state.lock().unwrap().output_streams -= 1;
        let _ = self.0.events.send(TaskEvent::Wake);
    }

    pub(crate) fn completed(&self, exit_code: i32) {
        let _ = self.0.events.send(TaskEvent::Exit(exit_code));
    }
}

struct Task {
    id: String,
    call_id: String,
    description: String,
    kind: &'static str,
    output_file: PathBuf,
    created_at_ms: u128,
    state: Mutex<TaskState>,
    changed: Notify,
    registry_changed: Arc<Notify>,
    cancellation: CancellationToken,
    session: Weak<Session>,
    events: mpsc::UnboundedSender<TaskEvent>,
    output: mpsc::Sender<TaskOutput>,
    notify_completion: AtomicBool,
    quiet: AtomicBool,
}

struct TaskState {
    status: &'static str,
    process_id: Option<i32>,
    exit_code: Option<i32>,
    output: Vec<u8>,
    event_count: usize,
    output_streams: usize,
    awaiting_tool_result: bool,
    admitted_at: Option<Instant>,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    detail: Option<String>,
}

enum TaskEvent {
    Exit(i32),
    Failed(String),
    Notice(String),
    Wake,
}

struct TaskOutput {
    stream: ExecOutputStream,
    bytes: Vec<u8>,
    frame: bool,
}

struct TaskReceiver {
    output: mpsc::Receiver<TaskOutput>,
    events: mpsc::UnboundedReceiver<TaskEvent>,
}

impl Task {
    fn snapshot(&self, include_output: bool) -> Value {
        let state = self.state.lock().unwrap();
        let mut value = json!({
            "task_id":self.id, "task_type":self.kind, "description":self.description,
            "status":state.status, "process_id":state.process_id, "exit_code":state.exit_code,
            "event_count":state.event_count, "output_file":self.output_file,
            "created_at_ms":self.created_at_ms
        });
        value["timeout_ms"] = json!(state.timeout.map(|timeout| timeout.as_millis()));
        value["detail"] = json!(state.detail);
        if include_output {
            value["output"] = Value::String(String::from_utf8_lossy(&state.output).into_owned());
        }
        value
    }

    fn running(&self) -> bool {
        matches!(
            self.state.lock().unwrap().status,
            "starting" | "running" | "finishing"
        )
    }

    fn background(&self, timeout: Duration) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.status != "running" {
            return false;
        }
        state.timeout = Some(timeout);
        state.deadline = Some(Instant::now() + timeout);
        self.notify_completion.store(true, Ordering::Release);
        drop(state);
        let _ = self.events.send(TaskEvent::Wake);
        true
    }
}

#[derive(Deserialize)]
struct ShellTaskArgs {
    cmd: String,
    description: Option<String>,
    #[serde(default)]
    run_in_background: bool,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct MonitorArgs {
    command: Option<String>,
    ws: Option<WebSocketArgs>,
    description: String,
    timeout_ms: Option<u64>,
    workdir: Option<String>,
    shell: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct WebSocketArgs {
    url: String,
    #[serde(default)]
    protocols: Vec<String>,
}

#[derive(Deserialize)]
struct TaskIdArgs {
    task_id: String,
}

#[derive(Deserialize)]
struct OutputArgs {
    task_id: String,
    #[serde(default = "yes")]
    block: bool,
    #[serde(default = "output_timeout")]
    timeout: u64,
}

fn yes() -> bool {
    true
}
fn output_timeout() -> u64 {
    30_000
}

pub(crate) async fn call(
    which: Tool,
    invocation: ToolInvocation,
) -> Result<Value, FunctionCallError> {
    let runtime = runtime(&invocation);
    match which {
        Tool::List => Ok(runtime.tasks.list()),
        Tool::Output => {
            let args: OutputArgs = arguments(&invocation)?;
            let task = runtime.tasks.get(&args.task_id)?;
            if args.block {
                wait_until(
                    &task,
                    Some(Instant::now() + Duration::from_millis(args.timeout.min(600_000))),
                    &invocation.cancellation_token,
                )
                .await;
            }
            Ok(task.snapshot(true))
        }
        Tool::Stop => {
            let args: TaskIdArgs = arguments(&invocation)?;
            let task = runtime.tasks.get(&args.task_id)?;
            task.cancellation.cancel();
            wait_until(
                &task,
                Some(Instant::now() + Duration::from_secs(30)),
                &invocation.cancellation_token,
            )
            .await;
            Ok(task.snapshot(false))
        }
        Tool::Monitor => {
            let args: MonitorArgs = arguments(&invocation)?;
            if args.command.is_some() == args.ws.is_some() {
                return Err(error("provide exactly one of command or ws"));
            }
            let max_timeout = if noninteractive(&invocation) {
                MONITOR_EXEC_MAX_MS
            } else {
                MONITOR_INTERACTIVE_MAX_MS
            };
            let timeout_ms = args
                .timeout_ms
                .unwrap_or(MONITOR_DEFAULT_MS)
                .clamp(1_000, max_timeout);
            let connector = args
                .ws
                .as_ref()
                .map(|_| WebSocketConnector::new(&invocation.turn.config.http_client_factory()))
                .transpose()
                .map_err(error)?;
            let cancellation = invocation.cancellation_token.clone();
            let (task, receiver, file) = create_task(
                &runtime,
                &invocation,
                args.description,
                "monitor",
                true,
                Some(Duration::from_millis(timeout_ms)),
            )
            .await?;
            spawn_collector(Arc::clone(&runtime), Arc::clone(&task), receiver, file);
            match (args.command, args.ws) {
                (Some(command), None) => {
                    let invocation = shell_invocation(
                        invocation,
                        &command,
                        args.workdir,
                        args.shell,
                        Some(1),
                        &task,
                    )?;
                    let _ = spawn_command(
                        &runtime.tasks,
                        ExecCommandHandler::default(),
                        invocation,
                        task.clone(),
                        None,
                    );
                }
                (None, Some(ws)) => {
                    spawn_websocket(
                        &runtime.tasks,
                        ws,
                        args.headers,
                        task.clone(),
                        connector.unwrap(),
                    );
                }
                _ => unreachable!(),
            }
            wait_admitted(&task, &cancellation).await?;
            Ok(
                json!({"taskId":task.id,"timeoutMs":timeout_ms,"persistent":false,"output_file":task.output_file}),
            )
        }
    }
}

fn noninteractive(invocation: &ToolInvocation) -> bool {
    matches!(
        &invocation.turn.session_source,
        SessionSource::Exec | SessionSource::Mcp
    )
}

pub(crate) fn extend_exec_spec(spec: &mut ToolSpec) {
    let ToolSpec::Function(spec) = spec else {
        unreachable!()
    };
    spec.description.push_str("\n\n");
    spec.description
        .push_str(include_str!("prompts/background_shell.md"));
    let properties = spec.parameters.properties.get_or_insert_default();
    properties.insert("run_in_background".to_string(), JsonSchema::boolean(Some(
        "Return after command admission and deliver completion automatically. Defaults to false.".to_string(),
    )));
    properties.insert(
        "description".to_string(),
        JsonSchema::string(Some(
            "Short description shown with background completion.".to_string(),
        )),
    );
    properties.insert("timeout_ms".to_string(), JsonSchema::number(Some(
        "Background lifetime from admission, or from yielding for a foreground command. Defaults to 1800000 ms (600000 in noninteractive sessions), maximum 7200000 ms.".to_string(),
    )));
    let mut output = spec.output_schema.take().unwrap().into_value();
    output["properties"]["task_id"] = json!({"type":"string","description":"Task identifier accepted by task_output and task_stop."});
    output["properties"]["output_file"] =
        json!({"type":"string","description":"Retained complete command output."});
    output["properties"]["status"] = json!({"type":"string"});
    spec.output_schema = Some(output.into());
}

pub(crate) async fn exec_command(
    handler: ExecCommandHandler,
    mut invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let args: ShellTaskArgs = arguments(&invocation)?;
    let runtime = runtime(&invocation);
    let cancellation = invocation.cancellation_token.clone();
    let background_default = if noninteractive(&invocation) {
        MONITOR_EXEC_MAX_MS
    } else {
        BACKGROUND_DEFAULT_MS
    };
    let timeout = Duration::from_millis(
        args.timeout_ms
            .unwrap_or(background_default)
            .clamp(1, BACKGROUND_MAX_MS),
    );
    let (task, receiver, file) = create_task(
        &runtime,
        &invocation,
        args.description.unwrap_or(args.cmd),
        "shell",
        args.run_in_background,
        args.run_in_background.then_some(timeout),
    )
    .await?;
    spawn_collector(Arc::clone(&runtime), Arc::clone(&task), receiver, file);
    invocation.cancellation_token = task.cancellation.clone();
    let payload = invocation.payload.clone();
    let result = spawn_command(
        &runtime.tasks,
        handler,
        invocation,
        Arc::clone(&task),
        Some(timeout),
    );
    let guard = task.cancellation.clone().drop_guard();
    wait_admitted(&task, &cancellation).await?;
    let native = if args.run_in_background {
        None
    } else {
        let output = tokio::select! {
            _ = cancellation.cancelled() => return Err(error("shell call interrupted")),
            result = result => result.map_err(error)??,
        };
        let value = output.code_mode_result(&payload);
        if value.get("session_id").is_none() {
            wait_until(&task, None, &cancellation).await;
            if cancellation.is_cancelled() {
                return Err(error("shell call interrupted"));
            }
            runtime.tasks.tasks.lock().unwrap().remove(&task.id);
        }
        Some(output)
    };
    guard.disarm();
    Ok(Box::new(ShellTaskOutput {
        native,
        task: task.snapshot(false),
        payload,
    }))
}

struct ShellTaskOutput {
    native: Option<Box<dyn ToolOutput>>,
    task: Value,
    payload: ToolPayload,
}

impl ShellTaskOutput {
    fn value(&self) -> Value {
        let mut value = match &self.native {
            Some(output) => output.code_mode_result(&self.payload),
            None => json!({"wall_time_seconds":0.0,"output":""}),
        };
        if self.native.is_none() {
            if !self.task["process_id"].is_null() {
                value["session_id"] = self.task["process_id"].clone();
            }
            if !self.task["exit_code"].is_null() {
                value["exit_code"] = self.task["exit_code"].clone();
            }
        }
        value["task_id"] = self.task["task_id"].clone();
        value["output_file"] = self.task["output_file"].clone();
        value["status"] = self.task["status"].clone();
        value
    }
}

impl ToolOutput for ShellTaskOutput {
    fn log_output(&self) -> String {
        self.value().to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.native
            .as_ref()
            .is_none_or(|output| output.success_for_logging())
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        super::JsonOutput(self.value()).to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.value()
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        self.native.as_ref().map_or_else(
            || call_id.to_string(),
            |output| output.post_tool_use_id(call_id),
        )
    }

    fn post_tool_use_cwd(&self) -> Option<codex_utils_path_uri::PathUri> {
        self.native
            .as_ref()
            .and_then(|output| output.post_tool_use_cwd())
    }

    fn post_tool_use_input(&self, payload: &ToolPayload) -> Option<Value> {
        self.native
            .as_ref()
            .and_then(|output| output.post_tool_use_input(payload))
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<Value> {
        self.native
            .as_ref()
            .and_then(|output| output.post_tool_use_response(call_id, payload))
    }
}

async fn wait_admitted(
    task: &Task,
    cancellation: &CancellationToken,
) -> Result<(), FunctionCallError> {
    let admission_guard = task.cancellation.clone().drop_guard();
    loop {
        let changed = task.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let state = task.state.lock().unwrap();
            if state.admitted_at.is_some() {
                admission_guard.disarm();
                return Ok(());
            }
            if !matches!(state.status, "starting" | "finishing") {
                return Err(error(state.detail.as_deref().unwrap()));
            }
        }
        tokio::select! {
            _ = changed => {},
            _ = cancellation.cancelled() => {task.cancellation.cancel();return Err(error("command admission interrupted"));},
        }
    }
}

async fn wait_until(task: &Task, deadline: Option<Instant>, cancellation: &CancellationToken) {
    loop {
        let changed = task.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !task.running() {
            break;
        }
        tokio::select! {
            _ = changed => {},
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => break,
            _ = cancellation.cancelled() => break,
        }
    }
}

async fn create_task(
    runtime: &Arc<Runtime>,
    invocation: &ToolInvocation,
    description: String,
    kind: &'static str,
    notify: bool,
    timeout: Option<Duration>,
) -> Result<(Arc<Task>, TaskReceiver, tokio::fs::File), FunctionCallError> {
    let id = format!("task-{}", Uuid::new_v4().simple());
    let directory = invocation
        .turn
        .config
        .codex_home
        .join("tmp")
        .join("background-tasks");
    tokio::fs::create_dir_all(&directory).await.map_err(error)?;
    let output_file = directory.join(format!("{id}.output"));
    let file = tokio::fs::File::create(&output_file).await.map_err(error)?;
    let (events, event_receiver) = mpsc::unbounded_channel();
    let (output, output_receiver) = mpsc::channel(64);
    let receiver = TaskReceiver {
        events: event_receiver,
        output: output_receiver,
    };
    let task = Arc::new(Task {
        id: id.clone(),
        call_id: invocation.call_id.clone(),
        description,
        kind,
        output_file: output_file.into_path_buf(),
        created_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        state: Mutex::new(TaskState {
            status: "starting",
            process_id: None,
            exit_code: None,
            output: Vec::new(),
            event_count: 0,
            output_streams: 0,
            awaiting_tool_result: kind == "shell",
            admitted_at: None,
            timeout,
            deadline: None,
            detail: None,
        }),
        changed: Notify::new(),
        registry_changed: Arc::clone(&runtime.tasks.changed),
        cancellation: runtime.cancellation.child_token(),
        session: Arc::downgrade(&invocation.session),
        events,
        output,
        notify_completion: AtomicBool::new(notify),
        quiet: AtomicBool::new(false),
    });
    runtime.tasks.tasks.lock().unwrap().insert(id, task.clone());
    Ok((task, receiver, file))
}

fn shell_invocation(
    mut invocation: ToolInvocation,
    command: &str,
    workdir: Option<String>,
    shell: Option<String>,
    timeout: Option<u64>,
    task: &Task,
) -> Result<ToolInvocation, FunctionCallError> {
    let mut args = json!({"cmd":command,"tty":false,"yield_time_ms":timeout.unwrap_or(10_000).clamp(1,30_000)});
    if let Some(workdir) = workdir {
        args["workdir"] = workdir.into();
    }
    if let Some(shell) = shell {
        args["shell"] = shell.into();
    }
    invocation.payload = ToolPayload::Function {
        arguments: serde_json::to_string(&args).map_err(error)?,
    };
    invocation.cancellation_token = task.cancellation.clone();
    Ok(invocation)
}

fn spawn_command(
    registry: &TaskRegistry,
    handler: ExecCommandHandler,
    invocation: ToolInvocation,
    task: Arc<Task>,
    background_timeout: Option<Duration>,
) -> tokio::sync::oneshot::Receiver<Result<Box<dyn ToolOutput>, FunctionCallError>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    registry.spawn(async move {
        let payload = invocation.payload.clone();
        let result = handler.handle_untracked(invocation).await;
        let admitted = task.state.lock().unwrap().admitted_at.is_some();
        if !admitted {
            let event = match &result {
                Ok(output) => {
                    TaskObserver(Arc::clone(&task)).admitted();
                    let result = output.code_mode_result(&payload);
                    let _ = task
                        .output
                        .send(TaskOutput {
                            stream: ExecOutputStream::Stdout,
                            bytes: result["output"].as_str().unwrap().as_bytes().to_vec(),
                            frame: false,
                        })
                        .await;
                    TaskEvent::Exit(result["exit_code"].as_i64().unwrap_or(0) as i32)
                }
                Err(err) => TaskEvent::Failed(err.to_string()),
            };
            let _ = task.events.send(event);
        } else if let Err(err) = &result {
            let _ = task.events.send(TaskEvent::Failed(err.to_string()));
        }
        if let Ok(output) = &result
            && let Some(timeout) = background_timeout
            && output
                .code_mode_result(&payload)
                .get("session_id")
                .is_some()
            && !task.notify_completion.load(Ordering::Acquire)
        {
            task.background(timeout);
        }
        task.state.lock().unwrap().awaiting_tool_result = false;
        let _ = task.events.send(TaskEvent::Wake);
        let _ = sender.send(result);
    });
    receiver
}

fn spawn_websocket(
    registry: &TaskRegistry,
    args: WebSocketArgs,
    headers: BTreeMap<String, String>,
    task: Arc<Task>,
    connector: WebSocketConnector,
) {
    registry.spawn(async move {
        let result = async {
            let mut request = args.url.into_client_request()?;
            for (name,value) in headers {
                if !name.eq_ignore_ascii_case("host") {request.headers_mut().insert(http::header::HeaderName::try_from(name)?,value.parse()?);}
            }
            if !args.protocols.is_empty() {
                request.headers_mut().insert("Sec-WebSocket-Protocol",args.protocols.join(", ").parse()?);
            }
            let config=WebSocketConfig::default().max_frame_size(Some(OUTPUT_BYTES)).max_message_size(Some(OUTPUT_BYTES));
            let (mut socket, _) = tokio::select! {
                _ = task.cancellation.cancelled() => return anyhow::Ok(()),
                result = tokio::time::timeout(Duration::from_secs(30),connector.connect(request,config)) => result??,
            };
            TaskObserver(task.clone()).admitted();
            loop {
                tokio::select! {
                    _ = task.cancellation.cancelled() => break,
                    message = socket.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            if text.len() > OUTPUT_BYTES {anyhow::bail!("WebSocket frame exceeds 1048576 bytes");}
                            let _ = task.output.send(TaskOutput{stream:ExecOutputStream::Stdout,bytes:text.as_bytes().to_vec(),frame:true}).await;
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            if bytes.len() > OUTPUT_BYTES {anyhow::bail!("WebSocket frame exceeds 1048576 bytes");}
                            let _ = task.output.send(TaskOutput{stream:ExecOutputStream::Stdout,bytes:format!("[binary frame, {} bytes]",bytes.len()).into_bytes(),frame:true}).await;
                        }
                        Some(Ok(Message::Ping(bytes))) => socket.send(Message::Pong(bytes)).await?,
                        Some(Ok(Message::Close(frame))) => {
                            let message=match frame {Some(frame)=>format!("WebSocket closed: {} {}",frame.code,frame.reason),None=>"WebSocket closed".to_string()};
                            let _=task.events.send(TaskEvent::Notice(message));
                            break;
                        }
                        None => break,
                        Some(Err(err)) => return Err(err.into()),
                        _ => {},
                    }
                }
            }
            anyhow::Ok(())
        }.await;
        let event = match result {Ok(())=>TaskEvent::Exit(0),Err(err)=>TaskEvent::Failed(format!("WebSocket error: {err}"))};
        let _ = task.events.send(event);
    });
}

fn spawn_collector(
    runtime: Arc<Runtime>,
    task: Arc<Task>,
    mut receiver: TaskReceiver,
    mut file: tokio::fs::File,
) {
    let registry = Arc::clone(&runtime.tasks);
    registry.spawn(async move {
        let mut monitor = MonitorStream::new();
        let mut batch_deadline = None;
        let mut stopped_reason=None;
        let mut stopping=false;
        let mut written=0;
        let mut disk_capped=false;
        let mut file_failed=false;
        let mut termination:Option<futures::future::BoxFuture<'static,()>>=None;
        let mut terminal = None;
        let (mut status, mut exit_code, mut detail) = loop {
            if terminal.is_some()
                && {
                    let state = task.state.lock().unwrap();
                    state.output_streams == 0 && !state.awaiting_tool_result
                }
                && receiver.output.is_empty()
            {
                break terminal.take().unwrap();
            }
            let deadline=task.state.lock().unwrap().deadline;
            tokio::select! {
                biased;
                _ = task.cancellation.cancelled(), if !stopping => {
                    stopping=true;
                    stopped_reason.get_or_insert(("stopped","Task stopped".to_string()));
                    let task=Arc::clone(&task);
                    termination=Some(Box::pin(async move {stop_process(&task).await}));
                },
                _ = async {tokio::time::sleep_until(deadline.unwrap()).await}, if deadline.is_some() && stopped_reason.is_none() && terminal.is_none() => {
                    let timeout=task.state.lock().unwrap().timeout.unwrap();
                    stopped_reason=Some(("timed_out",format!("{} expired after {} milliseconds",if task.kind=="monitor" {"Monitor"} else {"Background command"},timeout.as_millis())));
                    task.cancellation.cancel();
                },
                _ = async {termination.as_mut().unwrap().await}, if termination.is_some() => {termination=None;},
                _ = async {tokio::time::sleep_until(batch_deadline.unwrap()).await}, if batch_deadline.is_some() => {
                    batch_deadline=None;
                    if !monitor.deliver(&runtime,&task,false).await {stopped_reason=Some(("stopped","Monitor stopped: too much output; restart with a more selective source".to_string()));task.cancellation.cancel();}
                },
                output = receiver.output.recv() => {
                        let TaskOutput{stream,bytes,frame}=output.unwrap();
                        if !file_failed {
                            if let Err(err) = write_output(&mut file,&mut written,&mut disk_capped,&bytes,frame).await {
                                file_failed=true;
                                stopped_reason=Some(("failed",err.to_string()));
                                task.cancellation.cancel();
                            }
                        }
                        {
                            let mut state=task.state.lock().unwrap();
                            state.output.extend_from_slice(&bytes);
                            if frame {state.output.push(b'\n');}
                            let excess=state.output.len().saturating_sub(OUTPUT_BYTES);
                            state.output.drain(..excess);
                        }
                        if task.kind=="monitor" && matches!(stream,ExecOutputStream::Stdout) {
                            if frame {
                                if !monitor.emit(&runtime,&task,shorten(&String::from_utf8_lossy(&bytes),3000)).await {stopped_reason=Some(("stopped","Monitor stopped: too much output; restart with a more selective source".to_string()));task.cancellation.cancel();}
                            } else {
                                monitor.push(&bytes);
                                if !monitor.lines.is_empty() && batch_deadline.is_none() {batch_deadline=Some(Instant::now()+Duration::from_millis(200));}
                            }
                        }
                        task.changed.notify_waiters();
                },
                event = receiver.events.recv() => match event.unwrap() {
                    TaskEvent::Exit(code) => {
                        let (status,detail)=stopped_reason.take().unwrap_or_else(||(if code==0 {"completed"} else {"failed"},format!("Task exited with code {code}")));
                        terminal=Some((status,Some(code),detail));
                    }
                    TaskEvent::Failed(message) => {
                        let (status,detail)=stopped_reason.take().unwrap_or(("failed",message));
                        terminal=Some((status,None,detail));
                    }
                    TaskEvent::Notice(message) => {
                        if !task.quiet.load(Ordering::Acquire) {
                            if let Err(err)=runtime.events.emit("monitor_notice",format!("{} ({}): {message}",task.description,task.id)).await {tracing::warn!("monitor notice failed: {err}");}
                        }
                    }
                    TaskEvent::Wake => {},
                }
            }
        };
        if let Some(termination)=termination {termination.await;}
        if task.kind=="monitor" {monitor.deliver(&runtime,&task,true).await;}
        if let Err(err)=file.flush().await {
            status="failed";exit_code=None;detail=err.to_string();
        }
        {
            let mut state=task.state.lock().unwrap();
            state.status="finishing";
        }
        let admitted=task.state.lock().unwrap().admitted_at.is_some();
        if admitted && !task.quiet.load(Ordering::Acquire) && task.notify_completion.load(Ordering::Acquire) {
            let count=task.state.lock().unwrap().event_count;
            let events=if task.kind=="monitor" {format!(" {count} monitor events delivered.")} else {String::new()};
            let text=format!("{} ({}): {detail}.{events} Output: {}",task.description,task.id,task.output_file.display());
            if let Err(err)=runtime.events.emit("task_notification",text).await {tracing::warn!("background task notification failed: {err}");}
        }
        {
            let mut state=task.state.lock().unwrap();
            state.status=status;
            state.exit_code=exit_code;
            state.detail=Some(detail);
        }
        task.changed.notify_waiters();
        task.registry_changed.notify_waiters();
    });
}

async fn write_output(
    file: &mut tokio::fs::File,
    written: &mut u64,
    capped: &mut bool,
    bytes: &[u8],
    frame: bool,
) -> std::io::Result<()> {
    if *capped {
        return Ok(());
    }
    let length = bytes.len() as u64 + u64::from(frame);
    if *written + length > OUTPUT_DISK_BYTES {
        file.write_all(b"\n[output truncated: exceeded 5GB disk cap]\n")
            .await?;
        *capped = true;
    } else {
        file.write_all(bytes).await?;
        if frame {
            file.write_all(b"\n").await?;
        }
        *written += length;
    }
    Ok(())
}

async fn stop_process(task: &Task) {
    let process_id = task.state.lock().unwrap().process_id;
    if let Some(process_id) = process_id
        && let Some(session) = task.session.upgrade()
    {
        session
            .services
            .unified_exec_manager
            .terminate_process(process_id)
            .await;
    }
}

struct MonitorStream {
    partial: Vec<u8>,
    lines: Vec<String>,
    tokens: usize,
    refilled: Instant,
    suppression_started: Option<Instant>,
    last_suppression: Option<Instant>,
    suppressed: usize,
}

impl MonitorStream {
    fn new() -> Self {
        Self {
            partial: Vec::new(),
            lines: Vec::new(),
            tokens: 10,
            refilled: Instant::now(),
            suppression_started: None,
            last_suppression: None,
            suppressed: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line = self.partial.drain(..=end).collect::<Vec<_>>();
            self.line(&line);
        }
        let excess = self.partial.len().saturating_sub(OUTPUT_BYTES);
        self.partial.drain(..excess);
    }

    fn line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if !text.is_empty() {
            self.lines.push(shorten(text, 500));
        }
    }

    async fn deliver(&mut self, runtime: &Runtime, task: &Task, flush_partial: bool) -> bool {
        if flush_partial {
            let partial = std::mem::take(&mut self.partial);
            self.line(&partial);
        }
        if self.lines.is_empty() {
            return true;
        }
        let batch = shorten(&std::mem::take(&mut self.lines).join("\n"), 3000);
        self.emit(runtime, task, batch).await
    }

    async fn emit(&mut self, runtime: &Runtime, task: &Task, batch: String) -> bool {
        if task.quiet.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        let refills = now.duration_since(self.refilled).as_millis() / 2000;
        if refills > 0 {
            self.tokens = (self.tokens + refills as usize).min(10);
            self.refilled += Duration::from_millis(refills as u64 * 2000);
        }
        if self.tokens == 0 {
            self.suppressed += 1;
            self.last_suppression = Some(now);
            let since = *self.suppression_started.get_or_insert(now);
            return now.duration_since(since) < Duration::from_secs(30);
        }
        self.tokens -= 1;
        if self.suppressed > 0 {
            let text = format!(
                "{} ({}): [{} events suppressed: output rate too high. Restart with a more selective filter.]",
                task.description, task.id, self.suppressed
            );
            if let Err(err) = runtime.events.emit("monitor_notice", text).await {
                tracing::warn!("monitor notice failed: {err}");
            }
            self.suppressed = 0;
        }
        if let Some(last) = self.last_suppression
            && now.duration_since(last) > Duration::from_secs(6)
        {
            self.suppression_started = None;
        }
        task.state.lock().unwrap().event_count += 1;
        let text = format!("{} ({})\n<event>{batch}</event>", task.description, task.id);
        if let Err(err) = runtime.events.emit("monitor_event", text).await {
            tracing::warn!("monitor event failed: {err}");
        }
        true
    }
}

fn shorten(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{head}...(truncated)")
    } else {
        head
    }
}
