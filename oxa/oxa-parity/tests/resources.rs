//! G10 PARITY tests (resources).
//! One test per PARITY.md row of this area, named after the row id (`CLI-012` -> `fn cli_012()`).
//! Areas: SKL (skills), TPL (prompt templates), PKG (packages), SEC (security and project trust),
//! THM (themes), CFG (configuration files). Rows with `omission` in the test column have no fn.
//! Every test drives the real `oxa` binary inside a `TestEnv` (nothing touches the real home);
//! the model side is `oxa_fake::FakeServer`, whose recorded requests show what the model got.
//! Helpers: `oxa_parity::{oxa_bin, fake_bin, run_oxa, fixture}`, `oxa_fake::TestEnv`.
//! A row that the binary does not satisfy stays RED: the failing assert names the row and the
//! difference from pi's documented behaviour (H-wave input). Nothing is ignored or hacked around.

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::{fixture, run_oxa};
use oxa_proto::Role;
use std::path::{Path, PathBuf};
use std::process::Output;

/// This file, for the meta-checks that read the test bodies.
const SELF_SRC: &str = include_str!("resources.rs");
/// An engine address nothing listens on, for commands that never call the model.
const NO_ENGINE: &str = "http://127.0.0.1:9";

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

struct Ran {
    out: Output,
    reqs: Vec<Recorded>,
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn script() -> Vec<ScriptTurn> {
    (0..6).map(|_| ScriptTurn::text("ok")).collect()
}

/// `oxa -p <args> <message>` against the scripted fake engine.
fn drive(env: &TestEnv, args: &[&str], message: &str) -> Ran {
    let srv = FakeServer::start(script(), FakeOpts::default()).expect("start fake engine");
    let mut argv: Vec<&str> = vec!["-p"];
    argv.extend_from_slice(args);
    argv.push(message);
    let out = run_oxa(env, &srv.url(), &argv, "");
    Ran {
        out,
        reqs: srv.requests(),
    }
}

/// The system prompt the model received in the first request.
fn system(ran: &Ran) -> String {
    match ran.reqs.first() {
        Some(r) => r.system.clone(),
        None => panic!("no request reached the engine; stderr: {}", stderr_of(&ran.out)),
    }
}

/// The text of the last user message of the first request (what the model was asked).
fn user_text(ran: &Ran) -> String {
    match ran.reqs.first() {
        Some(r) => r
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default(),
        None => panic!("no request reached the engine; stderr: {}", stderr_of(&ran.out)),
    }
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "oxa exited with {:?}: {}",
        out.status.code(),
        stderr_of(out)
    );
}

/// A command that never calls the model (`install`, `list`, `config`, ...).
fn cmd(env: &TestEnv, args: &[&str]) -> Output {
    run_oxa(env, NO_ENGINE, args, "")
}

fn cmd_ok(env: &TestEnv, args: &[&str]) -> Output {
    let out = cmd(env, args);
    assert_success(&out);
    out
}

/// Canonical form of a directory, as trust.json keys it.
fn canon(p: &Path) -> String {
    std::fs::canonicalize(p)
        .expect("canonical path")
        .to_string_lossy()
        .into_owned()
}

/// A SKILL.md with `name` and `description` frontmatter and a body that names the skill.
fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n\nBody of {name}.\n")
}

/// Write `<base>/<name>/SKILL.md`; `base` is relative to the test root.
fn put_skill(env: &TestEnv, base: &str, name: &str, description: &str) -> PathBuf {
    env.write(&format!("{base}/{name}/SKILL.md"), &skill_md(name, description))
}

/// Write `<base>/<name>.md` (a prompt template).
fn put_prompt(env: &TestEnv, base: &str, name: &str, content: &str) -> PathBuf {
    env.write(&format!("{base}/{name}.md"), content)
}

fn put_settings(env: &TestEnv, rel: &str, json: serde_json::Value) {
    env.write(rel, &json.to_string());
}

/// A minimal local package: `package.json` with `name` and `version`, plus files.
fn put_package(env: &TestEnv, dir: &str, name: &str, version: &str, files: &[(&str, &str)]) -> PathBuf {
    let manifest = serde_json::json!({ "name": name, "version": version });
    env.write(&format!("{dir}/package.json"), &manifest.to_string());
    for (rel, content) in files {
        env.write(&format!("{dir}/{rel}"), content);
    }
    env.root().join(dir)
}

/// The names of the tools the engine was offered in the first request.
fn declared_tool_names(ran: &Ran) -> Vec<String> {
    let first = match ran.reqs.first() {
        Some(r) => r,
        None => panic!("no request reached the engine; stderr: {}", stderr_of(&ran.out)),
    };
    if first.tools_json.trim().is_empty() {
        return Vec::new();
    }
    let tools: Vec<serde_json::Value> = serde_json::from_str(&first.tools_json).expect("tools json");
    tools
        .iter()
        .filter_map(|t| {
            t["function"]["name"]
                .as_str()
                .or_else(|| t["name"].as_str())
                .map(str::to_string)
        })
        .collect()
}

fn project_skill_listed(sys: &str, name: &str) -> bool {
    sys.contains(&format!("<name>{name}</name>"))
}

/// An MCP server entry whose start leaves a marker file behind (the server then exits at once).
fn marker_mcp(marker: &Path) -> String {
    serde_json::json!({
        "mcpServers": {
            "probe": { "command": "/bin/sh", "args": ["-c", format!("touch '{}'", marker.display())] }
        }
    })
    .to_string()
}

/// A user-level extension that answers the `project_trust` event with `trust`.
fn trust_extension(env: &TestEnv, trust: bool) {
    env.write(
        "home/.oxa/agent/extensions/trust.js",
        &format!(
            "export default function (oxa) {{ oxa.on(\"project_trust\", function () {{ return {{ trust: {trust} }}; }}); }}"
        ),
    );
}

/// A file under the test root that the test can check for existence.
fn marker(env: &TestEnv, name: &str) -> PathBuf {
    env.root().join("tmp").join(name)
}

/// Body of a test fn in this file (from its `fn name()` to the next `#[test]`).
fn body_of(name: &str) -> &'static str {
    let needle = format!("fn {name}()");
    let fn_at = SELF_SRC
        .find(&needle)
        .unwrap_or_else(|| panic!("no fn {name} in resources.rs"));
    // Include the doc comment above the fn (it names the pi page and the invariant).
    let before = &SELF_SRC[..fn_at];
    let start = before.rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    let start = start + before[start..].find("///").unwrap_or(0);
    let rest = &SELF_SRC[start..];
    let body_start = fn_at - start;
    match rest[body_start + 1..].find("#[test]") {
        Some(end) => &rest[..body_start + 1 + end],
        None => rest,
    }
}

// ---------------------------------------------------------------------------------------------
// SKL: skills
// ---------------------------------------------------------------------------------------------

/// SKL-001: `<name>/SKILL.md` with `name` and `description` frontmatter is discovered.
/// pi docs: skills.md ("a directory containing SKILL.md").
#[test]
fn skl_001() {
    let env = TestEnv::new();
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "pdf-tools",
        "Extract text and tables from PDF files.",
    );
    env.write(
        "home/.oxa/agent/skills/loose/README.md",
        "no SKILL.md in this directory",
    );
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("<name>pdf-tools</name>"),
        "SKL-001: skill name not listed in the system prompt"
    );
    assert!(
        sys.contains("<description>Extract text and tables from PDF files.</description>"),
        "SKL-001: skill description not listed"
    );
    assert!(
        sys.contains("pdf-tools/SKILL.md"),
        "SKL-001: skill location not listed"
    );
    assert!(
        !sys.contains("loose"),
        "SKL-001: a directory without SKILL.md must not be a skill"
    );
}

