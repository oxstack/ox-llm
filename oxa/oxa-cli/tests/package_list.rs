//! Package listing must read the project's declarations exactly when the owner's
//! `defaultProjectTrust` trusts the project, and explain the trust gate without bypassing it.
#[cfg(target_os = "linux")]
#[path = "common/strace.rs"]
mod strace;
use oxa_cli::run_pkg;
use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_perm::TrustStore;
use oxa_resources::{declared_packages, Scope};

fn command(env: &TestEnv, verb: &str, words: &[&str]) -> String {
    let dirs = Dirs::under(env.root());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let words = words.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        run_pkg(verb, &words, &dirs, &env.work(), &mut out, &mut err).unwrap(),
        0
    );
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

/// The owner's own trust default, user settings only.
fn owner_trust(env: &TestEnv, value: &str) {
    env.write(
        "home/.oxa/agent/settings.json",
        &format!(r#"{{"defaultProjectTrust":"{value}"}}"#),
    );
}

fn synthetic_package(env: &TestEnv) -> std::path::PathBuf {
    env.write(
        "package/skills/synthetic/SKILL.md",
        "---\nname: synthetic\ndescription: Synthetic fixture.\n---\nReturn a short answer.\n",
    );
    env.root().join("package")
}

#[test]
fn package_list_reads_project_declarations_by_default() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    // A fresh install, no settings file: the project is trusted, so declarations once hidden
    // behind the trust gate are listed and used.
    env.write("work/.oxa/settings.json", r#"{"packages":[]}"#);
    assert_eq!(command(&env, "list", &[]), "No packages declared.\n");
    let source = synthetic_package(&env);
    let installed = command(&env, "install", &["-l", source.to_str().unwrap()]);
    assert!(installed.contains("(project)"), "{installed}");
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("project "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(!listed.contains("not trusted"), "{listed}");
    assert!(!listed.contains("hidden"), "{listed}");
    assert!(
        !dirs.trust_file().exists(),
        "listing under the default saves no decision"
    );

    // `always` is the same answer written out.
    owner_trust(&env, "always");
    let listed = command(&env, "list", &[]);
    assert!(
        listed.contains("project ") && listed.contains("installed at"),
        "{listed}"
    );
}

#[test]
fn package_list_hides_project_declarations_when_the_owner_opts_out() {
    for value in ["never", "ask"] {
        let env = TestEnv::new();
        owner_trust(&env, value);
        assert_eq!(
            command(&env, "list", &[]),
            "Project package declarations, if any, are hidden: the project is not trusted (run oxa once in it to decide).\n",
            "{value}"
        );
        let source = synthetic_package(&env);
        let installed = command(&env, "install", &["-l", source.to_str().unwrap()]);
        assert!(installed.contains("(project)"), "{installed}");
        let dirs = Dirs::under(env.root());
        for words in [&[][..], &["-l"][..]] {
            let listed = command(&env, "list", words);
            assert!(listed.contains("project is not trusted"), "{value}: {listed}");
            assert!(
                listed.contains("run oxa once in it to decide"),
                "{value}: {listed}"
            );
            assert!(!listed.contains("No packages declared."), "{value}: {listed}");
            assert!(!listed.contains("installed at"), "{value}: {listed}");
        }
        assert!(!dirs.trust_file().exists(), "listing must not grant trust");

        // A saved decision by the owner is honoured, and a saved denial beats `always`.
        let mut trust = TrustStore::load(&dirs.trust_file());
        trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
        trust.approve(&dirs.trust_file(), &env.work()).unwrap();
        let listed = command(&env, "list", &[]);
        assert!(
            listed.contains("project ") && listed.contains("installed at"),
            "{value}: {listed}"
        );
        owner_trust(&env, "always");
        trust.save(&dirs.trust_file(), &env.work(), false).unwrap();
        assert!(
            command(&env, "list", &[]).contains("project is not trusted"),
            "{value}: a saved denial wins over always"
        );
    }
}

#[test]
fn package_list_explains_hidden_project_declarations_after_install() {
    let env = TestEnv::new();
    owner_trust(&env, "never");
    env.write(
        "package/skills/synthetic/SKILL.md",
        "---\nname: synthetic\ndescription: Synthetic fixture.\n---\nReturn a short answer.\n",
    );
    let source = env.root().join("package");
    let installed = command(&env, "install", &["-l", source.to_str().unwrap()]);
    assert!(installed.contains("(project)"), "{installed}");
    let dirs = Dirs::under(env.root());
    let settings_before = std::fs::read(dirs.project_settings_file(&env.work())).unwrap();
    for words in [&[][..], &["-l"][..]] {
        let listed = command(&env, "list", words);
        assert!(listed.contains("project is not trusted"), "{listed}");
        assert!(listed.contains("run oxa once in it to decide"), "{listed}");
        assert!(!listed.contains("No packages declared."), "{listed}");
        assert!(!listed.contains("installed at"), "{listed}");
    }
    assert_eq!(
        std::fs::read(dirs.project_settings_file(&env.work())).unwrap(),
        settings_before
    );
    assert!(!dirs.trust_file().exists(), "listing must not grant trust");
    let settings = Settings::load(&dirs, &env.work(), false);
    assert!(declared_packages(&dirs, &env.work(), &settings, false).is_empty());

    // Once explicitly trusted and content-approved, the normal installed state is shown.
    let mut trust = TrustStore::load(&dirs.trust_file());
    trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
    trust.approve(&dirs.trust_file(), &env.work()).unwrap();
    assert!(trust.trusted_as_approved(&env.work()));
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("project "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(!listed.contains("hidden"), "{listed}");
}

#[test]
fn package_list_keeps_user_packages_visible_and_empty_state_accurate() {
    let env = TestEnv::new();
    owner_trust(&env, "never");
    // Without reading untrusted settings we cannot distinguish empty from hidden.
    let untrusted_empty = command(&env, "list", &[]);
    assert!(untrusted_empty.contains("if any"), "{untrusted_empty}");
    assert!(
        untrusted_empty.contains("project is not trusted"),
        "{untrusted_empty}"
    );
    env.write("work/.oxa/settings.json", r#"{"packages":[]}"#);
    assert_eq!(command(&env, "list", &["-l"]), untrusted_empty);
    env.write("package/skills/synthetic/SKILL.md", "Synthetic skill.");
    let source = env.root().join("package");
    command(&env, "install", &[source.to_str().unwrap()]);
    command(&env, "install", &["-l", source.to_str().unwrap()]);
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("user "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(listed.contains("project is not trusted"), "{listed}");
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), false);
    let visible = declared_packages(&dirs, &env.work(), &settings, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].0, Scope::User);
    command(&env, "remove", &[source.to_str().unwrap()]);
    command(&env, "remove", &["-l", source.to_str().unwrap()]);
    assert_eq!(command(&env, "list", &[]), untrusted_empty);
    let mut trust = TrustStore::load(&dirs.trust_file());
    trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
    trust.approve(&dirs.trust_file(), &env.work()).unwrap();
    assert_eq!(command(&env, "list", &[]), "No packages declared.\n");
}

// Run only the reporting path in the traced subprocess, not fixture creation or checks.
#[test]
fn package_list_untrusted_settings_worker() {
    let Some(root) = std::env::var_os("OXA_PACKAGE_LIST_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let dirs = Dirs::under(&root);
    // The owner's `never` opt-in: `env_clear` above drops nothing Settings reads, but naming it in
    // the user file makes this listing's trust independent of the shipped default.
    std::fs::create_dir_all(dirs.agent_dir.as_path()).expect("create the agent directory");
    std::fs::write(&dirs.settings_file(), r#"{"defaultProjectTrust":"never"}"#)
        .expect("write the owner's trust default");
    for words in [vec![], vec!["-l".to_string()]] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_pkg("list", &words, &dirs, &root.join("work"), &mut out, &mut err).unwrap(),
            0
        );
        let listed = String::from_utf8(out).unwrap();
        assert!(listed.contains("project is not trusted"), "{listed}");
        assert!(!listed.contains("installed at"), "{listed}");
        assert!(!listed.contains("No packages declared."), "{listed}");
        assert!(err.is_empty());
    }
    assert!(!dirs.trust_file().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn package_list_does_not_open_untrusted_special_or_oversized_settings() {
    use std::{fs, os::unix::fs::symlink, process::Command};

    for kind in ["fifo", "symlink", "oversized"] {
        let env = TestEnv::new();
        owner_trust(&env, "never");
        let dirs = Dirs::under(env.root());
        let path = dirs.project_settings_file(&env.work());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = env.root().join("synthetic-target.json");
        let contents = br#"{"packages":["./synthetic-package"]}"#;
        match kind {
            "fifo" => {
                assert!(Command::new("mkfifo").arg(&path).status().unwrap().success());
            }
            "symlink" => {
                fs::write(&target, contents).unwrap();
                symlink(&target, &path).unwrap();
            }
            "oversized" => {
                fs::write(&path, contents).unwrap();
                // Sparse synthetic input, never a real user file or an unbounded device.
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(16 * 1024 * 1024)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = fs::symlink_metadata(&path).unwrap();
        let trace = env.root().join("settings-opens.trace");
        // A hard subprocess deadline makes a regressed FIFO open fail rather than hang CI.
        // strace is mandatory: output alone cannot prove that a symlink was never followed.
        let output = Command::new("timeout")
            .args(["--kill-after=1s", "5s"])
            .arg(strace::executable())
            .args(["-f", "-e", "trace=open,openat,openat2", "-o"])
            .arg(&trace)
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "package_list_untrusted_settings_worker", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OXA_PACKAGE_LIST_FIXTURE", env.root())
            .current_dir(env.work())
            .output()
            .expect("timeout and strace are required for the untrusted settings read proof");
        assert!(
            output.status.success(),
            "{kind}: mandatory traced listing failed or timed out: {output:?}"
        );
        let opens = fs::read_to_string(trace).unwrap();
        assert!(!opens.contains(path.to_str().unwrap()), "{kind}: {opens}");
        assert!(!opens.contains(target.to_str().unwrap()), "{kind}: {opens}");
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(before.file_type(), after.file_type());
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        if kind == "symlink" {
            assert_eq!(fs::read_link(&path).unwrap(), target);
            assert_eq!(fs::read(&target).unwrap(), contents);
        }
        assert!(!dirs.trust_file().exists());
    }
}

/// The trust decision runs before any settings read, so it must not open a project file at all:
/// a FIFO at the project settings path used to hang `oxa list` in the fully untrusted state.
#[cfg(target_os = "linux")]
#[test]
fn package_list_listing_never_opens_a_project_settings_path() {
    use std::{fs, os::unix::fs::FileTypeExt, process::Command};

    let env = TestEnv::new();
    owner_trust(&env, "never");
    let dirs = Dirs::under(env.root());
    let path = dirs.project_settings_file(&env.work());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(Command::new("mkfifo").arg(&path).status().unwrap().success());
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("project is not trusted"), "{listed}");
    assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
    assert!(!dirs.trust_file().exists());
}
