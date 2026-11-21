//! W0-FROZEN (architect-owned).
//!
//! # oxa-session
//! Session files (pi's JSONL v3 tree), the transcript fold and tree operations.
//!
//! ## Contracts
//! * **W1.** The wire is a pure fold of the active branch: [`Transcript::project`]. For every
//!   entry that is not a rebasing body the fold only appends. `Transcript::rebase` is the one
//!   place the wire may shrink or change, and it records why (`oxa_proto::rebase_why`).
//! * **W2.** An assistant `Msg` entry stores the exact wire fields as streamed; `encode_entry`
//!   and `decode_entry` round-trip them byte for byte (`load(save(x)) == x`).
//! * **W3.** The prefix of an epoch is an entry (`EntryBody::Prefix`, a pi `message` entry with role `system`) and is
//!   replayed from the file on resume, never rebuilt.
//! * **File safety.** Files are created `0600` inside `0700` directories, opened with
//!   `O_APPEND | O_NOFOLLOW`; a symlinked session path is refused (`unsafe_path`). A torn last
//!   line (no trailing `\n` or invalid JSON) is dropped on load, never repaired in place.
//! * **Layout.** `<sessions_dir>/--<cwd with '/' -> '-'>--/<timestamp>_<id>.jsonl` (pi's scheme),
//!   first line = the session header.

pub mod advice;
mod clock;
mod durable;
mod goal;
mod inbox;
mod lifecycle;
mod lock;

pub use inbox::{
    await_delivery, inbox_pending, remove_inbox, send_inbox, send_inbox_control, send_inbox_receipt,
    send_inbox_steer, send_inbox_steer_receipt, valid_inbox_id, withdraw_inbox, Inbox, InboxItem, InboxRetry,
    Outcome as DeliveryOutcome, INBOX_MESSAGE_BYTES,
};
mod meta;
mod ops;
mod scan;
mod spill;
mod store;
mod transcript;
mod tree;

pub use clock::{local_time, LocalTime};
pub use goal::{
    CheckResult, Goal, GoalBudget, GoalCheck, GoalPlan, GoalStatus, HeldGoalLease, LeadGoalState,
    OpenGoalAsk, PendingGoalSteer, SteeringLease, Subgoal, GOAL_ENTRY,
};
pub use lifecycle::SpillLifecycleLock;
pub use lock::{lock_holder, lock_path_of, SessionLock, LOCKED};
pub use meta::{search_sessions, sort_sessions, SessionSort};
pub use ops::{clone_session, fork_file, fork_session, set_label, set_name};
pub use scan::SessionScan;
pub use spill::{inherited_spill_dirs, remove_session_preserving_spills, remove_spill_dir_unless_forked};
pub use store::{
    count_files, decode_entry, decode_header, encode_entry, encode_header, encode_visible_entry, iso_from_ms,
    new_entry_id, parse_utc_seconds, purge_session_file, remove_all_spill, remove_spill_dir,
    session_dir_name, session_files_under, FsStore, MemStore,
};
pub use transcript::{bash_text, RebaseLog, Transcript};
pub use tree::Tree;

use oxa_proto::{Entry, OxaError};
use std::path::{Path, PathBuf};

/// A session file (or an in-memory session of [`MemStore`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionRef {
    pub path: PathBuf,
    /// The session id (uuid v4 text unless given by `--session-id`).
    pub id: String,
}

/// The first line of a session file (pi's `SessionHeader`, `type: "session"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    /// Always `oxa_proto::names::SESSION_VERSION` when written.
    pub version: u32,
    pub id: String,
    pub ts_ms: u64,
    pub cwd: PathBuf,
    /// Path of the session this one was forked or cloned from (pi's `parentSession`).
    pub parent_session: Option<String>,
}

/// What the session picker and `list` show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub sref: SessionRef,
    pub cwd: PathBuf,
    /// Display name from the last `SessionInfo` entry.
    pub name: Option<String>,
    pub created_ms: u64,
    pub modified_ms: u64,
    /// Number of user + assistant messages.
    pub message_count: u32,
    /// First user message, single line, for the picker.
    pub first_message: String,
    /// All user + assistant text, for search.
    pub all_text: String,
    pub parent_session: Option<String>,
}

