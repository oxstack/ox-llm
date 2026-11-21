//! D13: mode choice and wiring.
// ported from pi packages/coding-agent/src/main.ts
pub(crate) mod lead_owner;
mod mode;

pub(crate) use lead_owner::lead_model;
pub use lead_owner::{with_lead_owner, LeadOwnerBinding};
use mode::headless_argument_error;
pub use mode::{choose_mode, RunMode};

use crate::hooks::{build_hooks, HookInputs, HookSets};
use crate::{parse_args, run_config, run_local, run_mcp, run_pkg, usage_text, Args, Subcommand};
use oxa_config::{
    dirs_from_env, resolve_model, Dirs, Endpoint, Env, Keymap, ModelRef, ModelsConfig, Settings,
};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_export::{default_output_name, export_html, format_for, ExportFormat, ExportOpts, ExportTheme};
use oxa_interactive::{
    ask_trust_interactive, image_part, run as run_interactive, ClipImage, InteractiveOpts, Reopen, ReopenFn,
    TrustChoice,
};
use oxa_perm::{
    gated_resources, resolve_trust, ApprovalDesk, DangerRules, DefaultTrust, ModePolicy, TrustInputs,
    TrustOutcome, TrustStore, UnattendedMode,
};
use oxa_proto::{codes, Engine, EntryBody, Level, Mode, OxaError, Part, Sandbox, ThinkingLevel, Value};
use oxa_resources::{discover, DiscoverOpts, Resources};
use oxa_rpc::{
    confirm_before_start, run_json_outcome_with_inbox, run_print_outcome_with_inbox, serve_outcome_gated, Out,
};
use oxa_sandbox::Bwrap;
use oxa_sandbox::NoSandbox;
use oxa_session::{decode_entry, decode_header, FsStore, MemStore, SessionStore, Tree};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) fn is_agent(env: &Env) -> bool {
    // Marker presence, including an empty value, is enough to refuse.
    env.pairs().iter().any(|(name, _)| {
        matches!(
            name.as_str(),
            "OXA_CODING_AGENT" | "AI_AGENT" | "OXA_SUBAGENT_DEPTH"
        )
    })
}

/// The process's standard streams and terminal facts, injectable for tests.
pub struct Stdio {
    pub stdin: Box<dyn Read + Send>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    /// The process working directory.
    pub cwd: std::path::PathBuf,
}

impl Stdio {
    /// The real streams (`libc::isatty` on fds 0 and 1, `std::env::current_dir`).
    pub fn real() -> Stdio {
        use std::io::IsTerminal;
        Stdio {
            stdin: Box::new(std::io::stdin()),
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            stdin_is_tty: std::io::stdin().is_terminal(),
            stdout_is_tty: std::io::stdout().is_terminal(),
            // An empty path means the working directory is gone; `run` reports it.
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }
}

pub(crate) const USAGE_HINT: &str = "Run `oxa --help` for the list of options.";

/// The whole program. Returns the exit code; never panics on user error.
///
/// Order: parse args (two passes for extension flags) -> `--help` / `--version` ->
/// subcommands (`mcp`, packages, `config`) -> `--export` -> resolve `Dirs`, trust, settings,
/// resources -> build hook sets (extensions, exec hooks, MCP unless `--no-mcp`, codemode,
/// subagent when enabled) -> engine (`HttpEngine`; down = one-line error, exit 2) ->
/// `--list-models` -> sandbox (`Bwrap::detect`, else `NoSandbox` + notice) -> `Session::open`
/// per the session flags -> the chosen mode. Piped stdin text is prepended to the first
/// prompt; `@file` text is included as pi does and images become `Part::Image`.
pub fn run(argv: &[String], env: &Env, io: Stdio) -> i32 {
    let Stdio {
        mut stdin,
        mut stdout,
        mut stderr,
        stdin_is_tty,
        stdout_is_tty,
        cwd,
    } = io;

    if let Some(help) = crate::help::from_argv(argv) {
        return match help {
            Ok(text) => {
                if stdout.write_all(text.as_bytes()).is_ok() {
                    0
                } else {
                    2
                }
            }
            Err(message) => {
                let _ = writeln!(stderr, "{message}");
                2
            }
        };
    }
    let a = match parse_args(argv, &[]) {
        Ok(a) => a,
        Err(e) => {
            let headless = headless_argument_error(argv, stdin_is_tty, stdout_is_tty);
            return if headless {
                finish_outcome(Err(e), &mut *stderr)
            } else {
                report(&mut *stderr, &e)
            };
        }
    };
    if a.help {
        let flags = help_extension_flags(&a, env, &cwd, &mut *stderr);
        let _ = stdout.write_all(usage_text(&flags).as_bytes());
        return 0;
    }
    if a.version {
        let _ = writeln!(stdout, "oxa {}", oxa_proto::BUILD_VERSION);
        return 0;
    }
    let headless = a.subcommand.is_none()
        && a.export.is_none()
        && a.list_models.is_none()
        && choose_mode(&a, stdin_is_tty, stdout_is_tty) != RunMode::Interactive;
    let report = |err: &mut dyn Write, error: &OxaError| {
        if headless {
            finish_outcome(Err(error.clone()), err)
        } else {
            report(err, error)
        }
    };
    if a.wire_evidence
        && (a.subcommand.is_some()
            || a.export.is_some()
            || a.list_models.is_some()
            || a.lead
            || choose_mode(&a, stdin_is_tty, stdout_is_tty) != RunMode::Interactive
            || is_agent(env)
            || crate::child::ChildIdentity::from_env(env, &mut Vec::new()).nested())
    {
        return report(
            &mut *stderr,
            &OxaError::new(
                codes::USAGE,
                "--wire-evidence needs a foreground owner terminal session",
            ),
        );
    }
    // A long flag oxa does not know may belong to an extension (`registerFlag`): the session
    // paths resolve it once the extensions are loaded (`build_hooks`). The paths that load no
    // extension (subcommands, `--export`, `--list-models`) refuse it here.
    let loads_extensions = a.subcommand.is_none() && a.export.is_none() && a.list_models.is_none();
    if let Some((name, _)) = a.unknown.first().filter(|_| !loads_extensions) {
        let e = OxaError::new(codes::USAGE, format!("Unknown option: --{name}")).with_hint(USAGE_HINT);
        return report(&mut *stderr, &e);
    }
    if cwd.as_os_str().is_empty() {
        let e = OxaError::new(codes::NOT_FOUND, "The current working directory no longer exists")
            .with_hint("change to an existing directory and try again");
        return report(&mut *stderr, &e);
    }

    crate::netguard::install(a.offline, env, a.session_dir.as_deref(), &cwd);

    #[cfg(feature = "share")]
    if !a.share_models.is_empty() || a.share_bind.is_some() {
        let refusal = if is_agent(env) {
            Some(OxaError::new(
                codes::INVALID,
                "Guest serving requires the owner outside an agent tool",
            ))
        } else if a.share_models.is_empty() {
            Some(OxaError::new(
                codes::USAGE,
                "--share-bind requires --share-model ALIAS",
            ))
        } else if !loads_extensions || choose_mode(&a, stdin_is_tty, stdout_is_tty) != RunMode::Interactive {
            Some(OxaError::new(
                codes::USAGE,
                "--share-model requires an interactive owner session",
            ))
        } else {
            oxa_proto::check_online("starting the LAN sharing listener").err()
        };
        if let Some(error) = refusal {
            return report(&mut *stderr, &error);
        }
    }

    if let Some(sub) = a.subcommand.clone() {
        if let Subcommand::Registered(oxa_interactive::command_registry::CommandId::Lead, words) = &sub {
            if !words.0.is_empty() {
                return finish(
                    crate::lead_chat::run_direct(&words.0, &cwd, env, &mut *stdout),
                    &mut *stderr,
                );
            }
            if !(stdin_is_tty && stdout_is_tty) {
                return report(
                    &mut *stderr,
                    &OxaError::new(
                        codes::USAGE,
                        "Lead needs a terminal. Use oxa lead -p <prompt> for print mode.",
                    ),
                );
            }
            let result = (|| {
                let binding = crate::lead_chat::local_binding(&cwd, env)?;
                let dirs = dirs_from_env(env, None)?;
                let mode = RunMode::Interactive;
                let home = crate::home_cwd::check(
                    mode,
                    &cwd,
                    env,
                    &dirs,
                    crate::home_cwd::explicit_opt_in(a.level),
                    &mut *stdin,
                    &mut *stdout,
                )?;
                let mut args = Args::default();
                let mut prep = binding.prepare(&mut args, env, mode, home.workspace)?;
                prep.notices.extend(home.warning.map(|w| (Level::Warning, w)));
                let session = binding.open(&prep, true)?;
                Ok((args, prep, session, binding))
            })();
            return match result {
                Ok((args, prep, session, binding)) => {
                    run_tui(&args, env, &cwd, prep, &mut *stderr, Some((session, binding)))
                }
                Err(error) => report(&mut *stderr, &error),
            };
        }

        if let Subcommand::Registered(oxa_interactive::command_registry::CommandId::LeadChat, words) = &sub {
            return finish(
                crate::lead_chat::run(&words.0, &cwd, env, &mut *stdout),
                &mut *stderr,
            );
        }
        let dirs = match dirs_from_env(env, None) {
            Ok(d) => d,
            Err(e) => return report(&mut *stderr, &e),
        };
        let result = match sub {
            Subcommand::Registered(id, words) => {
                use oxa_interactive::command_registry::CommandId;
                match id {
                    CommandId::LeadServe if words.0.is_empty() => {
                        crate::lead_serve::run(&dirs, &cwd, env, &mut *stdout)
                    }
                    CommandId::LeadRun => crate::lead_loop::run(&words.0, &dirs, &cwd, env, &mut *stdout),
                    CommandId::LeadAsks if words.0.is_empty() => {
                        crate::lead_loop::asks(&dirs, &cwd, env, &mut *stdout)
                    }
                    CommandId::Usage => crate::usage_cmd::run(&words.0, &dirs, &cwd, &mut *stdout),
                    CommandId::Cache => crate::cache_cmd::run(&words.0, &dirs, &cwd, env, &mut *stdout),
                    CommandId::PrThreads => crate::pr_threads_cmd::run(&words.0, &dirs, &cwd, &mut *stdout),
                    CommandId::WorkerRun | CommandId::WorkerList => crate::workers::run_cmd::run(
                        id,
                        &words.0,
                        &dirs,
                        &cwd,
                        env,
                        &mut *stdin,
                        &mut *stdout,
                        &mut *stderr,
                    ),
                    CommandId::Lead
                    | CommandId::LeadServe
                    | CommandId::LeadBoard
                    | CommandId::LeadStatus
                    | CommandId::LeadAsks
                    | CommandId::LeadChat
                    | CommandId::Goal
                    | CommandId::Admission
                    | CommandId::Help
                    | CommandId::Status
                    | CommandId::WorkStatus
                    | CommandId::WorkBoard
                    | CommandId::Send
                    | CommandId::SendApprovals
                    | CommandId::Memory
                    | CommandId::MemoryList
                    | CommandId::MemoryShow
                    | CommandId::MemoryRm
                    | CommandId::Models
                    | CommandId::Local
                    | CommandId::LocalAdd => Err(OxaError::new(
                        codes::USAGE,
                        "registered command handler or arguments are unavailable",
                    )),
                }
            }
            #[cfg(feature = "share")]
            Subcommand::Share(words) => {
                let serve = words.first().is_some_and(|word| word == "serve");
                if is_agent(env) {
                    return report(
                        &mut *stderr,
                        &OxaError::new(
                            codes::INVALID,
                            if serve {
                                "Guest serving requires the owner outside an agent tool"
                            } else {
                                "Guest key commands require the owner outside an agent tool"
                            },
                        ),
                    );
                }
                if serve {
                    return match crate::share_cmd::run(&words[1..], &dirs, &cwd, &mut *stdout) {
                        Ok(()) => 0,
                        Err(error) => report(&mut *stderr, &error),
                    };
                }
                oxc_gateway::share::command::run(&words, &dirs.home.join(".oxa/share"), &mut *stdout)
                    .map(|()| 0)
                    .map_err(|error| {
                        let code = if matches!(error, oxc_gateway::share::command::Error::Usage) {
                            codes::USAGE
                        } else {
                            codes::INVALID
                        };
                        OxaError::new(code, error.to_string())
                    })
            }
            Subcommand::Login(logout, words) => crate::login_cmd::run(
                logout,
                &words.0,
                &dirs,
                &cwd,
                env,
                stdin_is_tty && stdout_is_tty,
                &mut *stdout,
            ),
            Subcommand::Status(words) => crate::status::run(&words, &dirs, &cwd, env, &mut *stdout),
            Subcommand::Models(words) => {
                crate::models_cmd::run(&words, &dirs, &cwd, env, &mut *stdout, &mut *stderr)
            }
            Subcommand::Work(words) => {
                return crate::work_cmd::run(&words, &cwd, env, &dirs, &mut *stdout, &mut *stderr)
            }
            Subcommand::Mcp(words) => run_mcp(&words, &dirs, &cwd, &mut *stdout, &mut *stderr),
            Subcommand::Memory(words) => crate::run_memory(&words, &dirs, &mut *stdout, &mut *stderr),
            Subcommand::Pkg(verb, words) => run_pkg(&verb, &words, &dirs, &cwd, &mut *stdout, &mut *stderr),
            Subcommand::Config(words) => run_config(&words, &dirs, &cwd, &mut *stdout),
            Subcommand::Local(_) if is_agent(env) => Err(OxaError::new(
                codes::INVALID,
                "Local model setup requires the owner outside an agent tool",
            )),
            Subcommand::Local(words) => run_local(&words, &dirs, &cwd, &mut *stdout),
            // #1512 S5: a follow-up (with its delivery receipt), an owner answer to an approval
            // request, or the request list; `approve_cmd` parses every form.
            Subcommand::Send(words) => {
                crate::approve_cmd::run(&words, &dirs, &cwd, env, &mut *stdout, &mut *stdin)
            }
            Subcommand::Identity(words) => crate::run_identity(&words, &dirs, env, &mut *stdout),
            Subcommand::ReportIssue(words) => crate::run_report_issue(&words, &dirs, &cwd, env, &mut *stdout),
            Subcommand::Wipe(words) => crate::run_wipe(
                &words,
                &dirs,
                &cwd,
                env,
                crate::WipeIo {
                    stdin: &mut *stdin,
                    stdin_is_tty,
                    out: &mut *stdout,
                    err: &mut *stderr,
                },
            ),
        };
        return finish(result, &mut *stderr);
    }

    if let Some((input, output)) = &a.export {
        return export_session(
            input,
            output.as_deref(),
            Path::new(env.get("HOME").unwrap_or("")),
            &cwd,
            &mut *stdout,
            &mut *stderr,
        );
    }

    let dirs = match dirs_from_env(env, a.session_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => return report(&mut *stderr, &e),
    };

    if let Some(search) = &a.list_models {
        let (models, diags) = ModelsConfig::load(&dirs, env);
        write_diagnostics(
            &mut *stderr,
            diags
                .iter()
                .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
        );
        let endpoint = match default_endpoint(&models) {
            Ok(ep) => with_api_key(ep, &a),
            Err(e) => return report(&mut *stderr, &e),
        };
        let user = Settings::load(&dirs, &cwd, false);
        let (engine, catalog_notices) =
            match crate::in_process::select_catalog(&user, &dirs, || fallback_engine(&models, &endpoint)) {
                Ok(e) => e,
                Err(e) => return report(&mut *stderr, &e),
            };
        write_diagnostics(
            &mut *stderr,
            catalog_notices.into_iter().map(|text| (Level::Info, text)),
        );
        let result = list_models(
            engine.as_ref(),
            &endpoint.name,
            search.as_deref(),
            &mut *stdout,
            &mut *stderr,
        );
        return finish(result, &mut *stderr);
    }

    let mode = choose_mode(&a, stdin_is_tty, stdout_is_tty);
    if mode == RunMode::Rpc && !a.files.is_empty() {
        let e = OxaError::new(codes::USAGE, "@file arguments are not supported in RPC mode")
            .with_hint(USAGE_HINT);
        return report(&mut *stderr, &e);
    }
    // Item 3 (sec-prod): a session in the home directory or above it needs the user's say-so.
    let home = match crate::home_cwd::check(
        mode,
        &cwd,
        env,
        &dirs,
        crate::home_cwd::explicit_opt_in(a.level),
        &mut *stdin,
        &mut *stdout,
    ) {
        Ok(h) => h,
        Err(e) => return report(&mut *stderr, &e),
    };
    let home_workspace = home.workspace;

    // CLI-019: pi's question when `--session <id>` names a session of another directory
    // (main.ts `promptConfirm`): the notice and the question on stdout, one answer line from
    // stdin (read byte by byte, so piped prompt text after it stays unread).
    let mut a = a;
    // `oxa.a2a.enabled` (user layer only): the local agent channel and control lane.
    let a2a_enabled = Settings::load(&dirs, &cwd, false).a2a_enabled();
    #[cfg(target_os = "linux")]
    let managed_steering = match mode {
        RunMode::Json => {
            use oxa_work::managed_steering::{ManagedSteering, FD_ENV};
            use std::os::fd::AsFd;
            if !a2a_enabled && env.get(FD_ENV).is_some() {
                return finish_outcome(
                    Err(OxaError::new(
                        codes::INVALID,
                        "native steering needs the agent channel; set oxa.a2a.enabled to true in the user settings",
                    )),
                    &mut *stderr,
                );
            }
            match env
                .get(FD_ENV)
                .map(|fd| ManagedSteering::receive(fd, &dirs.agent_dir, std::io::stdin().as_fd()))
                .transpose()
            {
                Ok(authority) => authority,
                Err(error) => return finish_outcome(Err(OxaError::new(codes::INVALID, error)), &mut *stderr),
            }
        }
        _ => None,
    };
    let cloud_diagnostics = oxc_gateway::diagnostics::Diagnostics::default();
    // Input the owner-question handshake read past its own answer; the RPC server replays it.
    let mut rpc_early: Vec<u8> = Vec::new();
    let prepared = {
        let mut confirm = |notice: &str, question: &str| -> bool {
            if mode == RunMode::Rpc {
                if !rpc_owner_present(mode, env) {
                    return false;
                }
                return confirm_before_start(&mut *stdin, &mut *stdout, &mut rpc_early, notice, question);
            }
            let _ = writeln!(stdout, "{notice}");
            let _ = write!(stdout, "{question} [y/N] ");
            let _ = stdout.flush();
            let answer = read_line(&mut *stdin);
            let answer = answer.trim().to_lowercase();
            answer == "y" || answer == "yes"
        };
        prepare(
            &mut a,
            env,
            &dirs,
            &cwd,
            mode,
            home_workspace,
            stdin_is_tty,
            stdout_is_tty,
            &mut confirm,
            cloud_diagnostics.clone(),
            None,
        )
    };
    let mut prep = match prepared {
        Ok(p) => p,
        Err(e) if e.code == FORK_DECLINED => {
            // The RPC stream carries JSON records only, and a declined offer is a normal
            // outcome there: the sentence goes to stderr.
            if mode == RunMode::Rpc {
                let _ = writeln!(stderr, "Aborted.");
            } else {
                let _ = writeln!(stdout, "Aborted.");
            }
            return if headless {
                finish_outcome(Ok(oxa_core::RunOutcome::Interrupted), &mut *stderr)
            } else {
                0
            };
        }
        Err(e) => {
            crate::in_process::report_diagnostics(&cloud_diagnostics, &dirs, &mut *stderr);
            return report(&mut *stderr, &e);
        }
    };
    prep.notices.extend(home.warning.map(|w| (Level::Warning, w)));
    // Second pass (pi): the long flags the first pass did not know are parsed again with the
    // flags the loaded extensions registered; one that nobody registered is a usage error.
    let a = if a.unknown.is_empty() {
        a
    } else {
        let again = match parse_args(argv, &prep.built.hook_sets.flags) {
            Ok(again) => again,
            Err(e) => return report(&mut *stderr, &e),
        };
        if let Some((name, _)) = again.unknown.first() {
            let e = OxaError::new(codes::USAGE, format!("Unknown option: --{name}")).with_hint(USAGE_HINT);
            return report(&mut *stderr, &e);
        }
        let again = resolve_continue_word(&again, &prep.built.dirs).unwrap_or(again);
        for (name, value) in &again.extension_flags {
            for h in &prep.built.hook_sets.hooks {
                h.set_flag(name, value);
            }
        }
        again
    };

    match mode {
        RunMode::Interactive => run_tui(&a, env, &cwd, prep, &mut *stderr, None),
        RunMode::Rpc => {
            let dirs = prep.built.dirs.clone();
            let replay = std::io::Cursor::new(rpc_early).chain(stdin);
            run_rpc(
                prep,
                a.max_time,
                a.max_tokens,
                dirs,
                Box::new(replay),
                stdout,
                &mut *stderr,
                a2a_enabled,
            )
        }
        RunMode::Print | RunMode::Json => {
            let stdin_text = match read_piped_stdin(&mut *stdin, stdin_is_tty) {
                Ok(t) => t,
                Err(e) => return report(&mut *stderr, &e),
            };
            let prompts = match build_prompts(&a, &dirs, &cwd, stdin_text) {
                Ok(p) => p,
                Err(e) => return report(&mut *stderr, &e),
            };
            if prompts.is_empty() && a.goal.is_none() && !a.continue_ && !a.resume {
                let e = OxaError::new(codes::USAGE, "no prompt: give a message, an @file or piped stdin")
                    .with_hint(USAGE_HINT);
                return report(&mut *stderr, &e);
            }
            run_headless(
                mode,
                a.max_time,
                a.max_tokens,
                prep,
                prompts,
                stdout,
                &mut *stderr,
                #[cfg(target_os = "linux")]
                managed_steering,
            )
        }
    }
}

/// Everything decided before a session is opened.
struct Prep {
    built: Built,
    #[cfg(feature = "share")]
    sharing: Option<crate::share_cmd::Prepared>,
    open: Open,
    session_id: Option<String>,
    notices: Vec<(Level, String)>,
    /// The gated project resources (for `/trust`).
    gated: Vec<String>,
    /// CLI-024: `-n/--name`, trimmed and non-empty; written as a `session_info` entry once
    /// the session is open.
    name: Option<String>,
    /// Explicit user continuation may resume a paused, unexhausted stored goal.
    continue_goal: bool,
}

/// Startup warnings after opening the session, so restored/selected model limits are final.
/// No extra engine requests: context tokens are a cheap estimate (four UTF-8 bytes per token).
fn startup_notices(built: &Built, session: &Session) -> Vec<(Level, String)> {
    const CONTEXT_WARNING_PERCENT: u64 = 5;
    let mut notices: Vec<_> = built
        .engine
        .handshake_warning()
        .map(|w| (Level::Warning, w))
        .into_iter()
        .collect();
    let state = session.state();
    let mut total = 0_u64;
    let mut largest = None;
    for file in &session.inner().resources.context_files {
        let tokens = file.content.len().div_ceil(4) as u64;
        total = total.saturating_add(tokens);
        if largest.is_none_or(|(_, size)| tokens > size) {
            largest = Some((file.path.as_path(), tokens));
        }
    }
    let window = u64::from(state.n_ctx);
    if window > 0 && total.saturating_mul(100) > window * CONTEXT_WARNING_PERCENT {
        if let Some((path, tokens)) = largest {
            let shown = if let Ok(rel) = path.strip_prefix(&built.cwd) {
                rel.display().to_string()
            } else if let Ok(rel) = path.strip_prefix(&built.dirs.home) {
                format!("~/{}", rel.display())
            } else {
                path.display().to_string()
            };
            notices.push((Level::Warning, format!(
                "context files use ~{total} tokens ({:.1}% of {}'s {window}-token window); largest: {shown} (~{tokens} tokens)",
                total as f64 * 100.0 / window as f64, state.model,
            )));
        }
    }
    notices
}

/// `Session::open`, then pi's `--name` (`appendSessionInfo`).
fn open_named(
    built: &Built,
    session_id: Option<String>,
    open: Open,
    name: Option<&str>,
) -> Result<Session, OxaError> {
    let mut session = Session::open(built.deps(session_id), open)?;
    // #1844 PR 2: a fork child continues its parent's prompt instead of prefilling its own. A
    // seed that cannot be used costs the child its fork, never the run: it works isolated.
    if let Some(path) = &built.fork_seed {
        let adopted = oxa_core::ForkSeed::read(path)
            .and_then(|seed| session.adopt_fork(&seed, &mut oxa_proto::VecSink::default()));
        if let Err(e) = adopted {
            eprintln!(
                "Warning: the fork seed was not used ({}); this child runs isolated and prefills its own context",
                e.message
            );
        }
    }
    // S1: the owner confirm broker. No broker (a bind failure) means no confirmation is possible.
    let _ = session.enable_owner_confirm(built.yolo.clone());
    session.set_output_schema(built.output_schema.clone());
    if let Some(goal) = &built.goal {
        session.set_goal(Some(goal.clone()), &mut oxa_proto::VecSink::default())?;
    }
    if let Some(name) = name {
        session.set_name(name, &mut oxa_proto::VecSink::default())?;
    }
    built.result_facts.attach(&mut session)?;
    Ok(session)
}

/// Open a session selected from the interactive UI and install the per-session services that
/// must survive a `/new`, `/resume`, or fork switch (including the recovery open after a switch
/// fails). The interactive worker closes the old session before it invokes this function.
fn open_interactive_session(base: &Built, deps: Deps, how: Open) -> Result<Session, OxaError> {
    let mut session = Session::open(deps, how)?;
    // No broker (a bind failure) means no confirmation is possible, matching the initial open.
    let _ = session.enable_owner_confirm(base.yolo.clone());
    Ok(session)
}

/// The parts of a session that do not depend on the open mode. Cloned into the interactive
/// `reopen` closure, which builds sessions for `/new`, `/resume`, fork and import.
#[derive(Clone)]
struct Built {
    lead: bool,
    status: Option<Arc<crate::status::StatusWriter>>,
    result_facts: Arc<crate::result_facts::ResultFacts>,
    output_schema: Option<oxa_core::output_schema::OutputSchema>,
    goal: Option<oxa_session::Goal>,
    engine: Arc<dyn Engine>,
    cloud_diagnostics: oxc_gateway::diagnostics::Diagnostics,
    store: Arc<dyn SessionStore>,
    sandbox: Arc<dyn Sandbox>,
    settings: Settings,
    dirs: Dirs,
    cwd: PathBuf,
    resources: Resources,
    theme_paths: Vec<PathBuf>,
    selection: ToolSelection,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    mode: Option<Mode>,
    /// A supervised subagent keeps SIGTERM as immediate supervisor cancellation.
    supervised_child: bool,
    /// #1844 PR 2: a fork child's seed (its parent's prefix and wire), installed before the
    /// first prompt so the first request continues the parent's conversation.
    fork_seed: Option<std::path::PathBuf>,
    /// RPC hosts opt into owner questions; unattended hosts need not handle UI requests.
    ask_owner_present: bool,
    /// #1552: an interactive session offers `report_publish`, whose approval dialog only a
    /// person's key press can answer; print, json and RPC runs keep the terminal confirmation.
    report_dialog: Option<Env>,
    /// The hook sets of the process (E-INT 4), shared by every session it opens.
    hook_sets: HookSets,
    /// O-182: the YOLO switch (`--yolo`, `/yolo`), shared by the policy of every session.
    yolo: oxa_proto::YoloSwitch,
    /// #202: the sandbox of the bash tool and its background jobs: the session's sandbox,
    /// bypassed while the switch is at yolo (full access).
    bash_sandbox: Arc<oxa_sandbox::FullAccess>,
    /// #1512 S3: this run requests the OS sandbox (`run_kind::os_sandbox_wanted`). The HUD
    /// combines this policy with platform support before showing it.
    os_sandbox: bool,
    /// #1512 S2: the dangerous-command rules of the user settings, shared by every session.
    danger: Arc<DangerRules>,
    /// #1512 S5: the approval desk under the agent directory (`None` when it cannot be opened).
    approval_desk: Option<Arc<ApprovalDesk>>,
    /// #1512 S5: what an unattended run does with a dangerous command (`oxa.approval.unattended`).
    unattended: UnattendedMode,
}

/// #597: `guard` plus the session's scratch directory (`/tmp` in its sandbox), which the file
/// tools read without asking.
fn session_guard(guard: oxa_perm::PathGuard, sandbox: &dyn Sandbox) -> oxa_perm::PathGuard {
    match sandbox.scratch_dir() {
        Some(dir) => guard.with_scratch(dir),
        None => guard,
    }
}

impl Built {
    /// #1512 S5: `policy` with the approval desk of this process, for `session_id` (bound again
    /// when the session has its id).
    fn desk_policy(&self, policy: ModePolicy, session_id: Option<String>) -> ModePolicy {
        match &self.approval_desk {
            Some(desk) => policy.with_desk(
                desk.clone(),
                session_id.unwrap_or_default(),
                self.unattended,
                self.os_sandbox,
            ),
            None => policy,
        }
    }

