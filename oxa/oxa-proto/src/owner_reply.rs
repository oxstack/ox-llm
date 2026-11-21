//! #1552: the owner's own typed `yes` as approval for a publication.
//!
//! The authority test is the origin of the message. The only caller of [`typed`] is the
//! interactive front end, from the line the owner typed into its input; text the model read from
//! a tool result, a file, the web or another session never reaches it. A publication that
//! shows its exact text and waits calls [`await_reply`] with its session and the digest of that
//! text; a `yes` the owner types in that session afterwards approves exactly that text once
//! ([`take_approved`]). Any other line the owner types in the session withdraws that session's
//! pending text and approval. A session switch withdraws nothing: the session and the digest
//! keep an answer bound to the request that showed it, so another request or session can never
//! consume it.
use std::sync::Mutex;

/// A request that was shown: the session that showed it and the digest of its exact text.
type Request = (String, String);

#[derive(Default)]
struct State {
    pending: Option<Request>,
    approved: Option<Request>,
}

static STATE: Mutex<State> = Mutex::new(State {
    pending: None,
    approved: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The exact text identified by `digest` was shown to the owner in `session`, who has not
/// answered yet.
pub fn await_reply(session: &str, digest: &str) {
    let mut s = state();
    // A new question in this session withdraws an earlier approval of this session; another
    // session's request leaves it alone.
    if s.approved
        .as_ref()
        .is_some_and(|(shown, _)| shown.as_str() == session)
    {
        s.approved = None;
    }
    s.pending = Some((session.to_string(), digest.to_string()));
}

/// A line the OWNER typed into the interactive input, in `session` (call from there only).
pub fn typed(session: &str, line: &str) {
    let mut s = state();
    if line.trim().eq_ignore_ascii_case("yes") {
        if let Some((shown, digest)) = s.pending.take_if(|(shown, _)| shown.as_str() == session) {
            s.approved = Some((shown, digest));
        }
    } else {
        if s.pending
            .as_ref()
            .is_some_and(|(shown, _)| shown.as_str() == session)
        {
            s.pending = None;
        }
        if s.approved
            .as_ref()
            .is_some_and(|(shown, _)| shown.as_str() == session)
        {
            s.approved = None;
        }
    }
}

/// `true` once, when the owner typed `yes` in `session` for exactly this text.
pub fn take_approved(session: &str, digest: &str) -> bool {
    let mut s = state();
    let matches = s
        .approved
        .as_ref()
        .is_some_and(|(shown, approved)| shown.as_str() == session && approved.as_str() == digest);
    if matches {
        s.approved = None;
    }
    matches
}

/// Withdraw this session's pending text identified by `digest` and its approval (the owner
/// denied that text). A denial of another text leaves this one alone.
pub fn clear_for(session: &str, digest: &str) {
    let mut s = state();
    let same = |(shown, text): &(String, String)| shown.as_str() == session && text.as_str() == digest;
    if s.pending.as_ref().is_some_and(same) {
        s.pending = None;
    }
    if s.approved.as_ref().is_some_and(same) {
        s.approved = None;
    }
}

/// Withdraw every pending text and approval.
pub fn clear() {
    let mut s = state();
    s.pending = None;
    s.approved = None;
}
