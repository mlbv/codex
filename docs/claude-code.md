# Optional Claude Code integration

The daemon package contains these integrations. The separately built Messages API executable excludes them. See [Installing & building](install.md#fork-products) for the hosted build and validation process.

Enable the capabilities individually in `config.toml`:

```toml
[features]
claude_memory = true
claude_messaging = true
claude_background_tasks = true
claude_delegation = true
```

All four switches default to `false`. A disabled capability contributes no tools or instructions and starts no services. Start or resume a conversation after changing these switches.

Background execution extends `exec_command`, and delegation extends Codex's native agent tools. The switches do not add a second shell or agent-spawning interface. Tool names remain the same inside code mode.

Applicable memory, background shell, Monitor, Agent, teammate, and task instructions originate from Claude Code 2.1.317. Adapter text maps Claude's tool names onto the native Codex interfaces.

| Switch | Capability |
| --- | --- |
| `claude_memory` | Shared memory directory, instructions, and initial `MEMORY.md` context |
| `claude_messaging` | `list_agents`, `send_message` |
| `claude_background_tasks` | Background execution and automatic completion for `exec_command`; `task_output`, `background_tasks`, `task_stop`, `monitor` |
| `claude_delegation` | Native agent lifecycle and messaging integration; `task_create`, `task_get`, `task_list`, `task_update`, `team_shutdown` |

## Shared memory

The memory feature resolves Claude Code's project memory directory and supplies its instructions and index. Resolution honors Claude's configuration directory, memory directory overrides, and canonical repository root. The agent reads and edits memory through the existing shell and file-editing tools.

Project and local memory-directory settings use Codex's existing workspace trust decision. Managed and user settings apply independently of that decision.

Files are ordinary Markdown, including any existing YAML frontmatter. A write or deletion affects only its named file. The agent updates `MEMORY.md` separately as it creates, changes, or removes entries. The initial context includes the index's first 200 lines, capped at 25 KB.

## Messages between sessions

Each opted-in Codex conversation runs a helper with its own Claude-compatible session registry entry and inbox. Claude Code's native `ListAgents` and `SendMessage` can discover and address it. On Windows the inbox is an authenticated named pipe; Unix uses a local socket. Claude's `CLAUDE_CONFIG_DIR` selects the registry location.

`list_agents` returns session names, IDs, working directories, status and exact reply addresses. `send_message` accepts a name, session ID or `uds:` address. Incoming messages preserve the sender's reply address and are delivered during the current turn or start an internal continuation when idle.

From the main conversation, `send_message` also accepts `notify_when_idle`. It can accompany a message or register a subscription without sending message text. The subscription reports the recipient's next idle, exited, or unavailable state once, then closes. Idle notifications use a 750 ms debounce, and an outstanding subscription expires after twelve hours. A notification can wake the requesting conversation when idle; registering the subscription does not consume a model turn in the recipient.

Incoming peer traffic follows Claude's shared `crossSessionInbound` setting: `accept`, `hold`, or `refuse`. Managed settings take precedence over user settings. Settings from trusted project directories can tighten that policy, with `refuse` taking precedence over `hold`, and `hold` over `accept`.

Accepted messages enter the conversation. Held messages and idle notices produce a visible warning and remain queued until the applicable policy permits them. Changing the shared Claude setting to `accept` releases them automatically, including while the conversation is idle. Each idle notice is delivered once. A refused message is dropped, and an idle-notification subscription is refused under the same policy.

The helper verifies the connected pipe or socket process identity. Its default self-sent exception uses verified ancestry rooted at the conversation's own helper; explicit inbound settings still apply. The shared daemon is excluded as an ancestry root because it also owns other conversations.

## Shell tasks and Monitor

`exec_command` retains its command preparation, environment selection, permissions, sandbox, hooks, terminal controls and output budget. `run_in_background = true` returns after command admission. A foreground command still running after `yield_time_ms` also becomes a background task. Both paths deliver completion automatically. Results include `task_id` and `output_file`; `task_output` reads retained output, `background_tasks` lists tasks, and `task_stop` cancels one. `write_stdin` continues to accept the native terminal session ID for interactive input.

`timeout_ms` sets the background lifetime, measured from admission for an explicit background call or from yielding for a foreground call. The default is thirty minutes, or ten minutes for noninteractive sessions, with a two-hour maximum. Commands use the selected Codex shell. The `shell` argument selects an interpreter explicitly for Bash or PowerShell syntax.

`monitor` accepts exactly one command or WebSocket source. Command stdout supplies event lines; WebSocket messages supply events. Events arrive automatically, including when the conversation is idle. The default watch is five minutes, with a thirty-minute interactive cap and a ten-minute noninteractive cap. `task_stop` ends a watch early. WebSocket connections use the configured network routing and permissions.

Tasks retain an output file and stop during conversation teardown. Monitor output is batched and rate limited using Claude Code 2.1.317's behavior.

A background command's completion notification is delivered after its accepted output has drained, so the resumed agent can read the complete result. An explicit user interruption holds queued background events until the user resumes the conversation.

## Subagents and teammates

`spawn_agent` retains Codex's native role, model and history-inheritance arguments. Background execution is the default; `run_in_background = false` waits for the agent's result. `name` registers a persistent alias accepted by native agent controls; a new agent using an existing name takes over that alias. Messages and follow-up assignments continue the existing native agent context.

A foreground invocation returns its result once through the waiting tool call. A background invocation reports completion automatically. The parent's foreground wait follows the child's full assignment: it stays pending while the child yields for a command, receives its completion, and finishes the resumed turn.

Agent identity, paths, admission limits, resumption and shutdown remain owned by Codex's native agent runtime. Shared task ownership and named aliases refer to those same native agents.

One implicit team belongs to each root session. Its persistent task list records owners, status, metadata and dependency edges. Idle teammates claim eligible tasks and continue through the native agent mailbox. Native messaging accepts registered names, `main` for the main conversation, and `*` for named teammates. `send_message` queues child messages; `followup_task` starts or resumes child work. Messages to `main` can wake it while idle. `team_shutdown` supplies the cooperative request/approval exchange.

With local-session messaging also enabled, `list_agents` includes native agents and independent local sessions. Its `send_message` routes native targets through the agent runtime and other addresses through the peer bridge. Use the offered tool schema: peer messaging uses `to`, while native messaging uses `target`.
