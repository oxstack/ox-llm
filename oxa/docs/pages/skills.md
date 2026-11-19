# Skills, prompts, context files and themes

These are the files that shape what the model knows and how oxa looks. Each one is plain text, read at start-up, on `/reload`, and at a new session. Changes to the files that go into the system prompt take effect at the next prefix epoch, which is the next `/reload`, model switch or new session.

## Skills

A skill is a directory with a `SKILL.md` file. The file starts with front matter and then holds the instructions:

```text
---
name: my-skill
description: What the skill does and when to use it.
---
Instructions the model reads when the task matches the description.
```

- `name` is required: lowercase letters, digits and hyphens, at most 64 characters, and the same as the directory name is recommended.
- `description` is required and says when to use the skill. It is at most 1024 characters. The system prompt lists each skill's name and description in a `<skills>` section. The model reads the full `SKILL.md` only when it needs it.
- `disable-model-invocation: true` in the front matter hides the skill from the model. You can still run it with `/skill:name`.
- A skill that breaks these rules is reported as a warning at start-up.

Where skills are found, in order. The first skill with a name wins; a later one with the same name is dropped with a warning:

1. `--skill <path>` (repeatable);
2. `~/.oxa/agent/skills/`;
3. `~/.agents/skills/`, except skills another product synced there (a `synced/` folder, or a folder whose `manifest.json` lists `skillId`s): they are written for that product's own tools, so oxa skips them unless `oxa.skills.includeSynced` is `true`;
4. the project's `.oxa/skills/` (trusted projects only);
5. the project's `.agents/skills/`, in the project directory and each parent up to the repository root (trusted projects only);
6. the paths in the `skills` setting;
7. packages that declare skills;
8. the skills built into oxa;
9. the system skills directory, `OXA_SYSTEM_SKILLS_DIR`, which defaults to `/usr/share/omarchy/default/agents/skills` and is ignored when it does not exist.

A skill in a user location with the same name as a built-in one replaces the built-in. A built-in or system skill that loses a name clash is dropped silently.

`enableSkillCommands` (default `true`) lists skills in the slash-command autocomplete. `/skill:name [arguments]` runs a skill in either case. `--no-skills` turns discovery off.

### Built-in skills

oxa ships four skills. They appear in the list whether or not their files have been written, and `oxa.builtinSkills: false` removes them.

| Name | Use |
|---|---|
| `ox-llm` | operating the ox-llm platform that runs the agent: the available models, usage and spend, session status, the `oxe` engine, logs and troubleshooting with the `oxa` CLI |
| `pull-request` | the pull-request workflow: branch and commit identity, opening a draft PR with a verified body, following up on reviews (the review text supplied with the task first, `gh` only where the session may use it, and a plain statement when neither source gave it), and the rules (no push to a queued PR, no force-push, no merging unless told) |
| `report-issue` | drafting an issue for the ox-llm repository from scrubbed system and session facts. The draft is shown to you, and it is posted only when you confirm in your terminal. Reporting is off until `oxa.reportIssue.repo` is set |
| `oxa-extend` | how to extend oxa: skills, prompt templates, extensions and exec hooks, where each file goes, and how to keep the cache warm |

## Prompt templates

A prompt template is a Markdown file in a `prompts/` directory: the user's `~/.oxa/agent/prompts/`, the project's `.oxa/prompts/` (trusted projects), the `prompts` setting, packages, or `--prompt-template <path>`. Its file name is the command name. `/name arguments` expands the template and sends the result as your message.

```text
---
description: Review a file for bugs
---
Review $1 for correctness. Focus on: ${2:-error handling}. Extra notes: $@
```

- `$1`, `$2` and so on are positional arguments. `$@` is all arguments. `${1:-default}` uses a default when the argument is missing.
- `description` is optional. Without it, oxa uses the first non-empty line of the body.
- An extension command with the same name wins over a template.

## Context files

Context files give the model standing instructions about the project. oxa reads `AGENTS.md` and `CLAUDE.md`, in this order:

1. the agent directory's file (`~/.oxa/agent/AGENTS.md` or `CLAUDE.md`);
2. one file per directory, from the outermost ancestor of the project directory down to the project directory itself.

If a directory has `AGENTS.override.md`, it is read instead of that directory's `AGENTS.md` and `CLAUDE.md`. The search stops at your home directory: a project under home never reads files from above it. A project outside home is searched up to the root, but not into another user's home. `--no-context-files` (`-nc`) turns the search off.

Context files are read in an untrusted project as well, because they are text. Their content is kept as written, and it is part of the frozen system prompt.

### System prompt files

- `~/.oxa/agent/SYSTEM.md` replaces the default system prompt. `.oxa/SYSTEM.md` in a trusted project wins over it.
- `~/.oxa/agent/APPEND_SYSTEM.md` and `.oxa/APPEND_SYSTEM.md` (trusted) are appended to the prompt.
- `--system-prompt <text or path>` and `--append-system-prompt <text or path>` do the same for one run. The append flag can be repeated.

### The identity

The system prompt starts with a short identity text that describes this machine and the engine. It is read from `~/.oxa/agent/identity.md`, which oxa refreshes by itself at every start from this machine (model, chip, memory, OS, oxa version) and the engine's model list; the file is rewritten only when a fact changed, so the system prompt and its cache stay the same otherwise, and a running session keeps its text until its next prefix epoch. Local models get one line each with their limits; cloud models are named by provider only (one line, so a provider's catalog change does not change the prompt), and `oxa --list-models [search]` prints them with their limits. When the engine does not answer, a file in the current format is kept as it is. `oxa identity --refresh` does the same by hand (with `--offline`, without asking the engine). Without the file, oxa uses a generic line. `oxa identity` without `--refresh` prints the current text.

## Themes

Three themes are built in:

| Name | Look |
|---|---|
| `system` (default) | follows the terminal's background colour, read when oxa starts; if that is not available, `COLORFGBG`; otherwise dark |
| `dark` | dark background |
| `light` | light background |

Choose one with `/theme <name>`, the `theme` setting, or `--use-theme <name>` for one run.

A custom theme is a JSON file with a `name`, an `appearance` (`dark` or `light`), optional `vars` (named colours), and a `colors` object that sets the interface tokens. A colour is a hex value (`#rrggbb`), a function (`okhsl(H S% L%)` or `oklch(L C H)`), or the name of a `vars` entry. A token that is not set uses its fallback; a token without a fallback must be set. The simplest start is a copy of the built-in dark theme's file (`oxa-tui/assets/themes/dark.json` in the source tree), with the values changed.

Theme files are read from `~/.oxa/agent/themes/`, the project's `.oxa/themes/` (trusted projects), the `themes` setting, packages, and `--theme <path>`. `--no-themes` disables all but the `--theme` paths. A file that cannot be read or has a bad colour is reported at start-up, and a custom theme named like a built-in one is ignored with a reason.

## Where to look next

- [extensions.md](extensions.md) for extensions and exec hooks;
- [quickstart.md](quickstart.md#where-files-live) for the directory layout;
- [settings.md](settings.md) for `skills`, `prompts`, `themes`, `enableSkillCommands` and `oxa.builtinSkills`.
