//! One observer for every page and every live stream (#2343).
//!
//! The observer stat()s its sources about once a second and parses a source only when its
//! identity (device, inode, length, mtime) changed. The parsed result is an immutable
//! [`Frame`] behind an `Arc`: a render clones the pointer and does no file I/O, and an idle
//! server with five open tabs costs the same as one with none, because no request and no
//! stream reads or parses anything.
//!
//! A read that fails after a good one keeps that good frame as its last success and
//! remembers the typed cause, so one source's trouble never blanks the others. The Board's
//! freshness clock advances in the published frame without a parse, so a sample that stops
//! being written still turns stale; a frame is published only when what a page renders
//! changes, so a quiet system sends nothing to any open tab.

use crate::lead_observation::Observation;
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

/// How often the sources are stat()ed. One round per server, never per tab.
const TICK: Duration = Duration::from_secs(1);

/// Identity of one source file as the last parse saw it. A missing file keeps its own value,
/// so an absent source is looked up again on every tick instead of being re-parsed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stamp {
    present: bool,
    len: u64,
    mtime: (i64, i64),
    ino: u64,
}

impl Stamp {
    fn read(path: &std::path::Path) -> Self {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return Self::default();
        };
        let at = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok());
        Self {
            present: true,
            len: meta.len(),
            mtime: at.map_or((0, 0), |d| (d.as_secs() as i64, d.subsec_nanos() as i64)),
            ino: meta.ino(),
        }
    }
}

/// One immutable snapshot of everything the pages render from the protected sources: the
/// Board sample with its stuck-work signals, the runner situation, the Lead session's worker
/// registry and the owner's open asks. Chat stays a live read of the attached session's
/// transcript and is not part of this frame.
pub struct Frame {
    /// Increases once per published frame; a stream sends only when it changed.
    pub generation: u64,
    /// What the frame's sources read last time, without a success: the observation itself
    /// when it is current, or the last good one with its typed cause.
    pub observation: Option<Observation>,
    /// The owner's open durable asks, read from the authorized Lead Session by the observer
    /// rather than in a render. `None` when no owner session is bound or its read was refused.
    pub asks: Option<Vec<oxa_session::OpenGoalAsk>>,
    /// The wall clock the frame was published at, in milliseconds.
    pub now_ms: u64,
}

impl Frame {
    fn new(
        generation: u64,
        observation: Option<Observation>,
        asks: Option<Vec<oxa_session::OpenGoalAsk>>,
        now_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            generation,
            observation,
            asks,
            now_ms,
        })
    }
}

/// The sources the observer owns. Only the Board sample and the runner situation are file
/// inputs; the Lead session's workers are read from the owner's registry under the same
/// identity rules and are re-read with the Board sample.
pub(crate) struct Sources {
    pub board: Option<PathBuf>,
    pub situation: Option<PathBuf>,
    pub uid: u32,
    pub agent_dir: PathBuf,
    pub sessions_dir: PathBuf,
    /// The Lead owner binding's parent session id, resolved on every read.
    pub parent: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// The owner's open durable asks with the exact facts a change is judged by: the
    /// revisions, never the question text the pages render.
    pub asks: Box<dyn Fn() -> Option<(Vec<(String, u64)>, Vec<oxa_session::OpenGoalAsk>)> + Send + Sync>,
}

/// The shared published frame and the wake-up every stream waits on.
pub(crate) struct Observer {
    published: Mutex<Arc<Frame>>,
    changed: Condvar,
}

impl Observer {
    /// Publish the first frame and the thread that keeps it current. The first read happens
    /// here, so the first page already has an observation.
    pub(crate) fn start(sources: Sources) -> std::sync::Arc<Self> {
        let mut state = State::new();
        state.refresh(&sources, oxa_proto::time::now_ms());
        let first = Frame::new(
            1,
            state.observation.take(),
            state.asks.take(),
            oxa_proto::time::now_ms(),
        );
        let observer = std::sync::Arc::new(Self {
            published: Mutex::new(first),
            changed: Condvar::new(),
        });
        let thread = observer.clone();
        let _ = std::thread::Builder::new()
            .name("oxa-lead-observer".into())
            .spawn(move || {
                let mut generation = 1;
                loop {
                    std::thread::sleep(TICK);
                    let now = oxa_proto::time::now_ms();
                    if state.refresh(&sources, now) {
                        generation += 1;
                        let frame = Frame::new(generation, state.observation.take(), state.asks.take(), now);
                        thread.publish(frame);
                    }
                }
            });
        observer
    }

    /// The current frame. A render only clones this pointer.
    pub(crate) fn frame(&self) -> Arc<Frame> {
        self.published
            .lock()
            .map_or_else(|poison| poison.into_inner().clone(), |frame| frame.clone())
    }

