//! Which model a worker runs: the owner's policy (`oxa.workers.models`, user settings only) by job
//! kind, the escalation after repeated failures, and the reason recorded for each choice.
//!
//! No model id is built in: the lists are the owner's. With no policy the worker inherits the
//! session model, exactly as before. The function [`choose`] is pure; the caller builds the
//! history from the session's registry records.
use super::launch::{check_value, valid_name};
use super::registry::WorkerState;
use serde_json::Value;
use std::collections::BTreeMap;

/// Setting: the model policy object (user settings only).
pub const SETTING_MODELS: &str = "oxa.workers.models";
/// The default for `escalateAfter`.
const ESCALATE_AFTER: usize = 2;
/// The longest `reason` text, in characters.
pub const MAX_REASON: usize = 300;

/// The owner's model choice policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPolicy {
    /// Model ids for any job without a row of its own kind.
    pub default: Vec<String>,
    /// Model ids per job kind.
    pub kinds: BTreeMap<String, Vec<String>>,
    /// Failed attempts on one job after which the escalation list is used.
    pub escalate_after: usize,
    /// Model ids to move to after `escalate_after` failures.
    pub escalate_to: Vec<String>,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        ModelPolicy {
            default: Vec::new(),
            kinds: BTreeMap::new(),
            escalate_after: ESCALATE_AFTER,
            escalate_to: Vec::new(),
        }
    }
}

/// One earlier worker of this session, as the choice sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub model: String,
    pub kind: String,
    /// The attempt belongs to the job being started.
    pub job_matches: bool,
    pub failed: bool,
}

/// The chosen model and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub model: String,
    pub why: String,
}

/// A job kind: lowercase letters, digits and dashes, at most 32 characters.
pub fn valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 32
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A job key: the same rule as a worker name.
pub fn valid_job(job: &str) -> bool {
    valid_name(job)
}

/// One line of at most [`MAX_REASON`] characters without control characters.
pub fn check_reason(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.chars().count() > MAX_REASON {
        return Err(format!("reason is longer than {MAX_REASON} characters"));
    }
    if text.chars().any(char::is_control) {
        return Err("reason must be one line without control characters".to_string());
    }
    Ok(text.to_string())
}

/// Whether an earlier worker counts as a failed attempt: the caller's recorded result wins; else a
/// dead process, or a finished outcome other than done / nothing_to_do / publication_pending. A
/// stop does not count,
/// whether the record is marked `stopped` (the child may have written `finished / interrupted`
/// since, which wins over the marker in the state) or the outcome is `interrupted` (a SIGINT from a
/// stop or the parent's shutdown: a resume case, not a model result).
pub fn failed(result: &str, state: &WorkerState, stopped: bool) -> bool {
    match result {
        "failed" => true,
        "ok" => false,
        _ if stopped => false,
        _ => match state {
            WorkerState::Dead => true,
            WorkerState::Finished(outcome) => {
                // A pending publication is accepted work awaiting its push, not a failed attempt.
                !matches!(
                    outcome.as_str(),
                    "done" | "nothing_to_do" | "publication_pending" | "interrupted"
                )
            }
            _ => false,
        },
    }
}

fn model_list(value: Option<&Value>, key: &str, notices: &mut Vec<String>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        notices.push(format!(
            "{SETTING_MODELS}.{key}: not a list of model ids, ignored"
        ));
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        match item.as_str() {
            Some(id) if check_value("model", id).is_ok() => {
                if !out.iter().any(|seen| seen.as_str() == id) {
                    out.push(id.to_string());
                }
            }
            _ => notices.push(format!(
                "{SETTING_MODELS}.{key}: an entry is not a valid model id, dropped"
            )),
        }
    }
    out
}

