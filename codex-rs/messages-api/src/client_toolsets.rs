//! Anthropic client toolsets (`computer_toolset_*`, `browser_toolset_*`) as Codex namespaces:
//! the toolset is the namespace, each member a function the client executes.

use serde_json::Value;
use serde_json::json;

pub(crate) struct Toolset {
    pub(crate) name: &'static str,
    pub(crate) members: Vec<Member>,
    /// What the Messages API adds to the system prompt for this toolset.
    pub(crate) instructions: Option<&'static str>,
}

pub(crate) struct Member {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) input_schema: Value,
    pub(crate) enabled_by_default: bool,
}

pub(crate) const COMPUTER_USE_INSTRUCTIONS: &str = "You have access to a set of functions you can use to answer the user's question. This includes access to a sandboxed computing environment. You do NOT currently have the ability to inspect files or interact with external resources, except by invoking the below functions.";

pub(crate) fn toolset(kind: &str) -> Option<Toolset> {
    match kind {
        "computer_toolset_20260801" => Some(Toolset {
            name: "computer",
            members: computer_members(),
            instructions: Some(COMPUTER_USE_INSTRUCTIONS),
        }),
        "browser_toolset_20260801" => Some(Toolset {
            name: "browser",
            members: browser_members(),
            instructions: None,
        }),
        _ => None,
    }
}

