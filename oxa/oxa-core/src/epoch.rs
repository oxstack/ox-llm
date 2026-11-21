//! C8: prefix epochs (W3).
// ported from pi packages/coding-agent/src/core/agent-session.ts (reload, system prompt rebuild)
use crate::agent_loop::persist;
use crate::prefix::{build_prefix_with, PrefixOpts};
use crate::toolset::{resolve_toolset, ToolSelection, Toolset};
use crate::SessionState;
use oxa_config::Settings;
use oxa_perm::{saved_trust, DefaultTrust, TrustOutcome, TrustStore};
use oxa_proto::{
    codes, AgentEvent, Entry, EntryBody, EventSink, FrozenPrefix, HookPoint, Level, OxaError, Source, Tool,
    ToolSpec, Ui, Value,
};
use oxa_resources::{discover, DiscoverOpts};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Everything one epoch is built from, gathered without touching the state.
struct Inputs {
    /// Built-ins plus the hook tools as read now.
    all: Vec<Arc<dyn Tool>>,
    active: Toolset,
    /// The `Direct` specs (the tool list of the prefix). Callable-only tools never reach the prefix.
    specs: Vec<ToolSpec>,
    extra: Vec<(String, String)>,
    warnings: Vec<String>,
    /// The tool tuning of this epoch (`tune::tune_for`).
    tune: oxa_proto::ToolTune,
}

fn gather(st: &SessionState) -> Inputs {
    // Built-ins come from the last epoch; hook tools are read again (`Hooks::tools`, W3).
    let mut all: Vec<Arc<dyn Tool>> = st
        .all_tools
        .iter()
        .filter(|t| {
            t.spec().source == Source::Builtin && !matches!(t.spec().name.as_str(), "goal_done" | "wait")
        })
        .cloned()
        .collect();
    let mut extra: Vec<(String, String)> = Vec::new();
    for h in &st.hooks {
        all.extend(h.tools());
        extra.extend(h.prompt_sections());
    }
    let policy = crate::goal::WaitPolicy::from_settings(&st.settings);
    // `wait` is declared in every session from its start (`oxa.tools.wait`, default on), with
    // the same spec a goal's `wait` has, so starting a goal does not change that tool entry.
    if st.settings.bool("oxa.tools.wait", true) {
        all.push(crate::goal::wait_tool(&st.shared, &st.hooks, policy, &st.jobs));
    }
    if let Some(goal) = st.goal.as_ref().filter(|goal| goal.pursuing()) {
        all.retain(|t| !matches!(t.spec().name.as_str(), "goal_done" | "wait"));
        all.extend(goal.tools(&st.shared, &st.hooks, policy, &st.jobs));
        extra.push((
            "goal".into(),
            crate::prompt::goal_start(&goal.snapshot().objective),
        ));
    }
    // The CLI choices survive an epoch; `defaultTools` is read from the settings as they are now.
    let sel = ToolSelection {
        default_tools: st.settings.default_tools(),
        default_edits: st.settings.default_tools_edits(),
        ..st.selection.clone()
    };
    let (mut active, warnings) = resolve_toolset(&all, &sel);
    crate::toolset::apply_codemode_mode(&mut active, &st.settings);
    let specs = active.specs();
    let tune = crate::tune::tune_for_model(
        &st.settings,
        st.info.n_ctx,
        crate::tune::budget_soft(st),
        st.info.images == Some(true),
        st.info.caps.remote,
    );
    Inputs {
        all,
        active,
        specs,
        extra,
        warnings,
        tune,
    }
}

/// Rebuild the live tool handlers from the current state without touching the prefix: a
/// resumed or switched-to session replays its stored prefix, but `wait` and the goal controls
/// must be callable and bound to this session's own goal runtime.
pub(crate) fn rebind_tools(st: &mut SessionState, sink: &mut dyn EventSink) {
    let Inputs {
        all,
        active,
        warnings,
        ..
    } = gather(st);
    for w in warnings {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: w,
        });
    }
    st.all_tools = all;
    st.active = active;
    crate::tune::refresh(st);
}

