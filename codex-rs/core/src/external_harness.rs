//! Responses requests for a harness that owns its conversation history outside a Codex session.
//!
//! Requests carry a Codex turn's identity: session and thread ids, turn metadata, context
//! windows, sticky turn routing, WebSocket continuation and the response-stream retry policy.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use codex_analytics::CompactionImplementation;
pub use codex_analytics::CompactionPhase;
pub use codex_analytics::CompactionReason;
pub use codex_analytics::CompactionTrigger;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_http_client::RetryAfter;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_model_provider::create_model_provider;
use codex_otel::SessionTelemetry;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::mcp::McpAttribution;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_rollout_trace::InferenceTraceContext;
use codex_terminal_detection::user_agent;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_utils_path_uri::PathUri;
use codex_utils_string::to_ascii_json_string;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;
use tracing::warn;
use uuid::Uuid;

use crate::client::ModelClient;
use crate::client::ModelClientSession;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::compact_remote_v2::RETAINED_MESSAGE_TOKEN_BUDGET;
use crate::compact_remote_v2::truncate_retained_messages_for_remote_compaction;
use crate::config::Config;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::session::Session;
use crate::tools::hosted_spec::WebSearchToolOptions;
use crate::tools::hosted_spec::create_web_search_tool;
use crate::tools::sandboxing::configured_windows_sandbox_selection;
use crate::turn_metadata::ExecutionMetadata;
use crate::turn_metadata::TurnMetadataState;
use crate::windows_sandbox::WindowsSandboxLevelExt;

const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_COMPACTION_STREAM_RETRIES: u64 = 2;
const USER_TURN_TRIGGER: &str = "user";

/// Where an external conversation sits in Codex's agent tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExternalThreadRole {
    Root,
    /// A root thread continuing `forked_from`'s history inside that thread's session.
    Fork {
        session_id: ThreadId,
        forked_from: ThreadId,
    },
    Subagent {
        session_id: ThreadId,
        parent_thread_id: ThreadId,
        depth: i32,
    },
}

/// What one request does for the turn.
#[derive(Clone, Copy, Debug)]
pub enum ExternalRequestKind {
    Turn,
    Compaction {
        trigger: CompactionTrigger,
        reason: CompactionReason,
        phase: CompactionPhase,
    },
}

/// Per-request model settings, resolved by the harness.
#[derive(Clone, Debug)]
pub struct ExternalRequest<'a> {
    pub model_info: &'a ModelInfo,
    pub effort: Option<ReasoningEffort>,
    pub summary: ReasoningSummary,
    pub service_tier: Option<String>,
    pub kind: ExternalRequestKind,
}

#[derive(Clone, Copy, Debug)]
struct ContextWindow {
    number: u64,
    id: Uuid,
}

/// Request identity shared by every client of one external conversation.
pub struct ExternalThread {
    config: Arc<Config>,
    auth_manager: Arc<AuthManager>,
    installation_id: String,
    originator: String,
    thread_id: ThreadId,
    role: ExternalThreadRole,
    session_source: SessionSource,
    thread_source: ThreadSource,
    git_root_discovery: Arc<GitRootDiscovery>,
    window: StdMutex<ContextWindow>,
}

/// What a harness persists to resume a thread: its identity and current context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalThreadState {
    pub thread_id: ThreadId,
    pub role: ExternalThreadRole,
    pub window_number: u64,
    pub window_id: Uuid,
}

impl ExternalThreadState {
    /// The session's root thread id: `prompt_cache_key`, the `session-id` header and
    /// standalone search's session.
    pub fn session_id(&self) -> ThreadId {
        match self.role {
            ExternalThreadRole::Root => self.thread_id,
            ExternalThreadRole::Fork { session_id, .. }
            | ExternalThreadRole::Subagent { session_id, .. } => session_id,
        }
    }

    /// Depth in the agent tree; roots are 0.
    pub fn depth(&self) -> i32 {
        match self.role {
            ExternalThreadRole::Root | ExternalThreadRole::Fork { .. } => 0,
            ExternalThreadRole::Subagent { depth, .. } => depth,
        }
    }
}

impl ExternalThread {
    pub fn new(
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        installation_id: String,
        originator: String,
        git_root_discovery: Arc<GitRootDiscovery>,
        role: ExternalThreadRole,
    ) -> Self {
        Self::resume(
            config,
            auth_manager,
            installation_id,
            originator,
            git_root_discovery,
            ExternalThreadState {
                thread_id: ThreadId::new(),
                role,
                window_number: 0,
                window_id: Uuid::now_v7(),
            },
        )
    }

