//! Lane CI acceptance is the gate: a local pass alone is never done.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};
use work::{git, run_observed_with, setup_with_files};

/// The lane coverage data every base commit of a lane task carries (`tools/ci/lane_coverage.json`,
/// read from the trusted base by the Rust lane guard).
const COVERAGE: &str = r#"{"workspaces":{"oxa":{"packages":"all"},"engine":{"packages":["server"]}},"contract":["tools/ci/lane_coverage.json"]}"#;

fn setup_lane(env: &TestEnv, done: &str) {
    setup_with_files(env, done, &[("tools/ci/lane_coverage.json", COVERAGE)]);
}

fn state_path(env: &TestEnv) -> PathBuf {
    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    env.agent_dir().join("work").join(key).join("test.json")
}

fn configure(env: &TestEnv) -> String {
    use std::os::unix::fs::PermissionsExt;
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","oxa":{"reportIssue":{"repo":"example/project"}}}"#,
    );
    let gh = env.write(
        "tmp/gh",
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$1 $2" in
  'repo view')
    printf '%s' '{"nameWithOwner":"example/project","url":"https://github.com/example/project"}' ;;
  'run list')
    head=$({ git rev-parse refs/heads/agent/test; })
    mode=failure
    if test -f "$HOME/lane-cancelled"; then mode=cancelled; fi
    if test -f "$HOME/lane-pass"; then mode=success; fi
    printf '[{"databaseId":7,"headSha":"%s","status":"completed","conclusion":"%s","event":"push"}]' "$head" "$mode" ;;
  'pr list') printf '[]' ;;
  'api graphql') printf '%s' '{"data":{"repository":{"pullRequest":{"mergeQueueEntry":null}}}}' ;;
  'run view')
    for arg in "$@"; do
      if test "$arg" = '--log-failed'; then
        printf 'FAIL [   0.004s] oxa-work::lane::tests::synthetic_case\n'
        exit 0
      fi
    done
    if test -f "$HOME/lane-pass"; then
      if test -f "$HOME/lane-skipped"; then
        printf '%s' '{"jobs":[{"name":"lane pr check","conclusion":"success"},{"name":"lane plan","conclusion":"skipped"},{"name":"lane (oxa, ubuntu-24.04)","conclusion":"skipped"}]}'
      else
        printf '%s' '{"jobs":[{"name":"lane pr check","conclusion":"success"},{"name":"lane plan","conclusion":"success"},{"name":"lane (oxa, ubuntu-24.04)","conclusion":"success"}]}'
      fi
    else
      printf '%s' '{"jobs":[{"name":"lane (oxa, ubuntu-24.04)","conclusion":"failure","steps":[{"name":"Affected crates","conclusion":"failure"}]}]}'
    fi ;;
  *) exit 7 ;;
esac
"##,
    );
    let git_wrapper = env.write(
        "tmp/git",
        "#!/bin/sh\nfor arg do if test \"$arg\" = push; then printf 'push\\n' >> \"$HOME/push-calls\"; fi; done\nexec /usr/bin/git \"$@\"\n",
    );
    let controller = env.write("tmp/oxc", "#!/bin/sh\nprintf '{}'\n");
    for file in [gh, git_wrapper, controller] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap())
}

fn lane_task(env: &TestEnv) {
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task","lane_ci":true}]"#,
    );
}

fn approve(command: &mut Command) {
    command.env("OXA_WORK_OWNER_CONFIRMED", "1");
}

fn run_lane_observed(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    path: String,
) -> (serde_json::Value, Vec<oxa_fake::Recorded>) {
    run_observed_with(env, turns, &["work", "../tasks.json"], Some(path), approve)
}

fn run_lane(env: &TestEnv, turns: Vec<ScriptTurn>, path: String) -> serde_json::Value {
    run_lane_observed(env, turns, path).0
}

fn edits() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"lane"}"#),
        ScriptTurn::text("done"),
    ]
}

fn repair_edits() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"lane"}"#),
        ScriptTurn::text("done"),
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"repaired"}"#),
        ScriptTurn::text("repaired"),
    ]
}

