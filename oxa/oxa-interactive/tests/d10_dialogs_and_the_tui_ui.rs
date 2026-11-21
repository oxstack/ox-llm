//! D10 acceptance tests: Dialogs and the TUI Ui.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_fake::TestEnv;
use oxa_interactive::{
    access_prompt, append_always, ask_missing_cwd, ask_trust, cycle_mode, Block, DialogHost, DialogKey,
    MissingCwdChoice, TrustChoice, UiProxy, UiRequest, ViewModel, DANGER_LABELS,
};
use oxa_proto::{
    Access, Answer, ComponentAction, CustomOpts, DangerAnswer, KeyId, Level, Mode, Placement, ToolClass, Ui,
    UiComponent, Value,
};
use oxa_tui::{Key, Theme};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::channel;

/// A `Ui` that answers `select` from a script and records what it was asked.
#[derive(Default)]
struct Scripted {
    answers: VecDeque<Option<usize>>,
    title: String,
    options: Vec<String>,
}

impl Scripted {
    fn new(answers: Vec<Option<usize>>) -> Scripted {
        Scripted {
            answers: answers.into(),
            ..Default::default()
        }
    }
}

impl Ui for Scripted {
    fn select(&mut self, title: &str, options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        self.title = title.to_string();
        self.options = options.to_vec();
        self.answers.pop_front().flatten()
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _: &str, _: Level) {}
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
}

fn press(host: &mut DialogHost, keymap: &Keymap, id: &str) -> DialogKey {
    host.key(&Key::press(id), keymap)
}

/// Rows a small fullscreen terminal leaves the dialog once the footer is taken: its frame,
/// the choices, the hint and two preview rows. `SHORT_ROWS` is what the driver passes on such
/// a screen, not the preview cap of a tall one.
const SHORT_ROWS: usize = 12;

/// A key press as the real TUI delivers it: the dialog is redrawn before the next key arrives.
fn press_shown(
    host: &mut DialogHost,
    keymap: &Keymap,
    width: u16,
    height: Option<usize>,
    id: &str,
) -> DialogKey {
    let key = host.key(&Key::press(id), keymap);
    host.render_in(width, height, &Theme::plain());
    key
}

/// The framed dialog's lines (from its top edge to its bottom edge), so assertions see only
/// what the dialog itself shows: the transcript's tool header shows the command too.
fn dialog_box(host: &mut DialogHost, width: u16, height: Option<usize>) -> Vec<String> {
    let lines = host.render_in(width, height, &Theme::plain());
    match (lines.first(), lines.last()) {
        (Some(top), Some(bottom)) if top.contains('╭') && bottom.contains('╰') => lines,
        _ => Vec::new(),
    }
}

/// A custom component that counts `up` presses and answers its count on `enter`.
struct Counter {
    n: u32,
}

impl UiComponent for Counter {
    fn render(&mut self, _width: u16) -> Vec<String> {
        vec![format!("count: {}", self.n)]
    }
    fn input(&mut self, key: &KeyId, _text: Option<&str>) -> ComponentAction {
        match key.as_str() {
            "up" => {
                self.n += 1;
                ComponentAction::Redraw
            }
            "enter" => ComponentAction::Done(Value::from(self.n)),
            _ => ComponentAction::Ignored,
        }
    }
}

/// Answer test: Allow once, Allow for this session, Deny, Escape = Deny, and the default focus.
/// `keys` are pressed on a fresh permission prompt; the reply is returned.
fn access_answer(keys: &[&str]) -> Answer {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(
        UiRequest::AskAccess {
            access: Access::Write(PathBuf::from("/w/a.rs")),
            reply: tx,
        },
        &mut vm,
        0,
    );
    for k in keys {
        press(&mut host, &keymap, k);
    }
    rx.recv().expect("the dialog answered")
}

