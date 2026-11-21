# Command line

`oxa worker run` runs one owner-supplied goal in its own Git worktree and writes JSON state lines.
`oxa worker list --parent kanban --json` reports workers and host load. See [owner worker runs](../work.md#owner-worker-runs).

```text
oxa [options] [--] [@files...] [messages...]
```

With no message, `oxa` opens the interface ([usage.md](usage.md)). With a message and `-p`, it runs the prompts and exits. Each `message` after the first is its own prompt. `@file` arguments are attached to the first prompt, and piped standard input is added to it.

In `-p` mode, standard output holds only the final reply and a newline. Progress goes to standard error as the run goes, flushed per event: the text of each message that is followed by tool calls, and one line per tool call (`[tool] bash: <first 100 characters of the command>`, then `[tool] bash ok 1.2s` or `failed`). A reply that is followed by another turn (a failed `done` check, a steering message) is progress too. The final reply is written once, when the run ends, and not at all when the run failed or was aborted. `--mode json` prints no progress lines; it writes one flushed record per event.

Print mode saves a session by default and announces its id on stderr before starting the agent. Continue that exact session with a new instruction:

```console
$ oxa -p "start the task"
$ oxa -p --continue SESSION_ID "continue with this instruction"
```

Replace `SESSION_ID` with the printed id. `--continue=<id>` also works. A word after `--continue` is taken as the id only if a session with that id exists; otherwise it stays part of the prompt and the most recent session continues, as before. The id is never sent as a prompt. `--continue` without a value (or `-c`) selects the most recent session. `--no-session` keeps the run ephemeral and prints no session id. An explicit continuation id cannot be combined with `--no-session` or `--session`.

## Send a session a message

```console
$ oxa send SESSION_ID "stop that, the owner chose plan B"
$ oxa send --follow-up SESSION_ID "task finished, red"
$ generate-large-message | oxa send SESSION_ID -
```

Use the full session id. This queues one message without starting an engine or requiring the session to be running. Success means **queued**, not that the model has answered.

- **Steer (the default).** A busy session takes the message after its running tool calls, before its next model request, exactly like text typed into its TUI with Enter. It does not cancel the running tool. `--steer` names this default.
- **Follow-up (`--follow-up`, before the session id).** A busy session takes the message when its turn ends, like Alt+Enter. A session that pursues a goal may keep calling tools for a long time without ending its turn, so there a follow-up that has waited `oxa.goal.followUpMaxWaitSeconds` (default 300, user setting, `0` = wait for the end) is delivered at the next tool boundary.
- An idle session starts a turn with either kind. A stopped session keeps the message until it is opened again in TUI or RPC mode, or by a headless goal run.

The TUI shows a queued steer as a `Steering:` line and a follow-up as a `Follow-up:` line. The same rules hold for an RPC owner and for a headless goal run (`oxa.inbox.headless`). The lead web page and the `worker` tool's `send` action send steers. A session started by an older oxa does not read steers: restart it, or send with `--follow-up`.
Use `-` as the text argument to read the entire message from standard input. This avoids the operating system's per-argument size limit for large messages.

The inbox lives at `inbox/<session-id>/` under the agent directory, in private `0700` directories with `0600` message files. Publication is an atomic rename within the opened inbox directory. Reads and privacy wipes also stay within the opened directories. The owner scans at startup, then uses inotify on Linux or polls every 200 ms on macOS. The agent directory remains masked by the sandbox. A message over 256 KiB is not refused: its full text is kept in the session's private, tool-readable spill directory under the temporary-data directory, and the queued follow-up names that file. Only that session (and sessions inheriting its history) can read the spill through model tools; other private agent state remains masked. The owner admits at most 64 messages or 1 MiB of inbox text across destinations; excess files remain on disk and are retried when the owner releases capacity. Oversized externally written files remain on disk and are not delivered. Once admitted to the owner's in-memory follow-up queue, a message file is removed. This is not transactional with the session transcript: a process crash can lose an admitted message or replay a file not yet removed. Worker prompts are unchanged.

## Headless results

Print, JSON and RPC runs write exactly one terminal JSON result on standard error, after diagnostics. For example:

```json
{"type":"oxa_result","outcome":"blocked","error_class":"budget_exhausted","exit_code":3}
```

The `outcome` is `done`, `nothing_to_do`, `blocked`, `failed` or `interrupted`. `error_class` is a stable error code (for example `engine_http`, `stream_broken`, `done_checks_failed` or `output_limit`), or `null` for successful and no-op runs. Diagnostics remain human-readable; callers should use the result and exit code instead of matching their wording.

The same record also carries `session_id`, `requests`, `turns`, `tokens`, `usage_scope`, `web_lookups` (the number of `web_search` calls), `files_changed`, `files_changed_evidence`, `files_changed_extra` and `files_changed_allowed`. The session id is the current saved session, or `null` for an ephemeral/unopened session. Request and token counters cover engine calls observed in this invocation (`usage_scope: "process"`), including discarded replies, summaries and prefills; resuming does not recount saved usage. `turns` counts attempted chat/subagent calls, including retries, and excludes summary/prefill calls. Child processes report their own usage separately.

`tokens` has `prompt`, `completion`, `cached`, `complete` and `unreported_requests`. Sums are over reported usage; missing usage makes them explicit lower bounds (`complete: false`), and an omitted cache count remains `null`. An in-flight request at shutdown is incomplete. Before a session opens, counters are `null` rather than a claim of zero work. `files_changed` is `null` with evidence `"unavailable"`: this worker record has no checked filesystem/commit diff. A successful tool call or model statement does not supply that evidence; the runner reports verified worktree/commit evidence separately. With `--allowed-changed-files <list>`, the record instead reports `files_changed` as the tool-observed list of files the run's successful `edit`/`write` calls named (evidence `"tool-observed"`), `files_changed_allowed` as the supplied set, and `files_changed_extra` as the observed files outside it. Both sides are resolved against the working directory and compared as workspace-relative names. This is acceptance evidence only: a shell rewrite names no file here, and nothing is refused because of it.

Every saved CLI session has a small atomic `<session>.status.json` sibling, private like the session file. `oxa status <session-id|session-file> [--session-dir <dir>]` reads it without contacting an engine. JSON mode's session header and RPC `get_state` expose its `statusFile` path. `--no-session` creates no status file.

The version-1 `oxa_status` object reports `session_id`, `pid`, `started_ms`, a monotonically increasing request `step`, `phase`, `last_tool`, `tokens`, `cost_usd`, `last_progress_ms`, `stall_timeout_ms`, `stall_deadline_ms` and the final `outcome` (`oxa_result`). Phases are `idle`, `working`, `model`, `prefill`, `compaction`, `tool`, `closing` and `finished`. Times are Unix milliseconds. Counters cover this process's work on this session, including discarded replies, summaries and prefills. Failed requests with no usage make `tokens.complete` false and increment `unreported_requests`; unreported cached usage is `null`. Local provider cost is zero; cloud cost is `null` because the engine wire does not report settled USD charges. This file contains no prompts, replies, tool arguments/results or diagnostic text.

`--stall-timeout <seconds>` opts into stall signalling (minimum 0.001 seconds). Status readers compare the recorded deadline to the current time; `oxa status` adds `stalled`. It is a progress signal and never cancels a task. Only advancing model/prefill/tool output and real phase boundaries update progress; repeated queue comments do not. Stream writes coalesce for at most one second or one quarter of the timeout, whichever is shorter; the deadline includes that grace so a coalesced delta cannot cause an early stall. Idle and finished sessions have no deadline. There is no polling thread or daemon, and reading status never changes it. A killed process can leave an active snapshot; its expired deadline remains visible. Settlement and shutdown hooks remain active work until they return. An unattended terminal record preserves earlier prompt failures; an interactive terminal record describes the UI exit. A status-write failure cannot report a successful process result.

| Exit code | Outcome | Meaning |
|---|---|---|
| 0 | `done` | The requested run completed |
| 1 | `failed` | Runtime failure, failed done checks or truncated output |
| 2 | `failed` | Usage error, unsupported option or engine unavailable |
| 3 | `blocked` | Daily cloud budget exhausted (`budget_exhausted`) |
| 4 | `nothing_to_do` | No agent run occurred, for example input handled by a hook, or RPC EOF without work |
| 5 | `blocked` | Cloud credentials unavailable (`cloud_credentials_unavailable`) |
| 6 | `blocked` | Cloud ledger unavailable (`cloud_ledger_unavailable`) |
| 7 | `blocked` | Offline policy refused the request (`offline`) |
| 8 | `blocked` | Other setup refusal, such as `cloud_sdk_unavailable` |
| 130 | `interrupted` | The run was cooperatively cancelled, for example by RPC `abort` |

For multiple prompts, the first unsuccessful outcome is retained even if a later prompt succeeds. RPC continues accepting commands after a run fails; the process reports its aggregate outcome when stdin closes. Reader-side rejections and malformed commands count as failures, and a later valid command does not clear them. Each RPC `agent_settled` record also gains an additive `result` field with that prompt's result. A handled RPC command that starts no agent turn counts as completed work; a handled prompt that starts no turn counts as `nothing_to_do`.

These results cover orderly returns, including startup refusals. A process killed before it can return cannot emit a terminal result. Help, version, export, model listing and standalone subcommands keep their existing output and status conventions (0 success, 1 runtime failure, 2 usage error).

Retries use the `retry` settings: transient gateway credentials/ledger refusals, upstream 429/5xx and connection failures retry with capped exponential backoff before output starts. Budget exhaustion, missing SDKs and other 4xx refusals are final. A transport break after output can replay the same logical request with the same idempotency key; the partial reply is shown as progress and excluded from model history. A refusal after output is final. Print mode immediately flushes retry notices to stderr; JSON and RPC modes emit flushed `notice` records. Provider-level retries (`retry.provider.maxRetries`) are visible too. `retry.enabled` switches retrying off at both layers: while it is false a failed request is sent once, with no provider retry either. `retry.maxRetries`, `retry.baseDelayMs` and `retry.maxAgentDelayMs` control the agent-level policy; provider retries have their own configured limit. This machine's own engine is treated differently: a local engine that is down or answers a plain 5xx gets `retry.localMaxTotalMs` (5 s) in total and then the error names the endpoint and how to restart it, because waiting cannot fix it. The same plain 5xx body from one endpoint stops after `retry.identical5xxRetries`; `503`, `529`, a `Retry-After` and overload markers keep the long cloud backoff. While a retry waits, the status line counts down the remaining seconds, the notice names the error, and the interrupt key cancels the wait.

A plain reply cut at its output limit also continues under the auto-retry policy. Local models keep the cut reply as context and receive an explicit continuation instruction, with a zero-token prefill when advertised and enabled. Cloud models receive a request for a shorter complete answer, with the cut reply kept only as shown progress. Each length recovery is a new logical request with a fresh idempotency key. `retry.maxRetries` bounds consecutive length recoveries; a completed model step resets the count. Disabling auto-retry leaves the cut reply terminal with `failed/output_limit`. Exhausting the bound does the same. A pending time limit or SIGTERM goes directly to the bounded wrap-up turn.

`--goal <file>` opts print/JSON mode into a persistent objective (UTF-8, at most 64 KiB). The session exposes `goal_done` and a cancellable `wait` of 1–3,600 seconds only while pursuing it. A normal assistant stop sends the same continuation instruction; two consecutive stops without tool use pause the goal with a `stalled` result. Completion or blockage requires evidence. The terminal record includes goal status, evidence and cumulative counters. A pursuing saved goal continues with `--continue` without rereading the file. Goal time and token budgets are stored with their spent counters, so restarting does not grant a fresh allowance. Ordinary sessions retain their existing tools and prefix. Interactive slash controls and inbox wake integration are separate follow-ups.

`--max-time <seconds>` sets a soft runtime limit for every mode (interactive, print, JSON and RPC), measured after startup/session loading. Fractional seconds are accepted. At the limit, or on SIGTERM, oxa lets the active tool batch finish, then gives the agent one final turn (output limit `oxa.run.wrapUpTokens`, else the model's own limit, at most 4,096 tokens; thinking off) to commit what works from this run and state what is unfinished. With `--max-time` the model also gets a short note at 70% and 90% of the time. The final `oxa_result` names the limit in `limit`: `time`, `tokens` or `signal`. That final request is not retried by the agent. The session and completed tool results are retained; the outcome is `interrupted` (130), and later prompts are left unstarted. If an in-flight or final request fails, its typed failure takes precedence over interruption; a final summary cannot hide an earlier blocked or failed request. Resume with the session id and a new instruction. RPC also exits on a limit/SIGTERM while stdin is still open. When the watched interactive session is idle, it exits without requesting a final model turn. `--no-session` continues to disable session storage.

`--max-tokens <N>` sets a positive integer token budget for every mode, with the same wrap-up and `interrupted` outcome as `--max-time`. Both limits belong to the invocation, not to one session: in interactive mode they follow `/new`, `/resume`, `/fork`, the resume picker and extension session swaps, and a new session inherits the remaining budget and any wrap-up already requested. The first SIGTERM of an interactive run with a limit is cooperative like the limit; a second SIGTERM restores the terminal and ends the process. The exit code is `interrupted` (130) only when the final turn finished cleanly; a failed turn keeps its own code. It counts newly processed prompt tokens (`prompt - cached`, floored at zero) plus output tokens across requests in this invocation, not usage restored from a session. Cached prefix tokens are free. Each subagent gets the remaining budget at spawn; its reported assistant usage is also charged to the parent, so later queued children see the reduced amount. This is a soft budget: active requests/tools (including concurrent children) finish, and the one final wrap-up turn is allowed beyond the budget. Both limits may be used together; the first reached requests wrap-up. Without either flag, normal work is unchanged.

`--usage-log <file>` writes one JSON line per model call of this invocation, in every mode, and creates the file owner-only (mode 0600; an existing file is truncated and narrowed). Each line has `request` (1-based call number), `turn` (chat turns so far; compaction, prefill and branch summaries do not count), `kind`, `model` (the session's model id, or null), `usage_reported`, `prompt`, `cached` (null when the server did not report it), `output` (completion tokens), `reasoning_tokens` (always null for now: the engine's usage does not split it out yet) and `duration_ms`. Calls that report no usage keep `usage_reported: false` with null counts. The sums of `prompt`, `cached` and `output` equal the `tokens` totals of `oxa_result` when that record says `complete`. Records hold numbers, ids and timings only: no message text, tool output or request body. `duration_ms` pairs each end with the oldest unfinished start, so it is exact for sequential calls and may swap between concurrent subagent calls. If a line cannot be written, the run continues and `oxa_result` carries `"usage_log": "write_failed"`.

For a top-level run, this is a graceful limit: an active tool or model request finishes before the final turn, so elapsed time can exceed the requested duration. The first SIGTERM does not cancel that tool. A further SIGTERM, or SIGTERM after the time limit has requested wrap-up, takes the default termination action so a stuck tool or final request can be stopped. This forced termination cannot guarantee a final result record or session flush. SIGKILL and process/system crashes cannot run a wrap-up turn. Supervised subagents retain immediate SIGTERM cancellation; the parent controls their cooperative completion.

If a local model spends its output budget on thinking without an answer, oxa retries once with thinking off. If that fallback still reaches the output limit without text or tool calls, the run fails with `error_class: "thinking_overrun"` and exit 1 in print, JSON and RPC modes. No further generation is attempted; the terminal result distinguishes this from a broken transport or an empty successful answer.

## Schema-checked answers

Use `oxa -p --output-schema schema.json "your task"` to require a JSON final answer. The file is read relative to the working directory before the session starts. Supported schema keywords are `type` (`object`, `array`, `string`, `number`, `integer`), `properties`, `items`, `enum`, `required`, boolean `additionalProperties`, `minLength` (Unicode character count), and inclusive numeric `minimum`/`maximum`, including nested schemas. The string annotations `title`, `description`, and `$schema` are also accepted as metadata only; they never trigger schema fetching or reference resolution. This offline subset supports the shipped review findings schema; it is not a general Draft 2020-12 validator. Unsupported keywords and malformed schemas are rejected rather than silently ignored; external references are never fetched. The runtime's `OutputSchema::validate` API returns a JSON Pointer and reason on failure; `matches` remains available for boolean checks. Numeric answer lexemes are checked without float rounding, with the same behavior whether dependency features enable arbitrary-precision JSON or not. Priority uses the existing finite enum form; integer/minimum enforce positive line numbers, minimum/maximum constrain confidence, minLength keeps finding strings nonempty, and additionalProperties rejects unrecognized finding/root fields.

The contract is supplied as a turn message, not a system/worker prefix change. Tools work normally. If the final text is not valid JSON matching the schema, oxa appends one repair instruction with the returned instance path and reason and tries again. A second mismatch fails with exit 1 and `error_class: "schema_mismatch"`; print mode emits no final answer on failure. JSON mode still streams all attempt events. The repair budget resets for each user prompt. Without the flag, no schema instructions or validation are added.

Validation always happens in oxa. Only cloud models explicitly advertising the `response_format` capability also receive an upstream JSON Schema `response_format`; it is never sent to the local engine. This option is process-local, is not saved as a setting, and is not inherited by workers.

## Commands

Commands take the place of the message. They do not start a session.

| Command | What it does |
|---|---|
| `oxa install <source> [-l]` | add a package (a git URL or a local path) to the settings and install it |
| `oxa remove <source> [-l]` | remove a package from the settings (`uninstall` is an alias) |
| `oxa update [source] [--extensions]` | update installed packages, all of them or the one named |
| `oxa list` | list the installed packages |
| `oxa work <taskfile> \| --issue N \| --pr N --model MODEL` | run tasks in separate git worktrees with external acceptance checks; `oxa work status [--json]` shows saved heads awaiting publication, their age and next step, without querying GitHub ([work.md](../work.md)) |
| `oxa login chatgpt --experimental [--paste] [--replace]` | explicit owner-only native ChatGPT sign-in |
| `oxa logout chatgpt` | remove the private ChatGPT token record |
| `oxa status <session-id\|session-file> [--session-dir <dir>]` | read a saved session's progress and stall signal without contacting an engine |
| `oxa cache <session-id\|path> [--json]` | [report saved prompt-cache evidence](#prompt-cache-report) for a local or cloud session |
| `oxa models [--json] [search]` | list the local and cloud models this install can serve — where each runs, its limits and which one is the default; `--json` prints one `oxa_models` object, and a search word filters by place, provider or model name; reads no key and no ledger, runs no model |
| `oxa send [--follow-up] <session-id> <text\|-> [--wait SECONDS]` | send one message to the session's TUI, RPC owner, headless goal run or a running `oxa work` worker (which takes it at its next turn boundary): a steer for its next tool boundary, or with `--follow-up` a follow-up for the end of its turn; `-` reads standard input. Prints one JSON line `{"id":<message-id>,"outcome":"queued"\|"delivered"\|"not_delivered"}`. With `--wait` the command waits up to that many seconds for the owner to take the file: `delivered` exits 0, `not_delivered` exits 3 |
| `oxa send <session-id> --retry <message-id> [--wait SECONDS]` | check the same message again without publishing it a second time (same receipt line and exit codes) |
| `oxa send <session-id> --withdraw <message-id>` | remove a message the owner has not taken yet (`withdrawn`, or `delivered` if it was already taken) |
| `oxa send <session-id> --approve <request-id> once\|session\|always\|deny` | answer a dangerous-command request filed by an unattended run (#1512). The answer requires confirmation from the session's owner-confirm broker, or directly from the controlling terminal. `always` also saves the rule key to the user settings. Answers go to the approval desk, never to the session's message queue |
| `oxa send --approvals` | list the approval requests still waiting for an answer: id, session, command and reasons. Commands refused outright are not stored in the approval desk |
| `oxa config [-l]` | print every setting with its value and where it comes from |
| `oxa local add <model.gguf> [--name ALIAS] [--ctx N] [--max-output N] [--margin-bytes N] [--enable] [--replace] [--engine-dir DIR] [-- LAUNCH ARGS]` | write the native local engine settings for one GGUF from the engine's own admission report, see [Local models](#local-models) |
| `oxa mcp <command>` | manage MCP servers, see below |
| `oxa memory <list\|show\|rm>` | list the memory index, show one memory entry, or remove one entry and its index line, see [Memory commands](#memory-commands) |
| `oxa identity [--refresh] [--offline]` | show the identity text of the system prompt; `--refresh` rewrites it from this machine and the engine, and `--offline` skips the engine |
| `oxa share serve --model ALIAS` | start the authenticated streaming OpenAI-compatible listener for explicitly selected native local models in a build with the `share` feature |
| `oxa share key <add\|list\|revoke>` | owner-only guest key and daily limit management, in a build compiled with the `share` feature |

| `oxa report-issue <draft\|submit>` | draft a scrubbed issue for the ox-llm repository, or post a draft after confirmation |
| `oxa wipe --session <id> \| --all [--yes] [--engine]` | remove the stored session files and spill files of one session or of all sessions. See [sessions.md](sessions.md#what-oxa-stores-and-how-to-remove-it) |

`oxa work board` keeps a snapshot usable when one pull request exceeds a 100-item GitHub
evidence page: that pull request is shown as **Evidence incomplete**, with the connection and
configured page limit in its reason, and is excluded from all board, review, landing, and refill
actions. Other pull requests are still published. If the merge-queue connection exceeds its page,
observed queue entries remain usable; unobserved pull requests have unknown queue state and take
no queue action. Both cases are reported on standard error.

`-l` (`--local`) writes to the project's `.oxa/settings.json` instead of the user settings. Package sources must be git URLs or local paths. npm sources are refused.

`oxa share key` is the owner-side guest key command: `key add NAME --daily-requests N --daily-tokens N` prints a new bearer once, `key list` lists the keys without their bearers, and `key revoke ID` revokes one. The store lives under `~/.oxa/share`. Only a build compiled with the `share` feature has the command, another build refuses it, and it is refused from inside an agent tool.

```console
$ oxa share key add guest --daily-requests 100 --daily-tokens 200000
$ oxa share key list
```

`oxa share serve` starts a foreground listener after the owner enables the native local engine and selects one or more local aliases with `--model`. It binds to the Tailscale interface by default and prints its `/v1` base URL. Set `--port` to use a fixed port. Explicit binds accept loopback for Tailscale Serve and addresses assigned by the local Tailscale daemon. `--same-owner` requires the connecting peer to pass the Tailscale owner check. Clients send a guest bearer and can list models or stream text chat. Ctrl+C stops admission and joins active cleanup.

Start an interactive owner session with `oxa --share-model ALIAS` to share that session's native engine. A queued owner request goes before a waiting guest, and an active guest keeps its lease through cleanup. The startup notice gives the guest API address. Use `/engine status` to see live guest activity and per-key usage for the UTC day. Exiting the owner session drains the listener.

Metered cloud aliases need `oxa.engine.inProcessCloud`, a configured price, and an entry in the user-only `oxa.share.remoteModels` allowlist. Choose each guest alias with `--model` for foreground serving or `--share-model` for an owner session. Personal subscription routes are excluded. Without `oxa.cloud.daily_budget_usd` there is no USD limit on guest spending (usage is still recorded, and guest request and token quotas apply); `oxa share` and the session status show a warning while that is the case.

`oxa update --self`, `--all` and `--models` report that they are not supported: oxa does not update itself and does not refresh model catalogues. Update oxa by installing a new build.

`oxa report-issue draft` takes `--title <text>` (required), and the notes either inline with `--notes <text>` or from a file with `--notes-file <path>` (relative paths are taken from the current directory). It writes the draft under `issue-drafts/` in the agent directory and prints its path; it posts nothing. `oxa report-issue submit <draft>` posts it after you type `yes` in a terminal. Inside an interactive oxa session, ask for the draft to be published instead: in yolo it posts at once and shows the result; otherwise oxa shows the exact text in an approval dialog (or you type `yes`), and the model can request it but never answer it.

`oxa config` lists the settings as oxa resolves them.

### Prompt-cache report

Run `oxa cache <session-id|path>` for a table, or add `--json` for a versioned `oxa_cache` object. The command reads a saved local or cloud session without contacting a model, changing the session, or adding anything to prompts. An id may be abbreviated if unique; use the `.jsonl` path if the session is outside your current session directory.

There is one row per **saved usage record**, in file order. Retries, failures, discarded replies and some prefills may have no record. The row number is not a provider request id. `input` and `cached` are recorded token counts; `null` (shown as `?` in the table) means not reported, not zero. The optional `previous_input_reuse_estimate` is what reusing the entire previous saved chat input would count as. It is a comparison baseline, not an expected engine hit or proof of which earlier request supplied a cache entry.

When two successive saved chats have known frozen system text and tool lists, the report compares those saved anchors. A system difference is a zero-based UTF-8 byte offset within the saved system text, not an offset in the full outbound request, a provider's rendered prompt, or the engine's tokens. A recorded boundary such as an epoch or compaction is an observation, not a miss cause. The runtime expectation, source request, full request divergence and miss cause remain unknown when the session did not retain that evidence. An equal anchor or equal token count does not establish an equal request. No text, hashes, tool data, or session, request or token IDs are printed.

The scan reads at most 256 MiB or 200,000 entries from the file as it existed at open. Lines over 2 MiB are skipped; an unreadable line or torn tail also makes the report partial (`scan_complete: false`). A complete scan still only describes saved records. If id lookup reaches its limit, give the file path instead.

### Foreground wire evidence

Run `oxa --wire-evidence` in an owner terminal, then exit normally to see a small JSON report on stderr. It is off by default, cannot be turned on in settings, and does not follow `/new`, `/resume`, or a model switch. Print, JSON, RPC and delegated agent runs cannot arm it.

The report compares ordered messages **at oxa's transport handoff**, after hooks and image shaping, with separate system-text and tool-list equality flags. `common_leading_messages` counts consecutive matching items from the beginning. Request and attempt numbers start at one and are local to this run; an oxa retry within the same request is another attempt. A failed or cancelled attempt, an intervening non-chat handoff, or an incomplete snapshot leaves a gap. A missing match or usage field is `null`, not zero. A rebase marker reports an observed transcript rebase, not its cause or its effect on caching.

A returned row also carries an `engine` block when the reply reported timings: the tokens the engine prefilled itself (`evaluated_prefill_tokens`), its `prefill_ms`, the checkpoint position it kept (`checkpoint_position`, with `checkpoint_known` saying whether the reply carried that key at all), and `cache_source`. `cache_source` is `unavailable`: no local engine endpoint reports whether a reused prefix came from live state, a checkpoint, a parked session, RAM or disk, and a token count cannot stand in for that answer. A reply without timings (a remote provider) has no `engine` block, not a zero one. A missing usage or timing value stays absent, never zero.

The capture keeps one prior fingerprint snapshot, allows up to 4,096 messages and 4 MiB of fingerprint input per snapshot, and stops collecting after 64 rows. It never limits chat. The report contains no message text, tool data, token IDs, fingerprints or session, model or request identifiers. It is not added to saved sessions, hook events, or `oxa cache` output. Provider serialization, engine tokens, provider or engine retries, and cache ownership are outside this observation. Neither a matching wire prefix nor a cached-token count proves which request was reused or why a cache missed, and no cache-miss cause is derived from it.

### Pull request review threads

Run `oxa pr threads <n>` to read the review threads of pull request `<n>` in the `origin` repository, or add `--json` for one versioned `oxa_pr_threads` object. It is read-only: it never resolves, replies to, or edits a thread, and it sends one fixed, bounded GraphQL query through the same trusted `gh` path the [wait probes](security.md) use. Your GitHub token stays outside the sandbox.

Each thread carries its GraphQL id, file path and line anchor, whether it is resolved or outdated, and every comment's author, timestamp, database id, commit and full body. Thread and comment ids are kept in full after a shape check (never scrubbed), so a follow-up can reference the exact thread. Bodies are scrubbed of credentials and cut at 32,000 characters with an explicit marker; `body_chars` keeps the original length and `body_cut` says whether it was cut. A `severity` (the highest `P0`–`P3` word) and a `security` flag are heuristics over the review text, not GitHub fields. Remote review text is labelled as data, not instructions.

The read is bounded: at most two pages of 100 threads, each with up to 100 comments. Evidence beyond the bound is reported as incomplete (`incomplete` with a reason, or `comments_incomplete` on a thread), never silently dropped. The command refuses with `--offline` and in a runner-owned `oxa work` worker session, where the runner owns every GitHub call.

### Local models

`oxa local add <model.gguf>` lets the owner wire one local model into the native local engine. Run it outside an agent tool. It needs no hand-written limits:

1. It finds `oxe` and `oxe-server` in the directory of the running `oxa` executable. There is no PATH or current-directory search; `--engine-dir DIR` names another installation explicitly.
2. It runs `oxe admission --json` for the model and launch profile. The engine reads only the GGUF header and reports `kv_bytes_per_position`, `scratch_bytes`, `mtp_state_bytes` and `margin_bytes` from its own allocation code, the context window, and the complete launch arguments the numbers are valid for.
3. It writes `oxa.local` (the `binary`, `models.<alias>.model` and `arguments`, and `limits.<alias>` with `context_window`, `max_output` and `allocation_budget`) into your **user** settings file only, after copying the existing file to `settings.json.bak-<unix seconds>` next to it (mode 0600). Project settings are never read or written.
4. It sets `oxa.engine.inProcessLocal` to `true` only when you pass `--enable`.

Options: `--name` is the model alias (default: the lowercased file name without extension), `--ctx` the context in positions (default: the model's own `context_length`), `--max-output` a narrower output limit (default: the context window, the engine's own bound), `--margin-bytes` the host margin (default: the engine's documented 1 GiB), `--replace` overwrites an existing alias. Everything after `--` is passed to the engine as launch arguments, for example `-- --oxe-cpus 0-7 --spec-type draft-mtp`. The admission report resolves them with the same rules as `oxe-server` and the written `arguments` are the resolved list.

**Cache capacity.** The launch the report resolves also carries the engine's cache options, so a second request (a subagent child, a worker, another session) does not wipe the first session's cached context and force a full re-prefill (45-100 s at 12K tokens on a laptop): `--state-cache default`, `--session-ram-bytes N`, `--prefix-cache default` and `--ctx-checkpoints 8`. `N` is the parked-session RAM, `MemTotal / 32` (1 GiB on a 32 GiB machine, 2 GiB on 64 GiB; 1 GiB when the memory size is unknown) and it is counted in the `scratch_bytes` of the budget. It is a default, not a cap. Your own value after `--` wins for each option, for example `-- --session-ram-bytes 4294967296 --ctx-checkpoints 12`, or `--session-ram-bytes 0` to switch the RAM off; edit `oxa.local.models.<alias>.arguments` for an alias that exists (the budget then follows `oxa local add --replace`). When `MemAvailable` is below what the weights, the engine allocations, the session RAM and the margin need, `oxa local add` prints a warning; it never refuses for it.

A value that the report or an option does not supply is an explained error and nothing is written: oxa never guesses a limit. The generated `oxa.local` is also checked by the same code that loads it at startup before the file is touched. At load time oxa still requires `MemAvailable` to cover the model file plus the budget; see [native local configuration](../../oxc-local/CONFIGURATION.md).

### Experimental ChatGPT sign-in

Run `oxa login chatgpt --experimental` in your own terminal. It prints the
experimental public-client notice and a browser URL; open that URL yourself.
The loopback callback must use `http://localhost:1455/auth/callback`, with port
1455 free. With `--paste`, paste the complete callback URL into the hidden
terminal prompt. Sign-in expires after five minutes; Ctrl+C or Escape cancels it.
No browser is launched automatically.

The command writes only the existing private
`~/.config/oxc/llm/chatgpt-auth.json`. It does not enable cloud access, change
models/settings, or print tokens/account details. Login is create-only by
default. `--replace` explicitly replaces an existing record for the same account;
it refuses an account switch. `oxa logout chatgpt` removes the checked token
record under the refresh lock and keeps that lock for concurrent refresh safety.
These commands are separate from chat/RPC and unavailable from agent tools.

A failure before atomic publication leaves the previous token record unchanged.
If the directory sync fails after atomic rename, the command returns a fixed
storage error: the new record is already visible, but its crash durability is
uncertain. The command does not roll back the record or repeat the token exchange.

### MCP commands

```console
$ oxa mcp add <name> [-l] [options] -- <command> [args...]
$ oxa mcp add <name> [-l] --url <url> [options]
$ oxa mcp remove <name> [-l]
$ oxa mcp list [--json]
$ oxa mcp login <name> [--timeout <seconds>]
$ oxa mcp logout <name>
```

- `add` options for a stdio server: `--env KEY=VALUE` (repeatable), `--cwd <dir>`, `--description <text>`, `--exposure <mode>`.
- `add` options for an HTTP server: `--url <url>`, `--header KEY=VALUE` (repeatable), `--bearer-token-env-var <VAR>`, `--oauth-client-id <id>`, `--oauth-client-secret <secret>`, `--oauth-callback-port <port>`, `--oauth-client-name <name>`, `--oauth-client-registration auto|cimd|dcr`.
- `--exposure` takes `codemode` (default), `deferred`, `direct` or `hidden`. See [mcp.md](mcp.md).
- `list` connects to every enabled server and reports its state, transport and tools. `--json` prints the same as JSON.
- `login` connects to the server and runs the OAuth sign-in. The default timeout is 300 seconds. `logout` deletes the stored credentials.

### Memory commands

```console
$ oxa memory list
$ oxa memory show <name>
$ oxa memory rm <name>
```

- The store is one Markdown file per entry under the agent directory's `memory/` folder, with a generated `MEMORY.md` index.
- The store is owner-only: the `memory/` folder is `0700` (an existing looser folder is tightened on the next save); entries and `MEMORY.md` are `0600`, written through a synced private temporary and renamed into place, and the lock file is created `0600`.
- An entry body is at most 8 KiB and its frontmatter (kind and description) at most 4 KiB; a longer save is refused and nothing is written. The index reads each entry only that far, so a larger hand-written file is never read whole.
- `list` prints the index. `show` prints an entry's kind, description and body. `rm` deletes the entry file and its index line.
- A name with no entry exits with 1. A usage error exits with 2.

## Options

### Model and output

| Option | Meaning |
|---|---|
| `--provider <name>` | provider to search for `--model`; requires `--model` |
| `--model <pattern>` | model pattern or id; accepts `provider/id` and a `:<thinking>` suffix |
| `--models <patterns>` | comma-separated model patterns for the Ctrl+P cycle |
| `--api-key <key>` | bearer key for the engine endpoint of this run; not stored |
| `--list-models [search]` | list the models the engine serves, optionally filtered by a fuzzy search, and exit |
| `--thinking <level>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max` |
| `--mode <mode>` | output mode: `text` (default), `json` or `rpc` ([rpc.md](rpc.md)) |
| `--shadow` | required with `oxa lead`; read-only proposal preset, on by default; the user setting `oxa.lead.shadow: false` turns it off |
| `--output-schema <file>` | check the final answer against a JSON schema file (a supported subset), retrying once on mismatch |
| `--print`, `-p` | non-interactive: process the prompts and exit |
| `--goal <file>` | pursue a stored UTF-8 objective in print/JSON mode; continue the saved pursuing goal after restart |
| `--max-time <seconds>` | softly limit a run, then allow one final wrap-up turn |
| `--max-tokens <N>` | softly limit new prompt + output tokens; cached tokens are free |
| `--usage-log <file>` | write one JSONL record per model call (tokens, duration, model, turn); owner-only file, no message text |
| `--wire-evidence` | show [adjacent wire observations](#foreground-wire-evidence) on exit; owner terminal only |
| `--allowed-changed-files <list>` | comma-separated allowed changed-file set; the result reports tool-observed extra files against it (acceptance evidence, never a refusal) |
| `--stall-timeout <seconds>` | flag a session as stalled after this long without progress; a signal that never cancels the run |
| `--export <input> [output]` | export a session file to HTML and exit |

| `--tui-mode <mode>` | `fullscreen` (default) or `regular` |
| `--verbose` | show more start-up information |

The live, owner-authorized Lead service exposes only the `worker` tool. It may create Git
worktrees and launch, list, steer, or stop child `oxa` processes; workers remain supervised across
service chat turns. New launches obey the current permission mode and the user-only
`oxa.workers.enabled` and `oxa.workers.max` settings. `worker start` takes optional `kind` (a job
kind such as `review`), `job` (a stable key across retries, default the worker name) and `reason`
(one line). Without `model`, the model is chosen in this order: the explicit `model`; after
`escalateAfter` failed workers on the same `job`, an `escalateTo` id; the row of `kind`; `default`;
the session model, all from the user-only `oxa.workers.models` setting (no ids are built in; within
a list the id this session used least is taken). The choice and its reason are recorded and shown by
`worker list`. `worker result {name, result: ok|failed, reason}` records how a worker's work turned
out, which is what escalation counts. A model id missing from the live catalog warns and still
starts. `worker send {name, text}` answers `receipt <id>: delivered` once the worker took the message,
`not_delivered` after `oxa.workers.sendAckSeconds` (default 10, 0 = do not wait), and
`send {name, retry: <id>}` or `{name, withdraw: <id>}` waits again or removes it. `start` and `stop`
results end with the same `receipt` line. The `oxa lead --shadow` preset is read-only,
and standalone one-shot `oxa lead chat <message>` exposes no tools because its process cannot keep
a background worker alive for a later turn.

### Sessions

| Option | Meaning |
|---|---|
| `--continue [id]`, `-c` | continue the named session; without an id, continue the most recent session of the current directory |
| `--resume`, `-r` | pick a session to resume |
| `--session <path\|id>` | open a session by file path, or by full or partial id |
| `--session-id <id>` | use the current directory's session with this id, creating it if needed |
| `--fork <path\|id>` | fork a session into a new session in the current directory |
| `--session-dir <dir>` | directory for session storage and lookup |
| `--no-session` | do not save the session (ephemeral) |
| `--name`, `-n <name>` | set the session's display name |

See [sessions.md](sessions.md).

### Tools

| Option | Meaning |
|---|---|
| `--tools`, `-t <list>` | allowlist of tool names or patterns (`*` matches any characters) |
| `--exclude-tools`, `-xt <list>` | denylist of tool names or patterns. Repeated lists and every `--no-<tool>` add to it |
| `--no-builtin-tools`, `-nbt` | disable the built-in tools by default; extension tools stay |
| `--no-tools`, `-nt` | disable all tools by default |
| `--no-mcp` | no MCP servers connect and no MCP tools are offered |
| `--no-web-search` | disable `web_search` (same as `OXA_WEB_SEARCH=0`) |
| `--no-<tool>` | disable one tool by name: `--no-read`, `--no-bash`, `--no-edit`, `--no-write`, `--no-grep`, `--no-find`, `--no-ls`, `--no-subagent`; the same as `--exclude-tools <tool>` |

### Animation

| Option | Meaning |
|---|---|
| `--animation <mode>` | animation of the `oxa` skin for this run: `arcade`, `minimal` or `off`, over the `ui.animation` setting. Anything else is a usage error. `off` is the same as `ui.reducedMotion` for one run (the working sprite, the YOLO flames and the arcade are all still); `arcade` and `minimal` clear it for the run, so the mode applies even when the setting asks for reduced motion. `OXA_REDUCED_MOTION` still wins over both |

See [settings.md](settings.md#ui) for what each mode draws.

See [tools.md](tools.md) for the tool names.

### Permissions and trust

| Option | Meaning |
|---|---|
| `--permission-mode <mode>` | `standard`, `auto`, `yolo`, `plan`, or the table modes `workspace` and `ask`; default: the `permissions.defaultMode` setting, else `auto` |
| `--standard` | permission mode standard: the permission table asks you |
| `--auto` | permission mode auto: no prompts; the sandbox and the deny-list stay on |
| `--yolo` | permission mode yolo, full access: no prompts, bash runs without the sandbox, no deny-list or protected paths for file tools ([security.md](security.md#standard-auto-and-yolo)) |
| `--approve`, `-a` | trust the project's configuration and resources for this run |
| `--no-approve`, `-na` | ignore the project's configuration and resources for this run |
| `--share-model <alias>` | share one native local model during the interactive owner session in a build with the `share` feature, repeat to select more aliases |
| `--share-bind <address>` | set an explicit bind address for `--share-model`. Sharing uses the Tailscale interface by default |
| `--offline` | offline mode: oxa's own non-local connections are refused, bash has no network, `web_search` is off, and `!cmd` and stdio MCP servers run without network. What it does not cover is in [security.md](security.md#offline-mode) |
| `--sandbox` | run bash and hooks in the OS sandbox (bubblewrap) for this interactive run, as `oxa.sandbox.interactive` does. Without it an interactive run has no OS sandbox: bash runs with your rights and dangerous commands ask. Unattended runs (subagents, workers, `oxa work`, headless goals) keep the sandbox |

See [security.md](security.md).

### Extensions, skills, prompts, themes

| Option | Meaning |
|---|---|
| `--extension`, `-e <path>` | load an extension file, or `builtin:<name>` (repeatable) |
| `--no-extensions`, `-ne` | disable extension discovery and built-in extensions |
| `--skill <path>` | load a skill file or directory (repeatable) |
| `--no-skills`, `-ns` | disable skill discovery and loading |
| `--prompt-template <path>` | load a prompt template file or directory (repeatable) |
| `--no-prompt-templates`, `-np` | disable prompt template discovery and loading |
| `--theme <path>` | load a theme file or directory (repeatable) |
| `--use-theme <name[/name]>` | set the interactive theme for this run |
| `--no-themes` | disable theme discovery and loading |
| `--no-context-files`, `-nc` | disable `AGENTS.md` and `CLAUDE.md` discovery |
| `--system-prompt <text\|path>` | replace the system prompt with text or with a file's contents |
| `--append-system-prompt <text\|path>` | append text or a file to the system prompt (repeatable) |

Options added by extensions are listed after the options when you run `oxa --help`, under "Extension CLI Flags".

### Information

| Option | Meaning |
|---|---|
| `--help`, `-h` | show the help |
| `--version`, `-v` | show the version; a dev build prints its full release version (for example `oxa 0.2.2-dev.29+1d3c121`), a stable build `oxa 0.2.1` |
| `--` | end option parsing; the rest are messages and files |

## Environment variables

Variables that oxa reads. Variables marked *set* are set by oxa for the bash tool, extension tools and check commands it starts, and you do not need to set them. Names marked *accepted and ignored* are kept for pi compatibility; nothing reads them. The names oxa uses between its own processes are listed under [Internal](#internal-variables).

Every boolean variable takes the same eight words, in any case: `1`, `true`, `yes` and `on` mean on; `0`, `false`, `no` and `off` mean off. Unset, empty and any other word leave the variable's own default in place (so a typo never turns a switch on or off by accident).

| Variable | Meaning |
|---|---|
| `OXA_ENGINE_URL` | engine endpoint for the default model entry, for an engine you started yourself (`oxe-server` listens on `http://127.0.0.1:8080` by default). With no `OXA_ENGINE_URL`, no `models.json` endpoint and no model configured, oxa dials nothing and says to configure a cloud key or run `oxa local add` |
| `OXA_AGENT_DIR` | agent directory. Default `~/.oxa/agent`; `~` is expanded |
| `OXA_LEAD_BOARD_LOG` | owner-selected absolute path to a private local board sample log; missing or refused data stays unavailable |
| `OXA_SESSION_DIR` | session storage; `--session-dir` wins over it |
| `OXA_PACKAGE_DIR` | accepted and ignored: oxa has no package store to relocate |
| `OXA_SYSTEM_SKILLS_DIR` | system skills directory, read last. Default `/usr/share/omarchy/default/agents/skills`; empty disables it |
| `OXA_OFFLINE` | `1`, `true`, `yes` or `on` turns offline mode on, as `--offline` does |
| `OXA_ALLOW_HOME_CWD` | `1`, `true`, `yes` or `on` allows a start in the home directory, `/` or a parent of home, as the user setting `oxa.allowHomeCwd` does. See [security.md](security.md#starting-in-your-home-directory) |
| `OXA_CONFIRM_MAX_REQUEST_BYTES` | largest owner confirm request a tool child may send (title and body as JSON, in bytes). A larger request is refused unread and the command says so. Default 8 MiB |
| `OXA_CONFIRM_READ_TIMEOUT_SECS` | seconds a tool child has to send its whole owner confirm request. Default 60 |
| `OXA_CONFIRM_MAX_CONNECTIONS` | owner confirm requests served at once; more are answered "none" at once. Default 64 |
| `OXA_WEB_SEARCH` | `0`, `false`, `no` or `off` disables `web_search`; `1`, `true`, `yes` or `on` says on |
| `BRAVE_SEARCH_API_KEY` | Brave Search key for `web_search`; wins over `brave-search.key` ([tools.md](tools.md#web_search)) |
| `OXA_CODEMODE_MAX_WORKERS` | script workers alive at once in the process. Default 2; overrides `codemode.maxWorkers` |
| `OXA_CLEAR_ON_SHRINK` | `1`, `true`, `yes` or `on` clears empty rows when the rendered output shrinks |
| `OXA_SKIN` | `pi`, `oxa` or `ascii` (ASCII glyphs only): the look of the interactive UI for this run, over the `ui.skin` setting |
| `OXA_REDUCED_MOTION` | `1`, `true`, `yes` or `on` switches the animations of the `oxa` skin off, as the `ui.reducedMotion` setting does; `0`, `false`, `no` or `off` says the opposite for one run |
| `NO_COLOR` | with the `oxa` skin: no colour and no animation |
| `OXA_ENV_NOTE` | `0`, `false`, `no` or `off` turns the environment note off for one run, `1`, `true`, `yes` or `on` turns it on. Wins over the `oxa.envNote` setting; unset leaves the setting in place |
| `OXA_TRUST_PROJECT` | `1` or `true` acts as `--approve` for the project trust decision, for unattended runs; an explicit `--no-approve` wins. `oxa work` and lead workers do not pass it on to their children |
| `OXA_RPC_OWNER_PRESENT` | `1` tells an RPC session that an owner can answer, so the `ask` tool is offered ([tools.md](tools.md#ask)) |
| `OXA_LEAD_SITUATION` | owner-selected absolute path to the runner's situation document for `oxa lead`, including the Lead loop's worker signals; a relative path is refused and an unreadable or unprotected file stays unavailable |
| `OXA_SKIP_VERSION_CHECK`, `OXA_SELF_UPDATE_COMMAND`, `OXA_TELEMETRY` | accepted and ignored: oxa has no version check, self-update or telemetry |
| `OXA_CACHE_RETENTION` | accepted and ignored: the engine owns its cache |
| `OXA_SHARE_VIEWER_URL`, `OXA_RADIUS_GATEWAY` | accepted and ignored: oxa does no uploads |
| `OXA_CLIPBOARD_OSC52` | `1`, `true`, `yes` or `on` also copies through OSC 52, the escape sequence that terminals relay to the clipboard |
| `VISUAL`, `EDITOR` | external editor for Ctrl+G, when `externalEditor` is not set; `VISUAL` first |
| `HTTP_PROXY`, `HTTPS_PROXY` | proxy for MCP HTTP requests only |
| `OXA_SESSION_ID`, `OXA_SESSION_FILE` | *set*: the current session id, and the session file (unset when the session is ephemeral) |
| `OXA_PROVIDER`, `OXA_MODEL`, `OXA_REASONING_LEVEL` | *set*: the provider (always `oxe`), the model id and the effective thinking level |
| `OXA_CODING_AGENT`, `AI_AGENT` | *set*: process markers; `OXA_CODING_AGENT` is `true` and `AI_AGENT` is `oxa` in child processes |
| `OXA_CONFIRM_SOCK` | *set*: the path of the session's private owner confirm socket (directory 0700, socket 0600), through which oxa commands ask you for a yes |
| `OXA_TOOL_CALL_ID` | *set*: the id of the running tool call, in the environment of an extension tool |
| `SUDO_ASKPASS` | *set*: on the one foreground `sudo` child of a named root action, the path of the session's `oxa-askpass` helper, which asks you for the password over the owner confirm socket ([security.md](security.md#grants-named-root-actions-and-the-sudo-password)); it is never set for any other command |
| `OXA_WORK_OWNER_CONFIRMED` | `1` from a surface that holds the owner's own confirmation: confirms the lane push of `oxa work` without the terminal question. Ignored while a coding-agent marker (`OXA_CODING_AGENT`, `AI_AGENT`, `OXA_SUBAGENT_DEPTH`) is set |
| `OXA_WORK_OUTPUT_DIR` | *set*: `oxa work` sets it for done-check commands to the task's private output directory, when it has one |
| `OXA_RUNNING_BIN` | *set*: inside the bash sandbox, the absolute path of the running `oxa` binary, when that file is readable there |

### Internal variables

These are set by oxa for its own child processes, or are test hooks. They are not for users, and setting them yourself is not supported.

- `OXA_SUBAGENT_DEPTH`: a parent sets `1` in a subagent child; a value above 0, or a non-number, marks the process as a child that cannot start subagents.
- `OXA_SESSION_HINT`: a parent sets `<parent session id>.s<n>` in a subagent child, and every chat request of the child names it.
- `OXA_CHILD_SOFT_TOKENS`: a parent may set the soft compaction limit of a subagent child; it overrides `oxa.compaction.softTokens`.
- `OXA_CHILD_FORK`: a parent sets the path of the fork seed of a local subagent child (its frozen prefix and its conversation, in an owner-only file it deletes when the child is gone); the child continues that conversation instead of starting its own. See [tools.md](tools.md#subagent).
- `OXA_WORKER_DEPTH`: oxa sets `1` in the environment of a lead worker session.
- `OXA_UNATTENDED`: oxa sets `1` in the environment of a worker it launches; the run counts as unattended and keeps the OS sandbox by default.
- `OXA_WORK_INBOX`: `oxa work` sets `1` in a worker; the worker then watches its session inbox like a pursuing goal does, so `oxa send` reaches it at its next turn boundary (user setting `oxa.inbox.headless: false` turns it off).
- `OXA_WORK_MODEL_DELEGATED`: `oxa work` sets `1` in a worker; a restored or project-selected model then never counts as chosen by the owner, while an actual model argument is still admitted by a protected Go key.
- `OXA_WORKTREE_TRUST_SOURCE`: `oxa work` names the trusted source checkout of a managed worktree; it counts only in `--mode json`, and only when the checkout, the trust record and the approved resources verify.
- `OXA_RUNNER_OWNS_GITHUB`: `oxa work` sets `1` in a worker: the runner owns GitHub access, so the worker runs no `gh`.
- `OXA_NATIVE_STEERING_FD`: `oxa work` passes the inherited descriptor of the steering lease to a native worker.
- `OXA_SUDO_LAUNCH`: the host runner puts a private selector in the environment of one foreground bash call and routes that call to the sudo launch it just registered; the selector is removed before the child starts and never reaches a model command.
- `OXE_API_KEY_FILE`: oxa sets it for a local engine child it starts: the private file with the child's bearer token.
- `OXE_API_KEY`: oxa does not read it; a local engine profile that sets this name or `OXE_API_KEY_FILE` is refused.
- `OXA_TEST_ISOLATION`, `OXA_TEST_PANIC`, `OXA_BRAVE_BASE_URL`: test hooks, honoured only in test processes (`OXA_BRAVE_BASE_URL` only for a loopback URL).

Some settings name the same things (`httpProxy`, `externalEditor`, `sessionDir`, `oxa.envNote`). `OXA_ENV_NOTE` wins over `oxa.envNote`. `OXA_SESSION_DIR` wins over `sessionDir`, and `BRAVE_SEARCH_API_KEY` wins over the key file. The rules for each setting are in [settings.md](settings.md).

## Examples

These run without a key and without network access against a local engine:

```sh
oxa --help
oxa --version
oxa -p "Say hello"
oxa --mode json -p "hi"
oxa -p --tools read,ls "List the files"
oxa -p --offline "hi"
```

Interactive use, and commands that need a key, an MCP server or an existing session, are shown in the other pages:

```console
$ oxa --continue
$ oxa --session-id 2f9c1a7e --name "parser work"
$ oxa --export ~/.oxa/agent/sessions/last.jsonl ~/last.html
$ oxa --permission-mode ask --tools read,grep,find,ls
$ oxa mcp add docs --url https://mcp.example.com/mcp --oauth-client-registration auto
$ oxa mcp login docs
```

`oxa --export` takes a session file, not an id. Find a session file under the session directory described in [sessions.md](sessions.md).
