//! A11: settings.json loading, merge and typed access.
// ported from pi packages/coding-agent/src/core/settings-manager.ts
use crate::schema::{check_root, find_key, kind_matches, kind_name, merge_settings, warn, DEFAULT_TOOLS};
use crate::{Diagnostic, Dirs};
use oxa_proto::{codes, OxaError, PermissionMode, ThinkingLevel, Value};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// pi's `steeringMode` / `followUpMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

/// The `compaction.*` group with oxa's defaults (plan sec. 1): `enabled` true, `reserve_tokens`
/// 16384, `keep_recent_tokens` 4000, `soft_tokens` from `oxa.compaction.softTokens` (`None` =
/// derive from the context window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u32,
    pub keep_recent_tokens: u32,
    pub soft_tokens: Option<u32>,
}

/// Default agent-level retries (`retry.maxRetries`): cloud routes see more transient 429/5xx.
pub const DEFAULT_MAX_RETRIES: u32 = 10;
/// Default stream silence limit (`httpIdleTimeoutMs`): long silent thinking at max effort is normal.
pub const DEFAULT_IDLE_MS: u64 = 900_000;

/// The `retry.*` group (pi's keys and defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// #2050: the whole retry wait of a failure on this machine's own engine. Waiting cannot
    /// make a local engine that is down answer, so this budget is short (default 5 s).
    pub local_max_total_ms: u64,
    /// #2050: retries of the same 5xx body (status and message) before giving up. A cloud
    /// provider that is briefly overloaded keeps `max_retries`.
    pub identical_5xx_retries: u32,
}

/// H: the `retry.provider.*` group (pi's keys and defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetrySettings {
    /// HTTP-level attempts after the first one (default 0).
    pub max_retries: u32,
    /// Longest server-requested wait honoured, in milliseconds; 0 = no limit (default 60000).
    pub max_retry_delay_ms: u64,
    /// The request's silence limit in milliseconds; `None` = `httpIdleTimeoutMs`.
    pub timeout_ms: Option<u64>,
}

/// The merged settings (defaults < user < project). Cheap to clone.
///
/// Consumers read values by pi's dotted key (`"compaction.reserveTokens"`, `"theme"`,
/// `"oxa.toolOutputCap"`) through the typed getters; a missing or mistyped value yields the
/// default passed by the caller. Hot groups have typed views.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    merged: Value,
    user: Value,
    project: Value,
    diagnostics: Vec<Diagnostic>,
    /// A settings file existed but could not be read or parsed (its layer is empty).
    layer_failed: bool,
    /// In-memory overrides ([`Settings::with_override`]) in the order applied, so a live
    /// refresh of the files can put them back ([`Settings::with_overrides_of`]).
    overrides: Vec<(String, Value)>,
    files: Option<Arc<SettingsFiles>>,
}

/// The settings that were actually loaded for this session, captured before reading
/// so an edit during the load is visible on the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFiles {
    user: PathBuf,
    project: Option<PathBuf>,
    user_stamp: Option<FileStamp>,
    project_stamp: Option<FileStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileStamp {
        mtime: metadata.modified().ok(),
        len: metadata.len(),
        ino,
    })
}

impl SettingsFiles {
    fn new(user: PathBuf, project: Option<PathBuf>) -> Self {
        Self {
            user_stamp: stamp(&user),
            project_stamp: project.as_deref().and_then(stamp),
            user,
            project,
        }
    }

    /// Stat only the paths that were eligible at the original load.
    pub fn observe(&self) -> Self {
        Self::new(self.user.clone(), self.project.clone())
    }

    pub fn project_changed_since(&self, previous: &Self) -> bool {
        self.project_stamp != previous.project_stamp
    }

    /// Startup admitted the session's own project settings path.
    pub fn allows_project(&self) -> bool {
        self.project.is_some()
    }
}

impl Settings {
    /// Read `<agent_dir>/settings.json` and, when `project_trusted`, `<cwd>/.oxa/settings.json`;
    /// merge per pi's rules ([`crate::merge_settings`]); validate ([`crate::validate`]). Missing
    /// files are empty objects. Never fails: problems become diagnostics.
    pub fn load(dirs: &Dirs, cwd: &Path, project_trusted: bool) -> Settings {
        let files = SettingsFiles::new(
            dirs.settings_file(),
            project_trusted.then(|| dirs.project_settings_file(cwd)),
        );
        Self::read_files(files, None)
    }

    /// A session with caller-supplied settings has no files to watch.
    pub fn files(&self) -> Option<&SettingsFiles> {
        self.files.as_deref()
    }

    /// If the project layer did not change or lacks fresh approval, preserve the
    /// layer admitted at launch. In either case its file does not get read again.
    pub fn refresh_files(&self, mut files: SettingsFiles, read_project: bool) -> Settings {
        if !read_project {
            files.project_stamp = self.files().and_then(|loaded| loaded.project_stamp.clone());
        }
        Self::read_files(files, (!read_project).then_some(&self.project)).with_overrides_of(self)
    }

