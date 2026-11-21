//! Download an exact Actions report through the existing Board Reader.
use crate::work_board::{QueryError, Reader};
use oxa_work::board::queue::{
    report::{
        attempt_for, select, ActionMetadata, ReportArchive, ReportAttemptIdentity, ReportSource,
        VerifiedArchive, VerifiedReports,
    },
    RejectionEvidence,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    path::Path,
};

mod ci_packet;
mod junit;

struct AttemptRead {
    workflow: Value,
    jobs: Value,
    artifacts: Value,
    verified_archives: BTreeMap<(u64, u64, String), Vec<String>>,
}

struct VerifiedAttempt {
    identity: ReportAttemptIdentity,
    reports: VerifiedReports,
}

pub(in crate::work_board) struct Settings {
    workflow_path: String,
    required_check_names: BTreeSet<String>,
    reports: Vec<(String, String)>,
    verified: BTreeMap<(String, u64, u64), VerifiedAttempt>,
}

impl Settings {
    /// A second sample's own copy: the policy, not the verified history. Two sample runs
    /// never share the mutable record of what one of them already verified, and each reads
    /// the archives it needs for its own rejections.
    pub(in crate::work_board) fn fork(&self) -> Self {
        Self {
            workflow_path: self.workflow_path.clone(),
            required_check_names: self.required_check_names.clone(),
            reports: self.reports.clone(),
            verified: BTreeMap::new(),
        }
    }

    pub(in crate::work_board) fn read(path: &Path) -> Result<Self, String> {
        let input = oxa_work::shadow::read_queue_report_policy(path)?;
        let invalid = "invalid queue report policy";
        let policy: Value = serde_json::from_slice(&input).map_err(|_| invalid)?;
        if policy.as_object().is_none_or(|map| map.len() != 3) {
            return Err(invalid.into());
        }
        let workflow_path = policy["workflow_path"]
            .as_str()
            .filter(|path| path.starts_with(".github/workflows/") && !path.chars().any(char::is_control))
            .ok_or(invalid)?
            .to_owned();
        let reports = policy["reports"]
            .as_array()
            .filter(|rows| !rows.is_empty())
            .ok_or(invalid)?;
        let label = |value: &Value| {
            value
                .as_str()
                .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or(invalid)
        };
        let checks = policy["required_check_names"]
            .as_array()
            .filter(|rows| !rows.is_empty())
            .ok_or(invalid)?;
        let required_check_names = checks.iter().map(label).collect::<Result<BTreeSet<_>, _>>()?;
        if required_check_names.len() != checks.len() {
            return Err(invalid.into());
        }
        let mut names = BTreeSet::new();
        let mut sources = Vec::new();
        for row in reports {
            if row.as_object().is_none_or(|map| map.len() != 2) {
                return Err(invalid.into());
            }
            let producer = label(&row["producer_name"])?;
            let artifact = label(&row["artifact_name"])?;
            if !names.insert(artifact.clone()) {
                return Err(invalid.into());
            }
            sources.push((producer, artifact));
        }
        Ok(Self {
            workflow_path,
            required_check_names,
            reports: sources,
            verified: BTreeMap::new(),
        })
    }
}

