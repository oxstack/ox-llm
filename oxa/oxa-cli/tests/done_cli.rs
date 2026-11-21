//! The `done` setting through the real program path (`oxa -p`): the exit code when the checks
//! still fail after the repair rounds, and the notice when an untrusted project's `done` is
//! not run. Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const NOTICE: &str = "done checks in .oxa/settings.json were not run: project is not trusted";
const FAILED: &str = "done checks failed";

/// Run `oxa <args>` in-process against a fake engine with `turns`; returns (code, stdout, stderr).
fn go(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str]) -> (i32, String, String) {
    go_env(env, turns, args, &[])
}

fn go_env(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    extra: &[(&str, &str)],
) -> (i32, String, String) {
    let server = FakeServer::start(turns, FakeOpts::default()).expect("fake engine");
    let mut owned = env.envs(&server.url());
    owned.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let cwd = env.work();
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let (out, err) = (Sink::default(), Sink::default());
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let text = |s: &Sink| String::from_utf8_lossy(&s.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text(&out), text(&err)));
    });
    rx.recv_timeout(Duration::from_secs(120)).expect("oxa finished")
}

fn trust_always(env: &TestEnv) {
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust": "always"}"#,
    );
}

fn project_done(env: &TestEnv, list: &str) {
    env.write("work/.oxa/settings.json", &format!(r#"{{"done": {list}}}"#));
}

fn write_turn(id: &str, path: &str) -> ScriptTurn {
    ScriptTurn::tool(
        id,
        "write",
        &serde_json::json!({"path": path, "content": "x"}).to_string(),
    )
}

fn edits_then_stops(rounds: usize) -> Vec<ScriptTurn> {
    let mut turns = Vec::new();
    for i in 0..rounds {
        turns.push(write_turn(&format!("c{i}"), &format!("f{i}.txt")));
        turns.push(ScriptTurn::text("finished"));
    }
    turns
}

#[test]
fn print_exits_nonzero_when_done_checks_still_fail() {
    let env = TestEnv::new();
    trust_always(&env);
    project_done(&env, r#"["exit 1"]"#);
    let (code, out, err) = go(&env, edits_then_stops(3), &["-p", "go"]);
    assert_eq!(code, 1, "stdout: {out}\nstderr: {err}");
    assert_eq!(
        err.lines().filter(|l| l.starts_with(FAILED)).count(),
        1,
        "one summary line: {err}"
    );
    assert!(err.contains("FAILED (exit 1)"), "{err}");
    assert!(out.contains("finished"), "the reply is still printed: {out}");
}

#[test]
fn print_exits_zero_when_done_checks_pass() {
    let env = TestEnv::new();
    trust_always(&env);
    project_done(&env, r#"["true"]"#);
    let (code, _, err) = go(&env, edits_then_stops(1), &["-p", "go"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains(FAILED), "{err}");
}

/// A fresh install trusts its project (`defaultProjectTrust` ships as `always`): a one-line
/// `oxa -p` in a clone runs the project's `done` checks and never fails on trust.
#[test]
fn fresh_default_runs_project_done_checks() {
    let env = TestEnv::new();
    project_done(&env, r#"["touch fresh-check-ran"]"#);
    let (code, _, err) = go(&env, edits_then_stops(1), &["-p", "go"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains(NOTICE), "a fresh install asks nothing: {err}");
    assert!(
        env.work().join("fresh-check-ran").exists(),
        "the project's done checks ran: {err}"
    );
}

#[test]
fn ask_opt_in_keeps_the_untrusted_done_refusal() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust": "ask"}"#,
    );
    project_done(&env, r#"["touch untrusted-check-ran"]"#);
    let (code, _, err) = go(&env, edits_then_stops(1), &["-p", "go"]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(err.matches(NOTICE).count(), 1, "{err}");
    assert!(!env.work().join("untrusted-check-ran").exists());
}

#[test]
fn untrusted_project_done_fails_with_an_owner_approval_route() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust": "never"}"#,
    );
    project_done(&env, r#"["touch untrusted-check-ran"]"#);
    for args in [["-p", "go"], ["--mode=json", "go"]] {
        let (code, _, err) = go(&env, edits_then_stops(1), &args);
        assert_eq!(code, 1, "{err}");
        assert_eq!(err.matches(NOTICE).count(), 1, "{err}");
        assert!(
            err.contains(&env.work().display().to_string()) && err.contains("--approve"),
            "{err}"
        );
        assert!(!env.work().join("untrusted-check-ran").exists());
    }
}

#[test]
fn no_notice_when_trusted_empty_or_absent() {
    let env = TestEnv::new();
    trust_always(&env);
    project_done(&env, r#"["true"]"#);
    let (_, _, err) = go(&env, edits_then_stops(1), &["-p", "go"]);
    assert!(!err.contains(NOTICE), "trusted: {err}");

    let env = TestEnv::new();
    project_done(&env, "[]");
    let (_, _, err) = go(&env, vec![ScriptTurn::text("hi")], &["-p", "go"]);
    assert!(!err.contains(NOTICE), "empty list: {err}");

    let env = TestEnv::new();
    let (_, _, err) = go(&env, vec![ScriptTurn::text("hi")], &["-p", "go"]);
    assert!(!err.contains(NOTICE), "no settings file: {err}");
}

#[test]
fn trust_project_env_runs_done_checks_without_a_saved_decision() {
    let env = TestEnv::new();
    project_done(&env, r#"["exit 1"]"#);
    let (code, _, err) = go_env(
        &env,
        edits_then_stops(3),
        &["-p", "go"],
        &[("OXA_TRUST_PROJECT", "1")],
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(FAILED) && !err.contains(NOTICE), "{err}");
}

#[test]
fn trust_project_env_does_not_override_no_approve() {
    let env = TestEnv::new();
    project_done(&env, r#"["exit 1"]"#);
    let (code, _, err) = go_env(
        &env,
        edits_then_stops(1),
        &["-p", "-na", "go"],
        &[("OXA_TRUST_PROJECT", "1")],
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(NOTICE), "{err}");
}
