//! K5a: the `worker` tool. `start` launches a background worker (a child `oxa` process in its own
//! git worktree, see `launch.rs`), `list` reads the registry, `stop` ends one. The workers of a
//! session are recorded per parent session id (`registry.rs`); children are spawned through the
//! process supervisor (`proc.rs`).
//!
//! * Not built in a worker child: `OXA_WORKER_DEPTH` set means nested (see [`built`]).
//! * `start` checks, in order: the name, the goal, the parent session id, a worker of that name in
//!   this session, `oxa.workers.max` live workers, an existing branch `agent/<name>`. Then it makes
//!   the worktree (`git worktree add --no-checkout` on the host, then the checkout inside the
//!   sandbox through `oxa work --supervise`, so no repository filter runs on the host), writes the goal file (mode 0600), and spawns the child.
//! * The child runs under `env -C <worktree>`: its cwd is the worktree. Its environment is exactly
//!   `launch::child_env` with the trusted `PATH`, passed to the supervisor (which clears the
//!   environment first), so no value reaches argv and no secret is listed in `ps`.
//! * `Hooks::shutdown` stops every worker through the supervisor.
//! * K5b: `send {name, text}` publishes `text` to the worker's session inbox (the writer behind
//!   `oxa send`) and reports the delivery and the worker's phase. The child id is read from the
//!   registry record of `name`. `poll_event("worker_done", {name})` is done when the worker finished
//!   or its process is gone. On `session_start` (startup, resume) each dead, unfinished worker is
//!   relaunched through the same launch path (`resume.rs`) unless `oxa.workers.resumeOnStart`
//!   is false.
use super::launch::{check_value, child_argv, child_env, valid_name, worktree_argv, WorkerMode, WorkerSpec};
use super::model_policy::{self, Attempt, ModelPolicy};
use super::proc::Supervisor;
use super::registry::{self, OwnerScope, WorkerRecord, WorkerState};
use super::resume;
use oxa_proto::{
    Access, Autonomy, CommandSpec, Exposure, HookPoint, HookReply, Hooks, Host, HostAction, Level, Mode,
    OxaError, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value, YoloSwitch,
};
use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// The name the model calls.
pub const TOOL_NAME: &str = "worker";
/// `Hooks::name` and the source of the tool.
const HOOK_NAME: &str = "builtin:worker";
/// The command the permission layer sees for this tool.
const COMMAND: &str = "oxa worker";
/// Setting: the switch (`oxa-config/src/schema.rs`).
pub const SETTING_ENABLED: &str = "oxa.workers.enabled";
/// Setting: the most live workers one session runs.
pub const SETTING_MAX: &str = "oxa.workers.max";
pub const SETTING_REPOSITORY: &str = "oxa.workers.repository";
pub const SETTING_SEND_ACK: &str = "oxa.workers.sendAckSeconds";

/// `oxa.workers.sendAckSeconds` from the user settings only (default 10, 0 = do not wait).
pub fn send_ack_seconds(settings: &oxa_config::Settings) -> u64 {
    u64::from(settings.user_u32(SETTING_SEND_ACK, 10))
}
/// Env: set in a worker child (`launch::child_env` sets it to `1`). Any value, even empty, means
/// nested; only an unset variable lets the hook set be built.
pub const ENV_WORKER_DEPTH: &str = "OXA_WORKER_DEPTH";

const DESCRIPTION: &str = "Background workers: each is a separate oxa process that runs one goal in its own git worktree on branch agent/<name>. Actions: start {name, goal, model?, kind?, job?, reason?, mode?} starts one (name: lowercase letters, digits and dashes, at most 40 characters). Outside Git, a trusted Lead needs the owner to set oxa.workers.repository; no repository tool argument is accepted. Without model the owner's policy (oxa.workers.models) picks one by kind (a job kind such as review, docs or tests); after repeated failures on the same job (job: a stable key such as pr-2127, default the name) it escalates; with no policy the worker uses this session's model. reason is one line on why you chose that model or kind. list shows this session's workers, their model, choice and state; result {name, result: ok or failed, reason} records how a worker's work turned out (call it with failed when review or CI rejects it, so the policy can escalate); stop {name} stops one; send {name, text} delivers a follow-up message to a worker's session inbox and reports its phase. mode is plan, ask, workspace, auto or yolo, never wider than this session; without mode the worker inherits this session's level (a yolo session starts yolo workers, so they can push, open PRs and read CI).";

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","list","stop","send","result"]},"name":{"type":"string"},"goal":{"type":"string"},"text":{"type":"string"},"retry":{"type":"string"},"withdraw":{"type":"string"},"model":{"type":"string"},"kind":{"type":"string"},"job":{"type":"string"},"reason":{"type":"string"},"result":{"type":"string","enum":["ok","failed"]},"mode":{"type":"string","enum":["plan","ask","workspace","auto","yolo"]}},"required":["action"]}"#;

/// `oxa.workers.enabled` from the user settings only (default true): a project file cannot turn
/// process launch on or off.
pub fn enabled(settings: &oxa_config::Settings) -> bool {
    settings.user_bool(SETTING_ENABLED, true)
}

/// `oxa.workers.max` from the user settings only (default 4): a project file cannot raise it.
pub fn max_workers(settings: &oxa_config::Settings) -> usize {
    usize::try_from(settings.user_u32(SETTING_MAX, 4)).unwrap_or(4)
}

/// Whether the worker hook set is built: not in a worker child (`worker_depth` is the value of
/// `OXA_WORKER_DEPTH`, `None` when unset) and the setting is not false.
pub fn built(worker_depth: Option<&str>, enabled: bool) -> bool {
    worker_depth.is_none() && enabled
}

/// What [`WorkerHooks::new`] needs.
pub struct WorkerConfig {
    /// The parent workspace used to resolve trusted host tools for resume.
    pub cwd: PathBuf,
    /// The binary a worker runs: this executable in production.
    pub oxa_bin: PathBuf,
    /// Only the trusted live Lead may use the user-configured repository fallback.
    pub lead_dirs: Option<oxa_config::Dirs>,
    pub agent_dir: PathBuf,
    pub max: usize,
    /// The process supervisor (`proc::start`: the trusted `kill` helper and
    /// `oxa.workers.stopGraceSeconds` are already resolved).
    pub supervisor: Supervisor,
    /// The directory the child writes its sessions to (and so its `<session>.status.json`).
    pub sessions_dir: PathBuf,
    /// The process environment of this session; the child environment is made from it.
    pub parent_env: Vec<(String, String)>,
    /// The parent's token allowance, tool exclusions and resolved session directory.
    pub parent_scope: ParentScope,
    /// The process's autonomy switch (the parent's level sets the worker's ceiling).
    pub yolo: YoloSwitch,
    /// `oxa.workers.resumeOnStart`: relaunch dead, unfinished workers when the session opens.
    pub resume_on_start: bool,
    /// Whether the surface permits a separate `wait` tool.
    pub advertise_wait: bool,
    /// `oxa.workers.sendAckSeconds`: how long `send` waits for the worker to take the message; 0 = do not wait.
    pub send_ack_seconds: u64,
    /// `oxa.workers.models`: the owner's model choice policy (empty: the session model).
    pub models: ModelPolicy,
    /// #1541: the shared build cache of this repository, for the Workers view (`None`: off).
    pub cache: Option<crate::worker_cache::CacheView>,
}

/// What the parent session allows a worker to have.
#[derive(Debug, Clone, Default)]
pub struct ParentScope {
    pub max_tokens: Option<u64>,
    pub exclude_tools: Vec<String>,
    /// Set when the parent's session directory is not the child's own default.
    pub session_dir: Option<PathBuf>,
    /// The parent's other narrowing switches.
    pub narrowing: super::launch::Narrowing,
}

/// State shared by the tool and the hook set.
pub struct WorkerCore {
    cwd: PathBuf,
    lead_dirs: Mutex<Option<oxa_config::Dirs>>,
    /// Private copy opened from the session executable before model-controlled work starts.
    oxa_bin: PathBuf,
    pin_error: Option<String>,
    /// The identity of the private copy; checked before each use as a defence in depth.
    bin_id: Option<BinId>,
    agent_dir: PathBuf,
    max: AtomicUsize,
    models: Mutex<ModelPolicy>,
    cache: Option<crate::worker_cache::CacheView>,
    sessions_dir: PathBuf,
    parent_env: Vec<(String, String)>,
    parent_scope: ParentScope,
    yolo: Mutex<YoloSwitch>,
    resume_on_start: bool,
    send_ack_seconds: u64,
    /// The owner Lead session: resume waits for [`WorkerHooks::resume_after_owner_check`].
    hold_resume: AtomicBool,
    /// Explicit owner opt-out: even the post-binding-check resume stays disabled.
    resume_allowed: AtomicBool,
    /// The owner Lead session over several chat turns: closing one session stops nothing.
    persistent: AtomicBool,
    /// Parent session ids whose workers were already considered for resume in this process.
    resumed: Mutex<Vec<String>>,
    supervisor: Supervisor,
    /// Serializes start and stop: the registry is read, changed and written back.
    gate: Mutex<()>,
    /// (session id, model) of the parent session, as the host last reported them.
    session: Mutex<(String, String)>,
    host: Mutex<Option<Arc<dyn Host>>>,
    owner_scope: Option<OwnerScope>,
    /// The parent's table mode as the host last reported it (`None` until known).
    table_mode: Mutex<Option<Mode>>,
}

