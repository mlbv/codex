//! Anthropic-defined client tools as function tools. The Messages API keeps their schemas and
//! descriptions server-side; Codex models get them inline.

use serde_json::Value;
use serde_json::json;

pub(crate) struct ClientToolDefinition {
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    /// What the Messages API adds to the system prompt for this tool.
    pub(crate) instructions: Option<&'static str>,
}

const BASH_DESCRIPTION: &str = "Run commands in a bash shell
* When invoking this tool, the contents of the \"command\" parameter does NOT need to be XML-escaped.
* State is persistent across command calls and discussions with the user.
* To inspect a particular line range of a file, e.g. lines 10-25, try 'sed -n 10,25p /path/to/the/file'.
* Please avoid commands that may produce a very large amount of output.
* Please run long lived commands in the background, e.g. 'sleep 10 &' or start a server in the background.
* Set `restart` to true, without `command`, to restart the shell.";

const EDITOR_DESCRIPTION: &str = "Custom editing tool for viewing, creating and editing files
* State is persistent across command calls and discussions with the user
* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep
* The `create` command cannot be used if the specified `path` already exists as a file
* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`
* `insert` inserts `insert_text` AFTER the line `insert_line` of `path`; `insert_line` 0 inserts at the beginning of the file

Notes for using the `str_replace` command:
* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!
* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique
* The `new_str` parameter should contain the edited lines that should replace the `old_str`";

const LEGACY_EDITOR_DESCRIPTION: &str = "Custom editing tool for viewing, creating and editing files
* State is persistent across command calls and discussions with the user
* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep
* The `create` command cannot be used if the specified `path` already exists as a file
* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`
* The `undo_edit` command will revert the last edit made to the file at `path`
* `insert` inserts `new_str` AFTER the line `insert_line` of `path`; `insert_line` 0 inserts at the beginning of the file

Notes for using the `str_replace` command:
* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!
* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique
* The `new_str` parameter should contain the edited lines that should replace the `old_str`";

const MEMORY_DESCRIPTION: &str = "Memory directory at /memories that persists across conversations. Every path is under /memories; every command except `rename` takes `path`.
* `view`: a directory listing (2 levels deep, sizes included) or a file with line numbers; `view_range` [start_line, end_line] limits a file view (1-indexed, end_line -1 = end of file). Text views of files longer than 16,000 characters are truncated; page through them with `view_range`. Image files (.jpg, .jpeg, .png) are displayed.
* `create`: creates or overwrites the file at `path` with `file_text`.
* `str_replace`: replaces `old_str`, which must appear exactly once in the file, with `new_str` (omitted = delete `old_str`).
* `insert`: inserts `insert_text` after line `insert_line` (0 = beginning of the file).
* `delete`: deletes the file or directory at `path`, recursively.
* `rename`: moves `old_path` to `new_path`.
The /memories directory itself cannot be deleted or renamed. Keep the memory directory organized: up to date, coherent, no needless files.";

const MEMORY_INSTRUCTIONS: &str = "IMPORTANT: ALWAYS VIEW YOUR MEMORY DIRECTORY BEFORE DOING ANYTHING ELSE.
MEMORY PROTOCOL:
1. Use the `view` command of your `memory` tool to check for earlier progress.
2. ... (work on the task) ...
   - As you make progress, record status / progress / thoughts etc in your memory.
ASSUME INTERRUPTION: Your context window might be reset at any moment, so you risk losing any progress that is not recorded in your memory directory.";

/// The definition behind an Anthropic tool `type`; `None` for types that are not client tools.
pub(crate) fn definition(kind: &str, tool: &Value) -> Option<ClientToolDefinition> {
    let family = kind.rsplit_once('_').map_or(kind, |(family, _)| family);
    match (family, kind) {
        ("computer", "computer_20241022" | "computer_20250124" | "computer_20251124") => {
            Some(computer(kind, tool))
        }
        ("bash", _) => Some(ClientToolDefinition {
            description: BASH_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to run. Required unless the tool is being restarted." },
                    "restart": { "type": "boolean", "description": "Specifying true will restart this tool. Otherwise, leave this unspecified." },
                },
            }),
            instructions: None,
        }),
        ("text_editor", "text_editor_20241022" | "text_editor_20250124") => {
            Some(ClientToolDefinition {
                description: LEGACY_EDITOR_DESCRIPTION.to_string(),
                input_schema: editor_schema(
                    &["view", "create", "str_replace", "insert", "undo_edit"],
                    "new_str",
                ),
                instructions: None,
            })
        }
        ("text_editor", _) => Some(ClientToolDefinition {
            description: EDITOR_DESCRIPTION.to_string(),
            input_schema: editor_schema(
                &["view", "create", "str_replace", "insert"],
                "insert_text",
            ),
            instructions: None,
        }),
        ("memory", _) => Some(ClientToolDefinition {
            description: MEMORY_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "enum": ["view", "create", "str_replace", "insert", "delete", "rename"] },
                    "path": { "type": "string" },
                    "view_range": { "type": "array", "items": { "type": "integer" } },
                    "file_text": { "type": "string" },
                    "old_str": { "type": "string" },
                    "new_str": { "type": "string" },
                    "insert_line": { "type": "integer" },
                    "insert_text": { "type": "string" },
                    "old_path": { "type": "string" },
                    "new_path": { "type": "string" },
                },
                "required": ["command"],
            }),
            instructions: Some(MEMORY_INSTRUCTIONS),
        }),
        _ => None,
    }
}

