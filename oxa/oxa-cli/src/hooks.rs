//! E-INT 4 (architect): the hook sets of a session, built once per process and shared by every
//! session the process opens (`/new`, `/resume`, fork).
//!
//! Order (it is the order in which the core fires them): JavaScript extensions, exec hooks,
//! MCP, web_search, wait probes (`wait_probe.rs`), `builtin:subagent`, codemode.
//!
//! * **Extensions** (`oxa-ext`): the paths `oxa_resources::discover` found
//!   (`Resources::extension_paths`: agent dir, trusted project, packages, settings, `-e`; `-ne`
//!   leaves only the `-e` paths). The flags they register are returned for the second pass of
//!   the argument parse (pi's two-pass parse; `dispatch::run`).
//! * **Exec hooks**: the `hooks` setting. Project settings are merged only for a trusted
//!   project (`Settings::load`), so an untrusted project cannot declare one.
//! * **MCP** unless `--no-mcp`: `mcp.json` (the project file only when trusted). Servers
//!   connect in the background. O-144: the client advertises no `sampling` capability and a
//!   server's `sampling/createMessage` is answered with JSON-RPC -32601 (`oxa-mcp`).
//! * **web_search** (`oxa-web`, O-169) unless `--no-web-search`, `OXA_WEB_SEARCH=0`,
//!   `oxa.webSearch.enabled: false` or `oxa.webSearch.provider: "none"`; the tool itself is
//!   declared per epoch only with a key and never offline.
//! * **builtin:subagent** (E11, O-146): on by default except on the Rust Flash engine and models
//!   below 32K (decided once per epoch: the tool is part of the frozen prefix, W3);
//!   `oxa.subagent.enabled` wins when set. Never in a subagent child (`HookInputs::nested`,
//!   depth 1). Agents: `<agent dir>/agents/*.md`, then the trusted project's
//!   `.oxa/agents/*.md` (`discover_agents`). The child binary is this executable
//!   (`std::env::current_exe`). It comes before codemode, so a codemode script can call it
//!   (through the same serial gate: one child at a time).
//! * **builtin:worker** (K5a): the `worker` tool (start, list, stop background workers in their
//!   own git worktrees). Not built in a worker child (`OXA_WORKER_DEPTH`); `oxa.workers.enabled`
//!   false (user settings only) switches it off. Shut down with the process (`Hooks::shutdown` stops the workers).
//! * **codemode**: off by default, because its tool is part of the frozen prefix (W3). It is
//!   on with the setting `codemode.enabled`, or when an enabled MCP server has codemode tools
//!   (`McpManager::wants_codemode`, pi's `autoEnableCodemode`); `tool_search` / `call_tool`
//!   with `codemode.toolSearch` or when a server has deferred tools. Both depend on
//!   configuration only, never on connection state.
use crate::workers::tool::{self as worker_tool, WorkerConfig, WorkerHooks};
use crate::Args;
use oxa_codemode::{CodemodeConfig, CodemodeHooks, Namespace};
use oxa_config::{Dirs, Settings};
use oxa_core::{discover_agents, SubagentHooks};
use oxa_ext::{parse_exec_hooks, ExecHooks, ExtensionHost, LoadOpts};
use oxa_mcp::{load_config, project_config_path, user_config_path, AccessGate, ManagerOpts, McpManager};
use oxa_perm::ModePolicy;
use oxa_proto::{Access, Decision, Exposure, Hooks, Level, Mode, OxaError, Policy, Sandbox, ToolSpec};
use oxa_resources::Resources;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Name of the extension host's log file inside the agent directory.
pub const EXTENSIONS_LOG: &str = "extensions.log";

/// The hook sets of a process.
#[derive(Clone, Default)]
pub struct HookSets {
    /// In firing order; what `Deps::hooks` gets.
    pub hooks: Vec<Arc<dyn Hooks>>,
    pub extensions: Option<Arc<ExtensionHost>>,
    pub mcp: Option<Arc<McpManager>>,
    /// Shared instance: the run timer publishes its exact deadline after startup.
    pub subagent: Option<Arc<SubagentHooks>>,
    /// K5a: the `worker` tool's hook set (start, list, stop; shut down with the process).
    pub workers: Option<Arc<WorkerHooks>>,
    /// The flags the loaded extensions registered (`registerFlag`), for the second pass of the
    /// argument parse.
    pub flags: Vec<oxa_proto::FlagSpec>,
}

