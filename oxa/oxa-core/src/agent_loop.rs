//! B3: the agent loop.
// ported from pi packages/agent/src/agent-loop.ts
use crate::exec::{fail_truncated_calls, reply_event, run_calls, RequestOpts};
use crate::host::observe_hooks;
use crate::SessionState;
use oxa_proto::{
    codes, miss_cause, rebase_why, shown_why, AgentEvent, Entry, EntryBody, EventMessage, EventSink, Finish,
    HookPoint, Level, OxaError, Part, Role, StopReason, TurnKind, Ui, Usage, Value, WireMsg,
};
use oxa_session::new_entry_id;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// The assistant stopped without tool calls and no steering was pending.
    Stopped,
    /// Cancelled; the partial reply (if any) was stored as `Shown { why: aborted }`.
    Aborted,
    /// A request failed after retries; stored as `Shown { why: failed | cut }`.
    Failed(OxaError),
}

/// The start of the stop message of a repetition loop (`stream_broken`, never a transient failure).
pub(crate) const REPEAT_STOP: &str = "Response stopped: the model fell into a repetition loop";

/// Persist `body` as a new entry under the current leaf, add it to the tree and fold it into
/// the transcript. The only way an entry is created. Emits nothing; callers emit events.
/// A store failure is returned and leaves tree and transcript unchanged.
pub fn append_entry(st: &mut SessionState, body: EntryBody) -> Result<Entry, OxaError> {
    let id = {
        let tree = &st.tree;
        new_entry_id(&|candidate: &str| {
            tree.get(candidate).is_some()
                || st
                    .goal
                    .as_ref()
                    .is_some_and(|goal| goal.reserves_entry_id(candidate))
        })
    };
    append_entry_with_id(st, id, body)
}

pub(crate) fn append_entry_with_id(
    st: &mut SessionState,
    id: String,
    body: EntryBody,
) -> Result<Entry, OxaError> {
    if st.tree.get(&id).is_some() {
        return Err(OxaError::new(
            oxa_proto::codes::INVALID,
            "Session entry identity already exists.",
        ));
    }
    let leaf = st.tree.leaf();
    let parent = if leaf.is_empty() {
        None
    } else {
        Some(leaf.to_string())
    };
    let entry = append_to_store(st.store.as_ref(), &st.sref, id, parent, body)?;
    st.tree.push(entry.clone());
    st.transcript.apply(&entry);
    // #2292: an explicit preservation requirement in a user message rides on the danger
    // floor's existing question. It stays until a newer requirement replaces it.
    if let Some(requirement) = preservation_of(&entry) {
        st.policy.set_preservation(Some(&requirement));
    }
    Ok(entry)
}

/// #2292: the explicit preservation requirement of a user message entry, if any. Only the
/// typed text is read (the environment note is stripped); assistant, tool and summary
/// entries never carry one.
pub(crate) fn preservation_of(entry: &Entry) -> Option<String> {
    let EntryBody::Msg {
        wire,
        parts,
        tool: None,
    } = &entry.body
    else {
        return None;
    };
    if wire.role != Role::User {
        return None;
    }
    let content = wire.content.as_deref()?;
    oxa_perm::preservation_requirement(crate::envnote::strip_note(content, parts))
}

pub(crate) fn append_to_store(
    store: &dyn oxa_session::SessionStore,
    session: &oxa_session::SessionRef,
    id: String,
    parent: Option<String>,
    body: EntryBody,
) -> Result<Entry, OxaError> {
    let entry = Entry {
        id,
        parent,
        ts_ms: now_ms(),
        body,
    };
    store.append(session, &entry)?;
    Ok(entry)
}

