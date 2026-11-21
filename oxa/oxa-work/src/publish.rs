//! Explicit publishing of the accepted commit; existing queued PRs are never pushed.
use super::{checked, state, tree, CheckpointObserver, LockedTask, Outcome, Record, Result, RunPolicy, Task};
use state::Step;
mod landing;
mod receipt;
pub use landing::{
    Admission, AdmissionAcknowledgment, AdmissionReceipt, DeliveryReceipt, LandingPublisher, NoticeIntent,
    RejectedLanding, RetryIntent,
};
pub(crate) use receipt::push_with_receipt;
pub(crate) use receipt::remote_tip as destination_branch_tip;
pub use receipt::{Divergence, PublishReceipt};
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn action_context(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
) -> Result<(state::Writer, state::State)> {
    let path = locked.policy.storage.join(format!("{}.json", locked.task.id));
    let value = state::load(&path)?;
    if value.task != *locked.task {
        return Err("task specification changed".into());
    }
    let mut writer = state::Writer::new(path, true);
    writer.replay(&value, observer)?;
    Ok((writer, value))
}
const MAX_PUBLICATION_RESPONSE_BYTES: usize = 1024 * 1024;

/// One part of the commits a publication would push: the name of a file that a
/// commit adds (`line` is `None`, `text` is empty) or one added line.
pub struct PushedContent<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub text: &'a str,
}

/// Why pushed content must not be published. `label` never contains the content;
/// `secret_path` withholds a path that itself contains secret text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentFinding {
    pub label: &'static str,
    pub secret_path: bool,
}

/// Classifies pushed content before publication.
pub type ContentScan<'a> = &'a dyn Fn(&PushedContent<'_>) -> Option<ContentFinding>;

/// Decode a path Git printed in C-style quotes (`"b/a\"b\\c\303\251"`).
fn unquote(quoted: &str) -> Option<String> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = chars.next()?;
        bytes.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    let digit = chars.next().filter(|digit| (b'0'..=b'7').contains(digit))?;
                    value = value * 8 + u32::from(digit - b'0');
                }
                u8::try_from(value).ok()?
            }
            other => other,
        });
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn diff_path(header: &str) -> Option<String> {
    let path = header.trim_end_matches(['\r', '\n']);
    if path == "/dev/null" {
        return None;
    }
    let path = if path.starts_with('"') {
        unquote(path)?
    } else {
        // Git ends an unquoted `+++` path that contains a space with a tab. A tab in
        // a real name would make Git quote the path, so one trailing tab is never part of it.
        path.strip_suffix('\t').unwrap_or(path).to_string()
    };
    Some(path.strip_prefix("b/").map(str::to_string).unwrap_or(path))
}

fn refusal(path: &str, line: Option<usize>, finding: ContentFinding) -> String {
    let path: String = if finding.secret_path {
        "a file whose path".into()
    } else {
        path.chars()
            .map(|ch| if ch.is_control() { '?' } else { ch })
            .take(200)
            .collect()
    };
    let at = line.map_or(String::new(), |line| format!(":{line}"));
    format!(
        "publishing refused: {path}{at} adds {}; remove it from the task branch history before publishing",
        finding.label
    )
}

/// The commits a push can add: reachable from `head` and from none of `known`.
fn pushed_revisions<'a>(head: &'a str, known: &'a [String]) -> Vec<&'a str> {
    let mut revisions = vec!["--ignore-missing", head, "--not"];
    revisions.extend(known.iter().map(String::as_str));
    revisions
}

/// The branch and tag tips the publication destination itself advertises. Local
/// remote-tracking refs are writable by the task and may describe a different
/// fetch URL, so only the destination's own answer may exclude history.
fn github_https_url(url: &str) -> Result<&str> {
    let path = url
        .strip_prefix("https://github.com/")
        .ok_or("configured GitHub authentication requires a github.com HTTPS push URL")?;
    let (owner, repo) = path.split_once('/').ok_or("invalid GitHub push URL")?;
    let valid = |part: &str| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if !valid(owner) || !valid(repo.strip_suffix(".git").unwrap_or(repo)) {
        return Err("invalid GitHub push URL".into());
    }
    Ok(url)
}

/// Only use this from outside any checkout or from a freshly exported bare
/// store. Neither system/global config nor project transport helpers are loaded.
fn github_git(policy: &RunPolicy, cwd: &Path, token: &str) -> Command {
    use base64::Engine as _;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let mut command = Command::new(&policy.git_binary);
    command
        .env_clear()
        .current_dir(cwd)
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        // Use environment config so the header is neither in argv nor on disk.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
        .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {auth}"))
        .args([
            "-c",
            "http.followRedirects=false",
            "-c",
            "credential.helper=",
            "-c",
            "core.hooksPath=/dev/null",
        ]);
    command
}

fn destination_tips(policy: &RunPolicy, repo: &Path, url: &str) -> Result<Vec<String>> {
    let token = super::policy_token(policy, repo)?;
    let mut command = destination_refs_command(policy, repo, url, token.as_deref())?;
    command.args(["ls-remote", "--heads", "--tags", "--refs", url]);
    let text = super::command_runner(
        command,
        Some(MAX_PUBLICATION_RESPONSE_BYTES),
        super::COMMAND_DEADLINE,
    )
    .filter(|(status, _)| *status == 0)
    .map(|(_, text)| text)
    .ok_or("publishing refused: cannot read the publication destination's branches")?;
    let mut tips: Vec<String> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, _)| oid)
        .filter(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect();
    tips.sort();
    tips.dedup();
    Ok(tips)
}

fn destination_refs_command(
    policy: &RunPolicy,
    repo: &Path,
    url: &str,
    token: Option<&str>,
) -> Result<Command> {
    if url.starts_with('-') {
        return Err("publishing refused: invalid publication destination".into());
    }
    if let Some(token) = token {
        github_https_url(url)?;
        if !matches!(std::fs::symlink_metadata("/.git"), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            return Err("cannot run Git outside a checkout".into());
        }
        return Ok(github_git(policy, Path::new("/"), token));
    }
    let mut command = super::git(
        policy,
        repo,
        &[
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.file.allow=always",
        ],
    );
    isolate_github_environment(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    Ok(command)
}

/// Scan the commits `revisions` select: each raw commit object, its own changes and
/// the lines a merge commit introduces itself. Any failure refuses publication.
fn scan_pushed(policy: &RunPolicy, tree: &Path, revisions: &[&str], scan: ContentScan<'_>) -> Result<()> {
    let unreadable = |_: String| "publishing refused: cannot scan the commits to publish".to_string();
    let log = |options: &[&str]| {
        // No user or repository preference may change what the scan sees.
        let mut args = vec![
            "-c",
            "core.quotePath=false",
            "-c",
            "log.showRoot=true",
            "-c",
            "log.showSignature=false",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.relative=false",
            "-c",
            "color.ui=false",
            "-c",
            "color.diff=false",
            "log",
            "--root",
        ];
        args.extend_from_slice(options);
        args.extend_from_slice(revisions);
        args.push("--");
        checked(super::git(policy, tree, &args)).map_err(unreadable)
    };
    // Every byte of each commit object is pushed: identities, the message and any
    // extra header (gpgsig, mergetag, encoding, unknown ones), so the raw object is
    // scanned rather than a formatted subset of it.
    let mut listing = vec!["rev-list", "--reverse"];
    listing.extend_from_slice(revisions);
    listing.push("--");
    let commits = checked(super::git(policy, tree, &listing)).map_err(unreadable)?;
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        if !valid_object_id(commit) {
            return Err(unreadable(String::new()));
        }
        let raw = checked(super::git(policy, tree, &["cat-file", "commit", commit])).map_err(unreadable)?;
        scan_commit_object(commit, &raw, scan)?;
    }
    let text = log(&[
        "--reverse",
        "--format=%x00%H",
        "--patch",
        "--cc",
        "--text",
        "--unified=0",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-relative",
        "--no-show-signature",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ])?;
    // `--unified=0` shows no unchanged line, so the key that owns an edited block scalar
    // is read from the commit's own copy of the file.
    let file_at = |commit: &str, path: &str| {
        if !valid_object_id(commit) {
            return None;
        }
        let spec = format!("{commit}:{path}");
        checked(super::git(policy, tree, &["show", "--no-textconv", &spec])).ok()
    };
    scan_patch(&text, scan, &file_at)?;
    // An empty added file has no content header in the patch, so the names of all
    // added files are listed separately and checked as well.
    let names = log(&[
        "--format=",
        "--name-only",
        "-z",
        "--diff-filter=A",
        "--cc",
        "--no-renames",
        "--no-relative",
    ])?;
    for name in names.split('\0').filter(|name| !name.is_empty()) {
        let item = PushedContent {
            path: name,
            line: None,
            text: "",
        };
        if let Some(finding) = scan(&item) {
            return Err(refusal(name, None, finding));
        }
    }
    Ok(())
}

