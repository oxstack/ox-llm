//! Exercise the built CLI with synthetic sessions only; never starts a model.
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "fixture-session";
const PRIVATE: &str = "fixture-private-text-not-for-the-report";
const BIN: &str = env!("CARGO_BIN_EXE_oxa");

struct Fixture {
    path: PathBuf,
    writer: BufWriter<File>,
    entries: usize,
}

impl Fixture {
    fn new(env: &TestEnv) -> Self {
        let path = env.work().join("sessions/project/session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        writeln!(
            writer,
            "{}",
            json!({
                "type":"session", "version":3, "id":ID, "cwd":"/fixture",
                "timestamp":"2026-01-01T00:00:00.000Z"
            })
        )
        .unwrap();
        Self {
            path,
            writer,
            entries: 0,
        }
    }

    fn entry(&mut self, mut value: Value) {
        value["parentId"] = if self.entries == 0 {
            Value::Null
        } else {
            json!(format!("entry-{}", self.entries))
        };
        self.entries += 1;
        value["id"] = json!(format!("entry-{}", self.entries));
        value["timestamp"] = json!("2026-01-01T00:00:00.000Z");
        writeln!(self.writer, "{value}").unwrap();
    }

    fn prefix(&mut self, color: &str) {
        self.entry(json!({"type":"message", "message":{"role":"system",
            "sections":{"preamble": format!("fixture: {color} {PRIVATE}")},
            "toolsJson":"[]", "sha256":"00".repeat(32)
        }}));
    }

    fn usage(&mut self, counts: Value) {
        self.entry(json!({"type":"usage", "kind":"chat", "usage":counts}));
    }
}

fn invoke(env: &TestEnv, reference: &str, json: bool, timer: Option<&Path>) -> Output {
    let mut cmd = if let Some(report) = timer {
        let mut cmd = Command::new("/usr/bin/time");
        cmd.args(["-f", "wall_s=%e user_s=%U system_s=%S peak_rss_kib=%M", "-o"])
            .arg(report)
            .arg(BIN);
        cmd
    } else {
        Command::new(BIN)
    };
    cmd.env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .env("OXA_SESSION_DIR", env.work().join("sessions"))
        .current_dir(env.work())
        .args(["--offline", "cache", reference]);
    if json {
        cmd.arg("--json");
    }
    cmd.output().unwrap()
}

fn scan(out: Output) -> Value {
    assert!(
        out.status.success(),
        "cache exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    for private in [PRIVATE, ID, "entry-", "/fixture", &"00".repeat(32)] {
        assert!(!stdout.contains(private));
    }
    serde_json::from_str(&stdout).unwrap()
}

#[test]
fn cache_shows_saved_divergence_without_attributing_hits_to_equal_counts() {
    let env = TestEnv::new();
    let mut f = Fixture::new(&env);
    f.prefix("red");
    let observed = [0, 0, 997, 1197, 1597, 1397, 1997, 1797, 2397];
    for (i, cached) in observed.into_iter().enumerate() {
        if i == 5 {
            f.prefix("tan");
        }
        f.entry(json!({"type":"message", "message":{"role":"assistant", "content":PRIVATE}}));
        f.usage(json!({"input":1000 + 200 * i, "cacheRead":cached}));
    }
    f.usage(json!({"input":2800}));
    f.usage(json!({"unreported":true}));
    f.writer.flush().unwrap();
    let before = fs::read(&f.path).unwrap();

    let report = scan(invoke(&env, "fixture-sess", true, None));
    assert_eq!(report["scan_complete"], true);
    let rows = report["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 11);
    for (i, cached) in observed.into_iter().enumerate() {
        assert_eq!(rows[i]["cached"], cached);
        assert!(rows[i]["expected_cached"].is_null());
        assert!(rows[i]["matched_request"].is_null());
        assert!(rows[i]["first_differing_message"].is_null());
        assert_eq!(rows[i]["miss_cause"], "unknown");
    }
    assert_eq!(rows[1]["previous_input_reuse_estimate"], 1000);
    assert_eq!(rows[5]["recorded_boundary"], "epoch");
    assert_eq!(rows[5]["saved_anchor"]["against_request"], 5);
    assert_eq!(rows[5]["saved_anchor"]["first_differing_system_byte"], 9);
    assert!(rows[5]["previous_input_reuse_estimate"].is_null());
    assert!(rows[9]["cached"].is_null());
    assert!(rows[10]["input"].is_null());
    assert!(rows[10]["cached"].is_null());

    let text = invoke(&env, f.path.to_str().unwrap(), false, None);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("system differs at byte 9"));
    assert!(!text.contains(PRIVATE));
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn cache_skips_oversized_records_and_torn_tail_without_inventing_history() {
    let env = TestEnv::new();
    let mut f = Fixture::new(&env);
    f.prefix("red");
    f.usage(json!({"input":128, "cacheRead":0}));
    f.entry(
        json!({"type":"message", "message":{"role":"toolResult", "content":"x".repeat(2 * 1024 * 1024)}}),
    );
    f.usage(json!({"input":256, "cacheRead":128}));
    write!(f.writer, "{{\"type\":\"usage\"").unwrap();
    f.writer.flush().unwrap();

    let report = scan(invoke(&env, f.path.to_str().unwrap(), true, None));
    assert_eq!(report["scan_complete"], false);
    assert_eq!(report["requests"].as_array().unwrap().len(), 2);
    assert_eq!(report["requests"][1]["cached"], 128);
    assert!(report["requests"][1]["saved_anchor"].is_null());
    assert!(report["requests"][1]["previous_input_reuse_estimate"].is_null());
}

#[test]
fn cache_realistic_session_cost() {
    let env = TestEnv::new();
    let mut f = Fixture::new(&env);
    f.prefix("red");
    let tool_output = "synthetic tool output line\n".repeat(5000);
    for i in 0..240 {
        f.entry(json!({"type":"message", "message":{"role":"user", "content":"continue"}}));
        f.usage(json!({"input":8192 + i, "cacheRead":8190 + i}));
        f.entry(json!({"type":"message", "message":{"role":"assistant", "content":"checking output"}}));
        f.entry(json!({"type":"message", "message":{"role":"toolResult", "content":tool_output}}));
        f.usage(json!({"input":12500 + i, "cacheRead":8191 + i}));
    }
    f.writer.flush().unwrap();
    let timing = env.work().join("cache-time.txt");
    let timer =
        (cfg!(target_os = "linux") && Path::new("/usr/bin/time").is_file()).then_some(timing.as_path());
    let report = scan(invoke(&env, ID, true, timer));
    assert_eq!(report["scan_complete"], true);
    assert_eq!(report["requests"].as_array().unwrap().len(), 480);
    if timer.is_some() {
        eprintln!(
            "cache fixture: {} bytes, 480 records; {}",
            fs::metadata(&f.path).unwrap().len(),
            fs::read_to_string(timing).unwrap().trim()
        );
    }
}
