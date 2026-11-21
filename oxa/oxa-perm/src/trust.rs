//! B9: project trust.
// ported from pi packages/coding-agent/src/core/project-trust.ts (docs: security.md, settings.md `defaultProjectTrust`)
use crate::normalize;
use oxa_proto::{codes, names, OxaError};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The shipped `defaultProjectTrust` (owner decision 2026-11-21): a project is trusted unless
/// the owner asks for the question (`ask`) or refuses projects (`never`), or a saved decision
/// says otherwise.
pub const SHIPPED_DEFAULT_TRUST: DefaultTrust = DefaultTrust::Trust;

/// pi's `defaultProjectTrust`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultTrust {
    Ask,
    Trust,
    Untrusted,
}

impl DefaultTrust {
    /// The decision [`SHIPPED_DEFAULT_TRUST`] and the owner's own value select. A missing,
    /// unknown or wrongly typed value takes the shipped default.
    pub fn from_setting(value: Option<&str>) -> DefaultTrust {
        match value {
            Some("always") => DefaultTrust::Trust,
            Some("never") => DefaultTrust::Untrusted,
            Some("ask") => DefaultTrust::Ask,
            _ => SHIPPED_DEFAULT_TRUST,
        }
    }
}

/// `trust.json`: saved decisions per directory, plus (B4, oxa only) the content hashes of the
/// project files the user approved, under the reserved key [`APPROVED_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrustStore {
    entries: Vec<(PathBuf, bool)>,
    approved: Approved,
}

/// B4: per project directory (its trust key), file path -> sha256 hex.
type Approved = BTreeMap<String, BTreeMap<String, String>>;

/// B4: the reserved `trust.json` key of the approved content hashes (not a directory; pi never
/// reads oxa's trust file).
pub const APPROVED_KEY: &str = "oxa.approved";

impl TrustStore {
    /// Read `trust.json` (missing or malformed = empty).
    pub fn load(file: &Path) -> TrustStore {
        TrustStore {
            entries: read_entries(file).unwrap_or_default(),
            approved: read_approved(file),
        }
    }
    /// The saved decision of the closest parent of `cwd` (or `cwd` itself) that has one.
    pub fn lookup(&self, cwd: &Path) -> Option<bool> {
        let mut current = Some(key_for(cwd));
        while let Some(dir) = current {
            if let Some((_, trusted)) = self.entries.iter().find(|(key, _)| *key == dir) {
                return Some(*trusted);
            }
            current = dir.parent().map(Path::to_path_buf);
        }
        None
    }
    /// Record a decision for `dir` and write the file (0600), keeping other entries.
    /// The file is re-read first, so entries written by another process survive. A file that
    /// exists but is not a valid trust store is an error and is left untouched.
    pub fn save(&mut self, file: &Path, dir: &Path, trusted: bool) -> Result<(), OxaError> {
        let key = key_for(dir);
        let _lock = lock_file(file)?;
        let mut entries = read_entries(file)?;
        entries.retain(|(existing, _)| *existing != key);
        entries.push((key, trusted));
        let approved = read_approved(file);
        write_entries(file, &entries, &approved)?;
        self.entries = entries;
        self.approved = approved;
        Ok(())
    }

    /// The saved decision trusts `cwd` and its executable files match the approval (or
    /// nothing was recorded yet). Folder policy need only check the saved decision.
    pub fn trusted_as_approved(&self, cwd: &Path) -> bool {
        self.lookup(cwd) == Some(true) && !matches!(self.approval(cwd), Approval::Changed(_))
    }