/// A full lowercase hexadecimal object id (SHA-1 or SHA-256).
fn valid_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Pass each line of one raw commit object (`git cat-file commit`: all headers,
/// signature blocks and the message) to `scan`.
fn scan_commit_object(commit: &str, raw: &str, scan: ContentScan<'_>) -> Result<()> {
    let place = format!("commit {}", commit.get(..12).unwrap_or(commit));
    for (index, text) in raw.lines().enumerate() {
        let item = PushedContent {
            path: &place,
            line: Some(index + 1),
            text,
        };
        if let Some(finding) = scan(&item) {
            return Err(refusal(&place, Some(index + 1), finding));
        }
    }
    Ok(())
}

/// Whether a YAML line ends in a block scalar indicator (`|`, `>`, with `-`/`+`).
fn opens_block_scalar(row: &str) -> bool {
    // `key: |`, `|2-`, `|+2`, `>2` and a trailing comment are all valid headers. The
    // indicator is the last run of `|`/`>` plus optional chomping and indentation markers.
    let mut text = row.trim_end();
    if let Some((before, _)) = text.split_once(" #") {
        text = before.trim_end();
    }
    let Some(at) = text.rfind(['|', '>']) else {
        return false;
    };
    let marker = &text[at + 1..];
    let mut chomp = false;
    let mut indent = false;
    for c in marker.chars() {
        match c {
            '+' | '-' if !chomp => chomp = true,
            '1'..='9' if !indent => indent = true,
            '0' => return false,
            _ => return false,
        }
    }
    true
}

/// The line that opens the block scalar `current` (the added line at 1-based `line`)
/// belongs to: the nearest earlier line that is indented less and ends in a block
/// scalar indicator. A less-indented line without one means `current` is not in a block.
fn enclosing_block_header(content: &str, line: usize, current: &str) -> Option<String> {
    let indent = |text: &str| text.len() - text.trim_start().len();
    let wanted = indent(current);
    let lines: Vec<&str> = content.lines().collect();
    for candidate in lines.get(..line.saturating_sub(1))?.iter().rev() {
        if candidate.trim().is_empty() {
            continue;
        }
        if indent(candidate) < wanted {
            return opens_block_scalar(candidate).then(|| candidate.to_string());
        }
    }
    None
}

/// The unchanged JSON key line an edited value belongs to: the nearest earlier non-blank
/// line of the committed file when it is a quoted key that ends its line (`"password":`).
/// `-U0` patches carry only the replaced value, so without this a split JSON credential
/// edited under an existing key reaches the scanners without its key.
fn enclosing_json_key(content: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let previous = lines
        .get(..line.saturating_sub(1))?
        .iter()
        .rev()
        .find(|candidate| !candidate.trim().is_empty())?;
    let key = previous.trim();
    (key.starts_with('"') && key.trim_end_matches(':').trim_end().ends_with('"') && key.ends_with(':'))
        .then(|| previous.to_string())
}

/// Pass every added file name and added line of `git log --patch --cc -U0` output to `scan`.
fn scan_patch(
    text: &str,
    scan: ContentScan<'_>,
    file_at: &dyn Fn(&str, &str) -> Option<String>,
) -> Result<()> {
    let mut commit = String::new();
    let mut recovered = false;
    let (mut path, mut added, mut hunk, mut parents, mut line) = (None::<String>, false, false, 1, 0);
    let mut block_scalar_header: Option<String> = None;
    let mut previous_added: Option<String> = None;
    for row in text.lines() {
        if let Some(id) = row.strip_prefix('\0') {
            commit = id.trim().to_string();
        }
        if row.starts_with('\0') || row.starts_with("diff --") {
            parents = if row.starts_with("diff --cc ") || row.starts_with("diff --combined ") {
                2
            } else {
                1
            };
            (path, added, hunk) = (None, false, false);
            block_scalar_header = None;
            continue;
        }
        if row.starts_with("@@") {
            let marks = row.bytes().take_while(|byte| *byte == b'@').count();
            parents = marks.saturating_sub(1).max(1);
            line = row[marks..]
                .split_ascii_whitespace()
                .find_map(|range| range.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok())
                .ok_or("publishing refused: cannot scan the commits to publish")?;
            hunk = true;
            recovered = false;
            block_scalar_header = None;
            continue;
        }
        if !hunk {
            if row.starts_with("new file mode ") {
                added = true;
            } else if let Some(header) = row.strip_prefix("+++ ") {
                previous_added = None;
                path = diff_path(header);
                if let Some(name) = path.as_deref().filter(|_| added) {
                    let item = PushedContent {
                        path: name,
                        line: None,
                        text: "",
                    };
                    if let Some(finding) = scan(&item) {
                        return Err(refusal(name, None, finding));
                    }
                }
            }
            continue;
        }
        let Some(name) = path.as_deref() else {
            continue;
        };
        let Some((marks, rest)) = row.split_at_checked(parents) else {
            continue;
        };
        if marks.contains('-') || row.starts_with('\\') {
            block_scalar_header = None;
            previous_added = None;
            continue;
        }
        if marks.contains('+') {
            // `--text` passes UTF-16/UTF-32 files through as NUL-interleaved bytes, in
            // which no pattern matches; the line is also checked with its NULs removed,
            // which is the ASCII text those encodings carry.
            if !recovered {
                recovered = true;
                if !added && block_scalar_header.is_none() && rest.starts_with([' ', '\t']) {
                    block_scalar_header = file_at(&commit, name)
                        .and_then(|content| enclosing_block_header(&content, line, rest));
                }
                // A JSON value edited under an unchanged key (`"password":` on the line
                // above) is scanned with that key, like a pair of added lines.
                if !added && previous_added.is_none() && rest.trim_start().starts_with('"') {
                    previous_added =
                        file_at(&commit, name).and_then(|content| enclosing_json_key(&content, line));
                }
            }
            let stripped = rest.replace('\0', "");
            let decoded = (stripped.len() != rest.len()).then_some(stripped.as_str());
            for text in std::iter::once(rest).chain(decoded) {
                let item = PushedContent {
                    path: name,
                    line: Some(line),
                    text,
                };
                if let Some(finding) = scan(&item) {
                    return Err(refusal(name, Some(line), finding));
                }
                if let Some(header) = block_scalar_header.as_deref() {
                    let context = format!("{header}\n{text}");
                    let item = PushedContent {
                        path: name,
                        line: Some(line),
                        text: &context,
                    };
                    if let Some(finding) = scan(&item) {
                        return Err(refusal(name, Some(line), finding));
                    }
                }
            }
            // A credential split over two added lines (`"password":` then the value) matches
            // neither line alone, so the pair is scanned with its context.
            if let Some(previous) = previous_added.as_deref() {
                let context = format!("{previous}\n{rest}");
                let item = PushedContent {
                    path: name,
                    line: Some(line),
                    text: &context,
                };
                if let Some(finding) = scan(&item) {
                    return Err(refusal(name, Some(line), finding));
                }
            }
            previous_added = Some(rest.to_string());
            // A line at or above the header's indentation leaves the block scalar: a later
            // key must not be scanned against this header.
            let indent = |text: &str| text.len() - text.trim_start().len();
            if block_scalar_header
                .as_deref()
                .is_some_and(|header| indent(rest) <= indent(header))
            {
                block_scalar_header = None;
            }
            if opens_block_scalar(rest) {
                block_scalar_header = Some(rest.to_string());
            }
        } else {
            block_scalar_header = None;
            previous_added = None;
        }
        line += 1;
    }
    Ok(())
}

pub(crate) fn isolate_github_environment(command: &mut Command) {
    // The destination comes from origin. Let the CLI choose its stored
    // login for that host, independently of host-agnostic shell overrides.
    for key in [
        "GH_HOST",
        "GH_REPO",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        command.env_remove(key);
    }
}

pub(crate) fn gh(
    binary: &Path,
    policy: &RunPolicy,
    token: Option<&str>,
    repo: &Path,
    lock: &File,
    args: &[&str],
) -> Result<String> {
    let mut command = Command::new(binary);
    super::isolate_git_environment(&mut command);
    isolate_github_environment(&mut command);
    if let Some(token) = token {
        command.env("GH_HOST", "github.com").env("GH_TOKEN", token);
    }
    command
        .env("PATH", &policy.host_path)
        .env("GH_PROMPT_DISABLED", "1")
        .current_dir(repo)
        .args(args);
    match super::command_runner_with_stdin(
        command,
        Some(MAX_PUBLICATION_RESPONSE_BYTES),
        Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
        super::COMMAND_DEADLINE,
    ) {
        Some((0, text)) if !token.is_some_and(|secret| text.contains(secret)) => Ok(text.trim().into()),
        _ => Err("GitHub publication request failed".into()),
    }
}
fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}
pub(crate) struct Repository {
    pub(crate) qualified: String,
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) name: String,
}
pub(crate) fn repository(value: &serde_json::Value) -> Result<Repository> {
    let identity = value["nameWithOwner"]
        .as_str()
        .ok_or("repository identity missing")?;
    let (owner, name) = identity.split_once('/').ok_or("invalid repository identity")?;
    let url = value["url"].as_str().ok_or("repository URL missing")?;
    let (host, path) = url
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
        .ok_or("repository URL must identify its HTTPS host")?;
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || path.trim_end_matches('/') != identity
        || host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
    {
        return Err("repository URL disagrees with its identity".into());
    }
    Ok(Repository {
        qualified: format!("{host}/{identity}"),
        host: host.into(),
        owner: owner.into(),
        name: name.into(),
    })
}
/// Validated open pull requests whose head is this task branch.
pub(crate) fn open_pull_requests(
    repo: &Path,
    gh_binary: &Path,
    policy: &RunPolicy,
    token: Option<&str>,
    lock: &File,
    repository: &Repository,
    branch: &str,
) -> Result<Vec<serde_json::Value>> {
    let prs = json(&gh(
        gh_binary,
        policy,
        token,
        repo,
        lock,
        &[
            "pr",
            "list",
            "--repo",
            &repository.qualified,
            "--state",
            "open",
            "--head",
            branch,
            "--json",
            "number,url,isDraft,headRepositoryOwner,isCrossRepository",
        ],
    )?)?;
    let prs = prs.as_array().ok_or("invalid PR list")?.to_vec();
    if prs.len() > 1 {
        return Err("multiple open PRs match the task branch".into());
    }
    if prs.first().is_some_and(|pr| {
        pr["headRepositoryOwner"]["login"].as_str() != Some(repository.owner.as_str())
            || pr["isCrossRepository"].as_bool() != Some(false)
    }) {
        return Err("PR head repository does not match origin".into());
    }
    Ok(prs)
}

