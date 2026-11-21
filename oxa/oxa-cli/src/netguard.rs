//! Wave H: the network decisions of the front end (O-167(b), P3, P4). Every
//! decision is here; `dispatch.rs` only calls in.
use oxa_config::{dirs_from_env, Env, Settings};
use oxa_proto::{Level, OxaError, PermissionMode, Sandbox};
use oxa_sandbox::{sandbox_net_setting, NetSandbox, SETTING_SANDBOX_NET};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Paths that model-controlled commands may write. Use the layout that is actually passed
/// to the sandbox, including a home workspace enabled by startup confirmation.
#[derive(Clone)]
pub(crate) struct TokenWorkspace {
    cwd: PathBuf,
    home: PathBuf,
    writable_home: bool,
    writes: Vec<PathBuf>,
}

impl TokenWorkspace {
    pub(crate) fn new(cwd: &Path, home: &Path, layout: &oxa_sandbox::Layout) -> Self {
        let mut writes = layout.write.clone();
        writes.extend(layout.write.iter().filter_map(|path| path.canonicalize().ok()));
        Self {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            writable_home: layout.allow_home_write,
            writes,
        }
    }
}

/// Network policy for a checkout run by the work supervisor.
pub(crate) fn with_net(
    sandbox: oxa_sandbox::Bwrap,
    settings: &Settings,
    source: &Path,
    checkout: &Path,
) -> Arc<NetSandbox> {
    let mut workspace = TokenWorkspace::new(source, &sandbox.home, &sandbox.layout);
    workspace.writes.push(checkout.to_path_buf());
    with_policy(
        Arc::new(sandbox),
        settings,
        true,
        GhLaunch::Direct,
        Vec::new(),
        Some(workspace),
    )
}

/// [`with_net`] for the run's [`ExecPolicy`](oxa_sandbox::ExecPolicy): credential discovery
/// for redaction starts `gh` without network through the policy's pinned bwrap when the bash
/// network is off.
pub(crate) fn with_net_policy(
    sandbox: Arc<dyn Sandbox>,
    settings: &Settings,
    policy: &oxa_sandbox::ExecPolicy,
    env: &Env,
    engine_key: Option<&str>,
    workspace: TokenWorkspace,
) -> Arc<NetSandbox> {
    with_policy(
        sandbox,
        settings,
        true,
        policy
            .pinned_bwrap
            .clone()
            .map_or(GhLaunch::Direct, GhLaunch::NoNet),
        host_secrets(env, engine_key),
        Some(workspace),
    )
}

/// Values a host-rights program can echo even though oxa does not inherit them into bash. In
/// particular, a worker's `GH_TOKEN` need not be in a `gh` login store.
fn host_secrets(env: &Env, engine_key: Option<&str>) -> Vec<String> {
    env.pairs()
        .iter()
        .filter(|(name, value)| oxa_sandbox::is_host_secret_name(name) && !value.is_empty())
        .map(|(_, value)| value.clone())
        .chain(engine_key.filter(|key| !key.is_empty()).map(str::to_owned))
        .collect()
}

/// S11: yolo gives bash the network whatever `oxa.sandbox.net` says; the setting still applies to
/// auto and standard. Say so at the start instead of ignoring the setting silently. Enforcement
/// does not change, and an offline session has no bash network in any mode, so it is not noted.
pub(crate) fn yolo_net_notice(level: PermissionMode, net: Option<&str>, offline: bool) -> Option<String> {
    (level == PermissionMode::Yolo && !offline && !sandbox_net_setting(net))
        .then(|| format!("yolo: bash has the network; {SETTING_SANDBOX_NET}=off applies to auto/standard"))
}

/// Acceptance commands keep the user's network policy without GitHub authentication.
pub(crate) fn without_github(sandbox: Arc<dyn Sandbox>, settings: &Settings) -> Arc<dyn Sandbox> {
    with_policy(sandbox, settings, false, GhLaunch::Direct, Vec::new(), None)
}

/// How credential discovery may start the owner's `gh` (#1952): the program is a PATH lookup
/// that can be a shim, so with the bash network off it must not have the network either.
pub(crate) enum GhLaunch {
    /// With the network, as the owner's own `gh`.
    Direct,
    /// Without network, through this pinned bwrap.
    NoNet(std::path::PathBuf),
    /// Never started: offline, a runner-owned GitHub, or no pinned bwrap to hold the network
    /// off. The `hosts.yml` tokens are still read and scrubbed.
    Never,
}

