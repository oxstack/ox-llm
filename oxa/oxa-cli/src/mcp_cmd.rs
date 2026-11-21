//! E9: `oxa mcp add|remove|list|login|logout`.
// ported from pi packages/coding-agent/src/cli (mcp commands; docs: cli.md "MCP commands", mcp.md)
use oxa_config::Dirs;
use oxa_mcp::{
    add_server, entry_from_add_options, load_config, parse_server, project_config_path, remove_server,
    same_server, user_config_path, AddOptions, ConfigScope, ManagerOpts, McpConfig, McpManager, ServerConfig,
    ServerKind, ServerState, ServerStatus, TokenStore, AUTH_FILE,
};
use oxa_proto::{codes, OxaError, Value};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE_HINT: &str = "oxa mcp add|remove|list|login|logout (see cli.md, MCP commands)";

/// Run an `oxa mcp ..` subcommand with pi's syntax and output (cli.md): `add <server>
/// [--env K=V].. [--cwd dir] [-l] -- <command> [args..]`, `add <server> --url <url> [--header
/// K=V].. [--bearer-token-env-var NAME]`, `remove <server> [-l]`, `list [--json]` (exit 1
/// when an entry is invalid or an enabled server is not connected), `login <server> [-l]
/// [--timeout s]`, `logout <server> [-l]`. `-l` = the project file. Edits keep unrelated
/// content of `mcp.json`. Returns the exit code.
pub fn run_mcp(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let Some((verb, rest)) = words.split_first() else {
        return Err(usage("missing mcp command"));
    };
    match verb.as_str() {
        "add" => cmd_add(rest, dirs, cwd, out),
        "remove" => cmd_remove(rest, dirs, cwd, out),
        "list" => cmd_list(rest, dirs, cwd, out, err),
        "login" => cmd_sign_in(rest, dirs, cwd, true, out, err),
        "logout" => cmd_sign_in(rest, dirs, cwd, false, out, err),
        other => Err(usage(&format!("unknown mcp command \"{other}\""))),
    }
}

fn usage(message: &str) -> OxaError {
    OxaError::new(codes::USAGE, message).with_hint(USAGE_HINT)
}

fn say(out: &mut dyn Write, line: &str) -> Result<(), OxaError> {
    writeln!(out, "{line}").map_err(|e| OxaError::io("write output", &e))
}

/// `-l` names the project file; otherwise the user file.
fn target_file(dirs: &Dirs, cwd: &Path, local: bool) -> PathBuf {
    if local {
        project_config_path(cwd)
    } else {
        user_config_path(&dirs.agent_dir)
    }
}

/// The value after the flag at `words[*i]`; advances `*i` onto it.
fn take_value<'a>(words: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str, OxaError> {
    *i += 1;
    words
        .get(*i)
        .map(String::as_str)
        .ok_or_else(|| usage(&format!("{flag} needs a value")))
}

fn key_value(text: &str) -> Result<(String, String), OxaError> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(usage(&format!("expected KEY=VALUE, got \"{text}\""))),
    }
}

fn port(text: &str) -> Result<u16, OxaError> {
    text.parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| usage(&format!("\"{text}\" is not a port number")))
}

/// `oxa mcp add`: the parsed options, the `-l` flag and `--oauth-client-name` (which
/// `AddOptions` does not carry; it is added to the entry afterwards).
struct AddArgs {
    name: String,
    local: bool,
    options: AddOptions,
    oauth_client_name: Option<String>,
}