    fn read_files(files: SettingsFiles, keep_project: Option<&Value>) -> Settings {
        let mut diagnostics = Vec::new();
        let mut user = read_layer(&files.user, &mut diagnostics);
        // A present user file that cannot be used must not drop the owner's security
        // settings (#1449): keep the most restrictive reading of them and say so.
        if !diagnostics.is_empty() {
            user = fail_safe_user_layer();
            // One warning: the read problem plus what oxa kept and how to recover.
            for d in &mut diagnostics {
                d.message.push_str(
                    "; oxa keeps the most restrictive security settings (offline on, bash network \
                     off, new projects not trusted) and uses defaults for the rest. Fix or restore \
                     this file and restart to recover",
                );
            }
        }
        let (project, project_src) = match (files.project.as_deref(), keep_project) {
            (Some(path), Some(layer)) => (layer.clone(), path.display().to_string()),
            (Some(path), None) => (read_layer(path, &mut diagnostics), path.display().to_string()),
            (None, _) => (Value::Null, String::from("project settings")),
        };
        // `read_layer` is the only source of diagnostics so far.
        let layer_failed = !diagnostics.is_empty();
        let mut settings = build(
            user,
            project,
            diagnostics,
            &files.user.display().to_string(),
            &project_src,
        );
        settings.layer_failed = layer_failed;
        settings.files = Some(Arc::new(files));
        settings
    }
    /// Whether [`Settings::load`] met a file it could not read or parse. A caller that applies
    /// a changed file live keeps its previous settings then.
    pub fn layer_failed(&self) -> bool {
        self.layer_failed
    }
    /// Whether the project's own settings file declares a non-empty `done` list. Reads the file
    /// whatever the trust (it only looks at that key and never merges it), so the caller can
    /// say that the checks were skipped for an untrusted project.
    pub fn project_declares_done(dirs: &Dirs, cwd: &Path) -> bool {
        let layer = read_layer(&dirs.project_settings_file(cwd), &mut Vec::new());
        layer
            .get("done")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|c| c.as_str().is_some_and(|c| !c.trim().is_empty())))
    }
    /// Build from already-parsed values (tests, SDK).
    pub fn from_values(user: Value, project: Value) -> Settings {
        build(user, project, Vec::new(), "user settings", "project settings")
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    /// Raw merged value at a dotted key.
    /// The whole merged settings object (pi's `getSettings()` copy).
    pub fn merged(&self) -> &Value {
        &self.merged
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        let mut cur = &self.merged;
        for part in key.split('.') {
            cur = cur.as_object()?.get(part)?;
        }
        Some(cur)
    }
    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(default)
    }
    /// A bool read from the user layer only; a project file never changes it (use for settings
    /// that start unattended model work). A missing or mistyped value yields `default`.
    pub fn user_bool(&self, key: &str, default: bool) -> bool {
        self.user_value(key).and_then(Value::as_bool).unwrap_or(default)
    }
    pub fn u32(&self, key: &str, default: u32) -> u32 {
        self.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    }
    fn u64(&self, key: &str, default: u64) -> u64 {
        self.get(key).and_then(Value::as_u64).unwrap_or(default)
    }
    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Value::as_str).map(str::to_string)
    }
    /// #202: the permission mode at start (`permissions.defaultMode`): the user's value, else
    /// `auto`. A project's value counts only when it does not allow more than that (it may
    /// lower the mode towards `standard` or `plan`, never raise it).
    pub fn default_permission_mode(&self) -> PermissionMode {
        let read = |layer: &Value| {
            layer
                .get("permissions")
                .and_then(|p| p.get("defaultMode"))
                .and_then(Value::as_str)
                .and_then(PermissionMode::parse)
        };
        let user = read(&self.user).unwrap_or(PermissionMode::DEFAULT);
        match read(&self.project) {
            Some(p) if p.rank() <= user.rank() => p,
            _ => user,
        }
    }
    /// The raw entries of a list setting as the user layer wrote them, before the merge applies
    /// its `+x` / `-x` edits to the (empty) default. `[]` when the user layer does not set it.
    /// A caller that patches its own built-in list (`oxa.approval.dangerous`) reads this, so an
    /// edit list is not turned into a plain list that replaces the built-ins.
    pub fn user_list(&self, key: &str) -> Vec<String> {
        let mut cur = &self.user;
        for part in key.split('.') {
            match cur.as_object().and_then(|o| o.get(part)) {
                Some(v) => cur = v,
                None => return Vec::new(),
            }
        }
        cur.as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// A string list; `+x` / `-x` patch entries are already applied over the lower layer.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn compaction(&self) -> CompactionSettings {
        CompactionSettings {
            enabled: self.bool("compaction.enabled", true),
            reserve_tokens: self.u32("compaction.reserveTokens", 16384),
            keep_recent_tokens: self.u32("compaction.keepRecentTokens", 4000),
            soft_tokens: self
                .get("oxa.compaction.softTokens")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
        }
    }
    /// Skill reactivation adds instructions to later prompts, so only an owner can enable it.
    pub fn compaction_reactivate_skills(&self) -> bool {
        self.user_value("compaction.reactivateSkills")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
    pub fn retry(&self) -> RetrySettings {
        RetrySettings {
            enabled: self.bool("retry.enabled", true),
            max_retries: self.u32("retry.maxRetries", DEFAULT_MAX_RETRIES),
            base_delay_ms: self.u64("retry.baseDelayMs", 2000),
            max_delay_ms: self.u64("retry.maxAgentDelayMs", 60000),
            local_max_total_ms: self.u64("retry.localMaxTotalMs", 5000),
            identical_5xx_retries: self.u32("retry.identical5xxRetries", 2),
        }
    }
    /// H: the `retry.provider.*` group (pi's keys and defaults). In oxa there is one HTTP
    /// layer, so these are the HTTP-level attempts inside one agent-level attempt.
    pub fn provider_retry(&self) -> ProviderRetrySettings {
        ProviderRetrySettings {
            max_retries: self.u32("retry.provider.maxRetries", 0),
            max_retry_delay_ms: self.u64("retry.provider.maxRetryDelayMs", 60000),
            timeout_ms: self.get("retry.provider.timeoutMs").and_then(Value::as_u64),
        }
    }
    /// H: `httpIdleTimeoutMs`: the silence limit of the engine stream in milliseconds; `None`
    /// when disabled (`0` or `"disabled"`). A value of another shape is the default (900000).
    pub fn http_idle_timeout_ms(&self) -> Option<u64> {
        match self.get("httpIdleTimeoutMs") {
            Some(Value::String(s)) if s == "disabled" => None,
            Some(v) => match v.as_u64() {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(DEFAULT_IDLE_MS),
            },
            None => Some(DEFAULT_IDLE_MS),
        }
    }
    /// `oxa.emptyReplyRetries`: how many times a cloud reply with no text and no tool call is
    /// asked again (default 1; 0 turns it off). User file only: the retries spend the owner's
    /// cloud allowance with fresh idempotency keys, so a project file cannot raise them.
    pub fn empty_reply_retries(&self) -> u32 {
        self.user_u32("oxa.emptyReplyRetries", 1)
    }
    /// H: the startup thinking level of `model` (pi's sdk.ts order after the CLI choices):
    /// `modelThinkingLevels[model]` (keyed by the engine model id; pi's `provider/modelId`
    /// form is accepted with the provider `oxe`), else `defaultThinkingLevel`, else `medium`.
    pub fn startup_thinking(&self, model: &str) -> ThinkingLevel {
        let per_model = self
            .get("modelThinkingLevels")
            .and_then(Value::as_object)
            .and_then(|m| {
                m.get(model)
                    .or_else(|| m.get(&format!("{}/{model}", crate::models::ENGINE_ENDPOINT)))
                    .and_then(Value::as_str)
                    .and_then(ThinkingLevel::parse)
            });
        per_model
            .or_else(|| {
                self.string("defaultThinkingLevel")
                    .as_deref()
                    .and_then(ThinkingLevel::parse)
            })
            .unwrap_or(ThinkingLevel::Medium)
    }
    /// H: the `sessionDir` setting as a path: `~` expands to `home`, a relative path resolves
    /// from `cwd` (pi settings.md). `None` when unset or empty.
    pub fn session_dir(&self, home: &Path, cwd: &Path) -> Option<PathBuf> {
        self.string("sessionDir")
            .filter(|s| !s.is_empty())
            .map(|s| resolve_user_path(&s, home, cwd))
    }
    /// H: `shellPath` with a leading `~` expanded (pi: `normalizePath`). `None` when unset.
    pub fn shell_path(&self, home: &Path) -> Option<PathBuf> {
        self.string("shellPath")
            .filter(|s| !s.is_empty())
            .map(|s| resolve_user_path(&s, home, Path::new("/")))
    }
    /// `done`: shell commands oxa runs itself before it reports completion; empty = off.
    pub fn done_commands(&self) -> Vec<String> {
        self.list("done")
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect()
    }
    /// H: `shellCommandPrefix`; `None` when unset or empty.
    pub fn shell_command_prefix(&self) -> Option<String> {
        self.string("shellCommandPrefix").filter(|s| !s.is_empty())
    }
    /// `steeringMode` (default `OneAtATime`, as pi).
    pub fn steering_mode(&self) -> QueueMode {
        queue_mode(self.string("steeringMode").as_deref())
    }
    /// `followUpMode` (default `OneAtATime`, as pi).
    pub fn follow_up_mode(&self) -> QueueMode {
        queue_mode(self.string("followUpMode").as_deref())
    }
    /// `oxa.toolOutputCap` in bytes (W4; default 16 KiB).
    pub fn tool_output_cap(&self) -> usize {
        self.u32("oxa.toolOutputCap", 16384) as usize
    }
    /// `defaultTools` after patches; `None` = oxa's default built-ins, i.e. no
    /// layer sets `defaultTools`.
    pub fn default_tools(&self) -> Option<Vec<String>> {
        if self.user.get("defaultTools").is_none() && self.project.get("defaultTools").is_none() {
            return None;
        }
        Some(self.list("defaultTools"))
    }
    /// The `+x` / `-x` entries of `defaultTools` when every layer that sets it only edits the
    /// default selection (no layer gives a plain list), user layer first; `None` otherwise.
    /// [`Settings::default_tools`] resolves such edits against the default built-ins; the tool
    /// selection uses these entries to edit the members of oxa's default selection that come
    /// from hook sets (the `subagent` tool, O-146). Within one layer an exclusion wins, as in
    /// the merge, so a `+x` of a layer that also has `-x` is left out.
    pub fn default_tools_edits(&self) -> Option<Vec<String>> {
        let layers: Vec<Vec<String>> = [&self.user, &self.project]
            .into_iter()
            .filter_map(|layer| layer.get("defaultTools"))
            .map(|v| {
                v.as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default()
            })
            .collect();
        let edit = |s: &String| s.starts_with('+') || s.starts_with('-');
        if layers.is_empty() || layers.iter().any(|l| l.is_empty() || !l.iter().all(edit)) {
            return None;
        }
        let mut out = Vec::new();
        for layer in layers {
            let excluded: Vec<&str> = layer.iter().filter_map(|e| e.strip_prefix('-')).collect();
            out.extend(
                layer
                    .iter()
                    .filter(|e| e.strip_prefix('+').is_none_or(|n| !excluded.contains(&n)))
                    .cloned(),
            );
        }
        Some(out)
    }
    /// Exec hooks: `hooks: {event: command}`.
    pub fn exec_hooks(&self) -> Vec<(String, String)> {
        self.get("hooks")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(event, cmd)| cmd.as_str().map(|c| (event.clone(), c.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// E11: `value` at `key` over every layer, in memory only (never persisted). A subagent
    /// child applies its soft compaction limit (`oxa.compaction.softTokens`) this way. A later
    /// [`Settings::set`] recomposes the layers and drops the override.
    pub fn with_override(mut self, key: &str, value: Value) -> Settings {
        if let Value::Object(root) = &mut self.merged {
            set_nested(root, key, value.clone());
        }
        self.overrides.retain(|(k, _)| k != key);
        self.overrides.push((key.to_string(), value));
        self
    }
    /// The in-memory overrides of `old` applied over these (freshly loaded) settings: a live
    /// refresh of the files must not undo what the session set for itself (a child's soft
    /// limit, an inherited-trust worker's empty `done`, `--verbose`, `--models`).
    pub fn with_overrides_of(mut self, old: &Settings) -> Settings {
        for (key, value) in &old.overrides {
            self = self.with_override(key, value.clone());
        }
        self
    }
    /// Where the value of `key` comes from: the project file, else the user file, else the
    /// built-in default.
    pub fn source(&self, key: &str) -> Source {
        if lookup(&self.project, key).is_some() {
            Source::Project
        } else if lookup(&self.user, key).is_some() {
            Source::User
        } else {
            Source::Default
        }
    }
    /// True when a layer or an in-memory override sets `key`. A built-in default does not count.
    pub(crate) fn is_explicit(&self, key: &str) -> bool {
        self.source(key) != Source::Default || self.overrides.iter().any(|(k, _)| k == key)
    }
    /// The value `key` has in the user file, if any (not the merged value).
    pub fn user_value(&self, key: &str) -> Option<&Value> {
        lookup(&self.user, key)
    }
    /// `defaultProjectTrust` (`ask`, `always`, `never`) from the user file only, or `None` when
    /// the owner left it unset, set it in a project file or typed another value. `None` selects
    /// the shipped default; a user file that cannot be read fails safe to `never`.
    pub fn project_trust_default(&self) -> Option<&'static str> {
        match self.user_value("defaultProjectTrust").and_then(Value::as_str) {
            Some("ask") => Some("ask"),
            Some("always") => Some("always"),
            Some("never") => Some("never"),
            _ => None,
        }
    }
    /// Folder trust is the default. Only the owner can select per-content approval.
    pub fn project_trust_requires_content(&self) -> bool {
        self.user_value("projectTrustPolicy").and_then(Value::as_str) == Some("content")
    }
    /// Whether the local agent channel (control lane) is on: `oxa.a2a.enabled`, default true.
    /// Read from the user file only; a project file cannot change it.
    pub fn a2a_enabled(&self) -> bool {
        self.user_value("oxa.a2a.enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }
    /// Read a numeric setting from the user file with no project override.
    pub fn user_u32(&self, key: &str, default: u32) -> u32 {
        self.user_value(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
    }
    /// Set a key in memory and persist it to the user file (`project = false`) or the project
    /// file, keeping unrelated content and key order of that file. A field of a descriptor with
    /// a hard range is refused outside the range. The read-modify-write holds the file's lock.
    pub fn set(
        &mut self,
        dirs: &Dirs,
        cwd: &Path,
        key: &str,
        value: Value,
        project: bool,
    ) -> Result<(), OxaError> {
        check_key_shape(key)?;
        if project && crate::descriptor::is_user_only(key) {
            return Err(OxaError::new(
                codes::INVALID,
                format!("setting `{key}` is user-only; it cannot be set in a project file"),
            ));
        }
        if let Some(def) = find_key(key) {
            if !kind_matches(def.kind, &value) {
                return Err(OxaError::new(
                    codes::INVALID,
                    format!("setting `{key}` expects {}", kind_name(def.kind)),
                ));
            }
        }
        if let Some(range) = crate::descriptor(key).and_then(|d| d.range).filter(|r| r.hard) {
            check_hard_range(key, &value, &range)?;
        }
        let path = if project {
            dirs.project_settings_file(cwd)
        } else {
            dirs.settings_file()
        };
        let _lock = lock_for_write(&path)?;
        let mut root = read_object_for_write(&path)?;
        set_nested(&mut root, key, value);
        self.persist(&path, root, project)
    }
    /// Change one key of the USER file with its current on-disk value: under the file's write
    /// lock, `change` receives the value the file holds right now (not this instance's possibly
    /// stale copy) and returns the value to store, so two concurrent updates merge instead of
    /// one replacing the other's work. An error from `change` writes nothing.
    pub fn update_user(
        &mut self,
        dirs: &Dirs,
        key: &str,
        change: impl FnOnce(Option<&Value>) -> Result<Value, OxaError>,
    ) -> Result<(), OxaError> {
        check_key_shape(key)?;
        let path = dirs.settings_file();
        let _lock = lock_for_write(&path)?;
        let mut root = read_object_for_write(&path)?;
        let current = lookup_in(&root, key).cloned();
        let value = change(current.as_ref())?;
        set_nested(&mut root, key, value);
        self.persist(&path, root, false)
    }
    /// Remove `key` from the user file (`project = false`) or the project file, pruning parent
    /// objects it leaves empty; every other key stays as it is. Nothing is written when the
    /// file does not hold the key. A symbolic link at the settings path is refused, not followed.
    pub fn unset(&mut self, dirs: &Dirs, cwd: &Path, key: &str, project: bool) -> Result<(), OxaError> {
        check_key_shape(key)?;
        let path = if project {
            dirs.project_settings_file(cwd)
        } else {
            dirs.settings_file()
        };
        let _lock = lock_for_write(&path)?;
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(OxaError::new(
                codes::INVALID,
                format!("{} is a symbolic link; it is not changed", path.display()),
            )
            .with_hint("Edit the file the link points to."));
        }
        let mut root = read_object_for_write(&path)?;
        if !remove_nested(&mut root, key) {
            // Nothing on disk to remove, so nothing is written. The file just read is still the
            // truth for this layer: adopt it, so an instance that still holds a value another
            // instance (or an editor) already removed stops reporting it.
            self.adopt(&path, root, project);
            return Ok(());
        }
        self.persist(&path, root, project)
    }
    /// Write `root` to `path` (the user file is private, 0600) and make it the layer in memory.
    fn persist(&mut self, path: &Path, root: Map<String, Value>, project: bool) -> Result<(), OxaError> {
        let mut text = serde_json::to_string_pretty(&root)
            .map_err(|e| OxaError::new(codes::INVALID, format!("cannot serialise settings: {e}")))?;
        text.push('\n');
        // The user file is private to the user; a project file keeps the umask default.
        write_atomic(path, &text, if project { 0o666 } else { 0o600 })?;
        self.adopt(path, root, project);
        Ok(())
    }
    /// Make `root` the layer for `project` in memory and recompose the merged settings.
    fn adopt(&mut self, path: &Path, root: Map<String, Value>, project: bool) {
        let source = path.display().to_string();
        let (mut checked, _) = check_root(&Value::Object(root), &source);
        if project {
            // A set/unset rereads the complete file before adopting the edited layer. Apply the
            // same trust boundary as load so an unrelated edit cannot restore rejected keys.
            strip_user_only(&mut checked, &source);
            self.project = checked;
        } else {
            self.user = checked;
        }
        self.merged = compose(&self.user, &self.project);
        self.overrides.clear();
    }
}

/// Where a setting's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    User,
    Project,
}

fn check_key_shape(key: &str) -> Result<(), OxaError> {
    if key.is_empty() || key.split('.').any(str::is_empty) {
        return Err(OxaError::new(
            codes::INVALID,
            format!("invalid setting key: {key:?}"),
        ));
    }
    Ok(())
}

/// A hard range refuses a value outside it; only whole numbers can be inside one.
fn check_hard_range(key: &str, value: &Value, range: &crate::Range) -> Result<(), OxaError> {
    let inside = value
        .as_u64()
        .is_some_and(|n| n >= range.min && range.max.is_none_or(|max| n <= max));
    if inside {
        return Ok(());
    }
    let upper = range.max.map_or_else(|| "any".to_string(), |max| max.to_string());
    Err(OxaError::new(
        codes::INVALID,
        format!(
            "setting `{key}` must be a whole number from {} to {upper}",
            range.min
        ),
    ))
}

/// The exclusive write lock of a settings file, `<file>.lock` (`settings.json.lock`), held by the
/// returned handle until it is dropped. Two read-modify-writes under it cannot lose each other's
/// change. The lock file is private (0600) and is never the settings file itself.
fn lock_for_write(path: &Path) -> Result<std::fs::File, OxaError> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("create {}", parent.display()), &e))?;
    }
    let lock_path = path.with_extension("json.lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| OxaError::io(&format!("open {}", lock_path.display()), &e))?;
    file.lock()
        .map_err(|e| OxaError::io(&format!("lock {}", lock_path.display()), &e))?;
    Ok(file)
}

