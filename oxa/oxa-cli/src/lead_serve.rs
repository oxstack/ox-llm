//! Minimal local board caller. Transport, policy, peer proof and rendering stay shared.
use crate::dispatch::LeadOwnerBinding;
use crate::lead_access::{self, proxy, Config, IdentityProvider, Mode, VerifiedIdentity};
use crate::lead_attach::{self, AttachedSession, TAIL_BYTES};
use crate::lead_chat::conversation::ConversationReply;
use crate::lead_http::{OwnerForm, OwnerSubmission, RefusalFact, RefusalLog, Stage};
use oxa_config::{Dirs, Env, Settings};
use oxa_perm::ApprovalDesk;
use oxa_proto::{codes, Entry, OxaError};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

fn invalid(reason: &str) -> OxaError {
    OxaError::new(codes::INVALID, reason)
}

/// The real service journal of the lead listener: one fixed, allowlisted line
/// per refused chat request, written to the process's stderr so the operator's
/// existing service unit already carries it to the system journal
/// (`journalctl --user -u oxa-lead.service`). Every line carries only the
/// response status and fixed stage/reason/fix tokens; owner identity, message
/// text, token counts, URLs, headers, keys and CSRF values are never written,
/// and no fact carries request or error bytes. There is no new file, rotation
/// or dependency, and a failed write never blocks or changes a refusal.
#[derive(Clone)]
struct ServiceJournal {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl ServiceJournal {
    /// The production sink: the process's standard error.
    fn stderr() -> Self {
        Self {
            out: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
        }
    }

    fn log(&self, fact: RefusalFact) {
        let Ok(mut out) = self.out.lock() else {
            return;
        };
        let _ = writeln!(
            out,
            "lead chat refusal status={} stage={} reason={} fix={}",
            fact.status,
            fact.stage.as_str(),
            fact.reason,
            fact.fix
        );
    }

    fn sink(&self) -> RefusalLog {
        let journal = self.clone();
        Arc::new(move |fact| journal.log(fact))
    }
}

/// The allowlisted fact of one refused conversation turn. The status is the chat
/// response the owner saw; the engine's typed refusal code is folded into the
/// fixed reason token, and the message bytes are never read or written.
fn conversation_refusal(error: &OxaError) -> RefusalFact {
    let fact = |stage, reason, fix| RefusalFact {
        status: 503,
        stage,
        reason,
        fix,
    };
    match error.code {
        "budget_not_configured" => fact(
            Stage::Limits,
            "budget_invalid",
            "set oxa.cloud.daily_budget_usd to a positive number, or unset it for no USD limit",
        ),
        "cost_unknown_history" => fact(
            Stage::Limits,
            "cost_unknown_history",
            "unset oxa.cloud.daily_budget_usd, or wait for the next spend day; earlier uncapped requests today have an unknown cost",
        ),
        "budget_exceeded" | codes::BUDGET_EXHAUSTED => fact(
            Stage::Limits,
            "metered_budget_exhausted",
            "wait for the next spend day or raise the daily USD budget",
        ),
        // The bounded literal turn's own local output bound is not metering: a
        // partial answer that ran out of its shared budget keeps its own reason.
        codes::LITERAL_OUTPUT_EXHAUSTED => fact(
            Stage::Reply,
            "literal_output_exhausted",
            "ask for a shorter answer; the bounded Lead reply stopped at its output limit",
        ),
        "subscription_limit_exceeded" => fact(
            Stage::Limits,
            "subscription_limits_exhausted",
            "wait for the next spend day or adjust the positive subscription limits",
        ),
        "subscription_limits_not_configured" => fact(
            Stage::Limits,
            "subscription_limits_absent",
            "set positive daily subscription limits in user settings",
        ),
        "subscription_limit_reached"
        | "subscription_usage_limit_reached"
        | "subscription_usage_not_included"
        | "subscription_private"
        | "region_blocked" => fact(
            Stage::Limits,
            "subscription_plan_refused",
            "check the subscription plan limits with the provider",
        ),
        "lead_reply_invalid" => fact(
            Stage::Reply,
            "invalid",
            "retry the chat turn; the reply did not meet the contract",
        ),
        // A citation the bounded input never offered is the reply's own failure:
        // it stays a fixed reason, without any reply or input bytes.
        "lead_evidence_invalid" => fact(
            Stage::Reply,
            "evidence_invalid",
            "cite only the observations offered in the bounded input, each at most once",
        ),
        "lead_reply_incomplete" => fact(
            Stage::Reply,
            "incomplete",
            "retry the chat turn; the reply did not complete",
        ),
        codes::USAGE => fact(
            Stage::Body,
            "message",
            "keep the chat message within the served bound",
        ),
        "lead_owner_unavailable" => fact(
            Stage::Recheck,
            "binding",
            "check oxa.lead.sessionId and the original dedicated Lead session file",
        ),
        codes::BUSY => fact(
            Stage::Busy,
            "session",
            "wait for the current Lead session turn to finish",
        ),
        codes::NOT_FOUND => fact(
            Stage::Provider,
            "model",
            "set oxa.lead.model to an exact model id from the engine's list",
        ),
        codes::OFFLINE => fact(
            Stage::Provider,
            "offline",
            "disable offline mode for lead chat",
        ),
        codes::ENGINE_DOWN => fact(
            Stage::Provider,
            "engine_down",
            "start or repair the engine endpoint",
        ),
        "cloud_rate_limited" => fact(
            Stage::Provider,
            "rate_limited",
            "wait for the provider rate limit to reset",
        ),
        "cloud_quota_exhausted" => fact(
            Stage::Provider,
            "quota_exhausted",
            "switch models or wait for the provider quota to reset",
        ),
        // Gateway admission refusals that are safe to name: fixed text only.
        "cloud_provider_forbidden" => fact(
            Stage::Provider,
            "provider_forbidden",
            "this subscription provider serves only the owner agent",
        ),
        "cloud_not_shared" => fact(
            Stage::Provider,
            "not_shared",
            "cloud models are never served to shared guests",
        ),
        _ => fact(
            Stage::Provider,
            "refused",
            "check the engine endpoint and provider status",
        ),
    }
}

/// The allowlisted identity fact of one refused request, from the typed access
/// refusal: known identity reasons stay distinguishable in the journal.
fn identity_fact(refusal: lead_access::Refusal) -> RefusalFact {
    let fact = |reason, fix| RefusalFact {
        status: 403,
        stage: Stage::Identity,
        reason,
        fix,
    };
    match refusal {
        lead_access::Refusal::Disabled => fact("disabled", "enable oxa.lead.serve in the user settings"),
        lead_access::Refusal::LocalOnly => {
            fact("non_loopback", "reach the lead listener from a loopback peer")
        }
        lead_access::Refusal::Unauthenticated => {
            fact("unverified", "verify the configured identity provider and login")
        }
        lead_access::Refusal::NotAllowed => fact(
            "not_allowed",
            "use the configured owner identity and exact authority",
        ),
        lead_access::Refusal::ClientToken => RefusalFact {
            status: 401,
            stage: Stage::Identity,
            reason: "client_token",
            fix: "open the URL printed by oxa lead serve",
        },
        lead_access::Refusal::ReadOnly => fact(
            "not_allowed",
            "use the configured owner identity and exact authority",
        ),
    }
}

enum Proof<'a> {
    Local(bool),
    Proxy(proxy::Proxy<'a>),
}
impl IdentityProvider for Proof<'_> {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        match self {
            Self::Local(owner) => owner.then_some(VerifiedIdentity::LocalOwner),
            Self::Proxy(proof) => proof.verified(),
        }
    }
}

