//! Drive the registered real binary with private local observations and no cloud/GitHub.
use oxa_fake::TestEnv;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Child, Command, Stdio},
    time::Duration,
};

fn command(test: &TestEnv) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .env("TZ", "UTC0")
        .current_dir(test.work())
        .stdin(Stdio::null());
    command
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
thread_local! {
    static AGENT: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
/// The bearer header of the local client token the running lead wrote for its owner.
/// A local lead publishes the token before it reports its listener; the brief wait covers a
/// read that races the atomic replace. Every test here records the agent dir it started.
fn bearer() -> String {
    let dir = AGENT.with(|agent| {
        agent
            .borrow()
            .clone()
            .expect("the lead start recorded its agent dir")
    });
    for _ in 0..200 {
        if let Ok(token) = std::fs::read_to_string(dir.join("lead-client-token")) {
            return format!("Authorization: Bearer {}\r\n", token.trim());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the running lead published no client token")
}
fn get(port: u16, path: &str, authority: &str, extra: &str) -> String {
    get_raw(port, path, authority, &format!("{}{extra}", bearer()))
}
fn get_raw(port: u16, path: &str, authority: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

fn start(test: &TestEnv, sample: &std::path::Path) -> Running {
    start_with_situation(test, sample, None)
}
fn start_with_situation(
    test: &TestEnv,
    sample: &std::path::Path,
    situation: Option<&std::path::Path>,
) -> Running {
    AGENT.with(|agent| *agent.borrow_mut() = Some(test.agent_dir()));
    let mut invocation = command(test);
    if let Some(path) = situation {
        invocation.env("OXA_LEAD_SITUATION", path);
    }
    let mut child = Running(
        invocation
            .args(["lead", "serve"])
            .env("OXA_LEAD_BOARD_LOG", sample)
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(test.root().join("lead.stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    // Keep reading until the child exits. After the listener line a local lead prints the URL
    // that carries the client token; a closed read end fails that write and the lead exits
    // before it serves anything.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = receive
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("lead serve did not report its listener");
        if line.contains("listening") {
            break;
        }
    }
    child
}

fn post(port: u16, path: &str, host: &str, origin: &str, extra: &str, body: &str) -> String {
    post_raw(port, path, host, origin, &format!("{}{extra}", bearer()), body)
}
fn post_raw(port: u16, path: &str, host: &str, origin: &str, extra: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n{extra}\r\n{body}", body.len()).unwrap();
    // Refusals may close with the rejected request body still unread.
    let mut input = BufReader::new(stream);
    let mut response = String::new();
    while !response.ends_with("\r\n\r\n") {
        assert!(input.read_line(&mut response).unwrap() > 0);
    }
    let length: usize = response
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    input.read_exact(&mut body).unwrap();
    response.push_str(std::str::from_utf8(&body).unwrap());
    response
}

fn form_nonce(page: &str) -> &str {
    page.split("name=csrf value=\"")
        .nth(1)
        .expect("no-JS form must be present")
        .split('"')
        .next()
        .unwrap()
}

fn with_fixed_chat(run: impl FnOnce(u16, &str, &std::path::Path)) {
    let test = TestEnv::new();
    // `start_with_situation` names the agent dir the token went to.
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"fixed_prompts":true,"port":port
        }}}})
        .to_string(),
    );
    let directory = test.root().join("inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let board = directory.join("board.jsonl");
    std::fs::write(
        &board,
        b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
    )
    .unwrap();
    let situation = directory.join("situation.json");
    let snapshot = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
        .lines()
        .find_map(|line| oxa_proto::work::Snapshot::from_json(line.as_bytes()).ok())
        .unwrap();
    std::fs::write(&situation, snapshot.to_json().unwrap()).unwrap();
    for path in [&board, &situation] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(reservation);
    // `start_with_situation` names the agent dir the token went to.
    let _server = start_with_situation(&test, &board, Some(&situation));
    let authority = format!("127.0.0.1:{port}");
    run(port, &authority, &test.root().join("lead.stderr"));
}

#[test]
fn fixed_chat_form_posts_without_js_and_reuses_protected_observations() {
    with_fixed_chat(|port, authority, _agent_dir| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        assert_eq!(nonce.len(), 43);
        assert!(page.contains("method=post action=/chat"));
        assert!(page.contains("form-action 'self'"));
        let origin = format!("http://{authority}");
        for prompt in ["status", "workers", "attention"] {
            let reply = post(
                port,
                "/chat",
                authority,
                &origin,
                "",
                &format!("csrf={nonce}&prompt={prompt}"),
            );
            assert_eq!(reply.lines().next(), Some("HTTP/1.1 303 See Other"));
            let target = reply
                .lines()
                .find_map(|line| line.strip_prefix("Location: "))
                .unwrap();
            assert_eq!(target, format!("/chat?prompt={prompt}"));
            let page = get(port, target, authority, "");
            assert!(page.contains("<!doctype html>") && page.contains("0 tokens"));
            let fact = if prompt == "status" {
                "0 items"
            } else {
                "Snapshot observations unavailable"
            };
            assert!(page.contains(fact), "{prompt}");
            assert!(get(port, &format!("{target}&fragment=1"), authority, "").contains(fact));
        }
        let refused = |path, host, source, extra, body: &str| {
            let reply = post(port, path, host, source, extra, body);
            assert!(reply.starts_with("HTTP/1.1 4"));
            assert!(!reply.contains("private-input-marker") && !reply.contains("0 tokens"));
        };
        let valid = format!("csrf={nonce}&prompt=status");
        refused("/chat", authority, "https://attacker.invalid", "", &valid);
        refused("/chat", "attacker.invalid", &origin, "", &valid);
        for body in [
            "csrf=wrong&prompt=status".into(),
            format!("{valid}&prompt=workers"),
            format!("csrf={nonce}&prompt=private-input-marker"),
            "x".repeat(257),
        ] {
            refused("/chat", authority, &origin, "", &body);
        }
        for extra in [
            "Origin: https://attacker.invalid\r\n",
            "Transfer-Encoding: chunked\r\n",
        ] {
            refused("/chat", authority, &origin, extra, &valid);
        }
        refused("/workers", authority, &origin, "", &valid);
    });
}

