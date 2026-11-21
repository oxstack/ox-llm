//! Issue #104: the `done` setting. oxa runs the project's done commands itself when a run
//! that edited files is about to finish. Scripted engine, isolated `TestEnv`, real sandbox-less
//! shell in the work folder.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, AgentEvent, CancelToken, CommandSpec, EntryBody, EventSink, ExecResult, ExecSpec, Exposure,
    HeadlessUi, HookPoint, HookReply, Hooks, Level, OxaError, Sandbox, Source, Tool, ToolClass, ToolCtx,
    ToolOutput, ToolSpec, Ui,
};
use oxa_resources::Resources;
use oxa_session::MemStore;
use serde_json::json;
use std::sync::Arc;

/// Runs the command directly in the work folder (the real sandbox needs bwrap).
struct Direct;
impl Sandbox for Direct {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

#[derive(Default)]
struct Rec(Vec<AgentEvent>);
impl EventSink for Rec {
    fn emit(&mut self, e: &AgentEvent) {
        self.0.push(e.clone());
    }
}
impl Rec {
    fn checks(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { text, .. } if text.starts_with("done check:") => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

fn run(env: &TestEnv, done: serde_json::Value, turns: Vec<ScriptTurn>) -> (Rec, Arc<ScriptedEngine>) {
    run_with(env, done, turns, Arc::new(Direct))
}

fn run_with(
    env: &TestEnv,
    done: serde_json::Value,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>) {
    let (rec, engine, _) = run_state(env, done, turns, sandbox);
    (rec, engine)
}

fn run_state(
    env: &TestEnv,
    done: serde_json::Value,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    let settings = if done.is_null() {
        Settings::default()
    } else {
        Settings::from_values(json!({ "done": done }), json!({}))
    };
    run_settings(env, settings, turns, sandbox)
}

fn run_settings(
    env: &TestEnv,
    settings: Settings,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    run_tools(env, settings, turns, sandbox, Vec::new())
}

fn run_tools(
    env: &TestEnv,
    settings: Settings,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
    extra: Vec<Arc<dyn Tool>>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    let (mut s, engine) = test_session(env, settings, turns, sandbox, extra);
    let mut rec = Rec::default();
    s.prompt("go".into(), Vec::new(), &mut rec, &mut HeadlessUi::default())
        .expect("prompt");
    let failed = s.inner().stats.done_failed;
    (rec, engine, failed)
}

fn test_session(
    env: &TestEnv,
    settings: Settings,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
    extra: Vec<Arc<dyn Tool>>,
) -> (Session, Arc<ScriptedEngine>) {
    let engine = Arc::new(ScriptedEngine::new(turns));
    let tools = oxa_tools::builtin_tools();
    let hooks: Vec<Arc<dyn Hooks>> = if extra.is_empty() {
        Vec::new()
    } else {
        vec![Arc::new(ExtTools(extra))]
    };
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools,
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox,
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    (Session::open(deps, Open::New).expect("open"), engine)
}

fn write(id: &str, path: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "write", &json!({"path": path, "content": "x"}).to_string())
}

const CHECK: &str = "test -f fixed.txt || { echo MISSING-FIXED; exit 3; }";

#[test]
fn failing_check_feeds_model_then_passes() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!([CHECK]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("really done"),
        ],
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert!(
        checks[0].contains("FAILED (exit 3)") && checks[0].contains("MISSING-FIXED"),
        "{checks:?}"
    );
    assert!(checks[1].ends_with("ok"), "{checks:?}");
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 4);
    let last_user = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .unwrap();
    assert!(last_user.content.as_ref().unwrap().contains("MISSING-FIXED"));
    assert_eq!(engine.remaining(), 0);
}

#[test]
fn passing_check_adds_no_round() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["true"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn no_edits_no_checks() {
    let env = TestEnv::new();
    let (rec, engine) = run(&env, json!(["exit 1"]), vec![ScriptTurn::text("nothing to do")]);
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 1);
}

#[test]
fn off_by_default() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        serde_json::Value::Null,
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn failing_forever_stops_after_two_rounds() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "b.txt"),
            ScriptTurn::text("done"),
            write("c3", "c.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
    );
    assert_eq!(rec.checks().len(), 3, "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 6);
    assert_eq!(engine.remaining(), 1);
}

