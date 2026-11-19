//! O-152: `oxa report-issue` (architect-owned), the code behind the built-in `report-issue`
//! skill. `draft` collects scrubbed facts and saves a draft; `submit` shows the exact text
//! and posts it with `gh issue create` only after the user typed `yes` on the controlling
//! terminal. Reporting is off until `oxa.reportIssue.repo` names the public repository.
//!
//! What a draft may contain: the user's description (scrubbed, capped, no transcript records),
//! the identity facts (version, machine, OS) and the session's own model with its limits and
//! the engine selection (#1553). Every fact is read in-process: a draft never runs a
//! controller CLI (`oxc`, `local-ai`) and never makes a request. Never prompts, transcripts,
//! file contents, emails, home paths, user or host names, IP addresses or secrets.
mod pushed;
mod semantic;
pub(crate) use pushed::PushScan;

use oxa_config::{Dirs, Env, ModelsConfig, Settings};
use oxa_proto::{codes, OxaError};
use oxa_resources::{read_identity, write_atomic};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const USAGE: &str =
    "usage: oxa report-issue draft --title TITLE [--notes-file FILE | --notes TEXT] | oxa report-issue submit DRAFT";

/// The setting that names the target repository (`OWNER/NAME`); unset = reporting is off.
pub const SETTING_REPORT_REPO: &str = "oxa.reportIssue.repo";

/// What `submit` says while no repository is configured.
pub const REPORT_DISABLED: &str = "Issue reporting is off: the public ox-llm repository does not exist yet. \
Nothing was posted. The draft stays where it is; you can file it by hand, or set \
`oxa.reportIssue.repo` (OWNER/NAME) in ~/.oxa/agent/settings.json once the repository exists.";

/// The longest user description kept in a draft (characters).
pub const MAX_NOTES_CHARS: usize = 2000;
/// The longest section kept in a draft or in the semantic judge's input (characters).
pub const MAX_SECTION_CHARS: usize = 4000;

/// Runs a program: `(program, args)` -> `Some((exit code, stdout))`, `None` when it could not
/// run (missing, failed to start, timed out).
pub type Runner<'a> = &'a mut dyn FnMut(&str, &[String]) -> Option<(i32, String)>;

/// Asks the user: the question -> `Some(true)` only for an explicit `yes`, `Some(false)` for
/// anything else, `None` when there is nobody to ask (no terminal).
pub type Confirm<'a> = &'a mut dyn FnMut(&str) -> Option<bool>;

/// What the scrubber replaces besides the generic patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubCtx {
    /// The home directory (replaced by `~`).
    pub home: Option<String>,
    /// The user's login name (replaced by `<user>`).
    pub user: Option<String>,
    /// The host name (replaced by `<host>`).
    pub host: Option<String>,
}

impl ScrubCtx {
    /// From the environment (`HOME`, `USER` / `LOGNAME`) and `/etc/hostname`.
    pub fn from_env(env: &Env, dirs: &Dirs) -> ScrubCtx {
        let host = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());
        ScrubCtx {
            home: Some(dirs.home.to_string_lossy().into_owned()),
            user: env.get("USER").or_else(|| env.get("LOGNAME")).map(str::to_string),
            host,
        }
    }
}

/// Use the existing native worker privacy policy for outbound advisor diagnostics.
pub fn advisor_scrubber(ctx: ScrubCtx) -> oxa_core::advice::AdviceScrubber {
    std::sync::Arc::new(move |text| scrub(text, &ctx))
}