    /// The generation a fresh stream starts from, sampled before its first event goes out:
    /// a frame published while that event is in flight still produces an update.
    pub(crate) fn generation(&self) -> u64 {
        self.frame().generation
    }

    /// Block until the generation differs from `seen`, or `timeout` passes. Returns the
    /// generation now published, so a stream never parses, reads or polls.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        let Ok(mut frame) = self.published.lock() else {
            std::thread::sleep(timeout);
            return seen;
        };
        while frame.generation == seen {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            frame = match self.changed.wait_timeout(frame, left) {
                Ok((frame, _)) => frame,
                Err(poison) => poison.into_inner().0,
            };
        }
        frame.generation
    }

    fn publish(&self, frame: Arc<Frame>) {
        if let Ok(mut published) = self.published.lock() {
            *published = frame;
        }
        self.changed.notify_all();
    }
}

/// What the observer thread remembers between ticks: the stamps it last parsed at and the
/// observation those parses produced.
struct State {
    board: Stamp,
    situation: Stamp,
    observation: Option<Observation>,
    /// The owner's open asks as the last read left them, and the facts a publish compares.
    asks: Option<Vec<oxa_session::OpenGoalAsk>>,
    ask_key: Option<Vec<(String, u64)>>,
}

impl State {
    fn new() -> Self {
        Self {
            board: Stamp::default(),
            situation: Stamp::default(),
            observation: None,
            asks: None,
            ask_key: None,
        }
    }

    /// Re-read whatever changed. `true` when a new frame is worth publishing.
    fn refresh(&mut self, sources: &Sources, now: u64) -> bool {
        // The owner's open asks are read on every tick, but only the facts (id, revision)
        // decide a publish: its owning session file changes its size as the session works.
        let mut published = false;
        match (sources.asks)() {
            Some((key, asks)) if self.ask_key.as_ref() != Some(&key) => {
                self.ask_key = Some(key);
                self.asks = Some(asks);
                published = true;
            }
            Some(_) => {}
            None if self.asks.take().is_some() || self.ask_key.take().is_some() => published = true,
            None => {}
        }
        let board = sources.board.as_deref().map_or(Stamp::default(), Stamp::read);
        let situation = sources.situation.as_deref().map_or(Stamp::default(), Stamp::read);
        if (board, situation) == (self.board, self.situation) {
            let aged = self.observation.as_mut().is_some_and(|o| age(o, now));
            return published || aged;
        }
        self.board = board;
        self.situation = situation;
        let Some(path) = sources.board.as_deref() else {
            self.observation = None;
            return true;
        };
        let mut observation = Observation::read(path, sources.situation.as_deref(), sources.uid, now);
        if let Some(parent) = (sources.parent)() {
            observation = observation.with_workers(&sources.agent_dir, &sources.sessions_dir, &parent);
        }
        self.observation = Some(observation);
        true
    }
}

