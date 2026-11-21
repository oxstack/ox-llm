//! Registry-neutral HTTP adapter. No listener, identity policy or command is declared here.
//! The owner-approved caller binds the listener and supplies verified request authorization.
use crate::lead_page::Page;
use oxa_http::server::{
    content_length, parse_headers, parse_request_line, read_head, split_head, HeadError, Headers,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One allowlisted chat-refusal fact: the response status plus fixed stage,
/// reason and remedy tokens. Owner identity, message text, token counts, URLs,
/// headers, keys and CSRF values never appear in a fact; call sites pass only
/// these constants, and no fact carries request or error bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefusalFact {
    pub status: u16,
    pub stage: Stage,
    pub reason: &'static str,
    pub fix: &'static str,
}

/// The actionable pipeline stage of a refused chat request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Identity,
    Origin,
    Csrf,
    Body,
    Limits,
    Busy,
    Recheck,
    Provider,
    Reply,
    Approval,
}
impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Origin => "origin",
            Self::Csrf => "csrf",
            Self::Body => "body",
            Self::Limits => "limits",
            Self::Busy => "busy",
            Self::Recheck => "recheck",
            Self::Provider => "provider",
            Self::Reply => "reply",
            Self::Approval => "approval",
        }
    }
}

/// The owner-approved caller's sink for refused chat requests.
pub type RefusalLog = std::sync::Arc<dyn Fn(RefusalFact) + Send + Sync>;

fn refuse(log: Option<&RefusalLog>, fact: RefusalFact) {
    if let Some(log) = log {
        log(fact);
    }
}

/// Every `/chat` path form; refusals on these paths are chat refusals.
pub(crate) fn chat_path(path: &str) -> bool {
    path == "/chat" || path.starts_with("/chat?")
}

/// Every `/approvals` path form; refusals on these paths are logged like chat refusals.
pub(crate) fn approvals_path(path: &str) -> bool {
    path == "/approvals" || path.starts_with("/approvals?")
}

/// Why an authenticated chat submission was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejected {
    /// The submission's CSRF token did not match the served form.
    Csrf,
    /// The submission's shape or content was malformed.
    Body,
}
impl Rejected {
    fn fact(self) -> RefusalFact {
        match self {
            Self::Csrf => RefusalFact {
                status: 403,
                stage: Stage::Csrf,
                reason: "invalid",
                fix: "reload the served chat form and resubmit",
            },
            Self::Body => RefusalFact {
                status: 403,
                stage: Stage::Body,
                reason: "late_parse",
                fix: "send one well-formed urlencoded chat submission",
            },
        }
    }
}

/// Serve a caller-bound loopback listener with at most sixteen live requests.
/// Listener configuration, feature selection and verified identity remain with the
/// owner-reviewed caller. This transport has no command or configuration declaration.
pub fn serve(
    listener: TcpListener,
    authorize: impl Fn(&TcpStream, &Request<'_>) -> bool + Send + Sync + 'static,
    render: impl Fn(Page, bool) -> String + Send + Sync + 'static,
) -> std::io::Result<()> {
    serve_fixed(listener, authorize, render, None, None, None, None)
}

/// The authenticated live route `/events/live` (#1366): the observer's generation, which
/// changes only when a source published a new frame, and the digest of the serving
/// executable so a replaced binary makes the page reload. Without it the route does not
/// exist and the page keeps its polling fallback. A stream waits on the observer's condvar
/// and never reads or parses a source itself, so any number of open tabs cost the same.
pub(crate) struct Live {
    /// The generation now published, sampled before the first event goes out.
    pub generation: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Block until the generation differs from the argument, or the timeout passes; returns
    /// the generation then published. The timeout is this stream's keep-alive budget.
    pub wait: Box<dyn Fn(u64, Duration) -> u64 + Send + Sync>,
    pub running_sha256: Option<String>,
}

/// A live stream ends after this long; the client's normal rotation reconnects.
const LIVE_LIFETIME: Duration = Duration::from_secs(300);
const LIVE_KEEP_ALIVE: Duration = Duration::from_secs(15);
const LIVE_WRITE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct FixedForm {
    pub csrf: String,
    pub scheme: &'static str,
}

pub(crate) enum OwnerSubmission {
    Message(String),
    /// A message from the attached-session form, with the full id of the session the page was
    /// rendered for. The service refuses it when the page's target has since changed.
    AttachedMessage {
        session: String,
        text: String,
    },
    Confirm(String),
    /// The owner's choice of a running session to attach the page to (`Some(id)`), or back to
    /// the start-up target (`None`). The id is only a lookup key into the live session list.
    Attach(Option<String>),
    /// The owner's answer to one approval request. It never enters the chat turn.
    Approve {
        id: String,
        answer: String,
    },
}

pub(crate) type OwnerPost = dyn Fn(OwnerSubmission) -> Result<(), &'static str> + Send + Sync;

#[derive(Clone)]
pub(crate) struct OwnerForm {
    pub csrf: String,
    pub post: Arc<OwnerPost>,
}

pub(crate) fn serve_fixed(
    listener: TcpListener,
    authorize: impl Fn(&TcpStream, &Request<'_>) -> bool + Send + Sync + 'static,
    render: impl Fn(Page, bool) -> String + Send + Sync + 'static,
    form: Option<FixedForm>,
    owner: Option<OwnerForm>,
    refusals: Option<RefusalLog>,
    live: Option<Live>,
) -> std::io::Result<()> {
    let live = live.map(Arc::new);
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let (authorize, render) = (Arc::new(authorize), Arc::new(render));
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream?;
        if active.fetch_add(1, Ordering::SeqCst) >= 16 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (authorize, render, slot) = (authorize.clone(), render.clone(), Slot(active.clone()));
        let form = form.clone();
        let owner = owner.clone();
        let refusals = refusals.clone();
        let live = live.clone();
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = connection_fixed(
                stream,
                |s, r| authorize(s, r),
                |page, fragment| render(page, fragment),
                form.as_ref(),
                owner.as_ref(),
                refusals.as_ref(),
                live.as_deref(),
            );
        });
    }
    Ok(())
}

