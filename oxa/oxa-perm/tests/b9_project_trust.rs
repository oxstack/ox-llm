//! B9 acceptance tests: Project trust.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_perm::{
    gated_resources, resolve_trust, saved_trust, DefaultTrust, TrustInputs, TrustOutcome, TrustStore,
    SHIPPED_DEFAULT_TRUST,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn inputs(default: DefaultTrust) -> TrustInputs {
    TrustInputs {
        flag: None,
        extension: None,
        saved: None,
        default,
        interactive: false,
        has_gated: true,
    }
}

/// precedence
#[test]
fn flag_beats_extension_beats_saved_beats_default() {
    // default alone decides
    assert_eq!(resolve_trust(&inputs(DefaultTrust::Trust)), TrustOutcome::Trusted);
    assert_eq!(
        resolve_trust(&inputs(DefaultTrust::Untrusted)),
        TrustOutcome::Untrusted
    );
    // saved beats default
    let saved = TrustInputs {
        saved: Some(true),
        ..inputs(DefaultTrust::Untrusted)
    };
    assert_eq!(resolve_trust(&saved), TrustOutcome::Trusted);
    let saved = TrustInputs {
        saved: Some(false),
        ..inputs(DefaultTrust::Trust)
    };
    assert_eq!(resolve_trust(&saved), TrustOutcome::Untrusted);
    // extension beats saved
    let ext = TrustInputs {
        extension: Some(false),
        saved: Some(true),
        ..inputs(DefaultTrust::Trust)
    };
    assert_eq!(resolve_trust(&ext), TrustOutcome::Untrusted);
    let ext = TrustInputs {
        extension: Some(true),
        saved: Some(false),
        ..inputs(DefaultTrust::Untrusted)
    };
    assert_eq!(resolve_trust(&ext), TrustOutcome::Trusted);
    // flag beats extension and saved
    let flag = TrustInputs {
        flag: Some(true),
        extension: Some(false),
        saved: Some(false),
        ..inputs(DefaultTrust::Untrusted)
    };
    assert_eq!(resolve_trust(&flag), TrustOutcome::Trusted);
    let flag = TrustInputs {
        flag: Some(false),
        extension: Some(true),
        saved: Some(true),
        ..inputs(DefaultTrust::Trust)
    };
    assert_eq!(resolve_trust(&flag), TrustOutcome::Untrusted);
    // ask: a saved decision beats it, and only an interactive run prompts
    let ask = TrustInputs {
        interactive: true,
        ..inputs(DefaultTrust::Ask)
    };
    assert_eq!(resolve_trust(&ask), TrustOutcome::AskUser);
    let ask_saved = TrustInputs {
        saved: Some(true),
        interactive: true,
        ..inputs(DefaultTrust::Ask)
    };
    assert_eq!(resolve_trust(&ask_saved), TrustOutcome::Trusted);
}

/// Owner decision 2026-11-21: a fresh install trusts the project; `ask` and `never` are opt-ins.
#[test]
fn shipped_default_trusts_and_the_owner_opts_out() {
    assert_eq!(SHIPPED_DEFAULT_TRUST, DefaultTrust::Trust);
    assert_eq!(DefaultTrust::from_setting(None), DefaultTrust::Trust);
    for (value, want) in [
        ("always", DefaultTrust::Trust),
        ("ask", DefaultTrust::Ask),
        ("never", DefaultTrust::Untrusted),
        ("Ask", SHIPPED_DEFAULT_TRUST),
        ("", SHIPPED_DEFAULT_TRUST),
    ] {
        assert_eq!(DefaultTrust::from_setting(Some(value)), want, "{value:?}");
    }
    // A fresh project with gated resources loads them without a question and saves nothing.
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(work.join(".oxa/skills/one")).expect("mkdir");
    let mut store = TrustStore::load(&env.agent_dir().join("trust.json"));
    assert_eq!(
        saved_trust(&store, &work, SHIPPED_DEFAULT_TRUST),
        TrustOutcome::Trusted
    );
    // The opt-ins keep the old answer, the closest saved decision wins over both, and a saved
    // denial still blocks the shipped default.
    assert_eq!(
        saved_trust(&store, &work, DefaultTrust::Ask),
        TrustOutcome::Untrusted
    );
    assert_eq!(
        saved_trust(&store, &work, DefaultTrust::Untrusted),
        TrustOutcome::Untrusted
    );
    let file = env.agent_dir().join("trust.json");
    store
        .save(&file, work.parent().unwrap(), true)
        .expect("save parent yes");
    assert_eq!(
        saved_trust(&store, &work, DefaultTrust::Untrusted),
        TrustOutcome::Trusted
    );
    store.save(&file, &work, false).expect("save denial");
    assert_eq!(
        saved_trust(&store, &work, SHIPPED_DEFAULT_TRUST),
        TrustOutcome::Untrusted
    );
}

