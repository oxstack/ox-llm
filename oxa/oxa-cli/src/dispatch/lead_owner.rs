//! Protected selection of the native Session shared by the owner chat callers.
use super::{open_named, prepare, Args, Prep, RunMode};
use oxa_config::{Dirs, Env, Settings};
use oxa_core::Session;
use oxa_proto::{codes, Entry, EntryBody, OxaError, Value};
use oxa_session::{
    decode_entry, decode_header, new_entry_id, session_dir_name, FsStore, SessionRef, SessionStore,
};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SETTINGS: u64 = 1_048_576;
const MAX_LINE: u64 = 65_536;
const ORIGIN: &str = "oxa.lead.owner";

/// One explicitly selected Session. Command and HTTP input cannot change its identity.
#[derive(Clone)]
pub struct LeadOwnerBinding {
    dirs: Dirs,
    project: PathBuf,
    uid: u32,
    session: SessionRef,
    file: (u64, u64),
}

pub(crate) fn unavailable() -> OxaError {
    OxaError::new(
        "lead_owner_unavailable",
        "The selected Lead session is unavailable.",
    )
}

fn canonical(path: &Path) -> Result<(), OxaError> {
    if path.canonicalize().ok().as_deref() != Some(path) {
        return Err(unavailable());
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, uid: u32) -> Result<File, OxaError> {
    canonical(path)?;
    crate::lead_input::open(path, uid).map_err(|_| unavailable())
}

pub(crate) fn private_directory(path: &Path, uid: u32) -> Result<(), OxaError> {
    canonical(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(unavailable());
    }
    Ok(())
}

/// The owner-private user settings, or `None` when no settings file exists.
pub(crate) fn user_settings(dirs: &Dirs, uid: u32) -> Result<Option<Value>, OxaError> {
    let path = dirs.settings_file();
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unavailable()),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    private_file(&path, uid)?
        .take(MAX_SETTINGS + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > MAX_SETTINGS {
        return Err(unavailable());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| unavailable())
}

fn selection(dirs: &Dirs, uid: u32) -> Result<Option<String>, OxaError> {
    let Some(settings) = user_settings(dirs, uid)? else {
        return Ok(None);
    };
    let Some(id) = settings.pointer("/oxa/lead/sessionId") else {
        return Ok(None);
    };
    let id = id
        .as_str()
        .filter(|id| id.len() <= 128 && crate::args::valid_session_id(id))
        .ok_or_else(unavailable)?;
    Ok(Some(id.to_owned()))
}

/// The user-only explicit model choice for the Lead conversation, read through the
/// same owner-private settings file as the session ID. HTTP, project and prompt
/// input never supply it; `None` keeps the ordinary startup model.
pub(crate) fn lead_model(dirs: &Dirs, uid: u32) -> Result<Option<String>, OxaError> {
    let Some(settings) = user_settings(dirs, uid)? else {
        return Ok(None);
    };
    let Some(model) = settings.pointer("/oxa/lead/model") else {
        return Ok(None);
    };
    let model = model
        .as_str()
        .filter(|model| !model.is_empty() && model.len() <= 128)
        .ok_or_else(unavailable)?;
    if model.chars().any(char::is_control) {
        return Err(unavailable());
    }
    Ok(Some(model.to_owned()))
}

fn read_line(reader: &mut impl BufRead) -> Result<Vec<u8>, OxaError> {
    let mut line = Vec::new();
    reader
        .take(MAX_LINE + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| unavailable())?;
    if line.len() as u64 > MAX_LINE || line.last() != Some(&b'\n') {
        return Err(unavailable());
    }
    Ok(line)
}

fn dedicated_file(path: &Path, uid: u32, id: &str, project: &Path) -> Result<Option<(u64, u64)>, OxaError> {
    let file = private_file(path, uid)?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    let mut reader = BufReader::new(file);
    let header = decode_header(&read_line(&mut reader)?).map_err(|_| unavailable())?;
    if header.id != id {
        return Ok(None);
    }
    let origin = decode_entry(&read_line(&mut reader)?).map_err(|_| unavailable())?;
    if header.cwd != project
        || header.parent_session.is_some()
        || origin.parent.is_some()
        || !matches!(origin.body, EntryBody::Custom { custom_type, data }
            if custom_type == ORIGIN && data.as_str() == Some(id))
    {
        return Err(unavailable());
    }
    Ok(Some((metadata.dev(), metadata.ino())))
}

impl LeadOwnerBinding {
    /// The dedicated Lead session's exact id: the parent of the workers it starts.
    pub(crate) fn session_id(&self) -> &str {
        &self.session.id
    }

    /// Explicit operator action after owner authentication. Returns a new ID for
    /// user settings. Opens no provider and imports no history, model, or goal.
    pub fn create(dirs: &Dirs, canonical_project: &Path, owner_uid: u32) -> Result<String, OxaError> {
        canonical(canonical_project)?;
        if !canonical_project.is_dir() {
            return Err(unavailable());
        }
        let parent = dirs.sessions_dir.parent().ok_or_else(unavailable)?;
        private_directory(parent, owner_uid)?;
        let directory = dirs.sessions_dir.join(session_dir_name(canonical_project));
        for path in [&dirs.sessions_dir, &directory] {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(unavailable()),
            }
            private_directory(path, owner_uid)?;
        }
        let store = FsStore::new(dirs.sessions_dir.clone());
        let session = store
            .create(canonical_project, None, None)
            .map_err(|_| unavailable())?;
        store.acquire(&session).map_err(|_| unavailable())?;
        store
            .append(
                &session,
                &Entry {
                    id: new_entry_id(&|_| false),
                    parent: None,
                    ts_ms: oxa_proto::time::now_ms(),
                    body: EntryBody::Custom {
                        custom_type: ORIGIN.into(),
                        data: Value::String(session.id.clone()),
                    },
                },
            )
            .and_then(|()| store.sync(&session))
            .map_err(|_| unavailable())?;
        for directory in [directory.as_path(), dirs.sessions_dir.as_path(), parent] {
            File::open(directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| unavailable())?;
        }
        Ok(session.id)
    }

    /// Load after verifying the transport's owner. The project is a trusted caller value.
    pub fn load(dirs: &Dirs, canonical_project: &Path, owner_uid: u32) -> Result<Option<Self>, OxaError> {
        let Some(id) = selection(dirs, owner_uid)? else {
            return Ok(None);
        };
        canonical(canonical_project)?;
        private_directory(&dirs.sessions_dir, owner_uid)?;
        let directory = dirs.sessions_dir.join(session_dir_name(canonical_project));
        private_directory(&directory, owner_uid)?;
        let mut selected = None;
        for entry in fs::read_dir(&directory).map_err(|_| unavailable())? {
            let path = entry.map_err(|_| unavailable())?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(file) = dedicated_file(&path, owner_uid, &id, canonical_project)? {
                if selected.is_some() {
                    return Err(unavailable());
                }
                selected = Some((path, file));
            }
        }
        let (path, file) = selected.ok_or_else(unavailable)?;
        Ok(Some(Self {
            dirs: dirs.clone(),
            project: canonical_project.to_owned(),
            uid: owner_uid,
            session: SessionRef { path, id },
            file,
        }))
    }

    /// Use the same protected native writer for goal-only work without opening a provider.
    pub(crate) fn with_goal_store<T>(
        &self,
        operation: impl FnOnce(&FsStore, &SessionRef) -> Result<T, OxaError>,
    ) -> Result<T, OxaError> {
        let store = self.lock_store()?;
        store.commit_lock();
        operation(&store, &self.session)
    }

    fn lock_store(&self) -> Result<Arc<FsStore>, OxaError> {
        self.recheck()?;
        let store = Arc::new(FsStore::new(self.dirs.sessions_dir.clone()));
        store.acquire(&self.session).map_err(|error| {
            if error.code == oxa_session::LOCKED {
                OxaError::new(codes::BUSY, "The selected Lead session is busy.")
            } else {
                unavailable()
            }
        })?;
        self.recheck()?;
        Ok(store)
    }

    /// The entries of the selected Session, read without its writer lock: no append, no sync and
    /// no checkpoint. `FsStore::load` drops a torn last line in memory and never repairs the file.
    pub(crate) fn read_entries(&self) -> Result<Vec<Entry>, OxaError> {
        self.recheck()?;
        FsStore::new(self.dirs.sessions_dir.clone()).load(&self.session)
    }

    pub(crate) fn recheck(&self) -> Result<(), OxaError> {
        canonical(&self.project)?;
        private_directory(&self.dirs.sessions_dir, self.uid)?;
        if dedicated_file(&self.session.path, self.uid, &self.session.id, &self.project)? != Some(self.file)
            || selection(&self.dirs, self.uid)?.as_deref() != Some(self.session.id.as_str())
        {
            return Err(unavailable());
        }
        Ok(())
    }

    /// The saved goal and its revision from the same owner-only reading as the thread.
    /// The observation is a revision and a status only: no worker or model is opened.
    pub(crate) fn goal(&self) -> Result<Option<(oxa_session::Goal, u64, &SessionRef)>, OxaError> {
        let entries = self.read_entries()?;
        Ok(oxa_session::Goal::latest(&entries)?
            .map(|goal| (goal, oxa_session::Goal::revision(&entries), &self.session)))
    }

    /// Record the owner's exact decision without opening a provider or applying a change.
    pub(crate) fn record_confirmation(&self, proposal_id: &str, proposal: Value) -> Result<(), OxaError> {
        const CONFIRMED: &str = "oxa.lead.proposal_confirmed";
        self.recheck()?;
        let store = FsStore::new(self.dirs.sessions_dir.clone());
        store.acquire(&self.session)?;
        store.commit_lock();
        self.recheck()?;
        let entries = store.load(&self.session)?;
        let data = serde_json::json!({"proposal_id": proposal_id, "proposal": proposal});
        for entry in &entries {
            if let EntryBody::Custom {
                custom_type,
                data: existing,
            } = &entry.body
            {
                if custom_type == CONFIRMED
                    && existing.get("proposal_id").and_then(Value::as_str) == Some(proposal_id)
                {
                    if existing != &data {
                        return Err(unavailable());
                    }
                    return store.sync(&self.session);
                }
            }
        }
        store.append(
            &self.session,
            &Entry {
                id: new_entry_id(&|id| entries.iter().any(|entry| entry.id == id)),
                parent: entries.last().map(|entry| entry.id.clone()),
                ts_ms: oxa_proto::time::now_ms(),
                body: EntryBody::Custom {
                    custom_type: CONFIRMED.into(),
                    data,
                },
            },
        )?;
        store.sync(&self.session)
    }
}

