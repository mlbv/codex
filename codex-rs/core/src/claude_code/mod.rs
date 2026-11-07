//! Optional Claude Code memory, messaging, task, and delegation tools.

mod delegation;
pub(crate) mod events;
mod memory;
mod messaging;
pub(crate) mod tasks;

use crate::CodexThread;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::NativeReview;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolExposure;
use crate::tools::registry::ToolRegistry;
use codex_features::Feature;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

pub(crate) struct Runtime {
    pub(crate) thread: Weak<CodexThread>,
    pub(crate) events: events::EventSink,
    pub(crate) cancellation: CancellationToken,
    pub(crate) messaging: Option<Arc<messaging::Messaging>>,
    pub(crate) tasks: Arc<tasks::TaskRegistry>,
    pub(crate) delegation: Arc<delegation::Team>,
    pub(crate) memory_context: Option<String>,
    pub(crate) idle: tokio::sync::Notify,
}

pub(crate) fn enabled(config: &Config) -> bool {
    [
        Feature::ClaudeMemory,
        Feature::ClaudeMessaging,
        Feature::ClaudeBackgroundTasks,
        Feature::ClaudeDelegation,
    ]
    .into_iter()
    .any(|feature| config.features.enabled(feature))
}

pub(crate) async fn initialize(thread: &Arc<CodexThread>) -> anyhow::Result<()> {
    let config = thread.config().await;
    if !enabled(&config) {
        return Ok(());
    }
    let cancellation = CancellationToken::new();
    let events = events::EventSink::new(Arc::downgrade(thread));
    let messaging = if config.features.enabled(Feature::ClaudeMessaging) {
        Some(Arc::new(
            messaging::start(Arc::clone(thread), events.clone(), cancellation.clone()).await?,
        ))
    } else {
        None
    };
    let delegation = if config.features.enabled(Feature::ClaudeDelegation) {
        delegation::Team::shared(
            thread.session.services.agent_control.identity(),
            config.codex_home.as_path(),
        )
        .await?
    } else {
        Arc::new(delegation::Team::new())
    };
    let memory_context = if config.features.enabled(Feature::ClaudeMemory) {
        Some(
            memory::context(&config)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        )
    } else {
        None
    };
    let runtime = Runtime {
        thread: Arc::downgrade(thread),
        events,
        cancellation,
        messaging,
        tasks: Arc::new(tasks::TaskRegistry::new()),
        delegation,
        memory_context,
        idle: tokio::sync::Notify::new(),
    };
    let runtime = thread
        .session
        .services
        .thread_extension_data
        .get_or_init(|| runtime);
    if config.features.enabled(Feature::ClaudeDelegation) {
        delegation::on_ready(&runtime).await?;
    }
    runtime.events.start();
    Ok(())
}

pub(crate) fn session_runtime(session: &Session) -> Option<Arc<Runtime>> {
    session.services.thread_extension_data.get::<Runtime>()
}

pub(crate) fn has_pending_work(session: &Session) -> bool {
    session_runtime(session)
        .is_some_and(|runtime| runtime.tasks.has_pending_work() || runtime.events.has_pending())
}

pub(crate) fn runtime(invocation: &ToolInvocation) -> Arc<Runtime> {
    session_runtime(&invocation.session).unwrap()
}

pub(crate) async fn turn_started(session: &Session, turn_id: &str) -> anyhow::Result<()> {
    if let Some(runtime) = session_runtime(session) {
        let _ = turn_id;
        if let Some(messaging) = &runtime.messaging {
            let thread = runtime.thread.upgrade().unwrap();
            let config = thread.config().await;
            messaging.configure(config.as_ref()).await?;
            messaging.set_status("busy").await?;
        }
    }
    Ok(())
}

pub(crate) async fn idle(session: &Session) -> anyhow::Result<()> {
    if let Some(runtime) = session_runtime(session) {
        runtime.idle.notify_waiters();
        runtime.events.flush().await?;
        let thread = runtime.thread.upgrade().unwrap();
        let config = thread.config().await;
        if config.features.enabled(Feature::ClaudeDelegation) {
            delegation::on_idle(&runtime).await?;
        }
        if let Some(messaging) = &runtime.messaging {
            messaging.configure(config.as_ref()).await?;
            let pending = runtime.tasks.has_pending_work()
                || runtime.events.has_pending()
                || thread.session.active_turn.lock().await.is_some();
            messaging
                .set_status(if pending { "busy" } else { "idle" })
                .await?;
        }
    }
    Ok(())
}

pub(crate) async fn shutdown(session: &Session) -> anyhow::Result<()> {
    if let Some(runtime) = session_runtime(session) {
        runtime.cancellation.cancel();
        runtime.events.close();
        runtime.tasks.shutdown_quiet().await?;
        if let Some(messaging) = &runtime.messaging {
            messaging.shutdown().await?;
        }
        delegation::on_stopped(&runtime).await;
        runtime.events.shutdown().await?;
    }
    Ok(())
}

