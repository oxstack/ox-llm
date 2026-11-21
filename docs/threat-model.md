# ox-llm threat model

Used by automated security review (Codex) and by human/agent reviewers. Keep it short and current.

## What we protect
- **Credentials:** provider API keys, the ChatGPT sign-in token record (access + refresh token), OpenCode keys, GitHub App keys/tokens, share/guest keys. They live in user-only files (mode 0600) and the protected key store.
- **The user's machine:** files outside the workspace, the user's shell, other processes, the network.
- **Money and quotas:** metered provider spend, daily budget caps, subscription allowances (ChatGPT, OpenCode Go).
- **Private data:** prompts, sessions, transcripts, usage ledgers, local model state.

## Who may be hostile
- **Model output** (any provider, any model): may try to read secrets, escape the sandbox of an unattended run, call tools it should not, or smuggle secrets into tool arguments. In yolo it already has the owner's rights.
- **Repository content and project settings** (`.oxa/`, project settings files, AGENTS.md, skills, MCP config in a cloned repo): trusted by default (owner decision 2026-11-21), so its hooks, MCP servers, extensions, skills and prompts run as you. The owner can opt into a question per project (`defaultProjectTrust: "ask"`) or into loading nothing (`"never"`); a project file cannot set either.
- **Upstream providers and the network:** malformed or hostile streams, oversized responses, redirects, wrong usage/billing fields.
- **LAN share guests** and **outside contributors** (fork PRs on this public repo).

## Risk we accept (owner 2026-11-21)
oxa is a power tool for its owner, not an enterprise endpoint product. It must stay powerful, so it carries risk on purpose. The guards (dangerous-command rules, publish floor, output masking, approvals) are seatbelts that catch mistakes and ordinary model misbehaviour. They are not a boundary against a determined attacker who already runs code as the owner.

So these are **not** P0/P1 and never block a merge (at most P3 hardening, filed as a follow-up):
- an attacker who already runs code with the owner's UID or in the owner's trusted, writable repo: same-UID ptrace or process dumping, hard links, symlink or rename races (TOCTOU), a hostile project script or background job undoing a check;
- shell obfuscation that slips a command past a guard (`coproc`, `exec -a`, clustered `env` options, wrappers, watch loops), when the command itself is something the owner's mode allows anyway;
- anything in yolo beyond its two hard refusals (publishing a secret, an OS-breaking wipe);
- theoretical chains with no concrete trigger a real model or project hits in normal use.

What still blocks (P0/P1): a secret leaving the machine or landing in model context, logs or session files; a remote party (network, LAN guest, fork PR, provider stream) gaining code execution, credentials or spend; metered money spent past the reservation, ledger or a configured cap; a project file changing user-only settings (trusted or not); data loss or an OS-breaking wipe under default settings; offline mode using the network.

**Power is the default; restrictions need the owner (owner 2026-11-21).** Any change that makes oxa less powerful (a new refusal, prompt, cap, fail-closed path, narrower default, or a removed capability) needs the owner's explicit acceptance. Its PR body carries one line per restriction: `Restricts: <what> — <why> — <drawback for the user>`, and the PR merges only after the owner accepts it with the `owner-accepted` label. This holds for review findings too: a finding that asks for such a change states the same three things, and outside the block list it is a P3 suggestion, never a blocker.

**Judgement over rules (owner 2026-11-21).** oxa should behave like a senior engineer, not a compliance tool. Where a guard is really needed outside the block list, the answer is judgement: a clean-context second model that reviews the risky step (#2336), a self-review before "done", or one longer deliberation, not a new refusal, prompt or cap. Review findings propose that kind of guard instead of a rule.

## Trust boundaries (must hold)
These are the block list above, spelled out. Anything else is hardening (P3 at most).
1. **Secrets never leave their store:** never printed, logged, put in prompts, tool output, session files, telemetry, test fixtures, CI logs or error messages; redaction covers streamed text and decoded tool arguments.
2. **User-only settings:** a project file can never change engine transport, native binary/provider selection, `allowRemote`, sharing, login, feature flags, or budgets.
3. **Sandbox:** the sandbox standard in `SECURITY.md` says which runs use bubblewrap (unattended runs without yolo). Where it is on, private paths are denied (credential files, `~/.oxa` private dirs, ledgers), and model output or remote input gets no new escape routes (bind mounts, env passthrough, `/proc`, sockets, setuid helpers). An escape that needs code already running as the owner outside the sandbox is hardening. Interactive and yolo runs have no OS sandbox by design. The file tools deny private paths in every mode except yolo outside plan.
4. **Offline mode** makes no network calls at all.
5. **Network guard:** the engine endpoint is loopback unless the user enables remote in user settings; no credential is sent across a redirect or to a host it was not issued for.
6. **Budgets and ledgers:** every metered request is reserved before sending and settled from validated provider usage; unknown usage stays reserved; a configured cap refuses the next request; ledger rows are append-only and validated. With no configured cap there is no cap check.
7. **Guests and outside PRs:** LAN-share guests never reach cloud providers or owner credentials; fork PRs never get secrets, tokens or token-spending reviews (`pull_request_target` is forbidden).
8. **Native engine (oxe):** the HTTP server listens on loopback only; `unsafe`, FFI (kernels, NPU), process spawning and file permission changes reachable from remote input need explicit review.

## Advisory Go review allowance

The fixed-price OpenCode Go review is advisory and defaults off. Trusted repository maintainers with Write access govern `GO_REVIEW_ENABLED`, `GO_REVIEW_MAX_PER_DAY` (40 reviews per UTC day by default), and the PR comment markers used to count reviews and suppress repeats for the same diff. The allowance depends on maintainers preserving those markers. Metered spend remains subject to the budget and ledger requirements above. Required CI and review gates remain in force.

## Most security-relevant paths
`oxa/oxc-gateway/src/` (providers, auth, ledger, redaction, share), `oxa/oxa-cli/src/{login_cmd,in_process,dispatch}.rs`, `oxa/oxa-sandbox/`, `oxa/oxa-perm/`, `oxa/oxa-proto/src/guard.rs`, `oxa/oxa-tools/src/bash.rs`, `engine/crates/{server,kernels-sys,image-npu,oxn}/`, `.github/workflows/`, `packaging/`.
