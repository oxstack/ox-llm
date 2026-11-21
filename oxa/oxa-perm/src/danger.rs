//! #1512 S1: the dangerous-command classifier. Pure: it parses a command line the way a POSIX
//! shell reads it and reports which simple commands match a rule. No policy, no prompt, no
//! wiring: the caller decides what a hit means.
//!
//! The parser is a small tokenizer, not a regex: quotes and backslashes are removed before any
//! rule looks at a word, so `r\m`, `'r'm` and `"rm"` are all `rm`. Command substitutions and
//! `sh -c` strings are parsed again and checked too. A line the tokenizer cannot read is one
//! `unparsed` hit (fail closed).
use crate::effects::{self, Effects};
use crate::paths::{is_inside, normalize, resolve_real};
use crate::policy::PathGuard;
use std::path::{Path, PathBuf};

/// The owner setting that lists the rules (`oxa.approval.dangerous`); user settings only.
pub const SETTING_DANGEROUS: &str = "oxa.approval.dangerous";

/// The owner setting that names root actions (`oxa.approval.rootActions`); user settings only.
pub const SETTING_ROOT_ACTIONS: &str = "oxa.approval.rootActions";

/// Every built-in rule: (id, one-line reason).
pub const BUILTIN_RULES: &[(&str, &str)] = &[
    (
        "rm-recursive-outside",
        "recursive delete outside the workspace, of / or ~, or of a glob or variable at the top",
    ),
    (
        "git-push-force",
        "git push with a force flag, --mirror, --delete or a +refspec",
    ),
    (
        "credential-write",
        "write, delete or redirect into a credential, agent or token path",
    ),
    (
        "pipe-to-shell",
        "a pipeline or heredoc feeds a shell, or an interpreter is fed by curl or wget",
    ),
    (
        "disk-destroy",
        "raw disk write, mkfs, wipefs, partitioning or a redirect to a disk device",
    ),
    (
        "chmod-recursive-open",
        "recursive chmod to world-writable modes, or chown -R outside the workspace",
    ),
    ("sudo", "privilege escalation: sudo, doas, su, pkexec or run0"),
    (
        "sql-drop",
        "SQL DROP of a table, database or schema, or a TRUNCATE statement",
    ),
    (
        "kill-foreign",
        "kill of processes by name, of pid 1, of -1 or of a process group",
    ),
    (
        "oxa-self-approve",
        "oxa send or oxa approve, or a write into the agent directory",
    ),
    (
        "eval-dynamic",
        "eval or source of a command the shell builds at run time",
    ),
    (
        "credential-read",
        "reads a credential store, prints a login token, or reads another process's environment",
    ),
    (
        "git-hook-write",
        "writes a git hook, git config or gitdir pointer: code git runs later, outside this session",
    ),
    ("unparsed", "the command line cannot be parsed (fail closed)"),
];

/// One match: the rule, its reason, and the simple command that matched, re-joined with
/// single spaces (redirections included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerHit {
    pub rule: String,
    pub reason: String,
    pub segment: String,
}

impl DangerHit {
    /// The string stored for a session or always grant: `rule segment`.
    pub fn key(&self) -> String {
        format!("{} {}", self.rule, self.segment)
    }
}

/// The enabled rules: built-in ids and custom `cmd:<words>` prefixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerRules {
    builtin: Vec<&'static str>,
    custom: Vec<Vec<String>>,
    /// Owner-named root actions (`oxa.approval.rootActions`), pinned to protected executables.
    root_actions: Vec<RootAction>,
}

/// Recursion cap for nested `$( )`, backticks and `sh -c` strings.
const MAX_DEPTH: usize = 8;

/// How many directories a `cd` behind `&&`/`||` may leave possible before the rest of the line
/// is treated as unknown.
const MAX_POSSIBLE_DIRS: usize = 8;

const RESERVED: &[&str] = &[
    "{", "}", "!", "if", "then", "else", "elif", "do", "while", "until",
];
pub(crate) const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
const INTERPRETERS: &[&str] = &["python", "python3", "perl", "ruby", "node"];
const WRITE_OPS: &[&str] = &[">", ">>", ">|", ">&", "&>", "&>>", "<>"];
const SUDO_WITH_ARG: &[&str] = &[
    "-u", "-g", "-C", "-D", "-p", "-r", "-t", "-U", "--user", "--group",
];
const XARGS_WITH_ARG: &[&str] = &[
    "-a",
    "-d",
    "-E",
    "-I",
    "-L",
    "-n",
    "-P",
    "-s",
    "--arg-file",
    "--delimiter",
    "--max-args",
    "--max-procs",
];

impl DangerRules {
    /// Every built-in rule, no custom ones.
    pub fn builtin() -> DangerRules {
        DangerRules {
            builtin: BUILTIN_RULES.iter().map(|row| row.0).collect(),
            custom: Vec::new(),
            root_actions: Vec::new(),
        }
    }

    /// The rules named by `entries`, which the caller has already patched (`apply_list_patch`
    /// over the built-in ids): built-in ids, and `cmd:<words>` for a custom rule. Unknown
    /// entries are ignored and reported in the returned warnings, never an error.
    pub fn from_list(entries: &[String]) -> (DangerRules, Vec<String>) {
        let mut builtin: Vec<&'static str> = Vec::new();
        let mut custom: Vec<Vec<String>> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for entry in entries {
            let e = entry.trim();
            if let Some(rest) = e.strip_prefix("cmd:") {
                let words: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
                if words.is_empty() {
                    warnings.push(format!("{SETTING_DANGEROUS}: empty `cmd:` rule ignored"));
                } else if !custom.contains(&words) {
                    custom.push(words);
                }
            } else if let Some(row) = BUILTIN_RULES.iter().find(|row| row.0 == e) {
                if !builtin.contains(&row.0) {
                    builtin.push(row.0);
                }
            } else {
                warnings.push(format!("{SETTING_DANGEROUS}: unknown rule `{e}` ignored"));
            }
        }
        (
            DangerRules {
                builtin,
                custom,
                root_actions: Vec::new(),
            },
            warnings,
        )
    }

    /// The owner's named root actions (`oxa.approval.rootActions`, user settings only): each
    /// entry is `<name>: <command words>`, where `*` matches exactly one word. A line that is
    /// exactly one simple command, `/usr/bin/sudo` (optionally `-n`) and a listed command,
    /// raises no `sudo` hit; every other rule still applies. Sudo itself still decides whether
    /// a password is needed. Invalid entries are ignored and reported in the warnings.
    pub fn with_root_actions(mut self, entries: &[String]) -> (DangerRules, Vec<String>) {
        let mut warnings = Vec::new();
        for entry in entries {
            let action = entry
                .split_once(':')
                .filter(|(name, _)| !name.trim().is_empty())
                .and_then(|(_, command)| RootAction::parse(command));
            match action {
                Some(action) => {
                    if !self.root_actions.contains(&action) {
                        self.root_actions.push(action);
                    }
                }
                None => warnings.push(format!(
                    "{SETTING_ROOT_ACTIONS}: entry `{}` ignored (expected `<name>: <protected absolute executable> [arguments]`)",
                    entry.trim()
                )),
            }
        }
        (self, warnings)
    }

    /// `true` for a listed action invoked via the absolute system sudo path.
    fn is_root_action(&self, command: &str, stages: &[Stage]) -> bool {
        if self.root_actions.is_empty() {
            return false;
        }
        let Some(words) = literal_sudo_words(command, stages) else {
            return false;
        };
        let argv: &[String] = match words {
            // Even a call that cannot use the foreground broker must not resolve sudo via PATH.
            [sudo, rest @ ..] if sudo == "/usr/bin/sudo" => match rest {
                [flag, tail @ ..] if flag == "-n" => tail,
                _ => rest,
            },
            _ => return false,
        };
        // The launcher chain must be just that `sudo`: the same argv comes out of `effects::of`.
        let fx = effects::of(words, &[], false);
        fx.privileged && fx.argv == argv && self.root_actions.iter().any(|action| action.matches(argv))
    }

    /// Every simple command of `command` that matches an enabled rule. `cwd` is the workspace
    /// (relative paths are taken from it) and `guard` decides which paths are credentials.
    pub fn classify(&self, command: &str, cwd: &Path, guard: &PathGuard) -> Vec<DangerHit> {
        let ctx = Ctx {
            cwd,
            root: cwd,
            guard,
        };
        let mut hits: Vec<DangerHit> = Vec::new();
        match parse_line(command) {
            Some(stages) => {
                self.check_stages(&stages, &ctx, 0, false, &mut hits);
                if self.is_root_action(command, &stages) {
                    hits.retain(|hit| hit.rule != "sudo");
                }
            }
            None => self.push(&mut hits, "unparsed", command.trim()),
        }
        hits
    }

    /// The publish rule (owner decision 2026-11-19), a floor prompt next to the built-in rules
    /// but not one of them: it is not listed in `BUILTIN_RULES` and `oxa.approval.dangerous`
    /// cannot turn it off. A hit is a git push or gh command beyond the session's own
    /// non-default branch, or any command chained with a publish command (see `publish.rs`).
    pub fn publish_classify(&self, command: &str, cwd: &Path) -> Vec<DangerHit> {
        crate::publish::record_baseline(cwd);
        let Some(stages) = parse_line(command) else {
            return vec![DangerHit {
                rule: "publish-unsafe".to_string(),
                reason: "the publish approval floor could not safely parse this command".to_string(),
                segment: command.trim().to_string(),
            }];
        };
        crate::publish::publish_hits(&stages, cwd)
            .into_iter()
            .map(|segment| DangerHit {
                rule: "publish-unsafe".to_string(),
                reason: "git push beyond the session's own non-default branch, gh merge/release/repo changes, or a command chained with a publish command".to_string(),
                segment,
            })
            .collect()
    }

