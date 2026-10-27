//! Codex response events as one Anthropic message's event stream.

use codex_core::ResponseEvent;
use codex_core::external_harness::record_history_item;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::TokenUsage;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::cite::CiteFilter;
use crate::codec;
use crate::server_tools::ServerCall;
use crate::server_tools::ServerResult;
use crate::server_tools::tool_search_input;
use crate::stop::StopMatcher;
use crate::tools::ToolSet;
use crate::web_run::ServerToolUses;
use crate::web_run::WebRunCall;
use crate::web_run::is_web_run;
use crate::web_run::markdown_links;

/// An output budget is a normal Messages API stop, while Codex treats it as an error.
pub(crate) fn messages_response_event(
    event: Result<ResponseEvent, CodexErr>,
) -> Result<ResponseEvent, CodexErr> {
    if let Err(err) = &event
        && let CodexErrorDetails::ResponseIncomplete(incomplete) = err.details()
        && incomplete.reason == "max_output_tokens"
        && let Some(response_id) = &incomplete.response_id
    {
        return Ok(ResponseEvent::Incomplete {
            response_id: response_id.clone(),
            token_usage: incomplete.token_usage.clone(),
            reason: incomplete.reason.clone(),
        });
    }
    event
}

/// The receiving side went away; abandon the response.
#[derive(Debug)]
pub(crate) struct Disconnected;

pub(crate) enum Flow {
    Continue,
    /// `response.completed`; `follow_up` when the model stopped short of ending its turn.
    Completed {
        follow_up: bool,
    },
    StopSequence,
    /// `response.incomplete` at the request's output budget.
    OutputLimit,
}

enum Open {
    Thinking {
        part: Option<i64>,
    },
    /// `text` is what the client has seen; `streamed` once any delta arrived.
    Text {
        text: String,
        streamed: bool,
    },
    /// A `tool_use` streaming its input; it cannot be withdrawn once started.
    ToolInput {
        item_id: String,
        streamed: String,
    },
    Block,
}

pub(crate) struct MessageStream<'a> {
    tx: &'a mpsc::Sender<Value>,
    tools: &'a ToolSet,
    model: String,
    turn_id: String,
    display_thinking: bool,
    stop: StopMatcher,
    cite: CiteFilter,
    started: bool,
    next_index: usize,
    open: Option<Open>,
    /// Items completed so far, stamped for history.
    pub(crate) outputs: Vec<ResponseItem>,
    usage: Option<TokenUsage>,
    output_tokens: i64,
    /// Executor input totals: uncached, cache writes, cache reads.
    input_tokens: (i64, i64, i64),
    /// Prompt size `message_start` reports; the backend counts it only at completion.
    input_estimate: i64,
    /// Input tokens and cache reads of the first sample, whose prompt is the request's input.
    first_sample: Option<(i64, i64)>,
    /// Per-sample and advisor usage, reported when more than one.
    iterations: Vec<Value>,
    tool_calls: Vec<String>,
    /// `web.run` calls of the current response, run once it completes.
    server_calls: Vec<ServerCall>,
    /// Blocks the message opens with: `mcp_tool_listing`.
    leading_blocks: Vec<Value>,
    /// Search results a text block may cite, by URL.
    citable: HashMap<String, Value>,
    /// End of the open text block's last cited link.
    cited_to: usize,
    pub(crate) server_tool_uses: ServerToolUses,
    stop_sequence: Option<String>,
    output_limited: bool,
    applied_edits: Vec<Value>,
}

impl<'a> MessageStream<'a> {
    pub(crate) fn new(
        tx: &'a mpsc::Sender<Value>,
        tools: &'a ToolSet,
        model: String,
        turn_id: String,
        display_thinking: bool,
        stop_sequences: Vec<String>,
        applied_edits: Vec<Value>,
    ) -> Self {
        Self {
            tx,
            tools,
            model,
            turn_id,
            display_thinking,
            stop: StopMatcher::new(stop_sequences),
            cite: CiteFilter::default(),
            started: false,
            next_index: 0,
            open: None,
            outputs: Vec::new(),
            usage: None,
            output_tokens: 0,
            input_tokens: (0, 0, 0),
            input_estimate: 0,
            first_sample: None,
            iterations: Vec::new(),
            tool_calls: Vec::new(),
            server_calls: Vec::new(),
            leading_blocks: Vec::new(),
            citable: HashMap::new(),
            cited_to: 0,
            server_tool_uses: ServerToolUses::default(),
            stop_sequence: None,
            output_limited: false,
            applied_edits,
        }
    }

