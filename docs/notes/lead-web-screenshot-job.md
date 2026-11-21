# Lead web S10: hosted screenshot job — proposal for the workflow owner

Status: proposal only. The fixture frames and their behaviour tests land separately
(Refs #2349); this document specifies the dev-only CI job so the owner of
`.github/workflows/` and the required gate (Claude) can apply it without redesign.
It follows the plan #2339 §4 ("Offscreen screenshots in CI") and the existing workflow
constraints: pinned actions, least privilege, no credentials with fork code, GitHub-hosted
runners only. No gate or required-check change is proposed here; the job is dev-only and
adds nothing to the required path.

## What already exists (the trusted preview path)

- `build.yml`'s `oxa-test-build` job builds the real binaries (`cargo build --profile ci
  -p oxa-cli -p oxa-fake`) and archives every workspace test with nextest. The archive
  includes the `oxa` binary the integration tests drive through `CARGO_BIN_EXE_oxa`; it is
  uploaded as `oxa-test-archive-x86` (retention 1 day). The S1 PR #2354 already renders its
  "after" screenshots from this hosted test-archive binary — the same trusted artifact this
  job uses. No local compile, no second build.
- The six native synthetic frames under `oxa/oxa-cli/tests/fixtures/lead/`
  (`healthy`, `needs_you`, `alarm`, `stale`, `partial`, `first_run`) and the rebase helper in
  `tests/lead_serve.rs` (`shift_timestamps`, `with_frame`). The job renders these same
  frames; it never invents data and never touches real owner observations.
- `tools/ci/apt_install.sh` for any package the runner image lacks. The ubuntu-24.04
  hosted image ships `google-chrome-stable`; verify in the first run and prefer it over a
  snap-transitional `chromium-browser` deb.

## Job shape

New dev-only workflow (e.g. `lead-shots.yml`) or a job in `checks.yml` outside the `gate`
aggregation — the workflow owner's choice. Suggested trigger:

```yaml
on:
  pull_request:
    paths:
      - "oxa/oxa-cli/src/lead_page/**"
      - "oxa/oxa-cli/src/lead_page.rs"
      - "oxa/oxa-cli/tests/fixtures/lead/**"
      - ".github/workflows/lead-shots.yml"
  workflow_dispatch:
permissions:
  contents: read
concurrency:
  group: lead-shots-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true
```

Runs on draft PRs too (screenshots are a development aid; the job is cheap and dev-only).
`pull_request`, never `pull_request_target`: fork code runs exactly as the existing test
shards run it — no secrets, read-only token, no credentials in the environment. The job
needs the `oxa-test-build` artifact of the same run, so it `needs: build` (or downloads
`oxa-test-archive-x86` from the current run via `actions/download-artifact`); on a fork PR
the archive is built from the PR merge commit by the existing no-secret pipeline.

Pinned actions, copied from the current `checks.yml`/`build.yml` pins:

- `actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1` (`persist-credentials: false`)
- `actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8`
- `actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7.0.2`

Runner: `ubuntu-24.04` (x64, matches the archive), `timeout-minutes: 15`.

## Steps (inline only — no new helper script files)

1. Checkout the PR head; record `GITHUB_SHA`.
2. Download `oxa-test-archive-x86` from this run; extract the archive
   (`tar --zstd -xf`, nextest archive layout) and locate the `oxa` binary. Record
   `oxa --version` and `sha256sum oxa` in `$GITHUB_STEP_SUMMARY` next to the head SHA:
   every screenshot is labelled with the exact head and binary it came from.
3. Materialize the six frames into `$RUNNER_TEMP/lead-shots/inputs` (dir 0700, files
   0600) with an inline Python step that mirrors `shift_timestamps` in
   `tests/lead_serve.rs`: shift only the native timestamp fields
   (`observed_at`, `expires_at`, `claimed_at`, `enqueued_at`, `resets_at`, `until`,
   `oldest_at` in seconds; `at_ms`, `merged_at_ms`, `started_ms` in milliseconds;
   `mergedAt`, `updated_at` as whole-second UTC ISO) so each frame renders its declared
   `age_secs` / `situation_age_secs` old at capture time. The board file is one JSONL line
   (`board_record` + `\n`); the situation is the `situation` document as-is; `first_run`
   writes no board file. This is CI-only glue, not a product path; the Rust helper stays
   the source of truth for the field list. (If a single materializer is preferred later,
   a dev-only `oxa lead serve --fixture <name>` behind the command registry is the clean
   follow-up — not part of this job.)
4. For each frame: write an isolated `HOME` with
   `{"oxa":{"lead":{"serve":{"enabled":true,"port":<reserved>}}}}` (reserve a free port
   with a Python socket bind to 0, then release it), start
   `OXA_LEAD_BOARD_LOG=… OXA_LEAD_SITUATION=… oxa lead serve` in the background with
   stdout drained, wait for the `listening` line, and read the printed client-token URL.
   Local mode mints the owner client token; the browser exchanges it once per frame
   (`/?t=<token>` → 303 + `oxa_lead` cookie) and reuses one `--user-data-dir` per frame so
   the cookie persists across that frame's captures. The token never reaches the summary,
   the artifact or a log line (mask it; the service journal already keeps it out).
5. Capture with the preinstalled headless Chrome, one PNG per
   page × viewport × theme × frame:
   - pages: `/`, `/board`, `/workers`, `/usage`, `/chat` (5)
   - viewports: `--window-size=390,844` (phone) and `--window-size=1280,800` (desktop)
   - themes: light (default) and dark (`--force-dark-mode`, which sets
     `prefers-color-scheme: dark`; the page's own system-dark CSS path renders it — after
     S1 the explicit toggle follows the system until picked, so this stays the real dark
     path, not a fabricated one)
   - `google-chrome --headless=new --disable-gpu --no-sandbox --hide-scrollbars
     --user-data-dir=<frame-profile> --window-size=<w>,<h> [--force-dark-mode]
     --virtual-time-budget=2000 --screenshot=<out.png> <url>`
   - 5 × 2 × 2 × 6 = 120 PNGs, named `<frame>/<page>-<viewport>-<theme>.png`.
   `--no-sandbox` is the throwaway CI container's standard headless flag (the runner has
   no usable user namespaces for Chrome's sandbox); it grants nothing beyond the job.
6. Verify, don't fabricate: every PNG exists and is non-empty; decode the top-left pixel
   of one light/dark pair per frame with inline Python stdlib (zlib/struct PNG decode) and
   assert the background differs — a dark capture that rendered light fails the job. No
   pixel-diff gate against a baseline (flaky, and a pin test the plan rejects).
7. Upload `lead-shots-<pr>-<head7>` (retention 7 days) and write a summary table: frame ×
   page rows with phone/desktop and light/dark artifact links, the head SHA, the binary
   version + sha256, the capture wall time and the runner queue delay (the capacity rule
   in AGENTS.md). Step-summary markdown cannot embed private artifact images inline; the
   table links them. If the owner wants inline images in a PR comment instead, that needs
   `pull-requests: write` and a comment step — an explicit owner choice, not the default.
8. Kill the server, remove the isolated HOME and inputs (they hold only synthetic data,
   but the token file must not linger in an artifact).

## What gates

- The job itself is dev-only: it never joins the required `gate`. A red screenshot job is
  a visible review signal, not a merge block, until the owner decides otherwise.
- Behaviour gates are the fixture tests in `tests/lead_serve.rs` (this PR): rows survive
  staleness, every gap is explicit, an absent source never reads healthy or alarming, no
  fabricated data. They run in the existing required test shards.
- Contrast gate (plan §4): a unit test that parses the `:root` token block and checks every
  text/background pair the components use at ≥ 4.5:1 in both themes. It needs S1's tokens
  (#2354) on main; it lands with or right after S1, in the same required shards. Spec:
  WCAG relative luminance from the sRGB tokens, pairs enumerated from the component rules
  (ink/bg, ink2/surface, muted/surface, on-accent/accent, ok/warn/bad text on their soft
  pairs, badge text on bad), both `data-theme` overrides.
- Budget gates (plan §4) land with S9 (#2348): Home ≤ 20 KB gzipped, CSS ≤ 16 KB raw /
  5 KB gz, JS ≤ 8 KB raw / 3 KB gz, `frame` event ≤ 256 bytes. Until then the job asserts
  only capture presence and the light/dark difference.

## Cost

Estimate before the first run: 120 captures × ~2 s (Chrome start + 2 s virtual time
budget) ≈ 4–5 min, plus one archive extract and six server starts; one x64 hosted runner,
≤ 15 min timeout. The first run records the measured wall time and queue delay in its
summary; this proposal's cost is **pending measurement** until that run exists. The job
adds no minutes to the required path and no macOS minutes.

## Constraints honoured

- Pinned action SHAs, `permissions: contents: read`, GitHub-hosted runners only, no
  `pull_request_target`, no secrets, fork code runs credential-free exactly like the test
  shards.
- No new helper script files: every step is inline in the workflow; existing helpers
  (`apt_install.sh`) are reused if needed.
- Exact binary/head: the hosted test-archive binary of the same run, labelled in the
  summary; no local build, no fabricated or hand-edited screenshot.
- Synthetic data only: the six committed frames; no real owner observation, token or path
  reaches an artifact or the summary.
- No new refusal, prompt or cap: the job observes and reports; the only hard failures are
  a missing capture and a dark frame that rendered light.
- Gate and workflow edits belong to Claude; this document and the fixture PR change no
  `.github/` file.