/// saved decisions
#[test]
fn closest_parent_wins() {
    let env = TestEnv::new();
    let parent = env.work().join("proj");
    let child = parent.join("sub");
    std::fs::create_dir_all(&child).expect("mkdir");
    let file = env.agent_dir().join("trust.json");

    let mut store = TrustStore::load(&file);
    assert_eq!(store.lookup(&child), None);
    store.save(&file, &parent, true).expect("save parent");
    store.save(&file, &child, false).expect("save child");

    let reloaded = TrustStore::load(&file);
    assert_eq!(reloaded.lookup(&child), Some(false));
    assert_eq!(reloaded.lookup(&child.join("deeper/not-yet")), Some(false));
    assert_eq!(reloaded.lookup(&parent), Some(true));
    assert_eq!(reloaded.lookup(&parent.join("other")), Some(true));
    assert_eq!(reloaded.lookup(&env.work().join("elsewhere")), None);
}

/// Untrusted with gated resources skipped
#[test]
fn non_interactive_ask_is_skip() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(work.join(".oxa/extensions")).expect("mkdir");
    let gated = gated_resources(&work);
    assert!(!gated.is_empty());

    let base = TrustInputs {
        has_gated: true,
        ..inputs(DefaultTrust::Ask)
    };
    assert_eq!(resolve_trust(&base), TrustOutcome::Untrusted);
    let interactive = TrustInputs {
        interactive: true,
        ..base
    };
    assert_eq!(resolve_trust(&interactive), TrustOutcome::AskUser);
}

/// no prompt
#[test]
fn nothing_gated_is_trusted() {
    let env = TestEnv::new();
    // a project with only a context file: AGENTS.md is not gated
    std::fs::write(env.work().join("AGENTS.md"), "rules").expect("write");
    let gated = gated_resources(&env.work());
    assert!(gated.is_empty(), "unexpected gated resources: {gated:?}");

    let quiet = TrustInputs {
        has_gated: !gated.is_empty(),
        interactive: true,
        ..inputs(DefaultTrust::Ask)
    };
    assert_eq!(resolve_trust(&quiet), TrustOutcome::Trusted);
    let untrusted_default = TrustInputs {
        has_gated: false,
        ..inputs(DefaultTrust::Untrusted)
    };
    assert_eq!(resolve_trust(&untrusted_default), TrustOutcome::Trusted);
}

