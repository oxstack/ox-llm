//! The "Waiting for your approval" panel (#1512 S6): the approval desk's pending requests.
//! Only the bound owner's view renders the panel. Credential masking precedes HTML escaping, and
//! the record text goes through the CLI listing's one-line form first, so line breaks, control and
//! bidi or zero-width characters show as escapes: the owner reads the text the shell will run.
use super::{escape, View};
use crate::approve_cmd::one_line;
use crate::report_cmd::{scrub, ScrubCtx};
use oxa_perm::ApprovalRecord;
use std::fmt::Write;
use std::path::Path;

/// The owner's answers, in the order the desk offers them.
const ANSWERS: [(&str, &str); 4] = [
    ("once", "Once"),
    ("session", "This session"),
    ("always", "Always"),
    ("deny", "Deny"),
];

/// Nothing renders when the caller supplied no desk reading, so pages without one are unchanged.
pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    let Some((waiting, csrf)) = view.approvals.zip(view.approval_csrf) else {
        return;
    };
    html.push_str(
        "<section id=approvals aria-labelledby=approvals-title><h2 id=approvals-title>Waiting for your approval</h2>",
    );
    if waiting.is_empty() {
        html.push_str("<p class=muted>Nothing is waiting for your approval.</p>");
    } else {
        html.push_str("<ol class=approvals>");
        let scrub_ctx = ScrubCtx::default();
        for rec in waiting {
            record(html, csrf, rec, now, &scrub_ctx);
        }
        html.push_str("</ol>");
    }
    html.push_str("</section>");
}

fn record(html: &mut String, csrf: &str, rec: &ApprovalRecord, now: u64, scrub_ctx: &ScrubCtx) {
    let folder = Path::new(&rec.cwd)
        .file_name()
        .map_or_else(|| "(root)".to_owned(), |name| name.to_string_lossy().into_owned());
    let _ = write!(
        html,
        "<li class=approval><p><code>{}</code> · session <code>{}</code> · <code>{}</code> · waiting {}</p><pre>{}</pre>",
        escape(&one_line(&rec.id)),
        escape(&one_line(&rec.session)),
        escape(&one_line(&folder)),
        age(now, rec.created_ms),
        escape(&one_line(&scrub_shell(&rec.command, scrub_ctx))),
    );
    html.push_str("<ul class=reasons>");
    for reason in &rec.reasons {
        let _ = write!(html, "<li>{}</li>", escape(&one_line(&scrub(reason, scrub_ctx))));
    }
    html.push_str("</ul>");
    let _ = write!(
        html,
        "<form method=post action=/approvals><input type=hidden name=csrf value=\"{}\"><input type=hidden name=id value=\"{}\">",
        escape(csrf),
        escape(&rec.id)
    );
    for (answer, label) in ANSWERS {
        let _ = write!(html, "<button name=answer value={answer}>{label}</button>");
    }
    html.push_str("</form>");
    html.push_str("</li>");
}

/// Credential masking that keeps every executable part of the command visible: the one
/// tokenizer-based masker `oxa_perm::mask_command`, with this crate's scrubber for known token
/// shapes and URL credentials in literal text.
pub(super) fn scrub_shell(command: &str, ctx: &ScrubCtx) -> String {
    oxa_perm::mask_command(command, &|lit| scrub(lit, ctx))
}

