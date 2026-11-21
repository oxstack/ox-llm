//! A later native board attempt continues the branch that a finished attempt left checked out.
#[path = "common/work_repo.rs"]
mod work_repo;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use work_repo::{git, setup};

const CHILD: &str = "OXA_NATIVE_HANDOFF_TEST_ROOT";
const BRANCH: &str = "agent/native";
const PR_BRANCH: &str = "agent/reviewed";

/// One native admission receipt: each lease generation has its own task id.
fn admission(
    ordinal: u32,
    number: u64,
    branch: &str,
    reviewed: Option<&str>,
) -> oxa_work::board::NativeBinding {
    let lease = oxa_work::board::Lease {
        holder: "codex:node/slot-1".into(),
        expires_at: 2_000_000_000,
        claimed_at: Some(1_000_000_000),
        generation: (20_000, ordinal),
    };
    let key = "a".repeat(64);
    match reviewed {
        Some(head) => oxa_work::board::NativeBinding::new_reviewed(number, &key, &lease, branch, head),
        None => oxa_work::board::NativeBinding::new(number, &key, &lease, branch),
    }
    .unwrap()
}

/// The admitted task; its typed reviewed source is the binding's pull request and head.
fn admitted_task(binding: &oxa_work::board::NativeBinding, base: &str) -> oxa_work::Task {
    let plain =
        oxa_work::board::NativeBinding::new(binding.number, &binding.key, &binding.lease, &binding.branch)
            .unwrap();
    let task = serde_json::from_value(serde_json::json!({
        "id": format!("i{}", binding.number), "model": "fake", "branch": binding.branch,
        "base": base, "prompt": "Synthetic native task"
    }))
    .unwrap();
    let mut task = oxa_work::native_task(&plain, task).unwrap();
    task.reviewed_pull_request = binding
        .reviewed_head
        .as_ref()
        .map(|head| oxa_work::ReviewedPullRequest {
            number: binding.number,
            head: head.clone(),
        });
    task
}

fn policy(root: &Path) -> oxa_work::RunPolicy {
    oxa_work::RunPolicy {
        git_binary: "/usr/bin/git".into(),
        host_path: std::env::var_os("PATH").unwrap(),
        terminate_binary: Some("/usr/bin/kill".into()),
        gh_binary: None,
        push_approved: false,
        content_scan: None,
        runner_owns_github: false,
        github_token_command: Vec::new(),
        github_token_write_paths: Vec::new(),
        autonomy: Default::default(),
        worker_github: Default::default(),
        authority_now: None,
        offline: false,
        storage: root.join("home/.oxa/agent/work/native-handoff"),
    }
}