impl Drop for WorkerCore {
    fn drop(&mut self) {
        // This launcher is one-shot; don't leave a binary copy behind at every run.
        if self.owner_scope.is_some() && self.pin_error.is_none() && self.supervisor.stop_all().is_ok() {
            let _ = fs::remove_file(&self.oxa_bin);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The worktree of a worker `name` for the repository at `repo`: the place
/// `launch::worktree_argv` creates it.
pub fn worktree_path(repo: &Path, name: &str) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    let repo_name = repo.file_name()?;
    Some(repo.parent()?.join(".oxa-worktrees").join(repo_name).join(name))
}

pub(super) fn parse_mode(text: &str) -> Option<WorkerMode> {
    match text {
        "plan" => Some(WorkerMode::Plan),
        "ask" => Some(WorkerMode::Ask),
        "workspace" => Some(WorkerMode::Workspace),
        "auto" => Some(WorkerMode::Auto),
        "yolo" => Some(WorkerMode::Yolo),
        _ => None,
    }
}

/// The host tools a worker launch runs outside the sandbox: a `PATH` free of the project (and of
/// worker worktrees), the project root and the home directory they were resolved against.
pub(super) struct HostTools {
    pub(super) path: std::ffi::OsString,
    pub(super) project: PathBuf,
    pub(super) host_project: PathBuf,
    pub(super) home: PathBuf,
}

impl HostTools {
    /// Resolved from the session `cwd` exactly like `oxa work`: the project is the Git root of
    /// the session directory and the `PATH` is trusted against the outermost one, so no
    /// repository-supplied shim is ever found.
    pub(super) fn resolve(parent_env: &[(String, String)], cwd: &Path) -> Result<HostTools, String> {
        let value = |name: &str| {
            parent_env
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let cwd_c = cwd
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", cwd.display()))?;
        let (project, host_project) =
            crate::work_cmd::git_boundaries(&cwd_c).ok_or("this directory is not in a git repository")?;
        let home = value("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or("HOME is not set")?;
        let path = value("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let path = crate::netguard::trusted_work_path(&path, host_project, &home)
            .ok_or("cannot resolve a trusted PATH")?;
        Ok(HostTools {
            path,
            project: project.to_path_buf(),
            host_project: host_project.to_path_buf(),
            home,
        })
    }

    pub(super) fn owner_scope(&self, sessions_dir: PathBuf, offline: bool) -> Result<OwnerScope, String> {
        let common = self.git_text(
            &self.project,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let meta = fs::metadata(common).map_err(|e| format!("cannot inspect repository metadata: {e}"))?;
        Ok(OwnerScope {
            repository: self.project.clone(),
            git_device: meta.dev(),
            git_inode: meta.ino(),
            sessions_dir,
            offline,
        })
    }

    pub(super) fn matches_worktree(&self, rec: &WorkerRecord, scope: &OwnerScope) -> bool {
        let tree = &rec.worktree;
        if worktree_path(&self.project, &rec.name).as_ref() != Some(tree)
            || rec.branch != format!("agent/{}", rec.name)
            || !fs::symlink_metadata(tree.join(".git")).is_ok_and(|m| m.is_file())
            || !tree.canonicalize().is_ok_and(|path| path == *tree)
        {
            return false;
        }
        self.git_text(tree, &["rev-parse", "--show-toplevel"])
            .is_ok_and(|root| Path::new(&root) == tree)
            && self
                .git_text(tree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .ok()
                .and_then(|path| fs::metadata(path).ok())
                .is_some_and(|m| m.dev() == scope.git_device && m.ino() == scope.git_inode)
            && self
                .git_text(tree, &["symbolic-ref", "--quiet", "HEAD"])
                .is_ok_and(|branch| branch == format!("refs/heads/{}", rec.branch))
    }

    fn git_text(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        let git = self
            .program("git")
            .ok_or("git was not found in a trusted PATH directory")?;
        let out = host_git(self, &git, cwd, args)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("cannot verify worker repository metadata".into());
        }
        String::from_utf8(out.stdout)
            .map(|text| text.trim_end_matches('\n').to_string())
            .map_err(|_| "worker repository metadata is not UTF-8".into())
    }

    fn program(&self, name: &str) -> Option<PathBuf> {
        crate::netguard::trusted_executable_for(
            name,
            Some(self.path.as_os_str()),
            Some(&self.project),
            Some(&self.home),
        )
    }
}

/// `git <args>` for the host: the trusted binary through `oxa_work::metadata_git` (no replace
/// objects, no filesystem monitor, `GIT_*` isolation, no protocols), run in `dir`.
fn host_git(tools: &HostTools, git: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut command = oxa_work::metadata_git(git, &tools.path, dir, args);
    command.stdin(Stdio::null());
    command
}

/// Confirm that Git will use the checkout and metadata covered by the permission check.
fn is_primary_checkout(tools: &HostTools, git: &Path, repo: &Path) -> bool {
    let metadata = repo.join(".git");
    if !fs::symlink_metadata(&metadata).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let matches = |arg, expected: &Path| {
        host_git(tools, git, repo, &["rev-parse", "--path-format=absolute", arg])
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && std::str::from_utf8(&out.stdout).is_ok_and(|path| {
                        Path::new(path.trim_end_matches('\n'))
                            .canonicalize()
                            .ok()
                            .as_deref()
                            == Some(expected)
                    })
            })
    };
    matches("--show-toplevel", repo) && matches("--git-common-dir", &metadata)
}

/// The identity of the executable a worker runs. Any rename over it or write to it changes the
/// inode or the change time, so a binary replaced after this session started (the sandbox leaves
/// a workspace-built `oxa` writable) is never executed outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl BinId {
    fn of(path: &Path) -> io::Result<BinId> {
        let m = fs::metadata(path)?;
        Ok(BinId {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

/// Copy the running executable into owner-private storage before any model-controlled command can
/// replace a workspace-built binary. Worker launches use only this copy, never reopen the original
/// pathname. Comparing the source identity around the copy rejects a concurrent replacement.
fn pin_executable(source: &Path, agent_dir: &Path) -> Result<PathBuf, String> {
    let before = BinId::of(source).map_err(|e| format!("cannot pin the oxa executable: {e}"))?;
    let mut input = fs::File::open(source).map_err(|e| format!("cannot pin the oxa executable: {e}"))?;
    let dir = agent_dir.join("workers").join("launchers");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create the worker launcher directory: {e}"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot protect the worker launcher directory: {e}"))?;
    let path = dir.join(format!("oxa-{}-{}", std::process::id(), now_ms()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .open(&path)
        .map_err(|e| format!("cannot create the pinned worker executable: {e}"))?;
    let copied =
        io::copy(&mut input, &mut output).map_err(|e| format!("cannot copy the worker executable: {e}"))?;
    output
        .sync_all()
        .map_err(|e| format!("cannot sync the pinned worker executable: {e}"))?;
    let after = BinId::of(source).map_err(|e| format!("cannot verify the oxa executable: {e}"))?;
    if before != after || copied != before.len {
        let _ = fs::remove_file(&path);
        return Err("the oxa executable changed while it was being pinned; restart oxa".into());
    }
    Ok(path)
}

/// Writes `text` to `path`, creating `dir` first: a fresh mode-0600 file beside it, renamed over
/// the name. An existing goal file is replaced, never followed (a symlink there stays a symlink
/// and its target is untouched), and broader permissions cannot survive the replace.
fn write_private(dir: &Path, path: &Path, text: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the goal file has no name"))?;
    let temp = dir.join(format!(".{}.{}.tmp", leaf.to_string_lossy(), std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let written = file.write_all(text.as_bytes()).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// The argv prefix that runs the child in `cwd`. The environment is not set here: the supervisor
/// clears it and applies exactly [`worker_env`], so the policy lives in one place.
fn cwd_wrapper(program: &Path, cwd: &Path) -> Vec<String> {
    vec![
        program.to_string_lossy().into_owned(),
        "-C".to_string(),
        cwd.to_string_lossy().into_owned(),
    ]
}

/// The exact environment of a worker: `launch::child_env` (no trust or session hint,
/// `OXA_WORKER_DEPTH=1`, the parent's offline policy) with `PATH` set to the trusted one.
fn worker_env(parent: &[(String, String)], offline: bool, path: &std::ffi::OsStr) -> Vec<(String, String)> {
    let mut env = child_env(parent, offline);
    env.retain(|(k, _)| k != "PATH");
    env.push(("PATH".into(), path.to_string_lossy().into_owned()));
    env
}

/// A single safe path component for a parent session id (`workers/<slug>/`): session ids are
/// alphanumeric with dashes, and anything else becomes a dash so no path escapes the directory.
fn artifact_slug(parent: &str) -> String {
    let slug: String = parent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    }
}

fn child_session_id(parent: &str, name: &str, started: u64) -> String {
    let mut hasher = DefaultHasher::new();
    parent.hash(&mut hasher);
    format!("worker-{:016x}-{name}-{started}", hasher.finish())
}

/// The status file of the worker session `session_id` that ran in `worktree`: the sibling
/// `<session>.status.json` of its session file (`<stamp>_<id>.jsonl`, written by the status writer
/// as `session_file.with_extension("status.json")`), in the session directory of the worktree.
/// The time stamp is the child's, so the file is found by its suffix; `None` until it exists.
fn status_path(sessions_dir: &Path, worktree: &Path, session_id: &str) -> Option<PathBuf> {
    let dir = sessions_dir.join(oxa_session::session_dir_name(worktree));
    let suffix = format!("_{session_id}.status.json");
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .map(|e| e.path())
        .collect();
    found.sort();
    found.pop()
}

pub(super) fn snapshot(rec: &WorkerRecord, sessions_dir: &Path) -> Option<Value> {
    let path =
        status_path(sessions_dir, &rec.worktree, &rec.session_id).unwrap_or_else(|| rec.status_file.clone());
    crate::status::read_snapshot(&path)
        .ok()
        .filter(|status| registry::names_worker(rec, status))
}

/// The state of a worker from its status file and its process group. The status file is found
/// under `sessions_dir` (see [`status_path`]); `rec.status_file` is the fallback.
fn live_state(rec: &WorkerRecord, sessions_dir: &Path) -> WorkerState {
    let path =
        status_path(sessions_dir, &rec.worktree, &rec.session_id).unwrap_or_else(|| rec.status_file.clone());
    let status: Option<Value> = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let alive = PathBuf::from(format!("/proc/{}", rec.pid)).exists();
    registry::state(rec, status.as_ref(), alive)
}

/// One worker of a session as the Lead pages and loop see it: the lane (its branch), its
/// model and live state, read from the registry and the worker's own status file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub branch: String,
    pub model: String,
    pub state: WorkerState,
    pub started_ms: u64,
    /// Seconds since the worker's status file changed; `None` when it has none.
    pub status_age_secs: Option<u64>,
}

/// Why no binding list could be read: an explicit state, never an empty list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingsError {
    /// No worker is recorded for this session.
    Absent,
    /// The registry is unreadable or corrupt; it is left untouched.
    Refused,
}

/// The workers recorded for `parent_session_id` with their live state. It needs no
/// `WorkerCore`: the registry and each worker's status file are the whole input.
pub fn bindings(
    agent_dir: &Path,
    sessions_dir: &Path,
    parent_session_id: &str,
) -> Result<Vec<Binding>, BindingsError> {
    let records = registry::load(agent_dir, parent_session_id).map_err(|_| BindingsError::Refused)?;
    if records.is_empty() {
        return Err(BindingsError::Absent);
    }
    let now = std::time::SystemTime::now();
    Ok(records
        .iter()
        .map(|rec| {
            let path = status_path(sessions_dir, &rec.worktree, &rec.session_id)
                .unwrap_or_else(|| rec.status_file.clone());
            Binding {
                name: rec.name.clone(),
                branch: rec.branch.clone(),
                model: rec.model.clone(),
                state: live_state(rec, sessions_dir),
                started_ms: rec.started_ms,
                status_age_secs: fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|at| now.duration_since(at).ok())
                    .map(|age| age.as_secs()),
            }
        })
        .collect())
}

pub(super) fn is_live(rec: &WorkerRecord, sessions_dir: &Path) -> bool {
    let sessions_dir = rec
        .owner_scope
        .as_ref()
        .map_or(sessions_dir, |s| s.sessions_dir.as_path());
    matches!(
        live_state(rec, sessions_dir),
        WorkerState::Starting | WorkerState::Working | WorkerState::Idle
    )
}

fn label(state: &WorkerState) -> String {
    match state {
        WorkerState::Starting => "starting".to_string(),
        WorkerState::Working => "working".to_string(),
        WorkerState::Idle => "idle".to_string(),
        WorkerState::Finished(outcome) => format!("finished ({outcome})"),
        WorkerState::Dead => "dead".to_string(),
        WorkerState::Stopped => "stopped".to_string(),
    }
}

fn describe(rec: &WorkerRecord, sessions_dir: &Path) -> String {
    let mut line = format!(
        "{} session {} branch {} model {}",
        rec.name, rec.session_id, rec.branch, rec.model
    );
    if !rec.kind.is_empty() {
        line.push_str(&format!(" kind {}", rec.kind));
    }
    if !rec.job.is_empty() {
        line.push_str(&format!(" job {}", rec.job));
    }
    if !rec.choice.is_empty() {
        line.push_str(&format!(" choice ({})", rec.choice));
    }
    line.push(' ');
    line.push_str(&label(&live_state(rec, sessions_dir)));
    if !rec.result.is_empty() {
        line.push_str(&format!(" result {}: {}", rec.result, rec.result_reason));
    }
    line
}

/// The session's earlier workers as the model choice sees them, for the job `job`.
fn attempts(records: &[WorkerRecord], job: &str, sessions_dir: &Path) -> Vec<Attempt> {
    records
        .iter()
        .map(|r| Attempt {
            model: r.model.clone(),
            kind: r.kind.clone(),
            job_matches: r.job_key() == job,
            failed: model_policy::failed(&r.result, &live_state(r, sessions_dir), r.stopped),
        })
        .collect()
}

impl WorkerCore {
    fn new(cfg: WorkerConfig) -> WorkerCore {
        let (oxa_bin, pin_error) = match pin_executable(&cfg.oxa_bin, &cfg.agent_dir) {
            Ok(path) => (path, None),
            Err(error) => (cfg.oxa_bin, Some(error)),
        };
        WorkerCore {
            bin_id: BinId::of(&oxa_bin).ok(),
            oxa_bin,
            pin_error,
            cwd: cfg.cwd,
            lead_dirs: Mutex::new(cfg.lead_dirs),
            agent_dir: cfg.agent_dir,
            max: AtomicUsize::new(cfg.max),
            models: Mutex::new(cfg.models),
            cache: cfg.cache,
            sessions_dir: cfg.sessions_dir,
            parent_env: cfg.parent_env,
            parent_scope: cfg.parent_scope,
            yolo: Mutex::new(cfg.yolo),
            resume_on_start: cfg.resume_on_start,
            send_ack_seconds: cfg.send_ack_seconds,
            hold_resume: AtomicBool::new(false),
            resume_allowed: AtomicBool::new(true),
            persistent: AtomicBool::new(false),
            resumed: Mutex::new(Vec::new()),
            supervisor: cfg.supervisor,
            gate: Mutex::new(()),
            session: Mutex::new((String::new(), String::new())),
            host: Mutex::new(None),
            owner_scope: None,
            table_mode: Mutex::new(None),
        }
    }

    pub(super) fn for_owner(cfg: WorkerConfig, scope: OwnerScope, default_model: String) -> Self {
        let mut core = Self::new(cfg);
        core.session = Mutex::new((registry::OWNER_PARENT.into(), default_model));
        core.owner_scope = Some(scope);
        core
    }

    pub(super) fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    fn session(&self) -> (String, String) {
        lock(&self.session).clone()
    }

    /// Learn the parent session id and model through the host (`HostAction::EngineLimits`).
    fn learn(&self) {
        let Some(host) = lock(&self.host).clone() else {
            return;
        };
        if let Ok(v) = host.call(HostAction::EngineLimits) {
            let id = v.get("sessionId").and_then(Value::as_str).unwrap_or("");
            if let Some(mode) = v.get("mode").and_then(Value::as_str).and_then(Mode::parse) {
                *lock(&self.table_mode) = Some(mode);
            }
            if !id.is_empty() {
                let model = v.get("model").and_then(Value::as_str).unwrap_or("");
                *lock(&self.session) = (id.to_string(), model.to_string());
            }
        }
    }

    /// Refuses to run a worker binary that is not the one this session started from.
    fn check_bin(&self) -> Result<(), String> {
        if let Some(error) = &self.pin_error {
            return Err(error.clone());
        }
        match (&self.bin_id, BinId::of(&self.oxa_bin)) {
            (Some(then), Ok(now)) if *then == now => Ok(()),
            _ => Err(format!(
                "the oxa executable {} changed since this session started (or cannot be read); restart oxa before starting workers",
                self.oxa_bin.display()
            )),
        }
    }

    /// Checks the empty worktree out inside the sandbox (`oxa work --supervise`, the same
    /// supervisor `oxa work` uses): hooks and credential helpers are off, there is no network,
    /// and the filters of the repository run in bwrap, never on the host.
    fn materialize(
        &self,
        tools: &HostTools,
        git: &Path,
        worktree: &Path,
        branch: &str,
        autonomy: Autonomy,
    ) -> Result<(), String> {
        self.check_bin()?;
        // Yolo deliberately has no OS sandbox. Non-Linux hosts have no bwrap and use the
        // repository's supported unsandboxed path too; the in-process Git guards remain.
        if autonomy == Autonomy::Yolo || !cfg!(target_os = "linux") {
            let out = host_git(tools, git, worktree, &["checkout", "HEAD", "--", "."])
                .output()
                .map_err(|e| format!("cannot run the worktree checkout: {e}"))?;
            return if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "the worktree checkout failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            };
        }
        let request = oxa_work::SupervisorRequest {
            source: tools.project.clone(),
            operation: oxa_work::SupervisorOperation::Checkout {
                branch: branch.to_string(),
                create: false,
                adopted: true,
                normalize: None,
            },
        };
        let json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.oxa_bin);
        command.env_clear();
        for (k, v) in &self.parent_env {
            if k != "PATH" && !k.starts_with("GIT_") {
                command.env(k, v);
            }
        }
        let out = command
            .env("PATH", &tools.path)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .current_dir(worktree)
            .args(["work", "--supervise", json.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run the sandboxed checkout: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "the sandboxed worktree checkout failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// `start`: see the module docs for the order of the checks.
    pub fn start(&self, args: &Value, cwd: &Path, mode: Mode, autonomy: Autonomy) -> ToolOutput {
        self.start_at(args, cwd, mode, autonomy, None)
    }

    fn start_at(
        &self,
        args: &Value,
        cwd: &Path,
        mode: Mode,
        autonomy: Autonomy,
        lead: Option<&Path>,
    ) -> ToolOutput {
        let _gate = lock(&self.gate);
        let name = str_arg(args, "name").unwrap_or("");
        if !valid_name(name) {
            return ToolOutput::error(format!(
                "worker: invalid name {name:?}; use lowercase letters, digits and dashes, at most 40 characters"
            ));
        }
        let goal = str_arg(args, "goal").unwrap_or("");
        if goal.trim().is_empty() {
            return ToolOutput::error("worker: start needs a goal");
        }
        let wanted = match str_arg(args, "mode") {
            None => None,
            Some(text) => match parse_mode(text) {
                Some(m) => Some(m),
                None => return ToolOutput::error(format!("worker: unknown mode {text:?}")),
            },
        };
        let (parent, session_model) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let explicit = match args.get("model") {
            Some(value) => match value.as_str() {
                Some(model) if !model.trim().is_empty() => Some(model.to_owned()),
                _ => return ToolOutput::error("worker: model must be a non-empty string"),
            },
            None => None,
        };
        if let Some(model) = &explicit {
            if let Err(error) = check_value("model", model) {
                return ToolOutput::error(format!("worker: {error}"));
            }
        }
        let kind = match args.get("kind") {
            None => None,
            Some(v) => match v.as_str().filter(|k| model_policy::valid_kind(k)) {
                Some(k) => Some(k.to_string()),
                None => return ToolOutput::error(
                    "worker: invalid kind; use lowercase letters, digits and dashes, at most 32 characters",
                ),
            },
        };
        let job = match args.get("job") {
            None => name.to_string(),
            Some(v) => match v.as_str().filter(|j| model_policy::valid_job(j)) {
                Some(j) => j.to_string(),
                None => return ToolOutput::error(
                    "worker: invalid job; use lowercase letters, digits and dashes, at most 40 characters",
                ),
            },
        };
        let reason = match args.get("reason") {
            None => String::new(),
            Some(v) => match v.as_str().map(model_policy::check_reason) {
                Some(Ok(text)) => text,
                Some(Err(e)) => return ToolOutput::error(format!("worker: {e}")),
                None => return ToolOutput::error("worker: reason must be a string"),
            },
        };
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let history = attempts(&records, &job, &self.sessions_dir);
        let policy = lock(&self.models).clone();
        let choice = match model_policy::choose(
            &policy,
            explicit.as_deref(),
            kind.as_deref(),
            &job,
            &session_model,
            &history,
        ) {
            Ok(choice) => choice,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let model = choice.model.clone();
        if let Err(error) = check_value("model", &model) {
            return ToolOutput::error(format!("worker: {error}"));
        }
        // Warn and record, never block: the live catalog is only asked, and an answer that
        // cannot be had leaves the model unverified.
        let host = lock(&self.host).clone();
        let verified = host
            .and_then(|h| h.call(HostAction::ModelInfo(model.clone())).ok())
            .and_then(|v| v.get("known").and_then(Value::as_bool));
        if records.iter().any(|r| r.name == name) {
            return ToolOutput::error(format!(
                "worker: a worker named {name} already exists in this session; stop it or choose another name"
            ));
        }
        let live = records.iter().filter(|r| is_live(r, &self.sessions_dir)).count();
        let max = self.max.load(Ordering::SeqCst);
        if live >= max {
            return ToolOutput::error(format!(
                "worker: {live} live workers already; the limit is {} (oxa.workers.max)",
                max
            ));
        }
        let mut tools = match HostTools::resolve(&self.parent_env, cwd) {
            Ok(tools) => tools,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        if let Some(lead) = lead {
            let Some(path) = crate::netguard::trusted_work_path(&tools.path, lead, &tools.home) else {
                return ToolOutput::error("worker: cannot resolve a trusted Lead PATH");
            };
            tools.path = path;
            if !self
                .supervisor
                .kill_is_trusted_in(&tools.host_project, &tools.home)
                || !self.supervisor.kill_is_trusted_in(lead, &tools.home)
            {
                return ToolOutput::error("worker: the process supervisor's kill helper is inside the repository or is no longer trusted");
            }
        }
        let Some(git) = tools.program("git") else {
            return ToolOutput::error("worker: git was not found in a trusted PATH directory");
        };
        if lead.is_some() && !is_primary_checkout(&tools, &git, cwd) {
            return ToolOutput::error(
                "worker: oxa.workers.repository must name a primary Git checkout with local Git metadata",
            );
        }
        // `env` runs outside the sandbox: resolved from the session's project like every other
        // host tool, never from a project-controlled PATH entry.
        let Some(env_bin) = tools.program("env").filter(|bin| {
            lead.is_none_or(|lead| {
                bin.canonicalize()
                    .is_ok_and(|real| !real.starts_with(lead) && !real.starts_with(&tools.host_project))
            })
        }) else {
            return ToolOutput::error("worker: env was not found in a trusted PATH directory");
        };
        let branch = format!("agent/{name}");
        let refname = format!("refs/heads/{branch}");
        if host_git(
            &tools,
            &git,
            cwd,
            &["rev-parse", "--verify", "--quiet", refname.as_str()],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        {
            return ToolOutput::error(format!(
                "worker: branch {branch} already exists; another owner has this name"
            ));
        }
        let Some(worktree) = worktree_path(cwd, name) else {
            return ToolOutput::error(
                "worker: this directory has no worktree location; start from a git repository",
            );
        };
        if worktree.exists() {
            return ToolOutput::error(format!("worker: {} already exists", worktree.display()));
        }
        let steps = worktree_argv(cwd, name, "HEAD");
        if steps.is_empty() {
            return ToolOutput::error(
                "worker: cannot plan the worktree; the directory must be a git repository with a UTF-8 path",
            );
        }
        if let Err(e) = self.check_bin() {
            return ToolOutput::error(format!("worker: {e}"));
        }
        for step in &steps {
            // Step 0 is the program name (`git`); the trusted binary runs in its place.
            let Some((_, rest)) = step.split_first() else {
                continue;
            };
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            match host_git(&tools, &git, cwd, &rest).output() {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    return ToolOutput::error(format!(
                        "worker: git worktree add failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
                Err(e) => return ToolOutput::error(format!("worker: cannot run git: {e}")),
            }
        }
        // The worktree exists with no files: materialize them inside the sandbox, where a
        // repository's smudge filters and attributes cannot reach the host. A failed checkout
        // removes the empty worktree and branch so the name can be tried again.
        if let Err(e) = self.materialize(&tools, &git, &worktree, &branch, autonomy) {
            let path = worktree.to_string_lossy().into_owned();
            let _ = host_git(
                &tools,
                &git,
                cwd,
                &["worktree", "remove", "--force", path.as_str()],
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
            let _ = host_git(&tools, &git, cwd, &["branch", "-D", branch.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return ToolOutput::error(format!("worker: {e}"));
        }

        // Artifacts are per parent session: two sessions of one agent directory may start a
        // worker with the same name and must not share a goal file or a log.
        let slug = if self.owner_scope.is_some() {
            parent.clone()
        } else {
            artifact_slug(&parent)
        };
        let dir = self.agent_dir.join("workers").join(slug);
        let goal_file = dir.join(format!("{name}.goal.md"));
        if let Err(e) = write_private(&dir, &goal_file, goal) {
            return ToolOutput::error(format!("worker: cannot write the goal file: {e}"));
        }

        let started = now_ms();
        // Session-store identifiers are limited to 128 bytes. The registry already records the
        // full parent relationship, so a bounded digest-shaped component is sufficient here.
        let session_id = child_session_id(&parent, name, started);
        let spec = WorkerSpec {
            name: name.to_string(),
            session_id: session_id.clone(),
            model: model.clone(),
            goal_file: goal_file.to_string_lossy().into_owned(),
            mode: wanted,
            max_tokens: self.parent_scope.max_tokens,
            exclude_tools: self.parent_scope.exclude_tools.clone(),
            session_dir: self.parent_scope.session_dir.clone(),
            narrowing: self.parent_scope.narrowing.clone(),
        };
        let (argv, notice) = match child_argv(&spec, mode, autonomy) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let env = worker_env(&self.parent_env, oxa_proto::offline(), &tools.path);
        if let Err(e) = self.check_bin() {
            return ToolOutput::error(format!("worker: {e}"));
        }
        let mut command = cwd_wrapper(&env_bin, &worktree);
        command.push(self.oxa_bin.to_string_lossy().into_owned());
        command.extend(argv.iter().cloned());
        let refs: Vec<&str> = command.iter().map(String::as_str).collect();
        let log = dir.join(format!("{name}.log"));
        let pgid = match self.supervisor.spawn(&refs, &env, &log) {
            Ok(pgid) => pgid,
            Err(e) => return ToolOutput::error(format!("worker: cannot start the process: {e}")),
        };
        let effective = argv
            .iter()
            .position(|a| a == "--permission-mode")
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "workspace".to_string());

        let kind_text = kind.as_deref().map(|k| format!(", kind {k}")).unwrap_or_default();
        let mut text = format!(
            "started worker {name}: model {model} ({}){kind_text}, job {job}, session {session_id}, branch {branch}, worktree {}",
            choice.why,
            worktree.display()
        );
        if verified == Some(false) {
            text.push_str(&format!(
                "\nwarning: {model} is not in this install's model catalog (oxa models); the worker may fail to start"
            ));
        }
        records.push(WorkerRecord {
            name: name.to_string(),
            session_id: session_id.clone(),
            pid: pgid,
            pgid,
            worktree: worktree.clone(),
            branch: branch.clone(),
            model,
            mode: effective,
            goal_file,
            // The child's real file carries a time stamp (see `status_path`); `live_state`
            // looks for it by session id and falls back to this name.
            status_file: self
                .sessions_dir
                .join(oxa_session::session_dir_name(&worktree))
                .join(format!("{session_id}.status.json")),
            log_file: log,
            started_ms: started,
            stopped: false,
            kind: kind.unwrap_or_default(),
            job,
            choice: choice.why,
            reason,
            verified,
            result: String::new(),
            result_reason: String::new(),
            owner_scope: self.owner_scope.clone(),
        });
        if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
            // Not recorded, so nothing would ever stop it: stop it now.
            let _ = self.supervisor.stop(pgid);
            return ToolOutput::error(format!("worker: {e}"));
        }
        if let Some(notice) = notice {
            text.push('\n');
            text.push_str(&notice);
        }
        // Owner policy: budgets warn and record, they never block. Each worker gets the parent's
        // whole allowance, so say so where the model and the owner can see it.
        if let Some(tokens) = self.parent_scope.max_tokens {
            text.push_str(&format!(
                "\nbudget: this worker may use up to {tokens} tokens (--max-tokens, the same allowance as this session; the session and its workers together may use more)"
            ));
        }
        text.push_str(&format!(
            "\nreceipt {session_id}: delivered (process group {pgid} started)"
        ));
        ToolOutput::ok(text)
    }

    /// `list`: one line per worker of this session.
    pub fn list(&self) -> ToolOutput {
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        if records.is_empty() {
            return ToolOutput::ok("no workers in this session");
        }
        let mut lines: Vec<String> = records.iter().map(|r| describe(r, &self.sessions_dir)).collect();
        lines.extend(self.cache.as_ref().and_then(crate::worker_cache::describe));
        ToolOutput::ok(lines.join("\n"))
    }

    /// `result {name, result, reason}`: record how a worker's work turned out. The record is the
    /// caller's word (review or CI rejected a worker that itself ended done); a later call replaces
    /// an earlier one. Registry write only: nothing is launched and no network is used.
    pub fn result(&self, args: &Value) -> ToolOutput {
        let _gate = lock(&self.gate);
        let name = str_arg(args, "name").unwrap_or("");
        let result = match str_arg(args, "result") {
            Some(r @ ("ok" | "failed")) => r,
            _ => return ToolOutput::error("worker: result must be ok or failed"),
        };
        let reason = match model_policy::check_reason(str_arg(args, "reason").unwrap_or("")) {
            Ok(reason) => reason,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(rec) = records.iter_mut().find(|r| r.name == name) else {
            return ToolOutput::error(format!("worker: no worker named {name:?}"));
        };
        rec.result = result.to_string();
        rec.result_reason = reason;
        let model = rec.model.clone();
        if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
            return ToolOutput::error(format!("worker: {e}"));
        }
        ToolOutput::ok(format!(
            "recorded result {result} for worker {name} (model {model})"
        ))
    }

    /// `stop {name}`: stop the worker's process group and mark it stopped.
    pub fn stop(&self, args: &Value) -> ToolOutput {
        let _gate = lock(&self.gate);
        let name = str_arg(args, "name").unwrap_or("");
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(index) = records.iter().position(|r| r.name == name) else {
            return ToolOutput::error(format!("worker: no worker named {name:?}"));
        };
        if records[index].stopped {
            return ToolOutput::ok(format!("worker {name} is already stopped"));
        }
        match self.supervisor.stop(records[index].pgid) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return ToolOutput::error(format!(
                    "worker: cannot stop {name}: this process does not own that worker\nreceipt {}: rejected",
                    records[index].session_id
                ))
            }
            Err(e) => {
                return ToolOutput::error(format!(
                    "worker: cannot stop {name}: {e}\nreceipt {}: not_delivered",
                    records[index].session_id
                ))
            }
        }
        records[index].stopped = true;
        if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
            return ToolOutput::error(format!("worker: {e}"));
        }
        ToolOutput::ok(format!(
            "stopped worker {name}\nreceipt {}: delivered (process group confirmed stopped)",
            records[index].session_id
        ))
    }

    /// `send {name, text}`: publish `text` to the inbox of the worker's session and return a
    /// receipt: `delivered` once the worker's inbox owner took the file, `not_delivered` when
    /// `oxa.workers.sendAckSeconds` ran out first, `queued` when the owner turned the wait off.
    /// `send {name, retry: <id>}` waits again for the same message and `{name, withdraw: <id>}`
    /// removes it, so a retry never creates a second writer. The session id is the one in the
    /// registry record, never a value from the model.
    pub fn send(&self, args: &Value) -> ToolOutput {
        let name = str_arg(args, "name").unwrap_or("");
        let text = str_arg(args, "text").unwrap_or("");
        let (retry, withdraw) = (str_arg(args, "retry"), str_arg(args, "withdraw"));
        if text.trim().is_empty() && retry.is_none() && withdraw.is_none() {
            return ToolOutput::error("worker: send needs text");
        }
        let (parent, _) = self.session();
        if parent.is_empty() {
            return ToolOutput::error("worker: the session id is not known yet");
        }
        let records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return ToolOutput::error(format!("worker: {e}")),
        };
        let Some(rec) = records.iter().find(|r| r.name == name) else {
            return ToolOutput::error(format!("worker: no worker named {name:?}"));
        };
        let phase = label(&live_state(rec, &self.sessions_dir));
        if let Some(id) = withdraw {
            return match oxa_session::withdraw_inbox(&self.agent_dir, &rec.session_id, id) {
                Ok(true) => ToolOutput::ok(format!("receipt {id}: withdrawn; worker phase: {phase}")),
                Ok(false) => ToolOutput::ok(format!(
                    "receipt {id}: delivered (already taken); worker phase: {phase}"
                )),
                Err(e) => ToolOutput::error(format!("worker: cannot withdraw from {name}: {e}")),
            };
        }
        let id = match retry {
            Some(id) => id.to_owned(),
            // #2183: the parent's message steers the worker at its next tool boundary.
            None => match oxa_session::send_inbox_steer_receipt(&self.agent_dir, &rec.session_id, text) {
                Ok(id) => id,
                Err(e) => return ToolOutput::error(format!("worker: cannot deliver to {name}: {e}")),
            },
        };
        if self.send_ack_seconds == 0 && retry.is_none() {
            return ToolOutput::ok(format!(
                "receipt {id}: queued (file accepted in the inbox of session {}); worker phase: {phase}",
                rec.session_id
            ));
        }
        let wait = std::time::Duration::from_secs(self.send_ack_seconds);
        match oxa_session::await_delivery(&self.agent_dir, &rec.session_id, &id, wait) {
            Ok(oxa_session::DeliveryOutcome::Delivered) => ToolOutput::ok(format!(
                "receipt {id}: delivered; worker phase: {phase}"
            )),
            Ok(oxa_session::DeliveryOutcome::NotDelivered) => ToolOutput::error(format!(
                "receipt {id}: not_delivered after {} s; worker phase: {phase}. The file is still queued: send {{name: {name:?}, retry: {id:?}}} to wait again or {{name: {name:?}, withdraw: {id:?}}} to remove it",
                self.send_ack_seconds
            )),
            Err(e) => ToolOutput::error(format!("worker: cannot check delivery to {name}: {e}")),
        }
    }

    /// The wait probe for `worker {name}`: `Ok(None)` while the worker runs, `Ok(Some(text))` when
    /// it finished or its process is gone. Only the typed outcome is returned: child logs can
    /// contain credentials or other private diagnostics and must not become parent-model input.
    pub fn probe(&self, args: &Value) -> Result<Option<String>, String> {
        let name = str_arg(args, "name").unwrap_or("");
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Err("worker: the session id is not known yet".to_string());
        }
        let records = registry::load(&self.agent_dir, &parent).map_err(|e| format!("worker: {e}"))?;
        let rec = records
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| format!("worker: no worker named {name:?}"))?;
        let state = live_state(rec, &self.sessions_dir);
        let outcome = match &state {
            WorkerState::Finished(outcome) => format!("finished, outcome {outcome}"),
            WorkerState::Dead => "process exited without a finished outcome".to_string(),
            WorkerState::Stopped => "stopped".to_string(),
            _ => return Ok(None),
        };
        Ok(Some(format!("worker {name}: {outcome}")))
    }

    /// Spawn the saved session in its worktree. Both resume callers share the same
    /// trusted executable, environment, supervisor and narrowing arguments.
    fn relaunch(
        &self,
        rec: &mut WorkerRecord,
        tools: &HostTools,
        table_mode: Mode,
        autonomy: Autonomy,
    ) -> Result<Option<String>, String> {
        let (argv, notice) = resume::resume_argv(rec, table_mode, autonomy, &self.parent_scope)?;
        if !rec.worktree.is_dir() {
            return Err(format!("worktree {} is gone", rec.worktree.display()));
        }
        let env_bin = tools
            .program("env")
            .ok_or("env was not found in a trusted PATH directory")?;
        self.check_bin()?;
        let env = worker_env(&self.parent_env, oxa_proto::offline(), &tools.path);
        let mut command = cwd_wrapper(&env_bin, &rec.worktree);
        command.push(self.oxa_bin.to_string_lossy().into_owned());
        command.extend(argv);
        let refs: Vec<&str> = command.iter().map(String::as_str).collect();
        let pgid = self
            .supervisor
            .spawn(&refs, &env, &rec.log_file)
            .map_err(|e| e.to_string())?;
        rec.pid = pgid;
        rec.pgid = pgid;
        Ok(notice)
    }

    /// Relaunch just the checked owner record under the caller's registry lease.
    pub(super) fn resume_owner(
        &self,
        records: &mut [WorkerRecord],
        index: usize,
        tools: &HostTools,
        mode: Mode,
        autonomy: Autonomy,
    ) -> Result<WorkerRecord, OxaError> {
        let rec = &mut records[index];
        let recorded = parse_mode(&rec.mode)
            .ok_or_else(|| OxaError::new(oxa_proto::codes::INVALID, "invalid recorded worker mode"))?;
        self.relaunch(rec, tools, mode, autonomy)
            .map_err(|e| OxaError::new(oxa_proto::codes::IO, format!("cannot resume worker: {e}")))?;
        rec.mode = super::launch::resume_mode(recorded, mode, autonomy)
            .as_arg()
            .into();
        rec.owner_scope = self.owner_scope.clone();
        let rec = rec.clone();
        if let Err(e) = registry::save(&self.agent_dir, registry::OWNER_PARENT, records) {
            let _ = self.supervisor.stop(rec.pgid);
            return Err(e);
        }
        Ok(rec)
    }

    /// Relaunch each dead, unfinished worker of the parent session (once per session id in this
    /// process): same launch path as `start` (env wrapper, worktree cwd, supervisor), argv from
    /// `resume::resume_argv`. Returns one notice per worker.
    pub fn resume(&self) -> Vec<String> {
        if !self.resume_on_start {
            return Vec::new();
        }
        let _gate = lock(&self.gate);
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Vec::new();
        }
        {
            let mut seen = lock(&self.resumed);
            if seen.contains(&parent) {
                return Vec::new();
            }
            seen.push(parent.clone());
        }
        let mut records = match registry::load(&self.agent_dir, &parent) {
            Ok(records) => records,
            Err(e) => return vec![format!("worker: cannot resume workers: {e}")],
        };
        let autonomy = lock(&self.yolo).level();
        // An unknown table mode is taken as the most restrictive one, never a wider one.
        let table_mode = (*lock(&self.table_mode)).unwrap_or(Mode::Plan);
        let mut live = records.iter().filter(|r| is_live(r, &self.sessions_dir)).count();
        let mut notices = Vec::new();
        let mut launched: Vec<(u32, String)> = Vec::new();
        for rec in records.iter_mut() {
            let alive = PathBuf::from(format!("/proc/{}", rec.pid)).exists();
            if !resume::due(rec, &live_state(rec, &self.sessions_dir), alive) {
                continue;
            }
            if live >= self.max.load(Ordering::SeqCst) {
                notices.push(format!(
                    "worker {}: not resumed, {live} live workers already (oxa.workers.max)",
                    rec.name
                ));
                continue;
            }
            let tools = match HostTools::resolve(&self.parent_env, &self.cwd) {
                Ok(tools) => tools,
                Err(e) => {
                    notices.push(format!("worker {}: not resumed: {e}", rec.name));
                    continue;
                }
            };
            match self.relaunch(rec, &tools, table_mode, autonomy) {
                Ok(notice) => {
                    let pgid = rec.pgid;
                    launched.push((
                        pgid,
                        format!(
                            "worker {}: relaunched with --continue={}",
                            rec.name, rec.session_id
                        ),
                    ));
                    live += 1;
                    notices.extend(notice);
                }
                Err(e) => notices.push(format!("worker {}: cannot relaunch: {e}", rec.name)),
            }
        }
        if !launched.is_empty() {
            if let Err(e) = registry::save(&self.agent_dir, &parent, &records) {
                // Not recorded, so nothing would ever stop them: stop them now, as `start` does.
                let mut retained = Vec::new();
                for (pgid, _) in &launched {
                    if let Err(error) = self.supervisor.stop(*pgid) {
                        retained.push(format!("{pgid} ({error})"));
                    }
                }
                if retained.is_empty() {
                    notices.push(format!(
                        "worker: relaunched workers were not recorded and were stopped: {e}"
                    ));
                } else {
                    notices.push(format!(
                        "worker: registry update failed ({e}); cleanup failed for process groups {}; they remain tracked by this session and require another stop attempt",
                        retained.join(", ")
                    ));
                }
            } else {
                notices.extend(launched.into_iter().map(|(_, notice)| notice));
            }
        }
        notices
    }

    /// The `workers` prompt section: one line per live worker of this session.
    fn prompt_lines(&self) -> Vec<(String, String)> {
        let (parent, _) = self.session();
        if parent.is_empty() {
            return Vec::new();
        }
        let records = registry::load(&self.agent_dir, &parent).unwrap_or_default();
        let lines: Vec<String> = records
            .iter()
            .filter(|r| is_live(r, &self.sessions_dir))
            .map(|r| describe(r, &self.sessions_dir))
            .collect();
        if lines.is_empty() {
            Vec::new()
        } else {
            vec![("workers".to_string(), lines.join("\n"))]
        }
    }
}

/// The `worker` tool.
struct WorkerTool {
    spec: ToolSpec,
    core: Arc<WorkerCore>,
}

fn worker_access(args: &Value, cwd: &Path, external: bool) -> Result<Vec<Access>, String> {
    match str_arg(args, "action") {
        // Listing only reads the worker registry and status through this trusted built-in. It
        // neither launches a command nor asks the network permission gate, including in plan mode.
        // Recording a result is a registry write of the same kind: no launch, no network.
        Some("list" | "result") => Ok(Vec::new()),
        Some("start") => {
            let name = str_arg(args, "name").unwrap_or("");
            let Some(path) = worktree_path(cwd, name) else {
                return Err(format!("worker: invalid name {name:?}"));
            };
            let mut access = vec![
                Access::Exec {
                    command: COMMAND.to_string(),
                    net: true,
                },
                Access::Write(path),
            ];
            if external {
                access.push(Access::Write(cwd.to_path_buf()));
            }
            Ok(access)
        }
        // Stopping owns a host process operation, but needs no network access.
        _ => Ok(vec![Access::Exec {
            command: COMMAND.to_string(),
            net: false,
        }]),
    }
}

fn approved_worktree_is_unchanged(path: &Path, approved: &Path) -> bool {
    oxa_perm::resolve_real(path).as_deref() == Some(approved)
}

impl WorkerTool {
    // No model input participates in repository selection. Read the user file again so the
    // retained Lead supervisor cannot keep starting workers after the owner removes the setting.
    fn external_repository(&self, cwd: &Path) -> Result<Option<PathBuf>, String> {
        let Some(dirs) = lock(&self.core.lead_dirs).clone() else {
            return Ok(None);
        };
        let canonical = cwd
            .canonicalize()
            .map_err(|e| format!("worker: cannot resolve Lead directory: {e}"))?;
        if crate::work_cmd::git_boundaries(&canonical).is_some() {
            return Ok(None);
        }
        let settings = oxa_config::Settings::load(&dirs, &canonical, false);
        let repo = settings
            .user_value(SETTING_REPOSITORY)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(Path::new)
            .ok_or("worker: outside a Git checkout, set oxa.workers.repository in user settings")?;
        if !repo.is_absolute() || repo.canonicalize().ok().as_deref() != Some(repo) || !repo.is_dir() {
            return Err(
                "worker: oxa.workers.repository must be a canonical absolute checkout directory".into(),
            );
        }
        let trust = oxa_perm::TrustStore::load(&dirs.trust_file());
        if trust.lookup(&canonical) == Some(false) {
            return Err("worker: the Lead directory is not trusted".into());
        }
        if !trust.trusted_as_approved(repo) {
            return Err("worker: oxa.workers.repository must be explicitly trusted".into());
        }
        Ok(Some(repo.to_path_buf()))
    }
}

impl Tool for WorkerTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// `start` runs `git worktree add` and a child session, so it asks for the command with the
    /// network on and for the worktree it writes. `stop` names a network-free command; `list` is
    /// an internal read-only query and requires no external access.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let repo = if str_arg(args, "action") == Some("start") {
            self.external_repository(cx.cwd)?
        } else {
            None
        };
        worker_access(args, repo.as_deref().unwrap_or(cx.cwd), repo.is_some())
    }

    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        match str_arg(args, "action") {
            Some("start") => {
                let name = str_arg(args, "name").unwrap_or("");
                let external = match self.external_repository(cx.cwd) {
                    Ok(repo) => repo,
                    Err(e) => return ToolOutput::error(e),
                };
                let repo = external.as_deref().unwrap_or(cx.cwd);
                let Some(path) = worktree_path(repo, name) else {
                    return ToolOutput::error(format!("worker: invalid name {name:?}"));
                };
                if external.is_some() {
                    // Both write locations must be the ones covered by the policy. Reject symlink
                    // redirects even if a broad permission happened to approve the redirect.
                    if [repo, path.as_path()]
                        .into_iter()
                        .any(|p| cx.checked_real(p) != Some(p) || !approved_worktree_is_unchanged(p, p))
                    {
                        return ToolOutput::error(
                            "worker: repository and worktree destination need unchanged permission approvals",
                        );
                    }
                    let lead = match cx.cwd.canonicalize() {
                        Ok(lead) => lead,
                        Err(e) => {
                            return ToolOutput::error(format!("worker: cannot resolve Lead directory: {e}"))
                        }
                    };
                    return self.core.start_at(
                        args,
                        repo,
                        cx.mode,
                        lock(&self.core.yolo).level(),
                        Some(&lead),
                    );
                }
                if lock(&self.core.lead_dirs).is_some() && cx.checked_real(&path).is_none() {
                    return ToolOutput::error("worker: worktree destination needs permission approval");
                }
                if let Some(approved) = cx.checked_real(&path) {
                    if !approved_worktree_is_unchanged(&path, approved) {
                        return ToolOutput::error(
                            "worker: worktree destination changed after permission approval",
                        );
                    }
                }
                self.core
                    .start(args, cx.cwd, cx.mode, lock(&self.core.yolo).level())
            }
            Some("list") => self.core.list(),
            Some("stop") => self.core.stop(args),
            Some("send") => self.core.send(args),
            Some("result") => self.core.result(args),
            _ => ToolOutput::error("worker: action must be start, list, stop, send or result"),
        }
    }

    fn default_selected(&self) -> bool {
        true
    }
}

/// The hook set: the `worker` tool, the session's learned id and model, and the shutdown.
pub struct WorkerHooks {
    core: Arc<WorkerCore>,
    tool: Arc<dyn Tool>,
}

impl WorkerHooks {
    /// The hook set.
    pub fn new(cfg: WorkerConfig) -> WorkerHooks {
        let mut description = DESCRIPTION.to_string();
        if cfg.advertise_wait {
            description.push_str(" When wait is available, use until {kind:\"worker_done\", name} instead of a plain wait to await a worker.");
        }
        let core = Arc::new(WorkerCore::new(cfg));
        let tool: Arc<dyn Tool> = Arc::new(WorkerTool {
            spec: ToolSpec {
                name: TOOL_NAME.to_string(),
                description,
                schema_json: SCHEMA.to_string(),
                class: ToolClass::Exec,
                exposure: Exposure::Direct,
                source: Source::Extension(HOOK_NAME.to_string()),
            },
            core: core.clone(),
        });
        WorkerHooks { core, tool }
    }

    /// Owner Lead session: the startup resume of saved workers waits until the caller has been
    /// verified ([`WorkerHooks::resume_after_owner_check`]); nothing is launched before.
    pub fn hold_resume(&self) {
        self.core.hold_resume.store(true, Ordering::SeqCst);
    }

    /// Disable both startup and explicit post-binding-check resume for a one-shot owner.
    pub fn suppress_resume(&self) {
        self.hold_resume();
        self.core.resume_allowed.store(false, Ordering::SeqCst);
    }

    /// Take the workers back up, after the owner binding was rechecked. Notices for the caller.
    pub fn resume_after_owner_check(&self, workers_enabled: bool) -> Vec<String> {
        if workers_enabled && self.core.resume_allowed.load(Ordering::SeqCst) {
            self.core.resume()
        } else {
            Vec::new()
        }
    }

    /// Keep the workers when a session closes: a worker the owner Lead started outlives one chat
    /// turn. They stop through [`WorkerHooks::stop_all`] (lead shutdown, Ctrl-C of the tree).
    pub fn keep_across_turns(&self) {
        self.core.persistent.store(true, Ordering::SeqCst);
    }

    /// Apply the current owner's launch ceiling to a supervisor retained from an earlier turn.
    /// Existing children keep their original mode; every new start or resume uses this ceiling.
    fn refresh_authority_from(&self, current: &WorkerHooks) {
        let max = current.core.max.load(Ordering::SeqCst);
        let yolo = lock(&current.core.yolo).clone();
        let lead_dirs = lock(&current.core.lead_dirs).clone();
        let models = lock(&current.core.models).clone();
        self.refresh_authority(max, &yolo, lead_dirs, models);
    }

    fn refresh_authority(
        &self,
        max: usize,
        yolo: &YoloSwitch,
        lead_dirs: Option<oxa_config::Dirs>,
        models: ModelPolicy,
    ) {
        self.core.max.store(max, Ordering::SeqCst);
        *lock(&self.core.models) = models;
        *lock(&self.core.yolo) = yolo.clone();
        *lock(&self.core.lead_dirs) = lead_dirs;
    }

    /// Stop every live worker now, whether or not the hook set is kept across turns.
    pub fn stop_all(&self) {
        let _ = self.core.supervisor.stop_all();
    }

    #[cfg(test)]
    pub(crate) fn parent_narrowing(&self) -> super::launch::Narrowing {
        self.core.parent_scope.narrowing.clone()
    }
}

/// The long-lived worker hook sets of the lead serve process, one per agent directory.
static OWNER_WORKERS: Mutex<Vec<(PathBuf, Arc<WorkerHooks>)>> = Mutex::new(Vec::new());

/// Return an owner hook set already retained by this process, updating the authority used for
/// future launches. Looking it up before building hooks avoids pinning a throwaway executable on
/// every HTTP owner message.
pub fn retained_owner_workers(
    key: &Path,
    max: usize,
    yolo: &YoloSwitch,
    lead_dirs: Option<oxa_config::Dirs>,
    models: ModelPolicy,
) -> Option<Arc<WorkerHooks>> {
    let all = lock(&OWNER_WORKERS);
    let hooks = all.iter().find(|(k, _)| k == key)?.1.clone();
    drop(all);
    hooks.refresh_authority(max, yolo, lead_dirs, models);
    Some(hooks)
}

/// The kept worker hook set for `key`: the one made earlier, else `made` (kept from now on).
/// The workers and their supervisor are the same on every owner turn.
pub fn kept_owner_workers(key: &Path, made: Arc<WorkerHooks>) -> Arc<WorkerHooks> {
    let mut all = lock(&OWNER_WORKERS);
    if let Some(hooks) = all.iter().find(|(k, _)| k == key).map(|(_, hooks)| hooks.clone()) {
        drop(all);
        // `retained_owner_workers` returns this same Arc to the next HTTP owner session after
        // applying its current authority. Do not read and rewrite its mutex-protected authority:
        // besides being redundant, doing both used to relock the same non-reentrant mutex and
        // deadlock the owner service. Authority refresh never needs the registry lock either.
        if !Arc::ptr_eq(&hooks, &made) {
            hooks.refresh_authority_from(&made);
        }
        return hooks;
    }
    made.keep_across_turns();
    all.push((key.to_path_buf(), made.clone()));
    made
}

/// Lead shutdown: stop every worker the owner Lead session started.
pub fn stop_owner_workers() {
    // Draining ends this owner service's lifetime. A later service in the same process must build
    // a new supervisor rather than reuse the stopped hook set.
    let all: Vec<Arc<WorkerHooks>> = lock(&OWNER_WORKERS).drain(..).map(|(_, h)| h).collect();
    for hooks in all {
        hooks.stop_all();
    }
}

#[cfg(test)]
fn stop_owner_workers_for(key: &Path) {
    let hooks = {
        let mut all = lock(&OWNER_WORKERS);
        all.iter()
            .position(|(candidate, _)| candidate == key)
            .map(|index| all.remove(index).1)
    };
    if let Some(hooks) = hooks {
        hooks.stop_all();
    }
}

impl Hooks for WorkerHooks {
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply {
        if matches!(p, HookPoint::SessionStart | HookPoint::ModelSelect) {
            self.core.learn();
        }
        // A session that is opened (a new process or `/resume`) takes its workers back up.
        if p == HookPoint::SessionStart
            && !self.core.hold_resume.load(Ordering::SeqCst)
            && matches!(
                payload.get("reason").and_then(Value::as_str),
                Some("startup" | "resume")
            )
        {
            for notice in self.core.resume() {
                ui.notify(&notice, Level::Info);
            }
        }
        if p == HookPoint::AgentSettled && payload["outcome"]["error_class"] == "cloud_quota_exhausted" {
            let (parent, _) = self.core.session();
            let is_go = |model: &str| crate::in_process::is_go_model(model, "");
            if !parent.is_empty() && payload["model"].as_str().is_some_and(is_go) {
                let workers = registry::load(&self.core.agent_dir, &parent).unwrap_or_default();
                let names: Vec<_> = workers
                    .iter()
                    .filter(|r| is_go(&r.model) && is_live(r, &self.core.sessions_dir))
                    .map(|r| r.name.as_str())
                    .collect();
                if !names.is_empty() {
                    ui.notify(
                        &format!("Workers using the affected OpenCode Go quota pool: {}. No worker model was changed.", names.join(", ")),
                        Level::Warning,
                    );
                }
            }
        }
        HookReply::Continue
    }
    fn poll_event(
        &self,
        kind: &str,
        args: &Value,
        _budget: std::time::Duration,
    ) -> Option<Result<Option<String>, String>> {
        matches!(kind, "worker_done" | TOOL_NAME).then(|| self.core.probe(args))
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.tool.clone()]
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            oxa_proto::codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        self.core.prompt_lines()
    }
    fn wants(&self, p: HookPoint) -> bool {
        matches!(
            p,
            HookPoint::SessionStart | HookPoint::ModelSelect | HookPoint::AgentSettled
        )
    }
    fn name(&self) -> &str {
        HOOK_NAME
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *lock(&self.core.host) = Some(host);
        self.core.learn();
    }
    /// Stops every live worker (SIGINT, then the group). Idempotent.
    fn shutdown(&self) {
        if self.core.persistent.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.core.supervisor.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    /// A fresh directory under the system temp directory.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxa-worker-{tag}-{}-{}", std::process::id(), now_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// (root, repo, agent dir, fake oxa): a git repository with one commit, and a fake `oxa` that
    /// sleeps (the child argv is ignored).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = scratch(tag);
        let repo = root.join("repo");
        let agent = root.join("agent");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&agent).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-c")
                .arg("user.name=test")
                .arg("-c")
                .arg("user.email=test@example.invalid")
                .args(args)
                .current_dir(&repo)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let fake = root.join("fake-oxa");
        // `work --supervise` is the sandboxed checkout: it records its argv and returns.
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = work ]; then echo \"$@\" >> '{}/ran-supervise'; exit 0; fi\nsleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        (root, repo, agent, fake)
    }

