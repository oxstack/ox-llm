//! G11 PARITY tests (mcp).
//! One test per PARITY.md row of this area, named after the row id (`MCP-012` -> `fn mcp_012()`).
//! Each row test documents its verdict in its doc comment: `pi:` (P: compared with pi's
//! documented output in `packages/coding-agent/docs/mcp.md`), `invariant:` (A: the adapted
//! behaviour and the invariant that forces it), `unsupported_in_oxa:` (O). Rows that do not pass
//! yet are `#[ignore = "MCP-0xx RED: ..."]` and keep their assertions, so un-ignoring one shows the
//! failure; the H wave takes them from there.
//! Drivers: the real `oxa` binary and `oxa-fake` (`cargo build -p oxa-cli -p oxa-fake` first). The
//! MCP servers are the fixture `fixtures/goldens/mcp/server.py` (a stdio test double, run with python3).
use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::{fixture, run_oxa};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Engine address for runs that must not reach an engine (`mcp` shell commands).
const NO_ENGINE: &str = "http://127.0.0.1:9";

/// PARITY.md and this file, read at compile time by the meta-checks.
const PARITY_MD: &str = include_str!("../../PARITY.md");
const SELF_SRC: &str = include_str!("mcp.rs");

// ---------------------------------------------------------------- helpers

fn fake(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(script, FakeOpts::default()).expect("fake engine")
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).expect("write json");
}

fn user_file(env: &TestEnv) -> PathBuf {
    env.agent_dir().join("mcp.json")
}

fn project_file(env: &TestEnv) -> PathBuf {
    env.work().join(".oxa").join("mcp.json")
}

/// Mark the test's work directory as a trusted project (`trust.json`, keyed by canonical path).
fn trust_work(env: &TestEnv) {
    let key = std::fs::canonicalize(env.work()).expect("canonical work dir");
    let mut map = serde_json::Map::new();
    map.insert(key.to_string_lossy().into_owned(), Value::Bool(true));
    write_json(&env.agent_dir().join("trust.json"), &Value::Object(map));
}

/// Copy the fixture server into the test root; returns its path.
fn install_server(env: &TestEnv) -> String {
    let src = std::fs::read_to_string(fixture("goldens/mcp/server.py")).expect("fixture server");
    let dst = env.root().join("mcp/server.py");
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::write(&dst, src).expect("copy fixture server");
    dst.to_string_lossy().into_owned()
}

/// A directory the fixture server writes its observations into.
fn record_dir(env: &TestEnv, name: &str) -> PathBuf {
    let dir = env.root().join("rec").join(name);
    std::fs::create_dir_all(&dir).expect("record dir");
    dir
}

/// A stdio entry running the fixture server with `mode`, observations in `rec`.
fn stdio_entry(script: &str, rec: &Path, mode: &str) -> Value {
    json!({
        "command": "python3",
        "args": ["-I", script, rec.to_string_lossy(), mode],
    })
}

fn merged(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    base
}

/// `oxa mcp list --json` with no engine: (exit code, parsed output, stderr).
fn list_json(env: &TestEnv) -> (i32, Value, String) {
    let out = run_oxa(env, NO_ENGINE, &["mcp", "list", "--json"], "");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (
        out.status.code().unwrap_or(-1),
        value,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn server_entry<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["servers"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["name"] == name))
        .unwrap_or_else(|| panic!("no server {name} in {list}"))
}

/// One codemode call that runs `code` as a script, then returns the final text.
fn run_script(env: &TestEnv, code: &str) -> (Output, Vec<Recorded>) {
    let server = fake(vec![
        ScriptTurn::tool("call_1", "codemode", &json!({ "code": code }).to_string()),
        ScriptTurn::text("done"),
    ]);
    let out = run_oxa(env, &server.url(), &["run the script"], "");
    (out, server.requests())
}

/// The text of the last tool result the engine saw.
fn last_tool_text(reqs: &[Recorded]) -> String {
    tool_text_of(reqs, reqs.len().saturating_sub(1))
}

/// The text of the tool result that answers call `id`, in the request after the one that made it.
fn tool_text_of(reqs: &[Recorded], index: usize) -> String {
    let text = reqs
        .get(index)
        .and_then(|r| r.messages.last())
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    // codemode prefixes its output with a status line; the script's own text follows it.
    text.strip_prefix("Script completed\n")
        .unwrap_or(&text)
        .to_string()
}

/// Run a script with `code` in a session whose user `mcp.json` is `servers`. Returns the output
/// and the requests the engine saw.
fn session_with(env: &TestEnv, servers: Value, code: &str) -> (Output, Vec<Recorded>) {
    write_json(&user_file(env), &json!({ "mcpServers": servers }));
    run_script(env, code)
}

/// A stdio `fake` server with the fixture; `rec` is its record directory.
fn fake_server_config(env: &TestEnv, rec_name: &str, mode: &str, extra: Value) -> (Value, PathBuf) {
    let script = install_server(env);
    let rec = record_dir(env, rec_name);
    (merged(stdio_entry(&script, &rec, mode), extra), rec)
}