    /// Call ids the client must answer before the turn can continue.
    pub(crate) fn tool_calls(&self) -> &[String] {
        &self.tool_calls
    }

    pub(crate) fn usage(&self) -> Option<&TokenUsage> {
        self.usage.as_ref()
    }

    pub(crate) fn first_sample(&self) -> Option<(i64, i64)> {
        self.first_sample
    }

    pub(crate) fn started(&self) -> bool {
        self.started
    }

    /// Generation ended early: a stop sequence, or `max_tokens`.
    pub(crate) fn stopped(&self) -> bool {
        self.stop_sequence.is_some() || self.output_limited
    }

    /// An executor sample's usage: summed into the top level, listed as an iteration.
    fn record_sample(&mut self, usage: Option<TokenUsage>) {
        if let Some(usage) = &usage {
            self.first_sample
                .get_or_insert((usage.input_tokens, usage.cached_input_tokens));
            let iteration = iteration("message", None, usage);
            self.output_tokens += usage.output_tokens;
            self.input_tokens.0 += iteration["input_tokens"].as_i64().unwrap_or_default();
            self.input_tokens.1 += usage.cache_write_input_tokens;
            self.input_tokens.2 += usage.cached_input_tokens;
            self.iterations.push(iteration);
        }
        self.usage = usage.or(self.usage.take());
    }

    /// An advisor sub-inference: listed, not summed.
    pub(crate) fn advisor_iteration(&mut self, model: String, usage: &TokenUsage) {
        self.iterations
            .push(iteration("advisor_message", Some(&model), usage));
    }

    pub(crate) fn take_server_calls(&mut self) -> Vec<ServerCall> {
        std::mem::take(&mut self.server_calls)
    }

    /// Blocks sent right after `message_start`.
    pub(crate) fn lead_with(&mut self, blocks: Vec<Value>) {
        self.leading_blocks = blocks;
    }

    pub(crate) fn estimate_input(&mut self, tokens: i64) {
        self.input_estimate = tokens;
    }

    /// Search results later text cites; citations exist only with a web search tool.
    pub(crate) fn cite(&mut self, citations: Vec<(String, Value)>) {
        if self.tools.web_search.is_some() {
            self.citable.extend(citations);
        }
    }

    /// Whether a resend can continue this message: a `tool_use` with partial input cannot.
    pub(crate) fn resumable(&self) -> bool {
        !matches!(self.open, Some(Open::ToolInput { .. }))
    }

    /// A server tool's result block, and its output entering history.
    pub(crate) async fn server_result(&mut self, result: ServerResult) -> Result<(), Disconnected> {
        let ServerResult {
            block,
            mut output,
            citations,
            ..
        } = result;
        self.cite(citations);
        self.open_block(Open::Block, block).await?;
        self.close_block().await?;
        record_history_item(&mut output, &self.turn_id);
        self.outputs.push(output);
        Ok(())
    }

    /// Resolves once the client stopped reading.
    pub(crate) async fn closed(&self) {
        self.tx.closed().await;
    }

    /// Ends the block a failed response left open; its resend starts new blocks.
    pub(crate) async fn abandon_open_block(&mut self) -> Result<(), Disconnected> {
        let held = self.stop.flush();
        self.emit_model_text(&held).await?;
        self.flush_model_text().await?;
        self.close_block().await
    }

    /// A complete text-only answer.
    pub(crate) async fn summary(
        &mut self,
        text: &str,
        usage: Option<TokenUsage>,
    ) -> Result<(), Disconnected> {
        self.record_sample(usage);
        self.open_text().await?;
        self.emit_text(text).await?;
        self.close_block().await?;
        self.finish(/*refusal*/ None).await
    }

