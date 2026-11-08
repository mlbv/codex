//! Native Code Mode cells whose nested tools execute at the Messages API client.

use std::cell::LazyCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::ExecuteToPendingOutcome;
use codex_code_mode::NotificationFuture;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::TerminateRequest;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitRequest;
use codex_code_mode::WaitToPendingOutcome;
use codex_code_mode::WaitToPendingRequest;
use codex_code_mode_runtime::InProcessCodeModeSession;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_function_output_items_with_policy;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExecutorState {
    cells: Vec<Cell>,
}

/// A session-log delta, without the round results its record's lane history reproduces.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct ExecutorUpdate {
    keep: usize,
    append: Vec<Cell>,
    /// Clear history positions on this many leading cells before applying the splice.
    #[serde(default, skip_serializing_if = "is_zero")]
    compact: usize,
    /// Amend the last kept cell without repeating its request or earlier rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    amend: Option<CellUpdate>,
    /// Leading `append` cells that replace cells the log already holds.
    #[serde(skip)]
    relogged: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Serialize, Deserialize)]
struct Splice<T> {
    keep: usize,
    append: Vec<T>,
}

#[derive(Serialize, Deserialize)]
struct CellUpdate {
    rounds: Splice<Round>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    results: Option<RoundResults>,
    response: Option<RuntimeResponse>,
    output: Option<ResponseItem>,
}

#[derive(Serialize, Deserialize)]
struct RoundResults {
    value: Option<Vec<Value>>,
}

/// Catalogs are inline in standalone snapshots and references in session-log updates.
#[derive(Clone)]
struct ToolCatalog {
    id: String,
    tools: Option<Arc<Vec<ToolDefinition>>>,
}

impl ToolCatalog {
    fn new(tools: Vec<ToolDefinition>) -> Self {
        let id = format!("{:x}", Sha256::digest(serde_json::to_vec(&tools).unwrap()));
        Self {
            id,
            tools: Some(Arc::new(tools)),
        }
    }
}

impl PartialEq for ToolCatalog {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Serialize for ToolCatalog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.tools {
            Some(tools) => tools.serialize(serializer),
            None => self.id.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolCatalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Catalog {
            Inline(Vec<ToolDefinition>),
            Reference(String),
        }
        Ok(match Catalog::deserialize(deserializer)? {
            Catalog::Inline(tools) => Self::new(tools),
            Catalog::Reference(id) => Self { id, tools: None },
        })
    }
}