/// The prefix options of an epoch, from the settings as they are now.
pub(crate) fn prefix_opts(st: &SessionState) -> PrefixOpts {
    PrefixOpts {
        diet_output: st.settings.diet_output(),
        child: crate::subagent::is_child_process(),
        headless: crate::prefix::is_headless(),
    }
}

/// Gather the inputs of `build_prefix` from the state as it is now: re-read hook tools
/// (`Hooks::tools`), resolve the toolset, collect `Hooks::prompt_sections`. Returns the
/// `Direct` specs and the extra sections. Does not modify `st`.
pub fn epoch_inputs(st: &SessionState) -> (Vec<ToolSpec>, Vec<(String, String)>) {
    let g = gather(st);
    (g.specs, g.extra)
}

/// Refuse anything that would change the prefix while a run is in progress.
fn ensure_idle(st: &SessionState) -> Result<(), OxaError> {
    if st.shared.busy.load(Ordering::SeqCst) {
        return Err(OxaError::new(
            codes::BUSY,
            "a run is in progress; the prefix changes only between runs",
        )
        .with_hint("wait for the run to settle, then /reload"));
    }
    Ok(())
}

/// Build a prefix from the current inputs and, if its sha differs from `st.prefix` (or
/// `force`, for the first epoch of a session), open a new epoch: append one `Prefix` entry,
/// `Transcript::rebase(same messages, epoch)`, update `st.prefix` / `all_tools` / `active`,
/// emit `PrefixEpoch { reprefill_tokens }` (= current context size, the cost) and a `Notice`.
/// Returns whether an epoch was opened. Only called between runs (`busy` otherwise): never
/// from a timer, a late MCP server or an extension during a run.
pub fn open_epoch(
    st: &mut SessionState,
    reason: &str,
    force: bool,
    sink: &mut dyn EventSink,
) -> Result<bool, OxaError> {
    open_epoch_at(st, reason, force, sink, None)
}

/// The printed cost of an epoch whose caller knows it better than the current context size:
/// a model switch, where the TARGET prefills the whole conversation cold (S3 / ENG-004).
pub(crate) struct EpochCost {
    pub tokens: u32,
    /// The model that pays it (named in the notice).
    pub who: Option<String>,
}

/// [`open_epoch`] with `force` and the caller's cost: `PrefixEpoch.reprefill_tokens` and the
/// one notice carry the same number, with its estimated seconds.
pub(crate) fn open_epoch_costed(
    st: &mut SessionState,
    reason: &str,
    sink: &mut dyn EventSink,
    cost: EpochCost,
) -> Result<bool, OxaError> {
    // The model switch caller owns the session, either idle or between goal requests.
    open_epoch_inner(st, reason, true, sink, Some(cost))
}

fn open_epoch_at(
    st: &mut SessionState,
    reason: &str,
    force: bool,
    sink: &mut dyn EventSink,
    known: Option<EpochCost>,
) -> Result<bool, OxaError> {
    ensure_idle(st)?;
    open_epoch_inner(st, reason, force, sink, known)
}

/// [`open_epoch`] at the start of a run, before its first request (the run holds `busy`, so
/// `ensure_idle` would refuse). Used only by the live settings refresh (L1).
pub(crate) fn open_epoch_at_run_start(
    st: &mut SessionState,
    reason: &str,
    sink: &mut dyn EventSink,
) -> Result<bool, OxaError> {
    open_epoch_inner(st, reason, false, sink, None)
}