/// Whole seconds, minutes or hours since the request was filed. A timestamp ahead of this clock reads as new.
pub(super) fn age(now: u64, created_ms: u64) -> String {
    let seconds = now.saturating_sub(created_ms / 1000);
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3599 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

#[cfg(test)]
mod tests {
    use crate::lead_page::{content_for, Page, View};
    use oxa_perm::ApprovalRecord;

    #[test]
    fn board_page_lists_pending_approvals_escaped() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s<1>".into(),
            cwd: "/work/<proj>".into(),
            command: "rm -rf \"<x>\" &".into(),
            keys: vec!["rm".into()],
            reasons: vec!["rm: <recursive>".into(), "password=opaque-example".into()],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(html.contains("Waiting for your approval"));
        assert!(
            html.contains("<pre>rm -rf &quot;&lt;x&gt;&quot; &amp;</pre>"),
            "{html}"
        );
        assert!(html.contains("session <code>s&lt;1&gt;</code>"));
        assert!(html.contains("<code>&lt;proj&gt;</code>"));
        assert!(html.contains("<li>rm: &lt;recursive&gt;</li>"));
        assert!(html.contains("<li>password=&lt;secret&gt;</li>"));
        assert!(!html.contains("opaque-example"));
        assert!(html.contains("waiting 2 min"));
        assert!(html.contains("<button name=answer value=once>Once</button>"));
        assert!(html.contains("name=csrf value=\"tok\""));
        assert!(!html.contains("<x>"));
        assert!(!html.contains("<proj>"));

        let read_only = View {
            approvals: Some(&waiting),
            ..Default::default()
        };
        let html = content_for(Page::Board, &read_only, 120);
        assert!(!html.contains("Waiting for your approval"));
        assert!(!html.contains("s&lt;1&gt;"));
        assert!(!html.contains("action=/approvals"));
    }

    /// A multiline command or one with bidi or zero-width characters renders the escaped one-line
    /// form of the CLI listing, never a joined line that hides the second command in a comment.
    #[test]
    fn approval_panel_shows_line_breaks_and_format_characters() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s\u{202e}1".into(),
            cwd: "/work/proj".into(),
            command: "echo safe # explanation\nrm -rf /srv/data\u{200b}".into(),
            keys: vec!["rm".into()],
            reasons: vec!["rm:\u{2066} recursive".into()],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("<pre>echo safe # explanation\\nrm -rf /srv/data\\u{200b}</pre>"),
            "{html}"
        );
        assert!(html.contains("session <code>s\\u{202e}1</code>"), "{html}");
        assert!(html.contains("<li>rm:\\u{2066} recursive</li>"), "{html}");
        for hidden in ['\u{202e}', '\u{200b}', '\u{2066}'] {
            assert!(!html.contains(hidden), "{html}");
        }
        assert!(!html.contains("explanation rm"), "{html}");
    }

    /// Masking a secret-looking assignment keeps the shell operator after it, so the owner sees
    /// every command the line runs.
    #[test]
    fn approval_panel_masking_keeps_shell_operators() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s1".into(),
            cwd: "/work/proj".into(),
            command: "echo token=demo&&rm -rf /srv/data;echo password=opaque-example|sh".into(),
            keys: vec!["rm".into()],
            reasons: vec![],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("&amp;&amp;rm -rf /srv/data;echo password=&lt;secret&gt;|sh</pre>"),
            "{html}"
        );
        assert!(!html.contains("token=demo"), "{html}");
    }

    /// An expansion runs: a command substitution inside a quoted value is shown in full, never
    /// hidden as part of a masked credential.
    #[test]
    fn approval_panel_shows_command_substitutions() {
        let waiting = vec![ApprovalRecord {
            id: "0123456789abcdef".into(),
            session: "s1".into(),
            cwd: "/work/proj".into(),
            command: r#"password="$(rm -rf /srv/data)" git push --force origin main"#.into(),
            keys: vec!["git-push-force".into()],
            reasons: vec![],
            status: "pending".into(),
            used: false,
            run_nonce: "0123456789abcdef".into(),
            created_ms: 0,
            answered_ms: None,
        }];
        let owner = View {
            approvals: Some(&waiting),
            approval_csrf: Some("tok"),
            ..Default::default()
        };
        let html = content_for(Page::Board, &owner, 120);
        assert!(
            html.contains("password=&quot;$(rm -rf /srv/data)&quot; git push --force origin main</pre>"),
            "{html}"
        );
    }

    #[test]
    fn approval_panel_masks_literal_runs_around_expansions() {
        for (command, shown) in [
            // A trailing `$` cannot start an expansion, so the whole value is literal.
            (
                r#"password="opaque-example$" git push --force origin main"#,
                "password=&lt;secret&gt; git push",
            ),
            // A real expansion stays visible; the literal credential run around it is masked.
            (
                r#"password="opaque-example$USER" git push --force origin main"#,
                "password=&lt;secret&gt;$USER&quot; git push",
            ),
            (
                r#"password="opaque-example${USER}" git push --force origin main"#,
                "password=&lt;secret&gt;${USER}&quot; git push",
            ),
            (
                r#"TOKEN=opaque-example$USER git push --force origin main"#,
                "TOKEN=&lt;secret&gt;$USER git push",
            ),
        ] {
            let waiting = vec![ApprovalRecord {
                id: "0123456789abcdef".into(),
                session: "s1".into(),
                cwd: "/work/proj".into(),
                command: command.into(),
                keys: vec!["git-push-force".into()],
                reasons: vec![],
                status: "pending".into(),
                used: false,
                run_nonce: "0123456789abcdef".into(),
                created_ms: 0,
                answered_ms: None,
            }];
            let owner = View {
                approvals: Some(&waiting),
                approval_csrf: Some("tok"),
                ..Default::default()
            };
            let html = content_for(Page::Board, &owner, 120);
            assert!(html.contains(shown), "missing {shown:?} in {html}");
            assert!(
                !html.contains("opaque-example"),
                "literal credential text is displayed: {html}"
            );
        }
    }

    /// A shell metacharacter inside a quoted or escaped credential value is part of the value:
    /// the whole value stays masked.
    #[test]
    fn approval_panel_masking_respects_shell_quoting() {
        for (command, secret) in [
            (
                r#"password="&opaque-example" git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password=x\&opaque-example git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password='&opaque-example' git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"API_KEY='a b opaque-example' git push --force origin main"#,
                "opaque-example",
            ),
            (
                r#"password='opaque$example' git push --force origin main"#,
                "opaque$example",
            ),
            (
                r#"password=opaque\$example git push --force origin main"#,
                r#"opaque\$example"#,
            ),
        ] {
            let waiting = vec![ApprovalRecord {
                id: "0123456789abcdef".into(),
                session: "s1".into(),
                cwd: "/work/proj".into(),
                command: command.into(),
                keys: vec!["git-push-force".into()],
                reasons: vec![],
                status: "pending".into(),
                used: false,
                run_nonce: "0123456789abcdef".into(),
                created_ms: 0,
                answered_ms: None,
            }];
            let owner = View {
                approvals: Some(&waiting),
                approval_csrf: Some("tok"),
                ..Default::default()
            };
            let html = content_for(Page::Board, &owner, 120);
            assert!(!html.contains(secret), "{command}: {html}");
            assert!(
                html.contains("git push --force origin main</pre>"),
                "{command}: {html}"
            );
        }
    }

    /// What the owner's page shows: executable text stays visible, secrets do not (#2115, #2116,
    /// #2120, #2121).
    #[test]
    fn approval_panel_masks_secrets_and_shows_what_runs() {
        for (command, shown, hidden) in [
            (
                r#"password="$(printf ')' ; rm -rf /srv/data)" git push --force origin main"#,
                "rm -rf /srv/data",
                "",
            ),
            (
                r#"password="${UNSET:-opaque-example}" git push --force origin main"#,
                "${UNSET:-",
                "opaque-example",
            ),
            (
                r#"password="${UNSET:+opaque-example}" git push --force origin main"#,
                "${UNSET:+",
                "opaque-example",
            ),
            (
                r#"git push --force "https://user:opaque-example@example.invalid/$REPO" main"#,
                "$REPO",
                "opaque-example",
            ),
            (
                "docker login -u me --password opaque-example registry.invalid && git push --force origin main",
                "git push --force origin main",
                "opaque-example",
            ),
            ("token=demo&&rm -rf x", "&amp;&amp;rm -rf x", "token=demo"),
            (
                r#"x=$(curl -H "Authorization: Bearer opaque-example" https://example.invalid) git push --force origin main"#,
                "curl -H",
                "opaque-example",
            ),
        ] {
            let waiting = vec![ApprovalRecord {
                id: "0123456789abcdef".into(),
                session: "s1".into(),
                cwd: "/work/proj".into(),
                command: command.into(),
                keys: vec!["git-push-force".into()],
                reasons: vec![],
                status: "pending".into(),
                used: false,
                run_nonce: "0123456789abcdef".into(),
                created_ms: 0,
                answered_ms: None,
            }];
            let owner = View {
                approvals: Some(&waiting),
                approval_csrf: Some("tok"),
                ..Default::default()
            };
            let html = content_for(Page::Board, &owner, 120);
            assert!(html.contains(shown), "{command}: missing {shown:?} in {html}");
            assert!(hidden.is_empty() || !html.contains(hidden), "{command}: {html}");
        }
    }
}
