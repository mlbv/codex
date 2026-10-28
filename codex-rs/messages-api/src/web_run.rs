//! Codex's standalone `web.run` calls as Anthropic `web_search`/`web_fetch` server tools: run by
//! this server, as Codex runs them, and rebuilt from the blocks clients echo back.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_api::ApiError as SearchApiError;
use codex_api::ApproximateLocation;
use codex_api::LocationType;
use codex_api::SearchCommands;
use codex_api::SearchFilters;
use codex_core::web_search_action_detail;
use codex_protocol::models::FileReference;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_web_search_extension::RUN_TOOL_NAME;
use codex_web_search_extension::SearchError;
use codex_web_search_extension::StandaloneSearch;
use codex_web_search_extension::WEB_NAMESPACE;
use codex_web_search_extension::parse_commands;
use codex_web_search_extension::search_action;
use codex_web_search_extension::search_request;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use url::Url;

use crate::anthropic::ContentBlock;
use crate::anthropic::DocumentSource;
use crate::anthropic::InputMessage;
use crate::anthropic::MessageContent;
use crate::anthropic::Role;
use crate::anthropic::ToolResultBlock;
use crate::anthropic::ToolResultContent;
use crate::anthropic::WebFetchResultContent;
use crate::anthropic::WebSearchResultContent;
use crate::error::ApiError;
use crate::server_tools::ServerResult;
use crate::tools::ServerTool;
use crate::tools::ToolSet;

pub(crate) const WEB_SEARCH: &str = "web_search";
pub(crate) const WEB_FETCH: &str = "web_fetch";
/// Prefix of the first search result's `encrypted_content`, which carries the call's output.
const OUTPUT_PREFIX: &str = "codex.wr1.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerToolKind {
    Search,
    Fetch,
}

impl ServerToolKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Search => WEB_SEARCH,
            Self::Fetch => WEB_FETCH,
        }
    }

    fn input_key(self) -> &'static str {
        match self {
            Self::Search => "query",
            Self::Fetch => "url",
        }
    }

    fn error_type(self) -> &'static str {
        match self {
            Self::Search => "web_search_tool_result_error",
            Self::Fetch => "web_fetch_tool_result_error",
        }
    }

    fn result_type(self) -> &'static str {
        match self {
            Self::Search => "web_search_tool_result",
            Self::Fetch => "web_fetch_tool_result",
        }
    }
}

pub(crate) fn is_web_run(namespace: Option<&str>, name: &str) -> bool {
    namespace == Some(WEB_NAMESPACE) && name == RUN_TOOL_NAME
}

/// A `web.run` call as the client sees it.
#[derive(Debug)]
pub(crate) struct WebRunCall {
    pub(crate) call_id: String,
    pub(crate) kind: ServerToolKind,
    /// `server_tool_use.input`: the Anthropic field, then the call's own arguments.
    pub(crate) input: Value,
    commands: Result<SearchCommands, String>,
    fetch_url: Option<String>,
}

impl WebRunCall {
    /// A fetch is one `open` of a literal URL while `web_fetch` is enabled; any other call is
    /// a search.
    pub(crate) fn classify(call_id: String, arguments: &str, tools: &ToolSet) -> Self {
        let commands = parse_commands(arguments).map_err(|err| err.to_string());
        let fetch_url = commands.as_ref().ok().and_then(single_open_url);
        let kind = match (&fetch_url, &tools.web_fetch, &tools.web_search) {
            (Some(_), Some(_), _) | (_, Some(_), None) => ServerToolKind::Fetch,
            _ => ServerToolKind::Search,
        };
        let primary = match kind {
            ServerToolKind::Search => commands
                .as_ref()
                .map(|commands| web_search_action_detail(&search_action(commands)))
                .unwrap_or_default(),
            ServerToolKind::Fetch => fetch_url.clone().unwrap_or_default(),
        };
        let mut input = Map::new();
        input.insert(kind.input_key().to_string(), Value::String(primary));
        if let Ok(Value::Object(arguments)) = serde_json::from_str::<Value>(arguments) {
            input.extend(arguments);
        }
        Self {
            call_id,
            kind,
            input: Value::Object(input),
            commands,
            fetch_url,
        }
    }
}

