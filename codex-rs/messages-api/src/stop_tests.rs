use pretty_assertions::assert_eq;

use super::*;

#[test]
fn text_without_sequences_passes_through() {
    let mut matcher = StopMatcher::new(Vec::new());
    assert_eq!(matcher.push("hello"), ("hello".to_string(), None));
}

#[test]
fn partial_sequence_is_held_until_resolved() {
    let mut matcher = StopMatcher::new(vec!["STOP".to_string()]);
    assert_eq!(matcher.push("abc ST"), ("abc ".to_string(), None));
    assert_eq!(matcher.push("ill"), ("STill".to_string(), None));
    assert_eq!(matcher.push(" x STO"), (" x ".to_string(), None));
    assert_eq!(
        matcher.push("P tail"),
        (String::new(), Some("STOP".to_string()))
    );
}

#[test]
fn earliest_sequence_wins_and_flush_releases_held_text() {
    let mut matcher = StopMatcher::new(vec!["end".to_string(), "\n\n".to_string()]);
    assert_eq!(
        matcher.push("a\n\nb end"),
        ("a".to_string(), Some("\n\n".to_string()))
    );
    let mut matcher = StopMatcher::new(vec!["é!".to_string()]);
    assert_eq!(matcher.push("café"), ("caf".to_string(), None));
    assert_eq!(matcher.flush(), "é");
}