/// What [`build_hooks`] reads.
pub struct HookInputs<'a> {
    pub args: &'a Args,
    pub dirs: &'a Dirs,
    pub cwd: &'a Path,
    pub settings: &'a Settings,
    pub resources: &'a Resources,
    pub sandbox: &'a Arc<dyn Sandbox>,
    pub trusted: bool,
    /// The same permission-table override the session uses.
    pub mode: Option<Mode>,
    /// This process is a subagent child (`OXA_SUBAGENT_DEPTH` / `OXA_SESSION_HINT`, see
    /// `child.rs`): `builtin:subagent` is not built (depth 1).
    pub nested: bool,
    /// The environment snapshot (`BRAVE_SEARCH_API_KEY`, `OXA_WEB_SEARCH`, test markers).
    pub env: &'a oxa_config::Env,
    /// The process's YOLO switch (O-182): the same one the policy of every session holds.
    pub yolo: oxa_proto::YoloSwitch,
}

/// The `worker` tool's hook set (K5a/K5b), or `None` when it is off (a worker child, the
/// `oxa.workers.enabled` setting (user settings only), no trusted `kill`, no executable); the reason is a notice.
/// One builder for every session that starts workers: the ordinary one and the live owner Lead
/// session ([`build_owner_hooks`]), so both use the same launch, limits (`oxa.workers.max`) and
/// supervisor.
#[derive(Clone, Copy)]
enum WorkerToolScope {
    /// The ordinary session's tool switches also constrain its worker.
    Inherit,
    /// The switches select the sole tool exposed to Lead, not its worker's tools.
    LeadExposure,
}

