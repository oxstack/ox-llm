# Unattended work

## Worker mode

`oxa work` workers start in their default mode inside the tool sandbox. When the owner's
own settings (`permissions.defaultMode` in the user layer) say `yolo`, the runner starts
every worker with `--yolo`: no prompts and no tool sandbox. Without yolo nothing changes.
A project's settings are not read for this and cannot raise the worker mode.

## Worker GitHub access

For a task whose GitHub access the runner owns, `oxa.work.workerGithub` (owner settings only)
says what the worker keeps. Unset follows the owner's mode:

| Mode | Value | The worker gets |
|---|---|---|
| yolo | `worker` | its own GitHub environment and `gh`, MCP, web search and subagents; the packet guidance allows `git` and `gh` |
| auto | `broker` | no token and no `gh`; the runner's packet guidance stays until the host-side GitHub tools land |
| standard | `runner` | today's behaviour: no GitHub access, the runner's packet only |

An explicit owner value wins in every mode. A project setting is ignored.

`oxa work board --shadow --repo OWNER/REPO --once` captures a read-only GitHub sample.
For continuous five-minute sampling use `--token-stdin`: the caller supplies a fresh
newline-terminated App token on a private pipe before each sample. No token is
retained across samples or read from a credential file. The five-minute user timer
may instead remint externally and repeat `--once` with `GH_TOKEN`. It requires a
trusted source project and an explicit `GH_TOKEN` supplied by a GitHub App caller;
offline mode refuses before GitHub or log creation. No worker or mutation runs.

