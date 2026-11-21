//! Runner-independent facts for the lead (#629). These describe observations, never actions.
//!
//! Only `PrMerged` is landed work. Worker success, local acceptance, publication and external
//! acceptance are separate observations. Producers supply checked evidence references, not logs,
//! prompts, tool arguments or diagnostic text. Storage, authority, deduplication and task state
//! belong to the runner. The JSON boundary is bounded, versioned and rejects unknown fields.
use serde_json::{json, Map, Value};
use std::fmt;

pub const VERSION: u64 = 1;
pub const MAX_EVENT_BYTES: usize = 8192;
pub const MAX_SNAPSHOT_BYTES: usize = 65536;
pub const MAX_PAGE_BYTES: usize = 262144;
pub const MAX_ITEMS: usize = 128;

/// Errors contain only a fixed reason, never rejected input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactsError(&'static str);
impl fmt::Display for FactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for FactsError {}
type Result<T> = std::result::Result<T, FactsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    Mechanical,
    Routine,
    Attention,
}

// Use the crate's existing JSON dependency; no serializer/runtime dependency is added.
trait Codec: Sized {
    fn value(&self) -> Value;
    fn read(value: &Value) -> Result<Self>;
    fn validate(&self) -> Result<()>;
}

fn require(ok: bool, reason: &'static str) -> Result<()> {
    ok.then_some(()).ok_or(FactsError(reason))
}
fn atom(value: &str, max: usize) -> Result<()> {
    require(
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)),
        "invalid fact identifier",
    )
}
fn commit(value: &str) -> Result<()> {
    require(
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "a full lowercase commit is required",
    )
}
fn object<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a Map<String, Value>> {
    let obj = value.as_object().ok_or(FactsError("expected a fact object"))?;
    require(
        obj.len() == keys.len() && keys.iter().all(|key| obj.contains_key(*key)),
        "missing or unknown fact field",
    )?;
    Ok(obj)
}
impl Codec for String {
    fn value(&self) -> Value {
        Value::String(self.clone())
    }
    fn read(value: &Value) -> Result<Self> {
        let text = value.as_str().ok_or(FactsError("expected fact text"))?;
        // Check before allocating a copy.
        require(
            !text.is_empty()
                && text.len() <= 1024
                && !text
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')),
            "fact text must be bounded and single-line",
        )?;
        Ok(text.to_owned())
    }
    fn validate(&self) -> Result<()> {
        require(
            !self.is_empty()
                && self.len() <= 1024
                && !self
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')),
            "fact text must be bounded and single-line",
        )
    }
}
impl Codec for u64 {
    fn value(&self) -> Value {
        (*self).into()
    }
    fn read(value: &Value) -> Result<Self> {
        value
            .as_u64()
            .ok_or(FactsError("expected an unsigned fact number"))
    }
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}
impl Codec for bool {
    fn value(&self) -> Value {
        (*self).into()
    }
    fn read(value: &Value) -> Result<Self> {
        value.as_bool().ok_or(FactsError("expected a fact boolean"))
    }
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}
impl<T: Codec> Codec for Option<T> {
    fn value(&self) -> Value {
        self.as_ref().map_or(Value::Null, Codec::value)
    }
    fn read(value: &Value) -> Result<Self> {
        if value.is_null() {
            Ok(None)
        } else {
            T::read(value).map(Some)
        }
    }
    fn validate(&self) -> Result<()> {
        self.as_ref().map_or(Ok(()), Codec::validate)
    }
}
impl<T: Codec> Codec for Vec<T> {
    fn value(&self) -> Value {
        Value::Array(self.iter().map(Codec::value).collect())
    }
    fn read(value: &Value) -> Result<Self> {
        let array = value.as_array().ok_or(FactsError("expected a fact list"))?;
        require(array.len() <= MAX_ITEMS, "fact list capacity exceeded")?;
        array.iter().map(T::read).collect()
    }
    fn validate(&self) -> Result<()> {
        require(self.len() <= MAX_ITEMS, "fact list capacity exceeded")?;
        self.iter().try_for_each(Codec::validate)
    }
}