/// Run from the current wire (the user message is already appended) until the assistant stops.
/// Implements the loop of the crate docs: `chat_with_retry` -> push reply -> `execute_calls`
/// -> push results -> drain steering (`Shared::take_steering`, each as a user message) ->
/// repeat. Emits `AgentStart`, `TurnStart`/`TurnEnd`, `Message*`, `AgentEnd` and after each
/// reply `CacheMiss` when `usage.cached < transcript.expected_cache()` (cause from the last
/// `RebaseLog`, else `cold_start` on the first request, else `unknown`). Appends a `Usage`
/// entry per request. Mid-run compaction (`When::MidRun`) is delegated to `maybe_compact`
/// between turns. Fires the observer hooks of each step.
///
/// A plain `Finish::Length` reply gets bounded recovery under the auto-retry policy. Local
/// recovery keeps the cut reply, appends an explicit continuation instruction, and uses an
/// advertised zero-token prefill. Remote recovery keeps the cut reply as shown progress only
/// and requests a concise complete answer. A cut reply with tool calls never runs them: each
/// gets `tool_errors::TRUNCATED`, so the model can reissue complete arguments. Soft termination
/// takes priority over either recovery path.
pub fn run_loop(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<RunEnd, OxaError> {
    crate::goal::deliver_saved(st, sink)?;
    crate::goal::ensure_begin(st);
    crate::goal::checkpoint(st, false, sink)?;
    st.shared
        .gate
        .running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let steering = st
        .goal
        .as_ref()
        .filter(|goal| goal.pursuing())
        .and_then(|_| st.shared.open_goal_steering(&st.sref));
    sink.emit(&AgentEvent::AgentStart);
    observe_hooks(st, HookPoint::AgentStart, &Value::Null, sink, ui);
    let mut run_msgs: Vec<EventMessage> = Vec::new();
    // pi's loop starts with the run's prompt messages (the user messages appended since the
    // last reply): `message_start` / `message_end` each, and they open `agent_end.messages`.
    let opening = crate::fire::opening_messages(st);
    // #2293 A: the declared file deliverables of this run, checked once at its end.
    let mut done = crate::done::DoneState::default();
    done.begin(st);
    // T6: the tool a task names for its acceptance, read once at run start. In a goal-engaged
    // run the conversation is the model's own pursuit, not the task: nothing is read from it.
    let planning = !crate::fire::goal_engaged(st);
    let task = if planning {
        crate::fire::opening_text(st)
    } else {
        String::new()
    };
    done.begin_toolchain(st, &task, planning);
    let result = run_inner(st, sink, ui, &mut run_msgs, &opening, &mut done);
    // A failed run records why its pursuing goal stopped; `/goal resume` (or the a2a resume)
    // continues it in this session.
    if let Ok(RunEnd::Failed(error)) = &result {
        crate::goal::record_failure(st, error, sink)?;
    }
    crate::goal::checkpoint(st, false, sink)?;
    drop(steering);
    crate::think::end_run(st, ui);
    // The completion record of the files this run declared, also after a wrap-up, an abort or
    // a failure. A run that declared none emits nothing, so an unrelated task pays nothing.
    crate::done::report_deliverables(st, &done, sink);
    sink.emit(&AgentEvent::AgentEnd {
        messages: run_msgs.clone(),
        will_retry: false,
    });
    let all: Vec<EventMessage> = opening.into_iter().chain(run_msgs).collect();
    crate::fire::agent_end(st, &all, sink, ui);
    result
}

fn run_inner(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
    opening: &[EventMessage],
    done: &mut crate::done::DoneState,
) -> Result<RunEnd, OxaError> {
    let mut turn_index: u32 = 0;
    let mut schema_repaired = None;
    let mut pending_completion = None;
    let mut wrapping_up = false;
    let mut wrap_up_failure = None;
    let mut length_recoveries = 0;
    // FH-N2: a headless run gets one nudge when its final reply ends on a plan or a promise.
    let mut headless_nudged = false;
    // The repeat guard warns the user once per run (`repeat.rs`, #1448).
    let mut repeat_warned = false;
    // Consecutive transient failures of one logical request. This state lives inside the run so
    // recovery preserves done checks, the turn limit and the request idempotency key.
    let mut goal_failures: u32 = 0;
    // #2183: when a tool boundary of a pursuing goal first saw a follow-up that is still queued.
    let mut follow_up_seen: Option<std::time::Instant> = None;
    // Replies in a row without a tool call while a goal is pursued: the first continues, the
    // second adds one nudge, the third and later wait before the next request. Run-local.
    let mut idle_streak: u32 = 0;
    // `oxa.maxTurns` (default unset: no limit): model replies of this run (#507).
    let max_turns = st.settings.u32(crate::repeat::MAX_TURNS, 0);
    // #2295: the verified-artifact line the failed-check repair request carries, taken by that
    // request. A run whose checks all passed has none here.
    let mut artifacts: Option<String> = None;
    'turns: loop {
        if crate::goal::terminal(st) {
            return Ok(RunEnd::Stopped);
        }
        if max_turns > 0 && turn_index >= max_turns && !wrapping_up {
            let e = crate::repeat::turn_limit_error(max_turns);
            persist(
                st,
                EntryBody::Shown {
                    text: e.to_string(),
                    why: shown_why::FAILED,
                },
                sink,
            )?;
            sink.emit(&AgentEvent::Notice {
                level: Level::Error,
                text: e.to_string(),
            });
            return Ok(RunEnd::Failed(e));
        }
        if !wrapping_up {
            // Time awareness: one short note at 70% and one at 90% of `--max-time`. It is a user
            // message at a turn boundary, so the frozen prefix does not change.
            let note = (crate::Handle {
                shared: st.shared.clone(),
            })
            .take_time_note();
            if let Some(note) = note {
                let tail = if note.percent >= 90 {
                    "; commit what works now"
                } else {
                    ""
                };
                let text = format!(
                    "[time: {}% of this run's time limit used, about {} min left{tail}]",
                    note.percent, note.minutes_left
                );
                push_user(st, text, sink, ui, run_msgs)?;
            }
            let reason = match st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) {
                1 => Some(crate::WrapUpReason::TimeLimit),
                2 => Some(crate::WrapUpReason::Signal),
                3 => Some(crate::WrapUpReason::TokenLimit),
                _ => None,
            };
            if let Some(reason) = reason {
                wrapping_up = true;
                crate::progress::notify(
                    st,
                    crate::Progress::WrapUp {
                        limit: reason.limit(),
                    },
                );
                // #2293 A: a declared file that is not on disk is named here, so the last
                // turn can still write it.
                push_user(st, crate::done::wrap_up_message(done), sink, ui, run_msgs)?;
                sink.emit(&AgentEvent::Notice {
                    level: Level::Info,
                    text: reason.notice().to_string(),
                });
            }
        }
        // E-INT 1: host requests that arrived since the last step (queries, appends) are
        // served between steps; session-shape actions wait for settlement.
        crate::host::serve_host(st, false, sink, ui);
        sink.emit(&AgentEvent::TurnStart);
        crate::fire::turn_start(st, turn_index, sink, ui);
        // The records of the user messages sent for the first time, then their hooks.
        announce_user_messages(st, sink);
        if turn_index == 0 {
            for m in opening {
                crate::fire::message(st, HookPoint::MessageStart, m, sink, ui);
                crate::fire::message(st, HookPoint::MessageEnd, m, sink, ui);
            }
        }
        let first = st.stats.requests == 0;
        let mut expected = expected_cached(st);

        let pending = pending_message();
        sink.emit(&AgentEvent::MessageStart {
            message: pending.clone(),
        });
        crate::fire::message(st, HookPoint::MessageStart, &pending, sink, ui);

        // Adaptive thinking (`think.rs`): a reply to a prompt starts the escalation from off.
        crate::think::before_reply(st, ui);
        let mut opts = RequestOpts {
            kind: TurnKind::Chat,
            max_tokens: wrapping_up.then(|| wrap_up_tokens(st)),
            thinking: wrapping_up.then_some(false),
            background: false,
            snapshot_keep: false,
            // The contract (including repair feedback) belongs to this run, not the
            // persisted conversation. Repeat it for tool/continuation requests too.
            // #2295: a verified-artifact line is the task of ONE request: it is taken here
            // and sent again by no request.
            extra: turn_extra(st, &mut artifacts, &schema_repaired),
            events: true,
            idem_key: String::new(),
        };
        let mut thinking_on = !wrapping_up && crate::think::request_thinking(st, TurnKind::Chat);
        let mut retried = false;
        let mut empty_retries: u32 = 0;
        let mut replays: u32 = 0;
        if let Some(capture) = st.shared.gate.wire_evidence.get() {
            capture.new_request();
        }
        let turn = loop {
            // A run that continues after a persisted cut, failure or wrap-up is still foreground
            // work: re-arm the signal that `persist` cleared, so input during the replay or the
            // next turn is steering, not a new run. The run's guard clears it at the end.
            st.shared
                .gate
                .running
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if !wrapping_up && st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                let text = "Request skipped for wrap-up.".to_string();
                let progress = persist(
                    st,
                    EntryBody::Shown {
                        text: text.clone(),
                        why: shown_why::ABORTED,
                    },
                    sink,
                )?;
                let mut message = pending.clone();
                message.stop = Some(StopReason::Aborted);
                message.error_message = Some(text);
                finish_discarded_turn(st, sink, ui, message, (turn_index, &progress.id));
                turn_index += 1;
                continue 'turns;
            }
            let mut capture = Capture {
                inner: &mut *sink,
                text: String::new(),
                got_data: false,
                detector: crate::loopguard::LoopDetector::new(),
                reasoning_detector: crate::loopguard::LoopDetector::new(),
                cancel: st.shared.cancel.clone(),
                looped: false,
            };
            // #456: one key per logical request, so a replay after a cut stream is the same
            // request to oxc (which can attach it to the still-running generation).
            if opts.idem_key.is_empty() {
                opts.idem_key = crate::retry::fresh_idem_key(st);
            }
            let res = if wrapping_up {
                crate::exec::request(st, &opts, &mut capture, ui)
            } else {
                crate::retry::chat_with_retry_keyed(st, &mut opts, &mut capture, ui)
            };
            let partial = std::mem::take(&mut capture.text);
            let got_data = capture.got_data;
            let looped = capture.looped;
            // Our own stop (degenerate repetition, #141), not a user abort: clear the flag we set.
            let res = match res {
                // The request may also have run to a clean end (finish and `[DONE]` in the same
                // read as the threshold-crossing delta): `looped` is authoritative either way.
                r if looped
                    && match &r {
                        Ok(_) => true,
                        Err(e) => e.code == oxa_proto::codes::ABORTED,
                    } =>
                {
                    st.shared.cancel.reset();
                    Err(OxaError::new(
                        oxa_proto::codes::STREAM_BROKEN,
                        format!("{REPEAT_STOP} (the same block of text repeated)."),
                    )
                    .with_hint(
                        "Rephrase the request or try again; the partial reply is shown but not sent back.",
                    ))
                }
                other => other,
            };

            let turn = match res {
                Ok(t) => {
                    goal_failures = 0;
                    t
                }
                Err(e) if replayable(st, &e, got_data, looped, replays) => {
                    // #456: a transport cut mid-output. The partial reply stays visible as
                    // `Shown { cut }` (never sent, W2) and the request is resent from the same
                    // wire; a fresh `MessageStart` opens the replay's message.
                    replays += 1;
                    // The cut request ran and its usage never arrived: count the request.
                    st.stats.requests += 1;
                    let text = if partial.is_empty() {
                        e.to_string()
                    } else {
                        partial
                    };
                    persist_continuing(
                        st,
                        EntryBody::Shown {
                            text,
                            why: shown_why::CUT,
                        },
                        sink,
                    )?;
                    sink.emit(&AgentEvent::Notice {
                        level: Level::Info,
                        text: format!("the reply stream broke ({}); resending the request", e.message),
                    });
                    sink.emit(&AgentEvent::MessageStart {
                        message: pending_message(),
                    });
                    continue;
                }
                Err(e) => {
                    let why = failure_why(&e, got_data);
                    // Partial text is shown, never sent (W2); the wire keeps the user message.
                    let text = if partial.is_empty() {
                        e.to_string()
                    } else if looped {
                        // Our own stop: the notice must stay visible after the partial reply.
                        format!("{partial}\n\n{e}")
                    } else {
                        partial
                    };
                    let retry_stopped = st.shared.gate.retry_stopped.swap(false, Ordering::SeqCst);
                    let delay = (!retry_stopped)
                        .then(|| crate::goal::failure_backoff(st, &e, goal_failures))
                        .flatten();
                    let progress = persist_with(st, EntryBody::Shown { text, why }, delay.is_some(), sink)?;
                    if let Some(delay) = delay {
                        goal_failures += 1;
                        let text = format!(
                                "Goal waiting {} s after a transient request failure ({}); no request is sent meanwhile, then the goal continues.",
                                delay.as_secs(), e.code
                            );
                        persist(
                            st,
                            EntryBody::Shown {
                                text: text.clone(),
                                why: shown_why::NOTICE,
                            },
                            sink,
                        )?;
                        let shared = st.shared.clone();
                        shared.wait_wake.reset();
                        *crate::queue::lock(&shared.gate.backoff_model) = Some(None);
                        sink.emit(&AgentEvent::Notice {
                            level: Level::Info,
                            text,
                        });
                        let deadline = std::time::Instant::now() + delay;
                        loop {
                            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                            let has_input = shared.has_steering();
                            let ended = crate::goal::wait_failure_backoff(
                                st,
                                if has_input {
                                    std::time::Duration::ZERO
                                } else {
                                    remaining
                                },
                            );
                            let mut waiting = crate::queue::lock(&shared.gate.backoff_model);
                            let model = waiting.take().flatten();
                            shared.wait_wake.reset();
                            drop(waiting);
                            match ended {
                                crate::goal::BackoffEnd::Aborted => return Ok(RunEnd::Aborted),
                                crate::goal::BackoffEnd::RetryStopped => return Ok(RunEnd::Failed(e)),
                                crate::goal::BackoffEnd::Elapsed => {}
                            }
                            let mut changed = false;
                            if let Some(model) = model.filter(|m| *m != st.model) {
                                match crate::session::switch_model(
                                    st,
                                    &model,
                                    crate::OverCap::Refuse,
                                    sink,
                                    ui,
                                ) {
                                    Ok(()) => changed = true,
                                    // A persistence error after the model changed must not replay the old key.
                                    Err(error) if st.model == model => return Err(error),
                                    Err(error) => sink.emit(&AgentEvent::Notice {
                                        level: Level::Warning,
                                        text: format!(
                                            "{error}. Stop the retry to use the full model switch dialog."
                                        ),
                                    }),
                                }
                            }
                            changed |= deliver_steering(st, sink, ui, run_msgs)?;
                            if changed {
                                opts.idem_key.clear();
                                goal_failures = 0;
                                replays = 0;
                                retried = false;
                                empty_retries = 0;
                                expected = expected_cached(st);
                                thinking_on =
                                    !wrapping_up && crate::think::request_thinking(st, TurnKind::Chat);
                            }
                            if changed || std::time::Instant::now() >= deadline {
                                sink.emit(&AgentEvent::MessageStart {
                                    message: pending_message(),
                                });
                                break;
                            }
                            *crate::queue::lock(&shared.gate.backoff_model) = Some(None);
                        }
                        continue;
                    }
                    if e.code == "cloud_quota_exhausted" {
                        return Ok(RunEnd::Failed(e));
                    }
                    let soft_stop = st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0;
                    if soft_stop {
                        let mut message = pending.clone();
                        if let EntryBody::Shown { text, .. } = progress.body {
                            message.msg.content = Some(text);
                        }
                        message.stop = Some(if e.code == oxa_proto::codes::ABORTED {
                            StopReason::Aborted
                        } else {
                            StopReason::Error
                        });
                        message.error_message = Some(e.to_string());
                        finish_discarded_turn(st, sink, ui, message, (turn_index, &progress.id));
                        turn_index += 1;
                    }
                    if !wrapping_up && soft_stop && e.code != oxa_proto::codes::ABORTED {
                        // The final turn may save work, but cannot erase the failure
                        // that made this request unsuccessful.
                        wrap_up_failure = Some(e);
                        continue 'turns;
                    }
                    return Ok(if let Some(error) = wrap_up_failure {
                        RunEnd::Failed(error)
                    } else if why == shown_why::ABORTED {
                        RunEnd::Aborted
                    } else {
                        RunEnd::Failed(e)
                    });
                }
            };
            // #184: thinking ran to the output limit and nothing else came out.
            if !retried && crate::think::reasoning_only_overrun(st, &turn, thinking_on) {
                crate::think::record_overrun(st, ui);
                account_discarded(st, &turn);
                if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                    // The normal fallback is a new request. A pending stop instead
                    // gets the bounded final instruction from the outer turn loop.
                    let progress = persist_continuing(
                        st,
                        EntryBody::Shown {
                            text: "Thinking reached its output limit; wrapping up.".to_string(),
                            why: shown_why::CUT,
                        },
                        sink,
                    )?;
                    finish_discarded_turn(
                        st,
                        sink,
                        ui,
                        reply_event(&turn, now_ms()),
                        (turn_index, &progress.id),
                    );
                    turn_index += 1;
                    continue 'turns;
                }
                retried = true;
                opts.thinking = Some(false);
                opts.idem_key = String::new();
                sink.emit(&AgentEvent::Notice {
                    level: Level::Info,
                    text: crate::think::OVERRUN_NOTICE.to_string(),
                });
                continue;
            }
            // A cloud reply that ended normally with no text and no tool call is asked again
            // (`oxa.emptyReplyRetries`, default 1); the empty reply is never kept.
            if st.info.caps.remote
                && !wrapping_up
                && turn.finish == Finish::Stop
                && turn.msg.tool_calls.is_empty()
                && turn.msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
                && empty_retries < st.settings.empty_reply_retries()
                && st.auto_retry
                && st.settings.retry().enabled
                && st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) == 0
                && !st.shared.cancel.is_cancelled()
            {
                empty_retries += 1;
                account_discarded(st, &turn);
                opts.idem_key = String::new();
                sink.emit(&AgentEvent::Notice {
                    level: Level::Info,
                    text: "the model returned an empty reply; asking again".to_string(),
                });
                continue;
            }
            if retried && crate::think::empty_length(&turn) {
                account_discarded(st, &turn);
                let e = OxaError::new(
                    crate::think::OVERRUN_ERROR_CLASS,
                    "The reply ran out of output tokens before any answer, even without thinking.",
                )
                .with_hint("Ask for a shorter answer or split the task.");
                let progress = persist(
                    st,
                    EntryBody::Shown {
                        text: e.to_string(),
                        why: shown_why::FAILED,
                    },
                    sink,
                )?;
                sink.emit(&AgentEvent::Notice {
                    level: Level::Error,
                    text: e.to_string(),
                });
                if !wrapping_up && st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                    wrap_up_failure = Some(e);
                    finish_discarded_turn(
                        st,
                        sink,
                        ui,
                        reply_event(&turn, now_ms()),
                        (turn_index, &progress.id),
                    );
                    turn_index += 1;
                    continue 'turns;
                }
                return Ok(RunEnd::Failed(e));
            }
            break turn;
        };

        if turn.finish == Finish::Length
            && turn.msg.tool_calls.is_empty()
            && st.info.caps.remote
            && !wrapping_up
            && st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) == 0
            && st.shared.literal_output_remaining() == Some(0)
        {
            // The bounded literal turn has spent its shared output budget: the
            // recovery request could never reserve a token, so it is neither
            // scheduled nor described with its internal instruction, and the
            // partial answer is never presented as a valid reply. The cut stays
            // as the audit entry; the typed local bound reaches the caller.
            account_discarded(st, &turn);
            let text = turn
                .msg
                .content
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "The reply reached its output limit before answering.".to_string());
            persist(
                st,
                EntryBody::Shown {
                    text,
                    why: shown_why::CUT,
                },
                sink,
            )?;
            let e = OxaError::new(
                codes::LITERAL_OUTPUT_EXHAUSTED,
                "The literal turn's output limit is exhausted.",
            )
            .with_hint("Ask for a shorter answer.");
            sink.emit(&AgentEvent::Notice {
                level: Level::Error,
                text: e.to_string(),
            });
            return Ok(RunEnd::Failed(e));
        }
        if turn.finish == Finish::Length
            && turn.msg.tool_calls.is_empty()
            && st.info.caps.remote
            && !wrapping_up
            && st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) == 0
            && length_recoverable(st, length_recoveries)
        {
            account_discarded(st, &turn);
            let text = turn
                .msg
                .content
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "The reply reached its output limit before answering.".to_string());
            let progress = persist_continuing(
                st,
                EntryBody::Shown {
                    text,
                    why: shown_why::CUT,
                },
                sink,
            )?;
            finish_discarded_turn(
                st,
                sink,
                ui,
                reply_event(&turn, now_ms()),
                (turn_index, &progress.id),
            );
            turn_index += 1;
            length_recoveries += 1;
            push_user(
                st,
                crate::prompt::LENGTH_RETRY_REMOTE.to_string(),
                sink,
                ui,
                run_msgs,
            )?;
            sink.emit(&AgentEvent::Notice {
                level: Level::Info,
                text: "Reply reached the output limit; requesting a concise complete answer.".to_string(),
            });
            continue 'turns;
        }
        if turn.finish != Finish::Length {
            length_recoveries = 0;
        }
        st.stats.output_truncated = turn.finish == Finish::Length;
        let usage: Usage = turn.usage;
        if !st.info.caps.remote {
            // Wave D D4: cache-miss accounting and the reload probe are local-engine features.
            note_cache(st, expected, &usage, first, sink);
            if !usage.cached_missing {
                crate::reload::after_reply(st, first, expected, usage.cached, sink);
                // R2
            }
        }

        // W2: the reply is pushed exactly as streamed, then its usage (which sets the next
        // expected cache).
        let ts = now_ms();
        let reply_entry = persist(
            st,
            EntryBody::Msg {
                wire: turn.msg.clone(),
                parts: Vec::new(),
                tool: None,
            },
            sink,
        )?
        .id;
        persist(
            st,
            EntryBody::Usage {
                kind: TurnKind::Chat.as_str().to_string(),
                usage,
            },
            sink,
        )?;
        let reply = reply_event(&turn, ts);
        sink.emit(&AgentEvent::MessageEnd {
            message: reply.clone(),
        });
        crate::fire::message(st, HookPoint::MessageEnd, &reply, sink, ui);
        if turn.finish == Finish::ContentFilter {
            sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: "The model declined to answer this request.".to_string(),
            });
        }
        run_msgs.push(reply.clone());
        account_reply(st, turn.timings, usage);
        st.last_usage = usage.reported();
        crate::goal::checkpoint(st, true, sink)?;
        // F4: capabilities that only a loaded engine reports (`/props` `oxe_hints`): re-read
        // once after the first reply while `prefill_only` is still false, and after a request
        // that recovered from a model load or restart. Never per request otherwise.
        let gate = &st.shared.gate;
        let reread = gate.reread_info.swap(false, std::sync::atomic::Ordering::SeqCst)
            || (!st.info.prefill_only
                && !gate
                    .info_rechecked
                    .swap(true, std::sync::atomic::Ordering::SeqCst));
        if reread {
            crate::session::refresh_budget(st, None, sink);
        }

        let calls = turn.msg.tool_calls.clone();
        if !calls.is_empty() {
            if let Some(goal) = &st.goal {
                goal.note_tools();
            }
            // A reply cut by the output limit may carry truncated arguments: fail, never run.
            let results = if turn.finish == Finish::Length {
                fail_truncated_calls(st, &calls, sink, ui)
            } else {
                run_calls(st, &calls, sink, ui)
            };
            let mut tool_msgs = Vec::with_capacity(results.len());
            let mut tool_entries = Vec::with_capacity(results.len());
            for r in &results {
                let entry = persist(
                    st,
                    EntryBody::Msg {
                        wire: r.msg.clone(),
                        parts: Vec::new(),
                        tool: Some(oxa_proto::ToolMeta {
                            is_error: r.is_error,
                            ui: r.ui.clone(),
                        }),
                    },
                    sink,
                )?;
                tool_entries.push(entry.id);
                let msg = EventMessage {
                    msg: r.msg.clone(),
                    ts_ms: now_ms(),
                    stop: None,
                    usage: None,
                    model: None,
                    tool_name: Some(r.tool_name.clone()),
                    is_error: r.is_error,
                    error_message: None,
                };
                // pi emits message_start / message_end for each tool result message.
                crate::fire::message(st, HookPoint::MessageStart, &msg, sink, ui);
                crate::fire::message(st, HookPoint::MessageEnd, &msg, sink, ui);
                tool_msgs.push(msg);
            }
            idle_streak = 0;
            done.note_edits(&results);
            st.stats.ci_polls += results.iter().filter(|r| r.ui["ciPoll"] == true).count() as u32;
            done.note_proof(&calls, &results);
            done.note_toolchain(&calls);
            // #2295: with `done` checks in the run, keep this batch's written files as the
            // verified result a repair names.
            done.note_artifacts(crate::done::checks_run(st));
            done.note_bash(st, &results);
            crate::exec::note_stored_images(st, &results, sink)?;
            // #2295: the next repair request (if any) carries this batch's verified files.
            artifacts = done.verified_artifacts(&st.cwd);
            // Adaptive thinking (`think.rs`): the batch decides the next follow-up.
            crate::think::observe(st, &calls, &results, sink, ui);
            st.stats.tool_calls += calls.len() as u32;
            st.stats.tool_results += results.len() as u32;
            st.stats.context_tokens = st.transcript.context();
            run_msgs.extend(tool_msgs.iter().cloned());
            turn_end(
                st,
                sink,
                ui,
                reply,
                tool_msgs,
                (turn_index, &reply_entry, &tool_entries),
            );
            turn_index += 1;

            crate::goal::account(st, false)?;
            if crate::goal::terminal(st) {
                let _ = crate::done::check(st, done, sink, ui);
                crate::advice::observe_done(st, done.observation.take(), false, sink)?;
                if st.stats.done_failed {
                    if let Some(goal) = &st.goal {
                        goal.reject_completion();
                    }
                }
                if let Some(goal) = st
                    .goal
                    .clone()
                    .filter(|goal| goal.snapshot().status == oxa_session::GoalStatus::Done)
                {
                    match crate::repair::schema_repair_turn(st, &turn, &mut schema_repaired, sink) {
                        Ok(false) => {}
                        repair => {
                            pending_completion = goal.defer_completion();
                            crate::goal::checkpoint(st, false, sink)?;
                            repair?;
                            continue;
                        }
                    }
                    // One nudge per open fact (unproven edits, running jobs): the goal stays
                    // pursuing for one more turn, then the completion goes through.
                    if !wrapping_up && !st.shared.has_input() {
                        match crate::done::finish_decision(st, done, true) {
                            crate::done::FinishDecision::Nudge(text) => {
                                pending_completion = goal.defer_completion();
                                crate::goal::checkpoint(st, false, sink)?;
                                crate::done::send_nudge(st, text, sink, ui, run_msgs)?;
                                continue;
                            }
                            crate::done::FinishDecision::Verifier { nudge, unproven } => {
                                // `proveIt` alone: the edit stays unproven for the deferred
                                // completion; the verifier question settles the goal stop.
                                if unproven {
                                    pending_completion = goal.defer_completion();
                                }
                                crate::goal::checkpoint(st, false, sink)?;
                                crate::done::send_nudge(st, nudge, sink, ui, run_msgs)?;
                                continue;
                            }
                            crate::done::FinishDecision::End { unresolved } => {
                                crate::done::record_end(st, done, &unresolved, sink)?;
                            }
                        }
                    }
                }
                crate::goal::checkpoint(st, false, sink)?;
                return Ok(RunEnd::Stopped);
            }
            crate::goal::checkpoint(st, false, sink)?;
            announce_exceeded(st, &results, sink)?;

            if st.shared.cancel.is_cancelled() || wrapping_up {
                return Ok(wrap_up_failure.map_or(RunEnd::Aborted, RunEnd::Failed));
            }
            if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                continue;
            }
            // The repeat guard's give-up point: the same call again after its notices (#1448).
            // The run no longer ends here. The advisor keeps its authority: a delivered
            // suggestion continues the run, an escalation still ends it. Either way the user
            // is warned once — shown and persisted with the remedy — and the next model reply
            // proceeds (steering and compaction run as on any turn).
            if let Some(r) = results.iter().find(|r| r.warn) {
                let mut text = crate::repeat::warning_text(&r.tool_name);
                let mut escalate = false;
                if let Some(call) = calls
                    .iter()
                    .find(|call| Some(&call.id) == r.msg.tool_call_id.as_ref())
                {
                    match crate::advice::on_repeat(st, call, &text, sink)? {
                        // The advisor delivered: the run continues through the warning below.
                        crate::advice::AdviceResult::Continue => {}
                        // No advisor, or none delivered: same warning path.
                        crate::advice::AdviceResult::Refused => {}
                        crate::advice::AdviceResult::Escalate => {
                            text = "Advice did not resolve the failure. The worker needs a stronger tier."
                                .into();
                            escalate = true;
                        }
                    }
                }
                if st.shared.cancel.is_cancelled() {
                    return Ok(RunEnd::Aborted);
                }
                if escalate {
                    persist(
                        st,
                        EntryBody::Shown {
                            text: text.clone(),
                            why: shown_why::NOTICE,
                        },
                        sink,
                    )?;
                    sink.emit(&AgentEvent::Notice {
                        level: Level::Error,
                        text: text.clone(),
                    });
                    // The files edited so far are still checked and the result shown; the run
                    // is over, so a failure is not sent back to the model.
                    let _ = crate::done::check(st, done, sink, ui);
                    crate::advice::observe_done(st, done.observation.take(), false, sink)?;
                    // Not a finished task: a caller must not read it as success.
                    return Ok(RunEnd::Failed(OxaError::new("advisor_escalation_required", text)));
                }
                // One warning per run; later repeats keep their model-facing notice.
                if !repeat_warned {
                    repeat_warned = true;
                    persist(
                        st,
                        EntryBody::Shown {
                            text: text.clone(),
                            why: shown_why::NOTICE,
                        },
                        sink,
                    )?;
                    sink.emit(&AgentEvent::Notice {
                        level: Level::Warning,
                        text,
                    });
                }
            }
            // Steering drains after the tool batch, each message as a user message.
            deliver_steering(st, sink, ui, run_msgs)?;
            // #2183: a pursuing goal can call tools without ever stopping or waiting, so a
            // follow-up is not left behind such a turn for ever: one that has waited
            // `oxa.goal.followUpMaxWaitSeconds` is delivered at this tool boundary.
            if crate::goal::follow_up_due(st, &mut follow_up_seen) {
                for text in drain_follow_ups(st, sink) {
                    push_user(st, text, sink, ui, run_msgs)?;
                }
            }
            // A wait that input woke hands that input to the model now: a follow-up is
            // otherwise only drained when the run stops, after the model has answered
            // the wait result without it.
            if calls.iter().any(|call| call.name == "wait") && st.shared.wait_wake.is_cancelled() {
                st.shared.wait_wake.reset();
                for text in drain_follow_ups(st, sink) {
                    push_user(st, text, sink, ui, run_msgs)?;
                }
            }
            // L5 mid-run (`oxa.compaction.midRun`): past the soft limit, compact before the next
            // request, keeping this unanswered batch verbatim. A failed or cancelled attempt
            // leaves the wire unchanged and the run goes on (the hard-limit check below still
            // protects it); a user abort during the summary request ends the run.
            if crate::compaction::compact_mid_run(st, sink, ui) != crate::compaction::MidRun::NotRun
                && st.shared.cancel.is_cancelled()
            {
                return Ok(RunEnd::Aborted);
            }
            // Above the hard limit the next request would overflow: compact between turns (W5).
            if let Err(e) = crate::compaction::maybe_compact(st, false, sink, ui) {
                persist(
                    st,
                    EntryBody::Shown {
                        text: e.to_string(),
                        why: shown_why::FAILED,
                    },
                    sink,
                )?;
                return Ok(RunEnd::Failed(e));
            }
            continue;
        }

        turn_end(st, sink, ui, reply, Vec::new(), (turn_index, &reply_entry, &[]));
        turn_index += 1;
        if wrapping_up {
            return Ok(wrap_up_failure.map_or(RunEnd::Aborted, RunEnd::Failed));
        }
        if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            continue;
        }
        if turn.finish == Finish::Length && !st.info.caps.remote && length_recoverable(st, length_recoveries)
        {
            length_recoveries += 1;
            push_user(
                st,
                crate::prompt::LENGTH_CONTINUE_LOCAL.to_string(),
                sink,
                ui,
                run_msgs,
            )?;
            sink.emit(&AgentEvent::Notice {
                level: Level::Info,
                text: "Reply reached the output limit; continuing the task.".to_string(),
            });
            crate::compaction::idle_prefill(st, sink, ui);
            continue;
        }
        if crate::repair::repair_turn(st, &turn, sink)? {
            continue;
        }
        if deliver_steering(st, sink, ui, run_msgs)? {
            continue;
        }
        let failed_check = crate::done::check(st, done, sink, ui);
        let advised = crate::advice::observe_done(st, done.observation.take(), true, sink)?;
        if matches!(advised, crate::advice::AdviceResult::Escalate) {
            return Ok(RunEnd::Failed(OxaError::new(
                "advisor_escalation_required",
                "Advice did not resolve the failure. The worker needs a stronger tier.",
            )));
        }
        if let Some(text) = failed_check {
            crate::done::inject(st, text, sink, ui, run_msgs)?;
            continue;
        }
        if matches!(advised, crate::advice::AdviceResult::Continue) {
            continue;
        }
        if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            continue;
        }
        if crate::repair::schema_repair_turn(st, &turn, &mut schema_repaired, sink)? {
            continue;
        }
        if let Some(goal) = st.goal.clone().filter(|g| g.engaged()) {
            crate::goal::checkpoint(st, false, sink)?;
            if pending_completion.is_some() && turn.finish != Finish::Length && !st.shared.has_input() {
                match crate::done::finish_decision(st, done, true) {
                    crate::done::FinishDecision::Nudge(text) => {
                        crate::done::send_nudge(st, text, sink, ui, run_msgs)?;
                        continue;
                    }
                    crate::done::FinishDecision::Verifier { nudge, .. } => {
                        crate::done::send_nudge(st, nudge, sink, ui, run_msgs)?;
                        continue;
                    }
                    crate::done::FinishDecision::End { unresolved } => {
                        crate::done::record_end(st, done, &unresolved, sink)?;
                    }
                }
            }
            if let Some(evidence) = pending_completion.take() {
                if goal.pursuing() {
                    goal.finish_completion(evidence)?;
                }
                crate::goal::checkpoint(st, false, sink)?;
                return Ok(RunEnd::Stopped);
            }
            let was_pursuing = goal.pursuing();
            let idle_action = crate::goal::idle_action(st, &goal, max_turns);
            let (mut snapshot, idle_noted) = goal.stopped(idle_action.enforces());
            let mut idle_stopped = None;
            if idle_action == oxa_config::OnExceed::Stop
                && was_pursuing
                && snapshot.status == oxa_session::GoalStatus::Paused
            {
                let reason = snapshot.pause_reason.clone().unwrap_or_default();
                snapshot = goal.stop_because(&reason);
                idle_stopped = Some(reason);
            }
            crate::goal::save_checkpoint(st, &snapshot, sink)?;
            if idle_noted {
                crate::goal::announce_idle(sink);
                crate::goal::record_notice(st, crate::goal::IDLE_NOTICE.into(), sink)?;
            }
            if was_pursuing && snapshot.status == oxa_session::GoalStatus::Paused {
                if let Some(reason) = snapshot.pause_reason.as_deref() {
                    crate::goal::announce_pause(reason, sink);
                    crate::goal::record_notice(st, format!("Goal paused: {reason}; /goal resume"), sink)?;
                }
            }
            if let Some(reason) = idle_stopped {
                crate::goal::record_exceeded(
                    st,
                    format!("Goal stopped: {reason} (oxa.onExceed.goalIdle is stop)"),
                    sink,
                )?;
            }
            if goal.pursuing() && st.active.get("goal_done").is_none() {
                // The owner's tool selection removed the goal controls: the continue
                // instruction would name a tool that does not exist, so stop with a reason.
                let snapshot = goal.pause_because(crate::goal::CONTROLS_MISSING);
                crate::goal::save_checkpoint(st, &snapshot, sink)?;
                crate::goal::announce_pause(crate::goal::CONTROLS_MISSING, sink);
                return Ok(RunEnd::Stopped);
            }
            if goal.pursuing() {
                // Queued input (a follow-up that woke a wait) is the next user message:
                // stop so `settle_runs` delivers it instead of the continuation prompt.
                if st.shared.has_input() {
                    return Ok(RunEnd::Stopped);
                }
                idle_streak += 1;
                // A reply that matched the output schema is an answer, not an idle reply: say
                // that it was recorded and what comes next (a wait or goal_done).
                let answered = st.output_schema.is_some();
                let mut text = if answered {
                    crate::prompt::GOAL_CONTINUE_AFTER_ANSWER
                } else {
                    crate::prompt::GOAL_CONTINUE
                };
                if idle_streak == 2 {
                    text = crate::prompt::GOAL_IDLE_NUDGE;
                } else if let Some(delay) = crate::goal::idle_wait(st, &goal, idle_streak) {
                    let note = format!(
                        "Goal idle: {idle_streak} replies without an action; waiting {} s (oxa.goal.idleBackoffSeconds; oxa.goal.idlePause pauses instead)",
                        delay.as_secs()
                    );
                    st.shared.wait_wake.reset();
                    persist(
                        st,
                        EntryBody::Shown {
                            text: note.clone(),
                            why: shown_why::NOTICE,
                        },
                        sink,
                    )?;
                    sink.emit(&AgentEvent::Notice {
                        level: Level::Info,
                        text: note,
                    });
                    if st.shared.has_input() {
                        return Ok(RunEnd::Stopped);
                    }
                    match crate::goal::wait_failure_backoff(st, delay) {
                        crate::goal::BackoffEnd::Aborted => return Ok(RunEnd::Aborted),
                        crate::goal::BackoffEnd::RetryStopped => return Ok(RunEnd::Stopped),
                        crate::goal::BackoffEnd::Elapsed => {}
                    }
                    st.shared.wait_wake.reset();
                    if st.shared.has_input() {
                        return Ok(RunEnd::Stopped);
                    }
                }
                push_user(st, text.into(), sink, ui, run_msgs)?;
                continue;
            }
        }
        if crate::is_headless()
            && !headless_nudged
            && turn.finish != Finish::Length
            && !st.shared.cancel.is_cancelled()
            && !st.shared.has_input()
            && turn
                .msg
                .content
                .as_deref()
                .is_some_and(|c| crate::prompt::ends_unfinished(c, done.proof.edited()))
        {
            headless_nudged = true;
            push_user(st, crate::prompt::HEADLESS_NUDGE.to_string(), sink, ui, run_msgs)?;
            sink.emit(&AgentEvent::Notice {
                level: Level::Info,
                text: "The reply ended on a plan or a question in a headless run; asking it to continue."
                    .to_string(),
            });
            continue;
        }
        if turn.finish != Finish::Length && !st.shared.has_input() {
            match crate::done::finish_decision(st, done, false) {
                crate::done::FinishDecision::Nudge(text) => {
                    crate::done::send_nudge(st, text, sink, ui, run_msgs)?;
                    continue;
                }
                crate::done::FinishDecision::Verifier { nudge, .. } => {
                    crate::done::send_nudge(st, nudge, sink, ui, run_msgs)?;
                    continue;
                }
                crate::done::FinishDecision::End { unresolved } => {
                    crate::done::record_end(st, done, &unresolved, sink)?;
                }
            }
        }
        return Ok(RunEnd::Stopped);
    }
}

