//! Read-only checkpoint observations; never acquire, rewrite or delete a task lock.
use crate::{
    load_task_checkpoint, state, valid_task_id, Outcome, Record, Result, RunPolicy, Step, TaskTokens,
};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Finished,
    Running,
    Interrupted,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub record: Record,
    pub liveness: Liveness,
    /// A projection of local evidence, not a live check of the branch or PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication: Option<PublicationStatus>,
}

#[derive(Debug, Serialize)]
pub struct PublicationStatus {
    pub phase: &'static str,
    pub age_secs: Option<u64>,
    pub disposition: &'static str,
}

impl PublicationStatus {
    fn read(record: &Record, now_ms: u64) -> Option<Self> {
        let (phase, disposition) = phase(record, !awaiting_publication(record))?;
        Some(Self {
            phase,
            age_secs: age_secs(record, now_ms),
            disposition,
        })
    }

    /// The record's publication state in one line, derived from the same local evidence as
    /// `oxa work status`. `None` when the saved head needs no publication.
    pub fn summary(record: &Record, now_ms: u64) -> Option<String> {
        let status = Self::read(record, now_ms)?;
        Some(format!(
            "publication pending: {} ({})",
            status.phase,
            age_text(status.age_secs)
        ))
    }
}

/// The saved head's publication phase and next step from local evidence only. A recorded PR
/// receipt needs a push that reported success, which an unreadable readback leaves unproven.
fn phase(record: &Record, awaiting: bool) -> Option<(&'static str, &'static str)> {
    use crate::Divergence;
    let head = record.head_sha.as_deref()?;
    if record.base_sha.as_deref()? == head {
        return None;
    }
    let receipt = record.publish_receipt.as_ref().filter(|r| r.requested == head);
    let pushed = match receipt {
        Some(r) => r.class == Divergence::Published && r.remote.as_deref() == Some(head),
        // A lane pass is observed through a run for this exact commit, which exists only after
        // the lane push reported success. The readback itself can still be unknown (#2310).
        None => record.lane.as_ref().is_some_and(|lane| lane.head == head),
    };
    let recorded_pr = record.pull_request.as_deref().is_some_and(|url| !url.is_empty());
    if pushed && recorded_pr {
        return None;
    }
    // The publication action ran to a recorded PR, so the only gap is that the destination
    // could not be read back; it is not waiting on a lane run that already finished (#2310).
    if !awaiting && recorded_pr {
        return Some((
            "published_readback_unknown",
            "A PR is recorded for this head, but the destination could not be read back after the push. Check the PR's head before any further publication.",
        ));
    }
    Some(if pushed {
        (
            "branch_without_pr_receipt",
            "Check for an existing PR, then resume through the authorized publication path; do not wait for lane CI just to look up the PR.",
        )
    } else if receipt.is_some_and(|r| !matches!(r.class, Divergence::Absent | Divergence::RemoteBehind)) {
        (
            "push_unconfirmed",
            "Check the destination and reconcile the branch tip before retrying; do not force-push.",
        )
    } else {
        (
            "committed_no_push_recorded",
            "Use the authorized publication path for the saved task; acceptance and approval still apply.",
        )
    })
}

fn age_secs(record: &Record, now_ms: u64) -> Option<u64> {
    record
        .head_saved_at_ms
        .and_then(|at| now_ms.checked_sub(at))
        .map(|ms| ms / 1000)
}

fn age_text(age_secs: Option<u64>) -> String {
    age_secs.map_or_else(|| "age unknown".to_owned(), |s| format!("{s}s since save"))
}

/// True when the execution finished with an accepted head that no recorded push covers,
/// which is the point from which the publication action (`--publish` or lane acceptance)
/// still owes its exact-head receipt.
fn awaiting_publication(record: &Record) -> bool {
    crate::completed_execution(record.step)
        && !record
            .worker
            .as_ref()
            .is_some_and(|worker| matches!(worker.outcome, crate::WorkerOutcome::Interrupted))
        && matches!(
            record.acceptance_outcome.unwrap_or(record.outcome),
            Outcome::Done | Outcome::NothingToDo
        )
}

