//! One Messages API request answered by a Codex turn on a lane.

use std::sync::Arc;
use std::time::Instant;

use codex_core::Prompt;
use codex_core::ResponseControls;
use codex_core::ResponseEvent;
use codex_core::ToolChoice as CodexToolChoice;
use codex_core::external_harness::ExternalRequest;
use codex_core::external_harness::ExternalRequestKind;
use codex_core::external_harness::ExternalToolSearch;
use codex_core::external_harness::ExternalTurn;
use codex_core::external_harness::StreamRetries;
use codex_core::external_harness::TurnWebSearch;
use codex_core::external_harness::estimate_history_tokens;
use codex_core::external_harness::estimate_text_tokens;
use codex_core::external_harness::record_history_item;
use codex_core::external_harness::turn_web_search;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_web_search_extension::RUN_TOOL_NAME;
use codex_web_search_extension::WEB_NAMESPACE;
use codex_web_search_extension::web_run_spec;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::future::join_all;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

use crate::advisor;
use crate::advisor::Transcript;
use crate::agent_summary;
use crate::align::align;
use crate::anthropic::ContentBlock;
use crate::anthropic::InputMessage;
use crate::anthropic::MessageContent;
use crate::anthropic::MessagesRequest;
use crate::anthropic::OutputFormat;
use crate::anthropic::SystemBlock;
use crate::anthropic::SystemPrompt;
use crate::anthropic::ToolChoice;
use crate::auto_review;
use crate::backend::Backend;
use crate::backend::Probe;
use crate::backend::SentPrompt;
use crate::backend::Thread;
use crate::compaction;
use crate::compactions;
use crate::context_edits;
use crate::error::ApiError;
use crate::history;
use crate::history::Conversation;
use crate::mcp_connector::LegacyToolConfiguration;
use crate::server_tools::ServerCall;
use crate::server_tools::ServerResult;
use crate::server_tools::ToolSearchIndex;
use crate::server_tools::mcp_result;
use crate::server_tools::tool_search_result;
use crate::settings;
use crate::settings::RequestSettings;
use crate::stream::Disconnected;
use crate::stream::Flow;
use crate::stream::MessageStream;
use crate::tokens::ContextUsage;
use crate::tools::McpToolset;
use crate::tools::PinnedTool;
use crate::tools::ToolConfig;
use crate::tools::ToolSet;
use crate::web_run;
use crate::web_run::SearchContext;
use crate::web_run::WEB_FETCH;
use crate::web_run::WEB_SEARCH;

/// Claude Code's per-request attribution; Anthropic strips it before inference.
pub(crate) const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";
const WORKING_DIRECTORY_MARKER: &str = "Primary working directory: ";

/// Which client conversation a request continues.
#[derive(Clone)]
pub(crate) struct Identity {
    pub(crate) session_id: String,
    pub(crate) agent_id: Option<String>,
    /// The agent that spawned `agent_id`; absent = the session's main agent.
    pub(crate) parent_agent_id: Option<String>,
}

/// The request-wide parts of every prompt a turn sends.
pub(crate) struct PromptParts {
    pub(crate) model: String,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) instructions: BaseInstructions,
    pub(crate) output_schema: Option<Value>,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) native_tools: Option<Vec<ToolSpec>>,
    /// `tool_choice` of the request's first sample; server tool follow-ups choose freely.
    pub(crate) forced: Option<ForcedTool>,
}

pub(crate) struct ForcedTool {
    tool_choice: CodexToolChoice,
    /// A namespaced tool cannot be named in `tool_choice`; its namespace alone is offered.
    tools: Option<Vec<ToolSpec>>,
}

impl PromptParts {
    /// Codex defaults: compaction and other requests outside the client's sampling.
    pub(crate) fn prompt(&self, input: Vec<ResponseItem>) -> Prompt {
        Prompt::new(
            input,
            self.tools.clone(),
            self.instructions.clone(),
            self.output_schema.clone(),
            self.parallel_tool_calls,
        )
    }

    /// A sample of the client's message.
    pub(crate) fn sample_prompt(&self, input: Vec<ResponseItem>, first: bool) -> Prompt {
        let forced = self.forced.as_ref().filter(|_| first);
        let tools = forced
            .and_then(|forced| forced.tools.clone())
            .unwrap_or_else(|| self.tools.clone());
        Prompt::new(
            input,
            tools,
            self.instructions.clone(),
            self.output_schema.clone(),
            self.parallel_tool_calls,
        )
        .with_controls(ResponseControls {
            tool_choice: forced
                .map(|forced| forced.tool_choice.clone())
                .unwrap_or_default(),
            ..ResponseControls::default()
        })
    }

    /// The `n > limit` figure clients read from an overflow error.
    pub(crate) fn approx_tokens(&self, input: &[ResponseItem]) -> u64 {
        let tools = serde_json::to_string(&self.tools).unwrap_or_default();
        let tokens = estimate_history_tokens(input, &self.model)
            .saturating_add(estimate_text_tokens(&tools, &self.model))
            .saturating_add(estimate_text_tokens(&self.instructions.text, &self.model));
        u64::try_from(tokens).unwrap_or(u64::MAX)
    }
}

/// A request translated for Codex, before any conversation state is touched.
pub(crate) struct Prepared {
    model: String,
    model_info: ModelInfo,
    settings: RequestSettings,
    tools: ToolSet,
    parts: PromptParts,
    conversation: Conversation,
    /// The label answering an agent summary, which never reaches the backend.
    agent_summary: Option<String>,
    applied_edits: Vec<Value>,
    stop_sequences: Vec<String>,
    cwd: AbsolutePathBuf,
    fetchable: String,
    /// `mcp_tool_listing` blocks the message opens with.
    mcp_listings: Vec<Value>,
    citations: Vec<(String, Value)>,
    /// Claude Code auto mode asks for its server-side check of the response's tool calls.
    server_review: bool,
}