/// The open PR's number when it is already admitted to the merge queue.
pub(crate) fn queued_pull_request(
    repo: &Path,
    gh_binary: &Path,
    policy: &RunPolicy,
    token: Option<&str>,
    lock: &File,
    repository: &Repository,
    branch: &str,
) -> Result<Option<u64>> {
    let prs = open_pull_requests(repo, gh_binary, policy, token, lock, repository, branch)?;
    let Some(pr) = prs.first() else {
        return Ok(None);
    };
    let number = pr["number"]
        .as_u64()
        .filter(|number| *number != 0)
        .ok_or("PR number missing")?;
    let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){mergeQueueEntry{id}}}}";
    let data = json(&gh(
        gh_binary,
        policy,
        token,
        repo,
        lock,
        &[
            "api",
            "graphql",
            "--hostname",
            &repository.host,
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={}", repository.owner),
            "-F",
            &format!("name={}", repository.name),
            "-F",
            &format!("number={number}"),
        ],
    )?)?;
    let pull = &data["data"]["repository"]["pullRequest"];
    if !pull.is_object() || pull.get("mergeQueueEntry").is_none() {
        return Err("cannot determine whether PR is in the merge queue".into());
    }
    Ok((!pull["mergeQueueEntry"].is_null()).then_some(number))
}

pub(crate) fn origin_push_url(policy: &RunPolicy, repo: &Path) -> Result<String> {
    let remote = checked(super::git(
        policy,
        repo,
        &["remote", "get-url", "--push", "--all", "origin"],
    ))?;
    if remote.lines().count() != 1 || remote.starts_with('-') {
        return Err("publishing requires one origin push URL".into());
    }
    if !policy.github_token_command.is_empty() {
        github_https_url(&remote)?;
    }
    Ok(remote)
}

/// Resolve the pinned origin destination through the same validated `gh` read.
pub(crate) fn origin_repository(
    repo: &Path,
    gh_binary: &Path,
    policy: &RunPolicy,
    token: Option<&str>,
    lock: &File,
    remote: &str,
) -> Result<Repository> {
    if token.is_some() {
        github_https_url(remote)?;
    }
    let repository = repository(&json(&gh(
        gh_binary,
        policy,
        token,
        repo,
        lock,
        &["repo", "view", remote, "--json", "nameWithOwner,url"],
    )?)?)?;
    if token.is_some() && repository.host != "github.com" {
        return Err("configured GitHub authentication requires github.com".into());
    }
    Ok(repository)
}

