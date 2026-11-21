//! A3: the session file store and the entry codec.
// ported from pi packages/coding-agent/src/core/session-manager.ts (file format: docs/session-format.md)
use crate::meta::meta_of_parts;
use crate::{SessionHeader, SessionMeta, SessionRef, SessionStore};
use oxa_proto::{
    codes, names, shown_why, Entry, EntryBody, FrozenPrefix, Mode, OxaError, Part, ProviderHistory, Role,
    ThinkingLevel, ToolCall, ToolMeta, Usage, WireImage, WireMsg,
};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Append the JSON line of `e` (without the trailing `\n`) to `out`.
///
/// Envelope: `{"type":T,"id":..,"parentId":..|null,"timestamp":"<ISO-8601 ms Z>", ..body}` with
/// pi's type names per variant (see `EntryBody` docs and README). A `Msg` entry stores pi's
/// `message` object for pi tooling and, for assistant messages, an extra `wire` object holding
/// `content`, `reasoning_content` and each tool call's `id`/`name`/`arguments` as the exact
/// strings streamed (W2). Key order is fixed; output is deterministic.
pub fn encode_entry(e: &Entry, out: &mut Vec<u8>) {
    encode_entry_inner(e, out, true);
}
fn encode_entry_inner(e: &Entry, out: &mut Vec<u8>, private: bool) {
    let (ty, mut fields) = body_fields(&e.body, e.ts_ms, private);
    if !private {
        let messages = match &e.body {
            EntryBody::Compaction { .. } => fields.get_mut("details").and_then(|v| v.get_mut("keptMessages")),
            EntryBody::Custom { custom_type, .. }
                if custom_type == crate::Transcript::EXTENSION_REBASE_ENTRY =>
            {
                fields.get_mut("data").and_then(|v| v.get_mut("messages"))
            }
            _ => None,
        };
        strip_nested_history(messages, false).expect("visible encoding does not parse private state");
        if matches!(&e.body, EntryBody::Custom { custom_type, .. } if custom_type == crate::GOAL_ENTRY) {
            if let Some(data) = fields.get_mut("data").and_then(Value::as_object_mut) {
                data.remove("lead");
            }
        }
    }
    let mut m = Map::new();
    m.insert("type".into(), Value::from(ty));
    m.insert("id".into(), Value::from(e.id.as_str()));
    m.insert(
        "parentId".into(),
        e.parent.as_deref().map_or(Value::Null, Value::from),
    );
    m.insert("timestamp".into(), Value::from(iso_from_ms(e.ts_ms)));
    m.extend(fields);
    write_json(&Value::Object(m), out);
}

/// Public entry representation for hooks, RPC and exports; private replay never leaves storage.
pub fn encode_visible_entry(e: &Entry, out: &mut Vec<u8>) {
    encode_entry_inner(e, out, false);
}

const REPLAY_INVALID: &str = "provider_history_invalid";
fn replay_invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(REPLAY_INVALID, message)
}

/// Parse one line. `decode_entry(encode_entry(x)) == x` for every `Entry`, bytewise on all
/// strings. Unknown `type` = `invalid` (the caller decides to skip). `Shown::why` is restored
/// with `oxa_proto::shown_why::intern`.
pub fn decode_entry(line: &[u8]) -> Result<Entry, OxaError> {
    let v: Value =
        serde_json::from_slice(line).map_err(|e| invalid(format!("malformed session line: {e}")))?;
    let m = v
        .as_object()
        .ok_or_else(|| invalid("session line is not a JSON object"))?;
    let ty = get_str(m, "type")?;
    let id = get_str(m, "id")?.to_string();
    let parent = opt_str(m, "parentId")?;
    let ts_ms = ms_from_iso(get_str(m, "timestamp")?).ok_or_else(|| invalid("bad entry timestamp"))?;
    let mut body = decode_body(ty, m)?;
    let messages = match &mut body {
        EntryBody::Compaction { details, .. } => details.get_mut("keptMessages"),
        EntryBody::Custom { custom_type, data }
            if custom_type == crate::Transcript::EXTENSION_REBASE_ENTRY =>
        {
            data.get_mut("messages")
        }
        _ => None,
    };
    strip_nested_history(messages, true)?;
    Ok(Entry {
        id,
        parent,
        ts_ms,
        body,
    })
}

