//! Wave H (lane hooks): `project_trust` at startup, through the real program path.
//!
//! pi (`core/project-trust.ts` `resolveProjectTrusted`): when the project has trust-gated
//! resources and no flag decides, the user-level and `-e` extensions get `project_trust` with
//! `{cwd}`; the first `yes` / `no` wins over the saved decision and the default, `remember:
//! true` saves it, `undecided` falls through. Project extensions are not loaded before.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

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

/// Run `oxa -p hi` in-process; returns the exit code and stderr.
fn go(env: &TestEnv) -> (i32, String) {
    go_args(env, vec!["-p".to_string(), "hi".to_string()])
}

/// Run `oxa --help` in-process; the help path decides trust without a session.
fn help(env: &TestEnv) -> (i32, String) {
    go_args(env, vec!["--help".to_string()])
}

fn go_args(env: &TestEnv, argv: Vec<String>) -> (i32, String) {
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).expect("fake engine");
    let owned = env.envs(&server.url());
    let cwd = env.work();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let err = Sink::default();
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(Sink::default()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let text = String::from_utf8_lossy(&err.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text));
    });
    rx.recv_timeout(Duration::from_secs(60)).expect("oxa finished")
}

/// A user-level extension answering `project_trust` with `answer` (a JS object literal); it
/// logs the event it got to `trust-log.txt` in the work directory.
fn user_ext(env: &TestEnv, answer: &str) {
    env.write(
        "home/.oxa/agent/extensions/trust.js",
        &format!(
            "export default function (oxa) {{ oxa.on('project_trust', (e, ctx) => {{ ctx.fs.writeFile('trust-log.txt', e.type + ' ' + e.cwd); return {answer}; }}); }}"
        ),
    );
}

/// A project extension: logs `loaded` when it runs (only when the project is trusted).
fn project_ext(env: &TestEnv) {
    env.write(
        "work/.oxa/extensions/local.js",
        "export default function (oxa) { oxa.on('session_start', (e, ctx) => { ctx.fs.writeFile('project-log.txt', 'loaded'); }); }",
    );
}

#[test]
fn extension_decides_trust() {
    let env = TestEnv::new();
    project_ext(&env);
    user_ext(&env, "{ trusted: 'yes', remember: true }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    let log = std::fs::read_to_string(env.work().join("trust-log.txt")).expect("project_trust fired");
    assert!(log.starts_with("project_trust "), "{log}");
    assert!(
        log.ends_with(&env.work().display().to_string()) || log.contains("work"),
        "{log}"
    );
    assert!(
        env.work().join("project-log.txt").exists(),
        "the extension's yes trusts the project: its extension loads"
    );
    let saved = std::fs::read_to_string(env.agent_dir().join("trust.json")).expect("remember saves");
    assert!(saved.contains("true"), "{saved}");
}

#[test]
fn undecided_falls_through_to_the_shipped_default() {
    let env = TestEnv::new();
    project_ext(&env);
    user_ext(&env, "{ trusted: 'undecided' }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(env.work().join("trust-log.txt").exists(), "project_trust fired");
    assert!(
        env.work().join("project-log.txt").exists(),
        "no decision: defaultProjectTrust ships as always, so the project's extension loads"
    );
    // `project_trust` asks before the default resolves, so the shipped default trusts the
    // project without writing a decision the owner never made.
    assert!(!env.agent_dir().join("trust.json").exists(), "nothing saved");
}

/// An owner who wants the question (`defaultProjectTrust: "ask"`) keeps the old headless
/// behaviour: no prompt, the resources are skipped, and the notice names them.
#[test]
fn ask_opt_in_skips_without_a_question() {
    let env = TestEnv::new();
    project_ext(&env);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"ask"}"#,
    );
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        !env.work().join("project-log.txt").exists(),
        "ask without a terminal skips the project's extension"
    );
    assert!(
        err.contains("not trusted") && err.contains(".oxa/extensions"),
        "{err}"
    );
    assert!(!env.agent_dir().join("trust.json").exists(), "nothing saved");
}

#[test]
fn not_fired_without_gated_resources() {
    let env = TestEnv::new();
    user_ext(&env, "{ trusted: 'no' }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        !env.work().join("trust-log.txt").exists(),
        "nothing to trust: no event"
    );
}

#[test]
fn content_policy_checks_changes_without_a_saved_folder_decision() {
    for user_settings in [
        r#"{"projectTrustPolicy":"content"}"#,
        r#"{"projectTrustPolicy":"content","defaultProjectTrust":"always"}"#,
    ] {
        let env = TestEnv::new();
        env.write("home/.oxa/agent/settings.json", user_settings);
        project_ext(&env);
        let marker = env.work().join("project-log.txt");
        let (code, err) = go(&env);
        assert_eq!(code, 0, "{err}");
        assert!(marker.exists(), "first use still trusts the folder");
        std::fs::remove_file(&marker).unwrap();
        let (code, err) = go(&env);
        assert_eq!(code, 0, "{err}");
        assert!(marker.exists(), "unchanged content still loads");
        std::fs::remove_file(&marker).unwrap();

        env.write("work/.oxa/extensions/local.js", "// changed\n");
        let (code, err) = go(&env);
        assert_eq!(code, 0, "{err}");
        assert!(
            err.contains("Project config not loaded") && err.contains("local.js"),
            "{err}"
        );
        assert_eq!(
            oxa_perm::TrustStore::load(&env.agent_dir().join("trust.json")).lookup(&env.work()),
            None
        );
    }
}

