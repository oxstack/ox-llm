//! Verify a saved task from an independent, clean, hash-checked Git export.
use crate::{
    acceptance_result, checked, derived, dirty_worktree, git, state, supervise, tree, valid_task_id,
    CheckOrigin, Outcome, Record, Result, RunPolicy, Step, SupervisorOperation, SupervisorRequest,
};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Owns the task lock and the exact object graph that passed acceptance.
/// Publication must consume this value, never reconstruct it from a saved verdict.
pub struct VerifiedTask {
    pub(crate) record: Record,
    pub(crate) export: Export,
    _lock: File,
    checked_at_ms: u64,
}
impl VerifiedTask {
    pub fn record(&self) -> &Record {
        &self.record
    }
    pub fn checked_at_ms(&self) -> u64 {
        self.checked_at_ms
    }

    /// Check the sealed object graph again immediately before publication.
    pub fn revalidate(&self, policy: &RunPolicy) -> Result<()> {
        self.export.verify(
            policy,
            self.record
                .head_sha
                .as_deref()
                .ok_or("verified head is unavailable")?,
            self.record
                .base_sha
                .as_deref()
                .ok_or("verified base is unavailable")?,
        )
    }
}

/// Recheck a finished task against its saved commands, with no retained ignored helpers.
/// This does not push, create a PR, or change the task's checkpoint.
pub fn verify_saved_task(repo: &Path, binary: &Path, policy: &RunPolicy, id: &str) -> Result<VerifiedTask> {
    if !valid_task_id(id) {
        return Err("invalid task identity".into());
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .open(policy.storage.join(format!("{id}.lock")))
        .map_err(|_| "cannot open saved task lock")?;
    lock.try_lock().map_err(|_| "task is already running")?;
    let saved = state::load(&policy.storage.join(format!("{id}.json")))?;
    let record = &saved.record;
    if saved.task.id != id || record.step != Step::Finished || !record.work_accepted() {
        return Err("only finished successful work can be verified for publication".into());
    }
    require_lane_evidence(&saved)?;
    let export = verify_commit(repo, binary, policy, &saved, &lock)?;
    Ok(VerifiedTask {
        record: saved.record,
        export,
        _lock: lock,
        checked_at_ms: crate::state::now_ms(),
    })
}

/// A lane task's saved success is only as good as its run at the saved head.
pub(crate) fn require_lane_evidence(saved: &state::State) -> Result<()> {
    if !saved.task.lane_ci {
        return Ok(());
    }
    let green = saved.record.lane.as_ref().is_some_and(|verdict| {
        Some(verdict.head.as_str()) == saved.record.head_sha.as_deref() && verdict.passed()
    });
    if green {
        Ok(())
    } else {
        Err("saved lane CI evidence is not green at this head".into())
    }
}

/// The declared acceptance crates must be among the crates the committed change selects:
/// the lane runs the suites of the crates that changed (its reverse-dependency expansion
/// only widens that set), so a declaration the head does not touch has no evidence of
/// having run. The refusal names every uncovered crate.
fn declared_covered_by_selection(
    declared: &[String],
    selected: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let uncovered: Vec<&str> = declared
        .iter()
        .filter(|name| !selected.contains(*name))
        .map(String::as_str)
        .collect();
    if uncovered.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the lane run does not select the declared acceptance crates: {}; touch each declared crate in the task diff or the work stays unverified",
            uncovered.join(", ")
        ))
    }
}

/// Check the saved commit while the caller holds the task lock.
pub(crate) fn verify_commit(
    repo: &Path,
    binary: &Path,
    policy: &RunPolicy,
    saved: &state::State,
    lock: &File,
) -> Result<Export> {
    verify_commit_with(repo, binary, policy, saved, lock, false)
}

