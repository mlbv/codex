//! Claude Code auto mode's safety checks answered by Codex's Guardian reviewer: Codex's policy,
//! context composition and review model, as a stateless, tool-less Guardian sub-agent turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;

use codex_core::ModelClient;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::config::Config;
use codex_core::context::ContextualUserFragment;
use codex_core::context::GuardianBudgetOmission;
use codex_core::external_harness::ExternalRequest;
use codex_core::external_harness::ExternalRequestKind;
use codex_core::external_harness::ExternalThread;
use codex_core::external_harness::ExternalTurn;
use codex_core::external_harness::ReviewedTurn;
use codex_core::external_harness::StreamRetries;
use codex_guardian_context::ContextProfile;
use codex_guardian_context::HistoryTruncation;
use codex_guardian_context::RequestBudget;
use codex_guardian_context::SectionInput;
use codex_guardian_context::TranscriptMode;
use codex_guardian_context::default_registry;
use codex_guardian_context::effective_input_token_limit;
use codex_guardian_context::estimate_input_tokens;
use codex_guardian_reviewer::GuardianApprovalRequest;
use codex_guardian_reviewer::GuardianAssessment;
use codex_guardian_reviewer::REVIEW_TIMEOUT;
use codex_guardian_reviewer::build_guardian_planned_action;
use codex_guardian_reviewer::compose_guardian_prompt;
use codex_guardian_reviewer::guardian_output_schema;
use codex_guardian_reviewer::parse_guardian_assessment;
use codex_guardian_reviewer::reviewer_instructions;
use codex_guardian_reviewer::select_review_model;
use codex_models_manager::ModelOrigin;
use codex_models_manager::manager::RefreshStrategy;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::CatalogModel;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::GuardianAssessmentOutcome;
use codex_protocol::protocol::GuardianRiskLevel;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::future::join_all;
use serde_json::Value;
use serde_json::json;

use crate::anthropic::MessagesRequest;
use crate::backend::Backend;
use crate::error::ApiError;
use crate::history::CLIENT_CONTEXT_PREFIX;
use crate::history::without_client_context;
use crate::tools::ToolSet;

/// The `safeguards` entry asking for auto mode's check; `safeguard_results` answers it.
pub(crate) const SERVER_REVIEW: &str = "dangerous_tool_use";

/// Claude Code 2.1.319's unconditional auto-mode allowlist: it allows these without reading a
/// verdict, and refuses any other call answered `skipped` as unchecked.
const ALLOWLISTED_TOOLS: [&str; 20] = [
    "Read",
    "Grep",
    "Glob",
    "LSP",
    "ToolSearch",
    "ListMcpResourcesTool",
    "RefreshMcpTools",
    "WaitForMcpServers",
    "ReportFindings",
    "TodoWrite",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "TaskStop",
    "GetTask",
    "ConnectGitHub",
    "EnterPlanMode",
    "ExitPlanMode",
    "ShowOnboardingRolePicker",
];

/// Codex's reviewer margin for item IDs and timestamps.
const INPUT_TOKEN_MARGIN: usize = 256;
const REVIEW_RETENTION: Duration = Duration::from_secs(10 * 60);
const REVIEWER_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

const CLAUDE_MD_SECTION: &str = "\n# claudeMd\n";
/// What Claude Code 2.1.319 generates after `claudeMd` in its context reminder: its own
/// sections, whose generated values open distinctively, then its closing text. Each is the
/// last occurrence, so headings in the user's CLAUDE.md never end it.
const CLAUDE_MD_ENDS: [&str; 4] = [
    "\n# userEmail\nThe user's email address is ",
    "\n# currentDate\nToday's date is ",
    "\n\n      Claude Code attached this context automatically",
    "\n</system-reminder>",
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Verdict {
    Allow,
    Deny {
        risk: GuardianRiskLevel,
        rationale: String,
    },
    /// The review did not complete; Codex refuses the action.
    Failed(String),
}

impl Verdict {
    /// Why the action is refused; `None` = allowed.
    pub(crate) fn refusal(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::Deny { risk, rationale } => Some(format!(
                "[Codex auto-review: {} risk] {rationale}",
                risk_name(*risk)
            )),
            Self::Failed(reason) => Some(format!("[Codex auto-review] {reason}")),
        }
    }

    /// Codex's verdict on a review that missed its deadline.
    pub(crate) fn timed_out() -> Self {
        Self::Failed(
            "Automatic approval review timed out while evaluating the requested approval."
                .to_string(),
        )
    }
}

/// One proposed tool call and the conversation that holds it.
pub(crate) struct Action {
    history: Arc<Vec<ResponseItem>>,
    request: GuardianApprovalRequest,
    claude_md: Option<String>,
}