    /// Fresh `Deps` (the policy and built-in tools are not `Clone`).
    fn deps(&self, session_id: Option<String>) -> Deps {
        // #457: the session's own sandbox (its scratch directory at /tmp), dropped with it.
        let sandbox = self.sandbox.for_session().unwrap_or_else(|| self.sandbox.clone());
        // Issue #127 (b): one background-job registry per session, sharing the session's
        // sandbox. The session kills its jobs on close/shutdown.
        // `oxa.bash.maxJobs` (16 unless the owner set it).
        let jobs = Arc::new(
            oxa_tools::JobRegistry::with_max_jobs(self.settings.bash_max_jobs())
                .enforcing(self.settings.on_exceed(oxa_config::Budget::BashJobs).enforces()),
        );
        let bash_sandbox: Arc<dyn Sandbox> = Arc::new(self.bash_sandbox.with_inner(sandbox.clone()));
        jobs.set_sandbox(bash_sandbox.clone());
        // pi's `shellPath` and `shellCommandPrefix` (SET-055, SET-056).
        let mut tools = oxa_tools::builtin_tools_with_bash_sandbox(
            oxa_tools::ShellConfig {
                shell_path: self.settings.shell_path(&self.dirs.home),
                command_prefix: self.settings.shell_command_prefix(),
            },
            jobs.clone(),
            bash_sandbox,
        );
        // Owner questions belong to lead sessions (including unattended leads). Keep worker
        // declarations and their small frozen prefixes unchanged.
        if !self.supervised_child {
            tools.push(Arc::new(
                oxa_tools::AskTool::new(self.dirs.agent_dir.join("asks.jsonl"))
                    .with_owner_present(self.ask_owner_present),
            ));
            // #2293 A: the model records the file a request asks it to produce, and the run's
            // end record checks it once more. Evidence only: no gate, no exit code.
            tools.push(Arc::new(oxa_tools::DeliverableTool::new()));
        }
        if !self.supervised_child && !self.lead {
            if let Some(env) = &self.report_dialog {
                tools.push(Arc::new(
                    crate::PublishTool::new(
                        self.dirs.clone(),
                        env.clone(),
                        // Reporting is an owner-consent boundary.  In particular, a trusted
                        // project's settings must not enable semantic dedupe (and thereby send
                        // report text to GitHub and the judge) on the owner's behalf.
                        Settings::load(&self.dirs, &self.cwd, false),
                        self.cwd.clone(),
                    )
                    .with_yolo(self.yolo.clone()),
                ));
            }
        }
        if self.lead {
            tools.retain(|tool| crate::lead_preset::READ_TOOLS.contains(&tool.spec().name.as_str()));
        }
        Deps {
            engine: self.engine.clone(),
            store: self.store.clone(),
            tools,
            // Extensions (E1/E2), exec hooks (E5), MCP (E7/E9, F6), the built-in subagent (E11)
            // and codemode (E6); see `hooks.rs`.
            hooks: self.hook_sets.hooks.clone(),
            // B3: the file tools keep the deny-list and protected paths in standard/auto.
            // #202/#314: only user-selected yolo outside plan mode bypasses that floor.
            // #1512 S2: dangerous commands ask in standard and auto; the keys of "always allow"
            // are read from the user settings now, so a save covers the next session too.
            policy: Box::new(
                self.desk_policy(
                    ModePolicy::new()
                        .with_guard(session_guard(
                            oxa_perm::PathGuard::with_real_home(
                                &self.dirs.home,
                                oxa_sandbox::account_home().as_deref(),
                                &self.dirs.agent_dir,
                                &self.dirs.tmp_dir,
                            ),
                            sandbox.as_ref(),
                        ))
                        .with_yolo(self.yolo.clone())
                        .with_danger(
                            self.danger.clone(),
                            Settings::load(&self.dirs, &self.cwd, false).list("oxa.approval.always"),
                        ),
                    session_id.clone(),
                ),
            ),
            // File names follow bash's current view, without giving hooks/extensions the
            // bash-only execution bypass. The shared switch also handles live /yolo changes.
            sandbox: self.bash_sandbox.file_view(sandbox),
            settings: self.settings.clone(),
            dirs: self.dirs.clone(),
            cwd: self.cwd.clone(),
            selection: self.selection.clone(),
            resources: self.resources.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            mode: self.mode,
            session_id,
            jobs,
        }
    }
}

/// #1512 S2: the dangerous-command rules of the user settings. `[]` (the default) is every
/// built-in rule; a plain entry replaces the built-ins and `+id` / `-id` patch them. The raw
/// user entries are read: the merged value has already applied a `+x` / `-x` list to the empty
/// default, which would turn `["+cmd:x"]` into a plain list that drops every built-in rule.
fn dangerous_rules(user_settings: &Settings) -> (DangerRules, Vec<String>) {
    let builtin_ids: Vec<String> = oxa_perm::BUILTIN_RULES
        .iter()
        .map(|row| row.0.to_string())
        .collect();
    let patch = user_settings.user_list(oxa_perm::SETTING_DANGEROUS);
    let entries = if patch.is_empty() {
        builtin_ids
    } else {
        oxa_config::apply_list_patch(&builtin_ids, &patch)
    };
    DangerRules::from_list(&entries)
}

/// `--goal` runs in print, JSON and the interactive terminal; RPC has no goal path.
fn goal_flag_accepted(mode: RunMode) -> bool {
    matches!(mode, RunMode::Interactive | RunMode::Print | RunMode::Json)
}

/// The first queued message of a `--goal` run: the goal start, then the first prompt when there
/// is one, so the first request carries the objective before any continuation.
fn goal_first_prompt(start: String, first: Option<String>) -> String {
    match first {
        Some(text) => format!("{start}\n\n{text}"),
        None => start,
    }
}

/// The sandbox bash and the exec hooks use, plus a start notice when it had to be replaced. A
/// refused bash network (`oxa.sandbox.net: off`, offline) that this platform cannot enforce
/// cannot be honoured. Unattended runs and offline mode keep bash disabled, because the command
/// would keep the network the owner turned off. An interactive session with only the
/// `oxa.sandbox.net` setting keeps bash and warns that the network is not restricted: the owner
/// is there to see it (yolo plan, S10).
fn bash_sandbox(
    sandbox: Arc<dyn Sandbox>,
    interactive: bool,
    offline: bool,
) -> (Arc<dyn Sandbox>, Option<String>) {
    if !sandbox.network() && !sandbox.network_enforced() {
        if interactive && !offline {
            return (sandbox, Some(oxa_proto::NET_NOT_RESTRICTED_NOTICE.to_string()));
        }
        let notice = format!(
            "{}; bash is disabled while the bash network is off",
            oxa_proto::NET_NOT_RESTRICTED_NOTICE
        );
        return (Arc::new(NoSandbox), Some(notice));
    }
    (sandbox, None)
}

/// Keep the refusing inner executor on Linux. Only FullAccess may run model bash
/// with host rights; plan commands, extensions and project hooks still use the inner sandbox.
fn no_bwrap_sandbox(
    interactive: bool,
    os_sandbox: bool,
    reason: &str,
    env: &Env,
    dirs: &Dirs,
) -> (Arc<dyn Sandbox>, Option<(Level, String)>) {
    if cfg!(target_os = "linux") {
        return (
            Arc::new(NoSandbox),
            if interactive && !os_sandbox {
                Some((Level::Warning, format!("bwrap is missing or unusable ({reason}); bash runs with no isolation; exec hooks requiring bwrap are disabled")))
            } else {
                crate::run_kind::bwrap_missing_notice(os_sandbox, reason).map(|text| (Level::Warning, text))
            },
        );
    }
    // The same cleared base environment bwrap gives: credential variables stay out.
    let mut layout = oxa_sandbox::Layout::build(&oxa_sandbox::LayoutInputs {
        home: dirs.home.clone(),
        agent_dir: Some(dirs.agent_dir.clone()),
        path_var: env.get("PATH").unwrap_or("").to_string(),
        env: env.pairs().to_vec(),
        ..Default::default()
    });
    // Toolchain locations only: the sandbox's /tmp redirections do not apply here.
    layout.env = oxa_sandbox::toolchain_env(env.pairs());
    let why = oxa_sandbox::NO_OS_SANDBOX_NOTICE;
    (
        Arc::new(oxa_sandbox::Unsandboxed {
            layout,
            home: dirs.home.clone(),
        }),
        Some((
            Level::Warning,
            format!("{why}; bash and hooks run with no isolation"),
        )),
    )
}

/// Settings, trust, resources, engine, sandbox and the open mode. Notices are printed by the
/// caller once the session is open, so a failed start prints only its one error line.
fn prepare(
    a: &mut Args,
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    mode: RunMode,
    home_workspace: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    confirm: &mut dyn FnMut(&str, &str) -> bool,
    cloud_diagnostics: oxc_gateway::diagnostics::Diagnostics,
    owner_open: Option<(&oxa_session::SessionRef, Arc<dyn SessionStore>)>,
) -> Result<Prep, OxaError> {
    let owner_chat = owner_open.is_some();
    if a.goal.is_some() && !goal_flag_accepted(mode) {
        return Err(OxaError::new(
            codes::USAGE,
            "--goal currently requires print, JSON or interactive mode.",
        ));
    }
    let goal = crate::goal_cmd::load(a, cwd)?;
    let output_schema = if a.lead {
        Some(crate::lead_preset::schema()?)
    } else {
        a.output_schema
            .as_ref()
            .map(|file| {
                let path = cwd.join(file);
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| OxaError::io(&format!("read output schema {}", path.display()), &e))?;
                let value = serde_json::from_str(&text)
                    .map_err(|e| OxaError::new(codes::USAGE, format!("Invalid output schema JSON: {e}")))?;
                oxa_core::output_schema::OutputSchema::new(value)
            })
            .transpose()?
    };
    let mut notices: Vec<(Level, String)> = Vec::new();
    let user_settings = Settings::load(dirs, cwd, false);
    // #1512 S2: the dangerous-command rules come from the user settings only.
    let (danger, danger_warnings) = dangerous_rules(&user_settings);
    notices.extend(danger_warnings.into_iter().map(|w| (Level::Warning, w)));
    // #1512 S5: the unattended answer (user settings only) and the desk that files requests.
    let unattended_mode = UnattendedMode::parse(
        &user_settings
            .string("oxa.approval.unattended")
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    let approval_desk = ApprovalDesk::open(&dirs.agent_dir).ok().map(Arc::new);
    // The read-only proposal session is on by default; only the user's own `false` turns it off.
    if a.lead && !user_settings.bool(crate::lead_preset::SETTING, true) {
        return Err(OxaError::new(
            codes::USAGE,
            format!(
                "lead --shadow is turned off; remove \"{}\": false from the user settings.json to use it",
                crate::lead_preset::SETTING
            ),
        ));
    }
    let (models, diags) = ModelsConfig::load(dirs, env);
    notices.extend(
        diags
            .into_iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    let endpoint = with_api_key(default_endpoint(&models)?, a);
    // E11: a subagent child sends its requests under the parent's child hint (`child.rs`).
    let child = crate::child::ChildIdentity::from_env(env, &mut notices);

    if !crate::in_process::cloud_enabled(&user_settings, dirs) {
        notices.extend(crate::netguard::endpoint_notice(&endpoint.base_url));
    }

    let gated = gated_resources(cwd);
    let inherited_source = env
        .get(oxa_work::WORKTREE_TRUST_SOURCE_ENV)
        .filter(|_| mode == RunMode::Json);
    let managed_source = inherited_source.map(Path::new).filter(|source| {
        let path = std::ffi::OsStr::new(env.get("PATH").unwrap_or(""));
        crate::netguard::trusted_executable_for("git", Some(path), Some(cwd), Some(&dirs.home))
            .is_some_and(|git| oxa_work::verify_managed_worktree(&git, path, source, cwd).is_ok())
    });
    let trusted = if inherited_source.is_some() && a.approve.is_none() {
        let trusted = managed_source.is_some_and(|source| {
            TrustStore::load(&dirs.trust_file()).approved_worktree_resources(source, cwd)
        });
        if !trusted {
            report_untrusted_project(&gated, &mut notices);
        }
        trusted
    } else {
        project_trusted(
            a,
            env,
            dirs,
            cwd,
            &gated,
            mode == RunMode::Interactive && stdin_is_tty && stdout_is_tty,
            &mut notices,
        )?
    };
    // Inherited trust covers verified resources. Project settings, MCP and
    // agent definitions need direct approval.
    let direct_project_trust = trusted && inherited_source.is_none();
    if !trusted && Settings::project_declares_done(dirs, cwd) {
        if managed_source.is_some() {
            notices.push((
                Level::Info,
                "Managed worktree: the work runner handles project done checks".into(),
            ));
        } else {
            let fix = format!(
                "{}, or pass --approve for this run",
                crate::approval::approval_fix(cwd)
            );
            if mode != RunMode::Interactive || !stdin_is_tty || !stdout_is_tty {
                return Err(OxaError::new(codes::DENIED, UNTRUSTED_DONE).with_hint(fix));
            }
            notices.push((Level::Warning, format!("{UNTRUSTED_DONE}\nFix: {fix}")));
        }
    }
    let mut settings = child.apply(if direct_project_trust {
        Settings::load(dirs, cwd, true)
    } else {
        user_settings.clone()
    });
    if inherited_source.is_some() {
        // The work runner owns acceptance. The child must not run a second
        // local done round just because trusted project resources were loaded.
        settings = settings.with_override("done", Value::Array(Vec::new()));
    }
    // A custom shell or prefix can change the argv after classification. Named root actions
    // are for the interactive owner with the default shell, never delegated runs.
    let danger = if mode == RunMode::Interactive
        && !child.nested()
        && !a.lead
        && settings.shell_path(&dirs.home).is_none()
        && settings.shell_command_prefix().is_none()
    {
        let (rules, warnings) =
            danger.with_root_actions(&user_settings.user_list(oxa_perm::SETTING_ROOT_ACTIONS));
        notices.extend(warnings.into_iter().map(|w| (Level::Warning, w)));
        rules
    } else {
        danger
    };
    // CLI-045 (pi cli.md): `--verbose` overrides `quietStartup` for this run (in memory only).
    if a.verbose {
        settings = settings.with_override("quietStartup", Value::Bool(false));
    }
    // SET-008: `--models` replaces `enabledModels` for this run (the TUI's cycle list reads it).
    if let Some(patterns) = &a.models {
        let list = patterns.iter().cloned().map(serde_json::Value::String).collect();
        settings = settings.with_override("enabledModels", serde_json::Value::Array(list));
    }
    // UX/DX item 5: `--animation` overrides `ui.animation` for this run (in memory only).
    settings = animation_override(settings, a.animation.as_deref());
    notices.extend(
        settings
            .diagnostics()
            .iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    // keybindings.json problems are reported in every mode (pi reports them at startup),
    // not only by the interactive terminal.
    let (_, keymap_diags) = Keymap::load(dirs);
    notices.extend(
        keymap_diags
            .iter()
            .map(|d| (d.level, format!("{}: {}", d.source, d.message))),
    );
    // SET-021 / SES-010: `--session-dir` > `OXA_SESSION_DIR` > the `sessionDir` setting
    // (relative to the working directory) > `<agent dir>/sessions`.
    let mut session_dirs = dirs.clone();
    if !owner_chat && a.session_dir.is_none() && env.get("OXA_SESSION_DIR").is_none() {
        if let Some(dir) = settings.session_dir(&dirs.home, cwd) {
            session_dirs.sessions_dir = dir;
        }
    }
    let dirs = &session_dirs;
    // Spaced --continue is ambiguous until the effective store is known.
    if let Some(resolved) = resolve_continue_word(a, dirs) {
        *a = resolved;
    }
    let store: Arc<dyn SessionStore> = match &owner_open {
        Some((_, store)) => store.clone(),
        None if a.no_session => Arc::new(MemStore::new()),
        None => Arc::new(FsStore::new(dirs.sessions_dir.clone())),
    };
    let (mut open, session_id) = match owner_open {
        Some((bound, _)) => (Open::Resume(bound.clone()), None),
        None => choose_open(
            a,
            store.as_ref(),
            cwd,
            mode,
            rpc_owner_present(mode, env),
            &mut notices,
            confirm,
        )?,
    };
    let continued_go = if a.model.is_none() && a.continue_ && !owner_chat && !child.nested() {
        continued_go_model(dirs, cwd, &user_settings, store.as_ref(), &mut open)?
    } else {
        None
    };
    // The engine client with the settings' silence limit (`httpIdleTimeoutMs`,
    // `retry.provider.timeoutMs`; SET-051, SET-054).
    let has_model_scope = a.models.as_ref().map_or_else(
        || !settings.list("enabledModels").is_empty(),
        |patterns| !patterns.is_empty(),
    );
    let resuming = owner_chat || a.continue_ || a.session.is_some() || a.fork.is_some() || a.resume;
    // The scope is the owner's: `--models` or the user settings' `enabledModels`. A project
    // file setting it is not the owner choosing a transport (never used for that here).
    let owner_scope: Vec<String> = if a.model.is_none() && !resuming && !child.nested() {
        a.models
            .clone()
            .unwrap_or_else(|| user_settings.list("enabledModels"))
    } else {
        Vec::new()
    };
    let requested_model = a.model.clone().or_else(|| continued_go.clone()).or_else(|| {
        // The core chooses a restored/scoped model instead of an unused default.
        if resuming || has_model_scope {
            None
        } else {
            settings.string("defaultModel")
        }
    });
    #[cfg(feature = "share")]
    let sharing = (!a.share_models.is_empty())
        .then(|| {
            crate::share_cmd::prepare_session(
                &user_settings,
                &settings,
                dirs,
                a.share_models.clone(),
                a.share_bind,
                (!child.nested() && mode != RunMode::Rpc).then(|| cloud_diagnostics.clone()),
            )
        })
        .transpose()?;
    #[cfg(feature = "share")]
    let shared = sharing
        .as_ref()
        .map(|prepared| prepared.engines.clone())
        .unwrap_or_default();
    #[cfg(not(feature = "share"))]
    let shared = crate::in_process::SharedEngines::default();
    let (engine, catalog_notices) = crate::in_process::select_observed(
        &user_settings,
        &settings,
        dirs,
        crate::in_process::ModelSelection {
            pattern: requested_model.as_deref(),
            scope: &owner_scope,
            endpoint: &endpoint.name,
            // An actual `--model` argument is the owner's choice of that model, also when a
            // launcher (`oxa work`, a supervised child) passed it: the protected user key is
            // what authorizes the Go transport. A coding-agent tool process is not the owner
            // (it carries `AI_AGENT`/`OXA_CODING_AGENT`), a restored session model is
            // provenance-less (it counts only when the owner enabled Go), and a project
            // `defaultModel` or `enabledModels` never counts.
            owner_selected: (a.model.is_some()
                && env.get("AI_AGENT").is_none()
                && env.get("OXA_CODING_AGENT").is_none())
                || (!child.nested()
                    && !is_agent(env)
                    && env.get(oxa_work::PlannedTask::DELEGATED_MODEL_ENV).is_none()
                    && ((continued_go.is_some()
                        && user_settings.bool(crate::in_process::GO_SETTING, false))
                        || !owner_scope.is_empty()
                        || requested_model.as_ref().is_some_and(|model| {
                            user_settings.string("defaultModel").as_ref() == Some(model)
                        }))),
        },
        (!child.nested() && mode != RunMode::Rpc).then(|| cloud_diagnostics.clone()),
        shared,
        || {
            no_model_check(&models)?;
            engine_with_settings(&endpoint, &settings)
        },
    )?;
    let engine = child.wrap(engine);
    notices.extend(catalog_notices.into_iter().map(|text| (Level::Info, text)));

    let opts = discover_opts(a, dirs, cwd, trusted)?;
    // O-152: the built-in skills' files, so the model can read their bodies (written only
    // when missing or changed; a failure only costs the on-demand bodies).
    if settings.bool(oxa_resources::SETTING_BUILTIN_SKILLS, true) && !a.no_skills {
        if let Err(e) = oxa_resources::install_builtin_skills(dirs) {
            notices.push((Level::Warning, e.message));
        }
    }
    // O-152, automatic: the identity section is refreshed at every start, before the prefix is
    // built, from cheap local facts (machine, OS, oxa version) and the engine's model list (a
    // few loopback requests), so a missed `oxa identity --refresh` or an update never leaves
    // a stale or generic identity. The file is rewritten only when its bytes change, so the
    // prefix (and the cache) changes only when a fact did; a running session keeps its epoch's
    // prefix (W3). When the engine does not answer, an existing file keeps its model list.
    // Never under test isolation or in a subagent child (its parent just did it); a failure
    // only keeps the previous text.
    if !owner_chat && env.get(oxa_proto::names::ENV_TEST_ISOLATION) != Some("1") && !child.nested() {
        let probe = (!oxa_proto::offline()).then_some(engine.as_ref());
        let facts = crate::identity_cmd::identity_facts(Path::new("/"), probe);
        // An old file is kept only when it is in the current format; one written by an older
        // oxa (old head, cloud model lines) is rewritten with `Models: not known`.
        let keep_known_models = facts.models.is_empty()
            && oxa_resources::read_identity(dirs)
                .is_some_and(|t| t.starts_with(oxa_core::prompt::IDENTITY_HEAD));
        if !keep_known_models {
            if let Err(e) = crate::identity_cmd::write_identity(dirs, &facts) {
                notices.push((Level::Warning, e.message));
            }
        }
    }
    // Resources, package sources included (E10).
    let resources = discover(dirs, cwd, &settings, &opts);
    notices.extend(resources.diagnostics.iter().map(|d| {
        let text = match &d.path {
            Some(p) => format!("{}: {}", p.display(), d.message),
            None => d.message.clone(),
        };
        (d.level, text)
    }));
    // THM-003 / THM-004: invalid theme files are reported at startup in every mode.
    let cli_themes: Vec<PathBuf> = a
        .themes
        .iter()
        .map(|p| resolve_path(&dirs.home, cwd, p))
        .collect();
    let theme_sources = crate::themes::ThemeSources {
        dirs,
        cwd,
        settings: &settings,
        trusted,
        package_paths: &resources.theme_paths,
        cli_paths: &cli_themes,
        no_themes: a.no_themes,
    };
    notices.extend(crate::themes::theme_diagnostics(&theme_sources));
    let theme_paths = crate::themes::theme_paths(&theme_sources);
    // O-182, #202: one switch for the policy of every session and for the subagent children.
    // The command line wins; else `permissions.defaultMode` (a project may only lower it).
    // `--permission-mode workspace|ask` alone keeps its old meaning: the table asks you.
    let level = a
        .level
        .or(match a.permission_mode {
            Some(Mode::Workspace | Mode::Ask) => Some(oxa_proto::PermissionMode::Standard),
            _ => None,
        })
        .unwrap_or_else(|| settings.default_permission_mode());
    let yolo = oxa_proto::YoloSwitch::new(level.autonomy());
    // #1512 S3: who is at the keyboard decides the OS sandbox (`run_kind.rs`): attended runs
    // have none by default, unattended runs keep bwrap, yolo has none. bwrap is detected either
    // way: plan mode and offline/net-off commands need it.
    let kind = crate::run_kind::run_kind(a, env, &child, mode);
    let yolo_level = matches!(level, oxa_proto::PermissionMode::Yolo);
    // Keep the configured policy separate from the live yolo override. If the user leaves
    // startup yolo, FullAccess immediately restores this policy.
    let os_sandbox = crate::run_kind::os_sandbox_wanted(kind, a, &user_settings);
    if let Some(text) = crate::run_kind::no_sandbox_notice(kind, os_sandbox, yolo_level) {
        notices.push((Level::Info, text.to_string()));
    }
    let layout = crate::sandbox_layout::layout(env, dirs, cwd, home_workspace, &mut notices);
    let interactive = mode == RunMode::Interactive
        && kind == crate::run_kind::RunKind::Attended
        && level != oxa_proto::PermissionMode::Plan;
    let sandbox: Arc<dyn Sandbox> = match Bwrap::detect(env.get("PATH").unwrap_or(""), &dirs.home) {
        Ok(mut b) => {
            b.layout = layout.clone();
            // #457: each session gets its own directory at /tmp (`Built::deps`).
            let root = oxa_sandbox::scratch_root(&dirs.home);
            match std::fs::create_dir_all(&root) {
                Ok(()) => b.layout.scratch_root = Some(root),
                Err(e) => notices.push((
                    Level::Warning,
                    format!("no session scratch directory ({e}); /tmp is discarded after each command"),
                )),
            }
            let b = b.with_git_dirs(cwd);
            notices.extend(b.layout.git.notes.iter().map(|n| (Level::Warning, n.clone())));
            Arc::new(b)
        }
        // No usable bubblewrap: interactive sessions run unsandboxed and say so, never claim
        // isolation; unattended runs on Linux disable bash.
        Err(e) => {
            let (sb, notice) = no_bwrap_sandbox(interactive, os_sandbox, &e.message, env, dirs);
            if let Some(notice) = notice {
                notices.push(notice);
            }
            sb
        }
    };
    // Where model bash, plan commands and hooks run is decided once (`ExecPolicy`): the pinned
    // bwrap for network-less host commands is found here, before any model command, and shared
    // with the credential discovery of the network guard.
    let net_off = env.get(oxa_work::RUNNER_GITHUB_ENV) == Some("1")
        || crate::run_kind::sandbox_net_off(&settings, &user_settings);
    let mut policy = oxa_sandbox::ExecPolicy::new(
        env.get("PATH").unwrap_or(""),
        &dirs.home,
        oxa_proto::offline(),
        net_off,
    );
    policy.os_sandbox = os_sandbox;
    policy.attended_tui = interactive;
    let host_net = crate::netguard::with_net_policy(
        sandbox,
        &settings,
        &policy,
        env,
        endpoint.api_key.as_deref(),
        crate::netguard::TokenWorkspace::new(cwd, &dirs.home, &layout),
    );
    let sandbox: Arc<dyn Sandbox> = host_net.clone();
    let (sandbox, net_notice) = bash_sandbox(sandbox, interactive && !os_sandbox, oxa_proto::offline());
    if let Some(notice) = net_notice {
        notices.push((Level::Warning, notice));
    }

    // `--model` resolves against the engine's models; its `:level` suffix sets thinking unless
    // `--thinking` is given.
    let mut model = continued_go;
    let mut thinking = a.thinking;
    // `defaultModel` (SET-004) is applied by the core when no model was chosen here
    // (`Session::open`), so a resumed session keeps its own model and `--models` scopes it.
    if let Some(pattern) = &a.model {
        let available = engine_model_refs(engine.info(None)?, &endpoint.name);
        let picked = resolve_model(pattern, a.provider.as_deref(), &available)?;
        model = Some(picked.model);
        thinking = thinking.or(picked.thinking);
    } else {
        // CLI-015 (pi main.ts): `--models` (else `enabledModels`) scopes the startup model of a
        // new session: `defaultModel` when it is in scope, else the first scoped model, with the
        // thinking level of its pattern unless `--thinking` is given. Patterns that match
        // nothing are reported.
        let patterns = a.models.clone().unwrap_or_else(|| settings.list("enabledModels"));
        if !patterns.is_empty() && !resuming {
            let available = engine_model_refs(engine.info(None)?, &endpoint.name);
            let (scoped, diags) = oxa_config::resolve_scoped(&patterns, &available);
            notices.extend(diags.into_iter().map(|d| (d.level, d.message)));
            let saved = settings.string("defaultModel");
            let pick = scoped
                .iter()
                .find(|m| saved.as_deref() == Some(m.model.as_str()))
                .or_else(|| scoped.first());
            if let Some(pick) = pick {
                model = Some(pick.model.clone());
                thinking = thinking.or(pick.thinking);
            }
        }
    }

    let table_mode = match (a.permission_mode, level) {
        (None, oxa_proto::PermissionMode::Plan) => Some(Mode::Plan),
        (m, _) => m,
    };
    if let Some(text) = oxa_proto::no_os_sandbox_notice() {
        notices.push((Level::Warning, text.to_string()));
    }
    if let Some(text) = crate::netguard::yolo_net_notice(
        level,
        settings.string(oxa_sandbox::SETTING_SANDBOX_NET).as_deref(),
        oxa_proto::offline(),
    ) {
        notices.push((Level::Warning, text));
    }
    if !matches!(mode, RunMode::Interactive) {
        notices.push((
            Level::Info,
            format!("permission mode {}: {}", level.as_str(), level.describe()),
        ));
    }
    // Only the attended TUI may keep network access when the configured network restriction
    // cannot be enforced; the policy says so and the owner is told. Never offline, plan mode or
    // a requested OS sandbox.
    for text in policy.start_notices() {
        if !notices.iter().any(|(_, t)| *t == text) {
            notices.push((Level::Warning, text));
        }
    }
    let bash_sandbox = Arc::new(
        oxa_sandbox::FullAccess::with_policy(sandbox.clone(), yolo.clone(), dirs.home.clone(), policy)
            .with_os_sandbox(os_sandbox)
            .with_host_controls(layout, host_net),
    );
    // Exec hooks keep their dedicated sandbox independently of interactive bash.
    let hook_sandbox = oxa_sandbox::FullAccess::hook_sandbox(&bash_sandbox);
    let hook_inputs = HookInputs {
        args: a,
        dirs,
        cwd,
        settings: &settings,
        resources: &resources,
        sandbox: &hook_sandbox,
        trusted: direct_project_trust,
        nested: child.nested(),
        env,
        yolo: yolo.clone(),
    };
    let hook_sets = if owner_chat {
        // The live owner Lead session: the worker tool only (M1), through the K5a/K5b builder.
        crate::hooks::build_owner_hooks(&hook_inputs, &mut notices)
    } else {
        build_hooks(&hook_inputs, &mut notices)?
    };
    // `ctx.mode` of the extensions (pi's names).
    if let Some(ext) = &hook_sets.extensions {
        ext.set_mode(match mode {
            RunMode::Interactive => "tui",
            RunMode::Print => "print",
            RunMode::Json => "json",
            RunMode::Rpc => "rpc",
        });
    }

    let name = match a.name.as_deref().map(str::trim) {
        Some("") => {
            return Err(OxaError::new(codes::INVALID, "--name requires a non-empty value"));
        }
        other => other.map(str::to_string),
    };

    let selection = ToolSelection {
        default_tools: settings.default_tools(),
        tools: a.tools.clone(),
        exclude: a.exclude_tools.clone(),
        no_builtin: a.no_builtin_tools,
        no_tools: a.no_tools,
        default_edits: settings.default_tools_edits(),
    };
    let status = (!a.no_session).then(|| {
        Arc::new(
            crate::status::StatusWriter::new(a.stall_timeout)
                .with_billing(cloud_diagnostics.clone())
                .with_engine(&engine),
        )
    });
    let usage_log = a
        .usage_log
        .as_deref()
        .map(crate::usage_log::UsageLog::create)
        .transpose()?;
    let mut result_facts = crate::result_facts::ResultFacts::new(status.clone())
        .with_billing(cloud_diagnostics.clone())
        .with_usage_log(usage_log);
    // #2297: only a task that explicitly supplies an allowed changed-file set gets the
    // tool-observed changed-file evidence; every other run keeps the unavailable record.
    if !a.allowed_changed_files.is_empty() {
        result_facts =
            result_facts.with_changed_file_scope(cwd.to_path_buf(), a.allowed_changed_files.clone());
    }
    let result_facts = Arc::new(result_facts);
    let built = Built {
        lead: a.lead,
        status,
        result_facts,
        output_schema,
        goal,
        engine,
        cloud_diagnostics,
        store,
        sandbox,
        settings,
        dirs: dirs.clone(),
        cwd: cwd.to_path_buf(),
        resources,
        theme_paths,
        selection,
        model,
        thinking,
        mode: table_mode,
        supervised_child: child.nested(),
        fork_seed: child.fork.clone(),
        ask_owner_present: mode != RunMode::Rpc || env.get("OXA_RPC_OWNER_PRESENT") == Some("1"),
        report_dialog: (mode == RunMode::Interactive).then(|| env.clone()),
        hook_sets,
        yolo,
        bash_sandbox,
        os_sandbox,
        danger: Arc::new(danger),
        approval_desk,
        unattended: unattended_mode,
    };
    Ok(Prep {
        built,
        #[cfg(feature = "share")]
        sharing,
        open,
        session_id,
        notices,
        gated,
        name,
        continue_goal: a.continue_,
    })
}

/// The resource discovery options of the flags.
/// The same project trust and executable-content approval for sessions and workers.
pub(crate) fn project_trusted(
    a: &Args,
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    gated: &[String],
    interactive: bool,
    notices: &mut Vec<(Level, String)>,
) -> Result<bool, OxaError> {
    // Trust: the user's own settings decide the default; project settings load only when the
    // project is trusted.
    let user_settings = Settings::load(dirs, cwd, false);
    let default = match user_settings.string("defaultProjectTrust").as_deref() {
        Some("always") => DefaultTrust::Trust,
        Some("never") => DefaultTrust::Untrusted,
        _ => DefaultTrust::Ask,
    };
    // `OXA_TRUST_PROJECT=1` is `--approve` for the trust decision, for unattended runs;
    // an explicit `-na` still wins.
    let approve = a
        .approve
        .or_else(|| matches!(env.get("OXA_TRUST_PROJECT"), Some("1" | "true")).then_some(true));
    let mut trust_store = TrustStore::load(&dirs.trust_file());
    // SEC-003 / EXT-008: the user-level and `-e` extensions answer `project_trust` before the
    // saved decision and the default (pi's `resolveProjectTrusted`).
    let extension = if approve.is_none() && !gated.is_empty() {
        extension_trust(a, dirs, cwd, &user_settings, &mut trust_store, notices)
    } else {
        None
    };
    let saved = trust_store.lookup(cwd);
    let outcome = resolve_trust(&TrustInputs {
        flag: approve,
        extension,
        saved,
        default,
        interactive,
        has_gated: !gated.is_empty(),
    });
    // B4: the user answered the trust question in this start (it approves the content too).
    let mut asked_now = false;
    let trusted = match outcome {
        TrustOutcome::Trusted => true,
        TrustOutcome::Untrusted => false,
        TrustOutcome::AskUser => {
            // The trust dialog (D10) on the terminal, before the session exists. A dismissed
            // question leaves the project untrusted for this run and is asked again next time.
            let (keymap, _) = Keymap::load(dirs);
            let choice = ask_trust_interactive(env.pairs(), &keymap, &cwd.display().to_string(), gated)?;
            asked_now = true;
            let file = dirs.trust_file();
            match choice {
                Some(TrustChoice::Trust) => {
                    trust_store.save(&file, cwd, true)?;
                    true
                }
                Some(TrustChoice::TrustParent) => {
                    trust_store.save(&file, cwd.parent().unwrap_or(cwd), true)?;
                    true
                }
                Some(TrustChoice::NoTrust) => {
                    trust_store.save(&file, cwd, false)?;
                    false
                }
                Some(TrustChoice::Skip) | None => {
                    notices.push((
                        Level::Warning,
                        "the project is not trusted for this run; /trust saves a decision".to_string(),
                    ));
                    false
                }
            }
        }
    };
    // Apply the owner's folder or content policy only after resolving trust.
    let trusted = if trusted && !gated.is_empty() {
        crate::approval::check(crate::approval::Check {
            env,
            dirs,
            cwd,
            store: &mut trust_store,
            via_flag: approve == Some(true),
            via_default: extension.is_none() && saved.is_none() && default == DefaultTrust::Trust,
            asked_now,
            content: user_settings.project_trust_requires_content(),
            interactive,
            notices,
        })?
    } else {
        trusted
    };
    // #201: a project trusted by a saved decision gets its approval baseline now (also with
    // no project config yet), before any tool runs: `.oxa` / `.agents` content the session
    // creates is then new at the next start and goes through the normal approval.
    if trusted && saved == Some(true) {
        trust_store.record_baseline(&dirs.trust_file(), cwd)?;
    }
    if !trusted {
        report_untrusted_project(gated, notices);
    }
    Ok(trusted)
}

const UNTRUSTED_DONE: &str = "done checks in .oxa/settings.json were not run: project is not trusted";

fn report_untrusted_project(gated: &[String], notices: &mut Vec<(Level, String)>) {
    if !gated.is_empty() {
        notices.push((
            Level::Warning,
            format!(
                "project resources were skipped (not trusted): {}",
                gated.join(", ")
            ),
        ));
    }
}

fn discover_opts(a: &Args, dirs: &Dirs, cwd: &Path, trusted: bool) -> Result<DiscoverOpts, OxaError> {
    Ok(DiscoverOpts {
        trusted,
        no_context: a.no_context_files,
        no_skills: a.no_skills,
        no_templates: a.no_prompt_templates,
        no_extensions: a.no_extensions,
        extra_skills: a
            .skills
            .iter()
            .map(|p| resolve_path(&dirs.home, cwd, p))
            .collect(),
        extra_templates: a
            .prompt_templates
            .iter()
            .map(|p| resolve_path(&dirs.home, cwd, p))
            .collect(),
        extra_extensions: a.extensions.clone(),
        system_prompt: match &a.system_prompt {
            Some(text) => Some(text_or_file(text, &dirs.home, cwd)?),
            None => None,
        },
        append_system_prompt: a
            .append_system_prompt
            .iter()
            .map(|text| text_or_file(text, &dirs.home, cwd))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

/// CLI-048: the flags the extensions of this run register, for `--help` (pi main.ts loads the
/// resources, then prints the help with them). Trust is decided without asking: a project
/// that would need the dialog stays untrusted. Problems are reported on `err` and give no
/// flags; the help is printed anyway.
fn help_extension_flags(a: &Args, env: &Env, cwd: &Path, err: &mut dyn Write) -> Vec<oxa_proto::FlagSpec> {
    if a.lead {
        return Vec::new();
    }
    let Ok(dirs) = dirs_from_env(env, a.session_dir.as_deref()) else {
        return Vec::new();
    };
    let user = Settings::load(&dirs, cwd, false);
    let default = match user.string("defaultProjectTrust").as_deref() {
        Some("always") => DefaultTrust::Trust,
        Some("never") => DefaultTrust::Untrusted,
        _ => DefaultTrust::Ask,
    };
    let store = TrustStore::load(&dirs.trust_file());
    let saved = store.lookup(cwd);
    let gated = gated_resources(cwd);
    let trusted = resolve_trust(&TrustInputs {
        flag: a.approve,
        extension: None,
        saved,
        default,
        interactive: false,
        has_gated: !gated.is_empty(),
    }) == TrustOutcome::Trusted;
    let trusted = if trusted && !gated.is_empty() {
        if user.project_trust_requires_content() {
            let blanket = a.approve == Some(true) || (saved.is_none() && default == DefaultTrust::Trust);
            blanket || !matches!(store.approval(cwd), oxa_perm::Approval::Changed(_))
        } else {
            if let Some(text) = store.folder_changes_notice(cwd) {
                write_diagnostics(err, [(Level::Warning, text)].into_iter());
            }
            true
        }
    } else {
        trusted
    };
    let settings = Settings::load(&dirs, cwd, trusted);
    let opts = match discover_opts(a, &dirs, cwd, trusted) {
        Ok(o) => o,
        Err(e) => {
            report(err, &e);
            return Vec::new();
        }
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    if resources.extension_paths.is_empty() {
        return Vec::new();
    }
    match oxa_ext::ExtensionHost::new(cwd.to_path_buf()) {
        Ok(host) => {
            for ext in host.load(&resources.extension_paths, &oxa_ext::LoadOpts::default()) {
                if let Some(error) = ext.error {
                    let _ = writeln!(err, "Error: extension {}: {error}", ext.path);
                }
            }
            let flags = oxa_proto::Hooks::flags(&host);
            oxa_proto::Hooks::shutdown(&host);
            flags
        }
        Err(e) => {
            let _ = writeln!(err, "Error: extensions are not available: {e}");
            Vec::new()
        }
    }
}

/// `--continue <word>`: the word is the id of an existing session, or else the first words of
/// the prompt (and the latest session continues). `None` when there is nothing to settle.
fn resolve_continue_word(a: &Args, dirs: &Dirs) -> Option<Args> {
    let store = FsStore::new(dirs.sessions_dir.clone());
    let (word, at) = a.continue_word.as_ref()?;
    let mut out = a.clone();
    out.continue_word = None;
    let is_id = !a.no_session
        && a.session.is_none()
        && a.fork.is_none()
        && !a.resume
        && crate::args::valid_session_id(word)
        && store.resolve(word).is_ok_and(|session| session.id == *word);
    if is_id {
        out.continue_id = Some(word.clone());
    } else {
        out.messages.insert((*at).min(out.messages.len()), word.clone());
    }
    Some(out)
}

/// Use the active branch of the same protected session that startup will open.
/// A project-configured session directory cannot authorize a new cloud route.
fn continued_go_model(
    dirs: &Dirs,
    cwd: &Path,
    user: &Settings,
    store: &dyn SessionStore,
    open: &mut Open,
) -> Result<Option<String>, OxaError> {
    let owner_sessions = dirs.agent_dir.join("sessions");
    if dirs.sessions_dir != owner_sessions {
        return Ok(None);
    }
    let session = match open {
        Open::Continue => store
            .list(Some(cwd))
            .into_iter()
            .max_by_key(|meta| meta.modified_ms)
            .map(|meta| meta.sref),
        Open::Resume(session) => Some(session.clone()),
        _ => None,
    };
    let Some(session) = session.filter(|session| {
        store.lists_path(&session.path)
            && session
                .path
                .canonicalize()
                .as_ref()
                .is_ok_and(|path| path == &session.path)
    }) else {
        return Ok(None);
    };
    // Keep the route and restored model bound to the same writer and session.
    store.acquire(&session)?;
    let tree = Tree::new(store.load(&session)?);
    let model = tree.branch(tree.leaf()).into_iter().rev().find_map(|entry| {
        if let EntryBody::ModelChange(model) = &entry.body {
            Some(model.clone())
        } else {
            None
        }
    });
    let go = model.filter(|model| crate::in_process::restores_go_model(user, model));
    if go.is_some() {
        *open = Open::Resume(session);
    } else {
        store.abort_lock();
    }
    Ok(go)
}

/// The open mode of the flags: `--no-session`, `--fork`, `--session`, `--session-id`,
/// `--continue`, `--resume` (interactive only, through the picker) or a new session.
fn choose_open(
    a: &Args,
    store: &dyn SessionStore,
    cwd: &Path,
    mode: RunMode,
    rpc_owner: bool,
    _notices: &mut Vec<(Level, String)>,
    confirm: &mut dyn FnMut(&str, &str) -> bool,
) -> Result<(Open, Option<String>), OxaError> {
    if a.no_session {
        return Ok((Open::Ephemeral, a.session_id.clone()));
    }
    if let Some(s) = &a.fork {
        // pi passes `--session-id` on to `SessionManager.forkFrom` (the fork's new id).
        return Ok((Open::Fork(store.resolve(s)?), a.session_id.clone()));
    }
    if let Some(s) = a.continue_id.as_ref().or(a.session.as_ref()) {
        let sref = store.resolve(s)?;
        // pi main.ts `resolveSessionPath`: a path opens as it is; an id found only in another
        // directory is offered as a fork into this one (declining exits 0 with "Aborted.").
        let is_path = s.contains('/') || s.contains('\\') || s.ends_with(".jsonl");
        if !is_path {
            if let Ok(header) = store.header(&sref) {
                if header.cwd != cwd {
                    let notice = format!("Session found in different project: {}", header.cwd.display());
                    // RPC asks the owner over the protocol when a client is listening; without
                    // one the offer is an error with the way to fork it yourself.
                    if mode == RunMode::Rpc && !rpc_owner {
                        return Err(OxaError::new(codes::INVALID, notice)
                            .with_hint("use --fork <id> to copy it into this directory"));
                    }
                    if confirm(&notice, "Fork this session into current directory?") {
                        return Ok((Open::Fork(sref), None));
                    }
                    return Err(OxaError::new(FORK_DECLINED, "Aborted."));
                }
            }
        }
        return Ok((Open::Resume(sref), None));
    }
    if let Some(id) = &a.session_id {
        // The current directory's session with this id resumes; otherwise a new one is created
        // with the id.
        if let Ok(sref) = store.resolve(id) {
            if store.header(&sref).map(|h| h.cwd == cwd).unwrap_or(false) {
                return Ok((Open::Resume(sref), None));
            }
        }
        return Ok((Open::New, Some(id.clone())));
    }
    if a.continue_ {
        return Ok((Open::Continue, None));
    }
    if a.resume {
        if mode == RunMode::Interactive {
            // The interactive mode opens the session picker at start (`open_picker`).
            return Ok((Open::New, None));
        }
        return Err(OxaError::new(
            codes::USAGE,
            "--resume needs the interactive session picker; use --continue or --session <id>",
        )
        .with_hint(USAGE_HINT));
    }
    Ok((Open::New, None))
}

fn headless_inbox_enabled(settings: &Settings) -> bool {
    settings.user_bool("oxa.inbox.headless", true)
}

#[cfg(test)]
#[test]
fn headless_inbox_uses_the_owners_switch_instead_of_the_project() {
    let project = serde_json::json!({"oxa": {"inbox": {"headless": true}}});
    let owner = serde_json::json!({"oxa": {"inbox": {"headless": false}}});
    assert!(!headless_inbox_enabled(&Settings::from_values(owner, project)));
    let project = serde_json::json!({"oxa": {"inbox": {"headless": false}}});
    assert!(headless_inbox_enabled(&Settings::from_values(
        serde_json::json!({}),
        project,
    )));
}

/// The mode runs that print to stdout and exit: `-p` and `--mode json`.
fn run_headless(
    mode: RunMode,
    max_time: Option<std::time::Duration>,
    max_tokens: Option<u64>,
    prep: Prep,
    prompts: Vec<(String, Vec<Part>)>,
    stdout: Box<dyn Write + Send>,
    stderr: &mut dyn Write,
    #[cfg(target_os = "linux")] managed_steering: Option<oxa_work::managed_steering::ManagedSteering>,
) -> i32 {
    let Prep {
        built,
        open,
        session_id,
        mut notices,
        name,
        continue_goal,
        ..
    } = prep;
    // FH-N2: nobody watches a print or JSON run; the prefix of this session carries the
    // autonomy block and the loop nudges once when a final reply ends on a plan.
    oxa_core::set_headless(true);
    let mut session = match open_named(&built, session_id, open, name.as_deref()) {
        Ok(s) => s,
        Err(e) => return finish_outcome_facts(Err(e), stderr, Some(&built.result_facts)),
    };
    let starts_goal = built.goal.is_some();
    #[cfg(target_os = "linux")]
    let starts_goal = if let Some(steering) = managed_steering.as_ref().filter(|_| session.goal().is_none()) {
        if let Err(error) = crate::goal_cmd::from_objective(steering.objective().into(), max_time, max_tokens)
            .and_then(|goal| session.set_goal(Some(goal), &mut oxa_proto::VecSink::default()))
        {
            return finish_outcome_facts(Err(error), stderr, Some(&built.result_facts));
        }
        true
    } else {
        starts_goal
    };
    if continue_goal {
        if let Some(mut goal) = session
            .goal()
            .filter(|g| g.status == oxa_session::GoalStatus::Paused && !g.exhausted())
        {
            if let Err(error) = goal
                .resume()
                .and_then(|()| session.set_goal(Some(goal), &mut oxa_proto::VecSink::default()))
            {
                return finish_outcome_facts(Err(error), stderr, Some(&built.result_facts));
            }
        }
    }
    let mut prompts = prompts;
    if let Some(goal) = session.goal() {
        if goal.status == oxa_session::GoalStatus::Pursuing {
            if crate::run_kind::unattended_os_sandbox(&built.settings) {
                built.bash_sandbox.require_os_sandbox();
                notices.retain(|(_, notice)| notice != crate::run_kind::NO_SANDBOX_NOTICE);
            }
            if starts_goal {
                let start = oxa_core::prompt::goal_start(&goal.objective);
                if let Some((text, _parts)) = prompts.first_mut() {
                    *text = format!("{start}\n\n{text}");
                } else {
                    prompts.push((start, vec![]));
                }
            } else if prompts.is_empty() {
                prompts.push((oxa_core::prompt::GOAL_CONTINUE.into(), vec![]));
            }
        }
    }
    if prompts.is_empty() {
        if let Some(goal) = session.goal() {
            let outcome = match goal.status {
                oxa_session::GoalStatus::Done => oxa_core::RunOutcome::Done,
                oxa_session::GoalStatus::Blocked => oxa_core::RunOutcome::Blocked("goal_blocked"),
                oxa_session::GoalStatus::Paused => oxa_core::RunOutcome::Stalled,
                oxa_session::GoalStatus::Canceled => oxa_core::RunOutcome::Interrupted,
                oxa_session::GoalStatus::Pursuing => oxa_core::RunOutcome::NothingToDo,
            };
            built.result_facts.goal(Some(goal));
            session.shutdown(&mut oxa_proto::HeadlessUi::default());
            let result = built
                .status
                .as_ref()
                .map_or(Ok(outcome), |status| status.finish(Ok(outcome), stderr));
            return finish_outcome_facts(result, stderr, Some(&built.result_facts));
        }
        return finish_outcome_facts(
            Err(OxaError::new(codes::USAGE, "No prompt or pursuing stored goal.")),
            stderr,
            Some(&built.result_facts),
        );
    }
    notices.extend(startup_notices(&built, &session));
    write_notices(stderr, notices);
    if mode == RunMode::Print {
        // Printed before the run so a driver that kills a long task still knows what to resume.
        let st = session.state();
        if st.session_file.is_some() {
            let _ = writeln!(
                stderr,
                "oxa: session {} (resume with --continue {})",
                st.session_id, st.session_id
            );
        }
    }
    let goal = session
        .goal()
        .filter(|g| g.status == oxa_session::GoalStatus::Pursuing);
    let goal_time = goal.as_ref().and_then(|g| {
        g.budget
            .max_time_ms
            .map(|cap| std::time::Duration::from_millis(cap.saturating_sub(g.elapsed_ms)))
    });
    let goal_tokens = goal
        .as_ref()
        .and_then(|g| g.budget.max_tokens.map(|cap| cap.saturating_sub(g.tokens_used)));
    let max_time = match (max_time, goal_time) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let max_tokens = match (max_tokens, goal_tokens) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let _limit = match crate::limits::Run::start(
        session.handle(),
        max_time,
        max_tokens,
        built.supervised_child,
        built.hook_sets.subagent.clone(),
        crate::limits::SigtermOwner::Signal,
    ) {
        Ok(limit) => limit,
        Err(error) => {
            let result = match &built.status {
                Some(status) => status.finish(Err(error), stderr),
                None => Err(error),
            };
            return finish_outcome_facts(result, stderr, Some(&built.result_facts));
        }
    };
    let mut stdout = Some(stdout);
    #[cfg(target_os = "linux")]
    let mut managed_out = None;
    let result = match mode {
        #[cfg(target_os = "linux")]
        RunMode::Json if managed_steering.is_some() => {
            use oxa_core::goal::{SteeringAuthority, SteeringLease};
            let steering = managed_steering.expect("matched managed steering");
            let expected = session.inner().sref.clone();
            let authority: Arc<SteeringAuthority> = Arc::new(move |requested, operation| {
                if requested == &expected {
                    let _ = steering.with_live(|lease| {
                        operation(Some(&SteeringLease {
                            holder: lease.holder.clone(),
                            generation: lease.generation,
                        }));
                    });
                }
            });
            let out = Arc::new(Mutex::new(stdout.take().expect("stdout is present")));
            managed_out = Some(out.clone());
            oxa_rpc::run_json_outcome_with_steering_authority(
                &mut session,
                prompts,
                out,
                stderr,
                &built.dirs.agent_dir,
                authority,
            )
        }
        RunMode::Json => run_json_outcome_with_inbox(
            &mut session,
            prompts,
            stdout.as_deref_mut().unwrap(),
            stderr,
            &built.dirs.agent_dir,
            headless_inbox_enabled(&built.settings),
        ),
        _ => run_print_outcome_with_inbox(
            &mut session,
            prompts,
            stdout.as_deref_mut().unwrap(),
            stderr,
            &built.dirs.agent_dir,
            headless_inbox_enabled(&built.settings),
        ),
    };
    let stats = session.stats();
    built.result_facts.ci_polls(stats.ci_polls);
    built.result_facts.goal(session.goal());
    built.result_facts.finish(stats);
    // `session_shutdown` for the hook sets, then their script contexts and MCP servers stop.
    let mut ui = oxa_proto::HeadlessUi::default();
    session.shutdown(&mut ui);
    write_notices(stderr, ui.notes);
    let result = match &built.status {
        Some(status) => status.finish(result, stderr),
        None => result,
    };
    if mode == RunMode::Json && built.supervised_child && max_tokens.is_some() {
        // Private child/parent accounting: message_end omits summaries and tool
        // prefill. Report the uncapped total, including the child's final turn.
        let usage = serde_json::json!({
            "type": "run_usage",
            "tokens": session.handle().run_tokens_used(),
            "complete": !session.handle().run_usage_unreported(),
        });
        if let Some(stdout) = stdout.as_deref_mut() {
            let _ = writeln!(stdout, "{usage}");
        }
        #[cfg(target_os = "linux")]
        if let Some(out) = managed_out {
            let _ = writeln!(out.lock().unwrap_or_else(|poison| poison.into_inner()), "{usage}");
        }
    }
    // Child stderr can be captured as a tool result in the parent's history.
    if matches!(
        &result,
        Err(_) | Ok(oxa_core::RunOutcome::Blocked(_) | oxa_core::RunOutcome::Failed(_))
    ) {
        crate::in_process::report_diagnostics(&built.cloud_diagnostics, &built.dirs, stderr);
    }
    finish_outcome_facts(result, stderr, Some(&built.result_facts))
}

/// `--mode rpc`: the protocol owns stdin and stdout; notices go to stderr.
fn run_rpc(
    prep: Prep,
    max_time: Option<std::time::Duration>,
    max_tokens: Option<u64>,
    dirs: Dirs,
    stdin: Box<dyn Read + Send>,
    stdout: Box<dyn Write + Send>,
    stderr: &mut dyn Write,
    control_lane: bool,
) -> i32 {
    let Prep {
        built,
        open,
        session_id,
        mut notices,
        name,
        ..
    } = prep;
    let session = match open_named(&built, session_id, open, name.as_deref()) {
        Ok(s) => s,
        Err(e) => return finish_outcome_facts(Err(e), stderr, Some(&built.result_facts)),
    };
    notices.extend(startup_notices(&built, &session));
    write_notices(stderr, notices);
    let _limit = match crate::limits::Run::start(
        session.handle(),
        max_time,
        max_tokens,
        built.supervised_child,
        built.hook_sets.subagent.clone(),
        crate::limits::SigtermOwner::Signal,
    ) {
        Ok(limit) => limit,
        Err(error) => {
            let result = match &built.status {
                Some(status) => status.finish(Err(error), stderr),
                None => Err(error),
            };
            return finish_outcome_facts(result, stderr, Some(&built.result_facts));
        }
    };
    let out: Out = Arc::new(Mutex::new(stdout));
    let input = Box::new(BufReader::new(stdin));
    let result = serve_outcome_gated(session, dirs, input, out, control_lane);
    let result = match &built.status {
        Some(status) => status.finish(result, stderr),
        None => result,
    };
    finish_outcome_facts(result, stderr, Some(&built.result_facts))
}

/// The interactive terminal: the first prompt and the further messages come from the command
/// line; `reopen` builds the sessions of `/new`, `/resume`, fork, clone and import.
fn run_tui(
    a: &Args,
    env: &Env,
    cwd: &Path,
    prep: Prep,
    stderr: &mut dyn Write,
    owner: Option<(Session, LeadOwnerBinding)>,
) -> i32 {
    let dirs = &prep.built.dirs.clone();
    let prompts = match build_prompts(a, dirs, cwd, None) {
        Ok(p) => p,
        Err(e) => return report(stderr, &e),
    };
    let mut prompts = prompts.into_iter();
    let (mut initial_prompt, initial_parts) = match prompts.next() {
        Some((text, parts)) => ((!text.is_empty()).then_some(text), parts),
        None => (None, Vec::new()),
    };
    let initial_follow_ups: Vec<String> = prompts.map(|(text, _)| text).collect();

    let Prep {
        built,
        #[cfg(feature = "share")]
        sharing,
        open,
        session_id,
        mut notices,
        gated,
        name,
        ..
    } = prep;
    let (mut session, owner) = match owner {
        Some((session, binding)) => (session, Some(binding)),
        None => match open_named(&built, session_id, open, name.as_deref()) {
            Ok(session) => (session, None),
            Err(e) => {
                let code = report(stderr, &e);
                // A fresh install has no model yet: say how to get one, after the error.
                if let Some(help) = crate::key_hints::no_model_help(&e, dirs, env) {
                    let _ = writeln!(stderr, "{help}");
                }
                return code;
            }
        },
    };
    // `--goal` in the terminal: the objective is set at open (`open_named`) and its start leads
    // the first queued prompt (with the attachments of that prompt), as in print mode.
    if let Some(goal) = &built.goal {
        let start = oxa_core::prompt::goal_start(&goal.objective);
        initial_prompt = Some(goal_first_prompt(start, initial_prompt.take()));
    }
    #[cfg(feature = "share")]
    let guest_listener = match sharing.map(crate::share_cmd::Prepared::start).transpose() {
        Ok(Some((running, address))) => {
            notices.push((
                Level::Info,
                format!("Guest OpenAI API listening at http://{address}/v1"),
            ));
            notices.extend(running.status.warning().map(|w| (Level::Warning, w.to_string())));
            Some(running)
        }
        Ok(None) => None,
        Err(error) => return report(stderr, &error),
    };
    notices.extend(startup_notices(&built, &session));
    // First run only: which optional keys are missing and how to add each. Never blocking.
    notices.extend(crate::key_hints::first_run_notice(dirs, env, &built.settings));
    let yolo = built.yolo.clone();
    // Its diagnostics are among the startup notices (`prepare`).
    let (keymap, _) = Keymap::load(dirs);
    let trust_file = dirs.trust_file();
    let trust_dir = cwd.to_path_buf();
    let save_trust: Box<dyn FnMut(TrustChoice) -> Result<(), OxaError> + Send> = Box::new(move |choice| {
        let mut store = TrustStore::load(&trust_file);
        // B4: `/trust` approves the project files as they are now.
        match choice {
            TrustChoice::Trust => store
                .save(&trust_file, &trust_dir, true)
                .and_then(|_| store.approve(&trust_file, &trust_dir)),
            TrustChoice::TrustParent => store
                .save(&trust_file, trust_dir.parent().unwrap_or(&trust_dir), true)
                .and_then(|_| store.approve(&trust_file, &trust_dir)),
            TrustChoice::NoTrust => store.save(&trust_file, &trust_dir, false),
            TrustChoice::Skip => Ok(()),
        }
    });
    let settings = built.settings.clone();
    let store = built.store.clone();
    let base = built.clone();
    let pinned = owner.is_some();
    let run_slot: Arc<std::sync::OnceLock<Arc<crate::limits::Run>>> = Arc::default();
    let reopen_run = run_slot.clone();
    let reopen: ReopenFn = Box::new(move |request, carry| {
        if let Some(binding) = &owner {
            if !matches!(&request, Reopen::Resume(session) if session == binding.session_ref()) {
                return Err(OxaError::new(
                    codes::INVALID,
                    "Lead is attached to its dedicated session.",
                ));
            }
            binding.recheck()?;
        }
        let mut deps = base.deps(None);
        // `/new` keeps the model and thinking level of the session it leaves (a `/model`
        // switch is present in the status bar but only as a `ModelChange` entry of the old
        // session); `/resume`, fork and import restore the target session's own, and any
        // startup `--model` choice in `base` is left untouched for them.
        let how = match request {
            Reopen::New => {
                if let Some((model, thinking)) = carry {
                    deps.model = Some(model);
                    deps.thinking = Some(thinking);
                }
                Open::New
            }
            Reopen::Resume(sref) => Open::Resume(sref),
            Reopen::Fork(sref) => Open::Fork(sref),
        };
        let mut session = open_interactive_session(&base, deps, how)?;
        if let Some(binding) = &owner {
            binding.recheck()?;
            if &session.inner().sref != binding.session_ref() {
                return Err(OxaError::new(
                    codes::INVALID,
                    "The selected Lead session changed.",
                ));
            }
            if let Some(workers) = &base.hook_sets.workers {
                let _ = workers.resume_after_owner_check(true);
            }
        }
        base.result_facts.attach(&mut session)?;
        session.set_output_schema(base.output_schema.clone());
        if let Some(run) = reopen_run.get() {
            run.attach(session.handle());
        }
        Ok(session)
    });
    // The limits belong to this invocation: the run follows every session swap.
    if a.max_time.is_some() || a.max_tokens.is_some() {
        let sigterm = if built.supervised_child {
            crate::limits::SigtermOwner::Signal
        } else {
            crate::limits::SigtermOwner::Terminal(oxa_tui::cooperative_sigterm())
        };
        match crate::limits::Run::start(
            session.handle(),
            a.max_time,
            a.max_tokens,
            built.supervised_child,
            built.hook_sets.subagent.clone(),
            sigterm,
        ) {
            Ok(run) => {
                let _ = run_slot.set(Arc::new(run));
            }
            Err(error) => return report(stderr, &error),
        }
    }
    let opts = InteractiveOpts {
        // `--tui-mode` over the `tuiMode` setting (default fullscreen).
        fullscreen: a
            .tui_mode
            .clone()
            .or_else(|| settings.string("tuiMode"))
            .as_deref()
            != Some("regular"),
        dirs: dirs.clone(),
        settings,
        keymap,
        store,
        hooks: built.hook_sets.hooks.clone(),
        mcp: built.hook_sets.mcp.clone(),
        initial_prompt,
        initial_parts,
        initial_follow_ups,
        theme: a.use_theme.clone().or_else(|| {
            a.themes.first().and_then(|p| {
                let path = resolve_path(&dirs.home, cwd, p);
                path.is_file().then(|| path.to_string_lossy().into_owned())
            })
        }),
        theme_paths: built.theme_paths.clone(),
        notices,
        env: env.pairs().to_vec(),
        caps: None,
        reopen,
        open_picker: a.resume,
        save_trust: Some(save_trust),
        gated,
        #[cfg(feature = "share")]
        engine_status: guest_listener.as_ref().map(|running| {
            let status = running.status.clone();
            Box::new(move || status.lines()) as oxa_interactive::EngineStatus
        }),
        #[cfg(not(feature = "share"))]
        engine_status: None,
        yolo: Some(yolo),
        os_sandbox: crate::run_kind::os_sandbox_active(built.os_sandbox, oxa_proto::OS_SANDBOX),
    };
    let evidence = a.wire_evidence.then(|| session.arm_wire_evidence());
    let result = if pinned {
        oxa_interactive::run_pinned(session, opts, "oxa lead")
    } else {
        run_interactive(session, opts)
    };
    if let Some(evidence) = evidence {
        if let Ok(report) = serde_json::to_string(&evidence.finish()) {
            let _ = writeln!(stderr, "Wire adjacency (oxa handoff only): {report}");
        }
    }
    let result = match &built.status {
        Some(status) => status.finish_interactive(result, stderr),
        None => result,
    };
    match result {
        Ok(code) => code,
        Err(e) => report(stderr, &e),
    }
}

pub(crate) fn engine_model_refs(mut info: oxa_proto::EngineInfo, endpoint: &str) -> Vec<ModelRef> {
    if !info.models.contains(&info.model) {
        info.models.insert(0, info.model);
    }
    info.models
        .into_iter()
        .map(|model| ModelRef {
            endpoint: endpoint.to_string(),
            model,
            thinking: None,
        })
        .collect()
}

/// The default engine endpoint of models.json (or the one `OXA_ENGINE_URL` names).
pub(crate) fn default_endpoint(models: &ModelsConfig) -> Result<Endpoint, OxaError> {
    models
        .endpoints
        .iter()
        .find(|ep| ep.name == models.default)
        .or_else(|| models.endpoints.first())
        .cloned()
        .ok_or_else(|| OxaError::new(codes::NOT_FOUND, "no engine endpoint is configured"))
}

/// A fresh install has no model source: say what to configure instead of dialing a port
/// nothing listens on.
fn no_model_check(models: &ModelsConfig) -> Result<(), OxaError> {
    if models.unconfigured {
        return Err(OxaError::new(codes::ENGINE_DOWN, "no model is configured").with_hint(
            "configure a cloud key (oxa login chatgpt --experimental, or a provider in settings) or run `oxa local add <model.gguf>`; to use an engine already running, set OXA_ENGINE_URL",
        ));
    }
    Ok(())
}

/// The HTTP engine for the default endpoint, unless nothing is configured.
fn fallback_engine(models: &ModelsConfig, endpoint: &Endpoint) -> Result<Arc<dyn Engine>, OxaError> {
    no_model_check(models)?;
    engine_for(endpoint)
}

pub(crate) fn engine_for(endpoint: &Endpoint) -> Result<Arc<dyn Engine>, OxaError> {
    crate::netguard::check_endpoint(&endpoint.base_url)?;
    let engine = HttpEngine::new(&endpoint.base_url, endpoint.api_key.clone())?;
    Ok(Arc::new(engine))
}

/// CLI-013: `--api-key` replaces the endpoint's key for this run (sent only as
/// `Authorization: Bearer`; never stored in a session or put on the model wire).
fn with_api_key(mut endpoint: Endpoint, a: &Args) -> Endpoint {
    if let Some(key) = &a.api_key {
        endpoint.api_key = Some(key.0.clone());
    }
    endpoint
}

/// UX/DX item 5: `--animation <mode>` overrides `ui.animation` for this run, in memory only, so the
/// setting on disk is untouched. `None` (no flag) keeps whatever the settings resolved to.
///
/// #869: `off` also overrides `ui.reducedMotion` (the mode and reduced motion draw the same static
/// badge, as cli.md says), and `arcade`/`minimal` clear it, so the flag is not silently ignored on
/// a machine whose settings ask for reduced motion. `OXA_REDUCED_MOTION` still wins for the run.
fn animation_override(settings: Settings, mode: Option<&str>) -> Settings {
    match mode {
        Some(mode) => {
            let reduced = mode == "off";
            settings
                .with_override("ui.animation", Value::String(mode.to_string()))
                .with_override("ui.reducedMotion", Value::Bool(reduced))
        }
        None => settings,
    }
}

/// [`engine_for`] with the stream silence limit of the settings: `retry.provider.timeoutMs`,
/// else `httpIdleTimeoutMs` (`0` / `"disabled"` = no limit).
fn engine_with_settings(endpoint: &Endpoint, settings: &Settings) -> Result<Arc<dyn Engine>, OxaError> {
    /// "Disabled": a silence limit no stream reaches (one year).
    const NO_LIMIT_MS: u64 = 365 * 24 * 3600 * 1000;
    // P3: the same remote-engine guard as `engine_for`.
    crate::netguard::check_endpoint(&endpoint.base_url)?;
    let silence = stream_silence_timeout(settings).unwrap_or(NO_LIMIT_MS);
    let opts = oxa_engine::StreamOpts {
        silence_limit_ms: silence,
        ..oxa_engine::StreamOpts::default()
    };
    // The core owns session retries so every retry uses its backoff and visible notice.
    let engine = HttpEngine::new(&endpoint.base_url, endpoint.api_key.clone())?.with_opts(opts, 0);
    Ok(Arc::new(engine))
}

/// Shared by HTTP and native engines; project layers affect timeouts, while the
/// native selector, catalog and credential locations remain user-only.
pub(crate) fn stream_silence_timeout(settings: &Settings) -> Option<u64> {
    settings
        .provider_retry()
        .timeout_ms
        .filter(|ms| *ms > 0)
        .or_else(|| settings.http_idle_timeout_ms())
}

/// The provider column: the controller's `remote_provider`, else the remote model id's
/// provider prefix (or `cloud` for an unprefixed id); the endpoint name for a local one.
pub(crate) fn row_provider(endpoint: &str, info: &oxa_proto::EngineInfo) -> String {
    match (info.caps.remote, info.caps.remote_provider) {
        (_, Some(p)) => p.to_string(),
        (true, None) => info
            .limits_for
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .map_or("cloud", |(provider, _)| provider)
            .to_string(),
        _ => endpoint.to_string(),
    }
}

/// `--list-models [search]`: a table of the engine's models (pi's columns), sorted by id.
pub(crate) fn list_models(
    engine: &dyn Engine,
    provider: &str,
    search: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, OxaError> {
    let info = engine.info(None)?;
    let mut ids = info.models.clone();
    if !ids.contains(&info.model) {
        ids.push(info.model.clone());
    }
    ids.sort();
    ids.dedup();
    let needle = search.map(|s| s.to_lowercase());
    let rows: Vec<[String; 6]> = ids
        .into_iter()
        .map(|id| {
            let model_info = if id == info.limits_for {
                Ok(info.clone())
            } else {
                engine.info(Some(&id))
            };
            match model_info {
                Ok(model_info) => [
                    row_provider(provider, &model_info),
                    id,
                    format_tokens(model_info.n_ctx as u64),
                    format_tokens(model_info.max_output as u64),
                    model_info.thinking.map_or_else(|| "?".to_string(), bool_word),
                    model_info.images.map_or_else(|| "?".to_string(), bool_word),
                ],
                // A failing per-model info call degrades only this row: '?' in the limits
                // columns (never another model's limits), and the listing continues.
                Err(e) => {
                    let _ = writeln!(err, "warning: cannot list {id}: {}", e.message);
                    [
                        provider.to_string(),
                        id,
                        "?".to_string(),
                        "?".to_string(),
                        "?".to_string(),
                        "?".to_string(),
                    ]
                }
            }
        })
        .filter(|row| match &needle {
            Some(n) => format!("{} {}", row[0], row[1])
                .to_lowercase()
                .contains(n.as_str()),
            None => true,
        })
        .collect();
    if rows.is_empty() {
        return empty_list(search, out);
    }
    let rows: Vec<Vec<String>> = rows.into_iter().map(Vec::from).collect();
    render_table(
        &["provider", "model", "context", "max-out", "thinking", "images"],
        &rows,
        out,
    )?;
    Ok(0)
}

/// The empty listing: the search word when one was given, else the plain fact.
pub(crate) fn empty_list(search: Option<&str>, out: &mut dyn Write) -> Result<i32, OxaError> {
    let text = match search {
        Some(s) => format!("No models matching \"{s}\""),
        None => "No models available".to_string(),
    };
    let write_err = |e: std::io::Error| OxaError::io("cannot write the model list", &e);
    writeln!(out, "{text}").map_err(write_err)?;
    Ok(0)
}

/// One padded table, shared by `--list-models` and `oxa models`: every row carries exactly the
/// header count of cells; every column but the last is padded, so the lines carry no trailing
/// spaces.
pub(crate) fn render_table(
    headers: &[&str],
    rows: &[Vec<String>],
    out: &mut dyn Write,
) -> Result<(), OxaError> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                s.push_str(cell);
            } else {
                s.push_str(&format!("{cell:<w$}  ", w = widths[i]));
            }
        }
        s
    };
    let write_err = |e: std::io::Error| OxaError::io("cannot write the model list", &e);
    let header_line = line(headers);
    writeln!(out, "{header_line}").map_err(write_err)?;
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        writeln!(out, "{}", line(&cells)).map_err(write_err)?;
    }
    Ok(())
}

/// `true` -> `yes`, `false` -> `no` for the `thinking`/`images` columns of `--list-models`
/// and `oxa models`.
pub(crate) fn bool_word(b: bool) -> String {
    if b {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

/// pi's token counts: `200K`, `1M`, `1.5M`; below a thousand the plain number. Shared with
/// `oxa models`.
pub(crate) fn format_tokens(count: u64) -> String {
    let scaled = |div: f64, unit: &str| {
        let v = count as f64 / div;
        if v.fract() == 0.0 {
            format!("{v}{unit}")
        } else {
            format!("{v:.1}{unit}")
        }
    };
    if count >= 1_000_000 {
        scaled(1_000_000.0, "M")
    } else if count >= 1_000 {
        scaled(1_000.0, "K")
    } else {
        count.to_string()
    }
}

/// True when `dest` names the `input` session: the same path, a hard link, or a symlink that
/// resolves to it. Writing such a destination would overwrite the session, so it is refused.
fn aliases_source(input: &Path, dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if input == dest {
        return true;
    }
    match (std::fs::metadata(input), std::fs::metadata(dest)) {
        (Ok(source), Ok(target)) => source.dev() == target.dev() && source.ino() == target.ino(),
        _ => false,
    }
}

/// `--export <input> [output]`: copy JSONL for a `.jsonl` destination, otherwise render HTML.
/// The output name is derived when omitted. Prints `Exported to: <path>`.
fn export_session(
    input: &str,
    output: Option<&str>,
    home: &Path,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match export_inner(input, output, home, cwd) {
        Ok(path) => {
            let _ = writeln!(out, "Exported to: {}", path.display());
            0
        }
        Err(e) => report(err, &e),
    }
}

fn export_inner(input: &str, output: Option<&str>, home: &Path, cwd: &Path) -> Result<PathBuf, OxaError> {
    use std::os::unix::fs::PermissionsExt;
    let input_path = resolve_path(home, cwd, input);
    let mut source = std::fs::File::open(&input_path)
        .map_err(|e| OxaError::io(&format!("cannot read {}", input_path.display()), &e))?;
    let mut text = String::new();
    source
        .read_to_string(&mut text)
        .map_err(|e| OxaError::io(&format!("cannot read {}", input_path.display()), &e))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next().ok_or_else(|| {
        OxaError::new(
            codes::INVALID,
            format!("{} has no session header", input_path.display()),
        )
    })?;
    let header = decode_header(first.as_bytes())?;
    let entries = lines
        .map(|line| decode_entry(line.as_bytes()))
        .collect::<Result<Vec<_>, OxaError>>()?;
    let dest = match output {
        Some(o) => resolve_path(home, cwd, o),
        None => default_output_name(Some(&input_path), &header.id, cwd),
    };
    if aliases_source(&input_path, &dest) {
        return Err(OxaError::new(
            codes::INVALID,
            format!(
                "cannot export to {}: the destination is the source session",
                dest.display()
            ),
        )
        .with_hint("choose a different output path"));
    }
    if format_for(&dest) == ExportFormat::Jsonl {
        let permissions = source
            .metadata()
            .map_err(|e| OxaError::io("cannot inspect source session", &e))?
            .permissions();
        return write_session_export(&source, &dest, &text, permissions);
    }
    let opts = ExportOpts {
        theme: ExportTheme::dark(),
        session_name: None,
        system_prompt: None,
        tools: Value::Array(Vec::new()),
        rendered_tools: Vec::new(),
        leaf: None,
    };
    let html = export_html(&header, &entries, &opts)?;
    write_session_export(&source, &dest, &html, std::fs::Permissions::from_mode(0o644))
}

/// Compare the opened files before truncating: pathname checks alone race with alias replacement.
fn write_session_export(
    source: &std::fs::File,
    dest: &Path,
    text: &str,
    permissions: std::fs::Permissions,
) -> Result<PathBuf, OxaError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let write_error = |e: std::io::Error| OxaError::io(&format!("cannot write {}", dest.display()), &e);
    let source_meta = source.metadata().map_err(write_error)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dest)
        .map_err(write_error)?;
    let output_meta = output.metadata().map_err(write_error)?;
    if source_meta.dev() == output_meta.dev() && source_meta.ino() == output_meta.ino() {
        return Err(OxaError::new(
            codes::INVALID,
            "export destination is the source session",
        ));
    }
    output.set_len(0).map_err(write_error)?;
    output.write_all(text.as_bytes()).map_err(write_error)?;
    output.set_permissions(permissions).map_err(write_error)?;
    Ok(dest.to_path_buf())
}

/// The error code of a declined cross-directory fork offer (CLI-019): `run` prints
/// "Aborted." and exits 0, as pi does.
const FORK_DECLINED: &str = "oxa_cli_fork_declined";

/// An RPC client is listening for owner questions (the host sets `OXA_RPC_OWNER_PRESENT=1`).
fn rpc_owner_present(mode: RunMode, env: &Env) -> bool {
    mode == RunMode::Rpc && env.get("OXA_RPC_OWNER_PRESENT") == Some("1")
}

/// One line from `stdin`, read byte by byte (nothing after the newline is consumed).
fn read_line(stdin: &mut dyn Read) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stdin.read(&mut byte) {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8_lossy(&line).into_owned()
}

/// Piped stdin (trimmed), `None` when stdin is a terminal or empty.
fn read_piped_stdin(stdin: &mut dyn Read, stdin_is_tty: bool) -> Result<Option<String>, OxaError> {
    if stdin_is_tty {
        return Ok(None);
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|e| OxaError::io("cannot read stdin", &e))?;
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

/// The prompts of a run, as pi builds them: piped stdin, the `@file` texts and the first
/// message form the first prompt (images become parts of it); each further message is its own
/// prompt.
fn build_prompts(
    a: &Args,
    dirs: &Dirs,
    cwd: &Path,
    stdin_text: Option<String>,
) -> Result<Vec<(String, Vec<Part>)>, OxaError> {
    let mut text = stdin_text.unwrap_or_default();
    let mut parts: Vec<Part> = Vec::new();
    for arg in &a.files {
        let path = resolve_path(&dirs.home, cwd, arg);
        let shown = path.display().to_string();
        let meta = std::fs::metadata(&path)
            .map_err(|_| OxaError::new(codes::NOT_FOUND, format!("File not found: {shown}")))?;
        if meta.len() == 0 {
            continue;
        }
        let data =
            std::fs::read(&path).map_err(|e| OxaError::io(&format!("Could not read file {shown}"), &e))?;
        match image_mime(&data) {
            Some(mime) => {
                let img = ClipImage {
                    data,
                    mime: mime.to_string(),
                };
                parts.push(image_part(&img, &dirs.tmp_dir)?);
                text.push_str(&format!("<file name=\"{shown}\"></file>\n"));
            }
            None => {
                let content = String::from_utf8_lossy(&data);
                let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
                text.push_str(&format!("<file name=\"{shown}\">\n{content}\n</file>\n"));
            }
        }
    }
    let mut messages = a.messages.iter();
    if let Some(first) = messages.next() {
        text.push_str(first);
    }
    let mut prompts = Vec::new();
    if !text.is_empty() || !parts.is_empty() {
        prompts.push((text, parts));
    }
    for message in messages {
        prompts.push((message.clone(), Vec::new()));
    }
    Ok(prompts)
}

/// The image type by its magic bytes (PNG, JPEG, GIF, WebP).
fn image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF8") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A path argument: `~` expands to `home` (from the `Env` given to `run`, never the process
/// environment: P56), a relative path is taken from `cwd`.
fn resolve_path(home: &Path, cwd: &Path, arg: &str) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// `--system-prompt` and `--append-system-prompt`: the value is text, or the contents of an
/// existing file.
fn text_or_file(value: &str, home: &Path, cwd: &Path) -> Result<String, OxaError> {
    let path = resolve_path(home, cwd, value);
    if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))
    } else {
        Ok(value.to_string())
    }
}

fn write_notices(err: &mut dyn Write, notices: Vec<(Level, String)>) {
    write_diagnostics(err, notices.into_iter());
}

pub(crate) fn write_diagnostics(err: &mut dyn Write, notices: impl Iterator<Item = (Level, String)>) {
    for (level, text) in notices {
        let label = match level {
            Level::Info => "Info",
            Level::Warning => "Warning",
            Level::Error => "Error",
        };
        let _ = writeln!(err, "{label}: {text}");
    }
}

/// One error line on stderr: the message, then the hint in parentheses. Returns the exit code.
fn report(err: &mut dyn Write, e: &OxaError) -> i32 {
    let line = match &e.hint {
        Some(hint) => format!("{} ({hint})", e.message),
        None => e.message.clone(),
    };
    let _ = writeln!(err, "Error: {line}");
    exit_code(e)
}

fn finish_outcome(result: Result<oxa_core::RunOutcome, OxaError>, err: &mut dyn Write) -> i32 {
    finish_outcome_facts(result, err, None)
}

fn finish_outcome_facts(
    result: Result<oxa_core::RunOutcome, OxaError>,
    err: &mut dyn Write,
    facts: Option<&crate::result_facts::ResultFacts>,
) -> i32 {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            report(err, &error);
            oxa_core::RunOutcome::from_error(&error)
        }
    };
    let _ = writeln!(
        err,
        "{}",
        crate::result_facts::ResultFacts::record(outcome, facts)
    );
    let _ = err.flush();
    outcome.exit_code()
}

