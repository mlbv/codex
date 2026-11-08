use codex_protocol::models::ResponseItem;

use super::history::estimate_item_token_count;
use super::history::estimate_item_token_count_with_text;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum TokenEstimator {
    #[default]
    Native,
    Gpt61Sol,
}

impl TokenEstimator {
    pub(crate) fn for_model(model: &str) -> Self {
        match model {
            "gpt-6.1-sol" => Self::Gpt61Sol,
            _ => Self::Native,
        }
    }

    pub(crate) fn item(self, item: &ResponseItem) -> i64 {
        match self {
            Self::Native => estimate_item_token_count(item),
            Self::Gpt61Sol => {
                if let ResponseItem::Reasoning {
                    encrypted_content: Some(content),
                    ..
                } = item
                {
                    // Fit to 6,984 measured reasoning samples; 1,471 held-out samples have
                    // 7.2% weighted absolute error.
                    let tokens = content
                        .len()
                        .saturating_mul(19)
                        .div_ceil(250)
                        .saturating_sub(124);
                    return i64::try_from(tokens).unwrap_or(i64::MAX);
                }
                estimate_item_token_count_with_text(item, calibrated_text_bytes)
            }
        }
    }

    pub(crate) fn text(self, text: &str) -> i64 {
        match self {
            Self::Native => {
                i64::try_from(codex_utils_string::approx_token_count(text)).unwrap_or(i64::MAX)
            }
            Self::Gpt61Sol => calibrated_text_bytes(text).saturating_add(3) / 4,
        }
    }
}

fn calibrated_text_bytes(text: &str) -> i64 {
    // Nonnegative fit to 11,049 measured outputs. On 2,522 held-out outputs, weighted
    // absolute error falls from 15.3% to 7.6%; on 2,090 input increments, 12.0% to 8.8%.
    // Costs are thousandths of byte-equivalents so modality estimates retain their scale.
    // Unicode bytes retain the native cost; this corpus cannot calibrate individual scripts.
    let mut cost = 0usize;
    let mut blank = false;
    for byte in text.bytes() {
        let next_blank = matches!(byte, b' ' | b'\t' | b'\r');
        let weight = match byte {
            b'a'..=b'z' | b'A'..=b'Z' => 528,
            b'0'..=b'9' => 1144,
            b' ' | b'\t' | b'\r' => {
                if blank {
                    0
                } else {
                    1784
                }
            }
            b'\n' => 3500,
            128..=255 => 1000,
            _ => 3260,
        };
        cost = cost.saturating_add(weight);
        blank = next_blank;
    }
    i64::try_from(cost.div_ceil(1000)).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "token_estimator_tests.rs"]
mod tests;