/// Parsed routing/identity input, deliberately without Debug or raw header serialization.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The single raw Host authority. This is input to policy, never expected/proven authority.
    pub authority: &'a str,
    pub forwarded_login: Option<&'a str>,
    /// The per-client token the request carries (`?t=`, bearer or cookie), when well-formed.
    /// Input to policy only; the transport never judges it.
    pub client_token: Option<&'a str>,
    /// The token came in the `?t=` query: a valid one is exchanged for a cookie by redirect.
    pub token_in_query: bool,
    /// Set by the authorization closure to 401 when the refusal is the client token.
    pub denied: std::cell::Cell<u16>,
}

/// Cookie that carries the lead client token after the first `?t=` visit.
pub(crate) const TOKEN_COOKIE: &str = "oxa_lead";

/// A client token is the base64url text the server minted: nothing else is carried or echoed.
fn token_text(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(value)
}

/// Handle one accepted connection, authorizing before any fact read or page rendering.
/// The callback receives the actual stream so the identity provider can bind OS/proxy
/// provenance to this request. Neither an address nor a header is a proof by itself.
pub fn connection(
    stream: TcpStream,
    authorize: impl FnOnce(&TcpStream, &Request<'_>) -> bool,
    render: impl FnOnce(Page, bool) -> String,
) -> std::io::Result<()> {
    connection_fixed(stream, authorize, render, None, None, None, None)
}

fn connection_fixed(
    mut stream: TcpStream,
    authorize: impl FnOnce(&TcpStream, &Request<'_>) -> bool,
    render: impl FnOnce(Page, bool) -> String,
    form: Option<&FixedForm>,
    owner: Option<&OwnerForm>,
    refusals: Option<&RefusalLog>,
    live: Option<&Live>,
) -> std::io::Result<()> {
    let start = Instant::now();
    let bytes: Vec<u8>;
    let parsed = match read_head(
        &mut Deadline {
            stream: &stream,
            start,
        },
        8192,
    ) {
        Ok(head) => {
            bytes = head;
            request(&bytes, form.is_some(), owner.is_some())
        }
        Err(HeadError::Closed) => return Ok(()),
        Err(HeadError::Io(kind)) => return Err(kind.into()),
        Err(HeadError::TooLarge) => Err("431 Request Header Fields Too Large"),
        Err(_) => Err("400 Bad Request"),
    };
    let mut set_cookie = String::new();
    let allowed = parsed
        .inspect_err(|status| {
            // Framing failures precede identity and any fact read: one fixed
            // early-parse line, with no request bytes.
            refuse(
                refusals,
                RefusalFact {
                    status: status[..3].parse().unwrap_or(400),
                    stage: Stage::Body,
                    reason: "early_parse",
                    fix: "send well-formed HTTP request framing",
                },
            );
        })
        .and_then(|(r, fields)| {
            // Answers to approval requests need the owner form; the fixed prompts never reach them.
            let post = (r.method == "POST" && r.path == "/chat" && (form.is_some() || owner.is_some()))
                || (r.method == "POST" && r.path == "/approvals" && owner.is_some());
            let approval = post && r.path == "/approvals";
            if r.method != "GET" && !post {
                if chat_path(r.path) || approvals_path(r.path) {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 405,
                            stage: Stage::Body,
                            reason: "method",
                            fix: "chat serves GET pages and POST submissions only",
                        },
                    );
                }
                return Err("405 Method Not Allowed");
            }
            // An identity refusal is reported by the caller's own authorization closure.
            if !authorize(&stream, &r) {
                return Err(if r.denied.get() == 401 {
                    "401 Unauthorized"
                } else {
                    "403 Forbidden"
                });
            }
            // The owner opened the printed URL: keep the token in a cookie, off the address bar.
            if let (true, Some(token)) = (r.method == "GET" && r.token_in_query, r.client_token) {
                set_cookie =
                    format!("Set-Cookie: {TOKEN_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/\r\n");
                return Ok((r.path, Some(r.path.to_string())));
            }
            let page = if post {
                let scheme = form.map_or("https", |form| form.scheme);
                if fields.origin != Some(format!("{scheme}://{}", r.authority).as_str())
                    || fields.kind != Some("application/x-www-form-urlencoded")
                    || ((form.is_none() || approval) && fields.fetch_site != Some("same-origin"))
                {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 403,
                            stage: Stage::Origin,
                            reason: "cross_origin",
                            fix: "send the chat form from its served origin",
                        },
                    );
                    return Err("403 Forbidden");
                }
                let mut body = vec![0; fields.length];
                let read = (|| {
                    for byte in &mut body {
                        let left = Duration::from_secs(5)
                            .checked_sub(start.elapsed())
                            .filter(|d| !d.is_zero())
                            .ok_or("400 Bad Request")?;
                        stream
                            .set_read_timeout(Some(left))
                            .map_err(|_| "400 Bad Request")?;
                        stream
                            .read_exact(std::slice::from_mut(byte))
                            .map_err(|_| "400 Bad Request")?;
                    }
                    Ok(())
                })();
                if let Err(status) = read {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 400,
                            stage: Stage::Body,
                            reason: "late_parse",
                            fix: "send one well-formed urlencoded chat submission",
                        },
                    );
                    return Err(status);
                }
                // A fixed form that does not authenticate is re-examined by the owner form
                // below when one is installed; only the final refusal is recorded.
                let mut form_rejection: Option<Rejected> = None;
                let fixed = match form {
                    Some(form) if !approval && body.len() <= 256 => match fixed_target(&body, &form.csrf) {
                        Ok(target) => Some(target),
                        Err(rejected) => {
                            form_rejection = Some(rejected);
                            None
                        }
                    },
                    _ => None,
                };
                if let Some(target) = fixed {
                    Some(target)
                } else if approval {
                    let owner = owner.ok_or("405 Method Not Allowed")?;
                    if scheme != "https" || fields.fetch_site != Some("same-origin") {
                        refuse(
                            refusals,
                            RefusalFact {
                                status: 403,
                                stage: Stage::Origin,
                                reason: "cross_origin",
                                fix: "send the approval answer from its served origin",
                            },
                        );
                        return Err("403 Forbidden");
                    }
                    let submission = approval_submission(&body, &owner.csrf).map_err(|rejected| {
                        refuse(refusals, rejected.fact());
                        "403 Forbidden"
                    })?;
                    (owner.post)(submission)?;
                    Some("/".into())
                } else {
                    let owner = owner.ok_or_else(|| {
                        refuse(
                            refusals,
                            form_rejection.map(Rejected::fact).unwrap_or(RefusalFact {
                                status: 403,
                                stage: Stage::Csrf,
                                reason: "invalid",
                                fix: "reload the served chat form and resubmit",
                            }),
                        );
                        "403 Forbidden"
                    })?;
                    if scheme != "https" || fields.fetch_site != Some("same-origin") {
                        refuse(
                            refusals,
                            RefusalFact {
                                status: 403,
                                stage: Stage::Origin,
                                reason: "cross_origin",
                                fix: "send the chat form from its served origin",
                            },
                        );
                        return Err("403 Forbidden");
                    }
                    let submission = owner_submission(&body, &owner.csrf).map_err(|rejected| {
                        refuse(refusals, rejected.fact());
                        "403 Forbidden"
                    })?;
                    (owner.post)(submission)?;
                    Some("/chat".into())
                }
            } else {
                None
            };
            Ok((r.path, page))
        });
    // The live stream starts only after the same authorization as every other path.
    if let (Ok((path, None)), Some(live)) = (&allowed, live) {
        if *path == "/events/live" {
            return stream_live(&mut stream, live, form.is_some() || owner.is_some());
        }
    }
    let location = match &allowed {
        Ok((_, Some(target))) => format!("Location: {target}\r\n{set_cookie}"),
        _ => String::new(),
    };
    let (status, kind, body) = match allowed {
        Ok((_, Some(_))) => ("303 See Other", "text/plain", String::new()),
        Ok(("/events", None)) => (
            "200 OK",
            "text/event-stream",
            format!(
                "retry: 5000\nevent: snapshot\ndata: {}\n\n",
                render(Page::Home, true)
            ),
        ),
        Ok(("/style.css", None)) => ("200 OK", "text/css", crate::lead_page::STYLE.into()),
        Ok(("/live.js", None)) => ("200 OK", "text/javascript", crate::lead_page::SCRIPT.into()),
        Ok((path, None)) => match Page::request(path) {
            Some((page, fragment)) => ("200 OK", "text/html", render(page, fragment)),
            None => ("404 Not Found", "text/plain", "Not found".into()),
        },
        Err(status) => (status, "text/plain", "Request refused".into()),
    };
    let has_form = form.is_some() || owner.is_some();
    let form_action = if has_form { "'self'" } else { "'none'" };
    let referrer = if has_form { "same-origin" } else { "no-referrer" };
    let headers = format!("HTTP/1.1 {status}\r\n{location}Content-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}\r\nReferrer-Policy: {referrer}\r\n\r\n", body.len());
    write_response(
        &mut stream,
        &[headers.as_bytes(), body.as_bytes()],
        Duration::from_secs(5),
    )
}