struct Patterns {
    pem: Regex,
    pem_begin: Regex,
    url_cred: Regex,
    known: Regex,
    bearer: Regex,
    keyed: Regex,
    email: Regex,
    home: Regex,
    ipv4: Regex,
    tailnet: Regex,
    long: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("scrub pattern");
        Patterns {
            pem: re(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
            pem_begin: re(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----"),
            url_cred: re(r"://[^/\s:@]+:[^/\s@]+@"),
            known: re(concat!(
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{16,}",
                r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}",
                r"|glpat-[A-Za-z0-9_-]{20,}|hf_[A-Za-z0-9]{30,})"
            )),
            bearer: re(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}"),
            keyed: re(concat!(
                r#"(?i)("?\b(?:api[_-]?key|apikey|access[_-]?token|auth[_-]?token|refresh[_-]?token|token"#,
                r#"|secret|client[_-]?secret|password|passwd|passphrase|authorization|share[_-]?key|key"#,
                // O-169: the Brave Search key variable (`\b` does not split `SEARCH_API_KEY`).
                r#"|brave[_-]?search[_-]?api[_-]?key|x-subscription-token)"#,
                r#""?\s*[:=]\s*"?)([^\s"',;}]+)"#
            )),
            email: re(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
            home: re(r"/(?:var/)?home/[^/\s\x22':]+|/Users/[^/\s\x22':]+|/root\b"),
            ipv4: re(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
            tailnet: re(r"\b[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.ts\.net\b"),
            long: re(r"[A-Za-z0-9_+/=-]{32,}"),
        }
    })
}

/// Remove what must never leave the machine: private keys, credentials in URLs, known token
/// formats, `Bearer ..`, values of secret-named keys (`token=`, `"password": ..`), long
/// mixed-case random strings, emails, home paths (`~`), the user and host names, IPv4
/// addresses other than loopback and tailnet host names. Idempotent.
pub fn scrub(text: &str, ctx: &ScrubCtx) -> String {
    let p = patterns();
    let mut s = p.pem.replace_all(text, "<secret>").into_owned();
    s = p.url_cred.replace_all(&s, "://<secret>@").into_owned();
    s = p.known.replace_all(&s, "<secret>").into_owned();
    s = p.bearer.replace_all(&s, "$1 <secret>").into_owned();
    s = p
        .keyed
        .replace_all(&s, |c: &regex::Captures| {
            // Already scrubbed, or an auth scheme whose credential the `bearer` rule took.
            let v = &c[2];
            if v == "<secret>" || v.eq_ignore_ascii_case("bearer") || v.eq_ignore_ascii_case("basic") {
                c[0].to_string()
            } else {
                format!("{}<secret>", &c[1])
            }
        })
        .into_owned();
    s = p
        .long
        .replace_all(&s, |c: &regex::Captures| {
            let t = &c[0];
            let mixed = t.chars().any(|ch| ch.is_ascii_uppercase())
                && t.chars().any(|ch| ch.is_ascii_lowercase())
                && t.chars().any(|ch| ch.is_ascii_digit());
            if mixed {
                "<secret>".to_string()
            } else {
                t.to_string()
            }
        })
        .into_owned();
    s = p.email.replace_all(&s, "<email>").into_owned();
    if let Some(home) = ctx.home.as_deref().filter(|h| h.len() > 1) {
        s = s.replace(home.trim_end_matches('/'), "~");
    }
    s = p.home.replace_all(&s, "~").into_owned();
    s = p.tailnet.replace_all(&s, "<host>").into_owned();
    s = p
        .ipv4
        .replace_all(&s, |c: &regex::Captures| {
            let ip = &c[0];
            if ip.starts_with("127.") || ip == "0.0.0.0" {
                ip.to_string()
            } else {
                "<ip>".to_string()
            }
        })
        .into_owned();
    for (name, mark) in [(&ctx.host, "<host>"), (&ctx.user, "<user>")] {
        // A name that is part of a marker (`<user>`, `<secret>`, ..) would break idempotence.
        let marker_word = |n: &str| ["user", "host", "secret", "email"].contains(&n.to_lowercase().as_str());
        if let Some(n) = name.as_deref().filter(|n| n.len() >= 3 && !marker_word(n)) {
            let word = Regex::new(&format!(r"(?i)\b{}\b", regex::escape(n))).expect("name pattern");
            s = word.replace_all(&s, mark).into_owned();
        }
    }
    s
}

/// The user's description as it may enter a draft: lines that look like transcript or
/// session records dropped, capped at [`MAX_NOTES_CHARS`], scrubbed.
pub fn clean_notes(notes: &str, ctx: &ScrubCtx) -> String {
    let record =
        Regex::new(r#""(?:role|content|messages|toolCallId|parentId)"\s*:"#).expect("record pattern");
    let kept: Vec<&str> = notes.lines().filter(|l| !record.is_match(l)).collect();
    let mut text = kept.join("\n").trim().to_string();
    if text.chars().count() > MAX_NOTES_CHARS {
        text = text.chars().take(MAX_NOTES_CHARS).collect::<String>() + " [cut]";
    }
    scrub(&text, ctx)
}

/// What the draft says about this run: the model of the session that drafted it and the
/// engine selection. Every fact is read in-process (#1553): a draft never runs a controller
/// CLI and never makes a request, so a Rust-only install drafts the same report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFacts {
    /// `OXA_MODEL`: the session's model, the only model a draft names. `None` when the draft
    /// was not made from a session (there is then no model to report).
    pub model: Option<String>,
    /// The default endpoint's name (`oxe`), the provider of a model id without a prefix.
    pub endpoint: String,
    /// How this run reaches its models, from the user-only `oxa.engine.*` selection.
    pub engine: String,
    /// The process's hard offline switch (`OXA_OFFLINE`, `--offline`, `oxa.offline`).
    pub offline: bool,
}

impl RunFacts {
    /// The facts of this process: `OXA_MODEL` from the environment and the engine selection
    /// from the user settings (a project file cannot choose it). No I/O beyond reading the
    /// settings and models files.
    pub fn new(dirs: &Dirs, env: &Env, settings: &Settings) -> RunFacts {
        let (models, _) = ModelsConfig::load(dirs, env);
        let cloud = settings.bool(crate::in_process::SETTING, false);
        let local = settings.bool(oxc_local::LOCAL_SETTING, false);
        let engine = match (cloud, local) {
            (true, true) => "in-process cloud gateway and local engine".to_string(),
            (true, false) => "in-process cloud gateway".to_string(),
            (false, true) => "in-process local engine".to_string(),
            (false, false) => format!("endpoint `{}`", models.default),
        };
        RunFacts {
            model: env
                .get("OXA_MODEL")
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string),
            endpoint: models.default,
            engine,
            offline: oxa_proto::offline(),
        }
    }
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_SECTION_CHARS {
        text.to_string()
    } else {
        text.chars().take(MAX_SECTION_CHARS).collect::<String>() + "\n[cut]"
    }
}

