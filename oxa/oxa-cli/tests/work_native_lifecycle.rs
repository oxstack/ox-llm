//! #307 assignment/leases: real native CLI composition with renewable ownership.
#![cfg(target_os = "linux")]
#[path = "common/work_repo.rs"]
mod work;

use oxa_fake::{FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_work::board::{assignment::*, *};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    env: TestEnv,
    claims: Claims,
    original: ItemLease,
    binding: NativeBinding,
    queue: PathBuf,
    config: PathBuf,
    storage: PathBuf,
    now: u64,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new();
        work::setup(&env, r#"["test -f .oxa/settings.json"]"#);
        let controls = env.agent_dir().join("native-lifecycle");
        let queue = controls.join("queue");
        let claims_path = controls.join("claims");
        for directory in [&controls, &queue, &claims_path] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let claims = Claims::open(&claims_path).unwrap();
        let worker: Worker = serde_json::from_value(json!({
            "id":"oxa:node/helper", "kind":"oxa", "roles":["builder"], "skills":[]
        }))
        .unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claimed_at = now - 120;
        claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from([(
                    42,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )]),
                claimed_at,
            )
            .unwrap();
        let entry = Entry {
            number: 42,
            head: "synthetic-revision".into(),
            release_pinned: false,
            action: Action::BuildIssue,
            lease: None,
            attempts_today: 0,
            eligible: true,
            evidence_incomplete: None,
        };
        let policy = Policy {
            enabled: true,
            ..Policy::default()
        };
        let original = claims
            .claim_item_for(&entry, &policy, &worker, "host-a", (claimed_at, 60), |_| Ok(None))
            .unwrap()
            .unwrap();
        let binding = NativeBinding::new(42, &"a".repeat(64), &original.lease, "agent/42").unwrap();
        assert!(claims.bind_native(&worker.id, &binding).unwrap());
        assert!(oxa_work::helper_queue::publish(
            &queue,
            &binding.key,
            "i42 fake agent/42 main\nSynthetic lifecycle task.\n",
            false,
            &policy,
        )
        .unwrap());
        let config = controls.join("config.json");
        fs::write(
            &config,
            json!({
                "enabled":true, "nativeExecution":true, "worker":worker, "host_id":"host-a",
                "queue":queue, "claims":claims_path, "pause":controls.join("paused"),
                "nativeSteering":{
                    "attempt":binding.native_task_id, "objective":"Complete the synthetic lifecycle task."
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let common = env.work().join("project/.git").canonicalize().unwrap();
        let key = format!("{:x}", Sha256::digest(common.as_os_str().as_encoded_bytes()));
        let storage = env.agent_dir().join("work").join(key);
        Self {
            env,
            claims,
            original,
            binding,
            queue,
            config,
            storage,
            now,
        }
    }

    fn run(&self, server: &FakeServer) -> (Child, Output) {
        self.invoke(
            server,
            &[
                "work",
                "--native-queue",
                "--config",
                self.config.to_str().unwrap(),
            ],
        )
    }

    fn activity(&self, enabled: bool) {
        let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap();
        config["nativeActivity"] = enabled.into();
        fs::write(&self.config, serde_json::to_vec(&config).unwrap()).unwrap();
    }

    fn without_steering(&self) {
        let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap();
        config.as_object_mut().unwrap().remove("nativeSteering");
        fs::write(&self.config, serde_json::to_vec(&config).unwrap()).unwrap();
    }

    fn invoke(&self, server: &FakeServer, args: &[&str]) -> (Child, Output) {
        let child = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .current_dir(self.env.work().join("project"))
            .env_clear()
            .envs(self.env.envs(&server.url()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.joined(child)
    }

    fn joined(&self, mut child: Child) -> (Child, Output) {
        let deadline = Instant::now() + Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("native lifecycle CLI exceeded its bounded budget");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = vec![];
        let mut stderr = vec![];
        child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
        child.stderr.take().unwrap().read_to_end(&mut stderr).unwrap();
        (
            child,
            Output {
                status,
                stdout,
                stderr,
            },
        )
    }

    fn reopened(&self) -> Claims {
        Claims::open(self.config.parent().unwrap().join("claims").as_path()).unwrap()
    }

    fn park(&self, observed_at: u64, deadline: u64) -> DependencyWait {
        renew(self);
        let rows = self.reopened().assignments().unwrap();
        let row = &rows[&42];
        let receipt = ItemLease {
            lease: self.original.lease.clone(),
            host_id: "host-a".into(),
            expires_at: row.data.item_owner.as_ref().unwrap().expires_at,
        };
        let wait = DependencyWait {
            dependency: Dependency::RequiredCi(99),
            deadline,
        };
        // This fixture has not launched a process or acquired a task lock.
        assert!(self
            .claims
            .wait_for_dependency(
                &self.original.lease.holder,
                "host-a",
                &self.binding,
                &receipt,
                observed_at,
                row.data.revision,
                &wait,
                |observed| Ok(observed == &wait),
            )
            .unwrap());
        wait
    }

    fn refill_next(&self) -> Output {
        self.claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from([(
                    44,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )]),
                self.now,
            )
            .unwrap();
        let controls = self.config.parent().unwrap();
        let taskfile = controls.join("next.txt");
        fs::write(&taskfile, "i44 fake agent/44 main\nSynthetic next task.\n").unwrap();
        fs::set_permissions(&taskfile, fs::Permissions::from_mode(0o600)).unwrap();
        let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap();
        config.as_object_mut().unwrap().remove("nativeSteering");
        config["intake"] = json!(true);
        config["partition"] = json!({"modulus":22,"residues":[0]});
        config["log"] = json!(controls.join("audit.jsonl"));
        config["packets"] = json!([{
            "number":44,"revision":"revision","action":"build_issue",
            "branch":"agent/44","taskfile":taskfile
        }]);
        fs::write(&self.config, config.to_string()).unwrap();

        let tools = self.env.tmp().join("parked-tools");
        fs::create_dir(&tools).unwrap();
        fs::set_permissions(&tools, fs::Permissions::from_mode(0o700)).unwrap();
        let gh = tools.join("gh");
        fs::write(&gh, "#!/bin/sh\n[ \"$GH_TOKEN\" = synthetic ] || exit 93\nprintf 'HTTP/2.0 200 OK\\r\\n\\r\\n'\ncat \"$0.reply\"\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(tools.join("gh.reply"), json!({"data":{"repository":{
            "mergeQueue":null,
            "pullRequests":{"totalCount":0,"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"last"}},
            "i44":{"number":44,"state":"OPEN","updatedAt":"revision"},"b44":null
        }}}).to_string()).unwrap();
        let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .env_clear()
            .args([
                "work",
                "board",
                "--refill",
                "--repo",
                "example/project",
                "--once",
                "--token-stdin",
                "--config",
            ])
            .arg(&self.config)
            .current_dir(self.env.work().join("project"))
            .envs(self.env.envs("http://127.0.0.1:1"))
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"synthetic\n").unwrap();
        self.joined(child).1
    }
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else {
                result.insert(path.strip_prefix(root).unwrap().into(), fs::read(path).unwrap());
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn renewed_owner_executes_and_settles_the_original_expired_receipt() {
    let fixture = Fixture::new();
    assert!(fixture.original.lease.expires_at < fixture.now);
    let renewed = fixture
        .claims
        .renew_item_for(
            42,
            &fixture.original.lease.holder,
            "host-a",
            &fixture.original,
            fixture.now - 90,
            600,
        )
        .unwrap()
        .unwrap();
    assert_eq!(renewed.lease, fixture.original.lease);
    assert!(renewed.expires_at > fixture.now);
    let reopened = fixture.reopened();
    let row = &reopened.assignments().unwrap()[&42];
    assert_eq!(row.state.lease.as_ref(), Some(&fixture.original.lease));
    assert_eq!(
        row.data.item_owner.as_ref().unwrap().expires_at,
        renewed.expires_at
    );
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
    let mut completion = ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic lifecycle proof."}"#,
    );
    completion.text = Some("Synthetic lifecycle complete".into());
    let server = FakeServer::start(vec![completion], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.max_tokens > 0)
            .count(),
        1
    );
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &fs::read(
            fixture
                .storage
                .join(format!("{}.json", fixture.binding.native_task_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint["record"]["id"], fixture.binding.native_task_id);
    assert_eq!(checkpoint["record"]["step"], "finished");
    let row = &fixture.reopened().assignments().unwrap()[&42];
    assert!(row.state.lease.is_none());
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
    assert!(
        row.data.lease_activity.is_none(),
        "omitted activity setting stays off"
    );
    assert_eq!(row.data.item_owner.as_ref().unwrap().host_id, "host-a");
    assert_eq!(
        row.data.item_owner.as_ref().unwrap().expires_at,
        renewed.expires_at
    );
    assert!(!fixture
        .queue
        .join(format!("board-{}.txt", fixture.binding.key))
        .exists());
}

#[test]
fn expired_current_owner_without_steering_never_takes_native_packet() {
    let fixture = Fixture::new();
    fixture.without_steering();
    let before = serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap();
    assert!(before["data"]["item_owner"]["expires_at"].as_u64().unwrap() < fixture.now);
    let queue = files(&fixture.queue);
    let server = FakeServer::start(vec![ScriptTurn::text("must not run")], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert!(
        server.requests().is_empty(),
        "expired current owner reached model: {output:?}"
    );
    assert_eq!(files(&fixture.queue), queue, "ready packet must not move");
    assert_eq!(
        serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap(),
        before,
        "expiry alone cannot settle or change the original assignment/lease/attempt"
    );
    assert!(
        !fixture
            .storage
            .join(format!("{}.json", fixture.binding.native_task_id))
            .exists(),
        "refusal precedes the first task checkpoint"
    );
}

#[test]
fn renewed_current_owner_without_steering_keeps_the_original_attempt() {
    let fixture = Fixture::new();
    renew(&fixture);
    fixture.without_steering();
    let before = &fixture.reopened().assignments().unwrap()[&42];
    assert!(fixture.original.lease.expires_at < fixture.now);
    assert!(before.data.item_owner.as_ref().unwrap().expires_at > fixture.now);
    let server = FakeServer::start(vec![ScriptTurn::text("done")], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(server.requests().len(), 1);
    let saved = checkpoint(&fixture);
    assert_eq!(saved["record"]["id"], fixture.binding.native_task_id);
    assert_eq!(saved["record"]["step"], "finished");
    let after = &fixture.reopened().assignments().unwrap()[&42];
    assert!(after.state.lease.is_none());
    assert_eq!(after.state.attempts, before.state.attempts);
    assert!(after.data.native_binding.as_ref() == Some(&fixture.binding));
    assert_eq!(
        serde_json::to_value(&after.data.item_owner).unwrap(),
        serde_json::to_value(&before.data.item_owner).unwrap()
    );
}

#[test]
fn verified_reclaim_fences_replay_without_consuming_the_old_started_packet() {
    let fixture = Fixture::new();
    let current = fixture
        .claims
        .renew_item_for(
            42,
            &fixture.original.lease.holder,
            "host-a",
            &fixture.original,
            fixture.now - 90,
            600,
        )
        .unwrap()
        .unwrap();
    assert_eq!(current.lease, fixture.original.lease);
    assert!(current.expires_at > fixture.now);
    let server = FakeServer::start(
        vec![ScriptTurn::fault(Fault::Http {
            status: 400,
            body: "Synthetic nonretryable worker failure".into(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    // Admit with valid current ownership, then join an actual failed child.
    // Its blocked whole-task checkpoint retains the original reservation.
    let (mut child, output) = fixture.run(&server);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(server.requests().len(), 1);
    let saved = checkpoint(&fixture);
    assert_eq!(saved["record"]["outcome"], "blocked");
    assert_eq!(saved["record"]["worker"]["error_class"], "engine_http");
    assert_eq!(
        fixture.reopened().assignments().unwrap()[&42]
            .state
            .lease
            .as_ref(),
        Some(&fixture.original.lease)
    );
    let started = fixture
        .queue
        .join("started")
        .join(format!("native-board-{}", fixture.binding.key))
        .join(format!("{}.txt", fixture.binding.native_task_id));
    assert_eq!(
        fs::read_to_string(started).unwrap(),
        "i42 fake agent/42 main\nSynthetic lifecycle task.\n"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .storage
                .join(format!("{}.lock", fixture.binding.native_task_id)),
        )
        .unwrap();
    lock.try_lock()
        .expect("the original child and inherited task lock are gone");
    assert!(fixture
        .claims
        .reclaim_item(
            42,
            Some(ManualAuthority::Lead),
            &current,
            current.expires_at,
            |receipt| {
                assert_eq!(receipt, &current);
                Ok(child.try_wait().map_err(|error| error.to_string())?.is_some())
            },
        )
        .unwrap());
    drop(lock); // Replay must be fenced by Claims, not a lock held by this test.
    let queue_before = files(&fixture.queue);
    let checkpoints_before = files(&fixture.storage);
    let (_, replay) = fixture.run(&server);
    assert!(
        replay.status.success(),
        "released assignment is an idle no-op: {replay:?}"
    );
    assert_eq!(server.requests().len(), 1, "replay makes no new model request");
    assert_eq!(files(&fixture.queue), queue_before);
    assert_eq!(files(&fixture.storage), checkpoints_before);
    let row = &fixture.reopened().assignments().unwrap()[&42];
    assert!(row.state.lease.is_none());
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
    assert!(row.data.lease_activity.is_none());
}

fn renew(fixture: &Fixture) {
    fixture
        .claims
        .renew_item_for(
            42,
            &fixture.original.lease.holder,
            "host-a",
            &fixture.original,
            fixture.now - 90,
            600,
        )
        .unwrap()
        .unwrap();
}

fn completion() -> FakeServer {
    let mut turn = ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic process milestone."}"#,
    );
    turn.text = Some("Synthetic activity complete".into());
    FakeServer::start(vec![turn], FakeOpts::default()).unwrap()
}

fn checkpoint(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(
            fixture
                .storage
                .join(format!("{}.json", fixture.binding.native_task_id)),
        )
        .unwrap(),
    )
    .unwrap()
}

fn assert_parked_queue_untouched(fixture: &Fixture) {
    let claim = serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap();
    let queue = files(&fixture.queue);
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert_eq!(files(&fixture.queue), queue, "a parked packet must not be taken");
    assert!(output.status.success(), "{output:?}");
    assert!(server.requests().is_empty());
    assert!(!fixture
        .storage
        .join(format!("{}.json", fixture.binding.native_task_id))
        .exists());
    assert_eq!(
        serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap(),
        claim
    );
}

#[test]
fn all_parked_native_work_keeps_its_packet_and_claim_without_a_model_call() {
    let fixture = Fixture::new();
    fixture.park(fixture.now, fixture.now + 300);
    assert_parked_queue_untouched(&fixture);
}

#[test]
fn an_elapsed_dependency_deadline_does_not_start_a_parked_native_packet() {
    let fixture = Fixture::new();
    let wait = fixture.park(fixture.now - 60, fixture.now - 30);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(wait.deadline < now);
    assert_parked_queue_untouched(&fixture);
}

#[test]
fn parked_native_work_yields_to_one_refilled_task_and_retains_its_original_receipt() {
    let fixture = Fixture::new();
    fixture.park(fixture.now, fixture.now + 300);
    let parked = serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap();
    let parked_packet = fixture.queue.join(format!("board-{}.txt", fixture.binding.key));
    let parked_bytes = fs::read(&parked_packet).unwrap();
    let refilled = fixture.refill_next();
    assert!(refilled.status.success(), "{refilled:?}");
    let next = fixture.reopened().assignments().unwrap()[&44]
        .data
        .native_binding
        .clone()
        .expect("a verified parked lease must permit the real refill");
    assert!(fixture.queue.join(format!("board-{}.txt", next.key)).is_file());
    let server = FakeServer::start(vec![ScriptTurn::text("finished")], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(server.requests().len(), 1);
    let rows = fixture.reopened().assignments().unwrap();
    assert_eq!(serde_json::to_value(&rows[&42]).unwrap(), parked);
    assert_eq!(fs::read(&parked_packet).unwrap(), parked_bytes);
    assert!(rows[&44].state.lease.is_none());
    assert_eq!(rows[&44].state.attempts, 1);
    assert!(rows[&44].data.native_binding.as_ref() == Some(&next));
    let completed: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.storage.join(format!("{}.json", next.native_task_id))).unwrap(),
    )
    .unwrap();
    assert_eq!(completed["record"]["id"], next.native_task_id);
    assert_eq!(completed["record"]["step"], "finished");
    let (_, idle) = fixture.run(&server);
    assert!(idle.status.success(), "{idle:?}");
    assert_eq!(server.requests().len(), 1, "all remaining work is still parked");
    assert_eq!(
        serde_json::to_value(&fixture.reopened().assignments().unwrap()[&42]).unwrap(),
        parked
    );
    assert_eq!(fs::read(parked_packet).unwrap(), parked_bytes);
}

#[test]
fn activity_requires_opt_in_and_finished_replay_does_not_advance_it() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        renew(&fixture);
        fixture.activity(enabled);
        let server = completion();
        let (_, output) = fixture.run(&server);
        assert!(output.status.success(), "activity={enabled}: {output:?}");
        let saved = checkpoint(&fixture);
        let row = &fixture.reopened().assignments().unwrap()[&42];
        assert!(row.state.lease.is_none());
        assert_eq!(row.state.attempts, 1);
        assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
        let activity = row.data.lease_activity.clone();
        if enabled {
            let activity = activity.as_ref().expect("enabled native result reached Claims");
            assert_eq!(activity.generation, fixture.original.lease.generation);
            assert!(activity.heartbeat_at.is_none(), "this adapter emits no heartbeat");
            let action = activity.last_action.as_ref().unwrap();
            let launch = saved["record"]["worker_launch_revision"].as_u64().unwrap();
            assert_eq!(action.launch_revision, launch);
            assert!(launch > 0 && launch < action.revision);
            assert!(action.revision < saved["record"]["checkpoint_revision"].as_u64().unwrap());
            assert!(action.at_ms > 0);
            assert!(action.at_ms <= saved["record"]["checkpoint_at_ms"].as_u64().unwrap());
        } else {
            assert!(activity.is_none(), "explicit false stays off");
        }
        let requests = server.requests().len();
        assert_eq!(requests, 1);
        let (_, replay) = fixture.run(&server);
        assert!(replay.status.success(), "{replay:?}");
        assert_eq!(server.requests().len(), requests);
        let replayed = &fixture.reopened().assignments().unwrap()[&42];
        assert_eq!(replayed.data.lease_activity, activity);
        assert!(replayed.state.lease.is_none());
        assert!(replayed.data.native_binding.as_ref() == Some(&fixture.binding));
    }
}

#[test]
fn saved_native_result_recovers_after_another_task_acknowledges_the_cursor() {
    let fixture = Fixture::new();
    renew(&fixture);
    fixture.activity(true);
    let claims_path = fixture.config.parent().unwrap().join("claims");
    // Read and lock remain possible; the real Claims atomic save must fail.
    // This leaves the actual synced WorkerFinished checkpoint before advancement.
    fs::set_permissions(&claims_path, fs::Permissions::from_mode(0o500)).unwrap();
    let server = completion();
    let (_, refused) = fixture.run(&server);
    fs::set_permissions(&claims_path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !refused.status.success(),
        "activity write fault must fire: {refused:?}"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "the real managed invocation completed"
    );
    let saved = checkpoint(&fixture);
    assert_eq!(saved["record"]["checkpoint_kind"], "worker_finished");
    let revision = saved["record"]["checkpoint_revision"].as_u64().unwrap();
    let launch = saved["record"]["worker_launch_revision"].as_u64().unwrap();
    let at_ms = saved["record"]["checkpoint_at_ms"].as_u64().unwrap();
    let row = &fixture.reopened().assignments().unwrap()[&42];
    assert!(row.data.lease_activity.is_none());
    assert_eq!(row.state.lease.as_ref(), Some(&fixture.original.lease));
    let cursor = || -> serde_json::Value {
        serde_json::from_slice(&fs::read(fixture.storage.join(".checkpoint-counter")).unwrap()).unwrap()
    };
    assert_eq!(cursor()["pending"], fixture.binding.native_task_id);

    // The ordinary caller's existing no-op observer acknowledges the foreign
    // pending result in the SAME repository cursor, without changing this task.
    let foreign = FakeServer::start(vec![ScriptTurn::text("finished")], FakeOpts::default()).unwrap();
    let (_, output) = fixture.invoke(&foreign, &["work", "../tasks.json"]);
    assert!(output.status.success(), "{output:?}");
    assert!(cursor()["pending"].is_null());
    assert!(cursor()["revision"].as_u64().unwrap() > revision);
    assert_eq!(checkpoint(&fixture), saved);
    assert!(fixture.reopened().assignments().unwrap()[&42]
        .data
        .lease_activity
        .is_none());

    let (_, resumed) = fixture.run(&server);
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(
        server.requests().len(),
        1,
        "resume must not invoke the model again"
    );
    let row = &fixture.reopened().assignments().unwrap()[&42];
    let activity = row
        .data
        .lease_activity
        .as_ref()
        .expect("saved native result re-offered");
    let action = activity.last_action.as_ref().unwrap();
    assert_eq!(
        (action.launch_revision, action.revision, action.at_ms),
        (launch, revision, at_ms)
    );
    assert_eq!(activity.generation, fixture.original.lease.generation);
    assert!(activity.heartbeat_at.is_none());
    assert!(row.state.lease.is_none());
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
}

/// #1376: the publication hold is not offline. The worker runs against the provider, finishes
/// locally verified, and the record says that publication is held (nothing was pushed).
#[test]
fn a_held_native_task_runs_online_and_ends_locally_verified_with_publication_held() {
    let fixture = Fixture::new();
    renew(&fixture);
    let mut done = ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic held task."}"#,
    );
    done.text = Some("Synthetic held task complete".into());
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("write", "write", r#"{"path":"held.txt","content":"held\n"}"#),
            done,
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let (_, output) = fixture.run(&server);
    assert!(output.status.success(), "{output:?}");
    assert!(
        server.requests().iter().any(|request| request.max_tokens > 0),
        "the worker reached the provider"
    );
    let checkpoint = checkpoint(&fixture);
    assert_eq!(checkpoint["record"]["step"], "finished");
    // The held task is accepted but owes its publication receipt, so it never reports done.
    assert_eq!(checkpoint["record"]["outcome"], "publication_pending");
    // The accepted verdict is kept next to the owed publication, not replaced by it.
    assert_eq!(checkpoint["record"]["acceptance_outcome"], "done");
    assert!(
        checkpoint["record"]["detail"]
            .as_str()
            .unwrap()
            .starts_with(
                "local verified; publication held; publication held for the runner: publication pending: committed_no_push_recorded"
            ),
        "{}",
        checkpoint["record"]["detail"]
    );
    // `--publish` on the native queue stays refused.
    let (_, refused) = fixture.invoke(
        &server,
        &[
            "work",
            "--native-queue",
            "--publish",
            "--config",
            fixture.config.to_str().unwrap(),
        ],
    );
    assert!(!refused.status.success(), "{refused:?}");
}
