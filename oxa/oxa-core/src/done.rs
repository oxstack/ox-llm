//! `done` setting (issue #104): shell commands oxa runs itself before it reports completion.
//! A run that edited files (`edit` / `write` succeeded, or a `bash`, sub-agent, MCP or extension
//! tool call changed the work folder) and is about to stop runs them in the session sandbox; a failure goes back to the model as a user
//! message, at most [`MAX_ROUNDS`] times. An entry `in <dir>: <command>` runs in `<dir>`, a folder
//! of the session folder, instead of in the session folder itself.
//!
//! ## Artifacts already verified in this run (#2295)
//!
//! When a `bash` call fails and a `done` check passes after it in the same batch, and a file
//! this run wrote successfully is still there, the repair message names those files before the
//! failure text: they are the last verified result, so the fix is the part that differs (a
//! diff against it), not a rebuild from scratch. The files are the batch's own successful
//! `write`/`edit` paths: relative to the session folder (an absolute one inside it works),
//! deduplicated at each file's last write, newest [`ARTIFACT_SHOWN`], each at most
//! [`ARTIFACT_PATH_CAP`] bytes, nothing outside the folder. No file content is read, no lookup
//! runs, a path that is gone is dropped quietly, and nothing is refused or asked: without a
//! failed `bash` call and a passing check in the same batch, the repair text is unchanged. A run
//! with no `done` checks to run (or a project's `done` in an untrusted folder) keeps nothing.
use crate::exec::CallResult;
use crate::SessionState;
use oxa_proto::{
    shown_why, AgentEvent, EntryBody, EventMessage, EventSink, ExecSpec, Level, OxaError, ToolCall, Ui,
};
use oxa_tools::strip_ansi;
use std::path::{Component, Path, PathBuf};

/// Automatic fix rounds after a failed check; the next failure ends the run.
const MAX_ROUNDS: u32 = 2;
/// Seconds one command may run.
const TIMEOUT_S: u32 = 600;
/// Bytes of output kept per command (the tail).
const OUT_CAP: usize = 4096;

pub(crate) enum CheckObservation {
    Passed,
    Failed {
        identity: String,
        rounds: u32,
        evidence: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct DoneState {
    /// A file tool succeeded since the last check.
    dirty: bool,
    rounds: u32,
    pub(crate) observation: Option<CheckObservation>,
    /// Fingerprint of the work folder at the last look (`None`: too big to look at).
    tree: Option<u64>,
    /// The "prove it" state of the run (B4a).
    pub(crate) proof: Proof,
    /// The file deliverables the model declared in this run, oldest first (#2293 A).
    deliverables: Vec<Deliverable>,
    /// T6: the tool the task's acceptance names ([`crate::think::declared_verifier`]) and when
    /// this run first ran it (`None`: no tool named, or never run).
    pub(crate) toolchain: Option<Toolchain>,
    /// T6: the first [`TRIES_SHOWN`] commands that named the task's tool without running it, to
    /// show beside the question.
    toolchain_tries: Vec<String>,
    /// Each finish fact gets one nudge turn per run ([`finish_decision`]).
    nudged_unproven: bool,
    nudged_jobs: bool,
    nudged_image: bool,
    nudged_toolchain: bool,
    /// The run consulted an image file (`read` on an image, placeholder included).
    image_read: bool,
    /// A check ran after the last image read. Before the image nudge only a recognizable
    /// passing check counts; after it any successful command does ([`Self::note_proof`]).
    image_checked: bool,
    /// Distinct paths the run's successful `edit`/`write` calls named, in first-use order
    /// (#2297). Tool-observed only; reported once at the end as acceptance evidence.
    changed_paths: Vec<String>,
    /// #2295: the files the current batch wrote successfully, in call order.
    written: Vec<String>,
    /// #2295: a `bash` call of the current batch failed.
    failed_command: bool,
}
/// T6: the acceptance tool a task names, and the proof state when a bash call first ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Toolchain {
    pub(crate) tool: String,
    /// The first run of the tool, as `proof.passed_at_all()` then. Acceptance evidence only when it is
    /// true (that call, or one after it, passed a check).
    first_use: Option<bool>,
}

impl Toolchain {
    /// The check that could not run as the task asks, so what the run checked is a proxy (or
    /// nothing): the tool never ran, or only ran without a passing check after it. Nothing is
    /// asked of a run that changed no file and ran no check (a read-only or failing run).
    pub(crate) fn unverified(&self, proof: &Proof) -> Option<&str> {
        let used = self.first_use.is_some_and(|checked| checked);
        if used || !(proof.edited() || proof.passed_at_all()) {
            return None;
        }
        Some(self.tool.as_str())
    }
}

impl DoneState {
    /// At run start: the tool the task's acceptance names, if a `bash` tool is declared to run it
    /// (otherwise nothing can run it, and nothing is asked of the run). `planning` is false in a
    /// goal-engaged run, whose `task` is the model's own pursuit and not the user's ask.
    pub(crate) fn begin_toolchain(&mut self, st: &SessionState, task: &str, planning: bool) {
        let has_bash = st.active.specs().iter().any(|t| t.name == "bash");
        self.toolchain = if has_bash && planning {
            crate::think::declared_verifier(task)
        } else {
            None
        }
        .map(|tool| Toolchain {
            tool,
            first_use: None,
        });
    }
    /// T6: whether this batch's commands run the acceptance tool the task names, and which
    /// commands the run named it in.
    pub(crate) fn note_toolchain(&mut self, calls: &[ToolCall]) {
        let Some(tool) = self.toolchain.as_ref().map(|t| t.tool.clone()) else {
            return;
        };
        for call in calls.iter().filter(|c| c.name == "bash") {
            let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.args_raw) else {
                continue;
            };
            let Some(command) = args.get("command").and_then(serde_json::Value::as_str) else {
                continue;
            };
            // Every program position of the command is a call, as `executed_check` reads it.
            // Only the first call is kept: a later check does not prove an earlier call's path.
            let ran = program_names(command).iter().any(|p| runs_tool(p, &tool));
            let first_use = self.toolchain.as_ref().map(|t| t.first_use);
            if ran {
                if first_use == Some(None) {
                    if let Some(state) = self.toolchain.as_mut() {
                        state.first_use = Some(self.proof.passed_at_all());
                    }
                }
            } else if first_use == Some(None) && self.toolchain_tries.len() < TRIES_SHOWN {
                if let Some(attempt) = crate::think::attempt_at(command, &tool) {
                    self.toolchain_tries.push(one_line(&attempt, TRIES_LINE));
                }
            }
        }
    }
}

/// The programs of `cmd`, from every program position (after a separator, `VAR=x`, `time`,
/// `timeout 60`), plus the interpreter of a leading `#!/usr/bin/env` shebang.
fn program_names(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let shebang = cmd
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("#!"))
        .and_then(|l| l.split_ascii_whitespace().nth(1));
    out.extend(shebang.map(str::to_string));
    for words in crate::think::simple_commands(cmd) {
        if let Some(program) = crate::think::program_words(&words).first() {
            out.push(program.clone());
        }
    }
    out
}

/// Whether program `program` is the tool `tool` (`/usr/bin/python3` is `python3`), with a
/// versioned name also accepting its bare form: the system's `python3` runs when `python3.12` is
/// not installed.
fn runs_tool(program: &str, tool: &str) -> bool {
    let program = program.split('/').next_back().unwrap_or(program);
    let bare = tool
        .split_once('.')
        .filter(|(_, version)| version.chars().all(|c| c.is_ascii_digit()))
        .map_or(tool, |(name, _)| name);
    program == tool || program == bare
}

/// Commands of an unused tool the nudge shows.
const TRIES_SHOWN: usize = 3;
/// Bytes of one such command the nudge shows.
const TRIES_LINE: usize = 120;

/// `text` as one bounded line.
fn one_line(text: &str, cap: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.len() <= cap {
        return line.to_string();
    }
    let mut cut = cap;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &line[..cut])
}

/// #2293 A: how many declared file deliverables one run keeps; the oldest goes beyond this.
const MAX_DELIVERABLES: usize = 8;
/// #2293 A: the longest shown path a declaration records (the tool refuses a longer one).
const MAX_DELIVERABLE_PATH: usize = 4096;