fn finish(result: Result<i32, OxaError>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => report(err, &e),
    }
}

/// 2 for usage errors, an unsupported pi flag and "no engine"; 1 for every other failure.
fn exit_code(e: &OxaError) -> i32 {
    match e.code {
        codes::USAGE | codes::ENGINE_DOWN | codes::UNSUPPORTED_IN_OXA => 2,
        codes::BUDGET_EXHAUSTED => oxa_proto::EXIT_BUDGET,
        codes::TERMINAL_CLOSED => 129,
        _ => 1,
    }
}

/// Fire `project_trust` on the extensions that may decide trust (pi: user/global and CLI `-e`
/// extensions only; project-local ones are not loaded before trust is resolved), in a host of
/// their own that is dropped afterwards. The first `yes` / `no` wins; `remember: true` saves it.
/// `None` = no extension decided.
fn extension_trust(
    a: &Args,
    dirs: &Dirs,
    cwd: &Path,
    user_settings: &Settings,
    store: &mut TrustStore,
    notices: &mut Vec<(Level, String)>,
) -> Option<bool> {
    use oxa_proto::{HookPoint, HookReply, Hooks};
    let opts = DiscoverOpts {
        trusted: false,
        no_context: true,
        no_skills: true,
        no_templates: true,
        no_extensions: a.no_extensions,
        extra_extensions: a.extensions.clone(),
        ..DiscoverOpts::default()
    };
    let paths = discover(dirs, cwd, user_settings, &opts).extension_paths;
    if paths.is_empty() {
        return None;
    }
    let host = match oxa_ext::ExtensionHost::new(cwd.to_path_buf()) {
        Ok(h) => h,
        Err(_) => return None,
    };
    let _ = host.load(&paths, &oxa_ext::LoadOpts::default());
    host.attach(Arc::new(TrustHost {
        cwd: cwd.to_path_buf(),
        mode: a.permission_mode.unwrap_or(Mode::Workspace),
    }));
    // Load errors are reported by the full load after trust is resolved.
    let _ = host.state().take_errors();
    let payload = serde_json::json!({ "cwd": cwd.display().to_string() });
    let mut ui = oxa_proto::HeadlessUi::default();
    let reply = host.fire(HookPoint::ProjectTrust, &payload, &mut ui);
    for e in host.state().take_errors() {
        notices.push((
            Level::Warning,
            format!(
                "Extension \"{}\" project_trust error: {}",
                e.extension_path, e.error
            ),
        ));
    }
    host.shutdown();
    match reply {
        HookReply::Trust(t) => Some(t),
        HookReply::TrustRemembered(t) => {
            if let Err(e) = store.save(&dirs.trust_file(), cwd, t) {
                notices.push((Level::Warning, e.message));
            }
            Some(t)
        }
        _ => None,
    }
}