    /// Continues a persisted thread.
    pub fn resume(
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        installation_id: String,
        originator: String,
        git_root_discovery: Arc<GitRootDiscovery>,
        state: ExternalThreadState,
    ) -> Self {
        let (session_source, thread_source) = match state.role {
            ExternalThreadRole::Root | ExternalThreadRole::Fork { .. } => {
                (SessionSource::Cli, ThreadSource::User)
            }
            ExternalThreadRole::Subagent {
                parent_thread_id,
                depth,
                ..
            } => (
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                }),
                ThreadSource::Subagent,
            ),
        };
        Self {
            config,
            auth_manager,
            installation_id,
            originator,
            thread_id: state.thread_id,
            role: state.role,
            session_source,
            thread_source,
            git_root_discovery,
            window: StdMutex::new(ContextWindow {
                number: state.window_number,
                id: state.window_id,
            }),
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub fn state(&self) -> ExternalThreadState {
        let window = self.context_window();
        ExternalThreadState {
            thread_id: self.thread_id,
            role: self.role,
            window_number: window.number,
            window_id: window.id,
        }
    }

    /// [`ExternalThreadState::session_id`].
    pub fn session_id(&self) -> String {
        self.state().session_id().to_string()
    }

    /// A client owning its own WebSocket; concurrent lanes of a thread each need one.
    pub fn model_client(&self) -> ModelClient {
        let config = self.config.as_ref();
        ModelClient::new(
            Some(Arc::clone(&self.auth_manager)),
            if config.features.enabled(Feature::UseAgentIdentity) {
                AgentIdentityAuthPolicy::ChatGptAuth
            } else {
                AgentIdentityAuthPolicy::JwtOnly
            },
            self.thread_id,
            config.model_provider.clone(),
            self.session_source.clone(),
            self.originator.clone(),
            config.model_verbosity,
            config.features.enabled(Feature::ContentItemKinds),
            config.features.enabled(Feature::ReasoningEffortOverride),
            config.features.enabled(Feature::EnableRequestCompression),
            config.features.enabled(Feature::RuntimeMetrics),
            Session::build_model_client_beta_features_header(config),
            config
                .features
                .enabled(Feature::ConcurrentReasoningSummaries),
            config.features.enabled(Feature::OutputTokenReplay),
            /*attestation_provider*/ None,
            config.http_client_factory(),
            config.workspace_routing_context(),
            Vec::new(),
        )
    }

    /// Whether this thread can change effort through durable configuration updates.
    pub fn effort_updates_enabled(&self, model_info: &ModelInfo) -> bool {
        self.config
            .features
            .enabled(Feature::ReasoningEffortOverride)
            && self.config.model_provider.is_openai()
            && model_info.supports_reasoning_effort_updates
    }

    /// Codex's session-start prewarm: a `generate=false` request under the startup turn
    /// context. The first turn adopts the returned session to continue from the warmup.
    pub async fn prewarm(
        self: &Arc<Self>,
        client: &ModelClient,
        cwd: AbsolutePathBuf,
        prompt: &Prompt,
        request: &ExternalRequest<'_>,
    ) -> Result<ModelClientSession> {
        let mut startup = self
            .turn(
                client.new_session(),
                INITIAL_SUBMIT_ID.to_string(),
                cwd,
                request.model_info,
            )
            .await;
        if !startup.client_session.is_websocket_prewarmed().await {
            let metadata = startup.responses_metadata(request, CodexResponsesRequestKind::Prewarm);
            startup
                .client_session
                .prewarm_websocket(
                    prompt,
                    request.model_info,
                    &startup.telemetry,
                    request.effort.clone(),
                    request.summary,
                    request.service_tier.clone(),
                    &metadata,
                )
                .await?;
        }
        Ok(startup.client_session)
    }

    /// Opens a turn on `client`, or on the session a prewarm left; tool-result follow-ups
    /// reuse the turn, a new user input does not.
    pub async fn start_turn(
        self: &Arc<Self>,
        client: &ModelClient,
        prewarmed: Option<ModelClientSession>,
        cwd: AbsolutePathBuf,
        model_info: &ModelInfo,
    ) -> ExternalTurn {
        let turn_id = Uuid::now_v7().to_string();
        let session = prewarmed.unwrap_or_else(|| client.new_session());
        let turn = self.turn(session, turn_id.clone(), cwd, model_info).await;
        turn.metadata.set_root_turn_id(turn_id);
        turn.metadata
            .set_turn_started_at_unix_ms(crate::turn_timing::now_unix_timestamp_ms());
        if matches!(self.thread_source, ThreadSource::User) {
            turn.metadata
                .set_turn_trigger(USER_TURN_TRIGGER.to_string());
        }
        turn.metadata
            .spawn_git_enrichment_task(Arc::clone(&self.git_root_discovery));
        turn
    }

    async fn turn(
        self: &Arc<Self>,
        client_session: ModelClientSession,
        turn_id: String,
        cwd: AbsolutePathBuf,
        model_info: &ModelInfo,
    ) -> ExternalTurn {
        let config = self.config.as_ref();
        let permission_profile = config.permissions.effective_permission_profile();
        let auto_review_enabled = crate::guardian::routes_approval_policy_to_guardian(
            config.permissions.approval_policy.value(),
            config.approvals_reviewer,
        );
        let windows_sandbox_selection = configured_windows_sandbox_selection(
            config.effective_local_windows_sandbox_type(),
            WindowsSandboxLevel::from_config(config),
            &PathUri::from_abs_path(&cwd),
        );
        let (forked_from, parent_thread_id) = match self.role {
            ExternalThreadRole::Root => (None, None),
            ExternalThreadRole::Fork { forked_from, .. } => (Some(forked_from), None),
            ExternalThreadRole::Subagent {
                parent_thread_id, ..
            } => (None, Some(parent_thread_id)),
        };
        let metadata = Arc::new(TurnMetadataState::new(
            self.session_id(),
            self.thread_id.to_string(),
            forked_from,
            parent_thread_id,
            &self.session_source,
            Some(self.thread_source.clone()),
            turn_id.clone(),
            cwd,
            &permission_profile,
            windows_sandbox_selection,
            /*enforce_managed_network*/ false,
            auto_review_enabled,
            model_info,
        ));
        metadata.set_responses_api_metadata(config.responses_api_metadata.clone());
        let auth = self.auth_manager.auth().await;
        let telemetry = SessionTelemetry::new(
            self.thread_id,
            model_info.slug.as_str(),
            model_info.slug.as_str(),
            auth.as_ref().and_then(CodexAuth::get_account_id),
            auth.as_ref().and_then(CodexAuth::get_account_email),
            auth.as_ref()
                .map(CodexAuth::auth_mode)
                .map(TelemetryAuthMode::from),
            self.originator.clone(),
            config.otel.log_user_prompt,
            user_agent(),
            self.session_source.clone(),
        )
        .with_auth_env(
            collect_auth_env_telemetry(
                &config.model_provider,
                self.auth_manager.codex_api_key_env_enabled(),
            )
            .to_otel_metadata(),
        );
        ExternalTurn {
            thread: Arc::clone(self),
            client_session,
            metadata,
            turn_id,
            auto_review_enabled,
            telemetry,
        }
    }

    /// Starts the context window that follows a compaction.
    pub fn advance_context_window(&self) {
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        window.number = window.number.saturating_add(1);
        window.id = Uuid::now_v7();
    }

    fn context_window(&self) -> ContextWindow {
        *self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One Codex turn: a user input and every model request answering it.
pub struct ExternalTurn {
    thread: Arc<ExternalThread>,
    client_session: ModelClientSession,
    metadata: Arc<TurnMetadataState>,
    turn_id: String,
    auto_review_enabled: bool,
    telemetry: SessionTelemetry,
}

/// Retry budget of one logical request, across its resends.
pub struct StreamRetries {
    retries: u64,
    connection_retry_delay: Duration,
}

impl Default for StreamRetries {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
        }
    }
}