/// Session persistence. [`FsStore`] is the file store; [`MemStore`] backs `--no-session`.
pub trait SessionStore: Send + Sync {
    /// Create a session for `cwd` and write its header. `id` = `--session-id` (must be new);
    /// `parent` = the file this session derives from.
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError>;
    /// Append one entry as one line with a single `write` (O_APPEND, 0600 in 0700, O_NOFOLLOW).
    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError>;
    /// Append `entries` in order. The file store writes them in one write; a failure leaves
    /// the entries before it written (as `append` in a loop would).
    fn append_many(&self, s: &SessionRef, entries: &[Entry]) -> Result<(), OxaError> {
        entries.iter().try_for_each(|e| self.append(s, e))
    }
    /// All entries in file order. A torn last line is dropped.
    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError>;
    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError>;
    /// Sessions of `cwd`, or of every directory when `None`; unreadable files are skipped.
    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta>;
    /// Whether `list(None)` covers a session path, even after that session was deleted.
    /// Resolving an explicit path need not imply coverage: file stores can open sessions
    /// outside their root. Cleanup must retain spills whose live references it cannot check.
    /// Unknown store implementations conservatively opt out of spill reclamation.
    fn lists_path(&self, _path: &Path) -> bool {
        false
    }
    /// Exhaustive live references for spill reclamation. Unlike picker listings, this must
    /// fail on unreadable directories, files or headers, not silently omit live sessions.
    fn spill_cleanup_sessions(&self) -> Result<Vec<SessionRef>, OxaError> {
        Err(OxaError::new(
            oxa_proto::codes::INVALID,
            "store cannot enumerate spill references",
        ))
    }
    /// Whether all cross-store children of this session are guaranteed to publish store links.
    /// Legacy sessions cannot establish this retroactively and must retain their spills.
    fn spill_stores_tracked(&self, _session: &SessionRef) -> Result<bool, OxaError> {
        Ok(false)
    }
    /// Serialize source loading through child publication with source deletion. This is
    /// independent of the writer lock, so an open session can still be forked normally.
    /// File-backed stores must coordinate across processes and lock both store endpoints.
    fn spill_lifecycle_lock(&self, _session: &SessionRef) -> Result<SpillLifecycleLock, OxaError> {
        Ok(SpillLifecycleLock::memory())
    }
    /// Hold all stores whose references reclamation will enumerate. The connected component
    /// must be rechecked after locking, in case a concurrent fork published another link.
    fn spill_cleanup_lock(&self) -> Result<SpillLifecycleLock, OxaError> {
        Ok(SpillLifecycleLock::memory())
    }
    /// File-store roots linked by cross-store forks. Cleanup must check all of these stores
    /// before reclaiming spills; errors leave the spills intact. Other store types need no roots.
    fn spill_cleanup_roots(&self) -> Result<Vec<PathBuf>, OxaError> {
        Ok(Vec::new())
    }
    /// Resolve a path, a full id or a unique id prefix (`--session`, `--fork`).
    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError>;
    /// Delete through the `trash` command when available, else unlink (pi's behaviour).
    fn delete(&self, s: &SessionRef) -> Result<(), OxaError>;
    /// R1: make the appends so far durable (`fdatasync` of the session file). Called once at
    /// the end of each turn, not per entry: a power loss costs at most the turn in progress.
    /// The default (in-memory stores) has nothing to sync.
    fn sync(&self, _s: &SessionRef) -> Result<(), OxaError> {
        Ok(())
    }
    /// Become the one writer of `s`: refuse (code `LOCKED`) when another process has it open.
    /// The lock is held until another session is acquired or the store is dropped. In-memory
    /// stores have nothing to lock.
    ///
    /// Two-phase so that a failed switch never unlocks the session still in use: `acquire`
    /// only STAGES the new lock (the current one is untouched); [`SessionStore::commit_lock`]
    /// makes it the held one and drops the old, [`SessionStore::abort_lock`] drops it.
    fn acquire(&self, _s: &SessionRef) -> Result<(), OxaError> {
        Ok(())
    }
    /// The staged lock of [`SessionStore::acquire`] becomes the held one; the old is released.
    fn commit_lock(&self) {}
    /// The staged lock is released; the held one is untouched.
    fn abort_lock(&self) {}
    /// [`SessionStore::delete`], telling how the file went (pi's picker says "Session moved
    /// to trash" or "Session deleted"). Default: `delete`, reported as [`Removed::Deleted`].
    fn remove(&self, s: &SessionRef) -> Result<Removed, OxaError> {
        self.delete(s).map(|()| Removed::Deleted)
    }
}

/// How [`SessionStore::remove`] removed a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// The `trash` command took it (or the file was gone after it ran).
    Trashed,
    /// Unlinked (no `trash`, or it failed).
    Deleted,
}