/// Reject locally visible unpublished secrets before resolving any publication credential.
pub(crate) fn scan_local_push(
    policy: &RunPolicy,
    repo: &Path,
    head: &str,
    scan: ContentScan<'_>,
) -> Result<()> {
    scan_pushed(policy, repo, &[head, "--not", "--remotes=origin"], scan)
}

/// The publication content scan for a push `head` would make to `origin`: every
/// commit the push URL's own advertised tips lack, the same scan `--publish` runs.
/// Nothing is sent; any failure, or a missing scan, refuses the push.
pub(crate) fn scan_push(policy: &RunPolicy, repo: &Path, head: &str, remote: &str) -> Result<()> {
    let scan = policy
        .content_scan
        .as_deref()
        .ok_or("publishing refused: the publication content scan is unavailable")?;
    scan_local_push(policy, repo, head, scan)?;
    let known = destination_tips(policy, repo, remote)?;
    scan_pushed(policy, repo, &pushed_revisions(head, &known), scan)
}

/// Paths under `paths` that any commit the push would add changes (reachable from `head`
/// and from no tip the destination advertises), empty when none does. A merge is listed
/// against each parent, so a control file that only a branch side changed still shows.
pub(crate) fn pushed_path_changes(
    policy: &RunPolicy,
    repo: &Path,
    head: &str,
    paths: &[&str],
    remote: &str,
) -> Result<String> {
    let known = destination_tips(policy, repo, remote)?;
    let mut args = vec!["log", "--format=", "--name-only", "--no-renames", "-m"];
    args.extend(pushed_revisions(head, &known));
    args.push("--");
    args.extend_from_slice(paths);
    checked(super::git(policy, repo, &args))
}

/// Push one exact accepted commit. Metadata probes deny every transport;
/// only this explicit, confirmed publication may use normal GitHub/file transports.
pub(crate) fn prepare_github_push(
    policy: &RunPolicy,
    repo: &Path,
    head: &str,
) -> Result<Option<super::committed::Export>> {
    if policy.github_token_command.is_empty() {
        return Ok(None);
    }
    let tree = checked(super::git(
        policy,
        repo,
        &["rev-parse", &format!("{head}^{{tree}}")],
    ))?;
    // Export afresh after the checks, without their writable Git config.
    super::committed::Export::create(policy, repo, head, &tree, head).map(Some)
}

pub(crate) fn push_head(
    policy: &RunPolicy,
    auth: Option<(&str, &super::committed::Export)>,
    repo: &Path,
    head: &str,
    branch: &str,
    lock: &File,
    remote: &str,
) -> Result<()> {
    if let Some((token, export)) = auth {
        let url = github_https_url(remote)?;
        let mut push = github_git(policy, &export.store, token);
        push.args([
            "push",
            "--no-follow-tags",
            "--recurse-submodules=no",
            url,
            &format!("{head}:refs/heads/{branch}"),
        ]);
        return match super::command_runner_with_stdin(
            push,
            Some(MAX_PUBLICATION_RESPONSE_BYTES),
            Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
            super::COMMAND_DEADLINE,
        ) {
            Some((0, _)) => Ok(()),
            _ => Err("accepted commit push failed".into()),
        };
    }
    let mut push = super::git(
        policy,
        repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.file.allow=always",
            // Only the scanned commit is sent: no implicit tags or submodule pushes.
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
            "push",
            "--no-follow-tags",
            "--recurse-submodules=no",
            "origin",
            &format!("{head}:refs/heads/{branch}"),
        ],
    );
    isolate_github_environment(&mut push);
    push.env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh:file");
    if !matches!(
        super::command_runner_with_stdin(
            push,
            Some(MAX_PUBLICATION_RESPONSE_BYTES),
            Stdio::from(lock.try_clone().map_err(|e| e.to_string())?),
            super::COMMAND_DEADLINE,
        ),
        Some((0, _))
    ) {
        return Err("accepted commit push failed".into());
    }
    Ok(())
}

const BODY_BEGIN: &str = "<!-- oxa-work-acceptance -->";
const BODY_END: &str = "<!-- /oxa-work-acceptance -->";
fn body(
    task: &Task,
    head: &str,
    lane: Option<&crate::lane::LaneVerdict>,
    existing: Option<&str>,
) -> Result<String> {
    let ci = match lane {
        Some(verdict) if verdict.head == head && verdict.passed() => {
            format!("CI: lane run {} passed at this commit", verdict.run_id)
        }
        Some(verdict) => format!(
            "CI: lane run {} concluded {} at {}",
            verdict.run_id, verdict.conclusion, verdict.head
        ),
        None => "CI: not qualified for this commit".into(),
    };
    let section =
        format!("{BODY_BEGIN}\nAccepted commit: `{head}`\nLocal acceptance: passed\n{ci}\n{BODY_END}");
    let validation = if lane.is_some() {
        "Validation: local acceptance and the lane run passed at the accepted commit."
    } else {
        "Validation: frozen local acceptance passed; CI qualification is separate."
    };
    let default = format!("T2 — task `{}` executed by `oxa work`.\n\nWhat/why: explicitly requested task; review its changes against the base.\n\nChanges: accepted worktree retained.\n\nRemoves: inspect the diff before promotion; no deletion clearance is inferred.\n\n{validation}\n", task.id);
    let original = existing.unwrap_or(&default);
    match (
        original.matches(BODY_BEGIN).count(),
        original.matches(BODY_END).count(),
    ) {
        (0, 0) => Ok(format!("{original}\n\n{section}\n")),
        (1, 1) => {
            let (before, rest) = original
                .split_once(BODY_BEGIN)
                .ok_or("invalid managed PR section")?;
            let (_, after) = rest.split_once(BODY_END).ok_or("invalid managed PR section")?;
            Ok(format!("{before}{section}{after}"))
        }
        _ => Err("ambiguous managed PR section; preserve the owner description".into()),
    }
}
fn pr_base(repo: &Path, task: &Task, policy: &RunPolicy) -> Result<String> {
    let reference = checked(super::git(
        policy,
        repo,
        &["rev-parse", "--symbolic-full-name", &task.base],
    ))?;
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "publishing requires a named local or origin base branch".into())
}
pub fn publish(
    repo: &Path,
    binary: &Path,
    task: &Task,
    policy: &RunPolicy,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    scan: ContentScan<'_>,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.publish(repo, binary, gh_binary, observer, scan, confirm)
}

