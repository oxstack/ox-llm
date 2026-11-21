# Lead web access scaffold

Run `oxa lead serve` to serve the read-only board after enabling it in user settings.
`OXA_LEAD_BOARD_LOG` selects an absolute private board log; refused input stays unavailable.
The owner must read the final authentication composition before merge or live exposure.
Chat remains separate work. The one owner-decision mutation is the approval answer below.

The home page's attention block reports the five stuck-work signals from the board
log's protected record: main required-check failures, no verified main merge for 45
minutes, over-age merge-queue membership at the exact PR head, workflow runs queued
over 15 minutes, and unanswered owner decisions. Each source is rendered from its own
observation: a source the record does not carry is shown as explicitly unavailable,
never as healthy, and a routine healthy read renders no attention block at all. With
`owner_chat` enabled, open owner asks are additionally read live from the authorized
durable Lead Session on every render, with their original revisions, through the
registered owner-bound caller; an unauthorized or absent session leaves that source
unavailable rather than quietly empty.

Optional user setting `oxa.lead.serve.fixed_prompts` defaults to false.
It enables no-JS, same-origin/CSRF-protected fixed questions at zero model tokens.
Private situation data needs browser proof separate from tool UIDs; absent inputs stay unavailable.

## Attach to a running session

With `owner_chat` on, the chat page can show and talk to a session that is running now (for
example the lead you started in tmux) instead of a dedicated Lead session. Find the session id
in the `id` field of the first line of its file under the sessions directory (the file name ends with it), then set the user-only
`oxa.lead.attachSessionId` to it and restart the service. The service then needs no dedicated
`oxa.lead.sessionId`; if both are set, attach wins.

The `/chat` page shows the newest part of the session's transcript (user and assistant text, each
tool call with a short masked argument preview, short result previews) and whether the session is
running. It refreshes like the other pages, and the page says when older history is not shown.
The message form sends exactly like `oxa send <id> <text>`: the text goes into the session's
inbox as a steer: a busy session reads it after its running tool calls, an idle one starts a
turn with it, also if it is not running yet. The service takes
no session lock, opens no provider and spends no model tokens. A message is text for the
model, never an approval: answer approvals on the approvals page. The same identity, origin and
CSRF checks as before apply, the session file must stay private to your user, and a refused send
is written to the service journal with stage `recheck`, reason `binding` or `inbox`.

### The list of running sessions

With `owner_chat` on, the chat page also lists every session of your user that is running
now: the lead TUI, an interactive `oxa`, a worker. Each row shows the first characters of the
id, the project directory's name (never the full path), the time of its last activity and a
**View** button. Choosing one attaches the page to it until the service restarts: the same
live transcript and the same message form as above (a message goes into that session's inbox,
like `oxa send`). The attached view also shows whether the session is running and working or
idle (from its newest message) and the prompt tokens of its newest request when it recorded
usage; the session does not record the model's window, so no share of the window is shown.

"Running" means that the process named in the session's lock file is alive; the page never
takes the lock, so looking cannot make a starting `oxa` see its session as locked. After a
crash the recycled process id of an unrelated process can briefly make a dead session look
running. A session that is not running cannot be chosen, and a message to one that stopped
waits in its inbox until the session runs again.

`oxa.lead.attachSessionId` stays the start-up default. When it is unset the page starts on the
dedicated Lead chat, or, with no dedicated session either, on the list alone. The **Back**
button returns to that start-up target. Everything the page shows here is visible only to the
verified owner on `/chat`; this includes the transcript of your other running sessions and
workers, masked and escaped as described above. A choice is not a setting: restart the service
and it starts on the default again.

## Waiting for your approval

The verified owner can see "Waiting for your approval" on the board and home pages.
The panel shows each pending request's ID, session, directory basename, command,
rule reasons and age. The page masks credentials in the command and rule reasons.

Viewing and answering require owner chat. Enable the user-only
`oxa.lead.serve.owner_chat` with Tailscale and exactly one allowed identity.
Each answer is a
same-origin, CSRF-checked form POST to `/approvals` with the fields `id` and
`answer` (`once`, `session`, `always` or `deny`), then a redirect to `/`.

