//! The advisor server tool: a separate, tool-less Codex sampling on the advisor model, reading the
//! executor's transcript as quoted context.

use codex_core::Prompt;
use codex_core::ResponseControls;
use codex_core::ResponseEvent;
use codex_core::external_harness::ExternalRequest;
use codex_core::external_harness::ExternalRequestKind;
use codex_core::external_harness::StreamRetries;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::backend::Backend;
use crate::backend::Thread;
use crate::server_tools::ServerResult;

pub(crate) const ADVISOR_DESCRIPTION: &str = "Consult a higher-intelligence advisor model for strategic guidance: a plan, the tricky parts, or a course correction. The advisor reads the whole conversation, including your work so far in this turn. Call it with no arguments before committing to an approach on complex tasks, and when stuck.";

const ADVISOR_INSTRUCTIONS: &str = "You advise another AI model, the executor, partway through its task. The user message quotes the executor's system prompt, tools and transcript. Reply with concise strategic guidance the executor can act on: a plan, the tricky parts, or a course correction. Do not perform the task, call tools, or restate the transcript.";

/// An `advisor_*` tool definition.
#[derive(Debug, Deserialize)]
pub(crate) struct AdvisorTool {
    pub(crate) name: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) max_uses: Option<u64>,
    #[serde(default)]
    pub(crate) max_tokens: Option<u64>,
}

/// The executor context an advisor call quotes.
pub(crate) struct Transcript<'a> {
    pub(crate) instructions: &'a str,
    pub(crate) tools: &'a [ToolSpec],
    pub(crate) history: &'a [ResponseItem],
}

/// An advisor call's result and, when it sampled, the advisor's usage.
pub(crate) struct Advice {
    pub(crate) result: ServerResult,
    pub(crate) usage: Option<TokenUsage>,
}

pub(crate) async fn advise(
    backend: &Backend,
    thread: &Thread,
    cwd: AbsolutePathBuf,
    tool: &AdvisorTool,
    call_id: &str,
    transcript: Transcript<'_>,
) -> Advice {
    let Ok(model_info) = backend.model_info(&tool.model).await else {
        return failure(call_id, "model_not_found");
    };
    let prompt = Prompt::new(
        vec![ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: render(&transcript),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        Vec::new(),
        BaseInstructions {
            text: ADVISOR_INSTRUCTIONS.to_string(),
            provenance: None,
        },
        /*output_schema*/ None,
        /*parallel_tool_calls*/ false,
    )
    .with_controls(ResponseControls {
        max_output_tokens: tool.max_tokens,
        ..Default::default()
    });
    let request = ExternalRequest {
        model_info: &model_info,
        effort: None,
        summary: model_info.default_reasoning_summary,
        service_tier: None,
        kind: ExternalRequestKind::Turn,
    };
    let client = thread.external.model_client();
    let mut turn = thread
        .external
        .start_turn(&client, /*prewarmed*/ None, cwd, &model_info)
        .await;
    let mut retries = StreamRetries::default();
    loop {
        let mut text = String::new();
        let outcome = match turn.stream(&prompt, &request).await {
            Ok(mut stream) => loop {
                match stream
                    .next()
                    .await
                    .map(crate::stream::messages_response_event)
                {
                    Some(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                        role,
                        content,
                        ..
                    }))) if role == "assistant" => {
                        for part in content {
                            if let ContentItem::OutputText { text: part, .. } = part {
                                text.push_str(&part);
                            }
                        }
                    }
                    Some(Ok(ResponseEvent::Completed { token_usage, .. })) => {
                        break Ok((token_usage, "end_turn"));
                    }
                    Some(Ok(ResponseEvent::Incomplete { token_usage, .. })) => {
                        break Ok((token_usage, "max_tokens"));
                    }
                    Some(Ok(_)) => {}
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
        match outcome {
            Ok((usage, stop_reason)) => {
                let mut content = json!({ "type": "advisor_result", "text": text });
                if tool.max_tokens.is_some() {
                    content["stop_reason"] = json!(stop_reason);
                }
                return Advice {
                    result: ServerResult::new(
                        json!({
                            "type": "advisor_tool_result",
                            "tool_use_id": call_id,
                            "content": content,
                        }),
                        advice_output(call_id, text),
                    ),
                    usage,
                };
            }
            Err(err) => {
                if let Err(err) = turn.recover(err, &request, &mut retries).await {
                    return failure(call_id, error_code(&err));
                }
            }
        }
    }
}

pub(crate) fn failure(call_id: &str, code: &str) -> Advice {
    Advice {
        result: ServerResult::new(
            json!({
                "type": "advisor_tool_result",
                "tool_use_id": call_id,
                "content": { "type": "advisor_tool_result_error", "error_code": code },
            }),
            advice_output(call_id, format!("advisor unavailable: {code}")),
        ),
        usage: None,
    }
}

pub(crate) fn advice_output(call_id: &str, text: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: codex_protocol::models::FunctionCallOutputPayload::from_text(text),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn error_code(err: &CodexErr) -> &'static str {
    match err.details() {
        CodexErrorDetails::ContextWindowExceeded => "prompt_too_long",
        CodexErrorDetails::UsageLimitReached(_) | CodexErrorDetails::QuotaExceeded => {
            "too_many_requests"
        }
        CodexErrorDetails::ServerOverloaded => "overloaded",
        _ => "unavailable",
    }
}

/// The executor's system prompt, tools and transcript as quoted text.
fn render(transcript: &Transcript<'_>) -> String {
    let mut text = format!(
        "<executor_system_prompt>\n{}\n</executor_system_prompt>\n<executor_tools>\n",
        transcript.instructions
    );
    for spec in transcript.tools {
        match spec {
            ToolSpec::Function(tool) => text.push_str(&format!(
                "- {}: {}\n",
                tool.name,
                first_line(&tool.description)
            )),
            ToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    let (name, description) = match tool {
                        ResponsesApiNamespaceTool::Function(tool) => {
                            (&tool.name, &tool.description)
                        }
                        ResponsesApiNamespaceTool::Custom(tool) => (&tool.name, &tool.description),
                    };
                    text.push_str(&format!(
                        "- {}{name}: {}\n",
                        namespace.name,
                        first_line(description)
                    ));
                }
            }
            other => text.push_str(&format!("- {}\n", other.name())),
        }
    }
    text.push_str("</executor_tools>\n<transcript>\n");
    for item in transcript.history {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let body = content
                    .iter()
                    .map(|part| match part {
                        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                            text.clone()
                        }
                        ContentItem::InputImage { .. } => "[image]".to_string(),
                        ContentItem::InputAudio { .. } => "[audio]".to_string(),
                        ContentItem::InputFile { .. } => "[file]".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                text.push_str(&format!("[{role}]\n{body}\n\n"));
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                ..
            } => text.push_str(&format!(
                "[tool call {}{name}]\n{arguments}\n\n",
                namespace.as_deref().unwrap_or_default()
            )),
            ResponseItem::FunctionCallOutput { output, .. } => text.push_str(&format!(
                "[tool result]\n{}\n\n",
                output.body.to_text().unwrap_or_default()
            )),
            ResponseItem::Compaction { .. } => {
                text.push_str("[earlier conversation compacted]\n\n");
            }
            _ => {}
        }
    }
    text.push_str("</transcript>");
    text
}

fn first_line(description: &str) -> &str {
    description.lines().next().unwrap_or_default()
}
