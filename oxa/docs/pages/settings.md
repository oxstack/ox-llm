# Settings

Settings are JSON. oxa reads two files:

| File | Scope |
|---|---|
| `~/.oxa/agent/settings.json` | user settings, for you on this machine |
| `<project>/.oxa/settings.json` | project settings, read only when the project is trusted |

Values are merged in this order, later layers winning: built-in defaults, user settings, project settings. Objects merge key by key. A scalar or a list in the project file replaces the user value. A list may also be patched with `+name` (add if absent) and `-name` (remove) entries. Environment variables override some settings; see [cli.md](cli.md#environment-variables).

Settings can be nested or dotted. These two files are the same:

```json
{ "compaction": { "reserveTokens": 16384 }, "oxa": { "mode": "plan" } }
```

```json
{ "compaction.reserveTokens": 16384, "oxa.mode": "plan" }
```

## Validation

A setting that is not in the list below is kept in the file, ignored, and reported as a warning at start-up: `unknown setting ...`. A setting with the wrong type is also reported, and the default is used. A project file that is not valid JSON, or whose top level is not an object, is ignored with a warning (`settings file is not valid JSON ...; ignored`). Invalid project settings do not stop oxa from starting. Your user file is different: if it exists but cannot be read, is empty, is not valid JSON or its top level is not an object, oxa fails safe. It keeps the most restrictive security settings (`oxa.offline` on, `oxa.sandbox.net` off, `oxa.engine.allowRemote` off, `defaultProjectTrust` never, `projectTrustPolicy` content), uses defaults for everything else, and warns at start-up with how to recover: fix or restore the file and restart. A missing file (first launch) and `{}` are normal and use the usual defaults. A live edit that fails to load keeps the settings already in use.

Settings that older files may contain but oxa does not implement are not listed here. Their use is reported as a warning: `setting ... is accepted but ignored in oxa`.

Where the user file alone counts, the table says so:

- user settings only: `defaultProjectTrust`, `projectTrustPolicy`, `httpProxy`, `oxa.allowHomeCwd`, `oxa.a2a.enabled`, `oxa.emptyReplyRetries`, `oxa.offline`, `oxa.github.tokenCommand`, `oxa.github.tokenTtlSeconds`, `oxa.engine.allowRemote`, `oxa.engine.inProcessCloud`, `oxa.cloud`, `oxa.engine.inProcessLocal`, `oxa.local`, `oxa.lead.shadow`, `oxa.lead.serve`, `oxa.lead.sessionId`, `oxa.lead.attachSessionId`, `oxa.lead.model`, `oxa.lead.github_reserve`, and every `oxa.sandbox.*` key. A project file cannot set them, because a project file cannot widen what the model can reach.
- project settings are trust-gated. In an untrusted project they are not applied. With the user-only `projectTrustPolicy: "content"`, changes to executable project content also need fresh approval. See [project trust](security.md#project-trust).

Interactive selectors (`/settings`, `/thinking` and `/model` with Ctrl+S) write the user file.

## Keys

Types: `bool`, `number` (whole, not negative), `string`, `list` (of strings), `object`, and enumerations written as `a / b / c`. `any` means the value is not checked.

The tables below are generated from the page sheet in `oxa/oxa-config/src/descriptor/sheet/`: change a row there and run `python3 tools/ci/settings_table.py --write`. The prose around them is hand-written.

<!-- generated:interface -->
### Interface

| Key | Type | Default | Meaning |
|---|---|---|---|
| `$schema` | string | `null` | editor schema hint; ignored |
| `theme` | string | `"system"` | `system`, `dark`, `light` or a name from a theme file; also `oxa` (OXA's own dark palette) and `oxa-omarchy` (follows the current Omarchy desktop theme, read-only; `system` when there is none). With the `oxa` skin the default is `oxa-omarchy` |
| `ui.animation` | `arcade` / `minimal` / `off` | `arcade` | what moves on the activity line of the `oxa` skin while a run is in progress: `arcade` is a one-row animated scene with a short `CLEAR` when the run is done; `minimal` is the small six-cell sprite; `off` is the static text badge. Never any sound. `ui.reducedMotion`, `NO_COLOR` and `TERM=dumb` give `off`. `--animation <mode>` overrides this key for one run |
| `ui.animationGraphics` | `auto` / `off` | `auto` | `auto` draws the scene as pixel art when the terminal answers the kitty graphics query at start and is kitty or Ghostty (every frame is sent once and deleted again on exit); `off` keeps the scene in text everywhere |
| `ui.animationScene` | `graphics` / `stack` / `stars` / `charge` | `null` | the scene of `ui.animation = "arcade"`. Unset: the pixel-art scene where the terminal can draw it, else `stack` (stacking columns). `stack`, `stars` (a ship in a starfield) and `charge` (the ox, in block characters) are the text scenes; `graphics` asks for the pixel-art scene on any terminal that answers the graphics query |
| `ui.notify` | `off` / `unfocused` / `always` | `unfocused` | terminal notification when a run settles (including errors/aborts) or a permission dialog opens. `unfocused` requires a terminal focus-out report; terminals without focus reporting stay quiet. Uses OSC 9 and a BEL fallback from oxa itself, with generic text (no prompt or file details); any sound is controlled by the terminal. `off` disables both |
| `ui.reducedMotion` | bool | `false` | no animation in the `oxa` skin: the working sprite and the YOLO flames are replaced by the static text badge. The environment variable `OXA_REDUCED_MOTION=1` (also `true`, `yes` or `on`) does the same for one run, and `OXA_REDUCED_MOTION=0` (also `false`, `no` or `off`) turns it off for one run; `NO_COLOR` and `TERM=dumb` imply it |
| `ui.skin` | `auto` / `pi` / `oxa` / `ascii` | `auto` | the look of the interactive UI: `oxa` is OXA's own (frames, a status line with meters, a key hint bar), `pi` is the plain look, `ascii` is `oxa` with ASCII glyphs only, and `auto` is `oxa` (ASCII when the locale is not UTF-8 or `TERM` is `linux`/`dumb`). With the `oxa` skin, `NO_COLOR` turns all colour off |
| `themes` | list | `[]` | extra theme files or directories |
| `tuiMode` | `fullscreen` / `regular` | `fullscreen` | start-up layout |
| `quietStartup` | `bool` or `"header"` | `false` | shorter start-up listing; `"header"` shows only the header |
| `hideThinkingBlock` | bool | `false` | hide thinking blocks in the transcript |
| `outputPad` | number | `1` | padding of output blocks, `0` or `1` |
| `editorPaddingX` | number | `0` | horizontal editor padding, 0 to 3 |
| `autocompleteMaxVisible` | number | `5` | rows of the autocomplete list |
| `markdown.codeBlockIndent` | string | `"  "` | prefix of each line of a code block |
| `markdown.mermaid` | `off` / `final` / `streaming` | `streaming` | accepted; diagrams are shown as code, so it has no effect |
| `fullscreenScrollbar` | `auto` / `always` / `hidden` | `auto` | scrollbar in full-screen mode; `auto` shows it while scrolling |
| `fullscreenWheelScrollLines` | number or `"auto"` | `"auto"` | lines per wheel step; `auto` speeds up fast spins |
| `fullscreenCopyOnSelect` | bool | `true` | copy a mouse selection on release |
| `fullscreenExitOutput` | `transcript` / `resume-hint` | `transcript` | what stays on screen after leaving full screen |
| `terminal.showImages` | bool | `true` | draw images in the terminal where the terminal supports it |
| `terminal.imageWidthCells` | number | `60` | width of drawn images, in cells |
| `terminal.clearOnShrink` | bool | `false` | clear empty rows when the output shrinks |
| `images.blockImages` | bool | `false` | do not send images to the model |
| `treeFilterMode` | `default` / `no-tools` / `user-only` / `labeled-only` / `all` | `default` | filter the tree opens with |
| `doubleEscapeAction` | `tree` / `fork` / `none` | `tree` | action of a double Escape on an empty editor |
| `showCacheMissNotices` | bool | `false` | show a notice for each cache miss ([compaction.md](compaction.md#cache-misses)) |
| `enableSkillCommands` | bool | `true` | list skills in the slash-command autocomplete |
| `followUpMode` | `all` / `one-at-a-time` | `one-at-a-time` | how queued follow-up messages are sent |
| `steeringMode` | `all` / `one-at-a-time` | `one-at-a-time` | how queued steering messages are sent |
<!-- /generated:interface -->

<!-- generated:models -->
### Models and thinking

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultModel` | string | `null` | model to start with, when the engine serves it |
| `enabledModels` | list | `null` | model patterns for the Ctrl+P cycle and the start model of a new session |
| `defaultThinkingLevel` | `off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max` | `medium` | thinking level of a new session |
| `modelThinkingLevels` | object | `{}` | thinking level per model id, for example `{"your-model-id": "low"}` |
| `oxa.thinking.budget` | non-negative integer / null | unset | optional reasoning-text token cap, only while thinking is on and a local engine advertises `thinking_budget`; `0` closes reasoning immediately. Unknown and remote engines omit this local field. The total output limit still applies; no default changes ([thinking policy](tools-context.md)) |
| `oxa.thinking.policy` | `fixed` / `auto` | unset | when thinking is on; unset is `auto` ([tools-context.md](tools-context.md)) |
| `oxa.thinking.toolFollowups` | `auto` / `on` / `off` | `auto` | thinking on the replies after tool results, when `oxa.thinking.policy` is `auto`. `off`: never. `on`: always. `auto`: off for routine steps, on while the run needs reasoning: after a failed call (non-zero exit, an edit that did not apply, a failing test or build), after a repeat notice, or after `oxa.thinking.escalation.fruitlessCalls` calls without progress. It goes back off after a successful edit or write or a passing test or build ([tools.md](tools.md#thinking-on-tool-follow-ups)). The level `off` always wins |
| `oxa.thinking.escalation.fruitlessCalls` | number | `6` | under `oxa.thinking.toolFollowups: auto`, the calls without an edit or write since the prompt that turn thinking on (re-reads, repeated calls, commands; reading a new file or range is progress and not counted); `0` = never on this ground |
| `cacheWarming` | `off` / `streaming` / `idle` | `idle` | `off` disables warm-up; `streaming` is accepted and behaves as `idle` |
| `oxa.bootPreload` | bool | `true` | prefill the frozen prefix when oxa starts, if the engine supports it |
| `oxa.prefill.paste` | bool | unset | while you type a message of at least 2000 bytes and stop for 0.7 seconds, with oxa idle, the engine prefills it so Enter starts sooner; only sent to an engine on this machine, and it needs an engine that supports prefill-only requests. Unset is on |
| `oxa.prefill.tools` | bool | unset | while a tool call that runs alone takes more than 2 seconds, the engine prefills the results of the calls before it, once they total at least 2048 bytes; needs an engine that supports prefill-only requests. Unset is on |
| `oxa.sampling.mode` | `greedy` \| `model` | `greedy` | `greedy` sends `temperature: 0` (deterministic; the engine's speculative decoding needs it). `model` sends the sampling the model card recommends (Qwen3.6 and Ornith: temperature 0.6, top_p 0.95, top_k 20 with thinking on; 0.7, 0.8, 20 and presence_penalty 1.5 with thinking off) for chat and subagent replies; other models stay greedy |
| `oxa.sampling.seed` | number | unset | a fixed seed sent with `oxa.sampling.mode: model` |
<!-- /generated:models -->

<!-- generated:tools -->
### Tools

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultTools` | list | `["read","bash","edit","write","grep","find","ls"]` | tools on by default; names may include `subagent`, `web_search`, `codemode` and `tool_search` |
| `oxa.bash.defaultTimeout` | number (seconds) | unset | timeout of a bash call that names none; unset is 120; `0` is no timeout. A command still running at its timeout is not killed: it moves to a background job (the result gives the job id and the wall time). A call's own `timeout: 0` is no timeout too, and beats this default |
| `oxa.bash.ciPollHint` | boolean | `true` | a `bash` command that polls for CI (a shell loop, `watch` or `--watch` around `gh run` / `gh pr checks`) keeps its result and gets a note with the native `wait` call (`until` `ci`) to use instead; `false` turns the note off. A hint, never a block. User settings only |
| `oxa.bash.waitCap` | number (seconds) | `300` | the longest a bash job read (`wait_s`) blocks; a larger `wait_s` is cut to it. User settings only. At least 1 |
| `oxa.bash.maxJobs` | number | `16` | background jobs (`bash` with `background: true`) that may run at once; above it a start goes ahead and the result warns, naming this key, unless `oxa.onExceed.bashJobs` is `stop`, which refuses the start. User settings only. At least 1 |
| `oxa.diet.output` | bool | unset | the system prompt's "be concise" rule is replaced by rules on tool use and a final answer of at most 6 lines. Unset is on |
| `oxa.diet.tools` | bool | unset | shorter `grep` and `bash` output: `grep` prints each path once, and runs of three or more blank lines in bash output become two. Unset is on |
| `oxa.emptyReplyRetries` | number | `1` | user settings only, a project file cannot set it: a cloud reply with no text and no tool call is asked once more (a new request, the empty reply is not kept); `0` turns it off. Waits between agent retries double from `retry.baseDelayMs` up to `retry.maxAgentDelayMs` (60 s) and follow a `Retry-After` up to that cap |
| `oxa.jobs.wakeOnFinish` | bool | `true` | user settings only (a project file cannot turn it on). When `true`, a background bash job that finishes on its own (not one stopped by the model or the user) queues one follow-up message with the command as written, its exit code and last lines; when the session is idle and its last run ended normally it starts a turn. A job wakes only the session it started in. `false` (opt-out): the output waits for a read |
| `oxa.liveSettings` | bool | `true` | on by default; `false` opts out. When on, a changed settings file is applied at the start of the next run without `/reload` (one `stat` per file; keys that feed the prompt rebuild it). Read from the settings the session holds, so turning it on takes effect at the next `/reload` or start. A file that no longer parses keeps the previous settings |
| `oxa.read.maxImageBytes` | number (bytes) | `20971520` | the largest image `read` sends to a model that takes images (PNG, JPEG, GIF, WebP), and the image budget of a request: a batch over it is not sent and names this limit, and when the session holds more, each request sends the newest images that fit and a text marker replaces the older ones (the stored session is unchanged; one warning is recorded). User settings only, so a project cannot raise or disable the bound. `0` is no limit. Other models get the text line `[image <name> <w>x<h>: not visible to this model]` whatever the size |
| `oxa.maxTurns` | number | unset | end a run after this many model replies with a `turn_limit` error (a cap against loops the repeat guard cannot see, such as alternating calls). Unset or `0` is no limit |
| `oxa.toolOutputCap` | number (bytes) | `16384` | user settings only (a project file cannot raise it): cap on one tool output sent to the model; on a cloud model with a known window and no value set it is 3/16 of the window in bytes (at least 16384), and a higher value you set applies to `read`, `grep`, `find` and `ls` too. When `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (never below 2048 bytes), unless this is set; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at that cap, the first page of `read` at 8 KiB within it; see [tools-context.md](tools-context.md) |
| `oxa.toolRepair` | bool | `true` | repair a malformed tool call with one extra turn ([tools.md](tools.md#repair-turn)) |
| `oxa.work.workerGithub` | `runner` / `broker` / `worker` | unset | owner-only (user layer; a project value is ignored): GitHub access of an `oxa work` worker whose task the runner owns. Unset follows the mode: yolo `worker` (own GitHub environment, `gh`, MCP, web search and subagents), auto `broker` (no token, host-side GitHub tools), standard `runner` (packet only) |
| `oxa.work.localChecks` | list of strings | `["cargo check"]` | owner-only local compile commands for `oxa work`. The runner adds `-p <crate>` for a touched crate. The hosted lane supplies the test verdict. |
| `oxa.toolRepeatGuard` | bool | `true` | answer a tool call that returned the same result three times in a row with a short note (the first note carries the suggestion and attempt; later ones count up and point back to it), and warn once (shown and kept in the session) if it goes on; the run continues ([tools.md](tools.md#repeated-calls)) |
| `oxa.goal.idleBackoffSeconds` | number (seconds) | `60` | a pursuing goal that replies without a tool call gets a continuation, then one nudge naming its three moves (a tool call, `wait`, `goal_done`); from the third such reply in a row oxa waits itself before the next request, this many seconds, doubling per further reply and capped at 900 (or this value if larger). The wait sends no request, ends on typed input, a finished job or abort, and is recorded. `0` turns the wait off. User settings only |
| `oxa.goal.idlePause` | boolean | unset | older owner switch for the idle rule of `oxa.onExceed.goalIdle`, user settings only: `true` pauses a goal after two replies without a tool call (notice and `/goal resume`), `false` only warns. Unset follows `oxa.onExceed.goalIdle` |
| `oxa.onExceed` | object | `{}` | user settings only (a project file cannot set it): what happens when a budget is exceeded, as `{"bashJobs": "warn"}` with the values `warn`, `pause` or `stop`. `bashJobs` (`oxa.bash.maxJobs`) and `subagentChildren` (`oxa.subagent.maxChildren`) default to `warn`: the work goes on with one warning that is also recorded in the session; `pause` and `stop` refuse the over-cap call and say which setting. `goalIdle` (default `pause`) is two replies without a tool call in a goal with no budget of its own, `goalUnknownUsage` (default `pause`) is a request without token usage under a goal token cap; `stop` ends the goal as blocked, `warn` only warns |
| `oxa.run.wrapUpTokens` | number (tokens) | unset | output cap of the final wrap-up turn of a run ended by `--max-time`, `--max-tokens` or SIGTERM; unset is the model's output limit, at most 4096. User settings only |
| `oxa.done.proveIt` | boolean | `false` | `true`: a run that changed files and ran no passing check afterwards gets one extra turn asking for proof (run the check, or say why none exists), then ends with a warning and a record. Takes the work-folder fingerprint at run start so `bash` edits count. User settings only |
| `oxa.done.waitForJobs` | boolean | `true` | A goal that completes, or a headless run that ends, while background jobs still run gets one extra turn to wait for or stop them, then ends with a warning and a record. `false` turns it off. User settings only |
| `oxa.done.imageCrossCheck` | boolean | `true` | A run that read an image file (`read` on a PNG, JPEG, GIF or WebP, including the placeholder line a model without image input gets) and changed files, with no check run since the image was read, gets one extra turn asking for a concrete independent cross-check of its reading (re-render and diff against the source, an independent reader such as OCR, or an invariant that must hold), or to say which image-derived claim stays unverified and why; then it ends with a warning and a record (`image_unverified` in `oxa_result`). A run that read no image, or changed no file, is never asked. Takes the work-folder fingerprint at run start so `bash` edits count. `false` opts out. User settings only |
| `oxa.goal.failureBackoffSeconds` | number (seconds) | `60` | a pursuing goal whose request fails transiently (stream cut, engine down or busy, timeout, 429 or 5xx) notes it, waits this long without a model request, and sends the same request again; each further consecutive failure doubles the wait, capped at 900 s unless this is set higher. There is no attempt cap. `0` opts out: the run ends as before. Auth errors, refusals, a repetition stop, an abort and an owner-set budget still stop the goal with the reason shown. During the wait, a steering message or an explicit model selection retries immediately with the changed request. Stop the retry first for a switch that requires compaction or a fork. A provider 429 with a retry-after over 15 minutes stops instead: switch models or wait for the quota to reset, then use `/goal resume`. Lead warns about live workers it recognizes as sharing the affected OpenCode Go pool without changing their models |
| `oxa.goal.followUpMaxWaitSeconds` | number (seconds) | `300` | a follow-up queued for a session that pursues a goal waits until the run stops or a `wait` is woken; a goal that keeps calling tools may do neither, so a follow-up that has waited this long is delivered at the next tool boundary instead. `0` keeps it waiting for the stop. A steer (typed with Enter, or `oxa send` without `--follow-up`) never waits: it is delivered at the next tool boundary. User settings only. |
| `oxa.tools.wait` | bool | `true` | user settings only (a project file cannot turn it back on): declare the `wait` tool in every session (interactive, print, RPC, lead and workers), not only while a goal is pursued. It is declared from the session start and its entry is the same in a goal, so the cached prefix does not change. `false` opts out (read at session start). Outside a goal a wait ends when the owner types a message, on cancel or on `--max-time`; with offline mode the GitHub probes stay refused |
| `oxa.wait.maxSeconds` | number (seconds) | `86400` | cap on one `wait` call's `seconds`. The default is the tool's own maximum (one day), so nothing is cut unless you set a lower value; a longer request is cut to it |
| `oxa.wait.pollSeconds` | number (seconds) | `30` | how often a `wait` with an `until` condition asks the hook sets for its state; no model request is made meanwhile |
| `oxa.subagent.enabled` | bool | unset | the `subagent` tool; unset is on unless the model is Flash or its window is below 32K |
| `oxa.subagent.isolated` | bool | `false` | run every local child in its own session and prefix instead of forking the parent's: the engine parks the parent's state and the child prefills its own context, and the call says so once |
| `oxa.subagent.smallContext` | bool | `false` | declare `subagent` on a model with a window below 32K |
| `oxa.subagent.softTokens` | number | `24000` | compaction point of a child agent; on a cloud model with a known window and no value set it is half the window (at least 24000) |
| `oxa.subagent.maxChildren` | number | `8` | children one parent session runs or queues over every call in flight, and the most `tasks` or `chain` steps one call takes. A warning threshold: above it the children still run (queued) and the result ends with a warning, unless `oxa.onExceed.subagentChildren` is `stop`, which refuses the call. User settings only. At least 1 |
| `oxa.subagent.remoteConcurrency` | number | `8` | children of one `tasks` call that run at once when some run on a cloud model (local children run one at a time). Never above `oxa.subagent.maxChildren`; lower it if your provider rate-limits parallel requests |
| `oxa.subagent.resultCap` | number (bytes) | `6144` | cap on the text a child returns; on a cloud model with a known window and no value set it is 3/32 of the window in bytes (at least 6144) |
| `oxa.webSearch.enabled` | bool | `true` | the `web_search` tool, when a key exists |
| `oxa.webSearch.provider` | `brave` / `none` | `brave` | search provider; `none` turns the tool off |
| `oxa.webSearch.maxTokens` | number | `2048` | size of the search result the provider returns |
| `codemode.enabled` | bool | `false` | the `codemode` tool ([extensions.md](extensions.md#codemode-and-tool_search)) |
| `codemode.mode` | `on` / `only` | `on` | `only` hides every other tool while codemode is on |
| `codemode.toolSearch` | bool | `false` | the `tool_search` and `call_tool` tools |
| `codemode.maxWorkers` | number | `0` | script workers alive at once; `0` is 2 |
| `done` | string list | `[]` | shell commands oxa runs itself when a run that edited files (`edit`/`write` tools) is about to finish, in the project folder, in the sandbox (10 min limit, 4 KB of output each). An entry of the form `in <dir>: <command>` runs in `<dir>` instead, for example `in oxa: cargo fmt --check` when the Rust workspace is in the `oxa/` subfolder; `<dir>` is relative to the project folder (an absolute path inside it works too). A folder that does not exist, or that leaves the project folder (through `..`, an absolute path or a symlink), is not run and the check shows `FAILED (not run)` with the reason; the notice shows the folder as `[oxa] cargo fmt --check`. A failure is sent back to the model as a message so it can fix it; after 2 automatic rounds the run stops and the failure stays in the transcript; in `-p` and `--mode json` oxa then prints `done checks failed` on standard error and exits 1 (the same code as any other failed run; the interactive mode only shows the failure). An untrusted project's `done` checks never run: unattended startup exits 1 and names the approval route (interactive mode shows a startup notice). Not re-run unless files were edited again. In a git repository, changes through `bash` are detected from the index, file list and file metadata without reading file contents. On filesystems with coarse change timestamps, a same-size in-place rewrite that restores mtime within the same ctime tick can be missed; successful `edit`/`write` calls always count as edits. Project value wins and needs project trust |
| `shellPath` | string | `null` | shell for bash and `!cmd`; it must be visible in the sandbox |
| `shellCommandPrefix` | string | `null` | text placed before each bash command, on its own line |
| `httpProxy` | string | `null` | proxy for MCP HTTP requests; user settings only |
| `httpIdleTimeoutMs` | number or `"disabled"` | `900000` | silence limit on the engine stream; `0` or `"disabled"` means none |
| `hooks` | object | `{}` | exec hooks, by event name ([extensions.md](extensions.md#exec-hooks)) |
| `extensions` | list | `[]` | extension files and directories to load |
| `skills` | list | `[]` | skill files and directories to load |
| `prompts` | list | `[]` | prompt template files and directories to load |
| `packages` | list | `[]` | installed package sources; `oxa install` edits this |
| `permissions.defaultMode` | `standard` / `auto` / `yolo` / `plan` | `auto` | permission mode at start ([security.md](security.md#standard-auto-and-yolo)); command-line flags win for one run; a project may lower it, never raise it |
<!-- /generated:tools -->

<!-- generated:sessions -->
### Sessions and compaction

| Key | Type | Default | Meaning |
|---|---|---|---|
| `sessionDir` | string | `null` | session storage; `--session-dir` and `OXA_SESSION_DIR` win over it |
| `compaction.enabled` | bool | `true` | compact automatically; `false` keeps only the emergency compaction |
| `compaction.reserveTokens` | number | `16384` | room kept free for the summary and the next reply |
| `compaction.keepRecentTokens` | number | `4000` | recent context kept verbatim after a compaction |
| `compaction.modelOverrides` | object | `null` | per-model `reserveTokens` and `keepRecentTokens` |
| `compaction.profile` | `auto`, `local` or `cloud` | `auto` | `auto` takes `cloud` for a cloud model and `local` otherwise. `cloud` compacts at 90 % of the window, keeps 20000 tokens verbatim and writes a summary of about 2000 tokens. A `compaction.keepRecentTokens`, `oxa.compaction.summaryTarget` or `oxa.compaction.softTokens` set in any layer wins over the profile. `local` keeps the values above |
| `compaction.reactivateSkills` | bool | `false` | retain invoked skills across compaction and ask the model to re-read them. User settings only |
| `oxa.compaction.idleSummary` | bool | unset | after a run that leaves the context between 80 % and 100 % of the soft limit, the summary of the next compaction is written in idle time, and that compaction reuses it with no model request if nothing it covers has changed. Unset is on |
| `oxa.compaction.midRun` | bool | unset | compact between tool steps of a run when the context passes the compaction point; unset is on |
| `oxa.compaction.summaryTarget` | number (tokens) | unset | soft length target of a compaction summary: the model is asked for about three quarters of it in words; `0` is no target. Unset is 700 |
| `oxa.compaction.softTokens` | number | `null` | context size that starts an idle compaction; unset uses the engine's point |
| `oxa.compaction.turnStart` | bool | unset | compact before a prompt is sent when the context is already above the compaction point (for example after an aborted turn); unset is on |
| `branchSummary.reserveTokens` | number | `16384` | output budget of a branch summary |
| `branchSummary.skipPrompt` | bool | `false` | leave out the "summarise the branch?" question |
<!-- /generated:sessions -->

The compaction rules are in [compaction.md](compaction.md).

<!-- generated:retries -->
### Retries

| Key | Type | Default | Meaning |
|---|---|---|---|
| `retry.enabled` | bool | `true` | retry a failed request; off sends each request once, with no provider retry |
| `retry.maxRetries` | number | `10` | retries of an agent request |
| `retry.baseDelayMs` | number | `2000` | first delay, doubled each retry |
| `retry.maxAgentDelayMs` | number | `60000` | longest delay the agent loop waits |
| `retry.localMaxTotalMs` | number | `5000` | whole retry wait when this machine's own engine fails: an engine that is down or keeps answering the same error does not recover by waiting, so oxa gives up after this budget and names the endpoint and how to restart it (`0` gives up at once). A cloud model through the local gateway is not local and keeps the long backoff |
| `retry.identical5xxRetries` | number | `2` | retries of the same plain 5xx body (status and message) from one endpoint: an identical answer will not change, so the request stops after it. `503`, `529`, a reply that names a wait (`retry after <n> s`) and overload markers keep the long backoff; `0` retries no identical body |
| `retry.provider.maxRetries` | number | `0` | silent retries of one HTTP request, before the first byte of the reply; `retry.enabled` false disables them |
| `retry.provider.maxRetryDelayMs` | number | `60000` | longest wait of one HTTP retry |
| `retry.provider.timeoutMs` | number | `null` | silence limit of the engine stream; unset uses `httpIdleTimeoutMs` |
<!-- /generated:retries -->

<!-- generated:permissions -->
### Permissions, trust and offline mode

| Key | Type | Default | Meaning |
|---|---|---|---|
| `oxa.allowHomeCwd` | bool | `false` | allow oxa to start in the home directory, `/` or a parent of home. Without it print, JSON and RPC modes refuse and interactive mode asks. An explicit `--yolo` or `--auto` on the command line also allows it, with a warning and no question; the environment variable `OXA_ALLOW_HOME_CWD` also allows it. User settings only. See [security.md](security.md#starting-in-your-home-directory) |
| `oxa.approval.always` | list | `[]` | exact `<rule> <command>` keys written by "Always allow" in the dangerous-command prompt (#1512). Each key matches one literal command of one rule. Shell parameter or command substitution markers anywhere in the command make saved grants inapplicable. Remove a line to revoke it. User settings only |
| `oxa.approval.dangerous` | list | `[]` | rules for dangerous commands, matched on the parsed command (#1512). `[]` (the default) is all built-in rules. `+id` adds a rule, `-id` removes one, `+cmd:<words>` adds a rule for a command (`+cmd:terraform destroy`), and a list without `+`/`-` entries replaces the built-in set (a plain `cmd:<words>` there turns the built-in rules off). Built-in ids: `rm-recursive-outside`, `git-push-force`, `credential-write`, `pipe-to-shell`, `disk-destroy`, `chmod-recursive-open`, `sudo`, `sql-drop`, `kill-foreign`, `oxa-self-approve`, `eval-dynamic`, `credential-read`, `git-hook-write`, `unparsed`. `credential-read` and `git-hook-write` are approval prompts, not a sandbox; `+cmd:<words>` also extends the token-printing commands `credential-read` knows. Unknown entries are ignored with a warning. Separately from these rules, a built-in publish check always runs: pushing the session's own non-default branch to `origin` with an explicit source refspec and `gh pr create\|edit\|comment` or read-only `gh` need no question, but a push with no refspec, a push to the default branch, a force push, a merge, a push elsewhere, an unresolved Git subcommand (a name that is not a builtin recognized by the classifier, so it can be a configured alias or an external `git-*` executable), or any command chained with a publish command asks (yolo never asks). A later sequential repository mutation is safe (`git push && git switch main`), while a mutation in the same concurrent pipeline asks (`git push \| git switch main`). User settings only |
| `oxa.approval.rootActions` | list | `[]` | root actions you name, so oxa does not ask before them (#784): each entry is `<name>: <absolute executable> [arguments]` where `*` matches exactly one argument, for example `boot: /usr/bin/bootctl set-default *`. Only a literal `/usr/bin/sudo` (optionally `-n`) command with the default shell and no command prefix can match. The target must be a canonical absolute executable path, owned by root and not writable by group or others, including its parent directories. Its device and inode must still match the ones recorded when the session loaded the action. Shell expansion, pipes and redirects keep the question; every other rule still applies. Only your interactive session uses the list: workers, subagents, headless and `oxa work` runs never do. Sudo still decides whether a password is needed. See [security.md](security.md#grants-named-root-actions-and-the-sudo-password). Invalid entries, including old bare target names, are ignored with a warning. User settings only |
| `oxa.approval.unattended` | enum | `"auto"` | what an unattended run (no one to ask: workers, subagents, headless goals) does with a dangerous command (#1512). `auto` refuses it under the OS sandbox and requests approval without it; `refuse` always refuses; `request` files a request for the owner (`oxa send <session> --approve <id> once\|session\|always\|deny`, or the Lead web approvals page). A `session` answer lasts for the current run and expires when a persisted session is resumed. Shell input classified as `pipe-to-shell` requires direct owner review because the command text cannot identify the input bytes for a retry. Nothing waits for an answer. User settings only |
| `oxa.mode` | `plan` / `workspace` / `ask` | `workspace` | table mode at start |
| `defaultProjectTrust` | `ask` / `always` / `never` | `ask` | what happens in a project that is not trusted yet; user settings only |
| `projectTrustPolicy` | `folder` / `content` | `folder` | trusted folders load new or changed resources with a notice; `content` requires fresh approval of changed executable project files. User settings only. See [security.md](security.md#project-trust) |
| `oxa.offline` | bool | `false` | same as `--offline`; user settings only |
| `oxa.engine.allowRemote` | bool | `false` | allow an engine on another host; user settings only |
| `oxa.engine.inProcessLocal` | bool | `false` | select the in-process owned local engine, including offline; user settings only |
| `oxa.inbox.headless` | bool | `true` | a headless print or JSON run (`--mode json`) of a persisted session whose goal is pursuing, or an `oxa work` worker, takes `oxa send` follow-ups from that session's inbox for the length of the run. Each message is an ordinary follow-up user message, at most 256 KiB and 64 queued; a larger file stays on disk. Other runs without a pursuing goal never watch the inbox, and `false` turns this off, `oxa work` workers included. Read from the user settings file only; a project file cannot change it |
| `oxa.local` | object | `{}` | literal native binary/model/arguments/environment and model limits; user settings only; `oxa local add` writes it from the engine's admission report, including the cache options (state cache, session RAM, prefix cache, 8 checkpoints; owner values win) ([Local models](cli.md#local-models)); see [native local configuration](../../oxc-local/CONFIGURATION.md) |
| `oxa.a2a.enabled` | bool | `true` | sessions accept follow-ups and control (steer, pause, resume, cancel) from the lead over a local authenticated channel, from the same user only. `false`: file inbox only, and `nativeSteering` is refused. User settings only. See [security.md](security.md#agent-control-channel) |
| `oxa.lead.shadow` | bool | `true` | the read-only `oxa lead --shadow` proposal preset; `false` turns it off; user settings only; no live runner actions |
| `oxa.engine.inProcessCloud` | bool | unset | select the in-process Rust cloud gateway for sessions and model discovery. Unset is on when the owner's key file has an `openrouter`, `openai` or `anthropic` key or `oxa login chatgpt` was done; `false` opts out. User settings only |
| `oxa.cloud.opencode.enabled` | bool | on when a key exists | OpenCode Go is on when the `opencode` key exists in the protected cloud key file and the run has an actual `--model` argument naming a Go model (including a delegated worker or nested child), or an owner model scope that is not a local alias; `false` opts out everywhere; project settings and restored sessions cannot enable it; offline mode refuses; user settings only |
| `oxa.cloud` | object | `{}` | native cloud models, prices, USD budget and subscription caps; user settings only; see below |
| `oxa.lead.serve` | object | `{}` | the lead web access scaffold: `enabled`, `identity` (`local` or `tailscale`), `bind` (a loopback address), `port`, `allowed_identities`. Off by default and no listener is installed yet; the default identity mode is `local` unless selected otherwise; user settings only. See [lead-web.md](lead-web.md) |
| `oxa.lead.serve.sample_secs` | number | `60` | seconds between two GitHub samples `oxa lead serve` runs itself (default 60); needs a token source (`oxa.github.tokenCommand` or `GH_TOKEN`); `Retry` on the page samples at once and a failed sample backs off to 15 minutes; user settings only |
| `oxa.lead.serve.repo` | string | unset | `OWNER/REPO` the in-process sampler reads; unset uses the serving project's `origin` URL; user settings only |
| `oxa.lead.sessionId` | string | unset | exact ID of the dedicated Lead Session created by the native owner operation, from owner-private user settings. Free text is disabled when unset. |
| `oxa.lead.attachSessionId` | string | unset | exact ID of a running owner session the lead web page attaches to (live transcript, messages through its inbox like `oxa send`), from owner-private user settings; with `oxa.lead.serve.owner_chat` it replaces the dedicated Lead chat; restart the service after changing it. |
| `oxa.lead.model` | string | unset | exact model id the Lead conversation resolves through the normal `--model` selector and gateway; user settings only, read from the owner-private settings file. Unset keeps the ordinary startup model; a route that does not resolve is reported, never silently replaced. A metered model still needs its daily USD cap, so choose a subscription route while the cap is absent. |
| `oxa.envNote` | bool | unset | append the environment note to each prompt you send: the first one in a session (and the first of a new day, or after a compaction) gets a short `<env>` block with the date, time, time zone, locale, kernel, model, context size, thinking level and permission mode; later ones get `[local time HH:MM ZONE]`. It is never put in the system prompt, so the prompt cache stays exact. Unset is on; `OXA_ENV_NOTE=0` or `=1` overrides it for one run |
| `oxa.github.tokenCommand` | list | `[]` | trusted argv that prints a GitHub token for `oxa work --publish` and lane CI; configured tokens are masked in shell output, never injected into shell environments; operands must be trusted absolute executable paths; empty uses the stored GitHub login; user settings only |
| `oxa.github.tokenTtlSeconds` | number | `300` | seconds between token lookups for networked bash (configured tokens are only for masking); `0` reads on each use; a rejected stored login invalidates the cache; user settings only |
| `oxa.sandbox.net` | `on` / `off` | `on` | network for bash in workspace and ask modes; `off` holds without the OS sandbox too (bash then runs through bubblewrap without network, or is refused) |
| `oxa.sandbox.allowHomeWrite` | bool | `false` | make the home directory the bash workspace when oxa starts in it, in every mode (its folders and files writable; dotfiles, dot-folders, `~/bin` and the home folder itself stay read-only; never `/` or a parent of home). In interactive mode confirming the start question does the same. User settings only. See [security.md](security.md#starting-in-your-home-directory) |
| `oxa.sandbox.interactive` | bool | `false` | the OS sandbox (bubblewrap) for bash in an attended run (the TUI, a person's `oxa -p`, the lead chat). Off by default: bash runs with your rights and dangerous commands ask. Exec hooks keep their dedicated sandbox. `--sandbox` turns it on for one run. Yolo has no OS sandbox. User settings only |
| `oxa.sandbox.readPaths` | list | `[]` | extra read-only paths for bash; `~/` is expanded; user settings only |
| `oxa.sandbox.unattended` | bool | `true` | the OS sandbox (bubblewrap) for bash in an unattended run, including a subagent, a lead worker, `oxa work`, an ownerless RPC host, or a headless run that starts or resumes a pursuing goal. Stays on by default; set `false` only if you accept bash without it there. User settings only |
| `oxa.sandbox.writePaths` | list | `[]` | extra writable paths for bash; `~/` is expanded; user settings only |
| `oxa.sandbox.sockets` | list | `[]` | host runtime sockets (for example `/run/docker.sock`, `/run/podman/podman.sock`) that stay reachable in network-enabled sandboxed bash with Bubblewrap FD binds; absolute paths of existing sockets; user settings only, a project file cannot list them. The start notice names each one. The start notice warns about the Docker socket’s root access. An online command fails closed if a configured socket cannot be pinned or Bubblewrap cannot mount the descriptor. Offline, a listed socket is masked at its path instead, and a socket below a writable mount (the workspace, a write bind, `$HOME` or `/tmp`) is refused: its mask could be moved away by renaming the parent, so relocate the socket or drop the setting. Yolo bash has no sandbox and needs no entry |
| `oxa.sandbox.gitConfigAllow` | list | `["branch.*", "user.*", "remote.*.fetch"]` | git config keys that sandboxed bash may change in the repository's `config` (`git branch -m`, `git config user.name`, a changed fetch refspec work); `*` matches one dotted component or a subsection, keys compare case-insensitively. Any other change is restored right after the command, with a note in its output, because git runs some keys (`core.hooksPath`, `alias.*`, `core.fsmonitor`) outside the sandbox later; an empty list restores every change; user settings only |
| `oxa.sandbox.scratchWarnSize` | string | `20GiB` | warn (never block) when this session's sandbox `/tmp` scratch holds more than this: one line is added to the command output, again only after the size has doubled; a size such as `20GiB` or `512MiB`, `off` disables; user settings only |
<!-- /generated:permissions -->

### Native cloud gateway

See [quickstart.md](quickstart.md#cloud-models) for the keys file and a first setup of each provider.

`oxa.engine.inProcessCloud` selects the Rust gateway for sessions and
`--list-models`. Unset, it is on as soon as the owner's key file holds an
`openrouter`, `openai` or `anthropic` key, or after `oxa login chatgpt`; with it
on, `oxa.cloud.discover_models` also defaults to `true` — except for an owner
who set `oxa.engine.inProcessCloud` explicitly together with their own
`oxa.cloud.models`, whose catalog is then used as written unless they ask for
discovery too. An explicit value
always wins, so `false` opts out (a local HTTP engine with a key in the file
sets it to `false`). There is no built-in model table: model data is always
fetched. The model list comes from each provider's models endpoint with your
key; limits and prices the provider does not list come from OpenRouter's public
model catalog (`GET https://openrouter.ai/api/v1/models`, sent with no key and
no data about you, matched by model id), and the last successful fetch stays in
the on-disk catalog cache for when a fetch fails. A model nobody lists limits
for gets a conservative default window (128,000 tokens, 8,192 output) and a
model nobody prices is usable with unknown cost, recorded as uncertain spend
(`--list-models` start-up notices say how many and where the data came from).
An entry of yours under `oxa.cloud.models` with the same alias
(`provider/upstream-model`) always wins; set one to pin a limit or price.
Public metadata is used only for a provider's default endpoint, never when you
point discovery at a proxy. Offline mode
refuses before opening the gateway; configuration or gateway failures are returned
directly. Project settings cannot enable this transport or change its cloud policy.

The native gateway includes recommended Anthropic, OpenAI and OpenRouter models
when the corresponding API key is configured. These built-in routes use the
providers' standard endpoints and bundled prices. An entry in `oxa.cloud.models`
with the same alias replaces the entire recommendation, including its pricing.
Set `oxa.cloud.recommended_models` to `false` in user settings to disable all
recommendations without disabling configured models or live discovery. The
built-in catalog makes no discovery requests and does not grant guest access.

The OpenCode Go subscription is the exception to that switch: a Go key in the
owner's secrets file (`~/.config/oxc/llm/cloud-secrets.json`, the `opencode`
entry) already enables the Go transport for an actual Go model argument, including
a delegated worker or nested child, without `inProcessCloud`.
`oxa.cloud.opencode.enabled: false` in the user settings opts out again, and `true`
enables Go even for a project-selected or restored model. Transport selection stays
user-only: a project settings file and a model restored by `--continue` never turn
the transport on by themselves. A configured local alias keeps a request local
only when the alias owns the asked model exactly; a fuzzy local match does not
hide the Go model of that name.

`oxa.cloud.discover_models` defaults to `true` with a key (see above) and
`false` otherwise; it discovers models at startup for the
configured OpenRouter, OpenAI and Anthropic API keys. It also enables the
OpenCode model cache when that provider is configured. A fresh cached catalog
avoids a discovery request. A stale OpenRouter catalog is refreshed at startup
before its prices can be used to reserve spend. Other stale catalogs can be
shown while a bounded refresh updates the cache for later starts. A failed live
lookup uses the configured catalog and reports the failure. Discovery stays
off in offline mode.

OpenRouter supplies model limits and token prices. For a discovered model with
no known rates, the owner can opt in by adding its exact public alias to
`oxa.cloud.unpriced_discovered_pricing` in user settings. Each entry needs
positive `input_per_million` and `output_per_million` USD rates. Choose rates
that bound the expected provider charge. The gateway uses them for the normal
budget reservation and records the settlement as `estimated_cost_usd`. Missing
final usage retains the reservation. Estimates count against the daily cap and
are recorded separately from actual costs. The setting is empty by default and
is only read when discovery is enabled. The model still needs known token limits.
Known provider or configured prices take precedence over this setting.
When the same provider has several configured base URLs, select the discovery
endpoint with `oxa.cloud.discovery.<provider>.base_url`.

Model rules. A built-in table keyed by model id (not by provider URL) says how an
OpenAI-compatible or OpenCode Go route writes each model's thinking switch, whether its
`reasoning_content` is replayed, which field carries the output cap and what window it has. Rows
exist for GLM-5.3 (`thinking` with `clear_thinking:false`), Kimi K3 and DeepSeek V4 (reasoning
carried on every tool-call turn; DeepSeek also the `thinking` switch) and for local aliases (the
engine's own template switch; the local request body is not rewritten). `oxa.cloud.model_rules`
in user settings overrides or adds rows: an object of id patterns (`*` wildcard, case-insensitive)
to fields `thinking` (`effort`, `object`, `object-keep`, `template`), `replay` (`as-is`, `always`,
`strip`), `token_field` (`max_tokens`, `max_completion_tokens`), `context_window`, `max_output`
(positive numbers) and `route` (`cloud`, `local`). Fields left out keep the built-in value; later
matches win; an unknown key or value is refused at start. A model with no row gets the plain
request.

OpenRouter privacy routing is the owner's choice and is never forced. By
default oxa sends no `provider` preferences, so your OpenRouter account and
workspace settings apply. To choose, set `oxa.cloud.openrouter_provider` in user
settings: `zdr` (`true` or `false`) limits or allows routing by zero data
retention, and `data_collection` (`allow` or `deny`) controls providers that may
train on or keep prompts. Values are sent exactly as set; any other key or value
is refused at load. A client request cannot override them.

The merged catalog is available through the model picker, `oxa models` and `--list-models`.
The LAN guest catalog lists only guest-allowlisted models.

Configure `anthropic`, `openai` (Responses API) or `chatgpt` (subscription) routes
under `oxa.cloud.models`, keyed by the model name used
with `--model`. Supply the upstream model, context/output limits and current
provider prices. This example uses an illustrative model and prices:

```json
{
  "oxa": {
    "engine": { "inProcessCloud": true },
    "cloud": {
      "daily_budget_usd": 10,
      "models": {
        "example-cloud": {
          "provider": "anthropic",
          "upstream_model": "replace-with-your-model",
          "context_window": 128000,
          "max_output": 8192,
          "pricing": {
            "input_per_million": 2,
            "output_per_million": 3,
            "cache_read_per_million": 0.2,
            "cache_write_per_million": 2.5
          }
        }
      }
    }
  }
}
```

The key is read for each request from the existing private
`~/.config/oxc/llm/cloud-secrets.json` (owner-only, mode `0600`, outside a checkout).
Keep keys out of settings. The file uses the existing provider-key JSON format.

To add a named provider, set `oxa.cloud.providers.myplan` in **user settings**:

```json
{ "api": "openai-chat", "base_url": "https://example.invalid/compatible/v1",
  "key": "myplan", "billing": "subscription" }
```

Use the provider's HTTPS API base and put the credential under `myplan` in
the existing secrets file. `api` accepts `openai-chat` (Chat Completions) or
`anthropic` (Messages). A keyed provider discovers models from `<base_url>/models`
and caches the last successful list. `oxa --list-models myplan` filters that list;
pass a qualified model name from the list to `oxa --model`. Explicit owner discovery and gateway
opt-outs still win. Offline mode does not contact the provider.

Use `subscription` for token accounting or `metered` for USD accounting. When a
provider does not publish rates, an owner model entry can supply them; public
catalog prices for other providers do not apply. A model entry can supply limits
and an upstream model ID, but cannot change the named provider's URL or key.
A named Chat Completions provider can finish a reply or tool call without reporting
usage. The ledger retains a conservative reservation instead of recording zero usage.

Private settlement rows live in `<agent directory>/cloud-ledger/`. Listing only explicitly configured routes
reads neither the key nor the ledger and contacts no provider. `daily_budget_usd` is
optional: without it there is no budget check, requests are admitted (including a
model with no price) and their cost is still recorded in the ledger (a request whose
cost cannot be bounded is recorded with `cost_unknown`). When set it must be a
positive number, a missing price refuses a request before credentials or provider I/O
(`cloud_pricing_unavailable`), and a request that would exceed it is refused with
`budget_exceeded` (HTTP 429). A cap set after earlier uncapped requests of unknown
cost the same day is refused with `cost_unknown_history`, which says so, instead of
claiming the budget is exhausted. Requests
retain their budget reservation until the private row is durable.

An `openai` route uses the same model/price fields, reads the `openai` key from
that key file, and defaults to `https://api.openai.com/v1`. Both native providers
preserve tool argument strings, including arguments supplied only by final
provider events. Completed native replay is authenticated to the provider,
model, account and exact visible history. It is stored only in private session
records, omitted from public output, and dropped when compaction changes context.
An invalid saved binding refuses the request before provider I/O.

A `chatgpt` route defaults to `https://chatgpt.com/backend-api/codex` and uses the
existing owner-only `chatgpt-auth.json` beside the key file. It does not discover
other applications' credentials or start a sign-in. After `oxa login chatgpt` the plan models
(`chatgpt/<model>` for every model the plan's own list offers, fetched live from `GET https://chatgpt.com/backend-api/codex/models` with your token, cached like the other provider lists) exist without a route in your settings; the window comes from that list, the output limit from OpenRouter's public catalog, and they are unpriced subscription routes;
the optional user-only `oxa.cloud.subscription_limits` object (positive integer
`daily_requests` and `daily_tokens`) only adds warnings. The separate optional `oxa.cloud.subscription` refresh object
accepts a public `client_id` override (`[A-Za-z0-9_.-]{1,128}`); its default is the
experimental public identity approved in #252. The refresh endpoint is fixed at
`https://auth.openai.com/oauth/token` and cannot be overridden in settings.
`refresh_timeout_ms` defaults to 15000, must be positive and is bounded by the
client's 30-second maximum. These settings contain no access or refresh token.
Client transport tests inject loopback endpoints privately; production refresh
configuration never supplies one.

Subscription calls retain actual
token usage and settle zero USD. Unknown paid work retains its full token bound;
the cap includes concurrent and crashed-process reservations. It never falls back
to a paid API balance. Missing metered pricing still refuses metered requests when a daily budget is set.
`oxa.cloud.subscription_limits` is optional for the ChatGPT plan and OpenCode Go
and has no default, so a plan route works right after `oxa login chatgpt`. If configured,
oxa warns when projected usage reaches 80% or 100% of either limit, including
concurrent reservations, and continues the request. Both routes keep
recording requests and tokens, including conservative reservations for unknown
outcomes. Provider limits and credential, offline, and LAN access checks still
apply. Missing metered pricing still refuses metered requests.

OpenCode Go and every other subscription request reserve tokens only. Their cost
is unknown to oxa, so no daily USD headroom is held for them and no local USD
cap or request bound pauses their admission: a tiny or fully consumed
`oxa.cloud.daily_budget_usd` never returns a local refusal for them, and the
former `oxa.cloud.opencode.daily_budget_usd` and
`oxa.cloud.opencode.max_request_usd` settings are no longer read. OpenCode's
reported paid charges settle into the ledger and count in the shared daily
total. Missing or estimated Go costs stay unknown, with no actual USD amount.
Historical Go estimates remain readable and do not count toward the daily total.
Holds written by older versions — including stored `reserved_usd` and `go_usd`
amounts — stay readable with their tokens counted toward the subscription caps,
while their recorded USD reserves nothing. OpenCode controls whether a call falls
back to its paid balance; if it reports a higher charge than expected, the
ledger records the full amount and admission continues.

The repository's automated Go pull-request review checks at most 200 GitHub comment pages (20,000
comments) for the configured daily quota and prior same-diff receipts. If GitHub history exceeds
that bound, review is deferred before a quota reservation or provider access; incomplete history
never permits an additional review.

<!-- generated:other -->
### Other

| Key | Type | Default | Meaning |
|---|---|---|---|
| `externalEditor` | string | `null` | editor for Ctrl+G; wins over `VISUAL` and `EDITOR` |
| `oxa.reportIssue.repo` | string | `null` (reports go to `oxstack/ox-llm`) | repository for `oxa report-issue submit`, as `OWNER/NAME`; a value that is not `OWNER/NAME` (for example `off`) turns reporting off |
| `oxa.reportIssue.dailyNew` | number | `null` | owner cap on new issues per worker per UTC day for semantic dedupe. Unset means no check. Over the cap the post preview warns and records it; the owner's yes still posts |
| `oxa.reportIssue.semanticDedupe` | bool | `false` | verify dogfood reports against current main and search open and recently closed issues before posting. Matching reports become comments, and the one preview question covers the post. New issues are limited to one per task; the per-worker daily count is `oxa.reportIssue.dailyNew` |
| `oxa.builtinSkills` | bool | `true` | include the skills built into oxa |
| `oxa.share.remoteModels` | string list | `[]` | user-only allowlist of metered cloud aliases eligible for explicit LAN sharing; personal subscriptions are excluded |
| `oxa.lead.github_reserve` | number | `null` | user settings only. Requests kept back per GitHub App resource (core and graphql) for required Lead reads: `oxa work board --app-quota-shadow` runs its optional reads (lead panels, queued-run pages, test reports) only while the App window stays above this reserve, and records each deferral in the sample (`quota_deferred`) instead of failing. Unset means no gate. Model and subscription limits are not touched. Needs `--app-quota-shadow`; `--app-quota-reserve N` overrides it for one run |
| `oxa.skills.includeSynced` | bool | `false` | also list the skills another product synced into `~/.agents/skills` (a `synced/` folder, or a folder whose `manifest.json` lists `skillId`s); they are written for that product's tools, so oxa skips them by default |
| `oxa.workers.enabled` | bool | `true` | user settings only (a project file cannot change it). The `worker` tool (start, list, stop background workers, each in its own git worktree); `false` removes it. Never offered inside a worker |
| `oxa.workers.max` | number | `4` | user settings only (a project file cannot change it). Most live workers one session may run; a `start` past this limit is refused |
| `oxa.workers.models` | object | `{}` | user settings only (a project file cannot change it). Which model a `worker start` without `model` gets: `{"default": [ids], "kinds": {"<kind>": [ids]}, "escalateAfter": 2, "escalateTo": [ids]}`, every field optional. The `kind` the caller passes picks the row (else `default`); within a list the entry this session used least is taken, so two ids alternate. After `escalateAfter` failed workers on one job (same `job` key), the next one gets the first `escalateTo` id that has not failed yet. Kind names are lowercase letters, digits and dashes (at most 32). No ids are built in: unset means the session model. A model id the live catalog does not know only warns. Example: `{"kinds": {"review": ["provider/fast-a", "provider/fast-b"]}, "escalateTo": ["provider/strong"]}` |
| `oxa.workers.repository` | string | unset | user settings only. Canonical absolute path of the explicitly trusted primary Git checkout used by `worker start` in a trusted live Lead session outside Git |
| `oxa.workers.resumeOnStart` | bool | `true` | user settings only (a project file cannot change it, either way). When a session is opened again, relaunch each of its workers that did not finish and whose process is gone, with `--continue=<id>` and no goal; one notice per worker. `false` leaves them as they are |
| `oxa.workers.sendAckSeconds` | number (seconds) | `10` | user settings only (a project file cannot change it). How long the `worker` tool's `send` waits for the worker to take the message before it answers `receipt <id>: not_delivered`; `0` does not wait and answers `queued`. `send` with `retry` waits again for the same message and with `withdraw` removes it |
| `oxa.workers.stopGraceSeconds` | number (seconds) | `10` | user settings only (a project file cannot change it). How long a stopped worker gets after SIGINT before its whole process group gets SIGTERM; SIGKILL follows 2 seconds later if the group is still alive |
| `oxa.workers.sharedCache` | bool | `true` | user settings only (a project file cannot change it). A worker derives one Cargo target directory per main repository, `<cache dir>/<hash>/cargo-target`, shared by every worktree of it: it is the only extra writable path of the worker sandbox and `CARGO_TARGET_DIR` points at it, unless you set `CARGO_TARGET_DIR` yourself. `false` turns it off |
| `oxa.workers.cacheDir` | path | unset (`<XDG cache home>/oxa/workers`) | user settings only. The root of the worker build caches; `~/` is expanded |
| `oxa.workers.cacheWarnGb` | number (GB) | unset | user settings only. When set and a repository's worker build cache is larger, the Workers view warns and names the directory to delete. Never a limit |
<!-- /generated:other -->

From a trusted live Lead directory outside Git, set `oxa.workers.repository` in your user settings (for example, `{"oxa":{"workers":{"repository":"/path/to/checkout"}}}`). Use the primary checkout's absolute path without symlinks, and [explicitly trust the checkout](security.md) first. Inside Git, `worker start` still uses the current repository. This setting is for new starts; it does not enable automatic resume from outside Git.

Pass `model` to `worker start` to select the worker's model. Omit it to inherit the parent session's current model. Reopening the same parent session resumes unfinished workers with the model recorded at their launch, even if the parent has switched models. The child resolves the model using the normal CLI routing and the owner's transport and credential settings.

A worker is never wider than its parent session. It runs with the parent's permission ceiling, `--exclude-tools` and `--no-<tool>` exclusions, `--tools`, `--no-tools`, `--no-builtin-tools`, `--no-mcp`, `--no-web-search`, `--no-extensions`, `--no-skills`, `--no-prompt-templates`, `--no-context-files`, `--no-approve`, `--offline` and session directory. `git` and `env` for the launch come from a trusted `PATH` directory outside the project, never from a project shim. The worktree is created without a checkout and its files are written inside the sandbox (`oxa work --supervise`), so a repository's smudge filters never run on the host; a launch is refused when the `oxa` executable changed after the session started. Token budgets warn and record; they never block: with `--max-tokens N` each worker gets the parent's whole allowance `N` (the start notice says so), so a session with `max` live workers can use up to (1 + `max`) times `N` in total.

## Example

A user file that sets a default model, a thinking level, a tool list and a subagent switch:

```json
{
  "defaultModel": "your-model-id",
  "defaultThinkingLevel": "low",
  "defaultTools": ["read", "grep", "find", "ls", "edit", "write"],
  "compaction": { "reserveTokens": 16384, "keepRecentTokens": 4000 },
  "oxa": { "mode": "workspace", "subagent": { "enabled": false } }
}
```

A project file that sets a retry count and an exec hook for one repository:

```json
{
  "retry": { "maxRetries": 5 },
  "hooks": { "tool_call": "/usr/local/bin/log-tool-call" }
}
```

The project's `hooks` entry runs a program. oxa loads it only after you approve the project's content, and asks again when that content changes.
