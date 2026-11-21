//! Safety: guards on permissions, network and the bash sandbox (design section 5, row A2d).

use super::{d, Apply, Control, Descriptor, Group, Item, Unit, WebEdit};
use crate::schema::Kind;

/// The Safety entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "oxa.workers.repository",
        Group::Safety,
        "Workers",
        "Lead worker repository",
        "Canonical absolute path to the explicitly trusted primary Git checkout used by a live Lead outside Git. User settings only.",
    )
    .key(Kind::String, "null")
    .text()
    .user_only(),
    // #202: the permission mode at start; a project's value may only lower it
    // (`Settings::default_permission_mode`).
    d(
        "permissions.defaultMode",
        Group::Safety,
        "Permissions",
        "Permission mode at start",
        "standard, auto, yolo or plan. A project may only lower it.",
    )
    .key(Kind::Enum(&["standard", "auto", "yolo", "plan"]), "\"auto\"")
    .choice(&[("standard", ""), ("auto", ""), ("yolo", ""), ("plan", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Changes how much oxa does without asking first."),
    d(
        "oxa.mode",
        Group::Safety,
        "Permissions",
        "Work mode at start",
        "plan, workspace or ask.",
    )
    .key(Kind::Enum(&["plan", "workspace", "ask"]), "\"workspace\"")
    .choice(&[("plan", ""), ("workspace", ""), ("ask", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "defaultProjectTrust",
        Group::Safety,
        "Permissions",
        "New projects",
        "Trust a project not seen before, ask first, or never trust.",
    )
    .key(Kind::Enum(&["ask", "always", "never"]), "\"always\"")
    .choice(&[("ask", ""), ("always", ""), ("never", "")])
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Trusting a new project lets its settings apply without asking."),
    d(
        "projectTrustPolicy",
        Group::Safety,
        "Permissions",
        "Project trust policy",
        "Trust a folder once, or ask again when its executable project files change.",
    )
    .key(Kind::Enum(&["folder", "content"]), "\"folder\"")
    .choice(&[("folder", ""), ("content", "")])
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Folder trust lets new and changed project code load without asking again."),
    d(
        "oxa.allowHomeCwd",
        Group::Safety,
        "Permissions",
        "Start in the home folder",
        "Allow oxa to start in home, / or a parent of home.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets oxa start in the home folder or above it, so tools reach all of it."),
    d(
        "oxa.offline",
        Group::Safety,
        "Network",
        "Offline",
        "No network use by oxa.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Turning this off lets oxa use the network again."),
    d(
        "oxa.engine.allowRemote",
        Group::Safety,
        "Network",
        "Engines on other hosts",
        "Allow an engine that is not on this machine.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets a model engine on another host receive requests."),
    d(
        "httpProxy",
        Group::Safety,
        "Network",
        "HTTP proxy for MCP",
        "Proxy for MCP HTTP requests.",
    )
    .key(Kind::String, "null")
    .text()
    .secret()
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Sends MCP HTTP requests through this proxy."),
    d(
        "oxa.sandbox.net",
        Group::Safety,
        "Bash sandbox",
        "Network in bash",
        "Network for bash in workspace and ask modes.",
    )
    .key(Kind::Enum(&["on", "off"]), "\"on\"")
    .choice(&[("on", "bash may use the network"), ("off", "bash has no network")])
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Lets bash commands reach the network in workspace and ask modes."),
    d(
        "oxa.github.tokenCommand",
        Group::Safety,
        "GitHub",
        "GitHub token command",
        "Command that prints a GitHub token for the work runner, not for shell authentication. Operands must be trusted absolute executable paths. Empty uses `gh auth token`.",
    )
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Text))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Runs this command outside the sandbox and lets the work runner authenticate to GitHub."),
    d(
        "oxa.github.tokenTtlSeconds",
        Group::Safety,
        "GitHub",
        "GitHub token lifetime",
        "Seconds between token lookups for networked bash (300 when unset, 0 on each use). Configured tokens are only used for masking.",
    )
    .key(Kind::Number, "null")
    .number(Unit::Seconds)
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.sandbox.allowHomeWrite",
        Group::Safety,
        "Bash sandbox",
        "Home writable in bash",
        "Make home the bash workspace when oxa starts there.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash write across the home folder when oxa starts there."),
    // B2 (lane harden): extra binds of the bash sandbox; user settings only.
    d(
        "oxa.sandbox.readPaths",
        Group::Safety,
        "Bash sandbox",
        "Extra readable folders",
        "More read-only paths for bash.",
    )
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Gives bash read access to these extra folders."),
    d(
        "oxa.sandbox.writePaths",
        Group::Safety,
        "Bash sandbox",
        "Extra writable folders",
        "More writable paths for bash.",
    )
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Gives bash write access to these extra folders."),
    d(
        "oxa.sandbox.sockets",
        Group::Safety,
        "Bash sandbox",
        "Host sockets for bash",
        "Runtime sockets (docker, podman) bash may reach.",
    )
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash use these host sockets; the docker socket gives root on this machine."),
    d(
        "oxa.sandbox.gitConfigAllow",
        Group::Safety,
        "Bash sandbox",
        "Git config keys bash may change",
        "Keys of the repository's git config that sandboxed commands may change (patterns like branch.*); every other change is reverted after the command.",
    )
    .key(Kind::StringList, "[\"branch.*\", \"user.*\", \"remote.*.fetch\"]")
    .control(Control::List(Item::Text))
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash commands change these git config keys, which git acts on outside the sandbox."),
    d(
        "oxa.sandbox.scratchWarnSize",
        Group::Safety,
        "Bash sandbox",
        "Scratch size warning",
        "Warn when the session's /tmp scratch grows past this size (20GiB, `off` for never).",
    )
    .key(Kind::String, "\"20GiB\"")
    .text()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Edit),
];
