//! G9 PARITY tests (keys_settings).
//! One test per PARITY.md row of this area, named after the row id (`CLI-012` -> `fn cli_012()`).
//! The four tests below are the meta-checks of the task; the row tests are added by the task.
//! Helpers: `oxa_parity::{oxa_bin, fake_bin, run_oxa, fixture}`, `oxa_fake::TestEnv`.
//!
//! How the rows are observed (the binary only, nothing is linked): `oxa -p hi` against
//! `oxa-fake`, which records every request it gets. A setting is seen through the request
//! body (`model`, `tools`, `chat_template_kwargs.enable_thinking`, tool results on the wire),
//! through the diagnostics on stderr (`Warning:` lines), through the files it writes (session
//! files, npm calls, hook notices) or through `oxa config` (the merged listing). Behaviour that
//! only exists in the interactive TUI (keybindings, display and terminal settings, selectors)
//! is driven through the pty harness (`oxa_parity::pty`, wave H): the shared `tui` rig below
//! starts the real `oxa` in a pseudo-terminal against an in-process fake engine, presses keys
//! and asserts on the screen, the raw output and the requests the fake recorded.
//!
//! Row verdicts and their tests (see PARITY.md): P rows carry `// pi docs:` and compare with
//! pi 1.5.2's documentation (`packages/coding-agent/docs/settings.md`, `keybindings.md`);
//! A rows carry `// invariant:` and assert the adapted behaviour; O rows assert the
//! unsupported-in-oxa behaviour (a `Warning:` that the setting is accepted and ignored).
//! A row that fails is RED: the assertion message names the cause, and the test is not hacked
//! around.
//!
//! Run: `cargo build -p oxa-cli -p oxa-fake`, then `cargo test -p oxa-parity --test keys_settings`.

use oxa_fake::TestEnv;
use oxa_parity::{fake_bin, run_oxa};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::Duration;

/// The 1x1 PNG of the image rows (69 bytes, written by the test itself, no fixture).
const PNG_1X1: [u8; 69] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 144,
    119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1, 1, 0, 201, 254, 146,
    239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// A running `oxa-fake` for one test: it answers `turns` in order and records every request
/// to `requests.jsonl` under the test root. It serves until its stdin closes (kept open here).
struct Rig {
    env: TestEnv,
    url: String,
    fake: Child,
    _keep_stdin: ChildStdin,
    _stdout: BufReader<ChildStdout>,
}

impl Rig {
    fn new(turns: &[Value]) -> Rig {
        Rig::with_fake(turns, &[])
    }

    fn with_fake(turns: &[Value], extra: &[&str]) -> Rig {
        let env = TestEnv::new();
        let script = turns.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        let script_path = env.write("script.jsonl", &script);
        let record = env.root().join("requests.jsonl");
        let mut args: Vec<String> = vec![
            "--script".into(),
            script_path.display().to_string(),
            "--record".into(),
            record.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut fake = Command::new(fake_bin())
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa-fake (build it first: cargo build -p oxa-cli -p oxa-fake)");
        let keep_stdin = fake.stdin.take().expect("fake stdin");
        let mut stdout = BufReader::new(fake.stdout.take().expect("fake stdout"));
        let mut line = String::new();
        stdout.read_line(&mut line).expect("the fake prints its URL");
        let url = line
            .trim()
            .strip_prefix("listening ")
            .expect("`listening <url>` line")
            .to_string();
        Rig {
            env,
            url,
            fake,
            _keep_stdin: keep_stdin,
            _stdout: stdout,
        }
    }

    /// The agent-directory `settings.json` (replaces the previous one).
    fn user_settings(&self, value: Value) {
        self.env
            .write("home/.oxa/agent/settings.json", &value.to_string());
    }

    /// The project `.oxa/settings.json` of the working directory.
    fn project_settings(&self, value: Value) {
        self.env.write("work/.oxa/settings.json", &value.to_string());
    }

    fn oxa(&self, args: &[&str]) -> Output {
        run_oxa(&self.env, &self.url, args, "")
    }

    /// `oxa <extra> -p hi`.
    fn print(&self, extra: &[&str]) -> Output {
        let mut args = extra.to_vec();
        args.extend(["-p", "hi"]);
        self.oxa(&args)
    }

    /// The request bodies the fake recorded, in order. The record file is written by a thread
    /// of the fake, so this waits a little first.
    fn bodies(&self) -> Vec<Value> {
        std::thread::sleep(Duration::from_millis(400));
        let text = std::fs::read_to_string(self.env.root().join("requests.jsonl")).unwrap_or_default();
        text.lines()
            .map(|line| {
                let record: Value = serde_json::from_str(line).expect("a record line");
                let body = record["body"].as_str().expect("a record body");
                serde_json::from_str(body).expect("a JSON request body")
            })
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.fake.kill();
        let _ = self.fake.wait();
    }
}

fn text(s: &str) -> Value {
    json!({ "text": s })
}

/// A reply the fake delays by `ms`, sending `: queued` comments every second meanwhile (the run
/// stays busy and the client is reading, so an abort is noticed).
fn held(s: &str, ms: u64) -> Value {
    json!({ "text": s, "fault": { "kind": "comments_only", "ms": ms } })
}

/// A reply of `n` numbered lines `<prefix>1 ..`, zero-padded to two digits when `n` > 9, as a
/// code block (one transcript row each).
fn numbered_reply(prefix: &str, n: usize) -> Value {
    let lines: Vec<String> = (1..=n)
        .map(|i| {
            if n > 9 {
                format!("{prefix}{i:02}")
            } else {
                format!("{prefix}{i}")
            }
        })
        .collect();
    text(&format!("```\n{}\n```", lines.join("\n")))
}

/// A turn that calls the built-in `bash` tool with `command`.
fn bash_call(command: &str) -> Value {
    json!({ "tool_calls": [ { "id": "c1", "name": "bash", "args": json!({ "command": command }).to_string() } ] })
}

/// A turn that fails with an HTTP `status` before any data (the engine's busy reply).
fn http_fault(status: u16) -> Value {
    json!({
        "text": "never sent",
        "fault": {
            "kind": "http",
            "status": status,
            "body": json!({ "error": { "message": "busy", "type": "unavailable_error", "code": status } }).to_string(),
        }
    })
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Assert the run succeeded; the message carries stderr.
fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "oxa failed: status {:?}\nstderr:\n{}",
        out.status.code(),
        stderr(out)
    );
}

/// A `Warning:` line of stderr that names `needle`.
fn warned(out: &Output, needle: &str) -> bool {
    stderr(out)
        .lines()
        .any(|l| l.starts_with("Warning:") && l.contains(needle))
}

/// The tool names of a request body, sorted.
fn tools(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["function"]["name"].as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn enable_thinking(body: &Value) -> Option<bool> {
    body["chat_template_kwargs"]["enable_thinking"].as_bool()
}

/// The content of the last `tool` message of a request body (what the tool result looks like
/// on the wire).
fn tool_result(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "tool"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

/// A request that carries the summary instruction of a compaction.
fn is_summary_request(body: &Value) -> bool {
    body["messages"].to_string().contains("context summary")
}

/// Write an executable stub script `rel` under the test root and return its path.
fn stub(env: &TestEnv, rel: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = env.write(rel, body);
    let mut perm = std::fs::metadata(&path).expect("stub metadata").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).expect("stub permissions");
    path
}

// ---------------------------------------------------------------------------------------------
// The TUI rig (wave H): the real `oxa` in a pseudo-terminal (`oxa_parity::pty`) against an
// in-process fake engine. Every test builds its own `TestEnv`, fake and pty; waits are bounded
// (`pty::DEFAULT_TIMEOUT`) and never sleep as an assertion.
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(dead_code)] // some helpers serve only part of the rows
mod tui {
    use super::*;
    pub use oxa_fake::{parse_script, FakeOpts, FakeServer};
    pub use oxa_parity::pty::{KeyEncoding, PtyOpts, PtySession, Screen, TermProfile, DEFAULT_TIMEOUT};
    use oxa_proto::Role;

    /// What a TUI test starts with.
    pub struct Setup {
        /// The agent-directory `settings.json`.
        pub settings: Value,
        /// The agent-directory `keybindings.json`.
        pub keybindings: Option<Value>,
        pub profile: TermProfile,
        pub size: (u16, u16),
        pub args: Vec<String>,
        /// The fake's script, one JSON turn per element.
        pub script: Vec<Value>,
        pub fake: FakeOpts,
        /// Extra environment of the `oxa` child (`{root}` in a value = the test root).
        pub env: Vec<(String, String)>,
        /// Files written under the test root before the start: `(relative path, content)`.
        pub files: Vec<(String, String)>,
        /// `oxa <args>` runs (print mode) against the fake before the TUI starts; each consumes
        /// script turns first.
        pub pre_runs: Vec<Vec<String>>,
        /// Executable files written under the test root: `(relative path, content)`.
        pub executables: Vec<(String, String)>,
        /// Wait until the editor is drawn before returning.
        pub wait_editor: bool,
    }

    impl Default for Setup {
        fn default() -> Self {
            Setup {
                settings: json!({}),
                keybindings: None,
                profile: TermProfile::xterm(),
                size: (24, 80),
                args: Vec::new(),
                script: Vec::new(),
                fake: FakeOpts::default(),
                env: Vec::new(),
                files: Vec::new(),
                executables: Vec::new(),
                pre_runs: Vec::new(),
                wait_editor: true,
            }
        }
    }

    /// One TUI session. Field order = drop order: the child first, then the engine, the dirs.
    pub struct Tui {
        pub pty: PtySession,
        pub server: FakeServer,
        pub env: TestEnv,
    }

    impl Setup {
        pub fn new() -> Setup {
            Setup::default()
        }
        pub fn settings(mut self, v: Value) -> Setup {
            self.settings = v;
            self
        }
        pub fn keys(mut self, v: Value) -> Setup {
            self.keybindings = Some(v);
            self
        }
        pub fn profile(mut self, p: TermProfile) -> Setup {
            self.profile = p;
            self
        }
        pub fn size(mut self, rows: u16, cols: u16) -> Setup {
            self.size = (rows, cols);
            self
        }
        pub fn args(mut self, a: &[&str]) -> Setup {
            self.args = a.iter().map(|s| s.to_string()).collect();
            self
        }
        pub fn script(mut self, turns: &[Value]) -> Setup {
            self.script = turns.to_vec();
            self
        }
        /// `n` replies `text`.
        pub fn replies(mut self, text: &str, n: usize) -> Setup {
            self.script = (0..n).map(|_| super::text(text)).collect();
            self
        }
        pub fn regular(self) -> Setup {
            let mut s = self;
            s.args.extend(["--tui-mode".to_string(), "regular".to_string()]);
            s
        }
        pub fn env(mut self, k: &str, v: &str) -> Setup {
            self.env.push((k.to_string(), v.to_string()));
            self
        }
        pub fn file(mut self, rel: &str, content: &str) -> Setup {
            self.files.push((rel.to_string(), content.to_string()));
            self
        }
        /// Run `oxa <args>` before the TUI starts (its turns are prepended to the script).
        pub fn pre_run(mut self, args: &[&str]) -> Setup {
            self.pre_runs.push(args.iter().map(|s| s.to_string()).collect());
            self
        }
        /// An executable stub (a script) at `rel` under the test root.
        pub fn exec(mut self, rel: &str, content: &str) -> Setup {
            self.executables.push((rel.to_string(), content.to_string()));
            self
        }
        pub fn fake(mut self, f: FakeOpts) -> Setup {
            self.fake = f;
            self
        }
        pub fn no_wait(mut self) -> Setup {
            self.wait_editor = false;
            self
        }
        pub fn start(self) -> Tui {
            let env = TestEnv::new();
            env.write("home/.oxa/agent/settings.json", &self.settings.to_string());
            if let Some(k) = &self.keybindings {
                env.write("home/.oxa/agent/keybindings.json", &k.to_string());
            }
            for (rel, content) in &self.files {
                env.write(rel, content);
            }
            for (rel, content) in &self.executables {
                super::stub(&env, rel, content);
            }
            let pre: Vec<Value> = (0..self.pre_runs.len())
                .map(|i| super::text(&format!("pre-run reply {i}")))
                .collect();
            let script = pre
                .iter()
                .chain(self.script.iter())
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let server = FakeServer::start(parse_script(&script).expect("script"), self.fake)
                .expect("start fake engine");
            for run in &self.pre_runs {
                let a: Vec<&str> = run.iter().map(String::as_str).collect();
                let out = run_oxa(&env, &server.url(), &a, "");
                assert!(
                    out.status.success(),
                    "pre-run {a:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
            // `{root}` in an environment value is the test root (known only now).
            let root = env.root().display().to_string();
            let child_env: Vec<(String, String)> = self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.replace("{root}", &root)))
                .collect();
            let pty = PtySession::spawn_with(
                &env,
                &server.url(),
                &args,
                PtyOpts {
                    size: self.size,
                    profile: self.profile,
                    env: child_env,
                    ..PtyOpts::default()
                },
            );
            pty.wait_ready().unwrap();
            let tui = Tui { pty, server, env };
            if self.wait_editor {
                tui.wait("the editor", |s| editor_rows(s).is_some());
            }
            tui
        }
    }

    /// A fake engine serving three models (`model-a` loaded, then `model-b`, `model-c`).
    pub fn three_models() -> FakeOpts {
        FakeOpts {
            model: "model-a".to_string(),
            models: ["model-a", "model-b", "model-c"]
                .iter()
                .map(|m| oxa_fake::FakeModel::moe(m, 1 << 20))
                .collect(),
            ..FakeOpts::default()
        }
    }

    /// The text of a transcript row: without the fullscreen scrollbar column and, for a closed
    /// code frame line, without its side borders and padding (`│  R10      │` -> `R10`).
    pub fn row_text(r: &str) -> &str {
        let end = r.trim_end();
        let closed = end.ends_with('│') || end.ends_with('█');
        let t = end.trim_end_matches(['│', '█']).trim();
        if closed {
            t.strip_prefix('│').map_or(t, |inner| inner.trim())
        } else {
            t
        }
    }

    /// The first row whose text (without the fullscreen scrollbar column and a code frame's
    /// side border) is exactly `needle`.
    ///
    /// The numbered reply is completely drawn at the bottom and the turn is over: the code
    /// frame's bottom border sits right after the `last` line and the "Working..." loader
    /// (blank line plus spinner above the editor) is gone. Both arrive/leave after the last
    /// line while the turn ends, and each shifts every transcript row above by one or two.
    pub fn reply_closed(s: &Screen, last: &str) -> bool {
        !s.contains("Working")
            && row_index(s, last).is_some_and(|r| {
                s.rows().get(r + 1).is_some_and(|f| {
                    let t = f.trim_end().trim_end_matches(['│', '█']).trim();
                    t.starts_with('└') && t.ends_with('┘') && t.chars().all(|c| matches!(c, '└' | '┘' | '─'))
                })
            })
    }

    pub fn row_index(s: &Screen, needle: &str) -> Option<usize> {
        s.rows().iter().position(|r| row_text(r) == needle)
    }