    fn hooks(fake: &Path, agent: &Path) -> WorkerHooks {
        hooks_with(fake, agent, Vec::new(), ParentScope::default())
    }

    fn hooks_with(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
    ) -> WorkerHooks {
        hooks_with_autonomy(fake, agent, parent_env, parent_scope, Autonomy::Standard)
    }

    fn hooks_with_autonomy(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
        autonomy: Autonomy,
    ) -> WorkerHooks {
        hooks_with_lead(fake, agent, parent_env, parent_scope, autonomy, None)
    }

    fn hooks_with_lead(
        fake: &Path,
        agent: &Path,
        parent_env: Vec<(String, String)>,
        parent_scope: ParentScope,
        autonomy: Autonomy,
        lead_dirs: Option<oxa_config::Dirs>,
    ) -> WorkerHooks {
        let kill = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("a kill executable");
        WorkerHooks::new(WorkerConfig {
            oxa_bin: fake.to_path_buf(),
            cwd: agent.parent().unwrap().join("repo"),
            lead_dirs,
            agent_dir: agent.to_path_buf(),
            max: 4,
            supervisor: Supervisor::new(Duration::from_secs(1), kill).expect("supervisor"),
            sessions_dir: agent.join("sessions"),
            parent_env,
            parent_scope,
            yolo: YoloSwitch::new(autonomy),
            resume_on_start: true,
            advertise_wait: true,
            send_ack_seconds: 0,
            models: ModelPolicy::default(),
            cache: None,
        })
    }