/// #2293 A: one file the request asks for, declared by the model's `deliverable` call. Evidence
/// only: a missing file is reported to the model, never refused, and changes no exit code.
#[derive(Debug, Clone)]
pub(crate) struct Deliverable {
    /// The path as the model wrote it; the receipt shows this.
    shown: String,
    /// The guarded absolute location the declaring call checked.
    abs: PathBuf,
}

impl Deliverable {
    /// A fresh look at the same location: the end-of-run receipt never repeats the declaring
    /// call's observation, which a later write or restart has made stale.
    fn observe(&self) -> (oxa_tools::DeliverableState, Option<u64>) {
        oxa_tools::observe_deliverable(&self.abs)
    }
}

/// #2293 A: the run's completion record for its declared files.
pub(crate) struct Evidence {
    /// One line per deliverable, with the view and the check's time.
    pub(crate) text: String,
    /// At least one declared file is not on disk: the notice is a warning, not information.
    pub(crate) incomplete: bool,
}

/// Bytes of one file path in the verified-artifact line (#2295); longer paths are left out.
const ARTIFACT_PATH_CAP: usize = 200;
/// Files named by the verified-artifact line (#2295), the newest kept.
const ARTIFACT_SHOWN: usize = 8;

/// #2295: the line that opens a failed-check repair with the files this run wrote and a check
/// just passed over. An absolute path inside `cwd` is shown relative to it; a path outside it,
/// with a control character, over [`ARTIFACT_PATH_CAP`] bytes or no longer on disk is left out,
/// returning `None` when nothing is left.
fn artifact_line(cwd: &Path, written: &[String]) -> Option<String> {
    let mut shown: Vec<&str> = Vec::new();
    for raw in written {
        let inside = Path::new(raw)
            .strip_prefix(cwd)
            .unwrap_or_else(|_| Path::new(raw));
        let Some(rel) = inside.to_str() else {
            continue;
        };
        // An absolute path outside the folder, a control character, an over-long path, or a
        // file a later command deleted is not a result to compare against.
        if inside == Path::new(raw) && Path::new(raw).is_absolute()
            || rel.is_empty()
            || rel.len() > ARTIFACT_PATH_CAP
            || rel.chars().any(char::is_control)
            || !cwd.join(rel).exists()
        {
            continue;
        }
        if !shown.contains(&rel) {
            shown.push(rel);
        }
    }
    if shown.is_empty() {
        return None;
    }
    let from = shown.len().saturating_sub(ARTIFACT_SHOWN);
    Some(format!(
        "Not re-derived: these files were written successfully in this run and the check passed \
         after their last write, so they are this run's verified result: {}. The failure is the \
         part that differs: compare it with that result and change only what differs instead of \
         rebuilding the files from scratch.",
        shown[from..].join(", ")
    ))
}

/// #2295: the path argument of a successful `write`/`edit` call (`None`: not a string).
fn written_path(call: &ToolCall) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&call.args_raw)
        .ok()?
        .get("path")?
        .as_str()
        .map(str::to_owned)
}

/// #2295: whether this run has `done` checks to run. An untrusted project's `done` is never
/// merged into the settings, so it is not in the list and nothing is kept for it.
pub(crate) fn checks_run(st: &SessionState) -> bool {
    !st.settings.done_commands().is_empty()
}

/// Whether the run changed files and no check has passed since the last change (B4a). Only
/// tracks the state: nothing is asked from here.
#[derive(Default)]
pub(crate) struct Proof {
    /// A file changed in this run: a successful edit or write, or a bash-like call that changed
    /// the work folder.
    edited: bool,
    /// A passing check ran after the last change.
    checked: bool,
    /// Any passing check ran in this run ([`Proof::passed_at_all`]).
    passed_any: bool,
}

impl Proof {
    fn changed(&mut self) {
        self.edited = true;
        self.checked = false;
    }

    fn passed(&mut self) {
        self.checked = true;
        self.passed_any = true;
    }

    /// Any passing check ran in this run (T6 `Toolchain::unverified`).
    pub(crate) fn passed_at_all(&self) -> bool {
        self.passed_any
    }

    /// A passing check ran after the last change (the verified-artifact line, #2295).
    pub(crate) fn checked(&self) -> bool {
        self.checked
    }

    /// A file changed in this run (FH-N2: a closing question after edits is not an open task).
    pub(crate) fn edited(&self) -> bool {
        self.edited
    }

    /// The run changed files and no check has passed since the last change. Read by
    /// [`finish_decision`].
    pub(crate) fn unproven(&self) -> bool {
        self.edited && !self.checked
    }
}

/// Seconds a git probe may run before the fingerprint gives up (`None`: checks stay as they were).
const PROBE_TIMEOUT_S: u64 = 20;

/// Hash of the work folder's git file list (index entries and untracked files) with each file's
/// size, times, inode and mode. Outside a git repo, without git, or on a timeout it is a bounded
/// walk of the folder ([`walk_fingerprint`]).
fn tree_fingerprint(cwd: &Path) -> Option<u64> {
    git_fingerprint(cwd).or_else(|| walk_fingerprint(cwd))
}

/// Entries the non-git walk looks at before it gives up (`None`: no checks from tree changes).
const WALK_CAP: usize = 20_000;

/// Hash of every readable file's relative path, size and modification time under `cwd`, plus
/// contents for regular files up to 1 MiB. Does not follow symlinks or descend
/// into `.git`, `node_modules`, `target` or unreadable subdirectories. `None` above [`WALK_CAP`]
/// entries or when `cwd` cannot be read.
fn walk_fingerprint(cwd: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut seen = 0usize;
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) if dir == cwd => return None,
            Err(_) => continue,
        };
        let mut entries = walk_entries(entries, &mut seen)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            name.hash(&mut h);
            dir.strip_prefix(cwd).ok().hash(&mut h);
            m.len().hash(&mut h);
            m.modified().ok().hash(&mut h);
            if m.is_file() && m.len() <= (1 << 20) {
                std::fs::read(e.path()).ok().hash(&mut h);
            }
            if m.is_dir() && !matches!(name.to_str(), Some(".git" | "node_modules" | "target")) {
                stack.push(e.path());
            }
        }
    }
    Some(h.finish())
}

fn walk_entries<T>(entries: impl Iterator<Item = std::io::Result<T>>, seen: &mut usize) -> Option<Vec<T>> {
    let mut collected = Vec::new();
    for entry in entries {
        *seen += 1;
        if *seen > WALK_CAP {
            return None;
        }
        if let Ok(entry) = entry {
            collected.push(entry);
        }
    }
    Some(collected)
}

/// The git part of [`tree_fingerprint`]; `None` outside a git repo, without git, or on a timeout.
/// The probes run on the host, so they only list files (`ls-files`): nothing that can start a
/// repository-configured program (`status` and `diff` run clean filters, textconv, external
/// diff). Files are stat-ed here instead of diffed by git. A same-size in-place rewrite that
/// restores mtime within one ctime tick can be missed on coarse-timestamp filesystems; file
/// tools mark edits directly, independently of this best-effort bash probe.
fn git_fingerprint(cwd: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let mut child = std::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut v);
            v
        });
        let t0 = std::time::Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().ok()? {
                break s;
            }
            if t0.elapsed().as_secs() >= PROBE_TIMEOUT_S {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let v = reader.join().ok()?;
        status.success().then_some(v)
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // `-s`: mode, object id and stage of every index entry (staging changes the hash).
    git(&["ls-files", "-z", "-s"])?.hash(&mut h);
    let files = git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])?;
    for name in files.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = cwd.join(std::ffi::OsStr::from_bytes(name));
        name.hash(&mut h);
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                m.len().hash(&mut h);
                m.modified().ok().hash(&mut h);
                // Inode, mode and change time catch a rewrite that keeps size and mtime.
                m.ino().hash(&mut h);
                m.mode().hash(&mut h);
                (m.ctime(), m.ctime_nsec()).hash(&mut h);
                // Do not read file contents: the metadata avoids repository-wide I/O and
                // stays stable as unchanged files age, unlike a recency-gated content hash.
            }
            Err(_) => 0u8.hash(&mut h),
        }
    }
    Some(h.finish())
}

impl DoneState {
    fn observe_tree(&mut self, now: Option<u64>, file_edit: bool) {
        // In a mixed batch, note_proof has already used call order to decide whether a check
        // followed the edit. Keep that decision and only synchronize the observed tree here.
        if file_edit {
            self.tree = now;
        } else if now != self.tree {
            self.dirty = true;
            self.proof.changed();
            self.tree = now;
        }
    }