/// pi's agent loop opens a turn with `message_start` / `message_end` for each user message it
/// sends for the first time (the prompt, a delivered steering or follow-up message), right
/// after `turn_start` (json.md "Event sequence"). Those are the user `Msg` entries of the
/// active branch since the last reply or failed request; hidden custom messages are not
/// announced (they are not user messages in pi either).
fn announce_user_messages(st: &mut SessionState, sink: &mut dyn EventSink) {
    let mut fresh: Vec<EventMessage> = Vec::new();
    for e in st.tree.branch(st.tree.leaf()).iter().rev() {
        match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == Role::User => fresh.push(EventMessage {
                msg: wire.clone(),
                ts_ms: e.ts_ms,
                stop: None,
                usage: None,
                model: None,
                tool_name: None,
                is_error: false,
                error_message: None,
            }),
            // The previous reply, tool result, failed request or rebase ends the search.
            EntryBody::Msg { .. }
            | EntryBody::Shown { .. }
            | EntryBody::Compaction { .. }
            | EntryBody::BranchSummary { .. }
            | EntryBody::Prefix(_) => break,
            _ => {}
        }
    }
    for message in fresh.into_iter().rev() {
        sink.emit(&AgentEvent::MessageStart {
            message: message.clone(),
        });
        sink.emit(&AgentEvent::MessageEnd { message });
    }
}