/// The context and output limits a model has in the installed identity's model list, which
/// `oxa identity --refresh` wrote from the engine. `(None, None)` when the model is not listed.
fn identity_limits(identity: &str, model: &str) -> (Option<u32>, Option<u32>) {
    let number = |limits: &str, label: &str| {
        limits
            .split_once(label)?
            .1
            .split_whitespace()
            .next()?
            .parse::<u32>()
            .ok()
    };
    for line in identity.lines() {
        let Some(entry) = line.trim_start().strip_prefix("- ") else {
            continue;
        };
        let (name, limits) = match entry.split_once(": ") {
            Some((name, limits)) => (name, limits),
            None => (entry, ""),
        };
        if name != model {
            continue;
        }
        return (number(limits, "context "), number(limits, "output up to "));
    }
    (None, None)
}

/// The provider of a model id, as the model list's provider column names it: the id's prefix
/// (`<provider>/<name>`), else the default endpoint (`oxe`) when the id has none.
fn model_provider(model: &str, endpoint: &str) -> String {
    model
        .split_once('/')
        .filter(|(provider, name)| !provider.is_empty() && !name.is_empty())
        .map_or_else(|| endpoint.to_string(), |(provider, _)| provider.to_string())
}

/// The session section: the session's model with its provider and limits (when known), then
/// the engine selection and the offline switch.
fn session_section(facts: &RunFacts, identity: Option<&str>) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(model) = facts.model.as_deref() {
        lines.push(format!("- model: {model}"));
        lines.push(format!("- provider: {}", model_provider(model, &facts.endpoint)));
        let (context, max_output) = identity
            .map(|identity| identity_limits(identity, model))
            .unwrap_or((None, None));
        if let Some(tokens) = context {
            lines.push(format!("- context: {tokens} tokens"));
        }
        if let Some(tokens) = max_output {
            lines.push(format!("- output: up to {tokens} tokens"));
        }
    }
    lines.push(format!("- engine: {}", facts.engine));
    lines.push(format!("- offline: {}", if facts.offline { "yes" } else { "no" }));
    lines.join("\n")
}