pub(crate) async fn report_error(session: &Session, error: impl Display) {
    session
        .send_event_raw(codex_protocol::protocol::Event {
            id: uuid::Uuid::new_v4().to_string(),
            msg: codex_protocol::protocol::EventMsg::Warning(
                codex_protocol::protocol::WarningEvent {
                    message: format!("Claude Code integration: {error}"),
                },
            ),
        })
        .await;
}

pub(crate) fn arguments<T: DeserializeOwned>(
    invocation: &ToolInvocation,
) -> Result<T, FunctionCallError> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return Err(error("expected function arguments"));
    };
    serde_json::from_str(arguments).map_err(error)
}

pub(crate) fn error(error: impl Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

pub(crate) fn tool(name: &str, description: &str, schema: Value) -> ResponsesApiTool {
    let description = match name {
        "monitor" => format!(
            "{}\n\n{}",
            include_str!("prompts/monitor.md"),
            TASK_ADAPTERS
        ),
        "bash" => format!(
            "{description}\n\n{}\n\n{}",
            include_str!("prompts/background_shell.md"),
            TASK_ADAPTERS
        ),
        "agent" => format!("{}\n\n{}", delegation_instructions().0, DELEGATION_ADAPTERS),
        _ => description.to_string(),
    };
    ResponsesApiTool {
        name: name.to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: serde_json::from_value(schema).unwrap(),
        output_schema: None,
    }
}

const TASK_ADAPTERS: &str = "In this Codex runtime, Bash is bash, Monitor is monitor, TaskOutput is task_output, and TaskStop is task_stop. Read command output files through the shell.";

pub(crate) const DELEGATION_ADAPTERS: &str = "In this Codex runtime, Agent is agent. SendMessage for teammates is team_send_message. TaskCreate, TaskGet, TaskList, and TaskUpdate are task_create, task_get, task_list, and task_update. subagent_type uses configured Codex agent roles; general-purpose selects the default role and fork inherits the current conversation.";

pub(crate) fn delegation_instructions() -> (&'static str, &'static str) {
    include_str!("prompts/delegation.md")
        .split_once("# SendMessage")
        .unwrap()
}

pub(crate) fn write_atomic(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(content)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Messaging(messaging::Tool),
    Tasks(tasks::Tool),
    Delegation(delegation::Tool),
}

struct NativeTool {
    kind: Kind,
    spec: ResponsesApiTool,
}

pub(crate) fn append_tools(config: &Config, registry: &mut ToolRegistry) {
    let mut specs = Vec::new();
    if config.features.enabled(Feature::ClaudeMessaging) {
        specs.extend(
            messaging::specs()
                .into_iter()
                .map(|(kind, spec)| (Kind::Messaging(kind), spec)),
        );
    }
    if config.features.enabled(Feature::ClaudeBackgroundTasks) {
        specs.extend(
            tasks::specs()
                .into_iter()
                .map(|(kind, spec)| (Kind::Tasks(kind), spec)),
        );
    }
    if config.features.enabled(Feature::ClaudeDelegation) {
        specs.extend(
            delegation::specs()
                .into_iter()
                .map(|(kind, spec)| (Kind::Delegation(kind), spec)),
        );
    }
    for (kind, spec) in specs {
        let native_review = match kind {
            Kind::Tasks(tasks::Tool::Bash | tasks::Tool::Monitor) => NativeReview::Required,
            _ => NativeReview::Disabled,
        };
        registry.register_trusted_with_native_review(
            Arc::new(NativeTool { kind, spec }),
            ToolExposure::Direct,
            native_review,
        );
    }
}

impl ToolExecutor<ToolInvocation> for NativeTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(&self.spec.name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(self.spec.clone())
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let output = match self.kind {
                Kind::Messaging(tool) => messaging::call(tool, invocation).await,
                Kind::Tasks(tool) => tasks::call(tool, invocation).await,
                Kind::Delegation(tool) => delegation::call(tool, invocation).await,
            }?;
            Ok(Box::new(JsonOutput(output)) as Box<dyn ToolOutput>)
        })
    }
}

impl CoreToolRuntime for NativeTool {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

struct JsonOutput(Value);

impl ToolOutput for JsonOutput {
    fn log_output(&self) -> String {
        self.0.to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.0.to_string(), Some(true))
            .to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.0.clone()
    }
}

/// Runs the local named-pipe/socket peer for one Codex conversation.
pub async fn run_bridge(args: Vec<String>) -> anyhow::Result<()> {
    messaging::run_bridge(args).await
}