fn with_policy(
    sandbox: Arc<dyn Sandbox>,
    settings: &Settings,
    github_auth: bool,
    requested_launch: GhLaunch,
    known: Vec<String>,
    workspace: Option<TokenWorkspace>,
) -> Arc<NetSandbox> {
    let runner_owns_github = std::env::var_os(oxa_work::RUNNER_GITHUB_ENV).is_some_and(|value| value == "1");
    let on = !runner_owns_github && sandbox_net_setting(settings.string(SETTING_SANDBOX_NET).as_deref());
    let offline = oxa_proto::offline();
    // The `gh` lookup can stall (a mise shim may try to install gh): run it lazily, at the first
    // bash command that has the network, never at startup.
    let scrub_host = github_auth
        && !test_isolated(
            std::env::var(oxa_proto::names::ENV_TEST_ISOLATION)
                .ok()
                .as_deref(),
        );
    let lookup = scrub_host && on && !offline;
    // Host-rights bash can read a file-backed login with the network off too, and `gh auth token
    // --user other` names any account: every credential is scrubbed from its output, so this
    // lookup does not depend on the network setting, nor on who owns GitHub for the run (a
    // runner-owned task still must not print a file-backed login). Offline, or when the runner
    // owns GitHub, `gh` itself is never started (a shim could install it over the network, and
    // the run must not use GitHub authentication): the `hosts.yml` tokens are scrubbed then.
    let launch = if runner_owns_github || offline {
        GhLaunch::Never
    } else if !on {
        match requested_launch {
            GhLaunch::NoNet(bwrap) => GhLaunch::NoNet(bwrap),
            _ => GhLaunch::Never,
        }
    } else {
        GhLaunch::Direct
    };
    let mut ns = NetSandbox::new(sandbox, on, offline);
    // A runner-owned or offline task never runs the owner's configured helper.
    let mut command = (scrub_host && !runner_owns_github && !offline)
        .then(|| github_token_command(settings))
        .unwrap_or_default();
    let workspace = workspace.map(|mut workspace| {
        // A host-side lookup must not trust the shared cache even if this particular
        // session does not mount it (for example, an interactive session).
        if !command.is_empty() {
            let cache = crate::worker_cache::cache_root(settings, &workspace.home, &Env::from_process());
            workspace.writes.extend(cache.canonicalize().ok());
            workspace.writes.push(cache);
        }
        workspace
    });
    if let Some(workspace) = workspace.clone().filter(|_| lookup) {
        let command = std::mem::take(&mut command);
        let ttl = github_token_ttl(settings);
        ns = if command.is_empty() {
            ns.with_gh_token_refreshing(move || gh_token(&command, &workspace), ttl)
        } else {
            // Only the runner's isolated GitHub transport authenticates with configured
            // credentials. Keep masking them in command output without passing them to bash.
            ns.with_gh_token_redaction_refreshing(move || gh_token(&command, &workspace), ttl)
        };
    }
    if scrub_host || !known.is_empty() {
        ns = ns.with_host_secrets_lazy(move || {
            let mut secrets = known.clone();
            if scrub_host {
                secrets.extend(gh_host_secrets(&launch));
            }
            // A host-rights command may start `gh` itself, or `env`, and read the owner's token
            // that this session never loaded. Asking the same helper is the session's own call;
            // a program the session never started cannot connect it to a prompt.
            if let Some(workspace) = workspace.as_ref().filter(|_| !command.is_empty()) {
                secrets.extend(gh_token(&command, workspace));
            }
            secrets
        });
    }
    Arc::new(ns)
}

/// Every GitHub CLI credential of this machine: the plain tokens in `hosts.yml` and the token
/// `gh auth token` returns for each host and account it lists (keyring logins included), in
/// every config store `gh` may read ([`gh_config_dirs`]). Host bash runs with a cleared
/// environment, so its `gh` can read another store than this process: every store is scrubbed.
/// Runs `gh` outside the sandbox with a short deadline, as [`GhLaunch`] says; any failure yields
/// what was found so far. Reads local state only. Tokens are never logged. With
/// [`GhLaunch::Never`] only the `hosts.yml` files are read and no program is started.
fn gh_host_secrets(launch: &GhLaunch) -> Vec<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let configs = gh_config_dirs(
        std::env::var_os("GH_CONFIG_DIR").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        home.as_deref(),
    );
    let exe = if matches!(launch, GhLaunch::Never) {
        None
    } else {
        trusted_gh_for(
            std::env::var_os("PATH").as_deref(),
            std::env::current_dir().ok().as_deref(),
            home.as_deref(),
        )
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    collect_host_secrets(launch, &configs, exe.as_deref(), &cwd)
}