    /// Check resources after the caller has verified the runner's linked worktree.
    /// This is a per-launch decision. It creates no standalone worktree approval.
    pub fn approved_worktree_resources(&self, source: &Path, worktree: &Path) -> bool {
        let (Ok(source), Ok(worktree)) = (source.canonicalize(), worktree.canonicalize()) else {
            return false;
        };
        if source == worktree
            || self.lookup(&source) != Some(true)
            || self.lookup(&worktree) == Some(false)
            || self.approval(&source) != Approval::Matches
        {
            return false;
        }
        let Some(approved) = self.approved.get(&key_string(&source)) else {
            return false;
        };
        let config = worktree.join(names::PROJECT_DIR);
        let extensions = config.join("extensions");
        let mcp = config.join("mcp.json");
        let settings = config.join("settings.json");
        // The approval manifest does not bind executable dependencies loaded
        // through these entry points. Inherit only resources the manifest covers.
        manifest(&worktree).into_iter().all(|(name, hash)| {
            let path = Path::new(&name);
            if path == mcp || path.starts_with(&extensions) {
                return false;
            }
            let Ok(relative) = path.strip_prefix(&worktree) else {
                // Parent resources are inherited only at an already approved absolute path.
                return approved.get(&name) == Some(&hash);
            };
            let original = source.join(relative);
            // A link written identically in different checkouts can resolve differently.
            path.canonicalize().ok().as_deref() == Some(path)
                && original.canonicalize().ok().as_deref() == Some(original.as_path())
                && original.to_str().and_then(|name| approved.get(name)) == Some(&hash)
                && (path != settings || settings_allow_worktree_inheritance(path))
        })
    }

    /// B4: how the executable project files of `cwd` ([`approval_files`]) compare with what the
    /// user approved for `cwd`.
    pub fn approval(&self, cwd: &Path) -> Approval {
        let Some(record) = self.approved.get(&key_string(cwd)) else {
            return Approval::NoRecord;
        };
        let changed = changes(record, manifest(cwd));
        if changed.is_empty() {
            Approval::Matches
        } else {
            Approval::Changed(changed)
        }
    }

    /// Apply the content policy after the caller has admitted the folder. Record its first
    /// snapshot without saving a folder decision; later reads must compare to that snapshot.
    pub fn check_content(&mut self, file: &Path, cwd: &Path) -> Result<Approval, OxaError> {
        match self.approval(cwd) {
            Approval::NoRecord => {
                self.record_baseline(file, cwd)?;
                Ok(self.approval(cwd))
            }
            approval => Ok(approval),
        }
    }

    /// Report differences without granting content approval or printing file contents.
    pub fn folder_changes_notice(&self, cwd: &Path) -> Option<String> {
        let record = self.approved.get(&key_string(cwd))?;
        let mut changed = changes(record, manifest(cwd));
        if record.contains_key(NOTICE_BASELINE) {
            let extra = notice_fingerprints(cwd);
            for (key, stamp) in extra {
                if record.get(&key) != Some(&stamp) {
                    let path = PathBuf::from(key.strip_prefix(NOTICE_PREFIX).expect("notice key"));
                    if !changed.iter().any(|item| item.path == path) {
                        changed.push(Changed {
                            path,
                            new: !record.contains_key(&key),
                        });
                    }
                }
            }
        }
        if changed.is_empty() {
            return None;
        }
        let paths: Vec<_> = changed.iter().map(Changed::path_line).collect();
        Some(format!(
            "Project config loaded under folder trust: {}",
            paths.join(", ")
        ))
    }

    /// Record the first baseline before tools run. For older records, add only the
    /// notice fingerprints, never replacing the hashes used by content policy.
    pub fn record_baseline(&mut self, file: &Path, cwd: &Path) -> Result<bool, OxaError> {
        let key = key_string(cwd);
        if self
            .approved
            .get(&key)
            .is_some_and(|r| r.contains_key(NOTICE_BASELINE))
        {
            return Ok(false);
        }
        let _lock = lock_file(file)?;
        let entries = read_entries(file)?;
        let mut approved = read_approved(file);
        let record = approved.entry(key).or_insert_with(|| manifest(cwd));
        let changed = !record.contains_key(NOTICE_BASELINE);
        if changed {
            record_notice_baseline(record, cwd);
            write_entries(file, &entries, &approved)?;
        }
        self.entries = entries;
        self.approved = approved;
        Ok(changed)
    }

    /// B4: record the current content of the executable project files of `cwd` as approved
    /// (an explicit approval of the user, or the first record of a trusted project).
    pub fn approve(&mut self, file: &Path, cwd: &Path) -> Result<(), OxaError> {
        let _lock = lock_file(file)?;
        let entries = read_entries(file)?;
        let mut approved = read_approved(file);
        let mut record = manifest(cwd);
        record_notice_baseline(&mut record, cwd);
        approved.insert(key_string(cwd), record);
        write_entries(file, &entries, &approved)?;
        self.entries = entries;
        self.approved = approved;
        Ok(())
    }
}