impl ExternalTurn {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub async fn stream(
        &mut self,
        prompt: &Prompt,
        request: &ExternalRequest<'_>,
    ) -> Result<ResponseStream> {
        let kind = match request.kind {
            ExternalRequestKind::Turn => CodexResponsesRequestKind::Turn,
            ExternalRequestKind::Compaction {
                trigger,
                reason,
                phase,
            } => CodexResponsesRequestKind::Compaction(CompactionTurnMetadata::new(
                trigger,
                reason,
                CompactionImplementation::ResponsesCompactionV2,
                phase,
            )),
        };
        let metadata = self.responses_metadata(request, kind);
        self.client_session
            .stream(
                prompt,
                request.model_info,
                &self.telemetry,
                request.effort.clone(),
                request.summary,
                request.service_tier.clone(),
                &metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
    }

    /// Codex's response-stream retry policy: `Ok` after the backoff means resend, `Err` is final.
    pub async fn recover(
        &mut self,
        err: CodexErr,
        request: &ExternalRequest<'_>,
        budget: &mut StreamRetries,
    ) -> Result<()> {
        let is_turn = matches!(request.kind, ExternalRequestKind::Turn);
        let provider = &self.thread.config.model_provider;
        let max_retries = if is_turn {
            provider.stream_max_retries()
        } else {
            provider
                .stream_max_retries()
                .min(MAX_COMPACTION_STREAM_RETRIES)
        };
        let retry_count = budget.retries.saturating_add(1);
        let Some(delay) = err.retry_delay(retry_count) else {
            return Err(err);
        };
        if self
            .thread
            .config
            .features
            .enabled(Feature::UnboundedConnectionRetries)
            && is_turn
            && matches!(err.details(), CodexErrorDetails::ConnectionFailed(_))
            && !self.thread.session_source.is_internal()
            && !provider.is_amazon_bedrock()
        {
            let retry_delay = budget.connection_retry_delay;
            warn!(turn_id = %self.turn_id, error = %err, ?retry_delay, "stream connection failed; waiting to retry");
            tokio::time::sleep(retry_delay).await;
            budget.connection_retry_delay = retry_delay
                .saturating_mul(2)
                .min(MAX_CONNECTION_RETRY_DELAY);
            return Ok(());
        }
        if budget.retries >= max_retries
            && self
                .client_session
                .try_switch_fallback_transport(&self.telemetry, request.model_info)
        {
            warn!("Falling back from WebSockets to HTTPS transport. {err:#}");
            budget.retries = 0;
            return Ok(());
        }
        if budget.retries < max_retries {
            budget.retries = retry_count;
            warn!(turn_id = %self.turn_id, retry_count, max_retries, error = %err, "stream disconnected - retrying request");
            let now = Instant::now();
            let retry_at = err
                .retry_after()
                .map(RetryAfter::deadline)
                .unwrap_or(now + delay);
            tokio::time::sleep_until(retry_at).await;
            return Ok(());
        }
        Err(err)
    }

    /// The `x-codex-turn-metadata` value a tool call of this turn sends with its own requests.
    pub fn tool_call_metadata(&self, request: &ExternalRequest<'_>) -> Option<String> {
        self.metadata
            .current_meta_value_for_mcp_request(self.execution_metadata(request))
            .and_then(|metadata| to_ascii_json_string(&metadata).ok())
    }

    fn execution_metadata<'a>(&self, request: &ExternalRequest<'a>) -> ExecutionMetadata<'a> {
        ExecutionMetadata {
            model: request.model_info.slug.as_str(),
            reasoning_effort: request
                .effort
                .clone()
                .or_else(|| request.model_info.default_reasoning_level.clone()),
            node_repl_disabled: request.model_info.node_repl_disabled,
            auto_review_enabled: self.auto_review_enabled,
            node_repl_auto_review_required: request.model_info.computer_use_review_required(),
        }
    }

    fn responses_metadata(
        &self,
        request: &ExternalRequest<'_>,
        kind: CodexResponsesRequestKind,
    ) -> CodexResponsesMetadata {
        let thread = self.thread.as_ref();
        let window = thread.context_window();
        let mut metadata = self.metadata.to_responses_metadata(
            thread.installation_id.clone(),
            format!("{}:{}", thread.thread_id, window.number),
            kind,
        );
        self.execution_metadata(request).apply_to(&mut metadata);
        CodexResponsesMetadata {
            window_number: Some(window.number),
            context_window_id: Some(window.id),
            mcp_attribution: Some(McpAttribution::default()),
            analytics_enabled: Some(thread.config.analytics_enabled != Some(false)),
            history_ingest_requested: thread
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                .then_some(true),
            ..metadata
        }
    }
}

