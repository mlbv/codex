use super::*;
use codex_protocol::AgentPath;

fn add_member(state: &mut State, path: &str, name: Option<&str>, shutdown: bool) -> ThreadId {
    let id = ThreadId::new();
    let member = Member {
        id,
        parent: state.root.unwrap(),
        name: name.map(str::to_owned),
        path: path.to_string(),
        shutdown,
        run_in_background: true,
    };
    if let Some(name) = name {
        state.aliases.insert(name.to_string(), id.to_string());
    }
    state.members.insert(id.to_string(), member);
    id
}

#[test]
fn replacing_alias_keeps_both_native_ids_and_paths_addressable() {
    let root = ThreadId::new();
    let mut state = State {
        root: Some(root),
        ..Default::default()
    };
    let first = add_member(&mut state, "/root/first", Some("reviewer"), false);
    let latest = add_member(&mut state, "/root/latest", Some("reviewer"), false);
    let path = AgentPath::root();
    assert_eq!(
        state.message_targets(root, &path, "reviewer"),
        Some(vec![latest])
    );
    assert_eq!(
        state.message_targets(root, &path, &first.to_string()),
        Some(vec![first])
    );
    assert_eq!(
        state.message_targets(root, &path, "/root/first"),
        Some(vec![first])
    );
    assert_eq!(
        state.message_targets(root, &path, "latest"),
        Some(vec![latest])
    );
    assert_eq!(state.message_targets(root, &path, "peer-session"), None);
    assert_eq!(state.message_targets(root, &path, "uds:peer-address"), None);
}

#[test]
fn broadcast_uses_current_active_names_and_excludes_the_sender() {
    let root = ThreadId::new();
    let mut state = State {
        root: Some(root),
        ..Default::default()
    };
    add_member(&mut state, "/root/old", Some("reviewer"), false);
    let latest = add_member(&mut state, "/root/latest", Some("reviewer"), false);
    add_member(&mut state, "/root/stopped", Some("stopped"), true);
    add_member(&mut state, "/root/anonymous", None, false);
    let path = AgentPath::root();
    assert_eq!(state.message_targets(root, &path, "*"), Some(vec![latest]));
    assert_eq!(state.message_targets(latest, &path, "*"), Some(vec![]));
}

#[test]
fn root_addresses_and_nested_relative_paths_resolve_without_peer_lookup() {
    let root = ThreadId::new();
    let mut state = State {
        root: Some(root),
        ..Default::default()
    };
    let parent = add_member(&mut state, "/root/parent", Some("parent"), false);
    let child = add_member(&mut state, "/root/parent/child", None, false);
    let path = AgentPath::try_from("/root/parent").unwrap();
    let root_id = root.to_string();
    for target in ["main", "team-lead", "/root", root_id.as_str()] {
        assert_eq!(
            state.message_targets(parent, &path, target),
            Some(vec![root])
        );
    }
    assert_eq!(
        state.message_targets(parent, &path, "child"),
        Some(vec![child])
    );
    assert_eq!(
        state.message_targets(root, &AgentPath::root(), "child"),
        None
    );
}