fn origin_head(env: &TestEnv) -> String {
    git(
        env,
        &env.work().join("remote.git"),
        &["rev-parse", "refs/heads/agent/test"],
    )
}

fn command(env: &TestEnv, path: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("PATH", path)
        .env("OXA_WORK_OWNER_CONFIRMED", "1")
        .output()
        .unwrap()
}

#[test]
fn lane_accepted_work_without_a_pr_receipt_is_reported_pending_not_done() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    run_lane(&env, edits(), path.clone());
    // Reading the saved result back composes what a caller sees: pending, not done, exit 3.
    let listed = command(&env, &path, &["work", "../tasks.json"]);
    assert_eq!(listed.status.code(), Some(3), "{listed:?}");
    let record: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(record["outcome"], "publication_pending", "{record}");
    // The lane run is the evidence behind the acceptance the pending receipt belongs to.
    assert_eq!(record["lane"]["conclusion"], "success", "{record}");
    assert_eq!(record["lane"]["head"], record["head_sha"], "{record}");
    let status = command(&env, &path, &["work", "status"]);
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(text.contains("pending publication, not done"), "{text}");
    assert!(text.contains("branch_without_pr_receipt"), "{text}");
}

#[test]
fn a_green_lane_run_at_the_pushed_head_owes_its_pr_receipt() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path);
    // The lane push delivered the exact head, but nothing opened its PR: accepted work with
    // publication still owed, never reported as done (#2304).
    assert_eq!(record["outcome"], "publication_pending", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("lane acceptance: publication pending: branch_without_pr_receipt ("),
        "{detail}"
    );
    assert_eq!(record["step"], "finished");
    assert_eq!(record["lane"]["conclusion"], "success", "{record}");
    assert_eq!(record["lane"]["head"], record["head_sha"], "{record}");
    assert_eq!(
        origin_head(&env),
        record["head_sha"].as_str().unwrap(),
        "the lane observed a different commit than the branch carries"
    );
    let push_calls = fs::read_to_string(env.home().join("push-calls")).unwrap();
    assert_eq!(push_calls.lines().count(), 1, "{push_calls}");
    let gh_calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(
        gh_calls.lines().any(|line| line.starts_with("run list")),
        "{gh_calls}"
    );
    assert!(
        !gh_calls
            .lines()
            .any(|line| line.starts_with("pr create") || line.starts_with("pr edit")),
        "acceptance opened a pull request: {gh_calls}"
    );
    let state = fs::read_to_string(state_path(&env)).unwrap();
    assert!(state.contains("\"lane\""), "{state}");
}