fn parse_add(words: &[String]) -> Result<AddArgs, OxaError> {
    let mut options = AddOptions::default();
    let mut name = None;
    let mut local = false;
    let mut oauth_client_name = None;
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        match word {
            "--" => {
                options.command = words[i + 1..].to_vec();
                break;
            }
            "-l" | "--local" => local = true,
            "--env" => options.env.push(key_value(take_value(words, &mut i, word)?)?),
            "--cwd" => options.cwd = Some(take_value(words, &mut i, word)?.to_string()),
            "--url" => options.url = Some(take_value(words, &mut i, word)?.to_string()),
            "--header" => options.headers.push(key_value(take_value(words, &mut i, word)?)?),
            "--bearer-token-env-var" => {
                options.bearer_token_env_var = Some(take_value(words, &mut i, word)?.to_string())
            }
            "--oauth-client-id" => {
                options.oauth_client_id = Some(take_value(words, &mut i, word)?.to_string())
            }
            "--oauth-client-secret" => {
                options.oauth_client_secret = Some(take_value(words, &mut i, word)?.to_string())
            }
            "--oauth-callback-port" => {
                options.oauth_callback_port = Some(port(take_value(words, &mut i, word)?)?)
            }
            "--oauth-client-name" => oauth_client_name = Some(take_value(words, &mut i, word)?.to_string()),
            "--oauth-client-registration" => {
                options.oauth_client_registration = Some(take_value(words, &mut i, word)?.to_string())
            }
            "--exposure" => options.exposure = Some(take_value(words, &mut i, word)?.to_string()),
            "--description" => options.description = Some(take_value(words, &mut i, word)?.to_string()),
            other if other.starts_with('-') => return Err(usage(&format!("unknown option \"{other}\""))),
            other => {
                if name.replace(other.to_string()).is_some() {
                    return Err(usage(&format!("unexpected argument \"{other}\"")));
                }
            }
        }
        i += 1;
    }
    let name = name.ok_or_else(|| usage("add needs a server name"))?;
    Ok(AddArgs {
        name,
        local,
        options,
        oauth_client_name,
    })
}

fn cmd_add(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let args = parse_add(words)?;
    let mut entry = entry_from_add_options(&args.name, &args.options)?;
    if let Some(client_name) = &args.oauth_client_name {
        let Value::Object(map) = &mut entry else {
            return Err(usage("--oauth-client-name applies to --url servers only"));
        };
        if !map.contains_key("url") {
            return Err(usage("--oauth-client-name applies to --url servers only"));
        }
        let oauth = map
            .entry("oauth")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(oauth) = oauth {
            oauth.insert("clientName".into(), Value::String(client_name.clone()));
        }
        parse_server(&args.name, &entry, ConfigScope::User).map_err(|m| usage(&m))?;
    }
    let file = target_file(dirs, cwd, args.local);
    let existing = load_config(&file, None);
    if let Some(other) = existing
        .servers
        .iter()
        .find(|s| s.name != args.name && same_server(&s.name, &args.name))
    {
        return Err(usage(&format!(
            "server \"{}\" conflicts with \"{}\" in {}",
            args.name,
            other.name,
            file.display()
        )));
    }
    add_server(&file, &args.name, &entry)?;
    say(
        out,
        &format!("Saved MCP server \"{}\" in {}.", args.name, file.display()),
    )?;
    say(out, "Run `oxa mcp list` to check the server.")?;
    Ok(0)
}

fn cmd_remove(words: &[String], dirs: &Dirs, cwd: &Path, out: &mut dyn Write) -> Result<i32, OxaError> {
    let (name, local) = parse_name_and_local(words, "remove")?;
    let file = target_file(dirs, cwd, local);
    if !remove_server(&file, &name)? {
        return Err(OxaError::new(
            codes::NOT_FOUND,
            format!("no MCP server \"{name}\" in {}", file.display()),
        ));
    }
    say(
        out,
        &format!(
            "Removed MCP server \"{name}\" from {}. Stored OAuth credentials are kept.",
            file.display()
        ),
    )?;
    Ok(0)
}

/// `<server> [-l]` for `remove`, `login` and `logout` without `--timeout`.
fn parse_name_and_local(words: &[String], verb: &str) -> Result<(String, bool), OxaError> {
    let mut name = None;
    let mut local = false;
    for word in words {
        match word.as_str() {
            "-l" | "--local" => local = true,
            other if other.starts_with('-') => {
                return Err(usage(&format!("unknown option \"{other}\" for {verb}")))
            }
            other => {
                if name.replace(other.to_string()).is_some() {
                    return Err(usage(&format!("unexpected argument \"{other}\"")));
                }
            }
        }
    }
    let name = name.ok_or_else(|| usage(&format!("{verb} needs a server name")))?;
    if !oxa_mcp::valid_server_name(&name) {
        return Err(usage(&format!("invalid server name \"{name}\"")));
    }
    Ok((name, local))
}