    /// Adds a hit for a built-in rule (only when enabled) or for a custom `cmd:` rule.
    fn push(&self, hits: &mut Vec<DangerHit>, rule: &str, segment: &str) {
        let reason = match BUILTIN_RULES.iter().find(|row| row.0 == rule) {
            Some(row) => {
                if !self.builtin.contains(&rule) {
                    return;
                }
                row.1.to_string()
            }
            None => format!("custom rule {rule}"),
        };
        if hits.iter().any(|h| h.rule == rule && h.segment == segment) {
            return;
        }
        hits.push(DangerHit {
            rule: rule.to_string(),
            reason,
            segment: segment.to_string(),
        });
    }

    fn check_stages(
        &self,
        stages: &[Stage],
        ctx: &Ctx<'_>,
        depth: usize,
        unknown: bool,
        hits: &mut Vec<DangerHit>,
    ) {
        // The names of the earlier stages of the current pipeline (`curl x | python3`).
        let mut pipeline: Vec<String> = Vec::new();
        // A literal `cd` moves the directory the later stages run in; a computed one makes
        // their relative targets unknown. A `cd` behind `&&`/`||` may be skipped, so both the
        // old and the new directory stay possible.
        let mut dirs: Vec<PathBuf> = vec![ctx.cwd.to_path_buf()];
        let mut untracked = false;
        // The parent's directories, saved at each subshell boundary.
        let mut scopes: Vec<(Vec<PathBuf>, bool)> = Vec::new();
        for (index, stage) in stages.iter().enumerate() {
            match stage.mark {
                Some(Mark::Open) => {
                    scopes.push((dirs.clone(), untracked));
                    continue;
                }
                Some(Mark::Close) => {
                    if let Some((outer, outer_untracked)) = scopes.pop() {
                        dirs = outer;
                        untracked = outer_untracked;
                    }
                    continue;
                }
                None => {}
            }
            if !stage.pipe_in {
                pipeline.clear();
            }
            let before = hits.len();
            let mut name = None;
            for dir in &dirs {
                let scoped = Ctx {
                    cwd: dir.as_path(),
                    root: ctx.root,
                    guard: ctx.guard,
                };
                let found = self.check_words(
                    &stage.words,
                    &stage.redirects,
                    &stage.bodies,
                    stage.pipe_in,
                    &pipeline,
                    unknown || untracked,
                    &scoped,
                    depth,
                    hits,
                );
                name = name.or(found);
            }
            // The same hit found from several possible directories is reported once.
            let mut k = before;
            while k < hits.len() {
                let dup = hits[before..k]
                    .iter()
                    .any(|seen| seen.rule == hits[k].rule && seen.segment == hits[k].segment);
                if dup {
                    hits.remove(k);
                } else {
                    k += 1;
                }
            }
            // `cd X | cat` runs `cd` in a subshell: the parent keeps its directory.
            let piped = stage.pipe_in
                || stages
                    .get(index + 1)
                    .filter(|next| next.mark.is_none())
                    .is_some_and(|next| next.pipe_in);
            if !piped && !stage.background {
                let mut changed = Vec::new();
                let mut is_cd = false;
                for dir in &dirs {
                    match cd_target(
                        &stage.words,
                        &Ctx {
                            cwd: dir,
                            root: ctx.root,
                            guard: ctx.guard,
                        },
                    ) {
                        Some(Some(next)) => {
                            is_cd = true;
                            // An existing non-directory cannot become the shell's cwd.  For a
                            // missing path retain both outcomes: an earlier command may create it,
                            // but a failed `cd` leaves the shell in its previous directory.
                            let exists = next.exists();
                            let is_dir = next.is_dir();
                            if (is_dir || !exists) && !changed.contains(&next) {
                                changed.push(next);
                            }
                            if !is_dir && !changed.contains(dir) {
                                changed.push(dir.clone());
                            }
                        }
                        Some(None) => {
                            is_cd = true;
                            untracked = true;
                        }
                        None => {}
                    }
                }
                if is_cd {
                    if changed.is_empty() {
                        changed = dirs.clone();
                    }
                    if stage.conditional {
                        for dir in &dirs {
                            if !changed.contains(dir) {
                                changed.push(dir.clone());
                            }
                        }
                    }
                    if changed.len() > MAX_POSSIBLE_DIRS {
                        changed.truncate(MAX_POSSIBLE_DIRS);
                        untracked = true;
                    }
                    dirs = changed;
                }
            }
            if let Some(name) = name {
                pipeline.push(name);
            }
        }
    }

    /// Checks one simple command: its redirections, then the command after the wrappers
    /// (`sudo`, `env`, `nice`, `xargs`, ...) are peeled off. Returns the command name, for the
    /// pipeline of the next stage.
    #[allow(clippy::too_many_arguments)]
    fn check_words(
        &self,
        words: &[String],
        redirects: &[(String, String)],
        bodies: &[String],
        pipe_in: bool,
        prior: &[String],
        unknown_in: bool,
        ctx: &Ctx<'_>,
        depth: usize,
        hits: &mut Vec<DangerHit>,
    ) -> Option<String> {
        let seg = segment(words, redirects);
        if depth > MAX_DEPTH {
            self.push(hits, "unparsed", &seg);
            return None;
        }
        let fx = effects::of(words, redirects, unknown_in);
        let mut unknown = fx.unknown_targets;
        // A redirection belongs to the simple command, whatever wraps it, and the shell opens
        // it in the current directory before `env` moves: `env -C / true > .npmrc` writes here.
        for (op, target) in redirects {
            if WRITE_OPS.contains(&op.as_str()) {
                self.write_hit(target, &seg, ctx, hits);
            }
        }
        // `env -C <dir> cmd`: the wrapped command runs in <dir>; a chain of them composes, and a
        // directory the shell computes makes the targets unknown.
        let mut moved: Option<PathBuf> = None;
        for dir in &fx.dirs {
            let base = Ctx {
                cwd: moved.as_deref().unwrap_or(ctx.cwd),
                root: ctx.root,
                guard: ctx.guard,
            };
            match base.resolve(dir) {
                // The shell chdirs into the directory itself, so a symbolic link is followed before the next
                // wrapper's `..`: `env -C link env -C ..` lands in the target's parent (#1768).
                Some(next) => moved = Some(std::fs::canonicalize(&next).unwrap_or(next)),
                None => unknown = true,
            }
        }
        let scoped = moved.as_ref().map(|dir| Ctx {
            cwd: dir.as_path(),
            root: ctx.root,
            guard: ctx.guard,
        });
        let ctx = scoped.as_ref().unwrap_or(ctx);
        let heredoc = redirects
            .iter()
            .any(|(op, _)| matches!(op.as_str(), "<<" | "<<-" | "<<<"));
        let mut inline = fx.inline.clone();
        let argv: &[String] = &fx.argv;
        // The words SQL can hide in: the command's own, here-strings and heredoc bodies.
        let mut sql_words: Vec<String> = words.to_vec();
        sql_words.extend(
            redirects
                .iter()
                .filter(|(op, _)| op == "<<<")
                .map(|(_, target)| target.clone()),
        );
        sql_words.extend(bodies.iter().cloned());
        // Privilege escalation is reported before the peeled argv is inspected: `su` and
        // `su -c ...` run as root even though nothing is left to peel after them.
        if fx.privileged {
            self.push(hits, "sudo", &seg);
        }
        if !argv.is_empty() {
            let name_raw = argv[0].as_str();
            let name = base_name(name_raw);
            // The command name itself is computed at run time: `$x -rf /`, `$(which rm)`.
            // A brace expansion builds the words at run time too: `{rm,-rf,/}` runs `rm -rf /`.
            // An unquoted glob in the name picks the program at run time too (`/bin/r?`).
            let glob_name = name_raw.contains(['*', '?', '[']) && !matches!(name_raw, "[" | "[[");
            if has_dynamic(name_raw) || brace_expansion_at(name_raw).is_some() || glob_name {
                self.push(hits, "eval-dynamic", &seg);
            }
            // A process substitution read as a script or as stdin: `bash <(curl ..)`,
            // `sh < <(curl ..)`, `source <(curl ..)`.
            let fed = |t: &str| t.starts_with("<(");
            let fed_arg = argv[1..].iter().any(|a| fed(a.as_str()));
            let fed_stdin = redirects
                .iter()
                .any(|(op, t)| op.as_str() == "<" && fed(t.as_str()));
            let fed_download = argv[1..]
                .iter()
                .chain(redirects.iter().map(|(_, t)| t))
                .any(|t| fed(t.as_str()) && (t.contains("curl") || t.contains("wget")));
            // A shell at the far end of an output process substitution reads this command's
            // output: `curl .. > >(bash)`, `tee >(sh) < file`.
            let output_fed_shell = argv[1..]
                .iter()
                .chain(redirects.iter().map(|(_, t)| t))
                .filter_map(|t| t.strip_prefix(">(").and_then(|rest| rest.strip_suffix(')')))
                .any(|inner| {
                    inner
                        .split_whitespace()
                        .next()
                        .is_some_and(|w| SHELLS.contains(&base_name(w)))
                });
            if output_fed_shell {
                self.push(hits, "pipe-to-shell", &seg);
            }
            if SHELLS.contains(&name) {
                if pipe_in || heredoc || fed_arg || fed_stdin {
                    self.push(hits, "pipe-to-shell", &seg);
                }
                if let Some(script) = shell_script(argv) {
                    inline.push(script);
                }
                for (op, target) in redirects {
                    if op.as_str() == "<<<" {
                        inline.push(target.clone());
                    }
                }
            }
            if INTERPRETERS.contains(&name)
                && ((pipe_in && prior.iter().any(|p| p == "curl" || p == "wget")) || fed_download)
            {
                self.push(hits, "pipe-to-shell", &seg);
            }
            let args = &argv[1..];
            match name {
                "eval" => {
                    if args.iter().any(|a| has_dynamic(a)) {
                        self.push(hits, "eval-dynamic", &seg);
                    } else if !args.is_empty() {
                        inline.push(args.join(" "));
                    }
                }
                "source" | "." => {
                    if args.iter().any(|a| has_dynamic(a)) {
                        self.push(hits, "eval-dynamic", &seg);
                    }
                    if fed_arg || fed_stdin {
                        self.push(hits, "pipe-to-shell", &seg);
                    }
                }
                _ => {}
            }
        }
        for script in &inline {
            match parse_line(script) {
                Some(stages) => self.check_stages(&stages, ctx, depth + 1, unknown, hits),
                None => self.push(hits, "unparsed", script),
            }
        }
        if argv.is_empty() {
            self.credential_read(&fx, &seg, ctx, hits);
            return None;
        }
        self.custom_hits(argv, &seg, hits);
        self.command_hits(argv, unknown, depth, ctx, &seg, hits, &sql_words);
        self.credential_read(&fx, &seg, ctx, hits);
        Some(base_name(&argv[0]).to_string())
    }