/// Answer mapping
#[test]
fn permission_prompt_once_session_deny() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    let req = UiRequest::AskAccess {
        access: Access::Write(PathBuf::from("/w/a.rs")),
        reply: tx,
    };
    assert!(host.handle(req, &mut vm, 0).is_none());
    assert!(host.is_open());
    assert!(!host.is_overlay());
    let text = host.render(80, &theme).join("\n");
    assert!(text.contains("Allow write?"), "{text}");
    assert!(text.contains("Write to /w/a.rs"), "{text}");
    assert!(text.contains("Allow once") && text.contains("Allow for this session") && text.contains("Deny"));

    // Deny has the focus by default: a stray Enter never grants access.
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Answer::Deny);
    assert!(!host.is_open());

    assert_eq!(access_answer(&["up", "up", "enter"]), Answer::Once);
    assert_eq!(access_answer(&["up", "enter"]), Answer::Session);
    assert_eq!(
        access_answer(&["down", "enter"]),
        Answer::Once,
        "down from Deny wraps to Allow once"
    );
    assert_eq!(access_answer(&["escape"]), Answer::Deny);
    assert_eq!(access_answer(&["up", "up", "escape"]), Answer::Deny);
}

#[test]
fn permission_prompts_queue_and_dropping_the_host_denies() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx1, rx1) = channel();
    let (tx2, rx2) = channel();
    host.handle(
        UiRequest::AskAccess {
            access: Access::Write(PathBuf::from("/w/a.rs")),
            reply: tx1,
        },
        &mut vm,
        0,
    );
    host.handle(
        UiRequest::AskAccess {
            access: Access::Read(PathBuf::from("/r/b.rs")),
            reply: tx2,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx1.recv().unwrap(), Answer::Deny);
    // The second request is now open, in arrival order.
    assert!(host.render(80, &theme).join("\n").contains("Allow read?"));
    drop(host);
    assert_eq!(
        rx2.recv().unwrap(),
        Answer::Deny,
        "quit answers every open request with Deny"
    );
}

/// alt+m cycles plan/workspace/ask
#[test]
fn mode_cycle() {
    let keymap = Keymap::defaults();
    assert!(keymap.matches(&KeyId::parse("alt+m").unwrap(), "app.mode.cycle"));
    let (m1, t1) = cycle_mode(Mode::Plan);
    assert_eq!(m1, Mode::Workspace);
    assert!(t1.starts_with("Mode: workspace"), "{t1}");
    let (m2, t2) = cycle_mode(m1);
    assert_eq!(m2, Mode::Ask);
    assert!(t2.starts_with("Mode: ask"), "{t2}");
    let (m3, t3) = cycle_mode(m2);
    assert_eq!(m3, Mode::Plan);
    assert_eq!(t3, "Mode: plan (read-only)");
}

/// choices and save
#[test]
fn trust_selector() {
    let env = TestEnv::new();
    let cwd = env.work().join("proj").display().to_string();
    let parent = env.work().display().to_string();
    let gated = vec!["settings.json".to_string(), "hooks".to_string()];

    let mut ui = Scripted::new(vec![Some(0), Some(1), Some(2), Some(3), None]);
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::Trust));
    assert_eq!(
        ui.options.len(),
        4,
        "trust, trust parent, do not trust, decide later"
    );
    assert!(ui.title.contains(&cwd), "{}", ui.title);
    assert!(ui.title.contains("settings.json, hooks"), "{}", ui.title);
    assert!(ui.options[1].contains(&parent), "{:?}", ui.options);
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::TrustParent));
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::NoTrust));
    assert_eq!(
        ask_trust(&mut ui, &cwd, &gated),
        Some(TrustChoice::Skip),
        "decide later is a choice"
    );
    assert_eq!(
        ask_trust(&mut ui, &cwd, &gated),
        None,
        "dismissed: the caller treats it as skip"
    );

    // A root cwd has no parent: that choice is not offered and the indices shift.
    let mut root = Scripted::new(vec![Some(1)]);
    assert_eq!(ask_trust(&mut root, "/", &[]), Some(TrustChoice::NoTrust));
    assert_eq!(root.options.len(), 3);
    assert!(root.title.contains("Gated resources: none"), "{}", root.title);
}

/// three outcomes
#[test]
fn missing_cwd_continue_fork_cancel() {
    let mut ui = Scripted::new(vec![Some(0), Some(1), Some(2), None]);
    assert_eq!(
        ask_missing_cwd(&mut ui, "/old", "/new"),
        MissingCwdChoice::Continue
    );
    assert!(ui.options[0].contains("/new"), "{:?}", ui.options);
    assert_eq!(ask_missing_cwd(&mut ui, "/old", "/new"), MissingCwdChoice::Fork);
    assert_eq!(ask_missing_cwd(&mut ui, "/old", "/new"), MissingCwdChoice::Cancel);
    assert_eq!(
        ask_missing_cwd(&mut ui, "/old", "/new"),
        MissingCwdChoice::Cancel,
        "dismissed = cancel"
    );
    assert!(ui.title.contains("/old"), "{}", ui.title);
}