#[test]
fn failed_check_without_new_edit_is_not_rerun() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot fix"),
        ],
    );
    assert_eq!(rec.checks().len(), 1);
    assert_eq!(engine.requests().len(), 3);
}

/// Records every command and cancels the token while the first one runs.
struct CancelDuringFirst(std::sync::Mutex<Vec<String>>);
impl Sandbox for CancelDuringFirst {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.0.lock().unwrap().push(s.command.clone());
        c.cancel();
        ExecResult {
            exit: None,
            ..Default::default()
        }
    }
}

#[test]
fn cancel_during_first_check_stops_done_phase() {
    let env = TestEnv::new();
    let spy = Arc::new(CancelDuringFirst(Default::default()));
    let (rec, engine) = run_with(
        &env,
        json!(["echo first", "echo second"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
        spy.clone(),
    );
    let ran = spy.0.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(ran[0].contains("first"), "{ran:?}");
    assert!(rec.checks().is_empty(), "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(engine.remaining(), 1);
}

/// Records every `ExecSpec` it receives, then runs it directly.
struct Recording {
    specs: std::sync::Mutex<Vec<ExecSpec>>,
}
impl Sandbox for Recording {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.specs.lock().unwrap().push(s.clone());
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

/// Recording `run` that first swaps the validated child folder for a symlink to an outside
/// directory, then delegates to Direct. Used as the deterministic child-replacement negative
/// control.
struct SwapChild {
    specs: std::sync::Mutex<Vec<ExecSpec>>,
    work: std::path::PathBuf,
    outside: std::path::PathBuf,
}
impl Sandbox for SwapChild {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        // Between validation and use, re-point `sub` at an outside directory.
        std::fs::remove_dir_all(self.work.join("sub")).expect("remove sub");
        std::os::unix::fs::symlink(&self.outside, self.work.join("sub")).expect("swap sub");
        self.specs.lock().unwrap().push(s.clone());
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

/// Runs one `done` entry after a `write`; returns the check notices.
fn one(env: &TestEnv, entry: &str) -> Vec<String> {
    let (rec, _) = run(
        env,
        json!([entry]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    rec.checks()
}

#[test]
fn in_dir_runs_the_command_in_that_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub/deep")).unwrap();
    std::fs::write(env.work().join("sub/marker.txt"), "x").unwrap();
    // The marker exists in `sub` only, so the check passes only with cwd = sub.
    assert_eq!(
        one(&env, "in sub: test -f marker.txt"),
        vec!["done check: [sub] test -f marker.txt ok".to_string()]
    );
    // `./sub/deep` style and an absolute path inside the session folder work too.
    assert_eq!(
        one(&env, "in ./sub/deep/..: test -f marker.txt")[0],
        "done check: [./sub/deep/..] test -f marker.txt ok"
    );
    let abs = env.work().join("sub");
    let entry = format!("in {}: test -f marker.txt", abs.display());
    assert!(one(&env, &entry)[0].ends_with("ok"), "{:?}", one(&env, &entry));
    // Without the prefix the command still runs in the session folder.
    let plain = one(&env, "test -f marker.txt");
    assert!(plain[0].contains("FAILED (exit 1)"), "{plain:?}");
}

#[test]
fn in_dir_failure_names_the_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    let c = one(&env, "in sub: echo BOOM; exit 4");
    assert!(
        c[0].starts_with("done check: [sub] echo BOOM; exit 4 FAILED (exit 4)") && c[0].contains("BOOM"),
        "{c:?}"
    );
}

#[test]
fn in_dir_refuses_missing_and_escaping_folders() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    // A sibling of the work folder exists, so a refusal is about the escape, not about it missing.
    std::fs::create_dir_all(env.root().join("outside")).unwrap();
    let marker = env.root().join("outside/ran.txt");
    let outside_abs = env.root().join("outside");
    let cases = [
        ("in nope: touch ran.txt".to_string(), "does not exist"),
        (
            "in ../outside: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        (
            "in sub/../../outside: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        (
            format!("in {}: touch ran.txt", outside_abs.display()),
            "outside the project folder",
        ),
        (
            "in ../nope: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        ("in : touch ran.txt".to_string(), "no folder"),
    ];
    for (entry, reason) in cases {
        let c = one(&env, &entry);
        assert_eq!(c.len(), 1, "{entry}: {c:?}");
        assert!(
            c[0].contains("FAILED (not run)") && c[0].contains(reason),
            "{entry}: {c:?}"
        );
    }
    assert!(!marker.exists(), "a refused check must not run");
    // A symlink that leaves the session folder is refused as well.
    std::os::unix::fs::symlink(env.root().join("outside"), env.work().join("link")).unwrap();
    let c = one(&env, "in link: touch ran.txt");
    assert!(
        c[0].contains("FAILED (not run)") && c[0].contains("outside"),
        "{c:?}"
    );
    assert!(!marker.exists());
    // A file is not a folder.
    std::fs::write(env.work().join("file.txt"), "x").unwrap();
    let c = one(&env, "in file.txt: true");
    assert!(
        c[0].contains("FAILED (not run)") && c[0].contains("not a folder"),
        "{c:?}"
    );
}

#[test]
fn refused_folder_goes_back_to_the_model() {
    let env = TestEnv::new();
    let (_, engine) = run(
        &env,
        json!(["in nope: true"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot"),
        ],
    );
    let reqs = engine.requests();
    let last_user = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .unwrap();
    assert!(last_user.content.as_ref().unwrap().contains("does not exist"));
}

#[test]
fn done_failed_flag_tracks_the_last_check_round() {
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "b.txt"),
            ScriptTurn::text("done"),
            write("c3", "c.txt"),
            ScriptTurn::text("done"),
        ],
        Arc::new(Direct),
    );
    assert!(failed, "two repair rounds used up and the check still fails");

    // Fixed within the rounds: not failed.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!([CHECK]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("done"),
        ],
        Arc::new(Direct),
    );
    assert!(!failed);

    // The model stops without fixing it (no new edit, so no re-run): still failed at the end.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot fix"),
        ],
        Arc::new(Direct),
    );
    assert!(failed);

    // No edits, no checks, no flag.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![ScriptTurn::text("hi")],
        Arc::new(Direct),
    );
    assert!(!failed);
}