#[test]
fn configured_github_auth_is_isolated_and_refreshed_at_publication_boundaries() {
    use std::os::unix::fs::PermissionsExt;
    const CONFIRM_FIXTURE: &str = "OXA_AUTH_CONFIRM_FIXTURE";
    const TEST_NAME: &str = "configured_github_auth_is_isolated_and_refreshed_at_publication_boundaries";
    if let Some(storage) = std::env::var_os(CONFIRM_FIXTURE) {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let repo = std::env::current_dir().unwrap();
        let tools = repo.join("../../tmp").canonicalize().unwrap();
        let task = oxa_work::read_tasks(&repo.join("../tasks.json"))
            .unwrap()
            .remove(0);
        let policy = oxa_work::RunPolicy {
            git_binary: tools.join("git"),
            host_path: std::env::var_os("PATH").unwrap(),
            terminate_binary: Some("/usr/bin/kill".into()),
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            runner_owns_github: false,
            github_token_command: vec![tools.join("mint").to_str().unwrap().into()],
            github_token_write_paths: Vec::new(),
            autonomy: Default::default(),
            worker_github: Default::default(),
            authority_now: None,
            offline: false,
            storage: storage.into(),
        };
        let record = oxa_work::publish(
            &repo,
            std::path::Path::new(env!("CARGO_BIN_EXE_oxa")),
            &task,
            &policy,
            &tools.join("gh"),
            &|_: &oxa_work::Record| Ok(()),
            &|_| None,
            &mut |_| {
                fs::copy(home.join("mint-count"), home.join("generation-at-confirmation")).unwrap();
                Some(true)
            },
        )
        .unwrap();
        assert_eq!(record.outcome, oxa_work::Outcome::Done, "{record:?}");
        assert_eq!(
            record.publish_receipt.as_ref().map(|receipt| receipt.class),
            Some(oxa_work::Divergence::Published)
        );
        assert_eq!(
            record.pull_request.as_deref(),
            Some("https://github.com/example/project/pull/1")
        );
        return;
    }
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    let project = env.work().join("project");
    let remote = env.work().join("remote.git");
    let url = "https://github.com/example/project.git";
    git(&env, &project, &["remote", "set-url", "--push", "origin", url]);
    git(&env, &project, &["config", "credential.helper", "!exit 42"]);
    let source = env.write(
        "tmp/mint",
        r#"#!/bin/sh
set -eu
test "$(pwd -P)" = /
test "$PATH" = /usr/bin:/bin
n=0
if test -f "$HOME/mint-count"; then read -r n < "$HOME/mint-count"; fi
n=$((n + 1))
printf '%s\n' "$n" > "$HOME/mint-count"
printf 'synthetic-lane-%s\n' "$n"
"#,
    );
    env.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"defaultProjectTrust":"always","oxa":{"github":{"tokenCommand":[source]}}})
            .to_string(),
    );
    let git_shim = env.write(
        "tmp/git",
        &format!(r#"#!/bin/sh
set -eu
home={home:?}
remote={remote:?}
for arg do
    case "$arg" in
      ls-remote|push)
        op=$arg
        test "${{GIT_CONFIG_KEY_0:-}}" = 'http.https://github.com/.extraHeader'
        test "${{GH_TOKEN+x}}" != x
        test "$GIT_ALLOW_PROTOCOL" = https
        read -r n < "$home/mint-count"
        if test -f "$home/generation-at-confirmation"; then
            read -r confirmed < "$home/generation-at-confirmation"
            test "$n" -gt "$confirmed"
        fi
        encoded=$(printf 'x-access-token:synthetic-lane-%s' "$n" | /usr/bin/base64 | tr -d '\n')
        test "$GIT_CONFIG_VALUE_0" = "Authorization: Basic $encoded"
        if test "$op" = ls-remote; then
            test "$PWD" = /
            shift $(( $# - 2 ))
            if test "$1" = --refs; then
                test "$2" = {url}
                exec env -u GIT_ALLOW_PROTOCOL -u GIT_CONFIG_COUNT /usr/bin/git ls-remote --heads --tags --refs "$remote"
            fi
            test "$1" = {url}
            exec env -u GIT_ALLOW_PROTOCOL -u GIT_CONFIG_COUNT /usr/bin/git ls-remote "$remote" "$2"
        fi
        test -f config
        if /usr/bin/git config --local --get credential.helper; then exit 42; fi
        shift $(( $# - 2 ))
        test "$1" = {url}
        printf '%s\n' "$n" >> "$home/push-generations"
        exec env -u GIT_ALLOW_PROTOCOL -u GIT_CONFIG_COUNT /usr/bin/git push "$remote" "$2"
        ;;
    esac
done
exec /usr/bin/git "$@"
"#, home = env.home()),
    );
    let gh = env.write(
        "tmp/gh",
        &format!(r##"#!/bin/sh
set -eu
remote={remote:?}
read -r n < "$HOME/mint-count"
test "$GH_TOKEN" = "synthetic-lane-$n"
test "$GH_HOST" = github.com
case "$1 $2" in
  'repo view') printf '%s' '{{"nameWithOwner":"example/project","url":"https://github.com/example/project"}}' ;;
  'pr list') printf '[]' ;;
  'run list')
    head=$(/usr/bin/git --git-dir="$remote" rev-parse refs/heads/agent/test)
    if test -f "$HOME/last-poll"; then
        read -r earlier < "$HOME/last-poll"
        test "$n" -gt "$earlier"
        status=completed
        conclusion=success
    else
        read -r earlier < "$HOME/push-generations"
        test "$n" -gt "$earlier"
        printf '%s\n' "$n" > "$HOME/last-poll"
        status=in_progress
        conclusion=
    fi
    printf '[{{"databaseId":7,"headSha":"%s","status":"%s","conclusion":"%s","event":"push"}}]' "$head" "$status" "$conclusion" ;;
  'run view') printf '%s' '{{"jobs":[{{"name":"lane plan","conclusion":"success"}},{{"name":"lane (oxa, ubuntu-24.04)","conclusion":"success"}}]}}' ;;
  'pr create')
    read -r confirmed < "$HOME/generation-at-confirmation"
    test "$n" -gt "$confirmed"
    printf '%s' 'https://github.com/example/project/pull/1' ;;
  *) exit 7 ;;
