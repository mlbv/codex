use codex_api::SearchCommands;
use codex_api::SearchQuery;
use codex_core::web_search_action_detail;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_items::ExtensionItem;
use codex_extension_items::web_search::WebSearchAction;
use codex_extension_items::web_search::WebSearchItem;
use codex_protocol::models::WebSearchAction as CoreWebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_tools::ToolExposure;
use url::Url;

use crate::output::SearchOutput;
use crate::search::RUN_TOOL_NAME;
use crate::search::StandaloneSearch;
use crate::search::WEB_NAMESPACE;
use crate::search::search_request;
use crate::search::web_run_spec;

const RESULTS_PAYLOAD_BYTES_METRIC: &str = "codex.web_search.results.payload_bytes";

pub(crate) struct WebSearchTool {
    pub(crate) session_id: String,
    pub(crate) search: StandaloneSearch,
    pub(crate) originator: Option<String>,
}

impl<'call> ToolExecutor<ToolCall<'call>> for WebSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(WEB_NAMESPACE, RUN_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        web_run_spec()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(self.handle_call(call))
    }
}

impl WebSearchTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let commands = parse_commands(&call)?;
        let command_action = command_action(&commands);
        let connection = self
            .search
            .connect()
            .await
            .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        let request = search_request(
            self.session_id.clone(),
            call.model.clone(),
            call.conversation_history.items(),
            commands,
            self.search.settings().clone(),
            u64::try_from(call.truncation_policy.token_budget()).unwrap_or(u64::MAX),
        );
        call.turn_item_emitter
            .emit_started(extension_turn_item(
                WebSearchItem {
                    id: call.call_id.clone(),
                    query: String::new(),
                    action: None,
                    results: None,
                },
                EventMsg::WebSearchBegin(WebSearchBeginEvent {
                    call_id: call.call_id.clone(),
                }),
            ))
            .await;
        let response = connection
            .search(
                &request,
                self.originator.as_deref(),
                call.codex_turn_metadata.as_deref(),
            )
            .await
            .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        let output = response.output;
        let results = response.results;
        if let Some(results) = results.as_ref()
            && let Some(metrics) = codex_otel::global()
            && let Ok(payload) = serde_json::to_vec(results)
        {
            let payload_bytes = i64::try_from(payload.len()).unwrap_or(i64::MAX);
            let _ = metrics.histogram(RESULTS_PAYLOAD_BYTES_METRIC, payload_bytes, &[]);
        }
        let legacy_action = legacy_action(&command_action);
        let query = web_search_action_detail(&legacy_action);
        call.turn_item_emitter
            .emit_completed(extension_turn_item(
                WebSearchItem {
                    id: call.call_id.clone(),
                    query: query.clone(),
                    action: Some(command_action),
                    results: results.clone(),
                },
                EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: call.call_id.clone(),
                    query,
                    action: legacy_action,
                    results,
                }),
            ))
            .await;

        Ok(Box::new(SearchOutput::new(output)))
    }
}

fn parse_commands(call: &ToolCall<'_>) -> Result<SearchCommands, FunctionCallError> {
    crate::search::parse_commands(call.function_arguments()?)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

/// The protocol's form of a call's action, as Codex's search events report it.
pub fn search_action(commands: &SearchCommands) -> CoreWebSearchAction {
    legacy_action(&command_action(commands))
}

fn legacy_action(action: &WebSearchAction) -> CoreWebSearchAction {
    match action {
        WebSearchAction::Search { query, queries } => CoreWebSearchAction::Search {
            query: query.clone(),
            queries: queries.clone(),
        },
        WebSearchAction::OpenPage { url } => CoreWebSearchAction::OpenPage { url: url.clone() },
        WebSearchAction::FindInPage { url, pattern } => CoreWebSearchAction::FindInPage {
            url: url.clone(),
            pattern: pattern.clone(),
        },
        WebSearchAction::Other => CoreWebSearchAction::Other,
    }
}

fn command_action(commands: &SearchCommands) -> WebSearchAction {
    commands
        .search_query
        .as_deref()
        .and_then(query_action)
        .or_else(|| commands.image_query.as_deref().and_then(query_action))
        .or_else(|| {
            commands
                .open
                .as_deref()
                .and_then(|operations| operations.first())
                .and_then(|operation| {
                    literal_url(&operation.ref_id)
                        .map(|url| WebSearchAction::OpenPage { url: Some(url) })
                })
        })
        .or_else(|| {
            commands
                .find
                .as_deref()
                .and_then(|operations| operations.first())
                .map(|operation| WebSearchAction::FindInPage {
                    url: literal_url(&operation.ref_id),
                    pattern: Some(operation.pattern.clone()),
                })
        })
        .unwrap_or(WebSearchAction::Other)
}

fn query_action(queries: &[SearchQuery]) -> Option<WebSearchAction> {
    match queries {
        [] => None,
        [query] => Some(WebSearchAction::Search {
            query: Some(query.q.clone()),
            queries: None,
        }),
        queries => Some(WebSearchAction::Search {
            query: None,
            queries: Some(queries.iter().map(|query| query.q.clone()).collect()),
        }),
    }
}

fn literal_url(ref_id: &str) -> Option<String> {
    Url::parse(ref_id).is_ok().then(|| ref_id.to_string())
}

fn extension_turn_item(item: WebSearchItem, legacy_event: EventMsg) -> ExtensionTurnItem {
    ExtensionTurnItem {
        item: ExtensionItem::WebSearch(item),
        legacy_events: vec![legacy_event],
    }
}

#[cfg(test)]
mod tests {
    use codex_api::SearchCommands;
    use codex_extension_items::web_search::WebSearchAction;
    use pretty_assertions::assert_eq;

    use super::command_action;
    use crate::search::search_request_headers;
    use codex_core::X_CODEX_TURN_METADATA_HEADER;

    #[test]
    fn search_request_headers_forward_thread_originator_and_turn_metadata() {
        let headers = search_request_headers(Some("chatgpt_cca"), Some("turn-metadata"));
        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some("chatgpt_cca")
        );
        assert_eq!(
            headers
                .get(X_CODEX_TURN_METADATA_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("turn-metadata")
        );
    }

    #[test]
    fn command_action_reports_queries_and_navigation_detail() {
        let cases = [
            (
                r#"{"image_query":[{"q":"waterfalls"},{"q":"mountains"}]}"#,
                WebSearchAction::Search {
                    query: None,
                    queries: Some(vec!["waterfalls".to_string(), "mountains".to_string()]),
                },
            ),
            (
                r#"{"open":[{"ref_id":"https://example.com/docs"}]}"#,
                WebSearchAction::OpenPage {
                    url: Some("https://example.com/docs".to_string()),
                },
            ),
            (
                r#"{"find":[{"ref_id":"https://example.com/docs","pattern":"install"}]}"#,
                WebSearchAction::FindInPage {
                    url: Some("https://example.com/docs".to_string()),
                    pattern: Some("install".to_string()),
                },
            ),
            (
                r#"{"find":[{"ref_id":"turn0search0","pattern":"install"}]}"#,
                WebSearchAction::FindInPage {
                    url: None,
                    pattern: Some("install".to_string()),
                },
            ),
            (
                r#"{"open":[{"ref_id":"turn0search0"}]}"#,
                WebSearchAction::Other,
            ),
        ];

        for (arguments, expected) in cases {
            let commands: SearchCommands =
                serde_json::from_str(arguments).expect("valid search command arguments");
            assert_eq!(command_action(&commands), expected);
        }
    }
}
