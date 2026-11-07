//! Responses-native citation markers `\u{e200}cite\u{e202}<ref>[\u{e202}<ref>…]\u{e201}` in model text.
//!
//! Clients get the text without them and a citation per ref of a known search result.

const OPEN: char = '\u{e200}';
const CLOSE: char = '\u{e201}';
const SEPARATOR: char = '\u{e202}';
const HEAD: &str = "\u{e200}cite";
/// Longer => not a marker: emitted as text, never held back further.
const MAX_MARKER_BYTES: usize = 512;

/// Holds back text while it could still be a marker.
#[derive(Default)]
pub(crate) struct CiteFilter {
    held: String,
}

impl CiteFilter {
    /// Text safe to emit, and the refs of the markers removed from it.
    pub(crate) fn push(&mut self, delta: &str) -> (String, Vec<String>) {
        self.held.push_str(delta);
        let mut text = String::new();
        let mut refs = Vec::new();
        loop {
            let Some(start) = self.held.find(OPEN) else {
                text.push_str(&std::mem::take(&mut self.held));
                break;
            };
            text.push_str(&self.held[..start]);
            self.held.drain(..start);
            match marker(&self.held) {
                Marker::Complete { len, refs: found } => {
                    refs.extend(found);
                    self.held.drain(..len);
                }
                Marker::Partial => break,
                Marker::Not => {
                    text.push(OPEN);
                    self.held.drain(..OPEN.len_utf8());
                }
            }
        }
        (text, refs)
    }

    /// Text held at the end of a block: never closed, so not a marker.
    pub(crate) fn flush(&mut self) -> String {
        std::mem::take(&mut self.held)
    }
}

/// `text` as clients received it.
pub(crate) fn strip(text: &str) -> String {
    let mut filter = CiteFilter::default();
    let (mut out, _) = filter.push(text);
    out.push_str(&filter.flush());
    out
}

enum Marker {
    Complete { len: usize, refs: Vec<String> },
    Partial,
    Not,
}

/// Classifies `text`, which starts with `OPEN`.
fn marker(text: &str) -> Marker {
    let Some(close) = text.find(CLOSE) else {
        let could_be = if text.len() < HEAD.len() {
            HEAD.starts_with(text)
        } else {
            text.starts_with(HEAD)
                && (text.len() == HEAD.len() || text[HEAD.len()..].starts_with(SEPARATOR))
                && !text[OPEN.len_utf8()..].contains(OPEN)
        };
        return if could_be && text.len() < MAX_MARKER_BYTES {
            Marker::Partial
        } else {
            Marker::Not
        };
    };
    let len = close + CLOSE.len_utf8();
    let Some(args) = text[..close].strip_prefix(HEAD) else {
        return Marker::Not;
    };
    if len > MAX_MARKER_BYTES
        || args.contains(OPEN)
        || !(args.is_empty() || args.starts_with(SEPARATOR))
    {
        return Marker::Not;
    }
    Marker::Complete {
        len,
        refs: args
            .split(SEPARATOR)
            .filter(|reference| !reference.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

#[cfg(test)]
#[path = "cite_tests.rs"]
mod tests;