#[test]
fn fixed_form_response_preserves_browser_origin() {
    with_fixed_chat(|port, authority, _agent_dir| {
        assert!(get(port, "/chat", authority, "").contains("Referrer-Policy: same-origin\r\n"));
    });
}

#[test]
fn fixed_post_redirects_to_the_selected_refresh_url() {
    with_fixed_chat(|port, authority, _agent_dir| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        for prompt in ["status", "workers", "attention"] {
            let reply = post(
                port,
                "/chat",
                authority,
                &format!("http://{authority}"),
                "",
                &format!("csrf={nonce}&prompt={prompt}"),
            );
            assert_eq!(reply.lines().next(), Some("HTTP/1.1 303 See Other"));
            assert!(reply.contains(&format!("Location: /chat?prompt={prompt}\r\n")));
        }
    });
}

#[test]
fn same_uid_client_cannot_read_protected_situation() {
    with_fixed_chat(|port, authority, _agent_dir| {
        assert!(get(port, "/asks", authority, "").contains("Owner asks unavailable"));
    });
}

#[test]
fn same_uid_client_needs_the_minted_token_for_pages_and_posts() {
    with_fixed_chat(|port, authority, stderr| {
        let dir = AGENT.with(|dir| dir.borrow().clone().unwrap());
        let file = dir.join("lead-client-token");
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        let token = std::fs::read_to_string(&file).unwrap();
        // Without the token neither a page nor a form submission is served.
        assert!(get_raw(port, "/", authority, "").starts_with("HTTP/1.1 401"));
        assert!(get_raw(port, "/", authority, "Authorization: Bearer wrong\r\n").starts_with("HTTP/1.1 401"));
        let nonce_page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&nonce_page);
        let origin = format!("http://{authority}");
        let body = format!("csrf={nonce}&prompt=status");
        let denied = post_raw(port, "/chat", authority, &origin, "", &body);
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        // The printed URL exchanges the token for a cookie and redirects without the query.
        let opened = get_raw(port, &format!("/chat?t={token}"), authority, "");
        assert!(opened.starts_with("HTTP/1.1 303"), "{opened}");
        assert!(opened.contains("Location: /chat\r\n"), "{opened}");
        assert!(opened.contains(&format!(
            "Set-Cookie: oxa_lead={token}; HttpOnly; SameSite=Strict; Path=/"
        )));
        let cookie = format!("Cookie: oxa_lead={token}\r\n");
        assert!(get_raw(port, "/chat", authority, &cookie).starts_with("HTTP/1.1 200"));
        let accepted = post_raw(port, "/chat", authority, &origin, &cookie, &body);
        assert!(accepted.starts_with("HTTP/1.1 303"), "{accepted}");
        // No token bytes reach the service journal.
        let journal = std::fs::read_to_string(stderr).unwrap();
        assert!(!journal.contains(token.trim()), "{journal}");
        assert!(journal.contains("reason=client_token"), "{journal}");
    });
}