/// The host of the trust-time extensions: the session does not exist yet, so only file reads
/// and writes are served (through the permission policy of the mode, no prompt); every other
/// action is refused (pi's limited `ProjectTrustContext`).
struct TrustHost {
    cwd: PathBuf,
    mode: Mode,
}

impl oxa_proto::Host for TrustHost {
    fn call(&self, action: oxa_proto::HostAction) -> Result<Value, OxaError> {
        use oxa_proto::{Access, Decision, HostAction, Policy};
        let gate = |access: Access| match ModePolicy::new().decide(self.mode, &access, &self.cwd) {
            Decision::Allow => Ok(()),
            _ => Err(OxaError::new(
                codes::DENIED,
                "not allowed while project trust is being resolved",
            )),
        };
        match action {
            HostAction::ReadFile(path) => {
                let p = oxa_perm::normalize(&path, &self.cwd);
                gate(Access::Read(p.clone()))?;
                std::fs::read_to_string(&p)
                    .map(Value::String)
                    .map_err(|e| OxaError::io(&format!("read {}", p.display()), &e))
            }
            HostAction::WriteFile { path, data } => {
                let p = oxa_perm::normalize(&path, &self.cwd);
                gate(Access::Write(p.clone()))?;
                std::fs::write(&p, &data)
                    .map(|_| Value::Null)
                    .map_err(|e| OxaError::io(&format!("write {}", p.display()), &e))
            }
            HostAction::IsIdle => Ok(Value::Bool(true)),
            _ => Err(OxaError::new(
                codes::UNSUPPORTED_IN_OXA,
                "only file access is available while project trust is being resolved",
            )),
        }
    }
}

