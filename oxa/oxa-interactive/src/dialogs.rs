//! D10: the `Ui` implementation of the TUI and its dialogs.
// ported from pi packages/coding-agent/src/modes/interactive/components/{extension-selector.ts,extension-input.ts,extension-editor.ts,trust-selector.ts,countdown-timer.ts}
use crate::{Block, UiRequest, ViewModel};
use oxa_config::Keymap;
use oxa_proto::{
    Access, Answer, ComponentAction, CustomOpts, DangerAnswer, KeyId, Level, Mode, Placement, ToolClass, Ui,
    UiComponent, Value,
};
use oxa_tui::{
    frame_lines, Component, CursorPos, Editor, EditorAction, EditorOpts, Input, InputAction, Interactive,
    Key, KeyKind, SelectAction, SelectItem, SelectList, Theme,
};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender};

/// Rows a choice list shows before it scrolls.
const MAX_VISIBLE: usize = 10;
/// The most preview rows a page shows. A short terminal shows fewer (see
/// [`DialogHost::render_in`]); page keys move through the rest of the preview.
const PREVIEW_ROWS: usize = 10;
/// The permission prompt's choices, in display order. Deny is the default focus so that a
/// stray Enter never grants access.
const ACCESS_LABELS: [&str; 3] = ["Allow once", "Allow for this session", "Deny"];
const ACCESS_ANSWERS: [Answer; 3] = [Answer::Once, Answer::Session, Answer::Deny];
/// #1512 S2: the dangerous-command prompt's choices, in display order. Deny is the default focus
/// (a stray Enter never runs a dangerous command); `Always` is saved to the user settings.
pub const DANGER_LABELS: [&str; 4] = [
    "Allow once",
    "Allow for this session",
    "Always allow (saved to your user settings)",
    "Deny",
];
pub const DANGER_ANSWERS: [DangerAnswer; 4] = [
    DangerAnswer::Once,
    DangerAnswer::Session,
    DangerAnswer::Always,
    DangerAnswer::Deny,
];
const HINT_CHOICE: &str = "up/down move  enter choose  esc cancel";
/// Dialog rows a fullscreen choice dialog spends around the preview page: the two frame
/// borders and the blank and hint rows. The choices and the row counter are counted apart.
const CHROME_ROWS: usize = 5;
const HINT_TEXT: &str = "enter submit  esc cancel";

/// The `oxa_proto::Ui` handed to `Session::prompt` on the worker thread: every call becomes a
/// [`UiRequest`] sent to the UI thread; calls with an answer block until the reply arrives.
///
/// If the UI thread is gone (send fails or the reply sender is dropped) dialogs return `None`
/// and `ask_access` returns `Answer::Deny`: never consent by accident.
pub struct UiProxy {
    to_ui: Sender<UiRequest>,
}

impl UiProxy {
    pub fn new(to_ui: Sender<UiRequest>) -> UiProxy {
        UiProxy { to_ui }
    }

    /// Send a request without waiting; `false` when the UI thread is gone.
    fn post(&self, req: UiRequest) -> bool {
        self.to_ui.send(req).is_ok()
    }
}