fn wait_gone(pid: &str, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The doc comment and the body of row test `fn <id>()` in this file (for the meta-checks).
fn row_source(id: &str) -> Option<(String, String)> {
    let needle = format!("\nfn {}()", id.to_lowercase().replace('-', "_"));
    let at = SELF_SRC.find(&needle)? + 1;
    let head_start = SELF_SRC[..at].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    let head = SELF_SRC[head_start..at].to_string();
    let body_end = SELF_SRC[at..]
        .find("\n}\n")
        .map(|i| at + i + 3)
        .unwrap_or(SELF_SRC.len());
    Some((head, SELF_SRC[at..body_end].to_string()))
}

/// `(id, verdict, note)` of every MCP row in PARITY.md.
fn mcp_rows() -> Vec<(String, String, String)> {
    PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| MCP-"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (cells[1].to_string(), cells[3].to_string(), cells[4].to_string())
        })
        .collect()
}

// ---------------------------------------------------------------- meta-checks

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = mcp_rows();
    assert_eq!(rows.len(), 23, "the MCP area has 23 rows");
    for (id, verdict, _) in &rows {
        assert!(
            row_source(id).is_some(),
            "{id} ({verdict}) has no test fn in tests/mcp.rs"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    for (id, verdict, _) in mcp_rows().iter().filter(|r| r.1 == "P") {
        let (head, _) = row_source(id).unwrap();
        assert!(
            head.contains("/// pi:") || head.contains("/// pi "),
            "{id} is P and must cite pi's mcp.md in its doc comment ({verdict})"
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    for (id, _, note) in mcp_rows().iter().filter(|r| r.1 == "A") {
        assert!(!note.is_empty(), "{id} is A and needs its invariant in the note");
        let (head, _) = row_source(id).unwrap();
        assert!(
            head.contains("/// invariant:"),
            "{id} is A and its test must name the invariant (`/// invariant:`)"
        );
    }
}

// ---------------------------------------------------------------- rows

/// pi: mcp.md "Configure servers": `mcp.json` is read from the user agent directory and from the
/// project's `.oxa/` (only when trusted); a project entry replaces a user entry of the same name;
/// an entry with only `enabled`, `exposure` or `toolExposure` overrides a user server and keeps the
/// rest of it.
#[test]
fn mcp_001() {
    let env = TestEnv::new();
    // The `untrusted` runs below name the owner's opt-in: the shipped default trusts the project.
    write_json(
        &env.agent_dir().join("settings.json"),
        &json!({"defaultProjectTrust": "never"}),
    );
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {
            "same": {"command": "user-cmd"},
            "keep": {"url": "https://127.0.0.1:9/user-mcp", "headers": {"X-H": "1"}}
        }}),
    );
    write_json(
        &project_file(&env),
        &json!({"mcpServers": {
            "same": {"url": "https://127.0.0.1:9/project-mcp"},
            "keep": {"enabled": false}
        }}),
    );

    // Untrusted project: its file is not read.
    // `mcp list` connects (pi mcp.md "Check servers"); none of these servers can be reached
    // (port 9 refuses, `user-cmd` does not exist), so it exits 1 without any config error.
    let (code, list, err) = list_json(&env);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not trusted"), "{err}");
    assert!(
        list["diagnostics"].as_array().is_some_and(|d| d.is_empty()),
        "{list}"
    );
    assert_eq!(server_entry(&list, "same")["transport"], "stdio");
    assert_eq!(server_entry(&list, "keep")["enabled"], true);

    trust_work(&env);
    let (code, list, err) = list_json(&env);
    assert_eq!(code, 1, "{err}");
    assert!(
        list["diagnostics"].as_array().is_some_and(|d| d.is_empty()),
        "{list}"
    );
    let same = server_entry(&list, "same");
    assert_eq!(same["transport"], "http", "project replaces the user entry");
    assert_eq!(same["defined_in"], "project");
    let keep = server_entry(&list, "keep");
    assert_eq!(keep["enabled"], false, "partial override");
    assert_eq!(keep["transport"], "http", "partial override keeps the url");
    assert_eq!(keep["defined_in"], "user");
    assert_eq!(keep["project_override"], true);
}

/// pi: mcp.md "Configure servers": stdio servers use `command`, `args`, `env` and `cwd`; `env`
/// values are passed to the process, `cwd` is the working directory it starts in.
/// invariant: stdio servers are spawned by the same process spawner as the other child processes
/// (W-guard, sandbox), so the spawn parameters are asserted from inside the child.
#[test]
fn mcp_002() {
    let env = TestEnv::new();
    let sub = env.root().join("sub-cwd");
    std::fs::create_dir_all(&sub).unwrap();
    let (entry, rec) = fake_server_config(
        &env,
        "mcp_002",
        "plain",
        json!({"env": {"OXA_TEST_VAR": "from-config"}, "cwd": sub.to_string_lossy()}),
    );
    let (out, reqs) = session_with(
        &env,
        json!({"fake": entry}),
        "const r = await tools.mcp__fake__echo({ text: \"hi\" }); text(\"got \" + r);",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        last_tool_text(&reqs).contains("echo:hi"),
        "{}",
        last_tool_text(&reqs)
    );
    let init: Value = serde_json::from_str(&std::fs::read_to_string(rec.join("init.json")).unwrap()).unwrap();
    assert_eq!(init["oxa_test_var"], "from-config");
    let seen = std::fs::canonicalize(init["cwd"].as_str().unwrap()).unwrap();
    assert_eq!(seen, std::fs::canonicalize(&sub).unwrap());
}