/// timeout returns None with countdown
#[test]
fn timed_dialogs() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
            timeout_ms: Some(3000),
            reply: tx,
        },
        &mut vm,
        1000,
    );
    assert!(host.render(60, &theme).join("\n").contains("(3s)"));
    assert!(!host.tick(1500), "2.5 s left still shows 3s: no change");
    assert!(host.tick(2100));
    assert!(host.render(60, &theme).join("\n").contains("(2s)"));
    assert!(host.tick(3999), "one millisecond left shows 1s");
    assert!(host.render(60, &theme).join("\n").contains("(1s)"));
    assert!(host.is_open());
    assert!(host.tick(4000), "expiry");
    assert!(!host.is_open());
    assert_eq!(rx.recv().unwrap(), None);

    // A timed confirm expires to None too, and keys after expiry go nowhere.
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: "Delete it".to_string(),
            timeout_ms: Some(1000),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.tick(1000));
    assert_eq!(rx.recv().unwrap(), None);
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::NotOpen);
}

/// Ui methods
#[test]
fn select_confirm_input_editor() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();

    // select: down, enter -> the second option
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["x".to_string(), "y".to_string(), "z".to_string()],
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    let shown = host.render(60, &theme).join("\n");
    assert!(
        shown.contains("Pick") && shown.contains("x") && shown.contains("z"),
        "{shown}"
    );
    press(&mut host, &keymap, "down");
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Some(1));

    // select dismissed with escape
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["x".to_string()],
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // confirm: Yes is first; down reaches No; escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: "Delete it".to_string(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.render(60, &theme).join("\n").contains("Delete it"));
    press(&mut host, &keymap, "enter");
    assert_eq!(rx.recv().unwrap(), Some(true));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: String::new(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "down");
    press(&mut host, &keymap, "enter");
    assert_eq!(rx.recv().unwrap(), Some(false));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: String::new(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // input: type, enter; and escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Input {
            title: "Name".to_string(),
            placeholder: "your name".to_string(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.render(60, &theme).join("\n").contains("your name"));
    host.key(&Key::char('a'), &keymap);
    host.key(&Key::char('b'), &keymap);
    host.key(&Key::press("enter"), &keymap);
    assert_eq!(rx.recv().unwrap(), Some("ab".to_string()));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Input {
            title: "Name".to_string(),
            placeholder: String::new(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // editor: prefilled, typed into, submitted; escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Editor {
            title: "Notes".to_string(),
            prefill: "one".to_string(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    host.key(&Key::char('x'), &keymap);
    host.key(&Key::press("enter"), &keymap);
    assert_eq!(rx.recv().unwrap(), Some("onex".to_string()));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Editor {
            title: "Notes".to_string(),
            prefill: String::new(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // Non-blocking requests change the view model and never open a dialog.
    assert!(host
        .handle(
            UiRequest::Notify {
                text: "saved".to_string(),
                level: Level::Warning
            },
            &mut vm,
            0
        )
        .is_none());
    assert!(matches!(
        vm.blocks.last(),
        Some(Block::Notice { level: Level::Warning, text }) if text == "saved"
    ));
    host.handle(
        UiRequest::SetStatus {
            key: "k".to_string(),
            text: Some("v".to_string()),
        },
        &mut vm,
        0,
    );
    assert_eq!(vm.status.ext_status.get("k").map(String::as_str), Some("v"));
    host.handle(
        UiRequest::SetStatus {
            key: "k".to_string(),
            text: None,
        },
        &mut vm,
        0,
    );
    assert!(vm.status.ext_status.is_empty());
    host.handle(
        UiRequest::SetWidget {
            key: "w".to_string(),
            lines: Some(vec!["line".to_string()]),
            at: Placement::BelowEditor,
        },
        &mut vm,
        0,
    );
    assert_eq!(vm.widgets_below.get("w"), Some(&vec!["line".to_string()]));
    assert!(vm.widgets_above.is_empty());
    host.handle(UiRequest::SetTitle("t".to_string()), &mut vm, 0);
    assert_eq!(vm.title_override.as_deref(), Some("t"));
    assert!(!host.is_open());

    // Editor text round trips are the caller's to answer.
    let (tx, _rx) = channel();
    assert!(matches!(
        host.handle(UiRequest::EditorText { reply: tx }, &mut vm, 0),
        Some(UiRequest::EditorText { .. })
    ));
    assert!(matches!(
        host.handle(UiRequest::SetEditorText("hi".to_string()), &mut vm, 0),
        Some(UiRequest::SetEditorText(t)) if t == "hi"
    ));
}

/// Ui::custom until Done
#[test]
fn custom_component_hosting() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();

    let (tx, rx) = channel();
    host.handle(
        UiRequest::Custom {
            component: Box::new(Counter { n: 0 }),
            opts: CustomOpts {
                overlay: true,
                as_editor: false,
            },
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.is_open());
    assert!(host.is_overlay());
    assert!(host.render(40, &theme).join("\n").contains("count: 0"));
    assert_eq!(press(&mut host, &keymap, "up"), DialogKey::Consumed);
    assert!(host.render(40, &theme).join("\n").contains("count: 1"));
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Some(Value::from(1)));
    assert!(!host.is_open());

    // Escape the component does not use answers None.
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Custom {
            component: Box::new(Counter { n: 0 }),
            opts: CustomOpts::default(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(!host.is_overlay(), "not an overlay unless asked");
    assert_eq!(press(&mut host, &keymap, "escape"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), None);
}

/// The permission text is deterministic and the same for every access class.
#[test]
fn access_prompt_text() {
    assert_eq!(
        access_prompt(&Access::Write(PathBuf::from("/w/a.rs"))),
        ("Allow write?".to_string(), "Write to /w/a.rs".to_string())
    );
    assert_eq!(
        access_prompt(&Access::Read(PathBuf::from("/r/b.rs"))).1,
        "Read /r/b.rs"
    );
    assert_eq!(
        access_prompt(&Access::Exec {
            command: "ls -la".to_string(),
            net: false
        }),
        (
            "Allow command?".to_string(),
            "Run ls -la (no network)".to_string()
        )
    );
    assert_eq!(
        access_prompt(&Access::Net {
            host: "example.test".to_string()
        })
        .1,
        "Connect to example.test"
    );
    assert_eq!(
        access_prompt(&Access::Mcp {
            server: "docs".to_string(),
            tool: "search".to_string(),
            class: ToolClass::Net,
        }),
        (
            "Allow MCP tool?".to_string(),
            "Call docs:search (network class)".to_string()
        )
    );
}

/// The UI proxy blocks the worker until the UI answers, and a missing UI never grants consent.
#[test]
fn ui_proxy_round_trip_and_no_consent_without_ui() {
    let (tx, rx) = channel();
    let mut proxy = UiProxy::new(tx);
    let worker = std::thread::spawn(move || {
        let picked = proxy.select("Pick", &["a".to_string(), "b".to_string()], None);
        let answer = proxy.ask_access(&Access::Net {
            host: "example.test".to_string(),
        });
        (picked, answer)
    });
    match rx.recv().unwrap() {
        UiRequest::Select { options, reply, .. } => {
            assert_eq!(options.len(), 2);
            reply.send(Some(1)).unwrap();
        }
        _ => panic!("expected a select"),
    }
    match rx.recv().unwrap() {
        UiRequest::AskAccess { reply, .. } => reply.send(Answer::Session).unwrap(),
        _ => panic!("expected an access prompt"),
    }
    assert_eq!(worker.join().unwrap(), (Some(1), Answer::Session));

    // A UI that drops the reply without answering counts as no answer.
    let (tx, rx) = channel();
    let mut proxy = UiProxy::new(tx);
    let worker = std::thread::spawn(move || proxy.ask_access(&Access::Write(PathBuf::from("/w/a.rs"))));
    match rx.recv().unwrap() {
        UiRequest::AskAccess { reply, .. } => drop(reply),
        _ => panic!("expected an access prompt"),
    }
    assert_eq!(worker.join().unwrap(), Answer::Deny);

    // No UI thread at all: every dialog is dismissed.
    let (tx, rx) = channel();
    drop(rx);
    let mut proxy = UiProxy::new(tx);
    assert_eq!(
        proxy.ask_access(&Access::Write(PathBuf::from("/w/a.rs"))),
        Answer::Deny
    );
    assert_eq!(proxy.confirm("t", "b", None), None);
    assert_eq!(proxy.select("t", &["a".to_string()], None), None);
}

// ---------------------------------------------------------------- #1512 S2: dangerous commands

const DANGER_KEY: &str = "git-push-force git push --force origin main";

/// A dangerous-command request for `git push --force origin main`, with its reply channel.
fn danger_request(reply: std::sync::mpsc::Sender<DangerAnswer>) -> UiRequest {
    UiRequest::AskDangerous {
        access: Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
        },
        reasons: vec!["git-push-force: git push with a force flag".into()],
        keys: vec![DANGER_KEY.into()],
        reply,
    }
}

/// The dialog shows the four choices, the command and the reason; Deny has the focus, so a
/// stray Enter never runs a dangerous command.
#[test]
fn dangerous_dialog_has_four_choices_deny_focused() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    assert!(host.handle(danger_request(tx), &mut vm, 0).is_none());
    let text = host.render(100, &theme).join("\n");
    assert!(text.contains("Dangerous command"), "{text}");
    assert!(text.contains("git push --force origin main"), "{text}");
    assert!(
        text.contains("git-push-force: git push with a force flag"),
        "{text}"
    );
    assert_eq!(DANGER_LABELS.len(), 4, "{DANGER_LABELS:?}");
    for label in DANGER_LABELS {
        assert!(text.contains(label), "{label}\n{text}");
    }
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Deny);
}

/// "Always allow" is the third choice. Its keys are handed to the driver first: nothing is
/// answered until they are saved to the user file, and the project file is never copied.
#[test]
fn dangerous_dialog_always_writes_user_settings_never_project() {
    let keymap = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    env.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"approval":{"always":["project-only"]}}}"#,
    );
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    host.handle(danger_request(tx), &mut vm, 0);
    // The dialog is on screen before the owner presses anything.
    let shown = host.render(100, &theme).join("\n");
    assert!(shown.contains("git push --force origin main"), "{shown}");
    // Deny is focused: one step up is "Always allow"; each press is followed by the redraw.
    press_shown(&mut host, &keymap, 100, None, "up");
    assert_eq!(
        press_shown(&mut host, &keymap, 100, None, "enter"),
        DialogKey::Closed
    );
    assert!(rx.try_recv().is_err(), "the worker waits for the save");
    let (keys, reply) = host.take_danger_always().expect("always is pending");
    assert_eq!(keys, vec![DANGER_KEY.to_string()]);
    let mut settings = Settings::load(&dirs, &cwd, false);
    append_always(&mut settings, &dirs, &cwd, &keys).expect("the user settings are writable");
    reply.send(DangerAnswer::Always).unwrap();
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Always);
    let user = std::fs::read_to_string(dirs.settings_file()).unwrap();
    assert!(user.contains(DANGER_KEY), "{user}");
    assert!(!user.contains("project-only"), "{user}");
    let project = std::fs::read_to_string(dirs.project_settings_file(&cwd)).unwrap();
    assert!(
        project.contains("project-only") && !project.contains(DANGER_KEY),
        "{project}"
    );
}