// Rebase JSON is visible input, never a source of replay authority. Still reject a
// malformed present carrier explicitly rather than silently ignoring corrupt storage.
fn strip_nested_history(messages: Option<&mut Value>, validate: bool) -> Result<(), OxaError> {
    if let Some(messages) = messages.and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(map) = message.as_object_mut() {
                if let Some(value) = map.remove("provider_history") {
                    if validate {
                        ProviderHistory::from_session_value(&value)
                            .map_err(|e| replay_invalid(e.to_string()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The header line (`{"type":"session","version":3,"id":..,"timestamp":..,"cwd":..[,"parentSession":..]}`).
pub fn encode_header(h: &SessionHeader, out: &mut Vec<u8>) {
    let mut m = Map::new();
    m.insert("type".into(), "session".into());
    m.insert("version".into(), Value::from(h.version));
    m.insert("id".into(), Value::from(h.id.as_str()));
    m.insert("timestamp".into(), Value::from(iso_from_ms(h.ts_ms)));
    m.insert("cwd".into(), Value::from(h.cwd.to_string_lossy().into_owned()));
    if let Some(p) = &h.parent_session {
        m.insert("parentSession".into(), Value::from(p.as_str()));
    }
    write_json(&Value::Object(m), out);
}

pub fn decode_header(line: &[u8]) -> Result<SessionHeader, OxaError> {
    let v: Value =
        serde_json::from_slice(line).map_err(|e| invalid(format!("malformed session header: {e}")))?;
    let m = v
        .as_object()
        .ok_or_else(|| invalid("session header is not a JSON object"))?;
    if get_str(m, "type")? != "session" {
        return Err(invalid("first line of a session file is not a session header"));
    }
    Ok(SessionHeader {
        version: get_u32(m, "version")?,
        id: get_str(m, "id")?.to_string(),
        ts_ms: ms_from_iso(get_str(m, "timestamp")?).ok_or_else(|| invalid("bad header timestamp"))?,
        cwd: PathBuf::from(get_str(m, "cwd")?),
        parent_session: opt_str(m, "parentSession")?,
    })
}

/// pi's directory name for a cwd: `--` + the absolute path without its leading `/`, with `/`
/// replaced by `-`, + `--` (`/home/u/proj` -> `--home-u-proj--`).
pub fn session_dir_name(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let rest = s.strip_prefix('/').unwrap_or(&s);
    let mapped: String = rest
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    format!("--{mapped}--")
}

/// A new 8-hex entry id for which `taken` returns false. Uses the OS random source.
pub fn new_entry_id(taken: &dyn Fn(&str) -> bool) -> String {
    loop {
        let id = hex(&random_bytes(4));
        if !taken(&id) {
            return id;
        }
    }
}

/// The file store rooted at a sessions directory.
#[derive(Debug, Clone)]
pub struct FsStore {
    root: PathBuf,
    /// The session this process holds open for writing (shared by clones of the store).
    held: std::sync::Arc<std::sync::Mutex<Option<crate::SessionLock>>>,
    /// A lock taken by `acquire` that `commit_lock` has not yet made the held one.
    staged: std::sync::Arc<std::sync::Mutex<Option<crate::SessionLock>>>,
}

impl FsStore {
    /// `root` is created lazily (0700) on first `create`.
    pub fn new(root: PathBuf) -> FsStore {
        FsStore {
            root,
            held: Default::default(),
            staged: Default::default(),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SessionStore for FsStore {
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError> {
        let id = match id {
            Some(given) => {
                validate_session_id(given)?;
                // Only an id the caller chose can collide: a fresh random id needs no scan of
                // every stored session.
                if find_session_path(&self.root, given).is_some() {
                    return Err(invalid(format!("session id `{given}` already exists")));
                }
                given.to_string()
            }
            None => uuid_v4(),
        };
        let dir = self.root.join(session_dir_name(cwd));
        ensure_dir(&self.root)?;
        ensure_dir(&dir)?;
        if let Some(parent) = parent {
            crate::spill::link_spill_stores(&self.root, parent)?;
        }
        let ts_ms = now_ms();
        let path = dir.join(session_file_name(ts_ms, &id));
        let header = SessionHeader {
            version: names::SESSION_VERSION,
            id: id.clone(),
            ts_ms,
            cwd: cwd.to_path_buf(),
            parent_session: parent.map(|p| p.to_string_lossy().into_owned()),
        };
        let mut line = Vec::new();
        encode_header(&header, &mut line);
        // Only sessions born with store-link publication can prove that there are no
        // undiscoverable pre-upgrade cross-store children. Never backfill this flag.
        let mut value: Value = serde_json::from_slice(&line).expect("encoded header");
        value["spillStoresTracked"] = Value::Bool(true);
        line.clear();
        write_json(&value, &mut line);
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| open_err(&path, &e))?;
        f.write_all(&line)
            .map_err(|e| OxaError::io(&format!("cannot write session {}", path.display()), &e))?;
        Ok(SessionRef { path, id })
    }

    fn acquire(&self, s: &SessionRef) -> Result<(), OxaError> {
        check_safe(&s.path)?;
        let held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let staged = self.staged.lock().unwrap_or_else(|p| p.into_inner());
        if [held.as_ref(), staged.as_ref()]
            .into_iter()
            .flatten()
            .any(|l| l.session() == s.path)
        {
            return Ok(());
        }
        drop(staged);
        // The new lock is only staged: the held one stays until `commit_lock`.
        let new = crate::SessionLock::acquire(&s.path)?;
        *self.staged.lock().unwrap_or_else(|p| p.into_inner()) = Some(new);
        Ok(())
    }

    fn commit_lock(&self) {
        let staged = self.staged.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(new) = staged {
            *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(new);
        }
    }

    fn abort_lock(&self) {
        drop(self.staged.lock().unwrap_or_else(|p| p.into_inner()).take());
    }

    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError> {
        self.append_many(s, std::slice::from_ref(e))
    }

    fn append_many(&self, s: &SessionRef, entries: &[Entry]) -> Result<(), OxaError> {
        check_safe(&s.path)?;
        if entries.is_empty() {
            return Ok(());
        }
        if entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Compaction { .. }))
        {
            let mut prior = self.load(s)?;
            for entry in entries {
                crate::transcript::validate_append(&prior, entry)?;
                prior.push(entry.clone());
            }
        }
        let mut lines = Vec::new();
        for e in entries {
            encode_entry(e, &mut lines);
            lines.push(b'\n');
        }
        let mut f = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&s.path)
            .map_err(|err| open_err(&s.path, &err))?;
        // An advisory lock over the check-and-write below, so that no other appender is mid-write
        // while the last byte is read (a half-written line would look torn).
        // SAFETY: `flock` only reads the descriptor, which `f` keeps open for the whole call.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(OxaError::io(
                &format!("cannot lock {}", s.path.display()),
                &io::Error::last_os_error(),
            ));
        }
        let mut buf = Vec::with_capacity(lines.len() + 1);
        // A torn last line (a crash mid-write) has no newline. Start a new line so the new entry
        // is not glued to the fragment. The fragment is skipped on load, never repaired in place.
        let len = f
            .metadata()
            .map_err(|e| OxaError::io("cannot stat session file", &e))?
            .len();
        if len > 0 {
            f.seek(SeekFrom::End(-1))
                .map_err(|e| OxaError::io("cannot read session file", &e))?;
            let mut last = [0u8; 1];
            f.read_exact(&mut last)
                .map_err(|e| OxaError::io("cannot read session file", &e))?;
            if last[0] != b'\n' {
                buf.push(b'\n');
            }
        }
        buf.extend_from_slice(&lines);
        // One write on an O_APPEND descriptor: concurrent appends never interleave within a line.
        f.write_all(&buf)
            .map_err(|e| OxaError::io(&format!("cannot append to {}", s.path.display()), &e))
    }

    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError> {
        check_safe(&s.path)?;
        Ok(read_session(&s.path)?.1)
    }

    fn sync(&self, s: &SessionRef) -> Result<(), OxaError> {
        crate::durable::sync_file(&s.path)
    }

    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError> {
        check_safe(&s.path)?;
        read_header(&s.path)
    }

    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        for path in session_files(&self.root, cwd) {
            if let Ok((header, entries)) = read_session(&path) {
                let sref = SessionRef {
                    path: path.clone(),
                    id: header.id.clone(),
                };
                out.push(meta_of_parts(sref, &header, &entries));
            }
        }
        out
    }

    fn lists_path(&self, path: &Path) -> bool {
        // Match session_files' two-level layout, not a prefix (roots may be nested).
        path.parent().and_then(Path::parent) == Some(self.root.as_path())
            && path.extension() == Some(std::ffi::OsStr::new("jsonl"))
    }

    fn spill_cleanup_sessions(&self) -> Result<Vec<SessionRef>, OxaError> {
        let mut sessions = Vec::new();
        check_safe(&self.root)?;
        let entries =
            fs::read_dir(&self.root).map_err(|e| OxaError::io("cannot enumerate session store", &e))?;
        for entry in entries {
            let dir = entry
                .map_err(|e| OxaError::io("cannot enumerate session directory", &e))?
                .path();
            check_safe(&dir)?;
            let meta =
                fs::symlink_metadata(&dir).map_err(|e| OxaError::io("cannot stat session directory", &e))?;
            if !meta.is_dir() {
                continue;
            }
            let files = fs::read_dir(&dir).map_err(|e| OxaError::io("cannot enumerate session files", &e))?;
            for file in files {
                let path = file
                    .map_err(|e| OxaError::io("cannot enumerate session file", &e))?
                    .path();
                if path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
                    continue;
                }
                check_safe(&path)?;
                let header = read_header(&path)?;
                validate_session_id(&header.id)?;
                sessions.push(SessionRef { path, id: header.id });
            }
        }
        Ok(sessions)
    }

    fn spill_stores_tracked(&self, session: &SessionRef) -> Result<bool, OxaError> {
        check_safe(&session.path)?;
        let value: Value = serde_json::from_slice(&read_first_line(&session.path)?)
            .map_err(|e| invalid(format!("malformed session header: {e}")))?;
        Ok(value["spillStoresTracked"] == Value::Bool(true))
    }

    fn spill_lifecycle_lock(&self, session: &SessionRef) -> Result<crate::SpillLifecycleLock, OxaError> {
        crate::SpillLifecycleLock::publication(&self.root, &session.path)
    }

    fn spill_cleanup_lock(&self) -> Result<crate::SpillLifecycleLock, OxaError> {
        crate::SpillLifecycleLock::cleanup(&self.root)
    }

    fn spill_cleanup_roots(&self) -> Result<Vec<PathBuf>, OxaError> {
        crate::spill::linked_spill_stores(&self.root)
    }

    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError> {
        if id_or_path.is_empty() {
            return Err(invalid("empty session reference"));
        }
        if id_or_path.contains('/') || id_or_path.ends_with(".jsonl") {
            let path = Path::new(id_or_path);
            if fs::symlink_metadata(path).is_err() {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    format!("no session file {id_or_path}"),
                ));
            }
            check_safe(path)?;
            let header = read_header(path)?;
            return Ok(SessionRef {
                path: path.to_path_buf(),
                id: header.id,
            });
        }
        let found: Vec<(String, PathBuf)> = session_files(&self.root, None)
            .into_iter()
            .filter_map(|p| read_header(&p).ok().map(|h| (h.id, p)))
            .collect();
        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        let idx = match_id(id_or_path, &ids)?;
        let (id, path) = &found[idx];
        Ok(SessionRef {
            path: path.clone(),
            id: id.clone(),
        })
    }

    fn delete(&self, s: &SessionRef) -> Result<(), OxaError> {
        self.remove(s).map(|_| ())
    }

    fn remove(&self, s: &SessionRef) -> Result<crate::Removed, OxaError> {
        check_safe(&s.path)?;
        remove_session_file(&s.path)
    }
}

