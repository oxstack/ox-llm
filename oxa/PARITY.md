# oxa behavior regression suite

This suite checks oxa's supported tools, context handling, sessions and interfaces.
It is derived from pi; historical row IDs and source comments retain that provenance.

## Format

Each row has a stable ID, behavior, verdict, invariant note and executable regression test.
P records inherited behavior; A records behavior adapted for oxa. Both are oxa contracts.
IDs are never reused. Features outside oxa's scope are not regression requirements.

## Ship gate

Every supported row must have a passing behavior test under oxa's single set of defaults.
Run the whole oxa workspace, including the regression suite and required sandbox checks.
No alternate profile or pi comparison pass is required. MCP, extensions and all hooks,
RPC, JSON mode, trust, permissions and offline behavior remain supported contracts.

CLI harnesses apply `oxa_parity::note_off_env()` after `TestEnv::envs` to keep golden
user messages stable. Environment-note behavior has its own cache/history regressions.

## CLI: Command line

pi page(s): cli.md. Tests: `oxa-parity/tests/cli.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| CLI-001 | `message` arguments provide the initial prompt(s) | P |  | `cli.rs::cli_001` |
| CLI-002 | `@path` includes a text file in the first prompt | P |  | `cli.rs::cli_002` |
| CLI-003 | `@path` with an image attaches the image | A | text-only engine: stored + shown, wire gets the fixed placeholder line | `cli.rs::cli_003` |
| CLI-004 | Piped stdin is prepended to the first prompt | P |  | `cli.rs::cli_004` |
| CLI-005 | `--` stops option parsing | P |  | `cli.rs::cli_005` |
| CLI-006 | `-p`, `--print`: run prompts, print final assistant text, exit | P |  | `cli.rs::cli_006` |
| CLI-007 | `--mode text`: text output; TUI when stdin and stdout are terminals | P |  | `cli.rs::cli_007` |
| CLI-008 | `--mode json`: JSONL events to stdout, then exit | P |  | `cli.rs::cli_008` |
| CLI-009 | `--mode rpc`: JSONL commands on stdin until shutdown | P |  | `cli.rs::cli_009` |
| CLI-010 | `--export <input> [output]`: export to HTML and exit; derived output name | P |  | `cli.rs::cli_010` |
| CLI-012 | `--model <pattern>`: exact id, fuzzy match, optional `:<thinking>` suffix | A | resolved over the engine model list (`/v1/models`); `provider/id` accepted only for a configured engine endpoint; a switch is a new epoch (W3) | `cli.rs::cli_012` |
| CLI-013 | `--api-key <key>` | A | optional bearer key for an engine endpoint; no provider selection | `cli.rs::cli_013` |
| CLI-014 | `--thinking <level>`: off..max, overrides the model suffix, clamped | A | W6: off = `enable_thinking:false`, any other level = true | `cli.rs::cli_014` |
| CLI-015 | `--models <patterns>`: comma-separated scope (exact, fuzzy, glob, `:<thinking>`) | A | over the engine model list | `cli.rs::cli_015` |
| CLI-016 | `--list-models [search]`: list and exit | A | engine models only | `cli.rs::cli_016` |
| CLI-017 | `-c`, `--continue`: most recent session of the cwd | P | W3: stored prefix bytes are replayed | `cli.rs::cli_017` |
| CLI-018 | `-r`, `--resume`: session selector; Tab includes other directories | P |  | `cli.rs::cli_018` |
| CLI-019 | `--session <path\|id>`: open by path or full/partial id; offer fork when it belongs to another directory | P |  | `cli.rs::cli_019` |
| CLI-020 | `--session-id <id>`: open or create; allowed characters; must start/end alphanumeric | P |  | `cli.rs::cli_020` |
| CLI-021 | `--fork <path\|id>`: fork into a new session in the cwd | P | a fork is a rebase (W1); engine restores from its nearest checkpoint | `cli.rs::cli_021` |
| CLI-022 | `--session-dir <dir>`: overrides env and `sessionDir` | P | env name `OXA_SESSION_DIR` | `cli.rs::cli_022` |
| CLI-023 | `--no-session`: in-memory session | P |  | `cli.rs::cli_023` |
| CLI-024 | `-n`, `--name <name>`: session display name | P |  | `cli.rs::cli_024` |
| CLI-025 | `--fork` conflicts with `--session`, `--continue`, `--resume`, `--no-session` | P |  | `cli.rs::cli_025` |
| CLI-026 | `--session-id` conflicts with `--session`, `--continue`, `--resume`; combines with `--fork` | P |  | `cli.rs::cli_026` |
| CLI-027 | `-t`, `--tools <list>`: allowlist with `*` patterns; `+name`/`-name`-only list edits the default selection | P | resolved once per prefix epoch (W3) | `cli.rs::cli_027` |
| CLI-028 | `--tools` keeps MCP tools unless an entry starts with `mcp__`; unmatched MCP tools are never declared directly | A | direct MCP tools are frozen at epoch start | `cli.rs::cli_028` |
| CLI-029 | `-xt`, `--exclude-tools <list>`: applied last, MCP included | P |  | `cli.rs::cli_029` |
| CLI-030 | `-nbt`, `--no-builtin-tools` | P |  | `cli.rs::cli_030` |
| CLI-031 | `-nt`, `--no-tools` | P |  | `cli.rs::cli_031` |
| CLI-032 | Default tools are `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`, `ask` | A | lead sessions also offer `subagent` where the model allows it (not Flash or below 32K; `oxa.subagent.enabled` wins); `defaultTools` replaces the selection | `cli.rs::cli_032` |
| CLI-034 | Built-in extensions `codemode` and `tool_search` are off by default; MCP turns them on when needed | A | `tool_search` returns matches as text; calls go through the frozen `call_tool` dispatcher (W3) | `cli.rs::cli_034` |
| CLI-035 | `-e`, `--extension <path>`: file, directory or `builtin:<name>`; repeatable | A | JavaScript in QuickJS; a `.ts` file is always rejected, with a hint to write JavaScript | `cli.rs::cli_035` |
| CLI-036 | `-ne`, `--no-extensions`: explicit `-e` still loads | P |  | `cli.rs::cli_036` |
| CLI-037 | `--no-mcp` | P |  | `cli.rs::cli_037` |
| CLI-038 | `--skill <path>` (repeatable) and `-ns`, `--no-skills` | P |  | `cli.rs::cli_038` |
| CLI-039 | `--prompt-template <path>` (repeatable) and `-np`, `--no-prompt-templates` | P |  | `cli.rs::cli_039` |
| CLI-040 | `--theme <path>` (repeatable), `--use-theme <name[/name]>`, `--no-themes` | P |  | `cli.rs::cli_040` |
| CLI-041 | `-nc`, `--no-context-files` | P |  | `cli.rs::cli_041` |
| CLI-042 | `--system-prompt <text\|path>` | P | becomes the preamble section of the frozen prefix | `cli.rs::cli_042` |
| CLI-043 | `--append-system-prompt <text\|path>`, repeatable | P | the `append` section of the frozen prefix | `cli.rs::cli_043` |
| CLI-044 | `--tui-mode fullscreen\|regular` | P |  | `cli.rs::cli_044` |
| CLI-045 | `--verbose` overrides `quietStartup` | P |  | `cli.rs::cli_045` |
| CLI-046 | `-a`, `--approve` / `-na`, `--no-approve`: project trust for this process | P |  | `cli.rs::cli_046` |
| CLI-047 | `--offline` | A | hard switch: every connection of the harness to a host that is not loopback is refused with one error, the bash sandbox has no network, oxa's own git and `gh` calls refuse, `web_search` is not declared; the local engine keeps working. Keeps pi's meaning (no automatic network activity; oxa has none) | `cli.rs::cli_047` |
| CLI-048 | `-h`, `--help` lists extension flags; `-v`, `--version` | P |  | `cli.rs::cli_048` |
| CLI-049 | Extensions may register long options; unknown short options are rejected | P |  | `cli.rs::cli_049` |
| CLI-050 | Mode choice by tty rules (TUI only when stdin and stdout are terminals) | P |  | `cli.rs::cli_050` |
| CLI-051 | `pi install <source>` / `pi remove <source>` (`uninstall` alias) / `pi list` | A | `oxa install\|remove\|list`; git and local sources; `npm:` is refused (O-175) | `cli.rs::cli_051` |
| CLI-052 | `pi config`: choose which package resources load | P | `oxa config` | `cli.rs::cli_052` |
| CLI-055 | `pi update --extensions` / `pi update <source>` / `--extension <source>` | A | `oxa update` for git + local packages | `cli.rs::cli_055` |
| CLI-059 | `pi mcp add <server> -- <command> [args]` with `--env`, `--cwd` | P | `oxa mcp add` | `cli.rs::cli_059` |
| CLI-060 | `pi mcp add <server> --url <url>` with `--header`, `--bearer-token-env-var` | P |  | `cli.rs::cli_060` |
| CLI-061 | `pi mcp remove <server>` keeps stored credentials and unrelated content | P |  | `cli.rs::cli_061` |
| CLI-062 | `pi mcp list [--json]`: connects, prints state, exit 1 on invalid/failed | P |  | `cli.rs::cli_062` |
| CLI-063 | `pi mcp login <server> [-l] [--timeout]` / `pi mcp logout <server> [-l]` | P |  | `cli.rs::cli_063` |
| CLI-064 | No engine reachable: one-line error + hint, exit code 2 | A | oxa only: `engine_down` | `cli.rs::cli_064` |

## MOD: Models, providers, thinking

pi page(s): models.md, models-configuration.md, providers.md, llama-cpp.md. Tests: `oxa-parity/tests/cli.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| MOD-001 | Model selection: `/model`, Ctrl+L selector, Ctrl+P / Shift+Ctrl+P cycling over the scoped list | A | over the engine model list; a switch is a new epoch (W3) and prints its re-prefill cost | `cli.rs::mod_001` |
| MOD-002 | `models.json` custom models | A | names engine endpoints (laptop, LAN) with an optional bearer key; other provider types are a clear error | `cli.rs::mod_002` |
| MOD-003 | Thinking levels off..max with per-model clamping | A | W6: boolean on the wire; `thinkingBudgets` is not a setting (generic unknown-key warning) | `cli.rs::mod_003` |
| MOD-004 | Model without preserved thinking: thinking defaults to off, status line says why | A | W6 (oxa only) | `cli.rs::mod_004` |
| MOD-010 | llama.cpp router management (`/llama`) | A | `/engine`: status and model load over `/_local/status` | `cli.rs::mod_010` |

## SET: Settings