/// The policy from a settings value. Malformed parts are dropped and named in the notices; this
/// never fails.
pub fn parse(value: Option<&Value>) -> (ModelPolicy, Vec<String>) {
    let mut policy = ModelPolicy::default();
    let mut notices = Vec::new();
    let Some(value) = value else {
        return (policy, notices);
    };
    let Some(object) = value.as_object() else {
        notices.push(format!("{SETTING_MODELS}: not an object, ignored"));
        return (policy, notices);
    };
    policy.default = model_list(object.get("default"), "default", &mut notices);
    policy.escalate_to = model_list(object.get("escalateTo"), "escalateTo", &mut notices);
    match object.get("kinds") {
        None => {}
        Some(Value::Object(kinds)) => {
            for (kind, list) in kinds {
                if !valid_kind(kind) {
                    notices.push(format!("{SETTING_MODELS}.kinds: bad kind name {kind:?}, dropped"));
                    continue;
                }
                let ids = model_list(Some(list), &format!("kinds.{kind}"), &mut notices);
                if !ids.is_empty() {
                    policy.kinds.insert(kind.clone(), ids);
                }
            }
        }
        Some(_) => notices.push(format!("{SETTING_MODELS}.kinds: not an object, ignored")),
    }
    if let Some(after) = object.get("escalateAfter") {
        match after
            .as_u64()
            .filter(|n| *n >= 1)
            .and_then(|n| usize::try_from(n).ok())
        {
            Some(n) => policy.escalate_after = n,
            None => notices.push(format!(
                "{SETTING_MODELS}.escalateAfter: not a whole number of at least 1, using {ESCALATE_AFTER}"
            )),
        }
    }
    (policy, notices)
}

/// The policy from the user settings only (a project file cannot set it), with its notices.
pub fn from_settings(settings: &oxa_config::Settings) -> (ModelPolicy, Vec<String>) {
    parse(settings.user_value(SETTING_MODELS))
}

/// The candidate least used among `attempts` (ties: list order), skipping ids that already failed
/// on this job while another entry is left.
fn least_used<'a>(list: &'a [String], attempts: &[&Attempt]) -> &'a String {
    let failed_here = |id: &String| {
        attempts
            .iter()
            .any(|a| a.job_matches && a.failed && &a.model == id)
    };
    let fresh: Vec<&String> = list.iter().filter(|id| !failed_here(*id)).collect();
    let pool: Vec<&String> = if fresh.is_empty() {
        list.iter().collect()
    } else {
        fresh
    };
    pool.into_iter()
        .min_by_key(|id| attempts.iter().filter(|a| &a.model == *id).count())
        .unwrap_or(&list[0])
}