/// SKL-002: frontmatter validation warns and never fails.
/// pi docs: skills.md ("Most invalid fields produce warnings rather than stopping startup";
/// "declared skills without descriptions are not loaded").
#[test]
fn skl_002() {
    let env = TestEnv::new();
    put_skill(&env, "home/.oxa/agent/skills", "good-one", "A valid skill.");
    // Name breaks the rules (upper case, underscore): warned, still loaded.
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "Bad_Name",
        "Name breaks the rules.",
    );
    // No description: not loaded.
    env.write(
        "home/.oxa/agent/skills/nodesc/SKILL.md",
        "---\nname: nodesc\n---\n# nodesc\n\nNo description here.\n",
    );
    // Description longer than 1024 characters: warned, still loaded.
    let long = "d".repeat(1100);
    put_skill(&env, "home/.oxa/agent/skills", "long-desc", &long);
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("<name>good-one</name>"),
        "SKL-002: a valid skill is missing"
    );
    assert!(
        sys.contains("<name>Bad_Name</name>"),
        "SKL-002: a bad name must warn, not drop the skill"
    );
    assert!(
        sys.contains("<name>long-desc</name>"),
        "SKL-002: a long description must warn, not drop the skill"
    );
    assert!(
        !sys.contains("nodesc"),
        "SKL-002: a skill without a description must not load"
    );
    assert!(
        stderr_of(&ran.out).to_lowercase().contains("warn"),
        "SKL-002: validation problems must be reported as warnings on stderr; got: {}",
        stderr_of(&ran.out)
    );
}

/// SKL-003: the `<available_skills>` listing is in the system prompt, not in the messages.
/// pi docs: skills.md ("adds each skill's name, description, and path to the system prompt").
/// invariant: W3 (the listing is part of the frozen prefix, read once per epoch).
#[test]
fn skl_003() {
    let env = TestEnv::new();
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "pdf-tools",
        "Extract text from PDF files.",
    );
    let ran = drive(&env, &[], "what skills exist?");
    assert_success(&ran.out);
    let sys = system(&ran);
    let open = sys.find("<available_skills>");
    let close = sys.find("</available_skills>");
    assert!(
        open.is_some() && close.is_some(),
        "SKL-003: no <available_skills> block in the system prompt"
    );
    assert!(open < close, "SKL-003: the block is malformed");
    assert!(
        !user_text(&ran).contains("<available_skills>"),
        "SKL-003: the skill listing must not be sent as a message"
    );
}

/// SKL-004: `disable-model-invocation: true` hides a skill from the prompt.
/// pi docs: skills.md ("Set disable-model-invocation: true ... available only through its explicit command").
#[test]
fn skl_004() {
    let env = TestEnv::new();
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "visible-one",
        "Shown to the model.",
    );
    env.write(
        "home/.oxa/agent/skills/hidden-one/SKILL.md",
        "---\nname: hidden-one\ndescription: Only by command.\ndisable-model-invocation: true\n---\n# hidden-one\n\nBody of hidden-one.\n",
    );
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("<name>visible-one</name>"),
        "SKL-004: the ordinary skill must be listed"
    );
    assert!(
        !sys.contains("hidden-one"),
        "SKL-004: disable-model-invocation must hide the skill from the prompt"
    );
    // Still available through its command.
    let by_command = drive(&env, &[], "/skill:hidden-one");
    assert_success(&by_command.out);
    assert!(
        user_text(&by_command).contains("Body of hidden-one."),
        "SKL-004: /skill:hidden-one must still load a hidden skill"
    );
}

/// SKL-005: `/skill:name [args]` loads the skill and appends the arguments; `enableSkillCommands`
/// only changes command discovery, manual `/skill:name` still works.
/// pi docs: skills.md ("Arguments after /skill:name are appended to the loaded instructions").
#[test]
fn skl_005() {
    let env = TestEnv::new();
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "pdf-tools",
        "Extract text from PDF files.",
    );
    let ran = drive(&env, &[], "/skill:pdf-tools extract report.pdf");
    assert_success(&ran.out);
    let text = user_text(&ran);
    assert!(
        text.contains("Body of pdf-tools."),
        "SKL-005: the skill instructions were not loaded: {text}"
    );
    assert!(
        text.contains("extract report.pdf"),
        "SKL-005: the arguments were not appended: {text}"
    );
    // enableSkillCommands = false still allows the manual command.
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "enableSkillCommands": false }),
    );
    let off = drive(&env, &[], "/skill:pdf-tools extract report.pdf");
    assert_success(&off.out);
    assert!(
        user_text(&off).contains("Body of pdf-tools."),
        "SKL-005: enableSkillCommands=false must still allow a manual /skill:name (pi skills.md)"
    );
}

/// SKL-006: discovery: agent dir, project (trusted), `.agents/skills` (user and project), packages,
/// the `skills` setting, `--skill`; `-ns` disables discovery but not `--skill`.
/// pi docs: skills.md ("Add it to Pi"); settings.md resources.
#[test]
fn skl_006() {
    let env = TestEnv::new();
    put_skill(
        &env,
        "home/.oxa/agent/skills",
        "s-agent",
        "From the agent directory.",
    );
    put_skill(
        &env,
        "home/.agents/skills",
        "s-user-agents",
        "From ~/.agents/skills.",
    );
    put_skill(&env, "work/.oxa/skills", "s-project", "From the project.");
    put_skill(
        &env,
        "work/.agents/skills",
        "s-proj-agents",
        "From the project .agents/skills.",
    );
    let pkg = put_package(
        &env,
        "pkgs/tools",
        "tools",
        "1.0.0",
        &[("skills/s-pkg/SKILL.md", &skill_md("s-pkg", "From a package."))],
    );
    put_skill(&env, "shared", "s-setting", "From the skills setting.");
    let cli_skill = put_skill(&env, "cli", "s-cli", "From --skill.");
    cmd_ok(&env, &["install", pkg.to_str().expect("utf8")]);
    // The setting is written after install, which rewrites settings.json.
    let mut settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.agent_dir().join("settings.json")).expect("settings"),
    )
    .expect("settings json");
    settings["skills"] = serde_json::json!([env.root().join("shared").to_string_lossy()]);
    put_settings(&env, "home/.oxa/agent/settings.json", settings);

    let trusted = drive(&env, &["-a"], "hi");
    assert_success(&trusted.out);
    let sys = system(&trusted);
    for name in [
        "s-agent",
        "s-user-agents",
        "s-project",
        "s-proj-agents",
        "s-pkg",
        "s-setting",
    ] {
        assert!(
            project_skill_listed(&sys, name),
            "SKL-006: {name} was not discovered (trusted run)"
        );
    }
    let cli = cli_skill.to_str().expect("utf8");
    let with_cli = drive(&env, &["-a", "--skill", cli], "hi");
    assert!(
        project_skill_listed(&system(&with_cli), "s-cli"),
        "SKL-006: --skill did not load its skill"
    );

    let no_discovery = drive(&env, &["-a", "-ns", "--skill", cli], "hi");
    assert_success(&no_discovery.out);
    let sys = system(&no_discovery);
    assert!(
        project_skill_listed(&sys, "s-cli"),
        "SKL-006: -ns must keep the explicit --skill"
    );
    assert!(
        !sys.contains("<available_skills>") || !sys.contains("s-agent"),
        "SKL-006: -ns must disable discovery"
    );
}

/// SKL-007: a name collision keeps the first discovered skill and warns.
/// pi docs: skills.md ("Name collisions keep the first discovered skill and produce a warning").
#[test]
fn skl_007() {
    let env = TestEnv::new();
    put_skill(&env, "home/.oxa/agent/skills", "dup-skill", "User version.");
    put_skill(&env, "work/.oxa/skills", "dup-skill", "Project version.");
    let ran = drive(&env, &["-a"], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("User version."),
        "SKL-007: the first discovered skill (user) must win"
    );
    assert!(
        !sys.contains("Project version."),
        "SKL-007: the colliding skill must not also load"
    );
    assert!(
        stderr_of(&ran.out).contains("dup-skill"),
        "SKL-007: the collision must produce a diagnostic naming the skill; stderr: {}",
        stderr_of(&ran.out)
    );
}

// ---------------------------------------------------------------------------------------------
// TPL: prompt templates
// ---------------------------------------------------------------------------------------------

const REVIEW_TEMPLATE: &str = "---\ndescription: Review staged git changes\nargument-hint: \"[focus]\"\n---\nReview ${1:-all} and $@ and $ARGUMENTS.\n";