/// A settings hash binds the declarations, but cannot bind the resources they
/// reference. A managed worker can still use the approved source's done checks
/// through the runner, which resolves and runs them in its existing sandbox.
fn settings_allow_worktree_inheritance(path: &Path) -> bool {
    let Some(serde_json::Value::Object(settings)) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    else {
        return false;
    };
    EXECUTABLE_SETTINGS
        .iter()
        .filter(|&&key| key != "done")
        .filter_map(|key| settings.get(*key))
        .all(|value| value.is_null() || value.as_array().is_some_and(Vec::is_empty))
}

/// B4: the approval state of a trusted project's executable files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Nothing was ever recorded for this directory (a project trusted before B4, or by an
    /// extension or the saved decision on its first run): the current content becomes the
    /// record.
    NoRecord,
    /// Every file matches the record.
    Matches,
    /// Files that are new or changed since the baseline. Only content policy gates on this.
    Changed(Vec<Changed>),
}

fn changes(record: &BTreeMap<String, String>, now: BTreeMap<String, String>) -> Vec<Changed> {
    now.into_iter()
        .filter_map(|(path, hash)| {
            let prior = record.get(&path);
            (prior != Some(&hash)).then_some(Changed {
                path: PathBuf::from(path),
                new: prior.is_none(),
            })
        })
        .collect()
}

// Notice-only slots are not absolute paths, so they can never satisfy a content or
// managed-worktree check. Stat rather than reading extra project-controlled files.
const NOTICE_BASELINE: &str = "notice";
const NOTICE_PREFIX: &str = "notice:";

fn record_notice_baseline(record: &mut BTreeMap<String, String>, cwd: &Path) {
    record.insert(NOTICE_BASELINE.into(), String::new());
    record.extend(notice_fingerprints(cwd));
}

fn notice_fingerprints(cwd: &Path) -> BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    let config = key_for(cwd).join(names::PROJECT_DIR);
    let mut paths = vec![config.join("settings.json")];
    let themes = config.join("themes");
    if std::fs::symlink_metadata(&themes).is_ok_and(|m| m.is_dir()) {
        if let Ok(entries) = std::fs::read_dir(&themes) {
            paths.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "json")),
            );
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::symlink_metadata(&path).ok()?;
            (meta.is_file() || meta.is_symlink()).then(|| {
                (
                    format!("{NOTICE_PREFIX}{}", path.display()),
                    format!(
                        "{}:{}:{}:{}:{}:{}:{}",
                        meta.dev(),
                        meta.ino(),
                        meta.size(),
                        meta.mtime(),
                        meta.mtime_nsec(),
                        meta.ctime(),
                        meta.ctime_nsec()
                    ),
                )
            })
        })
        .collect()
}

/// One project file that changed since the approval (B4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub path: PathBuf,
    /// Not in the record at all.
    pub new: bool,
}

impl Changed {
    /// Identify the change without displaying project-controlled contents.
    pub fn path_line(&self) -> String {
        format!("{} ({})", self.path.to_string_lossy().escape_debug(), self.kind())
    }

    /// One line for the content approval question.
    pub fn line(&self) -> String {
        format!(
            "{} ({}: {})",
            self.path.display(),
            self.kind(),
            summary(&self.path)
        )
    }

    fn kind(&self) -> &'static str {
        if self.new {
            "new"
        } else {
            "changed"
        }
    }
}

/// The settings keys that make the project run or load code (B4): the hash of
/// `settings.json` covers these only, so a changed theme asks nothing.
const EXECUTABLE_SETTINGS: &[&str] = &[
    "hooks",
    "shellPath",
    "shellCommandPrefix",
    "done",
    "packages",
    "extensions",
    "skills",
    "prompts",
];