/// Append every queued steering message as a user message. `true` when there was one.
fn deliver_steering(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) -> Result<bool, OxaError> {
    let taken = drain_steering(st, sink);
    let mut appended = !taken.is_empty();
    for text in taken {
        push_user(st, text, sink, ui, run_msgs)?;
    }
    if let Some(request) = st.shared.take_goal_steering() {
        // The assignment guard covers the write, then releases before events or hooks.
        if let Some(entry) = request.deliver(st, append_user_entry)? {
            announce_user_entry(st, entry, sink, ui, run_msgs);
            appended = true;
        }
    }
    Ok(appended)
}

/// Append one user message to the transcript and announce it.
pub(crate) fn push_user(
    st: &mut SessionState,
    text: String,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) -> Result<(), OxaError> {
    let entry = append_user_entry(st, text)?;
    announce_user_entry(st, entry, sink, ui, run_msgs);
    Ok(())
}

fn append_user_entry(st: &mut SessionState, text: String) -> Result<Entry, OxaError> {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(text.clone()),
            parts: vec![Part::Text(text)],
            tool: None,
        },
    )
}

fn announce_user_entry(
    st: &mut SessionState,
    entry: Entry,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) {
    let EntryBody::Msg { wire, .. } = &entry.body else {
        unreachable!("the user append only creates messages");
    };
    let msg = EventMessage {
        msg: wire.clone(),
        ts_ms: entry.ts_ms,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    };
    run_msgs.push(msg.clone());
    sink.emit(&AgentEvent::EntryAppended { entry });
    crate::fire::message(st, HookPoint::MessageStart, &msg, sink, ui);
    crate::fire::message(st, HookPoint::MessageEnd, &msg, sink, ui);
}