/// An in-memory store (`--no-session`, `Open::Ephemeral`, tests). Same semantics as
/// [`FsStore`] without files; `SessionRef::path` is `mem:<id>`.
#[derive(Debug, Default)]
pub struct MemStore {
    sessions: std::sync::Mutex<Vec<(SessionHeader, Vec<Entry>)>>,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Vec<(SessionHeader, Vec<Entry>)>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SessionStore for MemStore {
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError> {
        let id = match id {
            Some(given) => {
                validate_session_id(given)?;
                given.to_string()
            }
            None => uuid_v4(),
        };
        let mut sessions = self.guard();
        if sessions.iter().any(|(h, _)| h.id == id) {
            return Err(invalid(format!("session id `{id}` already exists")));
        }
        let header = SessionHeader {
            version: names::SESSION_VERSION,
            id: id.clone(),
            ts_ms: now_ms(),
            cwd: cwd.to_path_buf(),
            parent_session: parent.map(|p| p.to_string_lossy().into_owned()),
        };
        sessions.push((header, Vec::new()));
        Ok(SessionRef {
            path: PathBuf::from(format!("mem:{id}")),
            id,
        })
    }

    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError> {
        let mut sessions = self.guard();
        let (_, entries) = sessions
            .iter_mut()
            .find(|(h, _)| h.id == s.id)
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("no session {}", s.id)))?;
        crate::transcript::validate_append(entries, e)?;
        entries.push(e.clone());
        Ok(())
    }

    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError> {
        let sessions = self.guard();
        sessions
            .iter()
            .find(|(h, _)| h.id == s.id)
            .map(|(_, entries)| entries.clone())
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("no session {}", s.id)))
    }

    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError> {
        let sessions = self.guard();
        sessions
            .iter()
            .find(|(h, _)| h.id == s.id)
            .map(|(h, _)| h.clone())
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("no session {}", s.id)))
    }

    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta> {
        let sessions = self.guard();
        sessions
            .iter()
            .filter(|(h, _)| cwd.is_none_or(|c| h.cwd == c))
            .map(|(h, entries)| {
                let sref = SessionRef {
                    path: PathBuf::from(format!("mem:{}", h.id)),
                    id: h.id.clone(),
                };
                meta_of_parts(sref, h, entries)
            })
            .collect()
    }

    fn spill_cleanup_sessions(&self) -> Result<Vec<SessionRef>, OxaError> {
        Ok(self.list(None).into_iter().map(|meta| meta.sref).collect())
    }

    fn spill_stores_tracked(&self, _session: &SessionRef) -> Result<bool, OxaError> {
        // In-memory sessions cannot have persisted pre-upgrade descendants.
        Ok(true)
    }

    fn lists_path(&self, path: &Path) -> bool {
        path.to_str()
            .and_then(|s| s.strip_prefix("mem:"))
            .is_some_and(|id| validate_session_id(id).is_ok())
    }

    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError> {
        if id_or_path.is_empty() {
            return Err(invalid("empty session reference"));
        }
        let query = id_or_path.strip_prefix("mem:").unwrap_or(id_or_path);
        let sessions = self.guard();
        let ids: Vec<&str> = sessions.iter().map(|(h, _)| h.id.as_str()).collect();
        let idx = match_id(query, &ids)?;
        let id = ids[idx].to_string();
        Ok(SessionRef {
            path: PathBuf::from(format!("mem:{id}")),
            id,
        })
    }

    fn delete(&self, s: &SessionRef) -> Result<(), OxaError> {
        let mut sessions = self.guard();
        let idx = sessions
            .iter()
            .position(|(h, _)| h.id == s.id)
            .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("no session {}", s.id)))?;
        sessions.remove(idx);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Entry body codec
// ---------------------------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, message)
}

fn write_json(v: &Value, out: &mut Vec<u8>) {
    serde_json::to_writer(out, v).expect("writing JSON to a Vec cannot fail");
}