#[cfg(test)]
mod model_limits_tests {
    use super::*;
    use oxa_proto::{AssistantTurn, CancelToken, ChatRequest, EngineInfo, StreamEvent};

    struct CatalogEngine;

    impl Engine for CatalogEngine {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            let models =
                serde_json::from_str(include_str!("../../oxa-engine/tests/fixtures/remote-models.json"))
                    .unwrap();
            let status = serde_json::json!({
                "model": "local", "context_size": 131072, "max_output": 16384,
                "model_limits": {"anthropic/claude-sonnet-5-5": {"capabilities":
                    {"remote": true, "remote_provider": "anthropic"}}}
            });
            oxa_engine::parse_info_for("http://localhost:1234", &models, &status, target)
        }

        fn count_tokens(&self, _: &str) -> Result<u32, OxaError> {
            panic!("listing models must not tokenize")
        }

        fn chat(
            &self,
            _: &ChatRequest,
            _: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            panic!("listing models must not generate")
        }
    }

    #[test]
    fn list_models_uses_each_models_limits() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&CatalogEngine, "oxe", Some("sonnet"), &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let row: Vec<_> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(&row[1..4], &["anthropic/claude-sonnet-5-5", "200K", "32K"]);

        let mut out = Vec::new();
        list_models(&CatalogEngine, "oxe", Some("local"), &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let row: Vec<_> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(&row[1..4], &["local", "131.1K", "16.4K"]);
    }

    #[test]
    fn list_models_shows_the_cloud_provider() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&CatalogEngine, "oxe", None, &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        let col0 = |id: &str| {
            let l = text.lines().find(|l| l.split_whitespace().nth(1) == Some(id));
            l.unwrap().split_whitespace().next().unwrap().to_string()
        };
        assert_eq!(col0("anthropic/claude-sonnet-5-5"), "anthropic");
        assert_eq!(col0("local"), "oxe");
    }

    #[test]
    fn list_models_provider_falls_back_to_remote_model_prefix() {
        let mut info = CatalogEngine.info(None).unwrap();
        info.caps.remote = true;
        info.caps.remote_provider = None;
        for (id, expected) in [
            ("chatgpt/subscription-model", "chatgpt"),
            ("future-provider/model", "future-provider"),
            ("unprefixed-model", "cloud"),
            ("/model", "cloud"),
            ("provider/", "cloud"),
        ] {
            info.limits_for = id.to_string();
            assert_eq!(row_provider("oxe", &info), expected, "{id}");
        }
        info.limits_for = "chatgpt/subscription-model".to_string();
        info.caps.remote_provider = Some("openrouter");
        assert_eq!(row_provider("oxe", &info), "openrouter", "gateway provider wins");
        info.caps.remote_provider = None;
        info.caps.remote = false;
        assert_eq!(row_provider("oxe", &info), "oxe", "local endpoint is unchanged");
    }

    /// A failing per-model `info(Some(id))` degrades only that row: '?' in the context,
    /// max-out, thinking and images columns, while every other model still lists.
    struct FailingInfoEngine {
        fail_id: String,
    }

    impl Engine for FailingInfoEngine {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            if target == Some(self.fail_id.as_str()) {
                return Err(OxaError::new(codes::ENGINE_HTTP, "boom"));
            }
            CatalogEngine.info(target)
        }

        fn count_tokens(&self, _: &str) -> Result<u32, OxaError> {
            panic!("listing models must not tokenize")
        }

        fn chat(
            &self,
            _: &ChatRequest,
            _: &CancelToken,
            _: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            panic!("listing models must not generate")
        }
    }

    #[test]
    fn list_models_degrades_a_failing_info_row() {
        let engine = FailingInfoEngine {
            fail_id: "anthropic/claude-sonnet-5-5".to_string(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        list_models(&engine, "oxe", None, &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();

        // The listing succeeds and still contains every model id.
        for id in ["local", "anthropic/claude-sonnet-5-5"] {
            assert!(
                text.lines().any(|l| l.split_whitespace().nth(1) == Some(id)),
                "missing {id}"
            );
        }

        // A one-line warning on stderr names the degrading row and its error.
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("anthropic/claude-sonnet-5-5") && err.contains("boom"),
            "stderr names the row and error: {err:?}"
        );

        // The failing row shows '?' in the context, max-out, thinking and images columns.
        let row: Vec<_> = text
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some("anthropic/claude-sonnet-5-5"))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(&row[2..], &["?", "?", "?", "?"]);
    }
}