fn single_open_url(commands: &SearchCommands) -> Option<String> {
    let [open] = commands.open.as_deref()? else {
        return None;
    };
    let only_open = SearchCommands {
        open: commands.open.clone(),
        response_length: commands.response_length,
        ..Default::default()
    };
    let url = Url::parse(&open.ref_id).ok()?;
    (only_open == *commands && matches!(url.scheme(), "http" | "https"))
        .then(|| open.ref_id.clone())
}

/// What one request may still run; Anthropic `max_uses` is per request.
#[derive(Debug, Default)]
pub(crate) struct ServerToolUses {
    pub(crate) search: u64,
    pub(crate) fetch: u64,
    /// Failed searches: counted against `max_uses`, not billed.
    pub(crate) failed_search: u64,
    pub(crate) advisor: u64,
}

impl ServerToolUses {
    /// Counts a use of `kind`; `false` once the tool's `max_uses` is spent.
    fn admit(&mut self, kind: ServerToolKind, tool: &ServerTool) -> bool {
        let used = match kind {
            ServerToolKind::Search => &mut self.search,
            ServerToolKind::Fetch => &mut self.fetch,
        };
        if tool.max_uses.is_some_and(|max_uses| *used >= max_uses) {
            return false;
        }
        *used += 1;
        true
    }
}

/// One planned call: run it, or answer with an Anthropic error code.
pub(crate) enum Plan<'a> {
    Run(&'a WebRunCall, &'a ServerTool, &'a SearchCommands),
    Refuse(&'a WebRunCall, &'static str, String),
}

/// Admits each call against its tool and `max_uses`, in call order.
pub(crate) fn plan<'a>(
    calls: &[&'a WebRunCall],
    tools: &'a ToolSet,
    uses: &mut ServerToolUses,
    fetchable: &str,
) -> Vec<Plan<'a>> {
    calls
        .iter()
        .map(|&call| {
            let tool = match call.kind {
                ServerToolKind::Search => tools.web_search.as_ref(),
                ServerToolKind::Fetch => tools.web_fetch.as_ref(),
            };
            match (&call.commands, tool) {
                (Err(err), _) => Plan::Refuse(call, "invalid_tool_input", err.clone()),
                (Ok(_), None) => Plan::Refuse(
                    call,
                    "invalid_tool_input",
                    "web.run is not enabled for this request".to_string(),
                ),
                (Ok(_), Some(_))
                    if call.kind == ServerToolKind::Fetch && call.fetch_url.is_none() =>
                {
                    Plan::Refuse(
                        call,
                        "invalid_tool_input",
                        "web.run: only `open` of one http(s) URL is enabled".to_string(),
                    )
                }
                (Ok(_), Some(_))
                    if call
                        .fetch_url
                        .as_deref()
                        .is_some_and(|url| call.kind == ServerToolKind::Fetch && !fetchable.contains(url)) =>
                {
                    Plan::Refuse(
                        call,
                        "url_not_in_prior_context",
                        "web.run: fetch only URLs from user messages, tool results, or earlier web results"
                            .to_string(),
                    )
                }
                (Ok(_), Some(tool)) if !uses.admit(call.kind, tool) => Plan::Refuse(
                    call,
                    "max_uses_exceeded",
                    format!(
                        "web.run: this request allows at most {} {} calls",
                        tool.max_uses.unwrap_or_default(),
                        call.kind.name()
                    ),
                ),
                (Ok(commands), Some(tool)) => Plan::Run(call, tool, commands),
            }
        })
        .collect()
}

/// Request identity a call's search carries.
pub(crate) struct SearchContext<'a> {
    pub(crate) search: &'a StandaloneSearch,
    pub(crate) session_id: &'a str,
    pub(crate) model: &'a str,
    pub(crate) originator: &'a str,
    /// `x-codex-turn-metadata` of the turn's current tool calls.
    pub(crate) turn_metadata: Option<String>,
    /// Text whose URLs fetches may open; grows with each result.
    pub(crate) fetchable: String,
    pub(crate) token_budget: u64,
}