    /// Every session file (`*.jsonl`) below the agent directory.
    pub fn session_files(env: &TestEnv) -> Vec<PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "jsonl") {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        walk(&env.agent_dir().join("sessions"), &mut out);
        out
    }

    /// The user texts of a recorded request, in order.
    pub fn user_texts(r: &oxa_fake::Recorded) -> Vec<String> {
        r.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .filter_map(|m| m.content.clone())
            .collect()
    }

    /// The `/tree` navigator's selected row (pi's `› ` cursor), trimmed.
    pub fn tree_selected(s: &Screen) -> String {
        s.rows()
            .into_iter()
            .find(|r| r.starts_with("› "))
            .map(|r| r.trim().to_string())
            .unwrap_or_default()
    }

    /// The navigator's position line `(i/n) [filter]`, trimmed.
    pub fn tree_position(s: &Screen) -> String {
        s.rows()
            .into_iter()
            .rev()
            .map(|r| r.trim().to_string())
            .find(|t| t.starts_with('(') && t.contains('/') && t.contains(')'))
            .unwrap_or_default()
    }

    /// `/tree`, and wait for the navigator with a selected row (or an empty filtered list).
    pub fn open_tree(t: &Tui) {
        t.slash("/tree");
        t.wait("the /tree navigator", |s| {
            s.contains("Session Tree")
                && (!tree_selected(s).is_empty() || tree_position(s).starts_with("(0/0)"))
        });
    }

    /// Move the navigator's selection up until the selected row contains `needle`.
    pub fn tree_select(t: &Tui, needle: &str) {
        for _ in 0..40 {
            if tree_selected(&t.screen()).contains(needle) {
                return;
            }
            let before = tree_selected(&t.screen());
            t.key("up");
            t.wait("the selection moved", |s| tree_selected(s) != before);
        }
        panic!("no tree row with {needle:?}\n{:?}", t.screen());
    }

    /// Rough lightness 0..255 of a cell colour (`None` = the terminal's default).
    pub fn lightness(c: &oxa_parity::pty::Color) -> Option<u32> {
        use oxa_parity::pty::Color;
        match *c {
            Color::Default => None,
            Color::Rgb(r, g, b) => Some((r as u32 + g as u32 + b as u32) / 3),
            Color::Idx(n) => Some(match n {
                0 => 0,
                8 => 128,
                7 => 192,
                15 => 255,
                1..=6 => 100,
                9..=14 => 200,
                16..=231 => {
                    let lv = [0u32, 95, 135, 175, 215, 255];
                    let i = (n - 16) as usize;
                    (lv[i / 36] + lv[(i / 6) % 6] + lv[i % 6]) / 3
                }
                _ => 8 + 10 * (n as u32 - 232),
            }),
        }
    }

    /// Standard base64 decoding (OSC 52 payloads); invalid characters are skipped.
    pub fn b64_decode(s: &str) -> Vec<u8> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let (mut buf, mut bits) = (0u32, 0u32);
        for c in s.bytes() {
            let Some(v) = A.iter().position(|&a| a == c) else {
                continue;
            };
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        out
    }

    /// The rows between the last two full-width horizontal rules (the editor box, whose borders
    /// may carry a `─── ↑ n more ───` scroll hint), if drawn.
    pub fn editor_rows(s: &Screen) -> Option<Vec<String>> {
        let rows = s.rows();
        let rules: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.chars().count() >= 10 && r.starts_with("───") && r.ends_with('─'))
            .map(|(i, _)| i)
            .collect();
        if rules.len() < 2 {
            return None;
        }
        let (a, b) = (rules[rules.len() - 2], rules[rules.len() - 1]);
        Some(rows[a + 1..b].to_vec())
    }

    /// #1512 S3: the pi footer's last row (model and thinking), above the extension status
    /// row (`no sandbox · …`, `sandbox`, `YOLO full access`) that is drawn below it.
    pub fn footer_row(s: &Screen) -> String {
        let mut row = s.size().0 - 1;
        while row > 1 && {
            let r = s.row(row);
            let t = r.trim();
            t == "sandbox" || t.starts_with("no sandbox") || t.starts_with("YOLO full access")
        } {
            row -= 1;
        }
        s.row(row)
    }

    /// The editor's text: its rows, right-trimmed, joined with `\n`.
    pub fn editor_text(s: &Screen) -> String {
        editor_rows(s)
            .unwrap_or_default()
            .iter()
            .map(|r| r.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    impl Tui {
        pub fn screen(&self) -> Screen {
            self.pty.screen()
        }
        /// Wait (bounded) until `pred` holds; panics with the screen dump otherwise.
        pub fn wait(&self, what: &str, pred: impl Fn(&Screen) -> bool) -> Screen {
            self.pty.wait_for_named(what, pred, DEFAULT_TIMEOUT).unwrap()
        }
        /// Content-based: wait until `pred` holds on a complete frame (input read, bounded by
        /// `DEFAULT_TIMEOUT`), then until it still holds after 250 ms of quiet. Use it for a
        /// baseline measured before the next input: a frame that merely looks right while the
        /// transcript is still being drawn must not end the wait.
        pub fn wait_steady(&self, what: &str, pred: impl Fn(&Screen) -> bool) -> Screen {
            self.pty.wait_steady(what, pred, 250, DEFAULT_TIMEOUT).unwrap()
        }
        pub fn wait_text(&self, needle: &str) -> Screen {
            self.pty.wait_for_text(needle).unwrap()
        }
        pub fn wait_gone(&self, needle: &str) -> Screen {
            self.pty.wait_for_text_gone(needle).unwrap()
        }
        /// Wait until the app has read the input sent so far and the screen has been quiet for a
        /// moment (`wait_stable`). Assertions that something did NOT happen also get a
        /// positive witness afterwards (a later key whose effect is checked).
        pub fn settle(&self) -> Screen {
            self.pty.wait_stable(250, DEFAULT_TIMEOUT).unwrap()
        }
        pub fn editor(&self) -> String {
            editor_text(&self.screen())
        }
        /// Wait until the editor text is exactly `want`.
        pub fn wait_editor(&self, want: &str) -> Screen {
            self.wait(&format!("editor == {want:?}"), |s| editor_text(s) == want)
        }
        pub fn key(&self, id: &str) {
            self.pty.send_key(id);
        }
        pub fn key_as(&self, id: &str, enc: KeyEncoding) {
            self.pty.send_key_as(id, enc);
        }
        pub fn keys(&self, ids: &[&str]) {
            for id in ids {
                self.pty.send_key(id);
            }
        }
        /// Type `text` and wait until the editor shows it.
        pub fn type_text(&self, text: &str) {
            self.pty.send_text(text);
            let last = text.lines().last().unwrap_or(text).to_string();
            self.wait(&format!("{text:?} typed"), move |s| {
                editor_text(s).contains(&last)
            });
        }
        /// Type a slash command line, let the command list settle, press Enter.
        pub fn slash(&self, line: &str) {
            self.type_text(line);
            self.settle();
            self.key("enter");
        }
        /// Type `text`, press enter, and wait for the reply `reply` on screen.
        pub fn prompt(&self, text: &str, reply: &str) {
            self.type_text(text);
            self.key("enter");
            self.wait_text(reply);
            // The reply text is on screen before the turn ends; the next input must not be
            // sent into the still-working turn (it would be queued or swallowed under load).
            self.wait("the turn finished", |s| !s.contains("Working"));
        }
        /// The read-only export is executed by the worker after the current run settles.
        /// Reply text and a quiet frame alone do not acknowledge that boundary.
        pub fn wait_worker_reply_end(&self, marker: &str) {
            let chats = self.chats().len();
            self.slash(&format!("/export {marker}"));
            self.wait("worker completed the read-only export", |s| {
                s.contains("Exported the session to") && s.contains(marker)
            });
            assert_eq!(
                self.chats().len(),
                chats,
                "the witness must not make a model request"
            );
        }
        /// The user prompts the engine got (chat requests only), in order.
        pub fn prompts(&self) -> Vec<String> {
            self.server
                .requests()
                .iter()
                .filter(|r| r.max_tokens > 0)
                .filter_map(|r| r.messages.iter().rev().find(|m| m.role == Role::User))
                .filter_map(|m| m.content.clone())
                .collect()
        }
        /// The chat requests (`max_tokens > 0`).
        pub fn chats(&self) -> Vec<oxa_fake::Recorded> {
            self.server
                .requests()
                .into_iter()
                .filter(|r| r.max_tokens > 0)
                .collect()
        }
        /// Wait until `n` chat requests were made.
        pub fn wait_chats(&self, n: usize) -> Vec<oxa_fake::Recorded> {
            let t = std::time::Instant::now();
            loop {
                let c = self.chats();
                if c.len() >= n {
                    return c;
                }
                assert!(
                    t.elapsed() < DEFAULT_TIMEOUT,
                    "{n} chat requests expected, got {}\n{:?}",
                    c.len(),
                    self.pty.wait_for(|_| true, Duration::from_millis(1)).unwrap()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        /// Ctrl+D on an empty editor; the process exits 0.
        pub fn exit(&mut self) {
            self.pty.send_key("ctrl+d");
            let st = self.pty.wait_exit(DEFAULT_TIMEOUT).unwrap();
            assert!(st.success(), "exit status {st}");
        }
        /// The process exited with status 0 (bounded wait).
        pub fn exited_ok(&mut self) -> bool {
            self.pty
                .wait_exit(DEFAULT_TIMEOUT)
                .map(|s| s.success())
                .unwrap_or(false)
        }
    }
}
#[cfg(target_os = "linux")]
use tui::*;

// ---------------------------------------------------------------------------------------------
// Meta-checks of the task (the acceptance tests)
// ---------------------------------------------------------------------------------------------

/// The rows of this area in PARITY.md: (id, verdict).
fn area_rows() -> Vec<(String, String)> {
    let md = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../PARITY.md"))
        .expect("PARITY.md at the workspace root");
    let mut rows = Vec::new();
    let mut in_area = false;
    for line in md.lines() {
        if line.starts_with("## ") {
            in_area = line.starts_with("## SET:") || line.starts_with("## KEY:");
            continue;
        }
        if !in_area {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() > 3 && (cells[1].starts_with("SET-") || cells[1].starts_with("KEY-")) {
            let verdict = cells[2..]
                .iter()
                .find(|c| matches!(**c, "P" | "A" | "O"))
                .copied()
                .unwrap_or("?");
            rows.push((cells[1].to_string(), verdict.to_string()));
        }
    }
    rows
}

/// `SET-001` -> `set_001`.
fn fn_name(id: &str) -> String {
    id.to_lowercase().replace('-', "_")
}

/// The source of test fn `name` (from its `fn` line to the next item's doc comment or
/// attribute).
fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src
        .find(&format!("fn {name}()"))
        .unwrap_or_else(|| panic!("no test fn `{name}` in keys_settings.rs"));
    let rest = &src[start..];
    let next = [rest[1..].find("\n#["), rest[1..].find("\n///")]
        .into_iter()
        .flatten()
        .min()
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    &rest[..next]
}

#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = area_rows();
    assert_eq!(
        rows.len(),
        69,
        "the SET and KEY areas of PARITY.md have 49 + 20 rows"
    );
    let src = include_str!("keys_settings.rs");
    for (id, _) in &rows {
        let name = fn_name(id);
        assert!(
            src.contains(&format!("fn {name}()")),
            "{id} has no test fn `{name}` (a row needs a test or a recorded omission)"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    let src = include_str!("keys_settings.rs");
    for (id, verdict) in area_rows() {
        if verdict != "P" {
            continue;
        }
        let body = body_of(src, &fn_name(&id));
        assert!(
            body.contains("// pi docs:"),
            "{id} is P but its test names no pi documentation (`// pi docs:`)"
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let src = include_str!("keys_settings.rs");
    for (id, verdict) in area_rows() {
        if verdict != "A" {
            continue;
        }
        let body = body_of(src, &fn_name(&id));
        assert!(
            body.contains("// invariant:"),
            "{id} is A but its test names no invariant (`// invariant:`)"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// SET: Settings (settings.md)
// ---------------------------------------------------------------------------------------------

/// SET-001: user + project `settings.json` merge; project overrides user; nested objects merge.
#[test]
fn set_001() {
    // pi docs: settings.md "Project settings override agent-directory settings. Resource lists
    // are combined." The project's `+bash` patches the user's list; its `oxa.subagent` object
    // merges with the user's `oxa.toolOutputCap` instead of replacing the object.
    let rig = Rig::new(&[bash_call("yes a | head -c 5000"), text("done")]);
    rig.user_settings(json!({
        "defaultProjectTrust": "always",
        "defaultTools": ["ls", "read"],
        "oxa": { "toolOutputCap": 1024 }
    }));
    rig.project_settings(json!({
        "defaultTools": ["+bash"],
        "oxa": { "subagent": { "enabled": false } }
    }));
    let out = rig.print(&[]);
    ok(&out);
    let bodies = rig.bodies();
    assert_eq!(
        tools(&bodies[0]),
        ["bash", "ls", "read"],
        "project +bash patches the user list"
    );
    let result = tool_result(&bodies[1]);
    assert!(
        result.len() < 2048,
        "the user's oxa.toolOutputCap survives the project's oxa object (tool result has {} bytes)",
        result.len()
    );
}

/// SET-002: unknown or invalid keys produce a diagnostic, never a failure.
#[test]
fn set_002() {
    // pi docs: settings.md: unknown or invalid settings are reported; the run continues.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "notARealKey": 1, "compaction": { "reserveTokens": "lots" } }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(
        warned(&out, "notARealKey"),
        "unknown key is reported: {}",
        stderr(&out)
    );
    assert!(
        warned(&out, "compaction.reserveTokens"),
        "wrong type is reported: {}",
        stderr(&out)
    );
    assert_eq!(rig.bodies().len(), 1, "the run went on after the diagnostics");
}

/// SET-004: `defaultModel` is an engine model id.
#[test]
fn set_004() {
    // invariant: the model id is the engine's (W3: the model is fixed for the prefix epoch).
    // A two-model fake (`--model-row`): without the setting the engine's loaded model is used;
    // with `defaultModel` naming the OTHER engine model, that one is.
    let rows = [
        "--model",
        "big",
        "--model-row",
        "big:131072",
        "--model-row",
        "other:65536",
    ];
    let control = Rig::with_fake(&[text("ok")], &rows);
    let out = control.print(&[]);
    ok(&out);
    assert_eq!(control.bodies()[0]["model"], "big", "control: the loaded model");

    let rig = Rig::with_fake(&[text("ok")], &rows);
    rig.user_settings(json!({ "defaultModel": "other" }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(!warned(&out, "defaultModel"), "{}", stderr(&out));
    assert_eq!(
        rig.bodies()[0]["model"],
        "other",
        "defaultModel picks the named engine model"
    );
}

/// SET-005: `defaultThinkingLevel` (default `medium`).
#[test]
fn set_005() {
    // invariant: W6: the thinking level is a boolean on the wire (`enable_thinking`). `medium`
    // is enabled, `off` is not.
    let rig = Rig::new(&[text("ok"), text("ok")]);
    let out = rig.print(&[]);
    ok(&out);
    rig.user_settings(json!({ "defaultThinkingLevel": "off" }));
    ok(&rig.print(&[]));
    let bodies = rig.bodies();
    assert_eq!(
        enable_thinking(&bodies[0]),
        Some(true),
        "defaultThinkingLevel defaults to medium (enable_thinking true)"
    );
    assert_eq!(
        enable_thinking(&bodies[1]),
        Some(false),
        "defaultThinkingLevel off"
    );
}

/// SET-006: `modelThinkingLevels` keyed by engine model id.
#[test]
fn set_006() {
    // invariant: W6 boolean; the per-model level is keyed by the engine model id and wins
    // over `defaultThinkingLevel` (pi sdk.ts).
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({
        "defaultThinkingLevel": "off",
        "modelThinkingLevels": { "fake-model": "high" }
    }));
    let out = rig.print(&[]);
    ok(&out);
    assert_eq!(
        enable_thinking(&rig.bodies()[0]),
        Some(true),
        "modelThinkingLevels names fake-model high over the default off"
    );
}

/// SET-008: `enabledModels` is a list of patterns over engine models.
#[test]
fn set_008() {
    // invariant: the patterns are matched against the engine's model ids (W3: a switch is a
    // new prefix epoch, never inside one). pi settings.md: "Model patterns used for startup
    // selection and model cycling. Uses the same format as `--models`": a new session starts on
    // the first match (pi main.ts `scopedModels[0]`), ctrl+p cycles only through the matches.
    let footer = |s: &Screen| footer_row(s);
    let t = Setup::new()
        .settings(json!({ "enabledModels": ["model-c", "model-b*"] }))
        .fake(three_models())
        .replies("scoped answer", 1)
        .start();
    t.wait("starts on the first match", |s| footer(s).contains("model-c"));
    t.key("ctrl+p");
    t.wait("model-b", |s| footer(s).contains("model-b"));
    t.key("ctrl+p");
    t.wait("back to model-c (model-a is out of scope)", |s| {
        footer(s).contains("model-c")
    });
    t.prompt("hi", "scoped answer");
    assert_eq!(t.wait_chats(1)[0].model, "model-c");
    drop(t);
    // `--models` replaces the setting for the run; `--model` wins over both.
    let t = Setup::new()
        .settings(json!({ "enabledModels": ["model-c"] }))
        .args(&["--models", "model-a,model-b"])
        .fake(three_models())
        .start();
    t.wait("starts on model-a", |s| footer(s).contains("model-a"));
    t.key("ctrl+p");
    t.wait("model-b", |s| footer(s).contains("model-b"));
    drop(t);
    let t = Setup::new()
        .settings(json!({ "enabledModels": ["model-c"] }))
        .args(&["--model", "model-b"])
        .fake(three_models())
        .start();
    t.wait("--model wins", |s| footer(s).contains("model-b"));
}

/// SET-009: `hideThinkingBlock` hides the thinking blocks in the transcript.
#[test]
fn set_009() {
    // pi docs: settings.md `hideThinkingBlock` "Hide thinking blocks in the transcript."
    // app.thinking.toggle (ctrl+t) still shows them on demand.
    let reply = json!({ "reasoning": "private musing", "text": "public answer" });
    let t = Setup::new()
        .settings(json!({ "hideThinkingBlock": true }))
        .script(std::slice::from_ref(&reply))
        .start();
    t.prompt("q", "public answer");
    let s = t.settle();
    assert!(!s.contains("private musing"), "hidden by the setting\n{s:?}");
    t.key("ctrl+t");
    t.wait_text("private musing");
    drop(t);
    let t = Setup::new().script(&[reply]).start();
    t.prompt("q", "public answer");
    t.wait_text("private musing");
}

/// SET-010: `showCacheMissNotices` uses the exact `cached_tokens`; the cause from `miss_cause`.
#[test]
fn set_010() {
    // invariant: W1/W2: the notice uses the engine's exact cached_tokens and miss_cause only.
    // pi settings.md `showCacheMissNotices` (default false): notices for cache misses only when
    // on. The fake's restart drops its cache, so the second reply misses.
    let run = |show: bool| {
        let t = Setup::new()
            .settings(if show {
                json!({ "showCacheMissNotices": true })
            } else {
                json!({})
            })
            .replies("cached answer", 2)
            .start();
        t.prompt("first", "cached answer");
        t.server.restart();
        t.type_text("second");
        t.key("enter");
        let chats = t.wait_chats(2);
        (t, chats)
    };
    let (t, chats) = run(true);
    let s = t.wait("the cache-miss notice", |s| s.contains("Cache miss ("));
    let row = s.rows().into_iter().find(|r| r.contains("Cache miss (")).unwrap();
    // The cause is never `unknown`: a miss without a rebase after the first request is the
    // engine's loss (`engine_evicted`; here the restart).
    assert!(
        row.contains("Cache miss (engine_evicted): 0 of "),
        "exact cached_tokens and the cause: {row}"
    );
    let usage = chats[1].usage.expect("the fake's usage");
    assert_eq!(usage.cached, 0);
    drop(t);
    let (t, _) = run(false);
    t.wait("the second reply", |s| {
        s.rows().iter().filter(|r| r.contains("cached answer")).count() == 2
    });
    let s = t.settle();
    assert!(!s.contains("Cache miss"), "off by default\n{s:?}");
}

/// SET-011: `cacheWarming` `off|streaming|idle`; maps to idle prefill.
#[test]
fn set_011() {
    // invariant: `streaming` = `idle` (H6 max_tokens:0 prefill + H3 background + H4 keep):
    // the boot preload at start and the park prefill after a /tree navigation; `off` sends
    // neither. Commands run in order on the worker, so a prefill caused by the navigation
    // comes before the next chat request.
    let prefills = |reqs: &[oxa_fake::Recorded]| reqs.iter().filter(|r| r.max_tokens == 0).count();
    for value in ["idle", "streaming", "off"] {
        let t = Setup::new()
            .settings(json!({ "cacheWarming": value, "branchSummary": { "skipPrompt": true } }))
            .script(&[text("first answer"), text("second answer"), text("third answer")])
            .start();
        t.prompt("first question", "first answer");
        let at_start = prefills(&t.server.requests());
        t.prompt("second question", "second answer");
        open_tree(&t);
        tree_select(&t, "first answer");
        t.key("enter");
        t.wait_gone("Session Tree");
        if value != "off" {
            // User input cancels a background prefill (ENG-002): wait for it before typing.
            let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT;
            while prefills(&t.server.requests()) <= at_start {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{value}: no park prefill after the navigation"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        t.prompt("third question", "third answer");
        let reqs = t.server.requests();
        let third = reqs
            .iter()
            .position(|r| r.max_tokens > 0 && String::from_utf8_lossy(&r.body).contains("third question"))
            .unwrap();
        let second = reqs
            .iter()
            .position(|r| r.max_tokens > 0 && String::from_utf8_lossy(&r.body).contains("second question"))
            .unwrap();
        let after_nav = prefills(&reqs[second..third]);
        if value == "off" {
            assert_eq!((at_start, after_nav), (0, 0), "off: no prefill at all");
        } else {
            assert!(at_start >= 1, "{value}: the boot preload ran");
            assert!(
                after_nav >= 1,
                "{value}: the park prefill after the navigation ran"
            );
            let p = reqs[second..third].iter().find(|r| r.max_tokens == 0).unwrap();
            assert!(
                p.background && p.snapshot_keep,
                "{value}: H3 background + H4 keep"
            );
        }
    }
}

/// SET-012: `steeringMode` `all|one-at-a-time`.
#[test]
fn set_012() {
    // pi docs: settings.md `steeringMode` "How queued steering messages are delivered":
    // `one-at-a-time` (default) one per turn, `all` together. Two messages are steered (Enter
    // while the run is busy) during a delayed tool call; they are delivered after the tool.
    for (mode, expect) in [
        (None, vec![vec!["go", "s1"], vec!["go", "s1", "s2"]]),
        (
            Some("one-at-a-time"),
            vec![vec!["go", "s1"], vec!["go", "s1", "s2"]],
        ),
        (Some("all"), vec![vec!["go", "s1", "s2"]]),
    ] {
        let mut settings = json!({ "defaultProjectTrust": "always" });
        if let Some(m) = mode {
            settings["steeringMode"] = json!(m);
        }
        let mut call = bash_call("echo tool-ran");
        call["fault"] = json!({ "kind": "comments_only", "ms": 2500 });
        let t = Setup::new()
            .settings(settings)
            .script(&[call, text("r2"), text("r3")])
            .start();
        t.type_text("go");
        t.key("enter");
        t.wait_chats(1);
        for s in ["s1", "s2"] {
            t.type_text(s);
            t.key("enter");
            t.wait_editor("");
        }
        let n = expect.len() + 1;
        let chats = t.wait_chats(n);
        let got: Vec<Vec<String>> = chats[1..n].iter().map(user_texts).collect();
        assert_eq!(
            got, expect,
            "{mode:?}: the user messages of the requests after the tool"
        );
        t.wait_text(if n == 2 { "r2" } else { "r3" });
        let s = t.settle();
        assert_eq!(t.chats().len(), n, "{mode:?}: no further request\n{s:?}");
    }
}

/// SET-013: `followUpMode` `all|one-at-a-time`.
#[test]
fn set_013() {
    // pi docs: settings.md `followUpMode` "How queued follow-up messages are delivered":
    // `one-at-a-time` (default) one per finished run, `all` together. Two follow-ups are queued
    // (alt+enter) while a delayed reply runs.
    for (mode, expect) in [
        (None, vec![vec!["go", "f1"], vec!["go", "f1", "f2"]]),
        (
            Some("one-at-a-time"),
            vec![vec!["go", "f1"], vec!["go", "f1", "f2"]],
        ),
        (Some("all"), vec![vec!["go", "f1", "f2"]]),
    ] {
        let settings = match mode {
            Some(m) => json!({ "followUpMode": m }),
            None => json!({}),
        };
        let t = Setup::new()
            .settings(settings)
            .script(&[held("r1", 2500), text("r2"), text("r3")])
            .start();
        t.type_text("go");
        t.key("enter");
        t.wait_chats(1);
        for f in ["f1", "f2"] {
            t.type_text(f);
            t.key("alt+enter");
            t.wait_editor("");
        }
        let n = expect.len() + 1;
        let chats = t.wait_chats(n);
        let got: Vec<Vec<String>> = chats[1..n].iter().map(user_texts).collect();
        assert_eq!(
            got, expect,
            "{mode:?}: the user messages of the follow-up requests"
        );
        t.wait_text(if n == 2 { "r2" } else { "r3" });
        let s = t.settle();
        assert_eq!(t.chats().len(), n, "{mode:?}: no further request\n{s:?}");
    }
}

/// SET-014: `externalEditor` (then `$VISUAL`, `$EDITOR`).
#[test]
fn set_014() {
    // pi docs: settings.md `externalEditor` "Command opened by the external-editor keybinding."
    // The setting wins over $VISUAL, which wins over $EDITOR.
    let script = |word: &str| format!("#!/bin/sh\nprintf '{word}' > \"$1\"\n");
    let setup = |settings: Value| {
        Setup::new()
            .settings(settings)
            .exec("bin/setting.sh", &script("by-setting"))
            .exec("bin/visual.sh", &script("by-visual"))
            .exec("bin/editor.sh", &script("by-editor"))
    };
    // The paths are only known after the TestEnv exists, so the commands are relative to the
    // working directory (`work/`).
    let t = setup(json!({ "externalEditor": "../bin/setting.sh" }))
        .env("VISUAL", "../bin/visual.sh")
        .env("EDITOR", "../bin/editor.sh")
        .start();
    t.type_text("x");
    t.key("ctrl+g");
    t.wait_editor("by-setting");
    drop(t);
    let t = setup(json!({}))
        .env("VISUAL", "../bin/visual.sh")
        .env("EDITOR", "../bin/editor.sh")
        .start();
    t.type_text("x");
    t.key("ctrl+g");
    t.wait_editor("by-visual");
    drop(t);
    let t = setup(json!({})).env("EDITOR", "../bin/editor.sh").start();
    t.type_text("x");
    t.key("ctrl+g");
    t.wait_editor("by-editor");
    drop(t);
    let t = setup(json!({})).start();
    t.type_text("x");
    t.key("ctrl+g");
    t.wait_text("No editor");
    assert_eq!(t.editor(), "x");
}

/// SET-015: `doubleEscapeAction` `tree|fork|none`.
#[test]
fn set_015() {
    // pi docs: settings.md `doubleEscapeAction` "Action for double Escape with an empty
    // editor": `tree` (default) opens /tree, `fork` the fork selector, `none` nothing. One
    // Escape alone keeps its meaning (interrupt, nothing when idle). The kitty profile sends
    // each Escape as its own CSI-u key: two legacy ESC bytes read together are one
    // `ctrl+alt+[` chord (pi keys.ts parses "\x1b\x1b" the same way).
    let any_open = |s: &Screen| s.contains("Session Tree") || s.contains("Fork from Message");
    for (value, opens) in [
        (None, Some("Session Tree")),
        (Some("tree"), Some("Session Tree")),
        (Some("fork"), Some("Fork from Message")),
        (Some("none"), None),
    ] {
        let settings = match value {
            Some(v) => json!({ "doubleEscapeAction": v }),
            None => json!({}),
        };
        let t = Setup::new()
            .profile(TermProfile::kitty())
            .settings(settings)
            .replies("an answer", 1)
            .start();
        t.prompt("a question", "an answer");
        t.keys(&["escape", "escape"]);
        match opens {
            Some(title) => {
                t.wait_text(title);
                t.key("escape");
                t.wait("the selector closed", |s| !any_open(s));
            }
            None => {
                let s = t.settle();
                assert!(!any_open(&s), "none: nothing opens\n{s:?}");
            }
        }
        // Input pacing, not an assertion: past pi's 500 ms window, so the next Escape is a
        // single one.
        std::thread::sleep(Duration::from_millis(600));
        t.key("escape");
        let s = t.settle();
        assert!(!any_open(&s), "{value:?}: one Escape opens nothing\n{s:?}");
        // Not with text in the editor.
        std::thread::sleep(Duration::from_millis(600));
        t.type_text("draft");
        t.keys(&["escape", "escape"]);
        let s = t.settle();
        assert!(
            !any_open(&s),
            "{value:?}: a non-empty editor keeps double Escape inert\n{s:?}"
        );
    }
}

/// SET-016: `treeFilterMode` (five values).
#[test]
fn set_016() {
    // pi docs: settings.md `treeFilterMode` "Initial filter used by /tree": default, no-tools,
    // user-only, labeled-only, all. The navigator's position line names the active filter.
    for (value, tag) in [
        (None, ""),
        (Some("default"), ""),
        (Some("no-tools"), " [no-tools]"),
        (Some("user-only"), " [user]"),
        (Some("labeled-only"), " [labeled]"),
        (Some("all"), " [all]"),
    ] {
        let settings = match value {
            Some(v) => json!({ "treeFilterMode": v }),
            None => json!({}),
        };
        let t = Setup::new().settings(settings).replies("an answer", 1).start();
        t.prompt("a question", "an answer");
        open_tree(&t);
        let s = t.wait(&format!("{value:?}: the position line"), |s| {
            tree_position(s).starts_with('(')
        });
        let pos = tree_position(&s);
        let filter = pos.find(')').map(|i| &pos[i + 1..]).unwrap_or("");
        assert_eq!(filter, tag, "{value:?}: {pos}");
    }
}

/// SET-017: `defaultProjectTrust` `ask|always|never`; agent-directory settings only.
#[test]
fn set_017() {
    // invariant: `defaultProjectTrust` is user-only; the shipped default `always` trusts a project with no setting and saves no decision, and `ask`/`never` are owner opt-ins (a project value is dropped with a warning, never honoured)
    // pi docs: settings.md `defaultProjectTrust` "Fallback project-trust behavior. Can only be
    // set in agent-directory settings." In print mode `ask` is untrusted (no dialog).
    // (a) `always` in the agent directory trusts the project: its defaultTools applies.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "defaultProjectTrust": "always" }));
    rig.project_settings(json!({ "defaultTools": ["ls"] }));
    ok(&rig.print(&[]));
    assert_eq!(tools(&rig.bodies()[0]), ["ls"], "always trusts the project");

    // (b) the shipped default trusts the project with no setting at all: its defaultTools applies.
    let rig = Rig::new(&[text("ok")]);
    rig.project_settings(json!({ "defaultTools": ["ls"] }));
    ok(&rig.print(&[]));
    assert_eq!(
        tools(&rig.bodies()[0]),
        ["ls"],
        "a fresh install trusts the project"
    );

    // (c) a project-level value is dropped as a user-only setting, so the project's own
    // `defaultTools` is still the trusted one; the warning names the key.
    let rig = Rig::new(&[text("ok")]);
    rig.project_settings(json!({ "defaultProjectTrust": "never", "defaultTools": ["ls"] }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(
        warned(&out, "is user-only and is ignored in a project file"),
        "a project-level defaultProjectTrust is ignored: {}",
        stderr(&out)
    );
    assert_eq!(
        tools(&rig.bodies()[0]),
        ["ls"],
        "the user layer decides, not the project"
    );
}

/// SET-018: `defaultTools`: plain names replace the defaults, `+name` adds, `-name` removes.
#[test]
fn set_018() {
    fn with_agent(args: &[&str]) -> Rig {
        let rig = Rig::with_fake(&[text("ok")], args);
        rig.env.write(
            "home/.oxa/agent/agents/worker.md",
            "---\nname: worker\ndescription: Test worker\n---\nDo the assigned task.\n",
        );
        rig
    }
    // invariant: with an available agent, the lead defaults include ask plus subagent
    // where O-146 offers it (not on the Rust Flash engine, not below a 32K window); the selection
    // is resolved once per prefix epoch (W3), so it is visible on the first request.
    let rig = with_agent(&[]);
    ok(&rig.print(&[]));
    assert_eq!(
        tools(&rig.bodies()[0]),
        [
            "ask",
            "bash",
            "deliverable",
            "edit",
            "find",
            "grep",
            "ls",
            "read",
            "subagent",
            "wait",
            "worker",
            "write"
        ],
        "the default set on a large non-Flash model: the lead built-ins, worker and subagent"
    );
    let flash = ["--model", "flash", "--model-row", "flash:65536:2048:flash"];
    let small = ["--n-ctx", "16384"];
    for (what, fake_args) in [("Flash", &flash[..]), ("below 32K", &small[..])] {
        let rig = with_agent(fake_args);
        ok(&rig.print(&[]));
        assert_eq!(
            tools(&rig.bodies()[0]),
            [
                "ask",
                "bash",
                "deliverable",
                "edit",
                "find",
                "grep",
                "ls",
                "read",
                "wait",
                "worker",
                "write"
            ],
            "{what}: the default set is the lead built-ins and worker"
        );
    }

    // `-name` edits the default selection (which includes `subagent`).
    let rig = with_agent(&[]);
    rig.user_settings(json!({ "defaultTools": ["-bash"] }));
    ok(&rig.print(&[]));
    assert_eq!(
        tools(&rig.bodies()[0]),
        [
            "ask",
            "deliverable",
            "edit",
            "find",
            "grep",
            "ls",
            "read",
            "subagent",
            "wait",
            "worker",
            "write"
        ],
        "-name removes"
    );
    let rig = with_agent(&small);
    rig.user_settings(json!({ "defaultTools": ["-bash"] }));
    ok(&rig.print(&[]));
    assert_eq!(
        tools(&rig.bodies()[0]),
        [
            "ask",
            "deliverable",
            "edit",
            "find",
            "grep",
            "ls",
            "read",
            "wait",
            "worker",
            "write"
        ],
        "-name removes (below 32K)"
    );

    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "defaultTools": ["read", "+grep"] }));
    ok(&rig.print(&[]));
    assert_eq!(
        tools(&rig.bodies()[0]),
        ["grep", "read"],
        "plain names replace, +name adds"
    );
}

/// SET-019: `codemode.mode` `on|only`.
#[test]
fn set_019() {
    // pi docs: settings.md `codemode.mode`: `only`: `codemode` lists every tool scripts can
    // call, and active built-in and extension tools are hidden from the model.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "codemode": { "enabled": true, "mode": "only" } }));
    ok(&rig.print(&[]));
    let names = tools(&rig.bodies()[0]);
    assert!(
        names.contains(&"codemode".to_string()),
        "codemode is offered: {names:?}"
    );
    assert_eq!(
        names,
        ["codemode"],
        "mode `only` hides the active built-in tools from the model"
    );
}

/// SET-021: `sessionDir`.
#[test]
fn set_021() {
    // invariant: the session store keeps one sub-directory per working directory below every sessions directory (pi stores flat).
    // pi docs: settings.md `sessionDir` "Relative paths resolve from the working directory.
    // PI_CODING_AGENT_SESSION_DIR and --session-dir override this setting." oxa's session
    // store keeps one sub-directory per working directory below every sessions directory (the
    // flag, the variable and the setting alike), so the file is looked for one level down.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "sessionDir": "custom-sessions" }));
    ok(&rig.print(&[]));
    let custom = rig.env.work().join("custom-sessions");
    let jsonl = |dir: &std::path::Path| {
        std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            })
            .unwrap_or(false)
    };
    let below = |dir: &std::path::Path| {
        std::fs::read_dir(dir)
            .map(|d| d.flatten().any(|e| e.path().is_dir() && jsonl(&e.path())))
            .unwrap_or(false)
    };
    assert!(
        below(&custom),
        "no session file under {} (the setting is not read)",
        custom.display()
    );
    assert!(
        !below(&rig.env.agent_dir().join("sessions")),
        "the default sessions directory is not used"
    );
}

/// SET-023: `compaction.enabled`.
#[test]
fn set_023() {
    // pi docs: settings.md `compaction.enabled`. A session over the soft limit is summarised
    // after the reply (the summary request carries the summary instruction); disabled, it is not.
    let base =
        json!({ "compaction": { "keepRecentTokens": 1 }, "oxa": { "compaction": { "softTokens": 5 } } });

    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(base.clone());
    ok(&rig.print(&[]));
    assert!(
        rig.bodies().iter().any(is_summary_request),
        "enabled by default: a summary request is made"
    );

    let mut disabled = base;
    disabled["compaction"]["enabled"] = json!(false);
    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(disabled);
    ok(&rig.print(&[]));
    assert!(
        !rig.bodies().iter().any(is_summary_request),
        "compaction.enabled false: no summary request"
    );
}

/// SET-024: `compaction.reserveTokens` (16384).
#[test]
fn set_024() {
    // invariant: W5: hard limit = n_ctx - max_tokens - reserveTokens. Over it the session is
    // compacted by pure appends (the current prompt kept, the earlier turns summarised). The
    // soft limit is set out of the way (it is clamped just below hard), so only the hard limit
    // decides. A session turn is ~9k tokens (system prompt + tools) on a 24k context whose
    // max_tokens is 6000: reserve 1000 leaves it under the limit, 12000 and the default 16384
    // put it over, so the second run (`-c`) summarises the first.
    let run = |reserve: Option<u32>| {
        let rig = Rig::with_fake(
            &[text("ok one"), text("summary text"), text("ok two")],
            &["--n-ctx", "24000"],
        );
        let mut settings = json!({ "oxa": { "compaction": { "softTokens": 1_000_000 } } });
        settings["compaction"] = json!({ "keepRecentTokens": 1 });
        if let Some(r) = reserve {
            settings["compaction"]["reserveTokens"] = json!(r);
        }
        rig.user_settings(settings);
        ok(&rig.print(&[]));
        ok(&rig.print(&["-c"]));
        let bodies = rig.bodies();
        assert!(
            bodies
                .iter()
                .all(|b| b["max_tokens"] == 6000 || is_summary_request(b)),
            "max_tokens 6000"
        );
        bodies.iter().any(is_summary_request)
    };
    assert!(
        !run(Some(1000)),
        "reserveTokens 1000: under the hard limit, no compaction"
    );
    assert!(
        run(Some(12000)),
        "reserveTokens 12000: over the hard limit, compacted"
    );
    assert!(run(None), "the default 16384: over the hard limit, compacted");
}

/// SET-025: `compaction.keepRecentTokens` (default 4000 instead of 20000).
#[test]
fn set_025() {
    // invariant: oxa's default kept tail is 4000 [P: pi uses 20000 with the same key]. The
    // kept tail is re-prefilled, so a short session fits inside it: nothing is cut and no
    // summary is requested.
    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(json!({ "oxa": { "compaction": { "softTokens": 5 } } }));
    ok(&rig.print(&[]));
    assert!(
        !rig.bodies().iter().any(is_summary_request),
        "the default tail keeps a short session whole"
    );
    let listing = stdout(&rig.oxa(&["config"]));
    assert!(
        listing.contains("compaction.keepRecentTokens = 4000  [default]"),
        "the default listed by `oxa config` is 4000:\n{listing}"
    );
}

/// SET-026: `compaction.modelOverrides` keyed by engine model id.
#[test]
fn set_026() {
    // invariant: S4: an override naming another model is parsed and ignored with a notice; the
    // summary always runs on the session model (one model is loaded at a time).
    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(json!({
        "compaction": { "keepRecentTokens": 1, "modelOverrides": { "fake-model": "other-model" } },
        "oxa": { "compaction": { "softTokens": 5 } }
    }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(
        stderr(&out).contains("compaction.modelOverrides"),
        "the override is reported: {}",
        stderr(&out)
    );
    let summary: Vec<Value> = rig.bodies().into_iter().filter(is_summary_request).collect();
    assert!(!summary.is_empty(), "a summary request was made");
    assert!(
        summary.iter().all(|b| b["model"] == "fake-model"),
        "the summary runs on the session model"
    );
}

/// SET-027: `oxa.compaction.softTokens`.
#[test]
fn set_027() {
    // invariant: oxa-only key. The soft limit triggers compaction after the reply (W5); a limit
    // far above the session never triggers it.
    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(
        json!({ "compaction": { "keepRecentTokens": 1 }, "oxa": { "compaction": { "softTokens": 5 } } }),
    );
    ok(&rig.print(&[]));
    assert!(
        rig.bodies().iter().any(is_summary_request),
        "a soft limit of 5 tokens triggers a summary"
    );

    let rig = Rig::new(&[text("ok one"), text("summary text")]);
    rig.user_settings(json!({ "compaction": { "keepRecentTokens": 1 }, "oxa": { "compaction": { "softTokens": 1000000 } } }));
    ok(&rig.print(&[]));
    assert!(
        !rig.bodies().iter().any(is_summary_request),
        "a soft limit above the session never triggers"
    );
}

/// SET-028: `oxa.toolOutputCap`, default 16 KiB.
#[test]
fn set_028() {
    // invariant: W4: the cap is applied deterministically to the tool result on the wire; the
    // default is 16 KiB, so a 5000-byte output is sent whole.
    let rig = Rig::new(&[bash_call("yes a | head -c 5000"), text("done")]);
    rig.user_settings(json!({ "oxa": { "toolOutputCap": 1024 } }));
    ok(&rig.print(&[]));
    let capped = tool_result(&rig.bodies()[1]).len();
    assert!(
        capped < 2048,
        "cap 1024 cuts the 5000-byte output (got {capped} bytes)"
    );

    let rig = Rig::new(&[bash_call("yes a | head -c 5000"), text("done")]);
    ok(&rig.print(&[]));
    assert_eq!(
        tool_result(&rig.bodies()[1]).len(),
        5000,
        "the default cap is 16 KiB: sent whole"
    );
}

/// SET-029: `branchSummary.reserveTokens`, `branchSummary.skipPrompt`.
#[test]
fn set_029() {
    // pi docs: settings.md `branchSummary.reserveTokens` "Tokens reserved when summarizing
    // branch history" (oxa sends the abandoned wire whole, W1, so the reserve bounds the
    // summary's output: min(max_output, reserve * 4 / 5)); `branchSummary.skipPrompt` "Skip the
    // branch-summary prompt and default to no summary".
    let setup = |settings: Value| {
        let t = Setup::new()
            .settings(settings)
            .script(&[
                text("first answer"),
                text("second answer"),
                text("the branch summary"),
            ])
            .start();
        t.prompt("first question", "first answer");
        t.prompt("second question", "second answer");
        open_tree(&t);
        tree_select(&t, "first answer");
        t.key("enter");
        t.wait_gone("Session Tree");
        t
    };
    // The prompt: pick "Summarize" (the second choice); the request carries the budget.
    let t = setup(json!({ "branchSummary": { "reserveTokens": 2000 } }));
    t.wait_text("No summary");
    t.key("down");
    t.key("enter");
    let chats = t.wait_chats(3);
    let summary = &chats[2];
    assert_eq!(summary.kind, "branch_summary");
    assert_eq!(
        summary.max_tokens,
        1600.min(chats[0].max_tokens),
        "reserve 2000 -> 1600 tokens"
    );
    t.wait_text("Branch summary (ctrl+o to expand)");
    drop(t);
    // skipPrompt: no question, no summary.
    let t = setup(json!({ "branchSummary": { "skipPrompt": true } }));
    t.wait_text("Navigated to selected point");
    let s = t.settle();
    assert!(!s.contains("No summary"), "no question\n{s:?}");
    assert_eq!(t.chats().len(), 2, "no summary request");
}

/// SET-030: `theme` (default `system`).
#[test]
fn set_030() {
    // pi docs: settings.md `theme` (default `system`): "Built-in or custom theme name.
    // `system` derives colors from the terminal theme." The terminal answers OSC 10/11 at start;
    // pi theme.ts detectTerminalTheme: the reported background decides, else COLORFGBG, else
    // dark. Observed on the user message block's background.
    let light_term = TermProfile {
        name: "light",
        fg_rgb: Some((0, 0, 0)),
        bg_rgb: Some((0xffff, 0xffff, 0xffff)),
        ..TermProfile::kitty()
    };
    let block_bg = |profile: TermProfile, settings: Value, env: &[(&str, &str)]| {
        let mut s = Setup::new()
            .profile(profile)
            .settings(settings)
            .replies("an answer", 1);
        for (k, v) in env {
            s = s.env(k, v);
        }
        let t = s.start();
        t.prompt("the question", "an answer");
        let s = t.screen();
        let cell = s.cell_of("the question").expect("the user message");
        lightness(&cell.bg)
    };
    let dark = |l: Option<u32>| l.is_none_or(|l| l < 100);
    let light = |l: Option<u32>| l.is_some_and(|l| l > 150);
    // Default = system: follows the terminal.
    assert!(
        dark(block_bg(TermProfile::kitty(), json!({}), &[])),
        "dark terminal: dark blocks"
    );
    assert!(
        light(block_bg(light_term.clone(), json!({}), &[])),
        "light terminal: light blocks"
    );
    // A terminal that answers nothing: COLORFGBG decides, else dark.
    assert!(
        dark(block_bg(TermProfile::silent(), json!({}), &[])),
        "no answer: dark"
    );
    assert!(
        light(block_bg(
            TermProfile::silent(),
            json!({}),
            &[("COLORFGBG", "0;15")]
        )),
        "COLORFGBG light"
    );
    // A named theme wins over the terminal.
    assert!(
        light(block_bg(TermProfile::kitty(), json!({ "theme": "light" }), &[])),
        "theme light"
    );
    assert!(
        dark(block_bg(light_term, json!({ "theme": "dark" }), &[])),
        "theme dark"
    );
}

/// SET-031: `quietStartup` `true|false|"header"`.
#[test]
fn set_031() {
    // pi docs: settings.md `quietStartup`: `true` hides the startup header and loaded-resource
    // listing; `"header"` keeps the header (version and key hints) but hides the model scope
    // line and the resource listing.
    let header = |settings: Value| {
        let t = Setup::new().settings(settings).start();
        let s = t.settle();
        (
            s.contains("oxa v"),
            s.contains("ctrl+d exit"),
            s.find_row("fake-model").is_some_and(|r| r < 5),
        )
    };
    assert_eq!(header(json!({})), (true, true, true), "default: everything");
    assert_eq!(header(json!({ "quietStartup": false })), (true, true, true));
    assert_eq!(
        header(json!({ "quietStartup": "header" })),
        (true, true, false),
        "header only"
    );
    assert_eq!(
        header(json!({ "quietStartup": true })),
        (false, false, false),
        "nothing"
    );
}

/// SET-032: `tuiMode` `regular|fullscreen`.
#[test]
fn set_032() {
    // pi docs: settings.md `tuiMode` (default `fullscreen`); `--tui-mode` wins for one run.
    let alt = |settings: Value, args: &[&str]| {
        let t = Setup::new().settings(settings).args(args).start();
        t.pty.modes().alt_screen
    };
    assert!(alt(json!({}), &[]), "fullscreen by default (alternate screen)");
    assert!(
        !alt(json!({ "tuiMode": "regular" }), &[]),
        "regular: the main screen"
    );
    assert!(alt(json!({ "tuiMode": "fullscreen" }), &[]));
    assert!(
        alt(json!({ "tuiMode": "regular" }), &["--tui-mode", "fullscreen"]),
        "the flag wins"
    );
}

/// SET-033: `fullscreenExitOutput` `transcript|resume-hint`.
#[test]
fn set_033() {
    // pi docs: settings.md `fullscreenExitOutput` "Output printed when fullscreen mode exits":
    // `transcript` (default) leaves the conversation on the normal screen, `resume-hint` only
    // the resume line.
    let exit = |settings: Value| {
        let mut t = Setup::new()
            .settings(settings)
            .replies("the final answer", 1)
            .start();
        t.prompt("a question", "the final answer");
        t.exit();
        t.screen()
    };
    for s in [
        exit(json!({})),
        exit(json!({ "fullscreenExitOutput": "transcript" })),
    ] {
        assert!(!s.modes().alt_screen);
        assert!(
            s.contains("a question") && s.contains("the final answer"),
            "the transcript stays\n{s:?}"
        );
        assert!(s.contains("To resume this session"), "{s:?}");
    }
    let s = exit(json!({ "fullscreenExitOutput": "resume-hint" }));
    assert!(!s.contains("the final answer"), "no transcript\n{s:?}");
    assert!(s.contains("To resume this session"), "{s:?}");
}

/// SET-034: `fullscreenScrollbar` `auto|always|hidden`.
#[test]
fn set_034() {
    // pi docs: settings.md `fullscreenScrollbar` (default `auto`) "Fullscreen transcript
    // scrollbar behavior". pi scroll-view.ts: `auto` is transient (only when the transcript is
    // taller than the view, while scrolling and for a second after: scrollbarHideDelayMs 1000);
    // `always` stays, also when the transcript fits (the thumb fills the track); `hidden` never.
    // The scrollbar is the last column of the transcript rows. Its track (`│`) shares its glyph
    // with a code frame's side border, so read the last column rather than a row's last glyph.
    let bar = |s: &Screen| {
        let (_, cols) = s.size();
        (0..10u16).any(|r| {
            s.cell(r, cols.saturating_sub(1))
                .is_some_and(|c| c.text == "│" || c.text == "█")
        })
    };
    let start = |settings: Value, long: bool| {
        let reply = if long {
            numbered_reply("R", 80)
        } else {
            text("short")
        };
        let t = Setup::new().settings(settings).script(&[reply]).start();
        t.prompt("q", if long { "R80" } else { "short" });
        t.settle();
        t
    };
    let t = start(json!({}), true);
    assert!(!bar(&t.screen()), "auto, at rest: none\n{:?}", t.screen());
    t.key("pageUp");
    t.wait("auto: shown while scrolling", |s| bar(s));
    t.wait("auto: hidden again after a second", |s| !bar(s));
    drop(t);
    let t = start(json!({}), false);
    t.key("pageUp");
    let s = t.settle();
    assert!(!bar(&s), "auto, short: none\n{s:?}");
    drop(t);
    let t = start(json!({ "fullscreenScrollbar": "always" }), false);
    assert!(bar(&t.screen()), "always: also when short\n{:?}", t.screen());
    drop(t);
    let t = start(json!({ "fullscreenScrollbar": "always" }), true);
    assert!(bar(&t.screen()), "always: at rest");
    drop(t);
    let t = start(json!({ "fullscreenScrollbar": "hidden" }), true);
    t.key("pageUp");
    let s = t.settle();
    assert!(!bar(&s), "hidden: never\n{s:?}");
}

/// SET-035: `fullscreenCopyOnSelect`.
#[test]
fn set_035() {
    // pi docs: settings.md `fullscreenCopyOnSelect` (default true) "Copy selected text
    // automatically in fullscreen mode". A mouse drag over the reply selects it; on release the
    // selection is copied (OSC 52 on a terminal without a clipboard tool).
    let drag = |settings: Value| {
        let t = Setup::new()
            .settings(settings)
            .replies("select these words", 1)
            .start();
        t.prompt("q", "select these words");
        let s = t.settle();
        let row = s.find_row("select these words").unwrap() + 1;
        let col = s.find_col(row - 1, "select").unwrap() + 1;
        let mark = t.pty.mark();
        t.pty.send_bytes(format!("\x1b[<0;{col};{row}M").as_bytes());
        t.pty
            .send_bytes(format!("\x1b[<32;{};{row}M", col + 8).as_bytes());
        t.pty
            .send_bytes(format!("\x1b[<0;{};{row}m", col + 17).as_bytes());
        t.settle();
        String::from_utf8_lossy(&t.pty.raw_since(mark)).into_owned()
    };
    let raw = drag(json!({}));
    let at = raw.find("\x1b]52;c;").expect("copied on release");
    let b64: String = raw[at + 7..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || "+/=".contains(*c))
        .collect();
    let copied = String::from_utf8(b64_decode(&b64)).unwrap();
    assert!(copied.contains("these words"), "the selection: {copied:?}");
    assert!(
        !drag(json!({ "fullscreenCopyOnSelect": false })).contains("\x1b]52;"),
        "off: no copy"
    );
}

/// SET-036: `fullscreenWheelScrollLines` `auto` or 1..100.
#[test]
fn set_036() {
    // pi docs: settings.md `fullscreenWheelScrollLines`: lines per mouse-wheel event, 1..100;
    // `auto` (default) moves one line per isolated event (faster spins accelerate, up to 6:
    // unit-tested in oxa-interactive d2); Alt+wheel moves five times as far. Measured on the
    // numbered line shown in screen row 10 before and after one wheel-up event.
    let number_at = |s: &Screen, row: u16| -> Option<i64> {
        // `row_text` drops the scrollbar column and a code frame's side border and padding.
        let line = s.row(row);
        row_text(&line).strip_prefix('R')?.parse().ok()
    };
    let moved = |settings: Value, button: u32, want: i64| {
        let t = Setup::new()
            .settings(settings)
            .script(&[numbered_reply("R", 80)])
            .start();
        t.prompt("count", "R80");
        // The baseline is the steady bottom view: R80 on screen and row 10 unchanged for a
        // quiet period (the numbered line in row 10 is the one measured).
        let s = t.wait_steady("the bottom view drawn", |s| {
            reply_closed(s, "R80") && number_at(s, 10).is_some()
        });
        let before = number_at(&s, 10).expect("a numbered line in row 10");
        t.pty.send_bytes(format!("\x1b[<{button};5;5M").as_bytes());
        // Wait for the frame with the exact expected position (bounded; a wrong distance times
        // out with the screen dump), then assert it unchanged after the redraw is quiet.
        let s = t.wait_steady("the view moved by the expected distance", |s| {
            number_at(s, 10) == Some(before - want)
        });
        number_at(&s, 10).map(|n| before - n)
    };
    assert_eq!(moved(json!({}), 64, 1), Some(1), "auto: one line");
    assert_eq!(moved(json!({ "fullscreenWheelScrollLines": 5 }), 64, 5), Some(5));
    assert_eq!(
        moved(json!({ "fullscreenWheelScrollLines": 2 }), 64 + 8, 10),
        Some(10),
        "alt: x5"
    );
    // 100 lines (500 is clamped to 100) reach the top of this transcript in one event.
    let t = Setup::new()
        .settings(json!({ "fullscreenWheelScrollLines": 500 }))
        .script(&[numbered_reply("R", 80)])
        .start();
    t.prompt("count", "R80");
    t.wait_steady("the bottom view drawn", |s| reply_closed(s, "R80"));
    t.pty.send_bytes(b"\x1b[<64;5;5M");
    t.wait("the top in one event", |s| s.row(0).starts_with("oxa v"));
}

/// SET-037: `editorPaddingX` 0..3.
#[test]
fn set_037() {
    // pi docs: settings.md `editorPaddingX` "Horizontal editor padding from 0 to 3 cells".
    let col = |settings: Value| {
        let t = Setup::new().settings(settings).start();
        t.type_text("abc");
        let s = t.screen();
        let row = s.rows().iter().rposition(|r| r.contains("abc")).unwrap() as u16;
        s.find_col(row, "abc").unwrap()
    };
    assert_eq!(col(json!({})), 0);
    assert_eq!(col(json!({ "editorPaddingX": 2 })), 2);
    assert_eq!(col(json!({ "editorPaddingX": 9 })), 3, "at most 3");
}

/// SET-038: `outputPad` 0|1.
#[test]
fn set_038() {
    // pi docs: settings.md `outputPad` (default 1) "Horizontal transcript padding for messages,
    // tool output, `!` command output, and summary blocks".
    let cols = |settings: Value| {
        let t = Setup::new()
            .settings(settings)
            .replies("the reply text", 1)
            .start();
        t.prompt("q", "the reply text");
        t.type_text("!echo bang-output");
        t.key("enter");
        let s = t.wait("the bash output", |s| {
            s.contains("bang-output") && s.contains("exit 0")
        });
        let reply = s.find_row("the reply text").unwrap();
        let bang = s.rows().iter().rposition(|r| r.trim() == "bang-output").unwrap() as u16;
        (
            s.find_col(reply, "the reply text").unwrap(),
            s.find_col(bang, "bang-output").unwrap(),
        )
    };
    assert_eq!(cols(json!({})), (1, 1));
    assert_eq!(cols(json!({ "outputPad": 0 })), (0, 0));
}

/// SET-039: `autocompleteMaxVisible` 3..20.
#[test]
fn set_039() {
    // pi docs: settings.md `autocompleteMaxVisible` (default 5) "Visible autocomplete entries,
    // from 3 to 20". The slash-command list under the editor shows that many rows.
    let shown = |settings: Value| {
        let t = Setup::new().settings(settings).start();
        t.pty.send_text("/");
        let s = t.wait("the command list", |s| s.contains("→ "));
        let rows = s.rows();
        let last_rule = rows.iter().rposition(|r| r.starts_with("───")).unwrap();
        rows[last_rule + 1..]
            .iter()
            .filter(|r| r.starts_with("→ ") || r.starts_with("  ") && !r.trim().is_empty())
            .count()
    };
    assert_eq!(shown(json!({})), 5);
    assert_eq!(shown(json!({ "autocompleteMaxVisible": 3 })), 3);
    assert_eq!(shown(json!({ "autocompleteMaxVisible": 8 })), 8);
    assert_eq!(shown(json!({ "autocompleteMaxVisible": 1 })), 3, "at least 3");
}

/// SET-041: `terminal.showImages`, `terminal.imageWidthCells`: inline display only.
#[test]
fn set_041() {
    // pi docs: settings.md `terminal.showImages`, `terminal.imageWidthCells` (inline display).
    // invariant: nothing reaches the wire: the request is the same with or without them.
    let rig = Rig::new(&[text("ok"), text("ok")]);
    ok(&rig.print(&[]));
    rig.user_settings(json!({ "terminal": { "showImages": false, "imageWidthCells": 10 } }));
    ok(&rig.print(&[]));
    let bodies = rig.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[0]["messages"], bodies[1]["messages"],
        "the display settings reach no message"
    );
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
}

/// SET-042: `terminal.clearOnShrink`.
#[test]
fn set_042() {
    // pi docs: settings.md `terminal.clearOnShrink` (default false; PI_CLEAR_ON_SHRINK=1 when
    // unset): "Clear empty rows when rendered content shrinks" (pi re-renders fully). In the
    // regular layout the slash-command list under the editor closes: with the setting the whole
    // live region is rewritten (its unchanged editor borders too), without it only what changed.
    let shrink = |settings: Value, env: &[(&str, &str)]| {
        let mut s = Setup::new().regular().settings(settings);
        for (k, v) in env {
            s = s.env(k, v);
        }
        let t = s.start();
        t.pty.send_text("/");
        t.wait("the command list", |s| s.contains("→ "));
        t.settle();
        let mark = t.pty.mark();
        t.key("escape"); // closes the list, the editor keeps its `/` (and its borders)
        t.wait("the list closed", |s| !s.contains("→ "));
        t.settle();
        String::from_utf8_lossy(&t.pty.raw_since(mark)).contains("───")
    };
    assert!(!shrink(json!({}), &[]), "default: only the changed rows");
    assert!(
        shrink(json!({ "terminal": { "clearOnShrink": true } }), &[]),
        "full repaint"
    );
    assert!(
        shrink(json!({}), &[("OXA_CLEAR_ON_SHRINK", "1")]),
        "env when unset"
    );
    assert!(!shrink(
        json!({ "terminal": { "clearOnShrink": false } }),
        &[("OXA_CLEAR_ON_SHRINK", "1")]
    ));
}

/// SET-046: `images.blockImages`: always effectively true on the wire.
#[test]
fn set_046() {
    // pi docs: settings.md `images.blockImages`.
    // invariant: images never reach the wire; the setting only changes the inline display
    // notice, so the request is the same either way.
    let rig = Rig::new(&[text("ok"), text("ok")]);
    std::fs::write(rig.env.work().join("pic.png"), PNG_1X1).expect("write the png");
    rig.user_settings(json!({ "images": { "blockImages": false } }));
    ok(&rig.print(&["@pic.png"]));
    rig.user_settings(json!({ "images": { "blockImages": true } }));
    ok(&rig.print(&["@pic.png"]));
    let bodies = rig.bodies();
    assert_eq!(
        bodies[0]["messages"], bodies[1]["messages"],
        "blockImages reaches no message"
    );
    let last = bodies[0]["messages"].to_string();
    assert!(
        last.contains("not visible to this model"),
        "the wire carries the placeholder, not the image"
    );
    assert!(
        !last.contains("image_url") && !last.contains("base64"),
        "no image data on the wire"
    );
}

/// SET-047: `markdown.codeBlockIndent`.
#[test]
fn set_047() {
    // pi docs: settings.md `markdown.codeBlockIndent` (default two spaces) "Prefix used to
    // indent rendered code blocks". The prefix sits inside the labelled frame: output pad,
    // the frame's side border, then the configured indent.
    let line = |settings: Value| {
        let t = Setup::new()
            .settings(settings)
            .replies("```\nlet code = 1;\n```", 1)
            .start();
        t.prompt("q", "let code = 1;");
        let s = t.screen();
        assert!(
            s.rows().iter().any(|r| r.starts_with(" ┌") && r.ends_with('┐')),
            "the code frame: {s:?}"
        );
        let row = s.find_row("let code = 1;").unwrap();
        s.row(row)
    };
    assert!(
        line(json!({})).starts_with(" │  let code"),
        "pad 1 + side + two spaces"
    );
    assert!(line(json!({ "markdown": { "codeBlockIndent": "│ " } })).starts_with(" ││ let code"));
    assert!(line(json!({ "markdown": { "codeBlockIndent": "" } })).starts_with(" │let code"));
}

/// SET-048: `markdown.mermaid` `off|final|streaming`.
#[test]
fn set_048() {
    // invariant: Mermaid is shown as a highlighted code block whatever the value (oxa draws no
    // diagrams); the setting is accepted without a diagnostic.
    for value in [None, Some("off"), Some("final"), Some("streaming")] {
        let settings = match value {
            Some(v) => json!({ "markdown": { "mermaid": v } }),
            None => json!({}),
        };
        let t = Setup::new()
            .settings(settings)
            .replies("```mermaid\ngraph TD; A-->B;\n```", 1)
            .start();
        t.prompt("q", "graph TD; A-->B;");
        let s = t.settle();
        assert!(
            s.rows().iter().any(|r| r.contains("┌─ mermaid ")),
            "{value:?}: the labelled code frame\n{s:?}"
        );
        assert!(
            s.contains("│  graph TD; A-->B;"),
            "{value:?}: the diagram source inside the frame\n{s:?}"
        );
        assert!(!s.contains("```"), "{value:?}: fences are consumed\n{s:?}");
        assert!(!s.contains("markdown.mermaid"), "{value:?}: no diagnostic");
    }
}

/// SET-050: `httpProxy` applies to MCP HTTP only; the engine connection is direct.
#[test]
fn set_050() {
    // pi docs: settings.md `httpProxy` "Proxy URL applied as HTTP_PROXY and HTTPS_PROXY for
    // Pi-managed HTTP clients".
    // invariant: oxa applies it to MCP HTTP only; the engine connection stays direct, so a
    // dead proxy does not stop the run.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({ "httpProxy": "http://127.0.0.1:9" }));
    let out = rig.print(&[]);
    ok(&out);
    assert_eq!(rig.bodies().len(), 1, "the engine was reached without the proxy");
}

/// SET-051: `httpIdleTimeoutMs` is the silence limit of the engine stream.
#[test]
fn set_051() {
    // pi docs: settings.md `httpIdleTimeoutMs`.
    // invariant: the limit is on silence, not wall clock: SSE comments count as liveness, so a
    // stream that keeps sending comments past the limit is not timed out; a stream that goes
    // silent mid-reply (the fake's `stall` fault: part of the reply, then nothing) is.
    let chatty = Rig::with_fake(&[text("ok")], &["--comments-only", "1500"]);
    chatty.user_settings(json!({ "httpIdleTimeoutMs": 300 }));
    let out = chatty.print(&[]);
    ok(&out);
    assert_eq!(stdout(&out).trim(), "ok", "comments are liveness: no timeout");

    // Ten-byte deltas; the stall starts after 200 body bytes, i.e. after the first data event
    // (a silence before any data would be retried with the same Idempotency-Key, which replays).
    let stalled = json!({"text": "a reply that stalls half way", "chunk": 10, "fault": {"kind": "stall", "bytes": 200, "ms": 6000}});
    let silent = Rig::with_fake(std::slice::from_ref(&stalled), &[]);
    silent.user_settings(json!({ "httpIdleTimeoutMs": 300, "retry": { "enabled": false } }));
    let started = std::time::Instant::now();
    let out = silent.print(&[]);
    let took = started.elapsed();
    assert!(
        !out.status.success(),
        "a silent stream is timed out: {}",
        stdout(&out)
    );
    assert!(
        took < Duration::from_millis(5000),
        "the idle limit (300 ms), not the 6 s stall, ended the request: {took:?}"
    );
    assert!(
        !stdout(&out).contains("a reply that stalls half way"),
        "the cut reply is not printed as a result"
    );

    // Control: the same stall under the default limit completes.
    let patient = Rig::with_fake(&[stalled], &[]);
    let out = patient.print(&[]);
    ok(&out);
    assert!(
        stdout(&out).contains("a reply that stalls half way"),
        "{}",
        stdout(&out)
    );
}

/// SET-053: `retry.enabled`, `retry.maxRetries`, `retry.baseDelayMs`, `retry.maxAgentDelayMs`.
#[test]
fn set_053() {
    // pi docs: settings.md `retry.enabled`, `retry.maxRetries`.
    // invariant: a retry replays only before the first data byte (oxa-engine README; the
    // idempotency key makes the replay the same reply); after data has arrived the cut reply is
    // an error and is not replayed.
    let rig = Rig::new(&[http_fault(503), text("ok")]);
    rig.user_settings(json!({ "retry": { "baseDelayMs": 10 } }));
    ok(&rig.print(&[]));
    assert_eq!(rig.bodies().len(), 2, "a busy reply before any data is retried");

    let rig = Rig::new(&[http_fault(503), text("ok")]);
    rig.user_settings(json!({ "retry": { "enabled": false, "baseDelayMs": 10 } }));
    let out = rig.print(&[]);
    assert!(
        !out.status.success(),
        "retry.enabled false: the busy reply is final"
    );
    assert_eq!(rig.bodies().len(), 1);

    let rig = Rig::new(&[http_fault(503), text("ok")]);
    rig.user_settings(json!({ "retry": { "maxRetries": 0, "baseDelayMs": 10 } }));
    let out = rig.print(&[]);
    assert!(!out.status.success(), "retry.maxRetries 0: no retry");
    assert_eq!(rig.bodies().len(), 1);

    // A cut before the first content event (150 of the 200-char reply's bytes): replayed.
    let before = "x".repeat(200);
    let rig = Rig::new(&[
        json!({ "text": before, "fault": { "kind": "drop_at", "bytes": 150 } }),
        text("ok"),
    ]);
    rig.user_settings(json!({ "retry": { "baseDelayMs": 10 } }));
    ok(&rig.print(&[]));
    assert_eq!(
        rig.bodies().len(),
        2,
        "a cut before the first data byte is replayed"
    );

    // A cut after the first content event (350 bytes): replayed while the retry policy allows (#456).
    let rig = Rig::new(&[
        json!({ "text": before, "fault": { "kind": "drop_at", "bytes": 350 } }),
        text("ok"),
    ]);
    rig.user_settings(json!({ "retry": { "baseDelayMs": 10 } }));
    ok(&rig.print(&[]));
    assert_eq!(rig.bodies().len(), 2, "a cut after data is replayed");

    // With retry disabled the same cut is a final error.
    let rig = Rig::new(&[
        json!({ "text": before, "fault": { "kind": "drop_at", "bytes": 350 } }),
        text("ok"),
    ]);
    rig.user_settings(json!({ "retry": { "enabled": false, "baseDelayMs": 10 } }));
    let out = rig.print(&[]);
    assert!(!out.status.success(), "retry.enabled false: a cut is an error");
    assert_eq!(rig.bodies().len(), 1, "no replay when retry is disabled");
}

/// SET-054: `retry.provider.timeoutMs`, `retry.provider.maxRetries`, `retry.provider.maxRetryDelayMs`.
#[test]
fn set_054() {
    // pi docs: settings.md `retry.provider.maxRetries` "Provider-level retry attempts" (0).
    // invariant: the same replay rule as SET-053 and one HTTP layer, so the provider count is
    // the number of HTTP retries.
    let rig = Rig::new(&[http_fault(503), http_fault(503), text("ok")]);
    rig.user_settings(
        json!({ "retry": { "maxRetries": 0, "baseDelayMs": 10, "provider": { "maxRetries": 2 } } }),
    );
    let out = rig.print(&[]);
    assert!(
        out.status.success(),
        "retry.provider.maxRetries 2 allows two more HTTP attempts; oxa made {} request(s)",
        rig.bodies().len()
    );
    assert_eq!(rig.bodies().len(), 3);
}

/// SET-055: `shellPath` is the custom shell executable.
#[test]
fn set_055() {
    // pi docs: settings.md `shellPath` "Custom shell executable path." The bash tool runs in
    // the sandbox, where `/` is read-only and home and `/tmp` are hidden, so the custom shell
    // lives in the working directory (a real one is usually under `/usr` or `/bin`).
    let rig = Rig::new(&[bash_call("echo hi"), text("done")]);
    let shell = stub(
        &rig.env,
        "work/custom-shell.sh",
        "#!/bin/sh\necho CUSTOM-SHELL\nexec /bin/sh \"$@\"\n",
    );
    rig.user_settings(json!({ "shellPath": shell.display().to_string() }));
    ok(&rig.print(&[]));
    assert!(
        tool_result(&rig.bodies()[1]).contains("CUSTOM-SHELL"),
        "the bash tool did not run the configured shellPath"
    );
}

/// SET-056: `shellCommandPrefix` is prepended to every shell command.
#[test]
fn set_056() {
    // pi docs: settings.md `shellCommandPrefix` "Prefix prepended to every shell command." An
    // invariant of the sandbox: the prefix is part of the sandboxed command.
    let rig = Rig::new(&[bash_call("echo $OXAPFX"), text("done")]);
    rig.user_settings(json!({ "shellCommandPrefix": "export OXAPFX=prefixed;" }));
    ok(&rig.print(&[]));
    assert!(
        tool_result(&rig.bodies()[1]).contains("prefixed"),
        "the bash command did not get the shellCommandPrefix"
    );
}

/// SET-059: `packages`, `extensions`, `skills`, `prompts`, `themes` resource lists.
#[test]
fn set_059() {
    // pi docs: settings.md "Resource lists are combined." A listed skill path that does not
    // exist is reported, and the run goes on.
    let rig = Rig::new(&[text("ok")]);
    let missing = rig.env.root().join("no-such-skills");
    rig.user_settings(json!({ "skills": [missing.display().to_string()] }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(
        warned(&out, "skill path does not exist"),
        "the missing skill path is reported: {}",
        stderr(&out)
    );
}

/// SET-060: `enableSkillCommands`.
#[test]
fn set_060() {
    // pi docs: settings.md `enableSkillCommands` (default true) "Register skills as
    // `/skill:name` commands": pi interactive-mode.ts offers them in the autocomplete only when
    // it is on; a typed `/skill:name` expands either way (agent-session.ts
    // `_expandSkillCommand`, as SKL-005 checks). The built-in `ox-llm` skill (O-152).
    let check = |settings: Value, offered: bool| {
        let t = Setup::new().settings(settings).replies("done", 1).start();
        t.pty.send_text("/skill:");
        let s = t.settle();
        assert_eq!(
            s.contains("skill:ox-llm"),
            offered,
            "autocomplete offers the skill command\n{s:?}"
        );
        t.type_text("ox-llm check the engine");
        t.key("escape"); // close a completion list, if one is open
        t.settle();
        t.key("enter");
        t.wait_text("done");
        let sent = user_texts(&t.wait_chats(1)[0]).pop().unwrap();
        assert!(
            sent.contains("check the engine") && sent.len() > 200,
            "the skill body is expanded: {sent}"
        );
    };
    check(json!({}), true);
    check(json!({ "enableSkillCommands": false }), false);
}

/// SET-061: `hooks: {event: command}`: exec hooks (JSON on stdin, reply on stdout, run in bwrap).
#[test]
fn set_061() {
    // invariant: oxa only: an exec hook runs in the sandbox for its event; a failing hook is
    // reported and does not stop the run. The event name is checked against the hook events.
    let rig = Rig::new(&[text("ok")]);
    rig.user_settings(json!({
        "hooks": {
            "agent_start": "cat >/dev/null; echo HOOK-RAN >&2; exit 3",
            "not_an_event": "true"
        }
    }));
    let out = rig.print(&[]);
    ok(&out);
    assert!(
        stderr(&out).contains("agent_start hook exited with status 3"),
        "the exec hook ran: {}",
        stderr(&out)
    );
    assert!(
        warned(&out, "not_an_event"),
        "an unknown event is reported: {}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------------------------
// KEY: Keybindings (keybindings.md)
//
// The keybindings act in the interactive TUI (the editor, the selectors, the fullscreen view):
// every row presses the documented default keys through the pty, in both key encodings where
// the row depends on it (the kitty profile for keys a legacy terminal cannot send), and checks
// a keybindings.json that rebinds them.
// ---------------------------------------------------------------------------------------------

/// KEY-001: `keybindings.json` in the agent directory overrides defaults; a list or a single
/// key per id; conflicts reported.
#[test]
fn key_001() {
    // pi docs: keybindings.md "Assign keybindings": a configured value (one key or a list)
    // replaces the default; an empty list disables. pi's KeybindingsManager records a conflict
    // when two configured ids claim the same key (packages/tui/src/keybindings.ts rebuild()).
    // oxa reports conflicts and unknown ids as startup warnings in the transcript.
    let mut t = Setup::new()
        .keys(json!({
            "app.exit": "ctrl+q",
            "tui.input.newLine": ["ctrl+j", "alt+n"],
            "app.session.new": "f7",
            "app.session.tree": "f7",
            "app.not.an.action": "f8",
        }))
        .start();
    let s = t.wait("the keybinding warnings", |s| {
        s.contains("app.not.an.action") && s.contains("f7")
    });
    let text = s.text();
    assert!(
        text.contains("app.session.new") && text.contains("app.session.tree"),
        "the conflict names both actions:\n{s:?}"
    );
    // A list: both keys insert a newline.
    t.type_text("a");
    t.key("alt+n");
    t.type_text("b");
    t.key("ctrl+j");
    t.type_text("c");
    t.wait_editor("a\nb\nc");
    t.key("ctrl+c");
    t.wait_editor("");
    // The old exit key no longer exits; the new one does.
    t.key("ctrl+d");
    t.settle();
    assert!(t.pty.exit_status().is_none(), "ctrl+d no longer exits");
    t.key("ctrl+q");
    assert!(t.exited_ok(), "ctrl+q exits");
}

/// KEY-002: key syntax: `ctrl+shift+alt+super` modifiers + key; kitty keyboard protocol and
/// legacy sequences.
#[test]
fn key_002() {
    // pi docs: keybindings.md "Key syntax": `modifier+key`, modifiers ctrl, shift, alt, super
    // (combinable); special keys and f1-f12; super needs the kitty keyboard protocol.
    let keys = json!({
        "tui.editor.cursorLineStart": ["ctrl+shift+alt+super+x", "alt+h"],
        "tui.editor.cursorLineEnd": ["f5", "ctrl+alt+e"],
    });
    // kitty: the four-modifier chord arrives as CSI-u; `home` is no longer bound.
    let t = Setup::new()
        .profile(TermProfile::kitty())
        .keys(keys.clone())
        .start();
    // pi packages/tui/src/terminal.ts pushes `ESC[>7u` (disambiguate, event types, alternate keys).
    assert_eq!(
        t.pty.modes().kitty_flags(),
        7,
        "the kitty protocol is on with pi's flags"
    );
    t.type_text("abc");
    t.key("ctrl+shift+alt+super+x");
    t.pty.send_text("Z");
    t.wait_editor("Zabc");
    t.key("f5");
    t.pty.send_text("Y");
    t.wait_editor("ZabcY");
    t.key("home");
    t.pty.send_text("W");
    t.wait_editor("ZabcYW");
    // The same chord in legacy bytes from the same terminal: alt+h is ESC h.
    t.key_as("alt+h", KeyEncoding::Legacy);
    t.pty.send_text("V");
    t.wait_editor("VZabcYW");
    drop(t);
    // xterm: legacy sequences only (ESC-prefixed alt, CSI ~ for f5, ESC ^E for ctrl+alt+e).
    let t = Setup::new().keys(keys).start();
    t.type_text("abc");
    t.key("alt+h");
    t.pty.send_text("Z");
    t.wait_editor("Zabc");
    t.key("ctrl+alt+e");
    t.pty.send_text("Y");
    t.wait_editor("ZabcY");
    t.key("ctrl+a"); // the list replaced the defaults: ctrl+a no longer moves the cursor
    t.pty.send_text("X");
    t.wait_editor("ZabcYX");
}

/// KEY-003: cursor movement in the editor.
#[test]
fn key_003() {
    // pi docs: keybindings.md "Cursor movement" (Linux defaults): cursorLeft (left, ctrl+b),
    // cursorRight (right, ctrl+f), cursorWordLeft (alt+left, ctrl+left, alt+b), cursorWordRight
    // (alt+right, ctrl+right, alt+f), cursorLineStart (home, ctrl+a), cursorLineEnd (end,
    // ctrl+e), jumpForward (ctrl+]), jumpBackward (ctrl+alt+]), cursorUp/Down (up/down, history
    // at the edges), historyPrevious/Next (none), pageUp/pageDown (pageUp, ctrl+pageUp, ..).
    // The cursor is observed by typing a marker `|` where it stands.
    for profile in [TermProfile::xterm(), TermProfile::kitty()] {
        let name = profile.name;
        // #1512 S3: 25 rows: the 20-row editor, its rules, the footer and the status row.
        let t = Setup::new()
            .profile(profile)
            .size(25, 80)
            .replies("first reply", 1)
            .start();
        let base = "one two three";
        let cases: &[(&[&str], &str)] = &[
            (&["left"], "one two thre|e"),
            (&["ctrl+b"], "one two thre|e"),
            (&["home"], "|one two three"),
            (&["ctrl+a"], "|one two three"),
            (&["home", "right"], "o|ne two three"),
            (&["home", "ctrl+f"], "o|ne two three"),
            (&["alt+left"], "one two |three"),
            (&["ctrl+left"], "one two |three"),
            (&["alt+b"], "one two |three"),
            (&["home", "alt+right"], "one| two three"),
            (&["home", "ctrl+right"], "one| two three"),
            (&["home", "alt+f"], "one| two three"),
            (&["home", "end"], "one two three|"),
            (&["home", "ctrl+e"], "one two three|"),
            (&["home", "ctrl+]", "t"], "one |two three"),
            (&["ctrl+alt+]", "o"], "one tw|o three"),
        ];
        for (keys, want) in cases {
            t.type_text(base);
            t.keys(keys);
            t.pty.send_text("|");
            t.wait_editor(want);
            t.key("ctrl+c"); // app.clear empties a non-empty editor
            t.wait_editor("");
            let _ = name;
        }
        // Up/down move between lines (ctrl+j inserts a newline).
        t.type_text("ab");
        t.key("ctrl+j");
        t.type_text("cdef");
        t.key("up");
        t.pty.send_text("|");
        t.wait_editor("ab|\ncdef");
        t.key("down");
        t.pty.send_text("^");
        t.wait_editor("ab|\ncde^f"); // the column is kept
        t.key("ctrl+c");
        t.wait_editor("");
        // pageUp / pageDown move by a page of the editor (more lines than it shows). In the
        // fullscreen layout plain pageUp/pageDown scroll the transcript (tui.altScreen.*, KEY-007),
        // so the editor's ctrl+pageUp / ctrl+pageDown are used here; plain keys in regular mode below.
        let long: String = (1..=40)
            .map(|i| format!("L{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        t.pty.send_text(&long); // LF is ctrl+j: a new line each (a paste would be a marker)
        t.wait(&format!("{name}: long paste"), |s| editor_text(s).contains("L40"));
        t.key("ctrl+pageUp");
        t.pty.send_text("|");
        let s = t.wait(&format!("{name}: marker after ctrl+pageUp"), |s| {
            editor_text(s).contains('|')
        });
        let row = editor_text(&s)
            .lines()
            .find(|l| l.contains('|'))
            .unwrap()
            .to_string();
        assert!(
            row.starts_with('L') && row.ends_with('|') && row != "L40|",
            "{name}: pageUp moved up: {row}"
        );
        t.key("backspace");
        t.key("ctrl+pageDown");
        t.pty.send_text("|");
        t.wait(&format!("{name}: marker after ctrl+pageDown"), |s| {
            editor_text(s).contains("L40|")
        });
        t.key("ctrl+c");
        t.wait_editor("");
        // History: up on an empty editor shows the previous prompt; historyPrevious has no key.
        t.prompt("remember me", "first reply");
        t.wait_editor("");
        t.key("up");
        t.wait_editor("remember me");
        t.key("down");
        t.wait_editor("");
    }
    // Regular layout: plain pageUp / pageDown page the editor (25 rows, as above).
    let t = Setup::new().regular().size(25, 80).start();
    let long: String = (1..=40)
        .map(|i| format!("L{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    t.pty.send_text(&long);
    t.wait("long text", |s| editor_text(s).contains("L40"));
    t.key("pageUp");
    t.pty.send_text("|");
    let s = t.wait("marker after pageUp", |s| editor_text(s).contains('|'));
    let row = editor_text(&s)
        .lines()
        .find(|l| l.contains('|'))
        .unwrap()
        .to_string();
    assert!(row.ends_with('|') && row != "L40|", "pageUp moved up: {row}");
    t.key("backspace");
    t.key("pageDown");
    t.pty.send_text("|");
    t.wait("marker after pageDown", |s| editor_text(s).contains("L40|"));
    drop(t);
    // Rebound: cursorLineStart = alt+a, historyPrevious = alt+p; `home` no longer moves.
    let t = Setup::new()
        .keys(json!({ "tui.editor.cursorLineStart": "alt+a", "tui.editor.historyPrevious": ["alt+p"] }))
        .replies("first reply", 1)
        .start();
    t.type_text("xyz");
    t.key("home");
    t.pty.send_text("|");
    t.wait_editor("xyz|");
    t.key("alt+a");
    t.pty.send_text("^");
    t.wait_editor("^xyz|");
    t.key("ctrl+c");
    t.prompt("older prompt", "first reply");
    t.wait_editor("");
    t.key("alt+p");
    t.wait_editor("older prompt");
}

/// KEY-004: text editing in the editor.
#[test]
fn key_004() {
    // pi docs: keybindings.md "Text editing" (Linux defaults only): deleteCharBackward
    // (backspace), deleteCharForward (delete, ctrl+d), deleteWordBackward (ctrl+w,
    // alt+backspace), deleteWordForward (alt+d, alt+delete), deleteToLineStart (ctrl+u),
    // deleteToLineEnd (ctrl+k), yank (ctrl+y), yankPop (alt+y), undo (ctrl+-).
    for profile in [TermProfile::xterm(), TermProfile::kitty()] {
        let t = Setup::new().profile(profile).start();
        let base = "one two three";
        let cases: &[(&[&str], &str)] = &[
            (&["backspace"], "one two thre|"),
            (&["home", "delete"], "|ne two three"),
            (&["home", "ctrl+d"], "|ne two three"),
            (&["ctrl+w"], "one two |"),
            (&["alt+backspace"], "one two |"),
            (&["home", "alt+d"], "| two three"),
            (&["home", "alt+delete"], "| two three"),
            (&["alt+b", "ctrl+u"], "|three"),
            (&["home", "alt+f", "ctrl+k"], "one|"),
            // The kill ring: two kills, yank the last, yank-pop to the one before.
            (&["ctrl+w", "ctrl+a", "alt+d", "ctrl+e", "ctrl+y"], " two one|"),
            (
                &["ctrl+w", "ctrl+a", "alt+d", "ctrl+e", "ctrl+y", "alt+y"],
                " two three|",
            ),
            (&["ctrl+w", "ctrl+-"], "one two three|"),
        ];
        for (keys, want) in cases {
            t.type_text(base);
            t.keys(keys);
            t.pty.send_text("|");
            t.wait_editor(want);
            t.key("ctrl+c");
            t.wait_editor("");
        }
    }
    // Rebound: undo = ctrl+z is not possible without unbinding app.suspend; use alt+u.
    let t = Setup::new()
        .keys(json!({ "tui.editor.undo": "alt+u", "tui.editor.deleteWordBackward": [] }))
        .start();
    t.type_text("alpha beta");
    t.key("ctrl+w");
    t.settle();
    assert_eq!(t.editor(), "alpha beta", "an empty list unbinds ctrl+w");
    t.key("backspace");
    t.wait_editor("alpha bet");
    t.key("alt+u");
    t.wait_editor("alpha beta");
}

/// KEY-005: input: newline, submit, tab, copy.
#[test]
fn key_005() {
    // pi docs: keybindings.md "Input": tui.input.newLine (shift+enter, ctrl+j), submit (enter),
    // tab (tab: completion), copy (ctrl+c). shift+enter needs the kitty protocol (a legacy
    // terminal sends a plain CR for it); ctrl+j works everywhere.
    let mut t = Setup::new()
        .profile(TermProfile::kitty())
        .fake(tui::FakeOpts {
            done_gap_ms: 2500,
            ..tui::FakeOpts::default()
        })
        .replies("got it", 2)
        .start();
    t.type_text("first line");
    t.key("shift+enter");
    t.type_text("second line");
    t.wait_editor("first line\nsecond line");
    t.key("ctrl+j");
    t.type_text("third");
    t.key("enter");
    t.wait_text("got it");
    assert_eq!(t.prompts(), vec!["first line\nsecond line\nthird".to_string()]);
    t.wait_worker_reply_end("key005-after-reply.html");
    // tab completes a slash command.
    t.type_text("/hotk");
    t.key("tab");
    t.wait("tab completed /hotkeys", |s| {
        editor_text(s).starts_with("/hotkeys")
    });
    t.key("ctrl+c");
    t.wait_editor("");
    t.exit();

    let t = Setup::new().replies("got it", 1).start();
    t.type_text("a");
    t.key("ctrl+j");
    t.type_text("b");
    t.wait_editor("a\nb");
    t.key("enter");
    t.wait_text("got it");
    assert_eq!(t.prompts(), vec!["a\nb".to_string()]);

    // Rebound: submit = ctrl+s, newLine = enter.
    let t = Setup::new()
        .keys(json!({ "tui.input.submit": "ctrl+s", "tui.input.newLine": ["enter"] }))
        .replies("got it", 1)
        .start();
    t.type_text("x");
    t.key("enter");
    t.type_text("y");
    t.wait_editor("x\ny");
    assert!(t.prompts().is_empty());
    t.key("ctrl+s");
    t.wait_text("got it");
    assert_eq!(t.prompts(), vec!["x\ny".to_string()]);
}

/// KEY-006: select lists.
#[test]
fn key_006() {
    // pi docs: keybindings.md "Selection": tui.select.up (up), down (down), pageUp (pageUp),
    // pageDown (pageDown), confirm (enter), cancel (escape, ctrl+c). Driven on the /thinking
    // list (seven levels, off preselected); the chosen level shows in the footer.
    let footer = |s: &Screen| footer_row(s);
    let open = |t: &Tui| {
        t.slash("/thinking");
        t.wait_text("Thinking level");
    };
    let chosen = |t: &Tui, level: &str| {
        t.wait_gone("Thinking level");
        t.wait(&format!("footer {level}"), |s| {
            footer(s).ends_with(&format!("• {level}"))
        });
    };
    // Starts from `off` (the default is `medium`, SET-005): the preselected row is `off`.
    let t = Setup::new()
        .settings(json!({ "defaultThinkingLevel": "off" }))
        .start();
    open(&t);
    t.keys(&["down", "down", "up", "down"]);
    t.key("enter");
    chosen(&t, "low");
    open(&t);
    t.key("pageDown");
    t.key("enter");
    chosen(&t, "max");
    open(&t);
    t.key("pageUp");
    t.key("enter");
    chosen(&t, "off");
    for cancel in ["escape", "ctrl+c"] {
        open(&t);
        t.key("down");
        t.key(cancel);
        chosen(&t, "off");
    }
    drop(t);
    // Rebound: confirm = f2, down = ctrl+n (enter and the down arrow no longer act in lists).
    let t = Setup::new()
        .settings(json!({ "defaultThinkingLevel": "off" }))
        .keys(json!({ "tui.select.confirm": "f2", "tui.select.down": "ctrl+n" }))
        .start();
    open(&t);
    t.key("down");
    t.key("ctrl+n");
    t.key("enter");
    let s = t.settle();
    assert!(s.contains("Thinking level"), "enter no longer confirms\n{s:?}");
    t.key("f2");
    chosen(&t, "minimal");
}

/// KEY-007: fullscreen scroll.
#[test]
fn key_007() {
    // pi docs: keybindings.md "Fullscreen": tui.altScreen.pageUp (pageUp), pageDown
    // (pageDown), halfPageUp / halfPageDown / lineUp / lineDown (no default), top (ctrl+home),
    // bottom (ctrl+end). The transcript is a reply of 80 numbered lines.
    let t = Setup::new()
        .keys(json!({
            "tui.altScreen.lineUp": "f5",
            "tui.altScreen.lineDown": "f6",
            "tui.altScreen.halfPageUp": "f7",
            "tui.altScreen.halfPageDown": "f8",
        }))
        .script(&[numbered_reply("R", 80)])
        .start();
    t.prompt("count", "R80");
    let row_of = |s: &Screen, needle: &str| row_index(s, needle);
    let at_bottom = t.wait_steady("the reply drawn to its closing fence", |s| {
        reply_closed(s, "R80") && row_of(s, "R70").is_some()
    });
    let r70 = row_of(&at_bottom, "R70").unwrap();
    // Numbered rows give a content origin independent of frame borders/editor height.
    let origin = |screen: &Screen| {
        screen.rows().iter().enumerate().find_map(|(row, line)| {
            let number = row_text(line).strip_prefix('R')?.parse::<i64>().ok()?;
            Some(number - row as i64)
        })
    };
    let bottom_origin = origin(&at_bottom).unwrap();
    t.key("pageUp");
    let s = t.wait_steady("a page up", |s| row_of(s, "R80").is_none() && origin(s).is_some());
    let page_shift = bottom_origin - origin(&s).unwrap();
    assert!(page_shift > 0, "page up must move the transcript");
    assert!(
        row_of(&s, "R70").is_none_or(|r| r > r70),
        "the view moved up\n{s:?}"
    );
    t.key("pageDown");
    t.wait("back at the bottom", |s| row_of(s, "R80").is_some());
    t.key("ctrl+home");
    t.wait("the top", |s| s.row(0).starts_with("oxa v"));
    t.key("ctrl+end");
    t.wait_steady("the bottom", |s| {
        reply_closed(s, "R80") && row_of(s, "R70") == Some(r70)
    });
    // lineUp moves the transcript down by exactly one row; lineDown back.
    t.key("f5");
    t.wait("one line up", |s| row_of(s, "R70") == Some(r70 + 1));
    t.key("f6");
    t.wait("one line down", |s| row_of(s, "R70") == Some(r70));
    // halfPageUp: R80 leaves the view but less far than a page; halfPageDown comes back.
    t.key("f7");
    let s = t.wait_steady("half a page up", |s| {
        row_of(s, "R80").is_none() && origin(s).is_some()
    });
    let half_shift = bottom_origin - origin(&s).unwrap();
    assert!(
        half_shift > 0 && half_shift < page_shift,
        "half-page movement must be positive and smaller than a page: {half_shift}/{page_shift}\n{s:?}"
    );
    // Page Up keeps four overlapping rows; Half Page Up is half the full viewport,
    // rounded down, rather than half the Page Up distance.
    assert_eq!(
        half_shift,
        (page_shift + 4) / 2,
        "half-page movement must be half the viewport: {half_shift}/{page_shift}\n{s:?}"
    );
    t.key("f8");
    t.wait("half a page down", |s| row_of(s, "R70") == Some(r70));
    drop(t);
    // An empty list unbinds: pageUp then pages the editor instead of the transcript.
    let t = Setup::new()
        .keys(json!({ "tui.altScreen.pageUp": [] }))
        .script(&[numbered_reply("R", 80)])
        .start();
    t.prompt("count", "R80");
    t.key("pageUp");
    let s = t.settle();
    assert!(
        row_of(&s, "R80").is_some(),
        "the transcript did not scroll\n{s:?}"
    );
    // Witness: the other scroll keys still work.
    t.key("ctrl+home");
    t.wait("the top", |s| s.row(0).starts_with("oxa v"));
}

/// KEY-008: fullscreen prompt jump.
#[test]
fn key_008() {
    // pi docs: keybindings.md "Fullscreen": tui.altScreen.previousPrompt (ctrl+shift+up,
    // ctrl+up), nextPrompt (ctrl+shift+down, ctrl+down) (Linux defaults only).
    for profile in [TermProfile::xterm(), TermProfile::kitty()] {
        let t = Setup::new()
            .profile(profile)
            .script(&[
                numbered_reply("A", 30),
                numbered_reply("B", 30),
                numbered_reply("C", 30),
            ])
            .start();
        t.prompt("first question", "A30");
        t.prompt("second question", "B30");
        t.prompt("third question", "C30");
        let near_top = |s: &Screen, needle: &str| s.find_row(needle).is_some_and(|r| r < 4);
        t.key("ctrl+shift+up");
        t.wait("third prompt at the top", |s| near_top(s, "third question"));
        t.key("ctrl+up");
        t.wait("second prompt at the top", |s| near_top(s, "second question"));
        t.key("ctrl+shift+up");
        t.wait("first prompt at the top", |s| near_top(s, "first question"));
        t.key("ctrl+shift+down");
        t.wait("second prompt again", |s| near_top(s, "second question"));
        t.key("ctrl+down");
        t.wait("third prompt again", |s| near_top(s, "third question"));
    }
    // Rebound: previousPrompt = f9 only.
    let t = Setup::new()
        .keys(json!({ "tui.altScreen.previousPrompt": "f9" }))
        .script(&[numbered_reply("A", 30), numbered_reply("B", 30)])
        .start();
    t.prompt("first question", "A30");
    t.prompt("second question", "B30");
    t.key("ctrl+up");
    let s = t.settle();
    assert!(s.contains("B30"), "ctrl+up is unbound\n{s:?}");
    t.key("f9");
    t.wait("second prompt at the top", |s| {
        s.find_row("second question").is_some_and(|r| r < 4)
    });
}

/// KEY-009: fullscreen search.
#[test]
fn key_009() {
    // pi docs: keybindings.md "Fullscreen": tui.altScreen.search (ctrl+shift+f),
    // searchNext (enter, ctrl+g), searchPrevious (shift+enter, ctrl+shift+g), searchClose
    // (escape). ctrl+shift+f and shift+enter need the kitty protocol (a legacy terminal sends
    // ctrl+f / CR for them).
    // invariant: oxa adds `alt+s` to `tui.altScreen.search`, which a legacy terminal can send;
    // ctrl+shift+f keeps working with the kitty protocol (asserted below), and OXA's footer
    // shows only keys the terminal can send (`key_009_legacy_terminal_shows_and_takes_the_fallback`).
    let row_of = |s: &Screen, needle: &str| row_index(s, needle);
    let t = Setup::new()
        .profile(TermProfile::kitty())
        .script(&[numbered_reply("R", 80)])
        .start();
    t.prompt("count", "R80");
    t.key("ctrl+shift+f");
    t.pty.send_text("R07");
    t.wait("R07 revealed", |s| row_of(s, "R07").is_some());
    t.key("escape");
    t.wait_gone("Search:");
    // Several matches: "R6" hits R60..R69; next / previous walk through them (the bar counts
    // `k/10`), and the current match is in view.
    let search_row = |s: &Screen| -> Option<String> {
        s.rows()
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .find(|r| r.starts_with("Search: R6"))
    };
    let counter = |s: &Screen| -> Option<usize> {
        let row = search_row(s)?;
        let k = row.split_whitespace().last()?.split('/').next()?.parse().ok()?;
        Some(k)
    };
    t.key("ctrl+shift+f");
    t.pty.send_text("R6");
    // The row itself, not `/10` anywhere on screen (the footer's token counts can contain it).
    let s = t.wait("ten matches", |s| {
        search_row(s).is_some_and(|r| r.ends_with("/10"))
    });
    let k0 = counter(&s).expect("a current match");
    let step = |k: usize, d: isize| ((k as isize - 1 + d).rem_euclid(10) + 1) as usize;
    let seen =
        |s: &Screen, k: usize| counter(s) == Some(k) && row_index(s, &format!("R6{}", k - 1)).is_some();
    t.key("enter");
    t.wait("next match", |s| seen(s, step(k0, 1)));
    t.key("ctrl+g");
    t.wait("next match again", |s| seen(s, step(k0, 2)));
    t.key("shift+enter");
    t.wait("previous match", |s| seen(s, step(k0, 1)));
    t.key("ctrl+shift+g");
    t.wait("previous match again", |s| seen(s, k0));
    t.key("escape");
    t.wait_gone("Search:");
    // Closed: typing goes to the editor again.
    t.type_text("typed after search");
    drop(t);
    // Rebound: search = f4 (works on a legacy terminal).
    let t = Setup::new()
        .keys(json!({ "tui.altScreen.search": "f4" }))
        .script(&[numbered_reply("R", 80)])
        .start();
    t.prompt("count", "R80");
    t.key("f4");
    t.pty.send_text("R05");
    t.wait("R05 revealed", |s| row_of(s, "R05").is_some());
    assert_eq!(t.editor(), "", "the query went to the search, not the editor");
    drop(t);
    // Adapted (oxa): the default `alt+s` opens the search on a legacy terminal.
    let t = Setup::new().script(&[numbered_reply("R", 80)]).start();
    t.prompt("count", "R80");
    t.key("alt+s");
    t.pty.send_text("R03");
    t.wait("R03 revealed", |s| row_of(s, "R03").is_some());
    assert_eq!(t.editor(), "", "the query went to the search, not the editor");
}

/// KEY-009 (O-199 first run, foot): the kitty keyboard protocol keeps one flags stack per
/// screen, so the push must be made on the alternate screen too; then ctrl+shift+f arrives as
/// CSI-u and opens the search. (Pushed on the main screen only, the alternate screen stays
/// legacy and ctrl+shift+f arrives as ctrl+f, the cursor key.)
#[test]
fn key_009_kitty_protocol_on_the_alternate_screen() {
    let foot = TermProfile {
        name: "foot",
        env: vec![("TERM".into(), "foot".into())],
        ..TermProfile::kitty()
    };
    // The footer's key hints are OXA's look (the harness drives pi's look by default).
    let t = Setup::new()
        .profile(foot)
        .env("OXA_SKIN", "oxa")
        .size(30, 200)
        .script(&[numbered_reply("R", 40)])
        .start();
    let m = t.pty.modes();
    assert!(m.alt_screen, "fullscreen by default: {m:?}");
    assert_eq!(
        m.kitty_flags(),
        7,
        "the alternate screen has the flags pushed: {m:?}"
    );
    t.prompt("count", "R40");
    let s = t.wait("the footer advertises ctrl+shift+f", |s| {
        s.contains("ctrl+shift+f search")
    });
    assert!(!s.contains("alt+s search"), "the first key is shown\n{s:?}");
    // The CSI-u bytes a kitty-protocol terminal sends (flags 7: `CSI 102 ; 6 u`).
    let bytes = t
        .pty
        .key_bytes("ctrl+shift+f", KeyEncoding::Auto)
        .expect("encodable");
    assert!(
        bytes.starts_with(b"\x1b[102"),
        "{:?}",
        String::from_utf8_lossy(&bytes)
    );
    t.pty.send_bytes(&bytes);
    t.wait("search open", |s| s.contains("Search:"));
    t.pty.send_text("R07");
    t.wait("R07 revealed", |s| row_index(s, "R07").is_some());
    assert_eq!(t.editor(), "", "the query went to the search, not the editor");
}

/// KEY-009 (O-199 first run): a terminal without the kitty keyboard protocol cannot send
/// ctrl+shift+f (it sends ctrl+f). The footer then shows the fallback `alt+s`, which works.
#[test]
fn key_009_legacy_terminal_shows_and_takes_the_fallback() {
    // The footer's key hints are OXA's look (the harness drives pi's look by default).
    let t = Setup::new()
        .env("OXA_SKIN", "oxa")
        .size(30, 200)
        .script(&[numbered_reply("R", 40)])
        .start();
    assert_eq!(t.pty.modes().kitty_flags(), 0, "legacy terminal");
    t.prompt("count", "R40");
    let s = t.wait("the footer advertises the fallback", |s| {
        s.contains("alt+s search")
    });
    assert!(
        !s.contains("ctrl+shift+f"),
        "a key the terminal cannot send is not advertised\n{s:?}"
    );
    t.key_as("alt+s", KeyEncoding::Legacy);
    t.wait("search open", |s| s.contains("Search:"));
    t.pty.send_text("R05");
    t.wait("R05 revealed", |s| row_index(s, "R05").is_some());
    assert_eq!(t.editor(), "", "the query went to the search, not the editor");
}

/// KEY-010: application keys: interrupt, clear, exit, suspend, external editor.
#[test]
fn key_010() {
    // pi docs: keybindings.md "Application": app.interrupt (escape), app.clear (ctrl+c: clear,
    // then exit), app.exit (ctrl+d, empty editor), app.suspend (ctrl+z), app.editor.external
    // (ctrl+g).
    const EDITOR: &str =
        "#!/bin/sh\ncat \"$1\" > \"$(dirname \"$0\")/seen.txt\"\nprintf 'from the stub editor\\n' > \"$1\"\n";
    let t = Setup::new()
        .script(&[held("never shown", 8000), text("second answer")])
        .start();
    // escape aborts the running reply; the session takes the next prompt.
    t.type_text("first");
    t.key("enter");
    t.wait_chats(1);
    t.key("escape");
    t.wait("the abort is shown", |s| s.contains("aborted"));
    t.prompt("again", "second answer");
    let s = t.screen();
    assert!(!s.contains("never shown"), "{s:?}");
    assert_eq!(t.prompts().last().map(String::as_str), Some("again"));
    // ctrl+g: the editor text goes to $EDITOR's file, its result comes back.
    drop(t);
    // (The command is relative to the working directory `work/`.)
    let mut t = Setup::new()
        .env("EDITOR", "../bin/ed.sh")
        .exec("bin/ed.sh", EDITOR)
        .start();
    let seen = t.env.root().join("bin/seen.txt");
    t.type_text("draft text");
    let mark = t.pty.mark();
    t.key("ctrl+g");
    t.wait_editor("from the stub editor");
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        "draft text",
        "the editor got the draft"
    );
    let raw = String::from_utf8_lossy(&t.pty.raw_since(mark)).into_owned();
    assert!(
        raw.contains("\x1b[?1049l") && raw.contains("\x1b[?1049h"),
        "the terminal is handed over and taken back"
    );
    // ctrl+c clears a non-empty editor and arms; a second ctrl+c on the empty editor exits.
    t.key("ctrl+c");
    t.wait_editor("");
    t.settle();
    assert!(t.pty.exit_status().is_none(), "clearing does not exit");
    // ctrl+z suspends: the terminal is restored before the stop signal and set up again after
    // it. (The harness has no job-control shell, so the kernel discards SIGTSTP for the
    // orphaned process group and oxa resumes at once.)
    let mark = t.pty.mark();
    t.key("ctrl+z");
    t.pty.wait_for_raw(mark, b"\x1b[?1049h").unwrap();
    let raw = String::from_utf8_lossy(&t.pty.raw_since(mark)).into_owned();
    let left = raw
        .find("\x1b[?1049l")
        .expect("ctrl+z leaves the alternate screen");
    assert!(raw[left..].contains("\x1b[?1049h"), "and comes back");
    t.type_text("still alive");
    t.key("ctrl+c");
    t.wait_editor("");
    t.key("ctrl+c");
    t.wait_editor("");
    t.key("ctrl+c");
    assert!(t.exited_ok(), "the second ctrl+c on an empty editor exits");
    drop(t);
    // ctrl+d on the empty editor exits.
    let mut t = Setup::new().start();
    t.exit();
    drop(t);
    // Rebound: interrupt = f9; escape no longer aborts.
    let t = Setup::new()
        .keys(json!({ "app.interrupt": "f9" }))
        .script(&[held("late answer", 3000), text("after f9")])
        .start();
    t.type_text("slow one");
    t.key("enter");
    t.wait_chats(1);
    t.key("escape");
    t.wait_text("late answer");
    t.wait_worker_reply_end("key010-after-reply.html");
    t.type_text("slow two");
    t.key("enter");
    t.wait_text("after f9");
    drop(t);
    let t = Setup::new()
        .keys(json!({ "app.interrupt": "f9" }))
        .script(&[held("never shown", 8000)])
        .start();
    t.type_text("slow");
    t.key("enter");
    t.wait_chats(1);
    t.key("f9");
    t.wait("the abort is shown", |s| s.contains("aborted"));
    assert!(!t.screen().contains("never shown"));
}

/// KEY-011: `app.clipboard.pasteImage` (ctrl+v).
#[test]
fn key_011() {
    // invariant: pasting attaches the file or image to the session; the wire gets the
    // placeholder, never the image (see SET-046). The clipboard is a stub `wl-paste` on a
    // Wayland session (WAYLAND_DISPLAY set, the stubs first on PATH).
    let octal: String = PNG_1X1.iter().map(|b| format!("\\{b:03o}")).collect();
    let paste = format!("#!/bin/sh\nprintf '{octal}'\n");
    let path = format!("{{root}}/bin:{}", std::env::var("PATH").unwrap_or_default());
    let t = Setup::new()
        .exec("bin/wl-paste", &paste)
        .exec("bin/wl-copy", "#!/bin/sh\ncat >/dev/null\n")
        .env("WAYLAND_DISPLAY", "wayland-test")
        .env("PATH", &path)
        .replies("seen it", 1)
        .start();
    t.key("ctrl+v");
    t.wait_text("1 image(s) attached to the next prompt");
    t.prompt("what is this", "seen it");
    t.wait_gone("image(s) attached");
    let wire = String::from_utf8_lossy(&t.chats()[0].body).into_owned();
    assert!(
        wire.contains("not visible to this model"),
        "the placeholder is on the wire: {wire}"
    );
    assert!(
        !wire.contains("image_url") && !wire.contains("base64"),
        "no image data on the wire"
    );
    drop(t);
    // An empty clipboard: a notice, nothing attached.
    let t = Setup::new()
        .exec("bin/wl-paste", "#!/bin/sh\nexit 1\n")
        .exec("bin/wl-copy", "#!/bin/sh\ncat >/dev/null\n")
        .env("WAYLAND_DISPLAY", "wayland-test")
        .env("PATH", &path)
        .start();
    t.key("ctrl+v");
    t.wait_text("The clipboard holds no image.");
    drop(t);
    // Rebound: pasteImage = f3; ctrl+v does nothing then.
    let t = Setup::new()
        .keys(json!({ "app.clipboard.pasteImage": "f3" }))
        .exec("bin/wl-paste", &paste)
        .exec("bin/wl-copy", "#!/bin/sh\ncat >/dev/null\n")
        .env("WAYLAND_DISPLAY", "wayland-test")
        .env("PATH", &path)
        .start();
    t.key("ctrl+v");
    let s = t.settle();
    assert!(!s.contains("attached"), "{s:?}");
    t.key("f3");
    t.wait_text("1 image(s) attached to the next prompt");
}

/// KEY-012: session keys: new, tree, fork, resume, togglePath, toggleSort, toggleNamedFilter,
/// rename, delete, deleteNoninvasive.
#[test]
fn key_012() {
    // pi docs: keybindings.md "Sessions": app.session.new / tree / fork / resume (no default),
    // and in the session picker togglePath (ctrl+p), toggleSort (ctrl+s), toggleNamedFilter
    // (ctrl+n), rename (ctrl+r), delete (ctrl+d), deleteNoninvasive (ctrl+backspace: kitty only:
    // a legacy terminal sends it as backspace).
    let t = Setup::new()
        .profile(TermProfile::kitty())
        .pre_run(&["-p", "alpha session"])
        .pre_run(&["-p", "beta session"])
        .replies("ok", 3)
        .start();
    let open = |t: &Tui| {
        t.slash("/resume");
        t.wait("the session picker", |s| {
            s.contains("Resume Session") && s.contains("alpha session")
        });
    };
    let header = |s: &Screen| {
        s.rows()
            .into_iter()
            .find(|r| r.contains("Sort:"))
            .unwrap_or_default()
    };
    open(&t);
    let before = t.screen().text();
    t.key("ctrl+p");
    t.wait("paths shown", |s| s.text() != before);
    t.key("ctrl+p");
    t.wait("paths hidden", |s| s.text() == before);
    let sort0 = header(&t.screen());
    t.key("ctrl+s");
    t.wait("another sort", |s| {
        header(s) != sort0 && header(s).contains("Sort:")
    });
    t.keys(&["ctrl+s", "ctrl+s"]);
    t.wait("the sort cycled back", |s| header(s) == sort0);
    // ctrl+r renames the selected session; ctrl+n then shows only named sessions.
    let selected = |s: &Screen| {
        s.rows()
            .into_iter()
            .find(|r| r.trim_start().starts_with('›'))
            .unwrap_or_default()
    };
    while !selected(&t.screen()).contains("alpha session") {
        let b = selected(&t.screen());
        t.key("down");
        t.wait("the selection moved", |s| selected(s) != b);
    }
    t.key("ctrl+r");
    t.wait_text("Rename Session");
    t.pty.send_text("named-alpha");
    t.key("enter");
    t.wait("renamed", |s| s.contains("named-alpha"));
    t.key("ctrl+n");
    t.wait("named only", |s| {
        s.contains("named-alpha") && !s.contains("beta session")
    });
    t.key("ctrl+n");
    t.wait("all again", |s| s.contains("beta session"));
    // ctrl+d deletes after a confirmation; the file is gone.
    let files_before = session_files(&t.env).len();
    while !selected(&t.screen()).contains("beta session") {
        let b = selected(&t.screen());
        t.key("down");
        t.wait("the selection moved", |s| selected(s) != b);
    }
    t.key("ctrl+d");
    t.wait_text("Delete session?");
    t.key("enter");
    t.wait_gone("beta session");
    assert_eq!(
        session_files(&t.env).len(),
        files_before - 1,
        "the session file was deleted"
    );
    // ctrl+backspace with a query deletes a word of the query (non-invasive); with an empty
    // query it asks to delete the selected session.
    t.pty.send_text("zzz");
    t.wait("query typed", |s| s.contains("> zzz"));
    t.key("ctrl+backspace");
    t.wait("query word deleted", |s| s.contains("> ▏"));
    t.key("ctrl+backspace");
    t.wait_text("Delete session?");
    t.key("escape");
    t.wait_gone("Delete session?");
    t.key("escape");
    t.wait_gone("Resume Session");
    drop(t);
    // The four session actions without a default key, bound here.
    let t = Setup::new()
        .keys(json!({
            "app.session.new": "f2",
            "app.session.tree": "f3",
            "app.session.fork": "f4",
            "app.session.resume": "f5",
        }))
        .pre_run(&["-p", "older session"])
        .replies("ok answer", 2)
        .start();
    t.prompt("current question", "ok answer");
    t.key("f3");
    t.wait_text("Session Tree");
    t.key("escape");
    t.wait_gone("Session Tree");
    t.key("f4");
    t.wait_text("Fork from Message");
    t.key("escape");
    t.wait_gone("Fork from Message");
    t.key("f5");
    t.wait("the picker", |s| {
        s.contains("Resume Session") && s.contains("older session")
    });
    t.key("escape");
    t.wait_gone("Resume Session");
    t.key("f2");
    t.wait_gone("current question");
    t.prompt("fresh question", "ok answer");
    let last = t.wait_chats(2).pop().unwrap();
    assert!(
        !String::from_utf8_lossy(&last.body).contains("current question"),
        "a new session starts without the old messages"
    );
}

/// KEY-013: model keys: select, cycleForward, cycleBackward, save.
#[test]
fn key_013() {
    // invariant: over engine models (the endpoint's /v1/models); a switch is a new prefix
    // epoch (W3): the next request names the new model. Keys: app.model.select (ctrl+l),
    // cycleForward (ctrl+p), cycleBackward (shift+ctrl+p, kitty only), app.models.save (ctrl+s
    // in the selector: select and save as defaultModel).
    let footer = |s: &Screen| footer_row(s);
    let t = Setup::new()
        .profile(TermProfile::kitty())
        .fake(three_models())
        .script(&[
            text("model answer"),
            // The visible answer is not an idle-session witness: hold the stream open after
            // its content delta, longer than the harness's 250 ms screen-quiet interval.
            json!({"text": "model answer", "fault": {"kind": "stall", "bytes": 350, "ms": 1500}}),
            text("model answer"),
        ])
        .start();
    assert!(footer(&t.screen()).contains("model-a"), "{:?}", t.screen());
    t.key("ctrl+l");
    t.wait("the model selector", |s| {
        s.contains("model-b") && s.contains("model-c")
    });
    t.key("down");
    t.key("enter");
    t.wait("model-b active", |s| footer(s).contains("model-b"));
    t.prompt("one", "model answer");
    assert_eq!(
        t.wait_chats(1)[0].model,
        "model-b",
        "the next request uses the chosen model"
    );
    t.key("ctrl+p");
    t.wait("model-c active", |s| footer(s).contains("model-c"));
    t.key("shift+ctrl+p");
    t.wait("model-b again", |s| footer(s).contains("model-b"));
    t.key("ctrl+p");
    t.key("ctrl+p");
    t.wait("cycle wraps to model-a", |s| footer(s).contains("model-a"));
    t.type_text("two");
    t.key("enter");
    assert_eq!(t.wait_chats(2)[1].model, "model-a");
    // The selector needs an idle session: answer text and even a quiet screen can precede
    // the stream's end. Wait for the busy indicator to clear, just as Tui::prompt does.
    t.wait("the second reply and idle session", |s| {
        let rows = s.rows();
        !s.contains("Working")
            && rows
                .iter()
                .position(|r| r.trim() == "two")
                .is_some_and(|i| rows[i..].iter().any(|r| r.contains("model answer")))
    });
    t.settle();
    // ctrl+s in the selector: select and save as the default model.
    t.key("ctrl+l");
    t.wait("the model selector", |s| s.contains("model-c"));
    t.keys(&["down", "down"]);
    t.key("ctrl+s");
    t.wait("model-c active", |s| footer(s).contains("model-c"));
    t.wait_text("Saved defaultModel");
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.env.agent_dir().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["defaultModel"], "model-c");
    drop(t);
    // Rebound: select = f2; ctrl+l no longer opens it.
    let t = Setup::new()
        .keys(json!({ "app.model.select": "f2" }))
        .fake(three_models())
        .start();
    t.key("ctrl+l");
    let s = t.settle();
    assert!(!s.contains("model-b"), "{s:?}");
    t.key("f2");
    t.wait("the model selector", |s| s.contains("model-b"));
}

/// KEY-014: thinking keys: cycle, save, toggle.
#[test]
fn key_014() {
    // invariant: W6: the level is a boolean on the wire (`enable_thinking`); shift+tab stays the
    // thinking cycle (app.thinking.cycle), ctrl+t collapses or expands thinking blocks
    // (app.thinking.toggle), ctrl+s in the thinking selector saves the level as the default
    // (app.thinking.save; pi thinking-selector.ts `onSelectAsDefault`).
    let footer = |s: &Screen| footer_row(s);
    let t = Setup::new()
        .settings(json!({ "defaultThinkingLevel": "off" }))
        .script(&[
            json!({ "reasoning": "secret musing", "text": "visible answer" }),
            text("second"),
        ])
        .start();
    assert!(footer(&t.screen()).ends_with("• off"), "{:?}", t.screen());
    t.key("shift+tab");
    t.wait("thinking minimal", |s| footer(s).ends_with("• minimal"));
    t.prompt("think", "visible answer");
    assert!(
        t.chats()[0].thinking,
        "a level above off is enable_thinking: true on the wire"
    );
    t.wait_text("secret musing");
    // ctrl+t collapses the thinking block, again expands it.
    t.key("ctrl+t");
    t.wait_gone("secret musing");
    t.key("ctrl+t");
    t.wait_text("secret musing");
    // The cycle wraps around: minimal -> low -> medium -> high -> xhigh -> max -> off.
    for level in ["low", "medium", "high", "xhigh", "max", "off"] {
        t.key("shift+tab");
        t.wait(&format!("thinking {level}"), |s| {
            footer(s).ends_with(&format!("• {level}"))
        });
    }
    t.prompt("plain", "second");
    assert!(!t.wait_chats(2)[1].thinking, "off is enable_thinking: false");
    // ctrl+s in the thinking selector selects the highlighted level and saves it.
    t.slash("/thinking");
    t.wait_text("Thinking level");
    t.key("down");
    t.key("ctrl+s");
    t.wait("thinking minimal again", |s| footer(s).ends_with("• minimal"));
    t.wait_text("Saved defaultThinkingLevel");
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.env.agent_dir().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["defaultThinkingLevel"], "minimal");
    drop(t);
    // Rebound: cycle = f4; shift+tab no longer cycles.
    let t = Setup::new()
        .settings(json!({ "defaultThinkingLevel": "off" }))
        .keys(json!({ "app.thinking.cycle": "f4" }))
        .start();
    t.key("shift+tab");
    let s = t.settle();
    assert!(footer(&s).ends_with("• off"), "{s:?}");
    t.key("f4");
    t.wait("f4 cycles", |s| footer(s).ends_with("• minimal"));
}

/// KEY-015: `app.mode.cycle` (alt+m): standard -> auto -> yolo.
#[test]
fn key_015() {
    // invariant: oxa only; the cycle is auto -> yolo -> standard (the default is alt+m).
    // The mode is display and permission state, never wire text (W3). Under yolo the badge
    // row `YOLO full access` sits below the footer, so look at the bottom two rows.
    let footer = |s: &Screen| {
        let n = s.size().0;
        format!("{}\n{}", s.row(n - 2), s.row(n - 1))
    };
    let is_mode = |s: &Screen, mode: &str| footer(s).split_whitespace().any(|word| word == mode);
    // Also keep the auto-compaction marker visible at a wider size: it must not be
    // mistaken for auto mode. Retain the normal 80-column layout (including its YOLO row).
    for cols in [80, 120] {
        let t = Setup::new().size(24, cols).replies("ok", 2).start();
        assert!(is_mode(&t.screen(), "auto"));
        t.prompt("one", "ok");
        // Each transition changes the footer's height under YOLO. Observe a complete, quiet
        // frame before sending another key, not a matching substring during the repaint.
        t.key("alt+m");
        t.wait_steady("yolo mode", |s| is_mode(s, "yolo"));
        t.key("alt+m");
        let standard = t.wait_steady("standard mode", |s| is_mode(s, "standard"));
        assert!(
            !is_mode(&standard, "auto"),
            "auto-compaction is not auto permission mode: {standard:?}"
        );
        t.key("alt+m");
        t.wait_steady("auto mode", |s| is_mode(s, "auto"));
        t.type_text("two");
        t.key("enter");
        let chats = t.wait_chats(2);
        assert_eq!(
            chats[0].system, chats[1].system,
            "no mode text in the frozen prefix"
        );
        drop(t);
    }
    // Rebound: f3 cycles; alt+m does not.
    let t = Setup::new().keys(json!({ "app.mode.cycle": "f3" })).start();
    t.key("alt+m");
    let s = t.settle();
    assert!(is_mode(&s, "auto"), "{s:?}");
    t.key("f3");
    t.wait_steady("yolo mode", |s| is_mode(s, "yolo"));
}

/// KEY-016: display and queue: tools expand, message copy, follow-up, dequeue.
#[test]
fn key_016() {
    // pi docs: keybindings.md "Display and message queue": app.tools.expand (ctrl+o),
    // app.message.copy (ctrl+x), app.message.followUp (alt+enter), app.message.dequeue (alt+up).
    let row_is = |s: &Screen, want: &str| row_index(s, want).is_some();
    // ctrl+o: a long tool output is collapsed, ctrl+o shows all of it, again collapses it.
    let t = Setup::new()
        .settings(json!({ "defaultProjectTrust": "always" }))
        .script(&[bash_call("seq 101 130"), text("listed")])
        .size(60, 80)
        .start();
    t.prompt("list", "listed");
    let s = t.wait("the collapsed tool output", |s| {
        row_is(s, "101") || s.contains("seq 101 130")
    });
    assert!(!row_is(&s, "121"), "collapsed: the tail is hidden\n{s:?}");
    t.key("ctrl+o");
    t.wait("the expanded tool output", |s| {
        row_is(s, "121") && row_is(s, "130")
    });
    t.key("ctrl+o");
    t.wait("collapsed again", |s| !row_is(s, "121"));
    drop(t);
    // ctrl+x copies the last assistant message (OSC 52 on a terminal without a clipboard tool).
    let t = Setup::new().replies("copy this reply", 1).start();
    t.key("ctrl+x");
    t.wait_text("No assistant message to copy yet.");
    t.prompt("say it", "copy this reply");
    let mark = t.pty.mark();
    t.key("ctrl+x");
    t.wait_text("Copied to the clipboard");
    let raw = String::from_utf8_lossy(&t.pty.raw_since(mark)).into_owned();
    assert!(
        raw.contains("\x1b]52;c;Y29weSB0aGlzIHJlcGx5"),
        "OSC 52 with the reply: {raw:?}"
    );
    drop(t);
    // alt+enter queues a follow-up while a reply runs; it is sent after the reply.
    let t = Setup::new()
        .script(&[held("first answer", 2500), text("follow answer")])
        .start();
    t.type_text("one");
    t.key("enter");
    t.wait_chats(1);
    t.type_text("later");
    t.key("alt+enter");
    t.wait("the queued follow-up", |s| {
        s.contains("later") && editor_text(s).is_empty()
    });
    t.wait_text("follow answer");
    assert_eq!(t.prompts(), vec!["one".to_string(), "later".to_string()]);
    // Idle, alt+enter adds a newline (issue #344); enter then sends both lines.
    drop(t);
    let t = Setup::new().replies("idle answer", 1).start();
    t.type_text("now");
    t.key("alt+enter");
    // Witness: the newline was applied (the cursor moved to a second line) before typing on.
    t.wait("a newline in the editor", |s| {
        editor_rows(s).is_some_and(|r| r.len() >= 2)
    });
    t.type_text("more");
    t.wait("both lines in the editor", |s| editor_text(s) == "now\nmore");
    t.key("enter");
    t.wait_text("idle answer");
    t.wait_chats(1);
    assert_eq!(t.prompts(), vec!["now\nmore".to_string()]);
    drop(t);
    // alt+up brings the queued messages back into the editor; they are not sent.
    let t = Setup::new()
        .fake(tui::FakeOpts {
            done_gap_ms: 2500,
            ..tui::FakeOpts::default()
        })
        .script(&[held("answer A", 2500), text("unused")])
        .start();
    t.type_text("one");
    t.key("enter");
    t.wait_chats(1);
    t.type_text("steer me");
    t.key("enter");
    t.wait("the queued steering", |s| {
        s.contains("steer me") && editor_text(s).is_empty()
    });
    t.key("alt+up");
    t.wait_editor("steer me");
    t.wait_text("answer A");
    // The answer is visible before the turn ends. Ctrl+C would abort an active turn,
    // so clear the single restored line with an editor key and wait for the worker's
    // read-only acknowledgement before submitting the next prompt.
    t.key("ctrl+u");
    t.wait_editor("");
    t.wait_worker_reply_end("key016-after-dequeue.html");
    assert_eq!(
        t.prompts(),
        vec!["one".to_string()],
        "the dequeued message was not sent"
    );
    // Witness: the next prompt is the second request.
    t.prompt("two", "unused");
    assert_eq!(t.prompts(), vec!["one".to_string(), "two".to_string()]);
    drop(t);
    // Rebound: tools.expand = f6; ctrl+o no longer expands.
    let t = Setup::new()
        .settings(json!({ "defaultProjectTrust": "always" }))
        .keys(json!({ "app.tools.expand": "f6" }))
        .script(&[bash_call("seq 101 130"), text("listed")])
        .size(60, 80)
        .start();
    t.prompt("list", "listed");
    t.key("ctrl+o");
    let s = t.settle();
    assert!(!row_is(&s, "121"), "ctrl+o is unbound\n{s:?}");
    t.key("f6");
    t.wait("expanded by f6", |s| row_is(s, "121"));
}

/// KEY-017: tree navigation.
#[test]
fn key_017() {
    // pi docs: keybindings.md "Tree navigation": app.tree.foldOrUp (ctrl+left, alt+left),
    // unfoldOrDown (ctrl+right, alt+right), editLabel (shift+l), toggleLabelTimestamp
    // (shift+t). A branch point makes the second question a foldable segment start.
    let t = Setup::new()
        .settings(json!({ "branchSummary": { "skipPrompt": true } }))
        .script(&[text("first answer"), text("second answer"), text("branch answer")])
        .start();
    t.prompt("first question", "first answer");
    t.prompt("second question", "second answer");
    // Branch: go back to the first answer and ask something else.
    open_tree(&t);
    tree_select(&t, "first answer");
    t.key("enter");
    t.wait_gone("Session Tree");
    t.prompt("branch question", "branch answer");
    open_tree(&t);
    tree_select(&t, "second question");
    for (fold, unfold) in [("ctrl+left", "ctrl+right"), ("alt+left", "alt+right")] {
        t.key(fold);
        t.wait("folded", |s| !s.contains("second answer"));
        t.key(unfold);
        t.wait("unfolded", |s| s.contains("second answer"));
    }
    // shift+l edits the label of the selected entry; shift+t shows label times.
    t.key("shift+l");
    t.wait_text("Label (empty to remove)");
    t.pty.send_text("mark");
    t.key("enter");
    t.wait("the label on the row", |s| tree_selected(s).contains("[mark]"));
    t.key("shift+t");
    t.wait("label times shown", |s| {
        tree_position(s).contains("[+label time]")
    });
    t.key("shift+t");
    t.wait("label times hidden", |s| {
        !tree_position(s).contains("[+label time]")
    });
    drop(t);
    // Rebound: editLabel = f2; shift+l is then text for the search.
    let t = Setup::new()
        .keys(json!({ "app.tree.editLabel": "f2" }))
        .replies("an answer", 1)
        .start();
    t.prompt("first question", "an answer");
    open_tree(&t);
    t.key("shift+l");
    let s = t.settle();
    assert!(!s.contains("Label (empty to remove)"), "{s:?}");
    t.key("backspace");
    t.key("f2");
    t.wait_text("Label (empty to remove)");
}

/// KEY-018: tree filters.
#[test]
fn key_018() {
    // pi docs: keybindings.md "Tree navigation": app.tree.filter.default (ctrl+d), noTools
    // (ctrl+t), userOnly (ctrl+u), labeledOnly (ctrl+l), all (ctrl+a), cycleForward (ctrl+o),
    // cycleBackward (shift+ctrl+o, kitty only). The position line names the filter.
    let t = Setup::new()
        .profile(TermProfile::kitty())
        .replies("an answer", 2)
        .start();
    t.prompt("first question", "an answer");
    t.prompt("second question", "an answer");
    open_tree(&t);
    let pos = |s: &Screen| tree_position(s);
    t.key("ctrl+u");
    t.wait("user only", |s| pos(s).ends_with("/2) [user]"));
    t.key("ctrl+t");
    t.wait("no tools", |s| pos(s).ends_with("[no-tools]"));
    t.key("ctrl+l");
    t.wait("labeled only", |s| pos(s).ends_with("(0/0) [labeled]"));
    t.key("ctrl+a");
    t.wait("all", |s| pos(s).ends_with("[all]"));
    t.key("ctrl+d");
    t.wait("default", |s| !pos(s).contains('[') && pos(s).ends_with("/4)"));
    t.key("ctrl+o");
    t.wait("cycle forward: no-tools", |s| pos(s).ends_with("[no-tools]"));
    t.key("ctrl+o");
    t.wait("cycle forward: user", |s| pos(s).ends_with("[user]"));
    t.key("shift+ctrl+o");
    t.wait("cycle backward: no-tools", |s| pos(s).ends_with("[no-tools]"));
    t.key("shift+ctrl+o");
    t.wait("cycle backward: default", |s| !pos(s).contains('['));
    drop(t);
    // Rebound: userOnly = f2; ctrl+u no longer filters.
    let t = Setup::new()
        .keys(json!({ "app.tree.filter.userOnly": "f2" }))
        .replies("an answer", 1)
        .start();
    t.prompt("first question", "an answer");
    open_tree(&t);
    t.key("ctrl+u");
    let s = t.settle();
    assert!(!tree_position(&s).contains("[user]"), "{s:?}");
    t.key("f2");
    t.wait("user only", |s| tree_position(s).ends_with("[user]"));
}

/// KEY-019: scoped models selector.
#[test]
fn key_019() {
    // pi docs: keybindings.md "Scoped models selector": app.models.enableAll (ctrl+a),
    // clearAll (ctrl+x), reorderUp (alt+up), reorderDown (alt+down); app.models.save (ctrl+s)
    // writes the configuration to settings (`enabledModels`), which then scopes the cycle keys.
    let footer = |s: &Screen| footer_row(s);
    let order = |s: &Screen| -> Vec<String> {
        s.rows()
            .iter()
            .filter_map(|r| r.find("model-").map(|i| r[i..i + 7].to_string()))
            .filter(|_| true)
            .collect()
    };
    let t = Setup::new().fake(three_models()).start();
    t.slash("/scoped-models");
    t.wait_text("0/3 enabled");
    t.key("ctrl+a");
    t.wait_text("3/3 enabled");
    t.key("ctrl+x");
    t.wait_text("0/3 enabled");
    // enter toggles the row under the cursor: model-b and model-c on.
    t.keys(&["down", "enter", "down", "enter"]);
    t.wait_text("2/3 enabled");
    // alt+up moves model-c above model-b, alt+down moves it back below, alt+up again.
    t.key("alt+up");
    t.wait("model-c above model-b", |s| {
        let o = order(s);
        o.iter().position(|m| m == "model-c") < o.iter().position(|m| m == "model-b")
    });
    t.key("alt+down");
    t.wait("model-c below model-b", |s| {
        let o = order(s);
        o.iter().position(|m| m == "model-c") > o.iter().position(|m| m == "model-b")
    });
    t.key("alt+up");
    t.wait("model-c above model-b again", |s| {
        let o = order(s);
        o.iter().position(|m| m == "model-c") < o.iter().position(|m| m == "model-b")
    });
    t.key("ctrl+s");
    t.wait_text("Saved enabledModels");
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(t.env.agent_dir().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["enabledModels"], json!(["model-c", "model-b"]));
    // The cycle keys now go through the scoped list only (model-a is not in it).
    t.key("ctrl+p");
    t.wait("model-c", |s| footer(s).contains("model-c"));
    t.key("ctrl+p");
    t.wait("model-b", |s| footer(s).contains("model-b"));
    t.key("ctrl+p");
    t.wait("model-c again", |s| footer(s).contains("model-c"));
    drop(t);
    // Rebound: enableAll = f2; ctrl+a no longer enables all.
    let t = Setup::new()
        .keys(json!({ "app.models.enableAll": "f2" }))
        .fake(three_models())
        .start();
    t.slash("/scoped-models");
    t.wait_text("0/3 enabled");
    t.key("ctrl+a");
    let s = t.settle();
    assert!(s.contains("0/3 enabled"), "{s:?}");
    t.key("f2");
    t.wait_text("3/3 enabled");
}

/// KEY-020: `app.models.toggleProvider` (ctrl+p) toggles all models of one engine endpoint.
#[test]
fn key_020() {
    // invariant: toggles all models of one engine endpoint (one endpoint here: the local
    // engine of models.json, so all three models switch together).
    let t = Setup::new().fake(three_models()).start();
    t.slash("/scoped-models");
    t.wait_text("0/3 enabled");
    t.key("ctrl+p");
    t.wait_text("3/3 enabled");
    t.key("enter"); // one off: the group is mixed, ctrl+p switches it all on again
    t.wait_text("2/3 enabled");
    t.key("ctrl+p");
    t.wait_text("3/3 enabled");
    t.key("ctrl+p");
    t.wait_text("0/3 enabled");
    drop(t);
    // Rebound: toggleProvider = f2 (ctrl+p does nothing in the selector then).
    let t = Setup::new()
        .keys(json!({ "app.models.toggleProvider": "f2" }))
        .fake(three_models())
        .start();
    t.slash("/scoped-models");
    t.wait_text("0/3 enabled");
    t.key("ctrl+p");
    let s = t.settle();
    assert!(s.contains("0/3 enabled"), "{s:?}");
    t.key("f2");
    t.wait_text("3/3 enabled");
}
