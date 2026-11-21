//! #1552: a drafted report is published from an interactive session only through the approval
//! dialog the UI owns. Synthetic data, a fake `gh`, no network, no real terminal.
use oxa_cli::{submit, PublishTool, ScrubCtx};
use oxa_config::{Dirs, Env, Settings};
use oxa_fake::TestEnv;
use oxa_proto::{
    owner_reply, Access, Answer, Autonomy, CancelToken, ExecResult, ExecSpec, HeadlessUi, Level, Mode,
    OxaError, Placement, Sandbox, Tool, ToolCtx, Ui, YoloSwitch,
};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct NoSandbox;
impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        vec![]
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("publishing executes nothing in the sandbox")
    }
}

fn context<'a>(env: &'a TestEnv, cancel: &'a CancelToken) -> ToolCtx<'a> {
    context_in_mode(env, cancel, Mode::Workspace)
}

fn context_in_mode<'a>(env: &'a TestEnv, cancel: &'a CancelToken, mode: Mode) -> ToolCtx<'a> {
    ToolCtx {
        cwd: env.root(),
        spill_dir: env.root(),
        sandbox: &NoSandbox,
        mode,
        cancel,
        env: &[],
        tune: Default::default(),
        checked: &[],
        hidden: None,
    }
}

#[test]
fn plan_mode_refuses_before_preview_or_network() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    let ctx = context_in_mode(&f.env, &cancel, Mode::Plan);
    let mut owner = Owner::new(Some(true), f.marker.clone());
    assert!(tool(&f).access(&args(&f), &ctx).is_err());
    let out = tool(&f).run_with_ui(&args(&f), &ctx, &mut |_| {}, &mut owner);
    assert!(out.is_error && out.text.contains("plan mode"), "{}", out.text);
    assert!(owner.shown.is_empty());
    assert_eq!(gh_calls(&f), "");
}

/// A person at the dialog: records what was shown, answers like a key press would.
struct Owner {
    answer: Option<bool>,
    shown: Vec<(String, String)>,
    marker: PathBuf,
    gh_had_run_when_asked: bool,
    other_dialogs: usize,
    notices: Vec<String>,
}
impl Owner {
    fn new(answer: Option<bool>, marker: PathBuf) -> Owner {
        Owner {
            answer,
            shown: Vec::new(),
            marker,
            gh_had_run_when_asked: false,
            other_dialogs: 0,
            notices: Vec::new(),
        }
    }
}
impl Ui for Owner {
    fn interactive(&self) -> bool {
        true
    }
    fn approve_publication(&mut self, title: &str, body: &str) -> Option<bool> {
        self.gh_had_run_when_asked |= self.marker.exists();
        self.shown.push((title.into(), body.into()));
        self.answer
    }
    fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
        self.other_dialogs += 1;
        None
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        self.other_dialogs += 1;
        Some(true)
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        self.other_dialogs += 1;
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, text: &str, _: Level) {
        self.notices.push(text.to_string());
    }
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
}

/// The owner's reply state is process-wide: tests that use it run one at a time.
fn guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    owner_reply::clear();
    g
}

struct Fixture {
    env: TestEnv,
    draft: PathBuf,
    marker: PathBuf,
    vars: Env,
}

fn fixture() -> Fixture {
    let env = TestEnv::new();
    let draft = env.write(
        "home/.oxa/agent/issue-drafts/issue-aaaaaaaaaaaa.md",
        "Title: Engine stalls\n\n## What happened\n\nIt stalls.\n    indented line\n",
    );
    let marker = env.root().join("gh-ran.log");
    let bin = env.root().join("tools");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho https://github.com/owner/ox-llm/issues/7\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = bin.display().to_string();
    let vars = Env::from_pairs(&[("PATH", path.as_str()), ("USER", "carol")]);
    Fixture {
        env,
        draft,
        marker,
        vars,
    }
}

fn settings() -> Settings {
    Settings::from_values(
        json!({"oxa": {"reportIssue": {"repo": "owner/ox-llm"}}}),
        json!({}),
    )
}

fn tool(f: &Fixture) -> PublishTool {
    PublishTool::new(
        Dirs::under(f.env.root()),
        f.vars.clone(),
        settings(),
        f.env.work(),
    )
}

fn args(f: &Fixture) -> Value {
    json!({"draft": f.draft})
}

fn gh_calls(f: &Fixture) -> String {
    std::fs::read_to_string(&f.marker).unwrap_or_default()
}