/// pi: mcp.md "Configure servers": HTTP servers take `url` and `headers`; `pi mcp add --url` with
/// `--header` and `--bearer-token-env-var` writes them (the bearer variable as an `Authorization`
/// header that reads the variable, `Bearer ${NAME}`).
#[test]
fn mcp_003() {
    let env = TestEnv::new();
    let out = run_oxa(
        &env,
        NO_ENGINE,
        &[
            "mcp",
            "add",
            "docs",
            "--url",
            "https://127.0.0.1:9/mcp",
            "--header",
            "X-Team=ops",
            "--bearer-token-env-var",
            "DOCS_TOKEN",
        ],
        "",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(user_file(&env)).unwrap()).unwrap();
    let docs = &saved["mcpServers"]["docs"];
    assert_eq!(docs["url"], "https://127.0.0.1:9/mcp");
    assert_eq!(docs["headers"]["X-Team"], "ops");
    assert_eq!(
        docs["headers"]["Authorization"], "Bearer ${DOCS_TOKEN}",
        "--bearer-token-env-var is written as the Authorization header"
    );
    let (_, list, _) = list_json(&env);
    assert_eq!(server_entry(&list, "docs")["transport"], "http");
}

/// pi: mcp.md "Configure servers": `type` is optional (a `url` selects streamable HTTP, a `command`
/// stdio); `type` may be `stdio`, `http` or `streamable-http`; `sse` is rejected with a hint to use
/// the streamable HTTP URL.
#[test]
fn mcp_004() {
    let env = TestEnv::new();
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {
            "plain-url": {"url": "https://127.0.0.1:9/a"},
            "typed": {"type": "streamable-http", "url": "https://127.0.0.1:9/b"},
            "legacy": {"type": "sse", "url": "https://c.example/sse"}
        }}),
    );
    let (code, list, err) = list_json(&env);
    assert_eq!(server_entry(&list, "plain-url")["transport"], "http");
    assert_eq!(server_entry(&list, "typed")["transport"], "http");
    assert_eq!(code, 1, "an invalid entry makes list exit 1");
    assert!(err.contains("legacy") && err.contains("streamable"), "{err}");
    assert!(
        list["servers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "legacy"),
        "the sse entry is skipped"
    );
}

/// pi: mcp.md "Configure servers": `${VAR}` in `env` values expands from the environment, and a
/// whole-value `!command` runs the command and uses its output.
/// invariant: header expansion uses the same `expand_value` and is covered by the HTTP tests
/// (oxa-mcp E8); this row checks the `env` path through a real stdio server.
#[test]
fn mcp_005() {
    let env = TestEnv::new();
    let (entry, rec) = fake_server_config(
        &env,
        "mcp_005",
        "plain",
        json!({"env": {"OXA_TEST_VAR": "${LANG}", "OXA_TEST_CMD": "!echo from-command"}}),
    );
    let (out, _) = session_with(
        &env,
        json!({"fake": entry}),
        "text(await tools.mcp__fake__echo({ text: \"x\" }));",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let init: Value = serde_json::from_str(&std::fs::read_to_string(rec.join("init.json")).unwrap()).unwrap();
    assert_eq!(init["oxa_test_var"], "C.UTF-8", "${{LANG}} expanded");
    assert_eq!(init["oxa_test_cmd"], "from-command", "!command output used");
}

/// pi: mcp.md "Configure servers": `timeout` is the per-request timeout in seconds; progress
/// notifications reset it, so a slow call that reports progress completes.
/// The default of 60 s is not waited for here: the timeout of 1 s is exercised instead.
#[test]
fn mcp_006() {
    let env = TestEnv::new();
    // Two servers: the fixture answers one request at a time, so a silent call that is still
    // running would delay the progress call past its timeout.
    let (silent, _) = fake_server_config(&env, "mcp_006_silent", "plain", json!({"timeout": 1}));
    let (progressing, _) = fake_server_config(&env, "mcp_006_progress", "plain", json!({"timeout": 1}));
    let code = r#"
        let a; try { await tools.mcp__silent__slow({ seconds: 3 }); a = "ok"; } catch (e) { a = "err:" + e; }
        const b = await tools.mcp__progress__slow({ seconds: 3, progress: true });
        text(a + " | " + b);
    "#;
    let (out, reqs) = session_with(&env, json!({"silent": silent, "progress": progressing}), code);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = last_tool_text(&reqs);
    let (silent, progressed) = text
        .split_once(" | ")
        .unwrap_or_else(|| panic!("two results, got: {text}"));
    assert!(
        silent.starts_with("err:"),
        "a silent 3 s call times out at 1 s: {text}"
    );
    assert_eq!(progressed, "slept", "progress resets the timeout: {text}");
}

/// pi: mcp.md "Configure servers": `enabled: false` keeps an entry without connecting it; an invalid
/// entry is reported and skipped without stopping the other servers.
#[test]
fn mcp_007() {
    let env = TestEnv::new();
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {
            "off": {"command": "x", "enabled": false},
            "bad name": {"command": "x"},
            "empty": {},
            "fine": {"url": "https://127.0.0.1:9/ok"}
        }}),
    );
    let (code, list, err) = list_json(&env);
    assert_eq!(code, 1, "invalid entries make list exit 1");
    assert_eq!(server_entry(&list, "off")["enabled"], false);
    assert_eq!(server_entry(&list, "fine")["enabled"], true);
    assert!(err.contains("bad name") && err.contains("empty"), "{err}");
}

