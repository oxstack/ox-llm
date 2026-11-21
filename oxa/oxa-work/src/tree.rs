//! Continue linked worktrees with ordinary sandboxed merges; retain every conflict and edit.
use super::{
    checked, dirty_worktree, git, load_task_checkpoint, lock_task, output, supervise, Outcome, Result,
    RunPolicy, Step, SupervisorOperation, SupervisorRequest, Task,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Hash-check the captured base before checkout or a resumed worker can consume it.
pub(crate) fn verify_objects(policy: &RunPolicy, repo: &Path, base: &str) -> Result<()> {
    let status = git(
        policy,
        repo,
        &[
            "-c",
            "core.alternateRefsCommand=:",
            "-c",
            "core.commitGraph=false",
            "-c",
            "fsck.skipList=/dev/null",
            "fsck",
            "--full",
            "--strict",
            "--no-reflogs",
            "--no-dangling",
            base,
        ],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .map_err(|_| "cannot verify captured base objects")?;
    if !status.success() {
        return Err("captured base object integrity failed; work retained".into());
    }
    Ok(())
}

pub(crate) struct Prepared {
    pub initial: String,
    pub prompt: String,
}

fn git_path(policy: &RunPolicy, tree: &Path, name: &str) -> Result<PathBuf> {
    checked(git(
        policy,
        tree,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ))
    .map(PathBuf::from)
}

fn merging(policy: &RunPolicy, tree: &Path) -> Result<bool> {
    Ok(git_path(policy, tree, "MERGE_HEAD")?.exists())
}

fn ref_exists(policy: &RunPolicy, repo: &Path, reference: &str) -> Result<bool> {
    let out = output(git(policy, repo, &["show-ref", "--verify", "--quiet", reference]))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&out.stderr).into()),
    }
}

#[derive(Clone)]
pub(crate) struct BaseAlias {
    pub reference: String,
    pub remote: bool,
}

/// Capture Git's actual resolution, never a tag or a symbolic local branch.
pub(crate) fn base_alias(
    policy: &RunPolicy,
    repo: &Path,
    base: &str,
    sha: &str,
) -> Result<Option<BaseAlias>> {
    let resolved = checked(git(
        policy,
        repo,
        &["rev-parse", "--symbolic-full-name", "--verify", base],
    ))?;
    let local = resolved.strip_prefix("refs/heads/");
    let mut remotes: Vec<String> = checked(git(policy, repo, &["remote"]))?
        .lines()
        .filter(|remote| !remote.is_empty())
        .map(str::to_owned)
        .collect();
    // Prefer the longest remote name so a slashed remote wins over its prefix.
    remotes.sort_by_key(|remote| std::cmp::Reverse(remote.len()));
    let branch = local.or_else(|| {
        remotes
            .iter()
            .find_map(|remote| resolved.strip_prefix(&format!("refs/remotes/{remote}/")))
    });
    let Some(branch) = branch else { return Ok(None) };
    let reference = format!("refs/heads/{branch}");
    if !ref_exists(policy, repo, &reference)?
        || output(git(policy, repo, &["symbolic-ref", "--quiet", &reference]))?
            .status
            .code()
            != Some(1)
        || checked(git(
            policy,
            repo,
            &["rev-parse", "--verify", &format!("{resolved}^{{commit}}")],
        ))? != sha
    {
        return Ok(None);
    }
    Ok(Some(BaseAlias {
        reference,
        remote: local.is_none(),
    }))
}

fn branch_checked_out(policy: &RunPolicy, repo: &Path, reference: &str) -> Result<bool> {
    let listing = checked(git(policy, repo, &["worktree", "list", "--porcelain"]))?;
    Ok(listing.lines().any(|line| line == format!("branch {reference}")))
}