/// A failed save is an error for the driver, which then answers `Session`: the command still
/// runs this session, and the user file is not written.
#[test]
fn dangerous_dialog_always_write_failure_still_grants_session() {
    let keymap = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    // The user settings path is a directory, so the write cannot succeed.
    std::fs::create_dir_all(dirs.settings_file()).unwrap();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    host.handle(danger_request(tx), &mut vm, 0);
    let shown = host.render(100, &theme).join("\n");
    for label in DANGER_LABELS {
        assert!(shown.contains(label), "{label}\n{shown}");
    }
    // Deny is focused: one step up is "Always allow". The dialog redraws between the keys.
    press_shown(&mut host, &keymap, 100, None, "up");
    assert_eq!(
        press_shown(&mut host, &keymap, 100, None, "enter"),
        DialogKey::Closed
    );
    assert!(rx.try_recv().is_err(), "the worker waits for the save");
    let (keys, reply) = host.take_danger_always().expect("always is pending");
    let mut settings = Settings::load(&dirs, &cwd, false);
    assert!(append_always(&mut settings, &dirs, &cwd, &keys).is_err());
    reply.send(DangerAnswer::Session).unwrap();
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Session);
}

/// #1772: a command with a second line shows two dialog lines, so a second command on a new
/// line is visibly a second line, and an escape sequence still cannot rewrite the dialog.
#[test]
fn dangerous_dialog_keeps_shell_line_boundaries() {
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, _rx) = channel();
    host.handle(
        UiRequest::AskDangerous {
            access: Access::Exec {
                command: "echo safe # note\nrm -rf /srv/data \u{1b}[2J".into(),
                net: false,
            },
            reasons: vec!["rm-recursive-outside: recursive delete".into()],
            keys: vec!["rm-recursive-outside rm -rf /srv/data".into()],
            reply: tx,
        },
        &mut vm,
        0,
    );
    let text = host.render(100, &theme).join("\n");
    assert!(
        text.lines()
            .any(|l| l.contains("echo safe") && !l.contains("rm -rf")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("rm -rf /srv/data") && !l.contains("echo safe")),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text:?}");
}

