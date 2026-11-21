//! G8 PARITY tests (tui), rewritten in wave H (lane tui) as behavioural tests.
//! One test per PARITY.md row of this area (TUI-* and USE-*), named after the row id
//! (`TUI-012` -> `fn tui_012()`, `USE-003` -> `fn use_003()`). TUI-024 is a recorded omission.
//!
//! How the rows are checked:
//! * Interactive rows drive the real `oxa` in a pseudo-terminal (`oxa_parity::pty`, the shared
//!   rig in `support/tui_rig.rs`) against the fake engine: what the user sees on the screen
//!   (`vt100` model), what the terminal receives (the raw output: OSC sequences, modes, key
//!   protocol), what the engine was sent and what the session file holds. Terminal profiles
//!   (xterm, kitty, tmux) and environment / settings overrides run the capability rows both
//!   ways. Waits are bounded and on screen content; a fixed pause is only ever input timing.
//! * Component rows call oxa-tui directly (`Renderer` + `TestTerminal`, the components, the
//!   width functions).
//! * P rows: the documented pi behaviour is recorded in `fixtures/goldens/tui/{usage,tui}.txt`
//!   (`documented_rows_have_tests` checks it exists); each test cites the pi doc or source it follows.
//! * A rows: the body names its invariant in an `// invariant:` comment.
//! * A needle check stays where the row is about the existence of names (TUI-002), and for
//!   the rows not converted in wave H (TUI-006, TUI-012, TUI-025, TUI-027: their wave G checks).
#![cfg(target_os = "linux")]

#[path = "support/tui_rig.rs"]
mod rig;
#[path = "support/tree_pty.rs"]
mod tree_pty;

use oxa_fake::{FakeCaps, FakeModel, FakeOpts, ScriptTurn, TestEnv};
use oxa_parity::fixture;
use oxa_parity::pty::{KeyEncoding, Screen, TermProfile};
use rig::{editor_text, replies, user_texts, Rig, RigOpts, WAIT};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// This file, for the meta-checks below.
const SELF_SRC: &str = include_str!("tui.rs");