    /// The `credential-read` rule: the command prints a login token, names a path on the guard's
    /// deny-list, or names another process's environment. It applies to every command name
    /// (`cat`, `cp`, `base64`, `curl -d @file`, `tar`), not a per-command list. A command that
    /// already asks to write a credential path is not asked twice.
    fn credential_read(&self, fx: &Effects, seg: &str, ctx: &Ctx<'_>, hits: &mut Vec<DangerHit>) {
        if !self.builtin.contains(&"credential-read") {
            return;
        }
        if hits
            .iter()
            .any(|h| h.segment == seg && matches!(h.rule.as_str(), "credential-write" | "oxa-self-approve"))
        {
            return;
        }
        let hit = fx.credential_source
            || fx.reads.iter().any(|raw| {
                effects::is_proc_environ(raw)
                    || ctx
                        .resolve(raw)
                        .is_some_and(|p| ctx.guard.denies(&p) && ctx.guard.denies_read(&p))
            });
        if hit {
            self.push(hits, "credential-read", seg);
        }
    }

    /// The `cmd:<words>` rules: the leading words of `argv` (basename on the first) equal them.
    fn custom_hits(&self, argv: &[String], seg: &str, hits: &mut Vec<DangerHit>) {
        for rule in &self.custom {
            let Some(head) = rule.first() else { continue };
            if argv.len() < rule.len() || base_name(&argv[0]) != head.as_str() {
                continue;
            }
            if argv[1..rule.len()]
                .iter()
                .zip(rule[1..].iter())
                .all(|(a, b)| a == b)
            {
                let id = format!("cmd:{}", rule.join(" "));
                self.push(hits, &id, seg);
            }
        }
    }

    /// The built-in rules of one command, by its name and arguments.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::collapsible_match)]
    fn command_hits(
        &self,
        argv: &[String],
        unknown: bool,
        depth: usize,
        ctx: &Ctx<'_>,
        seg: &str,
        hits: &mut Vec<DangerHit>,
        words: &[String],
    ) {
        let name = base_name(&argv[0]);
        let args = &argv[1..];
        let ops = operands(args);
        if name.starts_with("mkfs") || matches!(name, "wipefs" | "fdisk" | "sfdisk" | "parted") {
            self.push(hits, "disk-destroy", seg);
        }
        match name {
            "rm" => {
                if is_recursive(args) {
                    if unknown || ops.iter().any(|t| target_outside(t, ctx)) {
                        self.push(hits, "rm-recursive-outside", seg);
                    } else if ops
                        .iter()
                        .any(|t| ctx.resolve(t).is_some_and(|p| ctx.guard.denies_within(&p)))
                    {
                        // A directory that holds a credential or agent path takes it along.
                        self.push(hits, "credential-write", seg);
                    }
                }
                for t in &ops {
                    self.write_hit(t, seg, ctx, hits);
                }
            }
            "tee" | "touch" | "truncate" | "unlink" | "rmdir" => {
                for t in &ops {
                    self.write_hit(t, seg, ctx, hits);
                }
            }
            "shred" => {
                if args.iter().any(|a| a.starts_with("/dev/")) {
                    self.push(hits, "disk-destroy", seg);
                }
                for t in &ops {
                    self.write_hit(t, seg, ctx, hits);
                }
            }
            "chmod" => {
                if is_recursive(args) && matches!(ops.first(), Some(mode) if mode_is_open(mode)) {
                    self.push(hits, "chmod-recursive-open", seg);
                }
                for t in ops.iter().skip(1) {
                    self.write_hit(t, seg, ctx, hits);
                }
            }
            "chown" | "chgrp" => {
                if is_recursive(args) && (unknown || ops.iter().skip(1).any(|t| target_outside(t, ctx))) {
                    self.push(hits, "chmod-recursive-open", seg);
                }
                for t in ops.iter().skip(1) {
                    self.write_hit(t, seg, ctx, hits);
                }
            }
            "cp" | "ln" | "install" | "mv" => {
                // `-t DIR` / `--target-directory[=]DIR` names the destination; the operands are
                // then the sources, so every one of them is written into that directory.
                match target_directory(args) {
                    Some(dir) => {
                        self.write_hit(&dir, seg, ctx, hits);
                        // `<dir>/<source name>`: joining an absolute source would drop the
                        // directory and name the source itself.
                        for t in ops.iter().filter(|t| dir != **t) {
                            let name = Path::new(t).file_name().map(|n| n.to_string_lossy().into_owned());
                            let target = match name {
                                Some(name) => format!("{}/{}", dir.trim_end_matches('/'), name),
                                None => dir.clone(),
                            };
                            self.write_hit(&target, seg, ctx, hits);
                        }
                    }
                    None => {
                        if let Some(t) = ops.last() {
                            self.write_hit(t, seg, ctx, hits);
                            // A directory destination writes each source's own name into it.
                            let dir_like =
                                t.ends_with('/') || ctx.resolve(t).is_some_and(|path| path.is_dir());
                            if dir_like && ops.len() > 1 {
                                let dir = t.trim_end_matches('/');
                                for source in ops.iter().take(ops.len() - 1) {
                                    if let Some(name) = Path::new(source).file_name() {
                                        let target = format!("{dir}/{}", name.to_string_lossy());
                                        self.write_hit(&target, seg, ctx, hits);
                                    }
                                }
                            }
                        }
                    }
                }
                // `mv` also removes its sources.
                if name == "mv" {
                    for t in &ops {
                        self.write_hit(t, seg, ctx, hits);
                    }
                }
            }
            "sed" => {
                if args
                    .iter()
                    .any(|a| short_has(a, 'i') || a.starts_with("--in-place"))
                {
                    // With `-e`/`--expression`/`-f`/`--file` the script is not an operand, so
                    // the first input file is an operand.
                    let script_in_option = args.iter().any(|a| {
                        a.starts_with("--expression")
                            || a.starts_with("--file")
                            || (a.starts_with('-')
                                && !a.starts_with("--")
                                && (a.contains('e') || a.contains('f')))
                    });
                    let skip = usize::from(!script_in_option);
                    for t in ops.iter().skip(skip) {
                        self.write_hit(t, seg, ctx, hits);
                    }
                }
            }
            "dd" => {
                for a in args {
                    if let Some(t) = a.strip_prefix("of=") {
                        if t.starts_with("/dev/") {
                            self.push(hits, "disk-destroy", seg);
                        }
                        self.write_hit(t, seg, ctx, hits);
                    }
                }
            }
            // A downloader's output file is written like any other target: `-o`, `--output`
            // and `--output-dir` (with `-O`/`--remote-name`, the URL's own name).
            "curl" | "wget" => {
                let mut output_dir: Option<&str> = None;
                let mut outputs: Vec<&str> = Vec::new();
                let mut remote_name = false;
                let mut k = 0usize;
                while k < args.len() {
                    let a = args[k].as_str();
                    if let Some(target) = a.strip_prefix("--output-dir=") {
                        output_dir = Some(target);
                        k += 1;
                    } else if let Some(target) = a
                        .strip_prefix("--output=")
                        .or_else(|| a.strip_prefix("--output-document="))
                    {
                        outputs.push(target);
                        k += 1;
                    } else if name == "wget" && a.starts_with("-P") && a.len() > 2 && !a.starts_with("--") {
                        output_dir = Some(&a[2..]);
                        k += 1;
                    } else if a.starts_with("-o") && a.len() > 2 && !a.starts_with("--") {
                        outputs.push(&a[2..]);
                        k += 1;
                    } else if name == "wget" && a.starts_with("-O") && a.len() > 2 && !a.starts_with("--") {
                        outputs.push(&a[2..]);
                        k += 1;
                    } else if matches!(a, "--output-dir" | "-P") {
                        if let Some(target) = args.get(k + 1) {
                            output_dir = Some(target);
                        }
                        k += 2;
                    } else if matches!(a, "-o" | "--output" | "--output-document")
                        || (a == "-O" && name == "wget")
                    {
                        if let Some(target) = args.get(k + 1) {
                            outputs.push(target);
                        }
                        k += 2;
                    } else if (a == "-O" && name == "curl") || a == "--remote-name" {
                        remote_name = true;
                        k += 1;
                    } else {
                        k += 1;
                    }
                }
                if remote_name {
                    if let Some(url) = args.iter().rev().find(|a| !a.starts_with('-')) {
                        if let Some(file) = url.split(['/', '?']).rfind(|part| !part.is_empty()) {
                            outputs.push(file);
                        }
                    }
                }
                for target in outputs {
                    let target = match output_dir.filter(|_| Path::new(target).is_relative()) {
                        Some(dir) => Path::new(dir).join(target).to_string_lossy().into_owned(),
                        None => target.to_string(),
                    };
                    self.write_hit(&target, seg, ctx, hits);
                }
            }
            "git" => self.git_hits(argv, seg, hits),
            "find" => self.find_hits(args, unknown, depth, ctx, seg, hits),
            "kill" => {
                if kill_foreign(args) {
                    self.push(hits, "kill-foreign", seg);
                }
            }
            "pkill" | "killall" => self.push(hits, "kill-foreign", seg),
            "oxa" => {
                // Global flags (`--offline`, `--model x`, `-q`) come before the verb, and some
                // take a value: skip both forms so `oxa --model send ..` is not read as the verb.
                let mut k = 1usize;
                while k < argv.len() && argv[k].starts_with('-') {
                    let takes_value = !argv[k].contains('=')
                        && matches!(
                            argv[k].as_str(),
                            "--model" | "--provider" | "--session-dir" | "--permission-mode" | "-m"
                        );
                    k += if takes_value { 2 } else { 1 };
                }
                let by_argv = matches!(argv.get(k).map(String::as_str), Some("send") | Some("approve"));
                // The CLI also takes leading switches before a subcommand (`oxa --offline send
                // ..`): a word that is not an option is the subcommand, and either reading marks
                // the self-approve rule.
                let by_args = args
                    .iter()
                    .map(String::as_str)
                    .find(|a| !a.starts_with('-'))
                    .is_some_and(|sub| matches!(sub, "send" | "approve"));
                if by_argv || by_args {
                    self.push(hits, "oxa-self-approve", seg);
                }
            }
            _ => {}
        }
        if sql_drop(words) {
            self.push(hits, "sql-drop", seg);
        }
    }