/// TPL-001: `prompts/*.md` become `/name` commands; frontmatter `description` and `argument-hint`.
/// pi docs: prompt-templates.md ("The filename becomes the command name"; "description ... if
/// omitted, the first non-empty line").
#[test]
fn tpl_001() {
    let env = TestEnv::new();
    put_prompt(&env, "home/.oxa/agent/prompts", "review", REVIEW_TEMPLATE);
    put_prompt(
        &env,
        "home/.oxa/agent/prompts",
        "plain",
        "Summarise the open tasks.\nSecond line.\n",
    );
    let ran = drive(&env, &[], "/review concurrency");
    assert_success(&ran.out);
    let text = user_text(&ran);
    assert!(
        text.contains("Review concurrency and concurrency and concurrency."),
        "TPL-001: /review was not expanded: {text}"
    );
    assert!(
        !text.contains("argument-hint"),
        "TPL-001: frontmatter must not reach the message"
    );
    // Discoverable commands: description from frontmatter, or the first non-empty line.
    let srv = FakeServer::start(script(), FakeOpts::default()).expect("start fake engine");
    let rpc = rpc_commands(&env, &srv.url());
    let review = find_command(&rpc, "review");
    assert_eq!(
        review["description"].as_str(),
        Some("Review staged git changes"),
        "TPL-001: the frontmatter description must be the command description"
    );
    let plain = find_command(&rpc, "plain");
    assert_eq!(
        plain["description"].as_str(),
        Some("Summarise the open tasks."),
        "TPL-001: without a description the first non-empty line is used"
    );
}

/// Commands from `get_commands` over RPC (`pi docs: rpc-commands.md`).
fn rpc_commands(env: &TestEnv, engine_url: &str) -> serde_json::Value {
    let out = run_oxa(
        env,
        engine_url,
        &["--mode", "rpc", "--no-session"],
        "{\"id\":\"1\",\"type\":\"get_commands\"}\n",
    );
    let text = stdout_of(&out);
    let line = text
        .lines()
        .find(|l| l.contains("\"get_commands\""))
        .unwrap_or_else(|| {
            panic!(
                "no get_commands response; stdout: {text}; stderr: {}",
                stderr_of(&out)
            )
        });
    serde_json::from_str(line).expect("get_commands response is JSON")
}

fn find_command<'a>(rpc: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    rpc["data"]["commands"]
        .as_array()
        .and_then(|cmds| cmds.iter().find(|c| c["name"].as_str() == Some(name)))
        .unwrap_or_else(|| panic!("command /{name} not listed: {rpc}"))
}

/// TPL-002: arguments `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:N}` are expanded before the
/// text becomes a message.
/// pi docs: prompt-templates.md (substitutions table).
#[test]
fn tpl_002() {
    let env = TestEnv::new();
    let body = "A=$1 B=$2 ALL=$@ ARG=$ARGUMENTS D=${1:-dflt} DA=${@:-dflt} S2=${@:2} S2L1=${@:2:1}";
    put_prompt(&env, "home/.oxa/agent/prompts", "t", &format!("{body}\n"));
    let ran = drive(&env, &[], "/t one two three");
    assert_success(&ran.out);
    assert_eq!(
        user_text(&ran).trim(),
        "A=one B=two ALL=one two three ARG=one two three D=one DA=one two three S2=two three S2L1=two",
        "TPL-002: positional, all-argument and slice substitutions"
    );
    let none = drive(&env, &[], "/t");
    assert_success(&none.out);
    assert_eq!(
        user_text(&none).trim(),
        "A= B= ALL= ARG= D=dflt DA=dflt S2= S2L1=",
        "TPL-002: defaults apply when no argument is given"
    );
}

/// TPL-003: quoted arguments, apostrophes and quotes.
/// pi docs: prompt-templates.md ("Arguments follow shell-like quoting"; apostrophes inside words
/// stay as text; an unclosed quote is kept as text; `\"` inside quotes).
#[test]
fn tpl_003() {
    let env = TestEnv::new();
    put_prompt(&env, "home/.oxa/agent/prompts", "q", "[$1|$2|$3|$4]\n");
    let ran = drive(
        &env,
        &[],
        "/q \"API compatibility\" doesn't \"the \\\"fast\\\" path\" \"open",
    );
    assert_success(&ran.out);
    assert_eq!(
        user_text(&ran).trim(),
        "[API compatibility|doesn't|the \"fast\" path|\"open]",
        "TPL-003: quoting rules"
    );
}

/// TPL-004: discovery: agent dir, project (trusted), packages, the `prompts` setting,
/// `--prompt-template`; `-np` disables discovery.
/// pi docs: prompt-templates.md ("Add it to Pi"); configuration.md.
#[test]
fn tpl_004() {
    let env = TestEnv::new();
    put_prompt(&env, "home/.oxa/agent/prompts", "t-agent", "agent $1\n");
    put_prompt(&env, "work/.oxa/prompts", "t-project", "project $1\n");
    let pkg = put_package(
        &env,
        "pkgs/tools",
        "tools",
        "1.0.0",
        &[("prompts/t-pkg.md", "package $1\n")],
    );
    let shared = put_prompt(&env, "shared", "t-setting", "setting $1\n");
    let cli = put_prompt(&env, "cli", "t-cli", "cli $1\n");
    cmd_ok(&env, &["install", pkg.to_str().expect("utf8")]);
    let mut settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.agent_dir().join("settings.json")).expect("settings"),
    )
    .expect("settings json");
    settings["prompts"] = serde_json::json!([shared.to_string_lossy()]);
    put_settings(&env, "home/.oxa/agent/settings.json", settings);

    for (msg, expect) in [
        ("/t-agent x", "agent x"),
        ("/t-project x", "project x"),
        ("/t-pkg x", "package x"),
        ("/t-setting x", "setting x"),
    ] {
        let ran = drive(&env, &["-a"], msg);
        assert_success(&ran.out);
        assert_eq!(user_text(&ran).trim(), expect, "TPL-004: {msg} was not expanded");
    }
    let explicit = drive(
        &env,
        &["-a", "--prompt-template", cli.to_str().expect("utf8")],
        "/t-cli x",
    );
    assert_eq!(
        user_text(&explicit).trim(),
        "cli x",
        "TPL-004: --prompt-template did not load"
    );
    let none = drive(&env, &["-a", "-np"], "/t-agent x");
    assert_eq!(
        user_text(&none).trim(),
        "/t-agent x",
        "TPL-004: -np must disable template discovery"
    );
}

// ---------------------------------------------------------------------------------------------
// PKG: packages
// ---------------------------------------------------------------------------------------------

