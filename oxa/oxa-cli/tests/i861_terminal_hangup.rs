//! #861: a lost PTY must stop the real interactive process, not leave a spinning orphan.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::process::Command;

const PTY_CLOSE: &str = r#"
import os, pty, select, signal, subprocess, sys, time
master, slave = pty.openpty()
args = [sys.argv[1], '--yolo', '--model', 'fake-model', '--no-mcp',
    '--no-extensions', '--no-skills']
if not sys.argv[2].startswith('trust-'):
    args += ['--no-context-files']
child = subprocess.Popen(args,
    stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
try:
    output = b''
    deadline = time.monotonic() + 15
    ready = b'Project trust:' if sys.argv[2].startswith('trust-') else b'READY'
    while ready not in output:
        assert child.poll() is None, 'interactive process exited before READY'
        assert time.monotonic() < deadline, 'interactive startup deadline'
        if select.select([master], [], [], .1)[0]:
            output += os.read(master, 65536)
    if sys.argv[2] == 'tool':
        os.write(master, b'Run the synthetic tool.\r')
        deadline = time.monotonic() + 15
        while not os.path.exists('tool-started'):
            assert child.poll() is None, 'tool process exited before admission'
            assert time.monotonic() < deadline, 'tool admission deadline'
            if select.select([master], [], [], .1)[0]:
                output += os.read(master, 65536)
    if sys.argv[2] in ['signal', 'trust-signal']:
        child.send_signal(signal.SIGHUP)
    else:
        # No controlling terminal: closing the master isolates EOF/EIO from automatic SIGHUP.
        os.close(master)
        master = None
    try:
        code = child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        raise AssertionError('terminal disappeared but interactive oxa stayed alive')
    assert code == 129, 'terminal-loss exit: ' + str(code)
finally:
    if master is not None:
        os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=3)
"#;

fn terminal_loss(case: &str) {
    let env = TestEnv::new();
    // The trust question is the owner's `ask` opt-in: the shipped default trusts the project
    // without a dialog, so these cases name `ask` for the question to exist at all.
    let mut settings = String::from(
        r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off","permissions":{"defaultMode":"yolo"},"retry":{"enabled":false}}"#,
    );
    if case.starts_with("trust-") {
        std::fs::create_dir_all(env.work().join(".oxa")).unwrap();
        std::fs::write(
            env.work().join(".oxa/APPEND_SYSTEM.md"),
            "synthetic gated context",
        )
        .unwrap();
        settings.insert_str(1, r#""defaultProjectTrust":"ask","#);
    }
    let turns = if case == "tool" {
        vec![ScriptTurn::tool(
            "sleeping",
            "bash",
            r#"{"command":"printf started > tool-started; sleep 20; printf finished > tool-finished"}"#,
        )]
    } else {
        vec![ScriptTurn::text("unused")]
    };
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    std::fs::write(env.home().join(".oxa/agent/settings.json"), settings).unwrap();
    let output = Command::new("python3")
        .args(["-I", "-c", PTY_CLOSE, env!("CARGO_BIN_EXE_oxa"), case])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if case == "tool" {
        assert!(!env.work().join("tool-finished").exists());
        assert_eq!(server.requests().len(), 1, "no final model turn on terminal loss");
    } else {
        assert!(
            server.requests().is_empty(),
            "idle terminal loss starts no model turn"
        );
    }
    let sessions = FsStore::new(env.agent_dir().join("sessions")).list(None);
    assert_eq!(sessions.len(), usize::from(!case.starts_with("trust-")));
    if case.starts_with("trust-") {
        assert!(!oxa_config::Dirs::under(env.root()).trust_file().exists());
    } else {
        let status: serde_json::Value = serde_json::from_slice(
            &std::fs::read(sessions[0].sref.path.with_extension("status.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(status["phase"], "finished");
        assert_eq!(status["outcome"]["error_class"], "terminal_closed");
        assert_eq!(status["outcome"]["exit_code"], 129);
    }
}

#[test]
fn lost_pty_exits_promptly_with_hangup_status() {
    terminal_loss("eof");
}

#[test]
fn sighup_shuts_down_through_the_interactive_loop() {
    terminal_loss("signal");
}

#[test]
fn lost_pty_cancels_active_tool_and_preserves_session() {
    terminal_loss("tool");
}

/// Pipe EOF preserves the existing empty-print usage error and idle RPC outcome.
#[test]
fn headless_stdin_eof_returns_without_a_model_turn() {
    for mode in ["print", "rpc"] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off"}"#,
        );
        let server = FakeServer::start(Vec::new(), FakeOpts::default()).unwrap();
        let output = Command::new("python3")
            .args([
                "-I",
                "-c",
                r#"
import subprocess, sys
args = [sys.argv[1], '--model', 'fake-model', '--no-mcp', '--no-extensions',
    '--no-skills', '--no-context-files']
args += ['-p'] if sys.argv[2] == 'print' else ['--mode', 'rpc']
child = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
try:
    out, err = child.communicate(input=b'', timeout=3)
    expected = 2 if sys.argv[2] == 'print' else 4
    assert child.returncode == expected, (child.returncode, err.decode())
    if sys.argv[2] == 'print':
        assert b'"error_class":"usage"' in err, err.decode()
        assert b'no prompt:' in err, err.decode()
    else:
        assert b'"outcome":"nothing_to_do"' in err, err.decode()
finally:
    if child.poll() is None:
        child.kill()
    child.wait(timeout=3)
"#,
                env!("CARGO_BIN_EXE_oxa"),
                mode,
            ])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(server.requests().is_empty());
    }
}

#[test]
fn terminal_loss_during_project_trust_exits_without_approval() {
    terminal_loss("trust-eof");
}

#[test]
fn sighup_during_project_trust_exits_without_approval() {
    terminal_loss("trust-signal");
}