/// The body of an issue: the description, the identity facts and this session's model and
/// engine state. Every part is scrubbed.
pub fn draft_body(notes: &str, dirs: &Dirs, ctx: &ScrubCtx, facts: &RunFacts) -> String {
    let mut parts: Vec<String> = Vec::new();
    let notes = clean_notes(notes, ctx);
    parts.push(format!("## What happened\n\n{}", notes));
    let identity = read_identity(dirs);
    let mut system: Vec<String> = vec![format!("- oxa version: {}", env!("CARGO_PKG_VERSION"))];
    if let Some(identity) = identity.as_deref() {
        system.extend(
            identity
                .lines()
                .filter(|l| {
                    l.starts_with("- ") && !l.starts_with("- oxa version") && !l.starts_with("- Models")
                })
                .map(str::to_string),
        );
    } else {
        system.push("- identity: not installed".to_string());
    }
    parts.push(format!("## System\n\n{}", scrub(&system.join("\n"), ctx)));
    parts.push(format!(
        "## Session\n\n{}",
        scrub(&session_section(facts, identity.as_deref()), ctx)
    ));
    parts.push(
        "Drafted by `oxa report-issue`: scrubbed of emails, home paths, user and host names, IP addresses and secrets; no prompts or transcripts."
            .to_string(),
    );
    parts.join("\n\n")
}

/// The draft file: `Title: <title>`, a blank line, the body.
fn draft_text(title: &str, body: &str) -> String {
    format!("Title: {title}\n\n{body}\n")
}

/// `(title, body)` of a draft file's text.
pub fn parse_draft(text: &str) -> Option<(String, String)> {
    let (first, rest) = text.split_once('\n')?;
    let title = first.strip_prefix("Title: ")?.trim().to_string();
    let body = rest.trim_matches('\n').to_string();
    (!title.is_empty() && !body.is_empty()).then_some((title, body))
}

/// Save a draft under `<agent dir>/issue-drafts/` (named by its content hash). Returns the
/// path and the exact `(title, body)` saved.
pub fn save_draft(
    title: &str,
    notes: &str,
    dirs: &Dirs,
    ctx: &ScrubCtx,
    facts: &RunFacts,
) -> Result<(PathBuf, String, String), OxaError> {
    let title = scrub(title.trim(), ctx).replace('\n', " ");
    if title.is_empty() {
        return Err(OxaError::new(codes::USAGE, "a title is required").with_hint(USAGE));
    }
    if clean_notes(notes, ctx).is_empty() {
        return Err(OxaError::new(
            codes::USAGE,
            "no description: the notes are empty, so there is nothing to report; write what happened and draft again",
        )
        .with_hint(USAGE));
    }
    let body = draft_body(notes, dirs, ctx, facts);
    let text = draft_text(&title, &body);
    let hash = Sha256::digest(text.as_bytes());
    let name: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let path = dirs
        .agent_dir
        .join("issue-drafts")
        .join(format!("issue-{name}.md"));
    write_atomic(&path, text.as_bytes())
        .map_err(|e| OxaError::io(&format!("cannot write the draft {}", path.display()), &e))?;
    Ok((path, title, body))
}

/// `OWNER/NAME` from the settings; `None` = reporting is off.
pub fn report_repo(settings: &Settings) -> Option<String> {
    let repo = settings.string(SETTING_REPORT_REPO)?;
    let valid = Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("repo pattern");
    valid.is_match(repo.trim()).then(|| repo.trim().to_string())
}

/// Post a saved draft. Order: read the draft; refuse one with text the scrubber would still
/// change; refuse while no repository is configured ([`REPORT_DISABLED`]); print the exact
/// title and body and ask `confirm` before a GitHub write. Semantic mode also
/// verifies the report against main and routes duplicates to comments. Exit codes:
/// 0 posted, 1 kept locally, 3 reporting is off.
pub fn submit(
    draft: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let storage = draft
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("report-ledger");
    submit_with_storage(draft, &storage, settings, ctx, confirm, runner, out)
}

pub(super) fn submit_with_storage(
    draft: &Path,
    storage: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    Ok(
        match submit_record(draft, storage, settings, ctx, confirm, runner, out)? {
            Submission::Created(_) | Submission::Commented(_) => 0,
            Submission::Local(_) => 1,
            Submission::Disabled => 3,
        },
    )
}