/// #2165: a dangerous command long enough to page. Its head is dangerous, its tail benign:
/// 24 shell lines (a dangerous head, whitespace-only padding, a benign tail) plus one reason.
fn long_danger_request(reply: std::sync::mpsc::Sender<DangerAnswer>) -> UiRequest {
    let mut command = "rm -rf /srv/data".to_string();
    for _ in 0..22 {
        command.push_str("\n   ");
    }
    command.push_str("\necho tail-visible");
    UiRequest::AskDangerous {
        access: Access::Exec { command, net: false },
        reasons: vec!["rm-recursive-outside: recursive delete outside the project".into()],
        keys: vec!["rm-recursive-outside rm -rf /srv/data".into()],
        reply,
    }
}

/// #2165: the same 24 rows, each one named, so a test can count the command rows a page shows.
fn numbered_danger_request(reply: std::sync::mpsc::Sender<DangerAnswer>) -> UiRequest {
    let mut command = "rm -rf /srv/data step-01".to_string();
    for n in 2..=23 {
        command.push_str(&format!("\nstep-{n:02}"));
    }
    command.push_str("\necho tail-visible step-24");
    UiRequest::AskDangerous {
        access: Access::Exec { command, net: false },
        // One reason row would be a 25th preview row; the named rows alone are the review here.
        reasons: Vec::new(),
        keys: vec!["rm-recursive-outside rm -rf /srv/data".into()],
        reply,
    }
}