/// pi: mcp.md "Configuration rules": server names use letters, digits, `_` and `-`; tools are named
/// `mcp__<server>__<tool>` with other characters replaced by `_`; names differing only in `-` and
/// `_` are the same server (the second is rejected); colliding tool names all get a hash suffix.
#[test]
fn mcp_008() {
    let env = TestEnv::new();
    let bad = run_oxa(&env, NO_ENGINE, &["mcp", "add", "bad.name", "--", "x"], "");
    assert_ne!(
        bad.status.code(),
        Some(0),
        "a dot is not allowed in a server name"
    );

    let (entry, _) = fake_server_config(&env, "mcp_008", "plain", json!({}));
    let code = r#"
        const a = await tools.mcp__my_srv__echo({ text: "x" });
        text(a + " | " + typeof tools.mcp__my_srv__a_b);
    "#;
    let (out, reqs) = session_with(&env, json!({"my-srv": entry}), code);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        last_tool_text(&reqs),
        "echo:x | undefined",
        "the server is my_srv and the colliding a.b / a_b tools carry a hash suffix"
    );

    let same = run_oxa(&env, NO_ENGINE, &["mcp", "add", "my_srv", "--", "x"], "");
    assert_eq!(same.status.code(), Some(2), "my_srv is the same server as my-srv");
}

/// pi: mcp.md "Background" (Diagnose connection problems): servers connect in the background at
/// session start; the first prompt waits up to 10 s, but only for servers with `direct` tools. A
/// server that is slow to start is therefore declared when it has direct tools.
#[test]
fn mcp_009() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_009", "slow_init", json!({"exposure": "direct"}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let server = fake(vec![ScriptTurn::text("done")]);
    let out = run_oxa(&env, &server.url(), &["hi"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reqs = server.requests();
    assert!(
        reqs[0].tools_json.contains("mcp__fake__echo"),
        "the first prompt waited for the direct server (2 s start): {}",
        reqs[0].tools_json
    );
}

/// pi: mcp.md "Control tool exposure": `codemode` (the default) is callable from scripts and is
/// neither declared to the model nor listed in the codemode description.
#[test]
fn mcp_010() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_010", "plain", json!({}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let (out, reqs) = run_script(&env, "text(await tools.mcp__fake__echo({ text: \"hi\" }));");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !reqs[0].tools_json.contains("mcp__fake__echo"),
        "{}",
        reqs[0].tools_json
    );
    assert_eq!(last_tool_text(&reqs), "echo:hi");
}

/// invariant: `deferred` tools are found with `tool_search` (matches as text) and called through the
/// one frozen `call_tool {name, args}`; the declared tool list does not change with the servers.
#[test]
fn mcp_011() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_011", "plain", json!({"exposure": "deferred"}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let server = fake(vec![
        ScriptTurn::tool("s1", "tool_search", &json!({ "query": "echo" }).to_string()),
        ScriptTurn::tool(
            "c1",
            "call_tool",
            &json!({ "name": "mcp__fake__echo", "args": { "text": "hi" } }).to_string(),
        ),
        ScriptTurn::text("done"),
    ]);
    let out = run_oxa(&env, &server.url(), &["find it"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reqs = server.requests();
    assert!(
        !reqs[0].tools_json.contains("mcp__fake__echo"),
        "not declared before a search"
    );
    assert!(
        tool_text_of(&reqs, 1).contains("mcp__fake__echo"),
        "{}",
        tool_text_of(&reqs, 1)
    );
    assert!(
        last_tool_text(&reqs).contains("echo:hi"),
        "{}",
        last_tool_text(&reqs)
    );
}

/// invariant: `direct` tools are declared to the model at the start of the epoch, like built-in
/// tools, and are callable by their full name in a model tool call.
#[test]
fn mcp_012() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_012", "plain", json!({"exposure": "direct"}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let server = fake(vec![
        ScriptTurn::tool("c1", "mcp__fake__echo", &json!({ "text": "hi" }).to_string()),
        ScriptTurn::text("done"),
    ]);
    let out = run_oxa(&env, &server.url(), &["say hi"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reqs = server.requests();
    assert!(
        reqs[0].tools_json.contains("mcp__fake__echo"),
        "{}",
        reqs[0].tools_json
    );
    assert!(
        tool_text_of(&reqs, 1).contains("echo:hi"),
        "{}",
        tool_text_of(&reqs, 1)
    );
}

/// pi: mcp.md "Control tool exposure": `hidden` servers are registered but unreachable, and
/// `toolExposure` overrides the server exposure per tool (exact names, then `*` patterns).
/// The first prompt waits for a server whose direct tools come only from `toolExposure` and
/// declares them; the reachability of every case from scripts is asserted too.
#[test]
fn mcp_013() {
    let env = TestEnv::new();
    let (hidden, _) = fake_server_config(&env, "mcp_013_hidden", "plain", json!({"exposure": "hidden"}));
    let (mixed, _) = fake_server_config(
        &env,
        "mcp_013_mixed",
        "plain",
        json!({"toolExposure": {"echo": "direct", "write_*": "hidden"}}),
    );
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {"hid": hidden, "mix": mixed}}),
    );
    let server = fake(vec![ScriptTurn::text("ignored")]);
    let _ = run_oxa(&env, &server.url(), &["hi"], "");
    let reqs = server.requests();
    assert!(
        !reqs[0].tools_json.contains("mcp__hid__"),
        "a hidden server declares nothing"
    );
    assert!(
        !reqs[0].tools_json.contains("mcp__mix__write_thing"),
        "write_* is hidden"
    );
    assert!(
        reqs[0].tools_json.contains("\"mcp__mix__echo\""),
        "the per-tool direct echo is declared in the first prompt: {}",
        reqs[0].tools_json
    );

    let code = r#"text(typeof tools.mcp__hid__echo + " " + typeof tools.mcp__mix__write_thing + " " + typeof tools.mcp__mix__echo);"#;
    let (out, reqs) = run_script(&env, code);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        last_tool_text(&reqs),
        "undefined undefined function",
        "hidden tools are unreachable from scripts; the direct echo is callable"
    );
}

