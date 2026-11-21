//! Worker registry: one JSON file per parent session under `<agent_dir>/workers/`, listing the
//! background workers that session started. Writes are atomic (temp file + rename, mode 0600).
//! A corrupt file is reported as an error and is never overwritten.
use crate::status::atomic;
use oxa_proto::OxaError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `.kanban` cannot be a session ID. A model tool running in a session named `kanban`
/// must not share the owner command's registry, work artifacts, or launch lease.
pub(super) const OWNER_PARENT: &str = ".kanban";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerScope {
    pub(super) repository: PathBuf,
    pub(super) git_device: u64,
    pub(super) git_inode: u64,
    pub(super) sessions_dir: PathBuf,
    pub(super) offline: bool,
}

/// A short write lease for independent owner launchers sharing one registry. Never
/// unlink its inode; a waiter may already have it open. Status readers use atomic snapshots.
pub(super) fn owner_lease(agent_dir: &Path) -> Result<File, OxaError> {
    let dir = agent_dir.join("workers");
    fs::create_dir_all(&dir).map_err(|e| io_err("cannot create worker registry directory", e))?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(dir.join(".kanban.lock"))
        .map_err(|e| io_err("cannot open owner worker lease", e))?;
    let m = file
        .metadata()
        .map_err(|e| io_err("cannot inspect owner worker lease", e))?;
    // SAFETY: geteuid has no preconditions.
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 || m.nlink() != 1 {
        return Err(OxaError::new(
            IO,
            "owner worker lease must be a private regular file",
        ));
    }
    file.lock()
        .map_err(|e| io_err("cannot lock owner worker registry", e))?;
    Ok(file)
}

pub const CORRUPT: &str = "worker_registry_corrupt";
const IO: &str = "worker_registry_io";
const BAD_PARENT: &str = "worker_registry_bad_parent";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub name: String,
    pub session_id: String,
    pub pid: u32,
    pub pgid: u32,
    pub worktree: PathBuf,
    pub branch: String,
    pub model: String,
    pub mode: String,
    pub goal_file: PathBuf,
    pub status_file: PathBuf,
    pub log_file: PathBuf,
    pub started_ms: u64,
    pub stopped: bool,
    /// The job kind the caller gave (`review`, `docs`, ...); empty when none.
    #[serde(default)]
    pub kind: String,
    /// The job this attempt belongs to; empty means the worker name.
    #[serde(default)]
    pub job: String,
    /// Why this model was chosen (`explicit`, `policy: kind review`, `escalation: ...`).
    #[serde(default)]
    pub choice: String,
    /// The caller's one-line reason for the model or kind.
    #[serde(default)]
    pub reason: String,
    /// Whether the model id was in the live catalog at start; `None` when it could not be asked.
    #[serde(default)]
    pub verified: Option<bool>,
    /// `ok` or `failed`, as recorded by the caller (`worker result`); empty until recorded.
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub result_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_scope: Option<OwnerScope>,
}

impl WorkerRecord {
    /// The job key: the recorded one, else the worker name.
    pub fn job_key(&self) -> &str {
        if self.job.is_empty() {
            &self.name
        } else {
            &self.job
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Working,
    Idle,
    Finished(String),
    Dead,
    Stopped,
}

fn io_err(what: &str, e: std::io::Error) -> OxaError {
    OxaError::new(IO, format!("{what}: {e}"))
}

fn file_path(agent_dir: &Path, parent_id: &str) -> Result<PathBuf, OxaError> {
    let valid = !parent_id.is_empty()
        && parent_id.len() <= 128
        && parent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid && parent_id != OWNER_PARENT {
        return Err(OxaError::new(
            BAD_PARENT,
            format!("invalid parent session id: {parent_id:?}"),
        ));
    }
    Ok(agent_dir.join("workers").join(format!("{parent_id}.json")))
}

/// The workers recorded for `parent_id`; no file means no workers.
pub fn load(agent_dir: &Path, parent_id: &str) -> Result<Vec<WorkerRecord>, OxaError> {
    let path = file_path(agent_dir, parent_id)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("cannot read worker registry", e)),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        OxaError::new(
            CORRUPT,
            format!("worker registry {} is corrupt: {e}", path.display()),
        )
    })
}