/// B4: the files of `cwd` that make oxa run or load code outside the sandbox on the next
/// start (project settings, `mcp.json`, extensions, skills (`.agents/skills` up the tree too),
/// prompt templates, `SYSTEM.md` / `APPEND_SYSTEM.md`), every regular file, sorted. Themes are
/// not code and are left out.
pub fn approval_files(cwd: &Path) -> Vec<PathBuf> {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let base = normalize(cwd, &current);
    let config = base.join(names::PROJECT_DIR);
    let mut out = Vec::new();
    for f in ["settings.json", "mcp.json", "SYSTEM.md", "APPEND_SYSTEM.md"] {
        let p = config.join(f);
        if p.is_file() {
            out.push(p);
        }
    }
    for d in ["extensions", "skills", "prompts"] {
        walk(&config.join(d), &mut out);
    }
    let home_skills = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| key_for(&home.join(".agents").join("skills")));
    let walk_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for dir in oxa_proto::guard::project_ancestors(&base, &walk_home) {
        let dir = dir.as_path();
        let skills = dir.join(".agents").join("skills");
        if home_skills.as_ref() != Some(&key_for(&skills)) {
            walk(&skills, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every regular file below `dir` (symlinks are not followed: a link is hashed as its target
/// path). Iterative with no depth limit, so approval covers every file the loaders can find.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() || t.is_symlink() => out.push(p),
                _ => {}
            }
        }
    }
}

/// path -> sha256 hex of what [`approval_files`] finds (settings: its executable keys only).
pub fn manifest(cwd: &Path) -> BTreeMap<String, String> {
    approval_files(cwd)
        .into_iter()
        .map(|p| {
            let bytes = content_for_hash(&p);
            let hex: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            (p.to_string_lossy().into_owned(), hex)
        })
        .collect()
}

fn content_for_hash(p: &Path) -> Vec<u8> {
    let meta = std::fs::symlink_metadata(p);
    if meta.as_ref().is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(p).unwrap_or_default();
        return format!("symlink:{}", target.display()).into_bytes();
    }
    let bytes = std::fs::read(p).unwrap_or_default();
    if p.file_name().and_then(|n| n.to_str()) == Some("settings.json")
        && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some(names::PROJECT_DIR)
    {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        let picked: serde_json::Map<String, serde_json::Value> = EXECUTABLE_SETTINGS
            .iter()
            .filter_map(|k| value.get(*k).map(|v| ((*k).to_string(), v.clone())))
            .collect();
        return serde_json::Value::Object(picked).to_string().into_bytes();
    }
    bytes
}

/// What a project file would run, in one line (for the approval question).
pub fn summary(p: &Path) -> String {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let clip = |s: String| -> String {
        if s.chars().count() > 160 {
            let cut: String = s.chars().take(157).collect();
            format!("{cut}...")
        } else {
            s
        }
    };
    let in_dir = |d: &str| p.components().any(|c| c.as_os_str() == d);
    if name == "mcp.json" {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let servers = v
            .get("mcpServers")
            .or_else(|| v.get("servers"))
            .and_then(|s| s.as_object());
        let parts: Vec<String> = servers
            .map(|m| {
                m.iter()
                    .map(|(n, c)| {
                        let cmd = c.get("command").and_then(|x| x.as_str());
                        let args: Vec<&str> = c
                            .get("args")
                            .and_then(|a| a.as_array())
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                            .unwrap_or_default();
                        match (cmd, c.get("url").and_then(|x| x.as_str())) {
                            (Some(cmd), _) => format!("{n}: runs `{}`", [vec![cmd], args].concat().join(" ")),
                            (None, Some(url)) => format!("{n}: connects to {url}"),
                            _ => n.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        return clip(format!("MCP servers {}", parts.join("; ")));
    }
    if name == "settings.json" {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let parts: Vec<String> = EXECUTABLE_SETTINGS
            .iter()
            .filter_map(|k| v.get(*k).map(|x| format!("{k}: {x}")))
            .collect();
        return clip(format!("settings {}", parts.join("; ")));
    }
    if in_dir("extensions") {
        return "extension code, runs inside oxa".to_string();
    }
    if in_dir("prompts") || in_dir("skills") {
        return if text.contains("!`") {
            "prompt text with !`command` expansion".to_string()
        } else {
            "prompt text".to_string()
        };
    }
    "system prompt text".to_string()
}

/// An exclusive `flock` on the sidecar `<file>.lock` (held until dropped), so two oxa processes
/// doing read-modify-write on the trust file do not drop each other's entries.
fn lock_file(file: &Path) -> Result<std::fs::File, OxaError> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| OxaError::io(&format!("create {}", dir.display()), &e))?;
    }
    let mut name = file.as_os_str().to_owned();
    name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&name)
        .map_err(|e| OxaError::io("open trust lock", &e))?;
    lock.lock().map_err(|e| OxaError::io("lock trust store", &e))?;
    Ok(lock)
}

/// The approval key of a directory (its trust key, as a string).
fn key_string(dir: &Path) -> String {
    key_for(dir).to_string_lossy().into_owned()
}

/// The [`APPROVED_KEY`] object of a trust file (missing or malformed = empty).
fn read_approved(file: &Path) -> Approved {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Approved::new();
    };
    let value: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_default();
    let Some(dirs) = value.get(APPROVED_KEY).and_then(|v| v.as_object()) else {
        return Approved::new();
    };
    dirs.iter()
        .filter_map(|(dir, files)| {
            let files = files.as_object()?;
            let map: BTreeMap<String, String> = files
                .iter()
                .filter_map(|(f, h)| h.as_str().map(|h| (f.clone(), h.to_string())))
                .collect();
            Some((dir.clone(), map))
        })
        .collect()
}