fn publish_locked(
    repo: &Path,
    binary: &Path,
    locked: &LockedTask<'_>,
    gh_binary: &Path,
    observer: &dyn CheckpointObserver,
    scan: ContentScan<'_>,
    confirm: &mut dyn FnMut(&str) -> Option<bool>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (task, policy, lock) = (locked.task, locked.policy, &locked.lock);
    let accepted = match value.record.acceptance_outcome.unwrap_or(value.record.outcome) {
        // Accepted work whose publication is still owed: only its receipt is missing (#2304).
        Outcome::PublicationPending => Outcome::Done,
        accepted => accepted,
    };
    if !super::completed_execution(value.record.step)
        || !matches!(accepted, Outcome::Done | Outcome::NothingToDo)
    {
        // Say why nothing was published; the unsaved copy carries the note, the checkpoint
        // stays as it was.
        let why = format!(
            "not published: only a finished task accepted as done can be published (this one is {accepted:?} at {:?})",
            value.record.step
        )
        .to_lowercase();
        value.record.detail = if value.record.detail.is_empty() {
            why
        } else {
            format!("{}; {why}", value.record.detail)
        };
        return Ok(value.record);
    }
    value.record.acceptance_outcome = Some(accepted);
    writer.save(&mut value, Step::Publishing, observer)?;
    let git = |repo: &Path, args: &[&str]| super::git(policy, repo, args);
    let mut publish_receipt = None;
    let result = (|| -> Result<(String, bool)> {
        if policy.offline {
            return Err("publishing is unavailable in offline mode".into());
        }
        if task.lane_ci
            && !value.record.lane.as_ref().is_some_and(|verdict| {
                Some(verdict.head.as_str()) == value.record.head_sha.as_deref() && verdict.passed()
            })
        {
            return Err("publication requires a passing lane CI run at the saved head".into());
        }
        let tree = &value.record.worktree;
        tree::validate(policy, repo, tree, task)?;
        if super::dirty_worktree(binary, tree, repo, policy, lock)? {
            return Err("uncommitted work must be saved before publishing".into());
        }
        let head = checked(git(tree, &["rev-parse", "HEAD"]))?;
        if value.record.head_sha.as_deref() != Some(head.as_str()) {
            return Err("task branch changed after acceptance; use --rerun before publishing".into());
        }
        let proof = super::committed::verify_commit(repo, binary, policy, &value, lock)?;
        let accepted_base = value
            .record
            .base_sha
            .as_deref()
            .ok_or("saved base is unavailable")?;
        let base = pr_base(tree, task, policy)?;
        if task.branch == base {
            return Err("task branch must differ from its base branch".into());
        }
        // The owner confirmation shows only metadata, so content is checked here,
        // before any remote request, against what the local origin refs record.
        // The destination's own tips are checked again after confirmation.
        scan_local_push(policy, tree, &head, scan)?;
        let remote = origin_push_url(policy, repo)?;
        let token = super::policy_token(policy, repo)?;
        let token = token.as_deref();
        let gh = |worktree: &Path, lock: &File, args: &[&str]| {
            let fresh = super::policy_token(policy, repo)?;
            gh(gh_binary, policy, fresh.as_deref(), worktree, lock, args)
        };
        let repository = origin_repository(tree, gh_binary, policy, token, lock, &remote)?;
        let name = repository.qualified.as_str();
        let prs = open_pull_requests(tree, gh_binary, policy, token, lock, &repository, &task.branch)?;
        if accepted == Outcome::NothingToDo
            && prs.is_empty()
            && value.record.head_sha == value.record.base_sha
        {
            return Err("nothing to publish".into());
        }
        let (pr_action, existing_body) = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            let value = json(&gh(
                tree,
                lock,
                &["pr", "view", &number, "--repo", name, "--json", "body"],
            )?)?;
            (
                format!("update PR #{number}"),
                Some(value["body"].as_str().ok_or("PR body missing")?.to_owned()),
            )
        } else {
            ("create a PR".into(), None)
        };
        let text = body(task, &head, value.record.lane.as_ref(), existing_body.as_deref())?;
        if confirm(&format!(
            "Push accepted commit {head} as {} to origin and {pr_action} in {name} (base {base})? Type yes to publish: ",
            task.branch
        )) != Some(true) {
            return Err("not published: the owner did not confirm (a session answers through its owner confirm broker, a terminal through /dev/tty)".into());
        }
        if remote != checked(git(repo, &["remote", "get-url", "--push", "--all", "origin"]))?
            || head != checked(git(tree, &["rev-parse", "HEAD"]))?
        {
            return Err("publication destination or task head changed during confirmation".into());
        }
        proof.verify(policy, &head, accepted_base)?;
        // Every commit the destination lacks is pushed, so the exclusions come from
        // the push URL's own advertised tips rather than from local refs.
        let known = destination_tips(policy, repo, &remote)?;
        scan_pushed(policy, tree, &pushed_revisions(&head, &known), scan)?;
        let export = prepare_github_push(policy, repo, &head)?;
        // Confirmation may remain open indefinitely: queue admission is checked
        // only after it returns, immediately before the push.
        let token = super::policy_token(policy, repo)?;
        let token = token.as_deref();
        if let Some(number) =
            queued_pull_request(tree, gh_binary, policy, token, lock, &repository, &task.branch)?
        {
            return Err(format!(
                "PR #{number} is in the merge queue; use a new task branch for follow-up work"
            ));
        }
        // The destination is read back into the saved receipt whether the push reports
        // success or not: a push that reports success must show the pushed commit at the
        // branch tip, and a rejected push names what the remote holds.
        let (pushed, receipt) = receipt::push_with_receipt(
            policy,
            token.zip(export.as_ref()),
            repo,
            &head,
            &task.branch,
            lock,
            &remote,
        );
        publish_receipt = Some(receipt);
        pushed?;
        let draft = false;
        let body = policy.storage.join(format!("{}.pr-body.md", task.id));
        crate::checkpoint::write_atomic(&body, text.as_bytes(), body.exists())?;
        let body_arg = body.to_str().ok_or("non-UTF8 PR body path")?;
        let result = if let Some(pr) = prs.first() {
            let number = pr["number"].as_u64().ok_or("PR number missing")?.to_string();
            gh(
                tree,
                lock,
                &[
                    "pr",
                    "edit",
                    &number,
                    "--repo",
                    name,
                    "--base",
                    &base,
                    "--body-file",
                    body_arg,
                ],
            )?;
            let was_draft = pr["isDraft"].as_bool().ok_or("PR draft state missing")?;
            if was_draft != draft {
                let mut args = vec!["pr", "ready", &number, "--repo", name];
                if draft {
                    args.push("--undo");
                }
                gh(tree, lock, &args)?;
            }
            pr["url"].as_str().ok_or("PR URL missing")?.to_string()
        } else {
            let title = format!("oxa work: {}", task.id);
            let mut args = vec![
                "pr",
                "create",
                "--repo",
                name,
                "--head",
                &task.branch,
                "--base",
                &base,
                "--title",
                &title,
                "--body-file",
                body_arg,
            ];
            if draft {
                args.push("--draft");
            }
            gh(tree, lock, &args)?
        };
        let _ = std::fs::remove_file(body);
        Ok((result.trim().to_string(), draft))
    })();
    // A refusal before another push has no newer destination observation.
    if let Some(receipt) = publish_receipt {
        value.record.publish_receipt = Some(receipt);
    }
    match result {
        Ok((url, draft)) => {
            value.record.pull_request = Some(url);
            value.record.draft = draft;
            value.record.outcome = accepted;
            value.record.detail.clear();
            writer.save(&mut value, Step::Finished, observer)?;
        }
        Err(error) => {
            value.record.outcome = Outcome::Blocked;
            value.record.detail = format!("publishing: {error}");
            writer.save(&mut value, Step::Publishing, observer)?;
        }
    }
    Ok(value.record)
}
/// Keep a report's per-user lease held if the controller dies during a host command.
pub fn inherit_report_lease(command: &mut Command, gate: &File) -> std::io::Result<()> {
    let gate = gate.try_clone()?;
    // SAFETY: the closure owns the descriptor, and only changes its flags
    // in this child. fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(gate.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn report_notes(
    task: &Task,
    policy: &RunPolicy,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    LockedTask::open(task, policy, false)?.report_notes(observer, reporter)
}