impl SearchContext<'_> {
    /// The call's result; a failed search answers with its Anthropic error code.
    pub(crate) async fn run(&self, plan: &Plan<'_>, history: &[ResponseItem]) -> ServerResult {
        match plan {
            Plan::Refuse(call, code, message) => refusal(call, code, message),
            Plan::Run(call, tool, commands) => {
                match self.search(call, tool, commands, history).await {
                    Ok(result) => result,
                    Err(err) => ServerResult {
                        failed: true,
                        ..refusal(call, error_code(&err), &format!("web.run failed: {err}"))
                    },
                }
            }
        }
    }

    async fn search(
        &self,
        call: &WebRunCall,
        tool: &ServerTool,
        commands: &SearchCommands,
        history: &[ResponseItem],
    ) -> Result<ServerResult, SearchError> {
        let mut settings = self.search.settings().clone();
        if tool.allowed_domains.is_some() || tool.blocked_domains.is_some() {
            settings.filters = Some(SearchFilters {
                allowed_domains: tool.allowed_domains.clone(),
                blocked_domains: tool.blocked_domains.clone(),
            });
        }
        if let Some(location) = &tool.user_location {
            settings.user_location = Some(ApproximateLocation {
                r#type: LocationType::Approximate,
                country: location.country.clone(),
                region: location.region.clone(),
                city: location.city.clone(),
                timezone: location.timezone.clone(),
            });
        }
        let max_output_tokens = tool
            .max_content_tokens
            .map_or(self.token_budget, |limit| limit.min(self.token_budget));
        let request = search_request(
            self.session_id.to_string(),
            self.model.to_string(),
            history,
            commands.clone(),
            settings,
            max_output_tokens,
        );
        let response = self
            .search
            .connect()
            .await?
            .search(
                &request,
                Some(self.originator),
                self.turn_metadata.as_deref(),
            )
            .await?;
        let results = response.results.unwrap_or_default();
        let block = match call.kind {
            ServerToolKind::Search => json!({
                "type": "web_search_tool_result",
                "tool_use_id": call.call_id,
                "content": search_results(&results, &response.output),
            }),
            ServerToolKind::Fetch => json!({
                "type": "web_fetch_tool_result",
                "tool_use_id": call.call_id,
                "content": {
                    "type": "web_fetch_result",
                    "url": call.input[ServerToolKind::Fetch.input_key()],
                    "content": {
                        "type": "document",
                        "source": { "type": "text", "media_type": "text/plain", "data": response.output },
                        "title": results.iter().find_map(|result| result.get("title")),
                    },
                    "retrieved_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                },
            }),
        };
        Ok(ServerResult {
            block,
            output: output_item(&call.call_id, response.output),
            urls: results
                .iter()
                .filter_map(|result| result.get("url").and_then(Value::as_str))
                .map(str::to_string)
                .collect(),
            citations: match call.kind {
                ServerToolKind::Search => results.iter().filter_map(citation).collect(),
                ServerToolKind::Fetch => Vec::new(),
            },
            failed: false,
        })
    }
}

/// A search result as a `web_search_result_location`; the snippet is the cited text.
fn citation(result: &Value) -> Option<(String, Value)> {
    let url = result.get("url")?.as_str()?;
    let field = |key| result.get(key).and_then(Value::as_str).unwrap_or_default();
    let cited_text = field("snippet").chars().take(150).collect::<String>();
    Some((
        url.to_string(),
        json!({
            "type": "web_search_result_location",
            "url": url,
            "title": result.get("title").and_then(Value::as_str).unwrap_or(url),
            "encrypted_index": field("ref_id"),
            "cited_text": cited_text,
        }),
    ))
}