/// Take steering messages from the shared queue (`Shared::take_steering` applies the mode)
/// and announce the queue that is left.
fn drain_steering(st: &SessionState, sink: &mut dyn EventSink) -> Vec<String> {
    // A change no front end has reported yet (a steer queued during this turn) is announced
    // first, so the queue that held the message is seen (pi emits `queue_update` on steer).
    if st.shared.dirty.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let (steering, follow_up) = st.shared.snapshot();
        sink.emit(&AgentEvent::QueueUpdate { steering, follow_up });
    }
    let taken = st.shared.take_steering();
    if !taken.is_empty() {
        let (steering, follow_up) = st.shared.snapshot();
        sink.emit(&AgentEvent::QueueUpdate { steering, follow_up });
    }
    taken
}

/// Take follow-ups from the shared queue (same announcements as [`drain_steering`]).
fn drain_follow_ups(st: &SessionState, sink: &mut dyn EventSink) -> Vec<String> {
    if st.shared.dirty.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let (steering, follow_up) = st.shared.snapshot();
        sink.emit(&AgentEvent::QueueUpdate { steering, follow_up });
    }
    let taken = st.shared.take_follow_up();
    if !taken.is_empty() {
        let (steering, follow_up) = st.shared.snapshot();
        sink.emit(&AgentEvent::QueueUpdate { steering, follow_up });
    }
    taken
}

