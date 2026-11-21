//! D-INT (architect): the UI loop state. Everything `run_loop` does per iteration lives here:
//! input routing (dialog > selector > search > global keys > viewport > editor), the app
//! actions, and the two layouts (regular = main screen with scrollback, fullscreen = alternate
//! screen with a viewport, selection and search).
// ported from pi packages/coding-agent/src/modes/interactive/interactive-mode.ts and packages/tui/src/tui-alt-screen.ts
use crate::app::{
    apply_event, blocks_from_entries, load_theme, now_ms, omarchy_theme_name, palette_watch, push_notice,
    refresh_status, resolve_ui_theme, theme_name, view_settings, ViewEnv,
};
use crate::dialogs::trust_question;
use crate::selectors::{
    cycle_model, cycle_thinking, setting_items, ForkPointSelector, ModelChoice, ModelSelector, Outcome,
    ScopedModelsSelector, SessionPick, SessionPicker, SettingsSelector, ThemeSelector, ThinkingSelector,
    TreeFilter, TreePick, TreeSelector,
};
use crate::skin::stage::{built_in, pending_of, Director};
use crate::skin::{
    frame_box, frame_inner_width, hot_theme, oxa_arcade_line, oxa_footer, oxa_splash, oxa_stage_line,
    oxa_title_text, oxa_yolo_banner, reduced_motion, scene_cells, Animation, ArcadeScene, ArcadeState,
    HintContext, SPRITE_FRAME_MS,
};
use crate::{
    activity_text, choose_clip_method, compose_fullscreen, copy_text, debug_dump, dispatch_slash,
    edit_externally, editor_border_token, highlight_matches, highlight_selection, hotkeys_lines, image_part,
    read_clipboard_image, render_block, render_footer, render_header, suspend_process, title_text, Activity,
    AppAction, Block, DialogHost, DialogKey, HeaderInfo, InteractiveOpts, RegularPrinter, Reopen,
    ScrollbarMode, SelectEvent, SelectorKind, SessionCmd, SessionInfo, SlashCx, SoftWrap, StatusModel,
    TranscriptSearch, TrustChoice, UiRequest, ViewCx, ViewModel, ViewSettings, Viewport, WorkerMsg,
};
use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::Handle;
use oxa_export::default_output_name;
use oxa_proto::{
    AgentEvent, Autonomy, DangerAnswer, Level, Mode, OxaError, Part, PermissionMode, ThinkingLevel, Value,
};
use oxa_session::{SessionMeta, SessionRef, Tree};
use oxa_tui::{
    detect_caps, list_themes, resolve_theme, title_sequence, CombinedProvider, Component, CursorPos, Editor,
    EditorAction, EditorOpts, FileIndex, Frame, InputEvent, Interactive, Key, KeyKind, Mouse, MouseKind,
    ProcessTerminal, Renderer, ScreenMode, SlashCommand, TermCaps, TermMode, TermOpts, Terminal, Theme,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// The editor's layout settings: `editorPaddingX` (0..3), `autocompleteMaxVisible` (3..20).
pub(crate) fn editor_opts_from(settings: &oxa_config::Settings) -> EditorOpts {
    let defaults = EditorOpts::default();
    EditorOpts {
        padding_x: settings.u32("editorPaddingX", 0).min(3) as u16,
        autocomplete_max_visible: settings
            .u32("autocompleteMaxVisible", defaults.autocomplete_max_visible as u32)
            .clamp(3, 20) as usize,
        ..defaults
    }
}

/// `fullscreenWheelScrollLines`: a number 1..100, or `auto` (the default; anything else).
/// pi accelerates `auto` everywhere but in a local macOS terminal.
pub(crate) fn wheel_accel(settings: &oxa_config::Settings, env: &[(String, String)]) -> crate::WheelAccel {
    let fixed = settings
        .get("fullscreenWheelScrollLines")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 100) as u16);
    let over_ssh = env
        .iter()
        .any(|(k, v)| matches!(k.as_str(), "SSH_CONNECTION" | "SSH_CLIENT" | "SSH_TTY") && !v.is_empty());
    let accelerate = !cfg!(target_os = "macos") || over_ssh;
    crate::WheelAccel::new(fixed, accelerate)
}

/// The notice when no editor command is configured.
const NO_EDITOR: &str = "No editor: set externalEditor, VISUAL or EDITOR";

/// Most files indexed for `@` completion.
const FILE_INDEX_MAX: usize = 5000;

/// Rows the fullscreen footer below a dialog spends before any extension status line: the
/// activity line, the HUD and the hint bar. A dialog bounds its preview page by what is left,
/// so a row counted as reviewed is a row on screen; an over-estimate only shortens the page.
const FULLSCREEN_FOOTER_ROWS: usize = 3;

/// The selector shown in place of the editor.
enum Overlay {
    Model(ModelSelector),
    Scoped(ScopedModelsSelector),
    Thinking(ThinkingSelector),
    Mode(crate::ModeSelector),
    Theme(ThemeSelector),
    Settings(SettingsSelector),
    Sessions(SessionPicker),
    Tree(TreeSelector),
    Fork(ForkPointSelector),
    /// `/mcp` (F7): the configured MCP servers of the session's manager.
    Mcp(crate::McpSelector),
}

/// A question the app itself asked through the dialog host; the answer arrives on `rx`.
enum Ask {
    Trust {
        rx: Receiver<Option<usize>>,
        choices: Vec<TrustChoice>,
    },
    /// pi's three-way choice before a `/tree` navigation.
    Summary {
        rx: Receiver<Option<usize>>,
        target: String,
        refill: Option<String>,
    },
    SummaryFocus {
        rx: Receiver<Option<String>>,
        target: String,
        refill: Option<String>,
    },
}

/// Rendered lines per block, kept between frames (fullscreen draws the whole transcript, and
/// only blocks that changed are rendered again).
#[derive(Default)]
struct BlockCache {
    width: u16,
    settings: Option<ViewSettings>,
    theme_epoch: u64,
    header: Vec<String>,
    entries: Vec<(Block, Vec<String>)>,
    /// The assembled content and the first line of every block.
    content: Vec<String>,
    starts: Vec<usize>,
    /// The block index the last `update` assembled `content` from (`None`: nothing changed).
    #[cfg(test)]
    rebuilt_from: Option<usize>,
}

impl BlockCache {
    /// Bring the cache up to date; `true` when the content changed.
    fn update(
        &mut self,
        vm: &ViewModel,
        header: &[String],
        width: u16,
        theme: &Theme,
        theme_epoch: u64,
        settings: &ViewSettings,
        env: &ViewEnv,
    ) -> bool {
        let mut changed = false;
        // The first block whose lines changed; `0` also when the header or layout changed.
        let mut first: usize = usize::MAX;
        #[cfg(test)]
        {
            self.rebuilt_from = None;
        }
        if self.width != width || self.theme_epoch != theme_epoch || self.settings.as_ref() != Some(settings)
        {
            self.width = width;
            self.theme_epoch = theme_epoch;
            self.settings = Some(settings.clone());
            self.entries.clear();
            changed = true;
            first = 0;
        }
        if self.header != header {
            self.header = header.to_vec();
            changed = true;
            first = 0;
        }
        if self.entries.len() > vm.blocks.len() {
            self.entries.truncate(vm.blocks.len());
            changed = true;
            first = first.min(self.entries.len());
        }
        let cx = ViewCx {
            width,
            theme,
            settings,
            caps: env.caps,
            hooks: env.hooks,
            cwd: &vm.status.cwd,
            home: &vm.status.home,
            keymap: env.keymap,
        };
        for (i, block) in vm.blocks.iter().enumerate() {
            match self.entries.get_mut(i) {
                Some((old, _)) if old == block => {}
                Some(slot) => {
                    *slot = (block.clone(), render_block(block, &cx));
                    changed = true;
                    first = first.min(i);
                }
                None => {
                    self.entries.push((block.clone(), render_block(block, &cx)));
                    changed = true;
                    first = first.min(i);
                }
            }
        }
        if changed {
            // Keep the lines of the blocks before the first changed one.
            let first = first.min(self.entries.len());
            if first == 0 {
                self.content.clear();
                self.starts.clear();
                self.content.extend(self.header.iter().cloned());
            } else if first < self.starts.len() {
                self.content.truncate(self.starts[first]);
                self.starts.truncate(first);
            }
            #[cfg(test)]
            {
                self.rebuilt_from = Some(first);
            }
            for (_, lines) in self.entries.iter().skip(self.starts.len()) {
                self.starts.push(self.content.len());
                if lines.is_empty() {
                    continue;
                }
                if !self.content.is_empty() {
                    self.content.push(String::new());
                }
                self.content.extend(lines.iter().cloned());
            }
        }
        changed
    }
}

pub(crate) struct Loop<'a> {
    opts: &'a mut InteractiveOpts,
    handle: Handle,
    /// The worker's stop signal; also says when the worker runs the boot preload.
    pub(crate) worker: crate::app::WorkerStop,
    cmds: Sender<SessionCmd>,
    caps: TermCaps,
    vm: ViewModel,
    theme: Theme,
    theme_name: String,
    theme_epoch: u64,
    /// The terminal's colours (OSC 10/11 or `COLORFGBG`), for the `system` theme.
    term_colors: Option<oxa_tui::TerminalColors>,
    view: ViewSettings,
    editor: Editor,
    editor_opts: EditorOpts,
    dialogs: DialogHost,
    asks: Vec<Ask>,
    overlay: Option<Overlay>,
    /// A `/tree` or `/fork` selector waiting for the tree snapshot of the worker.
    pending_open: Option<SelectorKind>,
    /// The last tree snapshot (fork points need the parents).
    tree: Option<Tree>,
    /// The session picker lists every directory.
    picker_all: bool,
    /// `-r`: open the session picker when the worker has announced its session.
    picker_pending: bool,
    /// The `/tree` navigator closed for pi's summary question: Escape there re-opens it with
    /// the same selection (pi `showTreeSelector(entryId)`).
    tree_back: Option<Overlay>,
    /// The loaded resources of the start-up listing (`[Context]`, `[Skills]`, ...).
    pub(crate) resources: Vec<(String, Vec<String>)>,
    /// When the last Escape on an empty, idle editor arrived (pi `lastEscapeTime`; 0 = none).
    last_escape_ms: u64,
    /// Models of the Ctrl+P cycle (`enabledModels`, or the session-only choice).
    scoped: Vec<String>,
    /// `scoped` follows the `enabledModels` patterns (until the user picks a session-only
    /// scope in /scoped-models).
    scope_from_settings: bool,
    /// A model switch sent to the worker and not yet answered: the next cycle key continues
    /// from it (keys pressed in quick succession each move one step).
    model_pending: Option<String>,
    /// The worker has answered a command since `model_pending` was set (its Status settles it).
    model_answered: bool,
    session: SessionInfo,
    inbox: Option<oxa_session::Inbox>,
    files: Option<FileIndex>,
    /// Images pasted for the next prompt.
    pending_parts: Vec<Part>,
    printer: RegularPrinter,
    renderer: Renderer,
    fullscreen: bool,
    viewport: Viewport,
    cache: BlockCache,
    mouse_sel: crate::Selector,
    /// `terminal.clearOnShrink`.
    clear_on_shrink: bool,
    /// Until when (ms) the transient `auto` scrollbar is shown.
    bar_until: u64,
    /// Lines per wheel event (`fullscreenWheelScrollLines`).
    wheel: crate::WheelAccel,
    search: TranscriptSearch,
    search_open: bool,
    last_frame: Vec<String>,
    /// OXA's look: the activity shown last and when it began (the elapsed time and the spinner).
    act_kind: Option<std::mem::Discriminant<Activity>>,
    act_since: u64,
    /// The retry attempt shown last: each attempt waits anew, so its countdown restarts (#2050).
    act_attempt: u32,
    /// OXA's look: when the theme was (re)built and when its palette file was last looked at.
    palette_checked_ms: u64,
    /// The theme line of the splash (`oxa-omarchy (tokyo-night)`), set when a theme is installed.
    theme_label: String,
    /// O-185: follows the desktop palette while `oxa-omarchy` is the chosen theme.
    palette: Option<oxa_tui::PaletteWatch>,
    /// O-184: the animation step that was last drawn (OXA's look redraws once per step, not
    /// once per loop turn, so an idle or busy engine costs the UI next to nothing).
    anim_step: u64,
    /// The arcade scene's counters of the run (coins, the closing scene).
    arcade: ArcadeState,
    /// Design 3.2 (C4): the scene director of the activity row, used when `ui.animationScene`
    /// is unset and the terminal does not draw pixels.
    stage: Director,
    /// OXA's fullscreen look: the editor's suggestion panel of this frame and the editor's
    /// first row among the bottom lines (the panel floats above it, over the transcript).
    popup_overlay: Option<(usize, Vec<String>)>,
    /// The graphics tier of the arcade: the frames the terminal holds, whether the terminal
    /// answered the graphics query and draws placeholders, image bytes waiting to be written
    /// before the next frame, and "delete everything first" (resize, back from a suspend).
    pixel: crate::skin::PixelStage,
    gfx_answered: bool,
    gfx_placeholders: bool,
    gfx_out: String,
    gfx_reset: bool,
    /// O-182: the theme as loaded; `theme` is its hot variant while YOLO is on (OXA's look).
    cool_theme: Theme,
    /// The effective permission mode last shown (plan takes precedence over the YOLO switch).
    yolo_shown: Option<PermissionMode>,
    /// #202: the level `/yolo off` and Ctrl+Alt+O return to; the full-access notice was shown.
    yolo_prev: Autonomy,
    yolo_noticed: bool,
    yolo_banner_since: u64,
    yolo_banner_until: u64,
    /// What was last written as the window title.
    sent_title: Option<String>,
    /// Assume focus until the terminal reports otherwise (unsupported terminals stay quiet).
    focused: bool,
    run_finished: bool,
    /// Enter after explicit Ctrl+C starts a new prompt, even before settlement is observed.
    abort_requested: bool,
    pub(crate) dirty: bool,
    /// Ctrl+C was pressed on an empty editor; a second press within two seconds quits.
    armed: bool,
    armed_at: Option<std::time::Instant>,
    reset_screen: bool,
    header_pending: bool,
    pub(crate) quit: Option<i32>,
    /// V020-OPTIMIZE #13: when the draft is prefilled (debounce, once per draft and wire).
    draft: crate::draft_trigger::DraftTrigger,
}