impl Reader<'_> {
    pub(in crate::work_board) fn sample_test_reports(
        &self,
        rejections: &mut [RejectionEvidence],
        settings: &mut Settings,
    ) -> Result<(), String> {
        let mut previous = std::mem::take(&mut settings.verified);
        let mut attempts_read = BTreeMap::new();
        let mut decoded_bytes = 0;
        for rejection in rejections {
            let mut attempts = BTreeMap::<_, Vec<_>>::new();
            for id in rejection.workflow_checks() {
                if let Some(attempt) = attempt_for(rejection, id) {
                    attempts.entry(attempt).or_default().push(id);
                }
            }
            for ((run, attempt), checks) in attempts {
                let Some(identity) = rejection.report_attempt_identity(&checks) else {
                    continue;
                };
                let key = (rejection.removal_event().to_owned(), run, attempt);
                let proof = match previous.remove(&key) {
                    Some(saved) if saved.identity == identity => {
                        for name in &saved.reports.failed_tests {
                            crate::work_board::charge(&mut decoded_bytes, name.len())
                                .map_err(|_| "decoded test names exceed sample limit")?;
                        }
                        Some(saved.reports)
                    }
                    _ => {
                        let sources = match attempts_read.entry((run, attempt)) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                entry.insert(self.read_attempt(run, attempt, &settings.workflow_path)?)
                            }
                        };
                        let Some(sources) = sources else { continue };
                        self.verify_attempt_reports(
                            rejection,
                            checks,
                            (run, attempt),
                            settings,
                            sources,
                            &mut decoded_bytes,
                        )?
                    }
                };
                if let Some(proof) = proof {
                    rejection.record_verified_reports(proof.clone());
                    settings.verified.insert(
                        key,
                        VerifiedAttempt {
                            identity,
                            reports: proof,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn read_attempt(
        &self,
        run: u64,
        attempt: u64,
        workflow_path: &str,
    ) -> Result<Option<AttemptRead>, String> {
        let workflow = self.actions_json(&format!("runs/{run}/attempts/{attempt}"))?;
        if workflow["path"].as_str() != Some(workflow_path) {
            return Ok(None);
        }
        Ok(Some(AttemptRead {
            workflow,
            jobs: self.actions_json(&format!("runs/{run}/attempts/{attempt}/jobs?per_page=100"))?,
            artifacts: self.actions_json(&format!("runs/{run}/artifacts?per_page=100"))?,
            verified_archives: BTreeMap::new(),
        }))
    }

    fn verify_attempt_reports(
        &self,
        rejection: &RejectionEvidence,
        checks: Vec<u64>,
        (run, attempt): (u64, u64),
        settings: &Settings,
        sources: &mut AttemptRead,
        decoded_bytes: &mut usize,
    ) -> Result<Option<VerifiedReports>, String> {
        let metadata = ActionMetadata {
            attempt: &sources.workflow,
            jobs: &sources.jobs,
            artifact_listing: &sources.artifacts,
        };
        let mut archives = Vec::new();
        for (producer_name, artifact_name) in &settings.reports {
            let source = ReportSource {
                workflow_path: &settings.workflow_path,
                required_check_names: &settings.required_check_names,
                producer_name,
                artifact_name,
                max_archive_bytes: crate::work_board::MAX_RESPONSE as u64,
            };
            let mut selected = checks.iter().map(|id| select(rejection, *id, &source, metadata));
            let Some(Some(first)) = selected.next() else {
                return Ok(None);
            };
            if selected.any(|archive| archive.as_ref() != Some(&first)) {
                return Ok(None);
            }
            archives.push(first);
        }
        let mut verified = Vec::new();
        let mut failed_tests = BTreeSet::new();
        for report in archives {
            let key = (report.artifact_id, report.size_bytes, report.sha256.to_owned());
            let names = match sources.verified_archives.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(junit::failed_tests(&self.verify_archive(&report)?)?),
            };
            for name in names {
                if !failed_tests.contains(name) {
                    crate::work_board::charge(decoded_bytes, name.len())
                        .map_err(|_| "decoded test names exceed sample limit")?;
                    failed_tests.insert(name.clone());
                }
            }
            verified.push(VerifiedArchive {
                id: report.artifact_id,
                sha256: report.sha256.to_owned(),
            });
        }
        Ok(Some(VerifiedReports {
            failed_check_runs: checks,
            workflow_run: run,
            run_attempt: attempt,
            archives: verified,
            failed_tests: failed_tests.into_iter().collect(),
        }))
    }

    fn verify_archive(&self, selected: &ReportArchive<'_>) -> Result<Vec<u8>, String> {
        let size = usize::try_from(selected.size_bytes).map_err(|_| "invalid GitHub archive size")?;
        let (remaining, duration) = self.read_budget().map_err(QueryError::message)?;
        if size > remaining {
            return Err("GitHub test archive exceeds remaining snapshot byte limit".into());
        }
        let (code, bytes) = oxa_work::command_output_bytes(
            self.actions_get(&format!("artifacts/{}/zip", selected.artifact_id)),
            Some(size),
            duration,
        )
        .ok_or("GitHub test archive exceeds its limit or timed out")?;
        self.charge_capture(bytes.len()).map_err(QueryError::message)?;
        if code != 0 || bytes.len() != size {
            return Err("GitHub test archive unavailable or incomplete".into());
        }
        if format!("{:x}", Sha256::digest(&bytes)) != selected.sha256 {
            return Err("GitHub test archive digest mismatch".into());
        }
        Ok(bytes)
    }
}