    /// `git push` with a force flag, a mirror or delete, or a `+refspec`.
    fn git_hits(&self, argv: &[String], seg: &str, hits: &mut Vec<DangerHit>) {
        // Global options (`git -C dir -c k=v`) come before the subcommand.
        let mut k = 1usize;
        while k < argv.len() {
            let a = argv[k].as_str();
            if matches!(a, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
                k += 2;
            } else if a.starts_with('-') {
                k += 1;
            } else {
                break;
            }
        }
        // `git -c core.hooksPath=/tmp/h commit` runs that hook for this one command.
        let mut c = 1usize;
        while c < k {
            if argv[c] == "-c" {
                if let Some(setting) = argv.get(c + 1) {
                    if program_git_key(setting.split('=').next().unwrap_or("")) {
                        self.push(hits, "git-hook-write", seg);
                    }
                }
                c += 2;
            } else {
                c += 1;
            }
        }
        if argv.get(k).map(String::as_str) == Some("config") && git_config_sets_program(&argv[k + 1..]) {
            self.push(hits, "git-hook-write", seg);
        }
        if argv.get(k).map(String::as_str) != Some("push") {
            return;
        }
        let rest = &argv[k + 1..];
        let force = rest.iter().any(|a| {
            let a = a.as_str();
            a.starts_with("--force")
                || a == "--mirror"
                || a == "--delete"
                || (!a.starts_with("--") && a.starts_with('-') && (short_has(a, 'f') || short_has(a, 'd')))
        });
        let refspec = rest.iter().any(|a| {
            a.starts_with('+')
                || a.split_once(':')
                    .is_some_and(|(source, destination)| source.is_empty() && !destination.is_empty())
        });
        if force || refspec {
            self.push(hits, "git-push-force", seg);
        }
    }

    /// `find` roots, `-delete` and `-exec`/`-ok` commands. A command run by `-exec` has
    /// unknown targets (`{}`) when the roots are outside the workspace.
    fn find_hits(
        &self,
        args: &[String],
        unknown: bool,
        depth: usize,
        ctx: &Ctx<'_>,
        seg: &str,
        hits: &mut Vec<DangerHit>,
    ) {
        // The traversal and depth options (`-P`, `-L`, `-H`, `-D debug`, `-O2`) come before the
        // roots and are not roots themselves.
        let mut from = 0usize;
        while from < args.len() {
            match args[from].as_str() {
                "-P" | "-L" | "-H" | "-O0" | "-O1" | "-O2" | "-O3" => from += 1,
                "-D" => from += 2,
                _ => break,
            }
        }
        let roots: Vec<&str> = args
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .take_while(|a| !is_find_expr(a))
            .collect();
        // Without a root `find` searches the current directory.
        let roots_outside = if roots.is_empty() {
            target_outside(".", ctx)
        } else {
            roots.iter().any(|r| target_outside(r, ctx))
        };
        let outside = unknown || roots_outside;
        if args.iter().any(|a| a == "-delete") {
            if outside {
                self.push(hits, "rm-recursive-outside", seg);
            } else {
                let held = if roots.is_empty() {
                    vec!["."]
                } else {
                    roots.clone()
                };
                if held
                    .iter()
                    .any(|r| ctx.resolve(r).is_some_and(|p| ctx.guard.denies_within(&p)))
                {
                    self.push(hits, "credential-write", seg);
                }
            }
        }
        let mut k = 0usize;
        while k < args.len() {
            if matches!(args[k].as_str(), "-fprint" | "-fprint0" | "-fls" | "-fprintf") {
                if let Some(target) = args.get(k + 1) {
                    self.write_hit(target, seg, ctx, hits);
                }
                k += if args[k] == "-fprintf" { 3 } else { 2 };
                continue;
            }
            if matches!(args[k].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let in_matched_dir = matches!(args[k].as_str(), "-execdir" | "-okdir");
                let end = args[k + 1..]
                    .iter()
                    .position(|a| a == ";" || a == "+")
                    .map_or(args.len(), |e| k + 1 + e);
                let cmd = &args[k + 1..end];
                if !cmd.is_empty() {
                    if in_matched_dir {
                        let roots = if roots.is_empty() {
                            vec!["."]
                        } else {
                            roots.clone()
                        };
                        for root in roots {
                            let Some(path) = ctx.resolve(root) else {
                                continue;
                            };
                            // A root itself can be matched (so its parent is an execution
                            // directory), while directory roots may also contain later matches.
                            let mut dirs = Vec::new();
                            if let Some(parent) = path.parent() {
                                dirs.push(parent.to_path_buf());
                            }
                            if path.is_dir() {
                                dirs.push(path);
                            }
                            for dir in dirs {
                                let nested = Ctx {
                                    cwd: &dir,
                                    root: ctx.root,
                                    guard: ctx.guard,
                                };
                                self.check_words(
                                    cmd,
                                    &[],
                                    &[],
                                    false,
                                    &[],
                                    outside,
                                    &nested,
                                    depth + 1,
                                    hits,
                                );
                            }
                        }
                    } else {
                        self.check_words(cmd, &[], &[], false, &[], outside, ctx, depth + 1, hits);
                    }
                }
                k = end + 1;
            } else {
                k += 1;
            }
        }
    }

    /// A write to `raw` (a redirect or a file operand).
    fn write_hit(&self, raw: &str, seg: &str, ctx: &Ctx<'_>, hits: &mut Vec<DangerHit>) {
        let Some(path) = write_target(raw, ctx) else {
            return;
        };
        if ctx.guard.denies(&path) {
            self.push(hits, "credential-write", seg);
        }
        if is_inside(&path, &ctx.guard.agent_dir) {
            self.push(hits, "oxa-self-approve", seg);
        }
        if is_git_hook_target(&path, ctx.root) {
            self.push(hits, "git-hook-write", seg);
        }
        // The path as written and where the file system resolves it: `/dev/disk/by-id/...` and
        // other symlinks name a disk device too.
        let real = resolve_real(&path);
        let on_disk = |p: &Path| {
            let p = p.to_string_lossy();
            [
                "/dev/sd",
                "/dev/nvme",
                "/dev/mmcblk",
                "/dev/vd",
                "/dev/xvd",
                "/dev/hd",
            ]
            .iter()
            .any(|prefix| p.starts_with(prefix))
        };
        if on_disk(&path) || real.as_deref().is_some_and(on_disk) {
            self.push(hits, "disk-destroy", seg);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootAction {
    words: Vec<String>,
    identity: (u64, u64),
}

impl RootAction {
    fn parse(command: &str) -> Option<Self> {
        let words: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if words.iter().any(|word| has_dynamic(word)) {
            return None;
        }
        let identity = protected_executable(words.first()?)?;
        Some(Self { words, identity })
    }

    fn matches(&self, argv: &[String]) -> bool {
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        let (expected, pattern) = self.words.split_first().expect("validated root action");
        program == expected
            && pattern.len() == args.len()
            && pattern.iter().zip(args).all(|(a, w)| a == "*" || a == w)
            && protected_executable(program) == Some(self.identity)
    }
}

#[cfg(unix)]
fn protected_executable(program: &str) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let path = Path::new(program);
    // Match the actual absolute argv, not an alias sudo might resolve differently. Unlike
    // canonicalizing a tool we launch ourselves, we must reject symlink spellings here.
    if !path.is_absolute() || path.canonicalize().ok()?.as_os_str() != path.as_os_str() {
        return None;
    }
    // Walk from the root: once a parent is verified, the model cannot swap its child entry.
    let chain: Vec<_> = path.ancestors().collect();
    let mut identity = None;
    for entry in chain.into_iter().rev() {
        let info = std::fs::symlink_metadata(entry).ok()?;
        if info.uid() != 0 || info.mode() & 0o022 != 0 {
            return None;
        }
        if entry == path {
            if !info.is_file() || info.mode() & 0o111 == 0 {
                return None;
            }
            identity = Some((info.dev(), info.ino()));
        } else if !info.is_dir() {
            return None;
        }
    }
    identity
}

#[cfg(not(unix))]
fn protected_executable(_: &str) -> Option<(u64, u64)> {
    None
}

/// Literal argv for a direct sudo invocation. Shared by named root actions and the
/// password broker's direct launch; neither may infer an argv from shell expansion.
pub fn literal_sudo_argv(command: &str) -> Option<Vec<String>> {
    let stages = parse_line(command)?;
    literal_sudo_words(command, &stages).map(<[String]>::to_vec)
}

fn literal_sudo_words<'a>(command: &str, stages: &'a [Stage]) -> Option<&'a [String]> {
    // Inspect raw text. The general classifier strips quotes and escapes and does not
    // expand pathname patterns, braces or tildes. Be conservative even within quotes.
    if command
        .chars()
        .any(|c| (c.is_control() && c != '\t') || "$`*?[]{}~\\#;|&<>()!".contains(c))
    {
        return None;
    }
    let [stage] = stages else { return None };
    let [sudo, _, ..] = stage.words.as_slice() else {
        return None;
    };
    matches!(sudo.as_str(), "sudo" | "/usr/bin/sudo" | "/bin/sudo").then_some(stage.words.as_slice())
}