/// invariant: the `mcp_servers` section of the system prompt is part of the frozen prefix (MCP-014
/// changes it at the next epoch only); each enabled codemode/deferred server is listed with one line
/// of its description, cut to 250 characters, and the whole section fits 4096 characters, with the
/// servers that do not fit counted as `… N more servers`.
#[test]
fn mcp_014() {
    let env = TestEnv::new();
    let long = "d".repeat(300);
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {"fake": {"command": "nonexistent-oxa-fixture-command", "description": long}}}),
    );
    let server = fake(vec![ScriptTurn::text("done")]);
    let out = run_oxa(&env, &server.url(), &["hi"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let system = &server.requests()[0].system;
    assert!(
        system.contains("- fake (codemode): "),
        "the section names the server"
    );
    // The cap is pi's 250 characters (extensions/mcp/index.ts MAX_SERVER_DESCRIPTION_CHARS and
    // `truncate`): 249 characters of the description and the ellipsis.
    let line = format!("- fake (codemode): {}…\n", "d".repeat(249));
    let line_end = format!("- fake (codemode): {}…", "d".repeat(249));
    assert!(
        system.contains(&line) || system.ends_with(&line_end) || system.contains(&format!("{line_end}<")),
        "a lone server keeps exactly 250 characters of its description: {system}"
    );
    assert!(!system.contains(&"d".repeat(250)), "description cut at 250 chars");

    // Many servers: each description gets less room, and past the 4096-character budget the rest
    // are counted (`… N more servers`). 230 names of 19 characters do not fit.
    let mut servers = serde_json::Map::new();
    for i in 0..230 {
        servers.insert(
            format!("srv{i:03}"),
            json!({"command": "nonexistent-oxa-fixture-command"}),
        );
    }
    write_json(&user_file(&env), &json!({"mcpServers": servers}));
    let server = fake(vec![ScriptTurn::text("done")]);
    let out = run_oxa(&env, &server.url(), &["hi"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let system = &server.requests()[0].system;
    assert!(
        system.contains("more server"),
        "the section is cut and counts the servers left out"
    );
    assert!(
        system.contains("- srv000 (codemode)"),
        "the first servers are kept"
    );
}

/// invariant: resource tools (`list_mcp_resources`, `read_mcp_resource`) are text-only on the O-144
/// engine: an image blob becomes the fixed placeholder line, text is passed through.
#[test]
fn mcp_015() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_015", "plain", json!({}));
    let code = r#"
        const listed = await tools.list_mcp_resources({ server: "fake" });
        const pic = await tools.read_mcp_resource({ server: "fake", uri: "res://pic" });
        const one = await tools.read_mcp_resource({ server: "fake", uri: "res://one" });
        text(listed + " | " + pic + " | " + one);
    "#;
    let (out, reqs) = session_with(&env, json!({"fake": entry}), code);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = last_tool_text(&reqs);
    assert!(text.contains("res://one"), "{text}");
    assert!(
        text.contains("[image image 0x0: not visible to this model]"),
        "{text}"
    );
    assert!(text.contains("resource body"), "{text}");
}

/// A local OAuth-protected MCP server for the sign-in rows: `POST /mcp` answers 401 until a
/// request carries `Authorization: Bearer tok-016`, then speaks enough JSON-RPC for a connect
/// (`initialize`, notifications, `tools/list`); the RFC 9728 / RFC 8414 metadata, dynamic
/// registration and the token endpoint answer like a real authorization server. Loopback only.
fn oauth_mcp_server() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let me = base.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut length = 0usize;
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    auth = h["authorization:".len()..].trim().to_string();
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let json = |status: &str, v: Value| (status.to_string(), v.to_string(), String::new());
            let (status, text, extra) = if path.starts_with("/.well-known/oauth-protected-resource") {
                json(
                    "200 OK",
                    json!({"resource": format!("{me}/mcp"), "authorization_servers": [me]}),
                )
            } else if path.starts_with("/.well-known/oauth-authorization-server") {
                json(
                    "200 OK",
                    json!({
                        "issuer": me,
                        "authorization_endpoint": format!("{me}/authorize"),
                        "token_endpoint": format!("{me}/token"),
                        "registration_endpoint": format!("{me}/register"),
                        "code_challenge_methods_supported": ["S256"],
                        "response_types_supported": ["code"]
                    }),
                )
            } else if path == "/register" {
                json("201 Created", json!({"client_id": "cid-016"}))
            } else if path == "/token" {
                json(
                    "200 OK",
                    json!({"access_token": "tok-016", "token_type": "Bearer", "expires_in": 3600}),
                )
            } else if path == "/mcp" && method == "POST" {
                if auth != "Bearer tok-016" {
                    (
                        "401 Unauthorized".to_string(),
                        String::new(),
                        format!("WWW-Authenticate: Bearer resource_metadata=\"{me}/.well-known/oauth-protected-resource/mcp\"\r\n"),
                    )
                } else {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    match req["method"].as_str().unwrap_or("") {
                        "initialize" => json(
                            "200 OK",
                            json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                                "protocolVersion": "2025-11-25",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "oauth-016", "version": "1"}
                            }}),
                        ),
                        "tools/list" => json(
                            "200 OK",
                            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
                                {"name": "ping", "description": "Ping", "inputSchema": {"type": "object"}}
                            ]}}),
                        ),
                        m if m.starts_with("notifications/") => {
                            ("202 Accepted".to_string(), String::new(), String::new())
                        }
                        _ => json("200 OK", json!({"jsonrpc": "2.0", "id": req["id"], "result": {}})),
                    }
                }
            } else {
                ("404 Not Found".to_string(), String::new(), String::new())
            };
            let ctype = if text.is_empty() {
                ""
            } else {
                "Content-Type: application/json\r\n"
            };
            let mut out = stream;
            let _ = write!(
                out,
                "HTTP/1.1 {status}\r\n{ctype}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    base
}