impl Action {
    /// `call` is the function call `call_id`.
    pub(crate) fn new(
        history: &[ResponseItem],
        call_id: String,
        call: ResponseItem,
        claude_md: Option<String>,
    ) -> Self {
        Self::within(conversation(history), call_id, call, claude_md)
    }

    fn within(
        history: Arc<Vec<ResponseItem>>,
        call_id: String,
        call: ResponseItem,
        claude_md: Option<String>,
    ) -> Self {
        Self {
            history,
            request: GuardianApprovalRequest::NativeToolCall {
                id: call_id,
                call: Box::new(call),
            },
            claude_md,
        }
    }
}

/// `history` without the context the client injected into user messages, which is not the
/// user's authorization.
fn conversation(history: &[ResponseItem]) -> Arc<Vec<ResponseItem>> {
    Arc::new(
        history
            .iter()
            .cloned()
            .filter_map(without_client_context)
            .collect(),
    )
}

/// Guardian reviews of actions proposed on `parent`.
pub(crate) struct Reviewer {
    pub(crate) backend: Arc<Backend>,
    pub(crate) parent: Arc<ExternalThread>,
    pub(crate) reviewed: Option<ReviewedTurn>,
    pub(crate) cwd: AbsolutePathBuf,
    /// The parent's model: its review-model override and fallback.
    pub(crate) model: ModelInfo,
    pub(crate) effort: Option<ReasoningEffort>,
}

impl Reviewer {
    /// Fails closed on any error and at the configured Guardian deadline.
    pub(crate) async fn review(&self, action: Action, parent_response_id: Option<&str>) -> Verdict {
        let timeout = self
            .backend
            .config
            .guardian_review_timeout
            .unwrap_or(REVIEW_TIMEOUT);
        match tokio::time::timeout(timeout, self.assess(action, parent_response_id)).await {
            Ok(Ok(assessment)) => match assessment.outcome {
                GuardianAssessmentOutcome::Allow => Verdict::Allow,
                GuardianAssessmentOutcome::Deny => Verdict::Deny {
                    risk: assessment.risk_level,
                    rationale: assessment.rationale,
                },
            },
            Ok(Err(error)) => {
                Verdict::Failed(format!("Automatic approval review failed: {error:#}"))
            }
            Err(_) => Verdict::timed_out(),
        }
    }

    /// `safeguard_results` for the client tool calls `calls` among `outputs`.
    pub(crate) async fn safeguard_results(
        &self,
        input: &[ResponseItem],
        outputs: &[ResponseItem],
        tools: &ToolSet,
        calls: &[String],
        parent_response_id: Option<&str>,
    ) -> Value {
        // Claude Code reads verdicts only for tool calls; an empty map still answers the request,
        // as a server reviewing nothing would.
        if calls.is_empty() {
            return available(serde_json::Map::new());
        }
        let history = conversation(&[input, outputs].concat());
        let claude_md = claude_md(input);
        let checks = outputs.iter().filter_map(|item| match item {
            ResponseItem::FunctionCall {
                call_id,
                name,
                namespace,
                ..
            } if calls.contains(call_id) => {
                let name = tools.anthropic_name(namespace.as_deref(), name);
                let action = (!ALLOWLISTED_TOOLS.contains(&name.as_str())).then(|| {
                    Action::within(
                        Arc::clone(&history),
                        call_id.clone(),
                        item.clone(),
                        claude_md.clone(),
                    )
                });
                Some(async move {
                    let result = match action {
                        None => json!({ "type": "skipped" }),
                        Some(action) => status(&self.review(action, parent_response_id).await),
                    };
                    (call_id.clone(), result)
                })
            }
            _ => None,
        });
        available(join_all(checks).await.into_iter().collect())
    }

    async fn assess(
        &self,
        action: Action,
        parent_response_id: Option<&str>,
    ) -> anyhow::Result<GuardianAssessment> {
        let config = self.backend.config.as_ref();
        let (effort, model) = self.review_model().await;
        let messages = ResolvedModelMessages::from_model(&model);
        let instructions = reviewer_instructions(
            config.guardian_transcript_mode,
            config.resolve_guardian_policy(messages),
            config.guardian_extra_policy.as_deref(),
            config.guardian_policy_template.as_deref(),
            messages,
            /*conversation_history*/ None,
        );
        let schema = guardian_output_schema();
        let prefix_tokens =
            TruncationPolicy::Bytes(instructions.len() + schema.to_string().len()).token_budget();
        let input = guardian_input(
            action,
            &model,
            config,
            prefix_tokens,
            &self.parent.thread_id().to_string(),
        )?;
        let prompt = Prompt::new(
            input,
            Vec::new(),
            BaseInstructions {
                text: instructions,
                provenance: Some(BaseInstructionsProvenance::Custom),
            },
            Some(schema),
            /*parallel_tool_calls*/ false,
        );
        let request = ExternalRequest {
            model_info: &model,
            effort,
            summary: model.default_reasoning_summary,
            service_tier: None,
            kind: ExternalRequestKind::Turn,
        };
        let (reviewer, client) = self.backend.reviewers.of(&self.parent);
        let mut turn = reviewer
            .start_review(
                &client,
                self.cwd.clone(),
                &model,
                self.reviewed.as_ref(),
                parent_response_id,
            )
            .await;
        let text = sample(&mut turn, &prompt, &request).await?;
        parse_guardian_assessment(&text)
    }