/// `anthropic-beta` that records MCP tool lists in `mcp_tool_listing` blocks.
const MCP_TOOL_LISTING_BETA: &str = "mcp-client-2026-09-15";

pub(crate) async fn prepare(
    backend: &Backend,
    mut request: MessagesRequest,
    betas: &[String],
) -> Result<Prepared, ApiError> {
    if request
        .container
        .as_ref()
        .is_some_and(|value| !value.is_null())
    {
        return Err(ApiError::invalid_request(
            "container: code execution containers are not supported by the Codex backend",
        ));
    }
    let server_review = auto_review::server_review(&request, backend)?;
    let title = crate::session_title::TitleRequest::parse(&request);
    let model_info = backend.model_info(&request.model).await?;
    let settings = settings::resolve(&request, &model_info, &backend.config)?;
    let plan = crate::lifecycle::apply(
        &mut request,
        betas.iter().any(|beta| {
            beta == "inline-tools-2026-09-15" || beta == "mid-conversation-tool-changes-2026-07-01"
        }),
    )?;
    let mut tools = ToolSet::from_request(&request.tools)?;
    let mcp_listings = connect_mcp_servers(
        backend,
        &request,
        &mut tools,
        betas.iter().any(|beta| beta == MCP_TOOL_LISTING_BETA),
    )
    .await?;
    let web_search = turn_web_search(&backend.config, &model_info, backend.web_search.is_some());
    let mut active = tools.subset(&plan.active)?;
    let mut initial = tools.subset(&plan.initial)?;
    // Tool-less and forced-tool requests are the client's helper calls.
    if request.native_web
        && !plan.active.is_empty()
        && matches!(request.tool_choice, None | Some(ToolChoice::Auto { .. }))
    {
        active.offer_web(&web_search);
        initial.offer_web(&web_search);
    }
    tools.limit_to(&active);
    if active.tool_search.is_some() {
        let index = ExternalToolSearch::new(active.deferred.clone());
        tools.tool_search_index = Some(ToolSearchIndex(index));
    }
    let parallel_tool_calls = match &request.tool_choice {
        None | Some(ToolChoice::None) => true,
        Some(
            ToolChoice::Auto {
                disable_parallel_tool_use,
            }
            | ToolChoice::Any {
                disable_parallel_tool_use,
            }
            | ToolChoice::Tool {
                disable_parallel_tool_use,
                ..
            },
        ) => !disable_parallel_tool_use,
    };
    let specs = inference_specs(&active, &web_search, &model_info.slug)?;
    let mut sampling_specs = if model_info.use_responses_lite {
        inference_specs(&initial, &web_search, &model_info.slug)?
    } else {
        specs.clone()
    };
    tools.web_run = matches!(web_search, Some(TurnWebSearch::Standalone));
    crate::lifecycle::render(&mut request.messages, &tools, model_info.use_responses_lite)?;
    let mut forced = match &request.tool_choice {
        None | Some(ToolChoice::Auto { .. }) => None,
        Some(ToolChoice::None) => Some(ForcedTool {
            tool_choice: CodexToolChoice::None,
            tools: None,
        }),
        Some(ToolChoice::Any { .. }) => Some(ForcedTool {
            tool_choice: CodexToolChoice::Required,
            tools: None,
        }),
        Some(ToolChoice::Tool { name, .. }) => Some(forced_tool(name, &tools, &specs)?),
    };
    let native_tools = if model_info.use_responses_lite
        && parallel_tool_calls
        && !specs.is_empty()
        && matches!(
            request.tool_choice,
            None | Some(ToolChoice::Auto { .. } | ToolChoice::Any { .. })
        ) {
        let code_mode = |tools: &[ToolSpec]| {
            codex_core::external_harness::code_mode_tool(
                tools,
                &active.deferred,
                &backend.config,
                &model_info,
            )
        };
        sampling_specs = vec![
            code_mode(&sampling_specs),
            codex_core::external_harness::code_mode_wait_tool(&model_info),
        ];
        if let Some(forced) = &mut forced {
            forced.tool_choice = CodexToolChoice::Required;
            if let Some(tools) = forced.tools.take() {
                forced.tools = Some(vec![code_mode(&tools)]);
            }
        }
        Some(specs.iter().chain(&active.deferred).cloned().collect())
    } else {
        None
    };
    let cwd = working_directory(&request).unwrap_or_else(|| backend.config.cwd.clone());
    let (mut instructions, commentary) = match title {
        Some(title) => {
            request.messages[0].content = MessageContent::Text(title.prompt);
            (title.instructions, None)
        }
        None => (
            system_text(request.system),
            model_info
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_template.as_deref())
                .and_then(commentary_section),
        ),
    };
    let instruction_tools = if model_info.use_responses_lite {
        &initial
    } else {
        &active
    };
    for addition in instruction_tools
        .instructions
        .iter()
        .map(|addition| &**addition)
        .chain(commentary)
    {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(addition);
    }
    let parts = PromptParts {
        model: model_info.slug.clone(),
        tools: sampling_specs,
        instructions: BaseInstructions {
            text: instructions,
            provenance: None,
        },
        output_schema: request
            .output_config
            .and_then(|output| output.format)
            .map(|OutputFormat::JsonSchema { schema }| schema),
        parallel_tool_calls,
        native_tools,
        forced,
    };
    let fetchable = web_run::fetchable_text(&request.messages);
    let citations = web_run::echoed_citations(&request.messages);
    let stored = compactions::resolve(&backend.config.codex_home, &request.messages).await?;
    let mut conversation = history::conversation_items(request.messages, &tools, &stored)?;
    let applied_edits = request
        .context_management
        .map(|management| context_edits::apply(&management.edits, &mut conversation, &tools))
        .unwrap_or_default();
    let agent_summary = agent_summary::answer(&conversation.items);
    Ok(Prepared {
        model: request.model,
        model_info,
        settings,
        tools,
        parts,
        conversation,
        agent_summary,
        applied_edits,
        stop_sequences: request.stop_sequences,
        cwd,
        fetchable,
        mcp_listings,
        citations,
        server_review,
    })
}