/// The tier a turn requests as the interactive client resolves it: `requested`, else the
/// configured tier, else the model's default tier while that tier's feature is enabled.
pub fn turn_service_tier(
    config: &Config,
    requested: Option<String>,
    model_info: &ModelInfo,
) -> Option<String> {
    let configured = requested
        .or_else(|| config.service_tier.clone())
        .or_else(|| {
            (config.notices.fast_default_opt_out == Some(true))
                .then(|| SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())
        })
        .or_else(|| {
            model_info.default_service_tier.clone().filter(|tier| {
                config.features.service_tier_enabled(tier) && model_info.supports_service_tier(tier)
            })
        });
    crate::session::get_service_tier(configured, &config.features, model_info)
}

/// The web search a Codex turn offers.
pub enum TurnWebSearch {
    /// Client-executed `web.run`.
    Standalone,
    Hosted(Box<ToolSpec>),
}

/// Codex's choice between `web.run` and the hosted tool; `standalone_available` = the
/// standalone search extension serves this config.
pub fn turn_web_search(
    config: &Config,
    model_info: &ModelInfo,
    standalone_available: bool,
) -> Option<TurnWebSearch> {
    let provider = create_model_provider(config.model_provider.clone(), /*auth_manager*/ None);
    let capabilities = provider.capabilities();
    let web_search_mode = config.web_search_mode.value();
    if standalone_available
        && web_search_mode != WebSearchMode::Disabled
        && capabilities.web_search
        && (model_info.use_responses_lite || config.features.enabled(Feature::StandaloneWebSearch))
    {
        return Some(TurnWebSearch::Standalone);
    }
    if model_info.use_responses_lite || !capabilities.web_search {
        return None;
    }
    create_web_search_tool(WebSearchToolOptions {
        web_search_mode: Some(web_search_mode),
        web_search_config: config.web_search_config.as_ref(),
        web_search_tool_type: model_info.web_search_tool_type,
    })
    .map(|spec| TurnWebSearch::Hosted(Box::new(spec)))
}