/// `true` when some simple command of `command` runs through a privilege launcher (`sudo`,
/// `doas`, `su`, `pkexec`, `run0`).
pub fn runs_privileged(command: &str) -> bool {
    parse_line(command).is_some_and(|stages| {
        stages
            .iter()
            .filter(|st| st.mark.is_none())
            .any(|st| effects::of(&st.words, &st.redirects, false).privileged)
    })
}

/// A write target git runs later: the worktree's `.git` file, a hooks directory, `config`,
/// `config.worktree`, `commondir` or `gitdir` of any `.git` directory (any depth), or the same
/// files of a linked worktree's git directory.
fn is_git_hook_target(path: &Path, root: &Path) -> bool {
    if crate::policy::protected_write(path, root) == Some(true) {
        return true;
    }
    let parts: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
    parts.windows(2).any(|w| {
        w[0] == ".git"
            && oxa_proto::guard::GIT_PROTECTED_IN_CWD
                .iter()
                .filter_map(|rel| rel.strip_prefix(".git/"))
                .any(|name| name == w[1])
    })
}

/// A git config key (`core.hooksPath`) whose value is a program git runs.
fn program_git_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.strip_prefix("core.")
        .is_some_and(crate::publish::program_running_core_key)
}

/// `git config` arguments that set a program-running key (any scope flag, `--file` included).
fn git_config_sets_program(args: &[String]) -> bool {
    let reads = |a: &str| {
        matches!(
            a,
            "--get"
                | "--get-all"
                | "--get-regexp"
                | "--list"
                | "-l"
                | "--unset"
                | "--unset-all"
                | "--remove-section"
                | "--get-urlmatch"
        )
    };
    if args.iter().any(|a| reads(a)) {
        return false;
    }
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    positional.len() >= 2
        && positional[..positional.len() - 1]
            .iter()
            .any(|key| program_git_key(key))
}

/// The workspace boundary, the directory the current command runs in, and the guard.
struct Ctx<'a> {
    /// Where the current command runs: a `cd` in an earlier stage of the same line moves it.
    cwd: &'a Path,
    /// The workspace boundary, which a `cd` never moves.
    root: &'a Path,
    guard: &'a PathGuard,
}

impl Ctx<'_> {
    /// `raw` as an absolute, normalised path; `None` when the shell computes part of it, or when
    /// lexical normalisation of a `..` would hide where the file system really resolves it (a
    /// symlinked directory before the `..`, as in `link/../victim`).
    fn resolve(&self, raw: &str) -> Option<PathBuf> {
        let expanded = expand_home(raw, &self.guard.home);
        // `~user` is another user's home, which this classifier cannot know; a brace expansion
        // names paths the shell builds.
        if has_dynamic(&expanded) || expanded.starts_with('~') || brace_expansion_at(&expanded).is_some() {
            return None;
        }
        let absolute = normalize(Path::new(&expanded), self.cwd);
        if parent_dir_after_a_link(&expanded, self.cwd) {
            return None;
        }
        Some(absolute)
    }
}

/// The file a write lands on, or `None` when the shell builds the path. A `..` after a symbolic
/// link (`link/../x`) defeats lexical normalisation, so that case is resolved the way the kernel
/// resolves it instead of being dropped.
fn write_target(raw: &str, ctx: &Ctx<'_>) -> Option<PathBuf> {
    if let Some(path) = ctx.resolve(raw) {
        return Some(path);
    }
    let expanded = expand_home(raw, &ctx.guard.home);
    if has_dynamic(&expanded)
        || expanded.starts_with('~')
        || brace_expansion_at(&expanded).is_some()
        || !parent_dir_after_a_link(&expanded, ctx.cwd)
    {
        return None;
    }
    let absolute = if expanded.starts_with('/') {
        PathBuf::from(&expanded)
    } else {
        ctx.cwd.join(&expanded)
    };
    resolve_real(&absolute)
}

/// `true` when a `..` in `raw` comes after a symbolic link: the file system then resolves the
/// `..` inside the link's target, which lexical normalisation cannot see (`link/../victim`).
fn parent_dir_after_a_link(raw: &str, cwd: &Path) -> bool {
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if !parts.contains(&"..") {
        return false;
    }
    let mut so_far = if raw.starts_with('/') {
        PathBuf::from("/")
    } else {
        cwd.to_path_buf()
    };
    for (i, part) in parts.iter().enumerate() {
        if *part == ".." {
            continue;
        }
        so_far.push(*part);
        if std::fs::symlink_metadata(&so_far).is_ok_and(|m| m.file_type().is_symlink())
            && parts[i + 1..].contains(&"..")
        {
            return true;
        }
    }
    false
}

/// One pending `<<`/`<<-`: its delimiter, whether leading tabs are stripped, and whether the
/// body is expanded by the shell (an unquoted delimiter).
struct Heredoc {
    delim: String,
    strip: bool,
    expands: bool,
}

/// The command substitutions of an expanded heredoc body: `$( ... )` and backticks.
fn heredoc_substitutions(body: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < body.len() {
        match body[p] {
            '\\' => p += 2,
            '$' if body.get(p + 1) == Some(&'(') => match scan_paren(body, p + 2) {
                Some((inner, np)) => {
                    out.push(inner);
                    p = np;
                }
                None => return out,
            },
            '`' => match scan_backtick(body, p + 1) {
                Some((inner, np)) => {
                    out.push(inner);
                    p = np;
                }
                None => return out,
            },
            _ => p += 1,
        }
    }
    out
}

/// A tokenized simple command. `pipe_in`: it reads the stdout of the stage before it.
#[derive(Debug, Default)]
pub(crate) struct Stage {
    pub(crate) words: Vec<String>,
    /// (operator, target), e.g. (">>", "~/.ssh/authorized_keys"); `<<` and `<<<` included.
    pub(crate) redirects: Vec<(String, String)>,
    pub(crate) pipe_in: bool,
    /// The previous stage is joined by `&&` or `||`, so this one may not run at all.
    conditional: bool,
    /// This command is followed by `&` and therefore cannot change the parent shell directory.
    pub(crate) background: bool,
    /// A subshell or command-substitution boundary rather than a command.
    pub(crate) mark: Option<Mark>,
    /// The bodies of the heredocs this command reads.
    pub(crate) bodies: Vec<String>,
}

/// The kind of a boundary stage: `(`/`$(`/backtick opens a subshell, `)` closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Open,
    Close,
}

/// Parses `inner`, which runs in a subshell of its own, between its boundary stages.
fn push_subshell(out: &mut Vec<Stage>, inner: &str, depth: usize) -> Option<()> {
    out.push(Stage {
        mark: Some(Mark::Open),
        ..Stage::default()
    });
    out.extend(parse_depth(inner, depth + 1)?);
    out.push(Stage {
        mark: Some(Mark::Close),
        ..Stage::default()
    });
    Some(())
}

/// Splits a line into simple commands, flattening command substitutions, backticks and
/// subshells into the same list. `None` when the line cannot be read (an open quote, a
/// trailing backslash, too much nesting).
pub(crate) fn parse_line(src: &str) -> Option<Vec<Stage>> {
    parse_depth(src, 0)
}

