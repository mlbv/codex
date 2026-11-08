//! Claude Code 2.1.319 auto mode's own classifier requests, answered from Codex's Guardian review
//! of the action under review instead of sampling Claude Code's classifier prompt.
//!
//! Stage 1 stops at `</block>` (`</severity>` when scored) and allows only an exact `<block>no`;
//! anything else escalates to stage 2, whose 120 s deadline outlasts stage 1's 60 s attempts.
//! Both stages of one action share its review.

use std::sync::Arc;
use std::time::Duration;

use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use futures::FutureExt;
use futures::future;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::anthropic::ContentBlock;
use crate::anthropic::MessageContent;
use crate::anthropic::MessagesRequest;
use crate::anthropic::Role;
use crate::anthropic::SystemBlock;
use crate::anthropic::SystemPrompt;
use crate::auto_review;
use crate::auto_review::Action;
use crate::auto_review::Reviewer;
use crate::auto_review::Verdict;
use crate::backend::Backend;
use crate::error::ApiError;
use crate::respond::Identity;
use crate::stream::MessageStream;
use crate::tools::ToolSet;

const MONITOR_PROMPT: &str = "You are a security monitor for autonomous AI coding agents.";
/// The scored output format replacing the prompt's `<block>` one.
const SEVERITY_FORMAT: &str = "Output <severity>N</severity>";
const BLOCK_STOP: &str = "</block>";
const SEVERITY_STOP: &str = "</severity>";
const TRANSCRIPT_START: &str = "<transcript>\n";
const TRANSCRIPT_END: &str = "</transcript>\n";
const CLAUDE_MD_START: &str = "<user_claude_md>\n";
const CLAUDE_MD_END: &str = "\n</user_claude_md>";
/// Transcript keys that are not tool calls; a tool sharing one is written `[name]`.
const CONTEXT_KEYS: [&str; 5] = [
    "meta",
    "host_context",
    "host_context_live",
    "result_from",
    "omitted",
];
/// The transcript's call ids: a tool use id's last characters.
const SHORT_ID_CHARS: usize = 6;
/// Ends the transcript of a subagent's hand-back review in place of an action.
const HANDBACK_START: &str = "Subagent has finished and is handing back control to the main agent.";
/// Within stage 1's first attempt; a review still running escalates to stage 2.
const FIRST_STAGE_WAIT: Duration = Duration::from_secs(45);
/// Stage 2 must answer within 120 s of its own start, which is at or after the review's.
const REVIEW_DEADLINE: Duration = Duration::from_secs(110);

pub(crate) struct Classification {
    /// Stage 1's stop sequence; stage 2 and single-stage requests set none.
    first_stage_stop: Option<String>,
    severity: bool,
    transcript: Transcript,
}

/// The classifier's transcript as Codex history, ending with the action under review.
struct Transcript {
    text: String,
    history: Vec<ResponseItem>,
    action: Option<Call>,
    /// The transcript ends with a subagent's hand-back, which proposes no action.
    handback: bool,
    claude_md: Option<String>,
}

struct Call {
    /// The client tool name.
    name: String,
    /// What Claude Code shows its classifier of the input.
    input: Value,
    short_id: Option<String>,
    call_id: String,
    item: ResponseItem,
}

enum Entry {
    User(String),
    Assistant(String),
    Call(String, Value, Option<String>),
    Output(String, Option<String>),
}

impl Classification {
    /// `None` = not a classifier request.
    pub(crate) fn parse(request: &MessagesRequest) -> Option<Self> {
        let system = match request.system.as_ref()? {
            SystemPrompt::Text(text) => vec![text.as_str()],
            SystemPrompt::Blocks(blocks) => blocks
                .iter()
                .map(|SystemBlock::Text { text }| text.as_str())
                .collect(),
        };
        if !request.tools.is_empty() || !system.iter().any(|text| text.starts_with(MONITOR_PROMPT))
        {
            return None;
        }
        let mut claude_md = None;
        let mut transcript = String::new();
        let texts = request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| match &message.content {
                MessageContent::Text(text) => vec![text.as_str()],
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
            });
        for text in texts {
            match between(text, CLAUDE_MD_START, CLAUDE_MD_END) {
                Some(content) => claude_md = Some(unindent(content)),
                None => transcript.push_str(text),
            }
        }
        let first_stage_stop = request
            .stop_sequences
            .iter()
            .find(|stop| *stop == BLOCK_STOP || *stop == SEVERITY_STOP)
            .cloned();
        Some(Self {
            severity: first_stage_stop.as_deref() == Some(SEVERITY_STOP)
                || system.iter().any(|text| text.contains(SEVERITY_FORMAT)),
            first_stage_stop,
            transcript: Transcript::parse(&transcript, claude_md),
        })
    }
}

