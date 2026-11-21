//! The native queue consumes the original claim receipt and settles after joining the worker.
use super::super::{board, load_task_checkpoint, validate, CheckpointKind, LockedTask, Outcome, Step};
use super::{Record, Result, RunPolicy, Task};

/// Supplied by the trusted queue caller after authenticating its worker.
pub struct NativeQueueOwner<'a> {
    pub claims: &'a board::Claims,
    pub worker_id: &'a str,
}

/// Materialize a prepared task with the canonical attempt identity and existing branch.
pub fn native_task(binding: &board::NativeBinding, mut task: Task) -> Result<Task> {
    let canonical = board::NativeBinding::new(binding.number, &binding.key, &binding.lease, &binding.branch)?;
    if binding != &canonical || task.branch != binding.branch {
        return Err("prepared task does not own the bound native attempt".into());
    }
    task.id.clone_from(&binding.native_task_id);
    if !task.runner_owns_github() {
        let kind = if task.base == task.branch {
            "pull request"
        } else {
            "issue"
        };
        task.prompt.insert_str(0, &crate::github_runner_guidance(kind));
    }
    validate(&task)?;
    Ok(task)
}

pub(super) fn from_packet(binding: &board::NativeBinding, packet: &str, policy: &RunPolicy) -> Result<Task> {
    let (header, body) = packet.split_once('\n').ok_or("native task packet has no body")?;
    let mut fields = header.split_ascii_whitespace();
    let id = fields.next().ok_or("native task packet has no source id")?;
    let model = fields.next().ok_or("native task packet has no model")?;
    let branch = fields.next().ok_or("native task packet has no branch")?;
    let base = fields.next().unwrap_or("main");
    let issue = id == format!("i{}", binding.number);
    let change = id == format!("c{}", binding.number);
    if fields.next().is_some()
        || branch != binding.branch
        || !(issue && base == "main" || change && base == branch)
        || packet.len() > crate::helper_queue::MAX_TASK_BYTES
        || packet.contains('\0')
        || body.trim().is_empty()
        || body.lines().any(|line| line.trim() == "---")
    {
        return Err("native task packet disagrees with its bound source".into());
    }
    // The binding-derived task ID is stable across worker recovery. Reuse the
    // saved budget for that ID so a newer runner cannot extend a live deadline.
    let max_time = load_task_checkpoint(policy, &binding.native_task_id)?
        .map_or(crate::RUNNER_DEFAULT_MAX_TIME, |saved| saved.task.max_time);
    native_task(
        binding,
        Task {
            id: id.into(),
            model: model.into(),
            branch: branch.into(),
            base: base.into(),
            prompt: body.into(),
            max_time,
            reviewed_pull_request: None,
            lane_ci: false,
        },
    )
}

impl NativeQueueOwner<'_> {
    pub(super) fn admit(&self, task: &Task) -> Result<board::NativeBinding> {
        let binding = self
            .claims
            .native_binding(&task.id, &task.branch)?
            .ok_or("native task has no retained admission binding")?;
        if self.worker_id != binding.lease.holder || !task.runner_owns_github() {
            return Err("native task lacks the authenticated worker or runner-owned GitHub policy".into());
        }
        let assignment = self.claims.assignments()?;
        if !assignment.get(&binding.number).is_some_and(|row| {
            row.state.lease.as_ref() == Some(&binding.lease)
                && row.data.native_binding.as_ref() == Some(&binding)
        }) {
            return Err("native admission no longer owns the original lease".into());
        }
        Ok(binding)
    }

    pub(super) fn finish(
        &self,
        policy: &RunPolicy,
        task: &Task,
        binding: &board::NativeBinding,
        observed: &Record,
    ) -> Result<()> {
        if task.id != binding.native_task_id
            || task.branch != binding.branch
            || observed.id != task.id
            || observed.branch != task.branch
            || !finished(observed)
        {
            return Err("native completion lacks a whole-task finish observation".into());
        }

        // The queue has joined the worker. A surviving child retains this same
        // lock and prevents reconciliation; retain it through checkpoint read and CAS.
        let _locked = LockedTask::open(task, policy, false)?;
        let saved =
            load_task_checkpoint(policy, &task.id)?.ok_or("native completion checkpoint is unavailable")?;
        if saved.task != *task
            || !finished(&saved.record)
            || saved.record.checkpoint_revision != observed.checkpoint_revision
            || saved.record.checkpoint_at_ms != observed.checkpoint_at_ms
            || saved.record.outcome != observed.outcome
            || binding
                .lease
                .claimed_at
                .and_then(|at| at.checked_mul(1000))
                .is_none_or(|at| at > saved.record.checkpoint_at_ms)
        {
            return Err("native completion does not match the current durable checkpoint".into());
        }
        if !self.claims.release_for(
            binding.number,
            self.worker_id,
            &binding.lease,
            super::state::now_ms() / 1000,
            "native task finished and its process exited",
        )? {
            return Err("native completion no longer owns the original lease".into());
        }
        Ok(())
    }
}