#[test]
fn registered_help_works_without_home_or_runtime_inputs() {
    let test = TestEnv::new();
    for args in [vec!["lead", "serve", "--help"], vec!["help", "lead", "serve"]] {
        let output = command(&test).args(args).env_remove("HOME").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("lead serve"));
    }
    for command_name in ["board", "status", "asks", "chat", "run"] {
        let output = command(&test)
            .args(["lead", command_name])
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostic = match command_name {
            "asks" | "run" => "HOME is not set",
            _ => "unavailable",
        };
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic));
    }
}

#[test]
fn real_server_is_user_enabled_authorized_and_rereads_private_board() {
    let test = TestEnv::new();
    test.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"lead":{"serve":{"enabled":true}}}}"#,
    );
    let disabled = command(&test).args(["lead", "serve"]).output().unwrap();
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("disabled in user settings"));
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
    );
    for (args, input, expected) in [
        (vec!["--offline", "lead", "serve"], "", "offline"),
        (
            vec!["lead", "serve"],
            "relative-private-marker",
            "must be absolute",
        ),
    ] {
        let output = command(&test)
            .args(args)
            .env("OXA_LEAD_BOARD_LOG", input)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
    let directory = test.root().join("board-input");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sample = directory.join("board.jsonl");
    std::fs::write(
        &sample,
        b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
    )
    .unwrap();
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(reservation);
    let child = start(&test, &sample);
    let authority = format!("127.0.0.1:{port}");
    let page = get(port, "/", &authority, "");
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("No open PRs in this snapshot"));
    assert!(!page.contains("method=post"));
    for (path, title) in [
        ("/", "Status"),
        ("/asks", "Asks"),
        ("/board", "Board"),
        ("/workers", "Workers"),
        ("/usage", "Usage"),
        ("/chat", "Chat with lead"),
    ] {
        for fragment in [false, true] {
            let path = if fragment {
                format!("{path}?fragment=1")
            } else {
                path.into()
            };
            let response = get(port, &path, &authority, "");
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "{path}: {}",
                response.lines().next().unwrap_or("<empty response>")
            );
            assert!(response.contains(&format!(">{title}</h1>")), "{path}");
            assert_eq!(response.contains("<!doctype html>"), !fragment, "{path}");
        }
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    for prompt in ["status", "workers", "attention"] {
        for suffix in ["", "&fragment=1"] {
            let path = format!("/chat?prompt={prompt}{suffix}");
            let reply = get(port, &path, &authority, "");
            assert!(reply.starts_with("HTTP/1.1 200"), "{path}");
            assert!(reply.contains("0 tokens") && reply.contains("role=log"));
            if prompt == "status" {
                assert!(
                    reply.contains("0 items")
                        && reply.contains("href=/board>Board · observed 1970-01-01 00:00 UTC")
                );
            }
            assert_eq!(reply.contains("<!doctype html>"), suffix.is_empty());
            assert!(get(port, &path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
        }
    }
    assert!(get(port, "/chat?prompt=unknown", &authority, "").starts_with("HTTP/1.1 404"));
    let entries: Vec<_> = (1..=101)
        .map(|number| {
            serde_json::json!({
                "number":number,"head":"synthetic-head","release_pinned":false,
                "action":"ready","lease":null,"attempts_today":0,"eligible":true
            })
        })
        .collect();
    let record = serde_json::json!({"schema_version":1,"observed_at":10,
        "board":{"observed_at":10,"entries":entries}});
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    for (path, first, last, absent) in [
        ("/board", 1, 50, 51),
        ("/board?page=1", 51, 100, 1),
        ("/board?page=2", 101, 101, 100),
    ] {
        let response = get(port, path, &authority, "");
        assert!(response.starts_with("HTTP/1.1 200"), "{path}");
        for number in [first, last] {
            assert!(response.contains(&format!("#{number}<")), "{path} #{number}");
        }
        assert!(!response.contains(&format!("#{absent}<")), "{path} #{absent}");
        let separator = if path.contains('?') { '&' } else { '?' };
        let fragment = get(port, &format!("{path}{separator}fragment=1"), &authority, "");
        assert!(fragment.starts_with("HTTP/1.1 200") && !fragment.contains("<!doctype"));
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    for page in ["-1", "01", "18446744073709551615"] {
        assert!(get(port, &format!("/board?page={page}"), &authority, "").starts_with("HTTP/1.1 404"));
    }
    let mut filtered = record.clone();
    for (index, row) in filtered["board"]["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        row["action"] = if index < 60 {
            "fix_findings"
        } else if index < 100 {
            "wait_review"
        } else {
            "ready"
        }
        .into();
    }
    std::fs::write(&sample, format!("{filtered}\n")).unwrap();
    for (query, first, last, absent) in [
        ("f=fix", 1, 50, 51),
        ("f=fix&page=1", 51, 60, 61),
        ("f=waiting", 61, 100, 1),
        ("f=ready", 101, 101, 100),
    ] {
        let path = format!("/board?{query}");
        let reply = get(port, &path, &authority, "");
        assert!(reply.starts_with("HTTP/1.1 200"), "{path}");
        for text in ["Needs fix (60)", "Waiting (40)", "Ready (1)"] {
            assert!(reply.contains(text), "{text}");
        }
        for number in [first, last] {
            assert!(reply.contains(&format!("#{number}<")), "{path}");
        }
        assert!(!reply.contains(&format!("#{absent}<")), "{path}");
        let fragment = get(port, &format!("{path}&fragment=1"), &authority, "");
        assert!(fragment.starts_with("HTTP/1.1 200") && !fragment.contains("<!doctype"));
        assert!(get(port, &path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    assert!(get(port, "/board?f=blocked", &authority, "").contains("Blocked observations unavailable"));
    for query in [
        "f=unknown",
        "f=fix&f=ready",
        "page=1&page=2",
        "f=fix&other=x",
        "f=fix&page=01",
    ] {
        assert!(
            get(port, &format!("/board?{query}"), &authority, "").starts_with("HTTP/1.1 404"),
            "{query}"
        );
    }
    let record = serde_json::json!({"schema_version":1,"observed_at":10,"board":{
        "observed_at":10,"entries":[{"number":17,"head":"private-head-marker",
        "release_pinned":true,"action":"fix_findings","lease":null,
        "attempts_today":2,"eligible":false}]}});
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    assert!(get(port, "/board", &authority, "").contains("href=/items/17"));
    for path in ["/items/17", "/items/17?fragment=1"] {
        let response = get(port, path, &authority, "");
        assert_eq!(response.lines().next(), Some("HTTP/1.1 200 OK"), "{path}");
        for text in [
            "Item #17",
            "Fix findings",
            "Release",
            "Unleased",
            "2",
            "aria-current=page>Board",
        ] {
            assert!(response.contains(text), "{text}");
        }
        assert!(!response.contains("private-head-marker"));
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    assert!(get(port, "/items/18", &authority, "").contains("Item unavailable"));
    for path in [
        "/items/0",
        "/items/01",
        "/items/-1",
        "/items/18446744073709551616",
        "/items/17/x",
    ] {
        assert!(
            get(port, path, &authority, "").starts_with("HTTP/1.1 404"),
            "{path}"
        );
    }
    assert!(get(port, "/", "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    assert!(get(
        port,
        "/",
        &authority,
        "Tailscale-User-Login: owner@example.invalid\r\n"
    )
    .starts_with("HTTP/1.1 403"));
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o644)).unwrap();
    let event = get(port, "/events", &authority, "");
    assert!(event.contains("event: snapshot\ndata: <main>"));
    assert!(event.contains("Board unavailable"));
    assert!(!event.contains(sample.to_str().unwrap()));
    drop(child);
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let proxy = |uid| {
        serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"port":port,"identity":"tailscale",
            "allowed_identities":["owner@example.invalid"],
            "proxy":{"uid":uid,"authority":"lead.example.invalid"}
        }}}})
        .to_string()
    };
    test.write("home/.oxa/agent/settings.json", &proxy(uid));
    let child = start(&test, &sample);
    let login = "Tailscale-User-Login: owner@example.invalid\r\n";
    assert!(get(port, "/", "lead.example.invalid", login).starts_with("HTTP/1.1 200"));
    for (host, header) in [
        ("attacker.invalid", login),
        ("lead.example.invalid", ""),
        (
            "lead.example.invalid",
            "Tailscale-User-Login: guest@example.invalid\r\n",
        ),
    ] {
        assert!(get(port, "/", host, header).starts_with("HTTP/1.1 403"));
    }
    drop(child);
    test.write("home/.oxa/agent/settings.json", &proxy(uid ^ 1));
    let _child = start(&test, &sample);
    assert!(get(port, "/", "lead.example.invalid", login).starts_with("HTTP/1.1 403"));
}

