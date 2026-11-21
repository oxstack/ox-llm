//! The lead web sampler: one in-process GitHub fold-in for `oxa lead serve` (#2262, S5).
//!
//! `oxa work board --shadow` samples on an external timer. This module runs the same sample
//! inside the serving process, so the board stays fresh without a user timer: one thread owns
//! every attempt (no two samples ever run at once), a Retry asks for one sooner, and the last
//! attempt's state, cause and next attempt time are available to the page.
//!
//! Interface, kept standalone so the observer and the page read it without owning it:
//! - [`Sampler::start`] starts the thread and returns the handle, or `None` when no sample is
//!   configured (no trusted `gh`, no board file, offline, no App token source).
//! - [`Sampler::status`] is the immutable snapshot the render reads.
//! - [`Sampler::retry`] asks for a sample now.
//!
//! The sample itself is `work_board::sample_once`, the same sample `oxa work board --shadow`
//! writes. A
//! fresh App token is taken per sample and never stored: a configured `oxa.github.tokenCommand`
//! (the owner's mint command) runs through the same vetted helper path as the runner's, and
//! `GH_TOKEN` in the service's own environment is accepted like `oxa work board` accepts it.
//! Without either, the sampler is off, the external timer keeps the board fresh, and the page
//! says so.

use crate::netguard;
use crate::work_board;
use oxa_config::{Dirs, Env, Settings};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The owner setting for seconds between two samples; 60 when unset or not a whole number.
pub(crate) const SETTING_SAMPLE_SECS: &str = "oxa.lead.serve.sample_secs";
const DEFAULT_SAMPLE_SECS: u64 = 60;
/// A failing sample waits one interval more each time, up to this cap, so a refused or
/// rate-limited GitHub is not hammered. A success returns to the plain interval.
const MAX_BACKOFF_SECS: u64 = 900;

pub(crate) fn interval(settings: &Settings) -> Duration {
    Duration::from_secs(
        settings
            .user_value(SETTING_SAMPLE_SECS)
            .and_then(|value| value.as_u64())
            .unwrap_or(DEFAULT_SAMPLE_SECS),
    )
}

/// The last attempt, and whether one is running. Every field is a fixed text or a
/// whole-second time: no path, token or raw error byte reaches the page.
#[derive(Clone, Debug, Default)]
pub(crate) struct Status {
    /// Seconds between two samples.
    pub every: u64,
    pub running: bool,
    /// Unix seconds when the current attempt started.
    pub started_at: Option<u64>,
    /// The last successful sample and the record time it carried (Unix seconds).
    pub last_ok: Option<u64>,
    pub observed_at: Option<u64>,
    /// The last failed attempt.
    pub failed: Option<Failure>,
    /// Unix seconds of the next scheduled attempt; `None` while one runs.
    pub next_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub at: u64,
    /// A fixed cause line from [`work_board::sample_cause`].
    pub cause: String,
}

/// The shared state the thread writes and the render reads.
#[derive(Default)]
struct Shared {
    status: Status,
    /// Set by a Retry; the sleeping thread samples at once.
    woke: bool,
    /// The last page load that may have asked for a sample after an idle period.
    last_request: Option<Instant>,
}

/// The serving process's sampler. The page holds an `Arc` of it too, so a Retry and the
/// request path reach the one thread that owns every attempt.
pub(crate) struct Sampler {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Clone for Sampler {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            stop: self.stop.clone(),
        }
    }
}