fn finished(record: &Record) -> bool {
    // Releasing the worker's claim does not certify its acceptance result.
    record.step == Step::Finished
        && record.checkpoint_kind == CheckpointKind::TaskTransition
        && record.checkpoint_revision > 0
        && matches!(
            record.outcome,
            Outcome::Done
                | Outcome::NothingToDo
                | Outcome::PublicationPending
                | Outcome::Red
                | Outcome::Unverified
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::PrivateDir;
    use crate::{blocked_record, state, QueueEvent, QueueOptions, QueueRun};
    use board::assignment::{ManualAuthority, Role, Worker, WorkerKind};
    use board::{Action, AttemptState, Claims, Entry, ImportState, Policy};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn setup() -> (PrivateDir, Claims, board::NativeBinding, Task, RunPolicy) {
        let directory = PrivateDir::new("native-completion");
        let claims = Claims::open(directory.path()).unwrap();
        claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from([(
                    1,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )]),
                10,
            )
            .unwrap();
        let worker = Worker {
            id: "codex:fixture/worker".into(),
            kind: WorkerKind::Codex,
            skills: Default::default(),
            roles: [Role::Builder].into(),
        };
        let entry = Entry {
            number: 1,
            head: "fixture-head".into(),
            release_pinned: false,
            action: Action::FixFindings,
            lease: None,
            attempts_today: 0,
            eligible: true,
            evidence_incomplete: None,
        };
        let lease = claims
            .claim_idle_for(
                &entry,
                &Policy {
                    enabled: true,
                    ..Policy::default()
                },
                &worker,
                10,
                20,
            )
            .unwrap()
            .unwrap();
        let binding = board::NativeBinding::new(1, &"a".repeat(64), &lease, "agent/fixture").unwrap();
        assert!(claims.bind_native(&worker.id, &binding).unwrap());
        let prepared = Task {
            id: "c1".into(),
            model: "fake".into(),
            branch: binding.branch.clone(),
            base: "main".into(),
            prompt: "Synthetic task".into(),
            max_time: 30,
            reviewed_pull_request: None,
            lane_ci: false,
        };
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin".into(),
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
            storage: directory.path().join("runner"),
        };
        (directory, claims, binding, prepared, policy)
    }

    #[test]
    fn new_native_packet_uses_the_longer_budget_but_recovery_uses_the_saved_budget() {
        let (_directory, _claims, binding, _prepared, policy) = setup();
        let packet = "c1 fake agent/fixture agent/fixture\nSynthetic task";
        let mut task = from_packet(&binding, packet, &policy).unwrap();
        assert_eq!(task.max_time, 4 * 60 * 60);
        task.max_time = crate::default_max_time();
        let _locked = LockedTask::acquire(&task, &policy).unwrap();
        let mut checkpoint = state::State::new(task.clone(), blocked_record(&task, String::new()), true);
        let deadline = checkpoint.deadline_at_ms;
        state::Writer::new(policy.storage.join(format!("{}.json", task.id)), false)
            .save(&mut checkpoint, Step::Preparing, &|_: &Record| Ok(()))
            .unwrap();

        assert_eq!(from_packet(&binding, packet, &policy).unwrap(), task);
        assert_eq!(
            load_task_checkpoint(&policy, &task.id)
                .unwrap()
                .unwrap()
                .deadline_at_ms,
            deadline
        );
    }

    #[test]
    fn only_the_exact_durable_whole_task_after_lock_release_settles_the_original_claim() {
        if crate::lock_process::in_parent() {
            return;
        }
        let (_directory, claims, binding, prepared, policy) = setup();
        let owner = NativeQueueOwner {
            claims: &claims,
            worker_id: &binding.lease.holder,
        };
        assert!(owner.admit(&prepared).is_err());
        let task = native_task(&binding, prepared).unwrap();
        assert!(owner.admit(&task).unwrap() == binding);
        let locked = LockedTask::acquire(&task, &policy).unwrap();
        let synthetic = blocked_record(&task, "synthetic record".into());
        assert!(owner.finish(&policy, &task, &binding, &synthetic).is_err());

        let mut saved = state::State::new(task.clone(), synthetic, true);
        let mut writer = state::Writer::new(policy.storage.join(format!("{}.json", task.id)), false);
        let observe = |_: &Record| Ok(());
        saved.record.outcome = Outcome::Done;
        writer
            .save_kind(
                &mut saved,
                Step::Acceptance,
                CheckpointKind::WorkerFinished,
                &observe,
            )
            .unwrap();
        assert!(owner.finish(&policy, &task, &binding, &saved.record).is_err());
        writer.save(&mut saved, Step::Reporting, &observe).unwrap();
        assert!(owner.finish(&policy, &task, &binding, &saved.record).is_err());
        writer.save(&mut saved, Step::Finished, &observe).unwrap();

        // A child surviving its controller owns the original file description.
        let surviving_child = locked.lock.try_clone().unwrap();
        drop(locked);
        assert!(owner.finish(&policy, &task, &binding, &saved.record).is_err());
        assert_eq!(
            claims.snapshot().unwrap()[&1].lease.as_ref(),
            Some(&binding.lease)
        );
        drop(surviving_child);

        let mut stale = saved.record.clone();
        stale.checkpoint_revision -= 1;
        assert!(owner.finish(&policy, &task, &binding, &stale).is_err());
        owner.finish(&policy, &task, &binding, &saved.record).unwrap();
        assert!(claims.snapshot().unwrap()[&1].lease.is_none());
        assert!(owner.admit(&task).is_err());
    }

    #[test]
    fn native_queue_dispatches_one_canonical_attempt_and_settles_before_the_event() {
        native_queue_settlement(Outcome::NothingToDo, 0);
    }

    #[test]
    fn native_queue_releases_finished_unverified_without_accepting_it() {
        native_queue_settlement(Outcome::Unverified, 1);
    }

    fn native_queue_settlement(outcome: Outcome, expected_failed: usize) {
        if crate::lock_process::in_parent() {
            return;
        }
        let (directory, claims, binding, prepared, policy) = setup();
        let owner = NativeQueueOwner {
            claims: &claims,
            worker_id: &binding.lease.holder,
        };
        let task = native_task(&binding, prepared).unwrap();
        let repo = directory.path().join("repo");
        let queue = directory.path().join("queue");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&queue).unwrap();
        assert!(Command::new(&policy.git_binary)
            .current_dir(&repo)
            .env_clear()
            .env("HOME", directory.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        fs::write(
            queue.join("task.json"),
            serde_json::to_vec(std::slice::from_ref(&task)).unwrap(),
        )
        .unwrap();

        let starts = Arc::new(AtomicUsize::new(0));
        let started = starts.clone();
        let receipt = binding.clone();
        let worker_claims = claims.clone();
        let worker: Arc<super::super::NativeWorkerFn> = Arc::new(move |found, locked| {
            assert_eq!(found.id, receipt.native_task_id);
            assert_eq!(found.branch, receipt.branch);
            assert_eq!(
                worker_claims.snapshot().unwrap()[&receipt.number].lease.as_ref(),
                Some(&receipt.lease)
            );
            let second = LockedTask::acquire(found, locked.policy);
            assert!(second.is_err(), "worker must receive the held task lock");
            started.fetch_add(1, Ordering::Relaxed);
            let mut saved = state::State::new(found.clone(), blocked_record(found, String::new()), true);
            saved.record.outcome = outcome;
            let mut writer =
                state::Writer::new(locked.policy.storage.join(format!("{}.json", found.id)), false);
            writer
                .save(&mut saved, Step::Finished, &|_: &Record| Ok(()))
                .unwrap();
            Ok(saved.record)
        });
        let options = QueueOptions {
            min_free_bytes: 0,
            once: true,
            ..QueueOptions::default()
        };
        let run = || QueueRun {
            repo: &repo,
            directory: &queue,
            options: &options,
            policy: &policy,
            df: Path::new("/usr/bin/df"),
        };
        let mut completed = 0;
        let status = run()
            .run_native(&owner, worker.clone(), &mut |event| {
                if let QueueEvent::Task { record } = event {
                    assert_eq!(record.id, task.id);
                    assert_eq!(record.outcome, outcome);
                    assert!(claims.snapshot().unwrap()[&binding.number].lease.is_none());
                    completed += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            (status.finished, status.failed, completed),
            (1, expected_failed, 1)
        );

        let again = run().run_native(&owner, worker, &mut |_| Ok(())).unwrap();
        assert_eq!((again.finished, again.failed), (0, 1));
        assert_eq!(starts.load(Ordering::Relaxed), 1);
    }
}