fn inference_specs(
    tools: &ToolSet,
    web_search: &Option<TurnWebSearch>,
    model: &str,
) -> Result<Vec<ToolSpec>, ApiError> {
    let mut specs = tools.specs.clone();
    if tools.tool_search.is_some() {
        specs.push(ExternalToolSearch::new(tools.deferred.clone()).spec());
    }
    match (web_search, &tools.web_search, &tools.web_fetch) {
        (_, None, None) => {}
        (Some(TurnWebSearch::Standalone), _, _) => specs.push(web_run_spec()),
        (Some(TurnWebSearch::Hosted(hosted)), Some(web_search), None) => {
            specs.push(web_search.apply_hosted(hosted.as_ref().clone()));
        }
        (Some(TurnWebSearch::Hosted(_)), _, Some(_)) => {
            return Err(ApiError::invalid_request(format!(
                "tools: web_fetch needs Codex standalone search, which `{}` does not use",
                model
            )));
        }
        (None, _, _) => {
            return Err(ApiError::invalid_request(format!(
                "tools: web search is unavailable for `{}` under the Codex config",
                model
            )));
        }
    }
    Ok(specs)
}

/// The prompt's size as Codex estimates it.
pub(crate) fn count_tokens(prepared: &Prepared) -> u64 {
    prepared.parts.approx_tokens(&prepared.conversation.items)
}