macro_rules! choice {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl Codec for $name {
            fn value(&self) -> Value { Value::String(match self { $(Self::$variant => $text),+ }.into()) }
            fn read(value: &Value) -> Result<Self> {
                match value.as_str() { $(Some($text) => Ok(Self::$variant),)+ _ => Err(FactsError("unknown fact kind")) }
            }
            fn validate(&self) -> Result<()> { Ok(()) }
        }
    }
}
macro_rules! record {
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ty:ty),* $(,)? } |$this:ident| $check:block) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name { $(pub $field: $ty),* }
        impl Codec for $name {
            fn value(&self) -> Value { json!({ $(stringify!($field): self.$field.value()),* }) }
            fn read(value: &Value) -> Result<Self> {
                let fields = object(value, &[$(stringify!($field)),*])?;
                let result = Self { $($field: <$ty>::read(&fields[stringify!($field)])?),* };
                result.validate()?;
                Ok(result)
            }
            fn validate(&self) -> Result<()> {
                $(self.$field.validate()?;)*
                let $this = self;
                $check
            }
        }
    }
}
macro_rules! tagged {
    ($(#[$meta:meta])* $name:ident { $($variant:ident($ty:ty) => $kind:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name { $($variant($ty)),+ }
        impl Codec for $name {
            fn value(&self) -> Value {
                match self { $(Self::$variant(data) => json!({"kind":$kind, "data":data.value()})),+ }
            }
            fn read(value: &Value) -> Result<Self> {
                let fields = object(value, &["kind", "data"])?;
                match fields["kind"].as_str() {
                    $(Some($kind) => <$ty>::read(&fields["data"]).map(Self::$variant),)+
                    _ => Err(FactsError("unknown fact kind")),
                }
            }
            fn validate(&self) -> Result<()> {
                match self { $(Self::$variant(data) => data.validate()),+ }
            }
        }
    }
}

// `publication_pending`: finished work whose publication action left no exact-head receipt (#2304).
choice!(WorkerOutcome {
    Done => "done", NothingToDo => "nothing_to_do", Blocked => "blocked",
    Failed => "failed", Interrupted => "interrupted", PublicationPending => "publication_pending",
});
choice!(TaskStep {
    Preparing => "preparing", Worker => "worker", Acceptance => "acceptance",
    Repair => "repair", Recheck => "recheck", Saving => "saving", Finished => "finished",
});
choice!(StepReason {
    Resumed => "resumed", Prepared => "prepared", WorkerDone => "worker_done",
    WorkerNothingToDo => "worker_nothing_to_do", WorkerBlocked => "worker_blocked",
    WorkerFailed => "worker_failed", ChecksPassed => "checks_passed", ChecksFailed => "checks_failed",
    RepairDone => "repair_done", RepairFailed => "repair_failed", Saved => "saved", SaveFailed => "save_failed",
    Interrupted => "interrupted", Deadline => "deadline", SessionUnavailable => "session_unavailable",
    SetupFailed => "setup_failed",
});
choice!(EvidenceKind {
    LocalCheck => "local_check", Gate => "gate", Review => "review", Queue => "queue",
    Status => "status", Standing => "standing",
});
choice!(MechanicalKind {
    TransientRetry => "transient_retry", NoBuildRequeue => "no_build_requeue",
    Refill => "refill", CleanBranchUpdate => "clean_branch_update",
});
choice!(IncidentKind {
    TransientRefusal => "transient_refusal", NoBuildRequeue => "no_build_requeue",
    RuleReminder => "rule_reminder", RuleConflict => "rule_conflict",
    ReviewLimit => "review_limit", ReservedAction => "reserved_action",
    BriefMissing => "brief_missing", HostIdle => "host_idle",
    BudgetWarning => "budget_warning", BudgetExhausted => "budget_exhausted",
    WorkerStalled => "worker_stalled", QueueRejected => "queue_rejected",
    MainFailed => "main_failed", SevereFinding => "severe_finding",
    InboxMessage => "inbox_message", UnreadInbox => "unread_inbox",
});
impl IncidentKind {
    pub fn class(self) -> EventClass {
        match self {
            Self::TransientRefusal | Self::NoBuildRequeue | Self::RuleReminder => EventClass::Mechanical,
            _ => EventClass::Attention,
        }
    }
}

/// A PR subject is independent of its current head: changing the head cannot lift a hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Repository,
    Task(String),
    PullRequest(u64),
    Host(String),
    Inbox(String),
}
impl Codec for Subject {
    fn value(&self) -> Value {
        match self {
            Self::Repository => json!({"kind":"repository", "data":null}),
            Self::Task(id) => json!({"kind":"task", "data":id}),
            Self::PullRequest(pr) => json!({"kind":"pull_request", "data":pr}),
            Self::Host(id) => json!({"kind":"host", "data":id}),
            Self::Inbox(id) => json!({"kind":"inbox", "data":id}),
        }
    }
    fn read(value: &Value) -> Result<Self> {
        let fields = object(value, &["kind", "data"])?;
        let data = &fields["data"];
        let subject = match fields["kind"].as_str() {
            Some("repository") if data.is_null() => Self::Repository,
            Some("task") => Self::Task(String::read(data)?),
            Some("pull_request") => Self::PullRequest(u64::read(data)?),
            Some("host") => Self::Host(String::read(data)?),
            Some("inbox") => Self::Inbox(String::read(data)?),
            _ => return Err(FactsError("unknown fact subject")),
        };
        subject.validate()?;
        Ok(subject)
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Repository => Ok(()),
            Self::PullRequest(pr) => require(*pr > 0, "PR number must be positive"),
            Self::Task(id) | Self::Host(id) | Self::Inbox(id) => atom(id, 128),
        }
    }
}

