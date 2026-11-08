//! Context usage anchored to the response that measured it, independently of tool continuations.

use codex_core::external_harness::estimate_excluded_reasoning_tokens;
use codex_core::external_harness::estimate_history_tokens;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextUsage {
    /// Client history prefix needed to retain this measurement.
    pub(crate) visible_items: usize,
    /// Model history prefix covered by the server count, including its generated output.
    pub(crate) items: usize,
    pub(crate) tokens: i64,
}

impl ContextUsage {
    pub(crate) fn measured(
        usage: &TokenUsage,
        includes_reasoning: Option<bool>,
        input_items: usize,
        history: &[ResponseItem],
        visible_items: usize,
        model: &str,
    ) -> Self {
        let excluded = if includes_reasoning == Some(false) {
            estimate_excluded_reasoning_tokens(&history[..input_items], model)
        } else {
            0
        };
        Self {
            visible_items,
            items: history.len(),
            tokens: usage.total_tokens.saturating_add(excluded),
        }
    }

    /// Installs a generated native call after earlier tool results, which were not sampled.
    pub(crate) fn cover_execution(
        &mut self,
        preceding: &[ResponseItem],
        visible_items: usize,
        model: &str,
    ) {
        self.tokens = self.estimate(preceding, model);
        self.items = preceding.len() + 1;
        self.visible_items = visible_items;
    }

    pub(crate) fn estimate(&self, history: &[ResponseItem], model: &str) -> i64 {
        self.tokens
            .saturating_add(estimate_history_tokens(&history[self.items..], model))
    }
}

#[cfg(test)]
#[path = "tokens_tests.rs"]
mod tests;