/// A fresh task worktree shares the source repository's refs, so a local base
/// branch that lags the frozen base makes the task's own diff look like the
/// whole repository divergence. Advance that branch when it cannot lose work:
/// a strict fast-forward that no worktree has checked out.
fn hold_stale_base(
    policy: &RunPolicy,
    repo: &Path,
    tree: &Path,
    alias: Option<&BaseAlias>,
    base: &str,
) -> Result<Option<(PathBuf, super::BaseUpdate)>> {
    let Some(alias) = alias else {
        return Ok(None);
    };
    let reference = alias.reference.clone();
    if output(git(policy, repo, &["symbolic-ref", "--quiet", &reference]))?
        .status
        .code()
        != Some(1)
    {
        return Ok(None);
    }
    let old = checked(git(policy, repo, &["rev-parse", "--verify", &reference]))?;
    if old == base || branch_checked_out(policy, repo, &reference)? {
        return Ok(None);
    }
    if !output(git(policy, repo, &["merge-base", "--is-ancestor", &old, base]))?
        .status
        .success()
    {
        return Ok(None);
    }
    // Git must first own this branch in an empty linked worktree. A checkout
    // interleaved after the ancestry read makes this add refuse the branch.
    let holder = tree.with_extension("base-hold");
    if holder.exists() {
        return Err("base preparation worktree retained; reconcile it before retrying".into());
    }
    let added = output(git(
        policy,
        repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--no-checkout",
            holder.to_str().ok_or("non-UTF8 base worktree")?,
            reference
                .strip_prefix("refs/heads/")
                .ok_or("invalid base branch")?,
        ],
    ))?;
    if !added.status.success() {
        return Ok(None);
    }
    Ok(Some((
        holder,
        super::BaseUpdate {
            reference,
            old,
            new: base.into(),
        },
    )))
}

fn remove_base_holder(policy: &RunPolicy, repo: &Path, holder: &Path) -> Result<()> {
    // This task owns the fresh no-checkout path. Preserve anything added to it.
    let entries = fs::read_dir(holder)
        .map_err(|e| e.to_string())?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    if entries.len() != 1
        || entries[0].file_name() != ".git"
        || !fs::symlink_metadata(holder.join(".git")).is_ok_and(|metadata| metadata.is_file())
    {
        return Err("base preparation contains work; retained for reconciliation".into());
    }
    checked(git(
        policy,
        repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "remove",
            "--force",
            holder.to_str().ok_or("non-UTF8 base worktree")?,
        ],
    ))?;
    Ok(())
}

fn merge_prompt(
    policy: &RunPolicy,
    tree: &Path,
    task: &Task,
    base: &str,
    diagnostic: &str,
) -> Result<String> {
    // Index-only, no helpers or worktree content conversion. Stat output is
    // bounded to twenty paths instead of retaining an unbounded name list.
    let paths = checked(git(
        policy,
        tree,
        &[
            "diff",
            "--cached",
            "--stat",
            "--stat-count=20",
            "--stat-width=120",
            "--stat-name-width=100",
            "--no-ext-diff",
            "--no-textconv",
            "--diff-filter=U",
        ],
    ))?;
    let pending = checked(git(policy, tree, &["rev-parse", "MERGE_HEAD"]))?;
    Ok(format!("{}\n\nThere is a merge in progress from base {} at commit {pending}. The current base for this run is {base}. Resolve the conflicts, stage the resolutions, and finish the merge commit before completing the task. Then run `git merge --no-edit {base}` to integrate the captured current base as well, resolving and committing any further conflicts. Preserve the task's existing work.\nConflicted paths (at most twenty; use git diff --name-only --diff-filter=U for the full list):\n{paths}\nMerge output:\n{diagnostic}", task.prompt, task.base))
}

fn linked_worktree_common(
    repo: &Path,
    tree: &Path,
    git_at: impl Fn(&Path, &[&str]) -> std::process::Command,
) -> Result<PathBuf> {
    let canonical = tree.canonicalize().map_err(|e| e.to_string())?;
    if !fs::symlink_metadata(tree.join(".git")).is_ok_and(|m| m.is_file())
        || canonical == repo.canonicalize().map_err(|e| e.to_string())?
    {
        return Err("retained directory is not a separate linked task worktree".into());
    }
    let actual = PathBuf::from(checked(git_at(tree, &["rev-parse", "--show-toplevel"]))?);
    let common = |p: &Path| -> Result<PathBuf> {
        PathBuf::from(checked(git_at(
            p,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))?)
        .canonicalize()
        .map_err(|e| e.to_string())
    };
    let source_common = common(repo)?;
    if actual.canonicalize().map_err(|e| e.to_string())? != canonical || common(tree)? != source_common {
        return Err("retained directory is not this repository's task worktree".into());
    }
    Ok(source_common)
}

/// Check the same linked-worktree boundary used by the runner before inheriting trust.
pub fn verify_managed_worktree(
    binary: &Path,
    host_path: &std::ffi::OsStr,
    source: &Path,
    tree: &Path,
) -> Result<()> {
    let git_at = |cwd: &Path, args: &[&str]| crate::metadata_git(binary, host_path, cwd, args);
    let source_top = PathBuf::from(checked(git_at(source, &["rev-parse", "--show-toplevel"]))?);
    if !source.is_absolute() || source_top != source {
        return Err("inherited trust source must be the repository root".into());
    }
    let common = linked_worktree_common(source, tree, git_at)?;
    let expected = crate::worktree_root(&common)?;
    let tree = tree.canonicalize().map_err(|error| error.to_string())?;
    if tree.parent() != Some(expected.as_path())
        || !tree
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(crate::valid_task_id)
    {
        return Err("inherited trust is limited to managed task worktrees".into());
    }
    Ok(())
}