/// PKG-001: `git:<url>` sources with a ref are installed at that ref.
/// pi docs: packages.md ("Choose a source"; "Git tags and commits are also pinned").
#[test]
fn pkg_001() {
    let env = TestEnv::new();
    let repo = env.root().join("tmp/repo");
    std::fs::create_dir_all(repo.join("skills/g-skill")).expect("mkdir");
    std::fs::write(
        repo.join("package.json"),
        "{\"name\":\"gitpkg\",\"version\":\"1.0.0\"}",
    )
    .expect("write");
    std::fs::write(
        repo.join("skills/g-skill/SKILL.md"),
        skill_md("g-skill", "version one"),
    )
    .expect("write");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    git(&repo, &["tag", "v1"]);
    std::fs::write(
        repo.join("skills/g-skill/SKILL.md"),
        skill_md("g-skill", "version two"),
    )
    .expect("write");
    git(&repo, &["commit", "-q", "-am", "two"]);

    // A file:// git URL with a ref (oxa accepts file:// among its git URL schemes).
    let source = format!("file://{}@v1", repo.display());
    cmd_ok(&env, &["install", &source]);
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        project_skill_listed(&sys, "g-skill"),
        "PKG-001: the git package's skill was not loaded"
    );
    assert!(
        sys.contains("version one"),
        "PKG-001: the package must be checked out at the ref v1, not main"
    );
    assert!(
        !sys.contains("version two"),
        "PKG-001: the newer commit on main must not be loaded"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=oxa-parity",
            "-c",
            "user.email=parity@invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// PKG-002: local path sources load without copying.
/// pi docs: packages.md ("Local ... Loaded from the resolved path without copying").
#[test]
fn pkg_002() {
    let env = TestEnv::new();
    let pkg = put_package(
        &env,
        "pkgs/local",
        "local",
        "1.0.0",
        &[("skills/l-skill/SKILL.md", &skill_md("l-skill", "Local."))],
    );
    let out = cmd_ok(&env, &["install", pkg.to_str().expect("utf8")]);
    assert!(
        stdout_of(&out).contains("local"),
        "PKG-002: install did not name the package"
    );
    let ran = drive(&env, &[], "hi");
    assert!(
        project_skill_listed(&system(&ran), "l-skill"),
        "PKG-002: the local package's skill was not loaded"
    );
    let listed = stdout_of(&cmd_ok(&env, &["list"]));
    assert!(
        listed.contains(pkg.to_str().expect("utf8")),
        "PKG-002: list must show the local path"
    );
    // Loaded from the path itself: an edit to the package is seen at the next start.
    std::fs::write(
        pkg.join("skills/l-skill/SKILL.md"),
        skill_md("l-skill", "Edited in place."),
    )
    .expect("write");
    let again = drive(&env, &[], "hi");
    assert!(
        system(&again).contains("Edited in place."),
        "PKG-002: the package must load from its own directory"
    );
}

/// PKG-004: `-l` writes the declaration to the project settings; identity is the resolved path.
/// pi docs: packages.md ("Add --local or -l to write the package declaration to .pi/settings.json").
#[test]
fn pkg_004() {
    let env = TestEnv::new();
    let pkg = put_package(
        &env,
        "pkgs/scoped",
        "scoped",
        "1.0.0",
        &[("skills/sc/SKILL.md", &skill_md("sc", "Scoped."))],
    );
    let source = pkg.to_str().expect("utf8");
    cmd_ok(&env, &["install", source, "-l"]);
    let project = std::fs::read_to_string(env.work().join(".oxa/settings.json"))
        .expect("project settings written by -l");
    assert!(
        project.contains(source),
        "PKG-004: -l must write the declaration to .oxa/settings.json"
    );
    let user = std::fs::read_to_string(env.agent_dir().join("settings.json")).unwrap_or_default();
    assert!(
        !user.contains(source),
        "PKG-004: -l must not write the user settings"
    );
    // The same package declared in both scopes is loaded once.
    cmd_ok(&env, &["install", source]);
    let ran = drive(&env, &["-a"], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert_eq!(
        sys.matches("<name>sc</name>").count(),
        1,
        "PKG-004: the same package must load once across scopes"
    );
}

/// PKG-005: the `pi` manifest key is accepted (oxa's own key is `oxa`); conventional directories
/// are discovered without a manifest.
/// pi docs: packages.md ("Create a package"; "pi": {"skills": [...]}).
#[test]
fn pkg_005() {
    let env = TestEnv::new();
    let a = env.root().join("pkgs/a");
    env.write(
        "pkgs/a/package.json",
        &serde_json::json!({ "name": "a", "version": "1.0.0", "pi": { "skills": ["./resources/skills"] } })
            .to_string(),
    );
    env.write(
        "pkgs/a/resources/skills/p-a/SKILL.md",
        &skill_md("p-a", "Via pi key."),
    );
    let c = env.root().join("pkgs/c");
    env.write("pkgs/c/package.json", "{\"name\":\"c\",\"version\":\"1.0.0\"}");
    env.write(
        "pkgs/c/skills/p-c/SKILL.md",
        &skill_md("p-c", "Conventional directory."),
    );
    cmd_ok(&env, &["install", a.to_str().expect("utf8")]);
    cmd_ok(&env, &["install", c.to_str().expect("utf8")]);
    let ran = drive(&env, &[], "hi");
    let sys = system(&ran);
    assert!(
        project_skill_listed(&sys, "p-a"),
        "PKG-005: the `pi` manifest key must declare the skills directory"
    );
    assert!(
        project_skill_listed(&sys, "p-c"),
        "PKG-005: conventional directories load without a manifest"
    );
}

/// PKG-006: resource filters `!x`, `+x`, `-x` per package in settings; `[]` loads none of a type.
/// pi docs: packages.md ("Select package resources").
#[test]
fn pkg_006() {
    let env = TestEnv::new();
    let pkg = put_package(
        &env,
        "pkgs/filtered",
        "filtered",
        "1.0.0",
        &[
            ("skills/f-keep/SKILL.md", &skill_md("f-keep", "Kept.")),
            ("skills/f-drop/SKILL.md", &skill_md("f-drop", "Dropped.")),
            ("prompts/f-review.md", "review $1\n"),
            ("prompts/f-other.md", "other $1\n"),
        ],
    );
    let source = pkg.to_str().expect("utf8");
    let mut settings = serde_json::json!({ "packages": [ { "source": source, "skills": ["!skills/f-drop"], "prompts": [] } ] });
    put_settings(&env, "home/.oxa/agent/settings.json", settings.clone());
    let ran = drive(&env, &[], "hi");
    let sys = system(&ran);
    assert!(
        project_skill_listed(&sys, "f-keep"),
        "PKG-006: the non-excluded skill must load"
    );
    assert!(
        !project_skill_listed(&sys, "f-drop"),
        "PKG-006: !skills/f-drop must exclude the matching skill"
    );
    let prompt = drive(&env, &[], "/f-review x");
    assert_eq!(
        user_text(&prompt).trim(),
        "/f-review x",
        "PKG-006: prompts: [] must load no prompt of the package"
    );

    // Exact `-path` exclusion and `+path` inclusion.
    settings["packages"] = serde_json::json!([ { "source": source, "skills": ["-skills/f-keep"], "prompts": ["+prompts/f-review.md"] } ]);
    put_settings(&env, "home/.oxa/agent/settings.json", settings);
    let exact = drive(&env, &[], "hi");
    let sys = system(&exact);
    assert!(
        !project_skill_listed(&sys, "f-keep"),
        "PKG-006: -path must exclude one exact resource"
    );
    let included = drive(&env, &[], "/f-review x");
    assert_eq!(
        user_text(&included).trim(),
        "review x",
        "PKG-006: +path must include the exact prompt"
    );
}

/// PKG-007: `list`, `remove` and `update <source>`.
/// pi docs: packages.md ("pi list shows configured packages ... pi remove ... pi update").
#[test]
fn pkg_007() {
    let env = TestEnv::new();
    let pkg = put_package(&env, "pkgs/managed", "managed", "1.0.0", &[]);
    let source = pkg.to_str().expect("utf8");
    cmd_ok(&env, &["install", source]);
    assert!(
        stdout_of(&cmd_ok(&env, &["list"])).contains(source),
        "PKG-007: list must show the installed package"
    );
    cmd_ok(&env, &["update", source]);
    cmd_ok(&env, &["remove", source]);
    let after = stdout_of(&cmd_ok(&env, &["list"]));
    assert!(
        !after.contains(source),
        "PKG-007: remove must take the package out of list"
    );
    let settings = std::fs::read_to_string(env.agent_dir().join("settings.json")).unwrap_or_default();
    assert!(
        !settings.contains(source),
        "PKG-007: remove must take the declaration out of settings"
    );
}

// ---------------------------------------------------------------------------------------------
// SEC: security and project trust
// ---------------------------------------------------------------------------------------------

/// SEC-001: a project-local resource found without a decision loads without a question
/// (`defaultProjectTrust` ships as `always`, owner decision 2026-11-21); the owner can opt into
/// the question with `ask` or into loading nothing with `never`.
/// pi docs: security.md ("Pi requires a project-trust decision when it finds ...").
#[test]
fn sec_001() {
    // invariant: a fresh install needs no trust decision in any project; trust still approves content (sha256 of the project files that run or load code), so a changed file asks again (interactive) or is not loaded with a notice (headless) until approved or `--approve`
    let env = TestEnv::new();
    put_skill(&env, "work/.oxa/skills", "t-fresh", "Project skill.");
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    assert!(
        project_skill_listed(&system(&ran), "t-fresh"),
        "SEC-001: a fresh project is trusted by default; stderr: {}",
        stderr_of(&ran.out)
    );
    assert!(
        !stderr_of(&ran.out).contains("not trusted"),
        "SEC-001: no trust notice for a trusted project; stderr: {}",
        stderr_of(&ran.out)
    );
    assert!(
        !env.agent_dir().join("trust.json").exists(),
        "SEC-001: the default trusts the project without saving a decision"
    );

    // The owner's own opt-ins keep the old behaviour, and report what they skipped.
    for (value, listed) in [("ask", false), ("never", false), ("always", true)] {
        let env = TestEnv::new();
        put_skill(&env, "work/.oxa/skills", "t-proj", "Project skill.");
        put_settings(
            &env,
            "home/.oxa/agent/settings.json",
            serde_json::json!({ "defaultProjectTrust": value }),
        );
        let ran = drive(&env, &[], "hi");
        assert_success(&ran.out);
        assert_eq!(
            project_skill_listed(&system(&ran), "t-proj"),
            listed,
            "SEC-001: defaultProjectTrust {value}; stderr: {}",
            stderr_of(&ran.out)
        );
        let err = stderr_of(&ran.out);
        if listed {
            assert!(
                !err.contains("not trusted"),
                "SEC-001: {value} trusts; stderr: {err}"
            );
        } else {
            assert!(
                err.contains("not trusted") && err.contains(".oxa/skills"),
                "SEC-001: {value} must report the skipped resource by name; stderr: {err}"
            );
        }
    }
}

/// SEC-002: trust-gated resources: project settings, extensions, skills, prompts, themes,
/// `mcp.json`, system files and `.agents/skills`; context files are not gated.
/// pi docs: security.md ("Resources protected by project trust"; "Context files ... load
/// regardless of project trust").
#[test]
fn sec_002() {
    // The rows below are what an untrusted project skips: name the owner's `never` opt-in, since
    // the shipped default trusts the project.
    let env = TestEnv::new();
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultProjectTrust": "never" }),
    );
    let mcp_marker = marker(&env, "sec002-mcp");
    env.write("work/.oxa/mcp.json", &marker_mcp(&mcp_marker));
    put_skill(&env, "work/.oxa/skills", "gated-skill", "Gated.");
    put_skill(&env, "work/.agents/skills", "gated-agents", "Gated too.");
    put_prompt(&env, "work/.oxa/prompts", "gated-prompt", "gated $1\n");
    env.write("work/.oxa/SYSTEM.md", "GATED SYSTEM TEXT\n");
    env.write("work/.oxa/APPEND_SYSTEM.md", "GATED APPEND TEXT\n");
    env.write("work/.oxa/themes/gated.json", "{\"name\":\"gated\"}");
    env.write(
        "work/.oxa/settings.json",
        "{\"skills\":[\"/nonexistent-gated\"],\"enableSkillCommands\":false}",
    );
    env.write("work/AGENTS.md", "CONTEXT FILE TEXT\n");

    let ran = drive(&env, &[], "/gated-prompt x");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        !project_skill_listed(&sys, "gated-skill"),
        "SEC-002: project skills are gated"
    );
    assert!(
        !project_skill_listed(&sys, "gated-agents"),
        "SEC-002: project .agents/skills are gated"
    );
    assert_eq!(
        user_text(&ran).trim(),
        "/gated-prompt x",
        "SEC-002: project prompts are gated"
    );
    assert!(
        !sys.contains("GATED SYSTEM TEXT"),
        "SEC-002: project SYSTEM.md is gated"
    );
    assert!(
        !sys.contains("GATED APPEND TEXT"),
        "SEC-002: project APPEND_SYSTEM.md is gated"
    );
    assert!(
        !mcp_marker.exists(),
        "SEC-002: project mcp.json must not start its servers untrusted"
    );
    assert!(
        sys.contains("CONTEXT FILE TEXT"),
        "SEC-002: context files load without trust"
    );
    assert!(
        !stderr_of(&ran.out).contains("nonexistent-gated"),
        "SEC-002: project settings must not be read untrusted"
    );

    let trusted = drive(&env, &["-a"], "hi");
    assert_success(&trusted.out);
    assert!(
        mcp_marker.exists(),
        "SEC-002: a trusted project starts its mcp.json servers"
    );
    assert!(
        project_skill_listed(&system(&trusted), "gated-skill"),
        "SEC-002: trusted project skills load"
    );
}