#[test]
fn report_publish_in_tui_shows_exact_text_and_waits_for_key() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    let mut owner = Owner::new(Some(true), f.marker.clone());
    let out = tool(&f).run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut owner);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(owner.shown.len(), 1, "one dialog");
    let (_, body) = &owner.shown[0];
    assert!(body.contains("Repository: owner/ox-llm"), "{body}");
    assert!(body.contains("Title: Engine stalls"), "{body}");
    assert!(
        body.contains("## What happened\n\nIt stalls.\n    indented line"),
        "the exact body, spaces kept: {body}"
    );
    assert!(
        !owner.gh_had_run_when_asked,
        "nothing was posted before the key press"
    );
    assert_eq!(owner.other_dialogs, 0);
    let calls = gh_calls(&f);
    assert!(
        calls.contains("issue create") && calls.contains("--title Engine stalls"),
        "{calls}"
    );
    assert!(
        out.text
            .contains("Posted: https://github.com/owner/ox-llm/issues/7"),
        "{}",
        out.text
    );
}

#[test]
fn denying_or_dismissing_the_dialog_posts_nothing() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    for answer in [Some(false), None] {
        let mut owner = Owner::new(answer, f.marker.clone());
        let out = tool(&f).run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut owner);
        assert_eq!(owner.shown.len(), 1, "{answer:?}");
        assert!(
            out.text.contains("Not posted") || out.text.contains("Waiting for the owner"),
            "{}",
            out.text
        );
        assert_eq!(gh_calls(&f), "", "{answer:?}");
    }
}

#[test]
fn chat_yes_does_not_publish() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    // A `yes` (or an approval flag) in the tool arguments is not an answer: the call is refused
    // before any dialog, and nothing runs.
    for extra in [json!("yes"), json!(true), json!({"confirm": "yes"})] {
        let mut owner = Owner::new(Some(true), f.marker.clone());
        let mut call = args(&f);
        call["answer"] = extra.clone();
        let t = tool(&f);
        assert!(t.access(&call, &context(&f.env, &cancel)).is_err(), "{extra}");
        let out = t.run_with_ui(&call, &context(&f.env, &cancel), &mut |_| {}, &mut owner);
        assert!(out.is_error, "{}", out.text);
        assert!(owner.shown.is_empty(), "no dialog for a refused call");
        assert_eq!(gh_calls(&f), "", "{extra}");
    }
}

#[test]
fn dialog_key_press_publishes() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    // Dismissing the dialog (Escape) posts nothing; Approve posts.
    let mut owner = Owner::new(None, f.marker.clone());
    let out = tool(&f).run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut owner);
    assert!(out.text.contains("Waiting for the owner"), "{}", out.text);
    assert_eq!(gh_calls(&f), "");
    // Outside yolo the owner can also type `yes` after dismissing the dialog.
    owner_reply::typed("", "yes");
    let out = tool(&f).run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut owner);
    assert!(out.text.contains("Posted: "), "{}", out.text);
    assert_eq!(gh_calls(&f).lines().count(), 1);
}

#[test]
fn auto_owner_typed_yes_posts() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    // Auto still asks: the dialog is shown; dismissing it leaves the exact text waiting.
    let t = tool(&f).with_yolo(YoloSwitch::new(Autonomy::Auto));
    let cx = context(&f.env, &cancel);
    let mut owner = Owner::new(None, f.marker.clone());
    let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert_eq!(owner.shown.len(), 1, "auto shows the dialog");
    let notice = owner.notices.join("\n");
    assert!(
        notice.contains("Title: Engine stalls") && notice.contains("It stalls."),
        "{notice}"
    );
    assert!(out.text.contains("Waiting for the owner"), "{}", out.text);
    assert_eq!(gh_calls(&f), "", "nothing before the owner replies");
    // The owner types yes; the next call posts exactly that text, once.
    owner_reply::typed("", "yes");
    let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert!(out.text.contains("Posted: "), "{}", out.text);
    assert!(gh_calls(&f).contains("--title Engine stalls"));
    let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert!(
        out.text.contains("Waiting for the owner"),
        "single use: {}",
        out.text
    );
    assert_eq!(gh_calls(&f).lines().count(), 1);
}