/// Use the exact native Session under its writer lock. The trusted Lead operation
/// sends chat with `Session::prompt_literal`.
pub fn with_lead_owner<T>(
    binding: &LeadOwnerBinding,
    env: &Env,
    workers: bool,
    run: impl FnOnce(&mut Session) -> Result<T, OxaError>,
) -> Result<T, OxaError> {
    let home_workspace = crate::home_cwd::check(
        RunMode::Print,
        &binding.project,
        env,
        &binding.dirs,
        None,
        &mut std::io::empty(),
        &mut std::io::sink(),
    )?
    .workspace;
    let mut args = Args::default();
    let prep = binding.prepare_inner(&mut args, env, RunMode::Print, home_workspace, workers)?;
    let mut session = binding.open(&prep, workers)?;
    let result = run(&mut session);
    if workers {
        // The serving process owns the retained worker hook set. End this chat turn without
        // shutting that hook set down; lead_serve stops it when the owner service exits.
        session.close(&mut oxa_proto::HeadlessUi::default());
    } else {
        session.shutdown(&mut oxa_proto::HeadlessUi::default());
    }
    result
}

impl LeadOwnerBinding {
    pub(super) fn prepare(
        &self,
        args: &mut Args,
        env: &Env,
        mode: RunMode,
        home_workspace: bool,
    ) -> Result<Prep, OxaError> {
        self.prepare_inner(args, env, mode, home_workspace, true)
    }