fn member(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> Member {
    Member {
        name,
        description,
        input_schema: json!({ "type": "object", "properties": properties, "required": required }),
        enabled_by_default: true,
    }
}

fn coordinate() -> Value {
    json!({ "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "[x, y] in screenshot pixels." })
}

fn modifiers() -> Value {
    json!({ "type": "string", "description": "Modifier keys held during the action: `shift`, `ctrl`, `alt`, `super`, or `+`-joined combinations." })
}

fn computer_members() -> Vec<Member> {
    let click = |name, description| {
        member(
            name,
            description,
            json!({ "coordinate": coordinate(), "text": modifiers() }),
            &[],
        )
    };
    vec![
        member(
            "screenshot",
            "Capture the full display and return it as an image.",
            json!({}),
            &[],
        ),
        member(
            "zoom",
            "Capture a region at full resolution, scaled to fit within the screenshot dimensions.",
            json!({ "region": { "type": "array", "items": { "type": "integer" }, "minItems": 4, "maxItems": 4, "description": "[x0, y0, x1, y1] in screenshot pixels." } }),
            &["region"],
        ),
        click(
            "left_click",
            "Click the left mouse button at `coordinate`, or at the current cursor position.",
        ),
        click(
            "right_click",
            "Click the right mouse button at `coordinate`, or at the current cursor position.",
        ),
        click(
            "middle_click",
            "Click the middle mouse button at `coordinate`, or at the current cursor position.",
        ),
        click(
            "double_click",
            "Double-click at `coordinate`, or at the current cursor position.",
        ),
        click(
            "triple_click",
            "Triple-click at `coordinate`, or at the current cursor position.",
        ),
        member(
            "left_click_drag",
            "Press the left mouse button at `start_coordinate`, drag to `coordinate`, and release.",
            json!({ "start_coordinate": coordinate(), "coordinate": coordinate(), "text": modifiers() }),
            &["start_coordinate", "coordinate"],
        ),
        member(
            "mouse_move",
            "Move the cursor to `coordinate` without clicking.",
            json!({ "coordinate": coordinate() }),
            &["coordinate"],
        ),
        member(
            "left_mouse_down",
            "Press the left mouse button at the current cursor position.",
            json!({}),
            &[],
        ),
        member(
            "left_mouse_up",
            "Release the left mouse button at the current cursor position.",
            json!({}),
            &[],
        ),
        member(
            "cursor_position",
            "Report the current cursor [x, y] position as text.",
            json!({}),
            &[],
        ),
        member(
            "scroll",
            "Scroll at `coordinate`, or at the current cursor position.",
            json!({
                "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "scroll_amount": { "type": "number", "description": "Scroll wheel notches." },
                "coordinate": coordinate(),
                "text": modifiers(),
            }),
            &["scroll_direction", "scroll_amount"],
        ),
        member(
            "type",
            "Type literal text at the keyboard focus.",
            json!({ "text": { "type": "string" } }),
            &["text"],
        ),
        member(
            "key",
            "Press a key or key combination, e.g. `Return`, `ctrl+s`, `alt+Tab`.",
            json!({ "text": { "type": "string" }, "repeat": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Presses; default 1." } }),
            &["text"],
        ),
        member(
            "hold_key",
            "Hold a key or key combination for `duration` seconds.",
            json!({ "text": { "type": "string" }, "duration": { "type": "number", "maximum": 300 } }),
            &["text", "duration"],
        ),
        member(
            "wait",
            "Pause for `duration` seconds before the next action.",
            json!({ "duration": { "type": "number", "maximum": 300 } }),
            &["duration"],
        ),
    ]
}

fn tab_id() -> Value {
    json!({ "type": "string", "description": "Tab to act on; default the active tab." })
}

fn target() -> Value {
    json!({
        "type": "object",
        "description": "A viewport point `{\"type\": \"coordinate\", \"x\": 640, \"y\": 380}` or an element `{\"type\": \"ref\", \"ref\": \"ref_2\"}` from `read_page`/`find`.",
        "properties": {
            "type": { "type": "string", "enum": ["coordinate", "ref"] },
            "x": { "type": "integer" },
            "y": { "type": "integer" },
            "ref": { "type": "string" },
        },
        "required": ["type"],
    })
}

fn coordinate_target() -> Value {
    json!({
        "type": "object",
        "description": "A viewport point in pixels, origin at the page's top-left.",
        "properties": {
            "type": { "type": "string", "enum": ["coordinate"] },
            "x": { "type": "integer" },
            "y": { "type": "integer" },
        },
        "required": ["type", "x", "y"],
    })
}

fn ref_target() -> Value {
    json!({
        "type": "object",
        "description": "An element reference from `read_page` or `find`.",
        "properties": {
            "type": { "type": "string", "enum": ["ref"] },
            "ref": { "type": "string" },
        },
        "required": ["type", "ref"],
    })
}

fn browser_members() -> Vec<Member> {
    let pointer = |name, description| {
        member(
            name,
            description,
            json!({ "target": target(), "modifiers": modifiers(), "tab_id": tab_id() }),
            &["target"],
        )
    };
    let coordinate_only = |name, description| {
        member(
            name,
            description,
            json!({ "target": coordinate_target(), "tab_id": tab_id() }),
            &["target"],
        )
    };
    let optional = |member: Member| Member {
        enabled_by_default: false,
        ..member
    };
    vec![
        member(
            "navigate",
            "Load an http(s) URL, or move through history with `back`, `forward` or `reload`. A URL without a scheme is treated as https.",
            json!({ "url": { "type": "string" }, "tab_id": tab_id() }),
            &["url"],
        ),
        member(
            "screenshot",
            "Capture the viewport as an image.",
            json!({ "tab_id": tab_id() }),
            &[],
        ),
        member(
            "zoom",
            "Capture a cropped, upscaled image of a viewport region.",
            json!({ "region": { "type": "array", "items": { "type": "integer" }, "minItems": 4, "maxItems": 4, "description": "[x0, y0, x1, y1] in viewport pixels." }, "tab_id": tab_id() }),
            &["region"],
        ),
        pointer(
            "left_click",
            "Left-click a viewport point or a referenced element.",
        ),
        pointer(
            "right_click",
            "Right-click a viewport point or a referenced element.",
        ),
        pointer(
            "middle_click",
            "Middle-click a viewport point or a referenced element.",
        ),
        pointer(
            "double_click",
            "Double left-click a viewport point or a referenced element.",
        ),
        pointer(
            "triple_click",
            "Triple left-click a viewport point or a referenced element, selecting a line or paragraph.",
        ),
        member(
            "hover",
            "Move the pointer over a viewport point or a referenced element without clicking.",
            json!({ "target": target(), "tab_id": tab_id() }),
            &["target"],
        ),
        member(
            "left_click_drag",
            "Press at `from`, drag to `target`, and release.",
            json!({ "from": coordinate_target(), "target": coordinate_target(), "tab_id": tab_id() }),
            &["from", "target"],
        ),
        coordinate_only(
            "left_mouse_down",
            "Press and hold the left button at a viewport point.",
        ),
        coordinate_only(
            "left_mouse_up",
            "Release the left button at a viewport point.",
        ),
        coordinate_only("mouse_move", "Move the pointer to a viewport point."),
        member(
            "scroll",
            "Scroll at a viewport point.",
            json!({
                "target": coordinate_target(),
                "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "scroll_amount": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Wheel notches; default 3." },
                "tab_id": tab_id(),
            }),
            &["target", "scroll_direction"],
        ),
        member(
            "scroll_to",
            "Scroll a referenced element into view.",
            json!({ "target": ref_target(), "tab_id": tab_id() }),
            &["target"],
        ),
        member(
            "type",
            "Type a literal string at the current focus.",
            json!({ "text": { "type": "string" }, "tab_id": tab_id() }),
            &["text"],
        ),
        member(
            "key",
            "Press a key (`Enter`), chord (`ctrl+a`), or space-separated sequence (`Backspace Backspace`).",
            json!({ "text": { "type": "string" }, "repeat": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Presses; default 1." }, "tab_id": tab_id() }),
            &["text"],
        ),
        member(
            "hold_key",
            "Hold a key or chord for `duration` seconds.",
            json!({ "text": { "type": "string" }, "duration": { "type": "number", "minimum": 0, "maximum": 30 }, "tab_id": tab_id() }),
            &["text", "duration"],
        ),
        member(
            "wait",
            "Pause for `duration` seconds.",
            json!({ "duration": { "type": "number", "minimum": 0, "maximum": 30 }, "tab_id": tab_id() }),
            &["duration"],
        ),
        member(
            "read_page",
            "Return the accessibility tree as text, with element references like `[ref_2]`.",
            json!({
                "filter": { "type": "string", "enum": ["interactive", "all"], "description": "Omit for all visible elements; `interactive` for interactive ones only; `all` includes off-viewport elements." },
                "depth": { "type": "integer", "minimum": 1, "maximum": 15, "description": "Tree depth; default 15." },
                "ref": { "type": "string", "description": "Scope to this element's subtree." },
                "tab_id": tab_id(),
            }),
            &[],
        ),
        member(
            "find",
            "Find up to 20 elements matching a natural-language description, tagged like `read_page`.",
            json!({ "query": { "type": "string" }, "tab_id": tab_id() }),
            &["query"],
        ),
        member(
            "get_page_text",
            "Return the page's visible text, main article content first.",
            json!({ "tab_id": tab_id() }),
            &[],
        ),
        member(
            "form_input",
            "Set a form element's value directly: a boolean for checkboxes, an option value or text for selects.",
            json!({ "target": ref_target(), "value": { "type": ["string", "number", "boolean"] }, "tab_id": tab_id() }),
            &["target", "value"],
        ),
        optional(member(
            "file_upload",
            "Set files on an `<input type=\"file\">` from executor paths or staged document ids; give at least one.",
            json!({
                "target": ref_target(),
                "paths": { "type": "array", "items": { "type": "string" } },
                "document_ids": { "type": "array", "items": { "type": "string" } },
                "tab_id": tab_id(),
            }),
            &["target"],
        )),
        optional(member(
            "read_console",
            "Return the tab's console entries since the last read, one per line.",
            json!({ "tab_id": tab_id() }),
            &[],
        )),
        optional(member(
            "read_network",
            "Return the tab's network requests since the last read: method, URL, status, MIME type, timing.",
            json!({ "tab_id": tab_id() }),
            &[],
        )),
        optional(member(
            "javascript_exec",
            "Run `text` as JavaScript in the page and return the last expression's value as text.",
            json!({ "text": { "type": "string" }, "tab_id": tab_id() }),
            &["text"],
        )),
        member("new_tab", "Open a tab and make it active.", json!({}), &[]),
        member("list_tabs", "Report the open tabs.", json!({}), &[]),
        member(
            "switch_tab",
            "Make `tab_id` the active tab.",
            json!({ "tab_id": { "type": "string" } }),
            &["tab_id"],
        ),
        member(
            "close_tab",
            "Close `tab_id`.",
            json!({ "tab_id": { "type": "string" } }),
            &["tab_id"],
        ),
    ]
}