fn authorized(
    config: &Config,
    uid: u32,
    stream: &TcpStream,
    request: &crate::lead_http::Request<'_>,
    journal: &ServiceJournal,
    token: &str,
) -> bool {
    let allowed = (|| {
        let peer = stream
            .peer_addr()
            .map_err(|_| lead_access::Refusal::Unauthenticated)?;
        // The same-user peer is not proof enough on loopback: any command of this user can
        // connect. The client must also hold the token the foreground command printed.
        if config.mode() == Mode::Local && !lead_access::client_token_ok(token, request.client_token) {
            request.denied.set(401);
            return Err(lead_access::Refusal::ClientToken);
        }
        let proof = match config.mode() {
            Mode::Local => Proof::Local(crate::lead_peer::owner_uid(stream) == Ok(uid)),
            Mode::Tailscale => Proof::Proxy(proxy::verify(config, stream, request.forwarded_login)?),
        };
        if request.method == "POST" && request.path == "/approvals" {
            lead_access::approval_post(config, peer.ip(), request, &proof)
        } else if request.method == "POST" {
            lead_access::chat_post(config, peer.ip(), request, &proof)
        } else {
            lead_access::read_only(
                config,
                request.method,
                peer.ip(),
                request.authority,
                request.forwarded_login,
                &proof,
                || (),
            )
        }
    })();
    if let Err(refused) = allowed {
        if crate::lead_http::chat_path(request.path) || crate::lead_http::approvals_path(request.path) {
            journal.log(identity_fact(refused));
        }
    }
    allowed.is_ok()
}

struct ChatReply {
    reply: ConversationReply,
    proposal: Option<(String, serde_json::Value)>,
    confirmed: bool,
}

/// The session the owner page talks to: the dedicated Lead session (a bounded turn under its
/// writer lock) or a running owner session it is attached to (read-only tail, send through the
/// inbox, #2075).
#[derive(Clone)]
enum OwnerSession {
    Dedicated(LeadOwnerBinding),
    Attached(AttachedSession),
}

const DEDICATED_FIX: &str = "check oxa.lead.sessionId and the original dedicated Lead session file";
const ATTACHED_FIX: &str = "check that oxa.lead.attachSessionId names a session of this user";
const NO_TARGET_FIX: &str = "choose a running session on the chat page, or set oxa.lead.attachSessionId";
const NOT_LIVE_FIX: &str = "choose a session from the list of running sessions";

impl OwnerSession {
    fn recheck(&self) -> Result<(), OxaError> {
        match self {
            Self::Dedicated(binding) => binding.recheck(),
            Self::Attached(attached) => attached.recheck(),
        }
    }

    fn fix(&self) -> &'static str {
        match self {
            Self::Dedicated(_) => DEDICATED_FIX,
            Self::Attached(_) => ATTACHED_FIX,
        }
    }

    /// The entries the durable goal is read from. An attached session shows only the newest
    /// part of its file, so a goal can be read from it only when that part is the whole file.
    fn read_entries_for_goal(&self) -> Result<Vec<Entry>, OxaError> {
        match self {
            Self::Dedicated(binding) => binding.read_entries(),
            Self::Attached(attached) => {
                let tail = attached.tail(TAIL_BYTES)?;
                if tail.truncated {
                    return Err(OxaError::new(
                        "lead_owner_unavailable",
                        "The selected Lead session is unavailable.",
                    ));
                }
                Ok(tail.entries)
            }
        }
    }
}

/// The configured owner proof of the service: the session it started on, if any.
fn recheck_start(start: &Option<OwnerSession>) -> Result<(), OxaError> {
    start.as_ref().map_or(Ok(()), OwnerSession::recheck)
}

/// Whether the page is on the target the service started on.
fn same_target(start: &Option<OwnerSession>, current: &Option<OwnerSession>) -> bool {
    match (start, current) {
        (None, None) | (Some(OwnerSession::Dedicated(_)), Some(OwnerSession::Dedicated(_))) => true,
        (Some(OwnerSession::Attached(a)), Some(OwnerSession::Attached(b))) => {
            a.session_id() == b.session_id()
        }
        _ => false,
    }
}

struct OwnerChat {
    dirs: Dirs,
    uid: u32,
    /// The session the service started on: the configured Lead session, or none (the list alone).
    start: Option<OwnerSession>,
    /// The session the page talks to now. The owner's `attach` choice replaces it until the
    /// service restarts; `oxa.lead.attachSessionId` stays the start-up default.
    target: Mutex<Option<OwnerSession>>,
    csrf: String,
    latest: Mutex<Option<ChatReply>>,
    /// A message was accepted into the attached session's inbox and not yet shown as sent.
    sent: AtomicBool,
    journal: ServiceJournal,
}

impl OwnerChat {
    fn new(dirs: Dirs, uid: u32, start: Option<OwnerSession>, csrf: String, journal: ServiceJournal) -> Self {
        Self {
            dirs,
            uid,
            target: Mutex::new(start.clone()),
            start,
            csrf,
            latest: Mutex::new(None),
            sent: AtomicBool::new(false),
            journal,
        }
    }