An answer is equivalent to `oxa send <session> --approve <id> <answer>`: the same
approval desk write, the same user-only save of `always` keys to
`oxa.approval.always` (never the project's settings), and the same informational
note to the session. An unknown or already answered request returns 404 and
changes nothing. A refused answer is written to the service journal like a
refused chat request, with stage `approval`.

Only the user settings file may configure `oxa.lead.serve`; a project's settings
cannot enable it or choose its listener or identity policy. It is disabled by
default. The default identity mode is `local`, the bind is `127.0.0.1`, and the
default port is `8743`. There are no deployment-specific port or identity rules.

Local mode requires independently verified local OS ownership, not merely a
loopback address, and a per-start client token. `oxa lead serve` prints
`http://127.0.0.1:<port>/?t=<token>` once; opening it sets an `oxa_lead` cookie
(HttpOnly, SameSite=Strict) and redirects to the same path without the query. Tools may send
the token as `Authorization: Bearer`; it is also in `<agent dir>/lead-client-token` (0600),
replaced on every start. A request without it gets 401. Tailscale mode is unchanged. Tailscale mode requires an authenticated request-bound proxy
identity, its matching forwarded login, and an exact `allowed_identities` match.
Local browser authority includes the port and must match the configured numeric bind. Proxy
proof must bind the independently authenticated expected authority to the request;
never copy an untrusted Host header into that proof. An untrusted
`Tailscale-User-Login` header and loopback peer alone prove nothing.
Only a loopback `bind` is accepted: a wider address is a configuration error, and
the access check refuses any request whose peer is not loopback.
No provider is enabled implicitly.

The explicit proxy adapter uses user-only `proxy.uid` and `proxy.authority` fields
inside `oxa.lead.serve`. Authority is configured independently of HTTP headers;
the accepted loopback stream's kernel owner must match the configured UID before
its login header can supply identity. Trust applies to every process with that
UID, so choose a principal dedicated to trusted proxy workloads. The kernel fallback
UID cannot identify a proxy principal: it can represent unmapped owners.
This adapter is Linux-only; other platforms refuse before binding. Missing trust also refuses.
Configuration does not install a listener or deploy a proxy.

Consumer integration must call `proxy::pre_bind` before binding, then
`proxy::verify` on the actual accepted stream and this request's single login
header. Reject duplicate/ambiguous identity and Host headers. Pass the resulting
proof, unchanged login and raw request authority to the existing `read_only`
boundary. A verification error refuses; it never permits a raw-header fallback.
The owner must read this producer before any consumer listener or exposure.

Configure the user layer for your own identity and ports, leaving it off until ready:

```json
{"oxa":{"lead":{"serve":{"enabled":false,"identity":"tailscale",
  "bind":"127.0.0.1","port":8743,
  "allowed_identities":["YOUR_LOGIN_IDENTITY"],
  "proxy":{"uid":12345,"authority":"lead.example.invalid:9448"}}}}}
```

The proxy UID and external authority above are synthetic examples. Replace them
with your trusted proxy principal and the exact authority the browser will send.

Operator guidance for Tailscale Serve (not an oxa command), using ports you selected:

```text
tailscale serve --bg --https=<tailnet-port> http://127.0.0.1:<local-port>
```

Choose a dedicated tailnet port whose ACL excludes shared or guest devices. This
is an operator step, not an action performed by oxa. Never assume the header is
trusted simply because Serve exists. Do not expose the backend directly or bypass
the verified proxy adapter. No account, tailnet, service or ACL is hard-coded.

The service writes chat refusals (4xx/503) to standard error. A user systemd
deployment can read them with `journalctl --user -u oxa-lead.service`. Each
line contains the response status, a fixed stage and reason, and a suggested
remedy. It omits the owner identity, message, token counts, URLs, headers,
credentials and CSRF values.

## One observation, live workers and live refresh

Every page is rendered from the same observation the Lead loop uses: the protected
Board sample (board, alarms, GitHub App quota, thread holds and the quota deferrals the
owner's reserve recorded) and the runner situation named by `OXA_LEAD_SITUATION`, which
the pages did not read before. The server no longer makes its own partial read.

- **Workers** shows the Lead session's live workers first: name, lane (branch), model,
  state and how old the worker's status file is (a Dead worker, a Stopped worker and a
  stale status show as such), then the runner situation rows. The parent is the session
  of the Lead owner binding, never a session list. No workers recorded and no runner
  observation render the "Lane status unavailable" text; a corrupt registry renders
  "Live workers refused" and is left untouched.
- **Usage** adds a line for each optional GitHub read the reserve deferred
  (`oxa.lead.github_reserve`), saying whether it waits for a window reset or for a first
  observation. Model plan windows still render only from an observation that reached the
  page; the provider-header retention that would feed them is not part of this version.
- **Live refresh.** With the route installed (always for `oxa lead serve`) the document
  advertises it and the page opens `GET /events/live`, behind the same authorization as
  every other path. The server sends `retry: 5000` and a `lead-update` event at once and
  again only when a fingerprint of the observation changes (board time, situation
  sequence, ask revisions, worker states), polled once a second; a `: keep-alive`
  comment goes out every 15 seconds and the stream ends after five minutes so the
  client reconnects. The event carries the SHA-256 of the serving executable, so a
  changed digest after an update reloads the page. Streams use the existing
  16-connection pool and per-write deadline. The one-shot `/events` route and the
  polling fallback remain for listeners without the route.

## Synthetic fixture frames

Six synthetic frames under `oxa/oxa-cli/tests/fixtures/lead/` drive the real service in
`tests/lead_serve.rs`: `healthy`, `needs_you`, `alarm`, `stale`, `partial` and `first_run`.
Each file holds the native wire formats — one complete Reader sample record (board, panels,
landing, queued runs, App quota, deferrals) and one runner situation document — with
synthetic identities only. Timestamps sit at a synthetic reference epoch; the test rebases
them so the frame renders the age it declares (`age_secs`, `situation_age_secs`), which
keeps the files deterministic while the freshness states stay real.

The tests assert behaviour through the real routes: a healthy frame renders no attention
block; open decisions render as a request, not an alarm; an alarm names every firing
source; a stale board keeps its rows and offers a retry; a partial sample keeps its known
sources and names each gap with its cause; a first run renders explicit missing states and
never a fabricated row; a malformed replacement degrades explicitly and a fresh sample
recovers. The hosted screenshot job proposed for #2349 renders these same frames at phone
and desktop widths in light and dark, so every `lead_page/` change is reviewed by looking
at it.