    /// The provider's review model when the catalog offers it, else the parent's model.
    async fn review_model(&self) -> (Option<ReasoningEffort>, CatalogModel) {
        let backend = &self.backend;
        let catalog = backend
            .models
            .model_catalog_with_origins(
                RefreshStrategy::InMemory,
                backend.config.http_client_factory(),
            )
            .await;
        let available = backend.models.build_available_models(
            catalog
                .into_iter()
                .filter(|entry| {
                    self.model.auto_review_model_override.is_some()
                        || matches!(
                            entry.origin,
                            ModelOrigin::Provider | ModelOrigin::ExplicitConfig
                        )
                })
                .map(|entry| Arc::unwrap_or_clone(entry.model))
                .collect(),
        );
        let selected = select_review_model(
            &self.model,
            self.effort.as_ref(),
            self.parent.review_model_id(),
            &available,
        );
        let model = backend
            .models
            .get_model_info(&selected.model, &backend.config.to_models_manager_config())
            .await;
        (selected.reasoning_effort, model)
    }
}

/// Whether the request asks for auto mode's server-side check. With checks left to Claude Code,
/// a 400 naming `safeguards` makes it retry without them and check actions itself.
pub(crate) fn server_review(
    request: &MessagesRequest,
    backend: &Backend,
) -> Result<bool, ApiError> {
    let Some(safeguards) = &request.safeguards else {
        return Ok(false);
    };
    if let Some(safeguard) = safeguards
        .iter()
        .find(|safeguard| safeguard.kind != SERVER_REVIEW)
    {
        return Err(ApiError::invalid_request(format!(
            "safeguards: `{}` is not supported",
            safeguard.kind
        )));
    }
    if !backend.auto_review {
        return Err(ApiError::invalid_request(
            "safeguards: auto mode's server-side check is disabled on this server",
        ));
    }
    Ok(!safeguards.is_empty())
}

/// The CLAUDE.md section of Claude Code's context reminder in `history`.
pub(crate) fn claude_md(history: &[ResponseItem]) -> Option<String> {
    history.iter().find_map(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content.iter().find_map(|part| match part {
                ContentItem::InputText { text }
                    if text.trim_start().starts_with(CLIENT_CONTEXT_PREFIX) =>
                {
                    let start = text.find(CLAUDE_MD_SECTION)? + CLAUDE_MD_SECTION.len();
                    let section = &text[start..];
                    let end = CLAUDE_MD_ENDS
                        .iter()
                        .filter_map(|end| section.rfind(*end))
                        .min()
                        .unwrap_or(section.len());
                    Some(section[..end].trim().to_string()).filter(|text| !text.is_empty())
                }
                _ => None,
            })
        }
        _ => None,
    })
}

pub(crate) type Review = Shared<BoxFuture<'static, Verdict>>;

/// Classifier stages of one action share its review, which outlives each request. A failed
/// review is dropped once resolved, so a later request reviews again; holders still read it.
#[derive(Default)]
pub(crate) struct Reviews(StdMutex<HashMap<[u8; 32], (Instant, Review)>>);

impl Reviews {
    pub(crate) fn get_or_start(
        &self,
        key: [u8; 32],
        start: impl FnOnce() -> BoxFuture<'static, Verdict>,
    ) -> Review {
        let mut reviews = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reviews.retain(|_, (started, review)| {
            started.elapsed() < REVIEW_RETENTION
                && !matches!(review.peek(), Some(Verdict::Failed(_)))
        });
        reviews
            .entry(key)
            .or_insert_with(|| {
                let task = tokio::spawn(start());
                let review = async move {
                    task.await.unwrap_or_else(|error| {
                        Verdict::Failed(format!("Automatic approval review failed: {error}"))
                    })
                };
                (Instant::now(), review.boxed().shared())
            })
            .1
            .clone()
    }
}

/// One Guardian reviewer thread and client per parent thread, so reviews reuse a connection.
#[derive(Default)]
pub(crate) struct Reviewers(
    StdMutex<HashMap<ThreadId, (Instant, Arc<ExternalThread>, ModelClient)>>,
);