/// Answers a prepared request. Errors before the first event become the HTTP response;
/// later ones are sent as an `error` event.
pub(crate) async fn respond(
    backend: Arc<Backend>,
    identity: Identity,
    prepared: Prepared,
    tx: mpsc::Sender<Value>,
) -> Result<(), ApiError> {
    let Prepared {
        model,
        model_info,
        settings,
        tools,
        parts,
        conversation,
        agent_summary,
        applied_edits,
        stop_sequences,
        cwd,
        fetchable,
        mcp_listings,
        citations,
        server_review,
    } = prepared;
    if let Some(label) = agent_summary {
        let mut stream = MessageStream::new(
            &tx,
            &tools,
            model,
            String::new(),
            settings.display_thinking,
            stop_sequences,
            applied_edits,
        );
        let _ = stream.summary(&label, /*usage*/ None).await;
        return Ok(());
    }
    let probe = Probe {
        fingerprint: fingerprint(&format!("{model}\n{}", parts.instructions.text)),
        items: &conversation.items,
    };
    let (thread, mut lane) = backend.lane(&identity, &probe).await;
    let lane = &mut *lane;
    let (mut input, matched) = align(&lane.history, conversation.items);
    let continues = lane.continued_by(matched, &input);
    if !continues && matched < lane.history.len() {
        lane.retain_prefix(&input[..matched]);
    }
    debug!(
        session = %identity.session_id,
        agent = identity.agent_id.as_deref().unwrap_or("root"),
        parent = identity.parent_agent_id.as_deref(),
        tools = serde_json::to_vec(&parts.tools)
            .ok()
            .map(|tools| crate::persist::fnv1a(&tools)),
        recorded = lane.history.len(),
        matched,
        new_items = input.len() - matched,
        continues,
        service_tier = settings.service_tier.as_deref(),
        "lane request"
    );
    lane.resume_turn = false;
    lane.pending_calls.clear();
    let mut external = ExternalRequest {
        model_info: &model_info,
        effort: settings::request_effort(
            &mut input,
            settings.effort.clone(),
            &model_info,
            thread.external.effort_updates_enabled(&model_info),
        ),
        summary: settings.summary,
        service_tier: settings.service_tier.clone(),
        kind: ExternalRequestKind::Turn,
    };
    let turn = match lane.turn.take().filter(|_| continues) {
        Some(turn) => turn,
        None => {
            let prewarmed = if lane.prewarmed {
                None
            } else {
                lane.prewarmed = true;
                // The backend rejects a prewarm that forces a tool.
                let warmup = parts.sample_prompt(Vec::new(), /*first*/ false);
                thread
                    .external
                    .prewarm(&lane.client, cwd.clone(), &warmup, &external)
                    .await
                    .inspect_err(|err| warn!(error = %err, "websocket prewarm failed"))
                    .ok()
            };
            thread
                .external
                .start_turn(&lane.client, prewarmed, cwd.clone(), &model_info)
                .await
        }
    };
    let turn = lane.turn.insert(turn);
    for item in &mut input[matched..] {
        record_history_item(item, turn.turn_id());
    }

    let mut context_usage = lane
        .prompt
        .as_ref()
        .and_then(|prompt| prompt.context)
        .filter(|context| matched >= context.visible_items);
    let projected = lane.executor.project(&input);
    let input_estimate = match (context_usage, &lane.prompt) {
        (Some(context), _) => context.estimate(&projected, &parts.model),
        (None, Some(prompt)) if matched >= prompt.items => {
            let covered = lane.executor.project(&input[..prompt.items]).len();
            prompt
                .input_tokens
                .saturating_add(estimate_history_tokens(&projected[covered..], &parts.model))
        }
        _ => i64::try_from(parts.approx_tokens(&projected)).unwrap_or(i64::MAX),
    };
    let sent = Instant::now();
    let previous = lane.prompt.as_ref().and_then(|prompt| {
        prompt
            .sent
            .map(|at| (prompt.input_tokens, sent.duration_since(at)))
    });
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        model,
        turn.turn_id().to_string(),
        settings.display_thinking,
        stop_sequences,
        applied_edits,
    );
    stream.estimate_input(input_estimate);
    stream.lead_with(mcp_listings);
    stream.cite(citations);
    if server_review {
        stream.review_with(auto_review::Reviewer {
            backend: Arc::clone(&backend),
            parent: Arc::clone(&thread.external),
            reviewed: Some(turn.reviewed()),
            cwd: cwd.clone(),
            model: model_info.clone(),
            effort: external.effort.clone(),
        });
    }
    let search_session = thread.external.session_id();
    let mut web = backend
        .web_search
        .as_ref()
        .filter(|_| tools.web_run)
        .map(|search| SearchContext {
            search,
            session_id: &search_session,
            model: &model_info.slug,
            originator: &backend.originator,
            turn_metadata: None,
            fetchable,
            token_budget: u64::try_from(
                TruncationPolicy::from(model_info.truncation_policy).token_budget(),
            )
            .unwrap_or(u64::MAX),
        });
    let server = ServerContext {
        backend: &backend,
        thread: &thread,
        cwd: &cwd,
    };
    let result = match conversation.compaction {
        Some(compaction) => {
            let executor = &mut lane.executor;
            let settled = async {
                if executor.pending() {
                    let frontier = executor.resume(&input).await.map_err(executor_error)?;
                    relay_executor_frontier(
                        frontier,
                        executor,
                        web.as_mut(),
                        &server,
                        &parts,
                        turn,
                        &external,
                        &input,
                        &tools,
                        &mut stream,
                    )
                    .await
                } else {
                    Ok(false)
                }
            }
            .await;
            match settled {
                Err(error) => Err(error),
                Ok(true) => {
                    let outcome = finish_sample(
                        &mut stream,
                        &lane.executor,
                        context_usage.as_ref(),
                        &parts,
                        &input,
                        None,
                    )
                    .await;
                    input.extend(stream.outputs.iter().cloned());
                    lane.history = Arc::new(input);
                    lane.pending_calls = stream.tool_calls().to_vec();
                    thread.record(lane, matched);
                    outcome
                }
                Ok(false) => {
                    let outcome = compaction::run(
                        turn,
                        &parts,
                        lane.executor.project(&input),
                        compaction.mid_turn && continues,
                        &mut external,
                        &mut stream,
                        &backend.config.codex_home,
                    )
                    .await;
                    if let Ok(replacement) = &outcome {
                        thread.external.advance_context_window();
                        thread.record_window();
                        lane.history = Arc::new(replacement.clone());
                        lane.executor.compact();
                        lane.prompt = None;
                        lane.resume_turn = true;
                        thread.record(lane, /*keep*/ 0);
                    }
                    outcome.map(|_| ())
                }
            }
        }
        None => {
            let outcome = sample(
                turn,
                &parts,
                &input,
                &external,
                &mut stream,
                &server,
                &tools,
                web,
                &mut lane.executor,
                &mut context_usage,
            )
            .await;
            debug!(usage = ?stream.usage(), outputs = stream.outputs.len(), "response finished");
            if let Some((input_tokens, cached)) = stream.first_sample() {
                if let Some((previous, since)) = previous
                    && let Some(streak) = lane.cache.judge(previous, since, input_tokens, cached)
                {
                    backend.watchdog.lock_out(format!(
                        "{streak}; session {}, agent {}, lane {}",
                        identity.session_id,
                        identity.agent_id.as_deref().unwrap_or("root"),
                        lane.index()
                    ));
                }
                lane.prompt = Some(SentPrompt {
                    items: input.len(),
                    input_tokens,
                    context: context_usage,
                    sent: Some(sent),
                });
            } else if let Some(prompt) = &mut lane.prompt {
                prompt.context = context_usage;
            }
            input.extend(stream.outputs.iter().cloned());
            lane.history = Arc::new(input);
            lane.pending_calls = stream.tool_calls().to_vec();
            thread.record(lane, matched);
            outcome
        }
    };
    if !lane.resume_turn && lane.pending_calls.is_empty() {
        lane.turn = None;
    }
    match result {
        Ok(()) | Err(Failure::Disconnected) => Ok(()),
        Err(Failure::Codex(err)) => {
            let prompt_tokens = parts.approx_tokens(&lane.history);
            let error =
                ApiError::from_codex(&err, prompt_tokens, model_context_window(&model_info));
            if stream.started() {
                let _ = tx.send(error.body()).await;
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Failure {
    Codex(CodexErr),
    Disconnected,
}

impl From<Disconnected> for Failure {
    fn from(_: Disconnected) -> Self {
        Self::Disconnected
    }
}

impl From<CodexErr> for Failure {
    fn from(err: CodexErr) -> Self {
        Self::Codex(err)
    }
}

/// Samples until the model ends its turn or calls client tools, resending completed output on
/// retries and follow-ups as a Codex turn does. `web.run` calls run here, as Codex runs them.
#[allow(clippy::too_many_arguments)]
async fn sample(
    turn: &mut ExternalTurn,
    parts: &PromptParts,
    input: &[ResponseItem],
    request: &ExternalRequest<'_>,
    stream: &mut MessageStream<'_>,
    server: &ServerContext<'_>,
    tools: &ToolSet,
    mut web: Option<SearchContext<'_>>,
    executor: &mut crate::executor::Executor,
    context_usage: &mut Option<ContextUsage>,
) -> Result<(), Failure> {
    let backend = server.backend;
    let mut retries = StreamRetries::default();
    let mut first = !executor.pending();
    if executor.pending() {
        let frontier = executor.resume(input).await.map_err(executor_error)?;
        if relay_executor_frontier(
            frontier,
            executor,
            web.as_mut(),
            server,
            parts,
            turn,
            request,
            input,
            tools,
            stream,
        )
        .await?
        {
            return finish_sample(stream, executor, context_usage.as_ref(), parts, input, None)
                .await;
        }
    }
    loop {
        let history = executor.project(&[input, stream.outputs.as_slice()].concat());
        let input_items = history.len();
        let mut includes_reasoning = None;
        let mut measured = false;
        let prompt = parts.sample_prompt(history, first);
        let mut executions = Vec::new();
        let outcome = match turn.stream(&prompt, request).await {
            Ok(mut response) => loop {
                let event = tokio::select! {
                    event = response.next() => event,
                    () = stream.closed() => return Err(Failure::Disconnected),
                };
                match event.map(crate::stream::messages_response_event) {
                    Some(Ok(ResponseEvent::ModelsEtag(etag))) => {
                        backend
                            .models
                            .refresh_if_new_etag(etag, backend.config.http_client_factory())
                            .await;
                    }
                    Some(Ok(event)) => {
                        if let ResponseEvent::ServerReasoningIncluded(included) = &event {
                            includes_reasoning = Some(*included);
                        }
                        if parts.native_tools.is_some() {
                            match &event {
                                ResponseEvent::OutputItemAdded(item) if is_executor_call(item) => {
                                    continue;
                                }
                                ResponseEvent::OutputItemDone(item) if is_executor_call(item) => {
                                    executions.push(item.clone());
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        let callable = match &event {
                            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall {
                                namespace,
                                name,
                                ..
                            })
                            | ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                                namespace,
                                name,
                                ..
                            }) => Some((namespace.as_deref(), name.as_str())),
                            ResponseEvent::OutputItemAdded(ResponseItem::ToolSearchCall {
                                ..
                            })
                            | ResponseEvent::OutputItemDone(ResponseItem::ToolSearchCall {
                                ..
                            }) => tools
                                .tool_search
                                .as_ref()
                                .map(|search| (None, search.name.as_str())),
                            ResponseEvent::OutputItemAdded(ResponseItem::WebSearchCall {
                                ..
                            })
                            | ResponseEvent::OutputItemDone(ResponseItem::WebSearchCall {
                                ..
                            }) => Some((Some("web"), "run")),
                            _ => None,
                        };
                        if let Some((namespace, name)) = callable
                            && !tools.is_available(namespace, name)
                        {
                            break Err(CodexErr::Fatal(format!(
                                "model called unavailable tool `{name}`"
                            )));
                        }
                        let usage = match &event {
                            ResponseEvent::Completed { token_usage, .. }
                            | ResponseEvent::Incomplete { token_usage, .. } => token_usage.clone(),
                            _ => None,
                        };
                        let flow = stream.on_event(event).await?;
                        if matches!(flow, Flow::OutputLimit) {
                            stream.finish_output().await?;
                        }
                        if let Some(usage) = usage {
                            let visible = [input, stream.outputs.as_slice()].concat();
                            let covered = executor.project(&visible);
                            measured = true;
                            *context_usage = Some(ContextUsage::measured(
                                &usage,
                                includes_reasoning,
                                input_items,
                                &covered,
                                visible.len(),
                                &parts.model,
                            ));
                        }
                        match flow {
                            Flow::Continue => {}
                            Flow::Completed { follow_up } => break Ok(follow_up),
                            Flow::StopSequence | Flow::OutputLimit => break Ok(false),
                        }
                    }
                    Some(Err(err)) => break Err(err),
                    None => {
                        break Err(CodexErr::Stream(
                            "stream closed before response.completed".to_string(),
                        ));
                    }
                }
            },
            Err(err) => Err(err),
        };
        let outcome = match outcome {
            // A started `tool_use` input cannot be withdrawn; the client resends instead.
            Err(err) if !stream.resumable() => return Err(err.into()),
            Err(err) => {
                if let Some(explanation) = refusal(&err) {
                    return finish_sample(
                        stream,
                        executor,
                        context_usage.as_ref(),
                        parts,
                        input,
                        Some(&explanation),
                    )
                    .await;
                }
                stream.abandon_open_block().await?;
                Err(err)
            }
            Ok(follow_up) => Ok(follow_up),
        };
        // Codex completes a response's tool calls even when its stream failed.
        let mut ran_server_calls = run_server_calls(
            web.as_mut(),
            server,
            parts,
            turn,
            request,
            input,
            tools,
            stream,
        )
        .await?;
        for mut execution in executions {
            record_history_item(&mut execution, turn.turn_id());
            let definitions = codex_tools::collect_code_mode_exec_prompt_tool_definitions(
                parts.native_tools.as_ref().unwrap().iter(),
                backend.config.code_mode.tool_input_schema_max_bytes,
            );
            let visible = [input, stream.outputs.as_slice()].concat();
            let preceding = executor.project(&visible);
            let frontier = executor
                .start(execution, definitions, visible.len())
                .await
                .map_err(executor_error)?;
            if measured && let Some(context) = context_usage.as_mut() {
                context.cover_execution(&preceding, visible.len(), &parts.model);
            }
            if relay_executor_frontier(
                frontier,
                executor,
                web.as_mut(),
                server,
                parts,
                turn,
                request,
                input,
                tools,
                stream,
            )
            .await?
            {
                return finish_sample(stream, executor, context_usage.as_ref(), parts, input, None)
                    .await;
            }
            ran_server_calls = true;
        }
        match outcome {
            Ok(follow_up) => {
                let follow_up = if ran_server_calls {
                    !stream.stopped() && stream.tool_calls().is_empty()
                } else {
                    follow_up
                };
                if !follow_up {
                    return finish_sample(
                        stream,
                        executor,
                        context_usage.as_ref(),
                        parts,
                        input,
                        None,
                    )
                    .await;
                }
                first = false;
                retries = StreamRetries::default();
            }
            Err(err) => turn.recover(err, request, &mut retries).await?,
        }
    }
}

async fn finish_sample(
    stream: &mut MessageStream<'_>,
    executor: &crate::executor::Executor,
    context_usage: Option<&ContextUsage>,
    parts: &PromptParts,
    input: &[ResponseItem],
    refusal: Option<&str>,
) -> Result<(), Failure> {
    stream.finish_output().await?;
    stream.review(input).await?;
    let history = executor.project(&[input, stream.outputs.as_slice()].concat());
    let tokens = context_usage
        .map(|context| context.estimate(&history, &parts.model))
        .unwrap_or_else(|| i64::try_from(parts.approx_tokens(&history)).unwrap_or(i64::MAX));
    stream.context_usage(tokens);
    Ok(stream.finish_usage(refusal).await?)
}

fn executor_error(error: String) -> Failure {
    Failure::Codex(CodexErr::Fatal(error))
}

fn is_executor_call(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::CustomToolCall {
            namespace, name, ..
        } => {
            name == "exec"
                && namespace
                    .as_deref()
                    .is_none_or(|namespace| namespace == codex_protocol::DEFAULT_FUNCTION_NAMESPACE)
        }
        ResponseItem::FunctionCall {
            namespace, name, ..
        } => {
            name == "wait"
                && namespace
                    .as_deref()
                    .is_none_or(|namespace| namespace == codex_protocol::DEFAULT_FUNCTION_NAMESPACE)
        }
        _ => false,
    }
}

/// Runs server-side nested calls until JavaScript reaches a client frontier or finishes.
#[allow(clippy::too_many_arguments)]
async fn relay_executor_frontier(
    mut frontier: Vec<ResponseItem>,
    executor: &mut crate::executor::Executor,
    mut web: Option<&mut SearchContext<'_>>,
    server: &ServerContext<'_>,
    parts: &PromptParts,
    turn: &ExternalTurn,
    request: &ExternalRequest<'_>,
    input: &[ResponseItem],
    tools: &ToolSet,
    stream: &mut MessageStream<'_>,
) -> Result<bool, Failure> {
    loop {
        for call in frontier {
            let (namespace, name) = match &call {
                ResponseItem::FunctionCall {
                    namespace, name, ..
                }
                | ResponseItem::CustomToolCall {
                    namespace, name, ..
                } => (namespace.as_deref(), name.as_str()),
                _ => unreachable!(),
            };
            if !tools.is_available(namespace, name) {
                return Err(executor_error(format!(
                    "script called unavailable tool `{name}`"
                )));
            }
            let call = match call {
                ResponseItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    namespace: None,
                    ..
                } if tools
                    .tool_search
                    .as_ref()
                    .is_some_and(|search| search.name == name) =>
                {
                    ResponseItem::ToolSearchCall {
                        encrypted_content: None,
                        id: None,
                        status: None,
                        call_id: Some(call_id),
                        execution: "client".to_string(),
                        arguments: serde_json::from_str(&arguments)
                            .map_err(|error| executor_error(error.to_string()))?,
                        internal_chat_message_metadata_passthrough: None,
                    }
                }
                call => call,
            };
            stream.on_event(ResponseEvent::OutputItemDone(call)).await?;
        }
        run_server_calls(
            web.as_deref_mut(),
            server,
            parts,
            turn,
            request,
            input,
            tools,
            stream,
        )
        .await?;
        if !stream.tool_calls().is_empty() {
            return Ok(true);
        }
        if !executor.pending() {
            return Ok(false);
        }
        frontier = executor
            .resume(&[input, stream.outputs.as_slice()].concat())
            .await
            .map_err(executor_error)?;
    }
}

/// A server call's result and, for an advisor call, the advisor model's usage.
type CallOutcome = (ServerResult, Option<(String, TokenUsage)>);

/// What server calls reach beyond the request: Codex services and the conversation's thread.
pub(crate) struct ServerContext<'a> {
    pub(crate) backend: &'a Backend,
    pub(crate) thread: &'a Thread,
    pub(crate) cwd: &'a AbsolutePathBuf,
}