/// Citations of search results the conversation already holds.
pub(crate) fn echoed_citations(messages: &[InputMessage]) -> Vec<(String, Value)> {
    messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| match &message.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::WebSearchToolResult {
                content: WebSearchResultContent::Results(results),
                ..
            } => Some(results),
            _ => None,
        })
        .flatten()
        .map(|result| {
            (
                result.url.clone(),
                json!({
                    "type": "web_search_result_location",
                    "url": result.url,
                    "title": result.title,
                    "encrypted_index": "",
                    "cited_text": result.title,
                }),
            )
        })
        .collect()
}

/// `(end offset, URL)` of each complete Markdown link `[label](url)` in `text`.
pub(crate) fn markdown_links(text: &str) -> Vec<(usize, &str)> {
    let mut links = Vec::new();
    let mut from = 0;
    while let Some(open) = text[from..].find("](") {
        let url_start = from + open + 2;
        let Some(close) = text[url_start..].find(')') else {
            break;
        };
        let url = &text[url_start..url_start + close];
        let end = url_start + close + 1;
        if !url.is_empty() && !url.contains(char::is_whitespace) {
            links.push((end, url));
        }
        from = end;
    }
    links
}

fn error_code(err: &SearchError) -> &'static str {
    match err {
        SearchError::Api(
            SearchApiError::RateLimitExceeded { .. }
            | SearchApiError::RateLimit(_)
            | SearchApiError::QuotaExceeded,
        ) => "too_many_requests",
        SearchError::Api(SearchApiError::Api { status, .. }) if status.as_u16() == 429 => {
            "too_many_requests"
        }
        SearchError::Api(
            SearchApiError::InvalidRequest { .. } | SearchApiError::InvalidPrompt { .. },
        ) => "invalid_tool_input",
        SearchError::Api(_) | SearchError::Codex(_) | SearchError::Transport(_) => "unavailable",
    }
}

/// What a fetch may open: user text, client tool results, and web results, as the Messages
/// API allows.
pub(crate) fn fetchable_text(messages: &[InputMessage]) -> String {
    let mut text = String::new();
    for message in messages {
        let MessageContent::Blocks(blocks) = &message.content else {
            if message.role == Role::User
                && let MessageContent::Text(body) = &message.content
            {
                text.push_str(body);
                text.push('\n');
            }
            continue;
        };
        for block in blocks {
            match (message.role, block) {
                (Role::User, ContentBlock::Text { text: body }) => text.push_str(body),
                (
                    Role::User,
                    ContentBlock::ToolResult {
                        content: Some(content),
                        ..
                    },
                ) => match content {
                    ToolResultContent::Text(body) => text.push_str(body),
                    ToolResultContent::Blocks(parts) => {
                        for part in parts {
                            if let ToolResultBlock::Text { text: body } = part {
                                text.push_str(body);
                                text.push('\n');
                            }
                        }
                    }
                },
                (
                    Role::Assistant,
                    ContentBlock::WebSearchToolResult {
                        content: WebSearchResultContent::Results(results),
                        ..
                    },
                ) => {
                    for result in results {
                        text.push_str(&result.url);
                        text.push('\n');
                    }
                }
                (
                    Role::Assistant,
                    ContentBlock::WebFetchToolResult {
                        content: WebFetchResultContent::WebFetchResult { url, .. },
                        ..
                    },
                ) => text.push_str(url),
                _ => continue,
            }
            text.push('\n');
        }
    }
    text
}

/// Results with a URL, as `web_search_result`s; the first carries the call's output.
fn search_results(results: &[Value], output: &str) -> Vec<Value> {
    results
        .iter()
        .filter_map(|result| {
            let url = result.get("url")?.as_str()?;
            let title = result.get("title").and_then(Value::as_str).unwrap_or(url);
            let reference = result
                .get("ref_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some((url, title, reference))
        })
        .enumerate()
        .map(|(index, (url, title, reference))| {
            let encrypted_content = if index == 0 {
                format!("{OUTPUT_PREFIX}{}", URL_SAFE_NO_PAD.encode(output))
            } else {
                reference.to_string()
            };
            json!({
                "type": "web_search_result",
                "url": url,
                "title": title,
                "encrypted_content": encrypted_content,
                "page_age": null,
            })
        })
        .collect()
}

fn refusal(call: &WebRunCall, code: &str, message: &str) -> ServerResult {
    ServerResult {
        urls: Vec::new(),
        citations: Vec::new(),
        failed: false,
        block: json!({
            "type": call.kind.result_type(),
            "tool_use_id": call.call_id,
            "content": { "type": call.kind.error_type(), "error_code": code },
        }),
        output: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call.call_id.clone()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                success: Some(false),
                ..FunctionCallOutputPayload::from_text(message.to_string())
            },
            internal_chat_message_metadata_passthrough: None,
        },
    }
}

