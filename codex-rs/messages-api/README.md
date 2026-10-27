# codex-messages-api

`codex messages-api`: Anthropic Messages API served through `codex_core::external_harness`.

## Divergences from the Messages API

- `anthropic-version`: `2026-10-21` by default; `2023-06-01` also accepted. The new version rejects removed sampling parameters, user-text citations, and direct assistant documents.
- Inline tool additions/removals resolve in conversation order. Lite models retain the initial catalog and append declarations; ordinary Responses models send the active catalog, so tool changes can invalidate their cache. Removed calls fail before client emission or server execution.
- Per-message `output_config.effort` takes effect on user input, including a system message immediately after it; tool-result-only turns defer a change. Models supporting configuration updates keep the request effort pinned and append changes to history while the feature is enabled. Other models use the request parameter. `between_tools` maps to the lowest supported effort; Codex has no separate between-tools thinking mode.
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
- Web tools, other models => hosted `web_search`: `web_search_tool_result.content` empty; `blocked_domains`, `max_uses` ignored; `web_fetch` => 400.
- `tool_search_tool_*` => Codex BM25 `tool_search` for both variants; regex `pattern` = Codex query. Without one, `defer_loading` tools load up front and `tool_reference` results only name the tool.
- `advisor_*` => tool-less Codex sampling on the advisor model; transcript quoted as text (encrypted reasoning omitted), proxy-written advisor prompt; `advisor_result` always plaintext. Other backends' `advisor_redacted_result` replays as unavailable advice.
- Usage: top level = executor samples summed; `iterations` listed when more than one sample or advisor call.
- `message_start` (headers, pings) deferred to the first output; compaction: upstream `response.created`. Failure before => HTTP error; after => `error` event.
- `message_start` input = estimate (backend counts at completion): lane's previous prompt count + byte estimate of new items; `message_delta` = actual. Claude Code keeps a field's start value when the delta reports 0 => fully cached prompt reports 1 uncached token taken from cache reads; no sample usage => delta repeats the estimate.
- MCP connector: Codex MCP client, `codex-mcp-client` handshake; elicitations declined; connections reused per URL+token; any URL scheme. Tools listed per request unless pinned or recorded. `mcp_tool_result` content = MCP text/images, other content as JSON text.
- Anthropic-defined client tools and toolsets (`bash_*`, `text_editor_*`, `memory_*`, `computer_*`, `computer_toolset_*`, `browser_toolset_*`) => functions / toolset namespaces with inline schemas and paraphrased descriptions; memory protocol and computer-use prompt appended to instructions.
- Context editing (`clear_thinking`, `clear_tool_uses`) applied before send, recomputed per request.
- `count_tokens` = Codex byte estimate (no counting endpoint).
- Tool input = one `input_json_delta` at call end unless `eager_input_streaming`. Stream failure inside an eager `tool_use` => error event, no resend: partial input cannot be withdrawn.
- 400: Files API sources, code execution, `container`.

## `/v1/images/generations`, `/v1/images/edits`

- OpenAI Images JSON => Codex Images API (`gpt-image-2`, login's image quota). Edits: `images[].image_url`, 1-5, data URLs (JSON, not multipart).
- Defaults `quality`/`size` = `auto`. Ignored: `model`, `n`, `output_format`, others unlisted. Response: `data[].b64_json` PNG and the backend's `generation_id` when present.
- Caller headers `x-codex-image-turn-id` and `x-codex-turn-metadata` are forwarded; absent a turn id, a new one is generated. `x-codex-imagegen-request-id` is returned on success and failure when the backend supplies it. Without caller metadata, a standalone image request has no surrounding Codex turn to attribute.

## Divergences from a Codex turn

- Base instructions = client system prompt + the catalog template's `## Intermediate commentary` section (absent: commentary 0.2 per final answer vs Codex 3.6, samples silent past an hour). Commentary streams as text blocks; replayed text ahead of a tool call = commentary. Tools = client tools as functions (`mcp__<server>__<tool>` -> namespace `mcp__<server>__`), never code mode.
- Requests carry what Codex turns never send when the client asks: forced `tool_choice`, advisor `max_output_tokens`, `input_file`.
- `web.run` search settings: request `allowed_domains`/`blocked_domains`/`user_location` replace config; `max_content_tokens` caps the output budget.
- Compaction: Claude Code full/`up_to` summary prompt => remote compaction v2, always `trigger: auto`, `reason: context_limit`; "Additional Instructions" ignored; recent-portion (`from`) prompt => model-written summary.
- Replacement history stored at `$CODEX_HOME/messages-api/compactions/<sha256>.json`; summary text carries only `[[codex-compaction:v2:<sha256>]]`. Inline history (~400 KB) after minutes of pings => Claude Code drops the stream ~½ of the time, each retry a full compaction. Inline `v1:` markers still decode. Entry deleted 30 d after last store/load; unstored digest => 400. Retained user messages exclude `<system-reminder>` parts.
- Analytics events not emitted; turn metadata still reports `analytics_enabled` from config.
- Claude Code agent summaries (prompt "Describe your most recent action in 3-5 words", every 30 s per running subagent) => answered here, never sent: latest tool call `Name: description|file_path|pattern|query|url|command|prompt`, else previous summary, else `Working`; usage 0. Sent, they read a sibling connection's lagging shared cache (384000-423936 of 431k-490k): watchdog lockouts 2026-10-05.
- Watchdog lockout, permanent: a session log appending > 50 MB / 10 min, or a lane missing > max(32k, 10%) of its expected cached prefix on 3 consecutive requests each < 5 min after the last, writes `$CODEX_HOME/messages-api/lockout.json`; `/v1/messages` then answers 400 naming it, across restarts. Lift: audit, fix, delete the file, restart. `GET /admin/status` shows it.
- Session log `$CODEX_HOME/messages-api/sessions/<session>.jsonl`: thread ids, context windows, lane histories, last prompt counts; a restart resumes them. Without a count, `message_start` falls back to the byte estimate of the whole history (encrypted reasoning: 1.5–1.8× actual observed). Turns and WebSocket continuations do not survive. Deleted 30 d after its last write (Codex keeps rollouts).
- Fork (`--fork-session`, `/branch`): new client session whose first request repeats a resident or logged thread past a model output => root thread `forked_from` that thread inside its session, sharing `prompt_cache_key`/`session-id` (Codex: fork = new session => cold cache). Unforked: 0 of 65k cached.

## Gaps

- Client abort before the first byte goes unnoticed: the request runs on. Sampling's first byte = first output item; slower than Claude Code's first-byte window => abort, retry, abandoned sample still running.
- Unchained request in a large context (fork, new lane, first after restart): shared cache lags the chained state by tens of k tokens; a 672k prompt cached nothing for any later request. Paid once per event.

- Rebuilt `web.run` outputs (client-edited history, lost log): exact only from a search's first result or a fetch document; result-less searches rebuild empty, refusals as the error code.