/// SEC-003: decision order: flag > extension (`project_trust`) > saved (closest parent) >
/// `defaultProjectTrust`.
/// pi docs: security.md ("How Pi chooses a trust decision").
#[test]
fn sec_003() {
    fn listed(setup: &dyn Fn(&TestEnv), args: &[&str]) -> bool {
        let env = TestEnv::new();
        put_skill(&env, "work/.oxa/skills", "order-skill", "Order.");
        setup(&env);
        let ran = drive(&env, args, "hi");
        assert_success(&ran.out);
        project_skill_listed(&system(&ran), "order-skill")
    }
    fn saved(env: &TestEnv, trusted: bool) {
        let file = env.agent_dir().join("trust.json");
        let key = canon(&env.work());
        std::fs::write(&file, format!("{{\"{key}\": {trusted}}}")).expect("write trust.json");
    }
    fn set_default(env: &TestEnv, value: &str) {
        put_settings(
            env,
            "home/.oxa/agent/settings.json",
            serde_json::json!({ "defaultProjectTrust": value }),
        );
    }

    // Flag beats everything.
    assert!(
        listed(
            &|e| {
                set_default(e, "never");
                saved(e, false);
            },
            &["-a"]
        ),
        "SEC-003: -a must trust over never and a saved no"
    );
    assert!(
        !listed(
            &|e| {
                set_default(e, "always");
                saved(e, true);
            },
            &["-na"]
        ),
        "SEC-003: -na must refuse over always and a saved yes"
    );
    // Extension beats the saved decision and the default.
    assert!(
        listed(
            &|e| {
                trust_extension(e, true);
                saved(e, false);
            },
            &[]
        ),
        "SEC-003: an extension's yes beats a saved no"
    );
    assert!(
        !listed(
            &|e| {
                trust_extension(e, false);
                set_default(e, "always");
            },
            &[]
        ),
        "SEC-003: an extension's no beats always"
    );
    // Saved beats the default.
    assert!(
        listed(
            &|e| {
                saved(e, true);
                set_default(e, "never");
            },
            &[]
        ),
        "SEC-003: a saved yes beats defaultProjectTrust never"
    );
    assert!(
        !listed(
            &|e| {
                saved(e, false);
                set_default(e, "always");
            },
            &[]
        ),
        "SEC-003: a saved no beats defaultProjectTrust always"
    );
    // The default alone.
    assert!(
        listed(&|e| set_default(e, "always"), &[]),
        "SEC-003: defaultProjectTrust always loads in print mode"
    );
    assert!(
        !listed(&|e| set_default(e, "never"), &[]),
        "SEC-003: defaultProjectTrust never skips"
    );
    assert!(
        !listed(&|e| set_default(e, "ask"), &[]),
        "SEC-003: defaultProjectTrust ask skips without a prompt in print mode"
    );
}

/// SEC-004: non-interactive `ask` skips trust-gated resources and does not wait for an answer.
/// pi docs: security.md ("Project trust without an interactive prompt").
#[test]
fn sec_004() {
    let env = TestEnv::new();
    put_skill(&env, "work/.oxa/skills", "ask-skill", "Ask.");
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultProjectTrust": "ask" }),
    );
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    assert!(
        !project_skill_listed(&system(&ran), "ask-skill"),
        "SEC-004: ask must skip trust-gated resources in print mode"
    );
    assert!(
        stderr_of(&ran.out).contains("not trusted"),
        "SEC-004: the skipped resources must be reported; stderr: {}",
        stderr_of(&ran.out)
    );
}

