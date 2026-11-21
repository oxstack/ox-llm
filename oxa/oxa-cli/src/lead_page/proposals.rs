//! Presentation of the existing typed native chat proposal; no action or policy writer.
use super::escape;
use crate::lead_chat::conversation::{ActionDraft, BoardChange};
use oxa_work::standing::{Condition, GateEvidence, Kind, ReservedClass, ReviewTier, Scope, Value};
use std::fmt::Write;

pub(super) fn render(html: &mut String, draft: &ActionDraft) {
    html.push_str("<h3>Proposed action</h3>");
    match draft {
        ActionDraft::Rule { rule, scope, value } => {
            let _ = write!(html, "<h4>Proposed standing rule · {}</h4><dl><dt>Applies to</dt><dd>{}</dd><dt>Change</dt><dd>{}</dd></dl>", kind(*rule), scope_label(scope), value_label(value));
        }
        ActionDraft::FollowUp { task, text } => {
            let _ = write!(
                html,
                "<h4>Proposed follow-up</h4><p>Task {}</p><p>{}</p>",
                escape(task),
                escape(text)
            );
        }
        ActionDraft::Board { item: number, change } => {
            let label = match change {
                BoardChange::ReleaseLease => "Release lease",
                BoardChange::Reprioritize => "Change priority",
                BoardChange::Park => "Park item",
            };
            let _ = write!(
                html,
                "<h4>Proposed board change</h4><p>{label} · {}</p>",
                item(*number)
            );
        }
    }
}

fn item(number: u64) -> String {
    format!("<a href=/items/{number}>#{number}</a>")
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Goal => "Goal",
        Kind::Priority => "Priority",
        Kind::Freeze => "Freeze",
        Kind::Hold => "Hold",
        Kind::Budget => "Budget",
        Kind::Model => "Model",
        Kind::Placement => "Placement",
        Kind::Gate => "Review gate",
        Kind::Eligible => "Eligibility",
        Kind::Decided => "Decision",
        Kind::Reserve => "Owner approval",
        Kind::Note => "Note",
    }
}

fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Repository => "This repository".into(),
        Scope::PullRequest(number) => format!("PR {}", item(*number)),
        Scope::Task(value) => format!("Task {}", escape(value)),
        Scope::Branch(value) => format!("Branch {}", escape(value)),
        Scope::Host(value) => format!("Host {}", escape(value)),
        Scope::Path(value) => format!("Path {}", escape(value)),
    }
}

fn list(labels: impl Iterator<Item = &'static str>) -> String {
    let text = labels.collect::<Vec<_>>().join(", ");
    if text.is_empty() {
        "None listed".into()
    } else {
        text
    }
}