    fn set_table_mode(hooks: &WorkerHooks, mode: Mode) {
        *lock(&hooks.core.table_mode) = Some(mode);
    }

    /// A project that ships `env` and `git` shims in `<repo>/shim`, first on the parent's PATH.
    /// A shim that runs leaves `<root>/ran-<name>`.
    fn shim_env(root: &Path, repo: &Path) -> Vec<(String, String)> {
        let shim = repo.join("shim");
        fs::create_dir_all(&shim).unwrap();
        for name in ["env", "git"] {
            let file = shim.join(name);
            fs::write(
                &file,
                format!("#!/bin/sh\ntouch '{}/ran-{name}'\nexit 1\n", root.display()),
            )
            .unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", shim.display())),
            ("HOME".into(), root.display().to_string()),
        ]
    }

    #[test]
    fn project_env_shim_is_refused() {
        let (root, repo, agent, fake) = fixture("envshim");
        let env = shim_env(&root, &repo);
        let tools = HostTools::resolve(&env, &repo).unwrap();
        let env_bin = tools.program("env").expect("a trusted env");
        assert!(!env_bin.starts_with(&repo), "{}", env_bin.display());
        // Resolved from the worktree as the project, the shim would have been accepted.
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!root.join("ran-env").exists(), "the project env shim ran");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_git_shim_is_refused() {
        let (root, repo, agent, fake) = fixture("gitshim");
        let env = shim_env(&root, &repo);
        assert!(!HostTools::resolve(&env, &repo)
            .unwrap()
            .program("git")
            .unwrap()
            .starts_with(&repo));
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(!root.join("ran-git").exists(), "the project git shim ran");
        assert!(worktree_path(&repo, "alpha").unwrap().is_dir());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_and_resume_get_the_same_child_env() {
        let (root, repo, agent, _) = fixture("exactenv");
        let out_file = root.join("env.out");
        let fake = root.join("env-oxa");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = work ] && exit 0\n/usr/bin/env > '{}'\nsleep 30\n",
                out_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), root.display().to_string()),
            ("OXA_TRUST_PROJECT".to_string(), "1".to_string()),
            ("OXA_SESSION_HINT".to_string(), "abc.s1".to_string()),
            ("KEEP_ME".to_string(), "yes".to_string()),
        ];
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        let capture_env = || {
            for _ in 0..100 {
                if let Ok(text) = fs::read_to_string(&out_file) {
                    if text.contains("KEEP_ME=yes\n") {
                        return text;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("missing env output");
        };
        let text = capture_env();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            !lines.iter().any(|l| l.starts_with("OXA_TRUST_PROJECT=")),
            "{text}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("OXA_SESSION_HINT=")),
            "{text}"
        );
        assert!(lines.contains(&"OXA_WORKER_DEPTH=1"), "{text}");
        assert!(lines.contains(&"KEEP_ME=yes"), "{text}");
        assert!(lines.iter().any(|l| l.starts_with("PATH=")), "{text}");
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        fs::remove_file(&out_file).unwrap();
        let notices = hooks.core.resume();
        assert!(
            notices.iter().any(|notice| notice.contains("relaunched")),
            "{notices:?}"
        );
        let resumed = capture_env();
        assert_eq!(
            resumed.lines().collect::<std::collections::BTreeSet<_>>(),
            text.lines().collect::<std::collections::BTreeSet<_>>()
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_notice_shows_the_budget() {
        let (root, repo, agent, fake) = fixture("budget");
        let scope = ParentScope {
            max_tokens: Some(4242),
            ..ParentScope::default()
        };
        let hooks = hooks_with(&fake, &agent, Vec::new(), scope);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("4242 tokens"), "{}", out.text);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_git_lead_starts_a_worker_in_the_configured_repository() {
        use oxa_proto::{CancelToken, Decision, Policy};

        let (root, repo, agent, fake) = fixture("lead-repo");
        let cwd = root.join("lead");
        fs::create_dir(&cwd).unwrap();
        let repo = repo.canonicalize().unwrap();
        fs::write(
            agent.join("settings.json"),
            json!({"oxa": {"workers": {"repository": repo}}}).to_string(),
        )
        .unwrap();
        let mut dirs = oxa_config::Dirs::under(&root);
        dirs.agent_dir = agent.clone();
        let hooks = hooks_with_lead(
            &fake,
            &agent,
            vec![("PATH".into(), "/usr/bin:/bin".into())],
            ParentScope::default(),
            Autonomy::Auto,
            Some(dirs),
        );
        set_session(&hooks, "lead-parent");
        let tool = &hooks.tool;
        let args = json!({"action": "start", "name": "alpha", "goal": "one", "mode": "yolo"});
        let mut cx = ToolCtx {
            cwd: &cwd,
            spill_dir: &root,
            sandbox: &oxa_sandbox::NoSandbox,
            mode: Mode::Workspace,
            cancel: &CancelToken::default(),
            env: &[],
            tune: Default::default(),
            checked: &[],
            hidden: None,
        };
        let error = tool.access(&args, &cx).unwrap_err();
        assert!(error.contains("must be explicitly trusted"), "{error}");
        oxa_perm::TrustStore::default()
            .save(&agent.join("trust.json"), &repo, true)
            .unwrap();
        let policy = oxa_perm::ModePolicy::new().with_yolo(YoloSwitch::new(Autonomy::Auto));
        let checked: Vec<_> = tool
            .access(&args, &cx)
            .unwrap()
            .into_iter()
            .filter_map(|access| {
                let (decision, pin) = policy.decide_pinned(cx.mode, &access, cx.cwd);
                assert_eq!(decision, Decision::Allow);
                match access {
                    Access::Write(path) => Some((path, pin.unwrap())),
                    _ => None,
                }
            })
            .collect();
        let denied = tool.run(&args, &cx, &mut |_| {});
        assert!(denied.is_error && denied.text.contains("permission approvals"));
        assert!(!worktree_path(&repo, "alpha").unwrap().exists());
        cx.checked = &checked;
        let out = tool.run(&args, &cx, &mut |_| {});
        assert!(!out.is_error, "{}", out.text);
        let record = registry::load(&agent, "lead-parent").unwrap().remove(0);
        assert_eq!(record.mode, "auto", "the repository must not widen worker mode");
        assert_eq!(record.worktree, worktree_path(&repo, "alpha").unwrap());
        assert!(record.worktree.join(".git").is_file());
        let branch = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "--verify", "refs/heads/agent/alpha"])
            .output()
            .unwrap();
        assert!(
            branch.status.success(),
            "the branch must be in the selected repository"
        );
        let _ = hooks.core.stop(&json!({"name": "alpha"}));

        fs::remove_file(agent.join("settings.json")).unwrap();
        // Even a project setting in the orchestration directory cannot choose a repository.
        fs::create_dir(cwd.join(".oxa")).unwrap();
        fs::write(
            cwd.join(".oxa/settings.json"),
            json!({"oxa": {"workers": {"repository": repo}}}).to_string(),
        )
        .unwrap();
        let mut model_input = args.clone();
        model_input["repository"] = json!(repo);
        let error = tool.access(&model_input, &cx).unwrap_err();
        assert!(
            error.contains("set oxa.workers.repository in user settings"),
            "{error}"
        );

        let in_repo = ToolCtx { cwd: &repo, ..cx };
        assert!(tool
            .access(&args, &in_repo)
            .unwrap()
            .contains(&Access::Write(record.worktree)));
        let _ = fs::remove_dir_all(&root);
    }

    fn set_session(hooks: &WorkerHooks, id: &str) {
        *lock(&hooks.core.session) = (id.to_string(), "model-x".to_string());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quota_stop_warns_about_live_workers_using_the_same_go_pool() {
        let (root, repo, agent, fake) = fixture("quota-pool");
        let hooks = hooks(&fake, &agent);
        let parent = "parent-1";
        set_session(&hooks, parent);
        let worker = |name: &str, model: &str, stopped: bool| WorkerRecord {
            name: name.into(),
            session_id: name.into(),
            pid: std::process::id(),
            pgid: 0,
            worktree: repo.clone(),
            branch: format!("agent/{name}"),
            model: model.into(),
            mode: "yolo".into(),
            goal_file: root.join("goal"),
            status_file: root.join("not-started.json"),
            log_file: root.join("worker.log"),
            started_ms: 0,
            stopped,
            ..WorkerRecord::default()
        };
        let records = [
            worker("shared-pool", "opencode/synthetic-worker", false),
            worker("other-provider", "openai/synthetic-worker", false),
            worker("stopped-worker", "opencode/synthetic-worker", true),
        ];
        registry::save(&agent, parent, &records).unwrap();
        let mut ui = oxa_proto::HeadlessUi::default();
        hooks.fire(
            HookPoint::AgentSettled,
            &json!({
                "outcome": {"error_class": "cloud_quota_exhausted"},
                "model": "opencode/synthetic-parent"
            }),
            &mut ui,
        );
        assert_eq!(ui.notes.len(), 1);
        let (level, note) = &ui.notes[0];
        assert_eq!(*level, Level::Warning);
        assert!(note.contains("shared-pool"));
        assert!(!note.contains("other-provider") && !note.contains("stopped-worker"));
        let after = registry::load(&agent, parent).unwrap();
        assert_eq!(after, records);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_start_writes_registry_goal_file_and_worktree() {
        let (root, repo, agent, fake) = fixture("start");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "write the thing"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        let records = registry::load(&agent, "parent-1").unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.name, "alpha");
        assert_eq!(rec.branch, "agent/alpha");
        assert_eq!(rec.worktree, worktree_path(&repo, "alpha").unwrap());
        assert!(rec.worktree.is_dir());
        assert_eq!(
            rec.goal_file,
            agent.join("workers").join("parent-1").join("alpha.goal.md")
        );
        assert_eq!(fs::read_to_string(&rec.goal_file).unwrap(), "write the thing");
        assert!(!rec.stopped);
        let stopped = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        assert!(!stopped.is_error, "{}", stopped.text);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_refuses_second_owner_for_same_name() {
        let (root, repo, agent, fake) = fixture("dup");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let args = json!({"action": "start", "name": "alpha", "goal": "one"});
        let first = hooks
            .core
            .start(&args, &repo, Mode::Workspace, Autonomy::Standard);
        assert!(!first.is_error, "{}", first.text);
        let second = hooks
            .core
            .start(&args, &repo, Mode::Workspace, Autonomy::Standard);
        assert!(second.is_error);
        assert!(second.text.contains("already exists"), "{}", second.text);
        assert_eq!(registry::load(&agent, "parent-1").unwrap().len(), 1);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    fn start_alpha(hooks: &WorkerHooks, repo: &Path) {
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "g"}),
            repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
    }

    #[test]
    fn bindings_report_live_dead_absent_and_refused_without_touching_the_registry() {
        let root = scratch("bindings");
        let (agent, sessions) = (root.join("agent"), root.join("sessions"));
        fs::create_dir_all(&agent).unwrap();
        assert_eq!(
            bindings(&agent, &sessions, "parent-1"),
            Err(BindingsError::Absent)
        );
        let record = |name: &str, pid: u32| WorkerRecord {
            name: name.into(),
            session_id: format!("child-{name}"),
            branch: format!("agent/{name}"),
            model: "provider/model".into(),
            pid,
            pgid: pid,
            worktree: root.join(name),
            ..WorkerRecord::default()
        };
        registry::save(
            &agent,
            "parent-1",
            &[record("alive", std::process::id()), record("gone", u32::MAX - 1)],
        )
        .unwrap();
        let found = bindings(&agent, &sessions, "parent-1").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].branch, "agent/alive");
        assert!(matches!(
            found[0].state,
            WorkerState::Starting | WorkerState::Working | WorkerState::Idle
        ));
        assert_eq!(found[1].state, WorkerState::Dead);
        // A corrupt registry is refused and its bytes are left as they were.
        let path = agent.join("workers").join("parent-2.json");
        fs::write(&path, "not json").unwrap();
        assert_eq!(
            bindings(&agent, &sessions, "parent-2"),
            Err(BindingsError::Refused)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_send_lands_in_child_inbox_dir() {
        let (root, repo, agent, fake) = fixture("send");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        let out = hooks
            .core
            .send(&json!({"action": "send", "name": "alpha", "text": "please add tests"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("file accepted"), "{}", out.text);
        assert!(out.text.contains("worker phase"), "{}", out.text);
        let inbox = agent.join("inbox").join(&rec.session_id);
        let texts: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".steer"))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert_eq!(texts, vec!["please add tests".to_string()]);
        // An unknown worker and empty text are refused; nothing else lands anywhere.
        assert!(hooks.core.send(&json!({"name": "nope", "text": "x"})).is_error);
        assert!(hooks.core.send(&json!({"name": "alpha", "text": " "})).is_error);
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_wait_hands_worker_completion_back_to_the_model() {
        use oxa_fake::{ScriptTurn, ScriptedEngine};
        use oxa_proto::{AgentEvent, EventSink, HeadlessUi};

        // The worker the wait watches. It exists before the turn starts, with its process
        // already gone, which is what a finished worker looks like to the probe; this test
        // asks only that a `wait` on `worker_done` hands that state back to the model.
        struct FinishDuringWait {
            agent: PathBuf,
            parent: String,
        }
        impl EventSink for FinishDuringWait {
            fn emit(&mut self, event: &AgentEvent) {
                if !matches!(event, AgentEvent::ToolExecutionUpdate { tool_name, .. } if tool_name == "wait")
                {
                    return;
                }
                let rec = registry::load(&self.agent, &self.parent).unwrap().remove(0);
                let status = json!({"session_id": rec.session_id, "pid": rec.pid, "phase": "finished",
                    "outcome": {"type": "oxa_result", "outcome": "done", "exit_code": 0}});
                fs::create_dir_all(rec.status_file.parent().unwrap()).unwrap();
                crate::status::atomic(&rec.status_file, &serde_json::to_vec(&status).unwrap()).unwrap();
            }
        }

        let (root, repo, agent, fake) = fixture("wait");
        let hooks = Arc::new(hooks(&fake, &agent));
        let engine = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::tool(
                "w",
                "wait",
                r#"{"seconds":5,"reason":"Review","until":{"kind":"worker_done","name":"alpha"}}"#,
            ),
            ScriptTurn::tool(
                "done",
                "goal_done",
                r#"{"status":"complete","evidence":"Review finished."}"#,
            ),
        ]));
        let mut session = oxa_core::Session::open(
            oxa_core::Deps {
                engine: engine.clone(),
                store: Arc::new(oxa_session::MemStore::new()),
                tools: vec![],
                hooks: vec![hooks.clone()],
                policy: Box::new(oxa_perm::ModePolicy::new()),
                sandbox: Arc::new(oxa_sandbox::NoSandbox),
                settings: oxa_config::Settings::default(),
                dirs: oxa_config::Dirs::under(&root),
                cwd: repo.clone(),
                selection: Default::default(),
                resources: Default::default(),
                model: None,
                thinking: None,
                mode: None,
                session_id: None,
                jobs: Arc::new(oxa_tools::JobRegistry::new()),
            },
            oxa_core::Open::New,
        )
        .unwrap();
        let goal = oxa_session::Goal::new("Review synthetic work.".into(), Default::default(), 1000).unwrap();
        session.set_goal(Some(goal), &mut oxa_proto::NullSink).unwrap();
        // The hook set learns the parent session (and model) through the host at attach.
        let (parent, _) = hooks.core.session();
        assert_eq!(parent, session.inner().sref.id);
        // A real launch, so the registry, the status file and the probe are the production ones;
        // the worker's process is a fake that lingers, so the outcome below is the status file's
        // own and not a guess from a missing process. The launch asks the session for the model
        // catalog, which only a run's host pump answers, so the test answers it here.
        *lock(&hooks.core.host) = Some(Arc::new(CatalogHost(true)));
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "g"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        let mut sink = FinishDuringWait {
            agent: agent.clone(),
            parent: parent.clone(),
        };
        session
            .prompt(
                "Wait for the review.".into(),
                vec![],
                &mut sink,
                &mut HeadlessUi::default(),
            )
            .unwrap();
        let outcome = session.stats().outcome.clone();
        drop(session);
        hooks.stop_all();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(outcome, oxa_core::RunOutcome::Done);
        let requests = engine.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools_json.contains("worker_done"));
        assert!(requests[1]
            .messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("w")
                && m.content.as_deref() == Some("worker alpha: finished, outcome done")));
    }

    #[test]
    fn worker_probe_reports_finished_outcome() {
        let (root, repo, agent, fake) = fixture("probe");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        let args = json!({"kind": "worker", "name": "alpha"});
        // Running: still pending.
        assert_eq!(
            hooks.poll_event("worker", &args, std::time::Duration::from_secs(5)),
            Some(Ok(None))
        );
        // Another kind is not ours.
        assert_eq!(
            hooks.poll_event("pr", &args, std::time::Duration::from_secs(5)),
            None
        );
        // The worker reports its typed outcome without forwarding its potentially secret log.
        let status = json!({"session_id": rec.session_id, "pid": rec.pid, "phase": "finished",
            "outcome": {"type": "oxa_result", "outcome": "done", "error_class": null, "exit_code": 0}});
        fs::create_dir_all(rec.status_file.parent().unwrap()).unwrap();
        let mut stale = status.clone();
        stale["session_id"] = json!("old-worker");
        fs::write(&rec.status_file, stale.to_string()).unwrap();
        assert_eq!(
            hooks.poll_event("worker_done", &args, Duration::from_secs(5)),
            Some(Ok(None))
        );
        fs::write(&rec.status_file, status.to_string()).unwrap();
        set_session(&hooks, "parent-2");
        assert!(matches!(
            hooks.poll_event("worker_done", &args, Duration::from_secs(5)),
            Some(Err(_))
        ));
        set_session(&hooks, "parent-1");
        let log: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        fs::write(&rec.log_file, log.join("\n")).unwrap();
        let text = hooks
            .poll_event("worker", &args, std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .expect("done");
        assert!(text.contains("outcome done"), "{text}");
        assert!(!text.contains("line 25") && !text.contains("log lines"), "{text}");
        // An unknown worker is a probe failure.
        assert!(hooks
            .poll_event(
                "worker",
                &json!({"name": "nope"}),
                std::time::Duration::from_secs(5)
            )
            .unwrap()
            .is_err());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_inherits_parent_model_and_retains_it_on_resume() {
        worker_model_launch_and_resume("model-inherited", None, "model-x");
    }

    #[test]
    fn worker_selects_explicit_model_and_retains_it_on_resume() {
        worker_model_launch_and_resume("model-explicit", Some("other/model"), "other/model");
    }

    fn worker_model_launch_and_resume(tag: &str, requested: Option<&str>, expected: &str) {
        let (root, repo, agent, _fake) = fixture(tag);
        // A fake oxa that records its argv next to itself, then sleeps. The worktree step
        // (`oxa work --supervise`) just succeeds.
        let fake = root.join("argv-oxa");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = work ]; then exit 0; fi\nprintf '%s\\n' \"$@\" > '{0}/argv.tmp'\nmv '{0}/argv.tmp' '{0}/argv.txt'\nexec /bin/sleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
        let hooks = hooks_with(&fake, &agent, env, ParentScope::default());
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        let mut args = json!({"action": "start", "name": "alpha", "goal": "g"});
        if let Some(model) = requested {
            args["model"] = json!(model);
        }
        let out = hooks
            .core
            .start(&args, &repo, Mode::Workspace, Autonomy::Standard);
        assert!(!out.is_error, "{}", out.text);
        let before = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_eq!(before.model, expected);
        let argv_file = root.join("argv.txt");
        let start_argv = wait_for_worker_argv(&argv_file, "--goal");
        assert_worker_model(&start_argv, expected);
        // The parent goes away: the worker is killed but not marked stopped.
        hooks.core.supervisor.stop(before.pgid).unwrap();
        assert!(!registry::load(&agent, "parent-1").unwrap()[0].stopped);
        fs::remove_file(&argv_file).unwrap();
        // Reopen the same parent with a different model. Resume uses the worker's record.
        let reopened = hooks_with(
            &fake,
            &agent,
            vec![("PATH".into(), "/usr/bin:/bin".into())],
            ParentScope::default(),
        );
        *lock(&reopened.core.session) = ("parent-1".into(), "changed-parent".into());
        set_table_mode(&reopened, Mode::Workspace);
        let notices = reopened.core.resume();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("alpha") && notices[0].contains("--continue="),
            "{notices:?}"
        );
        let argv = wait_for_worker_argv(&argv_file, &format!("--continue={}", before.session_id));
        assert_worker_model(&argv, expected);
        assert!(!argv.lines().any(|l| l == "--goal"), "{argv}");
        let after = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_ne!(after.pid, before.pid);
        // A second open in the same process does not launch it again, and a stopped worker is
        // never relaunched.
        assert!(reopened.core.resume().is_empty());
        let _ = reopened.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    fn wait_for_worker_argv(path: &Path, marker: &str) -> String {
        for _ in 0..100 {
            if let Ok(argv) = fs::read_to_string(path) {
                if argv.lines().any(|line| line == marker) {
                    return argv;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("worker did not record {marker} in its argv");
    }

    fn assert_worker_model(argv: &str, expected: &str) {
        let mut lines = argv.lines();
        assert_eq!(lines.find(|line| *line == "--model"), Some("--model"), "{argv}");
        assert_eq!(lines.next(), Some(expected), "{argv}");
    }

    #[test]
    fn worker_refuses_malformed_model_before_creating_worktree() {
        let (root, repo, agent, fake) = fixture("invalid-model");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        for model in [json!("  "), json!("--yolo"), json!("m\nother"), json!(42)] {
            let out = hooks.core.start(
                &json!({"action": "start", "name": "alpha", "goal": "one", "model": model}),
                &repo,
                Mode::Workspace,
                Autonomy::Standard,
            );
            assert!(out.is_error && out.text.contains("model"), "{}", out.text);
            assert!(registry::load(&agent, "parent-1").unwrap().is_empty());
            assert!(!worktree_path(&repo, "alpha").unwrap().exists());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lead_started_worker_survives_the_next_owner_turn() {
        let (root, repo, agent, fake) = fixture("owner-turn");
        let made = Arc::new(hooks(&fake, &agent));
        // Turn 1: the owner Lead session starts a worker, then its session closes.
        let first = kept_owner_workers(&agent, made.clone());
        assert!(Arc::ptr_eq(&first, &made));
        set_session(&first, "parent-1");
        start_alpha(&first, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        first.shutdown();
        // Turn 2: the kept hook set is found before another one (and its pinned launcher) is built.
        let second = retained_owner_workers(
            &agent,
            4,
            &YoloSwitch::new(Autonomy::Standard),
            None,
            ModelPolicy::default(),
        )
        .expect("the owner hooks should be retained");
        assert!(Arc::ptr_eq(&second, &made));
        assert!(
            PathBuf::from(format!("/proc/{}", rec.pid)).exists(),
            "worker died with the turn"
        );
        assert!(second.core.list().text.contains("alpha"));
        // Lead shutdown stops the tree.
        stop_owner_workers_for(&agent);
        assert!(!PathBuf::from(format!("/proc/{}", rec.pid)).exists());
        let restarted = Arc::new(hooks(&fake, &agent));
        assert!(Arc::ptr_eq(
            &kept_owner_workers(&agent, restarted.clone()),
            &restarted
        ));
        stop_owner_workers();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retained_owner_workers_refresh_the_current_launch_ceiling() {
        let (root, repo, agent, fake) = fixture("owner-authority");
        let first = Arc::new(hooks(&fake, &agent));
        let kept = kept_owner_workers(&agent, first.clone());
        set_session(&kept, "parent-authority");
        start_alpha(&kept, &repo);

        let current = retained_owner_workers(
            &agent,
            1,
            &YoloSwitch::new(Autonomy::Standard),
            None,
            ModelPolicy::default(),
        )
        .expect("the owner hooks should be retained");
        assert!(Arc::ptr_eq(&current, &first));

        // LeadOwner::keep_workers passes the hook found above back through this function. Both
        // arguments consequently share one core, so authority refresh must not hold its source
        // mutex while locking the destination mutex.
        let reused = kept_owner_workers(&agent, current);
        assert!(Arc::ptr_eq(&reused, &first));
        assert_eq!(reused.core.max.load(Ordering::SeqCst), 1);
        assert_eq!(lock(&reused.core.yolo).level(), Autonomy::Standard);

        let out = reused.core.start(
            &json!({"action": "start", "name": "beta", "goal": "two"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("limit is 1"), "{}", out.text);
        assert!(!worktree_path(&repo, "beta").unwrap().exists());
        stop_owner_workers_for(&agent);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn owner_worker_test_cleanup_is_scoped_to_its_hook_set() {
        let (first_root, first_repo, first_agent, first_fake) = fixture("owner-cleanup-first");
        let first = kept_owner_workers(&first_agent, Arc::new(hooks(&first_fake, &first_agent)));
        set_session(&first, "parent-first");
        start_alpha(&first, &first_repo);

        let (second_root, second_repo, second_agent, second_fake) = fixture("owner-cleanup-second");
        let second = kept_owner_workers(&second_agent, Arc::new(hooks(&second_fake, &second_agent)));
        set_session(&second, "parent-second");
        start_alpha(&second, &second_repo);
        let second_record = registry::load(&second_agent, "parent-second").unwrap().remove(0);

        stop_owner_workers_for(&first_agent);
        assert!(
            PathBuf::from(format!("/proc/{}", second_record.pid)).exists(),
            "cleaning one owner hook set stopped another hook set's worker"
        );

        stop_owner_workers_for(&second_agent);
        let _ = fs::remove_dir_all(&first_root);
        let _ = fs::remove_dir_all(&second_root);
    }

    #[test]
    fn no_resume_before_owner_binding_recheck() {
        let (root, repo, agent, fake) = fixture("held-resume");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        hooks.hold_resume();
        // The session opens (SessionStart, startup) before the owner binding is rechecked.
        let mut ui = oxa_proto::HeadlessUi::default();
        hooks.fire(HookPoint::SessionStart, &json!({"reason": "startup"}), &mut ui);
        let held = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_eq!(held.pid, rec.pid, "a worker was relaunched before the recheck");
        // After the recheck the caller resumes explicitly.
        let notices = hooks.resume_after_owner_check(true);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("relaunched"), "{notices:?}");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn one_shot_owner_suppresses_explicit_resume() {
        let (root, repo, agent, fake) = fixture("suppressed-resume");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        hooks.suppress_resume();

        assert!(hooks.resume_after_owner_check(true).is_empty());
        let held = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_eq!(held.pid, rec.pid, "one-shot owner relaunched a saved worker");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_resume_honors_worker_opt_out() {
        let (root, repo, agent, fake) = fixture("disabled-explicit-resume");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();

        assert!(hooks.resume_after_owner_check(false).is_empty());
        let held = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_eq!(held.pid, rec.pid, "worker opt-out relaunched a saved worker");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saved_worker_still_auto_resumes_with_default_setting() {
        // `resume_on_start` is what `oxa.workers.resumeOnStart` (default true) sets.
        let key = oxa_config::schema()
            .iter()
            .find(|k| k.key == resume::SETTING_RESUME)
            .expect("the setting is in the schema");
        assert_eq!(key.default, "true");
        let (root, repo, agent, fake) = fixture("auto-resume");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        let notices = hooks.core.resume();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("relaunched"), "{notices:?}");
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_never_runs_a_checkout_filter_on_the_host() {
        let (root, repo, agent, fake) = fixture("filter");
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-c")
                .arg("user.name=test")
                .arg("-c")
                .arg("user.email=test@example.invalid")
                .args(args)
                .current_dir(&repo)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        fs::write(repo.join("a.txt"), "content\n").unwrap();
        fs::write(repo.join(".gitattributes"), "a.txt filter=probe\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "files"]);
        // A smudge filter that leaves a marker outside the worktree when it runs.
        let marker = root.join("filter-ran");
        git(&[
            "config",
            "filter.probe.smudge",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ]);
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(!marker.exists(), "a repository filter ran on the host");
        // The checkout was handed to the sandboxed supervisor as a checkout request.
        let ran = fs::read_to_string(root.join("ran-supervise")).unwrap_or_default();
        assert!(
            ran.contains("--supervise") && ran.contains("\"kind\":\"checkout\""),
            "supervisor argv: {ran}"
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_stops_relaunched_workers_when_registry_write_fails() {
        let (root, repo, agent, fake) = fixture("rollback");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        set_table_mode(&hooks, Mode::Workspace);
        start_alpha(&hooks, &repo);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        hooks.core.supervisor.stop(rec.pgid).unwrap();
        // A read-only registry directory makes the save fail (a privileged user ignores modes).
        let dir = agent.join("workers");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let writable = fs::write(dir.join(".probe"), b"x").is_ok();
        let notices = hooks.core.resume();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        if !writable {
            assert!(notices.iter().any(|n| n.contains("were stopped")), "{notices:?}");
            // The relaunched process is gone and the registry still holds the old record.
            let now = registry::load(&agent, "parent-1").unwrap().remove(0);
            assert_eq!(now.pid, rec.pid);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_start_uses_the_private_copy_after_the_original_is_replaced() {
        let (root, repo, agent, fake) = fixture("pin");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        // Another file renamed over the binary after the session started.
        let other = root.join("other-oxa");
        fs::write(
            &other,
            format!("#!/bin/sh\ntouch '{}/replacement-ran'\n", root.display()),
        )
        .unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&other, &fake).unwrap();
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one"}),
            &repo,
            Mode::Workspace,
            Autonomy::Standard,
        );
        assert!(!out.is_error, "worker start failed");
        assert!(!root.join("replacement-ran").exists(), "the replacement ran");
        assert!(worktree_path(&repo, "alpha").unwrap().exists());
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn yolo_worker_materializes_without_the_sandbox_supervisor() {
        let (root, repo, agent, fake) = fixture("yolo-checkout");
        fs::write(repo.join("tracked.txt"), "content\n").unwrap();
        let ok = Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
            && Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "tracked",
                ])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success();
        assert!(ok, "fixture commit failed");
        let hooks = hooks_with_autonomy(&fake, &agent, Vec::new(), ParentScope::default(), Autonomy::Yolo);
        set_session(&hooks, "parent-1");
        let out = hooks.core.start(
            &json!({"action": "start", "name": "alpha", "goal": "one", "mode": "yolo"}),
            &repo,
            Mode::Workspace,
            Autonomy::Yolo,
        );
        assert!(!out.is_error, "yolo worker start failed");
        assert!(worktree_path(&repo, "alpha")
            .unwrap()
            .join("tracked.txt")
            .is_file());
        assert!(
            !root.join("ran-supervise").exists(),
            "yolo invoked the sandbox supervisor"
        );
        let _ = hooks.core.stop(&json!({"action": "stop", "name": "alpha"}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_true_cannot_enable_workers() {
        let user_off = json!({"oxa": {"workers": {"enabled": false}}});
        let project_on = json!({"oxa": {"workers": {"enabled": true}}});
        let set = oxa_config::Settings::from_values(user_off, project_on.clone());
        assert!(!enabled(&set));
        assert!(!built(None, enabled(&set)));
        // The project is ignored in both directions: unset by the user means the default.
        assert!(enabled(&oxa_config::Settings::from_values(json!({}), project_on)));
        let project_off = json!({"oxa": {"workers": {"enabled": false}}});
        assert!(enabled(&oxa_config::Settings::from_values(
            json!({}),
            project_off
        )));
    }

    #[test]
    fn project_cannot_raise_worker_max() {
        let set = |user: Value, project: Value| oxa_config::Settings::from_values(user, project);
        let two = json!({"oxa": {"workers": {"max": 2}}});
        let ten = json!({"oxa": {"workers": {"max": 10}}});
        assert_eq!(max_workers(&set(two.clone(), ten.clone())), 2);
        assert_eq!(max_workers(&set(json!({}), ten)), 4);
        assert_eq!(max_workers(&set(two, json!({}))), 2);
    }

    #[test]
    fn worker_controls_read_the_user_file_only() {
        use oxa_config::Settings;
        // The user turned workers off and capped them: a trusted project cannot undo that.
        let user = Settings::from_values(
            json!({"oxa": {"workers": {"enabled": false, "max": 1}}}),
            json!({"oxa": {"workers": {"enabled": true, "max": 99}}}),
        );
        assert!(!enabled(&user));
        assert_eq!(max_workers(&user), 1);
        // A project alone cannot change the defaults either way.
        let project = Settings::from_values(
            json!({}),
            json!({"oxa": {"workers": {"enabled": false, "max": 99}}}),
        );
        assert!(enabled(&project));
        assert_eq!(max_workers(&project), 4);
    }

    #[test]
    fn worker_tool_absent_in_worker_child() {
        // A worker child sets OXA_WORKER_DEPTH: the hook set (and so the tool) is not built,
        // whatever the setting says. Outside a worker it is built unless switched off.
        assert!(built(None, true));
        assert!(!built(Some("1"), true));
        assert!(!built(Some(""), true));
        assert!(!built(None, false));
    }

    #[test]
    fn worker_list_declares_no_command_or_network_access() {
        let cwd = Path::new("/example/project");
        assert_eq!(
            worker_access(&json!({"action": "list"}), cwd, false).unwrap(),
            Vec::new()
        );

        let stop = worker_access(&json!({"action": "stop", "name": "alpha"}), cwd, false).unwrap();
        assert_eq!(
            stop,
            vec![Access::Exec {
                command: COMMAND.to_string(),
                net: false,
            }]
        );
    }

    #[test]
    fn approved_new_worktree_keeps_its_nonexistent_tail() {
        let root = scratch("pin-new");
        let path = root.join("missing-parent").join("repo").join("alpha");
        let approved = oxa_perm::resolve_real(&path).expect("permission pin");

        assert!(approved_worktree_is_unchanged(&path, &approved));

        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("missing-parent")).unwrap();
        assert!(!approved_worktree_is_unchanged(&path, &approved));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn child_session_id_is_bounded_for_the_longest_parent_id() {
        let parent = "p".repeat(128);
        let id = child_session_id(&parent, "forty-character-worker-name-000000000000", u64::MAX);
        // Session identifiers are treated as private metadata by the log scanners. Keep assertion
        // failures useful without copying the generated identifier into test output.
        assert!(id.len() <= 128, "generated child session id is too long");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        let other = child_session_id(
            &format!("{parent}x"),
            "forty-character-worker-name-000000000000",
            u64::MAX,
        );
        assert!(id != other, "distinct parents must produce distinct child ids");
    }

    // ---- model per job -------------------------------------------------------------------

    fn policy(value: Value) -> ModelPolicy {
        model_policy::parse(Some(&value)).0
    }

    /// Like `hooks`, with the owner's model policy.
    fn hooks_with_policy(fake: &Path, agent: &Path, models: ModelPolicy) -> WorkerHooks {
        let hooks = hooks_with(
            fake,
            agent,
            vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
            ParentScope::default(),
        );
        *lock(&hooks.core.models) = models;
        hooks
    }

    /// A fake oxa that appends each launch's argv (one element per line, then `--`) to
    /// `<root>/argv.log`.
    fn logging_oxa(root: &Path) -> PathBuf {
        let fake = root.join("log-oxa");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nif [ \"$1\" = work ]; then exit 0; fi\nprintf '%s\\n' \"$@\" >> '{0}/argv.log'\necho -- >> '{0}/argv.log'\nexec /bin/sleep 30\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    /// The `--model` values of the launches so far, once there are `count` of them.
    fn launched_models(root: &Path, count: usize) -> Vec<String> {
        // Wait for up to 30 s: a slow runner may take a moment to reach the fake oxa's first line.
        // It cannot recover a line that was never written: a worker stopped before the child writes
        // its argv is not recorded at all, so stop a worker only after this returns its launch.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let text = fs::read_to_string(root.join("argv.log")).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let models: Vec<String> = lines
                .windows(2)
                .filter(|w| w[0] == "--model")
                .map(|w| w[1].to_string())
                .collect();
            if models.len() >= count {
                return models;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("fewer than {count} launches recorded");
    }

    fn start_with(hooks: &WorkerHooks, repo: &Path, extra: Value) -> ToolOutput {
        let mut args = json!({"action": "start", "goal": "g"});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        hooks.core.start(&args, repo, Mode::Workspace, Autonomy::Standard)
    }

    fn stop_all(hooks: &WorkerHooks, names: &[&str]) {
        for name in names {
            let _ = hooks.core.stop(&json!({"action": "stop", "name": name}));
        }
    }

    #[test]
    fn policy_alternates_models_by_kind_and_records_the_choice() {
        let (root, repo, agent, _) = fixture("policy-kind");
        let hooks = hooks_with_policy(
            &logging_oxa(&root),
            &agent,
            policy(json!({"kinds": {"review": ["a", "b"]}})),
        );
        set_session(&hooks, "parent-1");
        let first = start_with(
            &hooks,
            &repo,
            json!({"name": "r1", "kind": "review", "job": "pr-1"}),
        );
        assert!(!first.is_error, "{}", first.text);
        assert!(
            first.text.contains("model a (policy: kind review)"),
            "{}",
            first.text
        );
        let second = start_with(&hooks, &repo, json!({"name": "r2", "kind": "review"}));
        assert!(!second.is_error, "{}", second.text);
        assert_eq!(launched_models(&root, 2), vec!["a", "b"]);
        let records = registry::load(&agent, "parent-1").unwrap();
        assert_eq!(
            (records[0].kind.as_str(), records[0].job.as_str()),
            ("review", "pr-1")
        );
        assert_eq!(records[0].choice, "policy: kind review");
        assert_eq!(records[1].job, "r2");
        let listed = hooks.core.list().text;
        assert!(
            listed.contains("model a") && listed.contains("kind review"),
            "{listed}"
        );
        stop_all(&hooks, &["r1", "r2"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stopped_workers_with_an_interrupted_finish_do_not_escalate() {
        let (root, repo, agent, _) = fixture("policy-stop-no-escalate");
        let hooks = hooks_with_policy(
            &logging_oxa(&root),
            &agent,
            policy(json!({"kinds": {"review": ["a", "b"]}, "escalateTo": ["s"]})),
        );
        set_session(&hooks, "parent-1");
        // Stop each worker only after its launch is recorded: a stop before the child writes its
        // argv loses that line for good. Both stopped launches still take a policy slot (a, then b).
        for (name, launches) in [("t1", 1), ("t2", 2)] {
            let out = start_with(
                &hooks,
                &repo,
                json!({"name": name, "kind": "review", "job": "pr-9"}),
            );
            assert!(!out.is_error, "{}", out.text);
            launched_models(&root, launches);
            let stop = hooks.core.stop(&json!({"action": "stop", "name": name}));
            assert!(!stop.is_error, "{}", stop.text);
        }
        // The child answers SIGINT with an interrupted finish, which wins over the stopped marker.
        for rec in registry::load(&agent, "parent-1").unwrap() {
            fs::create_dir_all(rec.status_file.parent().unwrap()).unwrap();
            fs::write(
                &rec.status_file,
                json!({"phase": "finished", "outcome": {"outcome": "interrupted"}}).to_string(),
            )
            .unwrap();
        }
        let third = start_with(
            &hooks,
            &repo,
            json!({"name": "t3", "kind": "review", "job": "pr-9"}),
        );
        assert!(!third.is_error, "{}", third.text);
        assert!(third.text.contains("policy: kind review"), "{}", third.text);
        assert_ne!(launched_models(&root, 3)[2], "s");
        stop_all(&hooks, &["t1", "t2", "t3"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn escalation_is_the_floor_after_recorded_failures_on_one_job() {
        let (root, repo, agent, _) = fixture("policy-escalate");
        let hooks = hooks_with_policy(
            &logging_oxa(&root),
            &agent,
            policy(json!({"kinds": {"review": ["a", "b"]}, "escalateTo": ["s"]})),
        );
        set_session(&hooks, "parent-1");
        for name in ["t1", "t2"] {
            let out = start_with(
                &hooks,
                &repo,
                json!({"name": name, "kind": "review", "job": "pr-9"}),
            );
            assert!(!out.is_error, "{}", out.text);
            let done = hooks.core.result(
                &json!({"action": "result", "name": name, "result": "failed", "reason": "review P1"}),
            );
            assert!(!done.is_error, "{}", done.text);
        }
        let third = start_with(
            &hooks,
            &repo,
            json!({"name": "t3", "kind": "review", "job": "pr-9"}),
        );
        assert!(!third.is_error, "{}", third.text);
        assert!(third.text.contains("model s (escalation"), "{}", third.text);
        assert!(third.text.contains("2 failed attempts on pr-9"), "{}", third.text);
        // Another job still gets the kind row, not the escalation.
        let other = start_with(
            &hooks,
            &repo,
            json!({"name": "t4", "kind": "review", "job": "pr-10"}),
        );
        assert!(other.text.contains("policy: kind review"), "{}", other.text);
        let models = launched_models(&root, 4);
        assert_eq!(models[2], "s");
        assert_ne!(models[3], "s");
        stop_all(&hooks, &["t1", "t2", "t3", "t4"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_model_wins_over_policy_and_is_recorded() {
        let (root, repo, agent, _) = fixture("policy-explicit");
        let hooks = hooks_with_policy(&logging_oxa(&root), &agent, policy(json!({"default": ["a"]})));
        set_session(&hooks, "parent-1");
        let out = start_with(
            &hooks,
            &repo,
            json!({"name": "x1", "model": "mine/m", "reason": "needs long context"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(launched_models(&root, 1), vec!["mine/m"]);
        let rec = registry::load(&agent, "parent-1").unwrap().remove(0);
        assert_eq!(
            (rec.choice.as_str(), rec.reason.as_str()),
            ("explicit", "needs long context")
        );
        stop_all(&hooks, &["x1"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_policy_keeps_the_session_model() {
        let (root, repo, agent, fake) = fixture("policy-none");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        let out = start_with(&hooks, &repo, json!({"name": "n1", "kind": "review"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("model model-x (session model)"), "{}", out.text);
        stop_all(&hooks, &["n1"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_kind_job_or_reason_is_refused_before_a_worktree() {
        let (root, repo, agent, fake) = fixture("policy-bad-args");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        for extra in [
            json!({"name": "b1", "kind": "Not Valid"}),
            json!({"name": "b1", "job": "has space"}),
            json!({"name": "b1", "reason": "two\nlines"}),
        ] {
            let out = start_with(&hooks, &repo, extra);
            assert!(out.is_error, "{}", out.text);
            assert!(!worktree_path(&repo, "b1").unwrap().exists());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn result_action_records_and_shows_the_reason() {
        let (root, repo, agent, fake) = fixture("result-action");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        start_alpha(&hooks, &repo);
        let ok = hooks
            .core
            .result(&json!({"action": "result", "name": "alpha", "result": "failed", "reason": "CI red"}));
        assert!(!ok.is_error, "{}", ok.text);
        assert!(hooks.core.list().text.contains("result failed: CI red"));
        // A second call replaces the first.
        hooks
            .core
            .result(&json!({"action": "result", "name": "alpha", "result": "ok", "reason": "fixed"}));
        assert!(hooks.core.list().text.contains("result ok: fixed"));
        let unknown = hooks
            .core
            .result(&json!({"action": "result", "name": "nobody", "result": "ok"}));
        assert!(unknown.is_error);
        let control = hooks
            .core
            .result(&json!({"action": "result", "name": "alpha", "result": "ok", "reason": "a\u{7}b"}));
        assert!(control.is_error);
        assert!(hooks.core.list().text.contains("result ok: fixed"));
        // Recording a result is no launch and needs no external access.
        let access = worker_access(&json!({"action": "result", "name": "alpha"}), &repo, false).unwrap();
        assert!(access.is_empty());
        stop_all(&hooks, &["alpha"]);
        let _ = fs::remove_dir_all(&root);
    }

    struct CatalogHost(bool);

    impl Host for CatalogHost {
        fn call(&self, action: HostAction) -> Result<Value, OxaError> {
            match action {
                HostAction::ModelInfo(model) => Ok(json!({"model": model, "remote": true, "known": self.0})),
                _ => Err(OxaError::new(oxa_proto::codes::INTERNAL, "not in the mock")),
            }
        }
    }

    #[test]
    fn unknown_model_warns_and_still_starts() {
        let (root, repo, agent, fake) = fixture("policy-unverified");
        let hooks = hooks(&fake, &agent);
        set_session(&hooks, "parent-1");
        *lock(&hooks.core.host) = Some(Arc::new(CatalogHost(false)));
        let out = start_with(&hooks, &repo, json!({"name": "u1", "model": "ghost/model"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("warning: ghost/model is not in this install"),
            "{}",
            out.text
        );
        assert_eq!(
            registry::load(&agent, "parent-1").unwrap()[0].verified,
            Some(false)
        );
        *lock(&hooks.core.host) = Some(Arc::new(CatalogHost(true)));
        let known = start_with(&hooks, &repo, json!({"name": "u2", "model": "real/model"}));
        assert!(!known.text.contains("warning"), "{}", known.text);
        assert_eq!(
            registry::load(&agent, "parent-1").unwrap()[1].verified,
            Some(true)
        );
        stop_all(&hooks, &["u1", "u2"]);
        let _ = fs::remove_dir_all(&root);
    }
}