pi page(s): settings.md. Tests: `oxa-parity/tests/keys_settings.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| SET-001 | User + project `settings.json` merge; project overrides user; nested objects merge | P | read once per epoch (W3) | `keys_settings.rs::set_001` |
| SET-002 | Unknown or invalid keys produce a diagnostic, never a failure | P |  | `keys_settings.rs::set_002` |
| SET-004 | `defaultModel` | A | an engine model id | `keys_settings.rs::set_004` |
| SET-005 | `defaultThinkingLevel` (default `medium`) | A | W6 boolean | `keys_settings.rs::set_005` |
| SET-006 | `modelThinkingLevels` | A | keyed by engine model id | `keys_settings.rs::set_006` |
| SET-008 | `enabledModels` | A | patterns over engine models | `keys_settings.rs::set_008` |
| SET-009 | `hideThinkingBlock` | P |  | `keys_settings.rs::set_009` |
| SET-010 | `showCacheMissNotices` | A | uses exact `cached_tokens`; cause from `miss_cause` | `keys_settings.rs::set_010` |
| SET-011 | `cacheWarming` `off\|streaming\|idle` | A | maps to idle prefill (`max_tokens:0` with the snapshot and park requests); `streaming` = `idle` | `keys_settings.rs::set_011` |
| SET-012 | `steeringMode` `all\|one-at-a-time` | P |  | `keys_settings.rs::set_012` |
| SET-013 | `followUpMode` `all\|one-at-a-time` | P |  | `keys_settings.rs::set_013` |
| SET-014 | `externalEditor` (then `$VISUAL`, `$EDITOR`) | P |  | `keys_settings.rs::set_014` |
| SET-015 | `doubleEscapeAction` `tree\|fork\|none` | P |  | `keys_settings.rs::set_015` |
| SET-016 | `treeFilterMode` (five values) | P |  | `keys_settings.rs::set_016` |
| SET-017 | `defaultProjectTrust` `ask\|always\|never`; agent-directory settings only | A | shipped default `always` (owner decision 2026-11-21); a fresh install trusts the project, `ask` and `never` are owner opt-ins, a project file value is ignored | `keys_settings.rs::set_017` |
| SET-018 | `defaultTools`: plain names replace, `+name` adds, `-name` removes | A | default set is the 7 built-ins + `subagent` where O-146 offers it; `+name`/`-name` edit that set (incl. `subagent`); plain names replace it; resolved per epoch | `keys_settings.rs::set_018` |
| SET-019 | `codemode.mode` `on\|only` | P |  | `keys_settings.rs::set_019` |
| SET-021 | `sessionDir` | A | the session store keeps one sub-directory per working directory below every sessions directory (pi stores flat in a custom `sessionDir`) | `keys_settings.rs::set_021` |
| SET-023 | `compaction.enabled` | P |  | `keys_settings.rs::set_023` |
| SET-024 | `compaction.reserveTokens` (16384) | A | W5: hard limit = `n_ctx - max_tokens - reserveTokens` | `keys_settings.rs::set_024` |
| SET-025 | `compaction.keepRecentTokens` | A | default 4000 instead of 20000 [P]: the kept tail is re-prefilled | `keys_settings.rs::set_025` |
| SET-026 | `compaction.modelOverrides` | A | keyed by engine model id; parsed, applied only when it names the session model (CMP-015) | `keys_settings.rs::set_026` |
| SET-027 | `oxa.compaction.softTokens` | A | oxa only: soft limit that triggers compaction after the reply (W5) | `keys_settings.rs::set_027` |
| SET-028 | `oxa.toolOutputCap` | A | oxa only: W4 cap, default 16 KiB | `keys_settings.rs::set_028` |
| SET-029 | `branchSummary.reserveTokens`, `branchSummary.skipPrompt` | P |  | `keys_settings.rs::set_029` |
| SET-030 | `theme` (default `system`) | P |  | `keys_settings.rs::set_030` |
| SET-031 | `quietStartup` `true\|false\|"header"` | P |  | `keys_settings.rs::set_031` |
| SET-032 | `tuiMode` `regular\|fullscreen` | P |  | `keys_settings.rs::set_032` |
| SET-033 | `fullscreenExitOutput` `transcript\|resume-hint` | P |  | `keys_settings.rs::set_033` |
| SET-034 | `fullscreenScrollbar` `auto\|always\|hidden` | P |  | `keys_settings.rs::set_034` |
| SET-035 | `fullscreenCopyOnSelect` | P |  | `keys_settings.rs::set_035` |
| SET-036 | `fullscreenWheelScrollLines` `auto` or 1..100 | P |  | `keys_settings.rs::set_036` |
| SET-037 | `editorPaddingX` 0..3 | P |  | `keys_settings.rs::set_037` |
| SET-038 | `outputPad` 0\|1 | P |  | `keys_settings.rs::set_038` |
| SET-039 | `autocompleteMaxVisible` 3..20 | P |  | `keys_settings.rs::set_039` |
| SET-041 | `terminal.showImages`, `terminal.imageWidthCells` | P | inline display only; nothing reaches the wire | `keys_settings.rs::set_041` |
| SET-042 | `terminal.clearOnShrink` | P |  | `keys_settings.rs::set_042` |
| SET-046 | `images.blockImages` | A | always effectively true on the wire; the setting only hides the inline display notice | `keys_settings.rs::set_046` |
| SET-047 | `markdown.codeBlockIndent` | P |  | `keys_settings.rs::set_047` |
| SET-048 | `markdown.mermaid` `off\|final\|streaming` | A | Mermaid shown as a highlighted code block; setting accepted | `keys_settings.rs::set_048` |
| SET-050 | `httpProxy` | A | applies to MCP HTTP only; the engine connection is direct | `keys_settings.rs::set_050` |
| SET-051 | `httpIdleTimeoutMs` | A | silence limit of the engine stream; SSE comments count as liveness; no wall-clock limit | `keys_settings.rs::set_051` |
| SET-053 | `retry.enabled`, `retry.maxRetries`, `retry.baseDelayMs`, `retry.maxAgentDelayMs` | A | replay a cut stream (also after data) under the same `Idempotency-Key` while retries allow (#456) | `keys_settings.rs::set_053` |
| SET-054 | `retry.provider.timeoutMs`, `retry.provider.maxRetries`, `retry.provider.maxRetryDelayMs` | A | same rule; one HTTP layer | `keys_settings.rs::set_054` |
| SET-055 | `shellPath` | P |  | `keys_settings.rs::set_055` |
| SET-056 | `shellCommandPrefix` | P | part of the sandboxed command | `keys_settings.rs::set_056` |
| SET-059 | `packages`, `extensions`, `skills`, `prompts`, `themes` resource lists | P |  | `keys_settings.rs::set_059` |
| SET-060 | `enableSkillCommands` | P |  | `keys_settings.rs::set_060` |
| SET-061 | `hooks: {event: command}` | A | oxa only: exec hooks (JSON on stdin, reply on stdout, run in bwrap) | `keys_settings.rs::set_061` |

## KEY: Keybindings

pi page(s): keybindings.md. Tests: `oxa-parity/tests/keys_settings.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| KEY-001 | `keybindings.json` in the agent directory overrides defaults; a list or a single key per id; conflicts reported | P | conflicts and unknown ids are shown as startup warnings (pi computes conflicts without showing them) | `keys_settings.rs::key_001` |
| KEY-002 | Key syntax: `ctrl+shift+alt+super` modifiers + key; kitty keyboard protocol and legacy sequences | P |  | `keys_settings.rs::key_002` |
| KEY-003 | Cursor movement: `tui.editor.cursorUp` (up), `cursorDown` (down), `historyPrevious` (none), `historyNext` (none), `cursorLeft` (left, ctrl+b), `cursorRight` (right, ctrl+f), `cursorWordLeft` (alt+left, ctrl+left, alt+b), `cursorWordRight` (alt+right, ctrl+right, alt+f), `cursorLineStart` (home, ctrl+a), `cursorLineEnd` (end, ctrl+e), `jumpForward` (ctrl+]), `jumpBackward` (ctrl+alt+]), `pageUp` (pageUp, ctrl+pageUp), `pageDown` (pageDown, ctrl+pageDown) | P |  | `keys_settings.rs::key_003` |
| KEY-004 | Text editing: `tui.editor.deleteCharBackward` (backspace), `deleteCharForward` (delete, ctrl+d), `deleteWordBackward` (ctrl+w, alt+backspace), `deleteWordForward` (alt+d, alt+delete), `deleteToLineStart` (ctrl+u), `deleteToLineEnd` (ctrl+k), `yank` (ctrl+y), `yankPop` (alt+y), `undo` (ctrl+-) | P | Linux defaults only (no Windows/WSL variants) | `keys_settings.rs::key_004` |
| KEY-005 | Input: `tui.input.newLine` (shift+enter, ctrl+j), `tui.input.submit` (enter), `tui.input.tab` (tab), `tui.input.copy` (ctrl+c) | P |  | `keys_settings.rs::key_005` |
| KEY-006 | Select lists: `tui.select.up` (up), `down` (down), `pageUp`, `pageDown`, `confirm` (enter), `cancel` (escape, ctrl+c) | P |  | `keys_settings.rs::key_006` |
| KEY-007 | Fullscreen scroll: `tui.altScreen.pageUp`, `pageDown`, `halfPageUp` (none), `halfPageDown` (none), `lineUp` (none), `lineDown` (none), `top` (ctrl+home), `bottom` (ctrl+end) | P |  | `keys_settings.rs::key_007` |
| KEY-008 | Fullscreen prompt jump: `tui.altScreen.previousPrompt` (ctrl+shift+up), `nextPrompt` (ctrl+shift+down) | P | Linux defaults only | `keys_settings.rs::key_008` |
| KEY-009 | Fullscreen search: `tui.altScreen.search` (ctrl+shift+f), `searchNext` (enter, ctrl+g), `searchPrevious` (shift+enter, ctrl+shift+g), `searchClose` (escape) | A | Linux defaults only; oxa adds `alt+s` to `search` (a terminal without the kitty keyboard protocol cannot send ctrl+shift+f), and the footer shows only keys the terminal can send | `keys_settings.rs::key_009` |
| KEY-010 | Application: `app.interrupt` (escape), `app.clear` (ctrl+c: clear, then exit), `app.exit` (ctrl+d, empty editor), `app.suspend` (ctrl+z), `app.editor.external` (ctrl+g) | P |  | `keys_settings.rs::key_010` |
| KEY-011 | `app.clipboard.pasteImage` (ctrl+v) | A | pastes files and images into the session; an image is shown, the wire gets the placeholder | `keys_settings.rs::key_011` |
| KEY-012 | Sessions: `app.session.new`, `tree`, `fork`, `resume` (all none), `togglePath` (ctrl+p), `toggleSort` (ctrl+s), `toggleNamedFilter` (ctrl+n), `rename` (ctrl+r), `delete` (ctrl+d), `deleteNoninvasive` (ctrl+backspace) | P |  | `keys_settings.rs::key_012` |
| KEY-013 | Models: `app.model.select` (ctrl+l), `cycleForward` (ctrl+p), `cycleBackward` (shift+ctrl+p), `app.models.save` (ctrl+s) | A | over engine models; a switch is a new epoch | `keys_settings.rs::key_013` |
| KEY-014 | Thinking: `app.thinking.cycle` (shift+tab), `app.thinking.save` (ctrl+s), `app.thinking.toggle` (ctrl+t) | A | W6 boolean; shift+tab stays thinking | `keys_settings.rs::key_014` |
| KEY-015 | `app.mode.cycle` (alt+m) | A | oxa only: standard -> auto -> yolo [P] | `keys_settings.rs::key_015` |
| KEY-016 | Display and queue: `app.tools.expand` (ctrl+o), `app.message.copy` (ctrl+x), `app.message.followUp` (alt+enter), `app.message.dequeue` (alt+up) | P |  | `keys_settings.rs::key_016` |
| KEY-017 | Tree: `app.tree.foldOrUp` (ctrl+left, alt+left), `unfoldOrDown` (ctrl+right, alt+right), `editLabel` (shift+l), `toggleLabelTimestamp` (shift+t) | P |  | `keys_settings.rs::key_017` |
| KEY-018 | Tree filters: `app.tree.filter.default` (ctrl+d), `noTools` (ctrl+t), `userOnly` (ctrl+u), `labeledOnly` (ctrl+l), `all` (ctrl+a), `cycleForward` (ctrl+o), `cycleBackward` (shift+ctrl+o) | P |  | `keys_settings.rs::key_018` |
| KEY-019 | Scoped models selector: `app.models.enableAll` (ctrl+a), `clearAll` (ctrl+x), `reorderUp` (alt+up), `reorderDown` (alt+down) | P |  | `keys_settings.rs::key_019` |
| KEY-020 | `app.models.toggleProvider` (ctrl+p) | A | toggles all models of one engine endpoint | `keys_settings.rs::key_020` |

