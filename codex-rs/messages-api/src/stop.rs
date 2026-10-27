//! Client-side `stop_sequences`; Codex requests carry none.

/// Holds back text while it could still begin a stop sequence.
pub(crate) struct StopMatcher {
    sequences: Vec<String>,
    pending: String,
}

impl StopMatcher {
    pub(crate) fn new(sequences: Vec<String>) -> Self {
        Self {
            sequences: sequences
                .into_iter()
                .filter(|sequence| !sequence.is_empty())
                .collect(),
            pending: String::new(),
        }
    }

    /// Text safe to emit, and the sequence that ended generation.
    pub(crate) fn push(&mut self, delta: &str) -> (String, Option<String>) {
        if self.sequences.is_empty() {
            return (delta.to_string(), None);
        }
        self.pending.push_str(delta);
        let earliest = self
            .sequences
            .iter()
            .filter_map(|sequence| {
                self.pending
                    .find(sequence.as_str())
                    .map(|at| (at, sequence))
            })
            .min_by_key(|(at, _)| *at);
        if let Some((at, sequence)) = earliest {
            let sequence = sequence.clone();
            let text = self.pending[..at].to_string();
            self.pending.clear();
            return (text, Some(sequence));
        }
        let len = self.pending.len();
        let held = (1..=len.min(self.longest().saturating_sub(1)))
            .rev()
            .filter(|held| self.pending.is_char_boundary(len - held))
            .find(|held| {
                let suffix = &self.pending[len - held..];
                self.sequences
                    .iter()
                    .any(|sequence| sequence.starts_with(suffix))
            })
            .unwrap_or(0);
        let text = self.pending.drain(..len - held).collect();
        (text, None)
    }

    pub(crate) fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    fn longest(&self) -> usize {
        self.sequences.iter().map(String::len).max().unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "stop_tests.rs"]
mod tests;