record!(
    /// Stable source/revision identifies a durable observation on restart, before its event seq.
    Checkpoint {
        source: String,
        revision: u64
    } | s
        | {
            atom(&s.source, 64)?;
            require(s.revision > 0, "checkpoint revision must be positive")
        }
);
record!(
    PullRequest {
        number: u64,
        head: String
    } | s
        | {
            require(s.number > 0, "PR number must be positive")?;
            commit(&s.head)
        }
);
record!(
    Evidence {
        kind: EvidenceKind,
        id: String
    } | s
        | { atom(&s.id, 256) }
);
record!(Running {
    task: String, host: String, branch: String, worktree: String, session_id: Option<String>, step: TaskStep,
} |s| {
    atom(&s.task, 128)?;
    atom(&s.host, 128)?;
    require(!s.branch.starts_with('-') && s.branch.len() <= 256, "invalid task branch")?;
    if let Some(id) = &s.session_id { atom(id, 128)?; }
    require(s.step != TaskStep::Finished, "finished task cannot be running")?;
    Ok(())
});
record!(
    /// One durable runner step, with no task-state contents or implied publication/landing.
    /// Finished means this local attempt ended; only the separate acceptance/merge facts
    /// provide those respective claims. A terminal step requires an explicit fixed reason.
    TaskTransition { task: String, step: TaskStep, session_id: Option<String>, reason: Option<StepReason> } |s| {
        atom(&s.task, 128)?;
        if let Some(id) = &s.session_id { atom(id, 128)?; }
        require(s.step != TaskStep::Finished || s.reason.is_some(), "finished step requires a reason")
    }
);
record!(WorkerFinished {
    task: String, session_id: Option<String>, outcome: WorkerOutcome, error_class: Option<String>,
} |s| {
    atom(&s.task, 128)?;
    if let Some(id) = &s.session_id { atom(id, 128)?; }
    if let Some(code) = &s.error_class { atom(code, 64)?; }
    require(
        match s.outcome {
            WorkerOutcome::Done | WorkerOutcome::NothingToDo | WorkerOutcome::PublicationPending => {
                s.error_class.is_none()
            }
            WorkerOutcome::Blocked | WorkerOutcome::Failed | WorkerOutcome::Interrupted => {
                s.error_class.is_some()
            }
        },
        "worker outcome and error class disagree",
    )
});
record!(
    LocalAcceptance {
        task: String,
        commit: String,
        evidence: Evidence
    } | s
        | {
            atom(&s.task, 128)?;
            commit(&s.commit)?;
            require(
                s.evidence.kind == EvidenceKind::LocalCheck,
                "local acceptance needs local check evidence",
            )
        }
);
record!(
    Publication {
        task: String,
        pr: PullRequest
    } | s
        | { atom(&s.task, 128) }
);
record!(
    ExternalAcceptance {
        pr: PullRequest,
        evidence: Evidence
    } | s
        | {
            require(
                matches!(
                    s.evidence.kind,
                    EvidenceKind::Gate | EvidenceKind::Review | EvidenceKind::Queue
                ),
                "external acceptance needs external evidence",
            )
        }
);
record!(
    Landed {
        pr: PullRequest,
        merge_commit: String,
        merged_at_ms: u64
    } | s
        | { commit(&s.merge_commit) }
);
record!(Blocked { subject: Subject, code: String, ask_id: Option<String> } |s| {
    atom(&s.code, 64)?;
    if let Some(id) = &s.ask_id { atom(id, 128)?; }
    Ok(())
});
record!(
    Ask {
        id: String,
        subject: Subject,
        question: String
    } | s
        | {
            atom(&s.id, 128)?;
            require(s.question.len() <= 512, "ask question capacity exceeded")
        }
);
record!(AskClosed { id: String } | s | { atom(&s.id, 128) });
record!(Incident { id: String, subject: Subject, kind: IncidentKind, evidence: Option<Evidence> } |s| { atom(&s.id, 256) });
record!(
    /// One durable standing revision. Ask details/answers stay in the protected standing file.
    StandingChanged {
        revision: u64, goal: Option<String>, ask_opened: Option<Ask>,
        ask_closed: Option<AskClosed>, incident: Option<Incident>
    } |s| {
        require(s.revision > 0, "standing revision must be positive")?;
        require(s.ask_opened.is_none() || s.ask_closed.is_none(), "standing mutation has two ask transitions")
    }
);
record!(
    Mechanical {
        subject: Subject,
        kind: MechanicalKind
    } | _s
        | { Ok(()) }
);