/// `oxa mcp list [--json]` (pi mcp.md "Check servers"): reads the user file and, when the
/// project is trusted (a saved trust decision for `cwd`), the project file; connects to every
/// enabled server and prints its state, tools and errors. Exit 1 when an entry is invalid or an
/// enabled server is not connected.
fn cmd_list(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let mut as_json = false;
    for word in words {
        match word.as_str() {
            "--json" => as_json = true,
            other => return Err(usage(&format!("unexpected argument \"{other}\" for list"))),
        }
    }
    let user_file = user_config_path(&dirs.agent_dir);
    let project_file = project_config_path(cwd);
    let trusted = crate::approval::saved_project_resources(dirs, cwd, err)?;
    let config = load_config(&user_file, trusted.then_some(project_file.as_path()));
    if !trusted && project_file.exists() {
        writeln!(
            err,
            "{} was not read: the project is not trusted (run oxa once in it to decide)",
            project_file.display()
        )
        .map_err(|e| OxaError::io("write output", &e))?;
    }
    for diagnostic in &config.diagnostics {
        writeln!(err, "{diagnostic}").map_err(|e| OxaError::io("write output", &e))?;
    }
    let statuses = connect_all(&config, dirs, cwd, trusted);
    let not_connected = statuses
        .iter()
        .any(|s| s.config.enabled && s.state != ServerState::Connected);
    if as_json {
        write_list_json(out, &config, &statuses)?;
    } else {
        write_list_text(out, &config, &statuses, &user_file)?;
    }
    Ok(if config.diagnostics.is_empty() && !not_connected {
        0
    } else {
        1
    })
}

/// Longest wait for the servers of one `mcp list` / `mcp login` to settle (each server's own
/// `timeout` usually ends it much earlier).
const CONNECT_WAIT: Duration = Duration::from_secs(120);

/// Connect every enabled server of `config` (the CLI user asked for it, so every server may
/// start) and return their states once none is pending or connecting.
fn connect_all(config: &McpConfig, dirs: &Dirs, cwd: &Path, trusted: bool) -> Vec<ServerStatus> {
    if config.servers.is_empty() {
        return Vec::new();
    }
    let manager = McpManager::new(ManagerOpts {
        config: config.clone(),
        cwd: cwd.to_path_buf(),
        agent_dir: dirs.agent_dir.clone(),
        home: dirs.home.clone(),
        trusted,
        gate: Arc::new(|_: &oxa_proto::Access| true),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    });
    manager.start();
    let deadline = Instant::now() + CONNECT_WAIT;
    let mut statuses = manager.servers();
    while Instant::now() < deadline
        && statuses
            .iter()
            .any(|s| matches!(s.state, ServerState::Pending | ServerState::Connecting))
    {
        std::thread::sleep(Duration::from_millis(20));
        statuses = manager.servers();
    }
    manager.stop();
    statuses.sort_by(|a, b| a.config.name.cmp(&b.config.name));
    statuses
}

fn transport_of(server: &ServerConfig) -> &'static str {
    match server.kind {
        ServerKind::Stdio(_) => "stdio",
        ServerKind::Http(_) => "http",
    }
}

/// pi's transport line: the command line of a stdio server, the URL of an HTTP server.
fn transport_line(server: &ServerConfig) -> String {
    match &server.kind {
        ServerKind::Stdio(s) => {
            let mut line = format!("stdio: {}", s.command);
            for arg in &s.args {
                line.push(' ');
                line.push_str(arg);
            }
            line
        }
        ServerKind::Http(h) => format!("http: {}", h.url),
    }
}

fn scope_of(server: &ServerConfig) -> &'static str {
    match server.defined_in {
        ConfigScope::User => "user",
        ConfigScope::Project => "project",
    }
}

fn status_of<'a>(statuses: &'a [ServerStatus], name: &str) -> Option<&'a ServerStatus> {
    statuses.iter().find(|s| s.config.name == name)
}

fn state_text(server: &ServerConfig, status: Option<&ServerStatus>) -> String {
    if !server.enabled {
        return "disabled".to_string();
    }
    match status.map(|s| &s.state) {
        Some(ServerState::Connected) => {
            let n = status.map(|s| s.tools.len()).unwrap_or(0);
            format!("connected, {n} tool{}", if n == 1 { "" } else { "s" })
        }
        Some(state) => state.label().to_string(),
        None => "not started".to_string(),
    }
}