/// pi: mcp.md "Authenticate with OAuth": `oxa mcp login` signs in to an HTTP server (PKCE, dynamic
/// client registration or a configured client, the callback server or a pasted redirect URL).
/// Driven end to end against a loopback OAuth server: the command connects (401: needs sign-in),
/// discovers the authorization server, registers a client, prints the authorization URL and
/// waits; the test plays the browser and calls the printed redirect URI with a code and the
/// state, the code is exchanged, the token stored (0600) and the server connects with it.
#[test]
fn mcp_016() {
    use std::io::{BufRead, BufReader, Read, Write};
    let env = TestEnv::new();
    let base = oauth_mcp_server();
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {"docs": {"url": format!("{base}/mcp")}}}),
    );
    let mut child = std::process::Command::new(oxa_parity::oxa_bin())
        .args(["mcp", "login", "docs", "--timeout", "30"])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(NO_ENGINE))
        .envs(oxa_parity::note_off_env())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oxa");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut printed = String::new();
    let auth_url = loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).expect("read") > 0,
            "no authorization URL: {printed}"
        );
        printed.push_str(&line);
        if line.starts_with(&format!("{base}/authorize?")) {
            break line.trim().to_string();
        }
    };
    let query: Vec<(String, String)> = auth_url
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let param = |k: &str| {
        query
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(param("client_id"), "cid-016", "dynamic registration: {auth_url}");
    assert_eq!(param("code_challenge_method"), "S256");
    assert!(!param("code_challenge").is_empty());
    let redirect = param("redirect_uri").replace("%3A", ":").replace("%2F", "/");
    let callback = redirect.strip_prefix("http://").expect("loopback redirect");
    let (host, path) = callback.split_once('/').expect("redirect path");
    let mut browser = std::net::TcpStream::connect(host).expect("callback server");
    write!(
        browser,
        "GET /{path}?code=code-016&state={} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        param("state")
    )
    .expect("browser request");
    let mut page = String::new();
    let _ = browser.read_to_string(&mut page);
    assert!(page.contains("Sign-in complete"), "{page}");

    let mut rest = String::new();
    let _ = stdout.read_to_string(&mut rest);
    let out = child.wait_with_output().expect("wait");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{printed}{rest}{err}");
    assert!(
        rest.contains("Signed in to MCP server \"docs\" (1 tools)."),
        "{rest}"
    );
    let auth = env.agent_dir().join("mcp-auth.json");
    let stored = std::fs::read_to_string(&auth).expect("token file");
    assert!(stored.contains("tok-016"), "{stored}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the token file is private");

    // Signed in: a second login connects at once.
    let again = run_oxa(&env, NO_ENGINE, &["mcp", "login", "docs"], "");
    assert_eq!(again.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&again.stdout).contains("Already signed in"));
}