    fn current(&self) -> Option<OwnerSession> {
        self.target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The owner chose a running session, or went back to the start-up target. A session that is
    /// not in the live list is refused and the target stays as it was.
    fn attach(&self, choice: Option<String>) -> Result<(), &'static str> {
        let next = match choice {
            Some(id) => match AttachedSession::select(&self.dirs, self.uid, &id) {
                Ok(attached) => Some(OwnerSession::Attached(attached)),
                Err(_) => {
                    self.journal.log(RefusalFact {
                        status: 404,
                        stage: Stage::Recheck,
                        reason: "not_live",
                        fix: NOT_LIVE_FIX,
                    });
                    return Err("404 Not Found");
                }
            },
            None => self.start.clone(),
        };
        *self
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
        self.sent.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn post(&self, submission: OwnerSubmission, env: &Env) -> Result<(), &'static str> {
        let mut state = self.latest.try_lock().map_err(|_| {
            self.journal.log(RefusalFact {
                status: 409,
                stage: Stage::Busy,
                reason: "locked",
                fix: "wait for the in-flight chat turn to finish",
            });
            "409 Conflict"
        })?;
        let submission = match submission {
            OwnerSubmission::Attach(choice) => return self.attach(choice),
            other => other,
        };
        let Some(session) = self.current() else {
            self.journal.log(RefusalFact {
                status: 503,
                stage: Stage::Recheck,
                reason: "binding",
                fix: NO_TARGET_FIX,
            });
            return Err("503 Service Unavailable");
        };
        // The attached form names the session it was rendered for. Another tab or device may
        // have switched the target since, and the text must not land in a different session.
        let submission = match submission {
            OwnerSubmission::AttachedMessage { session: id, text } => {
                if !matches!(&session, OwnerSession::Attached(attached) if attached.session_id() == id) {
                    self.journal.log(RefusalFact {
                        status: 409,
                        stage: Stage::Reply,
                        reason: "stale_target",
                        fix: "reload the page and send again to the session it shows",
                    });
                    return Err("409 Conflict");
                }
                OwnerSubmission::Message(text)
            }
            other => other,
        };
        let binding_refusal = || {
            self.journal.log(RefusalFact {
                status: 503,
                stage: Stage::Recheck,
                reason: "binding",
                fix: session.fix(),
            });
            "503 Service Unavailable"
        };
        session.recheck().map_err(|_| binding_refusal())?;
        let OwnerSession::Dedicated(binding) = &session else {
            // Attached: the text goes to the session's inbox, as `oxa send` does. It is a
            // follow-up for the model, never an approval; there are no proposals here.
            let (OwnerSession::Attached(attached), OwnerSubmission::Message(message)) =
                (&session, submission)
            else {
                self.journal.log(RefusalFact {
                    status: 409,
                    stage: Stage::Reply,
                    reason: "proposal",
                    fix: "confirm the currently displayed proposal id",
                });
                return Err("409 Conflict");
            };
            attached.send(&message).map_err(|_| {
                self.journal.log(RefusalFact {
                    status: 503,
                    stage: Stage::Recheck,
                    reason: "inbox",
                    fix: ATTACHED_FIX,
                });
                "503 Service Unavailable"
            })?;
            self.sent.store(true, Ordering::SeqCst);
            return Ok(());
        };
        match submission {
            OwnerSubmission::Message(message) => {
                let reply =
                    crate::lead_chat::converse_bound(&message, binding, env, true).map_err(|error| {
                        self.journal.log(conversation_refusal(&error));
                        "503 Service Unavailable"
                    })?;
                session.recheck().map_err(|_| binding_refusal())?;
                let proposal = reply
                    .action
                    .as_ref()
                    .map(proposal_for_confirmation)
                    .transpose()
                    .map_err(|_| {
                        self.journal.log(RefusalFact {
                            status: 503,
                            stage: Stage::Reply,
                            reason: "proposal",
                            fix: "confirm the currently displayed proposal id",
                        });
                        "503 Service Unavailable"
                    })?;
                *state = Some(ChatReply {
                    reply,
                    proposal,
                    confirmed: false,
                });
            }
            OwnerSubmission::Confirm(id) => {
                let refusal = || {
                    self.journal.log(RefusalFact {
                        status: 409,
                        stage: Stage::Reply,
                        reason: "proposal",
                        fix: "confirm the currently displayed proposal id",
                    });
                    "409 Conflict"
                };
                let current = state.as_mut().ok_or_else(refusal)?;
                let (expected, proposal) = current.proposal.as_ref().ok_or_else(refusal)?;
                if id != *expected || current.confirmed {
                    self.journal.log(RefusalFact {
                        status: 409,
                        stage: Stage::Reply,
                        reason: "proposal",
                        fix: "confirm the currently displayed proposal id",
                    });
                    return Err("409 Conflict");
                }
                binding.record_confirmation(&id, proposal.clone()).map_err(|_| {
                    self.journal.log(RefusalFact {
                        status: 503,
                        stage: Stage::Recheck,
                        reason: "session",
                        fix: "recheck the dedicated Lead Session state",
                    });
                    "503 Service Unavailable"
                })?;
                current.confirmed = true;
            }
            // The run closure answers approvals itself, so they never wait on the chat turn lock.
            OwnerSubmission::Approve { .. }
            | OwnerSubmission::Attach(_)
            | OwnerSubmission::AttachedMessage { .. } => return Err("409 Conflict"),
        }
        Ok(())
    }
}

/// The owner's answers to approval requests (#1512 S6). The writes are the ones `oxa send
/// --approve` makes, through the same path, so a web answer and a CLI answer are equivalent.
struct OwnerApprovals {
    session: Option<OwnerSession>,
    dirs: Dirs,
    cwd: PathBuf,
    desk: ApprovalDesk,
    journal: ServiceJournal,
}

impl OwnerApprovals {
    fn post(&self, id: &str, answer: &str) -> Result<(), &'static str> {
        let refuse = |status: u16, text: &'static str, reason: &'static str| -> Result<(), &'static str> {
            self.journal.log(RefusalFact {
                status,
                stage: Stage::Approval,
                reason,
                fix: "answer a request still listed as waiting for approval",
            });
            Err(text)
        };
        if recheck_start(&self.session).is_err() {
            return refuse(503, "503 Service Unavailable", "binding");
        }
        let waiting = match self.desk.pending() {
            Ok(waiting) => waiting,
            Err(_) => return refuse(503, "503 Service Unavailable", "desk"),
        };
        // Unknown and already answered ids are the same refusal: only pending requests answer.
        let Some(found) = waiting.iter().find(|rec| rec.id == id) else {
            return refuse(404, "404 Not Found", "not_pending");
        };
        let session = found.session.clone();
        // The owner form is this answer's confirmation: it already showed the request, its
        // command and reasons, and passed the form's CSRF and origin checks.
        if crate::approve_cmd::approve_with(
            &self.dirs,
            &self.cwd,
            &session,
            id,
            answer,
            &mut std::io::sink(),
            &mut |_| Some(true),
        )
        .is_err()
        {
            return refuse(409, "409 Conflict", "answer");
        }
        Ok(())
    }
}

fn proposal_for_confirmation(
    draft: &crate::lead_chat::conversation::ProposedAction,
) -> Result<(String, serde_json::Value), &'static str> {
    let unavailable = "503 Service Unavailable";
    let id = oxa_mcp::pkce_pair(&oxa_mcp::random_bytes().map_err(|_| unavailable)?).0;
    let proposal = serde_json::to_value(draft).map_err(|_| unavailable)?;
    Ok((id, proposal))
}

struct Source {
    uid: u32,
    board: Option<PathBuf>,
    /// The runner situation the loop also reads (`OXA_LEAD_SITUATION`); the pages never had it.
    situation: Option<PathBuf>,
    agent_dir: PathBuf,
    sessions_dir: PathBuf,
    /// Whether the authenticated live route is installed, and the digest of this executable.
    live: bool,
    running_sha256: Option<String>,
    form: Option<crate::lead_http::FixedForm>,
    owner: Option<Arc<OwnerChat>>,
    desk: Option<ApprovalDesk>,
    /// The one cached frame every page and every live stream reads (#2343). Until
    /// [`Observer::start`] runs, each render reads the sources once itself.
    observer: Option<Arc<crate::lead_observer::Observer>>,
}