    fn prepare_inner(
        &self,
        args: &mut Args,
        env: &Env,
        mode: RunMode,
        home_workspace: bool,
        workers: bool,
    ) -> Result<Prep, OxaError> {
        let binding = self;
        let dirs = &binding.dirs;
        let cwd = &binding.project;
        // Stage the same lock Session::open will adopt, before provider I/O.
        let store = self.lock_store()?;
        // The serving process owns the normal startup policy. Refuse a mismatch
        // before provider preparation, without changing policy for other requests.
        let (offline, remote) = crate::netguard::policy_from(false, env, &Settings::load(dirs, cwd, false));
        if offline && !oxa_proto::offline() {
            return Err(oxa_proto::offline_error("Lead chat"));
        }
        if !remote && oxa_proto::remote_engine_allowed() {
            return Err(unavailable());
        }
        args.no_extensions = true;
        args.no_tools = true;
        // Nothing but the worker tool (M1): `no_tools` drops every other tool and the worker
        // tool is the one named exception.
        args.tools = Some(if workers {
            vec![crate::workers::tool::TOOL_NAME.to_string()]
        } else {
            Vec::new()
        });
        args.no_skills = true;
        // Prepare through the ordinary gateway, then require the configured Lead
        // route to match its resolved model ID exactly.
        let route = lead_model(dirs, binding.uid)?;
        args.model = route.clone();
        let interactive = mode == RunMode::Interactive;
        let mut prep = prepare(
            args,
            env,
            dirs,
            cwd,
            mode,
            home_workspace,
            interactive,
            interactive,
            &mut |_, _| false,
            Default::default(),
            Some((&binding.session, store)),
        )
        // A configured route the normal selector cannot resolve is reported as
        // itself; every other preparation failure keeps the bounded refusal.
        .map_err(|error| match (&route, error.code) {
            (Some(_), oxa_proto::codes::NOT_FOUND) => error,
            _ => unavailable(),
        })?;
        if let Some(route) = route.as_deref() {
            if prep.built.model.as_deref() != Some(route) {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    "The configured Lead model ID must match exactly",
                ));
            }
        }
        if workers {
            self.keep_workers(&mut prep, mode);
        } else if let Some(workers) = &prep.built.hook_sets.workers {
            // A one-shot process cannot supervise work after returning. It has no tool and must
            // not briefly relaunch a saved child during SessionStart either.
            workers.suppress_resume();
        }
        Ok(prep)
    }

    /// The worker tool of the owner Lead session: saved workers are resumed only after the owner
    /// binding was rechecked (`open`), and outside a terminal session the one hook set (supervisor
    /// and children) of this process is used on every chat turn, so a worker the Lead started
    /// outlives the turn that started it. Children stop at lead shutdown or Ctrl-C of the tree
    /// (`stop_owner_workers`), and when the terminal session closes.
    fn keep_workers(&self, prep: &mut Prep, mode: RunMode) {
        let Some(made) = prep.built.hook_sets.workers.clone() else {
            return;
        };
        let workers = if mode == RunMode::Interactive {
            made
        } else {
            let kept = crate::workers::tool::kept_owner_workers(&self.dirs.agent_dir, made.clone());
            if !Arc::ptr_eq(&kept, &made) {
                let old = Arc::as_ptr(&made) as *const ();
                for hook in &mut prep.built.hook_sets.hooks {
                    if Arc::as_ptr(hook) as *const () == old {
                        *hook = kept.clone();
                    }
                }
                prep.built.hook_sets.workers = Some(kept.clone());
            }
            kept
        };
        workers.hold_resume();
    }

    pub(super) fn session_ref(&self) -> &SessionRef {
        &self.session
    }

    pub(super) fn open(&self, prep: &Prep, workers: bool) -> Result<Session, OxaError> {
        // Verified before any session is opened; nothing starts for an unverified caller.
        self.recheck()?;
        let mut session =
            open_named(&prep.built, None, prep.open.clone(), None).map_err(|_| unavailable())?;
        self.recheck()?;
        if session.inner().sref != self.session {
            return Err(unavailable());
        }
        // The dedicated session keeps one history across surfaces, but a one-shot chat declares no
        // tools while the terminal declares `worker`: a replayed stored prefix would keep the
        // other surface's tool set, so the terminal asks for its own before the first prompt.
        session
            .sync_tools(&mut oxa_proto::NullSink)
            .map_err(|_| unavailable())?;
        // Only now, with the binding verified, do saved workers start again.
        if let Some(worker_hooks) = &prep.built.hook_sets.workers {
            let _ = worker_hooks.resume_after_owner_check(workers);
        }
        Ok(session)
    }
}
