//! The report-issue workflow for an explicit user-launched --report-problems run.
//! Normal `report-issue submit` retains its controlling-terminal confirmation.
use crate::report_cmd::{
    self, clean_notes, report_repo, save_draft, scrub, Confirm, RunFacts, Runner, ScrubCtx,
};
use oxa_config::{Dirs, Env, Settings};
use oxa_work::{ProblemNote, Record, ReportRecord};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    path::Path,
    process::{Command, Stdio},
};

pub(crate) fn process(
    program: &str,
    args: &[String],
    cwd: &Path,
    lock: &File,
    gate: &File,
    semantic_github: bool,
) -> Option<(i32, String)> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    if semantic_github {
        command.env("GH_HOST", "github.com");
    }
    oxa_work::inherit_report_lease(&mut command, gate).ok()?;
    oxa_work::command_runner_with_stdin(
        command,
        Some(1_048_576),
        Stdio::from(lock.try_clone().ok()?),
        oxa_work::COMMAND_DEADLINE,
    )
}
fn call(runner: Runner<'_>, args: &[String]) -> Result<String, String> {
    match runner("gh", args) {
        Some((0, text)) => Ok(text),
        _ => Err("GitHub search or write failed; the local draft is retained".into()),
    }
}
fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}
fn visible(text: &str) -> String {
    text.chars().map(|ch| {
        if (ch.is_control() && ch != '\n')
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        { ch.escape_unicode().to_string() } else { ch.to_string() }
    }).collect()
}
pub(crate) fn scrub_note(note: &ProblemNote, ctx: &ScrubCtx) -> Option<ProblemNote> {
    let note = ProblemNote {
        title: visible(&scrub(&note.title, ctx)).chars().take(200).collect(),
        notes: visible(&clean_notes(&note.notes, ctx))
            .chars()
            .take(2000)
            .collect(),
        keywords: note
            .keywords
            .iter()
            .map(|query| visible(&scrub(query, ctx)).chars().take(100).collect())
            .collect(),
    };
    (!note.title.trim().is_empty()
        && !note.notes.trim().is_empty()
        && (2..=3).contains(&note.keywords.len())
        && note
            .keywords
            .iter()
            .all(|query: &String| !query.trim().is_empty()))
    .then_some(note)
}
fn matches(note: &ProblemNote, candidate: &Value) -> bool {
    let title = candidate["title"].as_str().unwrap_or_default().to_lowercase();
    if title == note.title.to_lowercase() {
        return true;
    }
    let all = format!(
        "{title} {}",
        candidate["body"].as_str().unwrap_or_default().to_lowercase()
    );
    let terms: std::collections::HashSet<_> = note
        .keywords
        .iter()
        .flat_map(|q| q.split_whitespace())
        .filter(|word| word.len() >= 2)
        .map(str::to_lowercase)
        .collect();
    terms.len() >= 3 && terms.iter().all(|word| all.contains(word))
}
fn with_report_lease(
    dirs: &Dirs,
    mut run: impl FnMut(&str, &[String], &File) -> Option<(i32, String)>,
    report: impl FnOnce(Runner<'_>) -> Result<ReportRecord, String>,
) -> Result<ReportRecord, String> {
    // Retain the queue's per-user reporting lease across every effect.
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(&dirs.agent_dir).map_err(|e| e.to_string())?;
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dirs.agent_dir.join("work-report.lock"))
        .map_err(|e| e.to_string())?;
    gate.lock().map_err(|e| e.to_string())?;
    report(&mut |program, args| run(program, args, &gate))
}

pub(crate) fn report_semantic(
    note: &ProblemNote,
    origin: (&Record, &Path),
    dirs: &Dirs,
    settings: &Settings,
    ctx: &ScrubCtx,
    confirm: Confirm<'_>,
    run_command: impl FnMut(&str, &[String], &File) -> Option<(i32, String)>,
) -> Result<ReportRecord, String> {
    let (record, storage) = origin;
    let title = visible(&note.title);
    let notes = visible(&note.notes);
    let run = record.session_id.as_deref().unwrap_or_default();

    // Use the protected project and task identity for caps, and the saved native
    // session for occurrence attribution. The local work runner shares one daily
    // allowance across invocations. Worker text supplies only symptom and evidence.
    let mut key = Sha256::new();
    key.update(storage.as_os_str().as_encoded_bytes());
    key.update([0]);
    key.update(record.id.as_bytes());
    let task = format!("{:x}", key.finalize());
    let occurrence = format!("Task: {task}\nWorker: oxa-work\nRun: {run}\n{notes}");
    with_report_lease(dirs, run_command, |runner| {
        let facts = RunFacts::new(dirs, &Env::from_pairs(&[]), settings);
        let (draft, _, _) =
            save_draft(&title, &occurrence, dirs, ctx, &facts).map_err(|err| err.to_string())?;
        let submission = report_cmd::submit_record(
            &draft,
            &report_cmd::report_ledger(dirs),
            settings,
            ctx,
            confirm,
            runner,
            &mut std::io::sink(),
        )
        .map_err(|err| err.to_string())?;
        let (disposition, url, detail) = match submission {
            report_cmd::Submission::Created(url) => ("filed", Some(url), String::new()),
            report_cmd::Submission::Commented(url) => ("existing_issue", Some(url), String::new()),
            report_cmd::Submission::Local(reason) => ("local", None, reason),
            report_cmd::Submission::Disabled => ("local", None, "issue reporting is not configured".into()),
        };
        Ok(ReportRecord {
            disposition: disposition.into(),
            draft,
            url,
            detail,
        })
    })
}