/// Runs the response's server calls concurrently and streams their results in call order.
#[allow(clippy::too_many_arguments)]
async fn run_server_calls(
    mut web: Option<&mut SearchContext<'_>>,
    server: &ServerContext<'_>,
    parts: &PromptParts,
    turn: &ExternalTurn,
    request: &ExternalRequest<'_>,
    input: &[ResponseItem],
    tools: &ToolSet,
    stream: &mut MessageStream<'_>,
) -> Result<bool, Failure> {
    let calls = stream.take_server_calls();
    if calls.is_empty() {
        return Ok(false);
    }
    let history = [input, stream.outputs.as_slice()].concat();
    let web_calls = calls
        .iter()
        .filter_map(|call| match call {
            ServerCall::WebRun(call) => Some(call.as_ref()),
            ServerCall::ToolSearch { .. } | ServerCall::Mcp { .. } | ServerCall::Advisor { .. } => {
                None
            }
        })
        .collect::<Vec<_>>();
    let fetchable = web
        .as_deref()
        .map(|web| web.fetchable.clone())
        .unwrap_or_default();
    let mut plans =
        web_run::plan(&web_calls, tools, &mut stream.server_tool_uses, &fetchable).into_iter();
    if let Some(web) = web.as_deref_mut() {
        web.turn_metadata = turn.tool_call_metadata(request);
    }
    let searcher = web.as_deref();
    let backend = server.backend;
    let mut advisor_uses = 0;
    let mut pending: Vec<BoxFuture<'_, CallOutcome>> = Vec::new();
    for call in &calls {
        pending.push(match call {
            ServerCall::WebRun(_) => {
                let (Some(plan), Some(searcher)) = (plans.next(), searcher) else {
                    return Err(Failure::Codex(CodexErr::Fatal(
                        "web.run call without standalone search".to_string(),
                    )));
                };
                let history = &history;
                async move { (searcher.run(&plan, history).await, None) }.boxed()
            }
            ServerCall::Advisor { call_id } => {
                let Some(tool) = &tools.advisor else {
                    return Err(Failure::Codex(CodexErr::Fatal(
                        "advisor call without an advisor tool".to_string(),
                    )));
                };
                advisor_uses += 1;
                if tool.max_uses.is_some_and(|max_uses| {
                    stream.server_tool_uses.advisor + advisor_uses > max_uses
                }) {
                    let advice = advisor::failure(call_id, "max_uses_exceeded");
                    futures::future::ready((advice.result, None)).boxed()
                } else {
                    let history = &history;
                    let transcript = Transcript {
                        instructions: &parts.instructions.text,
                        tools: &parts.tools,
                        history,
                    };
                    async move {
                        let advice = advisor::advise(
                            backend,
                            server.thread,
                            server.cwd.clone(),
                            tool,
                            call_id,
                            transcript,
                        )
                        .await;
                        (
                            advice.result,
                            advice.usage.map(|usage| (tool.model.clone(), usage)),
                        )
                    }
                    .boxed()
                }
            }
            ServerCall::ToolSearch { call_id, arguments } => {
                let Some(index) = &tools.tool_search_index else {
                    return Err(Failure::Codex(CodexErr::Fatal(
                        "tool_search call without deferred tools".to_string(),
                    )));
                };
                let result = tool_search_result(index, tools, call_id, arguments.clone());
                futures::future::ready((result, None)).boxed()
            }
            ServerCall::Mcp {
                call_id,
                server,
                tool,
                arguments,
            } => {
                let Some(server) = tools.mcp_servers.get(server) else {
                    return Err(Failure::Codex(CodexErr::Fatal(format!(
                        "call to unknown MCP server `{server}`"
                    ))));
                };
                async move {
                    (
                        mcp_result(call_id, &backend.mcp.call(server, tool, arguments).await),
                        None,
                    )
                }
                .boxed()
            }
        });
    }
    let results = tokio::select! {
        results = join_all(pending) => results,
        () = stream.closed() => return Err(Failure::Disconnected),
    };
    stream.server_tool_uses.advisor += advisor_uses;
    for ((result, advisor_usage), call) in results.into_iter().zip(&calls) {
        if let Some((model, usage)) = advisor_usage {
            stream.advisor_iteration(model, &usage);
        }
        if result.failed
            && matches!(call, ServerCall::WebRun(call) if call.kind == web_run::ServerToolKind::Search)
        {
            stream.server_tool_uses.failed_search += 1;
        }
        if let Some(web) = web.as_deref_mut() {
            for url in &result.urls {
                web.fetchable.push_str(url);
                web.fetchable.push('\n');
            }
        }
        stream.server_result(result).await?;
    }
    Ok(true)
}

