//! V020-OPTIMIZE #1 (OUT-DIET), #6 (PREFIX-BUDGET), #10 (the subagent rule) and OXA-ANCHOR-MOE:
//! the frozen prefix of a default session with the output diet on and off, with and without a
//! `subagent` tool. Prints the byte table (run with `-- --nocapture`); checks the diet cost and
//! the anchor.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, CommandSpec, Engine, Exposure, FrozenPrefix, HookPoint, HookReply, Hooks, Host, OxaError, Source,
    Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value,
};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The rule texts of V020-OPTIMIZE #1 and #10, pinned here word for word.
const DIET_RULES: [&str; 5] = [
    "Call tools directly: write no text before a tool call unless the user must know something first",
    "Put independent tool calls in one reply, not one per reply",
    "Copy oldText from your latest read of the file and use the shortest block that is unique",
    "After a successful edit or write, do not read the file again to check it",
    "Final answer: at most 6 lines unless asked for more; never paste file contents back",
];
const SUBAGENT_RULE: &str =
    "For a wide search or to read many files, delegate to a subagent and use its summary";
const SUBAGENT_CHECK_RULE: &str =
    "A subagent's summary can be wrong: before you rely on a claim from it for an edit, read the cited lines yourself";
/// 2026-11-19: the done check, added with the output diet when `bash` is declared.
const VERIFY_RULE: &str =
    "Before you call a task done, run the project's own tests or checks and report the result; if you ran none, say so";
const SHOW_PATHS: &str = "Show file paths clearly when working with files";
const CONCISE: &str = "Be concise in your responses";

/// A stand-in for the `subagent` tool: declared (Direct) so the prompt lists it.
struct SubagentTool(ToolSpec);

impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("delegated")
    }
}

struct SubagentDecl;

impl Hooks for SubagentDecl {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SubagentTool(ToolSpec {
            name: "subagent".to_string(),
            description: "Delegate tasks to specialized subagents with isolated context.".to_string(),
            schema_json: r#"{"type":"object","properties":{"task":{"type":"string","description":"Task to delegate"}}}"#
                .to_string(),
            class: ToolClass::Exec,
            exposure: Exposure::Direct,
            source: Source::Builtin,
        }))]
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
    fn attach(&self, _host: Arc<dyn Host>) {}
}

