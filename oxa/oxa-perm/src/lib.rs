//! W0-FROZEN (architect-owned).
//!
//! # oxa-perm
//! Permission modes, the access policy and project trust.
//!
//! ## Contracts (plan sec. 2)
//! * Trust gates what LOADS (project settings, extensions, skills, context files, MCP config);
//!   modes gate what tools DO. The two are orthogonal.
//! * [`ModePolicy`] is a pure table over `(Mode, Access, cwd)` plus per-session grants:
//!
//! | Access | plan | workspace (default) | ask |
//! |---|---|---|---|
//! | `Read` inside cwd | Allow | Allow | Allow |
//! | `Read` outside cwd | Ask | Ask | Ask |
//! | `Write` inside cwd | Deny | Allow | Ask |
//! | `Write` outside cwd | Deny | Ask | Ask |
//! | `Exec`, no net, read-only command | Allow | Allow | Ask |
//! | `Exec`, no net, other | Deny | Allow (sandboxed) | Ask |
//! | `Exec`, net | Deny | Ask | Ask |
//! | `Net` | Deny | Ask | Ask |
//! | `Mcp` class Read | Allow | Allow | Ask |
//! | `Mcp` class Write / Exec / Net | Deny | Ask | Ask |
//!
//!   A remembered grant turns `Ask` into `Allow` for the same scope; it never overrides `Deny`.
//!   Grant scope: `Read`/`Write` = the path's directory subtree; `Exec` = the exact command
//!   for `net`, any command otherwise (ask mode); `Net` = the host; `Mcp` = server + tool.
//! * "Inside cwd" is decided after resolving `..` and symlinks ([`is_inside`]): a symlink that
//!   points outside is outside.
//! * With a [`PathGuard`], exact home Cargo configurations (`.cargo/config.toml` and
//!   `.cargo/config`) read without asking, like built-in skills and the session's output.
//!   This does not grant directory access or writes, and the deny-list still wins.
//! * Grants are per session and are never written to disk.
#![forbid(unsafe_code)]

mod danger;
mod desk;
mod effects;
mod paths;
mod policy;
mod preserve;
mod publish;
mod shellmask;
mod trust;

pub use danger::{
    literal_sudo_argv, runs_privileged, DangerHit, DangerRules, BUILTIN_RULES, SETTING_DANGEROUS,
    SETTING_ROOT_ACTIONS,
};
pub use desk::{ApprovalDesk, Record as ApprovalRecord, UnattendedMode};
pub use paths::{is_inside, is_read_only_command, normalize, resolve_real};
pub use policy::{deny_reasons, HiddenPaths, ModePolicy, PathGuard};
pub use preserve::{destructive as destructive_rule, requirement as preservation_requirement};
pub use shellmask::mask_command;
pub use trust::{
    approval_files, gated_resources, manifest, resolve_trust, summary, Approval, Changed, DefaultTrust,
    TrustInputs, TrustOutcome, TrustStore, APPROVED_KEY,
};