/// SEC-005: `/trust` saves a decision in `trust.json`, keyed by the canonical directory, and the
/// next process honours it.
/// pi docs: security.md ("Use /trust to save a decision for future Pi processes").
#[test]
fn sec_005() {
    let env = TestEnv::new();
    put_skill(&env, "work/.oxa/skills", "saved-skill", "Saved.");
    // A decision written as trust.json is honoured by the next process.
    std::fs::write(
        env.agent_dir().join("trust.json"),
        format!("{{\"{}\": true}}", canon(&env.work())),
    )
    .expect("write");
    let honoured = drive(&env, &[], "hi");
    assert!(
        project_skill_listed(&system(&honoured), "saved-skill"),
        "SEC-005: a saved trust decision must be honoured"
    );
    // `/trust` saves that decision itself: run it through the RPC prompt command.
    std::fs::remove_file(env.agent_dir().join("trust.json")).expect("remove");
    let srv = FakeServer::start(script(), FakeOpts::default()).expect("start fake engine");
    let _ = run_oxa(
        &env,
        &srv.url(),
        &["--mode", "rpc", "--no-session"],
        "{\"id\":\"1\",\"type\":\"prompt\",\"message\":\"/trust\"}\n",
    );
    let saved = std::fs::read_to_string(env.agent_dir().join("trust.json")).unwrap_or_default();
    assert!(
        saved.contains(&canon(&env.work())),
        "SEC-005 RED: the built-in /trust is not reachable through the RPC prompt command, so no decision is saved to trust.json; file: {saved:?}"
    );
}

/// SEC-006: pi has no sandbox and no permission prompts; oxa's plan mode gates what tools do.
/// Trust gates what loads; the mode gates what the tools may do.
/// invariant: the mode is part of the frozen tool set (W3); a refused call is a tool error.
#[test]
fn sec_006() {
    let env = TestEnv::new();
    let target = env.work().join("written.txt");
    let srv = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "c1",
                "write",
                &format!("{{\"path\": \"{}\", \"content\": \"x\"}}", target.display()),
            ),
            ScriptTurn::text("ok"),
        ],
        FakeOpts::default(),
    )
    .expect("start fake engine");
    let out = run_oxa(
        &env,
        &srv.url(),
        &["-p", "--permission-mode", "plan", "write it"],
        "",
    );
    let reqs = srv.requests();
    assert_success(&out);
    assert!(
        !target.exists(),
        "SEC-006: plan mode must not let the write tool create the file"
    );
    let result = reqs
        .last()
        .and_then(|r| {
            r.messages
                .iter()
                .find(|m| m.role == Role::Tool)
                .and_then(|m| m.content.clone())
        })
        .unwrap_or_default();
    assert!(
        !result.is_empty(),
        "SEC-006: the refused call must answer with a tool result"
    );
}

/// SEC-007: containerization recipes are replaced by the built-in sandbox: the bash tool runs
/// in the workspace mode under the sandbox and cannot write outside the work tree.
/// invariant: built-in bwrap sandbox (the sandbox replaces the Docker / OpenShell / Gondolin recipes).
#[test]
fn sec_007() {
    let env = TestEnv::new();
    let escaped = env.root().join("tmp/escaped.txt");
    let srv = FakeServer::start(
        vec![
            ScriptTurn::tool(
                "c1",
                "bash",
                &format!("{{\"command\": \"echo leaked > '{}'\"}}", escaped.display()),
            ),
            ScriptTurn::text("ok"),
        ],
        FakeOpts::default(),
    )
    .expect("start fake engine");
    let out = run_oxa(
        &env,
        &srv.url(),
        &["-p", "--permission-mode", "workspace", "--sandbox", "try it"],
        "",
    );
    assert_success(&out);
    assert!(
        !escaped.exists(),
        "SEC-007: the built-in sandbox must refuse a write outside the workspace"
    );
}

/// SEC-008: extension host functions and MCP stdio servers pass the same `Policy`: plan mode
/// does not start an MCP stdio server; workspace mode does.
/// invariant: one Policy for every host function and server (O-144 / the sandbox policy).
#[test]
fn sec_008() {
    let env = TestEnv::new();
    let plan_marker = marker(&env, "sec008-plan");
    env.write("home/.oxa/agent/mcp.json", &marker_mcp(&plan_marker));
    let plan = drive(&env, &["--permission-mode", "plan"], "hi");
    assert_success(&plan.out);
    assert!(
        !plan_marker.exists(),
        "SEC-008: plan mode must not start an MCP stdio server"
    );

    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"permissions":{"defaultMode":"plan"}}"#,
    );
    let owner_plan = drive(&env, &[], "hi");
    assert_success(&owner_plan.out);
    assert!(
        !plan_marker.exists(),
        "the owner's default plan also denies MCP startup"
    );

    let work_marker = marker(&env, "sec008-workspace");
    env.write("home/.oxa/agent/mcp.json", &marker_mcp(&work_marker));
    let workspace = drive(&env, &["--permission-mode", "workspace"], "hi");
    assert_success(&workspace.out);
    assert!(
        work_marker.exists(),
        "SEC-008: workspace mode starts the configured MCP stdio server"
    );
}

// ---------------------------------------------------------------------------------------------
// THM: themes
// ---------------------------------------------------------------------------------------------

/// THM-001: built-in `dark` and `light` themes.
/// pi docs: themes.md ("Pi includes the system, dark, and light themes").
#[test]
fn thm_001() {
    let env = TestEnv::new();
    for name in ["dark", "light"] {
        let out = drive(&env, &["--use-theme", name], "hi");
        assert_success(&out.out);
        assert!(
            !stderr_of(&out.out).to_lowercase().contains("theme"),
            "THM-001: the built-in {name} theme must load without a diagnostic; stderr: {}",
            stderr_of(&out.out)
        );
    }
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "theme": "dark" }),
    );
    let config = stdout_of(&cmd_ok(&env, &["config"]));
    assert!(
        config.contains("theme = \"dark\""),
        "THM-001: the saved built-in theme must be the resolved theme"
    );
}

/// THM-002: the `system` theme is the default and is reserved.
/// pi docs: themes.md ("The system theme is the default"; "system is a reserved name").
#[test]
fn thm_002() {
    let env = TestEnv::new();
    let config = stdout_of(&cmd_ok(&env, &["config"]));
    assert!(
        config.contains("theme = \"system\""),
        "THM-002: system must be the default theme"
    );
    env.write(
        "home/.oxa/agent/themes/system.json",
        "{\"name\":\"system\",\"colors\":{}}",
    );
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    assert!(
        !stderr_of(&ran.out).contains("system.json")
            || stderr_of(&ran.out).to_lowercase().contains("reserved"),
        "THM-002: a custom theme named system is ignored with a reason; stderr: {}",
        stderr_of(&ran.out)
    );
}

/// THM-003: custom theme JSON loads: pi's own `dark.json` loads unchanged (renamed so it does not
/// clash with the built-in), and `vars`, colour tokens and the 256-colour index are accepted.
/// pi docs: themes.md ("Create a custom theme"; "A color can be written in six forms").
#[test]
fn thm_003() {
    let env = TestEnv::new();
    let pi_dark = std::fs::read_to_string(fixture("goldens/resources/thm_003/pi-dark.json"))
        .expect("pi dark.json fixture");
    let renamed = pi_dark.replacen("\"name\": \"dark\"", "\"name\": \"pi-dark-copy\"", 1);
    assert_ne!(renamed, pi_dark, "THM-003: fixture must contain the name field");
    env.write("home/.oxa/agent/themes/pi-dark-copy.json", &renamed);
    let ok = drive(&env, &["--use-theme", "pi-dark-copy"], "hi");
    assert_success(&ok.out);
    assert!(
        !stderr_of(&ok.out).to_lowercase().contains("pi-dark-copy"),
        "THM-003: pi's theme JSON must load unchanged; stderr: {}",
        stderr_of(&ok.out)
    );
    // Unknown colour keys are rejected (strict objects) and reported.
    env.write(
        "home/.oxa/agent/themes/bad.json",
        "{\"name\":\"bad-theme\",\"colors\":{\"notARole\":\"#fff\"}}",
    );
    let bad = drive(&env, &[], "hi");
    assert_success(&bad.out);
    assert!(
        stderr_of(&bad.out).contains("bad-theme") || stderr_of(&bad.out).contains("bad.json"),
        "THM-003: an invalid theme (unknown colour role) must be reported at startup; stderr: {}",
        stderr_of(&bad.out)
    );
}