#[test]
fn refused_chat_posts_record_allowlisted_facts_in_the_real_service_journal() {
    with_fixed_chat(|port, authority, stderr| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        let origin = format!("http://{authority}");
        let valid = format!("csrf={nonce}&prompt=status");
        // Each refusal is one chat fact: identity, origin, CSRF and body.
        for (host, source, extra, body) in [
            ("attacker.invalid", origin.as_str(), "", valid.as_str()),
            (authority, "https://attacker.invalid", "", valid.as_str()),
            (authority, origin.as_str(), "", "csrf=wrong&prompt=status"),
            (authority, origin.as_str(), "", &format!("{valid}&extra=1")),
        ] {
            let reply = post(port, "/chat", host, source, extra, body);
            assert!(reply.starts_with("HTTP/1.1 403"), "{host} {body}: {reply}");
        }
        // A wrong-method chat request is also a chat refusal.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "DELETE /chat HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");

        let journal = std::fs::read_to_string(stderr).unwrap();
        let lines: Vec<&str> = journal
            .lines()
            .filter_map(|line| line.strip_prefix("lead chat refusal "))
            .collect();
        assert_eq!(lines.len(), 5, "{journal}");
        for (line, expected) in lines.iter().zip([
            "status=403 stage=identity reason=not_allowed fix=",
            "status=403 stage=origin reason=cross_origin fix=",
            "status=403 stage=csrf reason=invalid fix=",
            "status=403 stage=body reason=late_parse fix=",
            "status=405 stage=body reason=method fix=",
        ]) {
            assert!(
                line.starts_with(expected) && line.len() > expected.len(),
                "{line}"
            );
        }
        // No payload, CSRF, identity or authority bytes ever reach the journal.
        assert!(!journal.contains(nonce));
        assert!(!journal.contains("csrf=wrong"));
        assert!(!journal.contains("attacker.invalid"));
        assert!(!journal.contains("prompt"));
        assert!(!journal.contains(authority));
    });
}