/// `text/event-stream` with no length: `retry` and one `lead-update` at once, another
/// whenever the observer publishes a new frame, a keep-alive comment while quiet, and an end
/// after [`LIVE_LIFETIME`]. Each write has its own deadline, so a stalled reader frees its
/// slot; between writes a stream only sleeps on the observer's condvar.
fn stream_live(stream: &mut TcpStream, live: &Live, has_form: bool) -> std::io::Result<()> {
    let form_action = if has_form { "'self'" } else { "'none'" };
    let referrer = if has_form { "same-origin" } else { "no-referrer" };
    let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}\r\nReferrer-Policy: {referrer}\r\n\r\n");
    let update = format!(
        "event: lead-update\ndata: {{\"running_sha256\":{}}}\n\n",
        live.running_sha256
            .as_deref()
            .map_or_else(|| "null".to_owned(), |digest| format!("\"{digest}\""))
    );
    // Sample the baseline before the first event goes out: a change that lands
    // while that event is in flight must still produce an update, not disappear
    // into the baseline.
    let mut last = (live.generation)();
    write_response(
        stream,
        &[headers.as_bytes(), b"retry: 5000\n", update.as_bytes()],
        LIVE_WRITE,
    )?;
    let began = Instant::now();
    while began.elapsed() < LIVE_LIFETIME {
        let left = LIVE_LIFETIME.saturating_sub(began.elapsed());
        let now = (live.wait)(last, LIVE_KEEP_ALIVE.min(left));
        if now != last {
            last = now;
            write_response(stream, &[update.as_bytes()], LIVE_WRITE)?;
        } else {
            write_response(stream, &[b": keep-alive\n\n"], LIVE_WRITE)?;
        }
    }
    Ok(())
}