## ENV: Environment variables

pi page(s): environment-variables.md. Tests: `oxa-parity/tests/cli.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| ENV-001 | Process marker `AI_AGENT=pi` for child processes | P | `AI_AGENT=oxa` | `cli.rs::env_001` |
| ENV-002 | Shell tool env `PI_SESSION_ID`, `PI_SESSION_FILE` (unset when ephemeral) | P | `OXA_SESSION_ID`, `OXA_SESSION_FILE` | `cli.rs::env_002` |
| ENV-003 | Shell tool env `PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL` | A | `OXA_MODEL`, `OXA_REASONING_LEVEL`; `OXA_PROVIDER` is always `oxe`. Set per run, never on the wire (W3) | `cli.rs::env_003` |
| ENV-004 | `PI_CODING_AGENT_DIR` overrides the config directory | P | `OXA_AGENT_DIR`, default `~/.oxa/agent` | `cli.rs::env_004` |
| ENV-005 | `PI_CODING_AGENT_SESSION_DIR`; overridden by `--session-dir` | P | `OXA_SESSION_DIR` | `cli.rs::env_005` |
| ENV-006 | `PI_PACKAGE_DIR` | P | `OXA_PACKAGE_DIR` | `cli.rs::env_006` |
| ENV-007 | `PI_OFFLINE` | A | accepted as `OXA_OFFLINE`; the same hard switch as `--offline` (truthy: 1, true, yes) | `cli.rs::env_007` |
| ENV-009 | `PI_CODEMODE_MAX_WORKERS` (default 2) | P | `OXA_CODEMODE_MAX_WORKERS` | `cli.rs::env_009` |
| ENV-012 | `PI_CLEAR_ON_SHRINK` | P | `OXA_CLEAR_ON_SHRINK`; `PI_HARDWARE_CURSOR` is not in oxa (O-175) | `cli.rs::env_012` |
| ENV-013 | `PI_CLIPBOARD_OSC52` | P | `OXA_CLIPBOARD_OSC52`; the hyperlink, program-status, image-protocol and true-colour overrides are not in oxa (O-175) | `cli.rs::env_013` |
| ENV-015 | `VISUAL`, `EDITOR` external editor fallback | P |  | `cli.rs::env_015` |
| ENV-016 | `HTTP_PROXY`, `HTTPS_PROXY` | A | MCP HTTP only | `cli.rs::env_016` |

## TOOLS: Built-in tools

pi page(s): cli.md#tools, usage.md, pi source tools/*. Tests: `oxa-parity/tests/tools.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| TOOLS-001 | `read`: schema `path`, `offset` (1-indexed), `limit`; paging notices; offset beyond end error | P | pi's notice texts | `tools.rs::tools_001` |
| TOOLS-002 | `read`: output limited to 2000 lines or 50 KB | A | W4: 2000 lines or the 16 KiB cap | `tools.rs::tools_002` |
| TOOLS-003 | `read` on an image returns the image | A | returns the fixed placeholder line | `tools.rs::tools_003` |
| TOOLS-004 | `read` on a binary file | A | refused with a fixed error (oxa only) | `tools.rs::tools_004` |
| TOOLS-005 | `write`: creates parent directories, overwrites, `Successfully wrote to <path>` | P | atomic write | `tools.rs::tools_005` |
| TOOLS-006 | `edit`: `path` + `edits[] {oldText, newText}` matched against the original file; success text | P |  | `tools.rs::tools_006` |
| TOOLS-007 | `edit`: not found / duplicate / empty / overlap / no-change error texts | P | pi's exact texts | `tools.rs::tools_007` |
| TOOLS-008 | `edit`: BOM, CRLF and mixed line endings preserved; fuzzy match fallback | P | edit-diff ported | `tools.rs::tools_008` |
| TOOLS-009 | `edit`: legacy single `oldText`/`newText` input accepted | P |  | `tools.rs::tools_009` |
| TOOLS-010 | `bash`: `command`, `timeout` seconds; combined stdout+stderr; exit code status line | P |  | `tools.rs::tools_010` |
| TOOLS-011 | `bash`: output truncated to the last 2000 lines or 50 KB, full output in a temp file | A | W4: head + tail + one fixed line naming a spill file (sha256 name) | `tools.rs::tools_011` |
| TOOLS-012 | `bash`: timeout kills the process tree; background-writer case | A | W4: pi's text without the `after S seconds` clause (no timings in model-facing text) | `tools.rs::tools_012` |
| TOOLS-013 | `bash` runs unsandboxed with the user's rights | A | bwrap sandbox; no bwrap = bash refuses; mode decides writable/net | `tools.rs::tools_013` |
| TOOLS-014 | `grep`: schema `pattern`, `path`, `glob`, `ignoreCase`, `literal`, `context`, `limit` (100); .gitignore; 500-char lines; notices | A | in-process (`ignore` + `regex`), sorted, deterministic; no rg download | `tools.rs::tools_014` |
| TOOLS-015 | `find`: `pattern`, `path`, `limit` (1000); glob relative to path; .gitignore | A | in-process, sorted; no fd download | `tools.rs::tools_015` |
| TOOLS-016 | `ls`: `path`, `limit` (500); sorted, `/` suffix, dotfiles; empty-directory text | P |  | `tools.rs::tools_016` |
| TOOLS-017 | grep/find/ls: `limit < 1` is treated as 1 | P |  | `tools.rs::tools_017` |
| TOOLS-018 | File-mutation queue serialises writes per path | P |  | `tools.rs::tools_018` |
| TOOLS-019 | Tool selection: `defaultTools`, `--tools`, `-xt`, `-nbt`, `-nt`, globs; exclusion wins | P | W3: resolved once per epoch; the default set includes `subagent` where O-146 offers it (selected like a built-in: allowlists must name it, `-xt`/`-nbt`/`-nt` remove it) | `tools.rs::tools_019` |
| TOOLS-020 | Read-class calls of one batch run in parallel; results are pushed in call order | P |  | `tools.rs::tools_020` |
| TOOLS-021 | Unknown tool / invalid JSON arguments yield a fixed error result | P |  | `tools.rs::tools_021` |
| TOOLS-022 | Same input + file state = same bytes; no ANSI, no timings (two runs identical) | A | W4 (oxa only) | `tools.rs::tools_022` |
| TOOLS-023 | MCP, extension and codemode results pass the same `shape` | A | W4 (oxa only) | `tools.rs::tools_023` |
| TOOLS-024 | (oxa only) `web_search` (O-169): a Brave Search tool, declared only with a key and never offline; pi has no built-in search | A | the tool list is frozen per prefix epoch; output is deterministic and capped, with source URLs | `tools.rs::tools_024` |
| TOOLS-025 | (oxa only) `deliverable` (O-169): records the output file a request names and checks it from `read`'s guarded view (existence and size only); the run ends with a typed record per declared file, and a missing one is named in the wrap-up message. Evidence only, never a gate; pi has no built-in | A | the record is per run and never a refusal; a refused (private-path) call records nothing | `tools.rs::tools_025` |

## USE: Using the terminal UI

pi page(s): usage.md. Tests: `oxa-parity/tests/tui.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| USE-001 | Enter submits; Shift+Enter inserts a newline; multi-line paste becomes a paste marker | P |  | `tui.rs::use_001` |
| USE-002 | `@` fuzzy file reference and Tab path completion | P |  | `tui.rs::use_002` |
| USE-003 | Enter while working queues a steering message; Alt+Enter queues a follow-up; Alt+Up restores the queue | P |  | `tui.rs::use_003` |
| USE-004 | Escape aborts the run; queued text returns to the editor | P | W2: an aborted reply is shown, never sent | `tui.rs::use_004` |
| USE-005 | `!cmd` runs a command and adds its output to the next prompt | P | enters as an append (W1); runs with the user's own rights [P] | `tui.rs::use_005` |
| USE-006 | `!!cmd` runs a command without adding output to context | P |  | `tui.rs::use_006` |
| USE-007 | Ctrl+O expands tool output; Ctrl+T toggles thinking blocks | P |  | `tui.rs::use_007` |
| USE-008 | Ctrl+G opens the external editor | P |  | `tui.rs::use_008` |
| USE-009 | Ctrl+Z suspends and resumes with the screen restored | P |  | `tui.rs::use_009` |
| USE-010 | Drag-and-drop / paste of image and file paths | A | image: stored + shown, placeholder on the wire | `tui.rs::use_010` |
| USE-011 | `/debug` writes diagnostic information to a file | P |  | `tui.rs::use_011` |
| USE-012 | Footer: context %, model, thinking, cwd | A | adds cache hit, prefill progress, tok/s, mode | `tui.rs::use_012` |

## SLASH: Slash commands

pi page(s): slash-commands.md. Tests: `oxa-parity/tests/slash.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| SLASH-001 | `/settings` | P |  | `slash.rs::slash_001` |
| SLASH-002 | `/model [provider/model]` | A | engine models; new epoch | `slash.rs::slash_002` |
| SLASH-003 | `/thinking [level]` | A | W6 boolean | `slash.rs::slash_003` |
| SLASH-004 | `/scoped-models` | A | engine models | `slash.rs::slash_004` |
| SLASH-007 | `/llama` | A | `/engine`: status, load | `slash.rs::slash_007` |
| SLASH-008 | `/new` | P |  | `slash.rs::slash_008` |
| SLASH-009 | `/resume` | P |  | `slash.rs::slash_009` |
| SLASH-010 | `/name [name]` | P |  | `slash.rs::slash_010` |
| SLASH-011 | `/session` | P | adds cache statistics | `slash.rs::slash_011` |
| SLASH-012 | `/tree` | A | a leaf move is a rebase (W1); optional branch summary | `slash.rs::slash_012` |
| SLASH-013 | `/fork` | P |  | `slash.rs::slash_013` |
| SLASH-014 | `/clone` | P |  | `slash.rs::slash_014` |
| SLASH-015 | `/compact [instructions]` | A | W5 summary request | `slash.rs::slash_015` |
| SLASH-017 | `/copy` | P |  | `slash.rs::slash_017` |
| SLASH-018 | `/export [path]` HTML or JSONL | P |  | `slash.rs::slash_018` |
| SLASH-020 | `/bug [description]` | A | writes a local redacted bundle; no upload | `slash.rs::slash_020` |
| SLASH-021 | `/trust` | P |  | `slash.rs::slash_021` |
| SLASH-022 | `/mcp` | P | exposure change to `direct` takes effect at the next epoch | `slash.rs::slash_022` |
| SLASH-023 | `/reload` | A | unchanged resources = no new epoch; changed = one new `Prefix` entry + cost notice (W3) | `slash.rs::slash_023` |
| SLASH-024 | `/hotkeys` | P | reflects the keymap | `slash.rs::slash_024` |
| SLASH-026 | `/quit` | P |  | `slash.rs::slash_026` |
| SLASH-027 | `/mode [plan\|workspace\|ask]` | A | oxa only | `slash.rs::slash_027` |
| SLASH-028 | `/skill:name [args]` commands from skills | P |  | `slash.rs::slash_028` |
| SLASH-029 | Prompt-template commands `/name args` | P |  | `slash.rs::slash_029` |
| SLASH-030 | Extension commands; name clash order (built-in, extension, template, skill) as pi | P |  | `slash.rs::slash_030` |