    pub(crate) async fn start(&mut self) -> Result<(), Disconnected> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let message = json!({
            "type": "message_start",
            "message": {
                "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.input_estimate,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 0,
                },
            },
        });
        self.send(message).await?;
        for block in std::mem::take(&mut self.leading_blocks) {
            let index = self.next_index;
            self.next_index += 1;
            self.send(
                json!({ "type": "content_block_start", "index": index, "content_block": block }),
            )
            .await?;
            self.send(json!({ "type": "content_block_stop", "index": index }))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn on_event(&mut self, event: ResponseEvent) -> Result<Flow, Disconnected> {
        match event {
            ResponseEvent::OutputItemAdded(item) => self.item_added(&item).await?,
            ResponseEvent::OutputTextDelta(delta) => return self.text(&delta).await,
            ResponseEvent::FunctionCallArgumentsDelta { item_id, delta } => {
                if let Some(Open::ToolInput {
                    item_id: open_id,
                    streamed,
                }) = &mut self.open
                    && *open_id == item_id
                {
                    streamed.push_str(&delta);
                    self.delta(json!({ "type": "input_json_delta", "partial_json": delta }))
                        .await?;
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => self.thinking(&delta, summary_index).await?,
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => self.thinking(&delta, content_index).await?,
            ResponseEvent::OutputItemDone(item) => return self.item_done(item).await,
            ResponseEvent::Completed {
                token_usage,
                end_turn,
                ..
            } => {
                self.record_sample(token_usage);
                let follow_up = end_turn == Some(false) && self.tool_calls.is_empty();
                return Ok(Flow::Completed { follow_up });
            }
            ResponseEvent::Incomplete { token_usage, .. } => {
                self.record_sample(token_usage);
                self.output_limited = true;
                return Ok(Flow::OutputLimit);
            }
            ResponseEvent::Created { .. }
            | ResponseEvent::SafetyBuffering(_)
            | ResponseEvent::ServerModel(_)
            | ResponseEvent::ModelVerifications(_)
            | ResponseEvent::TurnModerationMetadata(_)
            | ResponseEvent::ServerReasoningIncluded(_)
            | ResponseEvent::ToolCallInputDelta { .. }
            | ResponseEvent::ReasoningSummaryDone { .. }
            | ResponseEvent::ReasoningSummaryPartAdded { .. }
            | ResponseEvent::RateLimits(_)
            | ResponseEvent::ModelsEtag(_) => {}
        }
        Ok(Flow::Continue)
    }

    async fn item_added(&mut self, item: &ResponseItem) -> Result<(), Disconnected> {
        match item {
            ResponseItem::Reasoning { .. } => self.open_thinking(String::new()).await,
            ResponseItem::Message { role, .. } if role == "assistant" => self.open_text().await,
            // Input streams only for tools that asked for it; others open complete.
            ResponseItem::FunctionCall {
                id: Some(id),
                name,
                namespace,
                call_id,
                ..
            } if !self.is_server_call(namespace.as_deref(), name)
                && self
                    .tools
                    .is_eager(&self.tools.anthropic_name(namespace.as_deref(), name)) =>
            {
                let kind = Open::ToolInput {
                    item_id: id.as_str().to_string(),
                    streamed: String::new(),
                };
                self.open_tool_use(kind, call_id, namespace.as_deref(), name)
                    .await
            }
            ResponseItem::WebSearchCall { id: Some(id), .. } => {
                self.open_web_search(id.as_str()).await
            }
            _ => Ok(()),
        }
    }

    async fn text(&mut self, delta: &str) -> Result<Flow, Disconnected> {
        if !matches!(self.open, Some(Open::Text { .. })) {
            self.open_text().await?;
        }
        if let Some(Open::Text { streamed, .. }) = &mut self.open {
            *streamed = true;
        }
        let (text, matched) = self.stop.push(delta);
        self.emit_model_text(&text).await?;
        if let Some(sequence) = matched {
            self.flush_model_text().await?;
            self.stop_sequence = Some(sequence);
            return Ok(Flow::StopSequence);
        }
        Ok(Flow::Continue)
    }

    /// Model output: citation markers become citations of known search results.
    async fn emit_model_text(&mut self, text: &str) -> Result<(), Disconnected> {
        let (text, refs) = self.cite.push(text);
        self.emit_text(&text).await?;
        for reference in refs {
            let citation = self
                .citable
                .values()
                .find(|citation| citation["encrypted_index"] == reference.as_str())
                .cloned();
            if let Some(citation) = citation {
                self.delta(json!({ "type": "citations_delta", "citation": citation }))
                    .await?;
            }
        }
        Ok(())
    }

    /// Text held as a possible marker when its block ends.
    async fn flush_model_text(&mut self) -> Result<(), Disconnected> {
        let held = self.cite.flush();
        self.emit_text(&held).await
    }

    async fn emit_text(&mut self, text: &str) -> Result<(), Disconnected> {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(Open::Text { text: emitted, .. }) = &mut self.open {
            emitted.push_str(text);
        }
        self.delta(json!({ "type": "text_delta", "text": text }))
            .await?;
        self.emit_citations().await
    }

    /// Cites each newly completed Markdown link to a known search result.
    async fn emit_citations(&mut self) -> Result<(), Disconnected> {
        let Some(Open::Text { text, .. }) = &self.open else {
            return Ok(());
        };
        if self.citable.is_empty() {
            return Ok(());
        }
        let from = self.cited_to;
        let links = markdown_links(&text[from..])
            .into_iter()
            .map(|(end, url)| (from + end, self.citable.get(url).cloned()))
            .collect::<Vec<_>>();
        for (end, citation) in links {
            self.cited_to = end;
            if let Some(citation) = citation {
                self.delta(json!({ "type": "citations_delta", "citation": citation }))
                    .await?;
            }
        }
        Ok(())
    }

    async fn thinking(&mut self, delta: &str, index: i64) -> Result<(), Disconnected> {
        if !self.display_thinking {
            return Ok(());
        }
        let Some(Open::Thinking { part }) = &mut self.open else {
            return Ok(());
        };
        let separator = if part.is_some_and(|part| part != index) {
            "\n\n"
        } else {
            ""
        };
        *part = Some(index);
        self.delta(json!({ "type": "thinking_delta", "thinking": format!("{separator}{delta}") }))
            .await
    }

    async fn item_done(&mut self, mut item: ResponseItem) -> Result<Flow, Disconnected> {
        match &item {
            ResponseItem::Reasoning { summary, .. } => {
                if !matches!(self.open, Some(Open::Thinking { .. })) {
                    let text = if self.display_thinking {
                        summary_text(summary)
                    } else {
                        String::new()
                    };
                    self.open_thinking(text).await?;
                }
                record_history_item(&mut item, &self.turn_id);
                // ResponseItem serializes infallibly; an empty signature only drops it on replay.
                let signature = codec::encode_signature(&item).unwrap_or_default();
                self.delta(json!({ "type": "signature_delta", "signature": signature }))
                    .await?;
                self.close_block().await?;
            }
            ResponseItem::Message { role, content, .. } if role == "assistant" => {
                let full = output_text(content);
                if full.is_empty() {
                    self.close_block().await?;
                    return Ok(Flow::Continue);
                }
                if !matches!(self.open, Some(Open::Text { streamed: true, .. })) {
                    if !matches!(self.open, Some(Open::Text { .. })) {
                        self.open_text().await?;
                    }
                    let (text, matched) = self.stop.push(&full);
                    self.emit_model_text(&text).await?;
                    if let Some(sequence) = matched {
                        self.flush_model_text().await?;
                        self.stop_sequence = Some(sequence);
                        return Ok(Flow::StopSequence);
                    }
                }
                let rest = self.stop.flush();
                self.emit_model_text(&rest).await?;
                self.flush_model_text().await?;
                self.close_block().await?;
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                call_id,
                arguments,
                ..
            } if self.is_server_call(namespace.as_deref(), name) => {
                let call = WebRunCall::classify(call_id.clone(), arguments, self.tools);
                let block = json!({
                    "type": "server_tool_use",
                    "id": call_id,
                    "name": call.kind.name(),
                    "input": {},
                });
                let input = call.input.to_string();
                self.complete_block(block, &input).await?;
                self.server_calls.push(ServerCall::WebRun(Box::new(call)));
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::FunctionCall {
                name,
                namespace: Some(namespace),
                call_id,
                arguments,
                ..
            } if self.tools.mcp_namespaces.contains_key(namespace) => {
                let server = self.tools.mcp_namespaces[namespace].clone();
                let block = json!({
                    "type": "mcp_tool_use",
                    "id": call_id,
                    "name": name,
                    "server_name": server,
                    "input": {},
                });
                self.complete_block(block, arguments).await?;
                self.server_calls.push(ServerCall::Mcp {
                    call_id: call_id.clone(),
                    server,
                    tool: name.clone(),
                    arguments: arguments.clone(),
                });
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::FunctionCall { name, call_id, .. }
                if self.tools.is_advisor(None, name) =>
            {
                let block = json!({
                    "type": "server_tool_use",
                    "id": call_id,
                    "name": name,
                    "input": {},
                });
                self.complete_block(block, "{}").await?;
                self.server_calls.push(ServerCall::Advisor {
                    call_id: call_id.clone(),
                });
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                execution,
                arguments,
                ..
            } if execution == "client" && self.tools.tool_search.is_some() => {
                let Some(tool) = &self.tools.tool_search else {
                    return Ok(Flow::Continue);
                };
                let block = json!({
                    "type": "server_tool_use",
                    "id": call_id,
                    "name": tool.name,
                    "input": {},
                });
                let input = tool_search_input(tool, arguments).to_string();
                self.complete_block(block, &input).await?;
                self.server_calls.push(ServerCall::ToolSearch {
                    call_id: call_id.clone(),
                    arguments: arguments.clone(),
                });
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                call_id,
                arguments,
                ..
            } => {
                let rest = match &self.open {
                    // The streamed prefix is final; a diverging remainder is dropped.
                    Some(Open::ToolInput { streamed, .. }) => arguments
                        .strip_prefix(streamed.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    _ => {
                        self.open_tool_use(Open::Block, call_id, namespace.as_deref(), name)
                            .await?;
                        arguments.clone()
                    }
                };
                if !rest.is_empty() {
                    self.delta(json!({ "type": "input_json_delta", "partial_json": rest }))
                        .await?;
                }
                self.tool_calls.push(call_id.clone());
                self.close_block().await?;
                record_history_item(&mut item, &self.turn_id);
            }
            ResponseItem::WebSearchCall {
                id: Some(id),
                action,
                ..
            } => {
                if !matches!(self.open, Some(Open::Block)) {
                    self.open_web_search(id.as_str()).await?;
                }
                let query = match action {
                    Some(WebSearchAction::Search { query, queries }) => query
                        .clone()
                        .or_else(|| queries.as_ref().map(|queries| queries.join(" "))),
                    Some(WebSearchAction::OpenPage { url })
                    | Some(WebSearchAction::FindInPage { url, .. }) => url.clone(),
                    Some(WebSearchAction::Other) | None => None,
                };
                let input = json!({ "query": query.unwrap_or_default() }).to_string();
                self.delta(json!({ "type": "input_json_delta", "partial_json": input }))
                    .await?;
                self.close_block().await?;
                // Codex consumes no result list; the model cites its sources in text.
                let result = json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": id.as_str(),
                    "content": [],
                });
                self.open_block(Open::Block, result).await?;
                self.close_block().await?;
                record_history_item(&mut item, &self.turn_id);
            }
            _ => return Ok(Flow::Continue),
        }
        self.outputs.push(item);
        Ok(Flow::Continue)
    }

    /// Ends the message. `refusal` carries a policy decline in place of a stop.
    pub(crate) async fn finish(&mut self, refusal: Option<&str>) -> Result<(), Disconnected> {
        self.start().await?;
        if self.stopped()
            && let Some(Open::Text { text, .. }) = &self.open
        {
            // The server's message never completed; history keeps what the client saw.
            let mut item = ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: text.clone(),
                    annotations: None,
                    logprobs: None,
                }],
                phase: Some(MessagePhase::FinalAnswer),
                internal_chat_message_metadata_passthrough: None,
            };
            record_history_item(&mut item, &self.turn_id);
            self.outputs.push(item);
        } else {
            let rest = self.stop.flush();
            self.emit_model_text(&rest).await?;
            self.flush_model_text().await?;
        }
        self.close_block().await?;
        let stop_reason = if refusal.is_some() {
            "refusal"
        } else if self.stop_sequence.is_some() {
            "stop_sequence"
        } else if self.output_limited {
            "max_tokens"
        } else if self.tool_calls.is_empty() {
            "end_turn"
        } else {
            "tool_use"
        };
        // Claude Code keeps `message_start`'s value of a field `message_delta` reports as 0.
        let (uncached, cache_writes, cache_reads) = match self.input_tokens {
            _ if self.first_sample.is_none() => (self.input_estimate, 0, 0),
            (0, writes, reads) if reads > 0 => (1, writes, reads - 1),
            (0, writes, reads) if writes > 0 => (1, writes - 1, reads),
            totals => totals,
        };
        let mut delta = json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": self.stop_sequence },
            "usage": {
                "input_tokens": uncached,
                "cache_creation_input_tokens": cache_writes,
                "cache_read_input_tokens": cache_reads,
                "output_tokens": self.output_tokens,
            },
        });
        if self.iterations.len() > 1 {
            delta["usage"]["iterations"] = json!(self.iterations);
        }
        let ServerToolUses {
            search,
            fetch,
            failed_search,
            ..
        } = self.server_tool_uses;
        let search = search - failed_search;
        if search > 0 {
            delta["usage"]["server_tool_use"]["web_search_requests"] = json!(search);
        }
        if fetch > 0 {
            delta["usage"]["server_tool_use"]["web_fetch_requests"] = json!(fetch);
        }
        if !self.applied_edits.is_empty() {
            delta["context_management"] = json!({ "applied_edits": self.applied_edits });
        }
        if let Some(explanation) = refusal {
            delta["delta"]["stop_details"] =
                json!({ "type": "refusal", "category": null, "explanation": explanation });
        }
        self.send(delta).await?;
        self.send(json!({ "type": "message_stop" })).await
    }

    async fn open_thinking(&mut self, text: String) -> Result<(), Disconnected> {
        let block = json!({ "type": "thinking", "thinking": text, "signature": "" });
        self.open_block(Open::Thinking { part: None }, block).await
    }

    async fn open_text(&mut self) -> Result<(), Disconnected> {
        let block = json!({ "type": "text", "text": "" });
        self.open_block(
            Open::Text {
                text: String::new(),
                streamed: false,
            },
            block,
        )
        .await
    }

    fn is_server_call(&self, namespace: Option<&str>, name: &str) -> bool {
        self.tools.web_run && is_web_run(namespace, name)
    }

    async fn open_tool_use(
        &mut self,
        kind: Open,
        call_id: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<(), Disconnected> {
        let block = match self.tools.toolset(namespace) {
            Some(toolset) => {
                json!({ "type": "tool_use", "id": call_id, "name": name, "toolset_name": toolset, "input": {} })
            }
            None => {
                let name = self.tools.anthropic_name(namespace, name);
                json!({ "type": "tool_use", "id": call_id, "name": name, "input": {} })
            }
        };
        self.open_block(kind, block).await
    }

    /// A block whose input is complete: start, one `input_json_delta`, stop.
    async fn complete_block(&mut self, block: Value, input: &str) -> Result<(), Disconnected> {
        self.open_block(Open::Block, block).await?;
        self.delta(json!({ "type": "input_json_delta", "partial_json": input }))
            .await?;
        self.close_block().await
    }

    async fn open_web_search(&mut self, id: &str) -> Result<(), Disconnected> {
        let block =
            json!({ "type": "server_tool_use", "id": id, "name": "web_search", "input": {} });
        self.open_block(Open::Block, block).await
    }

    async fn open_block(&mut self, kind: Open, block: Value) -> Result<(), Disconnected> {
        self.start().await?;
        self.close_block().await?;
        self.cited_to = 0;
        self.open = Some(kind);
        let index = self.next_index;
        self.send(json!({ "type": "content_block_start", "index": index, "content_block": block }))
            .await
    }

    async fn close_block(&mut self) -> Result<(), Disconnected> {
        if self.open.take().is_none() {
            return Ok(());
        }
        let index = self.next_index;
        self.next_index += 1;
        self.send(json!({ "type": "content_block_stop", "index": index }))
            .await
    }

    async fn delta(&mut self, delta: Value) -> Result<(), Disconnected> {
        let index = self.next_index;
        self.send(json!({ "type": "content_block_delta", "index": index, "delta": delta }))
            .await
    }

    async fn send(&self, event: Value) -> Result<(), Disconnected> {
        self.tx.send(event).await.map_err(|_| Disconnected)
    }
}

fn iteration(kind: &str, model: Option<&str>, usage: &TokenUsage) -> Value {
    let uncached = usage.input_tokens - usage.cached_input_tokens - usage.cache_write_input_tokens;
    let mut iteration = json!({
        "type": kind,
        "input_tokens": uncached.max(0),
        "cache_read_input_tokens": usage.cached_input_tokens,
        "cache_creation_input_tokens": usage.cache_write_input_tokens,
        "output_tokens": usage.output_tokens,
    });
    if let Some(model) = model {
        iteration["model"] = json!(model);
    }
    iteration
}

fn output_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentItem::OutputText { text, .. } => Some(text.as_str()),
            ContentItem::InputText { .. }
            | ContentItem::InputImage { .. }
            | ContentItem::InputAudio { .. }
            | ContentItem::InputFile { .. } => None,
        })
        .collect()
}

fn summary_text(summary: &[ReasoningItemReasoningSummary]) -> String {
    summary
        .iter()
        .map(|ReasoningItemReasoningSummary::SummaryText { text }| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