/// The single-tool `computer` versions: one `action` per call.
fn computer(kind: &str, tool: &Value) -> ClientToolDefinition {
    let mut actions = vec![
        "key",
        "type",
        "mouse_move",
        "left_click",
        "left_click_drag",
        "right_click",
        "middle_click",
        "double_click",
        "screenshot",
        "cursor_position",
    ];
    if kind != "computer_20241022" {
        actions.extend([
            "left_mouse_down",
            "left_mouse_up",
            "scroll",
            "hold_key",
            "wait",
            "triple_click",
        ]);
    }
    let zoom = kind == "computer_20251124"
        && tool
            .get("enable_zoom")
            .and_then(Value::as_bool)
            .unwrap_or_default();
    if zoom {
        actions.push("zoom");
    }
    let dimension = |key| tool.get(key).and_then(Value::as_u64).unwrap_or_default();
    let mut description = format!(
        "Use a mouse and keyboard to interact with a computer, and take screenshots.
* This is an interface to a desktop GUI. You do not have access to a terminal or applications menu. You must click on desktop icons to start applications.
* Some applications may take time to start or process actions, so you may need to wait and take successive screenshots to see the results of your actions. E.g. if you click on Firefox and a window doesn't open, try taking another screenshot.
* The screen's resolution is {}x{}.",
        dimension("display_width_px"),
        dimension("display_height_px"),
    );
    if let Some(display) = tool.get("display_number").and_then(Value::as_u64) {
        description.push_str(&format!(
            "
* The display number is {display}"
        ));
    }
    description.push_str("
* Whenever you intend to move the cursor to click on an element like an icon, you should consult a screenshot to determine the coordinates of the element before moving the cursor.
* If you tried clicking on a program or link but it failed to load, even after waiting, try adjusting your cursor position so that the tip of the cursor visually falls on the element that you want to click.
* Make sure to click any buttons, links, icons, etc with the cursor tip in the center of the element. Don't click boxes on their edges unless asked.");
    let mut properties = json!({
        "action": { "type": "string", "enum": actions },
        "coordinate": { "type": "array", "items": { "type": "integer" }, "description": "(x, y): the pixel to move the mouse to, or the drag end." },
        "text": { "type": "string", "description": "Text to type, the key or chord to press, or modifier keys held during a click or scroll." },
    });
    if kind != "computer_20241022" {
        properties["start_coordinate"] = json!({ "type": "array", "items": { "type": "integer" }, "description": "(x, y): where `left_click_drag` starts." });
        properties["scroll_direction"] =
            json!({ "type": "string", "enum": ["up", "down", "left", "right"] });
        properties["scroll_amount"] =
            json!({ "type": "integer", "description": "Scroll wheel notches." });
        properties["duration"] =
            json!({ "type": "number", "description": "Seconds for `hold_key` or `wait`." });
    }
    if zoom {
        properties["region"] = json!({ "type": "array", "items": { "type": "integer" }, "description": "(x0, y0, x1, y1): the area `zoom` captures at full resolution." });
    }
    ClientToolDefinition {
        description,
        input_schema: json!({ "type": "object", "properties": properties, "required": ["action"] }),
        instructions: Some(crate::client_toolsets::COMPUTER_USE_INSTRUCTIONS),
    }
}

fn editor_schema(commands: &[&str], insert_field: &str) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "enum": commands },
            "path": { "type": "string", "description": "Absolute path to file or directory, e.g. `/repo/file.py` or `/repo`." },
            "file_text": { "type": "string", "description": "Required parameter of `create` command, with the content of the file to be created." },
            "old_str": { "type": "string", "description": "Required parameter of `str_replace` command containing the string in `path` to replace." },
            "new_str": { "type": "string", "description": "Optional parameter of `str_replace` command containing the new string (if not given, no string will be added)." },
            "insert_line": { "type": "integer", "description": "Required parameter of `insert` command." },
            "view_range": { "type": "array", "items": { "type": "integer" }, "description": "Optional parameter of `view` command when `path` points to a file. If none is given, the full file is shown. If provided, the file will be shown in the indicated line number range, e.g. [11, 12] will show 2 lines. Indexing at 1 to start. Setting `[start_line, -1]` shows all lines from `start_line` to the end of the file." },
        },
        "required": ["command", "path"],
    });
    let insert_text = json!({ "type": "string", "description": "Required parameter of `insert` command containing the string to insert." });
    if insert_field == "new_str" {
        schema["properties"]["new_str"]["description"] = json!(
            "Optional parameter of `str_replace` command containing the new string (if not given, no string will be added). Required parameter of `insert` command containing the string to insert."
        );
    } else {
        schema["properties"][insert_field] = insert_text;
    }
    schema
}