#[test]
fn in_dir_recording_sandbox_cwd_stays_at_work_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub/deep")).unwrap();
    std::fs::write(env.work().join("sub/marker.txt"), "x").unwrap();
    let recording = Arc::new(Recording {
        specs: std::sync::Mutex::new(Vec::new()),
    });
    let (rec, _) = run_with(
        &env,
        json!(["in sub: test -f marker.txt"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        recording.clone(),
    );
    assert_eq!(
        rec.checks(),
        vec!["done check: [sub] test -f marker.txt ok".to_string()]
    );
    let specs = recording.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    // The sandbox authority stays at the original work folder, never the child.
    assert_eq!(s.cwd, env.work().canonicalize().unwrap());
    assert!(s.writable);
    // The command goes through the workspace-relative cd, not an absolute child path.
    assert!(
        s.command.starts_with("cd -- 'sub' || exit 111\n"),
        "command: {}",
        s.command
    );
}

#[test]
fn child_local_shell_prefix_runs_in_the_child_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    // Exists only in the child: the prefix fails unless it starts there.
    std::fs::write(env.work().join("sub/activate.sh"), "touch prefix-ran\n").unwrap();
    let settings = Settings::from_values(
        json!({
            "done": ["in sub: test -f prefix-ran"],
            "shellCommandPrefix": ". ./activate.sh",
        }),
        json!({}),
    );
    let (rec, _, failed) = run_settings(
        &env,
        settings,
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        Arc::new(Direct),
    );
    assert_eq!(
        rec.checks(),
        vec!["done check: [sub] test -f prefix-ran ok".to_string()]
    );
    assert!(!failed);
    assert!(env.work().join("sub/prefix-ran").exists());
}

#[test]
fn swapped_subdir_symlink_cannot_move_the_bind_root() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    std::fs::create_dir_all(env.root().join("outside")).unwrap();
    let recording = Arc::new(SwapChild {
        specs: std::sync::Mutex::new(Vec::new()),
        work: env.work(),
        outside: env.root().join("outside"),
    });
    // After validation and just before the command runs, `run` replaces the child with a
    // symlink that leaves the workspace. Validation was on the real folder, so it is accepted.
    // The command's exit code is not deterministic under Direct (the `cd` may land outside
    // through the swapped symlink), so we assert only cwd and the command text.
    let (_rec, _) = run_with(
        &env,
        json!(["in sub: touch ran.txt"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        recording.clone(),
    );
    let specs = recording.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!(s.cwd, env.work().canonicalize().unwrap());
    assert!(s.writable);
    assert!(
        s.command.starts_with("cd -- 'sub' || exit 111\n"),
        "command: {}",
        s.command
    );
}

#[test]
fn bash_made_change_in_a_git_repo_triggers_checks() {
    let env = TestEnv::new();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(env.work())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "echo hi > made-by-bash.txt"}).to_string(),
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}