impl Sampler {
    /// Start the sampler thread, or `None` when this service has no sample to run.
    pub(crate) fn start(dirs: &Dirs, cwd: &Path, env: &Env) -> Option<Self> {
        if oxa_proto::offline() {
            return None;
        }
        let board = std::env::var_os("OXA_LEAD_BOARD_LOG").map(PathBuf::from)?;
        let repo = repository(dirs, cwd)?;
        let settings = Settings::load(dirs, cwd, false);
        // Without a trusted `gh` nothing can sample; the page then says `no trusted gh`.
        let gh = netguard::trusted_executable_for(
            "gh",
            Some(&std::ffi::OsString::from(env.get("PATH").unwrap_or_default())),
            Some(cwd),
            Some(&dirs.home),
        );
        let every = interval(&settings).as_secs();
        let shared = Arc::new(Mutex::new(Shared {
            status: Status {
                every,
                next_at: Some(now() + every),
                ..Status::default()
            },
            ..Shared::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let job = Job {
            dirs: dirs.clone(),
            cwd: cwd.to_path_buf(),
            repo,
            board,
            gh,
            token: Token::of(dirs, cwd, env, &settings),
            host_path: std::ffi::OsString::from(env.get("PATH").unwrap_or_default()),
            state: shared.clone(),
            stop: stop.clone(),
        };
        std::thread::Builder::new()
            .name("oxa-lead-sampler".into())
            .spawn(move || job.run())
            .ok()?;
        Some(Self { shared, stop })
    }

    pub(crate) fn status(&self) -> Status {
        lock(&self.shared).status.clone()
    }

    /// A sample now: a running attempt finishes, then one starts at once. This also drops the
    /// back-off of a failing sample.
    pub(crate) fn retry(&self) {
        lock(&self.shared).woke = true;
    }

    /// A page was served: after `every` seconds without one, sample now instead of waiting out
    /// the interval. Loads within that period ask once.
    pub(crate) fn page_load(&self) {
        let mut shared = lock(&self.shared);
        let now = Instant::now();
        let idle = Duration::from_secs(shared.status.every.max(1));
        if shared
            .last_request
            .is_none_or(|last| now.duration_since(last) >= idle)
        {
            shared.woke = true;
        }
        shared.last_request = Some(now);
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn backoff(failures: u32, every: u64) -> u64 {
    every
        .saturating_mul(1u64 << failures.min(4))
        .min(every.max(MAX_BACKOFF_SECS))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The repository this service samples: the owner's `oxa.lead.serve.repo`, else the serving
/// project's `origin` URL. Only `OWNER/REPO` is used; any other form leaves the sampler off.
fn repository(dirs: &Dirs, cwd: &Path) -> Option<String> {
    if let Some(repo) = Settings::load(dirs, cwd, false)
        .user_value("oxa.lead.serve.repo")
        .and_then(|value| value.as_str().map(str::to_owned))
    {
        return oxa_work::valid_repository(&repo).then_some(repo);
    }
    let config = std::fs::read_to_string(cwd.join(".git/config")).ok()?;
    let (mut section, mut origin, mut any) = (String::new(), None, None);
    for line in config.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
        } else if let Some(url) = line.strip_prefix("url = ") {
            if url.contains("github.com") {
                any.get_or_insert_with(|| url.to_owned());
                if section == "remote \"origin\"" {
                    origin = Some(url.to_owned());
                }
            }
        }
    }
    let url = origin.or(any)?;
    let path = url
        .split_once("github.com")
        .map(|(_, rest)| rest.trim_start_matches([':', '/']))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let repo = format!("{}/{}", parts.next()?, parts.next()?);
    (parts.next().is_none() && oxa_work::valid_repository(&repo)).then_some(repo)
}

/// Where one sample's App token comes from: a fresh one per sample, never kept.
#[derive(Clone, Debug, Default)]
struct Token {
    /// `oxa.github.tokenCommand` resolved to one trusted program, from user settings only.
    command: Option<Vec<String>>,
    /// `GH_TOKEN` in the serving process's environment, like `oxa work board` accepts it.
    inherited: Option<String>,
}

impl Token {
    fn of(dirs: &Dirs, cwd: &Path, env: &Env, settings: &Settings) -> Self {
        let argv = netguard::github_token_command(settings);
        // Under test isolation no command is resolved and no ambient credential is used.
        if std::env::var_os(oxa_proto::names::ENV_TEST_ISOLATION).is_some() {
            return Self::default();
        }
        Self {
            command: netguard::token_program(&argv, cwd, &dirs.home),
            inherited: env.get("GH_TOKEN").filter(|s| !s.is_empty()).map(str::to_owned),
        }
    }

    fn configured(&self) -> bool {
        self.command.is_some() || self.inherited.is_some()
    }

    fn fresh(&self) -> Result<String, &'static str> {
        if let Some(argv) = &self.command {
            let program = oxa_work::trusted_token_program(argv, None, None, &[], None)
                .ok_or("the GitHub App token command is not a trusted helper")?;
            return oxa_work::token_from_command(program, oxa_work::COMMAND_DEADLINE)
                .ok_or("the GitHub App token command failed");
        }
        self.inherited
            .clone()
            .ok_or("no GitHub App token source is configured")
    }
}

struct Job {
    dirs: Dirs,
    cwd: PathBuf,
    repo: String,
    board: PathBuf,
    /// The trusted `gh` this host resolved at start; no lookup runs per attempt.
    gh: Option<PathBuf>,
    token: Token,
    host_path: std::ffi::OsString,
    state: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Job {
    fn run(self) {
        if !self.token.configured() {
            // Nothing to sample with: keep the status honest and leave the board to the timer.
            let mut shared = lock(&self.state);
            shared.status.next_at = None;
            shared.status.failed = Some(Failure {
                at: now(),
                cause: "no GitHub App token source: set oxa.github.tokenCommand, or keep the external timer"
                    .into(),
            });
            return;
        }
        let mut failures = 0u32;
        loop {
            let started = now();
            {
                let mut shared = lock(&self.state);
                shared.woke = false;
                shared.status.running = true;
                shared.status.started_at = Some(started);
                shared.status.next_at = None;
            }
            let outcome = self.attempt();
            let finished = now();
            let mut shared = lock(&self.state);
            shared.status.running = false;
            shared.status.started_at = None;
            let due = match outcome {
                Ok(observed_at) => {
                    failures = 0;
                    shared.status.last_ok = Some(finished);
                    shared.status.observed_at = Some(observed_at);
                    shared.status.failed = None;
                    shared.status.every
                }
                Err(cause) => {
                    failures = failures.saturating_add(1);
                    shared.status.failed = Some(Failure { at: finished, cause });
                    backoff(failures, shared.status.every)
                }
            };
            shared.status.next_at = Some(finished.saturating_add(due));
            // Sleep in short steps, so a Retry or a stop is taken at once.
            let deadline = Instant::now() + Duration::from_secs(due);
            while Instant::now() < deadline && !self.stop.load(Ordering::SeqCst) {
                if lock(&self.state).woke {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    /// One sample through the same sample `oxa work board --shadow` writes, with the same
    /// protected record and the same bounds.
    fn attempt(&self) -> Result<u64, String> {
        if !work_board::sampler_trusted(&self.cwd, &self.dirs) {
            return Err("the serving project is not trusted, so no sample ran; approve the project".into());
        }
        let token = self.token.fresh().map_err(work_board::sample_cause)?;
        let gh = self.gh.as_deref().ok_or("no trusted gh executable")?;
        work_board::sample_once(
            &self.dirs,
            &self.cwd,
            &self.repo,
            &self.board,
            gh,
            &self.host_path,
            token,
        )
        .map_err(|error| work_board::sample_cause(&error))
    }
}