/// An owner budget a tool result exceeded (`oxa.onExceed`): one warning notice and one recorded
/// `Shown` entry per result, whatever the action was (warn, or a refused call).
fn announce_exceeded(
    st: &mut SessionState,
    results: &[crate::exec::CallResult],
    sink: &mut dyn EventSink,
) -> Result<(), OxaError> {
    for result in results {
        if let Some((_, notice)) = oxa_proto::ToolOutput::exceeded_notice(&result.ui) {
            sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: notice.to_string(),
            });
            persist(
                st,
                EntryBody::Shown {
                    text: notice.to_string(),
                    why: shown_why::NOTICE,
                },
                sink,
            )?;
        }
    }
    Ok(())
}

/// Persist a body and announce the entry. A dropped reply (aborted, failed, cut) ends the
/// foreground signal (`Gate::running`) before the entry is announced, so settlement's remaining
/// busy claim does not turn input typed after it into steering for a finished turn. A dropped
/// reply the same run replaces with another request uses [`persist_continuing`].
pub(crate) fn persist(
    st: &mut SessionState,
    body: EntryBody,
    sink: &mut dyn EventSink,
) -> Result<Entry, OxaError> {
    persist_with(st, body, false, sink)
}

/// [`persist`] for a cut reply the run goes on from (a replay after a cut stream, a length
/// recovery, a wrap-up turn): the run still takes input, so the foreground signal stays set
/// through the events emitted before the next request (the request loop also re-arms it).
fn persist_continuing(
    st: &mut SessionState,
    body: EntryBody,
    sink: &mut dyn EventSink,
) -> Result<Entry, OxaError> {
    persist_with(st, body, true, sink)
}