/// Codex's client-executed `tool_search` over deferred tools.
pub struct ExternalToolSearch {
    handler: crate::tools::handlers::ToolSearchHandler,
}

impl ExternalToolSearch {
    /// Deferred functions and namespaces; other specs are not searchable.
    pub fn new(deferred: Vec<ToolSpec>) -> Self {
        let search_infos = deferred
            .into_iter()
            .filter_map(|spec| codex_tools::ToolSearchInfo::from_tool_spec(spec, None))
            .collect();
        Self {
            handler: crate::tools::handlers::ToolSearchHandler::new(
                search_infos,
                crate::tools::handlers::tool_search_spec::ToolSearchSourceListing::Omit,
            ),
        }
    }

    pub fn spec(&self) -> ToolSpec {
        use crate::tools::registry::ToolExecutor;
        self.handler.spec()
    }

    /// A `tool_search_call`'s `arguments`; `Err` is the message the model receives.
    pub fn search(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<Vec<LoadableToolSpec>, String> {
        let params: codex_protocol::models::SearchToolCallParams =
            serde_json::from_value(arguments)
                .map_err(|err| format!("failed to parse tool_search arguments: {err}"))?;
        self.handler
            .search_arguments(&params)
            .map_err(|err| err.to_string())
    }

    /// The specs a search returning `tools` (`(namespace, name)`) produced, for replay.
    pub fn load(&self, tools: &[(String, String)]) -> Vec<LoadableToolSpec> {
        let mut loaded = Vec::new();
        for (namespace, name) in tools {
            loaded.extend(self.handler.load(|spec| {
                loadable_names(spec)
                    .iter()
                    .any(|(candidate_namespace, candidate)| {
                        candidate_namespace == namespace && candidate == name
                    })
            }));
        }
        codex_tools::coalesce_loadable_tool_specs(loaded)
    }
}

/// `(namespace, name)` of each tool a loadable spec carries.
pub fn loadable_names(spec: &LoadableToolSpec) -> Vec<(String, String)> {
    match spec {
        LoadableToolSpec::Function(tool) => vec![(
            codex_protocol::DEFAULT_FUNCTION_NAMESPACE.to_string(),
            tool.name.clone(),
        )],
        LoadableToolSpec::Namespace(namespace) => namespace
            .tools
            .iter()
            .map(|tool| {
                let name = match tool {
                    codex_tools::ResponsesApiNamespaceTool::Function(tool) => tool.name.clone(),
                    codex_tools::ResponsesApiNamespaceTool::Custom(tool) => tool.name.clone(),
                };
                (namespace.name.clone(), name)
            })
            .collect(),
    }
}

/// Stamps a conversation item entering history as a Codex session does.
pub fn record_history_item(item: &mut ResponseItem, turn_id: &str) {
    Session::stamp_response_item_for_history(item, turn_id);
    Session::assign_missing_response_item_id(item);
}

/// Retained user messages of a remote compaction's replacement history, newest kept first.
pub fn retain_messages_for_compaction(messages: Vec<ResponseItem>) -> Vec<ResponseItem> {
    truncate_retained_messages_for_remote_compaction(
        messages
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect(),
        RETAINED_MESSAGE_TOKEN_BUDGET,
    )
    .into_iter()
    .map(|envelope| envelope.item)
    .collect()
}