esac
"##),
    );
    for exe in [source, git_shim, gh] {
        fs::set_permissions(exe, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let record = run_lane(&env, edits(), path.clone());
    // Green lane evidence at the pushed head; publication still owes its PR receipt (#2304).
    assert_eq!(record["outcome"], "publication_pending", "{record}");
    assert_eq!(record["lane"]["conclusion"], "success", "{record}");
    assert_eq!(origin_head(&env), record["head_sha"].as_str().unwrap());
    let push_count = || {
        fs::read_to_string(env.home().join("push-generations"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(push_count(), 1);

    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .current_dir(&project)
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("PATH", path)
        .env(CONFIRM_FIXTURE, state_path(&env).parent().unwrap())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(push_count(), 2);
}

#[test]
fn a_green_workflow_with_skipped_test_jobs_is_not_lane_evidence() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    env.write("home/lane-skipped", "");
    let record = run_lane(&env, edits(), path);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("plan or test jobs were skipped"),
        "{record}"
    );
}

#[test]
fn a_local_pass_with_a_failed_lane_run_is_red_after_one_repair() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    // No "lane-pass" marker: every lane observation fails.
    let record = run_lane(&env, repair_edits(), path);
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "red", "{record}");
    assert_eq!(record["lane_repairs"], 1, "{record}");
    assert_eq!(record["lane"]["conclusion"], "failure", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(
        detail.contains("oxa-work::lane::tests::synthetic_case"),
        "the repair was not told which test failed: {detail}"
    );
    assert_eq!(
        origin_head(&env),
        record["head_sha"].as_str().unwrap(),
        "the branch did not carry the repaired commit"
    );
    let push_calls = fs::read_to_string(env.home().join("push-calls")).unwrap();
    assert_eq!(push_calls.lines().count(), 2, "{push_calls}");
}

#[test]
fn a_head_that_changes_any_push_workflow_is_refused_before_any_push() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    // The base already carries the lane runner, so the worker edits a tracked
    // control rather than creating a new path the write tool may not create.
    env.write(
        "work/project/.github/workflows/macos.yml",
        "name: trusted\non: push\n",
    );
    git(&env, &env.work().join("project"), &["add", "."]);
    git(
        &env,
        &env.work().join("project"),
        &["commit", "-m", "lane runner fixture"],
    );
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let (record, _) = run_lane_observed(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                r#"{"path":".github/workflows/macos.yml","content":"name: tampered\non: push\n"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        path,
    );
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("refuses a head that changes GitHub workflows"),
        "{record}"
    );
    assert!(
        !env.home().join("push-calls").exists(),
        "a head that rewrites the lane gate was pushed"
    );
    assert!(
        !env.home().join("gh-calls").exists(),
        "a refused head reached GitHub"
    );
}

#[test]
fn a_local_repair_consumes_the_single_lane_repair_allowance() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["grep -Fx repaired answer.txt"]"#);
    lane_task(&env);
    let path = configure(&env);
    // No "lane-pass": the lane run fails after the local repair passes.
    let (record, _) = run_lane_observed(&env, repair_edits(), path);
    assert_eq!(record["outcome"], "red", "{record}");
    assert_eq!(record["lane_repairs"], 1, "{record}");
    assert_eq!(
        record["task_tokens"]["attempts"], 2,
        "a second repair worker followed a spent local repair: {record}"
    );
}