/// The entry type name and the body fields of one variant, in fixed order.
fn body_fields(body: &EntryBody, ts_ms: u64, private: bool) -> (&'static str, Map<String, Value>) {
    let mut f = Map::new();
    let ty = match body {
        EntryBody::Prefix(p) => {
            let mut sections = Map::new();
            for (name, text) in &p.sections {
                sections.insert(name.clone(), Value::from(text.as_str()));
            }
            let mut msg = Map::new();
            msg.insert("role".into(), "system".into());
            msg.insert("sections".into(), Value::Object(sections));
            msg.insert("toolsJson".into(), Value::from(p.tools_json.as_str()));
            msg.insert("sha256".into(), Value::from(hex(&p.sha256)));
            msg.insert("timestamp".into(), Value::from(ts_ms));
            f.insert("message".into(), Value::Object(msg));
            "message"
        }
        EntryBody::Msg { wire, parts, tool } => {
            let mut msg = Map::new();
            msg.insert("role".into(), pi_role(wire.role).into());
            msg.insert(
                "content".into(),
                Value::from(wire.content.as_deref().unwrap_or("")),
            );
            if let Some(id) = &wire.tool_call_id {
                msg.insert("toolCallId".into(), Value::from(id.as_str()));
            }
            msg.insert("timestamp".into(), Value::from(ts_ms));
            f.insert("message".into(), Value::Object(msg));
            f.insert("wire".into(), wire_value_inner(wire, private));
            f.insert(
                "parts".into(),
                Value::Array(parts.iter().map(part_value).collect()),
            );
            if let Some(t) = tool {
                msg_tool_fields(&mut f, t);
            }
            "message"
        }
        EntryBody::Shown { text, why } => {
            f.insert("text".into(), Value::from(text.as_str()));
            f.insert("why".into(), Value::from(*why));
            "shown"
        }
        EntryBody::BashExecution {
            command,
            output,
            exit,
            excluded,
        } => {
            let mut msg = Map::new();
            msg.insert("role".into(), "bashExecution".into());
            msg.insert("command".into(), Value::from(command.as_str()));
            msg.insert("output".into(), Value::from(output.as_str()));
            msg.insert("exitCode".into(), exit.map_or(Value::Null, Value::from));
            // pi's required `cancelled` / `truncated`, derived as the RPC `bash` response does:
            // no exit code = cancelled; the stored output ends with user_bash's notice.
            msg.insert("cancelled".into(), Value::from(exit.is_none()));
            msg.insert("truncated".into(), Value::from(bash_truncated(output)));
            msg.insert("excludeFromContext".into(), Value::from(*excluded));
            msg.insert("timestamp".into(), Value::from(ts_ms));
            f.insert("message".into(), Value::Object(msg));
            "message"
        }
        EntryBody::ModelChange(model) => {
            // pi's `provider`: oxa has one provider type, the engine (`oxe`, as in RPC and events).
            f.insert("provider".into(), Value::from("oxe"));
            f.insert("modelId".into(), Value::from(model.as_str()));
            "model_change"
        }
        EntryBody::ThinkingLevelChange(level) => {
            f.insert("thinkingLevel".into(), Value::from(level.as_str()));
            "thinking_level_change"
        }
        EntryBody::ModeChange(mode) => {
            f.insert("mode".into(), Value::from(mode.as_str()));
            "mode_change"
        }
        EntryBody::Usage { kind, usage } => {
            let mut u = Map::new();
            if usage.unreported {
                u.insert("unreported".into(), Value::Bool(true));
            } else {
                u.insert("input".into(), Value::from(usage.prompt));
                u.insert("output".into(), Value::from(usage.completion));
                // Not reported is not a miss: leave `cacheRead` out (decoded as `cached_missing`).
                if !usage.cached_missing {
                    u.insert("cacheRead".into(), Value::from(usage.cached));
                }
                u.insert("cacheWrite".into(), Value::from(0u32));
                u.insert(
                    "totalTokens".into(),
                    Value::from(u64::from(usage.prompt) + u64::from(usage.completion)),
                );
            }
            f.insert("kind".into(), Value::from(kind.as_str()));
            f.insert("usage".into(), Value::Object(u));
            "usage"
        }
        EntryBody::Compaction {
            summary,
            first_kept,
            tokens_before,
            details,
            from_hook,
        } => {
            f.insert("summary".into(), Value::from(summary.as_str()));
            f.insert("firstKeptEntryId".into(), Value::from(first_kept.as_str()));
            f.insert("tokensBefore".into(), Value::from(*tokens_before));
            f.insert("details".into(), details.clone());
            f.insert("fromHook".into(), Value::from(*from_hook));
            "compaction"
        }
        EntryBody::ContextEdit { target, replacement } => {
            f.insert("targetId".into(), Value::from(target.as_str()));
            f.insert("replacement".into(), opt_value(replacement.as_deref()));
            "context_edit"
        }
        EntryBody::BranchSummary {
            from,
            summary,
            details,
            from_hook,
        } => {
            f.insert("fromId".into(), Value::from(from.as_str()));
            f.insert("summary".into(), Value::from(summary.as_str()));
            f.insert("details".into(), details.clone());
            f.insert("fromHook".into(), Value::from(*from_hook));
            "branch_summary"
        }
        EntryBody::Custom { custom_type, data } => {
            f.insert("customType".into(), Value::from(custom_type.as_str()));
            f.insert("data".into(), data.clone());
            "custom"
        }
        EntryBody::CustomMessage {
            custom_type,
            content,
            display,
            details,
        } => {
            f.insert("customType".into(), Value::from(custom_type.as_str()));
            f.insert("content".into(), Value::from(content.as_str()));
            f.insert("display".into(), Value::from(*display));
            f.insert("details".into(), details.clone());
            "custom_message"
        }
        EntryBody::Label { target, label } => {
            f.insert("targetId".into(), Value::from(target.as_str()));
            f.insert("label".into(), opt_value(label.as_deref()));
            "label"
        }
        EntryBody::SessionInfo { name } => {
            f.insert("name".into(), Value::from(name.as_str()));
            "session_info"
        }
    };
    (ty, f)
}

/// Whether a stored `!cmd` output ends with the truncation notice `user_bash` appends
/// (`\n\n[Output truncated. Full output: <path>]`).
fn bash_truncated(output: &str) -> bool {
    output.contains("\n\n[Output truncated") && output.ends_with(']')
}

fn pi_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "toolResult",
    }
}

fn opt_value(s: Option<&str>) -> Value {
    s.map_or(Value::Null, Value::from)
}