/// A default session in `env` working in `cwd` under `settings`, with the built-ins and
/// (optionally) `subagent`. The agent directory is `env`'s, whatever `cwd` is.
fn open(env: &TestEnv, cwd: &std::path::Path, settings: &str, subagent: bool) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, settings).unwrap();
    std::fs::create_dir_all(cwd).unwrap();
    let settings = Settings::load(&dirs, cwd, true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    let hooks: Vec<Arc<dyn Hooks>> = if subagent {
        vec![Arc::new(SubagentDecl)]
    } else {
        Vec::new()
    };
    let engine: Arc<dyn Engine> = Arc::new(ScriptedEngine::new(Vec::new()));
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks,
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: cwd.to_path_buf(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The prefix without the working directory, and with the test's random root replaced by a
/// fixed word (built-in skill paths live under the agent directory, which the test creates).
fn stable_sha(p: &FrozenPrefix, root: &std::path::Path) -> String {
    let root = root.to_string_lossy().into_owned();
    let mut h = Sha256::new();
    for (name, text) in p.sections.iter().filter(|(n, _)| n != "cwd") {
        h.update(name.as_bytes());
        h.update([0x1f]);
        h.update(text.replace(&root, "<root>").as_bytes());
        h.update([0x1e]);
    }
    h.update([0]);
    h.update(p.tools_json.as_bytes());
    hex(&h.finalize())
}

/// Pinned: SHA-256 over every section but `cwd`, then `\0` and the tools JSON, of the default
/// session (the seven built-ins) and the same with a `subagent` tool. These are the default
/// bytes: a prefix change is a PARITY event.
/// 2026-11-14: the identity fallback text changed with the env note PR (#5).
/// 2026-11-15: issue #127 (b) added `background`/`job` to the `bash` schema and dropped its
/// `required: ["command"]` (a `job` read has no command), so the tools JSON changed.
/// 2026-11-15: the built-in `pull-request` skill (#181) joined the skills section.
/// 2026-11-16: issue #519 added `kill` to the `bash` schema and documented per-job cancellation
/// and explicit background timeouts; both default tool prefixes intentionally changed.
/// 2026-11-19: S12 documents explicit zero in the bash timeout schema.
/// 2026-11-19: R2 rewrote the built-in `ox-llm` skill description (no controller), and the
/// new identity text (Rust-only suite, cloud models as one provider line) and the done-check
/// rule; hashes copied from the `stable sha256` lines of this test in CI.
/// 2026-11-19: N1 changed the bash tool text (wait cap 300, timeout moves to a background job).
/// 2026-11-19: the `report-issue` skill text stopped describing a controller CLI digest, and
/// the draft now names only the session's model with in-process facts (#1553).
const DEFAULT_STABLE_SHA: &str = "1cd45063add45a653c65bbcf8cd197e7e8dd3d00bab34f5c6ce4758a577afa9c";
const DEFAULT_STABLE_SHA_SUBAGENT: &str = "039c756ef6ffab30c82adea27ef1330e295f140c44bf91a54209442bfa978663";

/// A calendar date (`YYYY-MM-DD`) or an 8-digit run anywhere in `s`.
fn has_date(s: &str) -> bool {
    let b = s.as_bytes();
    let dashed = b.windows(10).any(|w| {
        w.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
    });
    let run = b.windows(8).any(|w| w.iter().all(|c| c.is_ascii_digit()));
    dashed || run
}

struct Case {
    label: &'static str,
    settings: &'static str,
}

const CASES: [Case; 2] = [
    Case {
        label: "diet on",
        settings: r#"{"oxa": {}}"#,
    },
    Case {
        label: "diet off",
        settings: r#"{"oxa": {"diet": {"output": false}}}"#,
    },
];

#[test]
fn prefix_bytes_and_the_anchor() {
    let env = TestEnv::new();
    let cwd = env.work();
    // Every row first, then the checks: a failing check still leaves the whole table printed.
    let mut rows: Vec<(bool, &str, FrozenPrefix)> = Vec::new();
    for subagent in [false, true] {
        for case in &CASES {
            rows.push((
                subagent,
                case.label,
                open(&env, &cwd, case.settings, subagent).inner().prefix.clone(),
            ));
        }
    }
    let mut totals: Vec<(bool, &str, usize)> = Vec::new();
    let mut default_sha = [String::new(), String::new()];
    for subagent in [false, true] {
        let title = if subagent {
            "with a subagent tool declared"
        } else {
            "default session (seven built-ins)"
        };
        eprintln!("== {title}: frozen prefix bytes (system = all sections joined, tools = tools_json)");
        eprintln!("{:<12} {:>7} {:>7} {:>7}", "arm", "system", "tools", "total");
        for (_, label, p) in rows.iter().filter(|r| r.0 == subagent) {
            let system = p.system_text();
            let total = system.len() + p.tools_json.len();
            eprintln!(
                "{:<12} {:>7} {:>7} {:>7}",
                label,
                system.len(),
                p.tools_json.len(),
                total
            );
            for (name, text) in &p.sections {
                eprintln!("    {name:<14} {:>6}", text.len());
            }
            if *label == "diet on" {
                default_sha[usize::from(subagent)] = stable_sha(p, env.root());
                eprintln!("    stable sha256 {}", default_sha[usize::from(subagent)]);
            }
            totals.push((subagent, label, total));
        }
    }

    for (subagent, label, p) in &rows {
        let system = p.system_text();
        let on = *label == "diet on";
        assert!(!system.contains("<tools>"), "{label}: no <tools> section");
        assert!(system.starts_with("You are a coding assistant"));
        for r in DIET_RULES {
            assert_eq!(system.contains(r), on, "{label}: {r}");
        }
        assert_eq!(
            system.contains(VERIFY_RULE),
            on,
            "{label}: done check only with the diet on"
        );
        assert_eq!(system.contains(CONCISE), !on, "{label}: concise rule");
        assert!(system.contains(SHOW_PATHS), "{label}: paths rule");
        assert!(
            system.ends_with(&format!("<cwd>\n{}\n</cwd>", cwd.display())),
            "{label}: cwd last"
        );
        assert_eq!(
            system.contains(SUBAGENT_RULE),
            on && *subagent,
            "{label}: subagent rule only with diet on and subagent declared"
        );
        assert!(system.contains("<rules>\n"), "{label}: rules section kept");
        assert_eq!(
            p.tools_json.contains("\"name\":\"subagent\""),
            *subagent,
            "{label}: subagent declared"
        );
    }

    // The default prefix stays byte-identical to the pinned bytes.
    assert_eq!(
        default_sha[0], DEFAULT_STABLE_SHA,
        "the default prefix bytes drifted"
    );
    assert_eq!(
        default_sha[1], DEFAULT_STABLE_SHA_SUBAGENT,
        "the default prefix bytes with a subagent drifted"
    );

    // The diet costs prefix bytes (its rules replace one line): exactly the rule lines' bytes,
    // one "\n- " + text per rule line, plus the subagent rule when it is declared.
    for subagent in [false, true] {
        let get = |label: &str| {
            totals
                .iter()
                .find(|(s, l, _)| *s == subagent && *l == label)
                .map(|t| t.2)
                .unwrap()
        };
        let (on, off) = (get("diet on"), get("diet off"));
        let line = |t: &str| t.len() + 3;
        let mut cost: i64 = DIET_RULES.iter().map(|r| line(r) as i64).sum::<i64>() - line(CONCISE) as i64
            + line(VERIFY_RULE) as i64;
        if subagent {
            cost += line(SUBAGENT_RULE) as i64 + line(SUBAGENT_CHECK_RULE) as i64;
        }
        eprintln!(
            "the output diet adds {} bytes to the prefix (subagent={subagent})",
            on as i64 - off as i64
        );
        assert_eq!(
            on as i64 - off as i64,
            cost,
            "the diet's prefix cost is its rule lines"
        );
    }
}

#[test]
fn anchor_sections_are_the_same_in_every_directory_and_carry_no_date_or_host_or_user() {
    let env = TestEnv::new();
    let (a, b) = (env.root().join("project-a"), env.root().join("project-b"));
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let host = host.trim();
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let anchor = |p: &FrozenPrefix| -> Vec<(String, String)> {
        p.sections
            .iter()
            .filter(|(n, _)| FrozenPrefix::ANCHOR_SECTIONS.contains(&n.as_str()))
            .cloned()
            .collect()
    };
    for case in &CASES {
        for subagent in [false, true] {
            let pa = open(&env, &a, case.settings, subagent).inner().prefix.clone();
            let pb = open(&env, &b, case.settings, subagent).inner().prefix.clone();
            assert!(!anchor(&pa).is_empty(), "{}: anchor sections exist", case.label);
            assert_eq!(
                anchor(&pa),
                anchor(&pb),
                "{} (subagent={subagent}): anchor sections",
                case.label
            );
            assert_eq!(pa.anchor_text(), pb.anchor_text(), "{}: anchor text", case.label);
            let text = pa.anchor_text();
            assert!(
                pa.system_text().starts_with(&text),
                "the anchor is a prefix of the system text"
            );
            // The test root (random digits) is not part of the install; ignore it.
            let install = text.replace(env.root().to_string_lossy().as_ref(), "");
            assert!(!has_date(&install), "{}: no date in the anchor", case.label);
            assert!(
                !text.contains(a.to_string_lossy().as_ref()),
                "no directory in the anchor"
            );
            if host.len() >= 3 {
                assert!(!text.contains(host), "{}: no host name in the anchor", case.label);
            }
            if user.len() >= 3 {
                assert!(
                    !text.contains(&user),
                    "{}: no user name in the anchor",
                    case.label
                );
            }
        }
    }
}