    pub(crate) fn note_edits(&mut self, results: &[CallResult]) {
        if results
            .iter()
            .any(|r| !r.is_error && (r.tool_name == "edit" || r.tool_name == "write"))
        {
            self.dirty = true;
        }
    }

    /// One batch, in call order (`calls` and `results` line up): a successful edit or write is a
    /// change; a successful bash call that runs a test or check after the last change proves it.
    /// A failed command is kept for the next repair (#2295); [`Self::note_artifacts`] then keeps
    /// the batch's written files.
    pub(crate) fn note_proof(&mut self, calls: &[ToolCall], results: &[CallResult]) {
        self.failed_command = false;
        for (call, res) in calls.iter().zip(results) {
            if res.is_error {
                // A failed command is the failure signal; a failed edit or write changed nothing.
                self.failed_command |= res.tool_name == "bash";
                continue;
            }
            match res.tool_name.as_str() {
                "edit" | "write" => {
                    self.proof.changed();
                    if let Some(path) = written_path(call) {
                        if !self.changed_paths.contains(&path) {
                            self.changed_paths.push(path.clone());
                        }
                        self.written.push(path);
                    }
                }
                "read" => {
                    // An image read opens the cross-check fact again, even after a check.
                    if res.ui["imageRead"] == true || !res.msg.images.is_empty() {
                        self.image_read = true;
                        self.image_checked = false;
                    }
                }
                "deliverable" => self.note_deliverable(&call.args_raw, &res.ui),
                "bash" => {
                    let args = serde_json::from_str::<serde_json::Value>(&call.args_raw).ok();
                    let exited_zero = res.ui["exit"].as_i64() == Some(0) && res.ui["timedOut"] != true;
                    // A detached part ran without its output or exit status being seen.
                    let detached = res.ui["detached"] == true;
                    if exited_zero && !detached {
                        let is_check = args
                            .as_ref()
                            .is_some_and(|args| crate::think::executed_check(&call.name, args).is_some());
                        if is_check {
                            self.proof.passed();
                        }
                        // After the one image nudge, any successful command counts as the
                        // cross-check the model was asked to run: a render-and-diff or an
                        // invariant probe is not a recognizable test runner.
                        if is_check || self.nudged_image {
                            self.image_checked = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// #2295: with `keep` (the run has `done` checks to run, [`checks_run`]), keep this batch's
    /// successful `write`/`edit` paths as the verified result a repair names, newest
    /// [`ARTIFACT_SHOWN`], each file once at its last write. The failure signal and the passing
    /// check are judged where the check runs ([`Self::verified_artifacts`]).
    pub(crate) fn note_artifacts(&mut self, keep: bool) {
        if !keep {
            self.written.clear();
            return;
        }
        let mut last: Vec<&str> = Vec::new();
        for path in &self.written {
            last.retain(|p| *p != path.as_str());
            last.push(path);
        }
        let from = last.len().saturating_sub(ARTIFACT_SHOWN);
        self.written = last[from..].iter().map(|p| (*p).to_string()).collect();
    }

    /// #2295: the verified-artifact line for a failed-check repair, only when the batch that
    /// ran the check also failed a command after writing the file; older writes are the parts a
    /// check or a formatter may already have rewritten, so they are not claimed here.
    pub(crate) fn verified_artifacts(&self, cwd: &Path) -> Option<String> {
        if !self.failed_command || !self.proof.checked() {
            return None;
        }
        artifact_line(cwd, &self.written)
    }

    /// Start of a run: remember the tree, so a later `bash` change can be told apart.
    pub(crate) fn begin(&mut self, st: &SessionState) {
        // Prove-it and the image cross-check need the fingerprint too, so a bash-made edit
        // counts as a change.
        if !st.settings.done_commands().is_empty() || prove_it(st) || image_cross_check(st) {
            self.tree = tree_fingerprint(&st.cwd);
        }
    }

    /// A successful `deliverable` call: remember the file it named and what it found. The
    /// typed `ui` of the call is the record; a result without it (an older session, a forged
    /// text) declares nothing. A later call for the same location replaces the earlier one.
    fn note_deliverable(&mut self, args_raw: &str, ui: &serde_json::Value) {
        let Ok(args) = serde_json::from_str::<serde_json::Value>(args_raw) else {
            return;
        };
        let (Some(shown), Some(abs)) = (args["path"].as_str(), ui["abs"].as_str()) else {
            return;
        };
        let declared = ui["deliverable"] == true
            && ui["state"]
                .as_str()
                .and_then(oxa_tools::DeliverableState::parse)
                .is_some();
        if !declared || shown.len() > MAX_DELIVERABLE_PATH {
            return;
        }
        let abs = PathBuf::from(abs);
        let item = Deliverable {
            shown: shown.to_string(),
            abs,
        };
        self.deliverables.retain(|d| d.abs != item.abs);
        self.deliverables.push(item);
        if self.deliverables.len() > MAX_DELIVERABLES {
            self.deliverables.remove(0);
        }
    }

    /// The completion record of the run's declared files, or `None` when it declared none (an
    /// unrelated task reads nothing and costs nothing here). Each file is looked at once more,
    /// now: `verified-now`, `missing`, or `unverified` with the reason. Never a gate.
    pub(crate) fn evidence(&self) -> Option<Evidence> {
        if self.deliverables.is_empty() {
            return None;
        }
        let check = oxa_session::local_time(now_secs())
            .map(|t| format!("{} {} {}", t.date, t.hm, t.zone_label()))
            .unwrap_or_else(|| now_secs().to_string());
        let mut lines = Vec::with_capacity(self.deliverables.len());
        let mut incomplete = false;
        for d in &self.deliverables {
            let (state, bytes) = d.observe();
            let bytes = bytes.unwrap_or(0);
            let (verdict, detail) = match state {
                oxa_tools::DeliverableState::Present => ("verified-now", format!("present, {bytes} bytes")),
                oxa_tools::DeliverableState::Empty => {
                    incomplete = true;
                    ("missing", "present but empty".to_string())
                }
                oxa_tools::DeliverableState::Missing => {
                    incomplete = true;
                    ("missing", "not on disk".to_string())
                }
                oxa_tools::DeliverableState::NotFile => {
                    incomplete = true;
                    ("missing", "not a regular file".to_string())
                }
                oxa_tools::DeliverableState::Unverified => {
                    incomplete = true;
                    ("unverified", "could not be checked now".to_string())
                }
            };
            lines.push(format!("{}: {verdict}, {detail}", d.shown));
        }
        let mut text = format!("Deliverable evidence (checked {check}, run folder):\n");
        for line in &lines {
            text.push_str("  ");
            text.push_str(line);
            text.push('\n');
        }
        if incomplete {
            text.push_str(
                "A declared file is not usable: write it before you finish, and say so if it cannot be.\n",
            );
        } else {
            text.push_str("Existence and size only; the content was checked by the declaring call.\n");
        }
        Some(Evidence {
            text: text.trim_end().to_string(),
            incomplete,
        })
    }

    /// After a batch with any tool that may change files (`bash`, sub-agent, MCP and extension
    /// tools; not the read-only built-ins): a changed work folder counts as an edit.
    pub(crate) fn note_bash(&mut self, st: &SessionState, results: &[CallResult]) {
        const READ_ONLY: [&str; 5] = ["read", "grep", "find", "ls", "deliverable"];
        let file_edit = results
            .iter()
            .any(|r| !r.is_error && (r.tool_name == "edit" || r.tool_name == "write"));
        // Already dirty and unproven: a look at the tree cannot change the answer.
        if (self.dirty && !self.proof.checked)
            || self.tree.is_none()
            || results.iter().all(|r| READ_ONLY.contains(&r.tool_name.as_str()))
        {
            // File tools mark the proof directly, but their successful edit must also advance
            // the fingerprint. Otherwise a passing check in the next batch compares with the
            // pre-edit tree and incorrectly treats the already-observed edit as a later change.
            if file_edit && self.tree.is_some() {
                self.observe_tree(tree_fingerprint(&st.cwd), true);
            }
            return;
        }
        let now = tree_fingerprint(&st.cwd);
        self.observe_tree(now, file_edit);
    }
}

/// #2293 A: the wrap-up message with the declared files that are not on disk. A last turn is
/// where writing a usable answer still helps: the advice is in the tool's own result while the
/// run has time, and here at the limit. Nothing is added when every declared file is present,
/// so an ordinary wrap-up reads as before.
pub(crate) fn wrap_up_message(state: &DoneState) -> String {
    let mut missing: Vec<String> = Vec::new();
    for d in &state.deliverables {
        if d.observe().0 != oxa_tools::DeliverableState::Present {
            missing.push(d.shown.clone());
        }
    }
    let mut text = crate::prompt::WRAP_UP.to_string();
    if missing.is_empty() {
        return text;
    }
    let extra = missing.len().saturating_sub(MAX_WRAP_UP_NAMES);
    missing.truncate(MAX_WRAP_UP_NAMES);
    let mut names = missing.join(", ");
    if extra > 0 {
        names.push_str(&format!(" and {extra} more"));
    }
    text.push_str(&format!(
        "\n\nA file deliverable is not on disk yet or is empty: {names}. Write the best answer you have with the write tool now, then check it once with the deliverable tool."
    ));
    text
}

/// #2293 A: how many deliverable names the wrap-up message lists before `and N more`.
const MAX_WRAP_UP_NAMES: usize = 4;

/// #2293 A: show the run's completion record once, at the end of the run (it also covers a
/// wrapped-up, aborted or failed run). A missing declaration is never invented: only the typed
/// `ui` of a `deliverable` call records one. A store failure loses the entry, not the notice.
pub(crate) fn report_deliverables(st: &mut SessionState, state: &DoneState, sink: &mut dyn EventSink) {
    let Some(evidence) = state.evidence() else {
        return;
    };
    let _ = crate::agent_loop::persist(
        st,
        EntryBody::Shown {
            text: evidence.text.clone(),
            why: shown_why::NOTICE,
        },
        sink,
    );
    sink.emit(&AgentEvent::Notice {
        level: if evidence.incomplete {
            Level::Warning
        } else {
            Level::Info
        },
        text: evidence.text,
    });
}

/// Seconds since the epoch, for the receipt's check time (an observation, not a guarantee).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `oxa.done.proveIt` (user settings, default off until the extra turn is the default for the
/// scripted suites too): a run that changed files and ran no passing check afterwards gets one
/// turn asking for proof.
fn prove_it(st: &SessionState) -> bool {
    st.settings.user_bool("oxa.done.proveIt", false)
}

/// `oxa.done.imageCrossCheck` (user settings, default on): a run that read an image file and
/// changed files, with no check since the image was read, gets one turn asking for an
/// independent cross-check of its reading, or for the unverified claim to be named.
fn image_cross_check(st: &SessionState) -> bool {
    st.settings.user_bool("oxa.done.imageCrossCheck", true)
}

/// What is still open when a run or goal is about to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FinishFact {
    /// Files changed and no passing check ran since.
    Unproven,
    /// Background jobs (ids) still running.
    JobsRunning(Vec<String>),
    /// T6: the tool the task's acceptance names never ran (tools tried).
    ToolchainUnused(String, Vec<String>),
    /// An image was read, files changed, and no check ran since the image was read.
    ImageUnverified,
}

pub(crate) enum FinishDecision {
    /// The run may end; these facts are still true (warn and record, never a block).
    End { unresolved: Vec<FinishFact> },
    /// One more user turn, once per fact and run.
    Nudge(String),
    /// T6: the task names a tool for its acceptance and this run never ran it. When
    /// `oxa.done.proveIt` is on, this supersedes that setting's nudge (asking about the
    /// substitute is the question that names what remains unverified) and is that nudge.
    Verifier { nudge: String, unproven: bool },
}

/// T6: the verifier fact of this decision, if any.
fn verifier_fact(state: &DoneState) -> Option<FinishFact> {
    let tool = state
        .toolchain
        .as_ref()
        .and_then(|t| t.unverified(&state.proof))?;
    Some(FinishFact::ToolchainUnused(
        tool.to_string(),
        state.toolchain_tries.clone(),
    ))
}

/// The one decision before a run or goal ends. Each fact that is true now gets one nudge turn in
/// this run; after that the run ends and the caller records what was left ([`record_end`]).
/// Nothing is asked of a cancelled run, a failed done check, a blocked goal or a read-only run.
/// With `oxa.done.proveIt` on, the turn before that is [`FinishDecision::Verifier`] once, which
/// is the proof nudge (or the task-tool question, which asks what the proof would name).
pub(crate) fn finish_decision(
    st: &SessionState,
    state: &mut DoneState,
    goal_completion: bool,
) -> FinishDecision {
    let none = || FinishDecision::End {
        unresolved: Vec::new(),
    };
    if st.shared.cancel.is_cancelled()
        || st.stats.done_failed
        || st
            .goal
            .as_ref()
            .is_some_and(|g| g.snapshot().status == oxa_session::GoalStatus::Blocked)
    {
        return none();
    }
    let mut facts = Vec::new();
    if prove_it(st) && state.proof.unproven() {
        facts.push(FinishFact::Unproven);
    }
    if image_cross_check(st) && state.image_read && state.proof.edited() && !state.image_checked {
        facts.push(FinishFact::ImageUnverified);
    }
    let headless = crate::is_headless();
    if st.settings.user_bool("oxa.done.waitForJobs", true) && (goal_completion || headless) {
        let running: Vec<String> = st
            .jobs
            .list()
            .into_iter()
            .filter(|j| j.status == oxa_tools::JobStatus::Running)
            .map(|j| j.id)
            .collect();
        if !running.is_empty() {
            facts.push(FinishFact::JobsRunning(running));
        }
    }
    facts.extend(verifier_fact(state));
    for fact in &facts {
        match fact {
            FinishFact::Unproven if !state.nudged_unproven && state.toolchain.is_none() => {
                state.nudged_unproven = true;
                return FinishDecision::Nudge(crate::prompt::PROVE_IT_NUDGE.to_string());
            }
            // T6 (`oxa.done.proveIt` off): what the run checked is a substitute for the task's
            // own call path; the task's tool never ran.
            FinishFact::ToolchainUnused(tool, tries) if !state.nudged_toolchain && !prove_it(st) => {
                state.nudged_toolchain = true;
                return FinishDecision::Nudge(crate::prompt::toolchain_nudge(tool, tries));
            }
            FinishFact::JobsRunning(ids) if !state.nudged_jobs => {
                state.nudged_jobs = true;
                return FinishDecision::Nudge(crate::prompt::jobs_nudge(ids, headless));
            }
            FinishFact::ImageUnverified if !state.nudged_image => {
                state.nudged_image = true;
                return FinishDecision::Nudge(crate::prompt::IMAGE_CROSS_CHECK_NUDGE.to_string());
            }
            _ => {}
        }
    }
    // `oxa.done.proveIt`: one turn that supersedes both nudges when the task's tool never ran, and
    // that nudge itself when `proveIt` asked for proof and nothing else was asked.
    let unproven = state.proof.unproven();
    let asked = state.nudged_unproven || state.nudged_toolchain;
    if prove_it(st) && !asked {
        if let Some(FinishFact::ToolchainUnused(tool, tries)) = verifier_fact(state) {
            state.nudged_unproven = true;
            state.nudged_toolchain = true;
            let nudge = crate::prompt::toolchain_nudge(&tool, &tries);
            return FinishDecision::Verifier { nudge, unproven };
        }
        if unproven {
            state.nudged_unproven = true;
            let nudge = crate::prompt::PROVE_IT_NUDGE.to_string();
            return FinishDecision::Verifier { nudge, unproven };
        }
    }
    FinishDecision::End { unresolved: facts }
}

/// The run ends: keep what the nudges did not settle in the session stats, and say so once.
pub(crate) fn record_end(
    st: &mut SessionState,
    state: &DoneState,
    unresolved: &[FinishFact],
    sink: &mut dyn EventSink,
) -> Result<(), OxaError> {
    st.stats.proved = state.proof.edited().then(|| !state.proof.unproven());
    st.stats.jobs_left = 0;
    // Read once, as `proved` above: a proof or check that settles after this record does not
    // revise the claim frozen with it.
    st.stats.verifier = state
        .toolchain
        .as_ref()
        .and_then(|t| t.unverified(&state.proof))
        .map(str::to_string);
    st.stats.image_unverified = false;
    for fact in unresolved {
        let text = match fact {
            FinishFact::Unproven => "Run ended without a passing check after its last edit".to_string(),
            FinishFact::JobsRunning(ids) => {
                st.stats.jobs_left = ids.len() as u32;
                format!("Run ended with background jobs still running: {}", ids.join(" "))
            }
            FinishFact::ToolchainUnused(tool, _) => format!(
                "Run ended without running the tool this task names for its acceptance \
                 ({tool} never ran): the checks that ran are a substitute, not evidence for \
                 the task's own call path"
            ),
            FinishFact::ImageUnverified => {
                st.stats.image_unverified = true;
                "Run ended without an independent check of what it read from an image".to_string()
            }
        };
        crate::agent_loop::persist(
            st,
            EntryBody::Shown {
                text: text.clone(),
                why: shown_why::NOTICE,
            },
            sink,
        )?;
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text,
        });
    }
    if !state.changed_paths.is_empty() {
        crate::progress::notify(
            st,
            crate::Progress::FilesChanged {
                paths: &state.changed_paths,
            },
        );
    }
    Ok(())
}

/// Send a finish nudge as the next user message, with an info notice for the user.
pub(crate) fn send_nudge(
    st: &mut SessionState,
    text: String,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) -> Result<(), OxaError> {
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text: "The run is about to end with work not settled; asking the model to finish it first."
            .to_string(),
    });
    crate::agent_loop::push_user(st, text, sink, ui, run_msgs)
}

/// Run the checks when due. `Some(text)` = a failure to send to the model for another round.
pub(crate) fn check(
    st: &mut SessionState,
    state: &mut DoneState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Option<String> {
    state.observation = None;
    let cmds = st.settings.done_commands();
    if cmds.is_empty() || !state.dirty || st.shared.cancel.is_cancelled() {
        return None;
    }
    state.dirty = false;
    let _ = ui;
    let mut failed = String::new();
    let mut failed_checks = Vec::new();
    for cmd in &cmds {
        // A cancel stops the whole done phase: no further command, no notice, no repair text.
        if st.shared.cancel.is_cancelled() {
            return None;
        }
        let (ok, line, out) = run_one(st, cmd);
        if st.shared.cancel.is_cancelled() {
            return None;
        }
        let mut text = line.clone();
        if !out.is_empty() {
            text.push('\n');
            text.push_str(&out);
        }
        let _ = crate::agent_loop::persist(
            st,
            EntryBody::Shown {
                text: text.clone(),
                why: shown_why::NOTICE,
            },
            sink,
        );
        sink.emit(&AgentEvent::Notice {
            level: if ok { Level::Info } else { Level::Error },
            text: text.clone(),
        });
        if !ok {
            let (dir, command) = parse_entry(cmd);
            failed_checks.push((dir.unwrap_or("."), command));
            failed.push_str(&text);
            failed.push_str("\n\n");
        }
    }
    // A check may rewrite files (formatters, generators): that is not a model edit.
    if state.tree.is_some() {
        state.tree = tree_fingerprint(&st.cwd);
    }
    // What print mode reads at the end: the last check round failed and nothing fixed it.
    st.stats.done_failed = !failed.is_empty();
    if failed.is_empty() {
        state.observation = Some(CheckObservation::Passed);
        state.rounds = 0;
        state.proof.passed();
        return None;
    }
    state.rounds = state.rounds.saturating_add(1);
    // #2295: the batch's verified files open the repair. A later plain turn does not repeat it;
    // a further batch of tool calls ([`DoneState::note_artifacts`]) replaces it.
    let artifacts = state.verified_artifacts(&st.cwd);
    let repair = (state.rounds <= MAX_ROUNDS).then(|| {
        let failed = failed.trim_end();
        match artifacts {
            Some(line) => format!(
                "The project's done checks failed. Fix the problems, then finish.\n\n{line}\n\n{failed}"
            ),
            None => format!("The project's done checks failed. Fix the problems, then finish.\n\n{failed}"),
        }
    });
    state.observation = Some(CheckObservation::Failed {
        identity: serde_json::json!(failed_checks).to_string(),
        rounds: state.rounds,
        // A newly delivered suggestion can support one repair after the normal rounds.
        // Subsequent checks can settle or escalate that advice without another call.
        evidence: (state.rounds <= MAX_ROUNDS + 1).then_some(failed),
    });
    repair
}

pub(crate) fn inject(
    st: &mut SessionState,
    text: String,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) -> Result<(), OxaError> {
    crate::agent_loop::push_user(st, text, sink, ui, run_msgs)
}

/// A `done` entry: the command and the folder it runs in (as written), if the entry has the
/// `in <dir>: <command>` form. Anything else is a plain command.
pub(crate) fn parse_entry(entry: &str) -> (Option<&str>, &str) {
    let e = entry.trim();
    if let Some(rest) = e.strip_prefix("in ") {
        if let Some((dir, cmd)) = rest.split_once(':') {
            return (Some(dir.trim()), cmd.trim());
        }
    }
    (None, e)
}

/// `<dir>` of an `in <dir>: <command>` entry, relative to the canonical session folder
/// (empty/`.` = the folder itself). `..`, absolute paths and symlinks that leave the session
/// folder are refused. The `Err` is the reason.
fn resolve_dir(cwd: &Path, dir: &str) -> Result<PathBuf, String> {
    if dir.is_empty() {
        return Err("no folder after `in`".to_string());
    }
    let mut lexical = PathBuf::new();
    for c in cwd.join(dir).components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let canon_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let outside = || format!("folder {dir} is outside the project folder");
    if !lexical.starts_with(cwd) && !lexical.starts_with(&canon_cwd) {
        return Err(outside());
    }
    match lexical.canonicalize() {
        Ok(real) if !real.starts_with(&canon_cwd) => Err(outside()),
        Ok(real) if !real.is_dir() => Err(format!("{dir} is not a folder")),
        Ok(real) => {
            let rel = real.strip_prefix(&canon_cwd).unwrap_or(Path::new("."));
            Ok(if rel.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                rel.to_path_buf()
            })
        }
        Err(_) => Err(format!("folder {dir} does not exist")),
    }
}

/// `s` as one POSIX shell word.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run one entry; returns (passed, header line, trimmed output).
fn run_one(st: &SessionState, entry: &str) -> (bool, String, String) {
    let (dir, cmd) = parse_entry(entry);
    let label = match dir {
        Some(d) => format!("[{d}] {cmd}"),
        None => cmd.to_string(),
    };
    // The sandbox authority always roots at the original workspace (`st.cwd`); a subdir entry
    // is selected inside that view with a shell `cd`, so a swapped symlink cannot move the
    // writable bind root to an outside directory.
    let child = match dir {
        None => None,
        Some(d) => match resolve_dir(&st.cwd, d) {
            Ok(p) => Some(format!(
                "cd -- {} || exit 111\n",
                single_quote(&p.to_string_lossy())
            )),
            Err(why) => return (false, format!("done check: {label} FAILED (not run)"), why),
        },
    };
    if cmd.is_empty() {
        return (
            false,
            format!("done check: {label} FAILED (not run)"),
            "no command after `:`".to_string(),
        );
    }
    let shell = oxa_tools::ShellConfig {
        shell_path: st.settings.shell_path(&st.dirs.home),
        command_prefix: st.settings.shell_command_prefix(),
    };
    // The `cd` goes before the fully resolved shell text, so shellCommandPrefix and a custom
    // shell both start in the child folder.
    let command = match shell.command(cmd) {
        Ok(c) => format!("{}{c}", child.as_deref().unwrap_or("")),
        Err(e) => return (false, format!("done check: {label} FAILED (not run)"), e),
    };
    let spec = ExecSpec {
        command,
        cwd: st.cwd.clone(),
        writable: true,
        net: st.sandbox.network(),
        timeout_s: Some(TIMEOUT_S),
        env: st.tool_env.clone(),
    };
    let res = st.sandbox.run(&spec, &st.shared.cancel, &mut |_| {});
    let out = tail(&strip_ansi(&String::from_utf8_lossy(&res.output)));
    if res.exit == Some(0) && !res.timed_out {
        return (true, format!("done check: {label} ok"), String::new());
    }
    let why = if res.timed_out {
        "timed out".to_string()
    } else {
        match res.exit {
            Some(n) => format!("exit {n}"),
            None => "killed".to_string(),
        }
    };
    (false, format!("done check: {label} FAILED ({why})"), out)
}

fn tail(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= OUT_CAP {
        return s.to_string();
    }
    let mut from = s.len() - OUT_CAP;
    while !s.is_char_boundary(from) {
        from += 1;
    }
    format!("[output truncated]\n{}", &s[from..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    // `declaration` builds a test tool result; the crate's fakes are a dev-dependency.

    #[cfg(target_os = "linux")]
    #[test]
    fn fingerprint_does_not_read_tracked_or_untracked_file_contents() {
        // Linux counts read(2) bytes even when they hit the page cache. Use this thread's
        // counter so concurrent tests and the git pipe-reader threads cannot affect it.
        fn bytes_read() -> u64 {
            std::fs::read_to_string("/proc/thread-self/io")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("rchar: "))
                .unwrap()
                .parse()
                .unwrap()
        }

        let dir = std::env::temp_dir().join(format!("oxa-fp-no-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        const FILE_SIZE: usize = 64 * 1024;
        for name in ["tracked.txt", "untracked.txt"] {
            std::fs::write(dir.join(name), vec![b'x'; FILE_SIZE]).unwrap();
        }
        assert!(std::process::Command::new("git")
            .args(["add", "--", "tracked.txt"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());

        // Prove that the counter detects a real content read before testing the probe.
        let before = bytes_read();
        assert_eq!(std::fs::read(dir.join("tracked.txt")).unwrap().len(), FILE_SIZE);
        assert!(bytes_read() - before >= FILE_SIZE as u64);
        for age in [0, 3600] {
            let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            for name in ["tracked.txt", "untracked.txt"] {
                std::fs::File::options()
                    .write(true)
                    .open(dir.join(name))
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            }
            let before = bytes_read();
            let fingerprint = tree_fingerprint(&dir);
            let read = bytes_read() - before;
            assert!(fingerprint.is_some());
            // Allow the tiny /proc accounting read itself, but not even one fixture file.
            assert!(read < FILE_SIZE as u64, "probe read {read} bytes at age {age}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_stays_stable_as_an_unchanged_file_ages() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-aging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.join("a.txt"), "unchanged\n").unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        // Cross the former two-second content-hashing window without changing the file.
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert_eq!(before, tree_fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_sees_a_same_size_rewrite_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-rewrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let f = dir.join("a.txt");
        std::fs::write(&f, "before\n").unwrap();
        let file = std::fs::File::options().write(true).open(&f).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        file.set_modified(old).unwrap();
        let metadata = file.metadata().unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "after!\n").unwrap();
        file.set_modified(old).unwrap();
        assert_eq!(metadata.len(), file.metadata().unwrap().len());
        assert_eq!(
            metadata.modified().unwrap(),
            file.metadata().unwrap().modified().unwrap()
        );
        assert_ne!(before, tree_fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_sees_a_mode_change_of_an_old_untracked_file() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git init");
        let f = dir.join("a.sh");
        std::fs::write(&f, "echo hi\n").unwrap();
        // Old mtime: the change below cannot be seen through contents or mtime.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        assert_eq!(before, tree_fingerprint(&dir).unwrap(), "stable without changes");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(before, tree_fingerprint(&dir).unwrap(), "chmod is a change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_entries_stops_enumerating_at_the_remaining_budget() {
        let mut seen = WALK_CAP - 2;
        let mut reads = 0;
        let entries = (0..10).map(|n| {
            reads += 1;
            Ok(n)
        });
        assert!(walk_entries(entries, &mut seen).is_none());
        assert_eq!(reads, 3, "stop before collecting or sorting the whole directory");
    }

    #[test]
    fn walk_entries_counts_errors_against_the_budget() {
        let mut seen = WALK_CAP - 1;
        let entries = [Ok(1), Err(std::io::Error::other("unreadable entry")), Ok(2)];
        assert!(walk_entries(entries.into_iter(), &mut seen).is_none());
        assert_eq!(seen, WALK_CAP + 1);
    }

    #[test]
    fn walk_entries_accepts_exactly_the_budget() {
        let mut seen = WALK_CAP - 2;
        assert_eq!(
            walk_entries([Ok(1), Ok(2)].into_iter(), &mut seen),
            Some(vec![1, 2])
        );
        assert_eq!(seen, WALK_CAP);
        assert_eq!(
            walk_entries(std::iter::empty::<std::io::Result<()>>(), &mut seen),
            Some(vec![])
        );
    }

    #[test]
    fn walk_fingerprint_detects_same_size_same_mtime_edits() {
        let env = oxa_fake::TestEnv::new();
        let path = env.work().join("existing.txt");
        std::fs::write(&path, "before").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = walk_fingerprint(&env.work()).unwrap();
        assert_eq!(walk_fingerprint(&env.work()), Some(before));
        std::fs::write(&path, "after!").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_ne!(walk_fingerprint(&env.work()), Some(before));
    }

    #[test]
    fn edited_without_a_passing_check_is_unproven() {
        let mut proof = Proof::default();
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn a_passing_check_after_the_last_edit_is_proven() {
        let mut proof = Proof::default();
        proof.changed();
        proof.passed();
        assert!(!proof.unproven());
        // A new edit after the check makes the run unproven again.
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn a_check_before_the_last_edit_does_not_prove_it() {
        let mut proof = Proof::default();
        proof.changed();
        proof.passed();
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn observed_edit_fingerprint_does_not_undo_a_later_passing_check() {
        let mut state = DoneState {
            tree: Some(1),
            ..DoneState::default()
        };
        state.dirty = true;
        state.proof.changed();

        // This is the file-edit branch of note_bash: synchronize the edit while it is known.
        state.observe_tree(Some(2), true);
        state.proof.passed();
        state.observe_tree(Some(2), false);

        assert!(!state.proof.unproven());
    }

    #[test]
    fn read_only_runs_are_never_asked() {
        let mut proof = Proof::default();
        assert!(!proof.unproven());
        proof.passed();
        assert!(!proof.unproven());
    }

    fn bash(id: &str, command: &str, exit: i64) -> (ToolCall, CallResult) {
        let call = ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            args_raw: serde_json::json!({ "command": command }).to_string(),
        };
        let result = CallResult {
            msg: oxa_proto::WireMsg::tool_result(id, ""),
            tool_name: "bash".to_string(),
            is_error: false,
            ui: serde_json::json!({ "exit": exit, "timedOut": false }),
            warn: false,
        };
        (call, result)
    }

    #[test]
    fn a_batch_runs_the_task_tool_at_a_program_position_and_a_mention_is_no_run() {
        let mut state = DoneState {
            toolchain: Some(Toolchain {
                tool: "python3.12".to_string(),
                first_use: None,
            }),
            ..DoneState::default()
        };
        let (mention, _) = bash("c1", "echo python3.12", 0);
        state.note_toolchain(&[mention]);
        assert_eq!(state.toolchain.as_ref().unwrap().first_use, None);
        assert_eq!(state.toolchain_tries.len(), 1, "the mention is shown as a try");
        // The system's `python3` runs the task's `python3.12` when the exact name is absent,
        // also after a wrapper and a separator.
        let (words, _) = bash("c2", "cd src && timeout 5 /usr/bin/python3 check.py", 0);
        state.note_toolchain(&[words]);
        assert_eq!(state.toolchain.as_ref().unwrap().first_use, Some(false));
    }

    #[test]
    fn a_task_tool_is_evidence_only_with_a_passing_check_after_it() {
        let mut state = DoneState {
            toolchain: Some(Toolchain {
                tool: "python3".to_string(),
                first_use: None,
            }),
            ..DoneState::default()
        };
        let unverified = |state: &DoneState| {
            state
                .toolchain
                .as_ref()
                .unwrap()
                .unverified(&state.proof)
                .map(str::to_string)
        };
        // A read-only run is never asked, whatever it ran.
        assert_eq!(unverified(&state), None);
        // Edited, tried the tool, then a passing check: no evidence for that attempt.
        state.proof.changed();
        let (tried, _) = bash("c1", "python3 check.py", 0);
        state.note_toolchain(&[tried]);
        let (passed, result) = bash("c2", "cargo test", 0);
        state.note_proof(&[passed], &[result]);
        assert_eq!(state.toolchain.as_ref().unwrap().first_use, Some(false));
        assert_eq!(unverified(&state).as_deref(), Some("python3"));
        // A run whose tool call had a passing check after it has acceptance evidence.
        let mut proved = DoneState {
            toolchain: Some(Toolchain {
                tool: "python3".to_string(),
                first_use: None,
            }),
            ..DoneState::default()
        };
        proved.proof.changed();
        let (passed, result) = bash("c3", "cargo test", 0);
        proved.note_proof(&[passed], &[result]);
        let (call, _) = bash("c4", "python3 check.py", 0);
        proved.note_toolchain(&[call]);
        assert_eq!(proved.toolchain.as_ref().unwrap().first_use, Some(true));
        assert_eq!(unverified(&proved), None);
        proved.proof.changed();
        let (again, _) = bash("c5", "python3 check.py", 0);
        proved.note_toolchain(&[again]);
        assert_eq!(
            proved.toolchain.as_ref().unwrap().first_use,
            Some(true),
            "an earlier proof of the first call stands"
        );
        assert_eq!(unverified(&proved), None);
    }

    #[test]
    fn echoed_check_does_not_prove_an_edit() {
        let mut state = DoneState::default();
        state.proof.changed();
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            args_raw: serde_json::json!({ "command": "echo pytest" }).to_string(),
        };
        let result = CallResult {
            msg: oxa_proto::WireMsg::tool_result("call-1", "pytest"),
            tool_name: "bash".to_string(),
            is_error: false,
            ui: serde_json::json!({ "exit": 0, "timedOut": false }),
            warn: false,
        };

        state.note_proof(&[call], &[result]);

        assert!(state.proof.unproven());
    }

    fn write_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "write".to_string(),
            args_raw: serde_json::json!({ "path": path, "content": "x" }).to_string(),
        }
    }

    fn command_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            args_raw: serde_json::json!({ "command": command }).to_string(),
        }
    }

    fn result(id: &str, name: &str, exit: Option<i64>, is_error: bool) -> CallResult {
        CallResult {
            msg: oxa_proto::WireMsg::tool_result(id, "result"),
            tool_name: name.to_string(),
            is_error,
            ui: match exit {
                Some(exit) => serde_json::json!({ "exit": exit, "timedOut": false }),
                None => serde_json::Value::Null,
            },
            warn: false,
        }
    }

    /// One batch that leaves a failure with a verified result: a file written, a command that
    /// fails, a check that passes over it. `state` then holds what the repair may name.
    fn failed_batch(state: &mut DoneState, work: &Path, files: &[&str]) {
        for rel in files {
            // A path ending in `/` is a folder the run created; anything else is a file.
            let path = work.join(rel.trim_end_matches('/'));
            if rel.ends_with('/') {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::write(path, "written by this run").unwrap();
            }
        }
        let mut calls: Vec<ToolCall> = files
            .iter()
            .enumerate()
            .map(|(i, f)| write_call(&format!("w{i}"), f))
            .collect();
        calls.push(command_call("read", "sqlite3 db .dump"));
        calls.push(command_call("check", "cargo test"));
        let mut all: Vec<CallResult> = (0..files.len())
            .map(|i| result(&format!("w{i}"), "write", None, false))
            .collect();
        all.push(result("read", "bash", Some(1), true));
        all.push(result("check", "bash", Some(0), false));
        state.note_proof(&calls, &all);
        state.note_artifacts(true);
    }

    #[test]
    fn a_failure_with_a_passing_check_names_the_files_this_batch_wrote() {
        let env = oxa_fake::TestEnv::new();
        let work = env.work();
        let mut state = DoneState::default();
        failed_batch(&mut state, &work, &["decode.json", "out/"]);
        let line = state
            .verified_artifacts(&work)
            .expect("a verified result is named");
        assert!(line.starts_with("Not re-derived:"), "{line}");
        // The folder keeps the trailing slash the call named.
        assert!(line.contains("decode.json, out/."), "{line}");
        assert!(
            line.contains("change only what differs instead of rebuilding"),
            "{line}"
        );
    }

    fn bash_call(id: &str, command: &str, exit: i64) -> (ToolCall, CallResult) {
        (
            ToolCall {
                id: id.to_string(),
                name: "bash".to_string(),
                args_raw: serde_json::json!({ "command": command }).to_string(),
            },
            CallResult {
                msg: oxa_proto::WireMsg::tool_result(id, ""),
                tool_name: "bash".to_string(),
                is_error: false,
                ui: serde_json::json!({ "exit": exit, "timedOut": false }),
                warn: false,
            },
        )
    }

    fn image_read_call(id: &str, ui: serde_json::Value, with_part: bool) -> (ToolCall, CallResult) {
        let mut msg = oxa_proto::WireMsg::tool_result(id, "[image board.png 8x8]");
        if with_part {
            msg.images = vec![oxa_proto::WireImage {
                mime: "image/png".to_string(),
                base64: "eA==".to_string(),
            }];
        }
        (
            ToolCall {
                id: id.to_string(),
                name: "read".to_string(),
                args_raw: serde_json::json!({ "path": "board.png" }).to_string(),
            },
            CallResult {
                msg,
                tool_name: "read".to_string(),
                is_error: false,
                ui,
                warn: false,
            },
        )
    }

    #[test]
    fn an_image_read_opens_the_cross_check_fact_and_a_passing_check_settles_it() {
        let mut state = DoneState::default();
        let (read, res) = image_read_call("c1", serde_json::json!({ "imageRead": true }), false);
        state.note_proof(&[read], &[res]);
        assert!(state.image_read);
        assert!(!state.image_checked);
        let (call, res) = bash_call("c2", "cargo test", 0);
        state.note_proof(&[call], &[res]);
        assert!(state.image_checked);
    }

    #[test]
    fn an_image_part_counts_as_an_image_read_without_the_marker() {
        let mut state = DoneState::default();
        let (read, res) = image_read_call("c1", serde_json::Value::Null, true);
        state.note_proof(&[read], &[res]);
        assert!(state.image_read);
        assert!(!state.image_checked);
    }

    #[test]
    fn a_new_image_read_after_a_check_reopens_the_fact() {
        let mut state = DoneState::default();
        let (read, res) = image_read_call("c1", serde_json::json!({ "imageRead": true }), false);
        state.note_proof(&[read], &[res]);
        let (call, res) = bash_call("c2", "cargo test", 0);
        state.note_proof(&[call], &[res]);
        assert!(state.image_checked);
        // Same batch, call order: the check ran before this read, so the fact stays open.
        let (read2, res2) = image_read_call("c3", serde_json::json!({ "imageRead": true }), false);
        state.note_proof(&[read2], &[res2]);
        assert!(!state.image_checked);
    }

    #[test]
    fn after_the_image_nudge_any_successful_command_settles_it() {
        let mut state = DoneState {
            nudged_image: true,
            ..DoneState::default()
        };
        let (read, res) = image_read_call("c1", serde_json::json!({ "imageRead": true }), false);
        state.note_proof(&[read], &[res]);
        // A failed command is not a check.
        let (call, res) = bash_call("c2", "render-diff board.png out.png", 1);
        state.note_proof(&[call], &[res]);
        assert!(!state.image_checked);
        // A render-and-diff is not a recognizable test runner, but after the nudge the
        // successful command counts as the cross-check the model was asked to run.
        let (call, res) = bash_call("c3", "render-diff board.png out.png", 0);
        state.note_proof(&[call], &[res]);
        assert!(state.image_checked);
    }

    #[test]
    fn before_the_nudge_a_plain_command_does_not_settle_the_image_fact() {
        let mut state = DoneState::default();
        let (read, res) = image_read_call("c1", serde_json::json!({ "imageRead": true }), false);
        state.note_proof(&[read], &[res]);
        let (call, res) = bash_call("c2", "echo done", 0);
        state.note_proof(&[call], &[res]);
        assert!(!state.image_checked);
    }

    /// A `deliverable` call's arguments and typed `ui`, as the tool writes them.
    fn declaration(
        path: &str,
        state: &str,
        bytes: Option<u64>,
        env: &oxa_fake::TestEnv,
    ) -> (ToolCall, CallResult) {
        let abs = env.work().join(path);
        let call = ToolCall {
            id: "d1".to_string(),
            name: "deliverable".to_string(),
            args_raw: serde_json::json!({ "path": path }).to_string(),
        };
        let result = CallResult {
            msg: oxa_proto::WireMsg::tool_result("d1", "Deliverable check"),
            tool_name: "deliverable".to_string(),
            is_error: false,
            ui: serde_json::json!({
                "deliverable": true,
                "path": path,
                "abs": abs.to_str().unwrap(),
                "state": state,
                "bytes": bytes,
            }),
            warn: false,
        };
        (call, result)
    }

    #[test]
    fn a_declared_file_on_disk_is_verified_now_at_the_end_of_the_run() {
        let env = oxa_fake::TestEnv::new();
        env.write("work/move.txt", "e2e4\n");
        let (call, result) = declaration("move.txt", "present", Some(5), &env);
        let mut state = DoneState::default();
        state.note_proof(&[call], &[result]);
        let evidence = state.evidence().expect("a declaration is a record");
        assert!(
            evidence.text.contains("move.txt: verified-now, present, 5 bytes"),
            "{}",
            evidence.text
        );
        assert!(
            evidence.text.contains("checked "),
            "the receipt names its check time: {}",
            evidence.text
        );
        assert!(!evidence.incomplete);
    }

    #[test]
    fn a_file_gone_after_its_declaration_is_reported_missing_and_stays_out_of_the_gate() {
        let env = oxa_fake::TestEnv::new();
        let path = env.write("work/move.txt", "e2e4\n");
        let (call, result) = declaration("move.txt", "present", Some(5), &env);
        let mut state = DoneState::default();
        state.note_proof(&[call], &[result]);
        std::fs::remove_file(&path).unwrap();
        let evidence = state.evidence().expect("the declaration stands");
        assert!(
            evidence.text.contains("move.txt: missing, not on disk"),
            "{}",
            evidence.text
        );
        assert!(
            evidence.incomplete,
            "a missing file is a warning, not information"
        );
    }

    #[test]
    fn a_failed_batch_without_a_check_or_a_written_file_is_unchanged() {
        let env = oxa_fake::TestEnv::new();
        let mut state = DoneState::default();
        // A written file and a passing check, but no failure: nothing to compare against.
        failed_batch(&mut state, &env.work(), &["a.txt"]);
        state.failed_command = false;
        assert_eq!(state.verified_artifacts(&env.work()), None);
        // A failure and a written file, but no check passed after it.
        let mut state = DoneState::default();
        failed_batch(&mut state, &env.work(), &["b.txt"]);
        state.proof.checked = false;
        assert_eq!(state.verified_artifacts(&env.work()), None);
        // A failure and a check, but this batch wrote no file (an older write is not claimed).
        let mut state = DoneState::default();
        state.proof.changed();
        state.note_proof(
            &[command_call("c", "cargo test")],
            &[result("c", "bash", Some(0), false)],
        );
        state.note_artifacts(true);
        assert_eq!(state.written, Vec::<String>::new());
        assert_eq!(state.verified_artifacts(&env.work()), None);
        // A run with no `done` checks to run keeps nothing at all.
        let mut state = DoneState::default();
        state.proof.changed();
        state.note_proof(&[write_call("w", "c.txt")], &[result("w", "write", None, false)]);
        state.note_artifacts(false);
        state.proof.passed();
        assert_eq!(state.verified_artifacts(&env.work()), None);
    }

    #[test]
    fn a_file_deleted_after_the_check_is_not_offered() {
        let env = oxa_fake::TestEnv::new();
        let mut state = DoneState::default();
        failed_batch(&mut state, &env.work(), &["keep.txt", "gone.txt"]);
        std::fs::remove_file(env.work().join("gone.txt")).unwrap();
        let line = state.verified_artifacts(&env.work()).unwrap();
        assert!(line.contains("keep.txt"), "{line}");
        assert!(!line.contains("gone.txt"), "{line}");
    }

    #[test]
    fn the_path_list_is_bounded_and_never_names_what_is_not_a_relative_path() {
        let env = oxa_fake::TestEnv::new();
        let long = format!("{}.txt", "l".repeat(ARTIFACT_PATH_CAP));
        let files: Vec<&str> = (0..ARTIFACT_SHOWN + 3)
            .map(|i| if i == 0 { "first.txt" } else { "same.txt" })
            .collect();
        let mut state = DoneState::default();
        failed_batch(&mut state, &env.work(), &files);
        std::fs::write(env.work().join(&long), "x").unwrap();
        state.written.push(long.clone());
        state.written.push("/etc/passwd".to_string());
        state.written.push("../outside.txt".to_string());
        let line = state.verified_artifacts(&env.work()).unwrap();
        assert!(line.contains("first.txt"), "{line}");
        assert!(!line.contains(&long), "an over-long path is left out");
        assert!(!line.contains("/etc/passwd"), "{line}");
        assert!(!line.contains("../outside.txt"), "{line}");
        // Each file once, at its last write: "same.txt" follows "first.txt" and is kept.
        assert_eq!(line.matches("same.txt").count(), 1, "{line}");
    }

    #[test]
    fn a_write_and_a_check_of_one_file_is_one_record() {
        let env = oxa_fake::TestEnv::new();
        env.write("work/answer.txt", "");
        let mut state = DoneState::default();
        for _ in 0..2 {
            let (call, result) = declaration("answer.txt", "empty", Some(0), &env);
            state.note_proof(&[call], &[result]);
        }
        assert_eq!(state.deliverables.len(), 1);
        let evidence = state.evidence().unwrap();
        assert_eq!(
            evidence.text.matches("answer.txt:").count(),
            1,
            "{}",
            evidence.text
        );
        assert!(evidence.text.contains("present but empty"), "{}", evidence.text);
    }

    #[test]
    fn a_run_that_declared_nothing_has_no_record_and_no_extra_read() {
        let state = DoneState::default();
        assert!(state.evidence().is_none());
        assert_eq!(wrap_up_message(&state), crate::prompt::WRAP_UP);
    }

    #[test]
    fn the_record_keeps_the_newest_eight_declarations() {
        let env = oxa_fake::TestEnv::new();
        for n in 0..10 {
            env.write(&format!("work/f{n}.txt"), "x\n");
        }
        let mut state = DoneState::default();
        for n in 0..10 {
            let (call, result) = declaration(&format!("f{n}.txt"), "present", Some(2), &env);
            state.note_proof(&[call], &[result]);
        }
        assert_eq!(state.deliverables.len(), MAX_DELIVERABLES);
        let evidence = state.evidence().unwrap();
        assert!(
            !evidence.text.contains("f0.txt"),
            "the oldest is dropped: {}",
            evidence.text
        );
        assert!(evidence.text.contains("f9.txt"));
    }

    #[test]
    fn only_the_tools_typed_record_declares_a_file() {
        let env = oxa_fake::TestEnv::new();
        env.write("work/move.txt", "e2e4\n");
        let (call, mut result) = declaration("move.txt", "present", Some(5), &env);
        // Text that looks like a receipt is not one, and an error result declares nothing.
        result.ui = serde_json::json!({ "path": "move.txt", "abs": env.work().join("move.txt") });
        let mut state = DoneState::default();
        state.note_proof(&[call.clone()], &[result]);
        assert!(state.deliverables.is_empty());
        let (_, mut failed) = declaration("move.txt", "present", Some(5), &env);
        failed.is_error = true;
        state.note_proof(&[call], &[failed]);
        assert!(state.deliverables.is_empty());
    }

    #[test]
    fn the_wrap_up_names_a_missing_deliverable_but_leaves_a_present_one_alone() {
        let env = oxa_fake::TestEnv::new();
        env.write("work/move.txt", "e2e4\n");
        let (call, result) = declaration("kv-store.proto", "missing", None, &env);
        let mut state = DoneState::default();
        state.note_proof(&[call.clone()], &[result]);
        let text = wrap_up_message(&state);
        assert!(text.starts_with(crate::prompt::WRAP_UP), "{text}");
        assert!(text.contains("kv-store.proto"));
        assert!(text.contains("the best answer you have"), "{text}");

        let (call, result) = declaration("move.txt", "present", Some(5), &env);
        let mut state = DoneState::default();
        state.note_proof(&[call], &[result]);
        assert_eq!(wrap_up_message(&state), crate::prompt::WRAP_UP);
    }
}
