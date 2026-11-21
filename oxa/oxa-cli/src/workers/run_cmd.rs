//! Owner-invoked foreground supervision. No Lead session, transport or dispatcher.
use super::registry::{self, WorkerRecord, WorkerState, OWNER_PARENT};
use super::tool::{self, HostTools, ParentScope, WorkerConfig, WorkerCore};
use super::{proc, resume};
use oxa_config::{Dirs, Env, Settings};
use oxa_interactive::command_registry::CommandId;
use oxa_proto::{codes, Mode, OxaError, PermissionMode, Value, YoloSwitch};
use oxa_session::{FsStore, SessionStore};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::time::Duration;

#[derive(Default)]
struct Options<'a> {
    name: Option<&'a str>,
    job: Option<&'a str>,
    kind: Option<&'a str>,
    goal_file: Option<&'a str>,
    model: Option<&'a str>,
    resume: bool,
}

fn usage(text: &str) -> OxaError {
    OxaError::new(codes::USAGE, text).with_hint("see oxa worker run --help or oxa worker list --help")
}

fn parse(words: &[String], list: bool) -> Result<Options<'_>, OxaError> {
    let mut options = Options::default();
    let mut words = words.iter().map(String::as_str);
    while let Some(word) = words.next() {
        let (flag, inline) = word.split_once('=').map_or((word, None), |(f, v)| (f, Some(v)));
        let slot = match flag {
            "--parent" if inline.or_else(|| words.next()) == Some("kanban") => continue,
            "--json" if list && inline.is_none() => continue,
            "--resume" if !list && inline.is_none() => {
                options.resume = true;
                continue;
            }
            "--name" if !list => &mut options.name,
            "--job" if !list => &mut options.job,
            "--kind" if !list => &mut options.kind,
            "--goal-file" if !list => &mut options.goal_file,
            "--model" if !list => &mut options.model,
            _ => {
                return Err(usage(
                    "unknown worker option or parent (the owner parent is kanban)",
                ))
            }
        };
        let value = inline
            .or_else(|| words.next())
            .filter(|v| !v.is_empty() && !v.starts_with("--"));
        if value.is_none() || slot.replace(value.unwrap_or_default()).is_some() {
            return Err(usage("worker option needs exactly one value"));
        }
    }
    if !list {
        if options.name.is_none() && options.job.is_none() {
            return Err(usage("worker run needs --name or --job"));
        }
        if options.resume {
            if options.goal_file.is_some() || options.model.is_some() || options.kind.is_some() {
                return Err(usage("resume uses the saved goal, kind and model"));
            }
        } else if options.goal_file.is_none() {
            return Err(usage("a new worker needs --goal-file FILE or --goal-file -"));
        }
    }
    Ok(options)
}

