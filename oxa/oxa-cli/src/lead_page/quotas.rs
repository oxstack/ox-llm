use super::View;
use crate::lead_quota::{model_windows, GitHubIdentity, GitHubResource};
use std::fmt::Write;

fn meter<N: std::fmt::Display>(html: &mut String, label: &str, max: N, value: N, value_text: &str) {
    let _ = write!(
        html,
        " <meter min=0 max={max} value={value} aria-label=\"{}\" aria-valuetext=\"{}\">{}</meter>",
        super::escape(label),
        super::escape(value_text),
        super::escape(value_text)
    );
}

pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section><h2>Plans and quotas</h2>");
    html.push_str("<dl class=quota-windows>");
    for (identity, label) in [(GitHubIdentity::App, "App"), (GitHubIdentity::Owner, "Owner")] {
        for (resource, api) in [
            (GitHubResource::Core, "REST core"),
            (GitHubResource::Graphql, "GraphQL"),
        ] {
            let _ = write!(html, "<dt>GitHub {label} · {api}</dt>");
            if let Some(window) = view
                .github_quotas
                .and_then(|quotas| quotas.window(identity, resource, now))
            {
                let _ = write!(
                    html,
                    "<dd>{} of {} remaining · resets {}",
                    window.remaining,
                    window.limit,
                    super::human_time(window.resets_at, now)
                );
                meter(
                    html,
                    &format!("GitHub {label} · {api} remaining"),
                    window.limit,
                    window.remaining,
                    &format!("{} of {} remaining", window.remaining, window.limit),
                );
                html.push_str("</dd>");
            } else {
                let reason = if view.github_quotas.is_none() {
                    "No GitHub quota observation reached this page."
                } else {
                    "No valid current quota window was reported."
                };
                let _ = write!(html, "<dd>Unknown. {reason}</dd>");
            }
        }
    }
    let plan = view
        .model_plan
        .and_then(|usage| usage.plan_type.as_deref())
        .filter(|plan| {
            !plan.is_empty()
                && plan.len() <= 40
                && plan
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        });
    let plan = plan.unwrap_or(if view.model_plan.is_none() {
        "Unknown. No model plan observation reached this page."
    } else {
        "Unknown. No valid model plan was reported."
    });
    let _ = write!(html, "<dt>Model plan</dt><dd>{plan}</dd>");
    let windows = view
        .model_plan
        .map_or([None; 2], |usage| model_windows(usage, now));
    for (label, window) in ["Primary", "Secondary"].into_iter().zip(windows) {
        let _ = write!(html, "<dt>{label} model window</dt>");
        if let Some(window) = window {
            let _ = write!(
                html,
                "<dd>{}% used · resets {}",
                window.used_percent,
                super::human_time(window.resets_at, now)
            );
            meter(
                html,
                &format!("{label} model window used"),
                100.0,
                window.used_percent,
                &format!("{}% used", window.used_percent),
            );
            html.push_str("</dd>");
        } else {
            let reason = if view.model_plan.is_none() {
                "No model plan observation reached this page."
            } else {
                "No valid current model window was reported."
            };
            let _ = write!(html, "<dd>Unknown. {reason}</dd>");
        }
    }
    for deferral in view.quota_deferred {
        let why = match deferral.until {
            Some(reset) => format!(
                "held back until the window resets {}",
                super::human_time(reset, now)
            ),
            None => "held back: no quota window observed".to_owned(),
        };
        let _ = write!(
            html,
            "<dt>Deferred by the GitHub reserve</dt><dd>{} ({}): {why}</dd>",
            super::escape(&deferral.work),
            super::escape(&deferral.resource)
        );
    }
    html.push_str("</dl></section>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead_page::{document_for, Page};
    use crate::lead_quota::GitHubQuotas;
    use serde_json::json;

    #[test]
    fn usage_page_shows_live_windows_without_inventing_missing_quotas() {
        let mut quotas = GitHubQuotas::default();
        let headers = "x-ratelimit-resource: core\nx-ratelimit-limit: 5000\nx-ratelimit-remaining: 0\nx-ratelimit-reset: 120";
        assert!(quotas.observe(GitHubIdentity::App, GitHubResource::Core, headers));
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "plan_type": "fixture-plan",
            "credits": {"balance": "fixture-private-field"},
            "rate_limit": {
                "primary_window": {"used_percent": 12.5, "reset_at": 120},
                "secondary_window": {"used_percent": 85, "reset_at": 180}
            }
        }));
        let html = document_for(
            Page::Usage,
            &View {
                github_quotas: Some(&quotas),
                model_plan: Some(&model),
                ..Default::default()
            },
            100,
        );
        assert!(html.contains("GitHub App · REST core</dt><dd>0 of 5000 remaining"));
        assert!(html.contains("GitHub Owner · REST core</dt><dd>Unknown"));
        assert!(html.contains("Model plan</dt><dd>fixture-plan"));
        assert!(html.contains("Primary model window</dt><dd>12.5% used"));
        assert!(html.contains("Secondary model window</dt><dd>85% used"));
        assert!(!html.contains("fixture-private-field"));
    }

    fn usage_page(
        github_quotas: Option<&GitHubQuotas>,
        model_plan: Option<&oxc_gateway::status::chatgpt_usage::Usage>,
        now: u64,
    ) -> String {
        document_for(
            Page::Usage,
            &View {
                github_quotas,
                model_plan,
                ..Default::default()
            },
            now,
        )
    }

    #[test]
    fn quota_meters_keep_identity_resource_units_and_measured_zero_explicit() {
        let mut quotas = GitHubQuotas::default();
        for (identity, resource, name, limit, remaining) in [
            (GitHubIdentity::App, GitHubResource::Core, "core", 5000, 0),
            (GitHubIdentity::App, GitHubResource::Graphql, "graphql", 100, 75),
            (GitHubIdentity::Owner, GitHubResource::Core, "core", 500, 200),
        ] {
            let headers = format!("x-ratelimit-resource: {name}\nx-ratelimit-limit: {limit}\nx-ratelimit-remaining: {remaining}\nx-ratelimit-reset: 180");
            assert!(quotas.observe(identity, resource, &headers));
        }
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "plan_type": "fixture-plan", "credits": {"balance": "fixture-private-field"},
            "rate_limit": {
                "primary_window": {"used_percent": 0, "reset_at": 180},
                "secondary_window": {"used_percent": 100, "reset_at": 180}
            }
        }));
        let html = usage_page(Some(&quotas), Some(&model), 100);
        let meters: Vec<_> = html
            .split("<meter ")
            .skip(1)
            .map(|part| part.split("</meter>").next().unwrap())
            .collect();
        assert_eq!(meters.len(), 5);
        for (label, max, value, description) in [
            (
                "GitHub App · REST core remaining",
                "5000",
                "0",
                "0 of 5000 remaining",
            ),
            (
                "GitHub App · GraphQL remaining",
                "100",
                "75",
                "75 of 100 remaining",
            ),
            (
                "GitHub Owner · REST core remaining",
                "500",
                "200",
                "200 of 500 remaining",
            ),
            ("Primary model window used", "100", "0", "0% used"),
            ("Secondary model window used", "100", "100", "100% used"),
        ] {
            let meter = meters
                .iter()
                .find(|meter| meter.contains(&format!("aria-label=\"{label}\"")))
                .unwrap();
            assert!(meter.contains(&format!("min=0 max={max} value={value} ")));
            assert!(meter.contains(&format!("aria-valuetext=\"{description}\"")));
        }
        assert!(html.contains("GitHub Owner · GraphQL</dt><dd>Unknown"));
        assert!(!html.contains("fixture-private-field"));
        // Read-only view: no forms, and the theme toggle is the document's only
        // button (chrome that writes localStorage, never server state).
        assert!(!html.contains("<form"));
        assert_eq!(html.matches("<button").count(), 1);
        assert!(html.contains("<button id=theme class=iconbtn"));
    }

    #[test]
    fn missing_invalid_and_expired_quota_windows_do_not_become_zero_meters() {
        let mut quotas = GitHubQuotas::default();
        for (identity, remaining, resets_at) in
            [(GitHubIdentity::App, 0, 120), (GitHubIdentity::Owner, 450, 180)]
        {
            let headers = format!("x-ratelimit-resource: core\nx-ratelimit-limit: 5000\nx-ratelimit-remaining: {remaining}\nx-ratelimit-reset: {resets_at}");
            assert!(quotas.observe(identity, GitHubResource::Core, &headers));
        }
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "rate_limit": {"primary_window": {"used_percent": 0, "reset_at": 120}}
        }));
        let boundary = usage_page(Some(&quotas), Some(&model), 120);
        assert_eq!(boundary.matches("<meter ").count(), 1);
        assert!(boundary.contains("aria-label=\"GitHub Owner · REST core remaining\""));
        assert!(boundary.contains("GitHub App · REST core</dt><dd>Unknown"));
        assert!(boundary.contains("Primary model window</dt><dd>Unknown"));
        assert!(boundary.contains("Secondary model window</dt><dd>Unknown"));
        assert!(!usage_page(Some(&quotas), Some(&model), 180).contains("<meter "));
        let unknown = usage_page(None, None, 100);
        assert!(unknown.contains("GitHub App · GraphQL</dt><dd>Unknown"));
        assert!(unknown.contains("GitHub Owner · REST core</dt><dd>Unknown"));
        assert!(unknown.contains("Primary model window</dt><dd>Unknown"));
        assert!(!unknown.contains("<meter "));
        let invalid = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "rate_limit": {
                "primary_window": {"used_percent": 101, "reset_at": 180},
                "secondary_window": {"used_percent": -1, "reset_at": 180}
            }
        }));
        assert!(!usage_page(None, Some(&invalid), 100).contains("<meter "));
    }
}