fn build_worker_hooks(
    i: &HookInputs<'_>,
    notices: &mut Vec<(Level, String)>,
    tool_scope: WorkerToolScope,
) -> Option<Arc<WorkerHooks>> {
    // Off in a worker child (`OXA_WORKER_DEPTH` set, even to an empty value: `Env::get` treats
    // empty as unset, so the pairs are read directly), under `--no-session` (a worker persists
    // its own session) and when `oxa.workers.enabled` is false (user settings only).
    let worker_depth = i
        .env
        .pairs()
        .iter()
        .rev()
        .find(|(k, _)| k == worker_tool::ENV_WORKER_DEPTH)
        .map(|(_, v)| v.as_str());
    if !i.args.no_session && worker_tool::built(worker_depth, worker_tool::enabled(i.settings)) {
        let agent_dir = if i.dirs.agent_dir.is_absolute() {
            i.dirs.agent_dir.clone()
        } else {
            i.cwd.join(&i.dirs.agent_dir)
        };
        let lead_dirs =
            (matches!(tool_scope, WorkerToolScope::LeadExposure) && i.trusted).then(|| oxa_config::Dirs {
                agent_dir: agent_dir.clone(),
                ..i.dirs.clone()
            });
        // The owner's model policy (user settings only); malformed parts are dropped with a notice.
        let (models, model_notices) = crate::workers::model_policy::from_settings(i.settings);
        // The HTTP owner opens one Session per message. Reuse its process-owned
        // supervisor before constructing WorkerCore, which pins a launcher copy
        // of the current executable as part of construction.
        if matches!(tool_scope, WorkerToolScope::LeadExposure) {
            if let Some(workers) = worker_tool::retained_owner_workers(
                &i.dirs.agent_dir,
                worker_tool::max_workers(i.settings),
                &i.yolo,
                lead_dirs.clone(),
                models.clone(),
            ) {
                return Some(workers);
            }
        }
        notices.extend(model_notices.into_iter().map(|n| (Level::Warning, n)));
        match std::env::current_exe() {
            Ok(bin) => {
                // The supervisor needs a trusted `kill` from the host PATH (never a project shim),
                // so no worker starts without a way to stop it.
                let host_path = std::ffi::OsString::from(i.env.get("PATH").unwrap_or(""));
                let project = i
                    .cwd
                    .canonicalize()
                    .ok()
                    .and_then(|cwd| {
                        crate::work_cmd::git_boundaries(&cwd).map(|(_, outer)| outer.to_path_buf())
                    })
                    .unwrap_or_else(|| i.cwd.to_path_buf());
                match crate::workers::proc::start(i.settings, &host_path, &project, &i.dirs.home) {
                    Ok(supervisor) => {
                        // The worker child writes its sessions where this session does.
                        let sessions_dir = i
                            .settings
                            .session_dir(&i.dirs.home, i.cwd)
                            .filter(|_| i.env.get("OXA_SESSION_DIR").is_none())
                            .unwrap_or_else(|| i.dirs.sessions_dir.clone());
                        // The child keeps the parent's allowance, exclusions and session
                        // directory: a worker is not a way around them.
                        let default_sessions = i.dirs.sessions_dir.clone();
                        let parent_scope = worker_tool::ParentScope {
                            max_tokens: i.args.max_tokens,
                            exclude_tools: i.args.exclude_tools.clone(),
                            session_dir: (sessions_dir != default_sessions).then(|| sessions_dir.clone()),
                            narrowing: crate::workers::launch::Narrowing {
                                offline: i.args.offline,
                                no_approve: i.args.approve == Some(false),
                                no_context_files: i.args.no_context_files,
                                // The live Lead's worker-only allowlist describes the tools exposed
                                // to Lead, not the tools its coding workers may use. Ordinary parent
                                // sessions still narrow their children exactly as before.
                                tools: matches!(tool_scope, WorkerToolScope::Inherit)
                                    .then_some(i.args.tools.clone())
                                    .flatten(),
                                no_tools: matches!(tool_scope, WorkerToolScope::Inherit) && i.args.no_tools,
                                no_builtin_tools: i.args.no_builtin_tools,
                                no_mcp: i.args.no_mcp,
                                no_web_search: i.args.no_web_search,
                                no_extensions: i.args.no_extensions,
                                no_skills: i.args.no_skills,
                                no_prompt_templates: i.args.no_prompt_templates,
                            },
                        };
                        let workers = Arc::new(WorkerHooks::new(WorkerConfig {
                            oxa_bin: bin,
                            cwd: i.cwd.to_path_buf(),
                            lead_dirs,
                            agent_dir,
                            max: worker_tool::max_workers(i.settings),
                            supervisor,
                            sessions_dir,
                            parent_env: i.env.pairs().to_vec(),
                            parent_scope,
                            yolo: i.yolo.clone(),
                            resume_on_start: crate::workers::resume::enabled(i.settings),
                            advertise_wait: matches!(tool_scope, WorkerToolScope::Inherit),
                            send_ack_seconds: worker_tool::send_ack_seconds(i.settings),
                            models,
                            cache: crate::worker_cache::view(i.settings, &i.dirs.home, i.env, i.cwd),
                        }));
                        return Some(workers);
                    }
                    Err(e) => notices.push((Level::Warning, format!("worker: the tool is off ({e})"))),
                }
            }
            Err(e) => notices.push((
                Level::Warning,
                format!("worker: the oxa executable is not known ({e}); the tool is off"),
            )),
        }
    }
    None
}

/// The hook sets of the live, owner-authorized Lead session (`with_lead_owner`): only the
/// `worker` tool. No extensions, exec hooks, MCP or other tools; the shadow preset
/// (`args.lead`) never reaches this and stays read-only.
pub(crate) fn build_owner_hooks(i: &HookInputs<'_>, notices: &mut Vec<(Level, String)>) -> HookSets {
    let mut sets = HookSets::default();
    if let Some(workers) = build_worker_hooks(i, notices, WorkerToolScope::LeadExposure) {
        sets.hooks.push(workers.clone());
        sets.workers = Some(workers);
    }
    sets
}