/// As `verify_commit`; `collecting_lane` lets a lane task's own exact-head lane run
/// stand in for the hosted evidence its Rust changes require, so that run can be
/// started. A saved green lane run at the head counts the same way.
pub(crate) fn verify_commit_with(
    repo: &Path,
    binary: &Path,
    policy: &RunPolicy,
    saved: &state::State,
    lock: &File,
    collecting_lane: bool,
) -> Result<Export> {
    // Explicit saved-work verification has its own budget. It may run long
    // after the original worker attempt's deadline has passed.
    let deadline_at_ms = crate::state::now_ms().saturating_add(saved.task.max_time.saturating_mul(1000));
    let record = &saved.record;
    // An explicit later online publication never relaxes the saved check policy.
    let verification_policy = RunPolicy {
        offline: saved.offline || policy.offline,
        runner_owns_github: saved.task.runner_owns_github(),
        ..policy.clone()
    };
    let policy = &verification_policy;
    let execution = saved
        .execution
        .as_ref()
        .ok_or("saved acceptance commands are unavailable")?;
    let head = record.head_sha.as_deref().ok_or("saved head is unavailable")?;
    let tree_id = record.tree_sha.as_deref().ok_or("saved tree is unavailable")?;
    let base = record.base_sha.as_deref().ok_or("saved base is unavailable")?;
    if execution.base != base {
        return Err("saved acceptance base disagrees with the task record".into());
    }
    tree::validate(policy, repo, &record.worktree, &saved.task)?;
    tree::ensure_merged(policy, &record.worktree, base, &saved.task.branch)?;
    same_saved_commit(policy, &record.worktree, head, tree_id)?;
    if dirty_worktree(binary, &record.worktree, repo, policy, lock)? {
        return Err("saved task has uncommitted changes".into());
    }
    let export = Export::create(policy, &record.worktree, head, tree_id, base)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(export.root.join(".committed-output"))
        .map_err(|_| "cannot create private committed-check outputs")?;
    if crate::done_commands(policy, &export.store, base)? != execution.commands {
        return Err("saved checks disagree with their captured base".into());
    }
    let checkout = supervise(
        binary,
        &export.tree,
        SupervisorRequest {
            source: repo.into(),
            operation: SupervisorOperation::Checkout {
                branch: saved.task.branch.clone(),
                create: false,
                adopted: false,
                normalize: None,
            },
        },
        policy,
        lock,
    )?;
    if !checkout.status.success() {
        return Err("clean acceptance checkout failed; saved work is retained".into());
    }
    same_saved_commit(policy, &export.tree, head, tree_id)?;
    if dirty_worktree(binary, &export.tree, repo, policy, lock)? {
        return Err("clean acceptance checkout is dirty".into());
    }
    // A local compile plan cannot verify saved Rust work for publication.
    let mut commands: Vec<(CheckOrigin, String)> = execution
        .commands
        .iter()
        .map(|command| (CheckOrigin::Settings, command.clone()))
        .collect();
    commands.extend(
        execution
            .derived_commands
            .iter()
            .cloned()
            .map(|command| (CheckOrigin::Derived, command)),
    );
    let requires_hosted = derived::committed_requires_hosted(policy, &export.tree, base, head)
        .map_err(|reason| format!("clean committed acceptance could not run: {reason}"))?;
    let declared = execution.declared.as_deref().unwrap_or_default();
    let lane_evidence = saved.task.lane_ci && (collecting_lane || require_lane_evidence(saved).is_ok());
    if requires_hosted && !lane_evidence {
        return Err(crate::HOSTED_TEST_EVIDENCE_REQUIRED.into());
    }
    if !declared.is_empty() && !lane_evidence {
        return Err(crate::HOSTED_TEST_EVIDENCE_REQUIRED.into());
    }
    if lane_evidence {
        // A green lane run only counts for what it actually ran. The lane coverage data
        // (`tools/ci/lane_coverage.json`, read from the trusted base, the same file
        // `tools/ci/lane_select.py` selects from) says which changed paths that is: engine
        // suites outside the portable crates, engine fixtures and shared inputs, `fuzz/` and
        // any crate the lane does not know have no lane evidence.
        let coverage = crate::lane::coverage::Coverage::load(policy, &export.tree, base);
        let changed = derived::committed_changed_paths(policy, &export.tree, base, head)
            .map_err(|reason| format!("clean committed acceptance could not run: {reason}"))?;
        let uncovered = coverage.uncovered(&changed, |path| {
            derived::has_cargo_ancestor(policy, &export.tree, base, path)
        });
        if let Some(path) = uncovered.first() {
            return Err(coverage.refusal(path));
        }
        if !declared.is_empty() {
            let selected = derived::committed_packages(policy, &export.tree, base, head)
                .map_err(|reason| format!("clean committed acceptance could not run: {reason}"))?;
            // A declaration the head does not touch has no evidence of having run, so
            // the work stays unverified rather than guessed.
            declared_covered_by_selection(declared, &selected)?;
        }
    }
    if commands.is_empty() {
        return Err(crate::NO_ACCEPTANCE_CHECKS.into());
    }
    if commands.len() > derived::MAX_CHECKS {
        return Err(format!(
            "clean committed acceptance could not run: more than {} checks",
            derived::MAX_CHECKS
        ));
    }
    // This fresh index comes from the verified commit and lives outside both
    // writable sandbox binds (checkout and Git metadata). Checks cannot hide
    // changes by setting assume-unchanged/skip-worktree or replacing their index.
    let index = export.root.join(".committed-index");
    let mut prepare_index = private_git(
        policy,
        &export.tree,
        &[
            "-c",
            "core.sparseCheckout=false",
            "-c",
            "index.sparse=false",
            "read-tree",
            "--reset",
            head,
        ],
    );
    prepare_index.env("GIT_INDEX_FILE", &index);
    checked(prepare_index)?;
    // read-tree alone has zeroed stat entries: diff-files would report even
    // untouched files as changed. Refresh before the index becomes read-only.
    let mut refresh_index = private_git(policy, &export.tree, &["update-index", "--refresh"]);
    refresh_index.env("GIT_INDEX_FILE", &index);
    checked(refresh_index)?;
    let round = acceptance_result(
        supervise(
            binary,
            &export.tree,
            SupervisorRequest {
                source: repo.into(),
                operation: SupervisorOperation::CommittedChecks {
                    commands: commands.iter().map(|(_, command)| command.clone()).collect(),
                    index: index.clone(),
                    deadline_at_ms,
                },
            },
            policy,
            lock,
        )?,
        &commands,
    )
    .map_err(|error| format!("clean committed acceptance could not run: {error}"))?;
    if !round.unrunnable.is_empty() {
        return Err(format!(
            "clean committed acceptance could not run: {}",
            round.unrunnable
        ));
    }
    if !round.failures.is_empty() {
        return Err(format!("clean committed acceptance failed: {}", round.failures));
    }
    same_saved_commit(policy, &export.tree, head, tree_id)?;
    if dirty_worktree(binary, &export.tree, repo, policy, lock)? {
        return Err("acceptance changed committed source files".into());
    }
    let inspected = supervise(
        binary,
        &export.tree,
        SupervisorRequest {
            source: repo.into(),
            operation: SupervisorOperation::InspectCommitted { index },
        },
        policy,
        lock,
    )?;
    if !inspected.status.success() {
        return Err("acceptance changed committed source files or could not be inspected".into());
    }
    export.verify(policy, head, base)?;
    same_saved_commit(policy, &record.worktree, head, tree_id)?;
    if dirty_worktree(binary, &record.worktree, repo, policy, lock)? {
        return Err("saved work changed during committed acceptance".into());
    }
    Ok(export)
}