/// #2165: Deny and Escape answer a long dangerous command immediately, before any page review.
#[test]
fn dangerous_dialog_deny_and_escape_answer_before_review() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(long_danger_request(tx), &mut vm, 0);
    host.render(100, &Theme::plain());
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Deny);

    let (tx, rx) = channel();
    host.handle(long_danger_request(tx), &mut vm, 0);
    host.render(100, &Theme::plain());
    assert_eq!(press(&mut host, &keymap, "escape"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Deny);

    // Deny answers even before the dialog has been drawn once: it never waits for a review.
    let (tx, rx) = channel();
    host.handle(long_danger_request(tx), &mut vm, 0);
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Deny);
}

/// #2165: a long dangerous command is paged like a publication preview. The first page shows
/// the command's head, not its tail, and pageup/pagedown move through the rest. An allow needs
/// no separate review: Deny stays the default focus and an answer is the owner's own key.
#[test]
fn dangerous_dialog_pages_long_commands() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    host.handle(long_danger_request(tx), &mut vm, 0);
    let first = host.render(100, &theme).join("\n");
    assert!(first.contains("rm -rf /srv/data"), "{first}");
    assert!(!first.contains("tail-visible"), "{first}");
    assert!(first.contains("pageup/pagedown"), "{first}");
    for label in DANGER_LABELS {
        assert!(first.contains(label), "{label}\n{first}");
    }

    // Page through every row; the last page shows the tail. The bound is on the presses, not
    // on a page count, so a page-size change cannot leave the test one page short.
    let mut presses = 0;
    loop {
        let shown = host.render(100, &theme).join("\n");
        if shown.contains("echo tail-visible") {
            break;
        }
        press(&mut host, &keymap, "pagedown");
        presses += 1;
        assert!(presses < 20, "the pages end");
    }
    let last = host.render(100, &theme).join("\n");
    assert!(last.contains("echo tail-visible"), "{last}");
    // Deny is still focused; three ups reach "Allow once", which answers.
    press(&mut host, &keymap, "up");
    press(&mut host, &keymap, "up");
    press(&mut host, &keymap, "up");
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Once);
}