/// Answers a classifier request from its action's Guardian review, in the stage's grammar.
pub(crate) async fn answer(
    backend: Arc<Backend>,
    identity: Identity,
    model: String,
    classification: Classification,
    tx: mpsc::Sender<Value>,
) -> Result<(), ApiError> {
    let Classification {
        first_stage_stop,
        severity,
        transcript,
    } = classification;
    // Guardian assesses actions; a hand-back proposes none.
    let reviewing = if transcript.handback {
        future::ready(Verdict::Allow).boxed().shared()
    } else {
        let key: [u8; 32] = Sha256::digest(format!(
            "{}\0{}\0{}",
            identity.session_id,
            transcript.claude_md.as_deref().unwrap_or_default(),
            transcript.text
        ))
        .into();
        backend.reviews.get_or_start(key, || {
            review(
                Arc::clone(&backend),
                identity.session_id,
                model.clone(),
                transcript,
            )
            .boxed()
        })
    };
    let text = match first_stage_stop {
        Some(_) => first_stage(
            severity,
            tokio::time::timeout(FIRST_STAGE_WAIT, reviewing)
                .await
                .is_ok_and(|verdict| verdict == Verdict::Allow),
        )
        .to_string(),
        None => {
            let verdict = tokio::select! {
                verdict = reviewing => verdict,
                () = tx.closed() => return Ok(()),
            };
            final_stage(severity, &verdict)
        }
    };
    let tools = ToolSet::default();
    let mut stream = MessageStream::new(
        &tx,
        &tools,
        model,
        String::new(),
        /*display_thinking*/ false,
        Vec::new(),
        Vec::new(),
    );
    let _ = stream.answer(&text, first_stage_stop).await;
    Ok(())
}

/// Reviews the action as the session's own history records it when one of its pending calls
/// matches, else as the transcript shows it.
async fn review(
    backend: Arc<Backend>,
    session_id: String,
    model: String,
    transcript: Transcript,
) -> Verdict {
    let Transcript {
        history,
        action,
        claude_md,
        ..
    } = transcript;
    let Some(call) = action else {
        return Verdict::Failed(
            "Automatic approval review failed: the request shows no action to review".to_string(),
        );
    };
    let threads = backend.logged_threads(&session_id).await;
    let recorded = matched(
        threads.iter().flat_map(|thread| {
            thread
                .histories()
                .into_iter()
                .map(move |(history, cwd)| ((Arc::clone(&thread.external), cwd), history))
        }),
        &call,
    );
    let ((parent, cwd), action) = match recorded {
        Some((parent, history, call_id, item)) => {
            let claude_md = claude_md.or_else(|| auto_review::claude_md(&history));
            (parent, Action::new(&history, call_id, item, claude_md))
        }
        None => (
            (
                threads.first().map_or_else(
                    || Arc::clone(&backend.detached),
                    |root| Arc::clone(&root.external),
                ),
                None,
            ),
            Action::new(&history, call.call_id, call.item, claude_md),
        ),
    };
    let parent_model = backend
        .models
        .get_model_info(
            backend.config.model.as_deref().unwrap_or(&model),
            &backend.config.to_models_manager_config(),
        )
        .await;
    let reviewer = Reviewer {
        cwd: cwd.unwrap_or_else(|| backend.config.cwd.clone()),
        backend,
        parent,
        reviewed: None,
        model: Arc::unwrap_or_clone(parent_model.model),
        effort: None,
    };
    tokio::time::timeout(
        REVIEW_DEADLINE,
        reviewer.review(action, /*parent_response_id*/ None),
    )
    .await
    .unwrap_or_else(|_| Verdict::timed_out())
}

/// The one pending call across `histories` that `call` shows, with its history and id; none
/// when distinct calls match.
fn matched<P: Clone>(
    histories: impl Iterator<Item = (P, Arc<Vec<ResponseItem>>)>,
    call: &Call,
) -> Option<(P, Arc<Vec<ResponseItem>>, String, ResponseItem)> {
    let mut found: Option<(P, Arc<Vec<ResponseItem>>, String, ResponseItem)> = None;
    for (parent, history) in histories {
        for (call_id, item) in pending(&history, call) {
            match &found {
                None => {
                    found = Some((
                        parent.clone(),
                        Arc::clone(&history),
                        call_id.to_string(),
                        item.clone(),
                    ));
                }
                // Lanes repeat the calls they share.
                Some((_, _, found_id, _)) if found_id == call_id => {}
                Some(_) => return None,
            }
        }
    }
    found
}

/// The unanswered calls in `history` that `call` shows: the transcript id, when given, and
/// Claude Code's view of the input both agree, as a hook may change what runs.
fn pending<'a>(
    history: &'a [ResponseItem],
    call: &Call,
) -> impl Iterator<Item = (&'a str, &'a ResponseItem)> {
    let names = ToolSet::default();
    history
        .iter()
        .enumerate()
        .filter_map(move |(index, item)| match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } if names.anthropic_name(namespace.as_deref(), name) == call.name
                && call
                    .short_id
                    .as_ref()
                    .is_none_or(|short_id| short_id_of(call_id) == *short_id)
                && shows(&call.input, arguments)
                && !history[index + 1..].iter().any(|later| {
                    matches!(later, ResponseItem::FunctionCallOutput { call_id: Some(answered), .. }
                    if answered == call_id)
                }) =>
            {
                Some((call_id.as_str(), item))
            }
            _ => None,
        })
}