impl Ui for UiProxy {
    fn select(&mut self, title: &str, options: &[String], timeout_ms: Option<u32>) -> Option<usize> {
        let (reply, rx) = channel();
        let req = UiRequest::Select {
            title: title.to_string(),
            options: options.to_vec(),
            timeout_ms,
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn confirm(&mut self, title: &str, body: &str, timeout_ms: Option<u32>) -> Option<bool> {
        let (reply, rx) = channel();
        let req = UiRequest::Confirm {
            title: title.to_string(),
            body: body.to_string(),
            timeout_ms,
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn approve_publication(&mut self, title: &str, body: &str) -> Option<bool> {
        let (reply, rx) = channel();
        let req = UiRequest::ApprovePublication {
            title: title.to_string(),
            body: body.to_string(),
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn input(&mut self, title: &str, placeholder: &str) -> Option<String> {
        let (reply, rx) = channel();
        let req = UiRequest::Input {
            title: title.to_string(),
            placeholder: placeholder.to_string(),
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn secret(&mut self, title: &str, body: &str) -> Option<String> {
        let (reply, rx) = channel();
        let req = UiRequest::Secret {
            title: title.to_string(),
            body: body.to_string(),
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn editor(&mut self, title: &str, prefill: &str) -> Option<String> {
        let (reply, rx) = channel();
        let req = UiRequest::Editor {
            title: title.to_string(),
            prefill: prefill.to_string(),
            reply,
        };
        if !self.post(req) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.post(UiRequest::Notify {
            text: text.to_string(),
            level,
        });
    }
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.post(UiRequest::SetStatus {
            key: key.to_string(),
            text: text.map(str::to_string),
        });
    }
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: Placement) {
        self.post(UiRequest::SetWidget {
            key: key.to_string(),
            lines: lines.map(|l| l.to_vec()),
            at,
        });
    }
    fn set_title(&mut self, t: &str) {
        self.post(UiRequest::SetTitle(t.to_string()));
    }
    fn set_editor_text(&mut self, t: &str) {
        self.post(UiRequest::SetEditorText(t.to_string()));
    }
    fn ask_access(&mut self, a: &Access) -> Answer {
        let (reply, rx) = channel();
        if !self.post(UiRequest::AskAccess {
            access: a.clone(),
            reply,
        }) {
            return Answer::Deny;
        }
        rx.recv().unwrap_or(Answer::Deny)
    }
    fn ask_dangerous(&mut self, a: &Access, reasons: &[String], keys: &[String]) -> DangerAnswer {
        let (reply, rx) = channel();
        if !self.post(UiRequest::AskDangerous {
            access: a.clone(),
            reasons: reasons.to_vec(),
            keys: keys.to_vec(),
            reply,
        }) {
            return DangerAnswer::Deny;
        }
        rx.recv().unwrap_or(DangerAnswer::Deny)
    }
    fn editor_text(&mut self) -> String {
        let (reply, rx) = channel();
        if !self.post(UiRequest::EditorText { reply }) {
            return String::new();
        }
        rx.recv().unwrap_or_default()
    }
    fn set_footer(&mut self, lines: Option<&[String]>) {
        self.post(UiRequest::SetFooter(lines.map(|l| l.to_vec())));
    }
    fn set_header(&mut self, lines: Option<&[String]>) {
        self.post(UiRequest::SetHeader(lines.map(|l| l.to_vec())));
    }
    fn set_working_message(&mut self, text: Option<&str>) {
        self.post(UiRequest::SetWorkingMessage(text.map(str::to_string)));
    }
    fn custom(&mut self, c: Box<dyn UiComponent>, opts: CustomOpts) -> Option<Value> {
        let (reply, rx) = channel();
        if !self.post(UiRequest::Custom {
            component: c,
            opts,
            reply,
        }) {
            return None;
        }
        rx.recv().ok().flatten()
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// What the dialog host did with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    /// No dialog is open; the key is free.
    NotOpen,
    /// Consumed; redraw.
    Consumed,
    /// Consumed and the dialog closed (its reply was sent).
    Closed,
    /// The open editor dialog asks for the external editor (`app.editor.external`, Ctrl+G;
    /// D-INT). The app edits [`DialogHost::editor_text`] and puts the result back with
    /// [`DialogHost::set_editor_text`]; the dialog stays open.
    ExternalEditor,
}

/// Owns the open dialog (one at a time; further blocking requests queue in arrival order) and
/// applies the non-blocking requests to the view model.
///
/// Dialogs (acceptance, plan D10):
/// * **permission prompt** for `AskAccess`: shows what is asked (`Access` in plain words: the
///   path, the command, the host, the MCP tool) and three choices `Allow once` /
///   `Allow for this session` / `Deny` -> `Answer::{Once, Session, Deny}`; Escape = `Deny`;
/// * **select**, **confirm**, **input**, **editor** (multi-line, Ctrl+G opens the external
///   editor through the app) for the extension-UI calls; Escape = `None`;
/// * **timed dialogs**: with `timeout_ms` a countdown is shown and expiry answers `None`
///   ([`DialogHost::tick`]);
/// * **custom components**: keys go to `UiComponent::input`; `ComponentAction::Done(v)` answers
///   `Some(v)`, Escape answers `None`; `opts.overlay` draws it as an overlay.
/// * Closing the host (quit) answers every open and queued request with `None` / `Deny`.
pub struct DialogHost {
    open: Option<Dialog>,
    queue: VecDeque<Dialog>,
    /// The last clock value seen by `handle` or `tick`.
    clock: u64,
    /// Hardware cursor of the open text dialog as of the last `render`.
    cursor_at: Option<CursorPos>,
    /// Edge consumed by the app, not by hooks. Queued permissions count only on opening.
    permission_opened: bool,
    /// #1512 S2: an "always allow" answer waiting for the driver to save its keys and answer
    /// (see [`DialogHost::take_danger_always`]).
    danger_always: Option<(Vec<String>, Sender<DangerAnswer>)>,
}

impl DialogHost {
    pub fn new() -> DialogHost {
        DialogHost {
            open: None,
            queue: VecDeque::new(),
            clock: 0,
            cursor_at: None,
            permission_opened: false,
            danger_always: None,
        }
    }

    pub(crate) fn take_permission_opened(&mut self) -> bool {
        std::mem::take(&mut self.permission_opened)
    }

    /// #1512 S2: the keys and the reply of an "always allow" answer. The caller saves the keys
    /// and then answers the reply (the driver's `finish_danger_always`); until it does, the
    /// worker waits. `None` when no such answer is pending.
    pub fn take_danger_always(&mut self) -> Option<(Vec<String>, Sender<DangerAnswer>)> {
        self.danger_always.take()
    }

    /// Take a request: blocking ones open or queue a dialog; the others are applied to `vm`
    /// (`Notify` adds a `Block::Notice`, `SetStatus` edits `vm.status.ext_status`, ...).
    /// `SetEditorText` and `EditorText` are returned to the caller (the app owns the editor).
    pub fn handle(&mut self, req: UiRequest, vm: &mut ViewModel, now_ms: u64) -> Option<UiRequest> {
        self.clock = now_ms;
        match req {
            UiRequest::Select {
                title,
                options,
                timeout_ms,
                reply,
            } => {
                if options.is_empty() {
                    Reply::Index(reply).settle(None);
                    return None;
                }
                let list = choice_list(&options);
                let dialog = Dialog::choice(
                    title,
                    Vec::new(),
                    list,
                    ChoiceKind::Select,
                    timeout_ms,
                    Reply::Index(reply),
                );
                self.push(dialog, now_ms);
            }
            UiRequest::Confirm {
                title,
                body,
                timeout_ms,
                reply,
            } => {
                let list = choice_list(&["Yes".to_string(), "No".to_string()]);
                let lines: Vec<String> = body.lines().map(str::to_string).collect();
                let dialog = Dialog::choice(
                    title,
                    lines,
                    list,
                    ChoiceKind::Confirm,
                    timeout_ms,
                    Reply::Bool(reply),
                );
                self.push(dialog, now_ms);
            }
            UiRequest::ApprovePublication { title, body, reply } => {
                let mut list = choice_list(&["Approve".to_string(), "Deny".to_string()]);
                // Deny is focused by default: a stray Enter never publishes.
                list.set_selected_index(1);
                let lines: Vec<String> = body.lines().map(str::to_string).collect();
                let dialog =
                    Dialog::choice(title, lines, list, ChoiceKind::Approve, None, Reply::Bool(reply));
                self.push(dialog, now_ms);
            }
            UiRequest::Input {
                title,
                placeholder,
                reply,
            } => {
                let dialog = Dialog {
                    title,
                    body: Body::Text {
                        input: Input::new(&placeholder),
                    },
                    reply: Reply::Text(reply),
                    timeout_ms: None,
                    deadline: None,
                    last_secs: None,
                };
                self.push(dialog, now_ms);
            }
            UiRequest::Secret { title, body, reply } => {
                // The prompt is the requesting process's text: one sanitized line, shown as data
                // in the title row, with the typed characters masked.
                let prompt = body.lines().last().map(sanitize_dialog_text).unwrap_or_default();
                let mut input = Input::new(&prompt);
                input.set_masked(true);
                let dialog = Dialog {
                    title,
                    body: Body::Text { input },
                    reply: Reply::Text(reply),
                    timeout_ms: None,
                    deadline: None,
                    last_secs: None,
                };
                self.push(dialog, now_ms);
            }
            UiRequest::Editor {
                title,
                prefill,
                reply,
            } => {
                let mut editor = Editor::new(EditorOpts::default());
                editor.set_text(&prefill);
                editor.set_focused(true);
                let dialog = Dialog {
                    title,
                    body: Body::Editor {
                        editor: Box::new(editor),
                    },
                    reply: Reply::Text(reply),
                    timeout_ms: None,
                    deadline: None,
                    last_secs: None,
                };
                self.push(dialog, now_ms);
            }
            UiRequest::AskAccess { access, reply } => {
                let (title, body) = access_prompt(&access);
                let mut list = choice_list(&ACCESS_LABELS);
                // Deny is focused by default: a stray Enter never grants access.
                list.set_selected_index(ACCESS_ANSWERS.len() - 1);
                let dialog = Dialog::choice(
                    title,
                    vec![body],
                    list,
                    ChoiceKind::Access,
                    None,
                    Reply::Access(reply),
                );
                self.push(dialog, now_ms);
            }
            UiRequest::AskDangerous {
                access,
                reasons,
                keys,
                reply,
            } => {
                // The command and the reasons come from the model: strip escape sequences and
                // other control characters so they cannot rewrite the dialog around them.
                // Each shell line is its own dialog line, so a second command on a new line is visibly
                // a second line (#1772).
                let mut lines: Vec<String> = access_prompt(&access)
                    .1
                    .split('\n')
                    .map(sanitize_dialog_text)
                    .collect();
                lines.extend(reasons.iter().map(|r| format!("- {}", sanitize_dialog_text(r))));
                let mut list = choice_list(&DANGER_LABELS);
                // Deny is focused by default: a stray Enter never runs a dangerous command.
                list.set_selected_index(DANGER_ANSWERS.len() - 1);
                let dialog = Dialog::choice(
                    "Dangerous command".to_string(),
                    lines,
                    list,
                    ChoiceKind::Danger,
                    None,
                    Reply::Danger(reply, keys),
                );
                self.push(dialog, now_ms);
            }
            UiRequest::Custom {
                component,
                opts,
                reply,
            } => {
                let dialog = Dialog {
                    title: String::new(),
                    body: Body::Custom {
                        component,
                        overlay: opts.overlay,
                    },
                    reply: Reply::Value(reply),
                    timeout_ms: None,
                    deadline: None,
                    last_secs: None,
                };
                self.push(dialog, now_ms);
            }
            UiRequest::EditorText { reply } => return Some(UiRequest::EditorText { reply }),
            UiRequest::SetEditorText(t) => return Some(UiRequest::SetEditorText(t)),
            UiRequest::Notify { text, level } => vm.blocks.push(Block::Notice { level, text }),
            UiRequest::SetStatus { key, text } => match text {
                Some(t) => {
                    vm.status.ext_status.insert(key, t);
                }
                None => {
                    vm.status.ext_status.remove(&key);
                }
            },
            UiRequest::SetWidget { key, lines, at } => {
                let map = match at {
                    Placement::AboveEditor => &mut vm.widgets_above,
                    Placement::BelowEditor => &mut vm.widgets_below,
                };
                match lines {
                    Some(l) => {
                        map.insert(key, l);
                    }
                    None => {
                        map.remove(&key);
                    }
                }
            }
            UiRequest::SetTitle(t) => vm.title_override = Some(t),
            UiRequest::SetFooter(l) => vm.footer_override = l,
            UiRequest::SetHeader(l) => vm.header_override = l,
            UiRequest::SetWorkingMessage(t) => vm.status.working_message = t,
        }
        None
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// `true` when the open dialog is an overlay (drawn over the transcript) rather than a
    /// replacement of the editor area.
    pub fn is_overlay(&self) -> bool {
        matches!(
            self.open.as_ref().map(|d| &d.body),
            Some(Body::Custom { overlay: true, .. })
        )
    }

    pub fn key(&mut self, key: &Key, keymap: &Keymap) -> DialogKey {
        let Some(dialog) = self.open.as_mut() else {
            return DialogKey::NotOpen;
        };
        if key.kind == KeyKind::Release {
            return DialogKey::Consumed;
        }
        if matches!(dialog.body, Body::Editor { .. }) && keymap.matches(&key.id, "app.editor.external") {
            return DialogKey::ExternalEditor;
        }
        match dialog.key(key, keymap) {
            KeyFlow::Pending => DialogKey::Consumed,
            KeyFlow::Settle(out) => {
                self.close_open(out);
                DialogKey::Closed
            }
        }
    }

    /// The text of the open editor dialog (`None` when the open dialog is not an editor).
    pub fn editor_text(&self) -> Option<String> {
        match self.open.as_ref().map(|d| &d.body) {
            Some(Body::Editor { editor }) => Some(editor.expanded_text()),
            _ => None,
        }
    }

    /// Replace the text of the open editor dialog; `false` when there is none.
    pub fn set_editor_text(&mut self, text: &str) -> bool {
        match self.open.as_mut().map(|d| &mut d.body) {
            Some(Body::Editor { editor }) => {
                editor.set_text(text);
                true
            }
            _ => false,
        }
    }

    pub fn paste(&mut self, text: &str) -> bool {
        match self.open.as_mut().map(|d| &mut d.body) {
            Some(Body::Text { input }) => input.paste(text),
            Some(Body::Editor { editor }) => editor.paste(text),
            _ => false,
        }
    }

    /// Advance countdowns; `true` when the display changed or a dialog timed out.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        self.clock = now_ms;
        let Some(dialog) = self.open.as_mut() else {
            return false;
        };
        if let Some(deadline) = dialog.deadline {
            if now_ms >= deadline {
                self.close_open(None);
                return true;
            }
        }
        let secs = dialog.secs_left(now_ms);
        let changed = secs != dialog.last_secs;
        dialog.last_secs = secs;
        changed
    }

    /// Lines of the open dialog (empty when none), without a viewport bound.
    pub fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        self.render_in(width, None, theme)
    }

    /// Lines of the open dialog (empty when none) when the terminal has `height` rows for it.
    ///
    /// The fullscreen layout keeps only the last terminal rows, so a dialog taller than the
    /// screen loses its head. A paged preview sizes its page to the rows the terminal can
    /// actually show; page keys move through the rest of it.
    pub fn render_in(&mut self, width: u16, height: Option<usize>, theme: &Theme) -> Vec<String> {
        self.cursor_at = None;
        let clock = self.clock;
        let Some(dialog) = self.open.as_mut() else {
            return Vec::new();
        };
        let inner = width.saturating_sub(4).max(1);
        let secs = dialog.secs_left(clock);
        let title = match secs {
            Some(s) => format!("{} ({s}s)", dialog.title),
            None => dialog.title.clone(),
        };
        let mut header = Some(title.clone());
        let mut body: Vec<String> = Vec::new();
        let mut cursor: Option<(usize, u16)> = None;
        // A preview on a terminal too short for the comfortable chrome drops its blank
        // separators, its row counter and, at the smallest sizes, its frame and hint. The
        // command and the choices stay on screen.
        let mut compact = false;
        let mut show_hint = true;
        let mut unframed = false;
        // Dialog rows the terminal can show: its height, or none for an unbounded draw.
        let available = height.map(|h| h.max(1));
        match &mut dialog.body {
            Body::Choice {
                lines,
                list,
                kind,
                scroll,
                page,
            } => {
                // The choices are drawn below the preview; their row count bounds its page.
                let choices = list.render(inner, theme);
                match kind {
                    // A select has no body text: its title is the question, wrapped.
                    ChoiceKind::Select => {
                        header = None;
                        body.extend(wrap(&title, inner as usize));
                    }
                    // Publication and command text is shown exactly as it will be published
                    // or run: no word re-flow. Page keys move through the rest of it.
                    ChoiceKind::Approve | ChoiceKind::Danger => {
                        let mut preview = Vec::new();
                        for line in lines.iter() {
                            preview.extend(hard_wrap(line, inner as usize));
                        }
                        // A page is at most the preview's cap and never more than the rows the
                        // terminal has left once the list, the hint, the frame and the row
                        // counter are drawn. A terminal too short for that layout keeps one
                        // command row plus the choices: it drops the blanks and the counter,
                        // then the hint, then the frame, so the command never falls off the top
                        // while the choices stay visible.
                        let choices_rows = choices.len();
                        let full_fixed = CHROME_ROWS + choices_rows + 1;
                        compact = available.is_some_and(|h| h < full_fixed + 1);
                        let fixed = if compact {
                            let chrome = 2 + choices_rows;
                            unframed = available.is_some_and(|h| h < chrome + 1);
                            show_hint = available.is_some_and(|h| h >= chrome + 2);
                            (if unframed { choices_rows } else { chrome }) + usize::from(show_hint)
                        } else {
                            full_fixed
                        };
                        let cap = available.map_or(PREVIEW_ROWS, |h| h.saturating_sub(fixed));
                        *page = cap.clamp(1, PREVIEW_ROWS);
                        let max_scroll = preview.len().saturating_sub(*page);
                        *scroll = (*scroll).min(max_scroll);
                        let end = (*scroll + *page).min(preview.len());
                        body.extend(preview[*scroll..end].iter().cloned());
                        if !compact && preview.len() > *page {
                            body.push(theme.fg(
                                "muted",
                                &format!(
                                    "rows {}–{} of {}  pageup/pagedown",
                                    *scroll + 1,
                                    end,
                                    preview.len()
                                ),
                            ));
                        }
                    }
                    _ => {
                        for line in lines.iter() {
                            body.extend(wrap(line, inner as usize));
                        }
                    }
                }
                if !compact {
                    body.push(String::new());
                }
                body.extend(choices);
                if !compact {
                    body.push(String::new());
                }
                if show_hint {
                    body.push(theme.fg("muted", HINT_CHOICE));
                }
            }
            Body::Text { input } => {
                let row = body.len();
                body.extend(input.render(inner, theme));
                if let Some(p) = input.cursor() {
                    cursor = Some((row + p.row, p.col));
                }
                body.push(String::new());
                body.push(theme.fg("muted", HINT_TEXT));
            }
            Body::Editor { editor } => {
                let row = body.len();
                body.extend(editor.render(inner, theme));
                if let Some(p) = editor.cursor() {
                    cursor = Some((row + p.row, p.col));
                }
                body.push(String::new());
                body.push(theme.fg("muted", HINT_TEXT));
            }
            Body::Custom { component, overlay } => {
                let lines = component.render(inner);
                if !*overlay {
                    // Replaces the editor area: the component draws itself, unframed.
                    return lines;
                }
                body = lines;
                header = None;
            }
        }
        if unframed {
            return body;
        }
        let framed = frame_lines(&body, width, header.as_deref(), "borderAccent", theme);
        self.cursor_at = cursor.map(|(row, col)| CursorPos {
            // One row for the top edge, two columns for `│ `.
            row: row + 1,
            col: col + 2,
        });
        framed
    }

    pub fn cursor(&self) -> Option<CursorPos> {
        self.cursor_at
    }

    /// Answer everything with `None` / `Deny` and close.
    pub fn close_all(&mut self) {
        if let Some(dialog) = self.open.take() {
            dialog.reply.settle(None);
        }
        while let Some(dialog) = self.queue.pop_front() {
            dialog.reply.settle(None);
        }
        if let Some((_, tx)) = self.danger_always.take() {
            let _ = tx.send(DangerAnswer::Deny);
        }
        self.cursor_at = None;
        self.permission_opened = false;
    }

    /// Open `dialog` now, or queue it behind the open one.
    fn push(&mut self, mut dialog: Dialog, now_ms: u64) {
        if self.open.is_none() {
            dialog.arm(now_ms);
            self.permission_opened |= matches!(dialog.reply, Reply::Access(_) | Reply::Danger(..));
            self.open = Some(dialog);
        } else {
            self.queue.push_back(dialog);
        }
    }

    /// Close the open dialog with `out` (`None` = dismissed) and open the next queued one.
    fn close_open(&mut self, out: Option<Outcome>) {
        if let Some(dialog) = self.open.take() {
            match (dialog.reply, out) {
                // The driver saves the keys first; it answers through `take_danger_always`.
                (Reply::Danger(tx, keys), Some(Outcome::Danger(DangerAnswer::Always))) => {
                    self.danger_always = Some((keys, tx));
                }
                (reply, out) => reply.settle(out),
            }
        }
        if let Some(mut next) = self.queue.pop_front() {
            next.arm(self.clock);
            self.permission_opened |= matches!(next.reply, Reply::Access(_) | Reply::Danger(..));
            self.open = Some(next);
        }
        self.cursor_at = None;
    }
}

impl Default for DialogHost {
    fn default() -> Self {
        DialogHost::new()
    }
}

impl Drop for DialogHost {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// The permission prompt text for an access, in plain words (also used by RPC's `confirm`
/// relay so both front ends ask the same question). Deterministic.
/// `Write(/w/a.rs)` -> title `Allow write?`, body `Write to /w/a.rs`.
pub fn access_prompt(a: &Access) -> (String, String) {
    match a {
        Access::Read(p) => ("Allow read?".to_string(), format!("Read {}", p.display())),
        Access::Write(p) => ("Allow write?".to_string(), format!("Write to {}", p.display())),
        Access::Exec { command, net } => {
            let scope = if *net { "network on" } else { "no network" };
            ("Allow command?".to_string(), format!("Run {command} ({scope})"))
        }
        Access::Net { host } => ("Allow network?".to_string(), format!("Connect to {host}")),
        Access::Mcp { server, tool, class } => (
            "Allow MCP tool?".to_string(),
            format!("Call {server}:{tool} ({} class)", class_word(*class)),
        ),
    }
}

fn class_word(class: ToolClass) -> &'static str {
    match class {
        ToolClass::Read => "read",
        ToolClass::Write => "write",
        ToolClass::Exec => "exec",
        ToolClass::Net => "network",
    }
}

/// The choice made in the project trust dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoice {
    /// Trust and remember for this directory.
    Trust,
    /// Trust the parent directory (and everything below it).
    TrustParent,
    /// Do not trust (project resources are not loaded), remembered.
    NoTrust,
    /// Decide later: untrusted for this run, ask again next time.
    Skip,
}

/// Run the trust question before the session starts (pi's trust selector): lists the gated
/// resources found in the project (`gated`: settings, extensions, hooks, MCP servers, ...).
/// `None` when dismissed (= `Skip`). Drawn with `ui.select`, so it also works over RPC.
pub fn ask_trust(ui: &mut dyn Ui, cwd: &str, gated: &[String]) -> Option<TrustChoice> {
    let (title, options, choices) = trust_question(cwd, gated);
    ui.select(&title, &options, None)
        .and_then(|i| choices.get(i).copied())
}

/// The trust question as data: the title, the option labels and the choice of each option
/// (the app's `/trust` shows it through its own dialog host).
pub(crate) fn trust_question(cwd: &str, gated: &[String]) -> (String, Vec<String>, Vec<TrustChoice>) {
    let mut options = vec!["Trust this project".to_string()];
    let mut choices = vec![TrustChoice::Trust];
    let parent = std::path::Path::new(cwd)
        .parent()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty());
    if let Some(parent) = parent {
        options.push(format!("Trust parent directory {parent} and projects below it"));
        choices.push(TrustChoice::TrustParent);
    }
    options.push("Do not trust (remember)".to_string());
    choices.push(TrustChoice::NoTrust);
    options.push("Decide later".to_string());
    choices.push(TrustChoice::Skip);

    let gated_text = if gated.is_empty() {
        "none".to_string()
    } else {
        gated.join(", ")
    };
    let title = format!("Project trust: {cwd}. Gated resources: {gated_text}");
    (title, options, choices)
}

/// The choice when the session's cwd no longer exists (pi's missing-cwd dialog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingCwdChoice {
    /// Continue the session in the current directory.
    Continue,
    /// Fork the session into the current directory.
    Fork,
    Cancel,
}

/// Ask what to do about a missing session cwd. Dismissed = `Cancel`.
pub fn ask_missing_cwd(ui: &mut dyn Ui, session_cwd: &str, current_cwd: &str) -> MissingCwdChoice {
    let title = format!("Session directory is missing: {session_cwd}");
    let options = [
        format!("Continue in {current_cwd}"),
        format!("Fork the session into {current_cwd}"),
        "Cancel".to_string(),
    ];
    match ui.select(&title, &options, None) {
        Some(0) => MissingCwdChoice::Continue,
        Some(1) => MissingCwdChoice::Fork,
        _ => MissingCwdChoice::Cancel,
    }
}

/// The mode after pressing the `app.mode.cycle` key, and the notice text shown for it
/// (`Mode: plan (read-only)`). Display only: the mode never reaches the wire (W3).
pub fn cycle_mode(current: Mode) -> (Mode, String) {
    let next = current.next();
    let text = match next {
        Mode::Plan => "Mode: plan (read-only)",
        Mode::Workspace => "Mode: workspace (writes inside cwd; the rest asks)",
        Mode::Ask => "Mode: ask (each write or command asks once per scope)",
    };
    (next, text.to_string())
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

fn choice_list(labels: &[impl AsRef<str>]) -> SelectList {
    let items = labels
        .iter()
        .enumerate()
        .map(|(i, label)| SelectItem {
            value: i.to_string(),
            label: label.as_ref().to_string(),
            description: None,
        })
        .collect();
    SelectList::new(items, MAX_VISIBLE)
}

/// Cut a line at `width` columns without touching its spaces, so the shown text is the exact
/// text (an empty line stays one empty row).
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for ch in text.chars() {
        let mut next = line.clone();
        next.push(ch);
        if !line.is_empty() && oxa_tui::visible_width(&next) > width {
            out.push(std::mem::take(&mut line));
        }
        line.push(ch);
    }
    out.push(line);
    out
}

/// Greedy word wrap by visible width. Words wider than `width` stay whole (the frame cuts them).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if oxa_tui::visible_width(&line) + 1 + oxa_tui::visible_width(word) <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
        }
        out.push(line);
    }
    out
}

/// What a dialog answers with. `None` in [`Reply::settle`] is a dismissal.
enum Outcome {
    Index(usize),
    Bool(bool),
    Text(String),
    Access(Answer),
    Danger(DangerAnswer),
    Value(Value),
}

/// The typed reply channel of a blocking request.
enum Reply {
    Index(Sender<Option<usize>>),
    Bool(Sender<Option<bool>>),
    Text(Sender<Option<String>>),
    Access(Sender<Answer>),
    /// The dangerous-command answer and the rule keys it covers.
    Danger(Sender<DangerAnswer>, Vec<String>),
    Value(Sender<Option<Value>>),
}

impl Reply {
    /// Send the outcome; a dismissal (`None`) is `None` / `Deny`. A dropped receiver is ignored.
    fn settle(self, out: Option<Outcome>) {
        match (self, out) {
            (Reply::Index(tx), Some(Outcome::Index(i))) => {
                let _ = tx.send(Some(i));
            }
            (Reply::Index(tx), _) => {
                let _ = tx.send(None);
            }
            (Reply::Bool(tx), Some(Outcome::Bool(b))) => {
                let _ = tx.send(Some(b));
            }
            (Reply::Bool(tx), _) => {
                let _ = tx.send(None);
            }
            (Reply::Text(tx), Some(Outcome::Text(s))) => {
                let _ = tx.send(Some(s));
            }
            (Reply::Text(tx), _) => {
                let _ = tx.send(None);
            }
            (Reply::Access(tx), Some(Outcome::Access(a))) => {
                let _ = tx.send(a);
            }
            (Reply::Access(tx), _) => {
                let _ = tx.send(Answer::Deny);
            }
            (Reply::Danger(tx, _), Some(Outcome::Danger(a))) => {
                let _ = tx.send(a);
            }
            (Reply::Danger(tx, _), _) => {
                let _ = tx.send(DangerAnswer::Deny);
            }
            (Reply::Value(tx), Some(Outcome::Value(v))) => {
                let _ = tx.send(Some(v));
            }
            (Reply::Value(tx), _) => {
                let _ = tx.send(None);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ChoiceKind {
    Select,
    Confirm,
    /// Publication approval: Approve is index 0, the exact text is shown unwrapped-by-words.
    Approve,
    Access,
    /// Dangerous-command approval: Deny is the last choice; the command is paged for review
    /// like a publication preview.
    Danger,
}

impl ChoiceKind {
    fn outcome(self, i: usize) -> Outcome {
        match self {
            ChoiceKind::Select => Outcome::Index(i),
            ChoiceKind::Confirm | ChoiceKind::Approve => Outcome::Bool(i == 0),
            ChoiceKind::Access => Outcome::Access(ACCESS_ANSWERS.get(i).copied().unwrap_or(Answer::Deny)),
            ChoiceKind::Danger => {
                Outcome::Danger(DANGER_ANSWERS.get(i).copied().unwrap_or(DangerAnswer::Deny))
            }
        }
    }
}

enum Body {
    Choice {
        /// Body text above the list (confirm and permission prompts).
        lines: Vec<String>,
        list: Box<SelectList>,
        kind: ChoiceKind,
        /// Wrapped-row offset of the paged preview.
        scroll: usize,
        /// Preview rows the last draw showed in one page.
        page: usize,
    },
    Text {
        input: Input,
    },
    Editor {
        editor: Box<Editor>,
    },
    Custom {
        component: Box<dyn UiComponent>,
        overlay: bool,
    },
}

enum KeyFlow {
    Pending,
    /// Close with this outcome (`None` = dismissed).
    Settle(Option<Outcome>),
}

struct Dialog {
    title: String,
    body: Body,
    reply: Reply,
    timeout_ms: Option<u32>,
    /// Absolute expiry on the host clock; set when the dialog opens.
    deadline: Option<u64>,
    /// Seconds shown at the last tick, to report display changes.
    last_secs: Option<u64>,
}

impl Dialog {
    fn choice(
        title: String,
        lines: Vec<String>,
        list: SelectList,
        kind: ChoiceKind,
        timeout_ms: Option<u32>,
        reply: Reply,
    ) -> Dialog {
        Dialog {
            title,
            body: Body::Choice {
                lines,
                list: Box::new(list),
                kind,
                scroll: 0,
                page: PREVIEW_ROWS,
            },
            reply,
            timeout_ms,
            deadline: None,
            last_secs: None,
        }
    }

    /// Start the countdown (a timeout of 0 means none, as in pi).
    fn arm(&mut self, now_ms: u64) {
        self.deadline = self
            .timeout_ms
            .filter(|&ms| ms > 0)
            .map(|ms| now_ms + u64::from(ms));
        self.last_secs = self.secs_left(now_ms);
    }

    /// Whole seconds left, rounded up (pi's countdown), `None` without a timeout.
    fn secs_left(&self, now_ms: u64) -> Option<u64> {
        self.deadline.map(|d| {
            let rem = d.saturating_sub(now_ms);
            rem.div_ceil(1000)
        })
    }

    fn key(&mut self, key: &Key, keymap: &Keymap) -> KeyFlow {
        let cancel = keymap.matches(&key.id, "tui.select.cancel");
        match &mut self.body {
            Body::Choice {
                list,
                kind,
                scroll,
                page,
                ..
            } => {
                if matches!(kind, ChoiceKind::Approve | ChoiceKind::Danger) && key.is("pagedown") {
                    *scroll = scroll.saturating_add(*page);
                    return KeyFlow::Pending;
                }
                if matches!(kind, ChoiceKind::Approve | ChoiceKind::Danger) && key.is("pageup") {
                    *scroll = scroll.saturating_sub(*page);
                    return KeyFlow::Pending;
                }
                match list.key(key, keymap) {
                    // Denial is always available; approval needs no separate review.
                    SelectAction::Selected(value) => {
                        KeyFlow::Settle(value.parse::<usize>().ok().map(|i| kind.outcome(i)))
                    }
                    SelectAction::Cancelled => KeyFlow::Settle(None),
                    SelectAction::Moved => KeyFlow::Pending,
                    SelectAction::Ignored if cancel => KeyFlow::Settle(None),
                    SelectAction::Ignored => KeyFlow::Pending,
                }
            }
            Body::Text { input } => match input.key(key, keymap) {
                InputAction::Submit(v) => KeyFlow::Settle(Some(Outcome::Text(v))),
                InputAction::Cancelled => KeyFlow::Settle(None),
                InputAction::Ignored if cancel => KeyFlow::Settle(None),
                _ => KeyFlow::Pending,
            },
            Body::Editor { editor } => match editor.key(key, keymap) {
                EditorAction::Submit(v) => KeyFlow::Settle(Some(Outcome::Text(v))),
                EditorAction::Ignored if cancel => KeyFlow::Settle(None),
                _ => KeyFlow::Pending,
            },
            Body::Custom { component, .. } => {
                let id: &KeyId = &key.id;
                match component.input(id, key.text.as_deref()) {
                    ComponentAction::Done(v) => KeyFlow::Settle(Some(Outcome::Value(v))),
                    ComponentAction::Ignored if cancel => KeyFlow::Settle(None),
                    _ => KeyFlow::Pending,
                }
            }
        }
    }
}

/// Dialog text from model input with escape sequences and other control characters removed.
fn sanitize_dialog_text(text: &str) -> String {
    oxa_tui::strip_ansi(text)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn a_dangerous_command_cannot_rewrite_the_dialog() {
        let hostile = "ls \u{1b}[2J\u{1b}[1;1HApproved\r\u{7}# \u{1b}[31m";
        let clean = sanitize_dialog_text(hostile);
        assert!(!clean.contains('\u{1b}'), "{clean:?}");
        assert!(!clean.chars().any(|c| c.is_control()), "{clean:?}");
        assert!(clean.contains("Approved"), "{clean:?}");
    }

    #[test]
    fn queued_permissions_notify_only_when_opened_once_each() {
        let mut host = DialogHost::new();
        let mut vm = ViewModel::default();
        let (reply, _rx) = channel();
        host.handle(
            UiRequest::Confirm {
                title: "Question".into(),
                body: String::new(),
                timeout_ms: None,
                reply,
            },
            &mut vm,
            0,
        );
        for _ in 0..2 {
            let (reply, _rx) = channel();
            host.handle(
                UiRequest::AskAccess {
                    access: Access::Write(std::path::PathBuf::from("/project/file")),
                    reply,
                },
                &mut vm,
                0,
            );
        }
        assert!(!host.take_permission_opened(), "queued is not open");
        host.key(&Key::press("escape"), &Keymap::defaults());
        assert!(host.take_permission_opened());
        assert!(!host.take_permission_opened(), "no repeat on redraw");
        host.key(&Key::press("escape"), &Keymap::defaults());
        assert!(host.take_permission_opened(), "next queued permission");
        host.close_all();
        assert!(!host.take_permission_opened());
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    fn open(host: &mut DialogHost) -> std::sync::mpsc::Receiver<Option<bool>> {
        let (reply, rx) = channel();
        host.handle(
            UiRequest::ApprovePublication {
                title: "Publish?".into(),
                body: "Title: A  B\n    indented".into(),
                reply,
            },
            &mut ViewModel::default(),
            0,
        );
        rx
    }

    #[test]
    fn deny_is_focused_and_the_text_is_shown_exactly() {
        let mut host = DialogHost::new();
        let rx = open(&mut host);
        let shown = host.render(80, &Theme::plain()).join("\n");
        assert!(
            shown.contains("Title: A  B") && shown.contains("    indented"),
            "{shown}"
        );
        host.key(&Key::press("enter"), &Keymap::defaults());
        assert_eq!(rx.recv().unwrap(), Some(false), "a stray Enter denies");
    }

    #[test]
    fn up_then_enter_approves_and_escape_dismisses() {
        let mut host = DialogHost::new();
        let rx = open(&mut host);
        host.render(80, &Theme::plain());
        host.key(&Key::press("up"), &Keymap::defaults());
        host.key(&Key::press("enter"), &Keymap::defaults());
        assert_eq!(rx.recv().unwrap(), Some(true));
        let rx = open(&mut host);
        host.key(&Key::press("escape"), &Keymap::defaults());
        assert_eq!(rx.recv().unwrap(), None);
    }

    #[test]
    fn long_lines_are_cut_not_reflowed() {
        assert_eq!(hard_wrap("ab  cd", 4), vec!["ab  ".to_string(), "cd".to_string()]);
        assert_eq!(hard_wrap("", 4), vec![String::new()]);
    }

    #[test]
    fn publication_approves_from_the_first_page() {
        let mut host = DialogHost::new();
        let rx = open_publication(&mut host, 24);
        let first = host.render(80, &Theme::plain()).join("\n");
        assert!(first.contains("line 1") && !first.contains("line 24"), "{first}");
        host.key(&Key::press("up"), &Keymap::defaults());
        host.key(&Key::press("enter"), &Keymap::defaults());
        assert_eq!(rx.recv().unwrap(), Some(true));
    }

    #[test]
    fn publication_paging_shows_the_rest_without_gating_approval() {
        let mut host = DialogHost::new();
        let rx = open_publication(&mut host, 24);
        host.render(80, &Theme::plain());
        for _ in 0..3 {
            host.key(&Key::press("pagedown"), &Keymap::defaults());
            host.render(80, &Theme::plain());
        }
        let last = host.render(80, &Theme::plain()).join("\n");
        assert!(last.contains("line 24") && !last.contains("│ line 1 "), "{last}");
        host.key(&Key::press("up"), &Keymap::defaults());
        host.key(&Key::press("enter"), &Keymap::defaults());
        assert_eq!(rx.recv().unwrap(), Some(true));
    }

    fn open_publication(host: &mut DialogHost, lines: usize) -> std::sync::mpsc::Receiver<Option<bool>> {
        let (reply, rx) = channel();
        let body = (1..=lines)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        host.handle(
            UiRequest::ApprovePublication {
                title: "Publish?".into(),
                body,
                reply,
            },
            &mut ViewModel::default(),
            0,
        );
        rx
    }
}