/// Connects the request's MCP servers and adds their toolsets; returns the `mcp_tool_listing`
/// blocks for lists fetched from servers.
async fn connect_mcp_servers(
    backend: &Backend,
    request: &MessagesRequest,
    tools: &mut ToolSet,
    record_listings: bool,
) -> Result<Vec<Value>, ApiError> {
    let mut toolsets = std::mem::take(&mut tools.mcp_toolsets);
    for server in &request.mcp_servers {
        let referenced = toolsets
            .iter()
            .filter(|toolset| toolset.mcp_server_name == server.name)
            .count();
        match (referenced, &server.tool_configuration) {
            (0, Some(legacy)) => toolsets.push(legacy_toolset(&server.name, legacy)),
            (1, _) => {}
            (0, None) => {
                return Err(ApiError::invalid_request(format!(
                    "mcp_servers.{}: no mcp_toolset references this server",
                    server.name
                )));
            }
            _ => {
                return Err(ApiError::invalid_request(format!(
                    "mcp_servers.{}: referenced by more than one mcp_toolset",
                    server.name
                )));
            }
        }
        tools
            .mcp_servers
            .insert(server.name.clone(), server.clone());
    }
    let recorded = recorded_mcp_listings(&request.messages);
    let mut listings = Vec::new();
    for toolset in &mut toolsets {
        let Some(server) = tools.mcp_servers.get(&toolset.mcp_server_name).cloned() else {
            return Err(ApiError::invalid_request(format!(
                "tools: mcp_toolset names unknown MCP server `{}`",
                toolset.mcp_server_name
            )));
        };
        let listed = match (&toolset.tools, recorded.get(&server.name)) {
            (Some(pinned), _) => pinned.clone(),
            (None, Some(recorded)) if record_listings => recorded.clone(),
            (None, _) => {
                let listed = backend.mcp.list_tools(&server).await?;
                if record_listings {
                    listings.push(json!({
                        "type": "mcp_tool_listing",
                        "mcp_server_name": server.name,
                        "tools": listed,
                    }));
                }
                listed
            }
        };
        tools.add_mcp_tools(toolset, listed.clone())?;
        toolset.tools = Some(listed);
    }
    tools.mcp_toolsets = toolsets;
    Ok(listings)
}