fn write_list_text(
    out: &mut dyn Write,
    config: &McpConfig,
    statuses: &[ServerStatus],
    user_file: &Path,
) -> Result<(), OxaError> {
    if config.servers.is_empty() {
        return say(
            out,
            &format!("No MCP servers configured in {}.", user_file.display()),
        );
    }
    for server in &config.servers {
        let status = status_of(statuses, &server.name);
        say(
            out,
            &format!(
                "{}: {} ({}, {})",
                server.name,
                state_text(server, status),
                server.exposure.as_str(),
                scope_of(server)
            ),
        )?;
        say(out, &format!("  {}", transport_line(server)))?;
        if server.project_override {
            say(out, "  project override: enabled, exposure or toolExposure")?;
        }
        if let Some(description) = &server.description {
            say(out, &format!("  {description}"))?;
        }
        let Some(status) = status else { continue };
        if status.state == ServerState::NeedsAuth {
            say(out, &format!("  sign in with: oxa mcp login {}", server.name))?;
        }
        if !status.tools.is_empty() {
            let tools: Vec<String> = status
                .tools
                .iter()
                .map(|t| {
                    if t.overridden {
                        format!("{} [{}]", t.name, t.exposure.as_str())
                    } else {
                        t.name.clone()
                    }
                })
                .collect();
            say(out, &format!("  tools: {}", tools.join(", ")))?;
        }
        if status.has_resources {
            say(out, "  offers resources")?;
        }
        if let ServerState::Failed(error) = &status.state {
            say(out, &format!("  {}", error.replace('\n', "\n  ")))?;
        }
    }
    for diagnostic in &config.diagnostics {
        say(out, &format!("config error: {diagnostic}"))?;
    }
    Ok(())
}

fn write_list_json(
    out: &mut dyn Write,
    config: &McpConfig,
    statuses: &[ServerStatus],
) -> Result<(), OxaError> {
    let servers: Vec<Value> = config
        .servers
        .iter()
        .map(|server| {
            let status = status_of(statuses, &server.name);
            let state = if !server.enabled {
                "disabled"
            } else {
                status.map(|s| s.state.label()).unwrap_or("not started")
            };
            let tools: Vec<&str> = status
                .map(|s| s.tools.iter().map(|t| t.name.as_str()).collect())
                .unwrap_or_default();
            let error = match status.map(|s| &s.state) {
                Some(ServerState::Failed(e)) => Some(e.clone()),
                _ => None,
            };
            json!({
                "name": server.name,
                "enabled": server.enabled,
                "transport": transport_of(server),
                "exposure": server.exposure.as_str(),
                "defined_in": scope_of(server),
                "project_override": server.project_override,
                "description": server.description,
                "state": state,
                "tools": tools,
                "resources": status.is_some_and(|s| s.has_resources),
                "error": error,
            })
        })
        .collect();
    let doc = json!({ "servers": servers, "diagnostics": config.diagnostics });
    let text = serde_json::to_string_pretty(&doc)
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("list output: {e}")))?;
    say(out, &text)
}

/// pi's default for the whole sign-in (`--timeout`, mcp cli.ts).
const DEFAULT_LOGIN_TIMEOUT_S: u64 = 300;