fn persist_with(
    st: &mut SessionState,
    body: EntryBody,
    continues: bool,
    sink: &mut dyn EventSink,
) -> Result<Entry, OxaError> {
    let entry = append_entry(st, body)?;
    if !continues
        && matches!(&entry.body, EntryBody::Shown { why, .. } if [shown_why::ABORTED, shown_why::FAILED, shown_why::CUT].contains(why))
    {
        st.shared
            .gate
            .running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    sink.emit(&AgentEvent::EntryAppended { entry: entry.clone() });
    Ok(entry)
}

/// Close a visible request replaced by wrap-up without adding an assistant message to the
/// wire. Its entry ID identifies the shown progress record, including for an unissued request.
fn finish_discarded_turn(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    message: EventMessage,
    ids: (u32, &str),
) {
    sink.emit(&AgentEvent::MessageEnd {
        message: message.clone(),
    });
    crate::fire::message(st, HookPoint::MessageEnd, &message, sink, ui);
    turn_end(st, sink, ui, message, Vec::new(), (ids.0, ids.1, &[]));
}

fn turn_end(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    message: EventMessage,
    tool_results: Vec<EventMessage>,
    ids: (u32, &str, &[String]),
) {
    sink.emit(&AgentEvent::TurnEnd {
        message: message.clone(),
        tool_results: tool_results.clone(),
    });
    crate::resume::sync_turn(st, sink); // R1: the turn's appends are durable
    crate::fire::turn_end(st, ids.0, &message, &tool_results, (ids.1, ids.2), sink, ui);
}

/// A request whose reply is not kept (the overrun turn, a failed retry and a context-full cut): its tokens still
/// count in the stats. No `Usage` entry, so the cache expectation is not moved by it.
pub(crate) fn account_discarded(st: &mut SessionState, turn: &oxa_proto::AssistantTurn) {
    st.stats.requests += 1;
    st.stats.prompt_tokens += turn.usage.prompt as u64;
    st.stats.completion_tokens += turn.usage.completion as u64;
    st.stats.cached_tokens += turn.usage.cached as u64;
}

fn account_reply(st: &mut SessionState, timings: Option<oxa_proto::Timings>, usage: Usage) {
    st.stats.requests += 1;
    st.stats.assistant_messages += 1;
    st.stats.prompt_tokens += usage.prompt as u64;
    st.stats.completion_tokens += usage.completion as u64;
    st.stats.cached_tokens += usage.cached as u64;
    st.stats.context_tokens = st.transcript.context();
    if let Some(t) = timings {
        if t.predicted_ms > 0.0 {
            st.stats.last_tok_s = t.predicted_n as f64 / (t.predicted_ms / 1000.0);
        }
    }
}

/// THE cache expectation (one rule for every request over the session's wire: chat turns, the
/// idle prefill, the park and the boot preload). Prompt tokens the engine should report as
/// cached on the next request:
///
/// 1. `Transcript::expected_cache()` when it is set: the last request over this wire
///    (`prompt + completion - 1`, F1; `prompt` after an H6 prefill), also after a resume (the
///    fold replays the usage entries).
/// 2. Otherwise, after a rebase (compaction, fork / leaf move, model switch epoch, context edit,
///    extension rewrite) or on any request after the first one of this process: the frozen
///    prefix (system text + tools, counted by the engine's tokenizer), the anchor the engine
///    keeps for every session of the model. What the server's flags or advertised capabilities
///    say does not change it: a server that does not keep the anchor is reported.
/// 3. A true cold start (a new session's first request, no rebase, nothing sent yet): 0, silent.
///
/// The anchor count is a lower bound (raw text, no template framing); when the engine cannot
/// count, nothing is expected.
pub(crate) fn expected_cached(st: &SessionState) -> u32 {
    let set = st.transcript.expected_cache();
    if set > 0 {
        return reasoning_bound(st, set);
    }
    if st.stats.requests == 0 && st.transcript.rebases().is_empty() {
        return 0;
    }
    let text = format!("{}{}", st.prefix.system_text(), st.prefix.tools_json);
    st.engine.count_tokens(&text).unwrap_or(0)
}

/// B6: the template's opener of an assistant turn (`<think>\n`, or `<think>\n\n</think>\n\n`
/// with thinking off) is part of the prompt of the request that generated the turn, and is the
/// part a template that drops reasoning renders differently. An upper bound of its tokens on
/// the Qwen templates.
pub(crate) const REASONING_OPENER_TOKENS: u32 = 8;

/// B6: what the engine can keep at a new user message on a model whose capabilities say it
/// does not keep reasoning (`preserve_thinking: false`, Flash). Its template, when it is told
/// to drop reasoning, re-renders every assistant turn at or before the last user message
/// without its think block, so the history departs from the engine's state at the first
/// assistant turn of the previous user turn: right after `turn_base` minus the opener. A
/// template that keeps reasoning (the Qwen default when the key is absent, what oxa sends to
/// Flash) or the engine's ECHO restoring it reports the full `prompt + completion - 1`, which
/// is above the bound: either way no false miss. Inside a tool loop (no new user message) the
/// reasoning is kept by every template, so the rule is unchanged there.
fn reasoning_bound(st: &SessionState, set: u32) -> u32 {
    if st.info.caps.preserve_thinking != Some(false) {
        return set;
    }
    let new_query = st.transcript.wire().last().map(|m| m.role) == Some(Role::User);
    let base = st.transcript.turn_base();
    if !new_query || base == 0 {
        return set;
    }
    set.min(base.saturating_sub(REASONING_OPENER_TOKENS))
}

/// B6: shown once per session when the server never reports `cached_tokens`.
pub(crate) const CACHED_UNREPORTED_TEXT: &str = "The engine does not report cached prompt tokens (usage.prompt_tokens_details.cached_tokens); cache misses cannot be detected for this session.";

/// Report a reply whose `cached` is below [`expected_cached`] (computed before the request) as
/// one `CacheMiss` with its cause. A reply without `cached_tokens` (B6) is neither a miss nor a
/// hit: one notice per session says so.
pub(crate) fn note_cache(
    st: &mut SessionState,
    expected: u32,
    usage: &Usage,
    first: bool,
    sink: &mut dyn EventSink,
) {
    if st.info.caps.remote {
        return;
    }
    if usage.cached_missing {
        if !st.stats.cached_unreported {
            st.stats.cached_unreported = true;
            sink.emit(&AgentEvent::Notice {
                level: Level::Warning,
                text: CACHED_UNREPORTED_TEXT.to_string(),
            });
        }
        return;
    }
    let cached = usage.cached;
    if cached < expected {
        let cause = miss_cause_for(st, first);
        sink.emit(&AgentEvent::CacheMiss {
            expected,
            cached,
            cause,
        });
        st.stats.cache_misses += 1;
    }
}

/// The cause of a cache miss, from where [`expected_cached`] took its value: the rebase since
/// the last request over this wire (rule 2), else `cold_start` on the process's first request
/// (a resume on an engine that no longer holds the session), else `engine_evicted` (the
/// engine lost state it held for this exact wire; an oxa wire change would be a rebase or a
/// W1 violation, which `oxa_fake::check_w1` reports separately).
fn miss_cause_for(st: &SessionState, first: bool) -> &'static str {
    let after_rebase = st.transcript.expected_cache() == 0;
    match st.transcript.rebases().last() {
        Some(r) if after_rebase => match r.why {
            rebase_why::COMPACTION => miss_cause::COMPACTION,
            rebase_why::LEAF_MOVE => miss_cause::LEAF_MOVE,
            rebase_why::CONTEXT_EDIT => miss_cause::CONTEXT_EDIT,
            rebase_why::EPOCH => miss_cause::EPOCH,
            rebase_why::EXTENSION => miss_cause::EXTENSION,
            _ => miss_cause::UNKNOWN,
        },
        _ if first => miss_cause::COLD_START,
        _ => miss_cause::ENGINE_EVICTED,
    }
}