impl<'a> Loop<'a> {
    pub(crate) fn new(
        term: &mut dyn Terminal,
        opts: &'a mut InteractiveOpts,
        handle: Handle,
        cmds: Sender<SessionCmd>,
        mut initial: ViewModel,
    ) -> Loop<'a> {
        let caps = opts.caps.unwrap_or_else(|| term.caps());
        let term_colors = crate::app::terminal_colors(&*opts, term.colors());
        let gfx_answered = term.graphics();
        let gfx_placeholders = oxa_tui::env_gfx_placeholders(&opts.env);
        let (theme, theme_name) = match load_theme(&*opts, caps, term_colors.as_ref()) {
            Ok(theme) => (theme, theme_name(&*opts)),
            Err(error) => {
                push_notice(
                    &mut initial,
                    Level::Warning,
                    format!("{}; using dark theme", error.message),
                );
                let theme = resolve_ui_theme(&*opts, "dark", caps, term_colors.as_ref())
                    .unwrap_or_else(|_| Theme::plain());
                (theme, "dark".to_string())
            }
        };
        let view = view_settings(&*opts, &caps);
        let editor_opts = editor_opts_from(&opts.settings);
        let mut editor = Editor::new(editor_opts.clone());
        editor.set_keymap(opts.keymap.clone());
        // The editor has the focus: its cursor is the hardware cursor (pi: the TUI positions
        // the terminal's cursor at the focused component's CURSOR_MARKER, for IME).
        Interactive::set_focused(&mut editor, true);
        let fullscreen = opts.fullscreen;
        let (cols, rows) = term.size();
        let viewport = Viewport::new(
            cols.max(1),
            rows as usize,
            ScrollbarMode::parse(&view.scrollbar),
            view.wheel_lines,
        );
        if fullscreen {
            term.set_mode(TermMode::AltScreen, true);
            term.set_mode(TermMode::Mouse, true);
        }
        term.set_mode(TermMode::FocusReporting, true);
        let scoped = opts.settings.list("enabledModels");
        let wheel = wheel_accel(&opts.settings, &opts.env);
        let clear_on_shrink = match opts
            .settings
            .get("terminal.clearOnShrink")
            .and_then(Value::as_bool)
        {
            Some(on) => on,
            // The shared true words (`1`, `true`, `yes`, `on`), under either name.
            None => opts
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == "OXA_CLEAR_ON_SHRINK" || k == "PI_CLEAR_ON_SHRINK")
                .is_some_and(|(_, v)| oxa_config::EnvFlag::parse(Some(v)) == oxa_config::EnvFlag::On),
        };
        let header_pending = !view.quiet_startup;
        let open_picker = opts.open_picker;
        let mut lp = Loop {
            opts,
            handle,
            worker: crate::app::WorkerStop::new(),
            cmds,
            caps,
            vm: initial,
            theme,
            theme_name,
            theme_epoch: 0,
            term_colors,
            view,
            editor,
            editor_opts,
            dialogs: DialogHost::new(),
            asks: Vec::new(),
            overlay: None,
            pending_open: None,
            tree: None,
            picker_all: false,
            picker_pending: false,
            tree_back: None,
            last_escape_ms: 0,
            resources: Vec::new(),
            scoped,
            scope_from_settings: true,
            model_pending: None,
            model_answered: false,
            session: SessionInfo::default(),
            inbox: None,
            files: None,
            pending_parts: Vec::new(),
            printer: RegularPrinter::new(),
            renderer: Renderer::new(if fullscreen {
                ScreenMode::Alt
            } else {
                ScreenMode::Main
            }),
            fullscreen,
            viewport,
            cache: BlockCache::default(),
            mouse_sel: crate::Selector::new(),
            wheel,
            clear_on_shrink,
            bar_until: 0,
            search: TranscriptSearch::new(),
            search_open: false,
            last_frame: Vec::new(),
            act_kind: None,
            act_since: 0,
            act_attempt: 0,
            palette_checked_ms: now_ms(),
            theme_label: String::new(),
            palette: None,
            anim_step: 0,
            arcade: ArcadeState::default(),
            stage: Director::new("", built_in().scenes),
            popup_overlay: None,
            pixel: crate::skin::PixelStage::new(caps.cell_px),
            gfx_answered,
            gfx_placeholders,
            gfx_out: String::new(),
            gfx_reset: false,
            cool_theme: Theme::plain(),
            yolo_shown: None,
            yolo_prev: Autonomy::Auto,
            yolo_noticed: false,
            yolo_banner_since: 0,
            yolo_banner_until: 0,
            sent_title: None,
            focused: true,
            run_finished: false,
            abort_requested: false,
            dirty: true,
            armed: false,
            armed_at: None,
            reset_screen: false,
            header_pending,
            quit: None,
            draft: Default::default(),
        };
        // `-r`: the picker opens once the worker has announced its session (the picker marks
        // and filters by it).
        lp.picker_pending = open_picker;
        lp.cool_theme = lp.theme.clone();
        lp.theme_label = lp.label_of_theme();
        // `--yolo`: the session starts hot, with the banner.
        lp.sync_yolo();
        lp
    }

    /// O-184: animation is wanted and possible: not reduced (`ui.reducedMotion`,
    /// `OXA_REDUCED_MOTION`), a theme with colour (not `NO_COLOR` / `TERM=dumb`) and the
    /// Unicode glyph set.
    fn motion(&self) -> bool {
        self.view.skin == crate::Skin::Oxa && !self.theme.is_plain() && !self.reduced_motion()
    }

    /// `OXA_REDUCED_MOTION` overrides `ui.reducedMotion` for one run when it names one of the
    /// eight words (`0`/`false`/`no`/`off` turn motion back on).
    fn reduced_motion(&self) -> bool {
        reduced_motion(self.opts.settings.bool("ui.reducedMotion", false), &self.opts.env)
    }

    /// The `ui.animation` setting (`arcade`, `minimal`, `off`); `Off` whenever motion is.
    /// Like [`Self::motion`], but the ASCII glyph set animates too (in ASCII).
    fn animation(&self) -> Animation {
        if self.view.skin.is_oxa() && !self.theme.is_plain() && !self.reduced_motion() {
            Animation::from_setting(self.opts.settings.string("ui.animation").as_deref())
        } else {
            Animation::Off
        }
    }

    /// The arcade plays as pixel art (the graphics tier): the terminal can, and the settings
    /// do not say otherwise.
    fn pixels(&self) -> bool {
        self.animation() == Animation::Arcade
            && self.view.skin == crate::Skin::Oxa
            && crate::skin::graphics_wanted(
                self.opts.settings.string("ui.animationScene").as_deref(),
                self.opts.settings.string("ui.animationGraphics").as_deref(),
                self.gfx_answered,
                self.gfx_placeholders,
            )
    }

    /// C4: the activity row's scene comes from the director (`skin/stage`) when the arcade plays,
    /// no pixel scene is drawn and `ui.animationScene` is unset (an explicit scene keeps its own).
    fn stage_on(&self) -> bool {
        self.animation() == Animation::Arcade
            && self.view.skin == crate::Skin::Oxa
            && self.opts.settings.string("ui.animationScene").is_none()
            && !self.pixels()
    }

    /// Delete every image of the graphics tier from the terminal, now (exit, suspend).
    fn drop_images(&mut self, term: &mut dyn Terminal) {
        self.gfx_out.clear();
        let mut out = String::new();
        self.pixel.clear(&mut out);
        if !out.is_empty() {
            term.write(out.as_bytes());
            term.flush();
        }
    }

    /// How long one animation step of OXA's look lasts.
    fn anim_step_ms(&self) -> u64 {
        if self.animation() != Animation::Off {
            SPRITE_FRAME_MS
        } else {
            1000
        }
    }

    /// Whether the session effectively has full access, not just the process's switch.
    fn yolo_on(&self) -> bool {
        self.permission_mode() == PermissionMode::Yolo
    }

    /// #202: the permission mode now: the session mode with the process's autonomy level.
    fn permission_mode(&self) -> PermissionMode {
        let level = self.opts.yolo.as_ref().map_or(Autonomy::Standard, |y| y.level());
        PermissionMode::of(self.vm.status.mode.unwrap_or(Mode::Workspace), level)
    }

    /// Install a freshly loaded theme: it is the cool one; the shown one is hot under YOLO.
    fn install_theme(&mut self, theme: Theme) {
        self.cool_theme = theme;
        self.theme = if self.yolo_on() && self.view.skin.is_oxa() {
            hot_theme(&self.cool_theme)
        } else {
            self.cool_theme.clone()
        };
        self.theme_label = self.label_of_theme();
        self.theme_epoch += 1;
        self.dirty = true;
    }

    fn refresh_view_settings(&mut self) {
        let tools_expanded = self.view.tools_expanded;
        self.view = view_settings(&*self.opts, &self.caps);
        self.view.tools_expanded = tools_expanded;
        self.viewport.scrollbar = ScrollbarMode::parse(&self.view.scrollbar);
        self.viewport.wheel_lines = self.view.wheel_lines;
        self.wheel = wheel_accel(&self.opts.settings, &self.opts.env);
    }

    fn apply_live_settings(&mut self, settings: oxa_config::Settings) {
        let old_theme = theme_name(&*self.opts);
        let old_skin = self.view.skin;
        let old_hidden = self.opts.settings.get("hideThinkingBlock");
        let keep_hidden = old_hidden == settings.get("hideThinkingBlock");
        let hidden = self.view.hide_thinking;
        self.opts.settings = settings;
        self.refresh_view_settings();
        if keep_hidden {
            self.view.hide_thinking = hidden;
        }
        self.editor_opts = editor_opts_from(&self.opts.settings);
        let new_theme = theme_name(&*self.opts);
        if new_theme != old_theme || self.view.skin != old_skin {
            match resolve_ui_theme(&*self.opts, &new_theme, self.caps, self.term_colors.as_ref()) {
                Ok(theme) => {
                    self.theme_name = new_theme;
                    self.palette = None;
                    self.install_theme(theme);
                }
                Err(error) => self.push_notice(Level::Warning, error.to_string()),
            }
        }
    }

    /// The theme line of the splash: the theme's name, with the desktop theme it follows.
    fn label_of_theme(&self) -> String {
        if self.cool_theme.is_plain() {
            String::new()
        } else if self.cool_theme.name == "oxa-omarchy" {
            match omarchy_theme_name(&*self.opts) {
                Some(desktop) => format!("oxa-omarchy ({desktop})"),
                None => "oxa-omarchy".to_string(),
            }
        } else {
            self.cool_theme.name.clone()
        }
    }

    /// O-182, #202: bring the screen in line with the autonomy switch: the status the HUD
    /// reads, the hot or cool theme, and (the first time yolo is switched on in the session)
    /// the banner and a notice that stays in the transcript.
    fn sync_yolo(&mut self) {
        // #1512 S3: the run's sandbox is fixed at start; the guard label follows it.
        if self.vm.status.os_sandbox != self.opts.os_sandbox {
            self.vm.status.os_sandbox = self.opts.os_sandbox;
            self.dirty = true;
        }
        let Some(switch) = self.opts.yolo.clone() else {
            self.sync_guard();
            return;
        };
        let level = switch.level();
        let approved = switch.approved();
        if self.vm.status.yolo_approved != approved {
            self.vm.status.yolo_approved = approved;
            self.dirty = true;
        }
        if self.vm.status.autonomy != level {
            self.vm.status.autonomy = level;
            self.dirty = true;
        }
        self.sync_guard();
        let mode = self.permission_mode();
        if self.yolo_shown == Some(mode) {
            return;
        }
        let was_full = self.yolo_shown == Some(PermissionMode::Yolo);
        self.yolo_shown = Some(mode);
        let full = mode == PermissionMode::Yolo;
        self.dirty = true;
        if full == was_full && self.vm.status.yolo == full {
            return;
        }
        self.vm.status.yolo = full;
        let cool = self.cool_theme.clone();
        self.install_theme(cool);
        if full {
            self.yolo_banner_since = now_ms();
            self.yolo_banner_until = now_ms() + 2500;
            if !self.yolo_noticed {
                self.yolo_noticed = true;
                self.push_notice(
                    Level::Warning,
                    "YOLO full access: no prompts, bash runs without the sandbox, the file tools \
                     ignore the deny-list. /mode auto or Ctrl+Alt+O turns it off."
                        .to_string(),
                );
            }
        } else {
            self.yolo_banner_until = 0;
        }
        // pi's look has no HUD: the state is an extension-style status entry in its footer.
        if !self.view.skin.is_oxa() {
            if full {
                self.vm
                    .status
                    .ext_status
                    .insert("yolo".to_string(), "YOLO full access".to_string());
            } else {
                self.vm.status.ext_status.remove("yolo");
            }
        }
    }

    /// #1512 S3: the pi skin shows the guard as an extension status entry, like the yolo one.
    /// The oxa skin draws it in the HUD instead.
    fn sync_guard(&mut self) {
        if self.view.skin.is_oxa() {
            return;
        }
        let label = self.vm.status.guard_label().map(str::to_string);
        let current = self.vm.status.ext_status.get("sandbox").cloned();
        if label == current {
            return;
        }
        match label {
            Some(text) => {
                self.vm.status.ext_status.insert("sandbox".to_string(), text);
            }
            None => {
                self.vm.status.ext_status.remove("sandbox");
            }
        }
        self.dirty = true;
    }

    /// O-182: `/yolo`, the `app.yolo.toggle` key. Off returns to the level before yolo.
    fn set_yolo(&mut self, want: Option<bool>) {
        let on = self.opts.yolo.as_ref().is_some_and(|y| y.full());
        let target = if want.unwrap_or(!on) {
            PermissionMode::Yolo
        } else if on {
            PermissionMode::of(Mode::Workspace, self.yolo_prev)
        } else {
            return self.push_notice(Level::Info, format!("Mode: {}", self.permission_mode().as_str()));
        };
        self.set_permission_mode(target, false);
    }

    /// #202: switch the permission mode (`/mode`, `/auto`, `/yolo`, the mode key, the picker);
    /// `save` also writes it as `permissions.defaultMode`.
    fn set_permission_mode(&mut self, mode: PermissionMode, save: bool) {
        let Some(switch) = self.opts.yolo.clone() else {
            return self.push_notice(
                Level::Warning,
                "The permission mode cannot be switched in this session.".to_string(),
            );
        };
        let current = self.vm.status.mode.unwrap_or(Mode::Workspace);
        if mode == PermissionMode::Plan {
            if current != Mode::Plan {
                self.send(SessionCmd::SetMode(Mode::Plan));
            }
        } else {
            let before = switch.level();
            if before != Autonomy::Yolo {
                self.yolo_prev = before;
            }
            switch.set_level(mode.autonomy());
            if current != Mode::Workspace {
                self.send(SessionCmd::SetMode(Mode::Workspace));
            }
        }
        // The status shows the new mode at once; the session confirms it with its next state.
        self.vm.status.mode = Some(if mode == PermissionMode::Plan {
            Mode::Plan
        } else {
            Mode::Workspace
        });
        self.sync_yolo();
        if !(mode == PermissionMode::Yolo && self.yolo_noticed && self.yolo_banner_until > now_ms()) {
            self.push_notice(
                Level::Info,
                format!("Mode: {} ({})", mode.as_str(), mode.describe()),
            );
        }
        if save {
            let cwd = self.cwd();
            let value = Value::String(mode.as_str().to_string());
            match self
                .opts
                .settings
                .set(&self.opts.dirs, &cwd, "permissions.defaultMode", value, false)
            {
                Ok(()) => self.push_notice(Level::Info, format!("Default mode saved: {}", mode.as_str())),
                Err(e) => self.push_notice(Level::Error, e.to_string()),
            }
        }
    }

    /// Foreground activity, including the interval before its start event reaches the UI.
    /// Background prefix/draft claims are excluded. The shared foreground signal ends when
    /// a failed, aborted or cut reply is persisted, so settlement's remaining busy claim does
    /// not turn later input into steering for a finished turn.
    fn running(&self) -> bool {
        foreground_running(
            &self.vm,
            self.handle.is_running(),
            self.worker.is_preloading(),
            self.worker.is_drafting(),
        )
    }

    /// Whether Enter / Alt+Enter queue the text (steering / follow-up) rather than start a run:
    /// a run is active, the user has not aborted it, and it still takes a tool batch. A run
    /// that is winding down after an abort (#906) takes no further batch, so a queued message
    /// would never be delivered; input typed in that window starts a new run instead. The
    /// abort the user asked for is the same window seen before the worker reports it
    /// ([`Self::abort_requested`]), so both are checked. Only what Enter does changes: Ctrl+C
    /// still aborts, and the footer hint still reads "steer" for that moment.
    fn accepts_steering(&self) -> bool {
        self.running() && !self.abort_requested && !self.handle.is_aborting()
    }

    pub(crate) fn animating(&self) -> bool {
        // Active work always refreshes its elapsed time. With the director, a pending scene
        // deadline can keep the activity row animating as well.
        activity_refreshing(
            self.running(),
            self.stage_on(),
            self.stage.next_deadline_ms(now_ms()).is_some(),
        ) || self.dialogs.is_open()
            || self.mouse_sel.dragging
            || now_ms() < self.yolo_banner_until
            || self.arcade.finishing(now_ms())
    }

    fn send(&mut self, cmd: SessionCmd) {
        // O-152: user input cancels the boot preload, so the command starts at once.
        // V020-OPTIMIZE #13: and a draft prefill (the command also makes it stale). Only the
        // background request is cancelled, and before the command is queued: the command
        // itself is never cancelled.
        let drafting = self.worker.draft_changed();
        if self.worker.is_preloading() || drafting {
            self.handle.cancel_background();
        }
        if self.cmds.send(cmd).is_err() {
            self.quit = Some(1);
        }
        // A background claim may have reset its tokens between the first cancellation
        // and publication. Once the command is visible, cancel any still-armed background
        // work again; this never cancels the foreground command itself.
        if self.worker.is_preloading() || self.worker.is_drafting() {
            self.handle.cancel_background();
        }
    }

    fn push_notice(&mut self, level: Level, text: String) {
        push_notice(&mut self.vm, level, text);
        self.dirty = true;
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.vm.status.cwd)
    }

    /// #1512 S2: answer a pending "always allow": its keys are saved to the user settings first.
    /// A failed write shows an error and answers `Session`, so the grant still holds this run.
    fn finish_danger_always(&mut self) {
        let Some((keys, reply)) = self.dialogs.take_danger_always() else {
            return;
        };
        let cwd = self.cwd();
        let answer = match append_always(&mut self.opts.settings, &self.opts.dirs, &cwd, &keys) {
            Ok(()) => DangerAnswer::Always,
            Err(e) => {
                self.push_notice(Level::Error, e.to_string());
                DangerAnswer::Session
            }
        };
        let _ = reply.send(answer);
    }

    // -----------------------------------------------------------------------------------------
    // Messages from the worker and from `Ui` calls
    // -----------------------------------------------------------------------------------------

    pub(crate) fn worker_msg(&mut self, msg: WorkerMsg) {
        if let Some(settings) = self.handle.take_live_settings() {
            self.apply_live_settings(settings);
        }
        self.dirty = true;
        match msg {
            // SET-010: cache-miss notices only with `showCacheMissNotices` (pi's default is
            // off); an unknown miss is still counted for the footer.
            WorkerMsg::Event(AgentEvent::CacheMiss { cause, .. })
                if !self.opts.settings.bool("showCacheMissNotices", false) =>
            {
                if cause == oxa_proto::miss_cause::UNKNOWN {
                    self.vm.status.unknown_misses += 1;
                }
            }
            WorkerMsg::Event(e) => {
                // A turn started: the run goes on, so input is steering again.
                if matches!(e, AgentEvent::AgentStart) {
                    self.abort_requested = false;
                }
                // AgentEnd can occur between tool/follow-up turns; only settled ends a run.
                if matches!(e, AgentEvent::AgentSettled { .. }) {
                    if let Some(inbox) = &self.inbox {
                        inbox.retry();
                    }
                    self.run_finished = true;
                    self.abort_requested = false;
                }
                apply_event(&mut self.vm, &e);
            }
            WorkerMsg::UnsentPrompt { text, mut parts } => {
                self.editor.prepend_text(&text);
                parts.append(&mut self.pending_parts);
                self.pending_parts = parts;
                if !self.pending_parts.is_empty() {
                    self.vm.widgets_above.insert(
                        "oxa.attachments".to_string(),
                        vec![format!(
                            "{} image(s) attached to the next prompt",
                            self.pending_parts.len()
                        )],
                    );
                }
                self.push_notice(
                    Level::Warning,
                    "Recovery prompt was not sent after the session changed; it is back in the editor."
                        .to_string(),
                );
            }
            WorkerMsg::LimitExit(code) => {
                self.push_notice(Level::Info, "Session limit or stop signal reached.".to_string());
                self.quit = Some(code);
            }
            WorkerMsg::Done(result) => {
                // The wire may have changed: the same draft is a new prefill.
                self.draft.wire_changed();
                // The worker answers every command with Done, then its fresh Status, and it runs
                // nothing else until the next command. A command that ended outside a run (a
                // manual `/compact`: its idle prefill reports progress after `CompactionEnd` and
                // the core sends no `AgentSettled` for it) leaves the activity at its last
                // progress event; close it here so the session is idle again.
                settle_worker_done(&mut self.vm, result.is_err());
                self.model_answered = self.model_pending.is_some();
                if let Err(e) = result {
                    self.push_notice(Level::Error, e.to_string());
                }
            }
            WorkerMsg::Reopened { entries, status } => {
                self.vm.blocks = blocks_from_entries(&entries);
                refresh_status(&mut self.vm.status, status);
                self.vm.status.degraded_context = false;
                self.vm.status.activity = Activity::Idle;
                self.vm.status.activity_ms = 0;
                self.vm.status.steering.clear();
                self.vm.status.follow_up.clear();
                self.printer.reset();
                self.reset_screen = true;
                // The screen is cleared for the new transcript: the header is printed again.
                self.header_pending = !self.view.quiet_startup;
                self.mouse_sel.clear();
                self.search.clear();
                self.search_open = false;
                self.viewport.scroll_to_bottom();
            }
            WorkerMsg::Status(s) => {
                refresh_status(&mut self.vm.status, s);
                if std::mem::take(&mut self.model_answered)
                    || self.model_pending.as_ref() == Some(&self.vm.status.model)
                {
                    self.model_pending = None;
                }
            }
            WorkerMsg::Session(info) => self.set_session(info),
            WorkerMsg::Tree(tree) => self.tree_arrived(tree),
            WorkerMsg::Rebind(handle) => self.rebind(handle),
            WorkerMsg::Open(kind) => self.open_selector(kind),
        }
    }

    /// The worker switched to another session object. Its queued editor steering and
    /// follow-up messages come back into the editor (with a notice; they were never sent),
    /// while inbox messages retain their destinations. Questions about the old session are dropped,
    /// and its tree, selectors and attachments are closed. Abort, steer and follow-up target
    /// the new session from here on.
    fn rebind(&mut self, handle: Handle) {
        self.abort_requested = false;
        // A typed publication `yes` stays bound to the request it answered: `rebind` does not
        // withdraw it, and only that exact request id can consume it (never another request).
        // Join the old callback before draining its queue back into the editor.
        self.inbox = None;
        let old = std::mem::replace(&mut self.handle, handle);
        old.abort_retry();
        // Unlike editor input, admitted inbox text must keep its destination across a
        // fresh Session/Shared. This includes messages admitted by a stale clone/fork watcher.
        old.transfer_inbox_to(&self.handle);
        let queued = old.dequeue();
        if !queued.is_empty() {
            let n = queued.len();
            self.editor.prepend_text(&queued.join("\n\n"));
            self.push_notice(
                Level::Warning,
                format!("{n} queued message(s) were not sent to the previous session; they are back in the editor."),
            );
        }
        if !self.asks.is_empty() {
            self.asks.clear();
            self.dialogs.close_all();
        }
        if matches!(self.overlay, Some(Overlay::Tree(_)) | Some(Overlay::Fork(_))) {
            self.overlay = None;
        }
        self.pending_open = None;
        self.tree = None;
    }

    fn set_session(&mut self, info: SessionInfo) {
        if self.session.id != info.id || self.inbox.is_none() {
            // A Reopen may have recovered an already-admitted message without a file
            // left to trigger the new watcher. Wake only the recovered destination.
            if self.handle.has_external_inbox_input() && self.handle.request_inbox_wake() {
                let _ = self.cmds.send(SessionCmd::Inbox {
                    session_id: info.id.clone(),
                });
            }
            // A completion retained behind a stop boundary (a failed or interrupted turn) is not
            // delivered on a reopen: the replacement session starts with no outcome, so waking
            // it would start a model turn nobody asked for. It stays queued for the next prompt.
            // Stop the old callback before installing one bound to the current handle.
            self.inbox = None;
            // W4: a finished background job of this session wakes it the way an inbox message does.
            let job_cmds = self.cmds.clone();
            self.handle
                .set_wake_notifier(std::sync::Arc::new(move |session_id: &str| {
                    let _ = job_cmds.send(SessionCmd::JobWake {
                        session_id: session_id.to_string(),
                    });
                }));
            let handle = self.handle.clone();
            let cmds = self.cmds.clone();
            let session_id = info.id.clone();
            let watch =
                oxa_session::Inbox::watch_messages(&self.opts.dirs.agent_dir, &info.id, move |item| {
                    // A clone/fork can change the worker's identity before the UI receives
                    // its Session notification. Keep even late admissions at their destination.
                    // #2183: a steer is queued for the next tool boundary, like typed steering.
                    let admitted = match item {
                        oxa_session::InboxItem::Steer(text) => handle.steer_for(&session_id, text),
                        oxa_session::InboxItem::FollowUp(text) => handle.follow_up_for(&session_id, text),
                        oxa_session::InboxItem::Control(_) => false,
                    };
                    if !admitted {
                        return false;
                    }
                    if !handle.request_inbox_wake() {
                        return true;
                    }
                    cmds.send(SessionCmd::Inbox {
                        session_id: session_id.clone(),
                    })
                    .is_ok()
                });
            match watch {
                Ok(inbox) => self.inbox = Some(inbox),
                Err(e) => self.push_notice(Level::Warning, format!("Session inbox unavailable: {e}")),
            }
        }
        // pi interactive-mode.ts: the autocomplete offers `/skill:name` only with
        // `enableSkillCommands` (a typed one still expands either way).
        let skill_commands = self.opts.settings.bool("enableSkillCommands", true);
        let commands: Vec<SlashCommand> = info
            .commands
            .iter()
            .filter(|c| skill_commands || !c.name.starts_with("skill:"))
            .map(|c| SlashCommand {
                name: c.name.clone(),
                description: Some(c.description.clone()).filter(|d| !d.is_empty()),
                argument_hint: c.argument_hint.clone(),
                complete_args: None,
            })
            .collect();
        // The front-end built-ins the core does not list (`/theme`, `/debug`, `/exit`) complete
        // too; `dispatch_slash` handles them before any text reaches the session.
        let mut commands = commands;
        for (name, description) in [
            ("theme", "Select the colour theme"),
            (
                "yolo",
                "Toggle yolo: full access, no prompts, bash without the sandbox",
            ),
            ("auto", "Auto mode: no prompts; dangerous commands still ask"),
            ("debug", "Write a debug dump"),
            ("exit", "Quit oxa"),
        ] {
            if !commands.iter().any(|c| c.name == name) {
                commands.push(SlashCommand {
                    name: name.to_string(),
                    description: Some(description.to_string()),
                    argument_hint: None,
                    complete_args: None,
                });
            }
        }
        self.session = info;
        if self.scope_from_settings {
            self.scoped = self.resolved_scope();
        }
        if matches!(self.overlay, Some(Overlay::Model(_) | Overlay::Scoped(_))) {
            let models = self.model_choices();
            match self.overlay.as_mut() {
                Some(Overlay::Model(selector)) => {
                    selector.set_models(models, &self.vm.status.model, &self.scoped);
                }
                Some(Overlay::Scoped(selector)) => selector.set_models(models),
                _ => {}
            }
        }
        if std::mem::take(&mut self.picker_pending) {
            self.open_selector(SelectorKind::Sessions);
        }
        let cwd = self.cwd();
        if cwd.as_os_str().is_empty() {
            return;
        }
        let files = self
            .files
            .get_or_insert_with(|| FileIndex::build(&cwd, FILE_INDEX_MAX))
            .clone();
        let mut provider = CombinedProvider::new(commands, files, &cwd, &self.opts.dirs.home);
        if !self.opts.hooks.is_empty() {
            let hooks = self.opts.hooks.clone();
            provider.set_extra(Box::new(move |prefix: &str| {
                hooks.iter().flat_map(|h| h.complete(prefix)).collect()
            }));
        }
        self.editor.set_autocomplete(Some(Box::new(provider)));
    }

    fn tree_arrived(&mut self, tree: Tree) {
        let rows = self.selector_rows();
        match self.pending_open.take() {
            Some(SelectorKind::Tree) => {
                let filter = match self.opts.settings.string("treeFilterMode").as_deref() {
                    Some("no-tools") => TreeFilter::NoTools,
                    Some("user-only") => TreeFilter::UserOnly,
                    Some("labeled-only") => TreeFilter::LabeledOnly,
                    Some("all") => TreeFilter::All,
                    _ => TreeFilter::Default,
                };
                self.overlay = Some(Overlay::Tree(TreeSelector::new(&tree, filter, rows, now_ms())));
            }
            Some(SelectorKind::ForkPoint) => {
                self.overlay = Some(Overlay::Fork(ForkPointSelector::new(&tree, rows)));
            }
            _ => {
                // A label was set while the navigator is open: it gets the new tree.
                if let Some(Overlay::Tree(sel)) = self.overlay.as_mut() {
                    sel.set_tree(&tree);
                }
            }
        }
        self.tree = Some(tree);
    }

    pub(crate) fn ui_req(&mut self, req: UiRequest) {
        self.dirty = true;
        match req {
            UiRequest::SetEditorText(t) => self.editor.set_text(&t),
            UiRequest::EditorText { reply } => {
                let _ = reply.send(self.editor.text());
            }
            other => {
                if let Some(back) = self.dialogs.handle(other, &mut self.vm, now_ms()) {
                    match back {
                        UiRequest::SetEditorText(t) => self.editor.set_text(&t),
                        UiRequest::EditorText { reply } => {
                            let _ = reply.send(self.editor.text());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Carry out the answers to the app's own questions.
    pub(crate) fn poll_asks(&mut self) {
        if self.asks.is_empty() {
            return;
        }
        let mut open = Vec::new();
        let before = self.asks.len();
        for ask in std::mem::take(&mut self.asks) {
            match ask {
                Ask::Trust { rx, choices } => match rx.try_recv() {
                    Ok(answer) => {
                        if let Some(choice) = answer.and_then(|i| choices.get(i).copied()) {
                            self.save_trust(choice);
                        }
                    }
                    Err(TryRecvError::Empty) => open.push(Ask::Trust { rx, choices }),
                    Err(TryRecvError::Disconnected) => {}
                },
                Ask::Summary { rx, target, refill } => match rx.try_recv() {
                    Ok(Some(0)) => self.navigate(target, false, None, refill),
                    Ok(Some(1)) => self.navigate(target, true, None, refill),
                    Ok(Some(_)) => {
                        let (tx, rx) = mpsc::channel();
                        let req = UiRequest::Input {
                            title: "Custom summarization instructions".to_string(),
                            placeholder: "What should the summary focus on?".to_string(),
                            reply: tx,
                        };
                        self.dialogs.handle(req, &mut self.vm, now_ms());
                        open.push(Ask::SummaryFocus { rx, target, refill });
                    }
                    // Dismissed: nothing moved, the navigator comes back with the same
                    // selection (pi).
                    Ok(None) => {
                        if self.overlay.is_none() {
                            self.overlay = self.tree_back.take();
                        }
                    }
                    Err(TryRecvError::Disconnected) => {}
                    Err(TryRecvError::Empty) => open.push(Ask::Summary { rx, target, refill }),
                },
                Ask::SummaryFocus { rx, target, refill } => match rx.try_recv() {
                    Ok(Some(text)) => {
                        let focus = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                        self.navigate(target, true, focus, refill);
                    }
                    // Cancelled: back to the summary question (pi loops).
                    Ok(None) => {
                        open.push(self.ask_summary(target, refill));
                        self.dirty = true;
                    }
                    Err(TryRecvError::Disconnected) => {}
                    Err(TryRecvError::Empty) => open.push(Ask::SummaryFocus { rx, target, refill }),
                },
            }
        }
        // An answered ask changes the screen (navigation, a re-opened navigator, a new dialog).
        if open.len() != before || !self.asks.is_empty() {
            self.dirty = true;
        }
        open.append(&mut self.asks);
        self.asks = open;
    }

    pub(crate) fn tick(&mut self, animating: bool) {
        if self
            .armed_at
            .is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(2))
        {
            self.armed = false;
            self.armed_at = None;
            self.dirty = true;
        }
        // pi's look redraws on every turn of an animating loop. OXA's look redraws when its
        // animation steps: 8 times a second with the sprite, once a second (the elapsed time)
        // without motion. Everything else that changes the screen marks it dirty itself.
        let animating = if self.view.skin.is_oxa() {
            let step = now_ms() / self.anim_step_ms();
            let stepped = animating && step != self.anim_step;
            if stepped {
                self.anim_step = step;
            }
            stepped
        } else {
            animating
        };
        if self.dialogs.tick(now_ms()) || animating || self.arcade.take_expired(now_ms()) {
            self.dirty = true;
        }
        // C4: the director's row changed (a new frame, a new activity or the scene taken away).
        if self.stage_on() && self.stage.redraw_due(now_ms()) {
            self.dirty = true;
        }
        self.draft_tick();
        // The transient scrollbar hides once its time is up.
        if self.viewport.auto_bar_visible && now_ms() >= self.bar_until {
            self.dirty = true;
        }
        self.follow_desktop_theme();
        // O-182: the count of auto-approved questions moves while a run is active.
        self.sync_yolo();
    }

    /// O-185: `oxa-omarchy` follows the desktop live. While it is the chosen theme, the
    /// palette files are looked at every two seconds (a few `stat` calls; faster while a
    /// change settles); a settled change that parses rebuilds the theme and repaints. A
    /// missing or half-written file keeps the current colours. No thread: nothing to stop.
    fn follow_desktop_theme(&mut self) {
        if self.theme_name != "oxa-omarchy" || crate::skin::env_no_color(&self.opts.env) {
            self.palette = None;
            return;
        }
        if self.palette.is_none() {
            self.palette = Some(palette_watch(&*self.opts));
            self.palette_checked_ms = now_ms();
        }
        let Some(watch) = self.palette.as_mut() else {
            return;
        };
        let now = now_ms();
        if now.saturating_sub(self.palette_checked_ms) < watch.interval_ms() {
            return;
        }
        self.palette_checked_ms = now;
        if watch.poll().is_none() {
            return;
        }
        if let Ok(theme) = resolve_ui_theme(&*self.opts, "oxa-omarchy", self.caps, self.term_colors.as_ref())
        {
            self.install_theme(theme);
        }
    }

    /// The shown status with the time spent in the current activity (OXA's look: the elapsed
    /// time and the spinner frame).
    fn timed_status(&mut self, mut status: StatusModel) -> StatusModel {
        let kind = std::mem::discriminant(&status.activity);
        let attempt = match status.activity {
            Activity::Retrying { attempt, .. } => attempt,
            _ => 0,
        };
        if self.act_kind != Some(kind) || self.act_attempt != attempt {
            self.act_kind = Some(kind);
            self.act_attempt = attempt;
            self.act_since = now_ms();
        }
        status.activity_ms = now_ms().saturating_sub(self.act_since);
        status
    }

    /// What the hint bar teaches right now.
    fn hint_context(&self) -> HintContext {
        if self.dialogs.is_open() {
            HintContext::Dialog
        } else if self.overlay.is_some() {
            HintContext::Overlay
        } else if self.search_open {
            HintContext::Search
        } else if self.running() {
            HintContext::Running
        } else {
            let text = self.editor.text();
            HintContext::Idle {
                has_text: !text.trim().is_empty(),
                slash: text.starts_with('/'),
                armed: self.armed,
            }
        }
    }

    /// V020-OPTIMIZE #13 (`oxa.prefill.paste`): once the editor's draft has been unchanged for
    /// `Session::DRAFT_PREFILL_QUIET` while the session is idle, ask the worker to prefill it
    /// (`SessionCmd::PrefillDraft`), once per draft and wire; the core decides the rest (size,
    /// hooks, engine). The text is what Enter would send ([`submitted_text`]); slash and `!`
    /// lines are not prefilled.
    fn draft_tick(&mut self) {
        let settings = &self.opts.settings;
        if !settings.prefill_paste() {
            return;
        }
        let blocked = self.running()
            || self.worker.is_preloading()
            || self.worker.is_drafting()
            || self.dialogs.is_open()
            || self.overlay.is_some();
        let text = (!blocked).then(|| submitted_text(&self.editor.expanded_text()));
        let key = text
            .as_deref()
            .and_then(|t| crate::draft_trigger::draft_key(t, &self.pending_parts));
        let gen = self.worker.draft_gen();
        if self.draft.tick(now_ms(), key, gen, self.worker.draft_done()) {
            let cmd = SessionCmd::PrefillDraft {
                text: text.unwrap_or_default(),
                parts: self.pending_parts.clone(),
                gen,
            };
            if self.cmds.send(cmd).is_err() {
                self.quit = Some(1);
            }
        }
    }

    /// Scrolling shows pi's transient `auto` scrollbar for a second (scroll-view.ts
    /// `scrollbarHideDelayMs` 1000).
    fn scrolled(&mut self) {
        self.bar_until = now_ms() + 1000;
    }

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    pub(crate) fn input(&mut self, term: &mut dyn Terminal, ev: InputEvent) {
        // V020-OPTIMIZE #13: any key or paste makes a draft prefill stale and cancels it (the
        // background request only; the same draft is sent again if it did not change).
        if matches!(ev, InputEvent::Key(_) | InputEvent::Paste(_)) && self.worker.draft_changed() {
            self.handle.cancel_background();
        }
        match ev {
            InputEvent::TerminalClosed => self.quit = Some(129),
            InputEvent::Key(key) => self.key(term, key),
            InputEvent::Paste(text) => self.paste(&text),
            InputEvent::Mouse(m) => self.mouse(term, m),
            InputEvent::FocusGained => self.focused = true,
            InputEvent::FocusLost => self.focused = false,
            InputEvent::Resize { .. } => {
                self.dirty = true;
                self.gfx_reset = true;
            }
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        self.dirty = true;
        if self.dialogs.is_open() {
            self.dialogs.paste(text);
            return;
        }
        if let Some(ov) = self.overlay.as_mut() {
            match ov {
                Overlay::Model(s) => s.paste(text),
                Overlay::Scoped(s) => s.paste(text),
                Overlay::Settings(s) => s.paste(text),
                Overlay::Sessions(s) => s.paste(text),
                Overlay::Tree(s) => s.paste(text),
                Overlay::Thinking(_)
                | Overlay::Mode(_)
                | Overlay::Theme(_)
                | Overlay::Fork(_)
                | Overlay::Mcp(_) => {
                    self.push_notice(
                        Level::Info,
                        "Paste ignored: this selector has no text field.".to_string(),
                    );
                }
            }
            return;
        }
        if self.search_open {
            let clean: String = text.chars().filter(|c| !c.is_control()).collect();
            let query = format!("{}{clean}", self.search.query);
            self.set_search(&query);
            return;
        }
        // A dropped (or pasted) image file: its path arrives as the paste. Image paths are
        // attached to the next prompt like a pasted image; anything else is text.
        let paths = crate::pasted_paths(text, &|p: &Path| p.is_file());
        if !paths.is_empty() && paths.iter().all(|p| crate::is_image_path(p)) {
            for path in paths {
                self.attach_image_file(&path);
            }
            return;
        }
        self.editor.paste_text(text);
    }

    /// Attach an image file (a dropped path) to the next prompt.
    fn attach_image_file(&mut self, path: &Path) {
        let mime = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "image/jpeg",
        };
        let read =
            std::fs::read(path).map_err(|e| OxaError::io(&format!("cannot read {}", path.display()), &e));
        let part = read.and_then(|data| {
            image_part(
                &crate::ClipImage {
                    data,
                    mime: mime.to_string(),
                },
                &self.opts.dirs.tmp_dir,
            )
        });
        match part {
            Ok(part) => {
                self.pending_parts.push(part);
                let n = self.pending_parts.len();
                let line = self
                    .theme
                    .dim(&format!("{n} image(s) attached to the next prompt"));
                self.vm
                    .widgets_above
                    .insert("oxa.attachments".to_string(), vec![line]);
            }
            Err(e) => self.push_notice(Level::Error, e.to_string()),
        }
    }

    fn key(&mut self, term: &mut dyn Terminal, key: Key) {
        // Kitty sends release events too. They must not consume the two-press exit arm.
        if key.kind == KeyKind::Release {
            return;
        }
        self.dirty = true;
        let was_armed = std::mem::take(&mut self.armed)
            && self
                .armed_at
                .take()
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(2));
        // Ctrl+C during a turn uses the existing cancel path, not idle clear/exit.
        // Do this before dialogs/overlays and selection-copy can consume the key.
        if self.opts.keymap.actions_for(&key.id).contains(&"app.clear")
            && (self.running() || matches!(self.vm.status.activity, Activity::Retrying { .. }))
        {
            self.abort_requested = true;
            self.handle.abort();
            self.handle.abort_retry();
            self.dialogs.key(&key, &self.opts.keymap);
            self.finish_danger_always();
            self.restore_queue();
            return;
        }
        if self.dialogs.is_open() {
            let outcome = self.dialogs.key(&key, &self.opts.keymap);
            self.finish_danger_always();
            match outcome {
                DialogKey::NotOpen => {}
                DialogKey::ExternalEditor => {
                    self.dialog_external_editor(term);
                    return;
                }
                DialogKey::Consumed | DialogKey::Closed => return,
            }
        }
        if self.overlay.is_some() {
            self.overlay_key(term, &key);
            return;
        }
        let acts: Vec<String> = self
            .opts
            .keymap
            .actions_for(&key.id)
            .into_iter()
            .map(String::from)
            .collect();
        let has = |a: &str| acts.iter().any(|x| x == a);
        // An extension shortcut gets a key that no built-in binding has (built-ins win, as in
        // pi, which skips extension shortcuts that clash with its keybindings).
        if acts.is_empty() && self.session.shortcuts.contains(&key.id) {
            self.send(SessionCmd::Shortcut(key.id.clone()));
            return;
        }

        if self.fullscreen {
            if self.search_open && self.search_key(&key, &has) {
                self.scrolled();
                return;
            }
            if has("tui.altScreen.search") {
                self.search_open = true;
                return;
            }
            // Ctrl+C with a selection copies it; Escape drops it (before clear / interrupt).
            if self.mouse_sel.selection.is_some() && (has("app.clear") || has("tui.input.copy")) {
                let text = self.mouse_sel.text(&self.cache.content);
                self.mouse_sel.clear();
                if !text.is_empty() {
                    self.copy(term, &text, true);
                }
                return;
            }
            if self.mouse_sel.selection.is_some() && has("app.interrupt") && !self.running() {
                self.mouse_sel.clear();
                return;
            }
            if !self.editor.autocomplete_open() && self.viewport_key(&has) {
                self.scrolled();
                return;
            }
        }

        if has("app.clear") {
            if !self.editor.is_empty() || !self.pending_parts.is_empty() {
                self.editor.set_text("");
                self.drop_attachments();
            } else if was_armed {
                self.quit = Some(0);
            } else {
                self.armed = true;
                self.armed_at = Some(std::time::Instant::now());
            }
            return;
        }
        if has("app.exit") && self.editor.is_empty() {
            self.quit = Some(0);
            return;
        }
        if has("app.interrupt") && !self.editor.autocomplete_open() {
            let idle = !self.running() && !matches!(self.vm.status.activity, Activity::Retrying { .. });
            let (steering, follow_up) = self.handle.queued();
            let nothing_queued = steering.is_empty() && follow_up.is_empty();
            if idle && self.editor.text().trim().is_empty() && nothing_queued {
                self.double_escape();
                return;
            }
            if self.running() {
                self.abort_requested = true;
                self.handle.abort();
            }
            if matches!(self.vm.status.activity, Activity::Retrying { .. }) {
                self.handle.abort_retry();
            }
            self.restore_queue();
            return;
        }
        if has("app.suspend") {
            self.apply(term, AppAction::Suspend);
            return;
        }
        if has("app.message.followUp") {
            if !self.running() {
                // Idle: Alt+Enter adds a line instead of sending (queueing is for a live run).
                self.editor.insert("\n");
                return;
            }
            let text = self.editor.expanded_text();
            if text.trim().is_empty() {
                return;
            }
            self.editor.set_text("");
            self.editor.add_history(&text);
            if self.accepts_steering() {
                self.handle.follow_up(text);
                self.wake_queued();
                self.sync_queue();
            } else {
                self.send_prompt(text);
            }
            return;
        }
        if has("app.message.dequeue") {
            self.restore_queue();
            return;
        }
        if has("app.message.copy") {
            match self.last_assistant_text() {
                Some(text) => {
                    self.copy(term, &text, false);
                }
                None => self.push_notice(Level::Warning, "No assistant message to copy yet.".to_string()),
            }
            return;
        }
        if has("app.tools.expand") {
            self.apply(term, AppAction::ToggleToolsExpanded);
            return;
        }
        if has("app.thinking.toggle") {
            self.apply(term, AppAction::ToggleThinkingHidden);
            return;
        }
        if has("app.thinking.cycle") && !self.editor.autocomplete_open() {
            let current = self.vm.status.thinking.unwrap_or(ThinkingLevel::Off);
            let next = cycle_thinking(&ThinkingLevel::ALL, current);
            self.send(SessionCmd::SetThinking(next));
            return;
        }
        if has("app.yolo.toggle") {
            self.set_yolo(None);
            return;
        }
        if has("app.mode.cycle") {
            let next = self.permission_mode().next();
            self.set_permission_mode(next, false);
            return;
        }
        if has("app.editor.external") {
            self.apply(term, AppAction::ExternalEditor);
            return;
        }
        if has("app.clipboard.pasteImage") {
            self.paste_image();
            return;
        }
        if has("app.model.select") {
            self.open_selector(SelectorKind::Model);
            return;
        }
        if has("app.model.cycleForward") || has("app.model.cycleBackward") {
            let forward = has("app.model.cycleForward");
            let from = self
                .model_pending
                .clone()
                .unwrap_or_else(|| self.vm.status.model.clone());
            match cycle_model(&self.session.models, &self.scoped, &from, forward) {
                Some(next) => {
                    let action = self.model_switch(next.clone());
                    if matches!(action, AppAction::Session(SessionCmd::SetModel(_))) {
                        self.model_pending = Some(next);
                    }
                    self.apply(term, action);
                }
                None => self.push_notice(Level::Info, "Only one model is available.".to_string()),
            }
            return;
        }
        if has("app.session.new") {
            self.slash(term, "/new");
            return;
        }
        if has("app.session.tree") {
            self.open_selector(SelectorKind::Tree);
            return;
        }
        if has("app.session.fork") {
            self.open_selector(SelectorKind::ForkPoint);
            return;
        }
        if has("app.session.resume") {
            self.open_selector(SelectorKind::Sessions);
            return;
        }
        match self.editor.key(&key, &self.opts.keymap) {
            EditorAction::Submit(text) => self.on_submit(term, text),
            EditorAction::Changed | EditorAction::Ignored => {}
        }
    }

    /// Scroll keys of the fullscreen viewport; `true` = consumed.
    fn viewport_key(&mut self, has: &dyn Fn(&str) -> bool) -> bool {
        let half = (self.viewport.rows / 2).max(1) as isize;
        if has("tui.altScreen.pageUp") {
            self.viewport.page_up();
        } else if has("tui.altScreen.pageDown") {
            self.viewport.page_down();
        } else if has("tui.altScreen.halfPageUp") {
            self.viewport.scroll_by(-half);
        } else if has("tui.altScreen.halfPageDown") {
            self.viewport.scroll_by(half);
        } else if has("tui.altScreen.lineUp") {
            self.viewport.scroll_by(-1);
        } else if has("tui.altScreen.lineDown") {
            self.viewport.scroll_by(1);
        } else if has("tui.altScreen.previousPrompt") {
            self.viewport.jump_prompt(false);
        } else if has("tui.altScreen.nextPrompt") {
            self.viewport.jump_prompt(true);
        } else if has("tui.altScreen.top") {
            self.viewport.scroll_to_top();
        } else if has("tui.altScreen.bottom") {
            self.viewport.scroll_to_bottom();
        } else {
            return false;
        }
        true
    }

    /// Keys while the search bar is open; `true` = consumed.
    fn search_key(&mut self, key: &Key, has: &dyn Fn(&str) -> bool) -> bool {
        if has("tui.altScreen.searchClose") {
            self.search_open = false;
            self.search.clear();
        } else if has("tui.altScreen.searchPrevious") {
            if let Some(line) = self.search.prev_match() {
                self.viewport.reveal(line);
            }
        } else if has("tui.altScreen.searchNext") {
            if let Some(line) = self.search.next_match() {
                self.viewport.reveal(line);
            }
        } else if has("tui.editor.deleteCharBackward") {
            let mut query = self.search.query.clone();
            query.pop();
            self.set_search(&query);
        } else if let Some(text) = key
            .text
            .as_deref()
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
        {
            let query = format!("{}{text}", self.search.query);
            self.set_search(&query);
        } else {
            // Scroll keys still work under the search bar; everything else waits for it to close.
            return !self.is_viewport_key(has);
        }
        true
    }

    fn is_viewport_key(&self, has: &dyn Fn(&str) -> bool) -> bool {
        [
            "pageUp",
            "pageDown",
            "halfPageUp",
            "halfPageDown",
            "lineUp",
            "lineDown",
            "previousPrompt",
            "nextPrompt",
            "top",
            "bottom",
        ]
        .iter()
        .any(|a| has(&format!("tui.altScreen.{a}")))
    }

    fn set_search(&mut self, query: &str) {
        let from = self.viewport.offset.min(self.viewport.total.saturating_sub(1));
        self.search.set_query(
            query,
            &self.cache.content,
            from + self.viewport.rows.saturating_sub(1),
        );
        if let Some(m) = self.search.current.and_then(|i| self.search.matches.get(i)) {
            self.viewport.reveal(m.line);
        }
    }

    fn mouse(&mut self, term: &mut dyn Terminal, m: Mouse) {
        if !self.fullscreen || self.dialogs.is_open() {
            return;
        }
        self.dirty = true;
        let rows = self.viewport.rows;
        if matches!(m.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            // pi: `fullscreenWheelScrollLines` lines per event (`auto` accelerates fast
            // spins); Alt+wheel moves five times as far.
            let dir: i8 = if m.kind == MouseKind::WheelUp { -1 } else { 1 };
            let lines = self.wheel.next(dir, now_ms()) as isize * if m.alt { 5 } else { 1 };
            self.viewport.scroll_by(dir as isize * lines);
            self.scrolled();
            return;
        }
        let inside = (m.row as usize) < rows;
        if !self.mouse_sel.dragging {
            if !inside {
                return;
            }
            if self.viewport.mouse(&m) {
                self.scrolled();
                return; // the scrollbar
            }
        }
        // A drag past the edges scrolls, and the selection follows to the edge row.
        let mut at = m;
        if self.mouse_sel.dragging && m.kind == MouseKind::Drag {
            if !inside {
                self.viewport.scroll_by(1);
                at.row = rows.saturating_sub(1) as u16;
            } else if m.row == 0 {
                self.viewport.scroll_by(-1);
            }
        } else if !inside {
            at.row = rows.saturating_sub(1) as u16;
        }
        self.mouse_sel.wrap = SoftWrap::Width(self.viewport.content_width());
        let viewport = &self.viewport;
        let event = self
            .mouse_sel
            .mouse(&at, &|row| viewport.line_at(row), &self.cache.content, now_ms());
        if let SelectEvent::Finished(text) = event {
            if self.view.copy_on_select {
                self.copy(term, &text, true);
            }
        }
    }

    fn on_submit(&mut self, term: &mut dyn Terminal, text: String) {
        let trimmed = submitted_text(&text);
        if trimmed.is_empty() {
            return;
        }
        // #1552: the line the owner typed is the one origin that can approve a publication,
        // and it acts only on its own session's request.
        oxa_proto::owner_reply::typed(self.session.id.as_str(), &trimmed);
        if let Some(rest) = trimmed.strip_prefix('!') {
            let (excluded, cmd) = match rest.strip_prefix('!') {
                Some(r) => (true, r),
                None => (false, rest),
            };
            let command = cmd.trim().to_string();
            if command.is_empty() {
                self.push_notice(Level::Warning, "No command after '!'".to_string());
                return;
            }
            // The live block: `bash_execution_update` chunks extend it, the stored entry closes it.
            self.vm.blocks.push(Block::Bash {
                command: command.clone(),
                output: String::new(),
                exit: None,
                excluded,
                running: true,
                expanded: false,
            });
            self.send(SessionCmd::UserBash { command, excluded });
            return;
        }
        // Built-in commands are handled here, before the text can reach the session
        // (`Session::prompt` -> `expand_input`), whatever the core lists as built-in.
        if trimmed.starts_with('/') && self.slash(term, &trimmed) {
            return;
        }
        if self.fullscreen {
            self.viewport.scroll_to_bottom();
        }
        // Explicit input after a cancel key (Ctrl+C or Escape) is a new prompt, not steering
        // for the canceled run. The worker channel runs it after that operation releases the
        // session. Ordinary queued input still uses the existing unsuccessful-outcome guard.
        if self.accepts_steering() {
            self.handle.steer(trimmed);
            self.wake_queued();
            self.sync_queue();
        } else {
            self.send_prompt(trimmed);
        }
    }

    /// Both Enter forms use the same explicit recovery path and retain attachments.
    fn send_prompt(&mut self, text: String) {
        let recovering = std::mem::take(&mut self.abort_requested);
        let parts = self.take_attachments();
        self.send(if recovering {
            SessionCmd::RecoveryPrompt {
                handle: self.handle.clone(),
                text,
                parts,
            }
        } else {
            SessionCmd::Prompt { text, parts }
        });
    }

    /// Run a built-in slash command line; `false` when it is not one.
    fn slash(&mut self, term: &mut dyn Terminal, line: &str) -> bool {
        let action = {
            let last = self.vm.blocks.iter().rev().find_map(|b| match b {
                Block::Assistant { text, .. } => Some(text.as_str()),
                _ => None,
            });
            let cx = SlashCx {
                vm: &self.vm,
                keymap: &self.opts.keymap,
                settings: &self.opts.settings,
                dirs: &self.opts.dirs,
                cwd: Path::new(self.vm.status.cwd.as_str()),
                commands: &self.session.commands,
                busy: self.running(),
                session_file: self.session.file.as_deref(),
                session_id: self.session.id.as_str(),
                last_assistant_text: last,
                engine_url: &self.session.engine_url,
                models: &self.session.models,
                queue: (!self.session.engine_url.is_empty()).then_some(self.session.queue),
                engine_status: self.opts.engine_status.as_deref(),
            };
            dispatch_slash(line, &cx)
        };
        match action {
            Some(action) => {
                self.apply(term, action);
                true
            }
            None => false,
        }
    }

    fn last_assistant_text(&self) -> Option<String> {
        self.vm.blocks.iter().rev().find_map(|b| match b {
            Block::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
    }

    /// An active run may consume this input, but its final queue take may already be past.
    /// Keep one handle-bound wake on the existing worker channel for that handoff window.
    fn wake_queued(&self) {
        if self.handle.request_queued_wake()
            && self
                .cmds
                .send(SessionCmd::QueuedInput(self.handle.clone()))
                .is_err()
        {
            self.handle.clear_queued_wake();
        }
    }

    /// Show the queues as they are now (the core reports them only when it takes from them).
    fn sync_queue(&mut self) {
        let (steering, follow_up) = self.handle.queued();
        self.vm.status.steering = steering;
        self.vm.status.follow_up = follow_up;
    }

    /// Bring the queued messages back into the editor (pi: joined by blank lines, ahead of
    /// what is already typed).
    /// pi: a second Escape within 500 ms on an empty editor opens `/tree` or `/fork`
    /// (`doubleEscapeAction`, schema default `tree`; `none` = nothing).
    fn double_escape(&mut self) {
        let action = setting_or_default(&self.opts.settings, "doubleEscapeAction");
        let (open, last) = double_escape_step(&action, self.last_escape_ms, now_ms());
        self.last_escape_ms = last;
        if let Some(kind) = open {
            self.open_selector(kind);
        }
    }

    fn restore_queue(&mut self) {
        let queued = self.handle.dequeue();
        if let Some(inbox) = &self.inbox {
            inbox.retry();
        }
        self.vm.status.steering.clear();
        self.vm.status.follow_up.clear();
        if queued.is_empty() {
            return;
        }
        self.editor.prepend_text(&queued.join("\n\n"));
    }

    // -----------------------------------------------------------------------------------------
    // Attachments, clipboard, external editor
    // -----------------------------------------------------------------------------------------

    fn paste_image(&mut self) {
        let method = self.clip_method();
        match read_clipboard_image(&method) {
            Ok(Some(img)) => match image_part(&img, &self.opts.dirs.tmp_dir) {
                Ok(part) => {
                    self.pending_parts.push(part);
                    let n = self.pending_parts.len();
                    let line = self
                        .theme
                        .dim(&format!("{n} image(s) attached to the next prompt"));
                    self.vm
                        .widgets_above
                        .insert("oxa.attachments".to_string(), vec![line]);
                }
                Err(e) => self.push_notice(Level::Error, e.to_string()),
            },
            Ok(None) => self.push_notice(Level::Info, "The clipboard holds no image.".to_string()),
            Err(e) => self.push_notice(Level::Error, e.to_string()),
        }
    }

    fn take_attachments(&mut self) -> Vec<Part> {
        self.vm.widgets_above.remove("oxa.attachments");
        std::mem::take(&mut self.pending_parts)
    }

    fn drop_attachments(&mut self) {
        let _ = self.take_attachments();
    }

    fn clip_method(&self) -> crate::ClipMethod {
        let env = &self.opts.env;
        choose_clip_method(env, &self.caps, &|tool: &str| on_path(env, tool))
    }

    /// Copy `text`; `quiet` = no notice on success (copy-on-select).
    /// `true` when the text was copied (an error is reported as a notice).
    fn copy(&mut self, term: &mut dyn Terminal, text: &str, quiet: bool) -> bool {
        let in_tmux = self.opts.env.iter().any(|(k, _)| k == "TMUX");
        let method = self.clip_method();
        match copy_text(text, &method, in_tmux) {
            Ok(sequence) => {
                // OSC 52: the terminal does the copy when it reads the sequence.
                if let Some(seq) = sequence {
                    term.write(seq.as_bytes());
                    term.flush();
                }
                if !quiet {
                    self.push_notice(Level::Info, "Copied to the clipboard".to_string());
                }
                true
            }
            Err(e) => {
                self.push_notice(Level::Error, e.to_string());
                false
            }
        }
    }

    fn after_external(&mut self) {
        // The editor program drew on the screen: repaint everything.
        self.renderer.reset();
        if !self.fullscreen {
            self.reset_screen = true;
        }
    }

    /// `externalEditor`, then `$VISUAL`, then `$EDITOR`.
    fn editor_command(&self) -> Option<String> {
        crate::resolve_editor_with(
            self.opts.settings.string("externalEditor").as_deref(),
            &self.opts.env,
        )
    }

    fn dialog_external_editor(&mut self, term: &mut dyn Terminal) {
        let Some(text) = self.dialogs.editor_text() else {
            return;
        };
        match self.editor_command() {
            None => self.push_notice(Level::Warning, NO_EDITOR.to_string()),
            Some(cmd) => {
                self.drop_images(term);
                match edit_externally(&text, &cmd, &self.opts.dirs.tmp_dir, term) {
                    Ok(Some(edited)) => {
                        self.dialogs.set_editor_text(&edited);
                    }
                    Ok(None) => {}
                    Err(e) => self.push_notice(Level::Error, e.to_string()),
                }
                self.after_external();
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Selectors
    // -----------------------------------------------------------------------------------------

    /// Rows a list selector may use.
    fn selector_rows(&self) -> usize {
        (self.viewport.rows.max(12) / 2).clamp(6, 20)
    }

    /// The `enabledModels` patterns (pi's `--models` format: ids, `endpoint/id`, globs,
    /// `:level` suffixes) resolved against the engine's models, in pattern order.
    fn resolved_scope(&self) -> Vec<String> {
        let patterns = self.opts.settings.list("enabledModels");
        if patterns.is_empty() || self.session.models.is_empty() {
            return patterns;
        }
        let endpoint = self.endpoint_label();
        let available: Vec<oxa_config::ModelRef> = self
            .session
            .models
            .iter()
            .map(|m| oxa_config::ModelRef {
                endpoint: endpoint.clone(),
                model: m.clone(),
                thinking: None,
            })
            .collect();
        oxa_config::resolve_scoped(&patterns, &available)
            .0
            .into_iter()
            .map(|m| m.model)
            .collect()
    }

    /// The models.json name of the session's engine endpoint (the scoped-models "provider"),
    /// else its URL.
    fn endpoint_label(&self) -> String {
        let url = self.session.engine_url.trim_end_matches('/');
        let pairs: Vec<(&str, &str)> = self
            .opts
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let (models, _) =
            oxa_config::ModelsConfig::load(&self.opts.dirs, &oxa_config::Env::from_pairs(&pairs));
        models
            .endpoints
            .iter()
            .find(|e| e.base_url.trim_end_matches('/') == url)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| url.to_string())
    }

    fn model_choices(&self) -> Vec<ModelChoice> {
        let endpoint = self.endpoint_label();
        let st = &self.vm.status;
        let mut ids = self.session.models.clone();
        if !st.model.is_empty() && !ids.contains(&st.model) {
            ids.insert(0, st.model.clone());
        }
        ids.into_iter()
            .map(|id| {
                let loaded = id == st.model;
                ModelChoice {
                    endpoint: endpoint.clone(),
                    n_ctx: (loaded && st.n_ctx > 0).then_some(st.n_ctx),
                    loaded,
                    id,
                }
            })
            .collect()
    }

    fn list_sessions(&self) -> Vec<SessionMeta> {
        let cwd = self.cwd();
        let mut list = self.opts.store.list(if self.picker_all {
            None
        } else {
            Some(cwd.as_path())
        });
        // A session without a conversation has nothing to resume: it is not offered (pi writes
        // a session file only once it has a user or assistant message, so such sessions never
        // reach its picker; oxa's file exists from the start, e.g. the one a `-r` start left).
        list.retain(|m| m.message_count > 0);
        list
    }

    fn open_selector(&mut self, kind: SelectorKind) {
        self.dirty = true;
        match kind {
            SelectorKind::Model => {
                let sel = ModelSelector::new(self.model_choices(), &self.vm.status.model, &self.scoped);
                self.overlay = Some(Overlay::Model(sel));
            }
            SelectorKind::ScopedModels => {
                self.overlay = Some(Overlay::Scoped(ScopedModelsSelector::new(
                    self.model_choices(),
                    &self.scoped,
                )));
            }
            SelectorKind::Thinking => {
                let current = self.vm.status.thinking.unwrap_or(ThinkingLevel::Off);
                let note = self.vm.status.thinking_note.clone();
                let sel = ThinkingSelector::new(&ThinkingLevel::ALL, current, note.as_deref());
                self.overlay = Some(Overlay::Thinking(sel));
            }
            SelectorKind::Mode => {
                let sel = crate::ModeSelector::new(self.permission_mode());
                self.overlay = Some(Overlay::Mode(sel));
            }
            SelectorKind::Theme => {
                let mut themes = list_themes(&self.opts.theme_paths);
                // OXA's own themes are offered with OXA's look (pi's list is pi's).
                if self.view.skin.is_oxa() {
                    let at = oxa_tui::BUILTIN_THEME_NAMES.len().min(themes.len());
                    for (i, name) in oxa_tui::OXA_THEME_NAMES.iter().enumerate() {
                        if !themes.iter().any(|t| t.name == *name) {
                            themes.insert(
                                (at + i).min(themes.len()),
                                oxa_tui::ThemeInfo {
                                    name: name.to_string(),
                                    path: None,
                                },
                            );
                        }
                    }
                }
                self.overlay = Some(Overlay::Theme(ThemeSelector::new(themes, &self.theme_name)));
            }
            SelectorKind::Settings => {
                self.overlay = Some(Overlay::Settings(SettingsSelector::new(setting_items(
                    &self.opts.settings,
                ))));
            }
            SelectorKind::Sessions => {
                self.picker_all = false;
                let picker =
                    SessionPicker::new(self.list_sessions(), Some(&self.session.id), false, now_ms());
                self.overlay = Some(Overlay::Sessions(picker));
            }
            SelectorKind::Tree | SelectorKind::ForkPoint => {
                if self.running() {
                    let name = if kind == SelectorKind::Tree {
                        "tree"
                    } else {
                        "fork"
                    };
                    self.push_notice(
                        Level::Warning,
                        format!("/{name} needs an idle session: wait for the current run to finish."),
                    );
                } else {
                    self.pending_open = Some(kind);
                    self.send(SessionCmd::Snapshot);
                }
            }
            SelectorKind::Trust => self.ask_trust(),
            SelectorKind::Hotkeys => {
                for line in hotkeys_lines(&self.opts.keymap) {
                    self.push_notice(Level::Info, line);
                }
            }
            SelectorKind::Mcp => match self.opts.mcp.clone() {
                Some(manager) => {
                    let rows = crate::mcp_rows(&manager);
                    self.overlay = Some(Overlay::Mcp(crate::McpSelector::new(rows)));
                }
                // No manager (no mcp.json, or --no-mcp): the empty list says so.
                None => self.overlay = Some(Overlay::Mcp(crate::McpSelector::new(Vec::new()))),
            },
        }
    }

    fn overlay_key(&mut self, term: &mut dyn Terminal, key: &Key) {
        let Some(mut ov) = self.overlay.take() else { return };
        let km: Keymap = self.opts.keymap.clone();
        let mut keep = true;
        let mut park_tree = false;
        let mut actions: Vec<AppAction> = Vec::new();
        match &mut ov {
            Overlay::Model(sel) => match sel.key(key, &km) {
                Outcome::Done(id) => {
                    keep = false;
                    if sel.saves_default() {
                        actions.push(AppAction::SaveSetting {
                            key: "defaultModel".to_string(),
                            value: Value::String(id.clone()),
                            scope_project: false,
                        });
                    }
                    actions.push(self.model_switch(id));
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Scoped(sel) => match sel.key(key, &km) {
                Outcome::Done((list, persist)) => {
                    keep = false;
                    if persist {
                        let value = Value::Array(list.iter().cloned().map(Value::String).collect());
                        actions.push(AppAction::SaveSetting {
                            key: "enabledModels".to_string(),
                            value,
                            scope_project: false,
                        });
                    }
                    self.scoped = list;
                    self.scope_from_settings = false;
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Thinking(sel) => match sel.key(key, &km) {
                Outcome::Done(level) => {
                    keep = false;
                    if sel.saves_default() {
                        actions.push(AppAction::SaveSetting {
                            key: "defaultThinkingLevel".to_string(),
                            value: Value::String(level.as_str().to_string()),
                            scope_project: false,
                        });
                    }
                    actions.push(AppAction::Session(SessionCmd::SetThinking(level)));
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Mode(sel) => match sel.key(key, &km) {
                Outcome::Done((mode, save)) => {
                    keep = false;
                    actions.push(AppAction::SetPermissionMode { mode, save });
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Theme(sel) => {
                let out = sel.key(key, &km);
                match out {
                    Outcome::Done(name) => {
                        keep = false;
                        actions.push(AppAction::SetTheme { name, persist: true });
                    }
                    Outcome::Cancelled => {
                        keep = false;
                        actions.push(AppAction::SetTheme {
                            name: sel.original().to_string(),
                            persist: false,
                        });
                    }
                    _ => {
                        if let Some(name) = sel.preview() {
                            actions.push(AppAction::SetTheme { name, persist: false });
                        }
                    }
                }
            }
            Overlay::Settings(sel) => {
                let out = sel.key(key, &km);
                for change in sel.take_changes() {
                    actions.push(AppAction::SaveSetting {
                        key: change.key,
                        value: change.value,
                        scope_project: false,
                    });
                }
                if out.closed() {
                    keep = false;
                }
            }
            Overlay::Sessions(sel) => match sel.key(key, &km) {
                Outcome::Done(SessionPick::Open(sref)) => {
                    keep = false;
                    if sref.id != self.session.id {
                        if self.running() {
                            actions.push(AppAction::Notify {
                                level: Level::Warning,
                                text: "/resume needs an idle session: wait for the current run to finish."
                                    .to_string(),
                            });
                        } else {
                            actions.push(AppAction::Session(SessionCmd::Reopen(Reopen::Resume(sref))));
                        }
                    }
                }
                Outcome::Done(SessionPick::Rename { session, name }) => {
                    self.rename_session(&session, &name);
                    sel.set_sessions(self.list_sessions(), self.picker_all);
                }
                Outcome::Done(SessionPick::Delete(sref)) => {
                    let (is_error, msg) = self.delete_session(&sref);
                    sel.set_sessions(self.list_sessions(), self.picker_all);
                    sel.set_status(is_error, &msg);
                }
                Outcome::Done(SessionPick::ToggleScope) => {
                    self.picker_all = !self.picker_all;
                    sel.set_sessions(self.list_sessions(), self.picker_all);
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Tree(sel) => match sel.key(key, &km) {
                Outcome::Done(TreePick::Navigate {
                    target,
                    summarize,
                    instructions,
                    refill_editor,
                }) => {
                    keep = false;
                    let leaf = self.tree.as_ref().map(shown_leaf);
                    if refill_editor.is_none() && leaf.as_deref() == Some(target.as_str()) {
                        // pi: selecting the current leaf is a no-op.
                        actions.push(AppAction::Notify {
                            level: Level::Info,
                            text: "Already at this point".to_string(),
                        });
                    } else {
                        park_tree = self.tree_pick(target, summarize, instructions, refill_editor);
                    }
                }
                // The navigator stays open; it gets the new tree when the worker answers.
                Outcome::Done(TreePick::SetLabel { target, label }) => {
                    self.send(SessionCmd::SetLabel { target, label });
                    self.send(SessionCmd::Snapshot);
                }
                // pi: copy the selected entry's text; the navigator stays open.
                Outcome::Done(TreePick::Copy { text }) => match text {
                    Some(text) => {
                        if self.copy(term, &text, true) {
                            self.push_notice(Level::Info, "Copied selected message to clipboard".to_string());
                        }
                    }
                    None => self.push_notice(Level::Error, "Selected entry has no text to copy".to_string()),
                },
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Mcp(sel) => match sel.key(key, &km) {
                Outcome::Done(request) => {
                    if let Some(manager) = self.opts.mcp.clone() {
                        if let Err(e) = crate::apply_mcp_request(&manager, &request) {
                            self.push_notice(Level::Error, e.to_string());
                        }
                        sel.set_rows(crate::mcp_rows(&manager));
                    }
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
            Overlay::Fork(sel) => match sel.key(key, &km) {
                Outcome::Done((entry, text)) => {
                    keep = false;
                    // pi: the fork holds everything before the chosen user message (the core's
                    // `fork_at` moves the leaf to its parent), and the message itself goes back
                    // into the editor.
                    let has_parent = self
                        .tree
                        .as_ref()
                        .and_then(|t| t.get(&entry))
                        .is_some_and(|e| e.parent.is_some());
                    actions.push(if has_parent {
                        AppAction::Session(SessionCmd::ForkAt { entry })
                    } else {
                        AppAction::Session(SessionCmd::Reopen(Reopen::New))
                    });
                    actions.push(AppAction::SetEditorText(text));
                }
                Outcome::Cancelled => keep = false,
                _ => {}
            },
        }
        if keep {
            self.overlay = Some(ov);
        } else if park_tree {
            self.tree_back = Some(ov);
        }
        for action in actions {
            self.apply(term, action);
        }
    }

    fn model_switch(&mut self, id: String) -> AppAction {
        if id
            == self
                .model_pending
                .clone()
                .unwrap_or_else(|| self.vm.status.model.clone())
        {
            return AppAction::None;
        }
        if !self.abort_requested && !self.handle.is_aborting() && self.handle.select_model_during_backoff(&id)
        {
            return AppAction::None;
        }
        if self.running() {
            return AppAction::Notify {
                level: Level::Warning,
                text: "/model needs an idle session or a goal waiting to retry.".to_string(),
            };
        }
        AppAction::Session(SessionCmd::SetModel(id))
    }

    /// A `/tree` selection. `target` empty = the first user message was chosen: the leaf would
    /// move above the first entry, which is a fresh start (a new session with the message back
    /// in the editor). Otherwise pi's summary question comes first unless
    /// `branchSummary.skipPrompt` is set. `true` = the question was asked (the navigator is
    /// kept for Escape).
    fn tree_pick(
        &mut self,
        target: String,
        summarize: bool,
        instructions: Option<String>,
        refill: Option<String>,
    ) -> bool {
        if target.is_empty() {
            self.send(SessionCmd::Reopen(Reopen::New));
            if let Some(text) = refill {
                if self.editor.text().trim().is_empty() {
                    self.editor.set_text(&text);
                }
            }
            return false;
        }
        if summarize || self.opts.settings.bool("branchSummary.skipPrompt", false) {
            self.navigate(target, summarize, instructions, refill);
            return false;
        }
        let ask = self.ask_summary(target, refill);
        self.asks.push(ask);
        true
    }

    /// pi's summary question (`Summarize branch?`, interactive-mode.ts showTreeSelector).
    fn ask_summary(&mut self, target: String, refill: Option<String>) -> Ask {
        let (tx, rx) = mpsc::channel();
        let req = UiRequest::Select {
            title: "Summarize branch?".to_string(),
            options: vec![
                "No summary".to_string(),
                "Summarize".to_string(),
                "Summarize with custom prompt".to_string(),
            ],
            timeout_ms: None,
            reply: tx,
        };
        self.dialogs.handle(req, &mut self.vm, now_ms());
        Ask::Summary { rx, target, refill }
    }

    fn navigate(
        &mut self,
        target: String,
        summarize: bool,
        instructions: Option<String>,
        refill: Option<String>,
    ) {
        self.send(SessionCmd::Navigate {
            target,
            summarize,
            instructions,
        });
        self.tree_back = None;
        // pi: the user message's text refills the editor only when the editor is empty.
        if let Some(text) = refill {
            if self.editor.text().trim().is_empty() {
                self.editor.set_text(&text);
            }
        }
        self.dirty = true;
    }

    fn rename_session(&mut self, session: &SessionRef, name: &str) {
        if session.id == self.session.id {
            self.send(SessionCmd::SetName(name.to_string()));
            return;
        }
        let store = self.opts.store.clone();
        let result = store.load(session).and_then(|entries| {
            let leaf = entries.last().map(|e| e.id.clone()).unwrap_or_default();
            oxa_session::set_name(store.as_ref(), session, &leaf, name)
        });
        if let Err(e) = result {
            self.push_notice(Level::Error, e.to_string());
        }
    }

    /// pi `onDeleteSession`: the picker's status line says what happened.
    fn delete_session(&mut self, session: &SessionRef) -> (bool, String) {
        if session.id == self.session.id {
            return (true, "Cannot delete the currently active session".to_string());
        }
        let removed = match oxa_session::remove_session_preserving_spills(
            self.opts.store.as_ref(),
            &self.opts.dirs.tmp_dir,
            session,
        ) {
            Ok(removed) => removed,
            Err(e) => return (true, format!("Failed to delete: {}", e.message)),
        };
        // P6: the session's spill files go with it (`<tmp_dir>/<id>`), so they do not outlive it,
        // unless a fork or clone of it still names them (#576).
        if let Err(e) = oxa_session::remove_spill_dir_unless_forked(
            self.opts.store.as_ref(),
            &self.opts.dirs.tmp_dir,
            session,
        ) {
            return (
                true,
                format!("Session removed, but its spill files were not: {}", e.message),
            );
        }
        match removed {
            oxa_session::Removed::Trashed => (false, "Session moved to trash".to_string()),
            oxa_session::Removed::Deleted => (false, "Session deleted".to_string()),
        }
    }

    fn ask_trust(&mut self) {
        if self.opts.save_trust.is_none() {
            self.push_notice(
                Level::Warning,
                "/trust: the trust store is not available here.".to_string(),
            );
            return;
        }
        let (title, options, choices) = trust_question(&self.vm.status.cwd, &self.opts.gated);
        let (tx, rx) = mpsc::channel();
        let req = UiRequest::Select {
            title,
            options,
            timeout_ms: None,
            reply: tx,
        };
        self.dialogs.handle(req, &mut self.vm, now_ms());
        self.asks.push(Ask::Trust { rx, choices });
    }

    fn save_trust(&mut self, choice: TrustChoice) {
        if choice == TrustChoice::Skip {
            return;
        }
        let Some(save) = self.opts.save_trust.as_mut() else {
            return;
        };
        match save(choice) {
            Ok(()) => {
                let what = match choice {
                    TrustChoice::Trust => "This project is trusted",
                    TrustChoice::TrustParent => "The parent directory is trusted",
                    TrustChoice::NoTrust | TrustChoice::Skip => "This project is not trusted",
                };
                self.push_notice(
                    Level::Info,
                    format!("{what}. Saved; it applies the next time oxa starts here."),
                );
            }
            Err(e) => self.push_notice(Level::Error, e.to_string()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------------------------

    fn apply(&mut self, term: &mut dyn Terminal, action: AppAction) {
        self.dirty = true;
        match action {
            AppAction::None => {}
            AppAction::Session(cmd) => self.send(cmd),
            AppAction::Open(kind) => self.open_selector(kind),
            AppAction::Notify { level, text } => self.push_notice(level, text),
            AppAction::Yolo(want) => self.set_yolo(want),
            AppAction::SetPermissionMode { mode, save } => self.set_permission_mode(mode, save),
            AppAction::ShowLines(lines) => {
                for line in lines {
                    self.push_notice(Level::Info, line);
                }
            }
            AppAction::SetEditorText(t) => self.editor.set_text(&t),
            AppAction::Copy(text) => {
                self.copy(term, &text, false);
            }
            AppAction::Export { path } => self.export(path),
            AppAction::DebugDump => self.debug_dump(term),
            AppAction::BugReport { description } => self.bug_report(term, &description),
            AppAction::SaveSetting {
                key,
                value,
                scope_project,
            } => {
                let cwd = self.cwd();
                match self
                    .opts
                    .settings
                    .set(&self.opts.dirs, &cwd, &key, value, scope_project)
                {
                    Ok(()) => {
                        self.refresh_view_settings();
                        self.push_notice(Level::Info, format!("Saved {key}"));
                    }
                    Err(e) => self.push_notice(Level::Error, e.to_string()),
                }
            }
            AppAction::SetTheme { name, persist } => {
                match resolve_ui_theme(&*self.opts, &name, self.caps, self.term_colors.as_ref()) {
                    Ok(theme) => {
                        self.install_theme(theme);
                        if persist {
                            self.theme_name = name.clone();
                            let cwd = self.cwd();
                            let value = Value::String(name);
                            if let Err(e) =
                                self.opts
                                    .settings
                                    .set(&self.opts.dirs, &cwd, "theme", value, false)
                            {
                                self.push_notice(Level::Error, e.to_string());
                            }
                        }
                    }
                    Err(e) => self.push_notice(Level::Error, e.to_string()),
                }
            }
            AppAction::McpRequest(_) => self.push_notice(
                Level::Warning,
                "/mcp: the MCP server list is not in this build yet (wave F).".to_string(),
            ),
            AppAction::RenameSession { session, name } => self.rename_session(&session, &name),
            AppAction::DeleteSession { session } => {
                let (is_error, msg) = self.delete_session(&session);
                let level = if is_error { Level::Error } else { Level::Info };
                self.push_notice(level, msg);
            }
            AppAction::SetTrust(trusted) => self.save_trust(if trusted {
                TrustChoice::Trust
            } else {
                TrustChoice::NoTrust
            }),
            AppAction::ToggleToolsExpanded => self.view.tools_expanded = !self.view.tools_expanded,
            AppAction::ToggleThinkingHidden => self.view.hide_thinking = !self.view.hide_thinking,
            AppAction::ExternalEditor => match self.editor_command() {
                None => self.push_notice(Level::Warning, NO_EDITOR.to_string()),
                Some(cmd) => {
                    let text = self.editor.expanded_text();
                    self.drop_images(term);
                    match edit_externally(&text, &cmd, &self.opts.dirs.tmp_dir, term) {
                        Ok(Some(edited)) => self.editor.set_text(&edited),
                        Ok(None) => {}
                        Err(e) => self.push_notice(Level::Error, e.to_string()),
                    }
                    self.after_external();
                }
            },
            AppAction::Suspend => {
                self.drop_images(term);
                suspend_process(term);
                self.after_external();
            }
            AppAction::Abort => self.handle.abort(),
            AppAction::AbortRetry => self.handle.abort_retry(),
            AppAction::Dequeue => self.restore_queue(),
            AppAction::Quit => self.quit = Some(0),
            AppAction::Many(list) => {
                for a in list {
                    self.apply(term, a);
                }
            }
        }
    }

    /// `/export [path]` (pi): HTML by default, JSONL when the path ends in `.jsonl` (a copy of
    /// the session file). A relative path is taken from the working directory; with no path the
    /// HTML goes to the default name (`default_output_name`, as `--export` and RPC do).
    fn export(&mut self, path: Option<PathBuf>) {
        use std::os::unix::fs::MetadataExt;

        let Some(source) = self.session.file.clone() else {
            self.push_notice(
                Level::Warning,
                "/export: this session is not saved to a file.".to_string(),
            );
            return;
        };
        let cwd = self.cwd();
        let target = path
            .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
            .unwrap_or_else(|| default_output_name(Some(&source), &self.session.id, &cwd));
        let aliases_source = source == target
            || match (std::fs::metadata(&source), std::fs::metadata(&target)) {
                (Ok(source), Ok(target)) => source.dev() == target.dev() && source.ino() == target.ino(),
                _ => false,
            };
        if aliases_source {
            self.push_notice(
                Level::Error,
                "/export: destination is the session file; choose a different path.".to_string(),
            );
            return;
        }
        match target {
            target if target.extension().is_some_and(|e| e == "jsonl") => {
                match std::fs::copy(&source, &target) {
                    Ok(_) => self.push_notice(
                        Level::Info,
                        format!("Exported the session to {}", target.display()),
                    ),
                    Err(e) => {
                        let err = OxaError::io(&format!("cannot write {}", target.display()), &e);
                        self.push_notice(Level::Error, err.to_string());
                    }
                }
            }
            path => self.send(SessionCmd::Export { path }),
        }
    }

    fn debug_dump(&mut self, term: &mut dyn Terminal) {
        let (cols, rows) = term.size();
        let mut text = debug_dump(&self.last_frame, cols, rows, &self.vm);
        text.push_str(&format!("--- terminal capabilities ---\n{:?}\n", self.caps));
        let path = self.opts.dirs.agent_dir.join("oxa-debug.log");
        match write_private(&path, &text) {
            Ok(()) => self.push_notice(Level::Info, format!("Debug dump written to {}", path.display())),
            Err(e) => self.push_notice(Level::Error, e.to_string()),
        }
    }

    /// `/bug`: a local text bundle with no transcript text and no environment: the
    /// description, versions, terminal facts and the block counts. Nothing leaves the machine.
    fn bug_report(&mut self, term: &mut dyn Terminal, description: &str) {
        let (cols, rows) = term.size();
        let facts = vec![
            format!(
                "layout: {}",
                if self.fullscreen { "fullscreen" } else { "regular" }
            ),
            format!("session id: {}", self.session.id),
            format!("engine: {}", self.session.engine_url),
            format!("capabilities: {:?}", self.caps),
            debug_dump(&[], cols, rows, &self.vm),
        ];
        match oxa_core::write_bug_report(&self.opts.dirs, description, &facts) {
            Ok(path) => self.push_notice(
                Level::Info,
                format!(
                    "Bug report written to {} (review it before you share it)",
                    path.display()
                ),
            ),
            Err(e) => self.push_notice(Level::Error, e.to_string()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------------------------

    /// The status as shown: a dialog that waits for the user is `Blocked`.
    fn shown_status(&self) -> StatusModel {
        let mut status = self.vm.status.clone();
        if self.dialogs.is_open() {
            status.activity = Activity::Blocked;
        }
        status
    }

    /// The window title, written when it changes.
    fn report_status(&mut self, term: &mut dyn Terminal) {
        let title = if self.view.skin.is_oxa() {
            oxa_title_text(&self.vm)
        } else {
            title_text(&self.vm)
        };
        if self.sent_title.as_deref() != Some(title.as_str()) {
            term.write(title_sequence(&title).as_bytes());
            self.sent_title = Some(title);
        }
    }

    fn header_lines(&self, width: u16) -> Vec<String> {
        let info = HeaderInfo {
            version: oxa_proto::BUILD_VERSION.to_string(),
            resources: self.resources.clone(),
            engine_url: self.session.engine_url.clone(),
            memory_policy: self.session.memory_policy,
            model: self.vm.status.model.clone(),
            model_remote: self.vm.status.model_remote,
            model_provider: self.vm.status.model_provider,
        };
        let header_only = self.opts.settings.get("quietStartup").and_then(Value::as_str) == Some("header");
        if self.view.skin.is_oxa() && self.vm.header_override.is_none() {
            // OXA's look: the splash (O-184: it heads every empty chat and scrolls away
            // with the conversation).
            let label = self.theme_label.clone();
            return oxa_splash(&info, &label, !header_only, width, &self.theme, self.view.skin);
        }
        let mut lines = render_header(&info, &self.vm, &self.opts.keymap, false, width, &self.theme);
        // pi `quietStartup: "header"`: the header (version and key hints) stays; the model
        // scope line (here the engine and model line, the last one without resources) and the
        // loaded-resource listing go.
        if header_only && self.vm.header_override.is_none() {
            lines.pop();
        }
        lines
    }

    /// Everything below the transcript: loader, widgets, the editor (or the open dialog or
    /// selector), the search bar and the footer. The cursor is relative to these lines.
    /// `rows` is the terminal height when only its last rows stay visible (fullscreen).
    fn bottom(&mut self, width: u16, rows: Option<usize>) -> (Vec<String>, Option<CursorPos>) {
        let mut lines: Vec<String> = Vec::new();
        // The loader line: a run in progress, or a run that waits for the user's answer.
        let status = if self.running() {
            self.shown_status()
        } else {
            self.vm.status.clone()
        };
        let oxa = self.view.skin.is_oxa();
        let mut status = if oxa { self.timed_status(status) } else { status };
        if oxa {
            // The tool queue of the reply at hand (an extension's working text comes first).
            if status.working_message.is_none() {
                status.working_message =
                    crate::skin::tool_queue_detail(&self.vm.blocks, &status.activity, self.view.skin);
            }
            // A run for the arcade is what the line shows: from the first activity to idle
            // (the handle may stay busy a moment longer; that is not a second run).
            let active = status.activity != Activity::Idle;
            self.arcade.observe(active, &self.vm.blocks, now_ms());
            let scene = ArcadeScene::from_setting(self.opts.settings.string("ui.animationScene").as_deref());
            let run = self.arcade.run(now_ms());
            let pixel_line = if self.pixels() {
                crate::skin::oxa_pixel_line(
                    &status,
                    &self.opts.keymap,
                    &run,
                    width,
                    &self.theme,
                    self.view.skin,
                    &mut self.pixel,
                    self.theme_epoch,
                    &mut self.gfx_out,
                )
            } else {
                None
            };
            // C4: the director picks the scene of the activity (design 3.2) when the arcade plays
            // its own scene; the closing scene after a run stays with the arcade.
            let staged = pixel_line.is_none() && self.stage_on();
            let now = now_ms();
            if staged {
                self.stage.reseed(&self.session.id);
                let ascii = self.view.skin != crate::Skin::Oxa;
                self.stage
                    .update(pending_of(&status.activity, ascii), scene_cells(width), now);
            }
            let line = pixel_line.or_else(|| {
                if staged && status.activity != Activity::Idle {
                    oxa_stage_line(
                        &self.stage,
                        &status,
                        &self.opts.keymap,
                        now,
                        width,
                        &self.theme,
                        self.view.skin,
                    )
                } else {
                    oxa_arcade_line(
                        &status,
                        &self.opts.keymap,
                        self.animation(),
                        scene,
                        &run,
                        width,
                        &self.theme,
                        self.view.skin,
                    )
                }
            });
            lines.extend(line);
        } else if let Some(text) = activity_text(&status) {
            lines.push(self.theme.dim(&text));
        }
        // O-182: the YOLO banner, for a moment after it was switched on.
        if oxa && now_ms() < self.yolo_banner_until {
            let motion = self.motion();
            lines.push(oxa_yolo_banner(
                now_ms().saturating_sub(self.yolo_banner_since),
                motion,
                width,
                &self.theme,
                self.view.skin,
            ));
        }
        // Queued messages wait above the editor (pi's pending messages).
        let queued = self.vm.status.steering.iter().map(|m| format!("Steering: {m}"));
        let queued: Vec<String> = queued
            .chain(self.vm.status.follow_up.iter().map(|m| format!("Follow-up: {m}")))
            .collect();
        if !queued.is_empty() {
            for line in &queued {
                let one_line: String = line
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                lines.push(oxa_tui::truncate_to_width(
                    &self.theme.dim(&one_line),
                    width as usize,
                    "...",
                ));
            }
            let key = self
                .opts
                .keymap
                .keys_for("app.message.dequeue")
                .first()
                .map(|k| k.to_string());
            if let Some(key) = key {
                let hint = self
                    .theme
                    .dim(&format!("\u{21b3} {key} to edit all queued messages"));
                lines.push(oxa_tui::truncate_to_width(&hint, width as usize, "..."));
            }
        }
        for widget in self.vm.widgets_above.values() {
            lines.extend(widget.iter().cloned());
        }
        let text = self.editor.text();
        self.editor_opts.border_token = if oxa && self.vm.status.yolo {
            // O-182: the prompt's frame is hot while YOLO is on.
            "error".to_string()
        } else {
            editor_border_token(&self.vm.status, &text).to_string()
        };
        // OXA's look: the suggestions are a panel; in fullscreen it floats over the transcript.
        self.editor_opts.popup = match self.view.skin {
            crate::Skin::Pi => oxa_tui::PopupStyle::List,
            crate::Skin::Oxa => oxa_tui::PopupStyle::Panel,
            crate::Skin::OxaAscii => oxa_tui::PopupStyle::PanelAscii,
        };
        self.editor_opts.popup_detached = oxa && self.fullscreen;
        self.popup_overlay = None;
        self.editor.set_opts(self.editor_opts.clone());
        let top = lines.len();
        // OXA's look: selectors sit in a closed frame (dialogs and the tree draw their own).
        let skin = self.view.skin;
        let framed = oxa
            && !self.dialogs.is_open()
            && self.overlay.is_some()
            && !matches!(self.overlay, Some(Overlay::Tree(_)))
            && width >= 8;
        let outer = width;
        let width = if framed { frame_inner_width(outer) } else { outer };
        let (body, cursor) = if self.dialogs.is_open() {
            // Fullscreen keeps only the last terminal rows. Everything drawn below the dialog
            // is counted first (the footer, an extension's own footer, widgets and the search
            // bar), so the dialog pages inside the rows that are really left for it.
            let below: usize =
                self.vm.widgets_below.values().map(Vec::len).sum::<usize>() + usize::from(self.search_open);
            let footer = match &self.vm.footer_override {
                Some(lines) => lines.len(),
                None => {
                    let statuses = self
                        .vm
                        .status
                        .ext_status
                        .values()
                        .filter(|text| !text.is_empty())
                        .count();
                    FULLSCREEN_FOOTER_ROWS + statuses
                }
            };
            let height = rows.map(|r| r.saturating_sub(below + footer));
            let body = self.dialogs.render_in(width, height, &self.theme);
            (body, self.dialogs.cursor())
        } else if let Some(ov) = self.overlay.as_mut() {
            let theme = &self.theme;
            let body = match ov {
                Overlay::Model(s) => s.render(width, theme),
                Overlay::Scoped(s) => s.render(width, theme),
                Overlay::Thinking(s) => s.render(width, theme),
                Overlay::Mode(s) => s.render(width, theme),
                Overlay::Theme(s) => s.render(width, theme),
                Overlay::Settings(s) => s.render(width, theme),
                Overlay::Sessions(s) => s.render(width, theme),
                Overlay::Tree(s) => s.render_framed(width, theme, &self.opts.keymap),
                Overlay::Fork(s) => s.render(width, theme),
                Overlay::Mcp(s) => s.render(width, theme),
            };
            if framed {
                (frame_box(&body, outer, theme, skin, "borderAccent"), None)
            } else {
                (body, None)
            }
        } else {
            let body = self.editor.render(width, &self.theme);
            if self.editor_opts.popup_detached && self.editor.autocomplete_open() {
                self.popup_overlay = Some((top, self.editor.popup(width, &self.theme)));
            }
            (
                body,
                if self.search_open {
                    None
                } else {
                    self.editor.cursor()
                },
            )
        };
        let mut cursor = cursor.map(|p| CursorPos {
            row: p.row + top,
            col: p.col,
        });
        let width = outer;
        lines.extend(body);
        if self.search_open {
            let label = format!("Search: {}", self.search.query);
            let col = oxa_tui::visible_width(&label).min(width.saturating_sub(1) as usize) as u16;
            cursor = Some(CursorPos {
                row: lines.len(),
                col,
            });
            let hint = self.theme.dim(&format!("  {}", self.search.position_text()));
            lines.push(oxa_tui::truncate_to_width(
                &format!("{label}{hint}"),
                width as usize,
                "",
            ));
        }
        for widget in self.vm.widgets_below.values() {
            lines.extend(widget.iter().cloned());
        }
        if oxa {
            // The footer shows the live state: the HUD's state segment and spinner.
            let ctx = self.hint_context();
            let saved = std::mem::replace(&mut self.vm.status, status);
            // The footer names only keys this terminal can send: without the kitty keyboard
            // protocol a shift chord arrives as its legacy byte (ctrl+shift+f as ctrl+f), so
            // the next bound key of the action is shown (alt+s for search), or none.
            let sendable;
            let keymap = if self.caps.kitty_keyboard {
                &self.opts.keymap
            } else {
                sendable = self.opts.keymap.retain_keys(oxa_tui::legacy_sendable);
                &sendable
            };
            lines.extend(oxa_footer(
                &self.vm,
                ctx,
                keymap,
                self.fullscreen,
                width,
                &self.theme,
                skin,
            ));
            self.vm.status = saved;
        } else {
            lines.extend(render_footer(&self.vm, width, &self.theme));
        }
        // Regular mode commits its splash once; policy changes belong in the live
        // region so a cold load, switch or idle unload remains observable.
        if !self.fullscreen {
            if let Some(policy) = self.session.memory_policy {
                lines.push(self.theme.dim(&format!("Memory: {policy}")));
            }
        }
        (lines, cursor)
    }

    /// Draw one frame in the active layout.
    pub(crate) fn draw(&mut self, term: &mut dyn Terminal) {
        self.dirty = false;
        // Consume edges even when notifications are disabled/focused: never replay them
        // later on focus-out. Input was polled first, so pending focus reports are current.
        let finished = std::mem::take(&mut self.run_finished);
        let permission = self.dialogs.take_permission_opened();
        let notify = match self.opts.settings.string("ui.notify").as_deref() {
            Some("off") => false,
            Some("always") => true,
            _ => !self.focused,
        };
        if notify {
            for (wanted, text) in [
                (finished, "oxa: Run finished"),
                (permission, "oxa: Permission required"),
            ] {
                if wanted {
                    term.write(oxa_tui::notify_sequence(text).as_bytes());
                    // OSC has no support acknowledgement. A standalone BEL supplies the
                    // fallback for terminals that ignore OSC 9; no external sound/player.
                    term.write(b"\x07");
                }
            }
        }
        self.report_status(term);
        if self.gfx_reset {
            self.gfx_reset = false;
            self.pixel.clear(&mut self.gfx_out);
        }
        if self.fullscreen {
            self.draw_fullscreen(term);
        } else {
            self.draw_regular(term);
        }
        term.flush();
    }

    /// Regular layout: committed finished blocks (once), then the live region: unfinished
    /// blocks and the bottom lines.
    fn draw_regular(&mut self, term: &mut dyn Terminal) {
        let (cols, _rows) = term.size();
        let width = cols.max(1);
        if self.reset_screen || self.printer.needs_reset(&self.vm) {
            self.renderer.clear(term);
            self.printer.reset();
            self.reset_screen = false;
        }
        let mut commit: Vec<String> = Vec::new();
        if self.header_pending {
            self.header_pending = false;
            commit = self.header_lines(width);
        }
        let step = {
            let env = ViewEnv {
                caps: self.caps,
                hooks: &self.opts.hooks,
                keymap: Some(&self.opts.keymap),
            };
            self.printer
                .step_in(&self.vm, width, &self.theme, &self.view, &env)
        };
        commit.extend(step.commit);
        if !commit.is_empty() {
            self.renderer.commit(&commit, term);
        }
        let mut live = step.live;
        let top = live.len();
        // The regular layout keeps the frame lines in the scrollback, so the dialog is not
        // bounded to the terminal height here.
        let (bottom, cursor) = self.bottom(width, None);
        live.extend(bottom);
        let cursor = cursor.map(|p| CursorPos {
            row: p.row + top,
            col: p.col,
        });
        // pi `terminal.clearOnShrink` (or OXA_CLEAR_ON_SHRINK=1 when unset): a live region that
        // got shorter is repainted whole instead of diffed.
        if self.clear_on_shrink && live.len() < self.last_frame.len() {
            self.renderer.repaint();
        }
        self.last_frame = live.clone();
        if !self.gfx_out.is_empty() {
            // New frames of the pixel scene: they must be there before the row that shows them.
            term.write(self.gfx_out.as_bytes());
            self.gfx_out.clear();
        }
        self.renderer.draw(&Frame { lines: live, cursor }, term);
    }

    /// Fullscreen layout: the viewport over the whole transcript on top, the bottom lines below.
    fn draw_fullscreen(&mut self, term: &mut dyn Terminal) {
        let (cols, rows) = term.size();
        let width = cols.max(1);
        let rows = rows as usize;
        self.reset_screen = false;
        // Only the screen's rows survive here: the dialog pages within them.
        let (bottom, cursor) = self.bottom(width, Some(rows));
        let bottom_rows = bottom.len().min(rows);
        let vp_rows = rows - bottom_rows;
        self.viewport.resize(width, vp_rows);
        // pi's `auto` scrollbar is transient: shown while scrolling and for a second after.
        self.viewport.auto_bar_visible = now_ms() < self.bar_until;
        let content_width = self.viewport.content_width().max(1);
        let header = if self.view.quiet_startup {
            Vec::new()
        } else {
            self.header_lines(content_width)
        };
        let changed = {
            let env = ViewEnv {
                caps: self.caps,
                hooks: &self.opts.hooks,
                keymap: Some(&self.opts.keymap),
            };
            self.cache.update(
                &self.vm,
                &header,
                content_width,
                &self.theme,
                self.theme_epoch,
                &self.view,
                &env,
            )
        };
        let user_blocks: Vec<usize> = self
            .vm
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_user())
            .map(|(i, _)| i)
            .collect();
        self.viewport
            .set_content(self.cache.content.len(), self.cache.starts.clone(), user_blocks);
        if changed && !self.search.query.is_empty() {
            self.search.refresh(&self.cache.content);
        }
        let first = self
            .viewport
            .offset
            .min(self.cache.content.len().saturating_sub(vp_rows));
        let searching = !self.search.matches.is_empty();
        let selection = self.mouse_sel.selection;
        let lines = if searching || selection.is_some() {
            // Only the visible lines are highlighted; the rows above them stay empty.
            let mut visible: Vec<String> = self
                .cache
                .content
                .iter()
                .skip(first)
                .take(vp_rows)
                .cloned()
                .collect();
            let count = visible.len();
            if searching {
                let shifted = shift_search(&self.search, first);
                visible = highlight_matches(&visible, 0, count, &shifted, &self.theme);
            }
            if let Some(sel) = selection {
                if let Some(shifted) = shift_selection(&sel, first, count) {
                    visible = highlight_selection(&visible, 0, count, &shifted, &self.theme);
                }
            }
            let mut shown: Vec<String> = Vec::with_capacity(first + count);
            shown.resize(first, String::new());
            shown.extend(visible);
            self.viewport.render(&shown, &self.theme)
        } else {
            self.viewport.render(&self.cache.content, &self.theme)
        };
        let (mut frame, _) = compose_fullscreen(&lines, &bottom, rows);
        let cut = bottom.len() - bottom_rows;
        if let Some((top, popup)) = self.popup_overlay.take() {
            crate::skin::float_above(&mut frame, (vp_rows + top).saturating_sub(cut), &popup);
        }
        let cursor = cursor.filter(|p| p.row >= cut).map(|p| CursorPos {
            row: vp_rows + p.row - cut,
            col: p.col,
        });
        self.last_frame = frame.clone();
        if !self.gfx_out.is_empty() {
            // New frames of the pixel scene: they must be there before the row that shows them.
            term.write(self.gfx_out.as_bytes());
            self.gfx_out.clear();
        }
        self.renderer.draw(&Frame { lines: frame, cursor }, term);
    }

    /// Leave the screen as the caller expects it: dialogs answered, a run aborted and the
    /// alternate screen left.
    pub(crate) fn finish(mut self, term: &mut dyn Terminal) -> Handle {
        self.dialogs.close_all();
        self.asks.clear();
        // The run in progress stops. The queues are dropped by `run_session`, which owns the
        // worker's shutdown (a caller of `run_loop` alone keeps them to inspect).
        self.handle.abort_retry();
        if self.running() {
            self.handle.abort();
        }
        self.drop_images(term);
        term.set_mode(TermMode::FocusReporting, false);
        if self.fullscreen {
            self.renderer.clear(term);
            term.set_mode(TermMode::Mouse, false);
            term.set_mode(TermMode::AltScreen, false);
            // pi `stopInteractiveTui`: with `fullscreenExitOutput: "transcript"` (the default)
            // the transcript is rendered onto the normal screen, so it stays in the scrollback.
            if self.opts.settings.string("fullscreenExitOutput").as_deref() != Some("resume-hint") {
                let (cols, _) = term.size();
                let (lines, _) = crate::app::transcript_lines(&self.vm, cols.max(1), &self.theme, &self.view);
                let mut out = String::new();
                for line in lines {
                    out.push_str(&line);
                    out.push_str("\x1b[0m\r\n");
                }
                term.write(out.as_bytes());
            }
        }
        term.flush();
        self.handle
    }
}

fn foreground_running(vm: &ViewModel, handle_running: bool, preloading: bool, drafting: bool) -> bool {
    vm.status.activity != Activity::Idle
        // A foreground `!cmd` / `!!cmd` remains active until its stored entry closes the live
        // block. It claims the handle directly, without starting the agent run gate.
        || vm
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Bash { running: true, .. }))
        || (handle_running && !preloading && !drafting)
}

fn activity_refreshing(running: bool, stage_on: bool, stage_has_deadline: bool) -> bool {
    running || (stage_on && stage_has_deadline)
}

fn settle_worker_done(vm: &mut ViewModel, failed: bool) {
    if vm.status.activity != Activity::Idle {
        apply_event(vm, &AgentEvent::AgentSettled { aborted: false });
    }
    // A failed `user_bash` command has no stored entry to replace its live block.
    if failed {
        if let Some(Block::Bash { running, .. }) = vm
            .blocks
            .iter_mut()
            .find(|block| matches!(block, Block::Bash { running: true, .. }))
        {
            *running = false;
        }
    }
}

/// The search with its matches moved up by `first` lines (for highlighting a visible slice).
fn shift_search(search: &TranscriptSearch, first: usize) -> TranscriptSearch {
    let current = search.current.and_then(|i| search.matches.get(i).copied());
    let matches: Vec<crate::SearchMatch> = search
        .matches
        .iter()
        .filter(|m| m.line >= first)
        .map(|m| crate::SearchMatch {
            line: m.line - first,
            ..*m
        })
        .collect();
    let current = current.filter(|c| c.line >= first).and_then(|c| {
        matches
            .iter()
            .position(|m| m.line == c.line - first && m.col_start == c.col_start)
    });
    TranscriptSearch {
        query: search.query.clone(),
        matches,
        current,
    }
}

/// The part of `sel` inside the `count` lines from `first`, in slice coordinates.
fn shift_selection(sel: &crate::Selection, first: usize, count: usize) -> Option<crate::Selection> {
    let (start, end) = sel.ordered();
    if count == 0 || end.line < first || start.line >= first + count {
        return None;
    }
    let anchor = if start.line < first {
        crate::Cell { line: 0, col: 0 }
    } else {
        crate::Cell {
            line: start.line - first,
            col: start.col,
        }
    };
    let head = if end.line >= first + count {
        crate::Cell {
            line: count - 1,
            col: u16::MAX - 1,
        }
    } else {
        crate::Cell {
            line: end.line - first,
            col: end.col,
        }
    };
    Some(crate::Selection { anchor, head })
}

/// `true` when `tool` is an executable file in a directory of `PATH` (from the environment
/// snapshot, never the process environment).
fn on_path(env: &[(String, String)], tool: &str) -> bool {
    let Some(path) = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()) else {
        return false;
    };
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|dir| Path::new(dir).join(tool).is_file())
}

/// Write a private text file (0600), creating its directory.
fn write_private(path: &Path, text: &str) -> Result<(), OxaError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let io = |e: std::io::Error| OxaError::io(&format!("cannot write {}", path.display()), &e);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    file.write_all(text.as_bytes()).map_err(io)
}

#[cfg(test)]
mod running_tests {
    use super::*;

    fn bash(running: bool) -> Block {
        Block::Bash {
            command: "sleep 10".into(),
            output: String::new(),
            exit: None,
            excluded: false,
            running,
            expanded: false,
        }
    }

    /// `user_bash` claims the shared handle without starting the agent run gate, so its live
    /// block must keep Ctrl+C and Escape on the abort path until the stored entry closes it.
    #[test]
    fn live_user_bash_is_foreground_activity() {
        let mut vm = ViewModel {
            blocks: vec![bash(true)],
            ..Default::default()
        };
        assert!(foreground_running(&vm, false, false, false));

        vm.blocks[0] = bash(false);
        assert!(!foreground_running(&vm, false, false, false));
    }

    /// A worker error only belongs to the command the sequential worker just finished; a
    /// queued command must retain its live block for its own streamed output and result.
    #[test]
    fn failed_user_bash_leaves_queued_block_running() {
        let mut vm = ViewModel {
            blocks: vec![bash(true), bash(true)],
            ..Default::default()
        };

        settle_worker_done(&mut vm, true);

        assert!(matches!(vm.blocks[0], Block::Bash { running: false, .. }));
        assert!(matches!(vm.blocks[1], Block::Bash { running: true, .. }));
    }

    /// A worker error can precede the stored bash entry, but it still completes the command.
    #[test]
    fn failed_user_bash_closes_live_block() {
        let mut vm = ViewModel {
            blocks: vec![bash(true)],
            ..Default::default()
        };

        settle_worker_done(&mut vm, true);

        assert!(!foreground_running(&vm, false, false, false));
        assert!(matches!(vm.blocks[0], Block::Bash { running: false, .. }));
    }

    /// An active tool without a matching stage scene still needs periodic redraws so its elapsed
    /// time advances instead of freezing until another event arrives.
    #[test]
    fn running_activity_refreshes_without_a_stage_deadline() {
        assert!(activity_refreshing(true, true, false));
    }
}

// ---------------------------------------------------------------------------------------------
// The trust question before a session exists
// ---------------------------------------------------------------------------------------------

/// The user setting that lists the keys of "always allow" (#1512 S2); user settings only.
const APPROVAL_ALWAYS: &str = "oxa.approval.always";

/// #1512 S2: save `keys` to the user settings' `oxa.approval.always`, each key once. The list is
/// read from the user layer only, so a project's value is never copied into the user file.
pub fn append_always(
    settings: &mut Settings,
    dirs: &Dirs,
    _cwd: &Path,
    keys: &[String],
) -> Result<(), OxaError> {
    // Under the user file's write lock, merge into the list the FILE holds now: two sessions
    // saving at once keep each other's keys (#1772).
    settings.update_user(dirs, APPROVAL_ALWAYS, |current| {
        let mut list: Vec<String> = current
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for key in keys {
            if !list.contains(key) {
                list.push(key.clone());
            }
        }
        Ok(Value::Array(list.into_iter().map(Value::String).collect()))
    })
}

/// The project trust question (D10 `ask_trust`'s question) drawn directly on `term`, for the
/// one question asked before the session (and with it the app loop) exists. `None` when
/// dismissed (Escape, Ctrl+C, Ctrl+D): untrusted for this run, asked again next time.
/// Terminal loss is an error, so callers cannot continue startup after the terminal disappears.
pub fn ask_trust_on(
    term: &mut dyn Terminal,
    keymap: &Keymap,
    theme: &Theme,
    cwd: &str,
    gated: &[String],
) -> Result<Option<TrustChoice>, OxaError> {
    let (title, options, choices) = trust_question(cwd, gated);
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = mpsc::channel();
    host.handle(
        UiRequest::Select {
            title,
            options,
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        now_ms(),
    );
    let mut renderer = Renderer::new(ScreenMode::Main);
    let answer = 'dialog: loop {
        match rx.try_recv() {
            Ok(answer) => break Ok(answer),
            Err(TryRecvError::Disconnected) => break Ok(None),
            Err(TryRecvError::Empty) => {}
        }
        let (cols, _) = term.size();
        let lines = host.render(cols.max(1), theme);
        renderer.draw(&Frame { lines, cursor: None }, term);
        term.flush();
        for ev in term.poll(250) {
            if ev == InputEvent::TerminalClosed {
                host.close_all();
                break 'dialog Err(OxaError::new(
                    oxa_proto::codes::TERMINAL_CLOSED,
                    "terminal disconnected",
                ));
            }
            if let InputEvent::Key(key) = ev {
                // Ctrl+C and Ctrl+D leave the question unanswered.
                if keymap.matches(&key.id, "app.clear") || keymap.matches(&key.id, "app.exit") {
                    host.close_all();
                } else {
                    host.key(&key, keymap);
                }
            }
        }
    };
    renderer.clear(term);
    term.flush();
    answer.map(|answer| answer.and_then(|i| choices.get(i).copied()))
}

/// [`ask_trust_on`] on the process terminal (raw mode for the time of the question).
/// Errors: `invalid` when stdin/stdout is not a terminal; `io`.
pub fn ask_trust_interactive(
    env: &[(String, String)],
    keymap: &Keymap,
    cwd: &str,
    gated: &[String],
) -> Result<Option<TrustChoice>, OxaError> {
    let caps = detect_caps(env);
    let mut term = ProcessTerminal::open(TermOpts {
        caps,
        ..TermOpts::default()
    })?;
    let theme = resolve_theme("dark", &[], term.caps().color, None).unwrap_or_else(|_| Theme::plain());
    let choice = ask_trust_on(&mut term, keymap, &theme, cwd, gated)?;
    drop(term);
    Ok(choice)
}

/// The leaf as the navigator shows it: usage records after a reply are never shown (pi has no
/// such entries), so the reply they follow counts as the current point.
fn shown_leaf(tree: &Tree) -> String {
    let mut id = tree.leaf().to_string();
    while let Some(e) = tree.get(&id) {
        match (&e.body, &e.parent) {
            (oxa_proto::EntryBody::Usage { .. }, Some(parent)) => id = parent.clone(),
            _ => break,
        }
    }
    id
}

/// pi's double-Escape window (interactive-mode.ts `now - lastEscapeTime < 500`).
const DOUBLE_ESCAPE_MS: u64 = 500;

/// One Escape on an empty, idle editor: `(selector to open, new last-escape time)`. `action` is
/// `doubleEscapeAction` (`tree` / `fork` / `none`; anything else counts as the default `tree`).
fn double_escape_step(action: &str, last_ms: u64, now: u64) -> (Option<SelectorKind>, u64) {
    let kind = match action {
        "none" => return (None, last_ms),
        "fork" => SelectorKind::ForkPoint,
        _ => SelectorKind::Tree,
    };
    if last_ms != 0 && now.saturating_sub(last_ms) < DOUBLE_ESCAPE_MS {
        (Some(kind), 0)
    } else {
        (None, now)
    }
}

/// The prompt text Enter sends for the editor's expanded text (`on_submit`); the draft prefill
/// sends the same (V020-OPTIMIZE #13).
fn submitted_text(expanded: &str) -> String {
    expanded.trim().to_string()
}

/// A string setting, or its schema default (`oxa_config::schema()`) when unset or not a string.
fn setting_or_default(settings: &oxa_config::Settings, key: &str) -> String {
    settings.string(key).unwrap_or_else(|| {
        oxa_config::schema()
            .iter()
            .find(|k| k.key == key)
            .and_then(|k| serde_json::from_str::<String>(k.default).ok())
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod double_escape_tests {
    use super::*;

    #[test]
    fn three_actions_and_the_window_edge() {
        // tree (default): the first Escape arms, a second within 500 ms opens /tree.
        assert_eq!(double_escape_step("tree", 0, 10_000), (None, 10_000));
        assert_eq!(
            double_escape_step("tree", 10_000, 10_499),
            (Some(SelectorKind::Tree), 0)
        );
        // The edge: 500 ms is too late (pi: `< 500`); it re-arms.
        assert_eq!(double_escape_step("tree", 10_000, 10_500), (None, 10_500));
        // fork opens the fork-point picker.
        assert_eq!(
            double_escape_step("fork", 10_000, 10_100),
            (Some(SelectorKind::ForkPoint), 0)
        );
        // none: nothing, ever.
        assert_eq!(double_escape_step("none", 0, 10_000), (None, 0));
        assert_eq!(double_escape_step("none", 10_000, 10_100), (None, 10_000));
        // An unknown value is the default.
        assert_eq!(
            double_escape_step("bogus", 10_000, 10_100),
            (Some(SelectorKind::Tree), 0)
        );
    }

    #[test]
    fn the_setting_falls_back_to_the_schema_default() {
        let unset = oxa_config::Settings::from_values(serde_json::json!({}), serde_json::json!({}));
        assert_eq!(setting_or_default(&unset, "doubleEscapeAction"), "tree");
        let fork = oxa_config::Settings::from_values(
            serde_json::json!({"doubleEscapeAction": "fork"}),
            serde_json::json!({}),
        );
        assert_eq!(setting_or_default(&fork, "doubleEscapeAction"), "fork");
    }
}

#[cfg(test)]
mod editor_layout_tests {
    use super::*;
    use serde_json::json;

    /// pi settings.md: `editorPaddingX` 0..3 (default 0), `autocompleteMaxVisible` 3..20
    /// (default 5).
    #[test]
    fn editor_layout_settings_are_read_and_clamped() {
        let of = |v: serde_json::Value| editor_opts_from(&oxa_config::Settings::from_values(v, json!({})));
        let d = of(json!({}));
        assert_eq!((d.padding_x, d.autocomplete_max_visible), (0, 5));
        let o = of(json!({ "editorPaddingX": 2, "autocompleteMaxVisible": 8 }));
        assert_eq!((o.padding_x, o.autocomplete_max_visible), (2, 8));
        let c = of(json!({ "editorPaddingX": 9, "autocompleteMaxVisible": 1 }));
        assert_eq!((c.padding_x, c.autocomplete_max_visible), (3, 3));
        assert_eq!(
            of(json!({ "autocompleteMaxVisible": 50 })).autocomplete_max_visible,
            20
        );
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// `fullscreenWheelScrollLines`: a number is fixed (clamped 1..100), anything else is auto.
    #[test]
    fn wheel_setting() {
        let s = |v: serde_json::Value| oxa_config::Settings::from_values(v, json!({}));
        assert_eq!(
            wheel_accel(&s(json!({ "fullscreenWheelScrollLines": 5 })), &env(&[])).next(1, 0),
            5
        );
        assert_eq!(
            wheel_accel(&s(json!({ "fullscreenWheelScrollLines": 500 })), &env(&[])).next(1, 0),
            100
        );
        assert_eq!(
            wheel_accel(&s(json!({ "fullscreenWheelScrollLines": "auto" })), &env(&[])).next(1, 0),
            1
        );
    }
}

#[cfg(test)]
mod block_cache_tests {
    use super::*;

    fn run(cache: &mut BlockCache, vm: &ViewModel) -> bool {
        let theme = oxa_tui::builtin_theme("dark", oxa_tui::ColorMode::Ansi256, None).unwrap();
        let env = ViewEnv {
            caps: TermCaps::BASIC,
            hooks: &[],
            keymap: None,
        };
        cache.update(vm, &[], 80, &theme, 0, &ViewSettings::default(), &env)
    }

    fn user(t: &str) -> Block {
        Block::User {
            text: t.into(),
            images: Vec::new(),
        }
    }

    /// A streamed delta in the last block must not reassemble the lines of the blocks before it.
    #[test]
    fn a_change_in_the_last_block_rebuilds_only_from_that_block() {
        let mut vm = ViewModel {
            blocks: vec![user("one"), user("two"), user("three")],
            ..Default::default()
        };
        let mut c = BlockCache::default();
        assert!(run(&mut c, &vm));
        vm.blocks[2] = user("three!");
        let before = c.content.clone();
        assert!(run(&mut c, &vm));
        assert_eq!(c.rebuilt_from, Some(2));
        assert_eq!(c.content[..c.starts[2]], before[..c.starts[2]]);
        assert!(c.content.iter().any(|l| l.contains("three!")));
        assert!(!run(&mut c, &vm));
    }

    /// Removing trailing blocks keeps the lines of the blocks before them.
    #[test]
    fn removing_blocks_rebuilds_from_the_first_removed_one() {
        let mut vm = ViewModel {
            blocks: vec![user("one"), user("two"), user("three")],
            ..Default::default()
        };
        let mut c = BlockCache::default();
        assert!(run(&mut c, &vm));
        vm.blocks.truncate(2);
        assert!(run(&mut c, &vm));
        assert_eq!(c.rebuilt_from, Some(2));
        assert_eq!(c.starts.len(), 2);
        assert!(!c.content.iter().any(|l| l.contains("three")));
        assert!(c.content.iter().any(|l| l.contains("two")));
    }

    /// A changed header shifts every block: everything is reassembled and the new header leads.
    #[test]
    fn a_header_change_rebuilds_everything() {
        let vm = ViewModel {
            blocks: vec![user("one"), user("two")],
            ..Default::default()
        };
        let theme = oxa_tui::builtin_theme("dark", oxa_tui::ColorMode::Ansi256, None).unwrap();
        let env = ViewEnv {
            caps: TermCaps::BASIC,
            hooks: &[],
            keymap: None,
        };
        let mut c = BlockCache::default();
        let s = ViewSettings::default();
        assert!(c.update(&vm, &[], 80, &theme, 0, &s, &env));
        let h = vec!["HEAD".to_string()];
        assert!(c.update(&vm, &h, 80, &theme, 0, &s, &env));
        assert_eq!(c.rebuilt_from, Some(0));
        assert_eq!(c.content[0], "HEAD");
        assert_eq!(c.starts[0], 1);
    }
}

#[cfg(test)]
mod catalog_tests;

#[cfg(test)]
mod export_tests;