fn repo(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn joined(files: &[&str]) -> String {
    files.iter().map(|f| repo(f)).collect::<Vec<_>>().join("\n")
}

/// The documented pi behaviour recorded for a row id.
fn documented(id: &str) -> String {
    for page in ["goldens/tui/usage.txt", "goldens/tui/tui.txt"] {
        let text = std::fs::read_to_string(fixture(page)).expect("read tui fixture");
        for line in text.lines() {
            if let Some((k, v)) = line.split_once(" | ") {
                if k == id {
                    return v.to_string();
                }
            }
        }
    }
    panic!("no documented pi behaviour recorded for {id}");
}

/// P row about names: oxa carries the documented behaviour's named elements.
fn p_row(id: &str, files: &[&str], needles: &[&str]) {
    let doc = documented(id);
    assert!(!doc.is_empty(), "{id}: empty documented behaviour");
    let text = joined(files);
    let missing: Vec<&str> = needles.iter().copied().filter(|n| !text.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "{id} (pi documents: {doc}): oxa missing {missing:?} in {files:?}"
    );
}

/// Every needle must occur in one of `files` (the wave G source checks, kept for the rows that
/// were not converted).
fn check(files: &[&str], needles: &[&str]) {
    let text = joined(files);
    let missing: Vec<&str> = needles.iter().copied().filter(|n| !text.contains(n)).collect();
    assert!(missing.is_empty(), "missing in {files:?}: {missing:?}");
}

/// The body of a test fn of this file, from its `fn` line to the next attribute.
fn fn_body(fname: &str) -> Option<&'static str> {
    let start = SELF_SRC.find(&format!("fn {fname}()"))?;
    let rest = &SELF_SRC[start..];
    let end = rest[1..].find("\n#[").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

struct ParityRow {
    id: String,
    verdict: String,
    test: String,
}

/// The USE and TUI rows of PARITY.md (both areas point at `tests/tui.rs`).
fn parity_rows() -> Vec<ParityRow> {
    let text = repo("PARITY.md");
    let mut rows = Vec::new();
    for line in text.lines() {
        if !(line.starts_with("| USE-") || line.starts_with("| TUI-")) {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // ["", id, behaviour, V, note, test, ""]
        rows.push(ParityRow {
            id: cells[1].to_string(),
            verdict: cells[3].to_string(),
            test: cells[5].trim_matches('`').to_string(),
        });
    }
    assert_eq!(rows.len(), 37, "USE (12) + TUI (25) rows of PARITY.md");
    rows
}

fn fname(id: &str) -> String {
    id.to_lowercase().replace('-', "_")
}

// ---------------------------------------------------------------- meta-checks

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    for r in parity_rows() {
        if r.test == "omission" {
            assert_eq!(r.verdict, "O", "{}: only O rows may be omitted", r.id);
            continue;
        }
        let f = fname(&r.id);
        assert_eq!(r.test, format!("tui.rs::{f}"), "{}: test column", r.id);
        assert!(
            SELF_SRC.contains(&format!("fn {f}()")),
            "{}: no test fn {f} in tests/tui.rs",
            r.id
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    let mut n = 0;
    for r in parity_rows().iter().filter(|r| r.verdict == "P") {
        assert!(!documented(&r.id).is_empty(), "{}: no documented pi output", r.id);
        n += 1;
    }
    assert_eq!(n, 25, "P rows of USE and TUI (10 + 15)");
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let mut n = 0;
    for r in parity_rows().iter().filter(|r| r.verdict == "A") {
        let body = fn_body(&fname(&r.id)).unwrap_or_else(|| panic!("{}: no fn", r.id));
        assert!(
            body.contains("// invariant:"),
            "{}: no `// invariant:` comment",
            r.id
        );
        n += 1;
    }
    assert_eq!(n, 12, "A rows of USE and TUI (2 + 10)");
}

// ---------------------------------------------------------------- helpers

/// A rig with settings written to the agent dir first.
fn with_settings(settings: serde_json::Value, script: Vec<ScriptTurn>, opts: RigOpts) -> Rig {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    let rig = Rig::start_in(env, script, opts);
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig
}

/// The raw output after `mark`, lossy text.
fn raw_since(rig: &Rig, mark: usize) -> String {
    String::from_utf8_lossy(&rig.pty.raw_since(mark)).into_owned()
}

/// The footer: the last two rows.
/// #1512 S3: the pi footer's extension status row (`no sandbox · …`, `sandbox`, `YOLO full
/// access`) is drawn below the footer. Such a row is skipped to find the footer rows.
fn is_status_row(text: &str) -> bool {
    let t = text.trim();
    t == "sandbox"
        || t.starts_with("no sandbox")
        || t.starts_with("YOLO full access")
        || t.ends_with("YOLO full access")
}

/// The last row that is not an extension status row.
fn last_footer_row(s: &Screen) -> u16 {
    let (rows, _) = s.size();
    let mut row = rows - 1;
    while row > 1 && is_status_row(&s.row(row)) {
        row -= 1;
    }
    row
}

fn footer(s: &Screen) -> (String, String) {
    let last = last_footer_row(s);
    (s.row(last - 1), s.row(last))
}

/// Wait until the editor holds exactly `text`.
fn editor_is(rig: &Rig, text: &str) -> Screen {
    rig.pty
        .wait_for_named(&format!("editor {text:?}"), |s| editor_text(s) == text, WAIT)
        .unwrap()
}

/// A tiny valid PNG (1x1, RGBA).
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00,
    0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05,
    0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
    0x60, 0x82,
];

/// Standard base64 (for OSC 52 payloads).
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// TUI-002 is about names: pi's built-in components exist under their names in oxa-tui.
fn tui_002_names() {
    p_row(
        "TUI-002",
        &[
            "oxa-tui/src/components/text.rs",
            "oxa-tui/src/markdown.rs",
            "oxa-tui/src/components/boxed.rs",
            "oxa-tui/src/components/mod.rs",
            "oxa-tui/src/components/input.rs",
            "oxa-tui/src/components/select_list.rs",
            "oxa-tui/src/components/loader.rs",
            "oxa-tui/src/components/border.rs",
            "oxa-tui/src/components/scrollbar.rs",
        ],
        &[
            "pub struct Text",
            "pub struct Markdown",
            "pub struct Boxed",
            "pub struct Input",
            "pub struct SelectList",
            "pub struct Loader",
            "pub struct Border",
            "pub struct Spacer",
            "pub struct Container",
            "pub struct TruncatedText",
            "pub fn scrollbar_",
        ],
    );
}

// ---------------------------------------------------------------- USE: using the terminal UI

/// Enter submits; Shift+Enter inserts a newline; multi-line paste becomes a paste marker
#[test]
fn use_001() {
    // pi docs: usage.md "Press Enter to send it. Use Shift+Enter to add a line"; tui.md paste
    // markers (`[paste #1 +N lines]`), the full text is what is sent.
    let rig = Rig::start(
        replies(2),
        RigOpts {
            profile: TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    rig.pty.send_text("first line");
    rig.pty.send_key("shift+enter");
    rig.pty.send_text("second line");
    editor_is(&rig, "first line\nsecond line");
    assert!(rig.generating().is_empty(), "Shift+Enter did not submit");
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(1);
    assert_eq!(user_texts(&reqs[0]), vec!["first line\nsecond line"]);
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.settle();
    // A long multi-line paste: a marker in the editor, the text on the wire.
    let pasted: String = (1..=12).map(|i| format!("pasted line {i}\n")).collect();
    let pasted = pasted.trim_end().to_string();
    rig.pty.paste(&pasted);
    let s = editor_is(&rig, "[paste #1 +12 lines]");
    assert!(!s.contains("pasted line 7"), "{s:?}");
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(2);
    assert_eq!(user_texts(&reqs[1]).last().unwrap(), &pasted);
}

/// `@` fuzzy file reference and Tab path completion
#[test]
fn use_002() {
    // pi docs: usage.md "Type @ to search for a file and add it to your prompt. Press Tab to
    // complete a path."
    let env = TestEnv::new();
    env.write("work/README.md", "readme body WORD-77\n");
    env.write("work/src/main.rs", "fn main() {}\n");
    env.write("work/src/lib.rs", "\n");
    let rig = Rig::start_in(env, replies(2), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.pty.send_text("look at @READ");
    rig.pty
        .wait_for_named(
            "the @ list",
            |s| {
                s.rows()
                    .iter()
                    .any(|r| r.contains("README.md") && !r.contains("@READ"))
            },
            WAIT,
        )
        .unwrap();
    rig.pty.send_key("tab");
    editor_is(&rig, "look at @README.md");
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(1);
    let sent = user_texts(&reqs[0]).join("\n");
    assert!(sent.contains("README.md"), "{sent}");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.settle();
    // Tab completes a path.
    rig.pty.send_text("src/ma");
    rig.pty.send_key("tab");
    editor_is(&rig, "src/main.rs");
}

/// Enter while working queues a steering message; Alt+Enter queues a follow-up; Alt+Up restores the queue
#[test]
fn use_003() {
    // pi docs: usage.md "Type a message and press Enter (adjust current task) ... Alt+Enter adds
    // work after the current task. Alt+Up returns queued messages to the editor."
    let rig = Rig::start(
        replies(4),
        RigOpts {
            fake: FakeOpts {
                done_gap_ms: 2500,
                ..FakeOpts::default()
            },
            ..RigOpts::default()
        },
    );
    rig.type_text("long task");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("reply 1").unwrap();
    rig.type_text("steer this");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("Steering: steer this").unwrap();
    rig.type_text("later work");
    rig.pty.send_key("alt+enter");
    let s = rig.pty.wait_for_text("Follow-up: later work").unwrap();
    assert_eq!(editor_text(&s), "");
    // Alt+Up: both back in the editor, the queue is empty.
    rig.pty.send_key("alt+up");
    let s = rig
        .pty
        .wait_for_named(
            "queue restored",
            |s| !s.contains("Steering:") && editor_text(s).contains("later work"),
            WAIT,
        )
        .unwrap();
    assert_eq!(editor_text(&s), "steer this\n\nlater work");
    rig.settle();
    assert_eq!(rig.generating().len(), 1, "nothing queued was sent");
    // A follow-up queued during a run is sent after it.
    rig.pty.send_key("ctrl+c");
    editor_is(&rig, "");
    rig.type_text("second task");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("reply 2").unwrap();
    rig.type_text("then this");
    rig.pty.send_key("alt+enter");
    rig.pty.wait_for_text("Follow-up: then this").unwrap();
    let reqs = rig.wait_requests(3);
    assert_eq!(user_texts(&reqs[2]).last().unwrap(), "then this");
}

/// Escape aborts the run; queued text returns to the editor
#[test]
fn use_004() {
    // pi docs: usage.md "Press Escape to stop the current task. Aborting returns queued messages
    // to the editor." W2: the aborted reply is shown, never sent.
    let rig = Rig::start(
        vec![ScriptTurn::text("PARTIAL ANSWER"), ScriptTurn::text("reply 2")],
        RigOpts {
            fake: FakeOpts {
                done_gap_ms: 4000,
                ..FakeOpts::default()
            },
            ..RigOpts::default()
        },
    );
    rig.type_text("start");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("PARTIAL ANSWER").unwrap();
    rig.type_text("queued text");
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("Steering: queued text").unwrap();
    rig.pty.send_key("escape");
    let s = rig
        .pty
        .wait_for_named(
            "aborted",
            |s| !s.contains("Steering:") && editor_text(s) == "queued text",
            WAIT,
        )
        .unwrap();
    assert!(
        s.contains("PARTIAL ANSWER"),
        "the partial reply stays on screen\n{s:?}"
    );
    rig.settle();
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(2);
    let texts: Vec<String> = reqs[1]
        .messages
        .iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        !texts.iter().any(|t| t.contains("PARTIAL ANSWER")),
        "never sent: {texts:?}"
    );
    assert_eq!(user_texts(&reqs[1]).last().unwrap(), "queued text");
}

/// `!cmd` runs a command and adds its output to the next prompt
#[test]
fn use_005() {
    // pi docs: usage.md "Prefix a command with ! to run it and include its output in the
    // conversation." (enters as an append, W1; runs with the user's own rights)
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.type_text("!echo bang-OUT-42");
    rig.pty.send_key("enter");
    rig.pty
        .wait_for_named(
            "command output",
            |s| s.rows().iter().any(|r| r.trim() == "bang-OUT-42"),
            WAIT,
        )
        .unwrap();
    assert!(rig.generating().is_empty(), "no model request for !cmd");
    rig.settle();
    rig.prompt("what did it print", "reply 1");
    let last = rig.generating().pop().unwrap();
    let users = user_texts(&last);
    assert!(
        users
            .iter()
            .any(|t| t.starts_with("Ran `echo bang-OUT-42`\n```\nbang-OUT-42\n") && t.ends_with("```")),
        "{users:#?}"
    );
}

/// `!!cmd` runs a command without adding output to context
#[test]
fn use_006() {
    // pi docs: usage.md "Use !! when you want to run a command without sending its output to
    // the model."
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.type_text("!!echo quiet-OUT-43");
    rig.pty.send_key("enter");
    rig.pty
        .wait_for_named(
            "command output",
            |s| s.rows().iter().any(|r| r.trim() == "quiet-OUT-43"),
            WAIT,
        )
        .unwrap();
    rig.settle();
    rig.prompt("anything", "reply 1");
    let last = rig.generating().pop().unwrap();
    let all: Vec<String> = last
        .messages
        .iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(!all.iter().any(|t| t.contains("quiet-OUT-43")), "{all:#?}");
}

/// Ctrl+O expands tool output; Ctrl+T toggles thinking blocks
#[test]
fn use_007() {
    // pi docs: usage.md "Press Ctrl+O to expand or collapse tool output. Press Ctrl+T to show or
    // hide thinking blocks."
    let env = TestEnv::new();
    let body: String = (1..=40).map(|i| format!("notes line {i:02}\n")).collect();
    env.write("work/notes.txt", &body);
    let thinking = ScriptTurn {
        reasoning: Some("DEEP-THOUGHT about the notes".to_string()),
        ..ScriptTurn::text("done reading")
    };
    let script = vec![
        ScriptTurn::tool("c1", "read", r#"{"path":"notes.txt"}"#),
        thinking,
    ];
    let rig = Rig::start_in(
        env,
        script,
        RigOpts {
            size: (60, 100),
            ..RigOpts::default()
        },
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.prompt("read the notes", "done reading");
    let s = rig.screen();
    assert!(
        s.contains("more lines") && s.contains("to expand"),
        "collapsed\n{s:?}"
    );
    assert!(!s.contains("notes line 40"), "{s:?}");
    assert!(s.contains("DEEP-THOUGHT"), "thinking shown by default\n{s:?}");
    rig.pty.send_key("ctrl+o");
    let s = rig.pty.wait_for_text("notes line 40").unwrap();
    assert!(!s.contains("more lines"), "{s:?}");
    rig.pty.send_key("ctrl+o");
    rig.pty.wait_for_text_gone("notes line 40").unwrap();
    rig.pty.send_key("ctrl+t");
    let s = rig.pty.wait_for_text_gone("DEEP-THOUGHT").unwrap();
    assert!(s.contains("Thinking..."), "{s:?}");
    rig.pty.send_key("ctrl+t");
    rig.pty.wait_for_text("DEEP-THOUGHT").unwrap();
}

/// Ctrl+G opens the external editor
#[test]
fn use_008() {
    // pi docs: usage.md "press Ctrl+G to work on a longer prompt in your configured external
    // editor": $VISUAL / $EDITOR edits the editor text in a temporary file.
    let env = TestEnv::new();
    let script = env.write(
        "bin/fake-editor",
        "#!/bin/sh\nprintf 'written in the editor\\n' >> \"$1\"\n",
    );
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let rig = Rig::start_in(
        env,
        replies(1),
        RigOpts {
            env: vec![
                ("EDITOR".into(), script.display().to_string()),
                ("VISUAL".into(), String::new()),
            ],
            ..RigOpts::default()
        },
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.type_text("draft");
    rig.pty.send_key("ctrl+g");
    let s = editor_is(&rig, "draftwritten in the editor");
    assert!(
        s.contains("ctrl+d exit"),
        "the screen is back after the editor\n{s:?}"
    );
}

/// Ctrl+Z suspends and resumes with the screen restored
#[test]
fn use_009() {
    // pi docs: usage.md (job control): Ctrl+Z leaves the terminal as it found it and stops the
    // process group; on SIGCONT the screen comes back.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.type_text("keep me");
    let mark = rig.pty.mark();
    rig.pty.send_key("ctrl+z");
    rig.pty.wait_for_raw(mark, b"\x1b[?1049l").unwrap();
    // The pty's process group is orphaned (no job-control shell), so the stop may not happen;
    // either way SIGCONT resumes it.
    rig.pty.send_signal(libc::SIGCONT);
    rig.pty.wait_for_raw(mark, b"\x1b[?1049h").unwrap();
    let s = rig
        .pty
        .wait_for_named(
            "screen restored",
            |s| s.modes().alt_screen && s.contains("ctrl+d exit") && editor_text(s) == "keep me",
            WAIT,
        )
        .unwrap();
    assert!(s.modes().bracketed_paste);
    rig.pty.send_key("ctrl+c");
    editor_is(&rig, "");
    rig.prompt("still alive", "reply 1");
}

/// Drag-and-drop / paste of image and file paths
#[test]
fn use_010() {
    // invariant: W1 (the image enters the transcript as an append) and the wire carries only a
    // placeholder, never the image bytes inline in the prompt text.
    // pi docs: usage.md "Paste an image or drag it into a compatible terminal" (a drop pastes
    // the file's path).
    let env = TestEnv::new();
    let img = env.root().join("work/shot.png");
    std::fs::write(&img, PNG_1X1).unwrap();
    let rig = Rig::start_in(env, replies(1), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.pty.paste(&img.display().to_string());
    rig.pty
        .wait_for_named(
            "attachment",
            |s| s.contains("image") && s.contains("attached"),
            WAIT,
        )
        .unwrap();
    rig.type_text("what is this");
    rig.pty.send_key("enter");
    let reqs = rig.wait_requests(1);
    let sent = user_texts(&reqs[0]).join("\n");
    assert!(sent.contains("[image"), "a placeholder on the wire: {sent}");
    assert!(!sent.contains("iVBOR"), "no inline image bytes");
}

/// `/debug` writes diagnostic information to a file
#[test]
fn use_011() {
    // pi docs: usage.md "Run /debug to write the rendered terminal lines and current session
    // messages to pi-debug.log in the agent directory" (oxa: oxa-debug.log).
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("debug me please", "reply 1");
    rig.slash("/debug");
    rig.pty.wait_for_text("oxa-debug.log").unwrap();
    let log = std::fs::read_to_string(rig.env.agent_dir().join("oxa-debug.log")).expect("the log");
    assert!(log.contains("reply 1"), "rendered lines");
    assert!(log.contains("debug me please"), "session messages");
}

/// Footer: context %, model, thinking, cwd
#[test]
fn use_012() {
    // invariant: the footer is display only; it adds cache, prefill, speed and mode (W6 explicit
    // settings are shown, never inferred from the footer).
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("hello", "reply 1");
    let s = rig.screen();
    let (l1, l2) = footer(&s);
    let cwd = rig.env.work().display().to_string();
    assert!(l1.contains(&cwd), "cwd: {l1:?}");
    assert!(l2.contains("%/"), "context percent: {l2:?}");
    assert!(l2.contains("fake-model • medium"), "model and thinking: {l2:?}");
    assert!(
        l2.contains("cache ") && l2.contains("auto"),
        "cache and mode: {l2:?}"
    );
}

// ---------------------------------------------------------------- TUI: terminal UI

/// Component model: a component renders lines for a width; the renderer diffs lines and writes minimal updates
#[test]
fn tui_001() {
    // pi docs: tui.md "A component renders an array of terminal lines for an available width;
    // the renderer writes minimal updates."
    use oxa_tui::{Component, CursorPos, Frame, Renderer, ScreenMode, TestTerminal, Text, Theme};
    let theme = Theme::plain();
    let mut text = Text::new("one two three four five six seven", 0, 0);
    let lines = text.render(10, &theme);
    assert!(
        lines.len() > 1 && lines.iter().all(|l| oxa_tui::visible_width(l) <= 10),
        "{lines:?}"
    );
    let mut term = TestTerminal::new(20, 5);
    let mut r = Renderer::new(ScreenMode::Alt);
    let frame = |l: &[&str]| Frame {
        lines: l.iter().map(|s| s.to_string()).collect(),
        cursor: Some(CursorPos { row: 0, col: 0 }),
    };
    r.draw(&frame(&["alpha", "bravo", "charlie"]), &mut term);
    let first = term.take_output();
    assert!(first.contains("alpha") && first.contains("charlie"));
    r.draw(&frame(&["alpha", "BRAVO", "charlie"]), &mut term);
    let second = term.take_output();
    assert!(second.contains("BRAVO"), "{second:?}");
    assert!(
        !second.contains("alpha") && !second.contains("charlie"),
        "only the changed line: {second:?}"
    );
    r.draw(&frame(&["alpha", "BRAVO", "charlie"]), &mut term);
    let third = term.take_output();
    assert!(
        !third.contains("BRAVO"),
        "an unchanged frame rewrites no line: {third:?}"
    );

    // The real binary: typing one character rewrites the editor line, not the screen.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.type_text("abc");
    let mark = rig.pty.mark();
    rig.pty.send_text("d");
    editor_is(&rig, "abcd");
    rig.pty.wait_stable(150, WAIT).unwrap();
    let out = raw_since(&rig, mark);
    assert!(
        !out.contains("oxa v") && !out.contains("ctrl+d exit"),
        "minimal update: {out:?}"
    );
}

/// Built-in components: text, box, select list, input, loader, border/spacer, scrollbar, Markdown
#[test]
fn tui_002() {
    // pi docs: tui.md "Built-in components" (the names); the behaviour of each is unit-tested in
    // oxa-tui (b12_basic_components.rs). Here: the names exist. (pi's VStack, HStack,
    // ScrollView, SettingsList, CancellableLoader, MouseRegion and Image components are not in
    // oxa, O-175: the product uses none of them; pi's Box is `Boxed`.)
    tui_002_names();
}

/// Editor: kill ring + yank-pop, undo, history, paste markers, word navigation
#[test]
fn tui_003() {
    // pi docs: tui.md (Editor: kill ring and yank, undo, input history, paste markers, word
    // navigation); keybindings.md tui.editor.* defaults.
    let rig = Rig::start(
        replies(2),
        RigOpts {
            profile: TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    rig.type_text("alpha beta gamma");
    // Ctrl+W kills "gamma", Ctrl+W kills "beta ", Ctrl+Y yanks the last kill, Alt+Y cycles.
    rig.pty.send_key("ctrl+w");
    editor_is(&rig, "alpha beta"); // editor_text trims trailing spaces
    rig.pty.send_key("ctrl+w");
    editor_is(&rig, "alpha");
    rig.pty.send_key("ctrl+y");
    // pi: consecutive kills accumulate into one kill-ring entry ("beta gamma").
    let s = rig
        .pty
        .wait_for(|s| editor_text(s).len() > "alpha".len(), WAIT)
        .unwrap();
    let yanked = editor_text(&s);
    assert!(
        yanked.starts_with("alpha ") && yanked.contains("beta"),
        "{yanked:?}"
    );
    // Undo restores the state before the yank.
    rig.pty.send_key("ctrl+-");
    editor_is(&rig, "alpha");
    // Word navigation: Alt+B to the start of "alpha", insert there.
    rig.pty.send_key("alt+b");
    rig.pty.send_text("X");
    editor_is(&rig, "Xalpha");
    // History: a sent prompt comes back with Up on an empty editor.
    rig.pty.send_key("ctrl+c");
    editor_is(&rig, "");
    rig.prompt("remember this prompt", "reply 1");
    rig.pty.send_key("up");
    editor_is(&rig, "remember this prompt");
    // Paste marker (USE-001 covers sending): a large paste collapses.
    rig.pty.send_key("ctrl+c");
    editor_is(&rig, "");
    rig.pty.paste(&"x\n".repeat(20));
    rig.pty
        .wait_for_named("paste marker", |s| editor_text(s).starts_with("[paste #"), WAIT)
        .unwrap();
}

/// IME support: hardware cursor positioned at the draw cursor
#[test]
fn tui_004() {
    // pi docs: tui.md "A component that displays a text cursor ... the TUI positions the
    // hardware cursor for IME": the terminal's cursor sits at the editor's insertion point.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.type_text("abc");
    let s = rig.pty.wait_stable(150, WAIT).unwrap();
    let row = s.find_row("abc").expect("editor row");
    assert_eq!(s.cursor(), (row, 3), "after `abc`\n{s:?}");
    rig.pty.send_key("left");
    rig.pty
        .wait_for_named("cursor moved left", |s| s.cursor() == (row, 2), WAIT)
        .unwrap();
    rig.pty.send_text("Z");
    editor_is(&rig, "abZc");
    rig.pty
        .wait_for_named("cursor after Z", |s| s.cursor() == (row, 3), WAIT)
        .unwrap();
}

/// Autocomplete: `/` commands, `@` fuzzy files (.gitignore), Tab paths, `autocompleteMaxVisible`
#[test]
fn tui_005() {
    // pi docs: tui.md / settings.md `autocompleteMaxVisible` (default 5); `@` respects
    // .gitignore.
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"autocompleteMaxVisible": 3}).to_string(),
    );
    env.write("work/.gitignore", "secret-ignored.txt\n");
    env.write("work/secret-ignored.txt", "x\n");
    env.write("work/secret-kept.txt", "x\n");
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(env.work())
        .status()
        .ok();
    let rig = Rig::start_in(env, replies(1), RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    // The `/` list sits below the editor: its rows end at the footer (cwd line).
    let list = |s: &Screen| -> Vec<String> {
        let rows = s.rows();
        let rules: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.is_empty() && r.chars().all(|c| c == '─'))
            .map(|(i, _)| i)
            .collect();
        let Some(&bottom) = rules.last() else {
            return Vec::new();
        };
        // #1512 S3: the footer ends above the extension status row.
        let cwd_row = usize::from(last_footer_row(s)).saturating_sub(1);
        rows[bottom + 1..cwd_row.max(bottom + 1)].to_vec()
    };
    rig.pty.send_text("/");
    let s = rig
        .pty
        .wait_for_named(
            "the / list",
            |s| list(s).iter().any(|r| r.contains("model")),
            WAIT,
        )
        .unwrap();
    let rows = list(&s);
    let items = rows
        .iter()
        .filter(|r| r.starts_with("→ ") || r.starts_with("  "))
        .count();
    assert!(items <= 3, "autocompleteMaxVisible 3: {rows:#?}");
    rig.pty.send_key("escape");
    rig.pty
        .wait_for_named("list closed", |s| list(s).is_empty(), WAIT)
        .unwrap();
    rig.pty.send_key("ctrl+c");
    editor_is(&rig, "");
    rig.pty.send_text("@secret");
    let s = rig
        .pty
        .wait_for_named("the @ list", |s| s.contains("secret-kept.txt"), WAIT)
        .unwrap();
    assert!(!s.contains("secret-ignored.txt"), ".gitignore is honoured\n{s:?}");
}

/// Markdown rendering with syntax highlighting, tables, lists, nested code, `codeBlockIndent`
#[test]
fn tui_006() {
    p_row(
        "TUI-006",
        &[
            "oxa-tui/src/highlight.rs",
            "oxa-tui/src/markdown.rs",
            "oxa-config/src/descriptor/display.rs",
        ],
        &["syntect", "table", "markdown.codeBlockIndent"],
    );
}

/// A streamed assistant response mixes a table with labelled Python code, not raw fences.
#[test]
fn code_fences_in_assistant_transcript() {
    let mut turn = ScriptTurn::text(
        "| name | value |\n|---|---|\n| answer | 42 |\n\n```python\ndef hello():\n    return 1\n```\n\nCode complete.",
    );
    turn.chunk = 3;
    let rig = Rig::start(vec![turn], RigOpts::default());
    rig.prompt("show a table and a function", "Code complete.");
    let s = rig.screen();
    assert!(!s.contains("```"), "{s:?}");
    assert!(s.contains("┌─ python "), "{s:?}");
    assert!(s.contains("│  def hello():"), "{s:?}");
    assert!(s.contains("│      return 1"), "{s:?}");
    assert!(
        s.contains("answer") && s.contains("42") && s.contains("┬"),
        "{s:?}"
    );
    assert!(
        s.rows().iter().any(|r| r.contains('└') && r.contains('┘')),
        "{s:?}"
    );
}

/// Mermaid diagrams
#[test]
fn tui_007() {
    // invariant: Mermaid layout is not ported (plan sec. 1); the diagram is shown as a highlighted
    // code block, and the markdown.mermaid setting is kept for compatibility.
    let md = "Diagram:\n\n```mermaid\ngraph TD\n  A-->B\n```\n";
    let rig = Rig::start(vec![ScriptTurn::text(md)], RigOpts::default());
    rig.prompt("diagram", "Diagram:");
    let s = rig.screen();
    assert!(
        s.contains("graph TD") && s.contains("A-->B"),
        "the source as a code block\n{s:?}"
    );
    // Fences are consumed; the labelled frame contains the indented source.
    assert!(!s.contains("```"), "{s:?}");
    assert!(s.rows().iter().any(|r| r.contains("┌─ mermaid ")), "{s:?}");
    assert!(s.rows().iter().any(|r| r.contains("│  graph TD")), "{s:?}");
    assert!(
        s.rows().iter().any(|r| r.contains('└') && r.contains('┘')),
        "{s:?}"
    );
}

/// LaTeX rendering
#[test]
fn tui_008() {
    // invariant: LaTeX is not rendered; math shows as its source text (plan sec. 1).
    let rig = Rig::start(
        vec![ScriptTurn::text("Euler: $e^{i\\pi} + 1 = 0$ done")],
        RigOpts::default(),
    );
    rig.prompt("math", "done");
    assert!(rig.screen().contains("$e^{i\\pi} + 1 = 0$"), "{:?}", rig.screen());
}

/// Hyperlinks (OSC 8) with detection and override
#[test]
fn tui_009() {
    // invariant: detection only, no override (O-175)
    // pi docs: terminal-setup.md: hyperlinks are detected (kitty yes, a plain xterm no). The
    // overrides (`PI_HYPERLINKS`, `terminal.hyperlinks`) are not in oxa (O-175): detection only.
    let md = "See [the docs](https://example.com/docs) now";
    let osc8 = "\x1b]8;;https://example.com/docs";
    for (name, profile, linked) in [
        ("kitty detected", TermProfile::kitty(), true),
        ("xterm not detected", TermProfile::xterm(), false),
    ] {
        let rig = Rig::start(
            vec![ScriptTurn::text(md)],
            RigOpts {
                profile,
                ..RigOpts::default()
            },
        );
        let mark = rig.pty.mark();
        rig.prompt("link please", "now");
        let out = raw_since(&rig, mark);
        assert_eq!(out.contains(osc8), linked, "{name}: OSC 8 in the output");
        assert!(
            rig.screen().contains("the docs"),
            "{name}: the link text is shown"
        );
    }
}

/// Clipboard: OSC 52, wl-copy/xclip
#[test]
fn tui_010() {
    // pi docs: tui.md clipboard: OSC 52 where the terminal supports it, wl-copy (Wayland) or
    // xclip (X11) otherwise; `/copy` copies the last assistant message.
    let reply = "COPY-ME-123";
    // OSC 52 (kitty): the payload is the base64 of the reply.
    let rig = Rig::start(
        vec![ScriptTurn::text(reply)],
        RigOpts {
            profile: TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    rig.prompt("say it", reply);
    let mark = rig.pty.mark();
    rig.slash("/copy");
    let b64 = base64(reply.as_bytes());
    rig.pty
        .wait_for_raw(mark, format!("\x1b]52;c;{b64}").as_bytes())
        .unwrap();
    drop(rig);
    // wl-copy and xclip from PATH.
    for (tool, display) in [
        ("wl-copy", ("WAYLAND_DISPLAY", "wayland-1")),
        ("xclip", ("DISPLAY", ":0")),
    ] {
        let env = TestEnv::new();
        let out = env.root().join(format!("{tool}.out"));
        let bin = env.write(
            &format!("bin/{tool}"),
            &format!("#!/bin/sh\ncat > {}\n", out.display()),
        );
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.parent().unwrap().display());
        let rig = Rig::start_in(
            env,
            vec![ScriptTurn::text(reply)],
            RigOpts {
                env: vec![("PATH".into(), path), (display.0.into(), display.1.into())],
                ..RigOpts::default()
            },
        );
        rig.pty.wait_for_text("ctrl+d exit").unwrap();
        rig.prompt("say it", reply);
        rig.slash("/copy");
        rig.pty.wait_for_text("Copied").unwrap();
        let t = std::time::Instant::now();
        while std::fs::read_to_string(&out).unwrap_or_default() != reply {
            assert!(t.elapsed() < WAIT, "{tool} got the text");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Inline images: kitty and iTerm2 protocols with detection and overrides
#[test]
fn tui_012() {
    // invariant: display only; detection only, no override (O-175)
    p_row(
        "TUI-012",
        &["oxa-tui/src/image.rs", "oxa-tui/src/caps.rs"],
        &["pub fn encode_kitty", "pub fn encode_iterm2"],
    );
}

/// Fullscreen mode: alt screen, follow/scroll, scrollbar modes, wheel lines, prompt jump, resize keeps position
#[test]
fn tui_013() {
    // pi docs: tui.md fullscreen (default): alt screen, the transcript scrolls while the editor
    // and status stay fixed; settings.md fullscreenScrollbar, fullscreenWheelScrollLines;
    // keybindings.md tui.altScreen.* (pageUp, previousPrompt).
    let long: String = (1..=60).map(|i| format!("long line {i:02}\n\n")).collect();
    let rig = Rig::start(
        vec![ScriptTurn::text("short one"), ScriptTurn::text(&long)],
        RigOpts {
            size: (24, 80),
            ..RigOpts::default()
        },
    );
    assert!(
        rig.screen().modes().alt_screen,
        "fullscreen = the alternate screen"
    );
    rig.prompt("first prompt", "short one");
    rig.prompt("second prompt", "long line 60");
    let s = rig.screen();
    assert!(
        s.contains("long line 60") && !s.contains("long line 01"),
        "follows the end\n{s:?}"
    );
    assert!(
        s.contains("ctrl+d exit") || s.contains("fake-model"),
        "status fixed"
    );
    // Page up: older lines, the editor and the footer stay.
    rig.pty.send_key("pageup");
    let s = rig.pty.wait_for_text_gone("long line 60").unwrap();
    assert!(s.contains("fake-model"), "footer fixed\n{s:?}");
    // The scrollbar is shown while scrolled (auto).
    assert!(
        s.rows()
            .iter()
            .any(|r| r.ends_with('█') || r.ends_with('│') || r.ends_with('┃')),
        "scrollbar\n{s:?}"
    );
    // Prompt jump: Ctrl+Up goes to the previous user prompt.
    rig.pty.send_key("ctrl+up");
    rig.pty.wait_for_text("second prompt").unwrap();
    rig.pty.send_key("ctrl+up");
    rig.pty.wait_for_text("first prompt").unwrap();
    // Wheel: three lines per step (auto) down.
    let before = rig.screen().rows();
    rig.pty.send_bytes(b"\x1b[<65;10;5M");
    rig.pty
        .wait_for_named("wheel scrolled", |s| s.rows() != before, WAIT)
        .unwrap();
    // Resize keeps the position (not back at the end).
    rig.pty.resize(30, 70);
    let s = rig
        .pty
        .wait_for_named(
            "resized",
            |s| s.size() == (30, 70) && s.rows().iter().all(|r| r.chars().count() <= 70),
            WAIT,
        )
        .unwrap();
    assert!(!s.contains("long line 60"), "the scroll position is kept\n{s:?}");
    // End: ctrl+end / pagedown back to following.
    for _ in 0..10 {
        rig.pty.send_key("pagedown");
    }
    rig.pty.wait_for_text("long line 60").unwrap();
}

/// Fullscreen: mouse selection, copy-on-select, transcript search next/previous
#[test]
fn tui_014() {
    // pi docs: tui.md fullscreen: mouse selection copies on select (fullscreenCopyOnSelect,
    // default true; OSC 52 where available); Ctrl+Shift+F searches the transcript, Enter /
    // Shift+Enter go to the next / previous match.
    let rig = Rig::start(
        vec![ScriptTurn::text("needle one\n\nfiller\n\nneedle two")],
        RigOpts {
            profile: TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    rig.prompt("find", "needle two");
    let s = rig.screen();
    let row = s.find_row("needle one").unwrap() + 1;
    let col = s.find_col(row - 1, "needle one").unwrap() + 1;
    let mark = rig.pty.mark();
    // Drag over "needle one": press, motion, release (SGR, 1-based).
    rig.pty.send_bytes(format!("\x1b[<0;{col};{row}M").as_bytes());
    rig.pty
        .send_bytes(format!("\x1b[<32;{};{row}M", col + 9).as_bytes());
    rig.pty
        .send_bytes(format!("\x1b[<0;{};{row}m", col + 9).as_bytes());
    let b64 = base64(b"needle one");
    rig.pty
        .wait_for_raw(mark, format!("\x1b]52;c;{b64}").as_bytes())
        .unwrap();
    // Search.
    rig.pty.send_key("ctrl+shift+f");
    rig.pty.wait_for_text("Search:").unwrap();
    rig.pty.send_text("needle");
    let s = rig.pty.wait_for_text("Search: needle").unwrap();
    assert!(s.contains("1/2") || s.contains("2/2"), "match counter\n{s:?}");
    let first = if s.contains("1/2") { "1/2" } else { "2/2" };
    let other = if first == "1/2" { "2/2" } else { "1/2" };
    rig.pty.send_key("enter");
    rig.pty.wait_for_text(other).unwrap();
    rig.pty.send_key("shift+enter");
    rig.pty.wait_for_text(first).unwrap();
    rig.pty.send_key("escape");
    rig.pty.wait_for_text_gone("Search: needle").unwrap();
}

/// Regular mode: finished blocks are printed once to the scrollback
#[test]
fn tui_015() {
    // pi docs: tui.md "Regular mode uses the terminal's normal scrollback; finished blocks are
    // printed once." (`--tui-mode regular`, settings.md tuiMode).
    let rig = Rig::start(
        vec![
            ScriptTurn::text("FIRST-BLOCK"),
            ScriptTurn::text("SECOND-BLOCK"),
            ScriptTurn::text("THIRD-BLOCK"),
        ],
        RigOpts {
            size: (12, 80),
            args: vec!["--tui-mode".into(), "regular".into()],
            ..RigOpts::default()
        },
    );
    assert!(!rig.screen().modes().alt_screen, "the normal screen");
    rig.prompt("one", "FIRST-BLOCK");
    rig.prompt("two", "SECOND-BLOCK");
    rig.prompt("three", "THIRD-BLOCK");
    rig.pty.resize(14, 70);
    rig.pty
        .wait_for_named("resized", |s| s.size() == (14, 70), WAIT)
        .unwrap();
    rig.settle();
    let s = rig.screen();
    let all = s.scrollback_text(); // scrollback + the visible screen
    for b in ["FIRST-BLOCK", "SECOND-BLOCK", "THIRD-BLOCK"] {
        assert_eq!(all.matches(b).count(), 1, "{b} printed once\n{all}");
    }
}

/// Exit output: transcript or resume hint
#[test]
fn tui_016() {
    // pi docs: settings.md fullscreenExitOutput "transcript" (default) | "resume-hint";
    // interactive-mode.ts: then `To resume this session: oxa --session <id>` (nothing without a
    // conversation).
    for (setting, transcript) in [
        (json!({}), true),
        (json!({"fullscreenExitOutput": "resume-hint"}), false),
    ] {
        let mut rig = with_settings(setting, vec![ScriptTurn::text("THE-ANSWER")], RigOpts::default());
        rig.prompt("THE-QUESTION", "THE-ANSWER");
        let id = rig
            .session_files()
            .first()
            .and_then(|f| {
                rig::session_lines(f)
                    .first()
                    .and_then(|h| h["id"].as_str().map(str::to_string))
            })
            .unwrap();
        rig.quit();
        let s = rig.screen();
        assert!(!s.modes().alt_screen);
        let all = s.scrollback_text(); // scrollback + the visible screen
        assert!(
            all.contains(&format!("To resume this session: oxa --session {id}")),
            "{all}"
        );
        assert_eq!(
            all.contains("THE-QUESTION") && all.contains("THE-ANSWER"),
            transcript,
            "{all}"
        );
    }
    // No conversation: no hint.
    let mut rig = Rig::start(replies(1), RigOpts::default());
    rig.quit();
    assert!(!rig.screen().contains("To resume"), "{:?}", rig.screen());
}

/// Thinking blocks hide/collapse; tool output expand/collapse; live output; diff view for edits
#[test]
fn tui_017() {
    // pi docs: tui.md: thinking hides (Ctrl+T, settings.md hideThinkingBlock), tool output
    // collapses and expands (Ctrl+O), a running command's output is shown live, an edit shows
    // a diff.
    let env = TestEnv::new();
    env.write(
        "work/code.txt",
        "keep this line\nold value here\nkeep that line\n",
    );
    let script = vec![
        ScriptTurn::tool(
            "c1",
            "bash",
            r#"{"command":"echo LIVE-1; sleep 1; echo LIVE-2; sleep 1; echo LIVE-3"}"#,
        ),
        ScriptTurn::tool(
            "c2",
            "edit",
            r#"{"path":"code.txt","oldText":"old value here","newText":"new value here"}"#,
        ),
        ScriptTurn::text("all done"),
    ];
    let env_settings = env.write(
        "home/.oxa/agent/settings.json",
        &json!({"hideThinkingBlock": true}).to_string(),
    );
    let _ = env_settings;
    let rig = Rig::start_in(
        env,
        script,
        RigOpts {
            size: (50, 100),
            ..RigOpts::default()
        },
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.type_text("work");
    rig.pty.send_key("enter");
    // Live: the first output line is on screen before the command ends (rows that are
    // exactly the output, not the command line that names them).
    let line = |s: &Screen, t: &str| s.rows().iter().any(|r| r.trim() == t);
    let s = rig
        .pty
        .wait_for_named("LIVE-1 shown", |s| line(s, "LIVE-1"), WAIT)
        .unwrap();
    assert!(
        !line(&s, "LIVE-3"),
        "the output was shown while the command ran\n{s:?}"
    );
    rig.pty
        .wait_for_named(
            "LIVE-3 shown",
            |s| line(s, "LIVE-3") || s.contains("all done"),
            WAIT,
        )
        .unwrap();
    rig.pty.wait_for_text("all done").unwrap();
    let s = rig.settle();
    // The edit's diff: removed and added lines.
    assert!(
        s.rows()
            .iter()
            .any(|r| r.contains("-") && r.contains("old value here")),
        "removed line\n{s:?}"
    );
    assert!(
        s.rows()
            .iter()
            .any(|r| r.contains("+") && r.contains("new value here")),
        "added line\n{s:?}"
    );
    assert_eq!(
        std::fs::read_to_string(rig.env.work().join("code.txt")).unwrap(),
        "keep this line\nnew value here\nkeep that line\n"
    );
}

/// Startup header and loaded-resource listing (`quietStartup`)
#[test]
fn tui_018() {
    // pi docs: tui.md / usage.md "The startup header lists the instructions and resources Pi
    // loaded"; settings.md quietStartup (true hides it; "header" keeps the header only).
    let setup = |settings: serde_json::Value| {
        let env = TestEnv::new();
        env.write("home/.oxa/agent/settings.json", &settings.to_string());
        env.write("work/AGENTS.md", "Be terse.\n");
        env.write(
            "home/.oxa/agent/skills/hello-skill/SKILL.md",
            "---\nname: hello-skill\ndescription: says hello\n---\nSay hello.\n",
        );
        env.write(
            "home/.oxa/agent/prompts/review.md",
            "---\ndescription: review\n---\nReview it.\n",
        );
        Rig::start_in(env, replies(1), RigOpts::default())
    };
    let rig = setup(json!({}));
    let s = rig.pty.wait_for_text("[Skills]").unwrap();
    assert!(s.contains("oxa v") && s.contains("ctrl+d exit"), "header\n{s:?}");
    let ctx = s.find_row("[Context]").expect("context section");
    assert!(s.row(ctx + 1).contains("AGENTS.md"), "{s:?}");
    let sk = s.find_row("[Skills]").unwrap();
    assert!(s.row(sk + 1).contains("hello-skill"), "{s:?}");
    let pr = s.find_row("[Prompts]").expect("prompts");
    assert!(s.row(pr + 1).contains("/review"), "{s:?}");
    drop(rig);
    let rig = setup(json!({"quietStartup": "header"}));
    let s = rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.pty.wait_stable(200, WAIT).unwrap();
    assert!(
        s.contains("oxa v") && !s.contains("[Skills]"),
        "header only\n{s:?}"
    );
    drop(rig);
    let rig = setup(json!({"quietStartup": true}));
    let s = rig.pty.wait_for(|s| s.contains("fake-model"), WAIT).unwrap();
    rig.pty.wait_stable(200, WAIT).unwrap();
    assert!(!s.contains("oxa v") && !s.contains("[Skills]"), "quiet\n{s:?}");
}

/// Selectors: model, thinking, theme, settings, sessions, tree, scoped models, MCP
#[test]
fn tui_019() {
    // pi docs: tui.md / usage.md: the selectors open from their commands and close with Escape.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.prompt("something", "reply 1");
    for (cmd, title) in [
        ("/model", "fake-model"),
        ("/thinking", "minimal"),
        ("/theme", "dark"),
        ("/settings", "Settings"),
        ("/resume", "Resume Session"),
        ("/tree", "Session Tree"),
        ("/scoped-models", "fake-model"),
        ("/mcp", "MCP"),
    ] {
        let mark = rig.pty.mark();
        rig.slash(cmd);
        rig.pty.wait_for_raw(mark, title.as_bytes()).unwrap();
        rig.pty.wait_for_text(title).unwrap();
        rig.pty.send_key("escape");
        rig.pty
            .wait_for_named(
                &format!("{cmd} closed"),
                |s| editor_text(s).is_empty() && s.contains("ctrl+d exit"),
                WAIT,
            )
            .unwrap();
        rig.pty.wait_stable(150, WAIT).unwrap();
    }
}

/// #2256: the model-location badge follows the active model. Switching from a cloud model
/// (`cloud · <provider>`) to a local one shows `local`, and back.
#[test]
fn model_switch_route_badge_follows_the_active_model() {
    let cloud = FakeModel {
        alias: "cloud-model".to_string(),
        cap: 1 << 20,
        max_output: None,
        caps: Some(FakeCaps {
            remote_provider: Some("openai"),
            ..FakeCaps::rust_moe()
        }),
    };
    let rig = Rig::start(
        replies(1),
        RigOpts {
            size: (30, 120),
            fake: FakeOpts {
                model: "cloud-model".to_string(),
                models: vec![cloud, FakeModel::moe("local-model", 1 << 20)],
                ..FakeOpts::default()
            },
            ..RigOpts::default()
        },
    );
    let s = rig.pty.wait_for_text("cloud · openai · cloud-model").unwrap();
    assert!(
        footer(&s).1.contains("cloud · openai · cloud-model"),
        "the footer names the cloud provider and the model: {:?}",
        footer(&s).1
    );
    rig.slash("/model local-model");
    let s = rig
        .pty
        .wait_for_named(
            "the footer follows the switch",
            |s| footer(s).1.contains("local · local-model"),
            WAIT,
        )
        .unwrap();
    assert!(
        !footer(&s).1.contains("cloud"),
        "the cloud badge is gone after the switch to a local model: {:?}",
        footer(&s).1
    );
    rig.slash("/model cloud-model");
    let s = rig
        .pty
        .wait_for_named(
            "the footer follows the switch back",
            |s| footer(s).1.contains("cloud · openai · cloud-model"),
            WAIT,
        )
        .unwrap();
    assert!(
        !footer(&s).1.contains("local ·"),
        "a cloud model is not shown as local: {:?}",
        footer(&s).1
    );
}

/// Keyboard: kitty protocol, legacy sequences, ESC timeout
#[test]
fn tui_020() {
    // invariant: kitty keyboard protocol flags 7 with the query; no modifyOtherKeys fallback; one constant ESC timeout of 10 ms counted from the ESC (O-175)
    // pi terminal.ts: push kitty flags 7 and query (`CSI > 7 u`, `CSI ? u`); legacy sequences
    // parse; a lone ESC becomes Escape after the timeout (one constant in oxa: pi's
    // `PI_TUI_ESC_TIMEOUT` and SSH default are not, O-175).
    let kitty = Rig::start(
        replies(1),
        RigOpts {
            profile: TermProfile::kitty(),
            ..RigOpts::default()
        },
    );
    assert_eq!(kitty.pty.modes().kitty_flags(), 7);
    let raw = String::from_utf8_lossy(&kitty.pty.raw()).into_owned();
    assert!(
        raw.contains("\x1b[>7u") && raw.contains("\x1b[?u"),
        "push + query"
    );
    kitty.pty.send_key_as("alt+b", KeyEncoding::Auto);
    kitty.type_text("kitty ok");
    drop(kitty);
    let legacy = Rig::start(replies(1), RigOpts::default());
    assert_eq!(legacy.pty.modes().kitty_flags(), 0);
    legacy.type_text("one two");
    legacy.pty.send_key_as("alt+b", KeyEncoding::Legacy);
    legacy.pty.send_text("X");
    editor_is(&legacy, "one Xtwo");
    drop(legacy);
    // ESC timeout: ESC then `x` 150 ms later is Escape (the timeout is 10 ms), then `x`.
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.pty.send_bytes(b"\x1b");
    std::thread::sleep(Duration::from_millis(150));
    rig.pty.send_bytes(b"x");
    rig.pty.send_text("|");
    let s = rig.pty.wait_for(|s| editor_text(s).ends_with('|'), WAIT).unwrap();
    assert_eq!(editor_text(&s), "x|");
}

/// Terminal restored after a panic or signal
#[test]
fn tui_021() {
    // invariant: the terminal is restored on panic and on SIGTERM/SIGHUP before exit (oxa only).
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        let mut rig = Rig::start(
            replies(1),
            RigOpts {
                profile: TermProfile::kitty(),
                ..RigOpts::default()
            },
        );
        assert!(rig.screen().modes().alt_screen);
        rig.pty.send_signal(sig);
        rig.pty.wait_exit(WAIT).unwrap();
        let m = rig.screen().modes().clone();
        assert!(
            !m.alt_screen && !m.bracketed_paste && !m.mouse,
            "signal {sig}: {m:?}"
        );
        assert!(m.kitty_stack.is_empty(), "signal {sig}: kitty flags popped");
        assert!(rig.screen().cursor_visible(), "signal {sig}: cursor shown");
    }
    // The panic path writes the same restore sequence (unit-tested in oxa-tui a12); its bytes
    // switch everything off.
    let seq = oxa_tui::restore_sequence();
    for off in ["\x1b[?1049l", "\x1b[?2004l", "\x1b[<u", "\x1b[?25h"] {
        assert!(seq.contains(off), "{off:?}");
    }
}

/// Wide characters, ANSI-aware wrap and truncate, shrink clear
#[test]
fn tui_022() {
    // pi docs: tui.md: wide characters take two cells, wrapping and truncation keep SGR state and
    // never split a character; settings.md terminal.clearOnShrink.
    use oxa_tui::{truncate_to_width, visible_width, wrap_ansi};
    assert_eq!(visible_width("你好"), 4);
    assert_eq!(visible_width("\x1b[31mab\x1b[0m"), 2);
    let cut = truncate_to_width("你好世界", 5, "");
    assert!(visible_width(&cut) <= 5 && !cut.contains('\u{fffd}'), "{cut:?}");
    let wrapped = wrap_ansi("\x1b[1mbold words that wrap around\x1b[0m", 10);
    assert!(
        wrapped.len() > 1 && wrapped.iter().all(|l| visible_width(l) <= 10),
        "{wrapped:?}"
    );
    assert!(
        wrapped[1].contains("\x1b[1m"),
        "SGR re-opened on the next line: {wrapped:?}"
    );
    // On screen: a CJK reply at 30 columns wraps without overflow.
    let cjk = "你好世界".repeat(20);
    let rig = Rig::start(
        vec![ScriptTurn::text(&cjk)],
        RigOpts {
            size: (24, 30),
            ..RigOpts::default()
        },
    );
    rig.prompt("wide", "你好世界");
    let s = rig.screen();
    assert!(s.rows().iter().all(|r| visible_width(r) <= 30), "{s:?}");
    assert!(
        s.rows().iter().filter(|r| r.contains("你好")).count() >= 3,
        "wrapped\n{s:?}"
    );
}

/// tmux setup (extended keys, passthrough)
#[test]
fn tui_023() {
    // invariant: OSC 52 passthrough; extended keys through tmux are not in oxa (O-175)
    // pi docs: tmux.md: inside tmux OSC 52 goes through the DCS passthrough; images and OSC 8
    // are off by detection. (pi's extended-keys request, modifyOtherKeys, is not in oxa: O-175.)
    let rig = Rig::start(
        replies(1),
        RigOpts {
            profile: TermProfile::tmux(),
            ..RigOpts::default()
        },
    );
    rig.prompt("say", "reply 1");
    let mark = rig.pty.mark();
    rig.slash("/copy");
    rig.pty.wait_for_raw(mark, b"\x1bPtmux;\x1b\x1b]52;c;").unwrap();
}

/// Custom screens, overlays and custom editor from extensions
#[test]
fn tui_025() {
    // invariant: extension components run as JS `render(width)` + `handleInput`, shimmed onto the
    // oxa-tui Component trait; they never get a second terminal renderer (oxa-ext owns the shim).
    check(
        &["oxa-ext/js/tui.js", "oxa-ext/src/api_ui.rs"],
        &["handleInput", "render("],
    );
}

/// Footer / status line shows mode, cache hit, prefill progress and tok/s
#[test]
fn tui_026() {
    // invariant: the status line is derived from engine usage; it never affects the wire (W6).
    let rig = Rig::start(replies(2), RigOpts::default());
    rig.prompt("first", "reply 1");
    rig.prompt("second", "reply 2");
    let (_, l2) = footer(&rig.screen());
    let last = rig.generating().pop().unwrap();
    assert!(l2.contains("cache "), "cache hit: {l2:?}");
    assert!(l2.contains("auto"), "mode: {l2:?}");
    // Alt+M cycles the mode; the footer follows.
    rig.pty.send_key("alt+m");
    rig.pty
        // The mode word itself (the context line also shows `(auto)` compaction).
        .wait_for_named(
            "mode changed",
            |s| !footer(s).1.split_whitespace().any(|w| w == "auto"),
            WAIT,
        )
        .unwrap();
    // Prefill progress and speed come from the engine's comments and timings (the fake sends
    // neither for a chat request): the footer renders them from the status.
    use oxa_interactive::{render_footer, Activity, ViewModel};
    let mut vm = ViewModel::default();
    vm.status.model = "m".into();
    vm.status.n_ctx = 1000;
    vm.status.activity = Activity::Prefill {
        done: 1200,
        total: 8000,
        secs: 3,
    };
    vm.status.tok_per_s = Some(42.5);
    vm.status.mode = Some(oxa_proto::Mode::Workspace);
    let text = render_footer(&vm, 120, &oxa_tui::Theme::plain()).join("\n");
    assert!(
        text.contains("prefill 1200/8000") && text.contains("42.5 tok/s") && text.contains("standard"),
        "{text}"
    );
    // Nothing of the footer is on the wire.
    let all: Vec<String> = last
        .messages
        .iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        !all.iter().any(|t| t.contains("cache ") || t.contains("%/")),
        "{all:?}"
    );
}

/// #1512 S2: a dangerous command stops at the four-choice dialog in the real TUI. Deny has the
/// focus, so Enter refuses it, nothing runs and the model hears the refusal.
#[test]
fn dangerous_command_asks_in_the_real_tui() {
    let env = TestEnv::new();
    let script = vec![
        ScriptTurn::tool("c1", "bash", r#"{"command":"git push --force origin main"}"#),
        ScriptTurn::text("refused as expected"),
    ];
    let rig = Rig::start_in(env, script, RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.type_text("push the branch");
    rig.pty.send_key("enter");
    let s = rig.pty.wait_for_text("Dangerous command").unwrap();
    for label in [
        "Allow once",
        "Allow for this session",
        "Always allow (saved to your user settings)",
        "Deny",
    ] {
        assert!(s.contains(label), "{label}\n{s:?}");
    }
    rig.pty.send_key("enter");
    rig.pty.wait_for_text("the user declined this action").unwrap();
    rig.pty.wait_for_text("refused as expected").unwrap();
}

/// #2165: a long dangerous command is paged in the real fullscreen TUI. The first page shows
/// the command's head, pageup/pagedown move through the rest, paging is a view rather than a
/// gate on the answer, and Deny answers immediately. Synthetic command: nothing dangerous is
/// ever allowed to run.
#[test]
fn long_dangerous_command_pages_in_the_real_tui() {
    // The dialog's own framed rows: the transcript's tool header also shows the command.
    fn dialog_rows(s: &Screen) -> Vec<String> {
        let rows = s.rows();
        let start = rows
            .iter()
            .position(|r| r.contains("Dangerous command"))
            .expect("the dialog title");
        let end = rows
            .iter()
            .position(|r| r.contains("up/down move"))
            .expect("the dialog hint");
        rows[start..=end].to_vec()
    }
    let env = TestEnv::new();
    let mut command = "rm -rf /srv/data".to_string();
    for _ in 0..22 {
        command.push_str("\n   ");
    }
    command.push_str("\necho tail-visible");
    let script = vec![
        ScriptTurn::tool("c1", "bash", &json!({ "command": command }).to_string()),
        ScriptTurn::text("refused as expected"),
    ];
    let rig = Rig::start_in(env, script, RigOpts::default());
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.type_text("push the branch");
    rig.pty.send_key("enter");
    let s = rig.pty.wait_for_text("Dangerous command").unwrap();
    let requests_at_ask = rig.generating().len();
    let shown = dialog_rows(&s).join("\n");
    assert!(shown.contains("rm -rf /srv/data"), "{shown}");
    assert!(!shown.contains("tail-visible"), "{shown}");
    assert!(shown.contains("pageup/pagedown"), "{shown}");

    // Two ups from Deny reach "Allow for this session"; the dialog stays open and the page
    // view is unchanged, so focus moves without running anything.
    rig.pty.send_key("up");
    rig.pty.send_key("up");
    let s = rig.pty.wait_for_text("→ Allow for this session").unwrap();
    assert!(s.contains("Dangerous command"), "{s:?}");
    assert!(!s.contains("refused as expected"), "{s:?}");
    assert_eq!(
        rig.generating().len(),
        requests_at_ask,
        "no new request: nothing ran"
    );

    // Page to the end: the benign tail becomes visible in the dialog itself. The bound is on
    // the presses, not on a page count, so a page-size change cannot leave the test short.
    for _ in 0..40 {
        rig.pty.send_key("pagedown");
    }
    rig.pty
        .wait_for_named(
            "the last preview page",
            |s| dialog_rows(s).iter().any(|r| r.contains("echo tail-visible")),
            WAIT,
        )
        .unwrap();

    // Deny still answers immediately; the model hears the refusal.
    rig.pty.send_key("escape");
    rig.pty.wait_for_text("the user declined this action").unwrap();
    rig.pty.wait_for_text("refused as expected").unwrap();
}

/// #2165: a short fullscreen terminal pages a long dangerous command by the rows it can show.
/// A ten-row page would push the dialog's head off the screen, so the page shrinks: the title,
/// the choices and one page of rows stay visible; paging is optional and Deny answers at once.
/// Synthetic command: nothing dangerous is ever allowed to run.
#[test]
fn short_terminal_dangerous_command_pages_visible_rows_in_the_real_tui() {
    // The dialog's own framed rows: the transcript's tool header also shows the command.
    fn dialog_rows(s: &Screen) -> Vec<String> {
        let rows = s.rows();
        let start = rows
            .iter()
            .position(|r| r.contains("Dangerous command"))
            .expect("the dialog title");
        let end = rows
            .iter()
            .position(|r| r.contains("up/down move"))
            .expect("the dialog hint");
        rows[start..=end].to_vec()
    }
    let env = TestEnv::new();
    let mut command = "rm -rf /srv/data step-01".to_string();
    for n in 2..=23 {
        command.push_str(&format!("\nstep-{n:02}"));
    }
    command.push_str("\necho tail-visible step-24");
    let script = vec![
        ScriptTurn::tool("c1", "bash", &json!({ "command": command }).to_string()),
        ScriptTurn::text("refused as expected"),
    ];
    let rig = Rig::start_in(
        env,
        script,
        RigOpts {
            size: (18, 100),
            ..RigOpts::default()
        },
    );
    rig.pty.wait_for_text("ctrl+d exit").unwrap();
    rig.type_text("push the branch");
    rig.pty.send_key("enter");
    let s = rig.pty.wait_for_text("Dangerous command").unwrap();
    let requests_at_ask = rig.generating().len();

    // The dialog's head and foot are both on screen; a ten-row page would have cut the title.
    let rows = s.rows();
    let hint = rows
        .iter()
        .position(|r| r.contains("up/down move"))
        .expect("the dialog hint");
    assert!(
        rows.get(hint + 1).is_some_and(|r| r.contains('╰')),
        "the dialog's bottom edge is on screen: {s:?}"
    );
    let shown = dialog_rows(&s).join("\n");
    assert!(shown.contains("step-01"), "{shown}");
    for label in ["Allow once", "Deny"] {
        assert!(shown.contains(label), "{label}\n{shown}");
    }
    // The page is smaller than a tall terminal's ten rows and names exactly the rows it shows.
    // The counter's total is the command's rows plus any rule reason rows below them.
    let page = dialog_rows(&s).iter().filter(|r| r.contains("step-")).count();
    assert!(
        (1..10).contains(&page),
        "a short terminal shows {page} command rows: {shown}"
    );
    let total: usize = shown
        .split(" of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|total| total.parse().ok())
        .expect("the row counter names every preview row");
    assert!(
        total >= 24,
        "the command's rows are all counted: {total}\n{shown}"
    );
    assert!(shown.contains(&format!("rows 1–{page} of {total}")), "{shown}");
    assert!(
        !shown.contains(&format!("step-{:02}", page + 1)),
        "the next row is on the next page: {shown}"
    );

    // Two ups from Deny reach "Allow for this session"; the dialog stays open and nothing runs.
    rig.pty.send_key("up");
    rig.pty.send_key("up");
    let s = rig.pty.wait_for_text("→ Allow for this session").unwrap();
    assert!(s.contains("Dangerous command"), "{s:?}");
    assert!(!s.contains("refused as expected"), "{s:?}");
    assert_eq!(
        rig.generating().len(),
        requests_at_ask,
        "no new request: nothing ran"
    );

    // Page to the end: the benign tail becomes visible in the dialog itself. The bound is on
    // the presses, not on a page count, so a page-size change cannot leave the test short.
    for _ in 0..40 {
        rig.pty.send_key("pagedown");
    }
    rig.pty
        .wait_for_named(
            "the last preview page",
            |s| dialog_rows(s).iter().any(|r| r.contains("step-24")),
            WAIT,
        )
        .unwrap();

    // Deny still answers immediately; the model hears the refusal.
    rig.pty.send_key("escape");
    rig.pty.wait_for_text("the user declined this action").unwrap();
    rig.pty.wait_for_text("refused as expected").unwrap();
}

/// Permission prompt Once / Session / Deny; mode cycle; timed dialogs
#[test]
fn tui_027() {
    // invariant: Deny is the default focus and an unanswered timed dialog resolves to Deny
    // (no tool runs without an explicit answer; W-guard permissions).
    check(
        &["oxa-interactive/src/dialogs.rs"],
        &[
            "Answer::Once",
            "Answer::Session",
            "Answer::Deny",
            "pub fn cycle_mode",
            "timeout_ms",
        ],
    );
}

/// #1512 S3: the real TUI over a PTY tells the truth about the OS sandbox. A default
/// (attended) start shows "no sandbox · dangerous commands ask"; `--sandbox` shows "sandbox"
/// and never the "no sandbox" text.
#[test]
fn sandbox_guard_is_shown_in_the_real_tui() {
    let rig = Rig::start(replies(1), RigOpts::default());
    rig.pty
        .wait_for_named(
            "no-sandbox guard",
            |s| {
                s.rows()
                    .iter()
                    .any(|r| r.contains("no sandbox · dangerous commands ask"))
            },
            WAIT,
        )
        .unwrap();
    drop(rig);
    let rig = Rig::start(
        replies(1),
        RigOpts {
            args: vec!["--sandbox".into()],
            ..RigOpts::default()
        },
    );
    let s = rig
        .pty
        .wait_for_named(
            "sandbox guard",
            |s| s.rows().iter().any(|r| r.trim() == "sandbox"),
            WAIT,
        )
        .unwrap();
    assert!(!s.rows().iter().any(|r| r.contains("no sandbox")), "{s:?}");
}