/// `mcp-client-2025-04-04` server settings as the toolset they stand for.
fn legacy_toolset(server: &str, legacy: &LegacyToolConfiguration) -> McpToolset {
    let allowed = legacy.allowed_tools.clone();
    McpToolset {
        mcp_server_name: server.to_string(),
        default_config: ToolConfig {
            enabled: Some(legacy.enabled.unwrap_or(true) && allowed.is_none()),
            defer_loading: None,
        },
        configs: allowed
            .into_iter()
            .flatten()
            .map(|tool| {
                (
                    tool,
                    ToolConfig {
                        enabled: Some(legacy.enabled.unwrap_or(true)),
                        defer_loading: None,
                    },
                )
            })
            .collect(),
        tools: None,
    }
}

/// Each server's last `mcp_tool_listing` in the conversation.
fn recorded_mcp_listings(messages: &[InputMessage]) -> HashMap<String, Vec<PinnedTool>> {
    let mut listings = HashMap::new();
    for message in messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::McpToolListing {
                    mcp_server_name,
                    tools,
                } = block
                {
                    listings.insert(mcp_server_name.clone(), tools.clone());
                }
            }
        }
    }
    listings
}

/// `tool_choice: tool` for `name`: a function by name, or its namespace with `required`.
fn forced_tool(name: &str, tools: &ToolSet, specs: &[ToolSpec]) -> Result<ForcedTool, ApiError> {
    let server_tool = [
        (WEB_SEARCH, tools.web_search.is_some()),
        (WEB_FETCH, tools.web_fetch.is_some()),
    ]
    .iter()
    .any(|(tool, enabled)| *enabled && *tool == name);
    // The hosted search tool cannot be named either; it alone is offered.
    if server_tool
        && let Some(hosted) = specs
            .iter()
            .find(|spec| matches!(spec, ToolSpec::WebSearch { .. }))
    {
        return Ok(ForcedTool {
            tool_choice: CodexToolChoice::Required,
            tools: Some(vec![hosted.clone()]),
        });
    }
    let (namespace, callable) = if server_tool {
        (Some(WEB_NAMESPACE.to_string()), RUN_TOOL_NAME.to_string())
    } else {
        tools.callable(name)
    };
    let Some(namespace) = namespace else {
        let known = specs
            .iter()
            .any(|spec| matches!(spec, ToolSpec::Function(function) if function.name == callable));
        if !known {
            return Err(ApiError::invalid_request(format!(
                "tool_choice.name: no tool named `{name}`"
            )));
        }
        return Ok(ForcedTool {
            tool_choice: CodexToolChoice::Function { name: callable },
            tools: None,
        });
    };
    let narrowed = specs.iter().find_map(|spec| match spec {
        ToolSpec::Namespace(candidate) if candidate.name == namespace => {
            let mut candidate = candidate.clone();
            candidate.tools.retain(|tool| match tool {
                ResponsesApiNamespaceTool::Function(function) => function.name == callable,
                ResponsesApiNamespaceTool::Custom(custom) => custom.name == callable,
            });
            (!candidate.tools.is_empty()).then_some(ToolSpec::Namespace(candidate))
        }
        _ => None,
    });
    let Some(narrowed) = narrowed else {
        return Err(ApiError::invalid_request(format!(
            "tool_choice.name: no tool named `{name}`"
        )));
    };
    Ok(ForcedTool {
        tool_choice: CodexToolChoice::Required,
        tools: Some(vec![narrowed]),
    })
}