pub(crate) fn run(
    id: CommandId,
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<i32, OxaError> {
    if crate::dispatch::is_agent(env) || crate::child::ChildIdentity::from_env(env, &mut Vec::new()).nested()
    {
        return Err(OxaError::new(
            codes::DENIED,
            "owner worker commands run outside agent tools",
        ));
    }
    let list = id == CommandId::WorkerList;
    let options = parse(words, list)?;
    if list {
        return list_workers(dirs, stdout);
    }
    let user = Settings::load(dirs, cwd, false);
    if !tool::enabled(&user) {
        return Err(OxaError::new(
            codes::DENIED,
            "workers are off (oxa.workers.enabled)",
        ));
    }
    // Read and validate before taking a lease or writing anything. Goal text goes to
    // the existing private goal file, never the child argv or the state stream.
    let start = options
        .goal_file
        .map(|path| {
            let objective = if path == "-" {
                crate::goal_cmd::read_objective(stdin)
            } else {
                let file = File::open(cwd.join(path)).map_err(|e| OxaError::io("read goal file", &e))?;
                crate::goal_cmd::read_objective(file)
            }?;
            let goal = crate::goal_cmd::from_objective(objective, None, None)?;
            let mut args = json!({"name": options.name.or(options.job), "goal": goal.objective});
            for (key, value) in [
                ("kind", options.kind),
                ("job", options.job),
                ("model", options.model),
            ] {
                if let Some(value) = value {
                    args[key] = json!(value);
                }
            }
            Ok::<_, OxaError>(args)
        })
        .transpose()?;

    let tools = HostTools::resolve(env.pairs(), cwd).map_err(|e| OxaError::new(codes::INVALID, e))?;
    let repo = &tools.project;
    let settings =
        if crate::work_cmd::trusted(repo, env, dirs, stderr).map_err(|e| OxaError::new(codes::INVALID, e))? {
            Settings::load(dirs, repo, true)
        } else {
            user.clone()
        };
    let sessions = if env.get("OXA_SESSION_DIR").is_none() {
        settings
            .session_dir(&dirs.home, repo)
            .unwrap_or_else(|| dirs.sessions_dir.clone())
    } else {
        dirs.sessions_dir.clone()
    };
    let sessions = cwd.join(sessions);
    let mut scope = tools
        .owner_scope(sessions.clone(), oxa_proto::offline())
        .map_err(|e| OxaError::new(codes::INVALID, e))?;
    let level = settings.default_permission_mode();
    let mode = if level == PermissionMode::Plan {
        Mode::Plan
    } else {
        Mode::Workspace
    };
    let autonomy = level.autonomy();
    let agent_dir = cwd.join(&dirs.agent_dir);
    let max = tool::max_workers(&user);
    let (models, notices) = super::model_policy::from_settings(&user);
    for notice in notices {
        let _ = writeln!(stderr, "worker: {notice}");
    }

    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])
        .map_err(|e| OxaError::io("install worker shutdown handler", &e))?;
    let supervisor = proc::start(&user, &tools.path, &tools.host_project, &tools.home)
        .map_err(|e| OxaError::io("start worker supervisor", &e))?;
    let bin = std::env::current_exe().map_err(|e| OxaError::io("resolve worker executable", &e))?;
    let lease = registry::owner_lease(&agent_dir)?;
    let mut records = registry::load(&agent_dir, OWNER_PARENT)?;
    let resume_index = if options.resume {
        let mut matching = records.iter().enumerate().filter(|(_, r)| {
            options.name.is_none_or(|name| name == r.name)
                && options.job.is_none_or(|job| job == r.job_key())
                && r.owner_scope.as_ref().is_some_and(|s| s.repository == *repo)
        });
        let (index, record) = matching
            .next()
            .ok_or_else(|| usage("no saved worker matches in this repository"))?;
        if matching.next().is_some() {
            return Err(usage("more than one attempt matches; use --name to select it"));
        }
        let saved = record.owner_scope.as_ref().expect("matched owner scope");
        scope.offline |= saved.offline;
        if saved.git_device != scope.git_device
            || saved.git_inode != scope.git_inode
            || saved.sessions_dir != sessions
            || !tools.matches_worktree(record, &scope)
        {
            return Err(OxaError::new(
                codes::DENIED,
                "worker repository, branch or session directory changed",
            ));
        }
        let alive =
            proc::recorded_group_alive(record.pgid).map_err(|e| OxaError::io("check worker liveness", &e))?;
        let status = tool::snapshot(record, &sessions);
        if !resume::due(record, &registry::state(record, status.as_ref(), alive), alive) {
            return Err(OxaError::new(
                codes::INVALID,
                "worker is active, stopped or already finished",
            ));
        }
        let store = FsStore::new(sessions.clone());
        let session = store.resolve(&record.session_id)?;
        if session.id != record.session_id || store.header(&session)?.cwd != record.worktree {
            return Err(OxaError::new(
                codes::DENIED,
                "saved session does not belong to this worktree",
            ));
        }
        let live = records.iter().filter(|r| tool::is_live(r, &sessions)).count();
        if live >= max {
            return Err(OxaError::new(
                codes::INVALID,
                "worker limit reached (oxa.workers.max)",
            ));
        }
        Some(index)
    } else {
        None
    };

    let core = WorkerCore::for_owner(
        WorkerConfig {
            cwd: repo.clone(),
            oxa_bin: bin,
            lead_dirs: None,
            agent_dir: agent_dir.clone(),
            max,
            supervisor,
            sessions_dir: sessions.clone(),
            parent_env: env.pairs().to_vec(),
            parent_scope: ParentScope {
                session_dir: Some(sessions.clone()),
                narrowing: super::launch::Narrowing {
                    offline: scope.offline,
                    ..Default::default()
                },
                ..Default::default()
            },
            yolo: YoloSwitch::new(autonomy),
            resume_on_start: false,
            advertise_wait: false,
            send_ack_seconds: 0,
            models,
            cache: None,
        },
        scope,
        user.string("defaultModel").unwrap_or_default(),
    );
    let record = if let Some(args) = start {
        let result = core.start(&args, repo, mode, autonomy);
        if result.is_error {
            return Err(OxaError::new(codes::INVALID, result.text));
        }
        registry::load(&agent_dir, OWNER_PARENT)?
            .pop()
            .ok_or_else(|| OxaError::new(codes::INTERNAL, "worker launch was not recorded"))?
    } else if let Some(index) = resume_index {
        core.resume_owner(&mut records, index, &tools, mode, autonomy)?
    } else {
        return Err(usage("worker run needs a goal or --resume"));
    };
    drop(lease);
    emit(
        stdout,
        &json!({
            "type": "started", "name": record.name, "session_id": record.session_id,
            "branch": record.branch, "model": record.model, "why": record.choice,
            "mode": record.mode, "pid": record.pid, "resumed": options.resume,
            "offline": record.owner_scope.as_ref().is_some_and(|s| s.offline),
        }),
    )?;
    supervise(&core, &record, &sessions, &mut signals, stdout)
}