/// The model for a new worker of job `job`: the explicit id; else the escalation floor after
/// `escalateAfter` failures on this job; else the row of `kind`; else the default list; else the
/// session model.
pub fn choose(
    policy: &ModelPolicy,
    explicit: Option<&str>,
    kind: Option<&str>,
    job: &str,
    session_model: &str,
    history: &[Attempt],
) -> Result<Choice, String> {
    if let Some(model) = explicit {
        return Ok(Choice {
            model: model.to_string(),
            why: "explicit".to_string(),
        });
    }
    let failures: Vec<&Attempt> = history.iter().filter(|a| a.job_matches && a.failed).collect();
    if failures.len() >= policy.escalate_after && !policy.escalate_to.is_empty() {
        let mut failed_models: Vec<&str> = Vec::new();
        for a in &failures {
            if !failed_models.contains(&a.model.as_str()) {
                failed_models.push(&a.model);
            }
        }
        let model = policy
            .escalate_to
            .iter()
            .find(|id| !failed_models.contains(&id.as_str()))
            .unwrap_or(&policy.escalate_to[0]);
        return Ok(Choice {
            model: model.clone(),
            why: format!(
                "escalation: {} failed attempts on {job} ({})",
                failures.len(),
                failed_models.join(", ")
            ),
        });
    }
    if let Some(kind) = kind {
        if let Some(list) = policy.kinds.get(kind) {
            let of_kind: Vec<&Attempt> = history.iter().filter(|a| a.kind == kind).collect();
            return Ok(Choice {
                model: least_used(list, &of_kind).clone(),
                why: format!("policy: kind {kind}"),
            });
        }
    }
    if !policy.default.is_empty() {
        let all: Vec<&Attempt> = history.iter().collect();
        let mut why = "policy: default".to_string();
        if let Some(kind) = kind {
            let known: Vec<&str> = policy.kinds.keys().map(String::as_str).collect();
            why.push_str(&format!("; no row for kind {kind}"));
            if !known.is_empty() {
                why.push_str(&format!("; configured kinds: {}", known.join(", ")));
            }
        }
        return Ok(Choice {
            model: least_used(&policy.default, &all).clone(),
            why,
        });
    }
    if session_model.is_empty() {
        return Err("the session model is not known yet".to_string());
    }
    Ok(Choice {
        model: session_model.to_string(),
        why: "session model".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_settings_are_dropped_not_fatal() {
        let value = json!({
            "default": ["a", 7, "--flag", "b"],
            "kinds": {"review": ["c"], "Bad Kind": ["d"], "tests": "e"},
            "escalateAfter": 0,
            "escalateTo": "s"
        });
        let (policy, notices) = parse(Some(&value));
        assert_eq!(policy.default, vec!["a", "b"]);
        assert_eq!(policy.kinds.len(), 1);
        assert_eq!(policy.kinds["review"], vec!["c"]);
        assert_eq!(policy.escalate_after, 2);
        assert!(policy.escalate_to.is_empty());
        assert!(notices.iter().all(|n| n.contains(SETTING_MODELS)), "{notices:?}");
        assert!(notices.len() >= 4, "{notices:?}");
        let (policy, notices) = parse(Some(&json!("nope")));
        assert_eq!(policy, ModelPolicy::default());
        assert_eq!(notices.len(), 1);
    }

    fn attempt(model: &str, kind: &str, job_matches: bool, failed: bool) -> Attempt {
        Attempt {
            model: model.into(),
            kind: kind.into(),
            job_matches,
            failed,
        }
    }

    #[test]
    fn order_explicit_then_escalation_then_kind_then_default_then_session() {
        let (policy, _) = parse(Some(&json!({
            "default": ["d1"],
            "kinds": {"review": ["a", "b"]},
            "escalateTo": ["s"]
        })));
        let pick = |explicit, kind, history: &[Attempt]| {
            choose(&policy, explicit, kind, "job1", "sess", history).unwrap()
        };
        assert_eq!(pick(Some("x"), Some("review"), &[]).why, "explicit");
        assert_eq!(pick(None, Some("review"), &[]).model, "a");
        let used = [attempt("a", "review", false, false)];
        assert_eq!(pick(None, Some("review"), &used).model, "b");
        let failed = [
            attempt("a", "review", true, true),
            attempt("b", "review", true, true),
        ];
        let c = pick(None, Some("review"), &failed);
        assert_eq!(c.model, "s");
        assert!(c.why.contains("2 failed attempts on job1 (a, b)"), "{}", c.why);
        let other_job = [
            attempt("a", "review", false, true),
            attempt("b", "review", false, true),
        ];
        assert_eq!(pick(None, Some("review"), &other_job).model, "a");
        let c = pick(None, Some("docs"), &[]);
        assert_eq!(c.model, "d1");
        assert!(
            c.why.contains("no row for kind docs") && c.why.contains("review"),
            "{}",
            c.why
        );
        let none = ModelPolicy::default();
        assert_eq!(choose(&none, None, None, "j", "sess", &[]).unwrap().model, "sess");
        assert!(choose(&none, None, None, "j", "", &[]).is_err());
    }

    #[test]
    fn recorded_result_beats_process_state() {
        assert!(failed("failed", &WorkerState::Finished("done".into()), false));
        assert!(!failed("ok", &WorkerState::Dead, false));
        assert!(failed("", &WorkerState::Dead, false));
        assert!(failed("", &WorkerState::Finished("blocked".into()), false));
        assert!(!failed("", &WorkerState::Finished("nothing_to_do".into()), false));
        assert!(!failed("", &WorkerState::Stopped, false));
        // A stop that the child answered with an interrupted finish, or a stopped record that died.
        assert!(!failed("", &WorkerState::Finished("interrupted".into()), false));
        assert!(!failed("", &WorkerState::Finished("blocked".into()), true));
        assert!(!failed("", &WorkerState::Dead, true));
        assert!(failed("failed", &WorkerState::Stopped, true));
    }
}