`oxa lead serve` can run this sample itself, with no timer: see
[the lead web page](pages/lead-web.md#the-sample-inside-the-server). Both paths write the
same log and run the same sample (`work_board::sample_once`); a fresh token is taken per
sample, and neither keeps one. Do not run both against one log unless that is what you
want: each records its own samples, and the page counts them by time.
The private, locked `work/<project-hash>/board/OWNER/REPO/board-shadow.jsonl` under the agent
directory appends `{schema_version:1,observed_at,board,would_take}`. Eligible shared
board entries form `would_take`; shadow reserves no leases or attempts. Use one
GitHub repository per capture. Read errors and incomplete connections stop sampling.

Add `--landing-shadow` to record latest-review holds and queue-age alarms in `landing`.
The sample records the observed main ref in `landing_main` independently of queue membership
and verified merge-group rejection evidence in `queue_rejections`, including the
removal event time.
Use `--queue-evidence` to record only the rejection evidence alongside the board.

Add `--lead-alarms` to also record the protected Lead view's stuck-work sources in the
same sample: the raw `panels` repository record (main required checks with classic
protection and ruleset requirements, the verified main merge and merge-queue
membership) and complete `status=queued` Actions pages in `runs`. Each source is
captured and degraded independently: a missing or oversized record stays absent,
the sample is never dropped for it, and the Lead view renders that source as
explicitly unavailable. A main alarm needs one failed required check at the exact
observed head; failing optional jobs, pending required checks and unobserved
requirements never page.

To verify the provenance of test-report archives, pass `--queue-report-policy FILE` with an owner-only
JSON file in a private directory. Set the trusted workflow path, the exact names
of required checks that consume those reports, and every JUnit producer and artifact.

```json
{
  "workflow_path": ".github/workflows/checks.yml",
  "required_check_names": ["workspace test gate"],
  "reports": [{"producer_name": "Workspace shard 1", "artifact_name": "test-report-1"}]
}
```

That option also enables queue evidence. The shadow journal records the checked
run, attempt, required check IDs, SHA-256 digests, and failed test identities after
verifying and reading every configured JUnit archive for that attempt.

Limit: 100 PRs/items per connection. Reported checks are matched against the base's
classic protection and active repository/organization rulesets; missing required
contexts and unknown/no required checks stay pending. Review
requires GitHub's approval decision and all latest approving writer reviews at the
current head; older approvals wait, including pure base merges without fresh evidence.
Snapshot reads reject changing open-PR sets and responses above 4 MiB. Detailed
merge state is checked; behind heads can be queue-ready when the main merge queue
composes and validates them, without requesting pre-emptive main merges.

The opt-in `oxa_work::board` library derives one next action per open PR from a
single normalized GitHub snapshot. `Policy.enabled` defaults to false and makes
no GitHub reads while disabled. Findings take precedence over failed required CI,
conflicts, missing nonempty `Removes:` lines for source removal, then green draft
promotion and reviewed readiness. Pending CI or unknown mergeability waits; other
PRs wait for review. Release-pinned PRs sort first, then action priority and number.
Active leases make entries ineligible without hiding them. Snapshot adapters must supply the
complete open set, current-head required checks, unresolved findings, source-removal
facts and review-policy approval for the PR's own diff. Callers
select private repository-scoped claim storage outside worker-writable binds.
`Claims::open(root).snapshot()` supplies durable state for `Board::read`.
Before dispatch, `claim(entry, policy, holder, now, ttl)` atomically reserves a
lease and an attempt under a kernel file lock, then syncs state before returning.
`budget_pools.limits.max_attempts_per_day` sets the daily attempt threshold as an
advisory default of three per UTC day, and zero warns immediately: a claim past the
threshold is still reserved, and `oxa work board --refill` prints one operator
warning on stderr with the attempt ordinal, the threshold and its UTC window. The
attempt count never fences admission: `enabled: false`, the pause marker, wait
actions, stale-head and protected-configuration checks, active leases and Claims
failures still do.
A crash after reservation consumes an attempt.
`release(number, lease)` fences stale workers and never refunds attempts. Active
leases survive UTC midnight; corrupt state, or state that vanished after a PR's
first reservation, fails closed. Execution, fresh-head validation and CLI polling
remain in the adapter; core claim storage never spawns workers or writes to GitHub.

A worker works out its own build cache: one Cargo target directory per main
repository, `<cache dir>/<hash>/cargo-target`, shared by every worktree of that
repository. It is the only extra writable path of the worker sandbox and
`CARGO_TARGET_DIR` points at it, so no manual `writePaths` step is needed. A
`CARGO_TARGET_DIR` that the owner sets himself wins. The user-only settings
`oxa.workers.sharedCache` (default on), `oxa.workers.cacheDir` and
`oxa.workers.cacheWarnGb` (no default, warning only) control it; a project file
cannot. The acceptance supervisor still uses its own isolated check output.

A native board task is held for publication, which is not offline: its worker
runs online with the runner's real `--offline` switch, so provider-backed models
work. The hold is enforced by runner-owned GitHub isolation (no token, no GitHub
adapters, no MCP, even for an owner in yolo) and by never pushing; the finished
record says `local verified; publication held`. A checkpoint saved before this
change with the offline restriction stays offline.

A managed worker can load project resources whose content matches the saved
approval for its source repository. The runner verifies that the checkout is
a managed worktree before reusing that approval. An explicit worktree denial
still wins. Inheritance is per launch, and creates no permanent worktree grant.
The managed worker refuses inherited approval when project MCP, extensions,
or settings can load dependencies outside the approval record. Worker
settings, MCP and agent definitions come from the user configuration. The
work runner takes acceptance checks from the approved source.

`oxa work tasks.json` opts into sequential tasks in separate git worktrees.
Run it inside the project repository. The JSON file is an array:

```json
[{"id":"docs-fix","model":"your-model","branch":"agent/docs-fix","base":"main","prompt":"Fix documentation links."}]
```

`base` defaults to `main`. IDs contain 1–128 letters, digits, hyphens or underscores.
In a task file, optional `max_time` is a positive integer number of seconds and
defaults to 2700 (45 minutes). Newly generated issue, pull request, and native
queue tasks get four hours. Existing checkpoints retain their saved budget.
The task's saved deadline is shared by its worker, repair and crash recovery.
A repair does not get a fresh budget. Each child receives its remaining
allowance as `--max-time` for one graceful wrap-up turn. If it remains alive 60
seconds after that allowance, the runner sends SIGTERM, then SIGKILL after another
10 seconds. The result is interrupted and its worktree is retained. `kill` must be
available as a trusted executable outside the project; status reads do not need it.
IDs and branches must be unique within one file. Existing task branches can be
continued. For a GitHub issue use `oxa work --issue 123 --model your-model`.
This explicitly invokes your installed,
authenticated `gh` CLI to fetch the issue title, body, state and comments, and that
one lookup is the whole task packet the worker receives: the worker stays inside a
sandbox that cannot read the issue, so the state and the comment thread (oldest
first, bounded to 16 KiB with an explicit truncation marker) arrive with the text.
If comments are truncated, the worker asks its runner for the missing data.
Repeating the same issue task, including with `--rerun`, reuses its saved packet.
The worker's local tools run in a sandbox with tool networking disabled.
The worker starts without GitHub authentication, MCP servers, extensions or web
search. The runner owns GitHub lookup and the configured model connection stays
available. These restrictions also apply when the saved issue task is rerun
from a task file.
The runner refuses GitHub issue responses over 4 MiB before starting a worker.
A reply body that is not text, or a malformed state, refuses the task instead of
dropping it. Issue lookup is refused when the environment or user settings enable
offline mode.
Issue lookup also requires source-project trust. Git and GitHub CLI host calls use
validated executables outside the repository. Worker and check-supervisor PATHs
exclude project directories, relative entries and unsafe aliases; project tools
can still run by explicit path inside the sandbox.
Use `oxa work --pr 123 --model your-model` for a pull request in the source
repository. The runner supplies its body, discussion, unresolved inline reviews,
failing checks, and changed files from the reviewed commit. Fetch that commit and
set the local PR branch to it before running. The runner refuses a missing or
different local tip. The worker starts at the reviewed commit and keeps the same
tool restrictions as an issue worker. Truncated sections request missing data
from the runner. Each reviewed commit has its own task ID and frozen packet.
When a newer reviewed commit is already on the local branch, a finished clean
task worktree for the same PR is left in place at a detached HEAD. The next round
gets its own worktree. Uncommitted changes or an active task block the transition.
For runner-owned issue and PR packets the checkout, acceptance, save, and saved
verification supervisors run under the same GitHub and network restriction.

Branches must be literal names; checkout-history aliases such as `@{-1}` are refused.
A local base branch that lags behind the commit the base names (for example a local
`main` after a fetch, though the runner itself never fetches) is fast-forwarded to that
commit for the new worktree, so the task's own diff against its base shows only its own
work. The branch is moved only when it is a strict ancestor of the base and no worktree
has it checked out; otherwise it is left as it is. That move is host metadata, so no
repository hook runs for it.
Every base and acceptance command list is resolved before any task in the batch
starts. All bases must therefore exist at batch startup. Host metadata commands
ignore replacement objects and inherited `GIT_*` overrides, disable filesystem
monitors, and never lazily fetch missing objects. Fetch the needed objects before
starting the batch. Checkout and checks also ignore replacement objects.

Each task runs a child `oxa --mode=json` with its model and a saved session. The installed offline policy,
including a parent `--offline` flag, is passed to every child. Normal agent permission, trust and
sandbox rules apply. Configure project trust before unattended use. The runner
passes `-na` on every child start, including repair, and removes `OXA_TRUST_PROJECT`.
Worker project settings, MCP servers and extensions are therefore ignored even
when a parent directory or the user's default would trust the new worktree.
User-level resources follow normal settings for tasks without a GitHub packet.
External acceptance uses the approved source project and the batch's frozen command list. The runner
creates detached Git metadata with callbacks suppressed on the host, then creates
the task branch and populates the fresh worktree inside the normal sandbox.
Reference hooks, checkout hooks and smudge filters stay inside it; a reference hook
that rejects the task branch still blocks the task.
Missing bwrap blocks before checkout or a worker starts. Configured hook directories
keep their normal read-only protection: a checkout that needs to populate a tracked
protected hook directory blocks with the worktree retained.
Acceptance commands are frozen from the captured base commit before checkout
callbacks can change settings. The runner executes those `done` commands outside the
agent after it exits, even without edits. The runner also derives a local compile
precheck from the Cargo packages touched in the diff against the captured base and
in untracked files. The owner can configure `oxa.work.localChecks` in user settings.
It defaults to `["cargo check"]`; the runner supplies `-p <crate>` for each selected
package. Workspace-wide changes and shared inputs outside packages in a tree with
Cargo manifests require hosted coverage. These changes do not select a package
for a local compile check. A successful local compile still leaves Rust work
unverified pending tests in hosted lane CI at the exact head.
The worker can name additional acceptance crates by ending its final reply with
`<oxa-acceptance>["crate-name"]</oxa-acceptance>`; a block that is not readable crate
names, or a crate that is not part of the repository, keeps the task unverified.
The source project must be trusted before a task starts. By default a trusted
folder's changed `done` commands are reported and run; `projectTrustPolicy: "content"`
requires fresh approval of changed executable project content. Invoking `work` does
not approve an untrusted project. If checks cannot run for lack of trust, the error
points to the source folder and interactive `/trust` route. Acceptance runs in the normal bwrap sandbox;
missing or unusable bwrap blocks the task. Checkout has a ten-minute limit. Each
check and the change inventory have a ten-minute ceiling within the remaining task deadline.
`in subdirectory: command` selects a folder inside that sandbox. The runner rejects
paths that escape the worktree. Each working-tree check round also
gets a private writable output directory beside the worktree (exposed as
`OXA_WORK_OUTPUT_DIR`, `CARGO_TARGET_DIR` and `TMPDIR`), so generated build outputs
never dirty the work the round judges; committed rounds keep their export-local one.
The final Git inspection is also sandboxed, with a read-only workspace, no network
and fsmonitor disabled. Clean filters stay inside that sandbox; an inspection
error blocks the task rather than producing a clean verdict.
Agent-internal done checks may also run when the project is trusted.
Each round records the evidence its verdict rests on: the task's record carries one
`checks` entry per check run with its origin, command, exit status, timeout and
resulting status. Saved verification (`oxa work verify`) evaluates the exact saved
commit and still requires hosted test evidence for Rust changes or declared suites.

The runner reads the child's exit code, terminal `oxa_result` and session header,
without retaining its event stream. It checkpoints the session ID as soon as the
header arrives. A blocked or failed worker remains blocked; an interrupted worker
remains interrupted. Neither consumes a repair attempt. Failed acceptance (local
or lane) after a successful worker continues that same saved session once, then
acceptance runs once more.
JSON lines include the worker result, session ID, captured `base_sha`, saved
`head_sha`/`tree_sha`, step, checkpoint revision and time, and the `checks` evidence
of the last acceptance round. Outcomes are `done`,
`nothing_to_do` (no worker commits or file changes), `red` (checks still fail),
`unverified` (checks were absent, could not run, or still require hosted evidence),
`blocked` (setup or
execution failed), or `interrupted`. Deadline interruptions
carry a typed `interruption: "deadline"`. Exit codes are 0 for success, 1 if any
task is red, unverified, blocked or interrupted, and 2 for invalid input, output or checkpoint
errors. A checkpoint/event-append error stops the entire batch before another
action. An ordinary task setup failure produces a blocked
JSON line and does not skip later tasks. If a task cannot be locked or has an
incompatible receipt, its existing state is preserved; a pre-worktree failure has
an empty `worktree` in the result. Locks use kernel ownership and are inherited
by the worker and check/save supervisors. Killing the runner does not allow a
second runner to start while its child still holds that lock.

Worktrees remain in `.oxa-worktrees/<repository>/<id>`
beside the repository (or the separate git directory). Private result receipts and
locks live under `~/.oxa/agent/work/<repository-hash>/` (or the configured agent
directory), outside the worker's writable Git bind.
The configured agent directory must also remain outside repositories, task
worktrees and Git metadata; an unsafe placement refuses admission before writing
owner state. New receipts are published
atomically without overwriting an existing receipt; a validated matching receipt
is replaced atomically under the task lock when continuing. Worktrees stay outside that git
directory so the normal sandbox can bind git objects and refs for commits.
Before finishing, the runner saves staged, unstaged and untracked work with an
ordinary sandboxed commit, including red, blocked and interrupted work. Normal
hooks remain active. Unresolved merges are kept in progress; a rejecting hook or
remaining dirty files block completion. No automatic cleanup or PR creation is implemented. A task with `lane_ci`
pushes its accepted commit as described below; other tasks never push. Keep
retained work until its saved commit has been published and inspected. Review
task prompts and bases before running them.

`done` here describes locally checked and saved work. For a `lane_ci` task it
also requires the lane run at the saved head to have passed. It does not mean
published or merged. Publication must verify a clean committed export,
bind checks to that commit and verify base-object integrity before pushing;
ignored files in a retained working directory are not commit-bound evidence.

`oxa work verify <id>` rechecks a saved successful task from its exact head in a
fresh sandboxed checkout with an independent, strictly validated object store.
It uses the captured base's checks and requires unchanged head/tree identities
and a clean tracked tree before and after checking. The checkout and Git metadata
are read-only during committed checks, so transient source substitutions cannot
pass. A separate private writable output mount is supplied as
`OXA_WORK_OUTPUT_DIR` and `TMPDIR`; `CARGO_TARGET_DIR` points to its `target`
subdirectory. Checks needing other generated outputs must direct them into that
output mount. Retained ignored helpers are absent. A separate index built from the verified commit stays outside the checks'
writable binds; final read-only inspection uses it rather than trusting index
flags or staged entries left by a check. Verification preserves the saved offline restriction and any current
offline request. It prints a JSON observation with the task, head, tree, base and
check time, and retains the task's work unchanged. It starts no worker and does
not push or create a pull request. The library returns a non-deserializable
`VerifiedTask` holding the task lock and checked object graph; a publisher must
consume that live verification rather than trust an old receipt or printed JSON.

To recover a task, run the same task file again. An unfinished checkpoint resumes
its saved step with the original base, commands, session and offline policy.
Recovery loads those inputs before resolving the current base branch or settings,
so a deleted base ref or invalid new checks cannot strand an existing attempt.
Finished successful work is returned without another worker or check run; add
`--rerun` to the task-file or issue command to start another attempt. A task spec
change requires a new ID. Unsupported or corrupt checkpoints are preserved and
refused. A matching linked worktree is reused; the
source checkout is never used as the task worktree. An existing branch checked
out elsewhere is blocked. A new task branch that is already published starts at
its pushed head instead of the base: the branch is created at the lone
remote-tracking tip of the same name when that tip descends from the captured
base (never for several diverging tips or an unrelated one), so reviewing or
continuing a pull request works from its head. Fetch first; no fetch is run here.
Dirty retained work is preserved and saved before
retrying. A clean retained branch gets an ordinary sandboxed
merge of the captured base; no reset, stash, force checkout or automatic fetch runs.

An unfinished merge is passed to the worker with its old pending commit, the
captured current base and at most twenty conflicted paths. The worker must finish
that merge and merge the captured base too. Aborted or unresolved merges and branch
switches remain blocked even if acceptance passes. Worker, merge and acceptance
failures are retained together. Receipt replacement keeps private modes.

`oxa work status --json` (also `--status`) reads checkpoints without acquiring a
task lock, changing state, constructing a model or calling GitHub. Its separate
`liveness` is `running`, `finished`, `interrupted`, or `unknown` when the kernel
cannot establish ownership. A missing original parent is not proof of a stopped
child: status also checks the inherited stdin lock. Without `--json` it prints
one summary line per task, plus any pending publication and its next step.

For a saved head that differs from its base, status distinguishes a commit with
no recorded push, an unconfirmed destination, and a branch whose exact head was
pushed but has no recorded PR. The age is time since the runner saved that head,
not the last retry; old checkpoints may have no age. These are local observations,
not proof that GitHub currently has no PR or that a manually pushed branch is absent.
Status adds no network probes. An authorized local worker owns the push, ready PR,
CI fixes and review replies; a worker without GitHub access hands off to its authorized
runner. Seeing a pending row does not grant publication authority.

The [checkpoint integration contract](work-state.md) defines the runner's public
event hook. Event/situation/standing stores and their CLI adapters are a separate
integration; this slice does not infer facts from worker prose or raw logs.
`oxa work tasks.json --publish` pushes the accepted, saved commit to `origin` and
creates or updates its PR through `gh`. Sign in with `gh auth login --hostname HOST`
for the host in `origin`. Publication uses that host's saved login. Red acceptance never pushes; green
acceptance makes the PR ready. Blocked tasks do not create PRs. Drift-only accepted reruns may update an existing PR.
The runner checks existing PR queue membership before every push and refuses a
queued PR; use a new task branch for follow-up work. It refuses branch changes
made after acceptance until `--rerun` checks them. PR bodies include task ID and
acceptance state, without raw prompts, transcripts or failed-check output.
Execution preserves a saved offline restriction across restarts; later explicit
publication uses the current network policy without changing that restriction.
A publication failure keeps a publication checkpoint; another `--publish` retries
without replaying the agent. Offline mode refuses network publication.
Before each push, the runner asks the owner to approve the accepted commit, branch and
PR destination. In an oxa session, publication and problem reporting use the authenticated
owner confirm broker (`OXA_CONFIRM_SOCK`). Yolo records a yes without a dialog; standard and
auto show a dialog that the owner's typed `yes` can answer. The Linux sandbox exposes only the
pinned broker socket, not its directory, and requires a same-user peer outside the sandbox's PID
namespace. Outside the sandbox on Linux and macOS, the peer must be the same user and the unique
ancestor running the same oxa executable. An unverified broker cannot approve.
A direct owner invocation asks on `/dev/tty`. Piped input and text the model read never approve.
After the yes the runner re-checks the origin push URL and task head and refuses if either changed.
When a task cannot be published because it did not finish with an accepted result, the record's
`detail` explains why.

### What the publication scan refuses

Before it asks for approval, `--publish` (and a lane push) scans every commit it would push
that `origin` does not have: the commit objects, the names of added files and every added
line. It refuses, and names only the file and line, when it finds a private key, a known
access token format (GitHub, Anthropic, AWS, Slack, Google, GitLab; also when a letter or digit
directly precedes the prefix), a `password`/`token`/`secret`-style assignment or quoted
literal with an opaque value (spaces inside quotes do not hide it), an assignment to a name
ending in `_key` or `credentials` with a long opaque value (`ENCRYPTION_KEY=...`; paths, URLs
and `${VAR}` references pass), a `Bearer`/`Basic` header (also as a quoted `"Authorization"`
key) or URL password, or the value of a secret-named environment variable or setting. A JSON
value edited under an unchanged key line is scanned together with that key. Lines added to a
secret store (`.env`, `credentials`, key and certificate files, registry logins such as
`.docker/config.json`) are refused. An empty secret-store file and a template such as
`.env.example` are not refused by name: they publish unless their content holds one of the
secrets above. A path that itself contains secret text is refused and withheld.

Deleting a credential in a later commit does not unblock publication, because the earlier
commit is still pushed. Rewrite the unpublished commits (`git rebase` or a fresh branch), and
rotate a credential that left the machine. The scan reads only git objects; it does not
change them.

`--report-problems` enables the `report-issue` workflow for the task's own structured
problem notes. The owner launches it directly, or from an agent tool that holds the session's
owner confirm broker; an agent-launched run with neither is refused. The plain workflow shows
the scrubbed worker queries and asks before sending any search. If you opt into semantic dedupe,
that setting authorizes its search instead. Each new report or comment shows its exact cleaned
text and asks the owner the same way as a publish. Without a yes it stays a local draft. Reports use the user setting `oxa.reportIssue.repo`;
project settings cannot choose a destination. Notes, titles and queries pass
through the existing report scrubber. The runner performs two or three symptom
queries against open issues and PRs and reads the top matches before writing.
It comments only new notes on a matching issue, links an existing fixing PR, or
files a new report with related links. Search failure, offline mode or missing
report configuration keeps a local scrubbed draft and records its path/detail.
A later explicit `--report-problems` retries local reports without replaying the
agent. A draft-write failure records a failed report and finishes the task checkpoint;
`--report-problems` can retry it. Reports contain only scrubbed structured symptom
summaries, without raw assistant output or acceptance logs.


Queue mode watches a directory of regular, non-hidden `*.json` task files.
Each file may contain up to 256 tasks and occupy up to 1 MiB. The per-file task
count is an advisory, configurable threshold: `--max-input-tasks N` sets it for
this run (default 256, between 1 and 4096); a file with more tasks still
contributes its first N tasks, and one notice names the limit and what was kept.
The kept records then go through the normal validation and admission, so a
duplicate id, a shared branch or a retired identity can still refuse an
individual task. The count notice is informational: it never counts as a failed
input or a failed task. The byte limit, the directory bounds and the identity
retention below do not change with that option. A queue accepts
up to 256 task files and 16 MiB of input per scan. The controller retains at
most 4,096 task identities and 16 MiB of task text across scans. Archive
completed inputs and restart the controller if it reaches the retention limit.
After adding task files, run
`oxa work --queue ./tasks --max 2 --min-free-mib 1024`; add
`--max-input-tasks 64` to bound each file more tightly for that run.

Files are admitted oldest modification time first (filename breaks ties), with
array order preserved inside a file. At most `--max` tasks run; surviving workers
from an interrupted controller still occupy slots until their locks release.
Task IDs must be unique across the queue. Completed task state prevents agent
replay on restart. Files remain available for inspection and manual archiving.
Write new files with an atomic rename so the runner reads a complete task file.

The default free-space floor is 512 MiB. The runner checks the repository, common
git directory, worktree storage and queue filesystems before each admission. A low
or unreadable free-space value pauses new work while active jobs finish. Invalid
JSON files report an error and do not skip other work. `--once` drains available
work and exits; when disk space blocks pending tasks it exits 1 with their files
retained. Without `--once`, the directory stays under watch for new files.
Appending tasks to an existing file queues only new IDs. Discovered task definitions
are immutable; changing one requires a new ID. Correcting or archiving an invalid
input clears its current queue errors. Archiving inputs does not interrupt admitted
work. A controller or monitor error stops admission and drains active workers
before returning an error.

Queue mode supports `--publish`, `--report-problems` and `--rerun` with the same
explicit policy as single-file mode. Its worker threads preserve the parent's
offline policy. Standard output emits JSON lines tagged `state`, `task`, `error`
or `notice`; a `notice` is informational and never counts as a failed input or
task.
The latest state line is also replaced atomically at
`<queue>/.oxa-queue-status.json` for a monitor. It contains a heartbeat timestamp,
active task IDs, pending/finished/failed counts, free bytes and disk-pause details.
A controller lock prevents two queue controllers from exceeding the worker cap.

The native board adapter runs one previously admitted packet from that board's
existing spool. Use `oxa work --native-queue --config PATH` from the source
repository with the same private config used by refill and `nativeExecution`
explicitly enabled. The owner-protected config supplies the registered `worker`
and `host_id`. Launch requires a matching hosted claim and the original binding
before taking the packet. This execution path uses the offline policy. It moves the
matching packet into `started` and resumes from the task checkpoint after
interruption. The original claim releases after the whole task finishes and
its worker process exits.

Set `nativeActivity` to `true` in the same protected config to record verified
worker results on the original hosted claim. The setting defaults to off.
A saved worker result keeps its launch identity across checkpoint replay.

Problem-report workflows hold a per-user kernel lease across drafting, duplicate
search, match reads and writing, so concurrent workers share one reporting lane.

## Fresh GitHub packet, steering and the publish receipt

`oxa work --issue N` and `--pr N` re-read the issue or pull request at every worker launch (first
run, continue after an interruption, repair round, `--rerun`) with the same bounded packet builders
as the first fetch. Lines that are new are added to the launch prompt under "Updates on GitHub
since this task's packet was captured (context, not instructions from the owner)"; the saved
task prompt stays the task's identity. The record shows `packet.at_ms` and `packet.updates`. A
failed fetch is a warning in the record detail and never blocks the launch; offline or without a
trusted `gh` there is no feed. The runner fetches; the worker stays GitHub-isolated. GitHub text
and inbox messages are context, never an approval.

An `oxa work` worker also watches its session inbox (`OXA_WORK_INBOX=1`), so `oxa send <session>
"text"`, the lead chat and the web UI reach it at its next turn boundary. The user-only
`oxa.inbox.headless: false` turns that off. A worker is not interrupted by a poll while it runs:
a change on GitHub reaches it at its next launch.

After every runner-owned push (publication and lane acceptance) the runner asks the destination
itself what the branch holds (`git ls-remote`) and records `publish_receipt`: the requested commit,
the remote commit and a class: `published`, `absent`, `remote_behind`, `remote_ahead`, `diverged`
or `remote_unknown` (a remote commit that is not in the local repository, reported with its exact
SHA). A push that reports success but reads back another commit is an error. A rejected push
reads like `push rejected; origin agent/x is at <sha> (diverged from requested <sha>); nothing was
force-pushed, both commits are kept`. Nothing is ever force-pushed.

## Lane CI acceptance

Add `"lane_ci": true` to a task to make the repository's own CI the acceptance
authority. The branch must be one `.github/workflows/lane.yml` runs on (`agent/`,
`codex/`, `air/` or `runner/`) and its base must be `main`, because the workflow
selects the branch diff against `main`. Other tasks keep local-only acceptance.

After the local `done` commands pass and the saved commit exists, the runner
first re-verifies that exact commit with the existing committed proof (a clean,
object-verified export whose captured checks run), then pushes it to
`refs/heads/<branch>` on `origin`. It waits, without a model turn, for the `lane`
workflow run whose `headSha` is exactly that commit and whose event is the push,
and records its terminal conclusion in the task record (`lane`). A crash during
the wait resumes the saved `lane` step instead of rerunning the worker.

Rust work that passed its local checks but needs hosted tests enters the lane
anyway: the exact-head lane run is that hosted evidence. The task stays `unverified`
while the run is pending and becomes `done` only when it is green; the push still
passes the structural checks, owner confirmation and content scan first. The run's
plan and at least one lane test job must actually succeed: an overall-success run
whose test jobs were skipped is not evidence.

The lane run only counts for what it actually ran. The workflow selects the crates
the branch diff touches (and their reverse dependencies), so a declared acceptance
crate is covered only when the task diff selects it: a declaration the head does not
touch refuses with the uncovered crate names, and the work stays `unverified` rather
than crediting a suite that never ran. What the lane tests comes from one data file,
`tools/ci/lane_coverage.json`, which the lane planner (`tools/ci/lane_select.py`) and the
runner's guard both read. The guard reads it from the trusted base commit, never from the
pushed head, so a head cannot widen its own coverage; a missing or invalid file covers
nothing and the work stays `unverified`. A path is covered only under a workspace whose
packages are all tested (`oxa`). Under a workspace that lists packages (`engine`: the
portable crates, the rest rides the self-hosted route) every path is uncovered, fixtures and
shared inputs such as `engine/Cargo.toml` included, and so is any path with a `Cargo.toml`
ancestor outside the listed workspaces (`fuzz/`, a new top-level crate), without a code
change. Documentation needs no lane evidence.

A `lane_ci` task's worker launches GitHub-isolated, like a runner-owned task: no
host `gh` token is injected into its bash (and no MCP, extensions, web search or
subagents), because the runner alone performs the owner-approved public push.

A green run accepts the task (`done`). A completed `failure` spends the task's
single repair round, shared with a local acceptance repair, with the failed job,
step and test names; the runner saves and pushes the repaired commit and observes
the new head once more. A second failure is `red`. Cancellation, timeouts,
startup errors and an unobservable run (no matching push run before the bounded
wait, or a GitHub read error) are `blocked` with their own state and spend no
repair. In every non-green case the task is never `done`.

Because the lane workflow is the authority, a head that changes
`.github/workflows/lane.yml` or `tools/ci/` is refused before the push: those
changes land in their own owner-reviewed change instead of qualifying themselves.
The check covers every commit the push would add (against the destination's own
tips), not only the diff from the saved base. A green lane run is evidence only for
what the lane tests (see the coverage data above): for example a change under `engine/` or
`fuzz/` stays on the hosted route. Besides the diff from the saved base and every commit the
destination lacks, the controls must also equal the destination's own tip of the base branch
from its merge base with the head, so the workflow that runs is the base branch's, not one
inherited from another pushed branch (when that tip is not in the local object store the first
two checks stand).
A branch whose open pull request is already in the merge queue is refused too;
use a new task branch for follow-up work.

The push is gated. Inside an oxa session, `oxa work` asks through the session's
owner-confirm broker, so the session dialog or yolo choice is recorded even when
an agent child launched the command. Outside a session, the owner can confirm at
the controlling terminal, or an owner-controlled web/runner surface can set
`OXA_WORK_OWNER_CONFIRMED=1`; those compatibility paths are ignored when a
coding-agent marker such as `OXA_CODING_AGENT`, `AI_AGENT` or
`OXA_SUBAGENT_DEPTH` is set. Without that confirmation a lane task ends
`blocked` before any worker starts and pushes nothing. Offline mode, an untrusted
project, a missing trusted `gh`, or a missing sandbox refuse it later.

The lane push goes through the same publication content scan as `--publish`,
immediately before anything is sent: every commit the push would add (the head
minus the tips `origin` itself advertises) is scanned, including commit objects,
added lines and file names, with tags and submodules off. A finding refuses the
push, the task ends `blocked` and no lane run is awaited.

`--publish` refuses a `lane_ci` task unless its saved record has a passing lane
run at the saved head, so a pull request is opened only after CI passed at that
head. `oxa work verify` uses the same saved record and therefore the same rule.