#[test]
fn bash_without_changes_runs_no_checks() {
    let env = TestEnv::new();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(env.work())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert!(rec.checks().is_empty());
}

fn git_in(env: &TestEnv, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(env.work())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?}");
}

#[test]
fn bash_edit_of_an_already_untracked_file_triggers_checks() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    std::fs::write(env.work().join("new.rs"), "a\n").unwrap();
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "echo b >> new.rs"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}

#[test]
fn fingerprint_does_not_run_repository_diff_helpers() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    git_in(&env, &["config", "user.email", "t@example.invalid"]);
    git_in(&env, &["config", "user.name", "t"]);
    std::fs::write(env.work().join("t.txt"), "a\n").unwrap();
    git_in(&env, &["add", "t.txt"]);
    git_in(&env, &["commit", "-q", "-m", "x"]);
    std::fs::write(env.work().join("helper.sh"), "#!/bin/sh\ntouch helper-ran\n").unwrap();
    let helper = env.work().join("helper.sh");
    git_in(&env, &["config", "diff.external", helper.to_str().unwrap()]);
    git_in(&env, &["config", "core.fsmonitor", "touch fsmonitor-ran; false"]);
    std::fs::write(env.work().join(".gitattributes"), "*.txt diff=x\n").unwrap();
    git_in(&env, &["config", "diff.x.textconv", "touch textconv-ran; cat"]);
    std::fs::set_permissions(&helper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(env.work().join("t.txt"), "b\n").unwrap();
    let _ = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    for m in ["helper-ran", "fsmonitor-ran", "textconv-ran"] {
        assert!(!env.work().join(m).exists(), "{m} executed on the host");
    }
}

#[test]
fn fingerprint_does_not_run_repository_filters() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    git_in(&env, &["config", "user.email", "t@example.invalid"]);
    git_in(&env, &["config", "user.name", "t"]);
    std::fs::write(env.work().join(".gitattributes"), "*.txt filter=x\n").unwrap();
    std::fs::write(env.work().join("t.txt"), "a\n").unwrap();
    git_in(&env, &["add", "."]);
    git_in(&env, &["commit", "-q", "-m", "x"]);
    git_in(&env, &["config", "filter.x.clean", "touch filter-ran; cat"]);
    let _ = std::fs::remove_file(env.work().join("filter-ran"));
    std::fs::write(env.work().join("t.txt"), "bb\n").unwrap();
    let _ = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "echo c >> t.txt"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert!(
        !env.work().join("filter-ran").exists(),
        "filter executed on the host"
    );
}