/// The exact wire message (W2): role, content, reasoning_content and tool calls as stored.
pub(crate) fn wire_value(w: &WireMsg) -> Value {
    wire_value_inner(w, false)
}
fn wire_value_inner(w: &WireMsg, private: bool) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), Value::from(w.role.as_str()));
    m.insert("content".into(), opt_value(w.content.as_deref()));
    if let Some(r) = &w.reasoning {
        m.insert("reasoning_content".into(), Value::from(r.as_str()));
    }
    let calls: Vec<Value> = w
        .tool_calls
        .iter()
        .map(|t| {
            let mut c = Map::new();
            c.insert("id".into(), Value::from(t.id.as_str()));
            c.insert("name".into(), Value::from(t.name.as_str()));
            c.insert("arguments".into(), Value::from(t.args_raw.as_str()));
            Value::Object(c)
        })
        .collect();
    m.insert("tool_calls".into(), Value::Array(calls));
    if let Some(history) = w.provider_history.as_ref().filter(|_| private) {
        m.insert("provider_history".into(), history.to_session_value());
    }
    if let Some(id) = &w.tool_call_id {
        m.insert("tool_call_id".into(), Value::from(id.as_str()));
    }
    if !w.images.is_empty() {
        // B2a: written only when present, so a message without images keeps its old bytes.
        let images: Vec<Value> = w
            .images
            .iter()
            .map(|i| {
                let mut o = Map::new();
                o.insert("mime".into(), Value::from(i.mime.as_str()));
                o.insert("base64".into(), Value::from(i.base64.as_str()));
                Value::Object(o)
            })
            .collect();
        m.insert("images".into(), Value::Array(images));
    }
    Value::Object(m)
}

fn part_value(p: &Part) -> Value {
    let mut m = Map::new();
    match p {
        Part::Text(text) => {
            m.insert("type".into(), "text".into());
            m.insert("text".into(), Value::from(text.as_str()));
        }
        Part::EnvNote(note) => {
            m.insert("type".into(), "envNote".into());
            m.insert("text".into(), Value::from(note.as_str()));
        }
        Part::Image {
            path,
            mime,
            sha256,
            w,
            h,
        } => {
            m.insert("type".into(), "image".into());
            m.insert("path".into(), Value::from(path.to_string_lossy().into_owned()));
            m.insert("mime".into(), Value::from(mime.as_str()));
            m.insert("sha256".into(), Value::from(hex(sha256)));
            m.insert("w".into(), Value::from(*w));
            m.insert("h".into(), Value::from(*h));
        }
    }
    Value::Object(m)
}

fn decode_body(ty: &str, m: &Map<String, Value>) -> Result<EntryBody, OxaError> {
    Ok(match ty {
        "message" => decode_message(m)?,
        "shown" => EntryBody::Shown {
            text: get_str(m, "text")?.to_string(),
            why: shown_why::intern(get_str(m, "why")?),
        },
        "model_change" => EntryBody::ModelChange(get_str(m, "modelId")?.to_string()),
        "thinking_level_change" => {
            let s = get_str(m, "thinkingLevel")?;
            EntryBody::ThinkingLevelChange(
                ThinkingLevel::parse(s).ok_or_else(|| invalid(format!("unknown thinking level `{s}`")))?,
            )
        }
        "mode_change" => {
            let s = get_str(m, "mode")?;
            EntryBody::ModeChange(Mode::parse(s).ok_or_else(|| invalid(format!("unknown mode `{s}`")))?)
        }
        "usage" => EntryBody::Usage {
            kind: get_str(m, "kind")?.to_string(),
            usage: decode_usage(get_obj(m, "usage")?),
        },
        "compaction" => EntryBody::Compaction {
            summary: get_str(m, "summary")?.to_string(),
            first_kept: get_str(m, "firstKeptEntryId")?.to_string(),
            tokens_before: get_u32(m, "tokensBefore")?,
            details: get_value(m, "details"),
            from_hook: get_flag(m, "fromHook"),
        },
        "context_edit" => EntryBody::ContextEdit {
            target: get_str(m, "targetId")?.to_string(),
            replacement: opt_str(m, "replacement")?,
        },
        "branch_summary" => EntryBody::BranchSummary {
            from: get_str(m, "fromId")?.to_string(),
            summary: get_str(m, "summary")?.to_string(),
            details: get_value(m, "details"),
            from_hook: get_flag(m, "fromHook"),
        },
        "custom" => EntryBody::Custom {
            custom_type: get_str(m, "customType")?.to_string(),
            data: get_value(m, "data"),
        },
        "custom_message" => EntryBody::CustomMessage {
            custom_type: get_str(m, "customType")?.to_string(),
            content: get_str(m, "content")?.to_string(),
            display: get_flag(m, "display"),
            details: get_value(m, "details"),
        },
        "label" => EntryBody::Label {
            target: get_str(m, "targetId")?.to_string(),
            label: opt_str(m, "label")?,
        },
        "session_info" => EntryBody::SessionInfo {
            name: get_str(m, "name")?.to_string(),
        },
        other => return Err(invalid(format!("unknown entry type `{other}`"))),
    })
}

fn decode_message(m: &Map<String, Value>) -> Result<EntryBody, OxaError> {
    let msg = get_obj(m, "message")?;
    let role = get_str(msg, "role")?;
    match role {
        "bashExecution" => Ok(EntryBody::BashExecution {
            command: get_str(msg, "command")?.to_string(),
            output: get_str(msg, "output")?.to_string(),
            exit: match msg.get("exitCode") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .ok_or_else(|| invalid("exit code out of range"))?,
                ),
            },
            excluded: get_flag(msg, "excludeFromContext"),
        }),
        "system" if msg.contains_key("sections") => Ok(EntryBody::Prefix(decode_prefix(msg)?)),
        "system" | "user" | "assistant" | "toolResult" => Ok(EntryBody::Msg {
            wire: decode_wire(m, msg, role)?,
            parts: decode_parts(m)?,
            tool: decode_tool(m),
        }),
        other => Err(invalid(format!("unknown message role `{other}`"))),
    }
}