fn output_item(call_id: &str, output: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText { text: output },
        ]),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// The `web.run` call behind an echoed `server_tool_use`; inputs from other backends carry
/// only the Anthropic field and become the equivalent command.
pub(crate) fn replay_call(id: String, name: &str, input: Value) -> Result<ResponseItem, ApiError> {
    let kind = match name {
        WEB_SEARCH => ServerToolKind::Search,
        WEB_FETCH => ServerToolKind::Fetch,
        other => {
            return Err(ApiError::invalid_request(format!(
                "messages: server tool `{other}` is not supported by the Codex backend"
            )));
        }
    };
    let Value::Object(mut input) = input else {
        return Err(ApiError::invalid_request(format!(
            "messages: `{name}` input must be an object"
        )));
    };
    let primary = input.remove(kind.input_key());
    let arguments = if input.is_empty() {
        let primary = primary.unwrap_or_default();
        match kind {
            ServerToolKind::Search => json!({ "search_query": [{ "q": primary }] }),
            ServerToolKind::Fetch => json!({ "open": [{ "ref_id": primary }] }),
        }
    } else {
        Value::Object(input)
    };
    Ok(ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: None,
        name: RUN_TOOL_NAME.to_string(),
        namespace: Some(WEB_NAMESPACE.to_string()),
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: id,
        internal_chat_message_metadata_passthrough: None,
    })
}

/// The output behind an echoed `web_search_tool_result`.
pub(crate) fn replay_search_output(
    tool_use_id: String,
    content: WebSearchResultContent,
) -> ResponseItem {
    let text = match content {
        WebSearchResultContent::Error(error) => error.error_code,
        WebSearchResultContent::Results(results) => {
            let carried = results.first().and_then(|result| {
                let payload = result
                    .encrypted_content
                    .as_deref()?
                    .strip_prefix(OUTPUT_PREFIX)?;
                String::from_utf8(URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
            });
            carried.unwrap_or_else(|| {
                results
                    .iter()
                    .map(|result| format!("{} ({})", result.title, result.url))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
    };
    output_item(&tool_use_id, text)
}

/// The output behind an echoed `web_fetch_tool_result`.
pub(crate) fn replay_fetch_output(
    tool_use_id: String,
    content: WebFetchResultContent,
) -> Result<ResponseItem, ApiError> {
    let item = match content {
        WebFetchResultContent::WebFetchToolResultError { error_code } => {
            FunctionCallOutputContentItem::InputText { text: error_code }
        }
        WebFetchResultContent::WebFetchResult { content, url } => match content.source {
            DocumentSource::Text { data } => {
                FunctionCallOutputContentItem::InputText { text: data }
            }
            DocumentSource::Base64 { media_type, data } => {
                FunctionCallOutputContentItem::InputFile {
                    file: FileReference::Inline {
                        filename: url
                            .rsplit('/')
                            .find(|part| !part.is_empty())
                            .unwrap_or("document")
                            .to_string(),
                        file_data: format!("data:{media_type};base64,{data}"),
                    },
                }
            }
            DocumentSource::Content { .. }
            | DocumentSource::Url { .. }
            | DocumentSource::File {} => {
                return Err(ApiError::invalid_request(
                    "web_fetch_tool_result: document source not supported by the Codex backend",
                ));
            }
        },
    };
    Ok(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(tool_use_id),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_content_items(vec![item]),
        internal_chat_message_metadata_passthrough: None,
    })
}

#[cfg(test)]
#[path = "web_run_tests.rs"]
mod tests;
