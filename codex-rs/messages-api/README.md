# codex-messages-api

`codex messages-api`: Anthropic Messages API served through `codex_core::external_harness`.

## Authentication

Messages API has an independent login. Configuration and session state still use `CODEX_HOME`; credentials default to `CODEX_HOME/messages-api/auth`, or the directory selected by `--auth-dir DIR` for every command:

```text
codex messages-api login
codex messages-api login --device-auth
codex messages-api login status
codex messages-api logout
codex messages-api
```

Browser/device login obtains a separate ChatGPT OAuth grant through the existing PKCE/device flow. Refresh and logout apply only to that grant. `login --with-api-key` reads an OpenAI API key from stdin instead. `login --with-access-token` accepts a personal access token or Agent Identity token through stdin, validates it using the configured auth route, and saves it in the independent store. Storage follows `cli_auth_credentials_store` using the independent directory, including its distinct keyring entry. Neither the ordinary Codex credential store nor process Codex API/access-token/workload identity credentials are used. Existing Codex credentials are not imported; run Messages API login after upgrading.

## Message threads

Send `anthropic-beta: message-threads-2026-08-12` on every thread request. Create a thread with the full conversation, system prompt, and tools:

```json
{"model":"gpt-5.5","max_tokens":1024,"system":"Keep replies short.","messages":[{"role":"user","content":"Hello"}],"thread":{"type":"create"}}
```

Use the returned response `id` as `previous_message_id` and send only new messages:

```json
{"model":"gpt-5.5","max_tokens":1024,"messages":[{"role":"user","content":"Tell me more"}],"thread":{"type":"continue","previous_message_id":"msg_<previous response id>"}}
```

Omit `system` and `tools` on continuation requests, including `null` values. Keep fixed parameters and the set of beta headers unchanged. For streaming responses, wait for `message_stop` before continuing from the response ID. Each response ID allows one continuation.

Threads expire after five minutes and live only in memory; restarting the proxy discards them. On `404` with `error.details.error_code: thread_not_found`, replay the full conversation in a new create request. Requests to `/v1/messages/count_tokens` must omit `thread` and include the full conversation.

See Anthropic's [message threads documentation](https://platform.claude.com/docs/en/build-with-claude/message-threads) for the protocol.

## Divergences from the Messages API

- `anthropic-version`: `2026-10-21` by default; `2023-06-01` also accepted. The new version rejects removed sampling parameters, user-text citations, and direct assistant documents.
- Inline tool additions/removals resolve in conversation order. Lite models retain the initial catalog and append declarations; ordinary Responses models send the active catalog, so tool changes can invalidate their cache. Removed calls fail before client emission or server execution.
- Per-message `output_config.effort` takes effect on user input, including a system message immediately after it; tool-result-only turns defer a change. Models supporting configuration updates keep the request effort pinned and append changes to history while the feature is enabled. Other models use the request parameter. `thinking.type: disabled` and `between_tools` return 400; the Codex backend cannot implement these modes. Use `adaptive` and `output_config.effort`.
- `clear_at: next_user_message` reminders disappear after any later user message, including tool results.
- Thinking text = reasoning summary; `display: omitted` => empty. Signature = Codex reasoning item; other backends' signatures dropped on replay.
- Ignored: legacy-version `temperature`, `top_p`, `top_k`, `metadata`, `cache_control` (caching keys on `prompt_cache_key`).
- `stop_sequences`: matched client-side; generation abandoned at the match.
- `max_tokens` ignored, no output cap: Claude Code always sends one (32000 for non-Claude ids) => 26-min reasoning-only samples cut at 32000, client re-prompts "Output token limit hit".
- `tool_choice`: `any` => `required`; `tool` => that function, or `required` with only it offered when namespaced (API cannot name namespaced functions); first sample only. `none` keeps tools.
- `speed: fast` => `priority` tier; `service_tier: standard_only` => default tier. Header `x-codex-speed` = `speed` when body omits it.
- Documents: base64/URL => `input_file`; filename from `title`.
- Web tools, model with standalone search (Responses Lite, or `features.standalone_web_search`) => Codex `web.run`, run by this server:
  - one call -> one `server_tool_use`: `web_fetch` iff `web_fetch` enabled && call = one `open` of an http(s) URL; else `web_search`. Input = `{query|url}` + `web.run` arguments.
  - Results: search results carrying a URL; `page_age` null. First result's `encrypted_content` = call output (replay); others = Codex ref id.
  - Citations: Markdown link to a result => `web_search_result_location`, `cited_text` = result snippet, `encrypted_index` = ref id. Fetch `citations` ignored.
  - Markers `\u{e200}cite\u{e202}<ref>…\u{e201}`: removed from text; each ref of a result => `web_search_result_location`. Echoed text matches its original without them => original replayed.
  - Fetch: prior-context check = URL substring of user text, client tool results, web results; credential rule absent; document = `web.run` page text, never base64.
  - Search failure => `too_many_requests`/`invalid_tool_input`/`unavailable` result.
  - Runs at once beside client tool calls (Messages API defers it); `pause_turn` never sent.
  - Search context: the last two user messages without `<system-reminder>` parts.
  - Output of a search without linked results: carried by one result with an empty `url`.