fn same_saved_commit(policy: &RunPolicy, tree: &Path, head: &str, tree_id: &str) -> Result<()> {
    if checked(git(policy, tree, &["rev-parse", "HEAD"]))? != head
        || checked(git(policy, tree, &["rev-parse", "HEAD^{tree}"]))? != tree_id
    {
        return Err("saved head or tree changed".into());
    }
    Ok(())
}

/// Every export has its own object store, no alternate or shared object links.
/// Only this generated subtree is removed when the proof leaves scope.
pub(crate) struct Export {
    root: PathBuf,
    pub(crate) store: PathBuf,
    pub(crate) tree: PathBuf,
    shallow: Option<Vec<u8>>,
}
impl Drop for Export {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Export {
    pub(crate) fn create(
        policy: &RunPolicy,
        source: &Path,
        head: &str,
        tree: &str,
        base: &str,
    ) -> Result<Self> {
        for oid in [head, tree, base] {
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("acceptance requires full object identities".into());
            }
        }
        let parent = source
            .parent()
            .ok_or("saved worktree has no parent")?
            .join(".oxa-acceptance");
        match fs::DirBuilder::new().mode(0o700).create(&parent) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("cannot create private acceptance root".into()),
        }
        let metadata = fs::symlink_metadata(&parent).map_err(|_| "cannot inspect acceptance root")?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("unsafe acceptance root".into());
        }
        let root = parent.join(format!(
            "{}-{}-{}",
            std::process::id(),
            state::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|_| "cannot create fresh acceptance export")?;
        let mut export = Self {
            store: root.join("objects.git"),
            tree: root.join("tree"),
            root,
            shallow: None,
        };
        let format = checked(git(policy, source, &["rev-parse", "--show-object-format"]))?;
        if !matches!(format.as_str(), "sha1" | "sha256") {
            return Err("unsupported object format".into());
        }
        checked(private_git(
            policy,
            source,
            &[
                "init",
                "--bare",
                "--template=",
                &format!("--object-format={format}"),
                export.store.to_str().ok_or("invalid acceptance path")?,
            ],
        ))?;
        // A shallow source intentionally omits ancestors. Preserve only its
        // reachable declared boundaries; strict import/fsck still validate every
        // exported object, without fetching or borrowing the source object store.
        let shallow_path = checked(git(
            policy,
            source,
            &["rev-parse", "--path-format=absolute", "--git-path", "shallow"],
        ))?;
        match File::open(shallow_path) {
            Ok(file) => {
                const LIMIT: u64 = 1024 * 1024;
                let mut bytes = vec![];
                file.take(LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "cannot read shallow boundaries")?;
                if bytes.len() as u64 > LIMIT {
                    return Err("shallow boundaries exceed export limit".into());
                }
                let text = std::str::from_utf8(&bytes).map_err(|_| "invalid shallow boundaries")?;
                let width = if format == "sha1" { 40 } else { 64 };
                let boundaries: std::collections::HashSet<_> = text.lines().collect();
                if boundaries
                    .iter()
                    .any(|id| id.len() != width || !id.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    return Err("invalid shallow boundary identity".into());
                }
                let roots = checked(git(policy, source, &["rev-list", "--max-parents=0", head, base]))?;
                let mut selected = Vec::new();
                for id in roots.lines().filter(|id| boundaries.contains(id)) {
                    writeln!(selected, "{id}").map_err(|_| "cannot capture shallow boundary")?;
                }
                if !selected.is_empty() {
                    private_file(&export.store.join("shallow"))?
                        .write_all(&selected)
                        .map_err(|_| "cannot write export boundaries")?;
                    export.shallow = Some(selected);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot open shallow boundaries".into()),
        }
        let revisions = export.root.join("revisions");
        let mut input = private_file(&revisions)?;
        writeln!(input, "{head}\n{base}").map_err(|_| "cannot write export identities")?;
        drop(input);
        let pack = export.root.join("objects.pack");
        let mut produce = git(
            policy,
            source,
            &[
                "-c",
                "pack.threads=1",
                "-c",
                "core.alternateRefsCommand=:",
                "-c",
                "core.commitGraph=false",
                "pack-objects",
                "--stdout",
                "--revs",
                "--no-reuse-delta",
                "--no-reuse-object",
                "--no-use-bitmap-index",
            ],
        );
        produce
            .stdin(File::open(&revisions).map_err(|_| "cannot read export identities")?)
            .stdout(private_file(&pack)?)
            .stderr(Stdio::null());
        if !produce
            .status()
            .map_err(|_| "cannot export saved objects")?
            .success()
        {
            return Err("saved object export failed".into());
        }
        let mut import = private_git(policy, &export.store, &["index-pack", "--stdin", "--strict"]);
        import
            .stdin(File::open(&pack).map_err(|_| "cannot read exported objects")?)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if !import
            .status()
            .map_err(|_| "cannot validate exported objects")?
            .success()
        {
            return Err("exported object integrity failed".into());
        }
        fs::remove_file(&pack).map_err(|_| "cannot retire temporary object pack")?;
        fs::remove_file(&revisions).map_err(|_| "cannot retire temporary identities")?;
        export.verify(policy, head, base)?;
        checked(private_git(
            policy,
            &export.store,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--no-checkout",
                "--detach",
                export.tree.to_str().ok_or("invalid acceptance path")?,
                head,
            ],
        ))?;
        same_saved_commit(policy, &export.tree, head, tree)?;
        Ok(export)
    }

    pub(crate) fn verify(&self, policy: &RunPolicy, head: &str, base: &str) -> Result<()> {
        match (fs::read(self.store.join("shallow")), &self.shallow) {
            (Ok(actual), Some(expected)) if actual == *expected => {}
            (Err(error), None) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("acceptance shallow boundary changed".into()),
        }
        for leaf in ["objects/info/alternates", "objects/info/http-alternates"] {
            match fs::symlink_metadata(self.store.join(leaf)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("acceptance object store has an alternate".into()),
            }
        }
        let result = private_git(
            policy,
            &self.store,
            &[
                "-c",
                "fsck.skipList=/dev/null",
                "-c",
                "core.alternateRefsCommand=:",
                "-c",
                "core.commitGraph=false",
                "fsck",
                "--full",
                "--strict",
                "--no-reflogs",
                "--no-dangling",
                head,
                base,
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "cannot verify acceptance objects")?;
        if !result.success() {
            return Err("acceptance object integrity failed".into());
        }
        Ok(())
    }
}

fn private_file(path: &Path) -> Result<File> {
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "cannot create private export file".into())
}
fn private_git(policy: &RunPolicy, cwd: &Path, args: &[&str]) -> Command {
    let mut command = git(policy, cwd, args);
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        policy: RunPolicy,
        head: String,
        tree: String,
        blob: String,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "oxa-committed-{}-{}-{}",
                std::process::id(),
                state::now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let source = root.join("source");
            let storage = root.join("private");
            fs::DirBuilder::new().mode(0o700).create(&storage).unwrap();
            let policy = RunPolicy {
                git_binary: "/usr/bin/git".into(),
                host_path: "/usr/bin:/bin".into(),
                terminate_binary: None,
                offline: true,
                gh_binary: None,
                push_approved: false,
                content_scan: None,
                runner_owns_github: false,
                github_token_command: Vec::new(),
                github_token_write_paths: Vec::new(),
                autonomy: Default::default(),
                worker_github: Default::default(),
                authority_now: None,
                storage,
            };
            checked(private_git(
                &policy,
                &root,
                &["init", "--template=", "-b", "main", source.to_str().unwrap()],
            ))
            .unwrap();
            checked(private_git(&policy, &source, &["config", "user.name", "Test"])).unwrap();
            checked(private_git(
                &policy,
                &source,
                &["config", "user.email", "test@example.invalid"],
            ))
            .unwrap();
            fs::write(source.join("payload.txt"), "approved\n").unwrap();
            fs::write(source.join(".gitignore"), "helper.sh\n").unwrap();
            checked(private_git(&policy, &source, &["add", "."])).unwrap();
            checked(private_git(
                &policy,
                &source,
                &["commit", "-m", "Synthetic fixture"],
            ))
            .unwrap();
            fs::write(source.join("helper.sh"), "exit 0\n").unwrap();
            let head = checked(private_git(&policy, &source, &["rev-parse", "HEAD"])).unwrap();
            let tree = checked(private_git(&policy, &source, &["rev-parse", "HEAD^{tree}"])).unwrap();
            let blob = checked(private_git(&policy, &source, &["rev-parse", "HEAD:payload.txt"])).unwrap();
            Self {
                root,
                source,
                policy,
                head,
                tree,
                blob,
            }
        }
        fn export(&self) -> Result<Export> {
            Export::create(&self.policy, &self.source, &self.head, &self.tree, &self.head)
        }
        fn corrupt(&self, store: &Path) {
            let data = self.root.join("forged.txt");
            fs::write(&data, "forged\n").unwrap();
            let other = checked(private_git(
                &self.policy,
                store,
                &["hash-object", "-w", "--no-filters", data.to_str().unwrap()],
            ))
            .unwrap();
            let object_root = if store == self.source {
                store.join(".git/objects")
            } else {
                store.join("objects")
            };
            let original = object_root.join(&self.blob[..2]).join(&self.blob[2..]);
            // Imported objects are packed; materialize a loose shadow with the wrong content.
            fs::create_dir_all(original.parent().unwrap()).unwrap();
            if original.exists() {
                fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
            }
            fs::copy(object_root.join(&other[..2]).join(&other[2..]), original).unwrap();
        }
    }

    #[test]
    fn a_clean_export_has_no_retained_ignored_helper_and_no_shared_objects() {
        let fixture = Fixture::new();
        let export = fixture.export().unwrap();
        checked(private_git(
            &fixture.policy,
            &export.tree,
            &["checkout", "HEAD", "--", "."],
        ))
        .unwrap();
        assert_eq!(fs::read(export.tree.join("payload.txt")).unwrap(), b"approved\n");
        assert!(fixture.source.join("helper.sh").exists());
        assert!(!export.tree.join("helper.sh").exists());
        fixture.corrupt(&fixture.source);
        assert_eq!(
            checked(private_git(
                &fixture.policy,
                &fixture.source,
                &["cat-file", "blob", &fixture.blob]
            ))
            .unwrap(),
            "forged"
        );
        assert_eq!(
            checked(private_git(
                &fixture.policy,
                &export.store,
                &["cat-file", "blob", &fixture.blob]
            ))
            .unwrap(),
            "approved"
        );
        export
            .verify(&fixture.policy, &fixture.head, &fixture.head)
            .unwrap();
        assert!(
            fixture.export().is_err(),
            "a fresh export must reject the forged source object"
        );
        let private = export.root.clone();
        drop(export);
        assert!(!private.exists());
        assert!(fixture.source.join("helper.sh").exists());
    }

    #[test]
    fn acceptance_cannot_introduce_a_new_shallow_boundary() {
        let fixture = Fixture::new();
        let export = fixture.export().unwrap();
        fs::write(export.store.join("shallow"), format!("{}\n", fixture.head)).unwrap();
        assert!(export
            .verify(&fixture.policy, &fixture.head, &fixture.head)
            .is_err());
    }

    #[test]
    fn post_acceptance_object_tampering_is_rejected_even_with_a_skip_list() {
        let fixture = Fixture::new();
        let export = fixture.export().unwrap();
        fixture.corrupt(&export.store);
        let skip = fixture.root.join("skip-list");
        fs::write(&skip, format!("{}\n", fixture.blob)).unwrap();
        checked(private_git(
            &fixture.policy,
            &export.store,
            &["config", "fsck.skipList", skip.to_str().unwrap()],
        ))
        .unwrap();
        assert!(export
            .verify(&fixture.policy, &fixture.head, &fixture.head)
            .is_err());
    }

    #[test]
    fn alternates_are_refused_instead_of_reopening_shared_object_access() {
        let fixture = Fixture::new();
        let export = fixture.export().unwrap();
        fs::write(
            export.store.join("objects/info/alternates"),
            fixture.source.join(".git/objects").as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        assert!(export
            .verify(&fixture.policy, &fixture.head, &fixture.head)
            .is_err());
    }
    #[test]
    fn a_forged_base_is_rejected_before_creating_a_worktree_or_child() {
        let fixture = Fixture::new();
        fixture.corrupt(&fixture.source);
        let skip = fixture.root.join("skip-list");
        fs::write(&skip, format!("{}\n", fixture.blob)).unwrap();
        checked(private_git(
            &fixture.policy,
            &fixture.source,
            &["config", "fsck.skipList", skip.to_str().unwrap()],
        ))
        .unwrap();
        let retained = fixture.root.join("task");
        let lock = private_file(&fixture.root.join("task.lock")).unwrap();
        let task = crate::Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "Synthetic task".into(),
            max_time: 2700,
            reviewed_pull_request: None,
            lane_ci: false,
        };
        assert!(tree::prepare(
            &fixture.source,
            &retained,
            &task,
            (&fixture.head, None),
            &fixture.root.join("unavailable-worker"),
            &fixture.policy,
            &lock,
            None,
        )
        .is_err());
        assert!(
            !retained.exists(),
            "corrupt base objects must block before worktree creation"
        );
    }

    #[test]
    fn a_lane_run_only_covers_the_declared_crates_the_diff_selects() {
        let selected = std::collections::BTreeSet::from(["one".to_owned(), "two".to_owned()]);
        // Every declared crate is selected: the green run's suites cover the declarations.
        assert!(declared_covered_by_selection(&["one".into(), "two".into()], &selected).is_ok());
        // A declaration the diff does not select has no evidence of having run, and the
        // refusal names it so the worker knows what to touch or withdraw.
        let error = declared_covered_by_selection(&["two".into(), "three".into()], &selected).unwrap_err();
        assert_eq!(
            error,
            "the lane run does not select the declared acceptance crates: three; \
             touch each declared crate in the task diff or the work stays unverified"
        );
    }
}