/// `oxa mcp login` / `oxa mcp logout` (pi mcp cli.ts): the server must be defined in the
/// chosen file and use OAuth (an HTTP server without an `Authorization` header). `logout`
/// deletes the stored credentials (nothing stored is not an error). `login` connects first
/// (already signed in = done), then runs the OAuth sign-in (`oxa_mcp::login`: the browser
/// callback, or a redirect URL pasted on a terminal) and reconnects.
fn cmd_sign_in(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    login: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let verb = if login { "login" } else { "logout" };
    let mut timeout_s = DEFAULT_LOGIN_TIMEOUT_S;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if login && words[i] == "--timeout" {
            let seconds = take_value(words, &mut i, "--timeout")?;
            timeout_s = match seconds.parse::<u64>() {
                Ok(s) if s > 0 => s,
                _ => {
                    return Err(usage(&format!(
                        "--timeout needs a positive number of seconds, got \"{seconds}\""
                    )))
                }
            };
        } else {
            rest.push(words[i].clone());
        }
        i += 1;
    }
    let (name, local) = parse_name_and_local(&rest, verb)?;
    // Sign-in can connect and evaluate configured values. Apply the same trust gate as
    // `mcp list` before reading a project server. Sign-out only removes a stored token.
    if login && local && !crate::approval::saved_project_resources(dirs, cwd, err)? {
        writeln!(
            err,
            "Project MCP server not loaded. Fix: {}",
            crate::approval::approval_fix(cwd)
        )
        .map_err(|e| OxaError::io("write output", &e))?;
        return Ok(1);
    }
    let file = target_file(dirs, cwd, local);
    let config = load_config(&file, None);
    let server = config.servers.iter().find(|s| s.name == name).ok_or_else(|| {
        OxaError::new(
            codes::NOT_FOUND,
            format!("no MCP server \"{name}\" in {}", file.display()),
        )
    })?;
    let http = match &server.kind {
        ServerKind::Http(h)
            if !h
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")) =>
        {
            h.clone()
        }
        _ => {
            let line = format!(
                "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
            );
            writeln!(err, "{line}").map_err(|e| OxaError::io("write output", &e))?;
            return Ok(1);
        }
    };
    let store = TokenStore::open(&dirs.agent_dir.join(AUTH_FILE));
    if !login {
        let removed = store.delete(&name, &http.url)?;
        say(
            out,
            &if removed {
                format!("Signed out of MCP server \"{name}\".")
            } else {
                format!("No stored credentials for MCP server \"{name}\".")
            },
        )?;
        return Ok(0);
    }

    let single = McpConfig {
        servers: vec![server.clone()],
        ..McpConfig::default()
    };
    let first = connect_all(&single, dirs, cwd, local);
    match first.first().map(|s| &s.state) {
        Some(ServerState::Connected) => {
            let n = first[0].tools.len();
            say(
                out,
                &format!("Already signed in to MCP server \"{name}\" ({n} tools)."),
            )?;
            return Ok(0);
        }
        Some(ServerState::NeedsAuth) => {}
        other => {
            let why = match other {
                Some(ServerState::Failed(e)) => e.clone(),
                Some(state) => state.label().to_string(),
                None => "unknown error".to_string(),
            };
            writeln!(err, "MCP server \"{name}\" failed to connect: {why}")
                .map_err(|e| OxaError::io("write output", &e))?;
            return Ok(1);
        }
    }

    let mut io = CliLoginIo::new(&name, out);
    // `!command` values run as in `connect_all`: the same (allow-all) gate, home and cwd.
    let run = |c: &str| oxa_mcp::run_value_command(&|_: &oxa_proto::Access| true, &dirs.home, cwd, c);
    let signed = oxa_mcp::login(
        &name,
        &http,
        &store,
        &mut io,
        Duration::from_secs(timeout_s),
        &oxa_proto::CancelToken::new(),
        &run,
    );
    if let Err(e) = signed {
        let line = if e.code == codes::TIMEOUT || e.code == codes::ABORTED {
            format!(
                "Sign-in to MCP server \"{name}\" was cancelled or not completed within {timeout_s} seconds."
            )
        } else {
            format!("Sign-in to MCP server \"{name}\" failed: {}", e.message)
        };
        writeln!(err, "{line}").map_err(|e| OxaError::io("write output", &e))?;
        return Ok(1);
    }
    let after = connect_all(&single, dirs, cwd, local);
    match after.first().map(|s| &s.state) {
        Some(ServerState::Connected) => {
            let n = after[0].tools.len();
            say(out, &format!("Signed in to MCP server \"{name}\" ({n} tools)."))?;
            Ok(0)
        }
        other => {
            let why = match other {
                Some(ServerState::Failed(e)) => e.clone(),
                Some(state) => state.label().to_string(),
                None => "unknown error".to_string(),
            };
            writeln!(err, "Signed in, but MCP server \"{name}\" did not connect: {why}")
                .map_err(|e| OxaError::io("write output", &e))?;
            Ok(1)
        }
    }
}

/// The terminal side of `oxa mcp login`: prints the authorization URL, and on a terminal
/// accepts a pasted redirect URL (pi: "If the browser cannot reach this machine, paste the URL
/// it was redirected to").
struct CliLoginIo<'a> {
    name: String,
    out: &'a mut dyn Write,
    pasted: Option<std::sync::mpsc::Receiver<String>>,
}

impl<'a> CliLoginIo<'a> {
    fn new(name: &str, out: &'a mut dyn Write) -> CliLoginIo<'a> {
        use std::io::IsTerminal;
        // Only a terminal is read: a pipe would be consumed for nothing.
        let pasted = std::io::stdin().is_terminal().then(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                while std::io::stdin()
                    .read_line(&mut line)
                    .map(|n| n > 0)
                    .unwrap_or(false)
                {
                    if tx.send(line.trim().to_string()).is_err() {
                        break;
                    }
                    line.clear();
                }
            });
            rx
        });
        CliLoginIo {
            name: name.to_string(),
            out,
            pasted,
        }
    }
}