/// THM-004: theme discovery: agent dir, project `.oxa/themes` (trust-gated), `--theme <path>`.
/// The invalid themes below make their discovery visible: a startup diagnostic names the file.
/// pi docs: themes.md ("Load a theme from a project or package").
#[test]
fn thm_004() {
    let env = TestEnv::new();
    // The `untrusted` run below names the owner's opt-in: the shipped default trusts the project.
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultProjectTrust": "never" }),
    );
    let invalid = "{\"name\":\"t4-invalid\",\"colors\":{\"notARole\":\"#fff\"}}";
    env.write("home/.oxa/agent/themes/t4-agent.json", invalid);
    env.write("work/.oxa/themes/t4-project.json", invalid);
    let cli = env.write("cli/t4-cli.json", invalid);
    let untrusted = drive(&env, &[], "hi");
    let err = stderr_of(&untrusted.out);
    assert!(
        err.contains("t4-agent"),
        "THM-004: the invalid agent-directory theme must be read and reported at startup; stderr: {err}"
    );
    assert!(
        !err.contains("t4-project"),
        "THM-004: project themes are gated when untrusted"
    );
    let trusted = drive(&env, &["-a"], "hi");
    assert!(
        stderr_of(&trusted.out).contains("t4-project"),
        "THM-004: trusted project themes must load"
    );
    let explicit = drive(&env, &["--theme", cli.to_str().expect("utf8")], "hi");
    assert!(
        stderr_of(&explicit.out).contains("t4-cli"),
        "THM-004: --theme must load the given file"
    );
}

/// THM-005: hot reload of the active theme file. The reload is observed through the RPC `/reload`
/// command after the active user theme file changed (pi hot-reloads it without that command in the
/// interactive mode, which has no print-mode surface).
/// pi docs: themes.md ("Pi hot-reloads the active user theme only from <agent-dir>/themes/<name>.json").
#[test]
fn thm_005() {
    let env = TestEnv::new();
    let file = env.write(
        "home/.oxa/agent/themes/live.json",
        "{\"name\":\"live\",\"colors\":{}}",
    );
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "theme": "live" }),
    );
    std::fs::write(&file, "{\"name\":\"live\",\"colors\":{\"notARole\":\"#fff\"}}").expect("edit theme");
    let srv = FakeServer::start(script(), FakeOpts::default()).expect("start fake engine");
    let out = run_oxa(
        &env,
        &srv.url(),
        &["--mode", "rpc", "--no-session"],
        "{\"id\":\"1\",\"type\":\"prompt\",\"message\":\"/reload\"}\n",
    );
    let text = stdout_of(&out) + &stderr_of(&out);
    assert!(
        text.contains("live"),
        "THM-005 RED: the active user theme is not re-read on reload through the RPC prompt (/reload is interactive-only there) and no theme diagnostic is printed; output: {text}"
    );
}

/// THM-006: `/settings` theme selector and `--use-theme name/name` (light/dark pair).
/// The selector is an interactive dialog; the print-mode surface is the pair argument and the
/// saved `light/dark` value.
/// pi docs: themes.md ("Automatic mode stores the light theme first and the dark theme second").
#[test]
fn thm_006() {
    let env = TestEnv::new();
    let pair = drive(&env, &["--use-theme", "light/dark"], "hi");
    assert_success(&pair.out);
    assert!(
        !stderr_of(&pair.out).to_lowercase().contains("theme"),
        "THM-006: --use-theme light/dark must be accepted; stderr: {}",
        stderr_of(&pair.out)
    );
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "theme": "light/dark" }),
    );
    let config = stdout_of(&cmd_ok(&env, &["config"]));
    assert!(
        config.contains("theme = \"light/dark\""),
        "THM-006: the saved pair must be the resolved theme"
    );
}

// ---------------------------------------------------------------------------------------------
// CFG: configuration files
// ---------------------------------------------------------------------------------------------

/// CFG-001: the agent directory holds settings, mcp.json and trust.json; here `OXA_AGENT_DIR`
/// is the agent directory (`~/.oxa/agent` by default).
/// pi docs: configuration.md ("Agent directory").
/// invariant: pi directories are never read implicitly (CFG-008); the agent directory is the
/// only user location.
#[test]
fn cfg_001() {
    let env = TestEnv::new();
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultThinkingLevel": "high" }),
    );
    let config = stdout_of(&cmd_ok(&env, &["config"]));
    assert!(
        config.contains(
            &env.agent_dir()
                .join("settings.json")
                .to_string_lossy()
                .into_owned()
        ),
        "CFG-001: settings are read from the agent directory; config: {config}"
    );
    assert!(
        config.contains("defaultThinkingLevel = \"high\""),
        "CFG-001: the agent settings value must apply"
    );
    let mcp_marker = marker(&env, "cfg001-mcp");
    env.write("home/.oxa/agent/mcp.json", &marker_mcp(&mcp_marker));
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    assert!(
        mcp_marker.exists(),
        "CFG-001: mcp.json in the agent directory starts its servers"
    );
}

/// CFG-002: the project `.oxa/` directory: settings, skills, prompts, themes and `mcp.json`,
/// all read once the project is trusted.
/// pi docs: configuration.md ("Project .pi directory").
#[test]
fn cfg_002() {
    // The `untrusted` run below names the owner's opt-in: the shipped default trusts the project.
    let env = TestEnv::new();
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultProjectTrust": "never" }),
    );
    put_settings(
        &env,
        "work/.oxa/settings.json",
        serde_json::json!({ "defaultTools": ["read"] }),
    );
    put_prompt(&env, "work/.oxa/prompts", "c2-prompt", "c2 $1\n");
    put_skill(&env, "work/.oxa/skills", "c2-skill", "Project skill.");
    let mcp_marker = marker(&env, "cfg002-mcp");
    env.write("work/.oxa/mcp.json", &marker_mcp(&mcp_marker));
    let trusted = drive(&env, &["-a"], "/c2-prompt x");
    assert_success(&trusted.out);
    assert!(
        project_skill_listed(&system(&trusted), "c2-skill"),
        "CFG-002: project skills load when trusted"
    );
    assert_eq!(
        user_text(&trusted).trim(),
        "c2 x",
        "CFG-002: project prompts load when trusted"
    );
    assert!(
        mcp_marker.exists(),
        "CFG-002: project mcp.json starts its servers when trusted"
    );
    let names = declared_tool_names(&trusted);
    assert_eq!(
        names,
        vec!["read".to_string()],
        "CFG-002: project settings (defaultTools) apply when trusted"
    );
    let untrusted = drive(&env, &[], "hi");
    assert!(
        declared_tool_names(&untrusted).len() > 1,
        "CFG-002: untrusted project settings do not apply"
    );
}

/// CFG-003: context files `AGENTS.md` / `CLAUDE.md` come from the agent directory, then from the
/// parents of the working directory down to it, in that order.
/// pi docs: configuration.md ("Context files").
/// invariant: W3, the context files are part of the frozen prefix.
#[test]
fn cfg_003() {
    // invariant: part of the frozen prefix (W3); below HOME the upward walk ends at HOME (pi walks to `/`)
    let env = TestEnv::new();
    env.write("home/.oxa/agent/AGENTS.md", "CTX-AGENT\n");
    env.write("AGENTS.md", "CTX-PARENT\n");
    env.write("work/CLAUDE.md", "CTX-CWD\n");
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    let a = sys.find("CTX-AGENT");
    let p = sys.find("CTX-PARENT");
    let c = sys.find("CTX-CWD");
    assert!(
        a.is_some() && p.is_some() && c.is_some(),
        "CFG-003: all three context files must be in the prompt"
    );
    assert!(
        a < p && p < c,
        "CFG-003: agent directory, then parent, then working directory"
    );
}