/// The key a directory is stored under: absolute, lexically normalised, symlinks resolved when
/// the directory exists (so the same directory always gets the same key).
fn key_for(dir: &Path) -> PathBuf {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let norm = normalize(dir, &current);
    std::fs::canonicalize(&norm).unwrap_or(norm)
}

/// The entries of a trust file. Missing file = none. Non-boolean values (such as `null`, which
/// pi writes to forget a decision) are skipped. Anything else that is not a JSON object is an
/// error.
fn read_entries(file: &Path) -> Result<Vec<(PathBuf, bool)>, OxaError> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(OxaError::io(&format!("read {}", file.display()), &e)),
    };
    let value: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| {
            OxaError::new(
                codes::INVALID,
                format!("trust store {} is not valid JSON: {e}", file.display()),
            )
        })?;
    let Some(object) = value.as_object() else {
        return Err(OxaError::new(
            codes::INVALID,
            format!("trust store {} must be a JSON object", file.display()),
        ));
    };
    Ok(object
        .iter()
        .filter_map(|(key, value)| value.as_bool().map(|trusted| (key_for(Path::new(key)), trusted)))
        .collect())
}

/// Write the entries as a JSON object with sorted keys, through a temporary 0600 file that is
/// renamed over `file`.
fn write_entries(file: &Path, entries: &[(PathBuf, bool)], approved: &Approved) -> Result<(), OxaError> {
    let mut sorted: Vec<&(PathBuf, bool)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut object = serde_json::Map::new();
    for (key, trusted) in sorted {
        object.insert(
            key.to_string_lossy().into_owned(),
            serde_json::Value::Bool(*trusted),
        );
    }
    if !approved.is_empty() {
        object.insert(
            APPROVED_KEY.to_string(),
            serde_json::to_value(approved).unwrap_or_default(),
        );
    }
    let mut text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(|e| OxaError::new(codes::INVALID, format!("trust store: {e}")))?;
    text.push('\n');

    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| OxaError::io(&format!("create {}", dir.display()), &e))?;
    }
    let tmp = file.with_extension(format!("json.tmp-{}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        out.write_all(text.as_bytes())?;
        drop(out);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, file)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        OxaError::io(&format!("write {}", file.display()), &e)
    })
}

/// Everything that decides trust, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustInputs {
    /// `-a` / `--approve` = `Some(true)`, `-na` / `--no-approve` = `Some(false)`.
    pub flag: Option<bool>,
    /// Reply of the `project_trust` hook (`HookReply::Trust`), from user-level extensions only.
    pub extension: Option<bool>,
    /// `TrustStore::lookup`.
    pub saved: Option<bool>,
    pub default: DefaultTrust,
    /// A person can answer a prompt.
    pub interactive: bool,
    /// The project has anything trust-gated at all ([`gated_resources`] non-empty).
    pub has_gated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    Trusted,
    Untrusted,
    /// Show the trust selector (interactive only).
    AskUser,
}

fn outcome(trusted: bool) -> TrustOutcome {
    if trusted {
        TrustOutcome::Trusted
    } else {
        TrustOutcome::Untrusted
    }
}