impl oxa_mcp::LoginIo for CliLoginIo<'_> {
    fn open_url(&mut self, url: &str) {
        let _ = writeln!(
            self.out,
            "Sign in to MCP server \"{}\" in your browser:\n{url}",
            self.name
        );
        if self.pasted.is_some() {
            let _ = writeln!(
                self.out,
                "If the browser cannot reach this machine, paste the URL it was redirected to:"
            );
        }
        let _ = self.out.flush();
    }
    fn pasted_redirect(&mut self) -> Option<String> {
        self.pasted
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
            .filter(|l| !l.is_empty())
    }
    fn status(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn run(env: &TestEnv, list: &[&str]) -> (Result<i32, OxaError>, String, String) {
        let dirs = Dirs::under(env.root());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_mcp(&words(list), &dirs, &env.work(), &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    #[test]
    fn add_list_remove_round_trip() {
        let env = TestEnv::new();
        let (code, out, _) = run(&env, &["add", "fs", "--env", "K=${V}", "--", "npx", "-y", "pkg"]);
        assert_eq!(code.unwrap(), 0);
        assert!(out.contains("Saved MCP server \"fs\""));
        let file = user_config_path(&Dirs::under(env.root()).agent_dir);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"command\": \"npx\""), "{text}");

        // `list` connects: `npx` is replaced by a command that cannot start (no network here),
        // so the server is listed as failed and the exit code is 1 (pi mcp.md).
        run(&env, &["add", "gone", "--", "/nonexistent/mcp-h"]).0.unwrap();
        let (code, out, err) = run(&env, &["remove", "fs"]);
        assert_eq!(code.unwrap(), 0, "{err}{out}");
        let (code, out, _) = run(&env, &["list", "--json"]);
        assert_eq!(code.unwrap(), 1);
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["servers"][0]["name"], "gone");
        assert_eq!(doc["servers"][0]["state"], "failed");
        assert!(doc["servers"][0]["error"].is_string());
        assert_eq!(doc["diagnostics"], json!([]));
        run(&env, &["add", "fs", "--", "npx", "-y", "pkg"]).0.unwrap();

        let (code, _, _) = run(&env, &["remove", "fs"]);
        assert_eq!(code.unwrap(), 0);
        let (code, _, _) = run(&env, &["remove", "fs"]);
        assert_eq!(code.unwrap_err().code, codes::NOT_FOUND);
    }

    #[test]
    fn project_login_rejects_changed_content() {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"projectTrustPolicy":"content"}"#,
        );
        env.write("work/.oxa/mcp.json", r#"{"mcpServers": {}}"#);
        let (code, _, _) = run(&env, &["list"]);
        assert_eq!(code.unwrap(), 0);
        env.write(
            "work/.oxa/mcp.json",
            r#"{"mcpServers":{"new":{"url":"http://127.0.0.1:9/mcp","enabled":false}}}"#,
        );
        let (code, _, err) = run(&env, &["login", "new", "-l"]);
        assert_eq!(code.unwrap(), 1);
        assert!(
            err.contains("changed or new since you approved") && err.contains("mcp.json"),
            "{err}"
        );
    }

    #[test]
    fn invalid_entry_lists_with_exit_one() {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/mcp.json",
            r#"{"mcpServers": {"bad": {"type": "sse", "url": "https://x/sse"}, "ok": {"command": "o", "enabled": false}}}"#,
        );
        let (code, out, err) = run(&env, &["list"]);
        assert_eq!(code.unwrap(), 1);
        assert!(out.contains("ok"));
        assert!(err.contains("SSE"), "{err}");
    }

    #[test]
    fn usage_errors_and_sign_in_do_not_panic() {
        let env = TestEnv::new();
        assert_eq!(run(&env, &[]).0.unwrap_err().code, codes::USAGE);
        assert_eq!(run(&env, &["frob"]).0.unwrap_err().code, codes::USAGE);
        assert_eq!(run(&env, &["add", "x"]).0.unwrap_err().code, codes::USAGE);
        assert_eq!(
            run(&env, &["add", "x", "--url", "https://h/mcp", "--", "cmd"])
                .0
                .unwrap_err()
                .code,
            codes::USAGE
        );
        run(&env, &["add", "web", "--url", "https://h.example/mcp"])
            .0
            .unwrap();
        assert_eq!(
            run(&env, &["login", "web", "--timeout", "0"]).0.unwrap_err().code,
            codes::USAGE
        );
        assert_eq!(
            run(&env, &["logout", "missing"]).0.unwrap_err().code,
            codes::NOT_FOUND
        );
        // pi: logout with nothing stored succeeds.
        let (code, out, _) = run(&env, &["logout", "web"]);
        assert_eq!(code.unwrap(), 0);
        assert!(out.contains("No stored credentials"), "{out}");
        run(&env, &["add", "local-only", "-l", "--", "tool"]).0.unwrap();
        assert!(env.work().join(".oxa/mcp.json").exists());
    }
}