/// The command that asks `exe` (`gh`) `args` for the store `config`, as `launch` allows.
fn gh_command(
    launch: &GhLaunch,
    exe: &Path,
    config: &Path,
    cwd: &Path,
    args: &[&str],
) -> Option<std::process::Command> {
    match launch {
        GhLaunch::Never => None,
        GhLaunch::Direct => {
            let mut command = std::process::Command::new(exe);
            command.args(args).env("GH_CONFIG_DIR", config);
            Some(command)
        }
        GhLaunch::NoNet(bwrap) => {
            let mut inner = vec![exe.to_string_lossy().into_owned()];
            inner.extend(args.iter().map(|a| (*a).to_string()));
            let env = [("GH_CONFIG_DIR".to_string(), config.to_string_lossy().into_owned())];
            let argv = oxa_sandbox::no_network_argv(bwrap, cwd, &env, &inner);
            let mut command = std::process::Command::new(argv.first()?);
            command.args(&argv[1..]);
            Some(command)
        }
    }
}

fn collect_host_secrets(
    launch: &GhLaunch,
    configs: &[std::path::PathBuf],
    exe: Option<&Path>,
    cwd: &Path,
) -> Vec<String> {
    let mut found = Vec::new();
    for config in configs {
        let text = std::fs::read_to_string(config.join("hosts.yml")).unwrap_or_default();
        let (tokens, accounts) = parse_gh_hosts(&text);
        found.extend(tokens);
        let Some(exe) = exe else { continue };
        let mut ask = |args: &[&str]| {
            let Some(command) = gh_command(launch, exe, config, cwd, args) else {
                return;
            };
            if let Some((0, out)) = oxa_work::command_runner(command, Some(4096), GH_DEADLINE) {
                let t = out.trim();
                if !t.is_empty() && !t.chars().any(char::is_whitespace) {
                    found.push(t.to_string());
                }
            }
        };
        ask(&["auth", "token", "--hostname", "github.com"]);
        for (host, user) in accounts.iter() {
            ask(&[
                "auth",
                "token",
                "--hostname",
                host.as_str(),
                "--user",
                user.as_str(),
            ]);
        }
    }
    found.sort();
    found.dedup();
    found
}