fn write_response(stream: &mut TcpStream, parts: &[&[u8]], budget: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    for part in parts {
        let mut remaining = *part;
        while !remaining.is_empty() {
            let left = budget
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(std::io::ErrorKind::TimedOut)?;
            stream.set_write_timeout(Some(left))?;
            match stream.write(remaining) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => remaining = &remaining[n..],
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Fields<'a> {
    origin: Option<&'a str>,
    fetch_site: Option<&'a str>,
    kind: Option<&'a str>,
    bearer: Option<&'a str>,
    cookie: Option<&'a str>,
    length: usize,
}

fn fixed_target(bytes: &[u8], expected: &str) -> Result<String, Rejected> {
    let (mut csrf, mut prompt) = (None, None);
    for field in std::str::from_utf8(bytes).map_err(|_| Rejected::Body)?.split('&') {
        let (key, value) = field.split_once('=').ok_or(Rejected::Body)?;
        match key {
            "csrf" if csrf.replace(value).is_none() => {}
            "prompt" if prompt.replace(value).is_none() => {}
            _ => return Err(Rejected::Body),
        }
    }
    if csrf != Some(expected) {
        return Err(Rejected::Csrf);
    }
    let target = format!("/chat?prompt={}", prompt.ok_or(Rejected::Body)?);
    let (Page::Chat(_), false) = Page::request(&target).ok_or(Rejected::Body)? else {
        return Err(Rejected::Body);
    };
    Ok(target)
}

fn owner_submission(bytes: &[u8], expected: &str) -> Result<OwnerSubmission, Rejected> {
    let (mut csrf, mut input, mut session) = (None, None, None);
    for field in std::str::from_utf8(bytes).map_err(|_| Rejected::Body)?.split('&') {
        let (key, value) = field.split_once('=').ok_or(Rejected::Body)?;
        match key {
            "csrf" if csrf.replace(value).is_none() => {}
            "session" if session.replace(value).is_none() => {}
            "message" | "proposal_id" | "attach" if input.replace((key, value)).is_none() => {}
            _ => return Err(Rejected::Body),
        }
    }
    if csrf != Some(expected) {
        return Err(Rejected::Csrf);
    }
    let input = input.ok_or(Rejected::Body)?;
    if session.is_some() && input.0 != "message" {
        return Err(Rejected::Body);
    }
    match input {
        ("message", value) => {
            let text = decode_message(value).ok_or(Rejected::Body)?;
            match session {
                None => Ok(OwnerSubmission::Message(text)),
                Some(id) if id.len() <= 128 && crate::args::valid_session_id(id) => {
                    Ok(OwnerSubmission::AttachedMessage {
                        session: id.to_owned(),
                        text,
                    })
                }
                Some(_) => Err(Rejected::Body),
            }
        }
        ("proposal_id", value)
            if value.len() == 43
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) =>
        {
            Ok(OwnerSubmission::Confirm(value.to_owned()))
        }
        ("attach", "") => Ok(OwnerSubmission::Attach(None)),
        ("attach", value) if value.len() <= 128 && crate::args::valid_session_id(value) => {
            Ok(OwnerSubmission::Attach(Some(value.to_owned())))
        }
        _ => Err(Rejected::Body),
    }
}

/// The owner's approval answer: exactly `csrf`, `id` and `answer`, with the answer allow-listed.
/// The id is only a lookup key; the caller refuses any id that is not pending.
fn approval_submission(bytes: &[u8], expected: &str) -> Result<OwnerSubmission, Rejected> {
    let (mut csrf, mut id, mut answer) = (None, None, None);
    for field in std::str::from_utf8(bytes).map_err(|_| Rejected::Body)?.split('&') {
        let (key, value) = field.split_once('=').ok_or(Rejected::Body)?;
        match key {
            "csrf" if csrf.replace(value).is_none() => {}
            "id" if id.replace(value).is_none() => {}
            "answer" if answer.replace(value).is_none() => {}
            _ => return Err(Rejected::Body),
        }
    }
    if csrf != Some(expected) {
        return Err(Rejected::Csrf);
    }
    let (Some(id), Some(answer)) = (id, answer) else {
        return Err(Rejected::Body);
    };
    let well_formed = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    if !well_formed || !matches!(answer, "once" | "session" | "always" | "deny") {
        return Err(Rejected::Body);
    }
    Ok(OwnerSubmission::Approve {
        id: id.to_owned(),
        answer: answer.to_owned(),
    })
}

fn decode_message(value: &str) -> Option<String> {
    let mut encoded = value.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = encoded.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let digit = |byte| char::from(byte).to_digit(16).map(|digit| digit as u8);
                digit(encoded.next()?)? * 16 + digit(encoded.next()?)?
            }
            byte => byte,
        });
    }
    let message = String::from_utf8(decoded).ok()?;
    (!message.trim().is_empty()
        && !message
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\r' | '\n')))
    .then_some(message)
}

/// Reads one byte at a time with the remaining share of a 5 s total budget armed before each read.
struct Deadline<'a> {
    stream: &'a TcpStream,
    start: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = Duration::from_secs(5)
            .checked_sub(self.start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(std::io::ErrorKind::TimedOut)?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

type Scanned<'a> = (Option<&'a str>, Option<&'a str>, Fields<'a>, bool);