#[test]
fn a_cancelled_lane_run_is_blocked_without_spending_the_repair() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-cancelled", "");
    let (record, _) = run_lane_observed(&env, edits(), path);
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert_eq!(record["lane"]["conclusion"], "cancelled", "{record}");
    assert_eq!(record["lane_repairs"], 0, "{record}");
    assert_eq!(
        record["task_tokens"]["attempts"], 1,
        "an infrastructure result started a repair worker: {record}"
    );
    assert!(
        record["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("infrastructure result"),
        "{record}"
    );
}

#[test]
fn publication_refuses_a_lane_task_without_green_evidence() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path.clone());
    // Green lane evidence, PR receipt still owed by the authorised publication path (#2304).
    assert_eq!(record["outcome"], "publication_pending", "{record}");
    // Remove the lane observation from the saved record: a saved local pass is
    // not publication evidence for a lane task.
    let path_to_state = state_path(&env);
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path_to_state).unwrap()).unwrap();
    state["record"].as_object_mut().unwrap().remove("lane");
    fs::write(&path_to_state, serde_json::to_vec(&state).unwrap()).unwrap();
    let pushes_before = fs::read_to_string(env.home().join("push-calls"))
        .unwrap()
        .lines()
        .count();
    let out = command(&env, &path, &["work", "../tasks.json", "--publish"]);
    let published: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(published["outcome"], "blocked", "{published}");
    assert!(
        published["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("lane CI"),
        "{published}"
    );
    let pushes_after = fs::read_to_string(env.home().join("push-calls"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        pushes_after, pushes_before,
        "publication pushed without lane evidence"
    );
    let gh_calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(
        !gh_calls.lines().any(|line| line.starts_with("pr create")),
        "{gh_calls}"
    );
}

#[test]
fn an_agent_marker_voids_the_ambient_owner_confirmation() {
    for marker in ["OXA_CODING_AGENT", "AI_AGENT", "OXA_SUBAGENT_DEPTH"] {
        let env = TestEnv::new();
        setup_lane(&env, r#"["true"]"#);
        lane_task(&env);
        let path = configure(&env);
        let server = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["work", "../tasks.json"])
            .current_dir(env.work().join("project"))
            .env_clear()
            .envs(env.envs(&server.url()))
            .env("PATH", path)
            .env("OXA_WORK_OWNER_CONFIRMED", "1")
            .env(marker, "1")
            .output()
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|error| panic!("{marker}: {error}: {}", String::from_utf8_lossy(&out.stdout)));
        assert_eq!(record["outcome"], "blocked", "{marker}: {record}");
        assert!(
            record["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("push confirmation"),
            "{marker}: {record}"
        );
    }
}

#[test]
fn an_agent_process_can_use_the_owner_confirm_broker() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    // A broker in the test harness would be rejected: it must be in the actual oxa
    // ancestor, or pinned outside the sandbox. Let a real session launch the agent child.
    let worker = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
    let record_path = env.tmp().join("broker-lane-record.json");
    // Host bash can clear XDG and test settings. Restore the isolated fixture environment
    // without replacing the broker socket or agent markers exported by the session.
    let child_env = env
        .envs(&worker.url())
        .into_iter()
        .filter(|(key, _)| key != "PATH")
        .chain(std::iter::once(("PATH".into(), path.clone())))
        .map(|(key, value)| format!("{key}={}", oxa_proto::sh_quote(&value)))
        .collect::<Vec<_>>()
        .join(" ");
    let command = serde_json::json!({"command": format!(
        "{child_env} exec {} work ../tasks.json > {}",
        oxa_proto::sh_quote(env!("CARGO_BIN_EXE_oxa")),
        oxa_proto::sh_quote(&record_path.to_string_lossy()),
    )});
    let owner = oxa_fake::FakeServer::start(
        vec![
            ScriptTurn::tool("lane", "bash", &command.to_string()),
            ScriptTurn::text("done"),
        ],
        oxa_fake::FakeOpts::default(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--yolo", "--no-session", "--mode", "json", "Run the lane task"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&owner.url()))
        .env("PATH", path)
        .output()
        .unwrap();
    let events = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{events}\n{stderr}");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&record_path).unwrap_or_default())
        .unwrap_or_else(|error| panic!("{error}: {events}\n{stderr}"));
    assert_eq!(
        record["outcome"], "publication_pending",
        "{record}\n{events}\n{stderr}"
    );
    assert_eq!(
        events.matches("owner confirm yes (yolo)").count(),
        1,
        "{events}\n{stderr}"
    );
}

#[test]
fn a_queue_launch_approves_lane_tasks_from_the_owner_environment() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    // A lane task arrives through the queue directory, not a task file: the queue
    // path never reaches the interactive ask, so only the owner-controlled
    // environment can approve its pushes.
    env.write(
        "work/queue/lane.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task","lane_ci":true}]"#,
    );
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let server = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .args([
            "work",
            "--queue",
            "../queue",
            "--max",
            "1",
            "--min-free-mib",
            "0",
            "--once",
        ])
        .current_dir(env.work().join("project"))
        .envs(env.envs(&server.url()))
        .env("PATH", path)
        .env("OXA_WORK_OWNER_CONFIRMED", "1");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output().unwrap());
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("queue finished");
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let task = events
        .iter()
        .find(|row| row["event"] == "task")
        .expect("one task event");
    assert_eq!(
        task["outcome"], "publication_pending",
        "the queue's approved lane task should be accepted with its PR receipt owed: {task}"
    );
    assert!(
        !task["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("push confirmation"),
        "the queue's lane task was refused for a missing push confirmation: {task}"
    );
    assert!(
        !server.requests().is_empty(),
        "the approved lane task started its worker and reached the model"
    );
}