/// flag > extension > saved (closest parent) > `defaultProjectTrust`. Nothing gated =
/// `Trusted` without asking unless there is a saved denial. `Ask` when not interactive = `Untrusted` (gated resources are
/// skipped, with one notice).
pub fn resolve_trust(i: &TrustInputs) -> TrustOutcome {
    if let Some(flag) = i.flag {
        return outcome(flag);
    }
    if !i.has_gated {
        return if i.saved == Some(false) {
            TrustOutcome::Untrusted
        } else {
            TrustOutcome::Trusted
        };
    }
    if let Some(extension) = i.extension {
        return outcome(extension);
    }
    if let Some(saved) = i.saved {
        return outcome(saved);
    }
    match i.default {
        DefaultTrust::Trust => TrustOutcome::Trusted,
        DefaultTrust::Untrusted => TrustOutcome::Untrusted,
        DefaultTrust::Ask if i.interactive => TrustOutcome::AskUser,
        DefaultTrust::Ask => TrustOutcome::Untrusted,
    }
}

/// The decision of the saved store and `defaultProjectTrust` alone, with no flag, extension
/// answer or prompt: what `/reload`, `oxa work` and the small subcommands that read project
/// resources use, so the shipped default decides them exactly as it decides a start.
pub fn saved_trust(store: &TrustStore, cwd: &Path, default: DefaultTrust) -> TrustOutcome {
    resolve_trust(&TrustInputs {
        flag: None,
        extension: None,
        saved: store.lookup(cwd),
        default,
        interactive: false,
        has_gated: !gated_resources(cwd).is_empty(),
    })
}

/// The trust-gated resources present in the project at `cwd`, as display paths in a fixed
/// order: project settings, extensions, skills, prompt templates, themes, `mcp.json`,
/// `SYSTEM.md` / `APPEND_SYSTEM.md`, exec hooks (pi's gated list; context files `AGENTS.md`
/// are not gated).
pub fn gated_resources(cwd: &Path) -> Vec<String> {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let base = normalize(cwd, &current);
    let config = base.join(names::PROJECT_DIR);
    let settings = config.join("settings.json");
    let mut out = Vec::new();

    present(&mut out, &settings);
    present(&mut out, &config.join("extensions"));
    present(&mut out, &config.join("skills"));
    // `.agents/skills` from `cwd` through its ancestors, except the user's own (`~/.agents/skills`).
    let home_skills = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| key_for(&home.join(".agents").join("skills")));
    let walk_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for dir in oxa_proto::guard::project_ancestors(&base, &walk_home) {
        let dir = dir.as_path();
        let skills = dir.join(".agents").join("skills");
        if home_skills.as_ref() != Some(&key_for(&skills)) {
            present(&mut out, &skills);
        }
    }
    present(&mut out, &config.join("prompts"));
    present(&mut out, &config.join("themes"));
    present(&mut out, &config.join("mcp.json"));
    present(&mut out, &config.join("SYSTEM.md"));
    present(&mut out, &config.join("APPEND_SYSTEM.md"));
    if has_exec_hooks(&settings) {
        out.push(format!("{} (hooks)", settings.display()));
    }
    out
}

fn present(out: &mut Vec<String>, path: &Path) {
    if path.exists() {
        out.push(path.display().to_string());
    }
}

/// The largest `settings.json` this check opens. A settings file is small; anything larger is
/// not read before the trust decision.
const MAX_HOOKS_SETTINGS_BYTES: u64 = 1024 * 1024;

/// `settings.json` declares at least one exec hook (`hooks: {event: command}`). Only a regular
/// file is opened: a link, FIFO or device keeps its presence entry (`gated_resources`) but is
/// never read, and neither is a file over [`MAX_HOOKS_SETTINGS_BYTES`]. The project-resource
/// decision before a settings read ([`saved_trust`]) runs this first, so neither a reporting-only
/// listing nor an untrusted start blocks on a FIFO, follows a link or allocates a large file.
fn has_exec_hooks(settings: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(settings) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_HOOKS_SETTINGS_BYTES {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(settings) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("hooks")
                .and_then(|hooks| hooks.as_object())
                .map(|hooks| !hooks.is_empty())
        })
        .unwrap_or(false)
}