fn decode_prefix(msg: &Map<String, Value>) -> Result<FrozenPrefix, OxaError> {
    let sections = get_obj(msg, "sections")?
        .iter()
        .map(|(name, text)| {
            text.as_str()
                .map(|t| (name.clone(), t.to_string()))
                .ok_or_else(|| invalid("prompt section is not a string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FrozenPrefix {
        sections,
        tools_json: get_str(msg, "toolsJson")?.to_string(),
        sha256: unhex32(get_str(msg, "sha256")?)?,
    })
}

fn decode_wire(
    m: &Map<String, Value>,
    msg: &Map<String, Value>,
    role_str: &str,
) -> Result<WireMsg, OxaError> {
    if let Some(w) = m.get("wire") {
        return decode_wire_value(w);
    }
    // A pi message without oxa's `wire` object (pi tooling, imports): derive the wire from pi's fields.
    decode_pi_wire(msg, role_str)
}

/// The inverse of [`wire_value`] (W2: every field as stored).
pub(crate) fn decode_wire_value(w: &Value) -> Result<WireMsg, OxaError> {
    let result = (|| {
        let w = w.as_object().ok_or_else(|| invalid("wire is not an object"))?;
        let role_name = get_str(w, "role")?;
        let role =
            Role::parse(role_name).ok_or_else(|| invalid(format!("unknown wire role `{role_name}`")))?;
        let tool_calls = match w.get("tool_calls") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(calls)) => calls
                .iter()
                .map(decode_tool_call)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(invalid("tool_calls is not an array")),
        };
        if role != Role::Assistant && w.contains_key("provider_history") {
            return Err(replay_invalid("Provider history requires an assistant message."));
        }
        Ok(WireMsg {
            role,
            content: opt_str(w, "content")?,
            reasoning: opt_str(w, "reasoning_content")?,
            tool_calls,
            provider_history: w
                .get("provider_history")
                .map(ProviderHistory::from_session_value)
                .transpose()
                .map_err(|e| replay_invalid(e.to_string()))?,
            tool_call_id: opt_str(w, "tool_call_id")?,
            images: decode_images(w.get("images"))?,
        })
    })();
    result.map_err(|error: OxaError| {
        if w.get("provider_history").is_some() && error.code != REPLAY_INVALID {
            replay_invalid("Malformed message containing provider history.")
        } else {
            error
        }
    })
}

fn decode_pi_wire(msg: &Map<String, Value>, role_str: &str) -> Result<WireMsg, OxaError> {
    let role = match role_str {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        _ => Role::System,
    };
    let (text, calls) = pi_content(msg.get("content").unwrap_or(&Value::Null));
    Ok(WireMsg {
        role,
        content: Some(text),
        reasoning: None,
        tool_calls: if role == Role::Assistant {
            calls
        } else {
            Vec::new()
        },
        provider_history: None,
        tool_call_id: if role == Role::Tool {
            msg.get("toolCallId").and_then(Value::as_str).map(String::from)
        } else {
            None
        },
        images: Vec::new(),
    })
}

/// The `images` of a stored wire message (B2a). Absent in every session written before images
/// existed: that is an empty list, not an error.
fn decode_images(v: Option<&Value>) -> Result<Vec<WireImage>, OxaError> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let o = item
                    .as_object()
                    .ok_or_else(|| invalid("image is not an object"))?;
                out.push(WireImage {
                    mime: get_str(o, "mime")?.to_string(),
                    base64: get_str(o, "base64")?.to_string(),
                });
            }
            Ok(out)
        }
        Some(_) => Err(invalid("images is not an array")),
    }
}

/// Text of a pi content value (a string or content blocks) and its tool calls.
fn pi_content(v: &Value) -> (String, Vec<ToolCall>) {
    match v {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(blocks) => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("toolCall") => calls.push(ToolCall {
                        id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: b.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        args_raw: match b.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        },
                    }),
                    _ => {}
                }
            }
            (text, calls)
        }
        _ => (String::new(), Vec::new()),
    }
}

fn decode_tool_call(v: &Value) -> Result<ToolCall, OxaError> {
    let o = v
        .as_object()
        .ok_or_else(|| invalid("tool call is not an object"))?;
    Ok(ToolCall {
        id: get_str(o, "id")?.to_string(),
        name: get_str(o, "name")?.to_string(),
        args_raw: get_str(o, "arguments")?.to_string(),
    })
}

fn msg_tool_fields(f: &mut Map<String, Value>, t: &ToolMeta) {
    f.insert("toolError".into(), Value::Bool(t.is_error));
    if !t.ui.is_null() {
        f.insert("toolUi".into(), t.ui.clone());
    }
}

fn decode_tool(m: &Map<String, Value>) -> Option<ToolMeta> {
    let is_error = m.get("toolError").and_then(Value::as_bool);
    let ui = m.get("toolUi").cloned();
    if is_error.is_none() && ui.is_none() {
        return None;
    }
    Some(ToolMeta {
        is_error: is_error.unwrap_or(false),
        ui: ui.unwrap_or(Value::Null),
    })
}

fn decode_parts(m: &Map<String, Value>) -> Result<Vec<Part>, OxaError> {
    match m.get("parts") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(parts)) => parts.iter().map(decode_part).collect(),
        Some(_) => Err(invalid("parts is not an array")),
    }
}

fn decode_part(v: &Value) -> Result<Part, OxaError> {
    let o = v.as_object().ok_or_else(|| invalid("part is not an object"))?;
    match get_str(o, "type")? {
        "text" => Ok(Part::Text(get_str(o, "text")?.to_string())),
        "envNote" => Ok(Part::EnvNote(get_str(o, "text")?.to_string())),
        "image" => Ok(Part::Image {
            path: PathBuf::from(get_str(o, "path")?),
            mime: get_str(o, "mime")?.to_string(),
            sha256: unhex32(get_str(o, "sha256")?)?,
            w: get_u32(o, "w")?,
            h: get_u32(o, "h")?,
        }),
        other => Err(invalid(format!("unknown part type `{other}`"))),
    }
}

/// pi's usage object; `cost` and `totalTokens` are derived, not stored separately.
fn decode_usage(o: &Map<String, Value>) -> Usage {
    if o.get("unreported").and_then(Value::as_bool) == Some(true) {
        return Usage::unreported();
    }
    let num = |k: &str| {
        o.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    Usage {
        prompt: num("input"),
        completion: num("output"),
        cached: num("cacheRead"),
        cached_missing: o.get("cacheRead").and_then(Value::as_u64).is_none(),
        unreported: false,
    }
}

fn get_str<'a>(m: &'a Map<String, Value>, key: &str) -> Result<&'a str, OxaError> {
    m.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing string field `{key}`")))
}

fn opt_str(m: &Map<String, Value>, key: &str) -> Result<Option<String>, OxaError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("field `{key}` is not a string"))),
    }
}

fn get_u32(m: &Map<String, Value>, key: &str) -> Result<u32, OxaError> {
    m.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid(format!("missing integer field `{key}`")))
}