#[test]
fn a_lane_task_without_owner_push_approval_runs_its_local_checks_but_is_never_done() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    let server = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", path)
        .env("OXA_CODING_AGENT", "1")
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("push confirmation"),
        "{record}"
    );
    assert!(
        server.requests().is_empty(),
        "an unapproved lane task started a worker"
    );
    assert!(
        !env.home().join("push-calls").exists(),
        "an unapproved lane task pushed"
    );
}

#[test]
fn a_lane_task_only_accepts_lane_triggering_branches() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"feature/test","model":"fake","prompt":"Finish task","lane_ci":true}]"#,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("OXA_CODING_AGENT", "1")
        .output()
        .unwrap();
    // An invalid lane task is refused while reading the task file, before any
    // worktree or worker exists.
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("lane workflow runs on"), "{stderr}");
    assert!(!env.work().join(".oxa-worktrees").exists());
}

#[test]
fn lane_task_with_a_fake_token_is_refused_before_push_and_no_lane_run_is_awaited() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    // Built at run time so this source never carries a token-shaped literal.
    let token = format!("ghp_{}", "x".repeat(36));
    let content = format!("fn main() {{}}\\nconst T: &str = \\\"{token}\\\";\\n");
    let record = run_lane(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                &format!(r#"{{"path":"oxa/src/client.rs","content":"{content}"}}"#),
            ),
            ScriptTurn::text("done"),
        ],
        path,
    );
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("publishing refused"), "{record}");
    assert!(
        !record.to_string().contains(&token),
        "the refusal echoed the secret"
    );
    assert!(
        !env.home().join("push-calls").exists(),
        "a commit with a token was pushed"
    );
    let remote = env.work().join("remote.git");
    let refs = git(&env, &remote, &["for-each-ref", "refs/heads/agent"]);
    assert!(refs.trim().is_empty(), "origin received the branch: {refs}");
    let gh_calls = fs::read_to_string(env.home().join("gh-calls")).unwrap_or_default();
    assert!(
        !gh_calls
            .lines()
            .any(|line| line.starts_with("run list") || line.starts_with("run view")),
        "a lane run was awaited: {gh_calls}"
    );
}