#[cfg(test)]
mod animation_override_tests {
    use super::*;

    /// #869: `--animation <mode>` reaches the settings the interactive driver reads
    /// (`ui.animation`), `off` is the same as `ui.reducedMotion` for the run (the YOLO flames
    /// follow the badge), and no flag leaves the resolved setting alone.
    #[test]
    fn the_flag_overrides_ui_animation_for_this_run_only() {
        let loaded = Settings::from_values(serde_json::json!({"ui": {"animation": "arcade"}}), Value::Null);
        assert_eq!(loaded.string("ui.animation").as_deref(), Some("arcade"));
        assert!(!loaded.bool("ui.reducedMotion", false));

        let off = animation_override(loaded.clone(), Some("off"));
        assert_eq!(off.string("ui.animation").as_deref(), Some("off"));
        assert!(off.bool("ui.reducedMotion", false));
        // The settings the flag was applied to are untouched: the override is in memory only.
        assert_eq!(loaded.string("ui.animation").as_deref(), Some("arcade"));
        assert!(!loaded.bool("ui.reducedMotion", false));

        assert_eq!(
            animation_override(loaded.clone(), None)
                .string("ui.animation")
                .as_deref(),
            Some("arcade")
        );
        assert!(!animation_override(loaded.clone(), None).bool("ui.reducedMotion", false));

        // The other two modes draw, even when the settings ask for reduced motion.
        let still = Settings::from_values(
            serde_json::json!({"ui": {"animation": "off", "reducedMotion": true}}),
            Value::Null,
        );
        for mode in ["arcade", "minimal"] {
            let s = animation_override(still.clone(), Some(mode));
            assert_eq!(s.string("ui.animation").as_deref(), Some(mode));
            assert!(!s.bool("ui.reducedMotion", false), "--animation {mode}");
        }
        // ... and the settings on disk still say reduced motion.
        assert!(still.bool("ui.reducedMotion", false));
    }
}