/// Build the hook sets. Problems that do not stop the start (a failed extension, a bad hook
/// entry, an `mcp.json` diagnostic) are pushed to `notices`.
pub fn build_hooks(i: &HookInputs<'_>, notices: &mut Vec<(Level, String)>) -> Result<HookSets, OxaError> {
    if i.args.lead {
        return Ok(HookSets {
            hooks: vec![Arc::new(crate::lead_preset::LeadHooks)],
            ..HookSets::default()
        });
    }
    let mut sets = HookSets::default();

    // ---- extensions ----
    if !i.resources.extension_paths.is_empty() {
        match ExtensionHost::new(i.cwd.to_path_buf()) {
            Ok(host) => {
                host.state()
                    .set_log(Some(file_log(i.dirs.agent_dir.join(EXTENSIONS_LOG))));
                let opts = LoadOpts::default();
                for ext in host.load(&i.resources.extension_paths, &opts) {
                    if let Some(error) = ext.error {
                        notices.push((Level::Error, format!("extension {}: {error}", ext.path)));
                    }
                }
                // Load errors were just reported; do not report them a second time.
                let _ = host.state().take_errors();
                sets.flags = host.flags();
                let host = Arc::new(host);
                sets.hooks.push(host.clone());
                sets.extensions = Some(host);
            }
            Err(e) => notices.push((Level::Error, format!("extensions are not available: {e}"))),
        }
    }
    // ---- exec hooks ----
    if let Some(value) = i.settings.get("hooks") {
        let (decls, warnings) = parse_exec_hooks(value);
        notices.extend(warnings.into_iter().map(|w| (Level::Warning, w)));
        if !decls.is_empty() {
            sets.hooks.push(Arc::new(ExecHooks::new(
                decls,
                i.sandbox.clone(),
                i.cwd.to_path_buf(),
            )));
        }
    }

    // ---- MCP ----
    if !i.args.no_mcp {
        let project = project_config_path(i.cwd);
        let mut config = load_config(
            &user_config_path(&i.dirs.agent_dir),
            i.trusted.then_some(project.as_path()),
        );
        // SET-050: `httpProxy` (agent-directory settings only, pi settings.md) applies to the
        // MCP HTTP servers; the engine connection stays direct.
        if let Some(proxy) = Settings::load(i.dirs, i.cwd, false).string("httpProxy") {
            for s in &mut config.servers {
                if let oxa_mcp::ServerKind::Http(h) = &mut s.kind {
                    h.proxy = Some(proxy.clone());
                }
            }
        }
        notices.extend(
            config
                .diagnostics
                .iter()
                .map(|d| (Level::Warning, format!("mcp.json: {d}"))),
        );
        if !config.servers.is_empty() {
            let wait_for_direct = config.servers.iter().any(|s| {
                // A server declares direct tools by its own exposure or per tool
                // (`toolExposure`, MCP-013); both are frozen into the first prefix.
                s.enabled
                    && (s.exposure == Exposure::Direct
                        || s.tool_exposure.iter().any(|(_, e)| *e == Exposure::Direct))
            });
            let manager = Arc::new(McpManager::new(ManagerOpts {
                config,
                cwd: i.cwd.to_path_buf(),
                agent_dir: i.dirs.agent_dir.clone(),
                home: i.dirs.home.clone(),
                trusted: i.trusted,
                gate: configured_server_gate(
                    i.mode
                        .or_else(|| i.settings.string("oxa.mode").as_deref().and_then(Mode::parse))
                        .unwrap_or(Mode::Workspace),
                    i.cwd,
                ),
                client_version: env!("CARGO_PKG_VERSION").to_string(),
            }));
            manager.start();
            if wait_for_direct {
                // Declared (direct) tools are read once, when the first prefix is built (W3).
                manager.wait_for_direct(Duration::from_secs(oxa_mcp::FIRST_PROMPT_WAIT_S));
            }
            sets.hooks.push(manager.clone());
            sets.mcp = Some(manager);
        }
    }

    // ---- web_search (O-169) ----
    // Before codemode, so a codemode script can call it. The hook set decides per epoch whether
    // the tool is declared (a key, not offline); here only the switches are read.
    if oxa_web::enabled(
        i.args.no_web_search,
        i.env.get(oxa_web::ENV_SWITCH),
        i.settings.bool(oxa_web::SETTING_ENABLED, true),
        i.settings.string(oxa_web::SETTING_PROVIDER).as_deref(),
    ) {
        let max_tokens = i
            .settings
            .get(oxa_web::SETTING_MAX_TOKENS)
            .and_then(|v| v.as_u64())
            .map(|n| n.min(u64::from(u32::MAX)) as u32);
        let get = |name: &str| i.env.get(name).map(str::to_string);
        let (cfg, warnings) = oxa_web::WebConfig::from_env(&get, &i.dirs.agent_dir, max_tokens);
        notices.extend(warnings.into_iter().map(|w| (Level::Warning, w)));
        let web = oxa_web::WebSearchHooks::new(cfg);
        if let Some(text) = web.startup_notice() {
            notices.push((Level::Warning, text));
        }
        sets.hooks.push(Arc::new(web));
    }

    // ---- wait probes (W3) ----
    // `wait` with `until` kind `ci` / `pr_merged` / `pr_activity`: answered through `gh`, no tools or prompt text.
    sets.hooks
        .push(Arc::new(crate::wait_probe::ProbeHooks::new(i.cwd, &i.dirs.home)));

    // ---- builtin:subagent ----
    // O-146: built unless switched off; the hook set itself declares the tool per epoch (on by
    // default, off on Flash and below 32K: `SubagentHooks::declared_for`).
    if !i.nested
        && i.settings
            .get(SubagentHooks::SETTING_ENABLED)
            .and_then(|v| v.as_bool())
            != Some(false)
    {
        match std::env::current_exe() {
            Ok(bin) => {
                let agents = discover_agents(i.dirs, i.cwd, i.trusted);
                let subagent = Arc::new(
                    SubagentHooks::new(bin, agents)
                        .with_settings(i.settings)
                        .with_tool_exclusions(i.args.exclude_tools.clone())
                        .with_yolo(i.yolo.clone()),
                );
                sets.hooks.push(subagent.clone());
                sets.subagent = Some(subagent);
            }
            Err(e) => notices.push((
                Level::Warning,
                format!("subagent: the oxa executable is not known ({e}); the tool is off"),
            )),
        }
    }

    // ---- builtin:worker (K5a) ----
    if let Some(workers) = build_worker_hooks(i, notices, WorkerToolScope::Inherit) {
        sets.hooks.push(workers.clone());
        sets.workers = Some(workers);
    }

    // ---- codemode ----
    let mcp = sets.mcp.clone();
    // pi activates the built-in tools through `defaultTools` / `--tools` (`+codemode`); a tool
    // named there must exist to be selected, so the hook set is built for it too.
    let named = |tool: &str| {
        i.settings
            .default_tools()
            .unwrap_or_default()
            .iter()
            .any(|t| t == tool)
            || i.args
                .tools
                .iter()
                .flatten()
                .any(|t| t.trim_start_matches('+') == tool)
    };
    let codemode = i.settings.bool("codemode.enabled", false)
        || named("codemode")
        || mcp.as_ref().is_some_and(|m| m.wants_codemode());
    let tool_search = i.settings.bool("codemode.toolSearch", false)
        || named("tool_search")
        || mcp.as_ref().is_some_and(|m| m.wants_tool_search());
    if codemode || tool_search {
        let config = CodemodeConfig {
            codemode,
            tool_search,
            // pi's `PI_CODEMODE_MAX_WORKERS` (here `OXA_CODEMODE_MAX_WORKERS`) overrides the
            // limit; then the oxa setting; 0 = the default of two.
            max_workers: i
                .env
                .get("OXA_CODEMODE_MAX_WORKERS")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or_else(|| usize::try_from(i.settings.u32("codemode.maxWorkers", 0)).unwrap_or(0)),
        };
        let sources: Vec<Arc<dyn Hooks>> = sets.hooks.clone();
        let cat_mcp = mcp.clone();
        let waited = std::sync::atomic::AtomicBool::new(false);
        let catalog = Arc::new(move || {
            // Codemode tools never touch the prefix, so servers connect in the background and
            // nothing waits for them at start. The first script (or search) gives the servers
            // that are still connecting a bounded moment, once, so that a script in the first
            // turn of a run finds its tools.
            if let Some(m) = &cat_mcp {
                if !waited.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    wait_while_connecting(m, CODEMODE_CONNECT_WAIT);
                }
            }
            catalog_of(&sources)
        });
        let ns_mcp = mcp.clone();
        let namespaces = Arc::new(move || match &ns_mcp {
            Some(m) => m
                .namespaces()
                .into_iter()
                .map(|(name, description, instructions)| Namespace {
                    name,
                    description,
                    instructions,
                })
                .collect(),
            None => Vec::new(),
        });
        sets.hooks.push(Arc::new(CodemodeHooks::new(
            config,
            catalog,
            namespaces,
            i.dirs.tmp_dir.join("codemode"),
        )));
    }
    Ok(sets)
}