/// Replaces the workers recorded for `parent_id` atomically, with mode 0600. A registry that is
/// already corrupt is refused, never replaced: its bytes are the only record of those workers.
pub fn save(agent_dir: &Path, parent_id: &str, records: &[WorkerRecord]) -> Result<(), OxaError> {
    let path = file_path(agent_dir, parent_id)?;
    load(agent_dir, parent_id)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err("cannot create worker registry directory", e))?;
    }
    let bytes = serde_json::to_vec_pretty(records)
        .map_err(|e| OxaError::new(IO, format!("cannot encode worker registry: {e}")))?;
    atomic(&path, &bytes).map_err(|e| io_err("cannot write worker registry", e))
}

/// Whether a status snapshot was written by this worker: the same session and the same process.
/// The status writer emits both fields; a snapshot without them, or with others, is not ours.
pub(crate) fn names_worker(rec: &WorkerRecord, status: &Value) -> bool {
    status.get("session_id").and_then(Value::as_str) == Some(rec.session_id.as_str())
        && status.get("pid").and_then(Value::as_u64) == Some(u64::from(rec.pid))
}

/// The worker's state from its status.json (if any) and whether its process is alive.
/// A snapshot that does not name this worker is ignored. A finished outcome wins; then a stop
/// request; then liveness; then the reported phase.
pub fn state(rec: &WorkerRecord, status_json: Option<&Value>, alive: bool) -> WorkerState {
    let status_json = status_json.filter(|s| names_worker(rec, s));
    let phase = status_json.and_then(|s| s.get("phase")).and_then(Value::as_str);
    let outcome = status_json
        .and_then(|s| s.get("outcome"))
        .filter(|o| !o.is_null());
    if phase == Some("finished") || outcome.is_some() {
        let name = outcome
            .and_then(|o| o.get("outcome"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return WorkerState::Finished(name.to_string());
    }
    if rec.stopped {
        return WorkerState::Stopped;
    }
    if !alive {
        return WorkerState::Dead;
    }
    match phase {
        Some("idle") => WorkerState::Idle,
        // "model", "prefill" and "compaction" are the phases of an in-flight model request.
        Some("working" | "tool" | "closing" | "model" | "prefill" | "compaction") => WorkerState::Working,
        // No status yet, or a phase this build does not know: the worker is still starting up.
        _ => WorkerState::Starting,
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerState::*;
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn rec(name: &str) -> WorkerRecord {
        WorkerRecord {
            name: name.into(),
            session_id: format!("{name}-sid"),
            pid: 4242,
            pgid: 4242,
            worktree: "/w/tree".into(),
            branch: "agent/x".into(),
            model: "m".into(),
            mode: "goal".into(),
            goal_file: "/w/goal".into(),
            status_file: "/w/status.json".into(),
            log_file: "/w/log".into(),
            started_ms: 1_700_000_000_000,
            stopped: false,
            ..WorkerRecord::default()
        }
    }

    /// A status body as the status writer emits it for `rec`'s own worker (session and process).
    fn own(rec: &WorkerRecord, body: Value) -> Option<Value> {
        let mut status = body;
        status["session_id"] = json!(rec.session_id);
        status["pid"] = json!(rec.pid);
        Some(status)
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-workers-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn registry_round_trip_is_atomic_and_0600() {
        let dir = scratch("roundtrip");
        assert!(load(&dir, "parent-1").unwrap().is_empty());
        let records = vec![rec("a"), rec("b")];
        save(&dir, "parent-1", &records).unwrap();
        assert_eq!(load(&dir, "parent-1").unwrap(), records);
        let path = dir.join("workers").join("parent-1.json");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A second save replaces the file and leaves no temporary files behind.
        let mut more = records.clone();
        more[0].stopped = true;
        save(&dir, "parent-1", &more).unwrap();
        assert_eq!(load(&dir, "parent-1").unwrap(), more);
        let names: Vec<String> = fs::read_dir(dir.join("workers"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["parent-1.json".to_string()]);
        // A parent id that could escape the workers directory is refused.
        assert!(save(&dir, "../escape", &[]).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_from_status_phase_and_liveness() {
        let live = rec("live");
        let mut stopped = rec("stopped");
        stopped.stopped = true;
        let done = json!({"phase":"finished","outcome":{"type":"oxa_result","outcome":"done","error_class":null,"exit_code":0}});
        let failed = json!({"phase":"finished","outcome":{"type":"oxa_result","outcome":"failed","error_class":"x","exit_code":1}});
        let working = json!({"phase":"working","outcome":null});
        let cases: Vec<(&WorkerRecord, Option<Value>, bool, WorkerState)> = vec![
            (&live, None, true, Starting),
            (&live, None, false, Dead),
            (
                &live,
                own(&live, json!({"phase":"idle","outcome":null})),
                true,
                Idle,
            ),
            (&live, own(&live, working.clone()), true, Working),
            (
                &live,
                own(&live, json!({"phase":"tool","outcome":null})),
                true,
                Working,
            ),
            (
                &live,
                own(&live, json!({"phase":"closing","outcome":null})),
                true,
                Working,
            ),
            (&live, own(&live, working.clone()), false, Dead),
            (&live, own(&live, done.clone()), false, Finished("done".into())),
            (&live, own(&live, failed), true, Finished("failed".into())),
            (&stopped, own(&stopped, working), true, Stopped),
            (&stopped, None, false, Stopped),
            (&stopped, own(&stopped, done), false, Finished("done".into())),
        ];
        for (rec, status, alive, want) in cases {
            assert_eq!(
                state(rec, status.as_ref(), alive),
                want,
                "status={status:?} alive={alive} stopped={}",
                rec.stopped
            );
        }
    }

    #[test]
    fn state_ignores_status_from_another_worker() {
        let live = rec("live");
        let done = json!({"phase":"finished","outcome":{"type":"oxa_result","outcome":"done","error_class":null,"exit_code":0}});
        let mine = own(&live, done.clone());
        let mut other_session = own(&live, done.clone()).unwrap();
        other_session["session_id"] = json!("someone-else-sid");
        let mut other_pid = own(&live, done.clone()).unwrap();
        other_pid["pid"] = json!(7);
        let unnamed = Some(done);
        // The worker's own terminal snapshot is its outcome.
        assert_eq!(state(&live, mine.as_ref(), false), Finished("done".into()));
        // A stale or reused status path names another session or process: it says nothing here.
        assert_eq!(state(&live, Some(&other_session), false), Dead);
        assert_eq!(state(&live, Some(&other_session), true), Starting);
        assert_eq!(state(&live, Some(&other_pid), false), Dead);
        assert_eq!(state(&live, unnamed.as_ref(), false), Dead);
    }

    #[test]
    fn active_request_phases_are_working() {
        let live = rec("live");
        for phase in ["model", "prefill", "compaction"] {
            let status = own(&live, json!({"phase":phase,"outcome":null}));
            assert_eq!(state(&live, status.as_ref(), true), Working, "phase={phase}");
        }
    }

    #[test]
    fn corrupt_registry_is_reported_not_overwritten() {
        let dir = scratch("corrupt");
        fs::create_dir_all(dir.join("workers")).unwrap();
        let path = dir.join("workers").join("parent-2.json");
        let original: &[u8] = b"{\"not\": a registry";
        fs::write(&path, original).unwrap();
        let err = load(&dir, "parent-2").unwrap_err();
        assert_eq!(err.code, CORRUPT);
        // A save over the corrupt registry is refused too, with the bytes and directory untouched.
        let err = save(&dir, "parent-2", &[rec("a")]).unwrap_err();
        assert_eq!(err.code, CORRUPT);
        assert_eq!(fs::read(&path).unwrap(), original);
        let names: Vec<String> = fs::read_dir(dir.join("workers"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["parent-2.json".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_registry_file_from_before_the_model_fields_still_loads() {
        let old = r#"[{"name":"a","session_id":"s","pid":1,"pgid":1,"worktree":"/w","branch":"agent/a",
            "model":"m","mode":"goal","goal_file":"/g","status_file":"/s","log_file":"/l",
            "started_ms":5,"stopped":false}]"#;
        let records: Vec<WorkerRecord> = serde_json::from_str(old).unwrap();
        assert_eq!(records[0].job_key(), "a");
        assert_eq!(records[0].verified, None);
        assert!(records[0].result.is_empty());
    }
}