#[test]
fn content_policy_tracks_first_resources_in_an_empty_default_trusted_project() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"projectTrustPolicy":"content"}"#,
    );
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    project_ext(&env);
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Project config not loaded") && err.contains("local.js"),
        "{err}"
    );
    assert!(!env.work().join("project-log.txt").exists());
}

/// `--help` is a project-resource read too: in an empty default-trusted project under the
/// owner's content opt-in it records the empty snapshot, so executable content a later
/// repository update adds is changed content, not the first approved baseline.
#[test]
fn content_policy_help_snapshots_an_empty_default_trusted_project() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"projectTrustPolicy":"content"}"#,
    );
    let (code, err) = help(&env);
    assert_eq!(code, 0, "{err}");
    project_ext(&env);
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Project config not loaded") && err.contains("local.js"),
        "{err}"
    );
    assert!(!env.work().join("project-log.txt").exists());
    assert_eq!(
        oxa_perm::TrustStore::load(&env.agent_dir().join("trust.json")).lookup(&env.work()),
        None,
        "the snapshot saves no folder decision"
    );
}

/// #201 (3): a project trusted by a saved decision, with no project config when a session
/// starts, gets its (empty) approval recorded at that start. `.oxa` content the session's tools
/// then create is new at the next start and needs the normal approval instead of being taken
/// as the project's first record.
#[test]
fn config_created_during_a_session_is_not_auto_approved_next_start() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"projectTrustPolicy":"content"}"#,
    );
    let trust = env.agent_dir().join("trust.json");
    let mut store = oxa_perm::TrustStore::load(&trust);
    store.save(&trust, &env.work(), true).expect("save trust");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    // What a tool of that session wrote (the sandbox leaves `.oxa` writable on purpose).
    env.write("work/.oxa/settings.json", r#"{"done": ["echo planted"]}"#);
    let (code, err) = go(&env);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("done checks in .oxa/settings.json were not run") && err.contains("--approve"),
        "{err}"
    );
    assert!(err.contains("settings.json"), "{err}");
    let store = oxa_perm::TrustStore::load(&trust);
    assert!(
        matches!(store.approval(&env.work()), oxa_perm::Approval::Changed(_)),
        "the planted file is still not approved"
    );
}

#[test]
fn parent_folder_trust_loads_changes_without_granting_content_approval() {
    let env = TestEnv::new();
    let work = env.work();
    let trust = env.agent_dir().join("trust.json");
    let mut store = oxa_perm::TrustStore::load(&trust);
    store.save(&trust, work.parent().unwrap(), true).unwrap();
    project_ext(&env);
    env.write("work/.oxa/settings.json", "{}");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    let marker = work.join("project-log.txt");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "loaded");
    std::fs::remove_file(&marker).unwrap();

    env.write(
        "work/.oxa/extensions/local.js",
        "export default function (oxa) { oxa.on('session_start', (e, ctx) => { ctx.fs.writeFile('project-log.txt', 'updated'); }); }",
    );
    env.write("work/.oxa/settings.json", r#"{"quietStartup":true}"#);
    env.write(
        "work/.oxa/themes/new-theme.json",
        r##"{"name":"new-theme","colors":{"notARole":"#fff"}}"##,
    );
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "updated");
    let notice = err
        .lines()
        .find(|line| line.contains("loaded under folder trust"))
        .expect(&err);
    assert!(notice.contains("local.js (changed)"), "{notice}");
    assert!(notice.contains("settings.json (changed)"), "{notice}");
    assert!(notice.contains("new-theme.json (new)"), "{notice}");
    std::fs::remove_file(&marker).unwrap();

    // Switching to strict mode still sees the original approval, and the project cannot opt out.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"projectTrustPolicy":"content"}"#,
    );
    env.write("work/.oxa/settings.json", r#"{"projectTrustPolicy":"folder"}"#);
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Project config not loaded") && err.contains("local.js"),
        "{err}"
    );
    assert!(!marker.exists());

    // An explicit child denial still beats the saved parent grant under folder policy, and
    // the shipped default does not overrule it.
    env.write("home/.oxa/agent/settings.json", "{}");
    store.save(&trust, &work, false).unwrap();
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("not trusted") && !err.contains("loaded under folder trust"),
        "{err}"
    );
    assert!(!marker.exists());
}