/// pi: mcp.md "Authenticate with OAuth": the token file is 0600, a 401 refreshes the token, and
/// `logout` deletes the stored credentials. Credentials belong to a server name and URL (the
/// token file is keyed by both), so the stored credential is written that way.
#[test]
fn mcp_017() {
    let env = TestEnv::new();
    write_json(
        &user_file(&env),
        &json!({"mcpServers": {"docs": {"url": "https://127.0.0.1:9/mcp"}}}),
    );
    let auth = env.agent_dir().join("mcp-auth.json");
    write_json(
        &auth,
        &json!({"docs": {"https://127.0.0.1:9/mcp": {"access_token": "t"}}}),
    );
    // oxa writes the token file 0600 and reads only a private one; the fixture matches.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = run_oxa(&env, NO_ENGINE, &["mcp", "logout", "docs"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Signed out of MCP server \"docs\"."));
    let left = std::fs::read_to_string(&auth).unwrap_or_default();
    assert!(
        !left.contains("access_token"),
        "logout deletes the credentials: {left}"
    );
    // Nothing stored: still a success (pi).
    let out = run_oxa(&env, NO_ENGINE, &["mcp", "logout", "docs"], "");
    assert_eq!(out.status.code(), Some(0));
}

/// pi: mcp.md "Inspect or change a server": `/mcp` lists servers (attention first), signs in, reconnects,
/// enables or disables, and saves exposure changes to the defining file.
/// Outside the TUI pi's `/mcp` prints the server status (mcp extension `formatStatus`, the
/// handler's non-TUI branch); `oxa -p /mcp` prints it on stdout. The selector actions (sign in,
/// enable/disable, exposure saved to the defining file) are TUI actions, exercised by the
/// selector tests of oxa-interactive (F7).
#[test]
fn mcp_018() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_018", "plain", json!({}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let server = fake(vec![ScriptTurn::text("done")]);
    let out = run_oxa(&env, &server.url(), &["-p", "/mcp"], "");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fake"),
        "/mcp prints the server status outside the TUI: {text}"
    );
}

/// pi: mcp.md "Diagnose connection problems": server logging notifications are appended to
/// `mcp.log` as `<time> [<server>] <level> <logger>: <message>`; the file moves to `mcp.log.1`
/// after it grows past 5 MB.
#[test]
fn mcp_019() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_019", "plain", json!({}));
    let code = r#"
        for (let i = 0; i < 4; i++) { await tools.mcp__fake__log_big({ bytes: 2000000 }); }
        text("logged");
    "#;
    let (out, reqs) = session_with(&env, json!({"fake": entry}), code);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(last_tool_text(&reqs), "logged");
    let log = env.agent_dir().join("mcp.log");
    let rotated = env.agent_dir().join("mcp.log.1");
    assert!(rotated.exists(), "8 MB of log rotated the file");
    let text = std::fs::read_to_string(&log).expect("mcp.log");
    assert!(
        text.contains("[fake] info fixture: "),
        "line format: {}",
        &text[..text.len().min(120)]
    );
}

/// pi: mcp.md "Diagnose connection problems": a stdio server is stopped by closing its stdin, then
/// SIGTERM, then SIGKILL of its process group. A server that ignores the first two is killed.
#[test]
fn mcp_020() {
    let env = TestEnv::new();
    let (plain, rec_plain) = fake_server_config(&env, "mcp_020_plain", "plain", json!({}));
    let (stubborn, rec_stubborn) = fake_server_config(&env, "mcp_020_stubborn", "stubborn", json!({}));
    let (out, reqs) = session_with(
        &env,
        json!({"fake": plain, "stuck": stubborn}),
        "text(await tools.mcp__fake__echo({ text: \"x\" }) + ' ' + await tools.mcp__stuck__echo({ text: \"y\" }));",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        last_tool_text(&reqs).contains("echo:x"),
        "{}",
        last_tool_text(&reqs)
    );
    assert!(
        rec_plain.join("eof").exists(),
        "closing stdin stops a well-behaved server"
    );
    let pid = std::fs::read_to_string(rec_stubborn.join("pid")).expect("stubborn pid");
    assert!(
        wait_gone(pid.trim(), std::time::Duration::from_secs(20)),
        "the stubborn server (pid {}) was not killed after the session",
        pid.trim()
    );
}