fn parse_depth(src: &str, depth: usize) -> Option<Vec<Stage>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out: Vec<Stage> = Vec::new();
    let mut cur = Stage::default();
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut p = 0usize;
    while p < n {
        let c = chars[p];
        let next = chars.get(p + 1).copied();
        match c {
            ' ' | '\t' | '\r' => p += 1,
            '\n' => {
                end_stage(&mut out, &mut cur, false);
                p += 1;
                for heredoc in std::mem::take(&mut heredocs) {
                    let end = skip_heredoc(&chars, p, &heredoc.delim, heredoc.strip);
                    let body: String = chars[p..end.min(n)].iter().collect();
                    if let Some(stage) = out.iter_mut().rev().find(|s| {
                        s.redirects
                            .iter()
                            .any(|(op, _)| matches!(op.as_str(), "<<" | "<<-"))
                    }) {
                        stage.bodies.push(body);
                    }
                    // An unquoted delimiter expands the body: its command substitutions run in
                    // the invoking shell before the command sees the text.
                    if heredoc.expands {
                        let body: Vec<char> = chars[p..end.min(chars.len())].to_vec();
                        for inner in heredoc_substitutions(&body) {
                            push_subshell(&mut out, &inner, depth)?;
                        }
                    }
                    p = end;
                }
            }
            ';' => {
                end_stage(&mut out, &mut cur, false);
                p += 1;
            }
            // A subshell keeps its own directory: `cd /; (cd /w); rm -rf var/tmp/victim`.
            '(' | ')' => {
                end_stage(&mut out, &mut cur, false);
                out.push(Stage {
                    mark: Some(if c == '(' { Mark::Open } else { Mark::Close }),
                    ..Stage::default()
                });
                p += 1;
            }
            '&' if next == Some('>') => {
                let (op, len) = if chars.get(p + 2) == Some(&'>') {
                    ("&>>", 3)
                } else {
                    ("&>", 2)
                };
                p = redirect(&chars, p + len, op, depth, &mut cur, &mut heredocs, &mut out)?;
            }
            '&' if next == Some('&') => {
                end_stage(&mut out, &mut cur, false);
                cur.conditional = true;
                p += 2;
            }
            '&' => {
                cur.background = true;
                end_stage(&mut out, &mut cur, false);
                p += 1;
            }
            '|' if next == Some('|') => {
                end_stage(&mut out, &mut cur, false);
                cur.conditional = true;
                p += 2;
            }
            '|' if next == Some('&') => {
                end_stage(&mut out, &mut cur, true);
                p += 2;
            }
            '|' => {
                end_stage(&mut out, &mut cur, true);
                p += 1;
            }
            // A line continuation outside a word is whitespace, not an empty argument.
            '\\' if next == Some('\n') => p += 2,
            '#' => {
                while p < n && chars[p] != '\n' {
                    p += 1;
                }
            }
            // `<(cmd)`: a process substitution is one word (a file name); `cmd` runs too.
            '<' | '>' if next == Some('(') => {
                let (inner, np) = scan_paren(&chars, p + 2)?;
                push_subshell(&mut out, &inner, depth)?;
                cur.words.push(format!("{c}({inner})"));
                p = np;
            }
            '<' | '>' => {
                let (op, len) = redirect_op(&chars, p);
                p = redirect(&chars, p + len, op, depth, &mut cur, &mut heredocs, &mut out)?;
            }
            _ => {
                let (word, np) = read_word(&chars, p, depth, &mut out)?;
                // `2>file`: a digit word glued to a redirection is a file descriptor, not an argument.
                let raw_digits = chars[p..np].iter().all(|ch| ch.is_ascii_digit());
                let fd = raw_digits && matches!(chars.get(np).copied(), Some('<') | Some('>'));
                if !fd {
                    cur.words.push(word);
                }
                p = np;
            }
        }
    }
    end_stage(&mut out, &mut cur, false);
    Some(out)
}

/// Closes the current simple command. `pipe`: the next one reads this one's output.
fn end_stage(out: &mut Vec<Stage>, cur: &mut Stage, pipe: bool) {
    if cur.words.is_empty() && cur.redirects.is_empty() {
        // An empty stage (`( `, `{ `, a doubled separator) keeps a pending pipe.
        cur.pipe_in = cur.pipe_in || pipe;
        return;
    }
    out.push(std::mem::take(cur));
    cur.pipe_in = pipe;
}

/// The operator at `p` (`<`, `>`, `>>`, `<<`, `<<-`, `<<<`, `<>`, `>|`, `>&`) and its length.
fn redirect_op(chars: &[char], p: usize) -> (&'static str, usize) {
    let c = chars[p];
    let nx = chars.get(p + 1).copied();
    let nx2 = chars.get(p + 2).copied();
    if c == '<' {
        match nx {
            Some('<') => {
                if nx2 == Some('<') {
                    ("<<<", 3)
                } else if nx2 == Some('-') {
                    ("<<-", 3)
                } else {
                    ("<<", 2)
                }
            }
            Some('>') => ("<>", 2),
            _ => ("<", 1),
        }
    } else {
        match nx {
            Some('>') => (">>", 2),
            Some('|') => (">|", 2),
            Some('&') => (">&", 2),
            _ => (">", 1),
        }
    }
}

/// Reads the target word of a redirection that ends at `start`.
fn redirect(
    chars: &[char],
    start: usize,
    op: &'static str,
    depth: usize,
    cur: &mut Stage,
    heredocs: &mut Vec<Heredoc>,
    out: &mut Vec<Stage>,
) -> Option<usize> {
    let mut p = start;
    while p < chars.len() && (chars[p] == ' ' || chars[p] == '\t') {
        p += 1;
    }
    let raw_start = p;
    // `sh < <(curl ..)`, `curl .. > >(sh)`: the target is a process substitution, whose
    // command runs too.
    let process_sub = chars.get(p).is_some_and(|c| matches!(*c, '<' | '>')) && chars.get(p + 1) == Some(&'(');
    let (target, np) = if process_sub {
        let (inner, np) = scan_paren(chars, p + 2)?;
        push_subshell(out, &inner, depth)?;
        (format!("{}({inner})", chars[p]), np)
    } else {
        read_word(chars, p, depth, out)?
    };
    if op == "<<" || op == "<<-" {
        // A quoted or escaped delimiter (`<<'EOF'`, `<<"EOF"`, `<<\EOF`) turns expansion off.
        let quoted = chars[raw_start..np]
            .iter()
            .any(|c| matches!(*c, '\'' | '"' | '\\'));
        heredocs.push(Heredoc {
            delim: target.clone(),
            strip: op == "<<-",
            expands: !quoted,
        });
    }
    cur.redirects.push((op.to_string(), target));
    Some(np)
}

/// Reads one word from `start`: quotes and backslashes removed, `$( )` and backticks kept
/// raw in the word (so a dynamic word is visible to the rules) and parsed into `out`.
fn read_word(chars: &[char], start: usize, depth: usize, out: &mut Vec<Stage>) -> Option<(String, usize)> {
    let n = chars.len();
    let mut w = String::new();
    let mut p = start;
    while p < n {
        let c = chars[p];
        match c {
            ' ' | '\t' | '\r' | '\n' | ';' | '|' | '&' | '(' | ')' | '<' | '>' => break,
            '\'' => {
                p += 1;
                loop {
                    let q = *chars.get(p)?;
                    p += 1;
                    if q == '\'' {
                        break;
                    }
                    w.push(q);
                }
            }
            '"' => {
                p += 1;
                loop {
                    let q = *chars.get(p)?;
                    match q {
                        '"' => {
                            p += 1;
                            break;
                        }
                        '\\' => {
                            let nx = *chars.get(p + 1)?;
                            if matches!(nx, '$' | '`' | '"' | '\\') {
                                w.push(nx);
                            } else if nx != '\n' {
                                w.push('\\');
                                w.push(nx);
                            }
                            p += 2;
                        }
                        '$' if chars.get(p + 1) == Some(&'(') => {
                            let (inner, np) = scan_paren(chars, p + 2)?;
                            w.push_str("$(");
                            w.push_str(&inner);
                            w.push(')');
                            push_subshell(out, &inner, depth)?;
                            p = np;
                        }
                        '`' => {
                            let (inner, np) = scan_backtick(chars, p + 1)?;
                            w.push('`');
                            w.push_str(&inner);
                            w.push('`');
                            push_subshell(out, &inner, depth)?;
                            p = np;
                        }
                        _ => {
                            w.push(q);
                            p += 1;
                        }
                    }
                }
            }
            '\\' => {
                let nx = *chars.get(p + 1)?;
                if nx != '\n' {
                    w.push(nx);
                }
                p += 2;
            }
            '$' if chars.get(p + 1) == Some(&'(') => {
                let (inner, np) = scan_paren(chars, p + 2)?;
                w.push_str("$(");
                w.push_str(&inner);
                w.push(')');
                push_subshell(out, &inner, depth)?;
                p = np;
            }
            '`' => {
                let (inner, np) = scan_backtick(chars, p + 1)?;
                w.push('`');
                w.push_str(&inner);
                w.push('`');
                push_subshell(out, &inner, depth)?;
                p = np;
            }
            _ => {
                w.push(c);
                p += 1;
            }
        }
    }
    Some((w, p))
}

/// The raw text up to the `)` that closes a `$(` opened just before `start`, and the index
/// after it. Quotes and backslashes are skipped over while counting parentheses.
pub(crate) fn scan_paren(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut depth = 1usize;
    let mut s = String::new();
    let mut p = start;
    while p < chars.len() {
        let c = chars[p];
        match c {
            '\'' => {
                s.push(c);
                p += 1;
                loop {
                    let q = *chars.get(p)?;
                    s.push(q);
                    p += 1;
                    if q == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                s.push(c);
                p += 1;
                loop {
                    let q = *chars.get(p)?;
                    s.push(q);
                    p += 1;
                    if q == '\\' {
                        let nx = *chars.get(p)?;
                        s.push(nx);
                        p += 1;
                    } else if q == '"' {
                        break;
                    }
                }
            }
            '\\' => {
                s.push(c);
                let nx = *chars.get(p + 1)?;
                s.push(nx);
                p += 2;
            }
            '(' => {
                depth += 1;
                s.push(c);
                p += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((s, p + 1));
                }
                s.push(c);
                p += 1;
            }
            _ => {
                s.push(c);
                p += 1;
            }
        }
    }
    None
}

/// The raw text up to the closing backtick, and the index after it.
pub(crate) fn scan_backtick(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut s = String::new();
    let mut p = start;
    while p < chars.len() {
        let c = chars[p];
        if c == '\\' {
            let nx = *chars.get(p + 1)?;
            s.push(c);
            s.push(nx);
            p += 2;
            continue;
        }
        if c == '`' {
            return Some((s, p + 1));
        }
        s.push(c);
        p += 1;
    }
    None
}