tagged!(
    /// No variant grants permission or updates runner state. Only PrMerged is evidence of landing.
    Fact {
        TaskStarted(Running) => "task_started",
        TaskTransition(TaskTransition) => "task_transition",
        WorkerFinished(WorkerFinished) => "worker_finished",
        LocalAcceptance(LocalAcceptance) => "local_acceptance",
        Published(Publication) => "published",
        ExternalAcceptance(ExternalAcceptance) => "external_acceptance",
        Enqueued(PullRequest) => "enqueued",
        PrMerged(Landed) => "pr_merged",
        Blocked(Blocked) => "blocked",
        Unblocked(Subject) => "unblocked",
        AskOpened(Ask) => "ask_opened",
        AskClosed(AskClosed) => "ask_closed",
        Incident(Incident) => "incident",
        StandingChanged(StandingChanged) => "standing_changed",
        Mechanical(Mechanical) => "mechanical",
    }
);
impl Fact {
    /// Fixed classification; neither the producer nor the model chooses the wake class.
    pub fn class(&self) -> EventClass {
        match self {
            Self::Mechanical(_) => EventClass::Mechanical,
            Self::Blocked(_) | Self::AskOpened(_) => EventClass::Attention,
            Self::Incident(incident) => incident.kind.class(),
            Self::StandingChanged(change) if change.ask_opened.is_some() => EventClass::Attention,
            Self::StandingChanged(change) => change
                .incident
                .as_ref()
                .map_or(EventClass::Routine, |incident| incident.kind.class()),
            _ => EventClass::Routine,
        }
    }
}