/// True for a finished, accepted record whose explicit publication action left no useful
/// exact-head receipt. The record is not "done" for anyone reading the result (#2304).
pub fn publication_pending(record: &Record) -> bool {
    awaiting_publication(record) && PublicationStatus::read(record, 0).is_some()
}

/// A bounded projection of one saved task, with no worker receipt or private paths.
#[derive(Debug, Serialize)]
pub struct TaskUsage {
    pub task: String,
    pub checkpoint_revision: u64,
    pub task_tokens: Option<TaskTokens>,
}

/// Read one validated checkpoint. A missing task and a legacy task with no usage are distinct.
pub fn task_usage(policy: &RunPolicy, id: &str) -> Result<Option<TaskUsage>> {
    read_task_usage(policy, id, None)
}

/// Read usage for a selected native task from its retained, authoritative Claims binding.
/// The producer supplies the binding and its protected repository policy. This read does
/// not establish current ownership, liveness or merged-PR attribution.
pub fn task_usage_for_binding(
    policy: &RunPolicy,
    binding: &crate::board::NativeBinding,
) -> Result<Option<TaskUsage>> {
    binding.validate()?;
    read_task_usage(policy, &binding.native_task_id, Some(&binding.branch))
}

fn read_task_usage(policy: &RunPolicy, id: &str, branch: Option<&str>) -> Result<Option<TaskUsage>> {
    if !valid_task_id(id) {
        return Err("invalid task id".into());
    }
    let Some(saved) = load_task_checkpoint(policy, id).map_err(|_| "task usage checkpoint unavailable")?
    else {
        return Ok(None);
    };
    if saved.record.id != id || branch.is_some_and(|branch| saved.record.branch != branch) {
        return Err("task usage checkpoint identity disagrees with the requested task".into());
    }
    Ok(Some(TaskUsage {
        task: saved.record.id,
        checkpoint_revision: saved.record.checkpoint_revision,
        task_tokens: saved.record.task_tokens,
    }))
}

pub fn status(storage: &Path, binary: &Path) -> Result<Vec<Status>> {
    let directory = match fs::read_dir(storage) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let locks = fs::read_to_string("/proc/locks");
    let now_ms = state::now_ms();
    let mut records = Vec::new();
    for entry in directory {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|suffix| suffix != "json") {
            continue;
        }
        let mut record = state::load(&path)?.record;
        let liveness = if record.step == Step::Finished {
            Liveness::Finished
        } else {
            match locked(&path.with_extension("lock"), locks.as_deref().ok(), binary) {
                Some(true) => {
                    record.outcome = Outcome::Running;
                    Liveness::Running
                }
                Some(false) => {
                    record.outcome = Outcome::Blocked;
                    record.detail = "task was interrupted; rerun its task file to resume".into();
                    Liveness::Interrupted
                }
                None => Liveness::Unknown,
            }
        };
        let publication = PublicationStatus::read(&record, now_ms);
        records.push(Status {
            record,
            liveness,
            publication,
        });
    }
    records.sort_by(|a, b| a.record.id.cmp(&b.record.id));
    Ok(records)
}