impl Source {
    fn new(env: &Env, uid: u32) -> Result<Self, OxaError> {
        let board = env.get("OXA_LEAD_BOARD_LOG").map(PathBuf::from);
        if board.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("lead board sample path must be absolute"));
        }
        let situation = env.get("OXA_LEAD_SITUATION").map(PathBuf::from);
        if situation.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("lead situation path must be absolute"));
        }
        Ok(Self {
            uid,
            board,
            situation,
            agent_dir: PathBuf::new(),
            sessions_dir: PathBuf::new(),
            live: false,
            running_sha256: None,
            form: None,
            owner: None,
            desk: None,
            observer: None,
        })
    }

    /// The exact id of the session the Lead's workers belong to: its owner binding's.
    fn parent_session(&self) -> Option<&str> {
        self.owner
            .as_ref()
            .and_then(|owner| owner.start.as_ref())
            .map(|start| match start {
                OwnerSession::Dedicated(binding) => binding.session_id(),
                OwnerSession::Attached(attached) => attached.session_id(),
            })
    }

    /// One bounded read of the protected sources, for a render before the observer runs.
    fn read_now(&self) -> Option<crate::lead_observation::Observation> {
        let board = self.board.as_deref()?;
        let observation = crate::lead_observation::Observation::read(
            board,
            self.situation.as_deref(),
            self.uid,
            oxa_proto::time::now_ms(),
        );
        Some(match self.parent_session() {
            Some(parent) => observation.with_workers(&self.agent_dir, &self.sessions_dir, parent),
            None => observation,
        })
    }

    fn render(&self, page: crate::lead_page::Page, fragment: bool) -> String {
        // Render from the one frame: no source is read or parsed on this path (#2343). Chat's
        // session tail, the durable asks and the CSRF state stay live reads below.
        let frame = self
            .observer
            .as_ref()
            .map(|observer| crate::lead_observer::Observer::frame(observer));
        // The frame's observation is never mutated, so the page borrows it instead of copying
        // a board of up to 128 rows; only a render without an observer reads one.
        let read = if frame.is_some() { None } else { self.read_now() };
        let observation = frame
            .as_ref()
            .and_then(|frame| frame.observation.as_ref())
            .or(read.as_ref());
        let observed = observation.as_ref().filter(|o| o.cause.is_none());
        let now = frame.as_ref().map_or_else(
            || {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
            },
            |frame| frame.now_ms / 1000,
        );
        let github_quotas = observed.and_then(|o| {
            crate::lead_quota::GitHubQuotas::from_app_sample(o.app_quota?, o.board.observed_at, now)
        });
        use crate::lead_page::Page;
        let approvals_page = matches!(page, Page::Home | Page::Board | Page::BoardQuery(_));
        let chat_page = matches!(page, Page::Chat(_));
        // Owner chat is configured only for a single verified Tailscale identity.
        let bound_owner = self
            .owner
            .as_ref()
            .filter(|owner| (approvals_page || chat_page) && recheck_start(&owner.start).is_ok());
        let owner = bound_owner.filter(|_| chat_page);
        let latest = owner.and_then(|owner| owner.latest.try_lock().ok());
        let waiting = if approvals_page && bound_owner.is_some() {
            self.desk.as_ref().and_then(|desk| desk.pending().ok())
        } else {
            None
        };
        // The authenticated live readback (#1391): open owner asks are read from the
        // authorized durable Session. The observer reads them for the frame, so an idle render
        // opens no session file; without an observer the render reads them itself.
        let durable_asks = match frame.as_ref() {
            Some(frame) => frame.asks.clone(),
            None => self.durable_asks(),
        };
        // The attached session's newest part is read without its writer lock (#2075).
        let current = owner.and_then(|owner| owner.current());
        let attached = match (&current, owner) {
            (Some(OwnerSession::Attached(attached)), Some(owner)) => {
                attached.tail(TAIL_BYTES).ok().map(|tail| (owner, attached, tail))
            }
            _ => None,
        };
        // The owner's running sessions to choose from (#1469), on the chat page only, and only
        // when the page lists them: an idle page costs no process scan, and the owner's own
        // chat message still answers with the rendered page.
        let attached_id = attached.as_ref().map(|(_, attached, _)| attached.session_id());
        // The owner's message acknowledges itself in the render that confirms it; that claim
        // is taken on every render, so no later render shows a send it did not confirm.
        let sent = owner.is_some_and(|owner| owner.sent.swap(false, Ordering::SeqCst));
        let rows: Option<Vec<crate::lead_page::SessionRow>> = owner.map(|owner| {
            // The chat page lists the owner's running sessions; nothing else lists them.
            if page.lists_sessions() {
                lead_attach::live_sessions(&owner.dirs, owner.uid)
            } else {
                Vec::new()
            }
            .into_iter()
            .map(|live| crate::lead_page::SessionRow {
                attached: attached_id == Some(live.id.as_str()),
                id: live.id,
                cwd_name: live.cwd_name,
                modified_ms: live.modified_ms,
            })
            .collect()
        });
        let back = owner
            .filter(|owner| !same_target(&owner.start, &current))
            .map(|owner| match owner.start {
                Some(OwnerSession::Dedicated(_)) => "Back to Lead chat",
                Some(OwnerSession::Attached(_)) => "Back to the default session",
                None => "Close the session view",
            });
        let view = crate::lead_page::View {
            sessions: owner
                .zip(rows.as_deref())
                .map(|(owner, rows)| crate::lead_page::SessionListView {
                    csrf: &owner.csrf,
                    rows,
                    back,
                }),
            attached: attached
                .as_ref()
                .map(|(owner, attached, tail)| crate::lead_page::AttachedView {
                    csrf: &owner.csrf,
                    entries: &tail.entries,
                    truncated: tail.truncated,
                    live: attached.live(),
                    short_id: attached.short_id(),
                    id: attached.session_id().to_owned(),
                    sent,
                    activity: lead_attach::activity(&tail.entries).map(|activity| match activity {
                        lead_attach::Activity::Working => "working",
                        lead_attach::Activity::Idle => "idle",
                    }),
                    context_tokens: lead_attach::context_tokens(&tail.entries),
                }),
            approvals: waiting.as_deref(),
            approval_csrf: bound_owner.map(|owner| owner.csrf.as_str()),
            owner_chat: owner
                .filter(|_| matches!(current, Some(OwnerSession::Dedicated(_))))
                .zip(latest.as_deref())
                .map(|(owner, latest)| crate::lead_page::OwnerChatView {
                    csrf: &owner.csrf,
                    reply: latest.as_ref().map(|latest| &latest.reply),
                    proposal: latest.as_ref().and_then(|latest| latest.proposal.as_ref()),
                    confirmed: latest.as_ref().is_some_and(|latest| latest.confirmed),
                }),
            board: observed.map(|o| &o.board),
            situation: observed.and_then(|o| o.situation.as_ref()),
            workers: observation.as_ref().and_then(|o| o.bindings.as_ref()),
            quota_deferred: observed.map_or(&[][..], |o| o.quota_deferred.as_slice()),
            live_events: self.live,
            running_sha256: self.running_sha256.as_deref(),
            github_quotas: github_quotas.as_ref(),
            durable_asks: durable_asks.as_deref(),
            // Stuck-work signals from the same protected record, with each
            // unobserved source explicit in the view (#1391).
            alarms: observed.map(|o| &o.alarms),
            fixed_prompt_csrf: self.form.as_ref().map(|form| form.csrf.as_str()),
            ..Default::default()
        };
        if fragment {
            crate::lead_page::content_for(page, &view, now)
        } else {
            crate::lead_page::document_for(page, &view, now)
        }
    }

    /// Open owner asks read live from the authorized durable Lead Session. The observer reads
    /// them for the frame; a render without an observer reads them itself.
    fn durable_asks(&self) -> Option<Vec<oxa_session::OpenGoalAsk>> {
        durable_asks(self.owner.as_deref())
    }
}

/// The session id the observer reads the Lead's workers for. The configured start-up session
/// is re-proved against the owner-only settings and binding on every tick, so a session the
/// owner changed, replaced or revoked shows no worker rows instead of stale ones. `None` with
/// no owner chat: the pages then have no worker source at all.
fn worker_owner(owner: Option<&OwnerChat>) -> Option<String> {
    let start = owner?.start.as_ref()?;
    start.recheck().ok()?;
    Some(match start {
        OwnerSession::Dedicated(binding) => binding.session_id().to_owned(),
        OwnerSession::Attached(attached) => attached.session_id().to_owned(),
    })
}

/// Open owner asks with their original revisions, read from the authorized durable Lead
/// Session. The binding revalidates the owner-only session file and the user settings on
/// every read; an unauthorized, busy or absent session stays explicitly unavailable rather
/// than quietly empty.
fn durable_asks(owner: Option<&OwnerChat>) -> Option<Vec<oxa_session::OpenGoalAsk>> {
    // The configured start-up session only: a session the owner picked on the page is not the
    // Lead's durable goal.
    let start = owner?.start.as_ref()?;
    start.recheck().ok()?;
    let entries = start.read_entries_for_goal().ok()?;
    let goal = oxa_session::Goal::latest(&entries).ok()?;
    Some(
        goal.map(|goal| goal.lead.map_or_else(Vec::new, |lead| lead.open_asks))
            .unwrap_or_default(),
    )
}