## SES: Sessions

pi page(s): sessions.md. Tests: `oxa-parity/tests/sessions.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| SES-001 | Sessions are stored per working directory as `--<path>--/<ts>_<id>.jsonl` | P | 0600 files in 0700 dirs, O_APPEND, O_NOFOLLOW | `sessions.rs::ses_001` |
| SES-002 | `-c` continues the latest; `-r` picker with search, sort, named filter, rename, delete | P |  | `sessions.rs::ses_002` |
| SES-003 | Deleting a session uses `trash` when available | P |  | `sessions.rs::ses_003` |
| SES-004 | Missing cwd on resume: Continue / Fork / Cancel | P |  | `sessions.rs::ses_004` |
| SES-005 | `/new`, `/resume`, `/name`, `/session` | P |  | `sessions.rs::ses_005` |
| SES-006 | `/fork` creates a new session file with a `parentSession` header | P |  | `sessions.rs::ses_006` |
| SES-007 | `/clone` duplicates the session at its position | P |  | `sessions.rs::ses_007` |
| SES-009 | `--no-session` keeps nothing on disk | P |  | `sessions.rs::ses_009` |
| SES-010 | `sessionDir` / env / flag precedence | P |  | `sessions.rs::ses_010` |
| SES-011 | Labels on entries (`/tree` Shift+L) | P |  | `sessions.rs::ses_011` |
| SES-012 | Automatic compaction keeps long sessions usable | A | automatic compaction at the context limit, in idle time when possible | `sessions.rs::ses_012` |
| SES-013 | `/export` and `--export` produce a self-contained HTML file | P | pi's template | `sessions.rs::ses_013` |
| SES-015 | `/bug` prepares a report | A | local redacted bundle | `sessions.rs::ses_015` |
| SES-016 | Torn last line of a session file is dropped on load | A | oxa only | `sessions.rs::ses_016` |

## FMT: Session file format

pi page(s): session-format.md, message-types.md. Tests: `oxa-parity/tests/sessions.rs`, `oxa-parity/tests/format.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| FMT-001 | `SessionHeader` `{type:"session", version:3, id, timestamp, cwd, parentSession?}` | P |  | `format.rs::fmt_001` |
| FMT-002 | Entry base: `type`, `id` (8 hex), `parentId`, `timestamp` (ISO-8601) | P |  | `format.rs::fmt_002` |
| FMT-003 | `SessionMessageEntry` (`type:"message"`) with `AgentMessage` (user, assistant, toolResult) | A | assistant entries also store the exact wire bytes (`args_raw`, untrimmed content, reasoning) for W2 | `format.rs::fmt_003` |
| FMT-004 | Leading system message (`role:"system"`, `sections`, tool declarations) as a `message` entry | A | the frozen prefix of an epoch (W3); replayed byte for byte on resume | `format.rs::fmt_004` |
| FMT-005 | Later system messages with section patches / `toolsRemoved` mid-session | A | no mid-session prompt change: a change is a new epoch entry (W3) | `format.rs::fmt_005` |
| FMT-006 | `ModelChangeEntry` | P |  | `format.rs::fmt_006` |
| FMT-007 | `ThinkingLevelChangeEntry` | P |  | `format.rs::fmt_007` |
| FMT-008 | `UsageEntry` (`kind`, usage) | A | kinds: `chat`, `compaction`, `branch_summary`, `prefill`; a subagent child runs without a session file, so its usage is not recorded in the parent (pi lists `subagent`) | `format.rs::fmt_008` |
| FMT-009 | `CompactionEntry` (`summary`, `firstKeptEntryId`, `tokensBefore`, `details`, `fromHook`, `systemMessage`) | A | head after compaction is `[system, user:<summary>, tail]` (W5); pi's `systemMessage` is not written, the epoch's prefix entry is the checkpoint | `format.rs::fmt_009` |
| FMT-010 | `ContextEditEntry` | A | oxa reads the entry type and applies a stored one when the session is loaded (replacement and omission reach the wire: a rebase, W1); oxa itself never writes one | `format.rs::fmt_010` |
| FMT-011 | `BranchSummaryEntry` (`fromId`, `summary`, `details`, `fromHook`) | P |  | `format.rs::fmt_011` |
| FMT-012 | `CustomEntry` (`customType`, `data`): never in context | P |  | `format.rs::fmt_012` |
| FMT-013 | `CustomMessageEntry` (`customType`, `content`, `display`, `details`): in context | P | an append (W1) | `format.rs::fmt_013` |
| FMT-014 | `LabelEntry` (`targetId`, `label`) | P |  | `format.rs::fmt_014` |
| FMT-015 | `SessionInfoEntry` (`name`) | P |  | `format.rs::fmt_015` |
| FMT-016 | `BashExecutionMessage` (`command`, `output`, `exitCode`, `cancelled`, `truncated`, `excludeFromContext`) | P |  | `format.rs::fmt_016` |
| FMT-017 | Tree structure: `id`/`parentId`; the leaf is the last entry; context building walks leaf to root | P | the wire is a pure fold of the active branch (W1) | `format.rs::fmt_017` |
| FMT-018 | Context building: compaction replaces entries before `firstKeptEntryId` | A | W5 rebase | `format.rs::fmt_018` |
| FMT-020 | `ImageContent` blocks in user messages | A | `Part::Image` stored; never serialised to the wire | `format.rs::fmt_020` |
| FMT-021 | oxa entry types `shown` and `mode_change` | A | oxa only: aborted/failed/cut replies (W2) and mode changes; ignored by pi tooling | `format.rs::fmt_021` |

## TREE: Tree navigation and branching

pi page(s): sessions.md#choose-how-to-branch, compaction.md#branch-summarization. Tests: `oxa-parity/tests/tree.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| TREE-001 | `/tree` navigator: fold/unfold, five filters, labels + timestamps | P |  | `tree.rs::tree_001` |
| TREE-002 | Selecting a user message moves the leaf to its parent and refills the editor | P |  | `tree.rs::tree_002` |
| TREE-003 | Selecting a non-user entry moves the leaf to it | P |  | `tree.rs::tree_003` |
| TREE-004 | Leaf move changes the context sent to the model | A | exactly one rebase (W1); the engine restores from its nearest checkpoint and oxa prefills the rest in idle time | `tree.rs::tree_004` |
| TREE-005 | Prompt on leaving a branch: summarize / don't / custom focus (`branchSummary.skipPrompt`) | P |  | `tree.rs::tree_005` |
| TREE-006 | Branch summary covers old leaf back to the common ancestor, newest-first within budget | A | request = the abandoned branch's wire + one instruction (pure append; foreground, the user waits on it: F12) | `tree.rs::tree_006` |
| TREE-007 | Nested branch summaries carry cumulative file lists | P |  | `tree.rs::tree_007` |
| TREE-008 | `BranchSummaryEntry` is written at the new leaf | P |  | `tree.rs::tree_008` |
| TREE-009 | Model and thinking level are recorded on the new branch | P |  | `tree.rs::tree_009` |
| TREE-010 | Double Escape opens `/tree` or `/fork` (`doubleEscapeAction`) | P |  | `tree.rs::tree_010` |
| TREE-011 | `session_before_tree` can cancel or supply a summary; `session_tree` fires after | P |  | `tree.rs::tree_011` |
| TREE-012 | Moving back to an earlier leaf reuses the engine cache | A | oxa only: stub-contract test (tokens extend) | `tree.rs::tree_012` |

## CMP: Compaction