#[cfg(test)]
mod session_scratch_tests {
    use super::*;
    use oxa_proto::ExecSpec;
    use oxa_sandbox::{Layout, NetSandbox};
    use std::os::unix::fs::FileTypeExt;

    fn socket_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .map(|path| {
                if path.symlink_metadata().is_ok_and(|m| m.file_type().is_socket()) {
                    1
                } else if path.is_dir() {
                    socket_count(&path)
                } else {
                    0
                }
            })
            .sum()
    }

    /// The host directory a session's sandbox mounts at `/tmp` (`None` = a fresh tmpfs).
    fn tmp_dir(d: &Deps, cwd: &Path) -> Option<PathBuf> {
        let spec = ExecSpec {
            command: "true".into(),
            cwd: cwd.to_path_buf(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        };
        let a = d.sandbox.bwrap_args(&spec);
        a.windows(3)
            .find(|w| w[0] == "--bind" && w[2] == "/tmp")
            .map(|w| PathBuf::from(&w[1]))
    }

    /// #457 (Codex P2): every session the process opens (the first one, `/new`, `/resume`,
    /// fork and clone all go through `Built::deps`) gets its own scratch directory, and the
    /// directory is deleted when that session's dependencies are dropped. The process sandbox
    /// itself (exec hooks) keeps the per-command tmpfs.
    #[test]
    fn each_session_gets_its_own_scratch_and_drops_it() {
        let env = oxa_fake::TestEnv::new();
        let home = env.home();
        let root = oxa_sandbox::scratch_root(&home);
        let bwrap = Bwrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            home: home.clone(),
            layout: Layout {
                scratch_root: Some(root.clone()),
                ..Layout::default()
            },
        };
        let base: Arc<dyn Sandbox> = Arc::new(NetSandbox::new(Arc::new(bwrap), true, false));
        let yolo = oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Standard);
        let built = Built {
            lead: false,
            status: None,
            result_facts: Arc::new(crate::result_facts::ResultFacts::default()),
            output_schema: None,
            goal: None,
            engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
            cloud_diagnostics: oxc_gateway::diagnostics::Diagnostics::default(),
            store: Arc::new(MemStore::new()),
            sandbox: base.clone(),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            resources: Resources::default(),
            theme_paths: Vec::new(),
            selection: ToolSelection::default(),
            model: None,
            thinking: None,
            mode: None,
            supervised_child: false,
            fork_seed: None,
            ask_owner_present: true,
            report_dialog: None,
            hook_sets: HookSets::default(),
            bash_sandbox: Arc::new(oxa_sandbox::FullAccess::new(
                base.clone(),
                yolo.clone(),
                "",
                home,
                false,
                false,
            )),
            os_sandbox: false,
            yolo,
            danger: Arc::new(oxa_perm::DangerRules::builtin()),
            approval_desk: None,
            unattended: UnattendedMode::default(),
        };
        let first = built.deps(None);
        let second = built.deps(None);
        let a = tmp_dir(&first, &env.work()).expect("first session has a scratch directory");
        let b = tmp_dir(&second, &env.work()).expect("second session has a scratch directory");
        assert_ne!(a, b, "two sessions share one /tmp");
        assert!(a.starts_with(&root) && b.starts_with(&root), "{a:?} {b:?}");
        std::fs::write(a.join("left-by-first"), "x").unwrap();
        assert!(!b.join("left-by-first").exists());
        assert_eq!(
            first.sandbox.persistence_notice(),
            Some(oxa_sandbox::SCRATCH_NOTICE)
        );
        assert!(base
            .persistence_notice()
            .unwrap()
            .contains("discarded after each command"));
        // #597: the model names that directory as `/tmp`; the file tools read it unasked, the
        // other session's directory still asks.
        use oxa_proto::{Access, Decision, Mode};
        let cwd = env.work();
        assert_eq!(
            first.sandbox.scratch_dir(),
            Some(a.as_path()),
            "the file tools and the sandbox name one directory"
        );
        assert_eq!(
            first
                .policy
                .decide(Mode::Workspace, &Access::Read(a.join("left-by-first")), &cwd),
            Decision::Allow
        );
        assert_eq!(
            first
                .policy
                .decide(Mode::Workspace, &Access::Read(b.join("left-by-first")), &cwd),
            Decision::Ask
        );
        drop(first);
        assert!(!a.exists(), "the first session's scratch outlived it");
        assert!(b.is_dir(), "ending one session removed another's scratch");
        drop(second);
        assert!(!b.exists(), "the second session's scratch outlived it");
    }

    /// #1819: after the worker closes an interactive session, both the requested replacement
    /// and the fallback open use this path and must export a fresh owner-confirmation broker.
    #[test]
    fn interactive_reopens_install_a_fresh_owner_confirmation_broker() {
        let env = oxa_fake::TestEnv::new();
        let base: Arc<dyn Sandbox> = Arc::new(NoSandbox);
        let yolo = oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Standard);
        let built = Built {
            lead: false,
            status: None,
            result_facts: Arc::new(crate::result_facts::ResultFacts::default()),
            output_schema: None,
            goal: None,
            engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
            cloud_diagnostics: oxc_gateway::diagnostics::Diagnostics::default(),
            store: Arc::new(MemStore::new()),
            sandbox: base.clone(),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            resources: Resources::default(),
            theme_paths: Vec::new(),
            selection: ToolSelection::default(),
            model: None,
            thinking: None,
            mode: None,
            supervised_child: false,
            fork_seed: None,
            ask_owner_present: true,
            report_dialog: None,
            hook_sets: HookSets::default(),
            bash_sandbox: Arc::new(oxa_sandbox::FullAccess::new(
                base,
                yolo.clone(),
                "",
                env.home(),
                false,
                false,
            )),
            os_sandbox: false,
            yolo,
            danger: Arc::new(oxa_perm::DangerRules::builtin()),
            approval_desk: None,
            unattended: UnattendedMode::default(),
        };

        let first = open_interactive_session(&built, built.deps(None), Open::New).unwrap();
        assert_eq!(socket_count(&built.dirs.tmp_dir), 1);
        drop(first);
        assert_eq!(socket_count(&built.dirs.tmp_dir), 0);

        // The worker calls the same closure a second time when the requested switch fails.
        let recovered = open_interactive_session(&built, built.deps(None), Open::New).unwrap();
        assert_eq!(socket_count(&built.dirs.tmp_dir), 1);
        drop(recovered);
    }

    /// #1512 S7: an edit list in `oxa.approval.dangerous` edits the built-in rules. Adding a
    /// custom rule keeps every built-in rule, `-id` removes only that rule, and only a plain
    /// list replaces the built-in set (the merged value must not turn `+x` into a plain list).
    #[test]
    fn dangerous_rules_edit_list_keeps_the_builtin_rules() {
        let env = oxa_fake::TestEnv::new();
        let guard = oxa_perm::PathGuard::new(&env.home(), &env.agent_dir(), &env.tmp());
        let cwd = env.work();
        let rules = |list: serde_json::Value| {
            let user = serde_json::json!({"oxa": {"approval": {"dangerous": list}}});
            dangerous_rules(&Settings::from_values(user, serde_json::json!({}))).0
        };
        let hits = |r: &DangerRules, cmd: &str| -> Vec<String> {
            r.classify(cmd, &cwd, &guard)
                .into_iter()
                .map(|h| h.rule)
                .collect()
        };
        let has = |r: &DangerRules, cmd: &str, rule: &str| hits(r, cmd).iter().any(|h| h == rule);
        let force = "git push --force origin main";
        let added = rules(serde_json::json!(["+cmd:terraform destroy"]));
        assert!(has(&added, "terraform destroy", "cmd:terraform destroy"));
        assert!(has(&added, force, "git-push-force"));
        assert!(has(&added, "sudo ls", "sudo"));
        let edited = rules(serde_json::json!(["-sudo", "+cmd:terraform destroy"]));
        assert!(!has(&edited, "sudo ls", "sudo"));
        assert!(has(&edited, force, "git-push-force"));
        assert!(has(&edited, "terraform destroy", "cmd:terraform destroy"));
        let removed = rules(serde_json::json!(["-sudo"]));
        assert!(!has(&removed, "sudo ls", "sudo"));
        assert!(has(&removed, force, "git-push-force"));
        let replaced = rules(serde_json::json!(["cmd:terraform destroy"]));
        assert!(hits(&replaced, force).is_empty());
        assert!(has(&replaced, "terraform destroy", "cmd:terraform destroy"));
    }
    /// #1512 S2: the dangerous-command rules and the always keys come from the user settings
    /// only. A project file that replaces the rules and lists the key changes nothing; the same
    /// key in the user file allows the command.
    #[test]
    fn approval_keys_are_read_from_user_layer_only() {
        use oxa_proto::{Access, Decision, Mode};
        let env = oxa_fake::TestEnv::new();
        let dirs = Dirs::under(env.root());
        let cwd = env.work();
        let key = "git-push-force git push --force origin main";
        // The default list is every built-in rule (an empty list never means "no rules").
        assert_eq!(dangerous_rules(&Settings::default()).0, DangerRules::builtin());
        let dangerous = Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
        };
        env.write(
            "work/.oxa/settings.json",
            &serde_json::json!({"oxa": {"approval": {"dangerous": [], "always": [key]}}}).to_string(),
        );
        let build = || {
            let base: Arc<dyn Sandbox> = Arc::new(NoSandbox);
            let yolo = oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Standard);
            Built {
                lead: false,
                status: None,
                result_facts: Arc::new(crate::result_facts::ResultFacts::default()),
                output_schema: None,
                goal: None,
                engine: Arc::new(oxa_fake::ScriptedEngine::new(Vec::new())),
                cloud_diagnostics: oxc_gateway::diagnostics::Diagnostics::default(),
                store: Arc::new(MemStore::new()),
                sandbox: base.clone(),
                settings: Settings::default(),
                dirs: dirs.clone(),
                cwd: cwd.clone(),
                resources: Resources::default(),
                theme_paths: Vec::new(),
                selection: ToolSelection::default(),
                model: None,
                thinking: None,
                mode: None,
                supervised_child: false,
                fork_seed: None,
                ask_owner_present: true,
                hook_sets: HookSets::default(),
                bash_sandbox: Arc::new(oxa_sandbox::FullAccess::new(
                    base.clone(),
                    yolo.clone(),
                    "",
                    env.home(),
                    false,
                    false,
                )),
                os_sandbox: false,
                yolo,
                danger: Arc::new(oxa_perm::DangerRules::builtin()),
                approval_desk: None,
                unattended: UnattendedMode::default(),
                report_dialog: None,
            }
        };
        let policy = build().deps(None).policy;
        assert_eq!(policy.decide(Mode::Workspace, &dangerous, &cwd), Decision::Ask);
        // The user layer, the same key: allowed.
        std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa": {"approval": {"always": [key]}}}).to_string(),
        )
        .unwrap();
        let policy = build().deps(None).policy;
        assert_eq!(policy.decide(Mode::Workspace, &dangerous, &cwd), Decision::Allow);
    }

    /// #1512 S5: the unattended answer is user-only. A project file that names `refuse` changes
    /// nothing; the same value in the user file is read.
    #[test]
    fn project_settings_cannot_set_approval_unattended() {
        let env = oxa_fake::TestEnv::new();
        let dirs = Dirs::under(env.root());
        let cwd = env.work();
        env.write(
            "work/.oxa/settings.json",
            &serde_json::json!({"oxa": {"approval": {"unattended": "refuse"}}}).to_string(),
        );
        assert_eq!(
            Settings::load(&dirs, &cwd, false).string("oxa.approval.unattended"),
            None
        );
        std::fs::create_dir_all(dirs.settings_file().parent().unwrap()).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa": {"approval": {"unattended": "request"}}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            Settings::load(&dirs, &cwd, false)
                .string("oxa.approval.unattended")
                .as_deref(),
            Some("request")
        );
    }
}