pub(crate) fn validate(policy: &RunPolicy, repo: &Path, tree: &Path, task: &Task) -> Result<()> {
    linked_worktree_common(repo, tree, |cwd, args| git(policy, cwd, args))?;
    if checked(git(policy, tree, &["symbolic-ref", "--quiet", "HEAD"]))?
        != format!("refs/heads/{}", task.branch)
    {
        return Err("retained worktree is on a different branch".into());
    }
    Ok(())
}

/// A published task branch may have no local ref at all: the worktree would
/// then start at the base, so a worker auditing a pull request's head would
/// redo work from a tree that predates it. Only a lone remote-tracking tip
/// that descends from the captured base (and carries commits the base lacks) is
/// adopted; no fetch runs, and an unrelated or diverging tip is never preferred
/// over the base. Selection is read-only; the sandboxed checkout materializes
/// this commit using the adopted-tip execution policy.
fn published_head(policy: &RunPolicy, repo: &Path, branch: &str, base: &str) -> Result<Option<String>> {
    let base_sha = match output(git(
        policy,
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("{base}^{{commit}}")],
    ))? {
        // An unresolvable base is left to the ordinary path, which reports it.
        out if !out.status.success() => return Ok(None),
        out => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    };
    let pattern = format!("refs/remotes/*/{branch}");
    let tracking = checked(git(
        policy,
        repo,
        &["for-each-ref", "--format=%(refname)", &pattern],
    ))?;
    let mut heads = Vec::new();
    for name in tracking.lines().filter(|name| !name.ends_with("/HEAD")) {
        // A stale or dangling tracking ref is ignored, not turned into a block.
        let read = output(git(
            policy,
            repo,
            &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
        ))?;
        if !read.status.success() {
            continue;
        }
        let tip = String::from_utf8_lossy(&read.stdout).trim().to_owned();
        heads.push(tip);
    }
    heads.sort();
    heads.dedup();
    let [tip] = heads.as_slice() else {
        return Ok(None);
    };
    if tip == &base_sha
        || output(git(
            policy,
            repo,
            &["merge-base", "--is-ancestor", &base_sha, tip],
        ))?
        .status
        .code()
            != Some(0)
    {
        return Ok(None);
    }
    Ok(Some(tip.clone()))
}

/// The earlier task that may hand its checked-out branch to this one.
#[derive(Clone, Copy)]
enum Lineage<'a> {
    /// An earlier reviewed round of the same pull request (its frozen typed number);
    /// this round's branch is at `reviewed`, which may differ from the earlier round's.
    Review { number: u64, reviewed: &'a str },
    /// An earlier native board attempt on the same branch. Each admission has its
    /// own task id, and therefore its own worktree.
    Native,
}

fn native_attempt(task: &Task) -> bool {
    task.id.starts_with("board-") && task.runner_owns_github()
}

/// Whether the earlier attempt's persisted admission receipt and this attempt's match.
/// A task ID or prompt is ordinary task input, so neither proves native lineage: an
/// earlier native attempt is handed over only to a later attempt of the same admitted
/// source, and only when the earlier one persisted its own receipt.
fn same_admission(
    prior_id: &str,
    prior: Option<&crate::board::NativeBinding>,
    task: &Task,
    current: Option<&crate::board::NativeBinding>,
) -> bool {
    match (current, prior) {
        (None, None) => !prior_id.starts_with("board-"),
        (Some(current), Some(prior)) => {
            prior.validate().is_ok()
                && current.validate().is_ok()
                && prior.native_task_id == prior_id
                && current.native_task_id == task.id
                && prior.branch == task.branch
                && current.branch == task.branch
                && prior.number == current.number
                && prior.reviewed_head.is_some() == current.reviewed_head.is_some()
        }
        _ => false,
    }
}