pub(crate) enum Submission {
    Created(String),
    Commented(String),
    Local(String),
    Disabled,
}

pub(crate) fn semantic_reporting(settings: &Settings) -> bool {
    settings.bool(semantic::SETTING, false)
}

pub(crate) fn report_ledger(dirs: &Dirs) -> PathBuf {
    dirs.agent_dir.join("issue-drafts/report-ledger")
}

pub(crate) use semantic::judge as judge_report;

pub(crate) fn submit_record(
    draft: &Path,
    storage: &Path,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
    out: &mut dyn Write,
) -> Result<Submission, OxaError> {
    oxa_proto::check_online("posting the issue with `gh`")?; // P4
    let w = |e: std::io::Error| OxaError::io("cannot write to the terminal", &e);
    let text = std::fs::read_to_string(draft)
        .map_err(|e| OxaError::io(&format!("cannot read the draft {}", draft.display()), &e))?;
    let (title, body) = parse_draft(&text).ok_or_else(|| {
        OxaError::new(
            codes::INVALID,
            "not an oxa issue draft (expected `Title: ..`, a blank line, the body)",
        )
    })?;
    let control = |text: &str| {
        text.chars().any(|ch| (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
    };
    if scrub(&title, ctx) != title || scrub(&body, ctx) != body || control(&title) || control(&body) {
        writeln!(
            out,
            "Not posted: the draft contains text the scrubber removes (an email, a home path, a name or a secret). Run `oxa report-issue draft` again."
        )
        .map_err(w)?;
        return Ok(Submission::Local("draft needs scrubbing".into()));
    }
    let Some(repo) = report_repo(settings) else {
        writeln!(out, "{REPORT_DISABLED}\nDraft: {}", draft.display()).map_err(w)?;
        return Ok(Submission::Disabled);
    };
    let mut publication = if semantic_reporting(settings) {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        let daily_limit = semantic::daily_new(settings);
        match semantic::plan(storage, &repo, (&title, &body), ctx, daily_limit, confirm, runner) {
            Ok(publication) => Some(publication),
            Err(reason) => {
                writeln!(out, "Kept locally at {} ({reason}).", draft.display()).map_err(w)?;
                return Ok(Submission::Local(reason.into()));
            }
        }
    } else {
        writeln!(out, "Repository: {repo}\nTitle: {title}\n\n{body}\n").map_err(w)?;
        match confirm(&format!("Post this issue publicly to {repo}? Type yes to post: ")) {
            Some(true) => {}
            Some(false) => {
                writeln!(out, "Not posted.").map_err(w)?;
                return Ok(Submission::Local("publication was declined".into()));
            }
            None => {
                writeln!(
                    out,
                    "Not posted: confirmation needs a terminal. Run `oxa report-issue submit {}` yourself in a terminal.",
                    draft.display()
                )
                .map_err(w)?;
                return Ok(Submission::Local(
                    "publication requires confirmation through the controlling terminal".into(),
                ));
            }
        }
        None
    };
    let body_file = draft.with_extension("body.md");
    let body = publication
        .as_ref()
        .map_or(body.as_str(), |plan| plan.body.as_str());
    let issue = publication.as_ref().and_then(|plan| plan.issue);
    let number;
    let mut args = if let Some(issue) = issue {
        if confirm(&format!(
            "Comment on {repo}#{issue}?\n\n{body}\n\nType yes to post: "
        )) != Some(true)
        {
            writeln!(out, "Comment retained locally at {}.", draft.display()).map_err(w)?;
            return Ok(Submission::Local("comment requires confirmation".into()));
        }
        number = issue.to_string();
        vec!["issue", "comment", &number, "--repo", &repo]
    } else {
        vec!["issue", "create", "--repo", &repo, "--title", &title]
    };
    let body_path = body_file.to_string_lossy();
    args.extend(["--body-file", &body_path]);
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    write_atomic(&body_file, body.as_bytes())
        .map_err(|e| OxaError::io(&format!("cannot write {}", body_file.display()), &e))?;
    let result = runner("gh", &args);
    let _ = std::fs::remove_file(&body_file);
    match result {
        Some((0, stdout)) => {
            if let Some(plan) = &mut publication {
                if plan.record_created(&repo, &stdout).is_err() {
                    writeln!(out, "Posted, but the local issue receipt could not be saved.").map_err(w)?;
                }
            }
            writeln!(out, "Posted: {}", stdout.trim()).map_err(w)?;
            let url = stdout.trim().to_owned();
            Ok(if issue.is_some() {
                Submission::Commented(url)
            } else {
                Submission::Created(url)
            })
        }
        Some((code, _)) => {
            writeln!(
                out,
                "GitHub did not confirm the write (exit {code}). Draft retained at {}.",
                draft.display()
            )
            .map_err(w)?;
            Ok(Submission::Local(format!(
                "GitHub did not confirm the write (exit {code})"
            )))
        }
        None => {
            writeln!(out, "Not posted: the GitHub CLI `gh` could not run.").map_err(w)?;
            Ok(Submission::Local("GitHub CLI could not run".into()))
        }
    }
}

/// The real confirmation: the question and the answer go through `/dev/tty` (never stdin, so
/// piped input cannot confirm). `None` without a controlling terminal, or when oxa's own tools
/// run this (`OXA_CODING_AGENT` is set: the model must not confirm for the user).
pub(crate) fn tty_confirm(env: &Env) -> impl FnMut(&str) -> Option<bool> + '_ {
    move |question: &str| {
        if env.get("OXA_CODING_AGENT").is_some() {
            return None;
        }
        // Keep concurrent workers' questions and answers together.
        static TERMINAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _terminal = TERMINAL.lock().ok()?;
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        tty.write_all(question.as_bytes()).ok()?;
        tty.flush().ok()?;
        let mut line = String::new();
        std::io::BufReader::new(tty).read_line(&mut line).ok()?;
        Some(line.trim().eq_ignore_ascii_case("yes"))
    }
}

use oxa_work::{command_runner, COMMAND_DEADLINE};

/// The real runner: the program's stdout, killed after 30 s. The program is resolved through
/// the trusted host-tool lookup (`PATH` from `env`, never inside `cwd` or a group- or
/// world-writable directory), the same lookup `oxa work` uses for `gh`.
pub fn process_runner<'a>(
    env: &'a Env,
    cwd: &'a Path,
    home: &'a Path,
) -> impl FnMut(&str, &[String]) -> Option<(i32, String)> + 'a {
    move |program: &str, args: &[String]| {
        let path = env.get("PATH").map(std::ffi::OsStr::new);
        let Some(exe) = crate::netguard::trusted_executable_for(program, path, Some(cwd), Some(home)) else {
            if path.is_some_and(|p| std::env::split_paths(p).any(|d| d.join(program).is_file())) {
                eprintln!("{}", untrusted_tool(program));
            }
            return None;
        };
        let mut command = std::process::Command::new(exe);
        command.args(args);
        command_runner(command, None, COMMAND_DEADLINE)
    }
}