fn locked(path: &Path, locks: Option<&str>, binary: &Path) -> Option<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let device = metadata.dev();
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffffff00);
    let matches_lock = |text: &str| {
        text.lines().any(|line| {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            let Some(at) = fields.iter().position(|field| *field == "FLOCK") else {
                return false;
            };
            if fields.get(at + 2).copied() != Some("WRITE") {
                return false;
            }
            let Some(identity) = fields.get(at + 4) else {
                return false;
            };
            let mut parts = identity.split(':');
            let parsed = (|| {
                Some((
                    u64::from_str_radix(parts.next()?, 16).ok()?,
                    u64::from_str_radix(parts.next()?, 16).ok()?,
                    parts.next()?.parse::<u64>().ok()?,
                ))
            })();
            parsed == Some((major, minor, metadata.ino()))
        })
    };
    if locks.is_some_and(matches_lock) {
        return Some(true);
    }
    // /proc/locks omits an inherited flock when its original owner has died.
    // Every worker/supervisor inherits this same description as stdin; fdinfo
    // still exposes the held lock. Inspect only descriptors of the file owner.
    let mut uncertain = locks.is_none();
    let executable = binary.file_name()?.as_encoded_bytes();
    let comm = &executable[..executable.len().min(15)];
    for process in fs::read_dir("/proc").ok()? {
        let process = process.ok()?;
        if !process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(owner) = process.metadata() else {
            continue;
        };
        if owner.uid() != metadata.uid() {
            continue;
        }
        // Relevant keepers are the same oxa worker/supervisor executable.
        // Ignore unrelated same-user sandboxes whose descriptors are private.
        let name = match fs::read(process.path().join("comm")) {
            Ok(name) => name,
            Err(_) => {
                uncertain = true;
                continue;
            }
        };
        if name.strip_suffix(b"\n").unwrap_or(&name) != comm {
            continue;
        }
        // Zombies have closed every descriptor, but Linux denies fd lookup
        // for them. Their inaccessible stdin is not an unknown live keeper.
        match fs::read_to_string(process.path().join("stat")) {
            Ok(stat)
                if stat
                    .rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_ascii_whitespace().next())
                    .is_some_and(|state| matches!(state, "Z" | "X")) =>
            {
                continue
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                uncertain = true;
                continue;
            }
        }
        let fd = process.path().join("fd/0");
        let inherited = match fs::metadata(fd) {
            Ok(inherited) => inherited,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                uncertain = true;
                continue;
            }
            Err(_) => continue,
        };
        if (inherited.dev(), inherited.ino()) != (device, metadata.ino()) {
            continue;
        }
        let info = match fs::read_to_string(process.path().join("fdinfo/0")) {
            Ok(info) => info,
            Err(_) => {
                uncertain = true;
                continue;
            }
        };
        if matches_lock(&info) {
            return Some(true);
        }
    }
    if uncertain {
        None
    } else {
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Lease, NativeBinding};
    use crate::{blocked_record, test_dir::PrivateDir, Task};

    /// A finished, accepted record at `head`: what publication still owes its receipt for.
    fn accepted(head: &str) -> Record {
        let task = Task {
            id: "test".into(),
            branch: "agent/test".into(),
            base: "main".into(),
            model: "fake".into(),
            prompt: "Finish task".into(),
            max_time: 60,
            reviewed_pull_request: None,
            lane_ci: false,
        };
        let mut record = blocked_record(&task, String::new());
        record.outcome = Outcome::Done;
        record.acceptance_outcome = Some(Outcome::Done);
        record.step = Step::Finished;
        record.base_sha = Some("a".repeat(40));
        record.head_sha = Some(head.into());
        record
    }

    /// A rejected push: the remote branch is behind the requested commit, nothing recorded as PR.
    fn rejected(head: &str) -> Record {
        let mut record = accepted(head);
        record.outcome = Outcome::Blocked;
        record.publish_receipt = Some(crate::PublishReceipt {
            requested: head.into(),
            remote: Some("c".repeat(40)),
            class: crate::Divergence::RemoteBehind,
        });
        record
    }

    #[test]
    fn a_rejected_lane_push_with_no_pr_is_reported_as_pending() {
        let head = "b".repeat(40);
        let mut record = rejected(&head);
        record.lane = Some(crate::lane::LaneVerdict {
            head: head.clone(),
            run_id: 7,
            conclusion: "failure".into(),
            failed: Vec::new(),
        });
        assert!(publication_pending(&record));
        let status = PublicationStatus::read(&record, 0).expect("reported, not hidden");
        assert_eq!(status.phase, "committed_no_push_recorded");
        // The rejected push itself is not walked again: the same shape with no lane run is
        // an ordinary execution attempt, and a lane that never ran owes nothing yet.
        let mut execution = accepted(&head);
        execution.outcome = Outcome::Blocked;
        execution.publish_receipt = record.publish_receipt.clone();
        assert!(!publication_pending(&execution));
        let mut no_lane_run = accepted(&head);
        no_lane_run.acceptance_outcome = None;
        assert!(!publication_pending(&no_lane_run));
    }

    #[test]
    fn an_accepted_head_with_no_push_receipt_is_pending_not_done() {
        let record = accepted(&"b".repeat(40));
        assert!(publication_pending(&record));
        assert_eq!(
            PublicationStatus::summary(&record, 0).unwrap(),
            "publication pending: committed_no_push_recorded (age unknown)"
        );
        assert_eq!(
            PublicationStatus::summary(&rejected(&record.head_sha.clone().unwrap()), 0).unwrap(),
            "publication pending: committed_no_push_recorded (age unknown)"
        );
        let mut published = record.clone();
        let head = record.head_sha.clone().unwrap();
        published.publish_receipt = Some(crate::PublishReceipt {
            requested: head.clone(),
            remote: Some(head.clone()),
            class: crate::Divergence::Published,
        });
        published.pull_request = Some("https://github.com/example/project/pull/1".into());
        assert!(!publication_pending(&published));
    }

    #[test]
    fn selected_native_usage_requires_the_retained_attempt_and_branch() {
        let directory = PrivateDir::new("bound-task-usage");
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin".into(),
            terminate_binary: None,
            offline: true,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            runner_owns_github: true,
            github_token_command: Vec::new(),
            github_token_write_paths: Vec::new(),
            autonomy: Default::default(),
            worker_github: Default::default(),
            authority_now: None,
            storage: directory.path().into(),
        };
        let lease = Lease {
            holder: "fixture".into(),
            expires_at: 2,
            claimed_at: Some(1),
            generation: (0, 1),
        };
        let key = "a".repeat(64);
        let binding = NativeBinding::new(42, &key, &lease, "agent/item").unwrap();
        assert!(task_usage_for_binding(&policy, &binding).unwrap().is_none());
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": binding.native_task_id,
            "branch": binding.branch,
            "model": "fake",
            "prompt": "Synthetic task."
        }))
        .unwrap();
        let mut record = blocked_record(&task, "Synthetic detail.".into());
        record.task_tokens = Some(TaskTokens {
            attempts: 2,
            complete_attempts: 1,
            completion: Some(0),
            ..TaskTokens::default()
        });
        let mut saved = state::State::new(task, record, true);
        let checkpoint = policy.storage.join(format!("{}.json", binding.native_task_id));
        let mut writer = state::Writer::new(checkpoint, false);
        writer
            .save(&mut saved, Step::Worker, &|_: &Record| Ok(()))
            .unwrap();

        let usage = task_usage_for_binding(&policy, &binding).unwrap().unwrap();
        assert_eq!(usage.task, binding.native_task_id);
        assert_eq!(usage.checkpoint_revision, saved.record.checkpoint_revision);
        let tokens = usage.task_tokens.unwrap();
        assert_eq!(tokens.completion, Some(0));
        assert!(tokens.prompt.is_none());
        assert!(!tokens.complete);

        let other_branch = NativeBinding::new(42, &key, &lease, "agent/other").unwrap();
        assert_eq!(other_branch.native_task_id, binding.native_task_id);
        assert!(task_usage_for_binding(&policy, &other_branch).is_err());
        let next_lease = Lease {
            generation: (0, 2),
            ..lease
        };
        let next = NativeBinding::new(42, &key, &next_lease, &binding.branch).unwrap();
        assert!(task_usage_for_binding(&policy, &next).unwrap().is_none());
    }
}