/// Preserve an earlier, finished task in place while releasing its branch for this one.
fn release_prior_branch(
    repo: &Path,
    tree: &Path,
    task: &Task,
    lineage: Lineage<'_>,
    native: Option<&crate::board::NativeBinding>,
    binary: &Path,
    policy: &RunPolicy,
) -> Result<()> {
    let branch = format!("refs/heads/{}", task.branch);
    let attached = checked(git(
        policy,
        repo,
        &["for-each-ref", "--format=%(refname)%00%(worktreepath)", &branch],
    ))?;
    if attached.is_empty() {
        // A first native attempt creates the branch.
        return Ok(());
    }
    let path = attached
        .strip_prefix(&branch)
        .and_then(|result| result.strip_prefix('\0'))
        .ok_or("cannot identify the task branch's worktree")?;
    if path.is_empty() {
        return Ok(());
    }
    let previous = Path::new(path);
    let blocked = match lineage {
        Lineage::Review { .. } => {
            "reviewed branch is already checked out outside a finished task for this pull request"
        }
        Lineage::Native => {
            "task branch is already checked out outside a finished native attempt for this branch"
        }
    };
    let prior_id = previous
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(blocked)?;
    // Execution IDs change between attempts on the same pull request, so a review
    // round is matched by the prior's frozen pull request number under its lock.
    let same_lineage = match lineage {
        Lineage::Review { .. } => true,
        Lineage::Native => prior_id.starts_with("board-"),
    };
    if previous.parent() != tree.parent()
        || !super::valid_task_id(prior_id)
        || !same_lineage
        || prior_id == task.id
    {
        return Err(blocked.into());
    }
    let prior_lock = lock_task(policy, prior_id, true)?;
    let prior = load_task_checkpoint(policy, prior_id)?.ok_or(blocked)?;
    let prior_kind = match lineage {
        Lineage::Review { number, .. } => {
            prior.task.reviewed_source().map(|(prior_number, _)| prior_number) == Some(number)
                && prior.record.work_accepted()
        }
        // A retry follows any finished attempt; its saved work is on the branch.
        Lineage::Native => native_attempt(&prior.task),
    };
    if prior.task.id != prior_id
        || !prior_kind
        || !same_admission(prior_id, prior.native_binding.as_ref(), task, native)
        || prior.task.branch != task.branch
        || prior.record.worktree != previous
        || prior.record.step != Step::Finished
    {
        return Err(blocked.into());
    }
    validate(policy, repo, previous, &prior.task)?;
    for operation in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        if git_path(policy, previous, operation)?.exists() {
            return Err(format!("previous task worktree has {operation} in progress"));
        }
    }
    if dirty_worktree(binary, previous, repo, policy, &prior_lock)? {
        return Err("previous task worktree has uncommitted changes".into());
    }
    let head = match lineage {
        Lineage::Review { reviewed, .. } => reviewed.to_string(),
        Lineage::Native => checked(git(policy, previous, &["rev-parse", "--verify", &branch]))?,
    };
    // Change only the attached HEAD. The index, files, branch ref and prior receipt stay intact.
    checked(git(
        policy,
        previous,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "--no-deref",
            "HEAD",
            &head,
            &head,
        ],
    ))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    repo: &Path,
    tree: &Path,
    task: &Task,
    base: (&str, Option<&BaseAlias>),
    binary: &Path,
    policy: &RunPolicy,
    lock: &fs::File,
    native: Option<&crate::board::NativeBinding>,
) -> Result<Prepared> {
    let (base, alias) = base;
    verify_objects(policy, repo, base)?;
    if let Some((number, reviewed)) = task.reviewed_source() {
        let branch = format!("refs/heads/{}", task.branch);
        let local = checked(git(policy, repo, &["rev-parse", "--verify", &branch]))
            .map_err(|_| "local pull request branch is unavailable at the reviewed head")?;
        if local != reviewed {
            return Err("local pull request branch is not at the reviewed head".into());
        }
        if !tree.exists() {
            release_prior_branch(
                repo,
                tree,
                task,
                Lineage::Review { number, reviewed },
                native,
                binary,
                policy,
            )?;
        }
    } else if native.is_some() && native_attempt(task) && !tree.exists() {
        release_prior_branch(repo, tree, task, Lineage::Native, native, binary, policy)?;
    }
    let retained = tree.exists();
    if !retained {
        let branch = format!("refs/heads/{}", task.branch);
        let exists = output(git(policy, repo, &["show-ref", "--verify", "--quiet", &branch]))?;
        let create = match exists.status.code() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(String::from_utf8_lossy(&exists.stderr).into()),
        };
        let published = if create {
            published_head(policy, repo, &task.branch, base)?
        } else {
            None
        };
        let tree_arg = tree.to_str().ok_or("non-UTF8 worktree path")?;
        let mut args = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--no-checkout",
        ];
        // A fresh branch is created only by the sandboxed checkout. If another
        // writer creates it meanwhile, Git refuses instead of replacing its work.
        if create {
            args.push("--detach");
        }
        args.push(tree_arg);
        args.push(if create {
            published.as_deref().unwrap_or(base)
        } else {
            &task.branch
        });
        // Only metadata on the host. Git still refuses a branch checked out
        // elsewhere; no task-branch update or checkout callback runs here.
        checked(git(policy, repo, &args))?;
        let holder = hold_stale_base(policy, repo, tree, alias, base)?;
        let checkout = supervise(
            binary,
            tree,
            SupervisorRequest {
                source: repo.to_path_buf(),
                operation: SupervisorOperation::Checkout {
                    branch: task.branch.clone(),
                    create,
                    adopted: published.is_some(),
                    normalize: holder.as_ref().map(|(_, update)| update.clone()),
                },
            },
            policy,
            lock,
        );
        if let Some((holder, _)) = holder {
            remove_base_holder(policy, repo, &holder)?;
        }
        let checkout = checkout?;
        if !checkout.status.success() {
            return Err(format!(
                "sandboxed worktree checkout failed: {}",
                String::from_utf8_lossy(&checkout.stderr)
            ));
        }
    }
    validate(policy, repo, tree, task)?;
    for operation in ["rebase-merge", "rebase-apply", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        if git_path(policy, tree, operation)?.exists() {
            return Err(format!("retained worktree has {operation} in progress"));
        }
    }
    let initial = checked(git(policy, tree, &["rev-parse", "HEAD"]))?;
    if let Some(reviewed) = task.reviewed_head() {
        if initial != reviewed {
            return Err("pull request branch moved during worktree preparation".into());
        }
        ensure_merged(policy, tree, base, &task.branch)?;
        if retained && dirty_worktree(binary, tree, repo, policy, lock)? {
            return Err("retained worktree has uncommitted changes; worker was not restarted".into());
        }
        return Ok(Prepared {
            initial,
            prompt: task.prompt.clone(),
        });
    }
    if merging(policy, tree)? {
        return Ok(Prepared {
            initial,
            prompt: merge_prompt(policy, tree, task, base, "Resuming the retained merge.")?,
        });
    }
    if retained && dirty_worktree(binary, tree, repo, policy, lock)? {
        return Err("retained worktree has uncommitted changes; worker was not restarted".into());
    }
    let merged = supervise(
        binary,
        tree,
        SupervisorRequest {
            source: repo.to_path_buf(),
            operation: SupervisorOperation::Merge { base: base.into() },
        },
        policy,
        lock,
    )?;
    if !merged.status.success() {
        let diagnostic = String::from_utf8_lossy(&merged.stderr);
        if merged.status.code() != Some(1) || !merging(policy, tree)? {
            return Err(format!("base merge failed: {diagnostic}"));
        }
        return Ok(Prepared {
            initial,
            prompt: merge_prompt(policy, tree, task, base, &diagnostic)?,
        });
    }
    ensure_merged(policy, tree, base, &task.branch)?;
    // A clean base-only merge is preparation, not new worker work.
    Ok(Prepared {
        initial: checked(git(policy, tree, &["rev-parse", "HEAD"]))?,
        prompt: task.prompt.clone(),
    })
}

pub(crate) fn ensure_merged(policy: &RunPolicy, tree: &Path, base: &str, branch: &str) -> Result<()> {
    if checked(git(policy, tree, &["symbolic-ref", "--quiet", "HEAD"]))? != format!("refs/heads/{branch}") {
        return Err("agent left the worktree on a different branch".into());
    }
    let unmerged = output(git(
        policy,
        tree,
        &[
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--diff-filter=U",
        ],
    ))?;
    match unmerged.status.code() {
        Some(0) => {}
        Some(1) => return Err("merge remains in progress; retained for resolution".into()),
        _ => {
            return Err(format!(
                "cannot inspect merge index: {}",
                String::from_utf8_lossy(&unmerged.stderr)
            ))
        }
    }
    if merging(policy, tree)? {
        return Err("merge remains in progress; retained for resolution".into());
    }
    let ancestor = output(git(policy, tree, &["merge-base", "--is-ancestor", base, "HEAD"]))?;
    match ancestor.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err("base is not merged into the task branch; retained for continuation".into()),
        _ => Err(format!(
            "cannot verify captured base: {}",
            String::from_utf8_lossy(&ancestor.stderr)
        )),
    }
}