fn open_epoch_inner(
    st: &mut SessionState,
    reason: &str,
    force: bool,
    sink: &mut dyn EventSink,
    known: Option<EpochCost>,
) -> Result<bool, OxaError> {
    let Inputs {
        all,
        active,
        specs,
        extra,
        warnings,
        tune,
    } = gather(st);
    for w in warnings {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: w,
        });
    }
    let prefix = build_prefix_with(&st.cwd, &st.resources, &specs, &extra, &prefix_opts(st));
    if !force && prefix.sha256 == st.prefix.sha256 {
        // Same bytes: no epoch. Callable-only tools may still have changed, which does not
        // touch the prefix (W3).
        st.all_tools = all;
        st.active = active;
        // The page size, diet and timeout follow the window and the settings without an epoch.
        crate::tune::refresh(st);
        return Ok(false);
    }

    // The cost is the context that must be prefilled again; the rebase drops the usage.
    let cost = known
        .as_ref()
        .map_or_else(|| st.transcript.context(), |c| c.tokens);
    let sha = prefix.sha256;
    // `append_entry` folds the entry: `Transcript::apply` rebases the kept messages with cause
    // `epoch` when a prefix was already in force, so no second rebase is logged here.
    persist(st, EntryBody::Prefix(prefix.clone()), sink)?;
    st.tune = tune;
    st.prefix = prefix;
    st.all_tools = all;
    st.active = active;
    st.stats.epochs += 1;
    sink.emit(&AgentEvent::PrefixEpoch {
        sha256: sha,
        reason: reason.to_string(),
        reprefill_tokens: cost,
    });
    let secs = crate::prefill_estimate_secs(cost);
    let text = match known.and_then(|c| c.who) {
        Some(who)
            if ["anthropic/", "openai/", "openrouter/"]
                .iter()
                .any(|p| who.starts_with(p)) =>
        {
            format!("new prefix epoch ({reason}): {who} gets the context of about {cost} tokens re-sent (the provider cache may miss)")
        }
        Some(who) => {
            format!("new prefix epoch ({reason}): {who} prefills about {cost} tokens cold (~{secs} s)")
        }
        None => format!("new prefix epoch ({reason}): about {cost} tokens to prefill again (~{secs} s)"),
    };
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text,
    });
    Ok(true)
}

/// Reload never widens the project layer admitted at startup, and rechecks the
/// saved decision before loading new content.
fn project_trusted(st: &SessionState, sink: &mut dyn EventSink) -> bool {
    if st.settings.files().is_some_and(|files| !files.allows_project()) {
        return false;
    }
    let owner = Settings::load(&st.dirs, &st.cwd, false);
    let trust_file = st.dirs.trust_file();
    let mut store = TrustStore::load(&trust_file);
    let outcome = saved_trust(
        &store,
        &st.cwd,
        DefaultTrust::from_setting(owner.project_trust_default()),
    );
    if outcome != TrustOutcome::Trusted {
        return false;
    }
    if owner.project_trust_requires_content() {
        let text = match store.check_content(&trust_file, &st.cwd) {
            Ok(oxa_perm::Approval::Matches | oxa_perm::Approval::NoRecord) => return true,
            Ok(oxa_perm::Approval::Changed(changed)) => format!(
                "Project config not loaded: {}. Restart and approve the changes to load it.",
                changed
                    .iter()
                    .map(|ch| ch.path_line())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(err) => err.message,
        };
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text,
        });
        return false;
    }
    if let Some(text) = store.folder_changes_notice(&st.cwd) {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text,
        });
    }
    true
}

/// Extension paths for a reload: the discovered ones plus earlier ones that still exist. While
/// the project is not trusted (new or changed content), earlier paths inside the project's own
/// `.oxa` or `.agents` are dropped, so a file rewritten by bash never loads without approval.
fn reload_extension_paths(
    discovered: &[std::path::PathBuf],
    earlier: &[std::path::PathBuf],
    cwd: &std::path::Path,
    trusted: bool,
) -> Vec<std::path::PathBuf> {
    let mut paths = discovered.to_vec();
    for p in earlier {
        let project = p.starts_with(cwd.join(".oxa")) || p.starts_with(cwd.join(".agents"));
        if (trusted || !project) && !paths.contains(p) && p.exists() {
            paths.push(p.clone());
        }
    }
    paths
}