/// Per-field rules in header order, so a doubly-wrong request gets the status of its first fault.
fn scan<'a>(headers: &Headers<'a>) -> Result<Scanned<'a>, &'static str> {
    let mut login = None;
    let mut authority = None;
    let mut fields = Fields::default();
    let mut length = false;
    for (name, value) in headers.iter() {
        if name.eq_ignore_ascii_case("tailscale-user-login") && login.replace(value).is_some() {
            return Err("403 Forbidden");
        }
        if name.eq_ignore_ascii_case("host") && authority.replace(value).is_some() {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("content-length") && std::mem::replace(&mut length, true)
            || name.eq_ignore_ascii_case("origin") && fields.origin.replace(value).is_some()
            || name.eq_ignore_ascii_case("content-type") && fields.kind.replace(value).is_some()
            || name.eq_ignore_ascii_case("sec-fetch-site") && fields.fetch_site.replace(value).is_some()
            || name.eq_ignore_ascii_case("authorization") && fields.bearer.replace(value).is_some()
            || name.eq_ignore_ascii_case("cookie") && fields.cookie.replace(value).is_some()
        {
            return Err("400 Bad Request");
        }
    }
    Ok((login, authority, fields, length))
}

fn request(bytes: &[u8], fixed: bool, owner: bool) -> Result<(Request<'_>, Fields<'_>), &'static str> {
    const BAD: &str = "400 Bad Request";
    let (line, block) = split_head(bytes).map_err(|e| match e {
        HeadError::Closed => "431 Request Header Fields Too Large",
        _ => BAD,
    })?;
    let first = parse_request_line(line).map_err(|_| BAD)?;
    if first.version != "HTTP/1.1" {
        return Err(BAD);
    }
    let Ok(headers) = parse_headers(block) else {
        // A malformed line only wins if no earlier line was already refused (a second
        // tailscale-user-login before it is 403).
        let lines: Vec<_> = block.split("\r\n").collect();
        let good = lines.iter().take_while(|l| parse_headers(l).is_ok()).count();
        return Err(parse_headers(&lines[..good].join("\r\n"))
            .ok()
            .and_then(|h| scan(&h).err())
            .unwrap_or(BAD));
    };
    let (login, authority, mut fields, _) = scan(&headers)?;
    let length = content_length(&headers).map_err(|_| BAD)?.unwrap_or(0);
    fields.length = length;
    if length > if owner { 4096 } else { 256 }
        || length != 0
            && !((fixed || owner)
                && first.method == "POST"
                && (first.target == "/chat" || first.target == "/approvals"))
    {
        return Err(BAD);
    }
    // A `?t=<token>` query is the one query this layer takes off the path; the page router
    // never sees it.
    let (path, query) = match first.target.split_once("?t=") {
        Some((path, token)) if first.method == "GET" => (path, token_text(token)),
        _ => (first.target, None),
    };
    let bearer = fields
        .bearer
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(token_text);
    let cookie = fields.cookie.and_then(|value| {
        value
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .find_map(|(name, value)| (name == TOKEN_COOKIE).then_some(value))
            .and_then(token_text)
    });
    Ok((
        Request {
            method: first.method,
            path,
            authority: authority.filter(|s| !s.is_empty()).ok_or(BAD)?,
            forwarded_login: login,
            client_token: query.or(bearer).or(cookie),
            token_in_query: query.is_some(),
            denied: std::cell::Cell::new(403),
        },
        fields,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[test]
    fn owner_chat_refuses_bad_identity_origin_fetch_site_and_framing_before_dispatch() {
        let body = "csrf=fixture&message=hello+Lead";
        let origin_fact = || RefusalFact {
            status: 403,
            stage: Stage::Origin,
            reason: "cross_origin",
            fix: "send the chat form from its served origin",
        };
        let framing_fact = || RefusalFact {
            status: 400,
            stage: Stage::Body,
            reason: "early_parse",
            fix: "send well-formed HTTP request framing",
        };
        for (allowed, origin, site, framing, expected, logged) in [
            (false, "https://example.invalid", "same-origin", "", "403", None),
            (
                true,
                "https://other.invalid",
                "same-origin",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-site",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-origin",
                "Transfer-Encoding: chunked\r\n",
                "400",
                Some(framing_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-origin",
                "Content-Length: 4097\r\n",
                "400",
                Some(framing_fact()),
            ),
            (true, "https://example.invalid", "same-origin", "", "303", None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let invoked = count.clone();
            let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = facts.clone();
            let handler = std::thread::spawn(move || {
                let owner = OwnerForm {
                    csrf: "fixture".into(),
                    post: Arc::new(move |submission| {
                        assert!(
                            matches!(submission, OwnerSubmission::Message(message) if message == "hello Lead")
                        );
                        invoked.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }),
                };
                let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
                connection_fixed(
                    server,
                    |_, _| allowed,
                    |_, _| panic!("unexpected render"),
                    None,
                    Some(&owner),
                    Some(&refusals),
                    None,
                )
                .unwrap();
            });
            let length = if framing.starts_with("Content-Length:") {
                String::new()
            } else {
                format!("Content-Length: {}\r\n", body.len())
            };
            // Refused headers never reach the body reader. Do not queue an unread payload
            // that would turn the fixture's TCP close into a reset.
            let payload = if expected == "303" { body } else { "" };
            client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            write!(client, "POST /chat HTTP/1.1\r\nHost: example.invalid\r\nOrigin: {origin}\r\nSec-Fetch-Site: {site}\r\nContent-Type: application/x-www-form-urlencoded\r\n{framing}{length}\r\n{payload}").unwrap();
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            handler.join().unwrap();
            assert!(
                reply.starts_with(&format!("HTTP/1.1 {expected}")),
                "authorized={allowed}, origin={origin}, fetch_site={site:?}, framing={framing:?}: {reply}"
            );
            assert_eq!(count.load(Ordering::SeqCst), usize::from(expected == "303"));
            assert_eq!(reply.contains("Location: /chat\r\n"), expected == "303");
            // The transport reports only its own chat refusals: identity belongs to the
            // caller's authorization; framing failures carry one fixed early-parse line.
            let facts = facts.lock().unwrap();
            assert_eq!(
                facts.as_slice(),
                logged.as_slice(),
                "authorized={allowed}, origin={origin}, fetch_site={site:?}"
            );
        }
    }

    #[test]
    fn malformed_heads_keep_their_status() {
        let status = |head: &[u8]| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let handler = std::thread::spawn(move || {
                connection(server, |_, _| true, |_, _| String::new()).unwrap();
            });
            client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            client.write_all(head).unwrap();
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            handler.join().unwrap();
            reply.lines().next().unwrap().to_owned()
        };
        let long = "a".repeat(8192);
        let cases: [(&str, &[u8], &str); 31] = [
            ("valid", b"GET / HTTP/1.1\r\nHost: h\r\n\r\n", "200 OK"),
            ("version 1.0", b"GET / HTTP/1.0\r\nHost: h\r\n\r\n", "400 Bad Request"),
            ("version 2", b"GET / HTTP/2\r\nHost: h\r\n\r\n", "400 Bad Request"),
            ("two-part line", b"GET /\r\nHost: h\r\n\r\n", "400 Bad Request"),
            ("duplicate login", b"GET / HTTP/1.1\r\nHost: h\r\nTailscale-User-Login: a\r\ntailscale-user-login: b\r\n\r\n", "403 Forbidden"),
            ("duplicate login + bad version", b"GET / HTTP/1.0\r\nHost: h\r\nTailscale-User-Login: a\r\ntailscale-user-login: b\r\n\r\n", "400 Bad Request"),
            ("duplicate login then transfer-encoding", b"GET / HTTP/1.1\r\nHost: h\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\nTransfer-Encoding: chunked\r\n\r\n", "403 Forbidden"),
            ("transfer-encoding then duplicate login", b"GET / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\n\r\n", "400 Bad Request"),
            ("duplicate login then bad name", b"GET / HTTP/1.1\r\nHost: h\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\nBad Name: v\r\n\r\n", "403 Forbidden"),
            ("bad name then duplicate login", b"GET / HTTP/1.1\r\nHost: h\r\nBad Name: v\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\n\r\n", "400 Bad Request"),
            ("duplicate login then duplicate host", b"GET / HTTP/1.1\r\nHost: h\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\nHost: i\r\n\r\n", "403 Forbidden"),
            ("duplicate host then duplicate login", b"GET / HTTP/1.1\r\nHost: h\r\nHost: i\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\n\r\n", "400 Bad Request"),
            ("duplicate login + oversized length", b"GET / HTTP/1.1\r\nHost: h\r\nTailscale-User-Login: a\r\nTailscale-User-Login: b\r\nContent-Length: 5000\r\n\r\n", "403 Forbidden"),
            ("duplicate host", b"GET / HTTP/1.1\r\nHost: h\r\nHOST: i\r\n\r\n", "400 Bad Request"),
            ("duplicate content-length", b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n", "400 Bad Request"),
            ("duplicate origin", b"GET / HTTP/1.1\r\nHost: h\r\nOrigin: a\r\nOrigin: a\r\n\r\n", "400 Bad Request"),
            ("duplicate content-type", b"GET / HTTP/1.1\r\nHost: h\r\nContent-Type: a\r\nContent-Type: a\r\n\r\n", "400 Bad Request"),
            ("duplicate sec-fetch-site", b"GET / HTTP/1.1\r\nHost: h\r\nSec-Fetch-Site: a\r\nSec-Fetch-Site: a\r\n\r\n", "400 Bad Request"),
            ("transfer-encoding", b"GET / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n", "400 Bad Request"),
            ("transfer-encoding + oversized length", b"GET / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\nContent-Length: 5000\r\n\r\n", "400 Bad Request"),
            ("non-canonical length", b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 010\r\n\r\n", "400 Bad Request"),
            ("body on GET", b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 1\r\n\r\n", "400 Bad Request"),
            ("oversized length", b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 257\r\n\r\n", "400 Bad Request"),
            ("bare LF in a value", b"GET / HTTP/1.1\r\nHost: h\nX: b\r\n\r\n", "400 Bad Request"),
            ("bare LF line ends", b"GET / HTTP/1.1\nHost: h\n\r\n\r\n", "400 Bad Request"),
            ("header without colon", b"GET / HTTP/1.1\r\nHost: h\r\nNoColon\r\n\r\n", "400 Bad Request"),
            ("control byte in value", b"GET / HTTP/1.1\r\nHost: h\r\nX: a\x01b\r\n\r\n", "400 Bad Request"),
            ("space in name", b"GET / HTTP/1.1\r\nHost: h\r\nBad Name: v\r\n\r\n", "400 Bad Request"),
            ("empty host", b"GET / HTTP/1.1\r\nHost: \r\n\r\n", "400 Bad Request"),
            ("missing host", b"GET / HTTP/1.1\r\n\r\n", "400 Bad Request"),
            ("not UTF-8", b"GET /\xff HTTP/1.1\r\nHost: h\r\n\r\n", "400 Bad Request"),
        ];
        for (name, head, want) in cases {
            assert_eq!(status(head), format!("HTTP/1.1 {want}"), "{name}");
        }
        assert_eq!(
            status(long.as_bytes()),
            "HTTP/1.1 431 Request Header Fields Too Large",
            "head without terminator at 8192 bytes"
        );
    }

    #[test]
    fn refused_owner_posts_report_allowlisted_stages_without_payload_bytes() {
        for (body, expected) in [
            (
                "csrf=wrong&message=hello+Lead",
                RefusalFact {
                    status: 403,
                    stage: Stage::Csrf,
                    reason: "invalid",
                    fix: "reload the served chat form and resubmit",
                },
            ),
            (
                "csrf=fixture&message=%FF",
                RefusalFact {
                    status: 403,
                    stage: Stage::Body,
                    reason: "late_parse",
                    fix: "send one well-formed urlencoded chat submission",
                },
            ),
            (
                "csrf=fixture&message=",
                RefusalFact {
                    status: 403,
                    stage: Stage::Body,
                    reason: "late_parse",
                    fix: "send one well-formed urlencoded chat submission",
                },
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = facts.clone();
            let handler = std::thread::spawn(move || {
                let owner = OwnerForm {
                    csrf: "fixture".into(),
                    post: Arc::new(|_| panic!("refused submission reached the owner callback")),
                };
                let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
                connection_fixed(
                    server,
                    |_, _| true,
                    |_, _| panic!("unexpected render"),
                    None,
                    Some(&owner),
                    Some(&refusals),
                    None,
                )
                .unwrap();
            });
            client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            write!(client, "POST /chat HTTP/1.1\r\nHost: example.invalid\r\nOrigin: https://example.invalid\r\nSec-Fetch-Site: same-origin\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            handler.join().unwrap();
            assert!(reply.starts_with("HTTP/1.1 403"), "{body}: {reply}");
            let facts = facts.lock().unwrap();
            assert_eq!(facts.as_slice(), [expected], "{body}");
        }
    }

    #[test]
    fn owner_confirmation_requires_the_proposal_id_as_the_only_submission() {
        let id = "a".repeat(43);
        let allowed = format!("csrf=fixture&proposal_id={id}");
        assert!(matches!(
            owner_submission(allowed.as_bytes(), "fixture"),
            Ok(OwnerSubmission::Confirm(received)) if received == id
        ));
        for refused in [
            format!("{allowed}&message=also+run"),
            format!("{allowed}&proposal_id={id}"),
            "csrf=fixture&proposal_id=short".into(),
        ] {
            assert!(
                matches!(
                    owner_submission(refused.as_bytes(), "fixture"),
                    Err(Rejected::Body)
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            owner_submission(allowed.as_bytes(), "different"),
            Err(Rejected::Csrf)
        ));
    }

    fn exchange(request: &str, permitted: bool) -> (String, usize) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let handler = std::thread::spawn(move || {
            connection(
                server,
                |_, _| permitted,
                |page, fragment| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let board = oxa_work::board::Board::default();
                    let view = crate::lead_page::View {
                        board: Some(&board),
                        ..Default::default()
                    };
                    if fragment {
                        crate::lead_page::content_for(page, &view, 100)
                    } else {
                        crate::lead_page::document_for(page, &view, 100)
                    }
                },
            )
            .unwrap()
        });
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        handler.join().unwrap();
        (response, reads.load(Ordering::SeqCst))
    }
    #[test]
    fn refusals_never_read_facts_or_reflect_input() {
        for (request, permitted, status) in [
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nTailscale-User-Login: synthetic-private-marker\r\n\r\n", false, "403"),
            ("POST / HTTP/1.1\r\nHost: example.invalid\r\n\r\n", true, "405"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nTailscale-User-Login: a\r\ntailscale-user-login: b\r\n\r\n", true, "403"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nContent-Length: 1\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nHOST: other.invalid\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\nHost: \r\n\r\n", true, "400"),
        ] {
            let (reply, reads) = exchange(request, permitted);
            assert!(reply.starts_with(&format!("HTTP/1.1 {status}")));
            assert_eq!(reads, 0);
            assert!(!reply.contains("synthetic-private-marker"));
        }
    }
    #[test]
    fn shared_renderer_and_sse_read_one_snapshot_after_authorization() {
        for path in ["/", "/events", "/style.css", "/live.js", "/unknown"] {
            let (reply, reads) = exchange(
                &format!("GET {path} HTTP/1.1\r\nHost: example.invalid\r\n\r\n"),
                true,
            );
            assert_eq!(reads, usize::from(matches!(path, "/" | "/events")));
            assert!(reply.contains("Cache-Control: no-store"));
            if path == "/events" {
                assert!(reply.contains("Content-Type: text/event-stream"));
                assert!(reply.contains("event: snapshot\ndata: <main>"));
            }
            if path == "/" {
                assert!(reply.contains("No open PRs in this snapshot"));
            }
            if path == "/unknown" {
                assert!(reply.starts_with("HTTP/1.1 404"));
            }
        }
    }

    fn live_client(allowed: bool, live: Live) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        std::thread::spawn(move || {
            let _ = connection_fixed(
                server,
                move |_, _| allowed,
                |_, _| panic!("the live route renders nothing"),
                None,
                None,
                None,
                Some(&live),
            );
        });
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client
            .write_all(b"GET /events/live HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
            .unwrap();
        client
    }

    fn read_updates(client: &mut TcpStream, want: usize) -> String {
        let (mut text, mut buffer) = (String::new(), [0u8; 1024]);
        while text.matches("event: lead-update").count() < want {
            let read = client.read(&mut buffer).unwrap();
            assert!(read > 0, "stream ended early: {text}");
            text.push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
        text
    }

    #[test]
    fn live_route_is_authorized_first_and_streams_changes_and_recovers_after_disconnect() {
        use std::sync::atomic::AtomicU64;
        let (reads, value) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU64::new(1)));
        let live = || {
            let (reads, value) = (reads.clone(), value.clone());
            let (read, current) = (reads.clone(), value.clone());
            Live {
                generation: Box::new(move || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    value.load(Ordering::SeqCst)
                }),
                wait: Box::new(move |seen, timeout| {
                    let deadline = Instant::now() + timeout;
                    while current.load(Ordering::SeqCst) == seen && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    read.fetch_add(1, Ordering::SeqCst);
                    current.load(Ordering::SeqCst)
                }),
                running_sha256: Some("a".repeat(64)),
            }
        };
        // Unauthorized: refused before the generation is ever read.
        let mut refused = live_client(false, live());
        let mut reply = String::new();
        refused.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        // Authorized: the first update at once, a second only after the generation changes.
        let mut client = live_client(true, live());
        let first = read_updates(&mut client, 1);
        assert!(first.contains("Content-Type: text/event-stream"), "{first}");
        assert!(!first.contains("Content-Length"), "{first}");
        assert!(first.contains("retry: 5000"), "{first}");
        assert!(first.contains(&format!("data: {{\"running_sha256\":\"{}\"}}", "a".repeat(64))));
        value.store(2, Ordering::SeqCst);
        read_updates(&mut client, 1);
        // A client that went away gets a fresh stream with an event on reconnect.
        drop(client);
        let mut again = live_client(true, live());
        read_updates(&mut again, 1);
    }

    #[test]
    fn live_stream_delivers_a_change_made_right_after_the_first_event() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::{Condvar, Mutex};
        // Force the interleaving the loaded CI runner hit by chance: the value
        // changes after the client has received the first event, but before the
        // server samples its baseline generation. The baseline call waits
        // (bounded) for that change; a baseline sampled after the first write
        // swallows it and never sends a second event. With the baseline sampled
        // before the first write the wait cannot be satisfied in time and simply
        // expires, and the change is delivered.
        let value = Arc::new(AtomicU64::new(1));
        let changed = Arc::new((Mutex::new(false), Condvar::new()));
        let baseline_call = Arc::new(AtomicBool::new(true));
        let (current, signal, baseline) = (value.clone(), changed.clone(), baseline_call.clone());
        let waited = value.clone();
        let live = Live {
            generation: Box::new(move || {
                if baseline.swap(false, Ordering::SeqCst) {
                    let (lock, cvar) = &*signal;
                    let mut got = lock.lock().unwrap();
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !*got {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        got = cvar.wait_timeout(got, left).unwrap().0;
                    }
                }
                current.load(Ordering::SeqCst)
            }),
            wait: Box::new(move |seen, timeout| {
                let deadline = Instant::now() + timeout;
                while waited.load(Ordering::SeqCst) == seen && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                waited.load(Ordering::SeqCst)
            }),
            running_sha256: None,
        };
        let mut client = live_client(true, live);
        let first = read_updates(&mut client, 1);
        assert!(first.contains("event: lead-update"), "{first}");
        value.store(2, Ordering::SeqCst);
        let (lock, cvar) = &*changed;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
        read_updates(&mut client, 1);
        drop(client);
    }

    #[test]
    fn slow_reader_cannot_reset_the_total_response_write_budget() {
        use std::sync::atomic::AtomicBool;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let finished = stop.clone();
        let reader = std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if matches!(client.read(&mut [0]), Ok(0)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let payload = vec![b'x'; 16 * 1024 * 1024];
        let began = Instant::now();
        let result = write_response(&mut server, &[&payload], Duration::from_secs(1));
        let elapsed = began.elapsed();
        stop.store(true, Ordering::SeqCst);
        drop(server);
        reader.join().unwrap();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_millis(1750),
            "partial writes renewed the one-second budget: {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod approval_transport_tests {
    use super::*;
    use std::net::TcpListener;

    /// One `POST /approvals` over a real loopback connection; returns the reply, how many
    /// answers reached the owner form, and the refusal facts the transport recorded.
    fn approval_exchange(origin: &str, site: &str, body: &str) -> (String, usize, Vec<RefusalFact>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let invoked = count.clone();
        let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = facts.clone();
        let handler = std::thread::spawn(move || {
            let owner = OwnerForm {
                csrf: "fixture".into(),
                post: Arc::new(move |submission| {
                    assert!(matches!(
                        submission,
                        OwnerSubmission::Approve { ref id, ref answer }
                            if id == "0123456789abcdef" && answer == "once"
                    ));
                    invoked.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            };
            let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
            connection_fixed(
                server,
                |_, _| true,
                |_, _| panic!("unexpected render"),
                None,
                Some(&owner),
                Some(&refusals),
                None,
            )
            .unwrap();
        });
        client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        // A cross-origin submission is refused before its body is read; an unread payload would
        // turn the close into a reset, so only the same-origin submissions send their body.
        let payload = if origin == "https://example.invalid" && site == "same-origin" {
            body
        } else {
            ""
        };
        write!(
            client,
            "POST /approvals HTTP/1.1\r\nHost: example.invalid\r\nOrigin: {origin}\r\nSec-Fetch-Site: {site}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{payload}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        handler.join().unwrap();
        let facts = facts.lock().unwrap().clone();
        (reply, count.load(Ordering::SeqCst), facts)
    }

    #[test]
    fn approval_post_refuses_bad_csrf() {
        let body = "csrf=wrong&id=0123456789abcdef&answer=once";
        let (reply, answered, facts) = approval_exchange("https://example.invalid", "same-origin", body);
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert_eq!(answered, 0);
        assert_eq!(
            facts,
            [RefusalFact {
                status: 403,
                stage: Stage::Csrf,
                reason: "invalid",
                fix: "reload the served chat form and resubmit",
            }]
        );
    }

    #[test]
    fn approval_post_refuses_cross_site_origin() {
        let body = "csrf=fixture&id=0123456789abcdef&answer=once";
        for (origin, site) in [
            ("https://other.invalid", "same-origin"),
            ("https://example.invalid", "same-site"),
            ("https://example.invalid", ""),
        ] {
            let (reply, answered, facts) = approval_exchange(origin, site, body);
            assert!(reply.starts_with("HTTP/1.1 403"), "{origin} {site:?}: {reply}");
            assert_eq!(answered, 0, "{origin} {site:?}");
            assert_eq!(facts.len(), 1, "{origin} {site:?}");
            assert_eq!(facts[0].stage, Stage::Origin, "{origin} {site:?}");
        }
    }

    #[test]
    fn approval_post_accepts_one_well_formed_answer_only() {
        let (reply, answered, facts) = approval_exchange(
            "https://example.invalid",
            "same-origin",
            "csrf=fixture&id=0123456789abcdef&answer=once",
        );
        assert!(reply.starts_with("HTTP/1.1 303"), "{reply}");
        assert!(reply.contains("Location: /\r\n"), "{reply}");
        assert_eq!(answered, 1);
        assert!(facts.is_empty());
        // An unlisted answer or an extra field is malformed and never reaches the owner form.
        for body in [
            "csrf=fixture&id=0123456789abcdef&answer=maybe",
            "csrf=fixture&id=0123456789abcdef&answer=once&extra=1",
            "csrf=fixture&id=0123456789abcdef",
        ] {
            let (reply, answered, _) = approval_exchange("https://example.invalid", "same-origin", body);
            assert!(reply.starts_with("HTTP/1.1 403"), "{body}: {reply}");
            assert_eq!(answered, 0, "{body}");
        }
    }

    #[test]
    fn approval_post_refuses_get_and_other_paths_at_the_transport() {
        // A GET to the same path is a method refusal, logged as a refusal on the approval path.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = facts.clone();
        let handler = std::thread::spawn(move || {
            let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
            connection_fixed(
                server,
                |_, _| true,
                |_, _| panic!("unexpected render"),
                None,
                None,
                Some(&refusals),
                None,
            )
            .unwrap();
        });
        client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        write!(
            client,
            "POST /approvals?x=1 HTTP/1.1\r\nHost: example.invalid\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        handler.join().unwrap();
        assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");
        assert_eq!(facts.lock().unwrap().len(), 1);
    }

    #[test]
    fn owner_attach_field_is_exclusive_and_takes_only_session_ids() {
        let parse = |body: &str| owner_submission(body.as_bytes(), "nonce");
        assert!(matches!(
            parse("csrf=nonce&attach=abc-123.x_y"),
            Ok(OwnerSubmission::Attach(Some(id))) if id == "abc-123.x_y"
        ));
        assert!(matches!(
            parse("csrf=nonce&attach="),
            Ok(OwnerSubmission::Attach(None))
        ));
        for body in [
            "csrf=nonce&attach=..%2Fx",
            "csrf=nonce&attach=a/b",
            "csrf=nonce&attach=-x",
            "csrf=nonce&attach=a&message=b",
            "csrf=nonce&attach=a&attach=b",
        ] {
            assert!(matches!(parse(body), Err(Rejected::Body)), "{body}");
        }
        assert!(matches!(parse("csrf=wrong&attach=a"), Err(Rejected::Csrf)));
    }

    #[test]
    fn owner_message_may_name_its_session_and_nothing_else_may() {
        let parse = |body: &str| owner_submission(body.as_bytes(), "nonce");
        assert!(matches!(
            parse("csrf=nonce&session=abc-123&message=hi"),
            Ok(OwnerSubmission::AttachedMessage { session, text }) if session == "abc-123" && text == "hi"
        ));
        assert!(matches!(
            parse("csrf=nonce&message=hi"),
            Ok(OwnerSubmission::Message(text)) if text == "hi"
        ));
        for body in [
            "csrf=nonce&session=..%2Fx&message=hi",
            "csrf=nonce&session=&message=hi",
            "csrf=nonce&session=a&session=a&message=hi",
            "csrf=nonce&session=a&attach=a",
            "csrf=nonce&session=a&proposal_id=x",
            "csrf=nonce&session=a",
        ] {
            assert!(matches!(parse(body), Err(Rejected::Body)), "{body}");
        }
    }
}