pub(crate) fn report(
    note: &ProblemNote,
    dirs: &Dirs,
    settings: &Settings,
    ctx: &ScrubCtx,
    offline: bool,
    confirm: Confirm<'_>,
    run: impl FnMut(&str, &[String], &File) -> Option<(i32, String)>,
) -> Result<ReportRecord, String> {
    with_report_lease(dirs, run, |runner| {
        report_legacy(note, dirs, settings, ctx, offline, confirm, runner)
    })
}

fn report_legacy(
    note: &ProblemNote,
    dirs: &Dirs,
    settings: &Settings,
    ctx: &ScrubCtx,
    offline: bool,
    confirm: Confirm<'_>,
    runner: Runner<'_>,
) -> Result<ReportRecord, String> {
    let note = &ProblemNote {
        title: visible(&note.title),
        notes: visible(&note.notes),
        keywords: note.keywords.iter().map(|query| visible(query)).collect(),
    };
    let facts = RunFacts::new(dirs, &Env::from_pairs(&[]), settings);
    let (draft, title, body) =
        save_draft(&note.title, &note.notes, dirs, ctx, &facts).map_err(|e| e.to_string())?;
    let mut result = ReportRecord {
        disposition: "local".into(),
        draft,
        url: None,
        detail: String::new(),
    };
    if offline {
        result.detail = "offline: report retained locally".into();
        return Ok(result);
    }
    let Some(repo) = report_repo(settings) else {
        result.detail = "issue reporting is not configured; draft retained locally".into();
        return Ok(result);
    };
    let repo = format!("github.com/{repo}");
    let note = ProblemNote {
        title,
        notes: clean_notes(&note.notes, ctx),
        keywords: note.keywords.iter().map(|q| scrub(q, ctx)).collect(),
    };
    if note.notes.is_empty()
        || !(2..=3).contains(&note.keywords.len())
        || note.keywords.iter().any(|q| q.trim().is_empty())
    {
        result.detail = "report has no safe symptom text or queries; draft retained locally".into();
        return Ok(result);
    }
    let attempt = (|| -> Result<(), String> {
        if confirm(&format!(
            "Search open issues and PRs in {repo} with these exact queries:\n{}\nType yes to send: ",
            note.keywords.join("\n")
        )) != Some(true)
        {
            return Err("search requires controlling-terminal confirmation; draft retained locally".into());
        }
        let mut candidates = vec![];
        for query in &note.keywords {
            for kind in ["issue", "pr"] {
                let text = call(
                    runner,
                    &words(&[
                        kind,
                        "list",
                        "--repo",
                        &repo,
                        "--state",
                        "open",
                        "--search",
                        query,
                        "--limit",
                        "5",
                        "--json",
                        "number,title,url",
                    ]),
                )?;
                let rows: Vec<Value> =
                    serde_json::from_str(&text).map_err(|_| "invalid GitHub search response".to_string())?;
                for row in rows {
                    let number = row["number"].as_u64().ok_or("search match has no number")?;
                    if !candidates.contains(&(kind, number)) {
                        candidates.push((kind, number));
                    }
                }
            }
        }
        // Read all top matches before any write, including PRs which may already fix it.
        let mut read = vec![];
        for (kind, number) in candidates {
            let fields = if kind == "issue" {
                "title,body,url,comments"
            } else {
                "title,body,url"
            };
            let text = call(
                runner,
                &words(&[
                    kind,
                    "view",
                    &number.to_string(),
                    "--repo",
                    &repo,
                    "--json",
                    fields,
                ]),
            )?;
            let value: Value =
                serde_json::from_str(&text).map_err(|_| "invalid GitHub match response".to_string())?;
            if !value.is_object()
                || value["title"].as_str().is_none()
                || value["body"].as_str().is_none()
                || value["url"].as_str().is_none()
            {
                return Err("incomplete GitHub match; draft retained locally".into());
            }
            read.push((kind, number, value));
        }
        if let Some((_, _, value)) = read
            .iter()
            .find(|(kind, _, v)| *kind == "pr" && matches(&note, v))
        {
            result.disposition = "existing_pr".into();
            result.url = value["url"].as_str().map(str::to_string);
            return Ok(());
        }
        let body_path = result.draft.with_extension("work-body.md");
        if let Some((_, number, value)) = read
            .iter()
            .find(|(kind, _, v)| *kind == "issue" && matches(&note, v))
        {
            let already = value["body"].as_str().unwrap_or_default().contains(&note.notes)
                || value["comments"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|c| c["body"].as_str().unwrap_or_default().contains(&note.notes))
                });
            if !already {
                if confirm(&format!(
                    "Comment on {repo}#{number}:\n\n{}\n\nType yes to post these notes: ",
                    note.notes
                )) != Some(true)
                {
                    return Err("problem report requires confirmation through the controlling terminal; draft retained locally".into());
                }
                oxa_resources::write_atomic(&body_path, note.notes.as_bytes()).map_err(|e| e.to_string())?;
                call(
                    runner,
                    &words(&[
                        "issue",
                        "comment",
                        &number.to_string(),
                        "--repo",
                        &repo,
                        "--body-file",
                        &body_path.to_string_lossy(),
                    ]),
                )?;
            }
            result.disposition = "existing_issue".into();
            result.url = value["url"].as_str().map(str::to_string);
        } else {
            let links = read
                .iter()
                .filter_map(|(_, _, v)| v["url"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let body = if links.is_empty() {
                body.clone()
            } else {
                format!("{body}\n\nRelated open reports (different symptoms):\n{links}\n")
            };
            let body = scrub(&body, ctx);
            if confirm(&format!(
                "Create issue in {repo}:\nTitle: {}\n\n{body}\n\nType yes to post this report: ",
                note.title
            )) != Some(true)
            {
                return Err("problem report requires confirmation through the controlling terminal; draft retained locally".into());
            }
            oxa_resources::write_atomic(&body_path, body.as_bytes()).map_err(|e| e.to_string())?;
            let url = call(
                runner,
                &words(&[
                    "issue",
                    "create",
                    "--repo",
                    &repo,
                    "--title",
                    &note.title,
                    "--body-file",
                    &body_path.to_string_lossy(),
                ]),
            )?;
            result.disposition = "filed".into();
            result.url = Some(url.trim().into());
        }
        let _ = std::fs::remove_file(body_path);
        Ok(())
    })();
    if let Err(error) = attempt {
        result.detail = error;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;

    #[test]
    fn locked_report_command_refuses_oversized_stdout() {
        let env = TestEnv::new();
        let lock = File::open("/dev/null").unwrap();
        let args = words(&["-c", "1048577", "/dev/zero"]);
        assert!(process("/usr/bin/head", &args, &env.work(), &lock, &lock, false).is_none());
    }

    fn note() -> ProblemNote {
        ProblemNote {
            title: "oxa work commit is read only".into(),
            notes: "Another synthetic reproduction of a read only git commit.".into(),
            keywords: vec!["oxa work".into(), "read only git".into()],
        }
    }
    fn settings() -> Settings {
        Settings::from_values(
            serde_json::json!({"oxa":{"reportIssue":{"repo":"example/project"}}}),
            serde_json::json!({}),
        )
    }
    #[test]
    fn declining_visible_queries_prevents_any_github_request() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut value = note();
        value.keywords[0] = "component\u{1b}[2Jsymptom\u{200f}".into();
        let mut calls = 0;
        let mut approved = false;
        let result = report(
            &value,
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut |prompt| {
                assert!(!prompt.contains('\u{1b}') && !prompt.contains('\u{200f}'));
                assert!(prompt.contains("\\u{1b}") && prompt.contains("\\u{200f}"));
                approved = true;
                Some(false)
            },
            |program, _, _| {
                if program == "gh" {
                    calls += 1;
                }
                Some((0, "[]".into()))
            },
        )
        .unwrap();
        assert!(approved);
        assert_eq!(calls, 0);
        assert_eq!(result.disposition, "local");
        assert!(result.draft.exists());
    }
    #[test]
    fn report_confirms_and_targets_the_same_qualified_repository() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut prompts = Vec::new();
        let mut calls = Vec::new();
        let result = report(
            &note(),
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut |prompt| {
                prompts.push(prompt.to_owned());
                Some(true)
            },
            |program, args: &[String], _: &File| {
                if program != "gh" {
                    return Some((0, "{}".into()));
                }
                calls.push(args.to_vec());
                match args[1].as_str() {
                    "list" => Some((0, "[]".into())),
                    "create" => Some((0, "https://github.com/example/project/issues/10".into())),
                    _ => panic!("unexpected report request: {args:?}"),
                }
            },
        )
        .unwrap();
        assert_eq!(result.disposition, "filed");
        assert_eq!(prompts.len(), 2);
        assert!(prompts
            .iter()
            .all(|prompt| prompt.contains("github.com/example/project")));
        assert_eq!(calls.len(), 5);
        assert!(calls.iter().all(|args| args
            .windows(2)
            .any(|pair| { pair[0] == "--repo" && pair[1] == "github.com/example/project" })));
    }

    #[test]
    fn matching_issue_is_commented_after_all_queries_and_reads() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut log = vec![];
        let runner = |program: &str, args: &[String], _: &File| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            log.push(args.to_vec());
            match (args[0].as_str(), args[1].as_str()) {
                ("issue", "list") => Some((0, r#"[{"number":7,"title":"oxa work commit is read only"}]"#.into())),
                ("pr", "list") => Some((0, "[]".into())),
                ("issue", "view") => Some((0, r#"{"title":"oxa work commit is read only","body":"Original symptom","url":"https://github.com/example/project/issues/7","comments":[]}"#.into())),
                ("issue", "comment") => { let body = std::fs::read_to_string(args.last().unwrap()).unwrap(); assert_eq!(body, note().notes); Some((0, "commented".into())) },
                _ => panic!("unexpected write: {args:?}"),
            }
        };
        let result = report(
            &note(),
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut |_| Some(true),
            runner,
        )
        .unwrap();
        assert_eq!(result.disposition, "existing_issue");
        assert_eq!(log.iter().filter(|a| a[1] == "list").count(), 4);
        assert_eq!(log.last().unwrap()[1], "comment");
    }
    #[test]
    fn failed_search_and_offline_never_file_blind() {
        for offline in [false, true] {
            let env = TestEnv::new();
            let dirs = Dirs::under(env.root());
            let mut gh_calls = 0;
            let runner = |program: &str, _: &[String], _: &File| {
                if program == "gh" {
                    gh_calls += 1;
                    None
                } else {
                    Some((0, "{}".into()))
                }
            };
            let result = report(
                &note(),
                &dirs,
                &settings(),
                &ScrubCtx::default(),
                offline,
                &mut |_| Some(true),
                runner,
            )
            .unwrap();
            assert_eq!(result.disposition, "local");
            assert!(result.draft.exists());
            assert!(!result.detail.is_empty());
            assert_eq!(gh_calls, if offline { 0 } else { 1 });
        }
    }
    #[test]
    fn an_open_fixing_pr_prevents_issue_or_comment_writes() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let runner = |program: &str, args: &[String], _: &File| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            if args[1] == "list" {
                return Some((
                    0,
                    if args[0] == "pr" {
                        r#"[{"number":9}]"#
                    } else {
                        "[]"
                    }
                    .into(),
                ));
            }
            assert_eq!(args[1], "view");
            Some((0, r#"{"title":"oxa work commit is read only","body":"Fixed by this PR","url":"https://github.com/example/project/pull/9"}"#.into()))
        };
        let result = report(
            &note(),
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut |_| Some(true),
            runner,
        )
        .unwrap();
        assert_eq!(result.disposition, "existing_pr");
    }
    #[test]
    fn new_report_is_scrubbed_and_related_reports_are_linked() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let mut n = note();
        n.notes = "Synthetic reproduction token=example at /home/synthetic/project with user@example.invalid"
            .into();
        let runner = |program: &str, args: &[String], _: &File| {
            if program != "gh" {
                return Some((0, "{}".into()));
            }
            match args[1].as_str() {
                "list" => Some((0, if args[0] == "issue" { r#"[{"number":8}]"# } else { "[]" }.into())),
                "view" => Some((0, r#"{"title":"Different symptom","body":"Unrelated reproduction","url":"https://github.com/example/project/issues/8","comments":[]}"#.into())),
                "create" => { let body = std::fs::read_to_string(args.last().unwrap()).unwrap(); assert!(!body.contains("token=example") && !body.contains("/home/synthetic") && !body.contains("user@example.invalid")); assert!(body.contains("issues/8")); Some((0, "https://github.com/example/project/issues/10".into())) },
                _ => panic!("unexpected command"),
            }
        };
        let result = report(
            &n,
            &dirs,
            &settings(),
            &ScrubCtx::default(),
            false,
            &mut |_| Some(true),
            runner,
        )
        .unwrap();
        assert_eq!(result.disposition, "filed");
    }
}