pi page(s): compaction.md. Tests: `oxa-parity/tests/compaction.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| CMP-001 | Auto compaction triggers when context exceeds `contextWindow - reserveTokens` | A | W5: soft limit crossed at run end -> compact in idle time; mid-run only above the hard limit | `compaction.rs::cmp_001` |
| CMP-002 | `keepRecentTokens` tail kept verbatim; cut-point rules (never between a tool call and its result) | A | default 4000; cut-point rules kept | `compaction.rs::cmp_002` |
| CMP-003 | Split user-message spans: turn-prefix summary + history summary | A | replaced by the W5 mid-run path; a prompt is never folded | `compaction.rs::cmp_003` |
| CMP-004 | Summary generated by a separate request with a summarizer system prompt over serialized text | A | whole wire + one user instruction, same tools, thinking off (pure append); background only for the threshold case, foreground for `/compact`, overflow recovery and the compaction before a model switch (F12) | `compaction.rs::cmp_004` |
| CMP-005 | Summary format sections (Goal, Constraints, Progress, Key Decisions, Next Steps, Critical Context) | P | pi's instruction texts | `compaction.rs::cmp_005` |
| CMP-006 | Repeated compaction updates the previous summary | P | update instruction | `compaction.rs::cmp_006` |
| CMP-007 | Cumulative file tracking: `<read-files>` / `<modified-files>` appended; details `{readFiles, modifiedFiles}` | P |  | `compaction.rs::cmp_007` |
| CMP-008 | `CompactionEntry` shape (`summary`, `firstKeptEntryId`, `tokensBefore`, `details`) | P |  | `compaction.rs::cmp_008` |
| CMP-009 | `/compact [instructions]` adds `Additional focus:` | P |  | `compaction.rs::cmp_009` |
| CMP-010 | Overflow recovery: compact once and retry the prompt | A | a failed reply is not on the wire (W2); one recovery retry | `compaction.rs::cmp_010` |
| CMP-011 | Length-stop recovery ordering | A | A plain length cut gets a bounded continuation ending in a user instruction; local history retains the cut reply and may use advertised H6 prefill. Tool calls inside a cut reply are never run and get the "was not executed ... Re-issue" error result before the loop continues. Generating requests never end in an assistant message | `compaction.rs::cmp_011` |
| CMP-012 | `compaction_start` / `compaction_end` events with `reason` manual\|threshold\|overflow | P |  | `compaction.rs::cmp_012` |
| CMP-013 | `session_before_compact` can cancel or supply a custom summary | P | a custom summary still lands as the W5 head | `compaction.rs::cmp_013` |
| CMP-014 | `session_compact`, `session_compact_failed` | P |  | `compaction.rs::cmp_014` |
| CMP-015 | `compaction.modelOverrides` per model | A | engine model ids. O-144: an override naming another model than the session's is ignored with a warning; the summary runs on the session model (one slot: a swap unloads the model and cold-prefills the whole wire on the other one) | `compaction.rs::cmp_015` |
| CMP-016 | A summary that stops for length is rejected | P |  | `compaction.rs::cmp_016` |
| CMP-017 | Summarization retry events | P |  | `compaction.rs::cmp_017` |
| CMP-018 | After compaction the new head is prefilled in idle time (`max_tokens:0`, `X-Oxe-Snapshot: keep`) | A | oxa only; only when the engine advertises `/props` `oxe_hints.prefill_only`, else skipped | `compaction.rs::cmp_018` |
| CMP-019 | The next prompt after a compaction is delivered live | A | oxa only (hardware gate 6.6) | `compaction.rs::cmp_019` |

## JSON: JSON event stream

pi page(s): json.md. Tests: `oxa-parity/tests/json.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| JSON-001 | LF-only JSONL framing on stdout | P |  | `json.rs::json_001` |
| JSON-002 | Session header record first (JSON mode only) | P |  | `json.rs::json_002` |
| JSON-003 | `agent_start` | P |  | `json.rs::json_003` |
| JSON-004 | `agent_end` (`messages`, `willRetry`) | P |  | `json.rs::json_004` |
| JSON-005 | `agent_settled` (`aborted`) | P |  | `json.rs::json_005` |
| JSON-006 | `turn_start` | P |  | `json.rs::json_006` |
| JSON-007 | `turn_end` (`message`, `toolResults`) | P |  | `json.rs::json_007` |
| JSON-008 | `message_start` (`message`) | P |  | `json.rs::json_008` |
| JSON-009 | `message_update` (`usage`, `assistantMessageEvent`), delta-only | P |  | `json.rs::json_009` |
| JSON-010 | `assistantMessageEvent` types `text_start\|delta\|end`, `thinking_start\|delta\|end`, `toolcall_start\|delta\|end` | P | `toolcall_delta` carries the raw argument bytes | `json.rs::json_010` |
| JSON-011 | `message_end` (`message`) is authoritative | P |  | `json.rs::json_011` |
| JSON-012 | Message `usage` object (`input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens`, `cost`) | A | exact engine counts; `cacheWrite` and `cost` are 0 | `json.rs::json_012` |
| JSON-013 | `tool_execution_start` (`toolCallId`, `toolName`, `args`) | P |  | `json.rs::json_013` |
| JSON-014 | `tool_execution_update` (`partialResult`) | P |  | `json.rs::json_014` |
| JSON-015 | `tool_execution_end` (`result`, `isError`, `durationMs`) | P | `durationMs` is in the event record only, never in a request to the engine (W4) | `json.rs::json_015` |
| JSON-016 | `queue_update` (`steering`, `followUp`) | P |  | `json.rs::json_016` |
| JSON-017 | `entry_appended` (`entry`) | P |  | `json.rs::json_017` |
| JSON-018 | `session_info_changed` (`name`) | P |  | `json.rs::json_018` |
| JSON-019 | `thinking_level_changed` (`level`) | P |  | `json.rs::json_019` |
| JSON-020 | `compaction_start` (`reason`) | P |  | `json.rs::json_020` |
| JSON-021 | `compaction_end` (`result`, `aborted`, `willRetry`, `errorMessage`) | P |  | `json.rs::json_021` |
| JSON-022 | `auto_retry_start` / `auto_retry_end` | P |  | `json.rs::json_022` |
| JSON-023 | `summarization_retry_scheduled` / `_attempt_start` / `_finished` | P |  | `json.rs::json_023` |
| JSON-024 | `bash_execution_update` (RPC only) | P |  | `json.rs::json_024` |
| JSON-025 | `extension_error` (RPC only) | P |  | `json.rs::json_025` |
| JSON-026 | oxa events `prefill`, `prefix_epoch`, `cache_miss`, `mode_changed`, `notice` | A | oxa only; unknown to pi clients, which ignore unknown types | `json.rs::json_026` |
| JSON-027 | Print mode writes only the final assistant text | P |  | `json.rs::json_027` |
| JSON-028 | JSON mode: a permission Ask is a Deny | A | oxa only (no dialog available) | `json.rs::json_028` |

## RPC: RPC mode

pi page(s): rpc.md, rpc-commands.md, rpc-extension-ui.md. Tests: `oxa-parity/tests/rpc.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| RPC-001 | Commands and responses correlate by `id`; responses `{type:"response", command, success, data\|error}` | P |  | `rpc.rs::rpc_001` |
| RPC-002 | Framing: one JSON object per LF-terminated line; no header record | P |  | `rpc.rs::rpc_002` |
| RPC-003 | Run lifecycle: events stream between command response and `agent_settled` | P |  | `rpc.rs::rpc_003` |
| RPC-004 | Error shapes for unknown command / invalid payload / busy | P |  | `rpc.rs::rpc_004` |
| RPC-005 | Shutdown when stdin closes | P |  | `rpc.rs::rpc_005` |
| RPC-006 | `prompt` | P | `streamingBehavior` steer/followUp honoured; images A (placeholder) | `rpc.rs::rpc_006` |
| RPC-007 | `steer` | P |  | `rpc.rs::rpc_007` |
| RPC-008 | `follow_up` | P |  | `rpc.rs::rpc_008` |
| RPC-009 | `abort` | P |  | `rpc.rs::rpc_009` |
| RPC-010 | `clear_queue` | P |  | `rpc.rs::rpc_010` |
| RPC-011 | `new_session` | P |  | `rpc.rs::rpc_011` |
| RPC-012 | `get_state` | A | adds `mode` and cache fields | `rpc.rs::rpc_012` |
| RPC-013 | `get_messages` | P |  | `rpc.rs::rpc_013` |
| RPC-014 | `set_model` | A | engine models; new epoch | `rpc.rs::rpc_014` |
| RPC-015 | `cycle_model` | A | engine models; new epoch | `rpc.rs::rpc_015` |
| RPC-016 | `get_available_models` | A | engine models; Model object fields that do not apply are fixed values | `rpc.rs::rpc_016` |
| RPC-017 | `set_thinking_level` | A | W6 boolean | `rpc.rs::rpc_017` |
| RPC-018 | `cycle_thinking_level` | A | W6 boolean | `rpc.rs::rpc_018` |
| RPC-019 | `get_available_thinking_levels` | A | clamped to the model | `rpc.rs::rpc_019` |
| RPC-020 | `set_steering_mode` | P |  | `rpc.rs::rpc_020` |
| RPC-021 | `set_follow_up_mode` | P |  | `rpc.rs::rpc_021` |
| RPC-022 | `compact` | A | W5 | `rpc.rs::rpc_022` |
| RPC-023 | `set_auto_compaction` | P |  | `rpc.rs::rpc_023` |
| RPC-024 | `set_auto_retry` | P |  | `rpc.rs::rpc_024` |
| RPC-025 | `abort_retry` | P |  | `rpc.rs::rpc_025` |
| RPC-026 | `bash` | P | like `!cmd`: user's own rights | `rpc.rs::rpc_026` |
| RPC-027 | `abort_bash` | P |  | `rpc.rs::rpc_027` |
| RPC-028 | `get_session_stats` | A | adds exact cache statistics; cost is 0 | `rpc.rs::rpc_028` |
| RPC-029 | `export_html` | P |  | `rpc.rs::rpc_029` |
| RPC-030 | `switch_session` | P |  | `rpc.rs::rpc_030` |
| RPC-031 | `fork` | P | rebase (W1) | `rpc.rs::rpc_031` |
| RPC-032 | `clone` | P |  | `rpc.rs::rpc_032` |
| RPC-033 | `get_fork_messages` | P |  | `rpc.rs::rpc_033` |
| RPC-034 | `get_entries` | P |  | `rpc.rs::rpc_034` |
| RPC-035 | `get_tree` | P |  | `rpc.rs::rpc_035` |
| RPC-036 | `get_last_assistant_text` | P |  | `rpc.rs::rpc_036` |
| RPC-037 | `set_session_name` | P |  | `rpc.rs::rpc_037` |
| RPC-038 | `get_commands` | P |  | `rpc.rs::rpc_038` |
| RPC-039 | `set_mode` | A | oxa only: plan \| workspace \| ask | `rpc.rs::rpc_039` |
| RPC-040 | Model object shape | A | one provider; costs 0; `contextWindow` = `n_ctx` | `rpc.rs::rpc_040` |
| RPC-041 | Extension UI request `select` | P |  | `rpc.rs::rpc_041` |
| RPC-042 | Extension UI request `confirm` | P |  | `rpc.rs::rpc_042` |
| RPC-043 | Extension UI request `input` | P |  | `rpc.rs::rpc_043` |
| RPC-044 | Extension UI request `editor` | P |  | `rpc.rs::rpc_044` |
| RPC-045 | Extension UI `notify` | P |  | `rpc.rs::rpc_045` |
| RPC-046 | Extension UI `setStatus` | P |  | `rpc.rs::rpc_046` |
| RPC-047 | Extension UI `setWidget` | P |  | `rpc.rs::rpc_047` |
| RPC-048 | Extension UI `setTitle` | P |  | `rpc.rs::rpc_048` |
| RPC-049 | Extension UI `set_editor_text` | P |  | `rpc.rs::rpc_049` |
| RPC-050 | Extension UI responses: value, confirmation, cancellation; timeouts | P |  | `rpc.rs::rpc_050` |
| RPC-051 | A permission Ask is relayed as an extension-UI `confirm` | A | oxa only | `rpc.rs::rpc_051` |
| RPC-052 | Rust RPC client drives the `examples/rpc-client` flow | A | no TS SDK; `oxa-rpc` ships a Rust client | `rpc.rs::rpc_052` |

## TUI: Terminal UI

pi page(s): tui.md, terminal-setup.md, themes.md (display), tmux.md. Tests: `oxa-parity/tests/tui.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| TUI-001 | Component model: a component renders lines for a width; the renderer diffs lines and writes minimal updates | P |  | `tui.rs::tui_001` |
| TUI-002 | Built-in components: text, box, select list, input, loader, border/spacer, scrollbar, Markdown | P | pi's extra layout components (VStack, HStack, ScrollView, SettingsList, CancellableLoader, MouseRegion, Image) are not in oxa (O-175); pi's Box is `Boxed` | `tui.rs::tui_002` |
| TUI-003 | Editor: kill ring + yank-pop, undo, history, paste markers, word navigation | P |  | `tui.rs::tui_003` |
| TUI-004 | IME support: hardware cursor positioned at the draw cursor | P |  | `tui.rs::tui_004` |
| TUI-005 | Autocomplete: `/` commands, `@` fuzzy files (.gitignore), Tab paths, `autocompleteMaxVisible` | P |  | `tui.rs::tui_005` |
| TUI-006 | Markdown rendering with syntax highlighting, tables, lists, nested code, `codeBlockIndent` | P | syntect | `tui.rs::tui_006` |
| TUI-007 | Mermaid diagrams | A | shown as a highlighted code block | `tui.rs::tui_007` |
| TUI-008 | LaTeX rendering | A | shown as source text | `tui.rs::tui_008` |
| TUI-009 | Hyperlinks (OSC 8) with detection | A | detection only, no override (O-175) | `tui.rs::tui_009` |
| TUI-010 | Clipboard: OSC 52, wl-copy/xclip | P |  | `tui.rs::tui_010` |
| TUI-012 | Inline images: kitty and iTerm2 protocols with detection | A | display only; detection only, no override (O-175) | `tui.rs::tui_012` |
| TUI-013 | Fullscreen mode: alt screen, follow/scroll, scrollbar modes, wheel lines, prompt jump, resize keeps position | P |  | `tui.rs::tui_013` |
| TUI-014 | Fullscreen: mouse selection, copy-on-select, transcript search next/previous | P |  | `tui.rs::tui_014` |
| TUI-015 | Regular mode: finished blocks are printed once to the scrollback | P |  | `tui.rs::tui_015` |
| TUI-016 | Exit output: transcript or resume hint | P |  | `tui.rs::tui_016` |
| TUI-017 | Thinking blocks hide/collapse; tool output expand/collapse; live output; diff view for edits | P |  | `tui.rs::tui_017` |
| TUI-018 | Startup header and loaded-resource listing (`quietStartup`) | P |  | `tui.rs::tui_018` |
| TUI-019 | Selectors: model, thinking, theme, settings, sessions, tree, scoped models, MCP | P |  | `tui.rs::tui_019` |
| TUI-020 | Keyboard: kitty protocol, legacy sequences, ESC timeout | A | kitty keyboard protocol flags 7 with the query; no modifyOtherKeys fallback; one constant ESC timeout of 10 ms counted from the ESC (O-175) | `tui.rs::tui_020` |
| TUI-021 | Terminal restored after a panic or signal | A | oxa only | `tui.rs::tui_021` |
| TUI-022 | Wide characters, ANSI-aware wrap and truncate, shrink clear | P |  | `tui.rs::tui_022` |
| TUI-023 | tmux setup (passthrough) | A | OSC 52 passthrough; extended keys through tmux are not in oxa (O-175) | `tui.rs::tui_023` |
| TUI-025 | Custom screens, overlays and custom editor from extensions | A | JS component `render(width)` + `handleInput`; pi-tui class shim | `tui.rs::tui_025` |
| TUI-026 | Footer / status line shows mode, cache hit, prefill progress and tok/s | A | oxa only | `tui.rs::tui_026` |
| TUI-027 | Permission prompt Once / Session / Deny; mode cycle; timed dialogs | A | oxa only | `tui.rs::tui_027` |