/// One protected stuck-work record for the Lead view: panels (main checks,
/// verified merge, queue membership), landing (queue age at the exact head) and
/// queued-run pages, each independently optional (#1391).
fn stuck_record(
    now: u64,
    main_red: bool,
    merged_seconds_before: u64,
    queued_prs: Option<usize>,
    overdue: bool,
    runs: Option<bool>,
) -> serde_json::Value {
    let merged = (now.saturating_sub(merged_seconds_before)) * 1000;
    let merged = format!("{}Z", &oxa_session::iso_from_ms(merged)[..19]);
    let head = "a".repeat(40);
    let entries = [serde_json::json!({
        "number":17,"head":head,"release_pinned":false,"action":"ready",
        "lease":null,"attempts_today":0,"eligible":true
    })];
    let merge_queue = match queued_prs {
        None => serde_json::Value::Null,
        Some(count) => serde_json::json!({"entries":{"totalCount":count,"nodes":(0..count)
            .map(|_| serde_json::json!({"pullRequest":{"number":17,"headRefOid":head}}))
            .collect::<Vec<_>>()}}),
    };
    let mut record = serde_json::json!({
        "schema_version":1,"observed_at":now,
        "board":{"observed_at":now,"entries":entries},
        "panels":{
            "id":"example-repo",
            "main":{
                "refUpdateRule":{"requiredStatusCheckContexts":["gate"]},
                "rules":{"totalCount":0,"nodes":[]},
                "target":{
                    "oid":head,
                    "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[
                        {"name":"gate","status":"COMPLETED",
                         "conclusion": if main_red { "FAILURE" } else { "SUCCESS" }}
                    ]}},
                    "associatedPullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[
                        {"repository":{"id":"example-repo"},"baseRefName":"main",
                         "mergeCommit":{"oid":head},"mergedAt":merged}
                    ]}
                }
            },
            "mergeQueue":merge_queue
        }
    });
    if overdue {
        record["landing"] = serde_json::json!({
            "17":{"next":"already_queued","queue_alarm":true,
                  "queue":{"enqueued_at":0,"group_head":null}}
        });
    } else {
        record["landing"] = serde_json::json!({
            "17":{"next":"already_queued","queue_alarm":false,
                  "queue":{"enqueued_at":now,"group_head":null}}
        });
    }
    if let Some(stalled) = runs {
        let mut rows = Vec::new();
        if stalled {
            let updated = (now.saturating_sub(3600)) * 1000;
            let updated = format!("{}Z", &oxa_session::iso_from_ms(updated)[..19]);
            rows.push(serde_json::json!({"id":29,"status":"queued","updated_at":updated}));
        }
        record["runs"] = serde_json::json!([{"total_count":rows.len(),"workflow_runs":rows}]);
    }
    record
}