/// The `gh` config stores a command on this machine may read: `GH_CONFIG_DIR` of this
/// process, `$XDG_CONFIG_HOME/gh` and `$HOME/.config/gh` (the defaults a cleared environment
/// falls back to). Duplicates removed, order kept.
fn gh_config_dirs(
    gh_config_dir: Option<&std::ffi::OsStr>,
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let candidates = [
        gh_config_dir.map(std::path::PathBuf::from),
        xdg_config_home.map(|x| std::path::PathBuf::from(x).join("gh")),
        home.map(|h| h.join(".config").join("gh")),
    ];
    for dir in candidates.into_iter().flatten() {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The plain `oauth_token` values and the (host, user) pairs of a `gh` `hosts.yml`. A line
/// parser for the fixed shape `gh` writes: hosts at column 0, `users:` with nested account
/// names, `oauth_token:` values at any depth.
fn parse_gh_hosts(text: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut tokens = Vec::new();
    let mut accounts = Vec::new();
    let (mut host, mut in_users, mut users_indent) = (String::new(), false, 0usize);
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let unquote = |v: &str| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if indent == 0 {
            host = unquote(trimmed.trim_end_matches(':'));
            in_users = false;
        } else if let Some(v) = trimmed.strip_prefix("oauth_token:") {
            let v = unquote(v);
            if !v.is_empty() && !v.chars().any(char::is_whitespace) {
                tokens.push(v);
            }
        } else if trimmed == "users:" {
            in_users = true;
            users_indent = indent;
        } else if in_users && indent <= users_indent {
            in_users = false;
        } else if in_users && indent == users_indent + 4 && trimmed.ends_with(':') && !host.is_empty() {
            accounts.push((host.clone(), unquote(trimmed.trim_end_matches(':'))));
        }
    }
    (tokens, accounts)
}

/// Owner setting: argv that prints the GitHub token. User layer only: a project file never
/// names a program that runs outside the sandbox.
pub(crate) const SETTING_GH_TOKEN_COMMAND: &str = "oxa.github.tokenCommand";
/// Owner setting: seconds before the token is read again.
pub(crate) const SETTING_GH_TOKEN_TTL: &str = "oxa.github.tokenTtlSeconds";
/// Default lifetime of a read token, in seconds.
const GH_TOKEN_TTL_DEFAULT: u64 = 300;

pub(crate) fn github_token_command(settings: &Settings) -> Vec<String> {
    settings
        .user_value(SETTING_GH_TOKEN_COMMAND)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The configured token command as one trusted program: a bare first word is resolved in the
/// tool directories of `PATH`; every operand must be a trusted absolute helper (#2262 reuses
/// this for the lead web sampler, which runs the command once per sample).
pub(crate) fn token_program(argv: &[String], cwd: &Path, home: &Path) -> Option<Vec<String>> {
    let (program, rest) = argv.split_first()?;
    let path = std::env::var_os("PATH");
    let program = if program.contains('/') {
        program.clone()
    } else {
        trusted_executable_for(program, path.as_deref(), Some(cwd), Some(home))?
            .to_str()?
            .to_owned()
    };
    Some(std::iter::once(program).chain(rest.iter().cloned()).collect())
}

fn github_token_ttl(settings: &Settings) -> std::time::Duration {
    let secs = settings
        .user_value(SETTING_GH_TOKEN_TTL)
        .and_then(|v| v.as_u64())
        .unwrap_or(GH_TOKEN_TTL_DEFAULT);
    std::time::Duration::from_secs(secs)
}

/// Under test isolation (`OXA_TEST_ISOLATION=1`) the host's `gh` login is never consulted: a
/// test must not read real credentials, and a slow `gh` (up to `GH_DEADLINE`) must not delay
/// startup and skew timing tests.
fn test_isolated(v: Option<&str>) -> bool {
    v == Some("1")
}

/// Read the token outside the sandbox through the owner's helper or `gh auth token`.
/// Failures return `None`. The token is never logged.
fn gh_token(command: &[String], workspace: &TokenWorkspace) -> Option<String> {
    let path = std::env::var_os("PATH");
    let cwd = workspace.cwd.as_path();
    let home = workspace.home.as_path();
    if let Some(program) = command.first() {
        let project = cwd
            .ancestors()
            .filter(|dir| dir.join(".git").exists())
            .last()
            .or_else(|| (!oxa_sandbox::is_home_itself(cwd, home)).then_some(cwd));
        let mut argv = command.to_vec();
        if !program.contains('/') {
            argv[0] = trusted_executable_for(program, path.as_deref(), Some(cwd), Some(home))?
                .to_str()?
                .to_owned();
        }
        let command = oxa_work::trusted_token_program(
            &argv,
            None,
            project,
            &workspace.writes,
            workspace.writable_home.then_some(home),
        )?;
        return oxa_work::token_from_command(command, oxa_work::COMMAND_DEADLINE);
    }
    let exe = trusted_gh_for(path.as_deref(), Some(cwd), Some(home))?;
    gh_token_from(&exe, GH_DEADLINE)
}

/// `trusted_gh` for a cwd that is the home directory (#122). The project rule cannot tell
/// `~/.local/bin` from `~/project/bin` there, so under `~` only the user's conventional
/// install directories (`USER_BIN`, including the mise shims) count; a `PATH` entry anywhere else below `~` (a writable
/// project shim) is refused, as are those whose resolved target is. System paths are unchanged,
/// and the writable-directory check of `trusted_gh` still applies.
fn trusted_gh_for(
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    trusted_executable_for("gh", path, cwd, home)
}

/// Host tools use the same project/PATH checks as the GitHub credential helper.
pub(crate) fn trusted_executable_for(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let Some(h) = home.map(canon) else {
        return trusted_executable(program, path, cwd);
    };
    if cwd.is_none_or(|c| canon(c) != h) {
        return trusted_executable(program, path, cwd);
    }
    let allowed: Vec<std::path::PathBuf> = USER_BIN.iter().map(|d| h.join(d)).collect();
    // Allowed by where it resolves to, so a symlink from `~/.local/bin` into a project is refused.
    let ok = |d: &Path| {
        let dc = canon(d);
        let under_home = |x: &Path| x.starts_with(&h) || x.starts_with(home.unwrap_or(&h));
        !(under_home(d) || under_home(&dc)) || allowed.iter().any(|a| dc.starts_with(a))
    };
    // The final executable must resolve to an allowed place too; otherwise keep searching.
    let file_ok = |f: &Path| {
        let fc = canon(f);
        let under_home = |x: &Path| x.starts_with(&h) || x.starts_with(home.unwrap_or(&h));
        !(under_home(f) || under_home(&fc)) || allowed.iter().any(|a| fc.starts_with(a))
    };
    std::env::split_paths(path?)
        .filter(|d| ok(d))
        .filter_map(|d| trusted_executable(program, Some(d.as_os_str()), None))
        .find(|f| file_ok(f))
}

/// The directories below `~` where a user installs their own tools.
const USER_BIN: &[&str] = &[
    ".local/bin",
    "bin",
    ".cargo/bin",
    ".local/share/mise/shims",
    ".local/share/mise/installs",
];

const GH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Run `exe auth token --hostname github.com` under one `deadline` that covers the process and
/// the reading of its output.
fn gh_token_from(exe: &Path, deadline: std::time::Duration) -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let project = cwd
        .as_deref()
        .and_then(|cwd| cwd.ancestors().filter(|dir| dir.join(".git").exists()).last());
    let mut command = oxa_work::token_helper_command(exe, project);
    command.args(["auth", "token", "--hostname", "github.com"]);
    oxa_work::token_from_command(command, deadline)
}

/// The host tool to run: the first executable with the requested name in an absolute `PATH` entry that is
/// not inside the project (`cwd`) and not a group- or world-writable directory. Relative
/// entries (`.`, `bin`, empty) are ignored, so a project shim never runs outside the sandbox.
fn trusted_executable(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let cwd_c = cwd.and_then(|c| std::fs::canonicalize(c).ok());
    std::env::split_paths(path?)
        .filter(|d| d.is_absolute())
        .filter(|d| {
            let dc = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
            !cwd.is_some_and(|c| d.starts_with(c) || dc.starts_with(c))
                && !cwd_c.as_ref().is_some_and(|c| dc.starts_with(c))
                && std::fs::metadata(&dc).is_ok_and(|m| m.permissions().mode() & 0o022 == 0)
        })
        .map(|d| d.join(program))
        .find(|p| {
            // The file's own resolved target must not lie in the project either.
            let pc = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            !cwd.is_some_and(|c| pc.starts_with(c))
                && !cwd_c.as_ref().is_some_and(|c| pc.starts_with(c))
                && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Keep repository code out of host lookups in workers and check supervisors,
/// while preserving the order of the user's installed tool directories.
pub(crate) fn trusted_work_path(
    path: &std::ffi::OsStr,
    project: &Path,
    home: &Path,
) -> Option<std::ffi::OsString> {
    use std::os::unix::fs::PermissionsExt;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let project = project.canonicalize().ok()?;
    let is_worker = |p: &Path| p.components().any(|part| part.as_os_str() == ".oxa-worktrees");
    let allowed: Vec<_> = USER_BIN.iter().map(|d| home.join(d)).collect();
    let entries = std::env::split_paths(path).filter(|dir| {
        if !dir.is_absolute() || is_worker(dir) {
            return false;
        }
        let Ok(real) = dir.canonicalize() else {
            return false;
        };
        if is_worker(&real)
            || !std::fs::metadata(&real).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o022 == 0)
        {
            return false;
        }
        if project == home {
            if real.starts_with(&home) && !allowed.iter().any(|p| real.starts_with(p)) {
                return false;
            }
        } else if dir.starts_with(&project) || real.starts_with(&project) {
            return false;
        }
        ["git", "gh", "bwrap"].iter().all(|name| {
            let exe = dir.join(name);
            if std::fs::symlink_metadata(&exe).is_err() {
                return true;
            }
            trusted_executable_for(name, Some(dir.as_os_str()), Some(&project), Some(&home))
                .is_some_and(|p| p.canonicalize().is_ok_and(|p| !is_worker(&p)))
        })
    });
    std::env::join_paths(entries).ok()
}

/// The setting that allows an engine host that is not loopback (P3). Read from the user's
/// settings only: a project file cannot send the prompts elsewhere.
pub(crate) const SETTING_ALLOW_REMOTE: &str = "oxa.engine.allowRemote";
/// The setting of the hard offline switch (P4).
pub(crate) const SETTING_OFFLINE: &str = "oxa.offline";
/// The environment variable of the hard offline switch (pi: `PI_OFFLINE`).
pub(crate) const ENV_OFFLINE: &str = "OXA_OFFLINE";

/// The shared user-facing boolean (`1`/`true`/`yes`/`on`), default `false`: unset, empty and
/// any other word stay off. See [`oxa_config::EnvFlag`].
pub(crate) fn truthy(v: Option<&str>) -> bool {
    oxa_config::EnvFlag::parse(v).or(false)
}

/// `(offline, allow_remote_engine)` from the flag, the environment and the user's settings.
/// Offline is on when any of `--offline`, `OXA_OFFLINE`, `oxa.offline` says so.
pub(crate) fn policy_from(flag_offline: bool, env: &Env, user: &Settings) -> (bool, bool) {
    let offline = flag_offline || truthy(env.get(ENV_OFFLINE)) || user.bool(SETTING_OFFLINE, false);
    (offline, user.bool(SETTING_ALLOW_REMOTE, false))
}

/// Install the process network policy (`oxa_proto::set_net_policy`) before anything connects.
pub(crate) fn install(flag_offline: bool, env: &Env, session_dir: Option<&Path>, cwd: &Path) {
    let user = match dirs_from_env(env, session_dir) {
        Ok(dirs) => Settings::load(&dirs, cwd, false),
        Err(_) => Settings::default(),
    };
    let (offline, allow_remote) = policy_from(flag_offline, env, &user);
    oxa_proto::set_net_policy(offline, allow_remote);
}

/// P3 at startup: the engine endpoint must be loopback unless `oxa.engine.allowRemote` (and
/// never a remote one while offline). The same check runs at every engine connect.
pub(crate) fn check_endpoint(base_url: &str) -> Result<(), OxaError> {
    match oxa_proto::url_host(base_url) {
        Some(host) => oxa_proto::check_engine_host(&host),
        None => Ok(()), // not a URL: `HttpEngine::new` reports it
    }
}

/// P3: the one startup notice for an allowed remote engine.
pub(crate) fn endpoint_notice(base_url: &str) -> Option<(Level, String)> {
    let host = oxa_proto::url_host(base_url)?;
    (!oxa_proto::is_loopback_host(&host)).then(|| (Level::Warning, oxa_proto::remote_engine_notice(&host)))
}

#[cfg(test)]
mod tests {
    /// An owner `GH_CONFIG_DIR` does not hide the default stores: host bash runs with a cleared
    /// environment, so its `gh` reads `$XDG_CONFIG_HOME/gh` or `$HOME/.config/gh`, and their
    /// tokens must be scrubbed too.
    #[test]
    fn every_gh_config_store_is_scrubbed() {
        use std::ffi::OsStr;
        use std::path::{Path, PathBuf};
        let dirs = super::gh_config_dirs(
            Some(OsStr::new("/owner/gh")),
            Some(OsStr::new("/home/u/.config")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            dirs,
            [PathBuf::from("/owner/gh"), PathBuf::from("/home/u/.config/gh")]
        );
        let dirs = super::gh_config_dirs(
            Some(OsStr::new("/owner/gh")),
            Some(OsStr::new("/xdg")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/owner/gh"),
                PathBuf::from("/xdg/gh"),
                PathBuf::from("/home/u/.config/gh")
            ]
        );
        assert_eq!(
            super::gh_config_dirs(None, None, Some(Path::new("/home/u"))),
            [PathBuf::from("/home/u/.config/gh")]
        );
        assert!(super::gh_config_dirs(Some(OsStr::new("")), None, None).is_empty());
    }

    #[test]
    fn hosts_yml_yields_every_account_and_plain_token() {
        let yml = "github.com:\n    users:\n        alice:\n            oauth_token: SYNTH_ONE\n        bob:\n    oauth_token: SYNTH_TWO\n    git_protocol: https\n    user: alice\nghe.example:\n    users:\n        carol:\n";
        let (tokens, accounts) = super::parse_gh_hosts(yml);
        assert_eq!(tokens, ["SYNTH_ONE", "SYNTH_TWO"]);
        assert_eq!(
            accounts,
            [
                ("github.com".to_string(), "alice".to_string()),
                ("github.com".to_string(), "bob".to_string()),
                ("ghe.example".to_string(), "carol".to_string()),
            ]
        );
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn token_command_is_user_only() {
        let command =
            serde_json::json!({"oxa": {"github": {"tokenCommand": ["/tmp/evil"], "tokenTtlSeconds": 1}}});
        let project_only = Settings::from_values(serde_json::json!({}), command.clone());
        assert!(github_token_command(&project_only).is_empty());
        assert_eq!(
            github_token_ttl(&project_only),
            std::time::Duration::from_secs(300)
        );
        let user = Settings::from_values(command, serde_json::json!({}));
        assert_eq!(github_token_command(&user), vec!["/tmp/evil".to_string()]);
        assert_eq!(github_token_ttl(&user), std::time::Duration::from_secs(1));
    }

    #[test]
    fn owner_token_command_supplies_the_token() {
        let dir = tmp("token-command");
        let project = dir.join("project");
        std::fs::create_dir(&project).unwrap();
        let workspace = TokenWorkspace::new(&project, &dir, &oxa_sandbox::Layout::default());
        let source = script(&dir, "mint", "echo ghs_MINTED");
        let argv = vec!["/bin/sh".to_string(), source.display().to_string()];
        assert_eq!(gh_token(&argv, &workspace), Some("ghs_MINTED".to_string()));
        let failing = script(&dir, "fail", "exit 3");
        assert_eq!(gh_token(&[failing.display().to_string()], &workspace), None);
    }

    #[test]
    fn token_command_rejects_a_writable_home_helper() {
        let home = tmp("token-home-workspace");
        let scripts = home.join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        let mint = script(&scripts, "mint", "echo SYNTH_MINTED");
        let argv = [mint.display().to_string()];
        let read_only = TokenWorkspace::new(&home, &home, &oxa_sandbox::Layout::default());
        assert_eq!(gh_token(&argv, &read_only).as_deref(), Some("SYNTH_MINTED"));
        let writable = TokenWorkspace::new(
            &home,
            &home,
            &oxa_sandbox::Layout {
                allow_home_write: true,
                ..Default::default()
            },
        );
        assert_eq!(gh_token(&argv, &writable), None);
    }

    #[test]
    fn test_isolation_skips_the_host_gh_login() {
        assert!(test_isolated(Some("1")));
        assert!(!test_isolated(None));
        assert!(!test_isolated(Some("0")));
    }

    /// #1952: with the bash network off, `gh` is started only through the no-network wrapper;
    /// with no pinned bwrap it is not started at all, and the `hosts.yml` tokens are still found.
    #[test]
    fn credential_discovery_never_gives_gh_the_network_the_owner_turned_off() {
        let d = tmp("gh-nonet");
        let ran = d.join("ran");
        let log = d.join("bwrap-args");
        let gh = script(
            &d,
            "gh",
            &format!("echo ran >> {}\necho SYNTH_FROM_GH", ran.display()),
        );
        let bwrap = script(
            &d,
            "bwrap",
            &format!(
                "echo \"$@\" >> {}\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
                log.display()
            ),
        );
        let config = d.join("cfg");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("hosts.yml"),
            "github.com:\n    oauth_token: SYNTH_FROM_FILE\n",
        )
        .unwrap();

        let none = collect_host_secrets(&GhLaunch::Never, &[config.clone()], Some(&gh), &d);
        assert_eq!(none, ["SYNTH_FROM_FILE"]);
        assert!(
            !ran.exists(),
            "gh must not start without a wrapper that holds the network off"
        );

        let found = collect_host_secrets(&GhLaunch::NoNet(bwrap), &[config], Some(&gh), &d);
        assert_eq!(found, ["SYNTH_FROM_FILE", "SYNTH_FROM_GH"]);
        assert!(ran.exists());
        let wrapped = std::fs::read_to_string(&log).unwrap();
        assert!(
            wrapped.contains("--unshare-net") && wrapped.contains(&gh.display().to_string()),
            "{wrapped}"
        );
    }

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-ng-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// #1449: an unusable user settings file keeps the process offline policy and the bash
    /// network decision restricted, separately; a missing file keeps the normal defaults.
    #[test]
    fn unusable_user_settings_keep_process_and_bash_network_restricted() {
        let env = Env::from_pairs(&[]);
        for (tag, content) in [("badjson", Some("{ \"oxa\": {\"offline\": ")), ("absent", None)] {
            let home = tmp(tag);
            let dirs = oxa_config::Dirs::under(&home);
            if let Some(text) = content {
                std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
                std::fs::write(dirs.settings_file(), text).unwrap();
            }
            let user = Settings::load(&dirs, &home, false);
            let (offline, allow_remote) = policy_from(false, &env, &user);
            let bash_net = sandbox_net_setting(user.string(SETTING_SANDBOX_NET).as_deref());
            let restricted = content.is_some();
            assert_eq!(offline, restricted, "{tag}: process offline policy");
            assert!(!allow_remote, "{tag}");
            assert_eq!(!bash_net, restricted, "{tag}: bash network decision");
            assert_eq!(user.diagnostics().is_empty(), !restricted, "{tag}");
        }
    }

    #[test]
    fn a_helper_that_leaves_the_pipe_open_cannot_stall_startup() {
        let d = tmp("pipe");
        // Exits 0 at once, but a background child keeps stdout open for 6 s.
        let gh = script(&d, "gh", "(sleep 6 >&1 &)\necho SYNTH_TOKEN; exit 0");
        let t = std::time::Instant::now();
        let r = gh_token_from(&gh, std::time::Duration::from_millis(1500));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "blocked {:?}",
            t.elapsed()
        );
        assert!(r.is_none() || r.as_deref() == Some("SYNTH_TOKEN"));
    }

    #[test]
    fn a_well_behaved_helper_gives_its_token() {
        let d = tmp("ok");
        let gh = script(&d, "gh", "echo SYNTH_TOKEN");
        // A parallel test's fork can hold the script's write fd (ETXTBSY): retry the spawn.
        let got = (0..5).find_map(|_| gh_token_from(&gh, GH_DEADLINE));
        assert_eq!(got.as_deref(), Some("SYNTH_TOKEN"));
    }

    #[test]
    fn an_oversized_helper_result_is_refused() {
        let d = tmp("oversized");
        let gh = script(&d, "gh", "printf '%04097d\\n' 1");
        assert!(gh_token_from(&gh, GH_DEADLINE).is_none());
    }

    /// #122: with cwd = home, a `gh` in a user-owned directory under home (`~/.local/bin`)
    /// is still found, so `GH_TOKEN` is injected the same as for a project cwd.
    #[test]
    fn cwd_home_still_finds_gh_under_home() {
        let d = tmp("homecwd");
        let home = d.join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        script(&bin, "gh", "echo GOOD");
        let p = std::env::join_paths([bin.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&p), Some(&home), Some(&home)),
            Some(bin.join("gh"))
        );
        // A project below home is still excluded.
        let proj = home.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        script(&proj, "gh", "echo EVIL");
        let q = std::env::join_paths([proj.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&q), Some(&proj), Some(&home)), None);
    }

    /// #122 review: with cwd = home a writable project shim on PATH is still refused.
    #[test]
    fn cwd_home_refuses_a_project_shim_under_home() {
        let d = tmp("homeshim");
        let home = d.join("home");
        let proj = home.join("project/bin");
        std::fs::create_dir_all(&proj).unwrap();
        script(&proj, "gh", "echo EVIL");
        let p = std::env::join_paths([proj.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&p), Some(&home), Some(&home)), None);
        // ... also behind a symlink from an allowed directory.
        let lb = home.join(".local");
        std::fs::create_dir_all(&lb).unwrap();
        std::os::unix::fs::symlink(&proj, lb.join("bin")).unwrap();
        let q = std::env::join_paths([lb.join("bin").to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&q), Some(&home), Some(&home)), None);
    }

    /// P1: the final executable is checked, not just its directory.
    #[test]
    fn cwd_home_refuses_a_gh_symlink_into_a_project_file() {
        let d = tmp("ghlink");
        let home = d.join("home");
        let proj = home.join("project/bin");
        let lb = home.join(".local/bin");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&lb).unwrap();
        script(&proj, "gh", "echo EVIL");
        std::os::unix::fs::symlink(proj.join("gh"), lb.join("gh")).unwrap();
        let p = std::env::join_paths([lb.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&p), Some(&home), Some(&home)), None);
    }

    /// Control: a real gh, and a symlink to another file inside the allowed dir, still work.
    #[test]
    fn cwd_home_accepts_a_real_gh_and_an_in_dir_symlink() {
        let d = tmp("ghreal");
        let home = d.join("home");
        let lb = home.join(".local/bin");
        std::fs::create_dir_all(&lb).unwrap();
        script(&lb, "gh", "echo GOOD");
        let p = std::env::join_paths([lb.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&p), Some(&home), Some(&home)),
            Some(lb.join("gh"))
        );
        let lb2 = home.join(".cargo/bin");
        std::fs::create_dir_all(&lb2).unwrap();
        script(&lb2, "gh-real", "echo GOOD");
        std::os::unix::fs::symlink(lb2.join("gh-real"), lb2.join("gh")).unwrap();
        let q = std::env::join_paths([lb2.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&q), Some(&home), Some(&home)),
            Some(lb2.join("gh"))
        );
    }

    /// A `gh` symlink in a good directory that points into the project is refused too.
    #[test]
    fn a_gh_symlink_into_the_project_is_refused() {
        let d = tmp("projlink");
        let (proj, good) = (d.join("proj"), d.join("good"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        script(&proj, "gh", "echo EVIL");
        std::os::unix::fs::symlink(proj.join("gh"), good.join("gh")).unwrap();
        let p = std::env::join_paths([good.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_executable("gh", Some(&p), Some(&proj)), None);
    }

    #[test]
    fn a_project_or_relative_path_entry_never_supplies_gh() {
        let d = tmp("shim");
        let proj = d.join("proj");
        let good = d.join("good");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        script(&proj, "gh", "echo EVIL");
        script(&good, "gh", "echo GOOD");
        let sep = |v: &[&str]| std::env::join_paths(v).unwrap();
        // Relative entries are skipped.
        let rel = sep(&["bin", ".", "", good.to_str().unwrap()]);
        assert_eq!(
            trusted_executable("gh", Some(&rel), Some(&proj)),
            Some(good.join("gh"))
        );
        // An absolute entry inside the project directory is skipped too.
        let p = sep(&[proj.to_str().unwrap(), good.to_str().unwrap()]);
        assert_eq!(
            trusted_executable("gh", Some(&p), Some(&proj)),
            Some(good.join("gh"))
        );
        // Only untrusted entries: nothing.
        let only = sep(&[proj.to_str().unwrap(), "rel"]);
        assert_eq!(trusted_executable("gh", Some(&only), Some(&proj)), None);
    }

    #[test]
    fn yolo_start_notes_net_off() {
        // S11: yolo ignores `oxa.sandbox.net=off`; the start says so, and enforcement is unchanged.
        let note = yolo_net_notice(PermissionMode::Yolo, Some("off"), false).expect("a notice");
        assert!(
            note.contains("oxa.sandbox.net=off applies to auto/standard"),
            "{note}"
        );
        assert!(!note.contains('\n'));
        // Nothing to say when the network is on, when the setting is unset, in other modes, or
        // when offline already removes the bash network.
        assert_eq!(yolo_net_notice(PermissionMode::Yolo, None, false), None);
        assert_eq!(yolo_net_notice(PermissionMode::Yolo, Some("on"), false), None);
        assert_eq!(yolo_net_notice(PermissionMode::Auto, Some("off"), false), None);
        assert_eq!(yolo_net_notice(PermissionMode::Yolo, Some("off"), true), None);
    }
}