fn report_locked(
    locked: &LockedTask<'_>,
    observer: &dyn CheckpointObserver,
    reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
) -> Result<Record> {
    let (mut writer, mut value) = action_context(locked, observer)?;
    let (policy, lock) = (locked.policy, &locked.lock);
    let prior_step = value.record.step;
    // Reporting is ancillary: a failed save/publication remains recoverable even
    // if reporting is interrupted between durable report receipts.
    let reporting_step = match prior_step {
        Step::Saving | Step::Publishing => prior_step,
        _ => Step::Reporting,
    };
    for index in 0..value.record.problem_notes.len() {
        if value
            .record
            .reports
            .get(index)
            .is_some_and(|r| !matches!(r.disposition.as_str(), "local" | "failed"))
        {
            continue;
        }
        writer.save(&mut value, reporting_step, observer)?;
        let report = reporter(
            &value.record.problem_notes[index],
            &value.record,
            lock,
            policy.offline || value.offline,
        )
        .unwrap_or_else(|detail| super::ReportRecord {
            disposition: "failed".into(),
            draft: PathBuf::new(),
            url: None,
            detail,
        });
        if index < value.record.reports.len() {
            value.record.reports[index] = report;
        } else {
            value.record.reports.push(report);
        }
        writer.save(&mut value, reporting_step, observer)?;
    }
    writer.save(
        &mut value,
        if matches!(prior_step, Step::Saving | Step::Publishing) {
            prior_step
        } else {
            Step::Finished
        },
        observer,
    )?;
    Ok(value.record)
}