fn value_label(value: &Value) -> String {
    match value {
        Value::Text(text) => escape(text),
        Value::Constraint { active: false, .. } => "Release constraint".into(),
        Value::Constraint {
            active: true,
            until: None,
        } => "Active".into(),
        Value::Constraint {
            active: true,
            until: Some(Condition::PrMerged(number)),
        } => format!("Active until {} merges", item(*number)),
        Value::Retired => "Retire rule".into(),
        Value::Budget {
            daily_usd_micros: amount,
        } => format!("USD {}.{:06} per day", amount / 1_000_000, amount % 1_000_000),
        Value::Gate { tier, evidence } => {
            let tier = match tier {
                ReviewTier::T1 => "T1",
                ReviewTier::T2 => "T2",
                ReviewTier::T3 => "T3",
            };
            let evidence = list(evidence.iter().map(|evidence| match evidence {
                GateEvidence::LocalChecks => "Local checks",
                GateEvidence::PrBuild => "PR build",
                GateEvidence::Review => "Review",
                GateEvidence::Queue => "Merge queue",
                GateEvidence::HardwareMeasurement => "Hardware measurement",
            }));
            format!("{tier} · Additional evidence: {evidence}")
        }
        Value::Reserve { classes } => {
            let classes = list(classes.iter().map(|class| match class {
                ReservedClass::Public => "Public actions",
                ReservedClass::Paid => "Paid actions",
                ReservedClass::Credentials => "Credentials",
                ReservedClass::Destructive => "Destructive changes",
                ReservedClass::Desktop => "Desktop",
            }));
            format!("Owner approval for: {classes}")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lead_chat::conversation::{ActionDraft, BoardChange, ConversationReply, ProposedAction};
    use crate::lead_page::{document_for, OwnerChatView, Page, Prompt, View};
    use oxa_work::standing::{
        Condition, GateEvidence, Kind, ReservedClass, ReviewTier, Scope, Source, Value,
    };

    fn page(draft: Option<ActionDraft>, confirmed: bool) -> String {
        let reply = ConversationReply {
            text: "Fixture reply".into(),
            facts: vec![],
            action: draft.map(|draft| ProposedAction {
                draft,
                source: Source::SessionEntry {
                    session_id: "private-session-source".into(),
                    entry_id: "private-entry-source".into(),
                },
            }),
        };
        let proposal = reply.action.as_ref().map(|action| {
            (
                "proposal-example".to_owned(),
                serde_json::to_value(action).unwrap(),
            )
        });
        // The process's reply lands under the thread bubble whose text it repeats, so the
        // session holds that same message, as a real reply does.
        let entries = [oxa_proto::Entry {
            id: "a1b2c3d4".into(),
            parent: None,
            ts_ms: 0,
            body: oxa_proto::EntryBody::Msg {
                wire: oxa_proto::WireMsg::assistant("Fixture reply"),
                parts: Vec::new(),
                tool: None,
            },
        }];
        let owner = OwnerChatView {
            csrf: "csrf-example",
            reply: Some(&reply),
            proposal: proposal.as_ref(),
            confirmed,
            entries: Some(&entries),
        };
        document_for(
            Page::Chat(Prompt::Status, None),
            &View {
                owner_chat: Some(owner),
                ..Default::default()
            },
            0,
        )
    }

    fn rule(kind: Kind, scope: Scope, value: Value) -> String {
        page(
            Some(ActionDraft::Rule {
                rule: kind,
                scope,
                value,
            }),
            false,
        )
    }

    fn protected(html: &str) {
        assert!(!html.contains("&quot;kind&quot;"));
        assert!(!html.contains("private-session-source") && !html.contains("private-entry-source"));
        assert!(html.contains("name=csrf value=\"csrf-example\""));
        assert!(html.contains("name=proposal_id value=\"proposal-example\""));
        assert_eq!(
            html.matches("<button class=btn>Confirm proposal</button>")
                .count(),
            1
        );
        assert!(!html.contains(">Undo<") && !html.contains(">Steer now<"));
    }

    #[test]
    fn typed_chat_proposals_show_human_actions_and_keep_existing_confirmation() {
        let follow = page(
            Some(ActionDraft::FollowUp {
                task: "task-<example>".into(),
                text: "Check <script>fixture</script> & report".into(),
            }),
            false,
        );
        assert!(follow.contains("Proposed follow-up") && follow.contains("task-&lt;example&gt;"));
        assert!(follow.contains("Check &lt;script&gt;fixture&lt;/script&gt; &amp; report"));
        protected(&follow);
        for (change, label) in [
            (BoardChange::ReleaseLease, "Release lease"),
            (BoardChange::Reprioritize, "Change priority"),
            (BoardChange::Park, "Park item"),
        ] {
            let html = page(Some(ActionDraft::Board { item: 42, change }), false);
            assert!(html.contains("Proposed board change") && html.contains(label));
            assert!(html.contains("href=/items/42>#42</a>"));
            protected(&html);
        }
        let note = rule(
            Kind::Note,
            Scope::Branch("feature/<example>".into()),
            Value::Text("Keep <b>evidence</b>".into()),
        );
        assert!(note.contains("Proposed standing rule") && note.contains("Note"));
        assert!(
            note.contains("Branch feature/&lt;example&gt;")
                && note.contains("Keep &lt;b&gt;evidence&lt;/b&gt;")
        );
        protected(&note);
    }

    #[test]
    fn rule_cards_keep_scope_conditions_and_exact_owner_policy_values() {
        let budget = rule(
            Kind::Budget,
            Scope::PullRequest(42),
            Value::Budget {
                daily_usd_micros: 1_000_001,
            },
        );
        assert!(budget.contains("USD 1.000001 per day") && budget.contains("href=/items/42>#42</a>"));
        let hold = rule(
            Kind::Hold,
            Scope::Repository,
            Value::Constraint {
                active: true,
                until: Some(Condition::PrMerged(43)),
            },
        );
        assert!(hold.contains("Active until") && hold.contains("href=/items/43>#43</a>"));
        assert!(hold.contains("This repository"));
        let release = rule(
            Kind::Freeze,
            Scope::Task("task-example".into()),
            Value::Constraint {
                active: false,
                until: None,
            },
        );
        assert!(release.contains("Release constraint") && release.contains("Task task-example"));
        let retired = rule(Kind::Note, Scope::Path("src/example.rs".into()), Value::Retired);
        assert!(retired.contains("Retire rule") && retired.contains("Path src/example.rs"));
        let gate = rule(
            Kind::Gate,
            Scope::Repository,
            Value::Gate {
                tier: ReviewTier::T3,
                evidence: vec![GateEvidence::PrBuild, GateEvidence::Review, GateEvidence::Queue],
            },
        );
        for label in ["T3", "PR build", "Review", "Merge queue"] {
            assert!(gate.contains(label));
        }
        assert!(gate.contains("Additional evidence:"));
        let reserve = rule(
            Kind::Reserve,
            Scope::Host("worker-example".into()),
            Value::Reserve {
                classes: vec![ReservedClass::Paid, ReservedClass::Credentials],
            },
        );
        assert!(
            reserve.contains("Paid actions")
                && reserve.contains("Credentials")
                && reserve.contains("Host worker-example")
        );
        for html in [&budget, &hold, &release, &retired, &gate, &reserve] {
            protected(html);
        }
    }

    #[test]
    fn confirmation_record_is_not_shown_as_application_and_questions_have_no_action() {
        let confirmed = page(
            Some(ActionDraft::Board {
                item: 42,
                change: BoardChange::Park,
            }),
            true,
        );
        assert!(confirmed.contains("Proposed board change"));
        assert!(confirmed.contains("Confirmation recorded. Application is pending."));
        assert!(!confirmed.contains("Confirm proposal") && !confirmed.contains(">Undo<"));
        let question = page(None, false);
        assert!(question.contains("Fixture reply"));
        assert!(!question.contains("Proposed action") && !question.contains("Confirm proposal"));
    }
}