/// #2165: jumping to the last page still allows; paging is a view, not a gate. Deny is
/// focused and answers at once.
#[test]
fn dangerous_dialog_jumping_to_the_last_page_still_answers() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    host.handle(long_danger_request(tx), &mut vm, 0);
    let mut presses = 0;
    loop {
        let shown = host.render(100, &theme).join("\n");
        if shown.contains("echo tail-visible") {
            break;
        }
        press(&mut host, &keymap, "pagedown");
        presses += 1;
        assert!(presses < 20, "the pages end");
    }
    let last = host.render(100, &theme).join("\n");
    assert!(last.contains("echo tail-visible"), "{last}");
    assert!(!last.contains("rm -rf /srv/data"), "{last}");
    // Deny is focused; three ups reach "Allow once". It answers without paging back.
    press(&mut host, &keymap, "up");
    press(&mut host, &keymap, "up");
    press(&mut host, &keymap, "up");
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Once);
}

/// #2165: a short terminal pages by the rows it can show, so the dialog's head and its choices
/// stay on screen. An allow that many hidden rows follow still answers.
#[test]
fn dangerous_dialog_short_terminal_pages_the_rows_it_can_show() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(numbered_danger_request(tx), &mut vm, 0);
    let first = dialog_box(&mut host, 100, Some(SHORT_ROWS));
    // The dialog fits the rows it was given: nothing is cut off the top.
    assert_eq!(first.len(), SHORT_ROWS, "{first:#?}");
    let shown = first.join("\n");
    assert!(shown.contains("Dangerous command"), "{shown}");
    assert!(shown.contains("step-01") && shown.contains("step-02"), "{shown}");
    assert!(!shown.contains("step-03"), "{shown}");
    assert!(shown.contains("rows 1–2 of 24"), "{shown}");
    assert!(shown.contains("Deny"), "{shown}");

    // "Always allow" is one up from Deny; the dialog answers.
    press_shown(&mut host, &keymap, 100, Some(SHORT_ROWS), "up");
    assert_eq!(
        press_shown(&mut host, &keymap, 100, Some(SHORT_ROWS), "enter"),
        DialogKey::Closed
    );
    // As in the TUI, "Always allow" waits for the driver to save its keys first.
    let (keys, reply) = host.take_danger_always().expect("always is pending");
    assert_eq!(keys, vec!["rm-recursive-outside rm -rf /srv/data".to_string()]);
    reply.send(DangerAnswer::Always).unwrap();
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Always);
}

/// #2165: each page shows only the rows that fit and continues where the previous page
/// ended, so paging down and up walks the whole command without skipping rows.
#[test]
fn dangerous_dialog_short_terminal_pages_do_not_skip_rows() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, _rx) = channel();
    host.handle(numbered_danger_request(tx), &mut vm, 0);
    dialog_box(&mut host, 100, Some(SHORT_ROWS));
    let mut pages = 1;
    loop {
        press_shown(&mut host, &keymap, 100, Some(SHORT_ROWS), "pagedown");
        pages += 1;
        assert!(pages < 20, "the pages end");
        let shown = dialog_box(&mut host, 100, Some(SHORT_ROWS)).join("\n");
        let steps: Vec<&str> = shown.lines().filter(|l| l.contains("step-")).collect();
        assert!(steps.len() <= 2, "a page shows what fits: {shown}");
        if shown.contains("step-24") {
            assert!(shown.contains("rows 23–24 of 24"), "{shown}");
            break;
        }
    }
    assert_eq!(pages, 12, "24 rows in pages of two");
    // Page up to the top: the first page is back.
    for _ in 0..pages {
        press_shown(&mut host, &keymap, 100, Some(SHORT_ROWS), "pageup");
    }
    let top = dialog_box(&mut host, 100, Some(SHORT_ROWS)).join("\n");
    assert!(top.contains("step-01") && top.contains("step-02"), "{top}");
    // The dialog is still open behind the cancelled host.
    host.close_all();
}

/// #1552: a denial of another text does not take back the owner's typed `yes` for the text
/// that was shown and approved.
#[test]
fn a_denial_of_another_text_keeps_the_typed_yes() {
    oxa_proto::owner_reply::clear();
    oxa_proto::owner_reply::await_reply("d10-session", "d10-text-shown");
    oxa_proto::owner_reply::typed("d10-session", "yes");
    // Another question in the same session is denied.
    oxa_proto::owner_reply::clear_for("d10-session", "d10-other-text");
    assert!(
        oxa_proto::owner_reply::take_approved("d10-session", "d10-text-shown"),
        "the approved text is still approved"
    );
    oxa_proto::owner_reply::clear();
}

