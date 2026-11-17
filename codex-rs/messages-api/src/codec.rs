//! Codex items carried through Anthropic-shaped transcripts.
//!
//! Thinking-block signatures hold the reasoning item; compaction summaries name the replacement
//! history the `compactions` store holds. Both resolve to the exact items Codex history recorded.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_protocol::models::ResponseItem;

use crate::compactions;

const SIGNATURE_PREFIX: &str = "codex.rs1.";
const COMPACTION_OPEN: &str = "[[codex-compaction:";
const INLINE_VERSION: &str = "v1:";
const STORED_VERSION: &str = "v2:";
const COMPACTION_CLOSE: &str = "]]";

/// A compaction marker's replacement history.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Marker<'a> {
    /// Encoded items.
    Inline(&'a str),
    /// `compactions` store digest.
    Stored(&'a str),
}

pub(crate) fn encode_signature(item: &ResponseItem) -> serde_json::Result<String> {
    Ok(format!(
        "{SIGNATURE_PREFIX}{}",
        encode_items(std::slice::from_ref(item))?
    ))
}

/// `None` for a signature another backend produced.
pub(crate) fn decode_signature(signature: &str) -> Option<ResponseItem> {
    let payload = signature.strip_prefix(SIGNATURE_PREFIX)?;
    match decode_items(payload)?.as_slice() {
        [item @ ResponseItem::Reasoning { .. }] => Some(item.clone()),
        _ => None,
    }
}

pub(crate) fn compaction_marker(digest: &str) -> String {
    format!("{COMPACTION_OPEN}{STORED_VERSION}{digest}{COMPACTION_CLOSE}")
}

/// Splits `text` around an embedded compaction marker.
pub(crate) fn find_compaction(text: &str) -> Option<(&str, Marker<'_>, &str)> {
    let start = text.find(COMPACTION_OPEN)?;
    let payload_start = start + COMPACTION_OPEN.len();
    let payload_len = text[payload_start..].find(COMPACTION_CLOSE)?;
    let payload = &text[payload_start..payload_start + payload_len];
    let marker = match payload.strip_prefix(INLINE_VERSION) {
        Some(encoded) => Marker::Inline(encoded),
        None => Marker::Stored(
            payload
                .strip_prefix(STORED_VERSION)
                .filter(|digest| compactions::is_digest(digest))?,
        ),
    };
    Some((
        &text[..start],
        marker,
        &text[payload_start + payload_len + COMPACTION_CLOSE.len()..],
    ))
}

#[cfg(test)]
pub(crate) fn inline_marker(items: &[ResponseItem]) -> String {
    format!(
        "{COMPACTION_OPEN}{INLINE_VERSION}{}{COMPACTION_CLOSE}",
        encode_items(items).unwrap_or_default()
    )
}

fn encode_items(items: &[ResponseItem]) -> serde_json::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(items)?))
}

pub(crate) fn decode_items(payload: &str) -> Option<Vec<ResponseItem>> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
