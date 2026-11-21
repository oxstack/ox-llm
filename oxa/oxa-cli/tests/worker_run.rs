//! Exercise the owner CLI, real Git worktree and supervisor against a loopback model.
#![cfg(target_os = "linux")]

use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Instant;

macro_rules! log_sample {
    ($($arg:tt)*) => { writeln!(std::io::stderr(), $($arg)*).unwrap() };
}

fn oxa(env: &TestEnv, url: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.env_clear()
        .envs(env.envs(url))
        .current_dir(env.work())
        .args(args);
    cmd
}

fn git(env: &TestEnv, args: &[&str]) {
    let result = Command::new("git")
        .env_clear()
        .envs(env.envs(""))
        .current_dir(env.work())
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn records(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn sample_rss(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap_or("unavailable")
        .trim()
        .to_owned()
}

#[test]
fn owner_launch_lists_and_resumes_the_same_worktree_and_goal() {
    let env = TestEnv::new();
    git(&env, &["init", "-q"]);
    env.write("work/tracked.txt", "synthetic repository\n");
    git(&env, &["add", "tracked.txt"]);
    git(
        &env,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    );
    let settings = |mode| {
        serde_json::json!({
        "permissions": {"defaultMode": mode},
        "oxa": {"sandbox": {"unattended": false}, "workers": {"models": {"kinds": {"build": ["fake-model"]}}}},
    }).to_string()
    };
    env.write("home/.oxa/agent/settings.json", &settings("yolo"));
    let first = FakeServer::start(
        vec![ScriptTurn::fault(Fault::HoldHeaders { ms: 8000 })],
        FakeOpts::default(),
    )
    .unwrap();
    let args = [
        "--offline",
        "worker",
        "run",
        "--kind",
        "build",
        "--job",
        "k41",
        "--goal-file",
        "-",
    ];
    let at = Instant::now();
    let mut parent = oxa(&env, &first.url(), &args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    parent
        .stdin
        .take()
        .unwrap()
        .write_all(b"Finish the synthetic worker goal.")
        .unwrap();
    let mut stream = BufReader::new(parent.stdout.take().unwrap());
    let mut line = String::new();
    assert_ne!(
        stream.read_line(&mut line).unwrap(),
        0,
        "{}",
        String::from_utf8_lossy(&parent.wait_with_output().unwrap().stderr)
    );
    let started: Value = serde_json::from_str(&line).unwrap();
    let startup = at.elapsed();
    assert_eq!(started["type"], "started");
    assert_eq!(started["branch"], "agent/k41");
    assert_eq!(started["model"], "fake-model");
    assert_eq!(started["why"], "policy: kind build");
    let pid = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    let tree = env.root().join(".oxa-worktrees/work/k41");
    assert_eq!(
        std::fs::read(tree.join("tracked.txt")).unwrap(),
        b"synthetic repository\n"
    );
    let goal = env.agent_dir().join("workers/.kanban/k41.goal.md");
    assert_eq!(
        std::fs::metadata(&goal).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let load = oxa(
        &env,
        &first.url(),
        &["worker", "list", "--parent", "kanban", "--json"],
    )
    .output()
    .unwrap();
    assert!(load.status.success());
    let list = &records(&load.stdout)[0];
    assert_eq!(list["live"], 1);
    assert_eq!(list["workers"][0]["session_id"], started["session_id"]);
    assert!(list["load_1m"].as_f64().is_some());
    assert!(list["free_memory_bytes"].as_u64().is_some());
    // The model phase proves the goal journal is already installed before the crash.
    loop {
        line.clear();
        assert_ne!(
            stream.read_line(&mut line).unwrap(),
            0,
            "worker exited before contacting the model"
        );
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["phase"] == "model" {
            break;
        }
    }
    log_sample!(
        "worker startup {} ms; launcher RSS {}; worker RSS {}; host load {}; free bytes {}",
        startup.as_millis(),
        sample_rss(parent.id()),
        sample_rss(pid),
        list["load_1m"],
        list["free_memory_bytes"]
    );
    // SAFETY: pid came from our private launcher and has not exited (the mock holds headers).
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    line.clear();
    std::io::Read::read_to_string(&mut stream, &mut line).unwrap();
    assert_eq!(parent.wait().unwrap().code(), Some(137));
    let tail = records(line.as_bytes());
    assert_eq!(tail.last().unwrap()["type"], "finished");
    assert_eq!(tail.last().unwrap()["exit_code"], 137);
    // Resume must neither reread the brief nor silently switch worktrees or widen authority.
    std::fs::remove_file(goal).unwrap();
    env.write("home/.oxa/agent/settings.json", &settings("standard"));
    let next = FakeServer::start(
        vec![ScriptTurn::tool(
            "done",
            "goal_done",
            r#"{"status":"complete","evidence":"Resumed the saved goal."}"#,
        )],
        FakeOpts::default(),
    )
    .unwrap();
    let at = Instant::now();
    let completed = oxa(&env, &next.url(), &["worker", "run", "--resume", "--job", "k41"])
        .output()
        .unwrap();
    assert_eq!(
        completed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    let events = records(&completed.stdout);
    assert_eq!(events[0]["session_id"], started["session_id"]);
    assert_eq!(events[0]["branch"], started["branch"]);
    assert_eq!(events[0]["model"], started["model"]);
    assert_eq!(events[0]["resumed"], true);
    assert_eq!(events[0]["offline"], true);
    assert_eq!(events[0]["mode"], "workspace");
    assert_eq!(events.last().unwrap()["outcome"], "done");
    assert!(next.requests()[0]
        .system
        .contains("Finish the synthetic worker goal."));
    log_sample!("worker resume through completion {} ms", at.elapsed().as_millis());
}

#[test]
fn agent_marker_is_refused_before_loading_the_goal_or_creating_a_registry() {
    let env = TestEnv::new();
    let output = oxa(
        &env,
        "http://127.0.0.1:1",
        &["worker", "run", "--job", "k41", "--goal-file", "absent"],
    )
    .env("OXA_WORKER_DEPTH", "1")
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside agent tools"));
    assert!(!env.agent_dir().join("workers").exists());
}