- Header `x-codex-web: native` (`ccx`): requests with tools and `tool_choice` absent/`auto` get what a Codex turn offers the model (`web.run` as both tools, hosted `web_search`, or nothing), unless they declare web tools themselves. No prior-context rule, no `max_uses`. Claude Code renders these blocks as nothing and bypasses its WebFetch/WebSearch permission rules and hooks for them.
- Web tools, other models => hosted `web_search`: `web_search_tool_result.content` empty; `blocked_domains`, `max_uses` ignored; `web_fetch` => 400.
- `tool_search_tool_*` => Codex BM25 `tool_search` for both variants; regex `pattern` = Codex query. Without one, `defer_loading` tools load up front and `tool_reference` results only name the tool.
- `advisor_*` => tool-less Codex sampling on the advisor model; transcript quoted as text (encrypted reasoning omitted), proxy-written advisor prompt; `advisor_result` always plaintext. Other backends' `advisor_redacted_result` replays as unavailable advice.
- Usage: top level follows the latest model sample plus estimated context not yet covered by that sample. `iterations` preserves per-sample and advisor usage when more than one inference ran; sum those entries for billing. Older reasoning is added only when the server explicitly excludes it from its input count.
- `message_start` (headers, pings) deferred to the first output; compaction: upstream `response.created`. Failure before => HTTP error; after => `error` event.
- `message_start` input = latest server context count plus a content estimate for later items; `message_delta` updates that count. The baseline and its covered history survive tool-only executor continuations and restarts. Nested client exchanges count through their native exec projection. Claude Code keeps a field's start value when the delta reports 0 => fully cached prompt reports 1 uncached token taken from cache reads.
- MCP connector: Codex MCP client, `codex-mcp-client` handshake; elicitations declined; connections reused per URL+token; any URL scheme. Tools listed per request unless pinned or recorded. `mcp_tool_result` content = MCP text/images, other content as JSON text.
- Anthropic-defined client tools and toolsets (`bash_*`, `text_editor_*`, `memory_*`, `computer_*`, `computer_toolset_*`, `browser_toolset_*`) => functions / toolset namespaces with inline schemas and paraphrased descriptions; memory protocol and computer-use prompt appended to instructions.
- Context editing (`clear_thinking`, `clear_tool_uses`) applied before send, recomputed per request.
- `count_tokens` = local content/modality estimate (no counting endpoint). For `gpt-6.1-sol`, plaintext is calibrated by letters, digits, punctuation, whitespace runs, and newlines; Unicode bytes retain the native cost. On a separate day's measurements, plaintext weighted absolute error is 7.6% for output and 8.8% for input growth (native: 15.3% and 12.0%). Its ciphertext estimate is `max(0, ceil(encoded_bytes × 19 / 250) − 124)` per reasoning item. Other models and modality costs retain native estimates. Measured usage always takes precedence.
- Tool input = one `input_json_delta` at call end unless `eager_input_streaming`. Concurrent eager calls retain independent content-block indices and argument buffers. Stream failure inside an eager `tool_use` => error event, no resend: partial input cannot be withdrawn.
- 400: Files API sources, code execution, `container`.
- `safeguards` (`dangerous_tool_use`, Claude Code auto mode): Codex's Guardian reviews the response's client tool calls before `message_delta`, whose `safeguard_results` reports `not_flagged`, or `flagged` with `[Codex auto-review: <risk> risk] <rationale>`; review failure or timeout => `flagged` (fails closed). Tools on Claude Code's unconditional auto-mode allowlist (`Read`, `Grep`, `Glob`, `LSP`, `ToolSearch`, `ListMcpResourcesTool`, `RefreshMcpTools`, `WaitForMcpServers`, `ReportFindings`, `TodoWrite`, `Task{Create,Get,Update,List,Stop}`, `GetTask`, `ConnectGitHub`, `EnterPlanMode`, `ExitPlanMode`, `ShowOnboardingRolePicker`) => `skipped`: it allows them without reading a verdict and refuses any other skipped call. `classifier_context` (Claude Code's rules) ignored. `--no-auto-review` => 400 naming `safeguards`; Claude Code then checks actions itself.

