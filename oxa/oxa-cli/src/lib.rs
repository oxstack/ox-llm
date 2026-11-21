//! W0-FROZEN (architect-owned).
//!
//! # oxa-cli: the `oxa` binary
//! Argument parsing (hand-written, no clap), mode dispatch and the `oxa mcp` / package /
//! `config` subcommands, and (O-152) `oxa identity` and `oxa report-issue`.
//!
//! ## Contracts
//! * Flags, conflicts and messages follow pi's cli.md with `pi` -> `oxa`. Omitted pi flags
//!   (`--api-key` for providers, `oxa auth ..`, `--offline` version checks) fail with the
//!   `unsupported_in_oxa` error and exit code 2.
//! * Mode choice (cli.md tty rules): `--mode rpc` -> RPC; `--mode json` -> JSON; `-p` or stdin
//!   not a terminal or stdout not a terminal -> print; otherwise interactive.
//! * Headless outcomes: one `oxa_result` JSON record on stderr and the documented exit code.
//!   Subcommands retain 0 success, 1 runtime failure, 2 usage error / no engine.
//! * `main` is the only place that reads the process environment (`Env::from_process`), the
//!   terminal state and the real clock for file names.
//! * No network unless asked: no update check, no telemetry.

mod approval;
mod approve_cmd;
mod args;
pub mod askpass;
mod cache_cmd;
mod child;
mod config_cmd;
mod dispatch;
mod gh_probe;
mod goal_cmd;
mod help;
mod home_cwd;
mod hooks;
mod identity_cmd;
mod in_process;
mod key_hints;
pub mod lead;
pub mod lead_access;
pub(crate) mod lead_attach;
pub(crate) mod lead_chat;
pub mod lead_http;
pub mod lead_input;
mod lead_loop;
mod lead_observation;
mod lead_observer;
pub mod lead_page;
pub mod lead_panels;
pub mod lead_peer;
mod lead_preset;
pub mod lead_quota;
pub mod lead_runs;
pub mod lead_serve;
mod limits;
mod local_cmd;
mod login_cmd;
mod mcp_cmd;
mod memory_cmd;
mod models_cmd;
mod netguard;
mod pkg_cmd;
mod pr_activity;
mod pr_threads_cmd;
mod report_cmd;
mod result_facts;
mod run_kind;
mod sandbox_layout;
#[cfg(feature = "share")]
mod share_cmd;
mod status;
mod suite_status;
mod themes;
mod usage_cmd;
mod usage_log;
mod wait_probe;
mod wipe_cmd;
mod work_board;
mod worker_cache;
pub use work_board::refill_input;
mod work_checks;
mod work_cmd;
pub mod work_refill;
mod work_reports;
pub mod workers;

pub use args::{
    no_tool_alias, parse_args, usage_text, ApiKey, Args, LoginArgs, ModeArg, RegistryArgs, Subcommand,
    NO_TOOL_ALIASES,
};
pub use config_cmd::run_config;
pub use dispatch::{choose_mode, run, with_lead_owner, LeadOwnerBinding, RunMode, Stdio};
pub use help::{requested, text};
pub use hooks::{build_hooks, HookInputs, HookSets, EXTENSIONS_LOG};
pub use identity_cmd::{identity_facts, run_identity, write_identity};
pub use local_cmd::run_local;
pub use mcp_cmd::run_mcp;
pub use memory_cmd::run_memory;
pub use pkg_cmd::run_pkg;
pub use report_cmd::{
    advisor_scrubber, clean_notes, parse_draft, process_runner, report_repo, run_report_issue, save_draft,
    scrub, submit, Confirm, PublishTool, Runner, ScrubCtx, DEFAULT_REPORT_REPO, MAX_NOTES_CHARS,
    REPORT_DISABLED, SETTING_REPORT_REPO,
};
pub use wipe_cmd::{run_wipe, WipeIo};