/// fixed order
#[test]
fn gated_resource_list() {
    let env = TestEnv::new();
    let work = env.work();
    // written in a different order than the expected list
    for rel in [
        ".oxa/APPEND_SYSTEM.md",
        ".oxa/SYSTEM.md",
        ".oxa/mcp.json",
        ".oxa/themes/dark.json",
        ".oxa/prompts/review.md",
        ".oxa/skills/x/SKILL.md",
        ".oxa/extensions/ext.js",
        "AGENTS.md",
    ] {
        env.write(&format!("work/{rel}"), "x");
    }
    env.write("work/.oxa/settings.json", r#"{"hooks": {"tool_call": "true"}}"#);

    let got = gated_resources(&work);
    let expected: Vec<String> = [
        ".oxa/settings.json",
        ".oxa/extensions",
        ".oxa/skills",
        ".oxa/prompts",
        ".oxa/themes",
        ".oxa/mcp.json",
        ".oxa/SYSTEM.md",
        ".oxa/APPEND_SYSTEM.md",
    ]
    .iter()
    .map(|rel| work.join(rel).display().to_string())
    .chain(std::iter::once(format!(
        "{} (hooks)",
        work.join(".oxa/settings.json").display()
    )))
    .collect();
    assert_eq!(got, expected);
}

/// trust.json
#[test]
fn save_keeps_other_entries_0600() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).expect("mkdir");
    let file = env.agent_dir().join("trust.json");
    env.write(
        "home/.oxa/agent/trust.json",
        r#"{"/elsewhere/proj": true, "/elsewhere/other": false, "/elsewhere/gone": null}"#,
    );

    let mut store = TrustStore::load(&file);
    assert_eq!(store.lookup(Path::new("/elsewhere/other/x")), Some(false));
    store.save(&file, &work, true).expect("save");

    let text = std::fs::read_to_string(&file).expect("read trust.json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let object = value.as_object().expect("object");
    let key = std::fs::canonicalize(&work).expect("canonical work");
    assert_eq!(
        object.get("/elsewhere/proj"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        object.get("/elsewhere/other"),
        Some(&serde_json::Value::Bool(false))
    );
    assert_eq!(
        object.get(key.to_str().expect("utf8")),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(text.ends_with('\n'));

    let mode = std::fs::metadata(&file).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(TrustStore::load(&file).lookup(&work), Some(true));
}

/// A malformed trust store loads as empty, and `save` refuses to overwrite it.
#[test]
fn malformed_trust_json_is_empty_and_never_clobbered() {
    let env = TestEnv::new();
    let file: PathBuf = env.write("home/.oxa/agent/trust.json", "[not, an, object");
    assert_eq!(TrustStore::load(&file), TrustStore::default());

    let mut store = TrustStore::default();
    assert!(store.save(&file, &env.work(), true).is_err());
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "[not, an, object");
}

/// #211: `save` waits for another process that holds the trust file's lock.
#[test]
fn save_waits_for_the_trust_file_lock() {
    let env = TestEnv::new();
    let work = env.work();
    std::fs::create_dir_all(&work).expect("mkdir");
    let file = env.agent_dir().join("trust.json");
    std::fs::create_dir_all(env.agent_dir()).expect("mkdir agent");
    let lock = std::fs::File::create(env.agent_dir().join("trust.json.lock")).expect("lock file");
    lock.lock().expect("flock");
    let (f2, w2) = (file.clone(), work.clone());
    let handle = std::thread::spawn(move || TrustStore::default().save(&f2, &w2, true));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!handle.is_finished(), "save must wait for the lock");
    drop(lock);
    handle.join().expect("thread").expect("save");
    assert_eq!(TrustStore::load(&file).lookup(&work), Some(true));
}

/// #261 review: a very deep skill file is still approved (the skill loader finds it too), and
/// the walk does not overflow the stack.
#[test]
fn approval_walk_covers_deep_trees() {
    let env = TestEnv::new();
    let work = env.work();
    let mut deep = work.join(".oxa").join("skills");
    for _ in 0..100 {
        deep.push("d");
    }
    std::fs::create_dir_all(&deep).expect("mkdir deep");
    std::fs::write(deep.join("SKILL.md"), "x").expect("write deep");
    let shallow = work.join(".oxa").join("skills").join("ok");
    std::fs::create_dir_all(&shallow).expect("mkdir");
    std::fs::write(shallow.join("SKILL.md"), "x").expect("write");
    let files = oxa_perm::approval_files(&work);
    assert!(files.contains(&shallow.join("SKILL.md")));
    assert!(
        files.contains(&deep.join("SKILL.md")),
        "deep skill must be approved"
    );
}
