use super::*;

#[test]
fn memory_key_uses_javascript_utf16_units() {
    assert_eq!(
        project_key("H:\\Projects\\sample-project"),
        "H--Projects-sample-project"
    );
    assert_eq!(project_key("/work/🦀/repo"), "-work----repo");
}