fn write_record(directory: &std::path::Path, record: &serde_json::Value) -> std::path::PathBuf {
    let sample = directory.join("board.jsonl");
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
    sample
}

#[test]
fn protected_view_shows_every_stuck_signal_and_keeps_missing_sources_explicit() {
    let test = TestEnv::new();
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
    );
    let directory = test.root().join("alarm-inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    let authority = format!("127.0.0.1:{port}");
    drop(reservation);

    // One active record carries every signal: main red, the 45-minute no-merge,
    // the over-age queue membership and a stalled queued run.
    let sample = write_record(
        &directory,
        &stuck_record(now, true, 46 * 60, Some(1), true, Some(true)),
    );
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    for expected in [
        "role=alert",
        "Checks on main are failing.",
        "No PR merged into main for 45 minutes or more at the last observation.",
        "PR #17 has waited too long in the merge queue",
        "Workflow runs queued for more than 15 minutes (1)",
        "Run 29",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    drop(server);

    // A partial observation keeps the active alarms and names the missing source.
    let sample = write_record(&directory, &stuck_record(now, true, 46 * 60, Some(1), true, None));
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    assert!(page.contains("Checks on main are failing."));
    assert!(page.contains("PR #17 has waited too long"));
    assert!(page.contains("Queued workflow runs unavailable."), "{page}");
    drop(server);

    // An absent panels record leaves its signals explicitly unavailable: the
    // landing age alone cannot claim queue membership it never observed.
    let mut partial = stuck_record(now, false, 10 * 60, None, true, None);
    partial.as_object_mut().unwrap().remove("panels");
    let sample = write_record(&directory, &partial);
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    for expected in [
        "Main checks unavailable.",
        "No-merge alarm unavailable.",
        "Merge queue membership unavailable.",
        "role=status",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    assert!(!page.contains("waited too long"), "{page}");
    assert!(!page.contains("role=alert"), "{page}");
    drop(server);

    // A routine healthy read stays quiet: no attention section at all.
    let sample = write_record(
        &directory,
        &stuck_record(now, false, 10 * 60, Some(0), false, Some(false)),
    );
    let _server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    assert!(!page.contains("id=attention"), "{page}");
    assert!(page.contains("Queue empty at observation"), "{page}");
}

#[test]
fn authorized_durable_asks_reach_the_protected_view_as_a_live_readback() {
    // The one authenticated live readback (#1391): an ask the Lead loop wrote to
    // the durable Session is read live by the protected view through the
    // registered owner-bound caller, with its original revision.
    let test = TestEnv::new();
    let dirs = oxa_config::Dirs::under(test.root());
    std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let authority = "lead.example.invalid";
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{
            "sessionId":id,
            "serve":{"enabled":true,"port":port,"identity":"tailscale","owner_chat":true,
                "allowed_identities":["owner@example.invalid"],
                "proxy":{"uid":uid,"authority":authority}}
        }}})
        .to_string(),
    );
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    let directory = test.root().join("live-inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    let sample = write_record(&directory, &stuck_record(now, true, 46 * 60, None, false, None));
    drop(reservation);

    // The Lead loop escalates the red unowned PR into the durable Session.
    let looped = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .current_dir(&project)
        .env("OXA_LEAD_BOARD_LOG", &sample)
        .args(["--offline", "lead", "run", "--once"])
        .output()
        .unwrap();
    assert!(looped.status.success(), "{looped:?}");
    let state: serde_json::Value = serde_json::from_slice(&looped.stdout).unwrap();
    assert_eq!(state["state"], "stuck_escalated", "{state}");
    let ask_id = state["stuck"]["escalated"][0].as_str().unwrap().to_owned();

    let mut invocation = command(&test);
    invocation
        .args(["lead", "serve"])
        .env("OXA_LEAD_BOARD_LOG", &sample)
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(test.root().join("lead-live.stderr")).unwrap());
    let mut child = Running(invocation.spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    // Drain the whole start output, as `start_with_situation` does, so the URL line finds a
    // reader even though this test starts the service itself.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = receive
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("lead serve did not report its listener");
        if line.contains("listening") {
            break;
        }
    }

    // A proxy-mode lead mints no client token: its proof is the forwarded login.
    let login = "Tailscale-User-Login: owner@example.invalid\r\n";
    let page = get_raw(port, "/", authority, login);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Checks on main are failing."), "{page}");
    assert!(page.contains("Unanswered owner decisions (1)"), "{page}");
    let asks = get_raw(port, "/asks", authority, login);
    assert!(asks.contains(&ask_id), "{asks}");
    assert!(asks.contains("revision"), "{asks}");
    assert!(
        asks.contains("read live from the authorized durable Session"),
        "{asks}"
    );
    // The runner situation stays a separate source: its absence never hides the
    // durable asks and never claims they were observed empty.
    assert!(asks.contains("Runner asks unavailable."), "{asks}");
    // The durable ask never reaches an unauthorized peer.
    let refused = get_raw(
        port,
        "/",
        authority,
        "Tailscale-User-Login: guest@example.invalid\r\n",
    );
    assert!(refused.starts_with("HTTP/1.1 403"));
    drop(child);
}