/// Whether Claude Code's classifier view `input` is of these call arguments: all of an
/// object's fields, or a string the arguments hold.
fn shows(input: &Value, arguments: &str) -> bool {
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    match input {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| arguments.get(key) == Some(value)),
        Value::String(text) => arguments
            .as_object()
            .is_some_and(|fields| fields.values().any(|value| value.as_str() == Some(text))),
        input => *input == arguments,
    }
}

fn short_id_of(call_id: &str) -> String {
    let chars = call_id.chars().collect::<Vec<_>>();
    chars[chars.len().saturating_sub(SHORT_ID_CHARS)..]
        .iter()
        .map(|c| {
            if c.is_ascii_alphanumeric() || *c == '_' || *c == '-' {
                *c
            } else {
                '_'
            }
        })
        .collect()
}

impl Transcript {
    fn parse(text: &str, claude_md: Option<String>) -> Self {
        let text = text
            .find(TRANSCRIPT_START)
            .map(|start| &text[start + TRANSCRIPT_START.len()..])
            .and_then(|rest| rest.find(TRANSCRIPT_END).map(|end| &rest[..end]))
            .unwrap_or_default();
        let mut history = Vec::new();
        let mut last_call = None::<String>;
        let mut action = None;
        let mut handback = false;
        let entries = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Map<String, Value>>(line).ok())
            .filter_map(entry);
        for (index, entry) in entries.enumerate() {
            action = None;
            handback = false;
            match entry {
                Entry::User(text) => {
                    handback = text.starts_with(HANDBACK_START);
                    history.push(message("user", ContentItem::InputText { text }));
                }
                Entry::Assistant(text) => history.push(message(
                    "assistant",
                    ContentItem::OutputText {
                        text,
                        annotations: None,
                        logprobs: None,
                    },
                )),
                Entry::Call(name, input, short_id) => {
                    let call_id = short_id
                        .clone()
                        .unwrap_or_else(|| format!("classifier_{index}"));
                    let item = ResponseItem::FunctionCall {
                        encrypted_content: None,
                        status: None,
                        id: None,
                        name: name.clone(),
                        namespace: None,
                        arguments: input.to_string(),
                        encrypted_function_args: None,
                        call_id: call_id.clone(),
                        internal_chat_message_metadata_passthrough: None,
                    };
                    history.push(item.clone());
                    last_call = Some(call_id.clone());
                    action = Some(Call {
                        name,
                        input,
                        short_id,
                        call_id,
                        item,
                    });
                }
                Entry::Output(output, short_id) => {
                    if let Some(call_id) = short_id.or_else(|| last_call.clone()) {
                        history.push(ResponseItem::FunctionCallOutput {
                            id: None,
                            call_id: Some(call_id),
                            name: None,
                            namespace: None,
                            output: FunctionCallOutputPayload::from_text(output),
                            internal_chat_message_metadata_passthrough: None,
                        });
                    }
                }
            }
        }
        Self {
            text: text.to_string(),
            history,
            action,
            handback,
            claude_md,
        }
    }
}

fn entry(mut fields: Map<String, Value>) -> Option<Entry> {
    let short_id = fields
        .remove("id")
        .and_then(|id| id.as_str().map(str::to_string));
    if fields.len() != 1 {
        return None;
    }
    let (key, value) = fields.into_iter().next()?;
    Some(match key.as_str() {
        "user" => Entry::User(plain(value)),
        "assistant" => Entry::Assistant(plain(value)),
        "outcome" | "tool_result" => Entry::Output(plain(value), short_id),
        key if CONTEXT_KEYS.contains(&key) => return None,
        key => Entry::Call(
            key.strip_prefix('[')
                .and_then(|key| key.strip_suffix(']'))
                .unwrap_or(key)
                .to_string(),
            value,
            short_id,
        ),
    })
}

fn plain(value: Value) -> String {
    match value {
        Value::String(text) => text,
        value => value.to_string(),
    }
}

fn message(role: &str, content: ContentItem) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![content],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn between<'t>(text: &'t str, start: &str, end: &str) -> Option<&'t str> {
    let from = text.find(start)? + start.len();
    let to = text.rfind(end)?;
    text.get(from..to)
}

/// Claude Code indents quoted blocks by two spaces.
fn unindent(text: &str) -> String {
    text.lines()
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stage 1 ends at its stop sequence, which the answer omits.
fn first_stage(severity: bool, allowed: bool) -> &'static str {
    match (severity, allowed) {
        (false, true) => "<block>no",
        (false, false) => "<block>yes",
        (true, true) => "<severity>0",
        (true, false) => "<severity>100",
    }
}

fn final_stage(severity: bool, verdict: &Verdict) -> String {
    match (severity, verdict.refusal()) {
        (false, None) => "<block>no</block>".to_string(),
        (false, Some(reason)) => format!(
            "<block>yes</block><reason>{}</reason>",
            reason.replace('<', "&lt;")
        ),
        (true, None) => "<severity>0</severity>".to_string(),
        (true, Some(_)) => "<severity>100</severity>".to_string(),
    }
}

#[cfg(test)]
#[path = "classifier_tests.rs"]
mod tests;