/// Advance the clock that bounds the Board read, so a source that stops being written turns
/// stale while nothing on disk changes. `true` only when that crossing changed what a page
/// renders, so a quiet system publishes nothing.
fn age(observation: &mut Observation, now: u64) -> bool {
    let before = crate::lead_page::stale(&observation.board, observation.now_ms / 1000);
    observation.now_ms = now;
    crate::lead_page::stale(&observation.board, now / 1000) != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    /// Sources with no owner session bound: the board, no situation, no asks.
    fn sources(test: &TestEnv, board: &std::path::Path) -> Sources {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(test.root()).unwrap().uid();
        Sources {
            board: Some(board.to_path_buf()),
            situation: None,
            uid,
            agent_dir: test.agent_dir(),
            sessions_dir: test.agent_dir().join("sessions"),
            parent: Box::new(|| None),
            asks: Box::new(|| None),
        }
    }

    fn private(test: &TestEnv) -> std::path::PathBuf {
        let directory = test.root().join("observer-inputs");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn record(observed_at: u64) -> String {
        format!("{{\"schema_version\":1,\"observed_at\":{observed_at},\"board\":{{\"observed_at\":{observed_at},\"entries\":[]}}}}\n")
    }

    #[test]
    fn an_unchanged_source_is_neither_parsed_nor_published() {
        let test = TestEnv::new();
        let directory = private(&test);
        let sample = directory.join("board.jsonl");
        std::fs::write(&sample, record(10)).unwrap();
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sources = sources(&test, &sample);
        let mut state = State::new();
        assert!(state.refresh(&sources, 10_000));
        assert_eq!(state.observation.as_ref().unwrap().board.observed_at, 10);
        // Five idle ticks with no size or mtime change publish nothing and reuse the parse.
        let kept = state.observation.as_ref().unwrap() as *const Observation;
        for now in [11_000, 12_000, 13_000, 14_000, 15_000] {
            assert!(!state.refresh(&sources, now));
        }
        assert_eq!(state.observation.as_ref().unwrap() as *const Observation, kept);
        // A quiet source still turns stale once its own age crosses the mark, without a parse.
        assert!(state.refresh(&sources, 10_000 + 901_000));
        assert_eq!(state.observation.as_ref().unwrap() as *const Observation, kept);
        assert!(crate::lead_page::stale(
            &state.observation.as_ref().unwrap().board,
            state.observation.as_ref().unwrap().now_ms / 1000
        ));
    }

    #[test]
    fn a_transient_read_failure_keeps_the_previous_frame() {
        let test = TestEnv::new();
        let directory = private(&test);
        let sample = directory.join("board.jsonl");
        std::fs::write(&sample, record(10)).unwrap();
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sources = sources(&test, &sample);
        let mut state = State::new();
        assert!(state.refresh(&sources, 10_000));
        assert!(state.observation.as_ref().unwrap().cause.is_none());
        // Loose permissions are read as a typed cause, never as an empty board.
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(state.refresh(&sources, 11_000));
        let failed = state.observation.as_ref().unwrap();
        assert!(failed.cause.is_some());
        // The next good read is parsed again and replaces the failed one.
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&sample, record(12)).unwrap();
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(state.refresh(&sources, 12_000));
        let good = state.observation.as_ref().unwrap();
        assert!(good.cause.is_none());
        assert_eq!(good.board.observed_at, 12);
    }

    #[test]
    fn an_open_ask_publishes_on_its_revision_and_a_refused_read_withdraws_it() {
        let test = TestEnv::new();
        let directory = private(&test);
        let sample = directory.join("board.jsonl");
        std::fs::write(&sample, record(10)).unwrap();
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ask = |revision: u64, question: &str| oxa_session::OpenGoalAsk {
            id: "ask-1".into(),
            revision,
            question: question.into(),
        };
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Some((
            vec![("ask-1".to_owned(), 1)],
            vec![ask(1, "the first question")],
        ))));
        let state_source = asked.clone();
        let mut sources = sources(&test, &sample);
        sources.asks = Box::new(move || state_source.lock().unwrap().clone());
        let mut state = State::new();
        assert!(state.refresh(&sources, 10_000));
        assert!(state.observation.as_ref().unwrap().cause.is_none());
        assert_eq!(state.asks.as_ref().unwrap().len(), 1);
        // The same ask, rewritten by its owner, is not a change: no page refresh is sent.
        *asked.lock().unwrap() = Some((vec![("ask-1".to_owned(), 1)], vec![ask(1, "rewritten")]));
        assert!(!state.refresh(&sources, 11_000));
        assert_eq!(state.asks.as_ref().unwrap()[0].question, "the first question");
        // A new revision is: the owner must see the answer they have not given yet.
        *asked.lock().unwrap() = Some((vec![("ask-1".to_owned(), 2)], vec![ask(2, "the next question")]));
        assert!(state.refresh(&sources, 12_000));
        assert_eq!(state.asks.as_ref().unwrap()[0].revision, 2);
        // A session that became unreadable withdraws the asks rather than showing them stale.
        *asked.lock().unwrap() = None;
        assert!(state.refresh(&sources, 13_000));
        assert!(state.asks.is_none());
        assert!(!state.refresh(&sources, 14_000));
    }

    #[test]
    fn an_idle_wait_does_not_poll_and_a_publish_wakes_every_stream() {
        let test = TestEnv::new();
        let directory = private(&test);
        let sample = directory.join("board.jsonl");
        std::fs::write(&sample, record(10)).unwrap();
        std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
        let observer = Observer::start(sources(&test, &sample));
        let seen = observer.generation();
        // Five idle streams on one condvar: the wait is one release and one timeout, no parse.
        let began = Instant::now();
        for _ in 0..5 {
            assert_eq!(observer.wait(seen, Duration::from_millis(50)), seen);
        }
        assert!(began.elapsed() < Duration::from_secs(2));
        // One new sample wakes every stream with the new generation.
        observer.publish(Frame::new(seen + 1, None, None, 0));
        for _ in 0..5 {
            assert_ne!(observer.wait(seen, Duration::from_secs(5)), seen);
        }
        // A frame a stream already knows is not re-sent.
        assert_eq!(observer.wait(seen + 1, Duration::from_millis(20)), seen + 1);
        // A render only clones the published pointer: two frames share one observation.
        assert!(Arc::strong_count(&observer.frame()) >= 2);
    }
}