impl LockedTask<'_> {
    pub fn publish(
        &mut self,
        repo: &Path,
        binary: &Path,
        gh_binary: &Path,
        observer: &dyn CheckpointObserver,
        scan: ContentScan<'_>,
        confirm: &mut dyn FnMut(&str) -> Option<bool>,
    ) -> Result<Record> {
        publish_locked(repo, binary, self, gh_binary, observer, scan, confirm)
    }

    pub fn report_notes(
        &mut self,
        observer: &dyn CheckpointObserver,
        reporter: &mut impl FnMut(&super::ProblemNote, &Record, &File, bool) -> Result<super::ReportRecord>,
    ) -> Result<Record> {
        report_locked(self, observer, reporter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(super) fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn policy(dir: &Path, command: Vec<String>) -> RunPolicy {
        RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: "/usr/bin:/bin".into(),
            terminate_binary: None,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            offline: false,
            runner_owns_github: false,
            github_token_command: command,
            github_token_write_paths: Vec::new(),
            worker_github: Default::default(),
            autonomy: Default::default(),
            authority_now: None,
            storage: dir.join("runner"),
        }
    }

    #[test]
    fn runner_publish_uses_the_token_source() {
        let dir = crate::test_dir::PrivateDir::new("publish-token-source");
        let repo = dir.path().join("project");
        std::fs::create_dir(&repo).unwrap();
        let fake_gh = script(
            dir.path(),
            "gh",
            "case \"$GH_TOKEN\" in '') ;; ghs_RUNNER_TOKEN) echo authenticated ;; *) exit 1 ;; esac",
        );
        let echoing_gh = script(dir.path(), "gh-echo", "printf '%s' \"$GH_TOKEN\"");
        let source = script(
            dir.path(),
            "token",
            "test \"$(pwd -P)\" = / && test \"$PATH\" = /usr/bin:/bin && echo ghs_RUNNER_TOKEN",
        );
        let lock = File::create(dir.path().join("lock")).unwrap();
        // Without the setting the runner keeps the stored login: no token is passed on.
        let plain = policy(dir.path(), Vec::new());
        assert_eq!(super::super::policy_token(&plain, &repo), Ok(None));
        let seen = gh(&fake_gh, &plain, None, dir.path(), &lock, &[]);
        assert_eq!(seen, Ok(String::new()));
        // With it, gh gets exactly the command's token.
        let owned = policy(dir.path(), vec![source.display().to_string()]);
        let token = super::super::policy_token(&owned, &repo).unwrap();
        assert_eq!(token.as_deref(), Some("ghs_RUNNER_TOKEN"));
        let seen = gh(&fake_gh, &owned, token.as_deref(), dir.path(), &lock, &[]);
        assert_eq!(seen, Ok("authenticated".into()));
        assert_eq!(
            gh(&echoing_gh, &owned, token.as_deref(), dir.path(), &lock, &[]),
            Err("GitHub publication request failed".into())
        );
        // A command that fails stops the publication instead of falling back.
        let failing = script(dir.path(), "bad", "exit 1");
        let broken = policy(dir.path(), vec![failing.display().to_string()]);
        assert!(super::super::policy_token(&broken, &repo).is_err());
    }

    #[test]
    fn token_program_requires_a_vetted_location() {
        let dir = crate::test_dir::PrivateDir::new("trusted-token-source");
        let repo = dir.path().join("project");
        let tools = dir.path().join("tools");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&tools).unwrap();
        let safe = script(&tools, "mint-token", "exit 0");
        let untrusted = script(&repo, "mint-token", "exit 0");
        let link = tools.join("shortcut");
        std::os::unix::fs::symlink(&untrusted, &link).unwrap();
        let resolve = |name: &str| {
            crate::trusted_token_program(
                &[name.to_owned()],
                Some(tools.as_os_str()),
                Some(&repo),
                &[],
                None,
            )
            .map(|command| Path::new(command.get_program()).to_path_buf())
        };
        assert_eq!(resolve("mint-token"), Some(safe.canonicalize().unwrap()));
        assert!(resolve("missing-tool").is_none());
        assert!(resolve("./mint-token").is_none());
        assert!(resolve(untrusted.to_str().unwrap()).is_none());
        assert!(resolve("shortcut").is_none());
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(resolve(safe.to_str().unwrap()).is_none());
    }

    #[test]
    fn runner_rejects_interpreter_operands_from_the_project() {
        let dir = crate::test_dir::PrivateDir::new("token-operands");
        let repo = dir.path().join("project");
        std::fs::create_dir(&repo).unwrap();
        let mint = script(&repo, "mint", "echo SYNTH_MINTED");
        let argv = vec!["/bin/sh".into(), mint.display().to_string()];
        let runner = policy(dir.path(), argv);
        assert!(super::super::policy_token(&runner, &repo).is_err());
    }

    #[test]
    fn runner_rejects_helpers_under_sandbox_write_mounts() {
        let dir = crate::test_dir::PrivateDir::new("token-write-mount");
        let repo = dir.path().join("project");
        let mount = dir.path().join("mounted");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&mount).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&mount, &alias).unwrap();
        let mint = script(&mount, "mint", "echo SYNTH_MINTED");
        let mut runner = policy(dir.path(), vec![mint.display().to_string()]);
        assert_eq!(
            super::super::policy_token(&runner, &repo).unwrap().as_deref(),
            Some("SYNTH_MINTED")
        );
        runner.github_token_write_paths.push(alias);
        assert!(super::super::policy_token(&runner, &repo).is_err());

        // A writable link remains unsafe even when the final executable is outside the mount.
        let safe = script(dir.path(), "trusted", "echo SYNTH_MINTED");
        let inside = mount.join("link");
        std::os::unix::fs::symlink(safe, &inside).unwrap();
        let outside = dir.path().join("outside-link");
        std::os::unix::fs::symlink(inside, &outside).unwrap();
        runner.github_token_command = vec![outside.display().to_string()];
        assert!(super::super::policy_token(&runner, &repo).is_err());
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn repo() -> (crate::test_dir::PrivateDir, RunPolicy) {
        let dir = crate::test_dir::PrivateDir::new("publish-scan");
        let policy = RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: std::env::var_os("PATH").unwrap(),
            terminate_binary: None,
            gh_binary: None,
            push_approved: false,
            content_scan: None,
            offline: false,
            runner_owns_github: false,
            github_token_command: Vec::new(),
            github_token_write_paths: Vec::new(),
            autonomy: Default::default(),
            worker_github: Default::default(),
            authority_now: None,
            storage: dir.path().join("storage"),
        };
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        (dir, policy)
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn containing(needle: &'static str) -> impl Fn(&PushedContent<'_>) -> Option<ContentFinding> {
        move |item| {
            (item.path.contains(needle) || item.text.contains(needle)).then_some(ContentFinding {
                label: "a test secret",
                secret_path: item.path.contains(needle),
            })
        }
    }

    #[test]
    fn lane_scan_refuses_local_content_before_running_the_token_helper() {
        let (dir, mut policy) = repo();
        let secret = "synthetic-refused-content";
        git_ok(dir.path(), &["commit", "-q", "--allow-empty", "-m", secret]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        policy.content_scan = Some(std::sync::Arc::new(containing(secret)));
        let tools = crate::test_dir::PrivateDir::new("publish-scan-helper");
        let marker = tools.path().join("helper-ran");
        let helper = super::tests::script(
            tools.path(),
            "mint",
            &format!(
                "touch {} && echo SYNTH_MINTED",
                oxa_proto::sh_quote(&marker.to_string_lossy())
            ),
        );
        policy.github_token_command = vec![helper.display().to_string()];
        let error = scan_push(
            &policy,
            dir.path(),
            &head,
            "https://github.com/example/project.git",
        )
        .unwrap_err();
        assert!(error.contains("adds a test secret"), "{error}");
        assert!(!marker.exists());
        assert!(super::super::policy_token(&policy, dir.path()).unwrap().is_some());
        assert!(marker.exists());
    }

    #[test]
    fn quoted_paths_are_decoded_before_the_secret_check() {
        assert_eq!(
            diff_path("\"b/a\\\"q\\\\z\\303\\251\""),
            Some("a\"q\\z\u{e9}".to_string())
        );
        let (dir, policy) = repo();
        let secret = "sy\"nth\\etic-value";
        std::fs::write(dir.path().join(format!("{secret}.txt")), "plain\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "fixture"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let error = scan_pushed(&policy, dir.path(), &[head.as_str()], &containing(secret)).unwrap_err();
        assert!(!error.contains("etic-value"), "{error}");
        assert!(!error.contains("nth"), "{error}");
    }

    #[test]
    fn a_tab_after_an_unquoted_patch_path_is_not_part_of_the_name() {
        assert_eq!(
            diff_path("b/app config/.env\t"),
            Some("app config/.env".to_string())
        );
        assert_eq!(diff_path("b/plain/.env"), Some("plain/.env".to_string()));
        let (dir, policy) = repo();
        std::fs::create_dir(dir.path().join("app config")).unwrap();
        std::fs::write(dir.path().join("app config/.env"), "PLACEHOLDER=1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("app config/.env"), "PLACEHOLDER=opaque\n").unwrap();
        git_ok(dir.path(), &["commit", "-q", "-am", "edit"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let revisions = [head.as_str(), "--not", base.as_str()];
        let error = scan_pushed(&policy, dir.path(), &revisions, &|item| {
            (item.line.is_some() && item.path.ends_with("/.env")).then_some(ContentFinding {
                label: "content to a file name used for secrets",
                secret_path: false,
            })
        })
        .unwrap_err();
        assert!(error.contains("app config/.env:1"), "{error}");
    }

    #[test]
    fn the_root_commit_is_scanned_whatever_log_show_root_says() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["config", "log.showRoot", "false"]);
        std::fs::write(dir.path().join("root.txt"), "synthetic-root-secret\n").unwrap();
        git_ok(dir.path(), &["add", "root.txt"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "root"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[head.as_str()],
            &containing("synthetic-root-secret"),
        )
        .unwrap_err();
        assert!(error.contains("root.txt:1"), "{error}");
    }

    #[test]
    fn utf16_additions_are_scanned_as_decoded_text() {
        let secret = format!("synthetic-{}-secret", "utf16");
        for (name, big_endian) in [("le.txt", false), ("be.txt", true)] {
            let (dir, policy) = repo();
            let text = format!("first line\n{secret}\nlast line\n");
            let mut bytes = if big_endian {
                vec![0xfe, 0xff]
            } else {
                vec![0xff, 0xfe]
            };
            for unit in text.encode_utf16() {
                bytes.extend(if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                });
            }
            std::fs::write(dir.path().join(name), bytes).unwrap();
            git_ok(dir.path(), &["add", name]);
            git_ok(dir.path(), &["commit", "-q", "-m", name]);
            let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
            let error = scan_pushed(
                &policy,
                dir.path(),
                &[head.as_str()],
                &containing("synthetic-utf16-secret"),
            )
            .unwrap_err();
            assert!(error.contains(name), "{error}");
            assert!(!error.contains(secret.as_str()), "{error}");
        }
    }

    fn items(text: &str) -> Vec<(String, Option<usize>, String)> {
        let seen = std::cell::RefCell::new(vec![]);
        scan_patch(
            text,
            &|item| {
                // Pair and block-scalar contexts join lines with a newline; only the lines
                // themselves are listed here.
                if !item.text.contains('\n') {
                    seen.borrow_mut()
                        .push((item.path.to_string(), item.line, item.text.to_string()));
                }
                None
            },
            &|_, _| None,
        )
        .unwrap();
        seen.into_inner()
    }

    #[test]
    fn added_names_and_lines_are_scanned_in_ordinary_and_merge_commits() {
        let patch = "\0aaaa\n\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+first\n+++ looks like a header\ndiff --git a/old.txt b/old.txt\nindex 1111111..2222222 100644\n--- a/old.txt\n+++ b/old.txt\n@@ -3 +3 @@\n-before\n+after\n\0bbbb\n\ndiff --cc merged.txt\nindex 1111111,2222222..3333333\n--- a/merged.txt\n+++ b/merged.txt\n@@@ -4,1 -4,1 +4,2 @@@\n- ours\n -theirs\n++introduced by the merge\n+ kept from the first parent\n";
        let seen = items(patch);
        let expected = [
            ("new.txt", None, ""),
            ("new.txt", Some(1), "first"),
            ("new.txt", Some(2), "++ looks like a header"),
            ("old.txt", Some(3), "after"),
            ("merged.txt", Some(4), "introduced by the merge"),
            ("merged.txt", Some(5), "kept from the first parent"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(p, l, t)| (p.to_string(), *l, t.to_string()))
            .collect();
        assert_eq!(seen, expected);
    }

    /// A YAML header may carry chomping/indentation in either order and a trailing comment; a
    /// JSON pair may split across two added lines. Both must still reach the scanners.
    #[test]
    fn split_and_indented_credentials_are_found() {
        for header in ["|2-", "|+2", ">-2", "|2 # credential", "|", r#"|-"#] {
            let patch = format!(
                "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,2 @@\n+password: {header}\n+  opaque-pass-value-17\n"
            );
            assert!(
                scan_patch(&patch, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
                "header {header:?} was not scanned as a block scalar"
            );
        }
        let split = "\0aaaa\n\ndiff --git a/settings.json b/settings.json\nnew file mode 100644\n--- /dev/null\n+++ b/settings.json\n@@ -0,0 +1,3 @@\n+{\n+  \"password\":\n+  \"opaque-pass-value-17\"\n";
        // A quoted YAML key opens a block scalar too (`"password": |-`).
        let quoted = "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,2 @@\n+\"password\": |-\n+  opaque-pass-value-17\n";
        assert!(
            scan_patch(quoted, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
            "a quoted YAML key did not open the block scalar"
        );
        assert!(
            scan_patch(split, &containing("opaque-pass-value-17"), &|_, _| None).is_err(),
            "a JSON credential split over two lines was not scanned"
        );
    }

    /// Audit of #1264: a JSON value edited under an existing key arrives in a `-U0` patch
    /// without that key; the key is recovered from the committed file.
    #[test]
    fn an_edited_json_value_recovers_its_unchanged_key() {
        let patch = "\0aaaa\n\ndiff --git a/settings.json b/settings.json\nindex 1111111..2222222 100644\n--- a/settings.json\n+++ b/settings.json\n@@ -3 +3 @@\n-  \"placeholder-value\"\n+  \"opaque-pass-value-17\"\n";
        let content = "{\n  \"password\":\n  \"opaque-pass-value-17\"\n}\n";
        let pair = |item: &PushedContent<'_>| {
            (item.text.contains("\"password\":") && item.text.contains("opaque-pass-value-17")).then_some(
                ContentFinding {
                    label: "a test secret",
                    secret_path: false,
                },
            )
        };
        let error = scan_patch(patch, &pair, &|commit, path| {
            (commit == "aaaa" && path == "settings.json").then(|| content.to_string())
        })
        .unwrap_err();
        assert!(error.contains("settings.json:3"), "{error}");
        // A value under an ordinary line (not a key that ends its line) gets no context.
        let unrelated = "[\n  \"first\",\n  \"opaque-pass-value-17\"\n]\n";
        assert_eq!(enclosing_json_key(unrelated, 3), None);
        assert_eq!(enclosing_json_key(content, 3).as_deref(), Some("  \"password\":"));
    }

    /// A later key at the header's own indentation leaves the block scalar, so a placeheld
    /// password cannot refuse an ordinary description line below it.
    #[test]
    fn a_block_scalar_ends_at_the_next_key() {
        let patch = "\0aaaa\n\ndiff --git a/settings.yml b/settings.yml\nnew file mode 100644\n--- /dev/null\n+++ b/settings.yml\n@@ -0,0 +1,3 @@\n+password: |-\n+  ${DB_PASSWORD}\n+description: release-notes-17\n";
        let scanned = std::cell::RefCell::new(vec![]);
        scan_patch(
            patch,
            &|item| {
                scanned.borrow_mut().push(item.text.to_string());
                None
            },
            &|_, _| None,
        )
        .expect("a placeholder is not a credential");
        let texts = scanned.into_inner();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("description: release-notes-17") && !text.contains("password")),
            "the description line was still scanned against the password header: {texts:?}"
        );
    }

    #[test]
    fn a_secret_path_is_withheld_from_the_refusal() {
        let patch =
            "\0aaaa\n\ndiff --git a/hidden b/hidden\nnew file mode 100644\n--- /dev/null\n+++ b/hidden\n";
        let error = scan_patch(
            patch,
            &|_| {
                Some(ContentFinding {
                    label: "a known access token format",
                    secret_path: true,
                })
            },
            &|_, _| None,
        )
        .unwrap_err();
        assert!(!error.contains("hidden"), "{error}");
    }

    fn craft_commit(dir: &Path, header: &str) -> String {
        let raw = git_ok(dir, &["cat-file", "commit", "HEAD"]);
        let (head, message) = raw.split_once("\n\n").unwrap();
        let file = dir.join("crafted-commit");
        std::fs::write(&file, format!("{head}\n{header}\n\n{message}\n")).unwrap();
        git_ok(
            dir,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                "crafted-commit",
            ],
        )
    }

    #[test]
    fn an_extra_commit_header_is_scanned() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
        let value = ["synthetic", "-header-", "value"].concat();
        let crafted = craft_commit(dir.path(), &format!("x-note {value}"));
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[crafted.as_str()],
            &containing("header-value"),
        )
        .unwrap_err();
        assert!(error.contains("commit "), "{error}");
        assert!(!error.contains("header-value"), "{error}");
    }

    #[test]
    fn a_multi_line_signature_header_is_scanned() {
        let (dir, policy) = repo();
        git_ok(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
        let value = ["synthetic", "-signature-", "value"].concat();
        let header =
            format!("gpgsig -----BEGIN PGP SIGNATURE-----\n \n {value}\n -----END PGP SIGNATURE-----");
        let crafted = craft_commit(dir.path(), &header);
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[crafted.as_str()],
            &containing("signature-value"),
        )
        .unwrap_err();
        assert!(error.contains("commit "), "{error}");
    }

    #[test]
    fn an_edited_block_scalar_recovers_its_unchanged_key() {
        let (dir, policy) = repo();
        std::fs::write(
            dir.path().join("settings.yml"),
            "name: x\ndb:\n  password: |-\n    placeholder-value\n",
        )
        .unwrap();
        git_ok(dir.path(), &["add", "."]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(
            dir.path().join("settings.yml"),
            "name: x\ndb:\n  password: |-\n    opaque-pass-value-17\n",
        )
        .unwrap();
        git_ok(dir.path(), &["commit", "-q", "-a", "-m", "edit"]);
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]);
        let scan = |item: &PushedContent<'_>| {
            (item.text.contains("password: |-") && item.text.contains("opaque-pass-value-17")).then_some(
                ContentFinding {
                    label: "a test secret",
                    secret_path: false,
                },
            )
        };
        let error = scan_pushed(
            &policy,
            dir.path(),
            &[head.as_str(), "--not", base.as_str()],
            &scan,
        )
        .unwrap_err();
        assert!(error.contains("settings.yml"), "{error}");
    }

    #[test]
    fn only_full_object_ids_reach_cat_file() {
        assert!(valid_object_id(&"a".repeat(40)));
        assert!(valid_object_id(&"0".repeat(64)));
        assert!(!valid_object_id("--output=x"));
        assert!(!valid_object_id(&"A".repeat(40)));
    }
}