#[test]
fn text_from_a_tool_result_saying_yes_never_posts() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    let t = tool(&f).with_yolo(YoloSwitch::new(Autonomy::Auto));
    let cx = context(&f.env, &cancel);
    let mut owner = Owner::new(None, f.marker.clone());
    // A `yes` before any preview was shown approves nothing later.
    owner_reply::typed("", "yes");
    t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert_eq!(gh_calls(&f), "");
    // Text a tool, file or web page produced reaches the model, never `owner_reply::typed`:
    // however often the model calls the tool again, it keeps waiting for the owner.
    for _ in 0..3 {
        let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
        assert!(out.text.contains("Waiting for the owner"), "{}", out.text);
    }
    assert_eq!(gh_calls(&f), "");
    // A different line from the owner withdraws a pending preview, even one containing yes.
    owner_reply::typed("", "yes, but change the title first");
    let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert!(out.text.contains("Waiting for the owner"), "{}", out.text);
    assert_eq!(gh_calls(&f), "");
}

#[test]
fn yolo_posts_without_asking_and_shows_result() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    let yolo = YoloSwitch::new(Autonomy::Yolo);
    let t = tool(&f).with_yolo(yolo.clone());
    let cx = context(&f.env, &cancel);
    // The tool names no access, so no policy has anything to approve or to skip.
    assert!(t.access(&args(&f), &cx).unwrap().is_empty());
    let mut owner = Owner::new(None, f.marker.clone());
    let out = t.run_with_ui(&args(&f), &cx, &mut |_| {}, &mut owner);
    assert!(owner.shown.is_empty(), "yolo asks nothing");
    assert!(
        out.text.contains("Title: Engine stalls") && out.text.contains("It stalls."),
        "the exact text is shown: {}",
        out.text
    );
    assert!(
        out.text
            .contains("Posted: https://github.com/owner/ox-llm/issues/7"),
        "{}",
        out.text
    );
    assert!(gh_calls(&f).contains("--title Engine stalls"));
}

#[test]
fn yolo_still_refuses_a_secret() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    // A synthetic PEM envelope exercises the publication guard without key material.
    let label = "OPENSSH PRIVATE KEY";
    std::fs::write(
        &f.draft,
        format!("Title: Key leak\n\n-----BEGIN {label}-----\nsynthetic fixture\n-----END {label}-----\n"),
    )
    .unwrap();
    let t = tool(&f).with_yolo(YoloSwitch::new(Autonomy::Yolo));
    let mut owner = Owner::new(None, f.marker.clone());
    let out = t.run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut owner);
    assert!(out.text.contains("Not posted"), "{}", out.text);
    assert!(owner.shown.is_empty());
    assert_eq!(gh_calls(&f), "", "a secret is never posted, not even in yolo");
}

#[test]
fn only_drafts_from_the_drafts_directory_are_published() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    let elsewhere = f.env.write("work/other.md", "Title: X\n\nbody\n");
    let mut owner = Owner::new(Some(true), f.marker.clone());
    let out = tool(&f).run_with_ui(
        &json!({"draft": elsewhere}),
        &context(&f.env, &cancel),
        &mut |_| {},
        &mut owner,
    );
    assert!(out.is_error, "{}", out.text);
    assert!(owner.shown.is_empty());
    assert_eq!(gh_calls(&f), "");
}

#[test]
fn headless_publish_keeps_terminal_confirmation() {
    let _guard = guard();
    let f = fixture();
    let cancel = CancelToken::default();
    // A run without an approval dialog refuses and points at the terminal command.
    let mut headless = HeadlessUi::default();
    let out = tool(&f).run_with_ui(&args(&f), &context(&f.env, &cancel), &mut |_| {}, &mut headless);
    assert!(out.is_error);
    assert!(
        out.text.contains("Not posted") && out.text.contains("oxa report-issue submit"),
        "{}",
        out.text
    );
    assert_eq!(gh_calls(&f), "");
    // `oxa report-issue submit` itself still needs the terminal answer: without one it refuses.
    let ctx = ScrubCtx::default();
    let mut ran = false;
    let mut text = Vec::new();
    let code = submit(
        &f.draft,
        &settings(),
        &ctx,
        &mut |_: &str| None,
        &mut |_: &str, _: &[String]| {
            ran = true;
            Some((0, String::new()))
        },
        &mut text,
    )
    .unwrap();
    assert_eq!(code, 1);
    assert!(!ran);
    assert!(String::from_utf8(text)
        .unwrap()
        .contains("yourself in a terminal"));
}