fn get_flag(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn get_value(m: &Map<String, Value>, key: &str) -> Value {
    m.get(key).cloned().unwrap_or(Value::Null)
}

fn get_obj<'a>(m: &'a Map<String, Value>, key: &str) -> Result<&'a Map<String, Value>, OxaError> {
    m.get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(format!("missing object field `{key}`")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Result<[u8; 32], OxaError> {
    if s.len() != 64 || !s.is_ascii() {
        return Err(invalid("sha256 must be 64 hex characters"));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| invalid("sha256 is not hex"))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Timestamps (ISO-8601, milliseconds, UTC)
// ---------------------------------------------------------------------------------------------

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form pi writes.
pub fn iso_from_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        mo,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

/// The inverse of [`iso_from_ms`]. Only the canonical form is accepted.
fn ms_from_iso(s: &str) -> Option<u64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let mut dp = date.splitn(3, '-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    let (hms, frac) = time.split_once('.')?;
    let mut tp = hms.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let mi: u64 = tp.next()?.parse().ok()?;
    let se: u64 = tp.next()?.parse().ok()?;
    let millis: u64 = frac.parse().ok()?;
    if tp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if h > 23 || mi > 59 || se > 59 || millis > 999 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    if days < 0 {
        return None;
    }
    let total = days as u64 * 86_400_000 + h * 3_600_000 + mi * 60_000 + se * 1000 + millis;
    // Reject anything that would not be written back in the same form (sign, padding, ...).
    if iso_from_ms(total) == s {
        Some(total)
    } else {
        None
    }
}

/// Parse a whole-second UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ` form.
pub fn parse_utc_seconds(s: &str) -> Option<u64> {
    let stem = s.strip_suffix('Z')?;
    if stem.len() != 19 || !stem.as_bytes()[..4].iter().all(u8::is_ascii_digit) || stem.as_bytes()[4] != b'-'
    {
        return None;
    }
    ms_from_iso(&format!("{stem}.000Z")).map(|ms| ms / 1000)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Howard Hinnant's `days_from_civil`: (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Ids and randomness
// ---------------------------------------------------------------------------------------------

static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `n` bytes from `/dev/urandom`. If that is unreadable, a splitmix stream seeded from the clock
/// (ids only, not secrets).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return buf;
    }
    let seed = now_ms()
        ^ FALLBACK_COUNTER
            .fetch_add(0x9E37_79B9, Ordering::SeqCst)
            .rotate_left(17);
    let mut state = seed;
    for b in buf.iter_mut() {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *b = (z ^ (z >> 31)) as u8;
    }
    buf
}

fn uuid_v4() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Session ids become file names and are matched by prefix: no separators, no control or
/// whitespace characters, no leading dot.
pub(crate) fn validate_session_id(id: &str) -> Result<(), OxaError> {
    let bad = id.is_empty()
        || id.len() > 128
        || id.starts_with('.')
        || id
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control() || c.is_whitespace());
    if bad {
        return Err(invalid(format!("invalid session id `{id}`"))
            .with_hint("use letters, digits, `-` or `_`; no path separators"));
    }
    Ok(())
}

/// `2024-12-03T14-00-00-000Z_<id>.jsonl` (pi's file name: the ISO stamp with `:` and `.` as `-`).
fn session_file_name(ts_ms: u64, id: &str) -> String {
    let stamp: String = iso_from_ms(ts_ms)
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    format!("{stamp}_{id}.jsonl")
}

// ---------------------------------------------------------------------------------------------
// File helpers (FsStore)
// ---------------------------------------------------------------------------------------------

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn unsafe_path(path: &Path) -> OxaError {
    OxaError::new(
        codes::UNSAFE_PATH,
        format!("refusing symlinked session path {}", path.display()),
    )
    .with_hint("session files and their directories must be real files and directories, not symlinks")
}

/// Refuse a session file that is a symlink, or whose directory is one.
pub(crate) fn check_safe(path: &Path) -> Result<(), OxaError> {
    if path.parent().is_some_and(is_symlink) || is_symlink(path) {
        return Err(unsafe_path(path));
    }
    Ok(())
}

/// Create a directory (recursively) with mode 0700. A symlink is refused.
pub(crate) fn ensure_dir(p: &Path) -> Result<(), OxaError> {
    if is_symlink(p) {
        return Err(unsafe_path(p));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .map_err(|e| {
            OxaError::io(&format!("cannot create session directory {}", p.display()), &e)
                .with_hint("to keep oxa's state elsewhere, set OXA_AGENT_DIR (or OXA_SESSION_DIR, or pass --session-dir) to a writable directory")
        })
}

fn open_err(path: &Path, e: &io::Error) -> OxaError {
    if e.raw_os_error() == Some(libc::ELOOP) {
        return unsafe_path(path);
    }
    if e.kind() == io::ErrorKind::NotFound {
        return OxaError::new(codes::NOT_FOUND, format!("no session file {}", path.display()));
    }
    OxaError::io(&format!("cannot open session file {}", path.display()), e)
}

fn open_read(path: &Path, extra_flags: libc::c_int) -> Result<File, OxaError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | extra_flags)
        .open(path)
        .map_err(|e| open_err(path, &e))
}

pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>, OxaError> {
    let mut f = open_read(path, 0)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))?;
    Ok(bytes)
}

/// Header and diagnostic reads must not block opening a FIFO passed as a session path.
pub(crate) fn open_regular_session(path: &Path) -> Result<File, OxaError> {
    check_safe(path)?;
    let file = open_read(path, libc::O_NONBLOCK)?;
    if !file.metadata().map_err(|e| open_err(path, &e))?.is_file() {
        return Err(invalid("session path must be a regular file"));
    }
    Ok(file)
}

fn read_first_line(path: &Path) -> Result<Vec<u8>, OxaError> {
    let f = open_read(path, 0)?;
    let mut line = Vec::new();
    BufReader::new(f)
        .read_until(b'\n', &mut line)
        .map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e))?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}

fn read_header(path: &Path) -> Result<SessionHeader, OxaError> {
    decode_header(&read_first_line(path)?)
}

/// The header and the entries of a session file. A torn last line (no trailing newline) is
/// dropped; complete lines that do not decode (a crash fragment that a later append
/// separated with a newline, an unknown entry type) are skipped. The file is never repaired.
fn read_session(path: &Path) -> Result<(SessionHeader, Vec<Entry>), OxaError> {
    let bytes = read_file(path)?;
    let pieces: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    // The last piece has no newline after it: a torn line, or empty when the file ends in `\n`.
    let complete = &pieces[..pieces.len() - 1];
    let first = complete
        .first()
        .ok_or_else(|| invalid(format!("{} has no header line", path.display())))?;
    let header = decode_header(first)?;
    let mut entries = Vec::new();
    for line in &complete[1..] {
        match decode_entry(line) {
            Ok(entry) => {
                crate::transcript::validate_append(&entries, &entry)?;
                entries.push(entry);
            }
            Err(error) if error.code == REPLAY_INVALID => return Err(error),
            Err(_) => {} // Preserve crash-fragment/unknown-entry recovery.
        }
    }
    Ok((header, entries))
}

/// Every session file below `root` (of `cwd`'s directory only, when given), sorted by path.
/// Symlinked directories and files are not listed.
fn session_files(root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    visit_session_files(root, cwd, usize::MAX, |p| out.push(p));
    out.sort();
    out
}