impl Reviewers {
    pub(crate) fn of(&self, parent: &ExternalThread) -> (Arc<ExternalThread>, ModelClient) {
        let mut reviewers = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reviewers.retain(|_, (used, _, _)| used.elapsed() < REVIEWER_IDLE_TTL);
        let (used, reviewer, client) = reviewers.entry(parent.thread_id()).or_insert_with(|| {
            let reviewer = Arc::new(parent.reviewer());
            let client = reviewer.model_client();
            (Instant::now(), reviewer, client)
        });
        *used = Instant::now();
        (Arc::clone(reviewer), client.clone())
    }
}

fn available(tool_uses: serde_json::Map<String, Value>) -> Value {
    json!([{
        "type": SERVER_REVIEW,
        "status": { "type": "available", "tool_uses": tool_uses },
    }])
}

/// Codex's reviewer input: CLAUDE.md as the inherited instructions, then the composed
/// transcript and planned action within the review model's input budget.
fn guardian_input(
    action: Action,
    model: &CatalogModel,
    config: &Config,
    prefix_tokens: usize,
    session_id: &str,
) -> anyhow::Result<Vec<ResponseItem>> {
    let Action {
        history,
        request,
        claude_md,
    } = action;
    let planned = build_guardian_planned_action(&request, /*reason*/ None)?;
    let mut profile = ContextProfile::synchronous();
    profile.transcript_format = config.guardian_transcript_mode;
    let sections = default_registry().prepare(&SectionInput {
        target: profile.target,
        history: &*history,
        transcript: &profile.transcript,
        root_conversation: &[],
        root_conversation_candidates: None,
        trusted_user_answers: &[],
        planned_action: Some(&planned),
        permissions: None,
        previous_reviews: None,
        trusted_tool: None,
        trusted_skill_paths: &[],
        images: None,
        node_repl: None,
    })?;
    let context = compose_guardian_prompt(
        sections,
        &profile,
        TranscriptMode::Full,
        /*history_version*/ 0,
        session_id,
        /*node_repl_evidence_sequence*/ 0,
    )?
    .context;
    let instructions = claude_md.map(|text| ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!("# CLAUDE.md instructions\n\n<INSTRUCTIONS>\n{text}\n</INSTRUCTIONS>"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let context = context.enforce_budget(
        RequestBudget {
            max_input_tokens: effective_input_token_limit(model, config.model_context_window)
                .saturating_sub(INPUT_TOKEN_MARGIN),
            existing_context_tokens: prefix_tokens
                + instructions.as_ref().map_or(0, estimate_input_tokens),
        },
        GuardianBudgetOmission
            .render()
            .into_text()
            .unwrap_or_default(),
        HistoryTruncation::Allow,
    )?;
    Ok(instructions
        .into_iter()
        .chain(context.into_messages())
        .collect())
}

/// The assistant text of one response, resent under Codex's stream retry policy.
async fn sample(
    turn: &mut ExternalTurn,
    prompt: &Prompt,
    request: &ExternalRequest<'_>,
) -> Result<String, CodexErr> {
    let mut retries = StreamRetries::default();
    loop {
        let outcome = match turn.stream(prompt, request).await {
            Ok(mut stream) => {
                let mut text = String::new();
                loop {
                    match stream.next().await {
                        Some(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                            role,
                            content,
                            ..
                        }))) if role == "assistant" => {
                            text.extend(content.into_iter().filter_map(|part| match part {
                                ContentItem::OutputText { text, .. } => Some(text),
                                _ => None,
                            }));
                        }
                        Some(Ok(ResponseEvent::Completed { .. })) => break Ok(text),
                        Some(Ok(_)) => {}
                        Some(Err(err)) => break Err(err),
                        None => {
                            break Err(CodexErr::Stream(
                                "stream closed before response.completed".to_string(),
                            ));
                        }
                    }
                }
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(text) => return Ok(text),
            Err(err) => turn.recover(err, request, &mut retries).await?,
        }
    }
}

fn status(verdict: &Verdict) -> Value {
    match verdict.refusal() {
        None => json!({ "type": "evaluated", "outcome": "not_flagged" }),
        Some(explanation) => {
            json!({ "type": "evaluated", "outcome": "flagged", "explanation": explanation })
        }
    }
}

fn risk_name(risk: GuardianRiskLevel) -> &'static str {
    match risk {
        GuardianRiskLevel::Low => "low",
        GuardianRiskLevel::Medium => "medium",
        GuardianRiskLevel::High => "high",
        GuardianRiskLevel::Critical => "critical",
    }
}

#[cfg(test)]
#[path = "auto_review_tests.rs"]
mod tests;