/// SHA-256 of the serving executable as 64 lowercase hex characters; `None` when it cannot
/// be read, in which case the page simply never learns of an update.
fn self_digest() -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let (mut hash, mut buffer) = (Sha256::new(), vec![0u8; 64 * 1024]);
    loop {
        match file.read(&mut buffer).ok()? {
            0 => break,
            n => hash.update(&buffer[..n]),
        }
    }
    Some(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// With no `oxa.cloud.daily_budget_usd` there is no budget check: a metered Lead model is admitted
/// and its cost recorded, not limited. The owner is told once at startup.
fn no_budget_warning(settings: &Settings) -> Option<&'static str> {
    let capped = settings
        .get("oxa.cloud.daily_budget_usd")
        .is_some_and(|cap| !cap.is_null());
    (!capped).then_some(
        "lead chat: no oxa.cloud.daily_budget_usd is set, so metered cloud use is recorded but not limited by USD",
    )
}

/// The file the owner's own tools read the client token from: private directory, private file,
/// replaced atomically on every start so an older token stops working.
const TOKEN_FILE: &str = "lead-client-token";

fn publish_client_token(agent_dir: &Path, token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(agent_dir)?;
    let temp = agent_dir.join(format!("{TOKEN_FILE}.{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, agent_dir.join(TOKEN_FILE)).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Called only by a registry-resolved handler; this module declares no command or flag.
pub fn run(dirs: &Dirs, cwd: &Path, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    if oxa_proto::offline() {
        return Err(invalid("lead serve is unavailable in offline mode"));
    }
    let config = Config::load(dirs, cwd).map_err(invalid)?;
    if !config.enabled() {
        return Err(invalid("lead serve is disabled in user settings"));
    }
    proxy::pre_bind(&config).map_err(invalid)?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|_| invalid("OS owner identity unavailable"))?
        .uid();
    let mut source = Source::new(env, uid)?;
    source.agent_dir = dirs.agent_dir.clone();
    source.sessions_dir = dirs.sessions_dir.clone();
    // The approval desk is read on every board render; a desk that cannot open shows as unavailable.
    source.desk = ApprovalDesk::open(&dirs.agent_dir).ok();
    // The one real service journal: fixed allowlisted lines on stderr, carried to
    // the system journal by the operator's existing service unit.
    let journal = ServiceJournal::stderr();
    if config.fixed_prompts() {
        source.form = Some(crate::lead_http::FixedForm {
            csrf: oxa_mcp::pkce_pair(&oxa_mcp::random_bytes()?).0,
            scheme: if config.mode() == Mode::Local {
                "http"
            } else {
                "https"
            },
        });
    }
    let mut dedicated_start = false;
    let owner = if config.owner_chat() {
        // `oxa.lead.attachSessionId` wins: the page then needs no dedicated session, takes no
        // session lock and opens no provider.
        // With neither key set the page starts on the list of running sessions alone.
        let start = match AttachedSession::load(dirs, uid)? {
            Some(attached) => Some(OwnerSession::Attached(attached)),
            None => {
                let project = cwd
                    .canonicalize()
                    .map_err(|_| invalid("Lead project unavailable"))?;
                LeadOwnerBinding::load(dirs, &project, uid)?.map(OwnerSession::Dedicated)
            }
        };
        dedicated_start = matches!(start, Some(OwnerSession::Dedicated(_)));
        let chat = Arc::new(OwnerChat::new(
            dirs.clone(),
            uid,
            start,
            oxa_mcp::pkce_pair(&oxa_mcp::random_bytes()?).0,
            journal.clone(),
        ));
        source.owner = Some(chat.clone());
        let env = env.clone();
        // Answers use the owner form's CSRF and origin checks, then the desk. They never wait on
        // the chat turn lock, so a running Lead turn does not hold up an approval.
        let approvals = source.desk.clone().map(|desk| OwnerApprovals {
            session: chat.start.clone(),
            dirs: dirs.clone(),
            cwd: cwd.to_path_buf(),
            desk,
            journal: journal.clone(),
        });
        if dedicated_start {
            stop_workers_on_interrupt();
        }
        Some(OwnerForm {
            csrf: chat.csrf.clone(),
            post: Arc::new(move |submission| match submission {
                OwnerSubmission::Approve { id, answer } => match &approvals {
                    Some(approvals) => approvals.post(&id, &answer),
                    None => Err("503 Service Unavailable"),
                },
                other => chat.post(other, &env),
            }),
        })
    } else {
        None
    };
    let form = source.form.clone();
    let client_token = oxa_mcp::pkce_pair(&oxa_mcp::random_bytes()?).0;
    let local = config.mode() == Mode::Local;
    let identity_journal = journal.clone();
    let refusal_log = journal.sink();
    let listener = TcpListener::bind(config.endpoint()).map_err(|_| invalid("cannot bind lead endpoint"))?;
    // Only the dedicated Lead chat spends model tokens; an attached page does not.
    if owner.is_some() && dedicated_start {
        if let Some(warning) = no_budget_warning(&Settings::load(dirs, cwd, false)) {
            writeln!(out, "{warning}").map_err(|_| invalid("cannot report lead startup"))?;
        }
    }
    // Published before the start line: the owner's tools act on that line.
    if local {
        publish_client_token(&dirs.agent_dir, &client_token)
            .map_err(|_| invalid("cannot write the lead client token"))?;
    }
    writeln!(out, "lead board: listening on configured loopback endpoint")
        .and_then(|()| out.flush())
        .map_err(|_| invalid("cannot report lead startup"))?;
    if local {
        // Stdout of the owner's own foreground command; the journal never carries the token.
        let (ip, port) = config.endpoint();
        writeln!(
            out,
            "lead board: open http://{}/?t={client_token}",
            SocketAddr::new(ip, port)
        )
        .and_then(|()| out.flush())
        .map_err(|_| invalid("cannot report lead startup"))?;
    }
    let served_token = client_token.clone();
    // The live route and the digest of the serving executable, computed once at start: a
    // changed digest after an update makes an open page reload itself.
    source.live = true;
    source.running_sha256 = self_digest();
    // One observer per server: it stat()s and parses the sources about once a second, and
    // every page and stream reads the frame it publishes (#2343).
    // The observer runs in this process and owns the same `Arc<OwnerChat>`: the start-up
    // session stays live, so a setting change or a revoked binding is seen on the next tick.
    let owner_state = source.owner.clone();
    let parent = move || worker_owner(owner_state.as_deref());
    // The owner's own durable asks (#1391): the authorized session's entries, revalidated on
    // every read. The observer keeps them in the frame, so an idle page reads no session file.
    let owner_state = source.owner.clone();
    let asks = move || {
        let asks = durable_asks(owner_state.as_deref())?;
        Some((
            asks.iter()
                .map(|ask| (ask.id.clone(), ask.revision))
                .collect::<Vec<_>>(),
            asks,
        ))
    };
    source.observer = Some(crate::lead_observer::Observer::start(
        crate::lead_observer::Sources {
            board: source.board.clone(),
            situation: source.situation.clone(),
            uid,
            agent_dir: source.agent_dir.clone(),
            sessions_dir: source.sessions_dir.clone(),
            parent: Box::new(parent),
            asks: Box::new(asks),
        },
    ));
    let source = Arc::new(source);
    let live = crate::lead_http::Live {
        generation: {
            let source = source.clone();
            Box::new(move || {
                source
                    .observer
                    .as_ref()
                    .map_or(0, |observer| observer.generation())
            })
        },
        wait: {
            let source = source.clone();
            Box::new(move |seen: u64, timeout: std::time::Duration| {
                source
                    .observer
                    .as_ref()
                    .map_or(seen, |observer| observer.wait(seen, timeout))
            })
        },
        running_sha256: source.running_sha256.clone(),
    };
    let served = crate::lead_http::serve_fixed(
        listener,
        move |stream, request| authorized(&config, uid, stream, request, &identity_journal, &served_token),
        move |page, fragment| source.render(page, fragment),
        form,
        owner,
        Some(refusal_log),
        Some(live),
    )
    .map_err(|_| invalid("lead listener failed"));
    if local {
        let _ = std::fs::remove_file(dirs.agent_dir.join(TOKEN_FILE));
    }
    // The workers the owner Lead started outlive its chat turns and stop with the Lead.
    crate::workers::tool::stop_owner_workers();
    served?;
    Ok(0)
}

/// Ctrl-C or SIGTERM of the lead process stops the whole tree: the workers the owner Lead
/// started, then the process. (`--resume` wakes them again from their records.)
fn stop_workers_on_interrupt() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let hit = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if signal_hook::flag::register(signal, hit.clone()).is_err() {
            return;
        }
    }
    let _ = std::thread::Builder::new()
        .name("oxa-lead-interrupt".into())
        .spawn(move || {
            while !hit.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            crate::workers::tool::stop_owner_workers();
            std::process::exit(130);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;

    /// An in-memory stand-in for the process's standard error, so the journal's
    /// exact lines can be asserted without any new file or dependency.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured_journal() -> (ServiceJournal, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (
            ServiceJournal {
                out: Arc::new(Mutex::new(Box::new(SharedSink(buffer.clone())))),
            },
            buffer,
        )
    }

    fn recorded(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(buffer.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn owner_chat_records_only_the_displayed_proposal_and_renders_text() {
        use oxa_fake::{FakeOpts, FakeServer, ScriptTurn};
        use oxa_session::SessionStore;

        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let project = test.work().canonicalize().unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        // The whole reply arrives in one JSON Markdown fence: the only harmless
        // formatting variation #1448 admits, with unchanged validation.
        let fenced = format!(
            "```json\n{}\n```",
            r#"{"text":"fixture <b>reply</b>","facts":[],"action":{"kind":"rule","rule":"note","scope":{"kind":"repository"},"value":{"kind":"text","value":"fixture note"}}}"#
        );
        let fake = FakeServer::start(vec![ScriptTurn::text(&fenced)], FakeOpts::default()).unwrap();
        let pairs = test.envs(&fake.url());
        let borrowed: Vec<_> = pairs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let env = Env::from_pairs(&borrowed);
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
        let settings = dirs.settings_file();
        std::fs::write(
            &settings,
            serde_json::json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (journal, buffer) = captured_journal();
        let owner = Arc::new(OwnerChat::new(
            dirs.clone(),
            uid,
            Some(OwnerSession::Dedicated(
                LeadOwnerBinding::load(&dirs, &project, uid).unwrap().unwrap(),
            )),
            "fixture".into(),
            journal,
        ));
        let mut source = Source::new(&env, uid).unwrap();
        source.owner = Some(owner.clone());
        {
            let _busy = owner.latest.lock().unwrap();
            assert_eq!(
                owner.post(OwnerSubmission::Message("blocked".into()), &env),
                Err("409 Conflict")
            );
        }
        owner
            .post(OwnerSubmission::Message("/goal is literal".into()), &env)
            .unwrap();
        let page = crate::lead_page::Page::Chat(crate::lead_page::Prompt::Status);
        let html = source.render(page, false);
        assert!(html.contains("fixture &lt;b&gt;reply&lt;/b&gt;"));
        assert!(!html.contains("fixture <b>reply</b>"));
        assert!(html.contains("Confirm proposal"));
        assert_eq!(
            owner.post(OwnerSubmission::Confirm("wrong".into()), &env),
            Err("409 Conflict")
        );
        let store = oxa_session::FsStore::new(dirs.sessions_dir.clone());
        let selected = store.resolve(&id).unwrap();
        let before = store.load(&selected).unwrap();
        let proposal_id = owner
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .proposal
            .as_ref()
            .unwrap()
            .0
            .clone();
        owner
            .post(OwnerSubmission::Confirm(proposal_id.clone()), &env)
            .unwrap();
        assert_eq!(
            owner.post(OwnerSubmission::Confirm(proposal_id.clone()), &env),
            Err("409 Conflict")
        );
        let after = store.load(&selected).unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert!(
            matches!(&after.last().unwrap().body, oxa_proto::EntryBody::Custom { custom_type, data } if custom_type == "oxa.lead.proposal_confirmed" && data["proposal_id"] == proposal_id)
        );
        assert!(source
            .render(page, true)
            .contains("Confirmation recorded. Application is pending."));
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| request.max_tokens > 0)
                .count(),
            1
        );
        // Every real block of the turns above left one allowlisted line, and
        // nothing else: no message text, no CSRF, no identity, no reply bytes.
        let lines = recorded(&buffer);
        assert_eq!(
            lines,
            [
                "lead chat refusal status=409 stage=busy reason=locked fix=wait for the in-flight chat turn to finish",
                "lead chat refusal status=409 stage=reply reason=proposal fix=confirm the currently displayed proposal id",
                "lead chat refusal status=409 stage=reply reason=proposal fix=confirm the currently displayed proposal id",
            ],
            "{lines:?}"
        );
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("blocked"));
        assert!(!text.contains("fixture"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn a_stale_attached_form_never_reaches_another_session() {
        use oxa_session::{FsStore, SessionStore};
        use std::os::unix::fs::MetadataExt;

        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(test.root()).unwrap().uid();
        let mut live = Vec::new();
        for _ in 0..2 {
            let store = FsStore::new(dirs.sessions_dir.clone());
            let session = store.create(&test.work(), None, None).unwrap();
            store.acquire(&session).unwrap();
            store.commit_lock();
            live.push((store, session.id));
        }
        let (a, b) = (live[0].1.clone(), live[1].1.clone());
        let inbox = |id: &str| {
            std::fs::read_dir(dirs.agent_dir.join("inbox").join(id)).map_or(0, |entries| entries.count())
        };
        let (journal, buffer) = captured_journal();
        let owner = OwnerChat::new(dirs.clone(), uid, None, "fixture".into(), journal);
        let env = Env::from_pairs(&[]);
        owner
            .post(OwnerSubmission::Attach(Some(a.clone())), &env)
            .unwrap();
        // Another tab or device switches the page's target to B after this page was rendered for A.
        owner
            .post(OwnerSubmission::Attach(Some(b.clone())), &env)
            .unwrap();
        let stale = OwnerSubmission::AttachedMessage {
            session: a.clone(),
            text: "meant for A".into(),
        };
        assert_eq!(owner.post(stale, &env), Err("409 Conflict"));
        assert_eq!((inbox(&a), inbox(&b)), (0, 0));
        // Back on the start-up target (none) nothing is attached, so no form can be delivered.
        owner.post(OwnerSubmission::Attach(None), &env).unwrap();
        let orphan = OwnerSubmission::AttachedMessage {
            session: b.clone(),
            text: "meant for B".into(),
        };
        assert_eq!(owner.post(orphan, &env), Err("503 Service Unavailable"));
        assert_eq!((inbox(&a), inbox(&b)), (0, 0));
        // A form for the session the page is on goes through.
        owner
            .post(OwnerSubmission::Attach(Some(b.clone())), &env)
            .unwrap();
        let current = OwnerSubmission::AttachedMessage {
            session: b.clone(),
            text: "meant for B".into(),
        };
        owner.post(current, &env).unwrap();
        assert_eq!((inbox(&a), inbox(&b)), (0, 1));
        let lines = recorded(&buffer);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("status=409 stage=reply reason=stale_target"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("status=503 stage=recheck reason=binding"),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains("meant for")));
    }

    #[test]
    fn lead_chat_no_budget_warns() {
        let none = Settings::from_values(serde_json::json!({"oxa":{"cloud":{}}}), serde_json::json!({}));
        let warning = no_budget_warning(&none).unwrap();
        assert!(warning.contains("not limited by USD"));
        let capped = Settings::from_values(
            serde_json::json!({"oxa":{"cloud":{"daily_budget_usd":5}}}),
            serde_json::json!({}),
        );
        assert!(no_budget_warning(&capped).is_none());
    }

    #[test]
    fn refusal_reasons_are_allowlisted_and_carry_no_error_bytes() {
        let budget = conversation_refusal(&OxaError::new(
            "budget_not_configured",
            "oxa.cloud.daily_budget_usd must be a positive number in oxa user settings, or unset for no USD limit.",
        ));
        assert_eq!(
            budget,
            RefusalFact {
                status: 503,
                stage: Stage::Limits,
                reason: "budget_invalid",
                fix: "set oxa.cloud.daily_budget_usd to a positive number, or unset it for no USD limit",
            }
        );
        for (code, stage, reason) in [
            (codes::BUDGET_EXHAUSTED, Stage::Limits, "metered_budget_exhausted"),
            ("cost_unknown_history", Stage::Limits, "cost_unknown_history"),
            (
                "subscription_limits_not_configured",
                Stage::Limits,
                "subscription_limits_absent",
            ),
            (
                "subscription_limit_exceeded",
                Stage::Limits,
                "subscription_limits_exhausted",
            ),
            ("lead_reply_invalid", Stage::Reply, "invalid"),
            ("lead_evidence_invalid", Stage::Reply, "evidence_invalid"),
            ("lead_reply_incomplete", Stage::Reply, "incomplete"),
            ("cloud_provider_forbidden", Stage::Provider, "provider_forbidden"),
            ("cloud_not_shared", Stage::Provider, "not_shared"),
            (
                codes::LITERAL_OUTPUT_EXHAUSTED,
                Stage::Reply,
                "literal_output_exhausted",
            ),
            (codes::USAGE, Stage::Body, "message"),
            ("lead_owner_unavailable", Stage::Recheck, "binding"),
            (codes::BUSY, Stage::Busy, "session"),
            (codes::NOT_FOUND, Stage::Provider, "model"),
            (codes::OFFLINE, Stage::Provider, "offline"),
            (codes::ENGINE_DOWN, Stage::Provider, "engine_down"),
            ("cloud_rate_limited", Stage::Provider, "rate_limited"),
            ("unclassified_engine_failure", Stage::Provider, "refused"),
        ] {
            let fact = conversation_refusal(&OxaError::new(code, "raw engine error bytes"));
            assert_eq!(fact.status, 503, "{code}");
            assert_eq!(fact.stage, stage, "{code}");
            assert_eq!(fact.reason, reason, "{code}");
            assert!(!fact.fix.is_empty(), "{code}");
        }
    }

    #[test]
    fn request_proof_composes_actual_kernel_owner_and_unchanged_authority() {
        let test = TestEnv::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let port = listener.local_addr().unwrap().port();
        let dirs = Dirs::under(test.root());
        std::fs::create_dir_all(&dirs.agent_dir).unwrap();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"port":port}}}})
            .to_string(),
        )
        .unwrap();
        let config = Config::load(&dirs, &test.work()).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let (journal, buffer) = captured_journal();
        let authority = format!("127.0.0.1:{port}");
        let mut request = crate::lead_http::Request {
            method: "GET",
            path: "/",
            authority: &authority,
            forwarded_login: None,
            client_token: Some("tok"),
            token_in_query: false,
            denied: Default::default(),
        };
        assert!(authorized(&config, uid, &stream, &request, &journal, "tok"));
        assert!(!authorized(
            &config,
            uid.wrapping_add(1),
            &stream,
            &request,
            &journal,
            "tok"
        ));
        request.authority = "attacker.invalid";
        assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
        request.authority = &authority;
        request.forwarded_login = Some("owner@example.invalid");
        assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
        request.forwarded_login = None;
        request.method = "POST";
        assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
        // None of those refusals was a chat request: identity facts name the chat
        // pipeline only.
        assert!(recorded(&buffer).is_empty());
        request.path = "/chat";
        request.method = "GET";
        request.authority = "attacker.invalid";
        assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=403 stage=identity reason=not_allowed fix=use the configured owner identity and exact authority"]
        );
        // The known identity reasons stay distinguishable in the journal.
        assert_eq!(request.path, "/chat");
        request.authority = &authority;
        assert!(!authorized(
            &config,
            uid.wrapping_add(1),
            &stream,
            &request,
            &journal,
            "tok"
        ));
        assert!(recorded(&buffer)[1].contains("reason=unverified"), "{buffer:?}");
        // A same-user peer without the minted token, or with another one, is refused as 401 and
        // the journal line names the fixed reason only.
        for given in [None, Some("other")] {
            request.client_token = given;
            request.denied.set(403);
            assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
            assert_eq!(request.denied.get(), 401);
        }
        let lines = recorded(&buffer);
        assert!(
            lines[2].contains("status=401 stage=identity reason=client_token"),
            "{lines:?}"
        );
        assert!(lines
            .iter()
            .all(|line| !line.contains("tok=") && !line.contains("other")));
        drop(client);
    }

    #[test]
    fn local_source_revalidates_private_sample_and_never_reflects_errors() {
        let test = TestEnv::new();
        let directory = test.root().join("board-input");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("board.jsonl");
        std::fs::write(
            &path,
            b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = std::fs::metadata(&directory).unwrap().uid();
        let path_text = path.to_str().unwrap();
        let env = Env::from_pairs(&[("OXA_LEAD_BOARD_LOG", path_text)]);
        let source = Source::new(&env, uid).unwrap();
        assert!(source
            .render(crate::lead_page::Page::Home, false)
            .contains("No open PRs in this snapshot"));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for (observed_at, expected) in [
            (now, "123 of 5000 remaining"),
            (now.saturating_sub(601), "Unknown"),
        ] {
            let sample = serde_json::json!({
                "schema_version": 1,
                "observed_at": observed_at,
                "board": {"observed_at": observed_at, "entries": []},
                "app_quota": {
                    "graphql": {"limit": 5000, "remaining": 123, "resets_at": now + 600},
                },
            });
            std::fs::write(&path, format!("{sample}\n")).unwrap();
            let usage = source.render(crate::lead_page::Page::Usage, true);
            assert!(usage.contains(&format!("GitHub App · GraphQL</dt><dd>{expected}")));
            assert!(usage.contains("GitHub Owner · GraphQL</dt><dd>Unknown"));
            assert!(usage.contains("Primary model window</dt><dd>Unknown"));
            assert!(usage.contains("Secondary model window</dt><dd>Unknown"));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = source.render(crate::lead_page::Page::Home, true);
        assert!(refused.contains("Board unavailable"));
        assert!(!refused.contains(path_text));
        assert!(Source::new(
            &Env::from_pairs(&[("OXA_LEAD_BOARD_LOG", "relative-private-marker")]),
            uid
        )
        .is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn the_page_advertises_live_events_only_when_the_route_is_installed() {
        let mut source = Source::new(&Env::from_pairs(&[]), 0).unwrap();
        let page = |source: &Source| source.render(crate::lead_page::Page::Home, false);
        assert!(!page(&source).contains("oxa-live-events"));
        source.live = true;
        source.running_sha256 = Some("a".repeat(64));
        let html = page(&source);
        assert!(html.contains("oxa-live-events"));
        assert!(html.contains(&"a".repeat(64)));
    }
}

#[cfg(test)]
mod approval_answer_tests {
    use super::*;
    use oxa_config::Settings;
    use oxa_fake::TestEnv;
    use oxa_perm::{ApprovalDesk, DangerHit};
    use std::os::unix::fs::PermissionsExt;

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured() -> (ServiceJournal, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (
            ServiceJournal {
                out: Arc::new(Mutex::new(Box::new(Sink(buffer.clone())))),
            },
            buffer,
        )
    }

    fn recorded(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(buffer.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn fixture() -> (TestEnv, Dirs, ApprovalDesk) {
        let test = TestEnv::new();
        let dirs = Dirs::under(test.root());
        let desk = ApprovalDesk::open(&dirs.agent_dir).unwrap();
        (test, dirs, desk)
    }

    fn hit() -> DangerHit {
        DangerHit {
            rule: "rm-recursive".into(),
            reason: "recursive delete outside the workspace".into(),
            segment: "rm -rf /srv/data".into(),
        }
    }

    fn file(test: &TestEnv, desk: &ApprovalDesk) -> String {
        desk.record("s1", &test.work(), "rm -rf /srv/data", &[hit()], "pending")
            .unwrap()
    }

    fn owner_answers(
        test: &TestEnv,
        dirs: &Dirs,
        desk: &ApprovalDesk,
        journal: ServiceJournal,
    ) -> OwnerApprovals {
        let project = test.work().canonicalize().unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner_id = LeadOwnerBinding::create(dirs, &project, uid).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"sessionId":owner_id}}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(dirs.settings_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
        OwnerApprovals {
            session: Some(OwnerSession::Dedicated(
                LeadOwnerBinding::load(dirs, &project, uid).unwrap().unwrap(),
            )),
            dirs: dirs.clone(),
            cwd: test.work(),
            desk: desk.clone(),
            journal,
        }
    }

    #[test]
    fn board_without_owner_binding_does_not_render_approval_details() {
        let (test, _dirs, desk) = fixture();
        file(&test, &desk);
        let mut source = Source::new(&Env::from_pairs(&[]), 0).unwrap();
        source.desk = Some(desk);
        for (page, fragment) in [
            (crate::lead_page::Page::Home, false),
            (crate::lead_page::Page::Home, true),
            (crate::lead_page::Page::Board, false),
        ] {
            let html = source.render(page, fragment);
            assert!(!html.contains("Waiting for your approval"));
            assert!(!html.contains("/srv/data"));
        }
    }

    #[test]
    fn approval_post_once_answers_pending_request() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        answers.post(&id, "once").unwrap();
        assert!(desk.pending().unwrap().is_empty());
        assert!(recorded(&buffer).is_empty());
    }

    #[test]
    fn approval_post_always_writes_user_settings_only() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let keys = desk.pending().unwrap()[0].keys.clone();
        assert!(!keys.is_empty());
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        answers.post(&id, "always").unwrap();
        // The keys land in the user settings file and nowhere else.
        let user = std::fs::read_to_string(dirs.settings_file()).unwrap();
        for key in &keys {
            assert!(user.contains(key.as_str()), "{user}");
        }
        assert_eq!(
            Settings::load(&dirs, &test.work(), false).list("oxa.approval.always"),
            keys
        );
        assert!(recorded(&buffer).is_empty());
    }

    #[test]
    fn approval_post_unknown_or_answered_id_is_refused() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        assert_eq!(answers.post("0123456789abcdef", "once"), Err("404 Not Found"));
        answers.post(&id, "deny").unwrap();
        // An answered request is no longer pending, so a second answer is refused the same way.
        assert_eq!(answers.post(&id, "once"), Err("404 Not Found"));
        assert_eq!(
            recorded(&buffer),
            [
                "lead chat refusal status=404 stage=approval reason=not_pending fix=answer a request still listed as waiting for approval",
                "lead chat refusal status=404 stage=approval reason=not_pending fix=answer a request still listed as waiting for approval",
            ]
        );
    }

    #[test]
    fn approval_post_rechecks_a_stale_owner_binding_before_answering() {
        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let (journal, buffer) = captured();
        let answers = owner_answers(&test, &dirs, &desk, journal);
        std::fs::remove_file(dirs.settings_file()).unwrap();

        assert_eq!(answers.post(&id, "once"), Err("503 Service Unavailable"));
        assert_eq!(desk.pending().unwrap().len(), 1);
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=503 stage=approval reason=binding fix=answer a request still listed as waiting for approval"]
        );
    }

    #[test]
    fn cli_and_web_answers_share_the_desk_lock() {
        use std::sync::{mpsc, Barrier};
        use std::time::Duration;

        let (test, dirs, desk) = fixture();
        let id = file(&test, &desk);
        let keys = desk.pending().unwrap()[0].keys.clone();
        let cwd = test.work();
        let (journal, _) = captured();
        let web = owner_answers(&test, &dirs, &desk, journal);
        // The desk's own advisory lock file: an answer from either entry point waits for it.
        let guard = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dirs.agent_dir.join("approvals").join(".lock"))
            .unwrap();
        guard.lock().unwrap();
        let start = Barrier::new(3);

        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let web_tx = tx.clone();
            let start_ref = &start;
            let request = &id;
            scope.spawn(move || {
                start_ref.wait();
                web_tx
                    .send(("session", web.post(request, "session").is_ok()))
                    .unwrap();
            });
            let cli_dirs = &dirs;
            let cli_cwd = &cwd;
            scope.spawn(move || {
                start_ref.wait();
                let result = crate::approve_cmd::approve_with(
                    cli_dirs,
                    cli_cwd,
                    "s1",
                    request,
                    "deny",
                    &mut Vec::new(),
                    &mut |_| Some(true),
                );
                tx.send(("deny", result.is_ok())).unwrap();
            });
            start.wait();
            let early = rx.recv_timeout(Duration::from_millis(400));
            drop(guard);
            assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
            let results = [rx.recv().unwrap(), rx.recv().unwrap()];
            assert_eq!(results.iter().filter(|(_, ok)| *ok).count(), 1);
            assert_eq!(
                desk.granted("s1", &cwd, &keys),
                results.contains(&("session", true)),
                "the stored decision is the winning answer"
            );
        });
    }

    #[test]
    fn approval_post_refusal_is_journaled() {
        // Local mode has no owner chat, so the approval gate refuses the POST; the refusal is
        // journaled as an identity fact on the approval path, like a refused chat submission.
        let test = TestEnv::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let port = listener.local_addr().unwrap().port();
        let dirs = Dirs::under(test.root());
        std::fs::create_dir_all(&dirs.agent_dir).unwrap();
        std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            dirs.settings_file(),
            serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
        )
        .unwrap();
        let config = Config::load(&dirs, &test.work()).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let (journal, buffer) = captured();
        let authority = format!("127.0.0.1:{port}");
        let request = crate::lead_http::Request {
            method: "POST",
            path: "/approvals",
            authority: &authority,
            forwarded_login: None,
            client_token: Some("tok"),
            token_in_query: false,
            denied: Default::default(),
        };
        assert!(!authorized(&config, uid, &stream, &request, &journal, "tok"));
        assert_eq!(
            recorded(&buffer),
            ["lead chat refusal status=403 stage=identity reason=not_allowed fix=use the configured owner identity and exact authority"]
        );
        drop(client);
    }
}