pub(crate) type ToolCatalogs = HashMap<String, Arc<Vec<ToolDefinition>>>;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct ExecArgs {
    tool_call_id: String,
    enabled_tools: ToolCatalog,
    source: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

impl ExecArgs {
    fn runtime_request(&self) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: self.tool_call_id.clone(),
            enabled_tools: self.enabled_tools.tools.as_ref().unwrap().as_ref().clone(),
            source: self.source.clone(),
            yield_time_ms: self.yield_time_ms,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Cell {
    /// None after compaction; operations still restore live cells and store/load state.
    at: Option<usize>,
    call: ResponseItem,
    request: Option<Operation>,
    rounds: Vec<Round>,
    /// Terminal responses keep only their status; `output` holds their content.
    response: Option<RuntimeResponse>,
    output: Option<ResponseItem>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
enum Operation {
    Exec(ExecArgs),
    Wait(WaitArgs),
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct WaitArgs {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS
}

impl Operation {
    fn max_output_tokens(&self) -> Option<usize> {
        match self {
            Self::Exec(request) => request.max_output_tokens,
            Self::Wait(request) => request.max_tokens,
        }
    }

    fn yield_time_ms(&self) -> u64 {
        match self {
            Self::Exec(request) => request
                .yield_time_ms
                .unwrap_or(codex_code_mode::DEFAULT_EXEC_YIELD_TIME_MS),
            Self::Wait(request) => request.yield_time_ms,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Round {
    content: Vec<codex_code_mode::FunctionCallOutputContentItem>,
    calls: Vec<ResponseItem>,
    /// In session-log updates, a visible cell's results that its record's lane history
    /// reproduces are null, which no tool result is.
    results: Option<Vec<Value>>,
}

#[derive(Default)]
pub(crate) struct Executor {
    state: ExecutorState,
    live: Option<Live>,
}

struct Live {
    session: InProcessCodeModeSession,
    scripts: HashMap<CellId, Arc<Delegate>>,
    pending: Option<Pending>,
    unobserved: Option<Observation>,
}

struct Pending {
    cell_id: CellId,
    delegate: Arc<Delegate>,
    calls: Vec<Invocation>,
}

enum Observation {
    Frontier {
        content: Vec<codex_code_mode::FunctionCallOutputContentItem>,
    },
    Response(RuntimeResponse),
}

#[derive(Default)]
struct Delegate {
    pending: Mutex<HashMap<String, Invocation>>,
    notifications: Mutex<Vec<String>>,
    preempt: Mutex<Option<CancellationToken>>,
    ready: Notify,
}

struct Invocation {
    call: CodeModeNestedToolCall,
    result: oneshot::Sender<Result<Value, String>>,
}

impl Cell {
    fn same_operation(&self, previous: &Self, compact: bool) -> bool {
        self.at == if compact { None } else { previous.at }
            && self.call == previous.call
            && self.request == previous.request
    }

    fn logged(&self) -> Self {
        Self {
            at: self.at,
            call: self.call.clone(),
            request: self.request.clone(),
            rounds: self
                .rounds
                .iter()
                .map(|round| round.logged(self.at.is_some()))
                .collect(),
            response: self.response.clone(),
            output: self.output.clone(),
        }
    }
}

impl Round {
    fn logged(&self, visible: bool) -> Self {
        Self {
            content: self.content.clone(),
            calls: self.calls.clone(),
            results: self
                .results
                .as_deref()
                .map(|results| logged_results(results, visible)),
        }
    }

    fn resolve(&mut self, outputs: &Outputs<'_>) -> Result<(), String> {
        match &mut self.results {
            Some(results) => resolve_results(results, &self.calls, outputs),
            None => Ok(()),
        }
    }
}

/// The session log's lane history keeps no output `success`, so it reproduces every result
/// except those `nested_result` marks `isError`.
fn logged_results(results: &[Value], visible: bool) -> Vec<Value> {
    if !visible {
        return results.to_vec();
    }
    results
        .iter()
        .map(|result| match result.get("isError") {
            Some(Value::Bool(true)) => result.clone(),
            _ => Value::Null,
        })
        .collect()
}

fn resolve_results(
    results: &mut [Value],
    calls: &[ResponseItem],
    outputs: &Outputs<'_>,
) -> Result<(), String> {
    for (result, call) in results.iter_mut().zip(calls) {
        if result.is_null() {
            *result = outputs.answer(call)?;
        }
    }
    Ok(())
}

/// A history's tool outputs by call id.
struct Outputs<'a>(HashMap<&'a str, &'a ResponseItem>);

impl<'a> Outputs<'a> {
    fn new(history: &'a [ResponseItem]) -> Self {
        Self(
            history
                .iter()
                .filter(|item| is_result(item))
                .map(|item| (item_call_id(item).unwrap(), item))
                .collect(),
        )
    }

    /// The value JavaScript receives for `call`.
    fn answer(&self, call: &ResponseItem) -> Result<Value, String> {
        let call_id = item_call_id(call).unwrap();
        self.0
            .get(call_id)
            .map(|item| match item {
                ResponseItem::FunctionCallOutput { output, .. }
                | ResponseItem::CustomToolCallOutput { output, .. } => {
                    nested_result(output.clone())
                }
                ResponseItem::ToolSearchOutput { tools, .. } => Value::Array(tools.clone()),
                _ => unreachable!(),
            })
            .ok_or_else(|| format!("missing executor tool result: {call_id}"))
    }
}

impl CellUpdate {
    fn between(current: &Cell, previous: &Cell) -> Self {
        let visible = current.at.is_some();
        let mut keep = current
            .rounds
            .iter()
            .zip(&previous.rounds)
            .take_while(|(current, old)| current == old)
            .count();
        let results = current
            .rounds
            .get(keep)
            .zip(previous.rounds.get(keep))
            .filter(|(current, old)| current.content == old.content && current.calls == old.calls)
            .map(|(current, _)| RoundResults {
                value: current
                    .results
                    .as_deref()
                    .map(|results| logged_results(results, visible)),
            });
        if results.is_some() {
            keep += 1;
        }
        Self {
            rounds: Splice {
                keep,
                append: current.rounds[keep..]
                    .iter()
                    .map(|round| round.logged(visible))
                    .collect(),
            },
            results,
            response: current.response.clone(),
            output: current.output.clone(),
        }
    }

    fn apply(self, cell: &mut Cell) {
        cell.rounds.truncate(self.rounds.keep);
        if let Some(results) = self.results {
            cell.rounds.last_mut().unwrap().results = results.value;
        }
        cell.rounds.extend(self.rounds.append);
        cell.response = self.response;
        cell.output = self.output;
    }
}

impl ExecutorUpdate {
    /// The size of the cells this update re-logs, as the log records them.
    pub(crate) fn relogged_bytes(&self) -> usize {
        match self.relogged {
            0 => 0,
            relogged => serde_json::to_vec(&self.append[..relogged]).unwrap().len(),
        }
    }

    /// Replaces inline catalogs with durable references and returns new definitions.
    pub(crate) fn reference_catalogs(&mut self, catalogs: &mut ToolCatalogs) -> ToolCatalogs {
        let mut added = ToolCatalogs::new();
        for cell in &mut self.append {
            let Some(Operation::Exec(request)) = &mut cell.request else {
                continue;
            };
            let catalog = &mut request.enabled_tools;
            let tools = catalog.tools.take().unwrap();
            if !catalogs.contains_key(&catalog.id) {
                catalogs.insert(catalog.id.clone(), Arc::clone(&tools));
                added.insert(catalog.id.clone(), tools);
            }
        }
        added
    }

    /// Resolves references and shares legacy inline catalogs before applying a record.
    pub(crate) fn resolve_catalogs(&mut self, catalogs: &mut ToolCatalogs) -> Result<(), String> {
        for cell in &mut self.append {
            let Some(Operation::Exec(request)) = &mut cell.request else {
                continue;
            };
            let catalog = &mut request.enabled_tools;
            let tools = match &catalog.tools {
                Some(tools) => Arc::clone(
                    catalogs
                        .entry(catalog.id.clone())
                        .or_insert_with(|| Arc::clone(tools)),
                ),
                None => Arc::clone(
                    catalogs
                        .get(&catalog.id)
                        .ok_or_else(|| format!("unknown executor tool catalog: {}", catalog.id))?,
                ),
            };
            catalog.tools = Some(tools);
        }
        Ok(())
    }
}

impl ExecutorState {
    fn intern_catalog(&self, tools: Vec<ToolDefinition>) -> ToolCatalog {
        let mut catalog = ToolCatalog::new(tools);
        if let Some(existing) = self.cells.iter().find_map(|cell| match &cell.request {
            Some(Operation::Exec(request)) if request.enabled_tools.id == catalog.id => {
                request.enabled_tools.tools.as_ref()
            }
            _ => None,
        }) {
            catalog.tools = Some(Arc::clone(existing));
        }
        catalog
    }

    pub(crate) fn update(&self, previous: &Self) -> ExecutorUpdate {
        let compact = self
            .cells
            .iter()
            .take_while(|cell| cell.at.is_none())
            .count()
            .min(previous.cells.len());
        let compact = if previous.cells[..compact]
            .iter()
            .any(|cell| cell.at.is_some())
        {
            compact
        } else {
            0
        };
        let mut keep = self
            .cells
            .iter()
            .zip(&previous.cells)
            .enumerate()
            .take_while(|(index, (current, old))| {
                current.same_operation(old, *index < compact)
                    && current.rounds == old.rounds
                    && current.response == old.response
                    && current.output == old.output
            })
            .count();
        let amend = self
            .cells
            .get(keep)
            .zip(previous.cells.get(keep))
            .filter(|(current, old)| current.same_operation(old, keep < compact))
            .map(|(current, old)| CellUpdate::between(current, old));
        if amend.is_some() {
            keep += 1;
        }
        ExecutorUpdate {
            keep,
            append: self.cells[keep..].iter().map(Cell::logged).collect(),
            compact,
            amend,
            relogged: (previous.cells.len() - keep).min(self.cells.len() - keep),
        }
    }

    /// Applies an update whose record leaves the lane history `history`.
    pub(crate) fn apply(
        &mut self,
        mut update: ExecutorUpdate,
        history: &[ResponseItem],
    ) -> Result<(), String> {
        let outputs = LazyCell::new(|| Outputs::new(history));
        if let Some(amend) = &mut update.amend {
            if let Some(RoundResults {
                value: Some(results),
            }) = &mut amend.results
            {
                let round = &self.cells[update.keep - 1].rounds[amend.rounds.keep - 1];
                resolve_results(results, &round.calls, &outputs)?;
            }
            for round in &mut amend.rounds.append {
                round.resolve(&outputs)?;
            }
        }
        for round in update.append.iter_mut().flat_map(|cell| &mut cell.rounds) {
            round.resolve(&outputs)?;
        }
        // TODO(0.168): drop with session logs whose terminal responses repeat their output.
        for response in update
            .amend
            .iter_mut()
            .map(|amend| &mut amend.response)
            .chain(update.append.iter_mut().map(|cell| &mut cell.response))
            .flatten()
        {
            settle(response);
        }
        for cell in self.cells.iter_mut().take(update.compact) {
            cell.at = None;
        }
        self.cells.truncate(update.keep);
        if let Some(amend) = update.amend {
            amend.apply(self.cells.last_mut().unwrap());
        }
        self.cells.extend(update.append);
        Ok(())
    }

    /// Copies completed cells and the pending frontier visible in a history prefix.
    pub(crate) fn prefix(&self, history: &[ResponseItem]) -> Self {
        let ids = history
            .iter()
            .filter_map(item_call_id)
            .collect::<HashSet<_>>();
        let results = history
            .iter()
            .filter(|item| is_result(item))
            .filter_map(item_call_id)
            .collect::<HashSet<_>>();
        let mut cells = Vec::new();
        for cell in &self.cells {
            if cell.at.is_some_and(|at| at > history.len()) {
                break;
            }
            let mut cell = cell.clone();
            if cell.at.is_none() {
                cells.push(cell);
                continue;
            }
            let mut rounds = Vec::new();
            let mut complete = true;
            for mut round in cell.rounds {
                if !round
                    .calls
                    .iter()
                    .all(|call| ids.contains(item_call_id(call).unwrap()))
                {
                    complete = false;
                    break;
                }
                if round.results.is_some()
                    && !round
                        .calls
                        .iter()
                        .all(|call| results.contains(item_call_id(call).unwrap()))
                {
                    round.results = None;
                    complete = false;
                }
                rounds.push(round);
                if !complete {
                    break;
                }
            }
            if !complete {
                if rounds.is_empty() {
                    break;
                }
                cell.output = None;
                cell.response = None;
            }
            cell.rounds = rounds;
            cells.push(cell);
            if !complete {
                break;
            }
        }
        Self { cells }
    }
}

impl Executor {
    pub(crate) fn restore(state: ExecutorState) -> Self {
        Self { state, live: None }
    }

    pub(crate) fn state(&self) -> &ExecutorState {
        &self.state
    }

    pub(crate) fn pending(&self) -> bool {
        self.state
            .cells
            .last()
            .is_some_and(|cell| cell.output.is_none())
    }

    pub(crate) fn retain_prefix(&mut self, history: &[ResponseItem]) {
        let prefix = self.state.prefix(history);
        if prefix != self.state {
            self.state = prefix;
            self.live = None;
        }
    }

    pub(crate) fn compact(&mut self) {
        for cell in &mut self.state.cells {
            cell.at = None;
        }
    }

    /// Replaces client-facing nested calls/results with the model's original exec exchange.
    pub(crate) fn project(&self, history: &[ResponseItem]) -> Vec<ResponseItem> {
        let hidden = self
            .state
            .cells
            .iter()
            .filter(|cell| cell.at.is_some())
            .flat_map(|cell| &cell.rounds)
            .flat_map(|round| &round.calls)
            .filter_map(item_call_id)
            .collect::<HashSet<_>>();
        let mut projected = Vec::new();
        let mut cells = self
            .state
            .cells
            .iter()
            .filter(|cell| cell.at.is_some())
            .peekable();
        for index in 0..=history.len() {
            while cells.peek().is_some_and(|cell| cell.at == Some(index)) {
                let cell = cells.next().unwrap();
                projected.push(cell.call.clone());
                projected.extend(cell.output.iter().cloned());
            }
            if let Some(item) = history.get(index)
                && item_call_id(item).is_none_or(|id| !hidden.contains(id))
            {
                projected.push(item.clone());
            }
        }
        projected
    }

    /// Returns a client frontier, or an empty list when the cell completed internally.
    pub(crate) async fn start(
        &mut self,
        call: ResponseItem,
        enabled_tools: Vec<ToolDefinition>,
        visible_at: usize,
    ) -> Result<Vec<ResponseItem>, String> {
        self.ensure_live().await?;
        let request = match &call {
            ResponseItem::FunctionCall {
                name, arguments, ..
            } if name == "wait" => serde_json::from_str::<WaitArgs>(arguments)
                .map(Operation::Wait)
                .map_err(|err| format!("failed to parse function arguments: {err}")),
            ResponseItem::CustomToolCall { call_id, input, .. }
            | ResponseItem::FunctionCall {
                call_id,
                arguments: input,
                ..
            } => codex_code_mode::parse_exec_source(input).map(|args| {
                Operation::Exec(ExecArgs {
                    tool_call_id: call_id.clone(),
                    enabled_tools: self.state.intern_catalog(enabled_tools),
                    source: args.code,
                    yield_time_ms: args.yield_time_ms,
                    max_output_tokens: args.max_output_tokens,
                })
            }),
            _ => unreachable!("executor receives native exec or wait"),
        };
        self.state.cells.push(Cell {
            at: Some(visible_at),
            call,
            request: request.as_ref().ok().cloned(),
            rounds: Vec::new(),
            response: None,
            output: None,
        });
        match request {
            Ok(request) => {
                let outcome = self.live.as_mut().unwrap().run(&request).await?;
                self.observe(outcome)
            }
            Err(error) => {
                let cell = self.state.cells.last_mut().unwrap();
                cell.output = Some(output_item(
                    &cell.call,
                    vec![FunctionCallOutputContentItem::InputText { text: error }],
                    false,
                ));
                Ok(Vec::new())
            }
        }
    }

    /// Feeds the current frontier's client results to JavaScript before any model follow-up.
    pub(crate) async fn resume(
        &mut self,
        history: &[ResponseItem],
    ) -> Result<Vec<ResponseItem>, String> {
        if !self.pending() {
            return Ok(Vec::new());
        }
        self.ensure_live().await?;
        if let Some(outcome) = self.live.as_mut().unwrap().unobserved.take() {
            return self.observe(outcome);
        }
        let cell = self.state.cells.last_mut().unwrap();
        let round = cell.rounds.last_mut().unwrap();
        let outputs = Outputs::new(history);
        let results = round
            .calls
            .iter()
            .map(|call| outputs.answer(call))
            .collect::<Result<Vec<_>, _>>()?;
        round.results = Some(results.clone());
        let outcome = self
            .live
            .as_mut()
            .unwrap()
            .answer(results, cell.request.as_ref().unwrap())
            .await?;
        self.observe(outcome)
    }

    fn observe(&mut self, outcome: Observation) -> Result<Vec<ResponseItem>, String> {
        let live = self.live.as_mut().unwrap();
        let cell = self.state.cells.last_mut().unwrap();
        match outcome {
            Observation::Frontier { content } => {
                let offset = cell
                    .rounds
                    .iter()
                    .map(|round| round.calls.len())
                    .sum::<usize>();
                let calls = live
                    .pending
                    .as_ref()
                    .unwrap()
                    .calls
                    .iter()
                    .enumerate()
                    .map(|(index, invocation)| {
                        client_call(
                            &invocation.call,
                            format!(
                                "cm_{}_{ordinal}",
                                item_call_id(&cell.call).unwrap(),
                                ordinal = offset + index
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                cell.rounds.push(Round {
                    content,
                    calls: calls.clone(),
                    results: None,
                });
                Ok(calls)
            }
            Observation::Response(mut response) => {
                let status = match &response {
                    RuntimeResponse::Yielded { cell_id, .. } => {
                        format!("Script running with cell ID {cell_id}")
                    }
                    RuntimeResponse::Terminated { .. } => "Script terminated".to_string(),
                    RuntimeResponse::Result {
                        error_text: None, ..
                    } => "Script completed".to_string(),
                    RuntimeResponse::Result { .. } => "Script failed".to_string(),
                };
                let error_text = match &response {
                    RuntimeResponse::Result { error_text, .. } => error_text.clone(),
                    _ => None,
                };
                let success = error_text.is_none();
                let mut items = cell
                    .rounds
                    .iter()
                    .flat_map(|round| &round.content)
                    .chain(response_content_ref(&response))
                    .cloned()
                    .map(protocol_content)
                    .collect::<Vec<_>>();
                if let Some(error_text) = error_text {
                    items.push(FunctionCallOutputContentItem::InputText {
                        text: format!("Script error:\n{error_text}"),
                    });
                }
                let budget = cell
                    .request
                    .as_ref()
                    .and_then(Operation::max_output_tokens)
                    .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
                let mut items = formatted_truncate_function_output_items_with_policy(
                    items,
                    TruncationPolicy::Tokens(budget),
                    estimate_audio_token_count,
                );
                items.insert(
                    0,
                    FunctionCallOutputContentItem::InputText {
                        text: format!("{status}\nOutput:\n"),
                    },
                );
                settle(&mut response);
                cell.response = Some(response);
                cell.output = Some(output_item(&cell.call, items, success));
                Ok(Vec::new())
            }
        }
    }

    async fn ensure_live(&mut self) -> Result<(), String> {
        if self.live.is_some() {
            return Ok(());
        }
        let mut live = Live {
            session: InProcessCodeModeSession::new(),
            scripts: HashMap::new(),
            pending: None,
            unobserved: None,
        };
        for cell in &self.state.cells {
            let Some(request) = &cell.request else {
                continue;
            };
            let mut outcome = live.run(request).await?;
            let terminal = matches!(
                &cell.response,
                Some(RuntimeResponse::Result { .. } | RuntimeResponse::Terminated { .. })
            );
            for round in &cell.rounds {
                let Observation::Frontier { content } = &outcome else {
                    return Err(
                        "executor replay completed before its recorded frontier".to_string()
                    );
                };
                let pending = &live.pending.as_ref().unwrap().calls;
                if (!terminal && *content != round.content)
                    || pending.len() != round.calls.len()
                    || !pending.iter().zip(&round.calls).all(|(pending, call)| {
                        client_call(&pending.call, item_call_id(call).unwrap().to_string()) == *call
                    })
                {
                    return Err(
                        "executor replay differs from its recorded tool frontier".to_string()
                    );
                }
                let Some(results) = &round.results else {
                    self.live = Some(live);
                    return Ok(());
                };
                outcome = live.answer(results.clone(), request).await?;
            }
            if cell.output.is_none() {
                live.unobserved = Some(outcome);
                self.live = Some(live);
                return Ok(());
            }
            let Observation::Response(response) = outcome else {
                return Err("executor replay requested an unrecorded tool call".to_string());
            };
            if !same_response_status(&response, cell.response.as_ref().unwrap()) {
                return Err("executor replay differs from its recorded cell lifecycle".to_string());
            }
        }
        self.live = Some(live);
        Ok(())
    }
}

impl Live {
    async fn run(&mut self, operation: &Operation) -> Result<Observation, String> {
        match operation {
            Operation::Exec(request) => {
                let delegate = Arc::new(Delegate::default());
                let preempt = delegate.arm();
                let started = self
                    .session
                    .execute(
                        request.runtime_request(),
                        delegate.clone(),
                        Some(preempt.clone()),
                    )
                    .await?;
                self.scripts
                    .insert(started.cell_id.clone(), delegate.clone());
                let response = started.initial_response().await?;
                self.collect(response, Some(delegate), preempt.is_cancelled())
                    .await
            }
            Operation::Wait(request) => {
                let cell_id = CellId::new(request.cell_id.clone());
                let delegate = self.scripts.get(&cell_id).cloned();
                if request.terminate {
                    let response = self
                        .session
                        .terminate(TerminateRequest {
                            cell_id,
                            max_output_tokens: request.max_tokens,
                        })
                        .await?
                        .into();
                    self.collect(response, delegate, false).await
                } else {
                    self.wait(cell_id, delegate, request.yield_time_ms, request.max_tokens)
                        .await
                }
            }
        }
    }

    async fn answer(
        &mut self,
        results: Vec<Value>,
        operation: &Operation,
    ) -> Result<Observation, String> {
        let pending = self.pending.take().unwrap();
        for (call, result) in pending.calls.into_iter().zip(results) {
            let _ = call.result.send(Ok(result));
        }
        self.wait(
            pending.cell_id,
            Some(pending.delegate),
            operation.yield_time_ms(),
            operation.max_output_tokens(),
        )
        .await
    }

    async fn wait(
        &mut self,
        cell_id: CellId,
        delegate: Option<Arc<Delegate>>,
        yield_time_ms: u64,
        max_output_tokens: Option<usize>,
    ) -> Result<Observation, String> {
        let preempt = delegate.as_ref().map(|delegate| delegate.arm());
        let response = self
            .session
            .wait(
                WaitRequest {
                    cell_id,
                    yield_time_ms,
                    max_output_tokens,
                },
                preempt.clone(),
            )
            .await?
            .into();
        self.collect(
            response,
            delegate,
            preempt.is_some_and(|token| token.is_cancelled()),
        )
        .await
    }

    async fn collect(
        &mut self,
        mut response: RuntimeResponse,
        delegate: Option<Arc<Delegate>>,
        tool_preempted: bool,
    ) -> Result<Observation, String> {
        if let Some(delegate) = &delegate {
            delegate.disarm();
        }
        let mut content = Vec::new();
        if matches!(response, RuntimeResponse::Yielded { .. }) && tool_preempted {
            let cell_id = response_cell_id(&response).clone();
            content.append(response_content(&mut response));
            loop {
                let outcome = self
                    .session
                    .wait_to_pending(WaitToPendingRequest {
                        cell_id: cell_id.clone(),
                    })
                    .await?;
                match outcome {
                    WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
                        content_items,
                        pending_tool_call_ids,
                        ..
                    }) => {
                        content.extend(content_items);
                        let delegate = delegate.as_ref().unwrap();
                        let calls = delegate.take(&pending_tool_call_ids).await;
                        let had_calls = !calls.is_empty();
                        let mut valid = Vec::new();
                        for call in calls {
                            match invocation_error(&call.call) {
                                Some(error) => {
                                    let _ = call.result.send(Err(error));
                                }
                                None => valid.push(call),
                            }
                        }
                        if !valid.is_empty() {
                            content.extend(delegate.notifications());
                            self.pending = Some(Pending {
                                cell_id,
                                delegate: delegate.clone(),
                                calls: valid,
                            });
                            return Ok(Observation::Frontier { content });
                        }
                        if !had_calls {
                            response = RuntimeResponse::Yielded {
                                cell_id,
                                content_items: Vec::new(),
                                code_mode_host_duration: None,
                            };
                            break;
                        }
                    }
                    WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(
                        completed,
                    ))
                    | WaitToPendingOutcome::MissingCell(completed) => {
                        response = completed;
                        break;
                    }
                }
            }
        }
        if let Some(delegate) = delegate {
            content.extend(delegate.notifications());
        }
        if !matches!(response, RuntimeResponse::Yielded { .. }) {
            self.scripts.remove(response_cell_id(&response));
        }
        content.append(response_content(&mut response));
        *response_content(&mut response) = content;
        Ok(Observation::Response(response))
    }
}

impl Delegate {
    fn arm(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut observer = self.preempt.lock().unwrap();
        if !self.pending.lock().unwrap().is_empty() {
            token.cancel();
        }
        *observer = Some(token.clone());
        token
    }

    fn disarm(&self) {
        *self.preempt.lock().unwrap() = None;
    }

    fn notifications(&self) -> Vec<codex_code_mode::FunctionCallOutputContentItem> {
        self.notifications
            .lock()
            .unwrap()
            .drain(..)
            .map(|text| codex_code_mode::FunctionCallOutputContentItem::InputText { text })
            .collect()
    }

    async fn take(&self, ids: &[String]) -> Vec<Invocation> {
        loop {
            let ready = self.ready.notified();
            {
                let mut pending = self.pending.lock().unwrap();
                if ids.iter().all(|id| pending.contains_key(id)) {
                    return ids.iter().map(|id| pending.remove(id).unwrap()).collect();
                }
            }
            ready.await;
        }
    }
}

impl CodeModeSessionDelegate for Delegate {
    fn invoke_tool<'a>(
        &'a self,
        call: CodeModeNestedToolCall,
        cancellation: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        let (result, receive) = oneshot::channel();
        let observer = self.preempt.lock().unwrap();
        self.pending.lock().unwrap().insert(
            call.runtime_tool_call_id.clone(),
            Invocation { call, result },
        );
        self.ready.notify_one();
        if let Some(observer) = observer.as_ref() {
            observer.cancel();
        }
        Box::pin(async move {
            tokio::select! {
                result = receive => result.map_err(|_| "executor tool result channel closed".to_string())?,
                () = cancellation.cancelled() => Err("executor tool call cancelled".to_string()),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        text: String,
        _cancellation: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.notifications.lock().unwrap().push(text);
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, invocation| &invocation.call.cell_id != cell_id);
    }
}

fn response_cell_id(response: &RuntimeResponse) -> &CellId {
    match response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}

fn response_content(
    response: &mut RuntimeResponse,
) -> &mut Vec<codex_code_mode::FunctionCallOutputContentItem> {
    match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::Terminated { content_items, .. }
        | RuntimeResponse::Result { content_items, .. } => content_items,
    }
}

/// Replay checks only a terminal response's status.
fn settle(response: &mut RuntimeResponse) {
    if !matches!(response, RuntimeResponse::Yielded { .. }) {
        response_content(response).clear();
    }
}

fn response_content_ref(
    response: &RuntimeResponse,
) -> &[codex_code_mode::FunctionCallOutputContentItem] {
    match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::Terminated { content_items, .. }
        | RuntimeResponse::Result { content_items, .. } => content_items,
    }
}

fn same_response_status(actual: &RuntimeResponse, recorded: &RuntimeResponse) -> bool {
    // Replay rebuilds live cells; the journal retains their original diagnostic output.
    if response_cell_id(actual) != response_cell_id(recorded) {
        return false;
    }
    match (actual, recorded) {
        (RuntimeResponse::Yielded { .. }, RuntimeResponse::Yielded { .. }) => {
            response_content_ref(actual) == response_content_ref(recorded)
        }
        (RuntimeResponse::Terminated { .. }, RuntimeResponse::Terminated { .. }) => true,
        (
            RuntimeResponse::Result {
                error_text: actual, ..
            },
            RuntimeResponse::Result {
                error_text: recorded,
                ..
            },
        ) => actual == recorded,
        _ => false,
    }
}

fn invocation_error(call: &CodeModeNestedToolCall) -> Option<String> {
    match (&call.tool_kind, &call.input) {
        (CodeModeToolKind::Function, None | Some(Value::Object(_)))
        | (CodeModeToolKind::Freeform, Some(Value::String(_))) => None,
        (CodeModeToolKind::Function, _) => Some(format!(
            "tool `{}` expects a JSON object for arguments",
            call.tool_name
        )),
        (CodeModeToolKind::Freeform, _) => {
            Some(format!("tool `{}` expects a string input", call.tool_name))
        }
    }
}

fn client_call(call: &CodeModeNestedToolCall, call_id: String) -> ResponseItem {
    let name = call.tool_name.name.clone();
    let namespace = call.tool_name.namespace.clone();
    match call.tool_kind {
        CodeModeToolKind::Function => ResponseItem::FunctionCall {
            id: None,
            encrypted_content: None,
            status: None,
            name,
            namespace,
            call_id,
            arguments: call.input.clone().unwrap_or_else(|| json!({})).to_string(),
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
        },
        CodeModeToolKind::Freeform => ResponseItem::CustomToolCall {
            id: None,
            encrypted_content: None,
            status: None,
            name,
            namespace,
            call_id,
            input: call
                .input
                .as_ref()
                .and_then(Value::as_str)
                .unwrap()
                .to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    }
}

fn item_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::ToolSearchCall { call_id, .. }
        | ResponseItem::ToolSearchOutput { call_id, .. } => call_id.as_deref(),
        _ => None,
    }
}

fn is_result(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
    )
}

fn output_item(
    call: &ResponseItem,
    items: Vec<FunctionCallOutputContentItem>,
    success: bool,
) -> ResponseItem {
    let output = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::ContentItems(items),
        success: Some(success),
    };
    let call_id = item_call_id(call).unwrap().to_string();
    match call {
        ResponseItem::CustomToolCall { .. } => ResponseItem::CustomToolCallOutput {
            id: None,
            call_id,
            name: None,
            output,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall { .. } => ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id),
            name: None,
            namespace: None,
            output,
            internal_chat_message_metadata_passthrough: None,
        },
        _ => unreachable!("executor output answers an exec call"),
    }
}

fn protocol_content(
    item: codex_code_mode::FunctionCallOutputContentItem,
) -> FunctionCallOutputContentItem {
    match item {
        codex_code_mode::FunctionCallOutputContentItem::InputText { text } => {
            FunctionCallOutputContentItem::InputText { text }
        }
        codex_code_mode::FunctionCallOutputContentItem::InputImage { image_url, detail } => {
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline { image_url },
                detail: Some(
                    match detail.unwrap_or(codex_code_mode::DEFAULT_IMAGE_DETAIL) {
                        codex_code_mode::ImageDetail::Auto => ImageDetail::Auto,
                        codex_code_mode::ImageDetail::Low => ImageDetail::Low,
                        codex_code_mode::ImageDetail::High => ImageDetail::High,
                        codex_code_mode::ImageDetail::Original => ImageDetail::Original,
                    },
                ),
            }
        }
        codex_code_mode::FunctionCallOutputContentItem::InputAudio { audio_url } => {
            FunctionCallOutputContentItem::InputAudio { audio_url }
        }
    }
}

fn nested_result(output: FunctionCallOutputPayload) -> Value {
    match output.body {
        FunctionCallOutputBody::Text(text) if output.success != Some(false) => Value::String(text),
        FunctionCallOutputBody::Text(text) => {
            json!({"content": [{"type": "text", "text": text}], "isError": true})
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let content = items
                .into_iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        json!({"type": "text", "text": text})
                    }
                    FunctionCallOutputContentItem::InputImage {
                        image: ImageReference::Inline { image_url },
                        ..
                    } => {
                        if let Some((mime_type, data)) = image_url
                            .strip_prefix("data:")
                            .and_then(|url| url.split_once(";base64,"))
                        {
                            json!({"type": "image", "mimeType": mime_type, "data": data})
                        } else {
                            json!({"image_url": image_url})
                        }
                    }
                    item => serde_json::to_value(item).unwrap(),
                })
                .collect::<Vec<_>>();
            json!({"content": content, "isError": output.success == Some(false)})
        }
    }
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