/// pi: mcp.md "Shell commands": `--no-mcp` disables MCP for one run; no server starts and codemode
/// is not offered.
#[test]
fn mcp_021() {
    let env = TestEnv::new();
    let (entry, rec) = fake_server_config(&env, "mcp_021", "plain", json!({}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let server = fake(vec![ScriptTurn::text("plain")]);
    let out = run_oxa(&env, &server.url(), &["--no-mcp", "hi"], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!server.requests()[0].tools_json.contains("codemode"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!rec.join("pid").exists(), "no server process was started");
}

/// invariant: MCP tools are classed by their annotations (`readOnlyHint` -> read, otherwise write);
/// in workspace mode a write-class call asks, and print mode has no one to ask, so it is not run.
#[test]
fn mcp_022() {
    let env = TestEnv::new();
    let (entry, _) = fake_server_config(&env, "mcp_022", "plain", json!({}));
    write_json(&user_file(&env), &json!({"mcpServers": {"fake": entry}}));
    let code = r#"
        let a; try { a = await tools.mcp__fake__write_thing({}); } catch (e) { a = "refused: " + e; }
        const b = await tools.mcp__fake__echo({ text: "r" });
        text(a + " | " + b);
    "#;
    let server = fake(vec![
        ScriptTurn::tool("c1", "codemode", &json!({ "code": code }).to_string()),
        ScriptTurn::text("done"),
    ]);
    let out = run_oxa(
        &env,
        &server.url(),
        &["--permission-mode", "workspace", "act"],
        "",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = last_tool_text(&server.requests());
    let (write, read) = text
        .split_once(" | ")
        .unwrap_or_else(|| panic!("two results, got: {text}"));
    assert!(
        !write.contains("write_thing:{}"),
        "a write-class call ran without approval: {write}"
    );
    assert_eq!(read, "echo:r", "a read-class call runs");
}

/// pi: mcp.md "Use MCP from the SDK": an embedder adds MCP to its session through the library.
/// invariant: the SDK surface is the Rust API of oxa-mcp (`McpManager`): an embedder loads
/// `mcp.json`, starts the manager, lists the servers and tools, calls a tool (codemode
/// exposure: reached through the manager, never through the frozen tool list) and stops it.
#[test]
fn mcp_025() {
    use oxa_mcp::{load_config, ManagerOpts, McpManager, ServerState};
    use std::sync::Arc;
    let env = TestEnv::new();
    let (entry, _rec) = fake_server_config(&env, "mcp_025", "plain", json!({}));
    let user = env.agent_dir().join("mcp.json");
    write_json(&user, &json!({ "mcpServers": { "fake": entry } }));
    let config = load_config(&user, None);
    assert_eq!(config.servers.len(), 1);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = asked.clone();
    let manager = McpManager::new(ManagerOpts {
        config,
        cwd: env.work(),
        agent_dir: env.agent_dir(),
        home: env.home(),
        trusted: false,
        gate: Arc::new(move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }),
        client_version: "parity".to_string(),
    });
    manager.start();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let st = manager.server("fake").expect("the configured server");
        match st.state {
            ServerState::Connected => break,
            ServerState::Failed(e) => panic!("the server failed: {e}\n{}", st.stderr_tail),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connected in time: {:?}",
            st.state
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        asked.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the embedder's gate was asked"
    );
    let status = manager.server("fake").unwrap();
    assert!(
        status.tools.iter().any(|t| t.name.contains("echo")),
        "{:?}",
        status.tools
    );
    // Codemode exposure (the default): the tools are in the catalog for codemode, and
    // `wants_codemode` tells the embedder to add the codemode extension (pi: "Add ... the
    // codemode extension for codemode servers").
    assert!(manager.wants_codemode());
    assert!(
        manager.catalog().iter().any(|t| t.name == "mcp__fake__echo"),
        "{:?}",
        manager.catalog()
    );
    let result = manager
        .call_tool(
            "mcp__fake__echo",
            &json!({"text": "from the sdk"}),
            &oxa_proto::CancelToken::new(),
            &mut |_| {},
        )
        .expect("call");
    assert!(!result.is_error);
    assert_eq!(result.content[0]["text"], json!("echo:from the sdk"));
    manager.stop();
}

/// unsupported_in_oxa: MCP `sampling/createMessage` (O-144). The engine has one inference slot, so
/// the client does not advertise `sampling` and the server's request gets JSON-RPC -32601; the
/// manager never calls the engine. The fixture's `sample` tool reports the code it got back.
#[test]
fn server_sampling_cannot_start_side_inference() {
    let env = TestEnv::new();
    let (entry, rec) = fake_server_config(&env, "mcp_026", "plain", json!({}));
    let (out, reqs) = session_with(
        &env,
        json!({"fake": entry}),
        "text(await tools.mcp__fake__sample({}));",
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        last_tool_text(&reqs),
        "sampling:-32601",
        "unsupported: the server's sampling request is refused"
    );
    let caps: Value = serde_json::from_str(&std::fs::read_to_string(rec.join("caps.json")).unwrap()).unwrap();
    assert!(
        caps.get("sampling").is_none(),
        "sampling is not advertised: {caps}"
    );
    assert_eq!(reqs.len(), 2, "the engine saw only the session's own requests");
}