/// Why a host tool on `PATH` was not run.
fn untrusted_tool(program: &str) -> String {
    format!(
        "oxa: `{program}` was not run: the `{program}` on PATH is not in a trusted location \
(it is inside the project or in a group- or world-writable directory). Install `{program}` \
system-wide or put it in a trusted PATH entry outside the project (for example ~/.local/bin)."
    )
}

/// `oxa report-issue draft|submit ..`. Returns the exit code.
pub fn run_report_issue(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let usage = |m: &str| OxaError::new(codes::USAGE, m.to_string()).with_hint(USAGE);
    let ctx = ScrubCtx::from_env(env, dirs);
    let mut runner = process_runner(env, cwd, &dirs.home);
    match words.first().map(String::as_str) {
        Some("draft") => {
            let mut title: Option<String> = None;
            let mut notes = String::new();
            let mut i = 1;
            while i < words.len() {
                let next = words.get(i + 1).cloned();
                match words[i].as_str() {
                    "--title" => title = Some(next.ok_or_else(|| usage("--title needs a value"))?),
                    "--notes" => notes = next.ok_or_else(|| usage("--notes needs a value"))?,
                    "--notes-file" => {
                        let f = next.ok_or_else(|| usage("--notes-file needs a value"))?;
                        let p = if Path::new(&f).is_absolute() {
                            PathBuf::from(&f)
                        } else {
                            cwd.join(&f)
                        };
                        notes = std::fs::read_to_string(&p)
                            .map_err(|e| OxaError::io(&format!("cannot read {}", p.display()), &e))?;
                    }
                    other => return Err(usage(&format!("unknown report-issue option: {other}"))),
                }
                i += 2;
            }
            let title = title.ok_or_else(|| usage("a title is required (--title)"))?;
            let facts = RunFacts::new(dirs, env, &Settings::load(dirs, cwd, false));
            let (path, title, body) = save_draft(&title, &notes, dirs, &ctx, &facts)?;
            let w = |e: std::io::Error| OxaError::io("cannot write the draft", &e);
            writeln!(out, "Draft: {}\n\nTitle: {title}\n\n{body}\n", path.display()).map_err(w)?;
            writeln!(
                out,
                "Nothing was posted. To post it, run `oxa report-issue submit {}` in a terminal.",
                path.display()
            )
            .map_err(w)?;
            Ok(0)
        }
        Some("submit") => {
            let [_, draft] = words else {
                return Err(usage("submit takes the draft path"));
            };
            let settings = Settings::load(dirs, cwd, false);
            let mut confirm = tty_confirm(env);
            let mut submit_runner = |program: &str, args: &[String]| {
                if program == "oxa" {
                    return semantic::judge(args.first()?, dirs, &settings, env).map(|reply| (0, reply));
                }
                if semantic_reporting(&settings) {
                    let project = cwd
                        .ancestors()
                        .find(|path| path.join(".git").exists())
                        .unwrap_or(cwd);
                    let gh = crate::netguard::trusted_executable_for(
                        "gh",
                        env.get("PATH").map(OsStr::new),
                        Some(project),
                        Some(&dirs.home),
                    )?;
                    let mut command = std::process::Command::new(gh);
                    command
                        .args(args)
                        .current_dir(&dirs.home)
                        .env("GH_HOST", "github.com");
                    command_runner(command, Some(1_048_576), COMMAND_DEADLINE)
                } else {
                    runner(program, args)
                }
            };
            let draft = cwd.join(draft);
            submit_with_storage(
                &draft,
                &report_ledger(dirs),
                &settings,
                &ctx,
                &mut confirm,
                &mut submit_runner,
                out,
            )
        }
        _ => Err(usage("report-issue needs `draft` or `submit`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use serde_json::json;

    #[test]
    fn identity_limits_read_the_installed_model_list() {
        let identity = "- Models (one is loaded at a time):\n  - moe: context 131072 tokens, output up to 8192 tokens\n  - opencode/big: context 1000000 tokens, output up to 384000 tokens\n  - bare\n";
        assert_eq!(identity_limits(identity, "moe"), (Some(131_072), Some(8_192)));
        assert_eq!(
            identity_limits(identity, "opencode/big"),
            (Some(1_000_000), Some(384_000))
        );
        assert_eq!(identity_limits(identity, "bare"), (None, None));
        assert_eq!(identity_limits(identity, "absent"), (None, None));
    }

    #[test]
    fn the_provider_is_the_id_prefix_else_the_endpoint() {
        assert_eq!(model_provider("opencode/deepseek", "oxe"), "opencode");
        assert_eq!(model_provider("moe", "oxe"), "oxe");
        assert_eq!(model_provider("openrouter/vendor/name", "oxe"), "openrouter");
    }

    #[test]
    fn the_engine_word_follows_the_user_selection() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let vars = Env::from_pairs(&[]);
        let cases = [
            (Settings::default(), "endpoint `oxe`"),
            (
                Settings::from_values(json!({"oxa": {"engine": {"inProcessCloud": true}}}), json!({})),
                "in-process cloud gateway",
            ),
            (
                Settings::from_values(json!({"oxa": {"engine": {"inProcessLocal": true}}}), json!({})),
                "in-process local engine",
            ),
            (
                Settings::from_values(
                    json!({"oxa": {"engine": {"inProcessCloud": true, "inProcessLocal": true}}}),
                    json!({}),
                ),
                "in-process cloud gateway and local engine",
            ),
        ];
        for (settings, word) in cases {
            assert_eq!(RunFacts::new(&dirs, &vars, &settings).engine, word);
        }
    }
}