#[cfg(test)]
mod goal_flag_tests {
    use super::*;

    /// G3: `--goal` with no print flag and a terminal on both ends opens the interactive
    /// session, and the mode guard in `prepare` accepts it (RPC still refuses it).
    #[test]
    fn interactive_goal_flag_is_accepted() {
        let argv: Vec<String> = ["--goal", "goal.txt"].iter().map(|s| s.to_string()).collect();
        let a = parse_args(&argv, &[]).expect("--goal parses");
        assert_eq!(a.goal.as_deref(), Some("goal.txt"));
        let mode = choose_mode(&a, true, true);
        assert_eq!(mode, RunMode::Interactive);
        assert!(goal_flag_accepted(mode));
        assert!(goal_flag_accepted(RunMode::Print));
        assert!(!goal_flag_accepted(RunMode::Rpc));
    }

    /// G3 fix: the goal start leads the first prompt, so the first request carries the
    /// objective; with no first prompt the start alone is sent.
    #[test]
    fn goal_start_leads_first_prompt() {
        assert_eq!(
            goal_first_prompt("START".into(), Some("hello".into())),
            "START\n\nhello"
        );
        assert_eq!(goal_first_prompt("START".into(), None), "START");
    }
}

#[cfg(test)]
mod export_source_tests {
    use super::*;

    /// A session file with the header `decode_header` requires, for the alias checks.
    const HEADER: &str =
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/work"}"#;

    fn session(env: &oxa_fake::TestEnv) -> PathBuf {
        let path = env.home().join("session-1.jsonl");
        std::fs::write(&path, format!("{HEADER}\n")).unwrap();
        path
    }

    /// `--export` must never write onto the session it reads: the same path, a hard link and a
    /// symlink all name the source file, whatever the destination suffix.
    #[test]
    fn destinations_aliasing_the_source_are_detected_everywhere() {
        let env = oxa_fake::TestEnv::new();
        let source = session(&env);
        let work = env.work();
        let hard = work.join("hard.jsonl");
        let soft = work.join("soft.html");
        std::fs::hard_link(&source, &hard).unwrap();
        std::os::unix::fs::symlink(&source, &soft).unwrap();
        let sibling = work.join("copy.jsonl");
        std::fs::write(&sibling, "other\n").unwrap();

        for dest in [source.clone(), hard, soft] {
            assert!(
                aliases_source(&source, &dest),
                "{} names the source session",
                dest.display()
            );
        }
        assert!(
            !aliases_source(&source, &sibling),
            "an existing other file is a copy"
        );
        assert!(
            !aliases_source(&source, &env.home().join("missing.jsonl")),
            "a new path is a new output"
        );
    }

    /// The refusal is a CLI error that names the destination, so the session is left alone.
    #[test]
    fn exporting_onto_the_source_is_refused() {
        let env = oxa_fake::TestEnv::new();
        let source = session(&env);
        let before = std::fs::read(&source).unwrap();
        let err = export_inner(
            source.to_str().unwrap(),
            Some(source.to_str().unwrap()),
            &env.home(),
            &env.work(),
        )
        .expect_err("a JSONL destination naming the source must be refused");
        assert_eq!(err.code, codes::INVALID);
        assert!(err.message.contains("source session"), "{}", err.message);
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }
}

#[cfg(test)]
mod bash_sandbox_tests {
    use super::*;

    /// A platform without an OS sandbox that reports the bash network as unenforceable.
    struct NoNetworkIsolation;

    impl Sandbox for NoNetworkIsolation {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn network(&self) -> bool {
            false
        }
        fn network_enforced(&self) -> bool {
            false
        }
        fn bwrap_args(&self, _s: &oxa_proto::ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(
            &self,
            _s: &oxa_proto::ExecSpec,
            _c: &oxa_proto::CancelToken,
            _live: &mut dyn FnMut(&[u8]),
        ) -> oxa_proto::ExecResult {
            oxa_proto::ExecResult::default()
        }
    }

    /// A refused bash network that this platform cannot enforce disables bash instead of running
    /// a command with the network the owner turned off.
    #[test]
    fn a_refused_network_that_cannot_be_enforced_disables_bash() {
        let (sandbox, notice) = bash_sandbox(Arc::new(NoNetworkIsolation), false, false);
        assert!(notice.unwrap_or_default().contains("bash is disabled"));
        assert_eq!(
            sandbox.check().unwrap_err().code,
            oxa_proto::codes::NO_SANDBOX,
            "the replacement never runs a command"
        );
    }

    /// Without a refused network the sandbox is kept, notice and all.
    #[test]
    fn an_allowed_network_keeps_the_sandbox() {
        struct NetOn;
        impl Sandbox for NetOn {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn network(&self) -> bool {
                true
            }
            fn network_enforced(&self) -> bool {
                false
            }
            fn bwrap_args(&self, _s: &oxa_proto::ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(
                &self,
                _s: &oxa_proto::ExecSpec,
                _c: &oxa_proto::CancelToken,
                _live: &mut dyn FnMut(&[u8]),
            ) -> oxa_proto::ExecResult {
                oxa_proto::ExecResult::default()
            }
        }
        let (sandbox, notice) = bash_sandbox(Arc::new(NetOn), false, false);
        assert!(notice.is_none());
        assert!(sandbox.check().is_ok());
    }

    /// Interactive: the unenforceable network setting keeps bash and warns.
    #[test]
    fn interactive_unenforced_network_keeps_bash_with_warning() {
        let (sandbox, notice) = bash_sandbox(Arc::new(NoNetworkIsolation), true, false);
        assert!(notice.unwrap_or_default().contains("NOT restricted"));
        assert!(sandbox.check().is_ok());
    }

    /// Offline is the owner's promise of no network: it still disables bash interactively.
    #[test]
    fn interactive_offline_still_disables_bash() {
        let (sandbox, notice) = bash_sandbox(Arc::new(NoNetworkIsolation), true, true);
        assert!(notice.unwrap_or_default().contains("bash is disabled"));
        assert!(sandbox.check().is_err());
    }

    /// S10: model bash can fall back without giving the inner executor host rights.
    #[cfg(target_os = "linux")]
    #[test]
    fn interactive_without_bwrap_runs_with_warning() {
        let test = oxa_fake::TestEnv::new();
        let env = Env::from_pairs(&[("PATH", "/usr/bin")]);
        let dirs = Dirs::under(test.root());
        let (sandbox, notice) = no_bwrap_sandbox(true, false, "no userns", &env, &dirs);
        let (level, msg) = notice.expect("a warning");
        assert_eq!(level, Level::Warning);
        assert!(msg.contains("no isolation"), "{msg}");
        let executor = Arc::new(
            oxa_sandbox::FullAccess::new(
                sandbox,
                oxa_proto::YoloSwitch::new(oxa_proto::Autonomy::Standard),
                "",
                dirs.home,
                false,
                false,
            )
            .with_os_sandbox(false),
        );
        let cancel = oxa_proto::CancelToken::new();
        let mut spec = oxa_proto::ExecSpec {
            command: "printf model-bash".into(),
            cwd: test.work(),
            writable: true,
            net: false,
            timeout_s: Some(5),
            env: Vec::new(),
        };
        let result = executor.run(&spec, &cancel, &mut |_| {});
        assert_eq!(result.exit, Some(0));
        assert_eq!(result.output, b"model-bash");
        assert!(result.unsandboxed);
        spec.writable = false;
        assert_eq!(executor.run(&spec, &cancel, &mut |_| {}).exit, None);
        assert!(oxa_sandbox::FullAccess::hook_sandbox(&executor).check().is_err());
    }

    /// Unattended runs keep "bash is disabled" on Linux (#1512).
    #[cfg(target_os = "linux")]
    #[test]
    fn print_without_bwrap_disables_bash() {
        let test = oxa_fake::TestEnv::new();
        let env = Env::from_pairs(&[("PATH", "/usr/bin")]);
        let dirs = Dirs::under(test.root());
        for interactive in [false, true] {
            let (sandbox, notice) = no_bwrap_sandbox(interactive, true, "no userns", &env, &dirs);
            let (_, msg) = notice.expect("a warning");
            assert!(msg.contains("bash is disabled"), "{msg}");
            assert_eq!(sandbox.check().unwrap_err().code, oxa_proto::codes::NO_SANDBOX);
        }
    }
}