/// How long the first codemode script waits for MCP servers that are still connecting.
const CODEMODE_CONNECT_WAIT: Duration = Duration::from_secs(5);

/// Wait until no server is pending or connecting, at most `limit`.
fn wait_while_connecting(m: &McpManager, limit: Duration) {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline
        && m.servers().iter().any(|s| {
            matches!(
                s.state,
                oxa_mcp::ServerState::Pending | oxa_mcp::ServerState::Connecting
            )
        })
    {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Every tool a codemode script may call right now: the built-ins and the tools of the hook
/// sets built before codemode (extensions, MCP with any exposure except `Hidden`), sorted by
/// name, the first of a name winning.
fn catalog_of(sources: &[Arc<dyn Hooks>]) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = oxa_tools::builtin_tools()
        .iter()
        .map(|t| t.spec().clone())
        .collect();
    for h in sources {
        specs.extend(h.tools().iter().map(|t| t.spec().clone()));
    }
    specs.retain(|s| s.exposure != Exposure::Hidden);
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs.dedup_by(|later, first| later.name == first.name);
    specs
}

/// The gate for starting or connecting a configured MCP server. A server is in `mcp.json`
/// because the user put it there (`oxa mcp add`, or a project the user trusted), so the
/// configuration is the approval: what the policy would ask about is allowed, what it denies
/// in the session's mode (plan mode starts nothing) stays denied.
fn configured_server_gate(mode: Mode, cwd: &Path) -> AccessGate {
    let cwd = cwd.to_path_buf();
    Arc::new(move |access: &Access| {
        !matches!(ModePolicy::new().decide(mode, access, &cwd), Decision::Deny(_))
    })
}

/// A log sink that appends `<level> <line>` to `path`. Failures are ignored: logging never
/// breaks a session.
fn file_log(path: PathBuf) -> oxa_ext::ExtLog {
    Arc::new(move |level: Level, line: &str| {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{} {}", level.as_str(), line.replace('\n', " "));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        args: &'a Args,
        dirs: &'a Dirs,
        cwd: &'a Path,
        settings: &'a Settings,
        resources: &'a Resources,
        sandbox: &'a Arc<dyn Sandbox>,
        env: &'a oxa_config::Env,
    ) -> HookInputs<'a> {
        HookInputs {
            args,
            dirs,
            cwd,
            settings,
            resources,
            sandbox,
            trusted: false,
            mode: args.permission_mode,
            nested: false,
            env,
            yolo: oxa_proto::YoloSwitch::new(oxa_proto::PermissionMode::Standard.autonomy()),
        }
    }

    fn tool_names(sets: &HookSets) -> Vec<String> {
        sets.hooks
            .iter()
            .flat_map(|h| h.tools())
            .map(|t| t.spec().name.clone())
            .collect()
    }

    #[test]
    fn live_lead_session_exposes_worker_tool() {
        let test = oxa_fake::TestEnv::new();
        let dirs = Dirs::under(test.root());
        let settings = Settings::default();
        let resources = Resources::default();
        let sandbox: Arc<dyn Sandbox> = Arc::new(oxa_sandbox::NoSandbox);
        let env = oxa_config::Env::from_pairs(&[("PATH", "/usr/bin:/bin")]);
        let args = Args {
            tools: Some(vec![worker_tool::TOOL_NAME.to_string()]),
            no_tools: true,
            offline: true,
            ..Args::default()
        };
        let cwd = test.work();
        let mut notices = Vec::new();
        let sets = build_owner_hooks(
            &inputs(&args, &dirs, &cwd, &settings, &resources, &sandbox, &env),
            &mut notices,
        );
        assert!(sets.workers.is_some(), "{notices:?}");
        assert_eq!(tool_names(&sets), vec!["worker".to_string()]);
        assert!(sets
            .hooks
            .iter()
            .flat_map(|h| h.tools())
            .all(|t| !t.spec().description.contains("worker_done")));
        let child_scope = sets.workers.as_ref().unwrap().parent_narrowing();
        assert_eq!(child_scope.tools, None, "Lead's allowlist leaked into its worker");
        assert!(
            !child_scope.no_tools,
            "Lead's no-tools baseline leaked into its worker"
        );
        assert!(
            child_scope.offline,
            "the parent's execution policy was not inherited"
        );
        let (child_args, _) = crate::workers::launch::child_argv(
            &crate::workers::launch::WorkerSpec {
                name: "lead-child".into(),
                session_id: "synthetic-session".into(),
                model: "synthetic-model".into(),
                goal_file: "/synthetic/goal".into(),
                mode: None,
                max_tokens: None,
                exclude_tools: Vec::new(),
                session_dir: None,
                narrowing: child_scope,
            },
            oxa_proto::Mode::Workspace,
            oxa_proto::Autonomy::Standard,
        )
        .unwrap();
        assert!(child_args.iter().any(|arg| arg == "--offline"));
        assert!(!child_args
            .iter()
            .any(|arg| arg == "--tools=worker" || arg == "--no-tools"));
        // Only the worker hook set: no extensions, MCP or subagent in the owner session.
        assert!(sets.extensions.is_none() && sets.mcp.is_none() && sets.subagent.is_none());
        // The same off switches as everywhere: a worker child gets none.
        let nested =
            oxa_config::Env::from_pairs(&[("PATH", "/usr/bin:/bin"), (worker_tool::ENV_WORKER_DEPTH, "1")]);
        let none = build_owner_hooks(
            &inputs(&args, &dirs, &cwd, &settings, &resources, &sandbox, &nested),
            &mut notices,
        );
        assert!(none.workers.is_none() && none.hooks.is_empty());
        // The worker settings are user-only: a trusted project file cannot give the owner
        // session the tool when the user turned it off.
        std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
        std::fs::write(dirs.settings_file(), r#"{"oxa":{"workers":{"enabled":false}}}"#).unwrap();
        test.write(
            "work/.oxa/settings.json",
            r#"{"oxa":{"workers":{"enabled":true}}}"#,
        );
        let layered = Settings::load(&dirs, &cwd, true);
        let denied = build_owner_hooks(
            &inputs(&args, &dirs, &cwd, &layered, &resources, &sandbox, &env),
            &mut notices,
        );
        assert!(denied.workers.is_none() && denied.hooks.is_empty());
    }

    #[test]
    fn shadow_lead_preset_stays_read_only_without_worker_tool() {
        let test = oxa_fake::TestEnv::new();
        let dirs = Dirs::under(test.root());
        let settings = Settings::default();
        let resources = Resources::default();
        let sandbox: Arc<dyn Sandbox> = Arc::new(oxa_sandbox::NoSandbox);
        let env = oxa_config::Env::from_pairs(&[("PATH", "/usr/bin:/bin")]);
        let args = Args {
            lead: true,
            shadow: true,
            ..Args::default()
        };
        let cwd = test.work();
        let mut notices = Vec::new();
        let sets = build_hooks(
            &inputs(&args, &dirs, &cwd, &settings, &resources, &sandbox, &env),
            &mut notices,
        )
        .unwrap();
        assert!(sets.workers.is_none());
        assert!(tool_names(&sets).is_empty());
        assert_eq!(sets.hooks.len(), 1);
        assert!(sets.hooks.iter().all(|h| !h.allows_goals()));
    }
}