## THM: Themes

pi page(s): themes.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| THM-001 | Built-in `dark` and `light` themes | P | pi's JSON copied | `resources.rs::thm_001` |
| THM-002 | `system` theme from terminal colours (OSC 10/11) | P |  | `resources.rs::thm_002` |
| THM-003 | Custom theme JSON: `vars`, colour tokens, 256-colour fallback | P | pi's theme JSON loads unchanged; theme diagnostics are reported at startup in every mode | `resources.rs::thm_003` |
| THM-004 | Theme discovery: agent dir, project `.pi/themes`, packages, `--theme` | P | `.oxa/themes`; project themes are trust-gated; theme diagnostics are reported at startup in every mode | `resources.rs::thm_004` |
| THM-005 | Hot reload of the active theme file | P |  | `resources.rs::thm_005` |
| THM-006 | `/settings` theme selector, `--use-theme name[/name]` (dark/light pair) | P |  | `resources.rs::thm_006` |

## CFG: Configuration files

pi page(s): configuration.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| CFG-001 | Agent directory `~/.pi/agent`: `settings.json`, `models.json`, `keybindings.json`, `mcp.json`, `trust.json`, resources | P | `~/.oxa/agent` (`OXA_AGENT_DIR`) | `resources.rs::cfg_001` |
| CFG-002 | Project `.pi/` directory: settings, extensions, skills, prompts, themes, `mcp.json` | P | `.oxa/` | `resources.rs::cfg_002` |
| CFG-003 | Context files `AGENTS.md` / `CLAUDE.md`: agent dir, then parents of cwd down to cwd | A | part of the frozen prefix (W3); below HOME the upward walk ends at HOME (pi walks to `/`) | `resources.rs::cfg_003` |
| CFG-004 | `AGENTS.override.md` same-directory rule | P |  | `resources.rs::cfg_004` |
| CFG-005 | `SYSTEM.md` replaces and `APPEND_SYSTEM.md` appends to the system prompt; project over user | P |  | `resources.rs::cfg_005` |
| CFG-006 | `.agents/skills` is read | P |  | `resources.rs::cfg_006` |

## SKL: Skills

pi page(s): skills.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| SKL-001 | Agent Skills layout: `<name>/SKILL.md` with `name` and `description` frontmatter | P |  | `resources.rs::skl_001` |
| SKL-002 | Frontmatter validation warns and never fails (name rules, description length) | P |  | `resources.rs::skl_002` |
| SKL-003 | `<available_skills>` listing in the system prompt | P | part of the frozen prefix; new skills appear after `/reload` (new epoch) | `resources.rs::skl_003` |
| SKL-004 | `disable-model-invocation` hides a skill from the prompt | P |  | `resources.rs::skl_004` |
| SKL-005 | `/skill:name [args]`; `enableSkillCommands` | P |  | `resources.rs::skl_005` |
| SKL-006 | Discovery: agent dir, project, `.agents/skills`, packages, settings, `--skill`; `-ns` | P |  | `resources.rs::skl_006` |
| SKL-007 | Name collisions: first loaded wins with a diagnostic | P |  | `resources.rs::skl_007` |

## TPL: Prompt templates

pi page(s): prompt-templates.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| TPL-001 | `prompts/*.md` become `/name` commands; frontmatter `description`, `argument-hint` | P |  | `resources.rs::tpl_001` |
| TPL-002 | Arguments `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:N}` | P | expanded before the text becomes a message | `resources.rs::tpl_002` |
| TPL-003 | Quoted arguments, apostrophes and quotes (CHANGELOG cases) | P |  | `resources.rs::tpl_003` |
| TPL-004 | Discovery: agent dir, project, packages, settings, `--prompt-template`; `-np` | P |  | `resources.rs::tpl_004` |

## PKG: Packages

pi page(s): packages.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| PKG-001 | `pi install git:<url>` / `https://` git sources with a ref | P |  | `resources.rs::pkg_001` |
| PKG-002 | Local path sources | P |  | `resources.rs::pkg_002` |
| PKG-004 | `-l`, `--local` project scope vs user scope; identity rules | P |  | `resources.rs::pkg_004` |
| PKG-005 | Package manifest (`pi` key in package.json) and conventional directories | A | `oxa` key, `pi` key accepted | `resources.rs::pkg_005` |
| PKG-006 | Resource filters `!x`, `+x`, `-x` per package in settings | P |  | `resources.rs::pkg_006` |
| PKG-007 | `pi list`, `pi remove`, `pi update <source>` | P |  | `resources.rs::pkg_007` |

## SEC: Security and project trust