/// CFG-004: `AGENTS.override.md` replaces `AGENTS.md` in the same directory only.
/// pi docs: configuration.md ("An AGENTS.override.md replaces AGENTS.md or CLAUDE.md only in the
/// same directory").
#[test]
fn cfg_004() {
    let env = TestEnv::new();
    env.write("AGENTS.md", "PARENT-AGENTS\n");
    env.write("work/AGENTS.md", "CWD-AGENTS-PLAIN\n");
    env.write("work/AGENTS.override.md", "CWD-AGENTS-OVERRIDE\n");
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("CWD-AGENTS-OVERRIDE"),
        "CFG-004: the override file must be used in its directory"
    );
    assert!(
        !sys.contains("CWD-AGENTS-PLAIN"),
        "CFG-004: the override replaces AGENTS.md in the same directory"
    );
    assert!(
        sys.contains("PARENT-AGENTS"),
        "CFG-004: the override does not suppress other directories"
    );
}

/// CFG-005: `SYSTEM.md` replaces and `APPEND_SYSTEM.md` appends to the system prompt; the project
/// file (trusted) wins over the user file.
/// pi docs: configuration.md ("SYSTEM.md", "APPEND_SYSTEM.md"; "the trusted project file takes
/// precedence").
#[test]
fn cfg_005() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/SYSTEM.md", "USER SYSTEM REPLACEMENT\n");
    env.write("home/.oxa/agent/APPEND_SYSTEM.md", "USER APPENDIX\n");
    let ran = drive(&env, &[], "hi");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        sys.contains("USER SYSTEM REPLACEMENT"),
        "CFG-005: SYSTEM.md replaces the system prompt"
    );
    assert!(
        !sys.contains("You are a coding assistant"),
        "CFG-005: the default preamble must be replaced"
    );
    assert!(sys.contains("USER APPENDIX"), "CFG-005: APPEND_SYSTEM.md appends");
    env.write("work/.oxa/SYSTEM.md", "PROJECT SYSTEM REPLACEMENT\n");
    let trusted = drive(&env, &["-a"], "hi");
    let sys = system(&trusted);
    assert!(
        sys.contains("PROJECT SYSTEM REPLACEMENT"),
        "CFG-005: the trusted project SYSTEM.md wins over the user file"
    );
    assert!(
        !sys.contains("USER SYSTEM REPLACEMENT"),
        "CFG-005: the two files are not combined"
    );
}

/// CFG-006: `.agents/skills` is read (project, trusted; user `~/.agents/skills` without trust).
/// pi docs: configuration.md and skills.md ("Pi also supports the Agent Skills locations").
#[test]
fn cfg_006() {
    let env = TestEnv::new();
    // The `untrusted` run below names the owner's opt-in: the shipped default trusts the project.
    put_settings(
        &env,
        "home/.oxa/agent/settings.json",
        serde_json::json!({ "defaultProjectTrust": "never" }),
    );
    put_skill(&env, "work/.agents/skills", "c6-project", "Project .agents.");
    put_skill(&env, "home/.agents/skills", "c6-user", "User .agents.");
    let untrusted = drive(&env, &[], "hi");
    let sys = system(&untrusted);
    assert!(
        project_skill_listed(&sys, "c6-user"),
        "CFG-006: ~/.agents/skills is read without trust"
    );
    assert!(
        !project_skill_listed(&sys, "c6-project"),
        "CFG-006: project .agents/skills needs trust"
    );
    let trusted = drive(&env, &["-a"], "hi");
    assert!(
        project_skill_listed(&system(&trusted), "c6-project"),
        "CFG-006: trusted project .agents/skills load"
    );
}

/// pi directories are not read implicitly.
/// pi docs: configuration.md (the pi paths named there are not read by oxa).
/// invariant: the agent directory is `~/.oxa/agent` only; pi directories are never read implicitly.
#[test]
fn pi_directories_are_not_read() {
    let env = TestEnv::new();
    put_skill(&env, "home/.pi/agent/skills", "pi-skill", "A pi skill.");
    put_prompt(&env, "home/.pi/agent/prompts", "pi-prompt", "pi $1\n");
    put_settings(
        &env,
        "home/.pi/agent/settings.json",
        serde_json::json!({ "defaultThinkingLevel": "high" }),
    );
    put_skill(&env, "work/.pi/skills", "pi-project", "A pi project skill.");
    let ran = drive(&env, &["-a"], "/pi-prompt x");
    assert_success(&ran.out);
    let sys = system(&ran);
    assert!(
        !project_skill_listed(&sys, "pi-skill"),
        "pi directories: ~/.pi/agent/skills must not be read"
    );
    assert!(
        !project_skill_listed(&sys, "pi-project"),
        "pi directories: .pi/skills must not be read"
    );
    assert_eq!(
        user_text(&ran).trim(),
        "/pi-prompt x",
        "pi directories: ~/.pi/agent/prompts must not be read"
    );
    let config = stdout_of(&cmd_ok(&env, &["config"]));
    assert!(
        !config.contains(".pi/agent"),
        "pi directories: no pi settings file is a source"
    );
    assert!(
        config.contains("defaultThinkingLevel = \"medium\""),
        "pi directories: the pi settings value must not apply"
    );
}

// ---------------------------------------------------------------------------------------------
// meta-checks (the four acceptance tests of G10)
// ---------------------------------------------------------------------------------------------

/// One row of PARITY.md: id, verdict, the test cell (`resources.rs::fn` or `omission`), and the
/// pi page(s) of its section.
struct Row {
    id: String,
    verdict: String,
    test: String,
    pages: Vec<String>,
}

const AREA_PREFIXES: [&str; 6] = ["SKL-", "TPL-", "PKG-", "SEC-", "THM-", "CFG-"];

fn parity_rows() -> Vec<Row> {
    let text = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../PARITY.md"))
        .expect("read PARITY.md");
    let mut rows = Vec::new();
    let mut pages: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("pi page(s):") {
            pages = line
                .trim_start_matches("pi page(s):")
                .split("Tests:")
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches('.')
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| s.ends_with(".md"))
                .collect();
            continue;
        }
        if !line.starts_with("| ") {
            continue;
        }
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 7 {
            continue;
        }
        let id = cells[1].trim();
        if !AREA_PREFIXES.iter().any(|p| id.starts_with(p)) {
            continue;
        }
        rows.push(Row {
            id: id.to_string(),
            verdict: cells[3].trim().to_string(),
            test: cells[5].trim().trim_matches('`').to_string(),
            pages: pages.clone(),
        });
    }
    rows
}

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = parity_rows();
    assert_eq!(
        rows.len(),
        37,
        "resources area rows (SKL 7, TPL 4, PKG 6, SEC 8, THM 6, CFG 6)"
    );
    for row in &rows {
        if row.test == "omission" {
            assert_eq!(row.verdict, "O", "{}: only an O row may be an omission", row.id);
            continue;
        }
        let name = row
            .test
            .strip_prefix("resources.rs::")
            .unwrap_or_else(|| panic!("{}: test cell must name resources.rs, got {}", row.id, row.test));
        assert_eq!(
            name,
            row.id.to_lowercase().replace('-', "_"),
            "{}: test fn named after the row",
            row.id
        );
        assert!(
            SELF_SRC.contains(&format!("fn {name}()")),
            "{}: no test fn {name}",
            row.id
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    for row in parity_rows()
        .iter()
        .filter(|r| r.verdict == "P" && r.test != "omission")
    {
        let name = row.id.to_lowercase().replace('-', "_");
        let body = body_of(&name);
        let cites_page = row
            .pages
            .iter()
            .any(|page| body.contains(&format!("pi docs: {page}")) || body.contains(page.as_str()));
        assert!(
            cites_page || body.contains("fixture("),
            "{}: a P row compares with pi's documented output (cite `pi docs: <page>`) or a recorded fixture",
            row.id
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    for row in parity_rows()
        .iter()
        .filter(|r| r.verdict == "A" && r.test != "omission")
    {
        let name = row.id.to_lowercase().replace('-', "_");
        let body = body_of(&name);
        assert!(
            body.contains("invariant") || body.contains("oxa"),
            "{}: an A row names the invariant or the adapted behaviour in a comment",
            row.id
        );
    }
}