/// Skips a heredoc body that starts at `start` (just after a newline), through the line that
/// is the delimiter. The body is not parsed.
fn skip_heredoc(chars: &[char], start: usize, delim: &str, strip: bool) -> usize {
    let n = chars.len();
    let mut p = start;
    while p < n {
        let mut e = p;
        while e < n && chars[e] != '\n' {
            e += 1;
        }
        let raw: String = chars[p..e].iter().collect();
        let line = raw.trim_end_matches('\r');
        let cmp = if strip {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if cmp == delim {
            return (e + 1).min(n);
        }
        p = e + 1;
    }
    n
}

/// What is left of a simple command after its wrappers.
pub(crate) struct Peeled {
    /// Index of the command name in the words.
    pub(crate) start: usize,
    pub(crate) privileged: bool,
    /// The targets are not known (`xargs`).
    pub(crate) unknown: bool,
    /// Scripts run by the wrapper (`su -c`, `flock -c`, `env -S`).
    pub(crate) inline: Vec<String>,
    /// Every directory the wrappers run the command in (`env -C`, `env --chdir`), in order:
    /// `env -C a env -C b cmd` runs in `a` then `b`.
    pub(crate) dirs: Vec<String>,
}

/// Skips assignments, reserved words and the wrappers that run another command.
pub(crate) fn peel(words: &[String], unknown_in: bool) -> Peeled {
    let mut out = Peeled {
        start: 0,
        privileged: false,
        unknown: unknown_in,
        inline: Vec::new(),
        dirs: Vec::new(),
    };
    let mut i = 0usize;
    loop {
        while i < words.len() && (RESERVED.contains(&words[i].as_str()) || is_assignment(&words[i])) {
            i += 1;
        }
        if i >= words.len() {
            break;
        }
        match base_name(&words[i]) {
            // `function f { rm -rf /; }`: the name, then the body (`{` is reserved).
            "function" => i += 2,
            // `coproc cmd ...` or `coproc NAME { ...; }`.
            "coproc" => {
                i += 1;
                if words.get(i + 1).map(String::as_str) == Some("{") {
                    i += 1;
                }
            }
            "busybox" | "toybox" => i += 1,
            "sudo" | "doas" | "pkexec" | "run0" => {
                out.privileged = true;
                i = skip_flags(words, i + 1, SUDO_WITH_ARG);
            }
            "su" => {
                out.privileged = true;
                out.inline
                    .extend(script_after(words, i + 1, &["-c", "--command"]));
                i = words.len();
            }
            "env" => i = skip_env(words, i + 1, &mut out.inline, &mut out.dirs),
            "xargs" => {
                out.unknown = true;
                i = skip_flags(words, i + 1, XARGS_WITH_ARG);
            }
            "nice" => i = skip_flags(words, i + 1, &["-n", "--adjustment"]),
            "nohup" | "command" | "builtin" | "setsid" => i = skip_flags(words, i + 1, &[]),
            "time" => i = skip_flags(words, i + 1, &["-f", "-o"]),
            "exec" => i = skip_flags(words, i + 1, &["-a"]),
            "stdbuf" => i = skip_flags(words, i + 1, &["-i", "-o", "-e"]),
            "ionice" => i = skip_flags(words, i + 1, &["-c", "-n", "-p", "-P", "-u"]),
            "timeout" => i = skip_positional(words, i + 1, &["-s", "-k", "--signal", "--kill-after"]),
            "chrt" | "taskset" => i = skip_positional(words, i + 1, &[]),
            // `watch` hands its words, joined, to `sh -c` unless `-x`/`--exec` is given.
            "watch" => {
                let from = i + 1;
                i = skip_flags(words, from, &["-n", "--interval"]);
                let exec = words[from..i].iter().any(|w| w == "--exec" || short_has(w, 'x'));
                if !exec && i < words.len() {
                    out.inline.push(words[i..].join(" "));
                    i = words.len();
                }
            }
            "flock" => match script_after(words, i + 1, &["-c", "--command"]) {
                Some(script) => {
                    out.inline.push(script);
                    i = words.len();
                }
                None => i = skip_positional(words, i + 1, &["-w", "--timeout", "-E", "--conflict-exit-code"]),
            },
            _ => break,
        }
    }
    out.start = i.min(words.len());
    out
}

/// Where the next stages run after this one: `None` when it does not change the directory,
/// `Some(Some(dir))` for a literal `cd`/`pushd`, `Some(None)` when the target is computed (a
/// variable, a glob or `cd -`), so later relative targets are unknown.
fn cd_target(words: &[String], ctx: &Ctx<'_>) -> Option<Option<PathBuf>> {
    let peeled = peel(words, false);
    let argv: &[String] = words.get(peeled.start..).unwrap_or(&[]);
    let name = base_name(argv.first()?);
    if name == "eval" {
        let script = argv.get(1..)?.join(" ");
        if script.is_empty() || has_dynamic(&script) {
            return None;
        }
        let stages = parse_line(&script)?;
        let mut current = ctx.cwd.to_path_buf();
        let mut changed = false;
        for stage in stages {
            if stage.mark.is_some() || stage.conditional || stage.background || stage.pipe_in {
                return Some(None);
            }
            let nested = Ctx {
                cwd: &current,
                root: ctx.root,
                guard: ctx.guard,
            };
            if let Some(target) = cd_target(&stage.words, &nested) {
                changed = true;
                match target {
                    Some(dir) => current = dir,
                    None => return Some(None),
                }
            }
        }
        return changed.then_some(Some(current));
    }
    if !matches!(name, "cd" | "pushd") {
        return None;
    }
    // `cd -L`, `cd -P`, `cd --` before the destination.
    let mut k = 1usize;
    while argv
        .get(k)
        .is_some_and(|word| word.starts_with('-') && word.len() > 1 && word.as_str() != "-")
    {
        k += 1;
    }
    match argv.get(k).map(String::as_str) {
        None | Some("") => Some(Some(ctx.guard.home.to_path_buf())),
        Some("-") => Some(None),
        Some(arg) => Some(ctx.resolve(arg)),
    }
}

/// Skips the options from `from`; an option in `with_arg` takes the next word too.
fn skip_flags(words: &[String], from: usize, with_arg: &[&str]) -> usize {
    let mut i = from;
    while i < words.len() && words[i].len() > 1 && words[i].starts_with('-') {
        let w = words[i].as_str();
        i += 1;
        if w == "--" {
            break;
        }
        if with_arg.contains(&w) {
            i += 1;
        }
    }
    i.min(words.len())
}

/// Skips the options, then one positional word (`timeout 5`, `flock FILE`, `taskset MASK`).
fn skip_positional(words: &[String], from: usize, with_arg: &[&str]) -> usize {
    let i = skip_flags(words, from, with_arg);
    if i < words.len() {
        i + 1
    } else {
        i
    }
}

/// Skips `env`'s options and `NAME=value` words. `env -S 'cmd ...'` runs that string.
fn skip_env(words: &[String], from: usize, inline: &mut Vec<String>, dirs: &mut Vec<String>) -> usize {
    let mut i = from;
    let mut moved = false;
    while i < words.len() {
        let w = words[i].as_str();
        if let Some(value) = w.strip_prefix("--chdir=") {
            dirs.push(value.to_string());
            moved = true;
            i += 1;
            continue;
        }
        if matches!(w, "-C" | "--chdir") && i + 1 < words.len() {
            // The wrapped command runs in this directory, not in the current one.
            dirs.push(words[i + 1].clone());
            moved = true;
            i += 2;
            continue;
        }
        if w == "-S" || w == "--split-string" {
            let Some(split) = words.get(i + 1) else {
                return words.len();
            };
            // The words after the split string are that command's own arguments.
            let rest = &words[i + 2..];
            inline.push(if rest.is_empty() {
                split.clone()
            } else {
                format!("{split} {}", rest.join(" "))
            });
            return words.len();
        }
        if matches!(w, "-u" | "--unset") {
            i += 2;
        } else if w.starts_with('-') || is_assignment(w) {
            i += 1;
        } else {
            break;
        }
    }
    i = i.min(words.len());
    // `env -C a env -C b cmd` runs in `a` then in `b`: the nested wrapper's directory is relative to the one
    // this `env` moved to, so it composes here (`b` is resolved against `a` by the caller's chain) rather than
    // standing alone against the shell's own directory (#1768).
    if moved && words.get(i).is_some_and(|w| w == "env") {
        i = skip_env(words, i + 1, inline, dirs);
    }
    i
}

/// The word after the first of `flags` at or after `from`.
fn script_after(words: &[String], from: usize, flags: &[&str]) -> Option<String> {
    let tail = words.get(from..)?;
    let k = tail.iter().position(|w| flags.contains(&w.as_str()))?;
    tail.get(k + 1).cloned()
}

/// The script of `sh -c <script>`, `bash -lc <script>`, `--login -c <script>`, ...
fn shell_script(argv: &[String]) -> Option<String> {
    let mut k = 1usize;
    while k < argv.len() {
        let a = argv[k].as_str();
        if a == "--" || a == "-" || !(a.starts_with('-') || a.starts_with('+')) {
            return None;
        }
        if matches!(a, "-o" | "+o" | "-O" | "+O") {
            k += 2;
            continue;
        }
        if a.starts_with("--") {
            k += 1;
            continue;
        }
        if a.starts_with('-') && a[1..].contains('c') {
            let mut next = k + 1;
            if argv.get(next).is_some_and(|word| word == "--") {
                next += 1;
            }
            return argv.get(next).cloned();
        }
        k += 1;
    }
    None
}

/// The destination directory of `cp`/`ln`/`install`: `-t DIR`, `--target-directory DIR` or
/// `--target-directory=DIR`.
fn target_directory(args: &[String]) -> Option<String> {
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            return None;
        }
        if a == "-t" || a == "--target-directory" {
            return args.get(i + 1).cloned();
        }
        if let Some(rest) = a.strip_prefix("--target-directory=") {
            return Some(rest.to_string());
        }
        // `-tDIR` and a short cluster ending in `t` (`-vt DIR`).
        if a.starts_with('-') && !a.starts_with("--") && a.len() > 2 {
            if let Some(rest) = a.strip_prefix("-t") {
                return Some(rest.to_string());
            }
            if a.ends_with('t') {
                return args.get(i + 1).cloned();
            }
        }
        i += 1;
    }
    None
}