fn read(project: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .current_dir(project)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Run through the native path, which persists the admission receipt with the attempt.
fn run_native(
    project: &Path,
    policy: &oxa_work::RunPolicy,
    binding: &oxa_work::board::NativeBinding,
    base: &str,
) -> oxa_work::Record {
    let binary = Path::new(env!("CARGO_BIN_EXE_oxa"));
    let task = admitted_task(binding, base);
    let mut locked = oxa_work::LockedTask::acquire(&task, policy).unwrap();
    let plan = locked.plan_native(project, false, binding).unwrap();
    locked
        .run_with_notes(
            project,
            binary,
            &plan,
            false,
            &|_: &oxa_work::Record| Ok(()),
            &|_| None,
        )
        .unwrap()
}

/// An ordinary task file entry that copies a native attempt's ID shape and prompt.
fn run_forged(project: &Path, policy: &oxa_work::RunPolicy, task: oxa_work::Task) -> oxa_work::Record {
    let binary = Path::new(env!("CARGO_BIN_EXE_oxa"));
    let plan = oxa_work::plan_task(project, task, policy).unwrap();
    oxa_work::run_task_with_policy(project, binary, &plan, policy).unwrap()
}

fn attached(worktree: &Path) -> String {
    read(worktree, &["symbolic-ref", "--quiet", "HEAD"])
}

fn branch_attempts(root: PathBuf) {
    let project = root.join("work/project");
    let policy = policy(&root);
    let mut records = vec![];
    for ordinal in [1, 2] {
        let record = run_native(&project, &policy, &admission(ordinal, 7, BRANCH, None), "main");
        // Locally verified, with publication held for the runner: never reported done (#2304).
        assert_eq!(
            record.outcome,
            oxa_work::Outcome::PublicationPending,
            "{record:?}"
        );
        records.push(record);
    }
    assert_ne!(records[0].id, records[1].id);
    assert_ne!(records[0].worktree, records[1].worktree);
    assert_eq!(
        read(&project, &["show", &format!("{BRANCH}:first.txt")]),
        "first attempt"
    );
    assert_eq!(
        read(&project, &["show", &format!("{BRANCH}:second.txt")]),
        "second attempt"
    );
    // The finished attempt keeps its files; only its HEAD was detached.
    assert!(records[0].worktree.join("first.txt").exists());

    // A task without an admission receipt cannot take over a native attempt's branch.
    let mut forged = admitted_task(&admission(9, 7, BRANCH, None), "main");
    forged.id = format!("board-{}-20000-9", "a".repeat(64));
    let refused = run_forged(&project, &policy, forged);
    assert_eq!(refused.outcome, oxa_work::Outcome::Blocked, "{refused:?}");
    assert_eq!(attached(&records[1].worktree), format!("refs/heads/{BRANCH}"));
}

fn reviewed_attempts(root: PathBuf) {
    let project = root.join("work/project");
    let policy = policy(&root);
    let base = read(&project, &["rev-parse", "main"]);
    let tip = |project: &Path| read(project, &["rev-parse", &format!("refs/heads/{PR_BRANCH}")]);
    let first_head = tip(&project);
    let first = run_native(
        &project,
        &policy,
        &admission(1, 42, PR_BRANCH, Some(&first_head)),
        &base,
    );
    assert_eq!(first.outcome, oxa_work::Outcome::PublicationPending, "{first:?}");
    // The reviewed head moves with the saved work; the pull request number does not.
    let second_head = tip(&project);
    assert_ne!(second_head, first_head);
    let second = run_native(
        &project,
        &policy,
        &admission(2, 42, PR_BRANCH, Some(&second_head)),
        &base,
    );
    assert_eq!(
        second.outcome,
        oxa_work::Outcome::PublicationPending,
        "{second:?}"
    );
    assert_ne!(first.id, second.id);
    assert_ne!(first.worktree, second.worktree);
    assert!(first.worktree.join("first.txt").exists());
    let refused = "outside a finished task for this pull request";
    let other_head = tip(&project);
    let other = run_native(
        &project,
        &policy,
        &admission(3, 43, PR_BRANCH, Some(&other_head)),
        &base,
    );
    assert_eq!(other.outcome, oxa_work::Outcome::Blocked, "{other:?}");
    assert!(other.detail.contains(refused), "{other:?}");
    // The same pull request without an admission receipt is refused as well.
    let mut forged = admitted_task(&admission(4, 42, PR_BRANCH, Some(&other_head)), &base);
    forged.id = format!("board-{}-20000-8", "a".repeat(64));
    let forged = run_forged(&project, &policy, forged);
    assert_eq!(forged.outcome, oxa_work::Outcome::Blocked, "{forged:?}");
    assert!(forged.detail.contains(refused), "{forged:?}");
    assert_eq!(tip(&project), other_head);
    assert_eq!(attached(&second.worktree), format!("refs/heads/{PR_BRANCH}"));
}

fn turns() -> Vec<ScriptTurn> {
    let write = |path: &str, content: &str| {
        let args = serde_json::json!({"path": path, "content": content}).to_string();
        [
            ScriptTurn::tool("write", "write", &args),
            ScriptTurn::text("done"),
        ]
    };
    let mut turns = write("first.txt", "first attempt").to_vec();
    turns.extend(write("second.txt", "second attempt"));
    turns
}

fn in_child(test: &str, env: &TestEnv) {
    let server = FakeServer::start(turns(), FakeOpts::default()).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env_clear()
        .envs(env.envs(&server.url()))
        .env(CHILD, env.root())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_later_native_attempt_continues_the_branch_of_a_finished_attempt() {
    if let Some(root) = std::env::var_os(CHILD) {
        branch_attempts(PathBuf::from(root));
        return;
    }
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    in_child(
        "a_later_native_attempt_continues_the_branch_of_a_finished_attempt",
        &env,
    );
}

#[test]
fn native_attempts_on_one_reviewed_pull_request_hand_over_its_branch() {
    if let Some(root) = std::env::var_os(CHILD) {
        reviewed_attempts(PathBuf::from(root));
        return;
    }
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let project = env.work().join("project");
    git(&env, &project, &["checkout", "-q", "-b", PR_BRANCH]);
    env.write("work/project/reviewed.txt", "pull request change\n");
    git(&env, &project, &["add", "reviewed.txt"]);
    git(&env, &project, &["commit", "-q", "-m", "pull request change"]);
    git(&env, &project, &["checkout", "-q", "main"]);
    in_child(
        "native_attempts_on_one_reviewed_pull_request_hand_over_its_branch",
        &env,
    );
}