/// Stored reason for a failed request (W2): cut when a data byte already arrived.
/// #456: a stream that broke after output (not our own loop stop) is resent while the retry
/// policy allows it. A refusal after output (budget, settlement) is never resent. A break before output is already retried inside `chat_with_retry`.
fn replayable(st: &SessionState, e: &OxaError, got_data: bool, looped: bool, replays: u32) -> bool {
    // A failed request yields to the final turn once a soft stop is pending.
    // The final attempt itself must not be replayed either.
    if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        return false;
    }
    let r = st.settings.retry();
    e.code == oxa_proto::codes::STREAM_BROKEN
        && got_data
        && !looped
        && !e.message.starts_with(oxa_proto::CUT_BY_REFUSAL)
        && st.auto_retry
        && r.enabled
        && replays < r.max_retries
        && !st.shared.cancel.is_cancelled()
}

fn length_recoverable(st: &SessionState, attempts: u32) -> bool {
    let policy = st.settings.retry();
    st.auto_retry && policy.enabled && attempts < policy.max_retries && !st.shared.cancel.is_cancelled()
}

fn failure_why(e: &OxaError, got_data: bool) -> &'static str {
    match e.code {
        oxa_proto::codes::ABORTED => shown_why::ABORTED,
        oxa_proto::codes::STREAM_BROKEN if got_data => shown_why::CUT,
        _ => shown_why::FAILED,
    }
}

/// The messages sent after the wire for one request and never stored: the output schema
/// contract (with its repair feedback) and, once, the verified-artifact line of this turn
/// (#2295, `done.rs`), taken here so no later reply repeats it.
fn turn_extra(
    st: &SessionState,
    artifacts: &mut Option<String>,
    schema_repaired: &Option<crate::output_schema::OutputValidationError>,
) -> Vec<WireMsg> {
    let mut extra: Vec<WireMsg> = st
        .output_schema
        .as_ref()
        .map(|schema| {
            let instruction = if let Some(error) = schema_repaired {
                format!("Your final answer did not match the output schema: {error}. Return only valid JSON matching this schema:")
            } else {
                "Your final answer must be JSON matching this schema:".to_owned()
            };
            vec![WireMsg::user(format!("{instruction}\n{}", schema.value()))]
        })
        .unwrap_or_default();
    extra.extend(artifacts.take().map(WireMsg::user));
    extra
}

/// The placeholder of the assistant message a request is about to stream.
pub(crate) fn pending_message() -> EventMessage {
    EventMessage {
        msg: WireMsg::assistant(""),
        ts_ms: now_ms(),
        stop: Some(StopReason::Pending),
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

/// Records the text of a streaming reply so an aborted or cut reply can be shown. A new
/// `MessageStart` (the retry after a context-full cut, F6) starts over.
struct Capture<'a> {
    inner: &'a mut dyn EventSink,
    text: String,
    got_data: bool,
    detector: crate::loopguard::LoopDetector,
    /// #513: a loop inside the reasoning is stopped the same way (kept apart from the answer).
    reasoning_detector: crate::loopguard::LoopDetector,
    cancel: oxa_proto::CancelToken,
    looped: bool,
}

impl EventSink for Capture<'_> {
    fn emit(&mut self, e: &AgentEvent) {
        if let AgentEvent::MessageStart { .. } = e {
            self.text.clear();
            self.got_data = false;
            self.detector.reset();
            self.reasoning_detector.reset();
            self.looped = false;
        }
        if let AgentEvent::MessageUpdate { delta, .. } = e {
            self.got_data = true;
            match delta {
                oxa_proto::MessageDelta::TextDelta { delta, .. } => {
                    self.text.push_str(delta);
                    if !self.looped && self.detector.push(delta) {
                        self.looped = true;
                        self.cancel.cancel();
                    }
                }
                oxa_proto::MessageDelta::ThinkingDelta { delta, .. }
                    if !self.looped && self.reasoning_detector.push(delta) =>
                {
                    self.looped = true;
                    self.cancel.cancel();
                }
                _ => {}
            }
        }
        self.inner.emit(e);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Output cap of the wrap-up turn: the owner's `oxa.run.wrapUpTokens`, else the model's own
/// output limit, at most 4096 (room to commit and report).
fn wrap_up_tokens(st: &SessionState) -> u32 {
    let own = st.settings.user_u32("oxa.run.wrapUpTokens", 0);
    if own > 0 {
        return own;
    }
    match st.info.max_output {
        0 => 4096,
        limit => limit.min(4096),
    }
}