#[test]
fn rust_unverified_work_collects_lane_evidence_and_becomes_done_on_green() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                r#"{"path":"oxa/src/client.rs","content":"fn main() {}\n"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        path,
    );
    assert_eq!(record["outcome"], "publication_pending", "{record}");
    assert_eq!(record["lane"]["conclusion"], "success", "{record}");
    assert_eq!(record["lane"]["head"], record["head_sha"], "{record}");
    assert_eq!(origin_head(&env), record["head_sha"].as_str().unwrap());
    let push_calls = fs::read_to_string(env.home().join("push-calls")).unwrap();
    assert_eq!(push_calls.lines().count(), 1, "{push_calls}");
}

fn assert_refused_before_any_push(env: &TestEnv, record: &serde_json::Value, expect: &str) {
    assert_ne!(record["outcome"], "done", "{record}");
    let detail = record["detail"].as_str().unwrap_or_default();
    assert!(detail.contains(expect), "{record}");
    assert!(
        !env.home().join("push-calls").exists(),
        "a refused head was pushed"
    );
}

/// #2105: the lane never runs the engine suites, so a green run is no evidence for an engine
/// fixture change (the coverage data lists only some engine packages).
#[test]
fn a_lane_green_run_is_no_evidence_for_an_engine_fixture_change() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                r#"{"path":"engine/tests/fixtures/f.txt","content":"x\n"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        path,
    );
    assert_refused_before_any_push(&env, &record, "does not test engine/tests/fixtures/f.txt");
}

/// #2106: `fuzz/` has no lane evidence either.
#[test]
fn a_lane_green_run_is_no_evidence_for_a_fuzz_change() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    env.write(
        "work/project/fuzz/oxa/Cargo.toml",
        "[package]\nname = \"fuzz-oxa\"\n",
    );
    git(&env, &env.work().join("project"), &["add", "."]);
    git(&env, &env.work().join("project"), &["commit", "-m", "fuzz crate"]);
    git(&env, &env.work().join("project"), &["push", "origin", "main"]);
    let record = run_lane(
        &env,
        vec![
            ScriptTurn::tool(
                "write",
                "write",
                r#"{"path":"fuzz/oxa/lib.rs","content":"fn f() {}\n"}"#,
            ),
            ScriptTurn::text("done"),
        ],
        path,
    );
    // The crate's own local compile check may stop it first; either way it is never done and
    // nothing is pushed (the rule itself is covered in oxa-work lane/coverage.rs).
    assert_refused_before_any_push(&env, &record, "");
}

/// #2104: a local base that is ahead of origin with a `tools/ci` commit is refused: that commit
/// is part of what the push would add.
#[test]
fn a_local_base_ahead_of_origin_with_a_ci_commit_is_refused_before_any_push() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    env.write("work/project/tools/ci/extra.py", "print('tampered')\n");
    git(&env, &env.work().join("project"), &["add", "."]);
    git(
        &env,
        &env.work().join("project"),
        &["commit", "-m", "unpushed ci change"],
    );
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path);
    assert_refused_before_any_push(&env, &record, "refuses a head that changes GitHub workflows");
}

/// #2102: workflow files that differ from the base branch's own history are refused even when
/// no unpushed commit touches them (here another pushed branch already carries the change).
#[test]
fn a_head_with_workflows_inherited_from_another_pushed_branch_is_refused() {
    let env = TestEnv::new();
    setup_lane(&env, r#"["true"]"#);
    let project = env.work().join("project");
    git(&env, &project, &["checkout", "-b", "other"]);
    env.write(
        "work/project/.github/workflows/x.yml",
        "name: inherited\non: push\n",
    );
    git(&env, &project, &["add", "."]);
    git(&env, &project, &["commit", "-m", "workflow on another branch"]);
    git(&env, &project, &["push", "origin", "other"]);
    git(&env, &project, &["checkout", "main"]);
    git(&env, &project, &["merge", "--ff-only", "other"]);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path);
    assert_refused_before_any_push(&env, &record, "differ from the base branch's");
}