/// The words that are not options (after `--`, everything is an operand).
fn operands(args: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut end = false;
    for a in args {
        if !end && a == "--" {
            end = true;
            continue;
        }
        if end || !a.starts_with('-') || a == "-" {
            out.push(a.as_str());
        }
    }
    out
}

/// `true` when `args` has a recursive flag (`-r`, `-R`, `-rf`, `--recursive`).
fn is_recursive(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "--recursive"
            || (a.starts_with('-') && !a.starts_with("--") && (a.contains('r') || a.contains('R')))
    })
}

/// A `chmod` mode that opens a file to everyone: an octal mode whose "other" digit grants
/// write (`777`, `666`, `757`, `o+w`), or a symbolic clause that grants write to `o` or `a`
/// (also with no `who`, which means all).
fn mode_is_open(mode: &str) -> bool {
    if mode.contains("777") || mode.contains("666") || mode.contains("a+rwx") || mode.contains("o+w") {
        return true;
    }
    if mode.len() >= 3 && mode.len() <= 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        let other = mode.as_bytes()[mode.len() - 1] - b'0';
        return other & 0o2 != 0;
    }
    symbolic_grants_other_write(mode)
}

/// `[ugoa]*([-+=]([rwxXst]*|[ugo]))+` clauses; true when one adds write for `o` or `a`.
fn symbolic_grants_other_write(mode: &str) -> bool {
    let chars: Vec<char> = mode.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let mut who_other = false;
        let mut who_seen = false;
        while i < chars.len() && matches!(chars[i], 'u' | 'g' | 'o' | 'a') {
            if matches!(chars[i], 'o' | 'a') {
                who_other = true;
            }
            who_seen = true;
            i += 1;
        }
        loop {
            if i >= chars.len() || !matches!(chars[i], '+' | '-' | '=') {
                return false;
            }
            let op = chars[i];
            i += 1;
            let start = i;
            while i < chars.len() && matches!(chars[i], 'r' | 'w' | 'x' | 'X' | 's' | 't' | 'u' | 'g' | 'o') {
                i += 1;
            }
            if start == i {
                return false;
            }
            // No `who` means all, like `a`.
            let adds_write = op != '-' && chars[start..i].contains(&'w');
            if adds_write && (who_other || !who_seen) {
                return true;
            }
            if i >= chars.len() || !matches!(chars[i], '+' | '-' | '=') {
                break;
            }
        }
        // A clause list continues with `,`.
        if i < chars.len() {
            if chars[i] != ',' {
                return false;
            }
            i += 1;
        }
    }
    false
}

/// `true` when `value` is a short-option cluster (`-abc`, not `--long`) with `letter`.
pub(crate) fn short_has(value: &str, letter: char) -> bool {
    value.starts_with('-') && !value.starts_with("--") && value[1..].contains(letter)
}

/// `kill` with pid 1, a negative pid (a process group) or `-1`, after its signal.
fn kill_foreign(args: &[String]) -> bool {
    let mut signal_done = false;
    let mut after_dd = false;
    for a in args {
        let a = a.as_str();
        if !after_dd && a == "--" {
            after_dd = true;
            continue;
        }
        if !after_dd
            && !signal_done
            && a.len() > 1
            && a.starts_with('-')
            && a[1..].chars().all(|c| c.is_ascii_alphanumeric())
        {
            signal_done = true;
            continue;
        }
        if let Ok(pid) = a.parse::<i64>() {
            // This pure classifier has no managed-child ownership evidence. Every literal
            // process target is therefore unverified (zero and negatives are process groups).
            let _ = pid;
            return true;
        }
    }
    false
}

/// A SQL `DROP` of a table, database or schema, or a `TRUNCATE` statement, in any word.
fn sql_drop(words: &[String]) -> bool {
    words.iter().any(|w| {
        // Split statements only outside SQL string literals. Doubled quotes escape a quote.
        let mut statements = Vec::new();
        let mut statement = String::new();
        let mut quoted = false;
        let mut chars = w.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\'' {
                if quoted && chars.peek() == Some(&'\'') {
                    statement.push(ch);
                    statement.push(chars.next().unwrap_or('\''));
                    continue;
                }
                quoted = !quoted;
                statement.push(' ');
            } else if ch == ';' && !quoted {
                statements.push(std::mem::take(&mut statement));
            } else if quoted {
                statement.push(' ');
            } else {
                statement.push(ch);
            }
        }
        statements.push(statement);
        statements.into_iter().any(|statement| {
            let normalized = statement
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            normalized.contains("drop table")
                || normalized.contains("drop database")
                || normalized.contains("drop schema")
                || normalized.starts_with("truncate ")
        })
    })
}

/// `find` arguments that are expressions, not roots.
fn is_find_expr(a: &str) -> bool {
    a.starts_with('-') || a.starts_with('(') || a.starts_with('!')
}

/// `true` when the text has a `$` or a backtick: a value the shell computes.
pub(crate) fn has_dynamic(text: &str) -> bool {
    text.contains('$') || text.contains('`')
}

/// `true` when the command takes data from a file or stdin that can change between the owner's
/// answer and a replay of the same text: an input redirection (`< targets`), or a reader that
/// turns its input into arguments (`xargs`, `parallel`, `read`, `mapfile`, `readarray`,
/// `--files-from`). A saved unattended approval never covers such a command, because the text
/// the owner reviewed does not fix what it acts on. The check is deliberately coarse: a false
/// positive only means the command is not replayed unattended.
pub(crate) fn has_mutable_input(text: &str) -> bool {
    let is_reader = |w: &str| {
        matches!(w, "xargs" | "parallel" | "read" | "mapfile" | "readarray")
            || w.ends_with("/xargs")
            || w.ends_with("/parallel")
            || w.starts_with("--files-from")
    };
    // A backslash-newline continues a word (`xa\<newline>rgs` is `xargs`): join it before
    // splitting, then also check the words the shell parser itself produces.
    let joined = text.replace("\\\r\n", "").replace("\\\n", "");
    joined.contains('<')
        || joined
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '{' | '}'))
            // Shell quoting and escapes inside a word do not change the command it names:
            // `x'a'rgs`, `"xargs"` and `xa\rgs` are all `xargs`.
            .map(|w| w.replace(['\'', '"', '\\'], ""))
            .any(|w| is_reader(&w))
        || parse_line(&joined).is_some_and(|stages| {
            stages.iter().any(|st| st.words.iter().any(|w| is_reader(w))) || effects::mutable_input(&stages)
        })
}

/// Where a brace expansion (`{a,b}`, `{1..3}`) starts in `w`, if it has one. `{}` (the
/// `find -exec` placeholder) and `${x}` are not brace expansions.
fn brace_expansion_at(w: &str) -> Option<usize> {
    let bytes = w.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'{' || (i > 0 && bytes[i - 1] == b'$') {
            continue;
        }
        let Some(len) = w[i + 1..].find('}') else { continue };
        let inner = &w[i + 1..i + 1 + len];
        if inner.contains(',') || inner.contains("..") {
            return Some(i);
        }
    }
    None
}

/// `true` for `NAME=value` (a valid name before the first `=`).
fn is_assignment(w: &str) -> bool {
    match w.find('=') {
        Some(eq) if eq > 0 => {
            let name = &w[..eq];
            !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// The last component of a command word (`/bin/rm`, `./rm` -> `rm`).
pub(crate) fn base_name(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

/// `~`, `~/` and `$HOME` at the start of `raw` become the home directory.
fn expand_home(raw: &str, home: &Path) -> String {
    let h = home.to_string_lossy();
    if raw == "~" || raw == "$HOME" || raw == "${HOME}" {
        return h.into_owned();
    }
    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("$HOME/"))
        .or_else(|| raw.strip_prefix("${HOME}/"))
    {
        return format!("{h}/{rest}");
    }
    raw.to_string()
}

/// The simple command as one string: words and redirections, single spaces.
fn segment(words: &[String], redirects: &[(String, String)]) -> String {
    let mut parts: Vec<String> = words.to_vec();
    for (op, target) in redirects {
        parts.push(op.clone());
        parts.push(target.clone());
    }
    parts.join(" ")
}

/// `true` when removing or changing `raw` reaches outside the workspace (or is a top-level
/// glob, a variable, `/` or the home directory itself).
fn target_outside(raw: &str, ctx: &Ctx<'_>) -> bool {
    let home = normalize(&ctx.guard.home, ctx.root);
    let is_dynamic = |c: char| matches!(c, '$' | '`' | '*' | '?' | '[');
    let first = raw
        .find(is_dynamic)
        .into_iter()
        .chain(brace_expansion_at(raw))
        .min();
    if let Some(idx) = first {
        // A glob, a brace expansion or a variable: safe only under a literal sub-directory of
        // the workspace, and only while it cannot climb back out (`oxa/per?/../../victim`).
        if raw[idx..].split('/').any(|part| part == "..") {
            return true;
        }
        let literal = &raw[..idx];
        let cut = match literal.rfind('/') {
            Some(k) => &literal[..k],
            None => "",
        };
        if cut.is_empty() {
            return true;
        }
        return match ctx.resolve(cut) {
            Some(p) => !(p.as_path() != ctx.root && is_inside(&p, ctx.root) && p != home),
            None => true,
        };
    }
    match ctx.resolve(raw) {
        Some(path) => path.as_path() == Path::new("/") || path == home || !is_inside(&path, ctx.root),
        None => true,
    }
}