/// The value at a dotted key of one layer, if the layer has it.
fn lookup<'a>(layer: &'a Value, key: &str) -> Option<&'a Value> {
    let mut cur = layer;
    for part in key.split('.') {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}

fn lookup_in<'a>(root: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let mut parts = key.split('.');
    let mut cur = root.get(parts.next()?)?;
    for part in parts {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}

/// Remove `key` (dotted) from `root`, pruning the parent objects it leaves empty. False when the
/// key is absent, in which case nothing changed.
fn remove_nested(root: &mut Map<String, Value>, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    remove_path(root, &parts)
}

fn remove_path(obj: &mut Map<String, Value>, parts: &[&str]) -> bool {
    match parts {
        [] => false,
        [last] => obj.shift_remove(*last).is_some(),
        [first, rest @ ..] => {
            let Some(Value::Object(child)) = obj.get_mut(*first) else {
                return false;
            };
            let removed = remove_path(child, rest);
            if removed && child.is_empty() {
                obj.shift_remove(*first);
            }
            removed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn settings_reader_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("oxa-settings-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("workspace.json");
        let link = dir.join("settings.json");
        std::fs::write(&target, r#"{"done":["echo injected"]}"#).unwrap();
        symlink(&target, &link).unwrap();

        let mut diagnostics = Vec::new();
        assert_eq!(read_layer(&link, &mut diagnostics), Value::Null);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("symbolic link"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_reply_retries_are_user_only() {
        let user_two = Settings::from_values(
            json!({"oxa": {"emptyReplyRetries": 2}}),
            json!({"oxa": {"emptyReplyRetries": 9}}),
        );
        assert_eq!(
            user_two.empty_reply_retries(),
            2,
            "the user file decides how far an empty reply may be retried"
        );
        let project_only = Settings::from_values(json!({}), json!({"oxa": {"emptyReplyRetries": 9}}));
        assert_eq!(
            project_only.empty_reply_retries(),
            1,
            "a project file cannot raise the retries that spend the owner's allowance"
        );
    }

    #[test]
    fn a2a_enabled_defaults_true_and_ignores_the_project_layer() {
        assert!(Settings::from_values(json!({}), json!({})).a2a_enabled());
        let project_off = Settings::from_values(json!({}), json!({"oxa": {"a2a": {"enabled": false}}}));
        assert!(project_off.a2a_enabled());
        let user_off = Settings::from_values(json!({"oxa": {"a2a": {"enabled": false}}}), json!({}));
        assert!(!user_off.a2a_enabled());
        let user_on_project_off = Settings::from_values(
            json!({"oxa": {"a2a": {"enabled": true}}}),
            json!({"oxa": {"a2a": {"enabled": false}}}),
        );
        assert!(user_on_project_off.a2a_enabled());
    }

    #[test]
    fn headless_inbox_setting_is_user_only() {
        let user_off = Settings::from_values(json!({"oxa": {"inbox": {"headless": false}}}), json!({}));
        assert!(!user_off.user_bool("oxa.inbox.headless", true));
        let project_on = Settings::from_values(
            json!({"oxa": {"inbox": {"headless": false}}}),
            json!({"oxa": {"inbox": {"headless": true}}}),
        );
        assert!(
            !project_on.user_bool("oxa.inbox.headless", true),
            "a project file cannot re-enable the headless inbox the user turned off"
        );
        let project_off = Settings::from_values(json!({}), json!({"oxa": {"inbox": {"headless": false}}}));
        assert!(project_off.user_bool("oxa.inbox.headless", true));
    }

    #[test]
    fn set_rejects_field_outside_hard_range() {
        let range = crate::Range {
            min: 1,
            max: Some(65535),
            hard: true,
            step: 1,
            note: "",
        };
        assert!(check_hard_range("oxa.lead.serve.port", &json!(1), &range).is_ok());
        assert!(check_hard_range("oxa.lead.serve.port", &json!(65535), &range).is_ok());
        assert!(check_hard_range("oxa.lead.serve.port", &json!(0), &range).is_err());
        assert!(check_hard_range("oxa.lead.serve.port", &json!(65536), &range).is_err());
        assert!(check_hard_range("oxa.lead.serve.port", &json!("8080"), &range).is_err());
    }

    #[test]
    fn remove_prunes_only_emptied_parents() {
        let mut root = json!({"a": {"b": {"c": 1}, "d": 2}, "e": {"f": 3}});
        let map = root.as_object_mut().expect("object");
        assert!(remove_nested(map, "a.b.c"));
        assert_eq!(root, json!({"a": {"d": 2}, "e": {"f": 3}}));
        let map = root.as_object_mut().expect("object");
        assert!(!remove_nested(map, "a.b.c"), "absent key changes nothing");
        assert!(remove_nested(map, "e.f"));
        assert_eq!(root, json!({"a": {"d": 2}}));
    }

    #[test]
    fn remove_keeps_sibling_key_order() {
        let mut root = json!({"theme": 1, "retry": 2, "compaction": 3, "p": {"x": {"y": 1}, "z": 2}});
        let map = root.as_object_mut().expect("object");
        assert!(remove_nested(map, "theme"));
        assert!(remove_nested(map, "p.x.y"));
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["retry", "compaction", "p"]);
        let inner: Vec<&str> = map["p"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(inner, ["z"]);
        let mut root = json!({"p": {"a": {"y": 1}}, "q": 1, "r": 2});
        let map = root.as_object_mut().expect("object");
        assert!(remove_nested(map, "p.a.y"));
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["q", "r"], "pruned empty parent keeps order");
    }

    #[test]
    fn project_cannot_set_sandbox_net() {
        let user = json!({"oxa": {"sandbox": {"net": "off"}}});
        let project = json!({"oxa": {"sandbox": {"net": "on"}}});
        let s = build(user, project, Vec::new(), "user", "project");
        assert_eq!(s.merged["oxa"]["sandbox"]["net"], json!("off"));
        assert!(s.project.get("oxa").is_none(), "pruned from the project layer");
    }

    #[test]
    fn project_user_only_key_diagnostic() {
        let project = json!({"oxa": {"sandbox": {"net": "on"}}, "theme": "dark"});
        let s = build(json!({}), project, Vec::new(), "user", "project");
        let hits: Vec<_> = s
            .diagnostics
            .iter()
            .filter(|d| d.source == "project" && d.message.contains("oxa.sandbox.net"))
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(s.merged["theme"], json!("dark"), "other project keys still apply");
    }

    #[test]
    fn named_cloud_providers_stay_user_only_after_project_adoption() {
        let owner =
            json!({"oxa":{"cloud":{"providers":{"owner":{"base_url":"https://owner.example.invalid"}}}}});
        let project = json!({"oxa":{"cloud":{"providers":{"injected":{"base_url":"https://project.example.invalid"}}}},"theme":"dark"});
        let mut settings = Settings::from_values(owner.clone(), project.clone());
        assert_eq!(
            settings.get("oxa.cloud.providers"),
            owner.pointer("/oxa/cloud/providers")
        );
        settings.adopt(
            Path::new("settings.json"),
            project.as_object().unwrap().clone(),
            true,
        );
        assert_eq!(
            settings.get("oxa.cloud.providers"),
            owner.pointer("/oxa/cloud/providers")
        );
        assert_eq!(settings.get("theme"), Some(&json!("dark")));
    }

    #[test]
    fn project_set_of_user_only_key_is_refused() {
        let root = Path::new("/nonexistent-oxa-test");
        let dirs = Dirs::under(root);
        let mut s = build(json!({}), json!({}), Vec::new(), "user", "project");
        let err = s
            .set(&dirs, root, "oxa.sandbox.net", json!("on"), true)
            .expect_err("refused");
        assert!(err.to_string().contains("user-only"));
    }
}

/// Merge the layers over the built-in defaults and validate each layer. Wrongly typed values
/// are dropped before the merge, so a bad project value falls back to the user's value.
fn build(
    user: Value,
    project: Value,
    mut diagnostics: Vec<Diagnostic>,
    user_src: &str,
    project_src: &str,
) -> Settings {
    let (user, user_diags) = check_root(&user, user_src);
    let (mut project, project_diags) = check_root(&project, project_src);
    diagnostics.extend(user_diags);
    diagnostics.extend(project_diags);
    diagnostics.extend(strip_user_only(&mut project, project_src));
    let merged = compose(&user, &project);
    Settings {
        merged,
        user,
        project,
        diagnostics,
        layer_failed: false,
        overrides: Vec::new(),
        files: None,
    }
}

/// Drop every key whose descriptor is `user_only` from the project layer, with one warning
/// per key. A cloned repository must not widen the sandbox, the network or the home-folder
/// rules; those keys count only from the user file (SECURITY.md, "Refusals that stay in yolo").
fn strip_user_only(project: &mut Value, source: &str) -> Vec<Diagnostic> {
    let Some(map) = project.as_object_mut() else {
        return Vec::new();
    };
    let mut diags = Vec::new();
    for d in crate::descriptor::all_rows().iter().filter(|d| d.user_only) {
        if remove_nested(map, d.key) {
            diags.push(warn(
                source,
                format!(
                    "setting `{}` is user-only and is ignored in a project file; set it in your user settings",
                    d.key
                ),
            ));
        }
    }
    diags
}

/// defaults < user < project. The defaults layer carries oxa's default tool list so that a
/// `+x` / `-x` patch with no explicit base patches the seven built-ins.
fn compose(user: &Value, project: &Value) -> Value {
    let defaults = defaults_layer();
    merge_settings(&merge_settings(&defaults, user), project)
}

fn defaults_layer() -> Value {
    let tools: Vec<Value> = DEFAULT_TOOLS
        .iter()
        .map(|t| Value::String((*t).to_string()))
        .collect();
    let mut m = Map::new();
    m.insert("defaultTools".to_string(), Value::Array(tools));
    Value::Object(m)
}

fn queue_mode(value: Option<&str>) -> QueueMode {
    match value {
        Some("all") => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

/// The user layer used when the user's settings file exists but cannot be read, parsed or is
/// not an object: every network and trust setting at its most restrictive value. Other
/// settings are absent, so they take their defaults.
fn fail_safe_user_layer() -> Value {
    serde_json::json!({
        "oxa": {"offline": true, "sandbox": {"net": "off"}, "engine": {"allowRemote": false}},
        "defaultProjectTrust": "never",
        "projectTrustPolicy": "content"
    })
}

/// Read one settings file. A missing file is an empty object; an unreadable or unparsable file
/// is a diagnostic and an empty object.
fn read_layer(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Value {
    let source = path.display().to_string();
    // Approval records the project settings path itself. Following a link here would let a
    // writable target change without changing the approved link, including during live reload.
    // Refuse links at the read boundary so startup, reload and the small inspection helpers all
    // share the same rule.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        diagnostics.push(warn(&source, "settings file must not be a symbolic link"));
        return Value::Null;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => {
                diagnostics.push(warn(&source, "settings file must contain a JSON object"));
                Value::Null
            }
            Err(e) => {
                diagnostics.push(warn(
                    &source,
                    format!("settings file is not valid JSON ({e}); ignored"),
                ));
                Value::Null
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => {
            diagnostics.push(warn(&source, format!("cannot read settings file: {e}")));
            Value::Null
        }
    }
}

/// The current content of a settings file as an object, for a read-modify-write.
fn read_object_for_write(path: &Path) -> Result<Map<String, Value>, OxaError> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Map::new()),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(Value::Null) => Ok(Map::new()),
            Ok(_) => Err(OxaError::new(
                codes::INVALID,
                format!("{} must contain a JSON object", path.display()),
            )
            .with_hint("Fix or remove the file, then set the value again.")),
            Err(e) => Err(OxaError::new(
                codes::INVALID,
                format!("{} is not valid JSON: {e}", path.display()),
            )
            .with_hint("Fix or remove the file, then set the value again.")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(OxaError::io(&format!("read {}", path.display()), &e)),
    }
}

/// Write `text` through a fresh, uniquely named sibling file (created exclusively with `mode`,
/// synced, then renamed over `path`), so a crash never leaves a half-written file and the write
/// never goes through a file that was already there.
fn write_atomic(path: &Path, text: &str, mode: u32) -> Result<(), OxaError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxaError::io(&format!("create {}", parent.display()), &e))?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp-{}-{nonce}-{count}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| OxaError::io(&format!("create {}", tmp.display()), &e))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| OxaError::io(&format!("write {}", tmp.display()), &e))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| OxaError::io(&format!("replace {}", path.display()), &e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else if let Some(parent) = path.parent() {
        // Best effort: make the rename itself durable.
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }
    result
}

/// `~` and `~/rest` expand to `home`; a relative path resolves from `base`.
fn resolve_user_path(raw: &str, home: &Path, base: &Path) -> PathBuf {
    let path = if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Set `key` (dotted) in `root`, creating or replacing intermediate objects as needed.
fn set_nested(root: &mut Map<String, Value>, key: &str, value: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur: &mut Map<String, Value> = root;
    for part in parents {
        let slot = cur
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        cur = match slot {
            Value::Object(m) => m,
            _ => return,
        };
    }
    cur.insert((*last).to_string(), value);
}