/// Visit regular session files without buffering directory contents. Count every directory
/// entry inspected, not just sessions. Return false if the walk reaches its budget.
pub(crate) fn visit_session_files(
    root: &Path,
    cwd: Option<&Path>,
    mut remaining: usize,
    mut visit: impl FnMut(PathBuf),
) -> bool {
    let project = cwd.map(|c| root.join(session_dir_name(c)));
    let dirs = project.into_iter().map(Some).chain(
        cwd.is_none()
            .then(|| fs::read_dir(root).ok())
            .flatten()
            .into_iter()
            .flatten()
            .map(|e| e.ok().map(|e| e.path())),
    );
    for dir in dirs {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        let Some(dir) = dir else { continue };
        if is_symlink(&dir) || !dir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            let Ok(entry) = entry else { continue };
            let p = entry.path();
            if p.extension().is_some_and(|x| x == "jsonl") && entry.file_type().is_ok_and(|t| t.is_file()) {
                visit(p);
            }
        }
    }
    true
}

/// The path of the session file with this id, if one exists under `root`.
fn find_session_path(root: &Path, id: &str) -> Option<PathBuf> {
    session_files(root, None)
        .into_iter()
        .find(|p| read_header(p).map(|h| h.id == id).unwrap_or(false))
}

/// Remove a session through the `trash` command when it is available, else unlink (pi's behaviour).
/// pi `deleteSessionFile`: the `trash` command first (its output captured, never on the
/// terminal; `--` before a path that starts with `-`); a success, or the file being gone
/// afterwards, counts as trashed. Otherwise unlink.
fn remove_session_file(path: &Path) -> Result<crate::Removed, OxaError> {
    remove_status_sibling(path)?;
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut cmd = std::process::Command::new("trash");
    if absolute.to_string_lossy().starts_with('-') {
        cmd.arg("--");
    }
    let ran = cmd
        .arg(&absolute)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Ok(status) = ran {
        if status.success() || !path.exists() {
            return Ok(crate::Removed::Trashed);
        }
    }
    fs::remove_file(path)
        .map(|()| crate::Removed::Deleted)
        .map_err(|e| OxaError::io(&format!("cannot delete session {}", path.display()), &e))
}

/// P6: every session file under `root` (the cwd directories, as `list` reads them), with its
/// header when the header reads. A file without a readable header is listed with `None`.
/// Sorted by path. Symlinks are not listed.
pub fn session_files_under(root: &Path) -> Vec<(PathBuf, Option<SessionHeader>)> {
    session_files(root, None)
        .into_iter()
        .map(|p| {
            let header = read_header(&p).ok();
            (p, header)
        })
        .collect()
}

/// P6: unlink a session file now. Unlike [`SessionStore::delete`] this never uses `trash`: a
/// wipe must not leave the content in the trash. A symlinked path is refused; a missing file is
/// `Ok` (the wipe is idempotent).
pub fn purge_session_file(path: &Path) -> Result<(), OxaError> {
    check_safe(path)?;
    remove_status_sibling(path)?;
    let _ = fs::remove_file(crate::lock_path_of(path));
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxaError::io(
            &format!("cannot delete session {}", path.display()),
            &e,
        )),
    }
}

/// Only the status artifact belonging to this session. Unlink never follows a final symlink;
/// a directory or other removal error is reported rather than broadening the deletion.
fn remove_status_sibling(session: &Path) -> Result<(), OxaError> {
    match fs::remove_file(session.with_extension("status.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OxaError::io("cannot delete session status", &e)),
    }
}

/// P6: the spill directory of session `id`, `<tmp_dir>/<id>` (see `session.rs`: `attach_ids`).
/// Removes it with everything in it and returns the number of files removed; a missing
/// directory is `Ok(0)`. The id must be a valid session id (no path separators), and a symlink
/// is refused.
pub fn remove_spill_dir(tmp_dir: &Path, id: &str) -> Result<usize, OxaError> {
    validate_session_id(id)?;
    remove_tree(&tmp_dir.join(id))
}

/// P6: the whole spill directory `tmp_dir` (every session's spill files and the codemode
/// scratch files). Returns the number of files removed; missing = `Ok(0)`; a symlink is refused.
pub fn remove_all_spill(tmp_dir: &Path) -> Result<usize, OxaError> {
    remove_tree(tmp_dir)
}

/// Count and remove a directory tree. Refuses a symlink at the top.
fn remove_tree(dir: &Path) -> Result<usize, OxaError> {
    if is_symlink(dir) {
        return Err(unsafe_path(dir));
    }
    if !dir.exists() {
        return Ok(0);
    }
    let n = count_files(dir);
    fs::remove_dir_all(dir).map_err(|e| OxaError::io(&format!("cannot remove {}", dir.display()), &e))?;
    Ok(n)
}

/// Regular files below `dir` (symlinks are not followed and not counted).
pub fn count_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            match fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() => count_files(&p),
                Ok(m) if m.is_file() => 1,
                _ => 0,
            }
        })
        .sum()
}

/// Pick the id that `query` names: an exact match, else the only id it is a prefix of.
fn match_id(query: &str, ids: &[&str]) -> Result<usize, OxaError> {
    if query.is_empty() {
        return Err(invalid("empty session id"));
    }
    if let Some(i) = ids.iter().position(|id| *id == query) {
        return Ok(i);
    }
    let hits: Vec<usize> = ids
        .iter()
        .enumerate()
        .filter(|(_, id)| id.starts_with(query))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => Err(OxaError::new(
            codes::NOT_FOUND,
            format!("no session matches `{query}`"),
        )),
        1 => Ok(hits[0]),
        n => Err(invalid(format!(
            "session id prefix `{query}` is ambiguous: {n} sessions match"
        ))
        .with_hint("give more characters of the id, or the session path")),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn civil_days_roundtrip_and_known_dates() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_733_234_400_123), "2024-12-03T14:00:00.123Z");
        for days in [-1i64, 0, 1, 59, 60, 365, 10_957, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, i64::from(m), i64::from(d)), days);
        }
    }

    #[test]
    fn iso_rejects_non_canonical_forms() {
        assert_eq!(ms_from_iso("2024-12-03T14:00:00.123Z"), Some(1_733_234_400_123));
        assert_eq!(ms_from_iso("2024-12-03T14:00:00Z"), None);
        assert_eq!(ms_from_iso("2024-12-03T14:00:00.123+00:00"), None);
        assert_eq!(ms_from_iso("2024-2-03T14:00:00.123Z"), None);
    }

    #[test]
    fn random_ids_are_eight_lowercase_hex_and_avoid_taken() {
        let id = new_entry_id(&|_| false);
        assert_eq!(id.len(), 8);
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        let first = new_entry_id(&|_| false);
        let second = new_entry_id(&|t: &str| t == first.as_str());
        assert_ne!(first, second);
    }

    #[test]
    fn dir_name_maps_separators() {
        assert_eq!(session_dir_name(Path::new("/home/u/proj")), "--home-u-proj--");
        assert_eq!(session_dir_name(Path::new("/")), "----");
    }
}