/// Policy declines answer as refusals, as the Messages API reports them.
fn refusal(err: &CodexErr) -> Option<String> {
    match err.details() {
        CodexErrorDetails::CyberPolicy { message }
        | CodexErrorDetails::BioPolicy { message }
        | CodexErrorDetails::MisalignmentPolicyViolation { message, .. } => Some(message.clone()),
        _ => None,
    }
}

pub(crate) fn model_context_window(model_info: &ModelInfo) -> u64 {
    model_info
        .context_window
        .and_then(|window| u64::try_from(window).ok())
        .unwrap_or_default()
}

fn system_text(system: Option<SystemPrompt>) -> String {
    match system {
        None => String::new(),
        Some(SystemPrompt::Text(text)) => text,
        Some(SystemPrompt::Blocks(blocks)) => blocks
            .into_iter()
            .map(|SystemBlock::Text { text }| text)
            .filter(|text| !text.starts_with(BILLING_HEADER_PREFIX))
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// The client's working directory as its environment notes report it.
fn working_directory(request: &MessagesRequest) -> Option<AbsolutePathBuf> {
    let system = match &request.system {
        Some(SystemPrompt::Text(text)) => vec![text.as_str()],
        Some(SystemPrompt::Blocks(blocks)) => blocks
            .iter()
            .map(|SystemBlock::Text { text }| text.as_str())
            .collect(),
        None => Vec::new(),
    };
    let messages = request
        .messages
        .iter()
        .flat_map(|message| match &message.content {
            MessageContent::Text(text) => vec![text.as_str()],
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    crate::anthropic::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        });
    system.into_iter().chain(messages).find_map(|text| {
        let start = text.find(WORKING_DIRECTORY_MARKER)? + WORKING_DIRECTORY_MARKER.len();
        let line = text[start..].lines().next()?.trim();
        AbsolutePathBuf::try_from(line).ok()
    })
}

const COMMENTARY_HEADING: &str = "## Intermediate commentary";

/// The catalog template's commentary section, through the next `#`/`##` heading. Client system
/// prompts lack it => commentary 0.2 per final answer (Codex 3.6), samples reasoning silently
/// past an hour.
fn commentary_section(template: &str) -> Option<&str> {
    let section = &template[template.find(COMMENTARY_HEADING)?..];
    let body = &section[COMMENTARY_HEADING.len()..];
    let end = ["\n# ", "\n## "]
        .iter()
        .filter_map(|heading| body.find(heading))
        .min()
        .map_or(section.len(), |end| COMMENTARY_HEADING.len() + end);
    Some(section[..end].trim_end())
}

fn fingerprint(instructions: &str) -> u64 {
    crate::persist::fnv1a(instructions.as_bytes())
}

#[cfg(test)]
#[path = "respond_tests.rs"]
mod tests;
