use pretty_assertions::assert_eq;

use super::CiteFilter;
use super::strip;

fn run(deltas: &[&str]) -> (String, Vec<String>) {
    let mut filter = CiteFilter::default();
    let mut text = String::new();
    let mut refs = Vec::new();
    for delta in deltas {
        let (out, found) = filter.push(delta);
        text.push_str(&out);
        refs.extend(found);
    }
    text.push_str(&filter.flush());
    (text, refs)
}

#[test]
fn markers_split_across_deltas_are_removed_and_their_refs_returned() {
    let (text, refs) = run(&[
        "Tag X5846. \u{e200}ci",
        "te\u{e202}turn0search1\u{e202}turn1sea",
        "rch0\u{e201} Done.",
    ]);
    assert_eq!(text, "Tag X5846.  Done.");
    assert_eq!(refs, vec!["turn0search1", "turn1search0"]);
}

#[test]
fn text_before_a_held_marker_is_emitted_at_once() {
    let mut filter = CiteFilter::default();
    assert_eq!(
        filter.push("Before \u{e200}cite"),
        ("Before ".to_string(), Vec::new())
    );
}

#[test]
fn other_references_and_unclosed_markers_stay_text() {
    let other = "\u{e200}visualize\u{e202}{\"path\":\"a.html\"}\u{e201}";
    assert_eq!(run(&[other]), (other.to_string(), Vec::new()));
    assert_eq!(
        run(&["x \u{e200}cite\u{e202}turn0"]),
        ("x \u{e200}cite\u{e202}turn0".to_string(), Vec::new())
    );
    let long = format!("\u{e200}cite\u{e202}{}\u{e201}", "a".repeat(600));
    assert_eq!(run(&[&long]), (long.clone(), Vec::new()));
}

#[test]
fn strip_matches_the_streamed_text() {
    let text = "A\u{e200}cite\u{e202}turn0search0\u{e201}B\u{e200}x\u{e201}C";
    assert_eq!(
        strip(text),
        run(&[
            "A\u{e200}c",
            "ite\u{e202}turn0search0\u{e201}B\u{e200}x\u{e201}C"
        ])
        .0
    );
    assert_eq!(strip(text), "AB\u{e200}x\u{e201}C");
}