record!(
    /// Sequence is repository-local, starts at one and never resets on rotation.
    Event {
        seq: u64,
        at_ms: u64,
        checkpoint: Checkpoint,
        fact: Fact
    } | s
        | {
            require(s.seq > 0, "event sequence must be positive")?;
            if let Fact::StandingChanged(change) = &s.fact {
                require(
                    s.checkpoint.source == "standing" && s.checkpoint.revision == change.revision,
                    "standing checkpoint identity disagrees",
                )?;
            }
            require(
                s.checkpoint.source != "standing" || matches!(s.fact, Fact::StandingChanged(_)),
                "standing source is reserved for standing revisions",
            )?;
            Ok(())
        }
);
record!(
    /// A bounded materialized read view, not task state. Never drop running/blocked/asks to fit.
    /// Retained terminal/incident history may be shorter; history_truncated makes that explicit.
    Snapshot {
        as_of_seq: u64, at_ms: u64, standing_revision: u64, goal: Option<String>,
        running: Vec<Running>, local_accepted: Vec<LocalAcceptance>, published: Vec<Publication>,
        landed: Vec<Landed>, blocked: Vec<Blocked>, asks: Vec<Ask>, incidents: Vec<Incident>,
        history_truncated: bool,
    } |s| {
        require(
            s.running.len() + s.local_accepted.len() + s.published.len() + s.landed.len()
                + s.blocked.len() + s.asks.len() + s.incidents.len() <= MAX_ITEMS,
            "situation capacity exceeded",
        )
    }
);
record!(
    /// Events after `after`, with an explicit retention gap. next is the last returned seq,
    /// or max(after, first_available - 1) for an empty page. first_available may be the next seq
    /// when no journal entry is retained. An empty page never means a corrupt store is healthy.
    EventPage { after: u64, events: Vec<Event>, next: u64, first_available: u64, gap: bool } |s| {
        require(s.first_available > 0, "first available sequence must be positive")?;
        require(s.gap == (s.after.saturating_add(1) < s.first_available), "incorrect event gap")?;
        require(s.events.is_empty() || s.after < u64::MAX, "event sequence exhausted")?;
        let start = s.after.saturating_add(1).max(s.first_available);
        for (offset, event) in s.events.iter().enumerate() {
            require(start.checked_add(offset as u64) == Some(event.seq), "event page is not contiguous")?;
        }
        let next = s.events.last().map_or(s.after.max(s.first_available - 1), |event| event.seq);
        require(s.next == next, "incorrect next event cursor")
    }
);

fn encode<T: Codec>(data: &T, kind: &str, limit: usize) -> Result<Vec<u8>> {
    data.validate()?;
    let bytes = serde_json::to_vec(&json!({"type":kind, "version":VERSION, "data":data.value()}))
        .map_err(|_| FactsError("fact serialization failed"))?;
    require(bytes.len() <= limit, "fact byte capacity exceeded")?;
    Ok(bytes)
}
fn decode<T: Codec>(bytes: &[u8], kind: &str, limit: usize) -> Result<T> {
    require(bytes.len() <= limit, "fact byte capacity exceeded")?;
    let value: Value = serde_json::from_slice(bytes).map_err(|_| FactsError("invalid facts JSON"))?;
    let fields = object(&value, &["type", "version", "data"])?;
    require(
        fields["type"].as_str() == Some(kind),
        "incorrect facts document type",
    )?;
    require(
        fields["version"].as_u64() == Some(VERSION),
        "unsupported facts version",
    )?;
    T::read(&fields["data"])
}
macro_rules! document {
    ($name:ident, $kind:literal, $limit:ident) => {
        impl $name {
            pub fn to_json(&self) -> Result<Vec<u8>> {
                encode(self, $kind, $limit)
            }
            pub fn from_json(bytes: &[u8]) -> Result<Self> {
                decode(bytes, $kind, $limit)
            }
        }
    };
}
document!(Event, "oxa_work_event", MAX_EVENT_BYTES);
document!(Snapshot, "oxa_work_situation", MAX_SNAPSHOT_BYTES);
document!(EventPage, "oxa_work_events", MAX_PAGE_BYTES);