/// `/reload`: re-read settings and resources, fire `ResourcesDiscover`, shut down and reload
/// hook sets, then [`open_epoch`]. No change in the prefix bytes = no new epoch, returns false.
pub fn reload(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Result<bool, OxaError> {
    ensure_idle(st)?;
    let trusted = project_trusted(st, sink);
    let settings = Settings::load(&st.dirs, &st.cwd, trusted);
    for d in settings.diagnostics() {
        sink.emit(&AgentEvent::Notice {
            level: d.level,
            text: format!("{}: {}", d.source, d.message),
        });
    }
    // Only the trust-independent discovery inputs are known here (see `SessionState`).
    let opts = DiscoverOpts {
        trusted,
        ..DiscoverOpts::default()
    };
    // Packages, extension and theme paths included (E10).
    let resources = if st.hooks.iter().any(|hook| hook.owns_resources()) {
        st.resources.clone()
    } else {
        discover(&st.dirs, &st.cwd, &settings, &opts)
    };
    for d in &resources.diagnostics {
        sink.emit(&AgentEvent::Notice {
            level: d.level,
            text: d.message.clone(),
        });
    }
    let prev_enabled = st.settings.compaction().enabled;
    // EXT-007 (pi /reload): the code-loaded hook sets are re-created. The paths are the ones
    // discovered now plus the earlier ones that still exist (`-e` paths are not discovered).
    let paths = reload_extension_paths(
        &resources.extension_paths,
        &st.resources.extension_paths,
        &st.cwd,
        trusted,
    );
    st.settings = settings;
    crate::session::sync_job_wake(st);
    crate::settings_refresh::resync(st);
    st.resources = resources;
    crate::host::observe_hooks(st, HookPoint::SessionShutdown, &Value::Null, sink, ui);
    for h in st.hooks.clone() {
        h.reload(&paths);
        crate::session::report_hook_errors(h.as_ref(), sink);
    }
    crate::host::attach_hosts(st, sink, ui);
    let start = serde_json::json!({ "reason": "reload" });
    crate::host::observe_hooks(st, HookPoint::SessionStart, &start, sink, ui);
    // X3: the reloaded `compaction.*` settings (and the engine's current limits) take effect now.
    crate::session::refresh_budget(st, Some(prev_enabled), sink);
    // Hook sets are re-read by `open_epoch` (`tools`, `prompt_sections`). The reply is ignored:
    // an observer point cannot change the prefix.
    let payload = serde_json::json!({ "cwd": st.cwd.display().to_string(), "reason": "reload" });
    crate::host::observe_hooks(st, HookPoint::ResourcesDiscover, &payload, sink, ui);
    open_epoch(st, "reload", false, sink)
}

/// The prefix in force at the end of `branch`: its last `Prefix` entry, byte for byte. Resume
/// uses this and never rebuilds (so a changed AGENTS.md shows up only after `/reload`).
pub fn replay_prefix(branch: &[&Entry]) -> Option<FrozenPrefix> {
    branch.iter().rev().find_map(|e| match &e.body {
        EntryBody::Prefix(p) => Some(p.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod reload_paths_tests {
    use super::reload_extension_paths;

    #[test]
    fn untrusted_reload_drops_earlier_project_extensions() {
        let d = std::env::temp_dir().join(format!("oxa-rp-{}", std::process::id()));
        let ext = d.join(".oxa/extensions");
        std::fs::create_dir_all(&ext).unwrap();
        let proj = ext.join("x.js");
        let other = d.join("flag.js");
        std::fs::write(&proj, "1").unwrap();
        std::fs::write(&other, "1").unwrap();
        let both = vec![proj.clone(), other.clone()];
        assert_eq!(reload_extension_paths(&[], &both, &d, false), vec![other.clone()]);
        assert_eq!(reload_extension_paths(&[], &both, &d, true), both);
        std::fs::remove_dir_all(&d).ok();
    }
}