/// GET that returns once the reply headers have arrived, for a stream that stays open.
fn get_open(port: u16, path: &str, authority: &str) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{}\r\n",
        bearer()
    )
    .unwrap();
    let mut reply = String::new();
    let mut byte = [0u8; 1];
    while !reply.ends_with("\r\n\r\n") {
        let read = stream.read(&mut byte).unwrap();
        assert!(read > 0, "stream ended in its headers: {reply}");
        reply.push(byte[0] as char);
    }
    stream
}

/// Append what arrives within the next second; `true` when that includes a `lead-update`.
fn quiet(stream: &mut TcpStream) -> bool {
    let mut text = [0u8; 2048];
    match stream.read(&mut text) {
        Ok(read) => String::from_utf8_lossy(&text[..read]).contains("lead-update"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

#[test]
fn idle_streams_share_one_frame_and_only_publish_real_changes() {
    // Five open tabs cost what none costs: the stream is woken only by a published frame,
    // never by a one-second poll. A transient read failure keeps the last page.
    let test = TestEnv::new();
    let dirs = oxa_config::Dirs::under(test.root());
    std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"sessionId":id,
            "serve":{"enabled":true,"port":port}}}})
        .to_string(),
    );
    let directory = test.root().join("observer-inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    let sample = write_record(
        &directory,
        &stuck_record(now, false, 10 * 60, Some(0), false, Some(false)),
    );
    let authority = format!("127.0.0.1:{port}");
    drop(reservation);
    let _server = start(&test, &sample);
    let mut tabs = (0..5)
        .map(|_| get_open(port, "/events/live", &authority))
        .collect::<Vec<_>>();
    let page = get(port, "/", &authority, "");
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("Queue empty at observation"), "{page}");
    // A corrupt worker registry is a typed refusal on the page; the board beside it stays.
    let worker = test.write(&format!("home/.oxa/agent/workers/{id}.json"), "not a registry");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o600)).unwrap();
    for tab in &mut tabs {
        assert!(quiet(tab), "a published frame must reach an open tab");
    }
    // Five idle tabs: no further event, so none of them refreshes the page.
    for tab in &mut tabs {
        assert!(!quiet(tab), "an idle stream polled the source");
    }
    let page = get(port, "/", &authority, "");
    assert!(page.contains("Queue empty at observation"), "{page}");
}