fn supervise(
    core: &WorkerCore,
    record: &WorkerRecord,
    sessions: &Path,
    signals: &mut signal_hook::iterator::Signals,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let supervisor = core.supervisor();
    let mut last_phase = String::new();
    let status = loop {
        let mut status = supervisor.exited(record.pgid);
        if matches!(status, Ok(None)) && signals.pending().next().is_some() {
            status = supervisor.stop(record.pgid);
        }
        if let Some(status) = status.map_err(|e| OxaError::io("wait for worker", &e))? {
            break status;
        }
        let snapshot = tool::snapshot(record, sessions);
        let phase = snapshot
            .as_ref()
            .and_then(|s| s["phase"].as_str())
            .unwrap_or("starting");
        if phase != "finished" && phase != last_phase {
            emit(
                out,
                &json!({"type":"status", "session_id": record.session_id, "phase": phase}),
            )?;
            last_phase = phase.to_owned();
        }
        std::thread::park_timeout(Duration::from_millis(200));
    };
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    let snapshot = tool::snapshot(record, sessions).unwrap_or_default();
    let result = &snapshot["outcome"];
    let outcome = result["outcome"]
        .as_str()
        .unwrap_or(if status.success() { "done" } else { "failed" });
    let reason = result["error_class"]
        .as_str()
        .or_else(|| status.signal().map(|_| "process_signaled"))
        .or_else(|| result.is_null().then_some("no_terminal_status"));
    emit(
        out,
        &json!({"type":"finished", "session_id": record.session_id,
        "outcome":outcome, "reason":reason, "exit_code":code}),
    )?;
    Ok(code)
}

fn emit(out: &mut dyn Write, value: &Value) -> Result<(), OxaError> {
    writeln!(out, "{value}")
        .and_then(|()| out.flush())
        .map_err(|e| OxaError::io("write worker JSON", &e))
}

fn list_workers(dirs: &Dirs, out: &mut dyn Write) -> Result<i32, OxaError> {
    let mut live = 0;
    let workers: Vec<_> = registry::load(&dirs.agent_dir, OWNER_PARENT)?
        .into_iter()
        .filter_map(|r| {
            let scope = r.owner_scope.as_ref()?;
            let running = proc::recorded_group_alive(r.pgid).ok();
            live += usize::from(running == Some(true));
            let status = tool::snapshot(&r, &scope.sessions_dir);
            let (phase, outcome) = match registry::state(&r, status.as_ref(), running != Some(false)) {
                WorkerState::Starting => ("starting", None),
                WorkerState::Working => ("working", None),
                WorkerState::Idle => ("idle", None),
                WorkerState::Finished(outcome) => ("finished", Some(outcome)),
                WorkerState::Dead => ("dead", None),
                WorkerState::Stopped => ("stopped", None),
            };
            Some(
                json!({"name":r.name,"job":r.job_key(),"branch":r.branch,"model":r.model,
            "session_id":r.session_id,"pid":r.pid,"phase":phase,"outcome":outcome,"live":running}),
            )
        })
        .collect();
    let mut load = 0.0;
    // SAFETY: `load` is room for the one double requested.
    let load_1m = (unsafe { libc::getloadavg(&mut load, 1) } == 1).then_some(load);
    emit(
        out,
        &json!({"parent":"kanban", "live":live, "load_1m":load_1m,
        "free_memory_bytes":free_memory(), "workers":workers}),
    )?;
    Ok(0)
}

#[cfg(target_os = "linux")]
fn free_memory() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::sysinfo>::uninit();
    // SAFETY: sysinfo fills the structure on success.
    if unsafe { libc::sysinfo(info.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: sysinfo initialized the structure.
    let info = unsafe { info.assume_init() };
    (info.freeram as u128 * u128::from(info.mem_unit)).try_into().ok()
}

#[cfg(not(target_os = "linux"))]
fn free_memory() -> Option<u64> {
    None
}