/// The last user message a request carried (the repair or nudge that opened its turn).
fn last_user(engine: &ScriptedEngine, request: usize) -> String {
    engine.requests()[request]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

/// One model reply with tool calls, in call order.
fn tools_turn(calls: Vec<oxa_proto::ToolCall>) -> ScriptTurn {
    ScriptTurn {
        tool_calls: calls,
        finish: oxa_proto::Finish::ToolCalls,
        ..ScriptTurn::text("")
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> oxa_proto::ToolCall {
    oxa_proto::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args_raw: args.to_string(),
    }
}

/// #2295: on a signal of failure, the repair names the files the batch wrote and a check passed
/// over, so the model fixes the difference instead of rebuilding its result.
#[test]
fn a_failure_with_a_passing_check_names_the_verified_file_in_the_repair() {
    let env = TestEnv::new();
    // A build check that passes in this folder: `make all` is a recognised check.
    env.write("work/Makefile", "all:\n\t@true\n");
    let (rec, engine) = run(
        &env,
        // The check passes on this turn's write and fails as the end-of-run done check.
        json!(["test -f done-marker"]),
        vec![
            // One batch: write the result, a command fails, a check passes over both.
            tools_turn(vec![
                call("w", "write", json!({"path": "answer.json", "content": "decoded"})),
                call("b", "bash", json!({"command": "false"})),
                // A build run that succeeds is the check (a bare `true` is not one).
                call("c", "bash", json!({"command": "make all"})),
            ]),
            ScriptTurn::text("done"),
        ],
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(checks[0].contains("FAILED"), "{checks:?}");
    // The write batch, the reply after it, then the repair request.
    assert_eq!(engine.requests().len(), 3);
    let repair = last_user(&engine, 2);
    assert!(repair.contains("done checks failed"), "{repair}");
    assert!(
        repair.contains(
            "Not re-derived: these files were written successfully in this run and the check \
             passed after their last write, so they are this run's verified result: answer.json."
        ),
        "the repair names the verified file: {repair}"
    );
    assert!(
        repair.contains("change only what differs instead of rebuilding"),
        "{repair}"
    );
    // The reply after the batch carries no line; the repair carries it once.
    assert!(!last_user(&engine, 1).contains("Not re-derived"));
    assert_eq!(last_user(&engine, 2).matches("Not re-derived").count(), 1);
}

/// A batch with a failing command but no passing check, or one that wrote no file, keeps the
/// repair exactly as it was.
#[test]
fn a_repair_without_a_verified_result_is_unchanged() {
    // A failing command and no passing check in the batch that wrote the file.
    let no_check = vec![
        tools_turn(vec![
            call("w", "write", json!({"path": "a.txt", "content": "x"})),
            call("b", "bash", json!({"command": "false"})),
        ]),
        ScriptTurn::text("done"),
    ];
    // The file was written in an earlier turn; this batch fails a command and passes a check,
    // but wrote nothing itself, so it has nothing verified to name.
    let no_write = vec![
        tools_turn(vec![call("w", "write", json!({"path": "b.txt", "content": "x"}))]),
        tools_turn(vec![
            call("b", "bash", json!({"command": "false"})),
            call("c", "bash", json!({"command": "cargo test"})),
        ]),
        ScriptTurn::text("done"),
    ];
    for (label, turns) in [("no check", no_check), ("no write", no_write)] {
        let env = TestEnv::new();
        let (rec, engine) = run(&env, json!(["exit 1"]), turns);
        assert!(
            !rec.checks().is_empty(),
            "{label}: no done check ran: {:?}",
            rec.checks()
        );
        let last = engine.requests().len() - 1;
        let repair = last_user(&engine, last);
        assert!(repair.contains("done checks failed"), "{label}: {repair}");
        assert!(!repair.contains("Not re-derived"), "{label}: {repair}");
    }
}

#[test]
fn tree_change_made_by_a_check_is_not_blamed_on_the_model() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    let (rec, _) = run(
        &env,
        json!(["echo x >> gen.txt; false"]),
        vec![
            write("w1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks().len(), 1, "{:?}", rec.checks());
}

/// A run that keeps repeating past the guard's give-up point warns and continues (#1448): the
/// reply after the loop still runs, and the ordinary done checks still gate the result.
#[test]
fn repeat_guard_warns_and_continues_and_still_runs_the_done_checks() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("b.txt"), "x").unwrap();
    let mut turns = vec![write("w1", "a.txt")];
    turns.extend((0..8).map(|i| ScriptTurn::tool(&format!("r{i}"), "read", r#"{"path":"b.txt"}"#)));
    turns.push(ScriptTurn::text("done"));
    // The ordinary repair round after the failing check: one more scripted reply.
    turns.push(ScriptTurn::text("fixed now"));
    let engine = Arc::new(ScriptedEngine::new(turns));
    let settings = Settings::from_values(json!({ "done": ["echo CHECKED; exit 1"] }), json!({}));
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Direct),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    let mut s = Session::open(deps, Open::New).expect("open");
    let mut rec = Rec::default();
    s.prompt("go".into(), Vec::new(), &mut rec, &mut HeadlessUi::default())
        .expect("prompt");
    // The run finished on its scripted reply; the failing done check then failed it.
    assert_eq!(*s.outcome(), oxa_core::RunOutcome::Failed("done_checks_failed"));
    assert_eq!(s.outcome().exit_code(), 1);
    let checks = rec.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(checks[0].contains("CHECKED"), "{checks:?}");
    // The user was warned once — shown and persisted — and every repeat ran.
    assert!(rec.0.iter().any(|e| matches!(
        e,
        AgentEvent::Notice { level: Level::Warning, text, .. }
            if text.starts_with("The model repeated the same tool call")
    )));
    let shown: Vec<_> = s
        .inner()
        .tree
        .branch(s.inner().tree.leaf())
        .into_iter()
        .filter_map(|e| match &e.body {
            EntryBody::Shown { text, why } => Some((text.clone(), *why)),
            _ => None,
        })
        .collect();
    let warnings: Vec<_> = shown
        .iter()
        .filter(|(text, _)| text.starts_with("The model repeated the same tool call"))
        .collect();
    assert_eq!(warnings.len(), 1, "{shown:?}");
    assert_eq!(warnings[0].1, oxa_proto::shown_why::NOTICE);
    // The one other shown entry is the done check's own result, not a second warning.
    assert_eq!(shown.len(), 2, "{shown:?}");
    // Nothing was cut short: the write, all eight repeats, the final reply and the check's
    // ordinary repair round all ran.
    assert_eq!(engine.requests().len(), 11);
    assert_eq!(engine.remaining(), 0);
}

/// A hook set that offers tools (how extension, MCP and sub-agent tools reach a session).
struct ExtTools(Vec<Arc<dyn Tool>>);

impl Hooks for ExtTools {
    fn fire(&self, _p: HookPoint, _payload: &serde_json::Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.clone()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// An extension / MCP / sub-agent style tool: not a built-in, writes a file when `write` is set.
struct Outside {
    spec: ToolSpec,
    write: bool,
}

impl Outside {
    fn tool(name: &str, write: bool) -> Arc<dyn Tool> {
        Arc::new(Outside {
            spec: ToolSpec {
                name: name.to_string(),
                description: "test tool".to_string(),
                schema_json: "{}".to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Extension("ext.js".to_string()),
            },
            write,
        })
    }
}

impl Tool for Outside {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn default_selected(&self) -> bool {
        true
    }
    fn access(&self, _args: &serde_json::Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &serde_json::Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if self.write {
            std::fs::write(cx.cwd.join("made-by-tool.txt"), "x").unwrap();
        }
        ToolOutput::ok("ok")
    }
}

fn outside(env: &TestEnv, write: bool) -> Vec<String> {
    let settings = Settings::from_values(json!({ "done": ["true"] }), json!({}));
    let (rec, _, _) = run_tools(
        env,
        settings,
        vec![ScriptTurn::tool("t1", "mcp_edit", "{}"), ScriptTurn::text("done")],
        Arc::new(Direct),
        vec![Outside::tool("mcp_edit", write)],
    );
    rec.checks()
}

#[test]
fn non_builtin_tool_edit_in_a_git_repo_triggers_checks() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    assert_eq!(outside(&env, true), vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_edit_outside_git_triggers_checks() {
    let env = TestEnv::new();
    assert_eq!(outside(&env, true), vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_edit_outside_git_with_unreadable_subdirectory_triggers_checks() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let blocked = env.work().join("unreadable");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(std::fs::read_dir(&blocked).is_err(), "fixture must be unreadable");
    let unchanged = outside(&env, false);
    let changed = outside(&env, true);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(unchanged.is_empty(), "{unchanged:?}");
    assert_eq!(changed, vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_without_changes_runs_no_checks() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("a.txt"), "a").unwrap();
    assert!(outside(&env, false).is_empty());
    git_in(&env, &["init", "-q"]);
    assert!(outside(&env, false).is_empty());
}

#[test]
fn bash_made_change_outside_git_triggers_checks() {
    let env = TestEnv::new();
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "echo hi > made-by-bash.txt"}).to_string(),
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}

fn advised_checks(
    env: &TestEnv,
    command: &str,
    paths: &[&str],
    after: u32,
    max_calls: u16,
    failures: u16,
) -> (Session, Arc<ScriptedEngine>, usize) {
    use oxa_core::advice::{AdviceContext, AdviceDispatcher, BoundAdvisor};
    use oxa_session::{
        advice::{AdviceGrant, AdvicePolicy, AdviceTier, AdviceTrigger, AdvisorRoute},
        SessionRef,
    };
    use std::{
        num::{NonZeroU16, NonZeroU32},
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Advisor(AtomicUsize);
    impl AdviceDispatcher for Advisor {
        fn authorized(&self, _: &SessionRef, _: u64) -> bool {
            true
        }

        fn call(
            &self,
            grant: &AdviceGrant,
            context: &AdviceContext,
            _: &CancelToken,
        ) -> Result<String, OxaError> {
            assert_eq!(grant.trigger, AdviceTrigger::DoneChecks);
            assert!(context.failing_evidence.contains("ready.txt"));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("Create the readiness marker.".into())
        }
    }

    let turns = paths
        .iter()
        .enumerate()
        .flat_map(|(index, path)| [write(&format!("check-{index}"), path), ScriptTurn::text("done")])
        .collect();
    let settings = Settings::from_values(json!({"done":[command]}), json!({}));
    let (mut session, worker) = test_session(env, settings, turns, Arc::new(Direct), Vec::new());
    let advisor = Arc::new(Advisor(AtomicUsize::new(0)));
    session.inner().shared.bind_advisor(BoundAdvisor {
        session: session.inner().sref.clone(),
        lease_identity: [4; 32],
        objective: "Qualify the synthetic work.".into(),
        worker: AdviceTier::Bulk,
        done_checks_after: NonZeroU32::new(after).unwrap(),
        policy: AdvicePolicy {
            triggers: vec![AdviceTrigger::DoneChecks],
            max_calls: NonZeroU16::new(max_calls).unwrap(),
            failures_before_escalation: NonZeroU16::new(failures).unwrap(),
            max_output: NonZeroU32::new(64).unwrap(),
        },
        route: AdvisorRoute {
            tier: AdviceTier::Main,
            model: "fixture-main".into(),
            pool: "fixture-pool".into(),
        },
        dispatcher: advisor.clone(),
        scrub: Arc::new(str::to_owned),
    });
    session
        .prompt(
            "go".into(),
            Vec::new(),
            &mut Rec::default(),
            &mut HeadlessUi::default(),
        )
        .unwrap();
    (session, worker, advisor.0.load(Ordering::SeqCst))
}

#[test]
fn advice_waits_for_the_done_threshold_and_records_only_a_real_passing_check() {
    let env = TestEnv::new();
    let (session, worker, calls) = advised_checks(
        &env,
        "test -f ready.txt",
        &["draft.txt", "other.txt", "ready.txt"],
        2,
        1,
        1,
    );
    assert_eq!(calls, 1);
    assert!(!session.inner().stats.done_failed);
    assert!(!worker.requests()[2].messages.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("readiness marker"))));
    assert!(worker.requests()[4].messages.iter().any(|message| message
        .content
        .as_deref()
        .is_some_and(|text| text.contains("readiness marker"))));
    assert!(session.inner().tree.entries().iter().any(|entry| matches!(
        &entry.body, oxa_proto::EntryBody::Custom { custom_type, data }
            if custom_type == "oxa.advice.v1" && data["phase"] == "resolved"
    )));
}

#[test]
fn the_same_done_check_only_requests_advice_once_when_its_exit_status_changes() {
    let env = TestEnv::new();
    let (session, _, calls) = advised_checks(
        &env,
        "test -f ready.txt || { test -f changed.txt && exit 2; exit 1; }",
        &["draft.txt", "changed.txt", "later.txt"],
        1,
        2,
        2,
    );
    assert_eq!(calls, 1);
    assert!(session.inner().stats.done_failed);
}

#[test]
fn a_final_failed_done_check_escalates_delivered_advice() {
    let env = TestEnv::new();
    let (session, _, calls) = advised_checks(
        &env,
        "test -f ready.txt",
        &["draft.txt", "other.txt", "later.txt"],
        2,
        1,
        1,
    );
    assert_eq!(calls, 1);
    assert_eq!(
        session.outcome(),
        &oxa_core::RunOutcome::Failed("advisor_escalation_required")
    );
}

#[test]
fn advice_can_arrive_at_the_final_automatic_check_and_get_one_repair() {
    let env = TestEnv::new();
    let (session, worker, calls) = advised_checks(
        &env,
        "test -f ready.txt",
        &["draft.txt", "other.txt", "later.txt", "ready.txt"],
        3,
        1,
        1,
    );
    assert_eq!(calls, 1);
    assert!(!session.inner().stats.done_failed);
    assert_eq!(worker.requests().len(), 8);
}