/// #2165: an 11-row terminal leaves the fullscreen dialog eight rows. The layout compacts so
/// the command and the choices stay on screen; the command is never dropped while an allow
/// remains reachable.
#[test]
fn dangerous_dialog_short_terminal_keeps_the_command_on_screen() {
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, _rx) = channel();
    host.handle(numbered_danger_request(tx), &mut vm, 0);
    // Eight rows: one command row, the four choices and the hint inside the frame.
    let lines = host.render_in(100, Some(8), &Theme::plain());
    let shown = lines.join("\n");
    assert_eq!(lines.len(), 8, "{shown}");
    assert!(shown.contains("Dangerous command"), "{shown}");
    assert!(shown.contains("step-01"), "{shown}");
    for label in DANGER_LABELS {
        assert!(shown.contains(label), "{label}\n{shown}");
    }
    // Seven rows: the hint goes first; the command and choices stay.
    let lines = host.render_in(100, Some(7), &Theme::plain());
    let shown = lines.join("\n");
    assert_eq!(lines.len(), 7, "{shown}");
    assert!(shown.contains("step-01") && shown.contains("Deny"), "{shown}");
    // Six rows: the frame goes too; the command is still there.
    let lines = host.render_in(100, Some(6), &Theme::plain());
    let shown = lines.join("\n");
    assert_eq!(lines.len(), 6, "{shown}");
    assert!(shown.contains("step-01") && shown.contains("Deny"), "{shown}");
    host.close_all();
}

/// #1772: two sessions that loaded their settings before either saved each add a different
/// always key, and both keys are in the user file afterwards.
#[test]
fn concurrent_always_saves_keep_both_keys() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    let mut first = Settings::load(&dirs, &cwd, false);
    let mut second = Settings::load(&dirs, &cwd, false);
    append_always(&mut first, &dirs, &cwd, &["rule-a command a".to_string()]).unwrap();
    append_always(&mut second, &dirs, &cwd, &["rule-b command b".to_string()]).unwrap();
    append_always(&mut second, &dirs, &cwd, &["rule-b command b".to_string()]).unwrap();
    let user = std::fs::read_to_string(dirs.settings_file()).unwrap();
    assert!(user.contains("rule-a command a"), "{user}");
    assert_eq!(user.matches("rule-b command b").count(), 1, "{user}");
}

/// #1552 / #1942: a typed `yes` after a session switch stays bound to the exact text that was
/// shown, and a `yes` typed in another session never approves it.
#[test]
fn a_typed_consent_stays_bound_to_its_request_across_a_session_switch() {
    oxa_proto::owner_reply::clear();
    let (session_a, session_b) = ("d10-session-a", "d10-session-b");
    let awaited = "d10-typed-awaited";
    let other = "d10-typed-other";
    oxa_proto::owner_reply::await_reply(session_a, awaited);
    // A `yes` typed in another session answers that session's request, not this one.
    oxa_proto::owner_reply::typed(session_b, "yes");
    assert!(
        !oxa_proto::owner_reply::take_approved(session_a, awaited),
        "another session's yes never approves it"
    );
    // The switch kept the request: the owner's own `yes` in the asking session approves it.
    oxa_proto::owner_reply::typed(session_a, "yes");
    assert!(
        !oxa_proto::owner_reply::take_approved(session_b, awaited),
        "the token comes back with the asking session"
    );
    assert!(
        oxa_proto::owner_reply::take_approved(session_a, awaited),
        "the text that was shown"
    );
    assert!(
        !oxa_proto::owner_reply::take_approved(session_a, awaited),
        "once only"
    );
    // A different request never consumes it, and any other line from the owner withdraws it.
    oxa_proto::owner_reply::await_reply(session_a, awaited);
    oxa_proto::owner_reply::typed(session_a, "yes");
    assert!(
        !oxa_proto::owner_reply::take_approved(session_a, other),
        "never another request"
    );
    assert!(
        oxa_proto::owner_reply::take_approved(session_a, awaited),
        "bound to its own text"
    );
    oxa_proto::owner_reply::await_reply(session_a, awaited);
    oxa_proto::owner_reply::typed(session_a, "yes, but change the title first");
    assert!(
        !oxa_proto::owner_reply::take_approved(session_a, awaited),
        "any other line withdraws it"
    );
    oxa_proto::owner_reply::clear();
}