## `/v1/images/generations`, `/v1/images/edits`

- OpenAI Images JSON => Codex Images API (`gpt-image-2`, login's image quota). Edits: `images[].image_url`, 1-5, data URLs (JSON, not multipart).
- Defaults `quality`/`size` = `auto`. Ignored: `model`, `n`, `output_format`, others unlisted. Response: `data[].b64_json` PNG and the backend's `generation_id` when present.
- Caller headers `x-codex-image-turn-id` and `x-codex-turn-metadata` are forwarded; absent a turn id, a new one is generated. `x-codex-imagegen-request-id` is returned on success and failure when the backend supplies it. Without caller metadata, a standalone image request has no surrounding Codex turn to attribute.

## Divergences from a Codex turn

- Base instructions = client system prompt + the catalog template's `## Intermediate commentary` section (absent: commentary 0.2 per final answer vs Codex 3.6, samples silent past an hour). Commentary streams as text blocks; replayed text ahead of a tool call = commentary. Client tools map to functions (`mcp__<server>__<tool>` -> namespace `mcp__<server>__`). Responses Lite requests permitting parallel tools use Codex's native JavaScript `exec`/`wait` executor: concurrent nested calls become separate client `tool_use` blocks in one response; their results resume the script before the next model sample. Sequential awaits can require further client responses. Yielded cells remain addressable by `wait`, including termination, while other scripts run. A nested shell tool's `run_in_background` remains its own task control. Disabling parallel tool use or forcing a specific tool retains direct calls.
- Requests carry what Codex turns never send when the client asks: forced `tool_choice`, advisor `max_output_tokens`, `input_file`.
- `web.run` search settings: request `allowed_domains`/`blocked_domains`/`user_location` replace config; `max_content_tokens` caps the output budget.
- Compaction: Claude Code full/`up_to` summary prompt => remote compaction v2, always `trigger: auto`, `reason: context_limit`; "Additional Instructions" ignored; recent-portion (`from`) prompt => model-written summary.
- Replacement history stored at `$CODEX_HOME/messages-api/compactions/<sha256>.json`; summary text carries only `[[codex-compaction:v2:<sha256>]]`. Inline history (~400 KB) after minutes of pings => Claude Code drops the stream ~½ of the time, each retry a full compaction. Inline `v1:` markers still decode. Entry deleted 30 d after last store/load; unstored digest => 400. Retained user messages exclude `<system-reminder>` parts.
- Analytics events not emitted; turn metadata still reports `analytics_enabled` from config.
- Claude Code session titles => tool-less requests with naming instructions and one user-text `<session>` envelope, recognized before system blocks are flattened, including billing and CLI identity prefixes in combined system strings. Only the naming instructions are retained; escaped session text follows an explicit naming task in the user's voice. Language and output schema are preserved; no commentary section. Unframed, GPT models answer the session's first prompt as the user's request.
- Claude Code agent summaries (prompt "Describe your most recent action in 3-5 words", every 30 s per running subagent) => answered here, never sent: latest tool call `Name: description|file_path|pattern|query|url|command|prompt`, else previous summary, else `Working`; usage 0. Sent, they read a sibling connection's lagging shared cache (384000-423936 of 431k-490k): watchdog lockouts 2026-10-05.
- Claude Code auto-mode classifier requests (system prompt "You are a security monitor for autonomous AI coding agents", no tools) => answered here from a Guardian review of the transcript's final call, never sampled. Stage 1 (stop `</block>`/`</severity>`): `<block>no`/`<severity>0` when allowed within 45 s, else `<block>yes`/`<severity>100`, escalating to stage 2. Stage 2 and single-stage: `<block>no</block>`, or `<block>yes</block><reason>…</reason>`; scored: `<severity>0|100</severity>`. Stages of one action share its review. A subagent hand-back review proposes no action => the stage's allow answer, without a Guardian review. Context: the resident or logged session's recorded history when exactly one pending call matches the action (short id, when given, and Claude Code's view of its input), otherwise the parsed transcript; CLAUDE.md from `<user_claude_md>`. Reviews end within 110 s, inside stage 2's 120 s; a failed one is not reused. `--no-auto-review` passes them through.
- Guardian reviews: Guardian sub-agent identity (`x-openai-subagent: guardian`, trigger `guardian_review`, `parent_response_id`, cache key `guardian:<parent thread>`, read-only permission metadata), the provider's review model (`codex-auto-review`) at low effort, Codex's Guardian policy and `guardian_*` config, CLAUDE.md as inherited instructions. One reviewer thread and connection per parent thread; each review stateless and tool-less: no reviewer conversation or delta transcripts, permission or environment context, denial circuit breaker, or catalog refresh for a missing review model.
- Watchdog lockout, permanent: a session log appending > 50 MB / 10 min, or a lane missing > max(32k, 10%) of its expected cached prefix on 3 consecutive requests each < 5 min after the last, writes `$CODEX_HOME/messages-api/lockout.json`; `/v1/messages` then answers 400 naming it, across restarts. Lift: audit, fix, delete the file, restart. `GET /admin/status` shows it.
- Session log `$CODEX_HOME/messages-api/sessions/<session>.jsonl`: thread ids, context windows, lane histories, last prompt counts; a restart resumes them. Without a count, `message_start` estimates the model-visible history using the same model calibration as `count_tokens`. Turns and WebSocket continuations do not survive. Deleted 30 d after its last write (Codex keeps rollouts).
- Fork (`--fork-session`, `/branch`): new client session whose first request repeats a resident or logged thread past a model output => root thread `forked_from` that thread inside its session, sharing `prompt_cache_key`/`session-id` (Codex: fork = new session => cold cache). Unforked: 0 of 65k cached.

## Gaps

- Client abort before the first byte goes unnoticed: the request runs on. Sampling's first byte = first output item; slower than Claude Code's first-byte window => abort, retry, abandoned sample still running.
- Unchained request in a large context (fork, new lane, first after restart): shared cache lags the chained state by tens of k tokens; a 672k prompt cached nothing for any later request. Paid once per event.

- Rebuilt `web.run` outputs (client-edited history, lost log): exact only from a search's first result or a fetch document; result-less searches rebuild empty, refusals as the error code.