pi page(s): security.md, containerization.md. Tests: `oxa-parity/tests/resources.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| SEC-001 | Trust prompt for project-local resources on first run in a directory | A | oxa trusts the project by default (no prompt, owner decision 2026-11-21); `ask` asks once in the interface, `ask`/`never` skip in headless runs; trust approves content (sha256 of the project files that run or load code), so a changed file asks again (interactive) or is not loaded with a notice (headless) until approved or `--approve` | `resources.rs::sec_001` |
| SEC-002 | Trust-gated resources: project settings, extensions, skills, prompts, themes, `mcp.json`, context files as pi lists them | A | the rows are what `defaultProjectTrust: "never"` (owner opt-in) hides; a fresh install loads them without a question (owner decision 2026-11-21) | `resources.rs::sec_002` |
| SEC-003 | Decision order: flag > extension (`project_trust`) > saved (closest parent) > `defaultProjectTrust` | P |  | `resources.rs::sec_003` |
| SEC-004 | Non-interactive `ask` skips trust-gated resources | P |  | `resources.rs::sec_004` |
| SEC-005 | `/trust` saves a decision; `trust.json` | P |  | `resources.rs::sec_005` |
| SEC-006 | pi has no sandbox and no permission prompts | A | built-in bwrap sandbox + modes plan / workspace / ask; trust gates what loads, modes gate what tools do; a deny-list of secrets (keys, tokens, the agent dir, browser profiles) is refused to the file tools and masked for bash in every mode; `.oxa/**` writes ask; `.git/hooks` and `.git/config` are never written by tools | `resources.rs::sec_006` |
| SEC-007 | Containerization recipes (Docker, OpenShell, Gondolin) | A | replaced by the built-in sandbox | `resources.rs::sec_007` |
| SEC-008 | Extension host functions and MCP stdio servers | A | pass the same `Policy` | `resources.rs::sec_008` |

## CM: Codemode

pi page(s): codemode.md. Tests: `oxa-parity/tests/codemode.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| CM-001 | `codemode` tool runs a JavaScript script in QuickJS | P | same runtime as extensions | `codemode.rs::cm_001` |
| CM-002 | Options line on line 1 only (`max_output_tokens`, `timeout_ms`) | P |  | `codemode.rs::cm_002` |
| CM-003 | `tools.<name>(args)` resolves to the tool's result | A | W4: a script gets the same shaped result text the model would get; a failing tool rejects with an `Error` carrying it; no structured `bash` or `CallToolResult` objects | `codemode.rs::cm_003` |
| CM-004 | `text(value)`, top-level `return`, `exit()` | P |  | `codemode.rs::cm_004` |
| CM-005 | `console.log/info/warn/error/debug` into `<console_output>` | P |  | `codemode.rs::cm_005` |
| CM-006 | `store(key, value)` / `load(key)` | P |  | `codemode.rs::cm_006` |
| CM-007 | `ALL_TOOLS`, `searchTools(query, {limit, namespace})` (BM25, default 8), `describeTool`, `describeNamespace` | P |  | `codemode.rs::cm_007` |
| CM-010 | Output cap keeps start and end; full text in a temp file | A | W4 `shape` | `codemode.rs::cm_010` |
| CM-011 | 256 MB VM memory limit; out-of-memory error | P |  | `codemode.rs::cm_011` |
| CM-012 | Worker cap (default 2, env override); waiters queue | P |  | `codemode.rs::cm_012` |
| CM-013 | A promise that can never settle fails immediately; no timers | P |  | `codemode.rs::cm_013` |
| CM-014 | Scripts cannot start other codemode scripts | P |  | `codemode.rs::cm_014` |
| CM-015 | `Promise.allSettled` over tool calls | P | engine calls are not involved | `codemode.rs::cm_015` |
| CM-016 | `codemode.mode` `on\|only` | A | the description is one fixed text, frozen per epoch (W3); no inline tool listing, so no `codemode.inlineBudget` (SET-020) | `codemode.rs::cm_016` |
| CM-017 | `tool_search` declares matched tools for the next call | A | matches returned as text; called through one frozen `call_tool {name, args}`; prefix sha unchanged | `codemode.rs::cm_017` |

## MCP: MCP servers

pi page(s): mcp.md. Tests: `oxa-parity/tests/mcp.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| MCP-001 | `mcp.json` user + project; project replaces a same-named server; partial override rules | P |  | `mcp.rs::mcp_001` |
| MCP-002 | stdio servers: `command`, `args`, `env`, `cwd` | A | spawned under the same `Policy` / sandbox rules | `mcp.rs::mcp_002` |
| MCP-003 | Streamable HTTP servers: `url`, `headers`, `bearerTokenEnvVar` | P |  | `mcp.rs::mcp_003` |
| MCP-004 | `type` optional; `sse` rejected with a hint | P |  | `mcp.rs::mcp_004` |
| MCP-005 | `${VAR}` expansion and whole-value `!command` in `env` and `headers` | P |  | `mcp.rs::mcp_005` |
| MCP-006 | `timeout` per request (default 60 s); progress notifications reset it | P |  | `mcp.rs::mcp_006` |
| MCP-007 | `enabled: false`; invalid entries are reported and skipped | P |  | `mcp.rs::mcp_007` |
| MCP-008 | Server name rules; tool names `mcp__<server>__<tool>`; mangling + collision hash | P |  | `mcp.rs::mcp_008` |
| MCP-009 | Background connect; first prompt waits up to 10 s | P |  | `mcp.rs::mcp_009` |
| MCP-010 | Exposure `codemode` (default): callable from scripts, not declared | P | never touches the prefix | `mcp.rs::mcp_010` |
| MCP-011 | Exposure `deferred` | A | via `tool_search` text + `call_tool` | `mcp.rs::mcp_011` |
| MCP-012 | Exposure `direct` | A | frozen at epoch start; a server that connects later is offered at the next epoch | `mcp.rs::mcp_012` |
| MCP-013 | Exposure `hidden`; per-tool `toolExposure` | P |  | `mcp.rs::mcp_013` |
| MCP-014 | `description` and the `mcp_servers` system prompt section (4096 / 250 char limits) | A | part of the frozen prefix; changes at the next epoch | `mcp.rs::mcp_014` |
| MCP-015 | Resource tools `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` | A | O-144 text-only engine: image / audio / binary content blocks of any MCP result become the fixed placeholder line | `mcp.rs::mcp_015` |
| MCP-016 | OAuth: PKCE, dynamic registration, configured client, callback server, pasted redirect URL | P |  | `mcp.rs::mcp_016` |
| MCP-017 | Token file 0600; refresh on 401; `logout` deletes credentials | P |  | `mcp.rs::mcp_017` |
| MCP-018 | `/mcp`: list order, sign in, reconnect, enable/disable, exposure change saved to the defining file | P |  | `mcp.rs::mcp_018` |
| MCP-019 | `mcp.log` with rotation | P |  | `mcp.rs::mcp_019` |
| MCP-020 | Stop: stdin close, SIGTERM, SIGKILL of the process group | P |  | `mcp.rs::mcp_020` |
| MCP-021 | `--no-mcp` | P |  | `mcp.rs::mcp_021` |
| MCP-022 | Permissions: MCP tool calls | A | workspace mode asks for MCP write-class tools | `mcp.rs::mcp_022` |
| MCP-025 | MCP from the SDK | A | Rust API of `oxa-mcp` | `mcp.rs::mcp_025` |

## EXT: Extensions

pi page(s): extensions.md, sdk.md. Tests: `oxa-parity/tests/extensions.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| EXT-001 | Extensions are TypeScript modules loaded in process (jiti); `export default function (pi)` | A | JavaScript in embedded QuickJS, same API shape: `export default function (oxa)`; `.ts` type-stripped at load or rejected with a hint | `extensions.rs::ext_001` |
| EXT-002 | Extension locations: agent dir, project `.pi/extensions`, packages, settings, `-e` | P | `.oxa/extensions` | `extensions.rs::ext_002` |
| EXT-004 | Sync and async factory functions | P |  | `extensions.rs::ext_004` |
| EXT-005 | A load error isolates one extension | P |  | `extensions.rs::ext_005` |
| EXT-006 | Handler order and composition rules | P |  | `extensions.rs::ext_006` |
| EXT-007 | `/reload` re-creates extensions | P | drops the QuickJS contexts | `extensions.rs::ext_007` |
| EXT-008 | event `project_trust` | P |  | `extensions.rs::ext_008` |
| EXT-009 | event `resources_discover` | P |  | `extensions.rs::ext_009` |
| EXT-010 | event `mcp_servers_change` | A | observer; server set changes apply at the next epoch | `extensions.rs::ext_010` |
| EXT-011 | event `session_start` | P |  | `extensions.rs::ext_011` |
| EXT-012 | event `session_info_changed` | P |  | `extensions.rs::ext_012` |
| EXT-013 | event `session_before_switch` | P |  | `extensions.rs::ext_013` |
| EXT-014 | event `session_before_fork` | P |  | `extensions.rs::ext_014` |
| EXT-015 | event `session_before_compact` | P | custom summary lands as the W5 head | `extensions.rs::ext_015` |
| EXT-016 | event `session_compact` | P |  | `extensions.rs::ext_016` |
| EXT-017 | event `session_compact_failed` | P |  | `extensions.rs::ext_017` |
| EXT-018 | event `session_before_tree` | P |  | `extensions.rs::ext_018` |
| EXT-019 | event `session_tree` | P |  | `extensions.rs::ext_019` |
| EXT-020 | event `session_shutdown` | P |  | `extensions.rs::ext_020` |
| EXT-021 | event `before_agent_start` | A | may append a message; a system prompt replacement is refused | `extensions.rs::ext_021` |
| EXT-022 | event `agent_start` | P |  | `extensions.rs::ext_022` |
| EXT-023 | event `agent_end` | P |  | `extensions.rs::ext_023` |
| EXT-024 | event `agent_before_settle` | P |  | `extensions.rs::ext_024` |
| EXT-025 | event `agent_settled` | P |  | `extensions.rs::ext_025` |
| EXT-026 | event `ui_prompt_start` | A | fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI) | `extensions.rs::ext_026` |
| EXT-027 | event `ui_prompt_end` | A | fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI) | `extensions.rs::ext_027` |
| EXT-028 | event `turn_start` | P |  | `extensions.rs::ext_028` |
| EXT-029 | event `turn_end` | P |  | `extensions.rs::ext_029` |
| EXT-030 | event `message_start` | P |  | `extensions.rs::ext_030` |
| EXT-031 | event `message_update` | P |  | `extensions.rs::ext_031` |
| EXT-032 | event `message_end` | P |  | `extensions.rs::ext_032` |
| EXT-033 | event `tool_execution_start` | P |  | `extensions.rs::ext_033` |
| EXT-034 | event `tool_execution_update` | P |  | `extensions.rs::ext_034` |
| EXT-035 | event `tool_execution_end` | P |  | `extensions.rs::ext_035` |
| EXT-036 | event `context` | A | result must have the current wire as a byte prefix, else refused unless `rebase: true` (logged as a miss cause) | `extensions.rs::ext_036` |
| EXT-037 | event `context_with_system` | A | same rule; the system part is read-only | `extensions.rs::ext_037` |
| EXT-039 | event `before_provider_request` | A | same rule as `context` | `extensions.rs::ext_039` |
| EXT-040 | event `after_provider_response` | A | observer; `status` is 200 and `headers` is `{}` (the engine client does not hand them up) | `extensions.rs::ext_040` |
| EXT-041 | event `provider_stream_event` | A | observer; `data` is oxa's engine increment, not a raw provider chunk | `extensions.rs::ext_041` |
| EXT-042 | event `cache_warming_decision` | A | may cancel the idle prefill | `extensions.rs::ext_042` |
| EXT-043 | event `model_select` | P |  | `extensions.rs::ext_043` |
| EXT-044 | event `thinking_level_select` | P |  | `extensions.rs::ext_044` |
| EXT-045 | event `tool_call` | P | block or replace args before execution; the wire keeps the original `args_raw` (W2) | `extensions.rs::ext_045` |
| EXT-046 | event `tool_result` | P | once, before the push | `extensions.rs::ext_046` |
| EXT-047 | event `user_bash` | P |  | `extensions.rs::ext_047` |
| EXT-048 | event `input` | P | transform or handle | `extensions.rs::ext_048` |
| EXT-049 | ctx.ui dialogs: `select`, `confirm`, `input`, `editor`, `notify`; timed dialogs; AbortSignal dismissal | P |  | `extensions.rs::ext_049` |
| EXT-050 | `ctx.ui.setStatus`, `setWidget` (placement), `setFooter`, `setHeader`, `setTitle`, `setEditorText`, working message | P |  | `extensions.rs::ext_050` |
| EXT-051 | `ctx.ui.custom` components, overlays, custom editor | A | JS components via the pi-tui class shim | `extensions.rs::ext_051` |
| EXT-052 | Autocomplete providers | P |  | `extensions.rs::ext_052` |
| EXT-053 | `ctx.mode`, `ctx.hasUI`, `ctx.cwd`, `ctx.isProjectTrusted()` | P | `ctx.permissionMode` added | `extensions.rs::ext_053` |
| EXT-054 | `ctx.sessionManager` read API (entries, branch, leaf, labels) | P |  | `extensions.rs::ext_054` |
| EXT-055 | `ctx.modelRegistry` / `ctx.model` / `ctx.thinkingLevel` / `ctx.scopedModels` | A | engine models only | `extensions.rs::ext_055` |
| EXT-057 | `ctx.signal`, `ctx.isIdle()`, `ctx.abort()`, `ctx.hasPendingMessages()`, `ctx.shutdown()` | P |  | `extensions.rs::ext_057` |
| EXT-058 | `ctx.getContextUsage()` | P | exact counts | `extensions.rs::ext_058` |
| EXT-059 | `ctx.compact()` | A | W5 | `extensions.rs::ext_059` |
| EXT-060 | `ctx.getSystemPrompt()` / `getSystemPromptOptions()` | A | returns the frozen prefix; read-only | `extensions.rs::ext_060` |
| EXT-061 | `ctx.waitForIdle()`, `ctx.newSession()`, `ctx.fork()`, `ctx.navigateTree()`, `ctx.switchSession()`, `ctx.reload()` | A | session-shape actions requested mid-run apply when the run settles; `ctx.newSession()` / `ctx.switchSession()` are held as a swap request that the TUI and print mode apply between commands (RPC/SDK front ends that do not poll `Session::take_swap_request` get `unsupported_in_oxa`) | `extensions.rs::ext_061` |
| EXT-062 | `pi.on(event, handler)` | P | `oxa.on` | `extensions.rs::ext_062` |
| EXT-063 | `pi.registerTool(definition)` with parameter schema validation | A | JSON Schema objects (no TypeBox import); tools join the frozen list at the next epoch | `extensions.rs::ext_063` |
| EXT-064 | `pi.sendMessage(message, {deliverAs, triggerTurn})` | P | always an append | `extensions.rs::ext_064` |
| EXT-065 | `pi.sendUserMessage(content, options)` | P |  | `extensions.rs::ext_065` |
| EXT-066 | `pi.appendEntry(customType, data)` | P |  | `extensions.rs::ext_066` |
| EXT-067 | `pi.setSessionName` / `getSessionName` / `setLabel` | P |  | `extensions.rs::ext_067` |
| EXT-068 | `pi.registerCommand(name, options)` / `pi.getCommands()` | P |  | `extensions.rs::ext_068` |
| EXT-069 | `pi.registerMessageRenderer`, `registerEntryRenderer`, `registerToolRenderer` | P | renderers return lines | `extensions.rs::ext_069` |
| EXT-070 | `pi.registerMarkdownTransformer(transformer, {messageTypes})` | P |  | `extensions.rs::ext_070` |
| EXT-071 | `pi.registerShortcut(shortcut, options)` | P |  | `extensions.rs::ext_071` |
| EXT-072 | `pi.registerFlag(name, options)` | P |  | `extensions.rs::ext_072` |
| EXT-073 | `pi.exec(command, args, options)` | A | through `Policy` + sandbox | `extensions.rs::ext_073` |
| EXT-074 | `pi.getActiveTools()` / `getAllTools()` / `setActiveTools(names)` | A | a change is a new epoch, applied when the run settles (W3) | `extensions.rs::ext_074` |
| EXT-075 | `pi.setModel(model)` | A | engine models; new epoch | `extensions.rs::ext_075` |
| EXT-076 | `pi.getThinkingLevel()` / `setThinkingLevel(level)` | P |  | `extensions.rs::ext_076` |
| EXT-077 | `pi.events` shared event bus | P |  | `extensions.rs::ext_077` |
| EXT-080 | `pi.getSettings()` | P |  | `extensions.rs::ext_080` |
| EXT-081 | Host `fs` and `fetch` functions | A | oxa only: no ambient authority; behind `Policy` | `extensions.rs::ext_081` |
| EXT-082 | Custom tools: `renderCall`, `renderResult`, keybinding hints | P |  | `extensions.rs::ext_082` |
| EXT-083 | Overriding built-in tools by name | A | allowed; frozen per epoch | `extensions.rs::ext_083` |
| EXT-084 | Tool output truncation helpers | A | W4 `shape` applies to every extension tool result | `extensions.rs::ext_084` |
| EXT-085 | Dynamic tool loading mid-session | A | deferred to the next epoch; `tool_search` + `call_tool` for discovery | `extensions.rs::ext_085` |
| EXT-086 | Tool exposure (`direct`, `codemode`, `deferred`, `hidden`) for extension tools | P |  | `extensions.rs::ext_086` |
| EXT-087 | Calling other tools from a tool | P | through the full loop path | `extensions.rs::ext_087` |
| EXT-088 | Error handling: a throwing handler emits `extension_error` and does not stop the run | P |  | `extensions.rs::ext_088` |
| EXT-089 | Mode behaviour: UI calls are no-ops / defaults in print and JSON mode, records in RPC | P |  | `extensions.rs::ext_089` |
| EXT-090 | Memory and time limits per extension context | A | oxa only | `extensions.rs::ext_090` |
| EXT-091 | Exec hooks: `settings.json` `hooks: {event: command}` | A | oxa only | `extensions.rs::ext_091` |
| EXT-092 | Subagent example extension (single / parallel / chain) | A | `builtin:subagent` (O-146: on by default on large non-Flash models; off on the Rust Flash engine and below 32K; `oxa.subagent.enabled` true/false wins; a member of the default tool selection): one child at a time, each `oxa --mode json` with its own session id, background, and on a local model continuing the parent's prefix and wire (an isolated child otherwise, then the parent is parked first); at most 8 children per parent session; see ENG-010..ENG-019 | `extensions.rs::ext_092` |
| EXT-093 | Example ports: plan-mode, permission-gate, custom-compaction, todo, status-line | A | each loads and passes a scripted session. O-144: pi's custom-compaction summarises with a second (cloud) model; the port sends no model request of its own (one model, one slot) and supplies custom instructions to the W5 summary request | `extensions.rs::ext_093` |
| EXT-094 | Example ports: tools, commands, input-transform, question, git-checkpoint, trigger-compact, protected-paths | P |  | `extensions.rs::ext_094` |
| EXT-095 | TypeScript SDK (sdk.md) | A | the Rust API of `oxa-core` is the SDK; other languages use RPC | `extensions.rs::ext_095` |

## ENG: Engine support (oxa only, O-144)

pi page(s): none (oxa only). Source: `OXA-ENGINE-SUPPORT.md`. Every oxa feature must be supported by the engine: one inference slot behind a queue, parked sessions, text only, thinking on/off, per-model limits. Tests: `oxa-parity/tests/tree.rs`, `oxa-parity/tests/subagent.rs`. Ids ENG-010..ENG-019 are the subagent rows (policy sec. 2 of that document), tests `oxa-parity/tests/subagent.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| ENG-001 | One oxa instance never has two model requests in flight: the run, the summary, the idle prefill and extension / codemode model calls pass one gate | A | one engine slot; a second request would only queue and park the first one's state | `tree.rs::eng_001` |
| ENG-002 | Input from the user (a queued steering or follow-up message, an abort) cancels this instance's own in-flight background request: the threshold compaction summary and the idle prefill. A cancelled prefill is skipped silently; a cancelled summary is decided again after the next run; the queued input then runs (`Session::prompt` before it returns, `Session::run_queued` after a navigation or `/compact`). Summaries the user waits on (`/compact`, overflow recovery, the compaction before a model switch, a branch summary) are foreground and end only by an abort (F12) | A | H3 orders the queue but does not cut a running request; the engine keeps the prefill progress of a cancelled request | `tree.rs::eng_002` |
| ENG-003 | Engine busy with another client: the `: queued [pos=<n>] wait_s=<s>` comments (and the two older forms) are shown as `queued #<pos>` or `queued <s>s` (activity line and footer; `engine_queue` record `{pos, waitS}` in JSON / RPC), never time out while they arrive, and do not change the exit code. An error after the early `200` (SSE `error` event) is classified like a status: overflow -> compaction, 4xx final, 5xx retried; `503` (`engine_busy`) is retried after its `Retry-After` with the same `Idempotency-Key` | A | controller queue (round-robin per session, cap 8 waiters per principal) | `tree.rs::eng_003` |
| ENG-004 | `/model`, `set_model`, the cycle key and extension `setModel` to a model whose hard limit (`cap - max_output - reserveTokens`) is below the current context: nothing is sent to the target above it. Interactive: the user sees both numbers and chooses compact first / fork / cancel. Everywhere else (RPC, extensions): compact first. Compact first = a foreground summary on the SOURCE model, sized for the target (O-145); the switch happens only when the rebased head fits, else the model is unchanged | A | Flash 16K vs 128K; the switch itself is a new epoch whose one notice prints the cold re-prefill tokens and estimated seconds for the whole wire | `tree.rs::eng_004` |
| ENG-005 | Context window, compaction threshold and maximum output come from `/_local/status` `model_limits[<model>]`; no per-model table in oxa; an engine without it uses the provisional minimum rule | A | per-alias limits (X3) | `tree.rs::eng_005` |
| ENG-006 | Every request names its session (`X-Session-Id`); idle prefill, the park and `X-Oxe-Snapshot: keep` are sent only when advertised: `model_limits[<model>].capabilities` (known before the model loads), else `/props` `oxe_hints`, re-read once after the first reply and after a recovered `503`. `preserve_thinking` is not sent to a model without that capability (Flash). A null `snap_pos` is one info notice | A | parked sessions and round-robin are keyed by the session; an engine without H6 would sample a token | `tree.rs::eng_006` |
| ENG-010 | `subagent`: at most one child process alive; `tasks` run in call order and show as `subagent k/n, queued` | A | one engine slot: parallel children would only queue and park each other | `subagent.rs::eng_010` |
| ENG-011 | Eight children per parent session (a warning threshold) over the whole tree: more than 8 `tasks` or `chain` steps, or children that run or wait over every call in flight, still run (queued) and the result ends with a warning naming `oxa.subagent.maxChildren` | A | O-146; pi caps only the `tasks` array | `subagent.rs::eng_011` |
| ENG-012 | Each child sends `X-Session-Id: <parent>.s<n>` (n counts up per parent session), `X-Oxe-Priority: background` and turn kind `subagent`. On a local model its first request continues the parent's frozen prefix and its wire (task appended), so the engine keeps the session's state and the parent's next turn reuses its context; a cloud child, a cloud session and `oxa.subagent.isolated` keep the isolated child | A | the engine parks and restores per session; a human at another prompt goes first | `subagent.rs::eng_012`, `subagent.rs::fork_child_continues_the_parents_prefix` |
| ENG-013 | Depth 1: a child never loads `builtin:subagent`, so it cannot spawn one (a fork child's shared prefix lists the tool; calling it is an unknown tool) | A | a nested child would park its parent's child | `subagent.rs::eng_013` |
| ENG-014 | An agent's `model:` other than the session's is ignored with a warning; the child runs on the session's model | A | one model loaded; a swap cold-prefills the whole wire | `subagent.rs::eng_014` |
| ENG-015 | An isolated child's soft compaction limit = min(`oxa.subagent.softTokens` 24000, the parent's `compact_at`, cap - 1); a fork child starts at the session's context and takes the session's own compaction point; either compacts like any session | A | each isolated child prefills its own context, so a call with children takes longer than one prompt | `subagent.rs::eng_015` |
| ENG-016 | The tool result is each child's final assistant text only, capped at `oxa.subagent.resultCap` (6 KiB) with a note naming the spill file | A | the result is prefilled at the parent's depth (no timings) | `subagent.rs::eng_016` |
| ENG-017 | The tool is not declared on a model whose context is below 32000 unless `oxa.subagent.smallContext`; re-evaluated at a model switch | A | Flash: 16K context, disk parks only | `subagent.rs::eng_017` |
| ENG-018 | Before the first isolated child of a call the parent is parked (a fork child needs none: it continues the session's state): one zero-token request with `X-Oxe-Snapshot: keep` (skipped when the engine does not advertise it); the parent's next request reuses exactly the parked tokens. After every child, also a failed or aborted one, its engine state is cleared by its hint; the parent's state and shared blocks stay | A | the park and the hint clear use the snapshot and clear-by-hint requests | `subagent.rs::eng_018` |
| ENG-019 | Explicit abort cancels the child's process group (TERM, then KILL after 2 s); forced parent shutdown cancels its engine request and starts no further task. The first SIGTERM lets the active child finish before cooperative wrap-up | A | cancel propagation and cooperative wrap-up | `subagent.rs::eng_019`, `subagent.rs::sigterm_finishes_the_active_subagent_before_wrap_up` |

## OUT: Out-of-scope pages

pi page(s): (one row per page). Tests: `oxa-parity/tests/cli.rs`.

| id | behaviour | V | note (invariant or reason) | test |
|---|---|---|---|---|
| OUT-011 | quickstart.md, index.md, how-pi-works.md | A | ported as oxa docs (F11); every command line in them runs against the fake | `cli.rs::out_011` |

## Retired import IDs

- `CLI-053`: omitted (removed with pi import); reserved and never reused.
- `SLASH-016`: omitted (removed with pi import); reserved and never reused.
- `SES-008`: omitted (removed with pi import); reserved and never reused.
- `FMT-019`: omitted (removed with pi import); reserved and never reused.
- `CFG-008`: omitted (removed with pi import); reserved and never reused.

## Summary

| section | P | A | rows |
|---|---|---|---|
| CLI | 43 | 14 | 57 |
| MOD | 0 | 5 | 5 |
| SET | 29 | 20 | 49 |
| KEY | 15 | 5 | 20 |
| ENV | 9 | 3 | 12 |
| TOOLS | 13 | 11 | 24 |
| USE | 10 | 2 | 12 |
| SLASH | 16 | 9 | 25 |
| SES | 11 | 3 | 14 |
| FMT | 11 | 9 | 20 |
| TREE | 9 | 3 | 12 |
| CMP | 11 | 8 | 19 |
| JSON | 25 | 3 | 28 |
| RPC | 39 | 13 | 52 |
| TUI | 15 | 10 | 25 |
| THM | 6 | 0 | 6 |
| CFG | 5 | 1 | 6 |
| SKL | 7 | 0 | 7 |
| TPL | 4 | 0 | 4 |
| PKG | 5 | 1 | 6 |
| SEC | 4 | 4 | 8 |
| CM | 11 | 4 | 15 |
| MCP | 16 | 7 | 23 |
| EXT | 61 | 29 | 90 |
| ENG | 0 | 16 | 16 |
| OUT | 0 | 1 | 1 |
| **total** | **375** | **181** | **556** |
