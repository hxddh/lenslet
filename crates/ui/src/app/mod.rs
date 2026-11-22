//! The Lenslet window: one header (☰, the run, ▶ the tests, Calls | Access,
//! search), the active view with its inspector, the Output drawer under
//! Calls, and at most one [`Modal`] on top.
//!
//! The root owns the chrome, the `lenslet run` it started and what
//! `lenslet doctor --json` said. Data lives in the [`Session`] model; lists
//! and inspectors are child entities that re-render on their own.

use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, CursorStyle, DragMoveEvent, Entity,
    ExternalPaths, FocusHandle, Focusable, MouseButton, PathPromptOptions, Pixels, Point, Render,
    SharedString, Stateful, Task, Window, actions, div, prelude::*, px, rgb,
};

mod access;
mod bodyview;
mod calls;
mod header;
mod inspector;
mod output;
mod session;
mod widgets;

use self::{
    access::{AccessDetail, AccessEvent, AccessView},
    calls::{CallsEvent, CallsList},
    inspector::{Inspector, InspectorEvent},
    output::{OutputEvent, OutputView},
    session::{Session, SessionEvent},
};
use crate::{
    daemon::{self, Doctor, EmulatorStatus, ManagedRun},
    feed::Source,
    fonts,
    i18n::{self, Tr},
    model::{Access, Trace, search_tokens, to_curl},
    prefs::{self, Lang, UiPrefs},
    search::{SearchEvent, SearchInput},
    store::AccessRow,
    theme::{self, Ui},
    tr,
};

actions!(
    lenslet,
    [
        OpenAbout,
        RunTests,
        StopRun,
        EditCommand,
        OpenProject,
        OpenFile,
        FollowNewest,
        FocusSearch,
        ToggleInspector,
        ToggleOutput,
        ShowCalls,
        ShowAccess,
        PrevRun,
        NextRun,
        CopyCurl,
        AcceptPermission,
        AcceptAll,
        ToggleTestMark,
        RemovePermission,
        UseEnglish,
        UseChinese,
        OpenDocs,
        OpenIssues,
        HideApp,
        HideOthers,
        ShowAll,
        MinimizeWindow,
        ZoomWindow,
        ToggleFullScreen,
        CloseWindow,
        Quit,
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        PageDown,
        PageUp,
        Confirm,
        Cancel,
        SubmitField,
        FocusNextControl,
        FocusPrevControl,
        Cut,
        Copy,
        Paste,
        SelectAll,
        Undo,
        Redo,
    ]
);

const TOAST_MILLIS: u64 = 2_400;
const PREFS_DEBOUNCE: Duration = Duration::from_millis(500);
/// How often the run the window started is checked for exit.
const CHILD_POLL: Duration = Duration::from_secs(1);
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_MIN_H: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Calls,
    Access,
}

/// Everything a menu item, a button or a shortcut can ask for.
#[derive(Clone, Debug)]
pub enum Cmd {
    Run,
    Stop,
    EditCommand,
    OpenProject,
    OpenFile,
    FollowNewest,
    OpenRun(PathBuf),
    StepRun(isize),
    Mode(Mode),
    ToggleInspector,
    ToggleOutput,
    FocusSearch,
    /// Copy text; the label names it in the toast.
    CopyText(Tr, String),
    CopyCurl(Arc<Trace>),
    Accept(Vec<(Access, bool, bool)>),
    Remove(Access),
    TestMark(Access, bool),
    ShowInAccess(Access),
    Lang(Lang),
    About,
    Quit,
}

#[derive(Clone)]
pub struct MenuEntry {
    pub title: SharedString,
    pub hint: Option<String>,
    pub cmd: Cmd,
}

/// What is on top of the window: at most one thing.
#[derive(Clone, Default)]
pub enum Modal {
    #[default]
    None,
    /// A right-click menu.
    Menu(Point<Pixels>, Vec<MenuEntry>),
    /// The ☰ menu (Linux; macOS has the menu bar).
    AppMenu,
    RunPicker,
    /// The ▾ next to ▶.
    RunMenu,
    EditCommand,
    /// Accept these permissions into the lock?
    Confirm(Vec<(Access, bool, bool)>),
    About,
}

impl Modal {
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }

    /// Dialogs take the keyboard; menus and popovers do not.
    pub fn is_dialog(&self) -> bool {
        matches!(self, Modal::EditCommand | Modal::Confirm(_) | Modal::About)
    }
}

#[derive(Clone)]
struct SplitDrag;
#[derive(Clone)]
struct OutputDrag;

struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub struct LensletApp {
    session: Entity<Session>,
    calls: Entity<CallsList>,
    inspector: Entity<Inspector>,
    access: Entity<AccessView>,
    access_detail: Entity<AccessDetail>,
    output: Entity<OutputView>,
    search: Entity<SearchInput>,
    command_field: Entity<SearchInput>,
    root_focus: FocusHandle,
    /// Move focus to the root on the next frame (a dialog opened from a
    /// child view), so Enter and Esc reach it.
    focus_root_next: bool,

    mode: Mode,
    prefs: UiPrefs,
    modal: Modal,
    /// `lenslet doctor --json` for the project, once it answered.
    doctor: Option<Result<Doctor, Tr>>,
    /// The project the last doctor ran in (to rerun when it changes).
    doctor_dir: Option<PathBuf>,
    doctor_busy: bool,
    /// A test command edited in this window, for this project.
    command_override: Option<(PathBuf, String)>,
    run: Option<ManagedRun>,
    starting: bool,
    toast: Option<(Tr, u64)>,
    toast_seq: u64,
    prefs_seq: u64,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl Drop for LensletApp {
    fn drop(&mut self) {
        self.stop_all_children();
    }
}

/// A child view that keeps its last frame until it notifies.
fn cached<V: Render>(view: &Entity<V>) -> gpui::AnyView {
    gpui::AnyView::from(view.clone()).cached(
        gpui::StyleRefinement::default()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col(),
    )
}

pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

impl LensletApp {
    pub fn new(
        prefs: UiPrefs,
        initial_paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Ui::install(cx, Ui::new(&prefs, window.appearance()));
        let project = prefs.project_dir.as_deref().map(PathBuf::from);
        let source = match std::env::var_os("LENSLET_TRACE") {
            Some(path) => Source::File(PathBuf::from(path)),
            None => Source::Runs {
                dir: lenslet_core::runs::dir(&home()),
                pinned: None,
            },
        };
        let session = cx.new(|cx| Session::new(source, project, cx));
        let calls = cx.new(|cx| CallsList::new(session.clone(), cx));
        let inspector = cx.new(|cx| Inspector::new(session.clone(), cx));
        let access = cx.new(|cx| AccessView::new(session.clone(), cx));
        let access_detail = cx.new(|cx| AccessDetail::new(session.clone(), cx));
        let output = cx.new(|cx| OutputView::new(session.clone(), prefs.output_open, cx));
        let search = cx.new(SearchInput::new);
        let command_field = cx.new(|cx| SearchInput::new_field("", "pytest -q", true, cx));

        let subscriptions = vec![
            cx.subscribe(&session, |this, _, event, cx| match event {
                SessionEvent::Toast(message) => this.show_toast(message.clone(), cx),
                SessionEvent::RunChanged => {
                    this.refresh_doctor(false, cx);
                    cx.notify();
                }
                SessionEvent::Runs | SessionEvent::Derived => cx.notify(),
                SessionEvent::Rows(_) | SessionEvent::Log => {}
            }),
            cx.subscribe(&calls, |this, _, event, cx| match event {
                CallsEvent::Selected(call) => {
                    let call = call.clone();
                    this.inspector
                        .update(cx, |inspector, cx| inspector.set_call(call, cx));
                }
                CallsEvent::ContextMenu(position, call) => {
                    let entries = this.call_menu(call.clone(), cx);
                    this.modal = Modal::Menu(*position, entries);
                    cx.notify();
                }
                CallsEvent::ReviewNotInLock => {
                    this.set_mode(Mode::Access, cx);
                    this.access.update(cx, |view, cx| view.show_not_in_lock(cx));
                }
                CallsEvent::Toast(message) => this.show_toast(message.clone(), cx),
            }),
            cx.subscribe(&inspector, |this, _, event, cx| match event {
                InspectorEvent::OpenAccess(access) => {
                    this.run_cmd_cx(Cmd::ShowInAccess(access.clone()), cx)
                }
                InspectorEvent::Toast(message) => this.show_toast(message.clone(), cx),
            }),
            cx.subscribe(&output, |this, _, event, cx| match event {
                OutputEvent::RevealTrace => {
                    if let Some(path) = this.session.read(cx).path.clone() {
                        cx.reveal_path(&path);
                    }
                }
                OutputEvent::Toast(msg) => this.show_toast(msg.clone(), cx),
            }),
            cx.subscribe(&access, |this, _, event, cx| {
                this.on_access_event(event, cx)
            }),
            cx.subscribe(&access_detail, |this, _, event, cx| {
                this.on_access_event(event, cx)
            }),
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Exit => this.focus_list(window, cx),
            }),
            cx.observe(&search, |this, search, cx| {
                let query = search.read(cx).query().to_string();
                this.apply_query(query, cx);
            }),
            cx.observe_window_appearance(window, |this, window, cx| this.restyle(window, cx)),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh_doctor(true, cx);
                }
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                let size = window.viewport_size();
                let (w, h) = (f32::from(size.width), f32::from(size.height));
                if w >= 960.0 && h >= 640.0 {
                    this.prefs.window_width = Some(w.round());
                    this.prefs.window_height = Some(h.round());
                    this.save_prefs(cx);
                }
            }),
        ];

        let h = home();
        cx.background_executor()
            .spawn(async move {
                daemon::cleanup_stale_children(&h);
            })
            .detach();

        let mut app = LensletApp {
            session,
            calls,
            inspector,
            access,
            access_detail,
            output,
            search,
            command_field,
            root_focus: cx.focus_handle(),
            focus_root_next: false,
            mode: Mode::Calls,
            prefs,
            modal: Modal::None,
            doctor: None,
            doctor_dir: None,
            doctor_busy: false,
            command_override: None,
            run: None,
            starting: false,
            toast: None,
            toast_seq: 0,
            prefs_seq: 0,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        app._tasks.push(app.poll_child(cx));
        for path in initial_paths {
            app.open_external_path(path, cx);
        }
        app.refresh_doctor(true, cx);
        let selected = app.calls.read(cx).selected();
        app.inspector
            .update(cx, |inspector, cx| inspector.set_call(selected, cx));
        let focus = app.calls.focus_handle(cx);
        window.focus(&focus);
        app
    }

    /// `SIGTERM` the run the window started and return at once; a
    /// background thread reaps it. Never blocks the main thread.
    pub fn stop_all_children(&mut self) {
        if let Some(run) = self.run.take() {
            daemon::stop_run(run);
        }
    }

    /// A path dropped on the window or passed on the command line: a folder
    /// is the project, anything else a run file.
    pub fn open_external_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let probe = path.clone();
            let is_dir = cx
                .background_executor()
                .spawn(async move {
                    crate::fsguard::assert_io_allowed();
                    probe.is_dir()
                })
                .await;
            this.update(cx, |app, cx| {
                if is_dir {
                    app.set_project(path, cx);
                } else {
                    cx.add_recent_document(&path);
                    app.session
                        .update(cx, |s, cx| s.switch(Source::File(path), cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// The project ▶ runs in: the folder the viewer opened, else the run's.
    fn project_dir(&self, cx: &App) -> Option<PathBuf> {
        self.prefs
            .project_dir
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| self.session.read(cx).project_dir())
    }

    fn set_project(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.prefs.project_dir = Some(dir.to_string_lossy().into_owned());
        self.session
            .update(cx, |s, cx| s.set_project(Some(dir.clone()), cx));
        self.save_prefs(cx);
        let shown = dir.display();
        self.show_toast(tr!("Project folder: {shown}", "项目目录：{shown}"), cx);
        self.refresh_doctor(true, cx);
    }

    /// The command ▶ runs: one edited here for this project, else the one
    /// `lenslet doctor` reports.
    fn test_command(&self, cx: &App) -> Option<String> {
        let project = self.project_dir(cx);
        if let Some((dir, command)) = self.command_override.as_ref()
            && Some(dir) == project.as_ref()
        {
            return Some(command.clone());
        }
        match self.doctor.as_ref() {
            Some(Ok(doctor)) => doctor.test_command.clone().filter(|c| !c.trim().is_empty()),
            _ => None,
        }
    }

    fn emulator_status(&self) -> EmulatorStatus {
        EmulatorStatus::from_doctor(self.doctor.as_ref())
    }

    // -- doctor and the run ---------------------------------------------------------

    /// Ask `lenslet doctor --json` in the project folder. Without `force`,
    /// only when the project changed.
    fn refresh_doctor(&mut self, force: bool, cx: &mut Context<Self>) {
        let dir = self.project_dir(cx);
        if self.doctor_busy || (!force && self.doctor.is_some() && self.doctor_dir == dir) {
            return;
        }
        self.doctor_busy = true;
        self.doctor_dir = dir.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { daemon::run_doctor(dir.as_deref(), DOCTOR_TIMEOUT) })
                .await;
            this.update(cx, |app, cx| {
                app.doctor_busy = false;
                if app.doctor.as_ref() != Some(&result) {
                    app.doctor = Some(result);
                    let status = app.emulator_status();
                    app.calls
                        .update(cx, |c, cx| c.set_emulator_status(status, cx));
                    cx.notify();
                }
                if app.doctor_dir != app.project_dir(cx) {
                    app.refresh_doctor(true, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// ▶: run the test command in the project, or ask for one.
    fn run_tests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.run.is_some() || self.starting {
            return;
        }
        let Some(dir) = self.project_dir(cx) else {
            self.prompt_open_project(cx);
            return;
        };
        let Some(command) = self.test_command(cx) else {
            self.open_edit_command(window, cx);
            return;
        };
        self.starting = true;
        cx.notify();
        let h = home();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move {
                    let bin = daemon::cli_binary()?;
                    let run = daemon::spawn_run(&bin, &dir, &command)?;
                    daemon::record_child(&h, run.child.id());
                    Ok::<_, Tr>(run)
                })
                .await;
            this.update(cx, |app, cx| {
                app.starting = false;
                match started {
                    Ok(run) => {
                        let command = run.command_line.clone();
                        app.run = Some(run);
                        app.set_mode(Mode::Calls, cx);
                        app.session.update(cx, |s, cx| s.follow_newest(cx));
                        app.show_toast(tr!("Running {command}", "正在运行 {command}"), cx);
                    }
                    Err(error) => app.show_toast(error, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stop_run(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.run.take() {
            let pid = run.child.id();
            daemon::stop_run(run);
            let h = home();
            cx.background_executor()
                .spawn(async move { daemon::forget_child(&h, pid) })
                .detach();
            self.show_toast(tr!("Stopping the run…", "正在停止运行…"), cx);
            cx.notify();
        }
    }

    /// Notice when the run the window started ends on its own.
    fn poll_child(&self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHILD_POLL).await;
                let Ok(exited) = this.update(cx, |app, cx| {
                    let run = app.run.as_mut()?;
                    let status = run.child.try_wait().ok().flatten()?;
                    let pid = run.child.id();
                    let command = run.command_line.clone();
                    app.run = None;
                    let code = status.code().unwrap_or(1);
                    app.show_toast(
                        tr!(
                            "{command} exited with code {code}",
                            "{command} 已退出，退出码 {code}"
                        ),
                        cx,
                    );
                    app.session.update(cx, |s, cx| s.refresh_runs(cx));
                    cx.notify();
                    Some(pid)
                }) else {
                    break;
                };
                if let Some(pid) = exited {
                    let h = home();
                    cx.background_executor()
                        .spawn(async move { daemon::forget_child(&h, pid) })
                        .await;
                }
            }
        })
    }

    // -- Modals --------------------------------------------------------------------

    fn open_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = modal;
        if self.modal.is_dialog() {
            // Take focus off any text field, so Esc and Enter reach the window.
            window.focus(&self.root_focus);
        }
        cx.notify();
    }

    fn toggle_popover(&mut self, modal: Modal, cx: &mut Context<Self>) {
        self.modal = if std::mem::discriminant(&self.modal) == std::mem::discriminant(&modal) {
            Modal::None
        } else {
            modal
        };
        cx.notify();
    }

    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_open() {
            self.modal = Modal::None;
            self.focus_list(window, cx);
            cx.notify();
        }
    }

    fn open_edit_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.test_command(cx).unwrap_or_default();
        self.command_field
            .update(cx, |field, cx| field.set_text(current, cx));
        self.modal = Modal::EditCommand;
        let focus = self.command_field.read(cx).handle();
        window.focus(&focus);
        cx.notify();
    }

    fn save_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.command_field.read(cx).query().trim().to_string();
        match self.project_dir(cx) {
            Some(dir) if !command.is_empty() => self.command_override = Some((dir, command)),
            Some(_) => self.command_override = None,
            None => self.prompt_open_project(cx),
        }
        self.close_modal(window, cx);
    }

    fn confirm_yes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Modal::Confirm(entries) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.focus_list(window, cx);
        self.session.update(cx, |s, cx| s.accept(entries, cx));
        cx.notify();
    }

    fn on_access_event(&mut self, event: &AccessEvent, cx: &mut Context<Self>) {
        match event {
            AccessEvent::Selected(row) => {
                let row = row.clone();
                self.access_detail
                    .update(cx, |detail, cx| detail.set_row(row, cx));
            }
            AccessEvent::ContextMenu(position, row) => {
                self.modal = Modal::Menu(*position, self.access_menu(row, cx));
                cx.notify();
            }
            AccessEvent::SelectCall(id) => {
                let id = id.clone();
                self.set_mode(Mode::Calls, cx);
                self.calls
                    .update(cx, |list, cx| list.select_id(Some(id), true, cx));
            }
            AccessEvent::AcceptAll(entries) => {
                self.modal = Modal::Confirm(entries.clone());
                self.focus_root_next = true;
                cx.notify();
            }
            AccessEvent::Toast(message) => self.show_toast(message.clone(), cx),
        }
    }

    fn apply_query(&mut self, query: String, cx: &mut Context<Self>) {
        let tokens = search_tokens(&query);
        self.calls
            .update(cx, |list, cx| list.set_query(query.clone(), cx));
        self.access
            .update(cx, |view, cx| view.set_tokens(tokens.clone(), cx));
        self.inspector
            .update(cx, |inspector, cx| inspector.set_tokens(tokens.clone(), cx));
        self.output
            .update(cx, |output, cx| output.set_tokens(tokens, cx));
    }

    /// The appearance or the language changed: publish the new look.
    fn restyle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if Ui::install(cx, Ui::new(&self.prefs, window.appearance())) {
            self.inspector
                .update(cx, |inspector, cx| inspector.restyle(cx));
            self.output.update(cx, |output, cx| output.restyle(cx));
            cx.refresh_windows();
        }
    }

    fn set_lang(&mut self, lang: Lang, window: &mut Window, cx: &mut Context<Self>) {
        if self.prefs.lang == lang {
            return;
        }
        self.prefs.lang = lang;
        if cfg!(target_os = "macos") {
            cx.set_menus(crate::menus(lang));
        }
        self.restyle(window, cx);
        self.save_prefs(cx);
        cx.notify();
    }

    /// Write preferences at most every [`PREFS_DEBOUNCE`], off the main thread.
    fn save_prefs(&mut self, cx: &mut Context<Self>) {
        self.prefs_seq += 1;
        let seq = self.prefs_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREFS_DEBOUNCE).await;
            let Ok(Some(prefs)) = this.update(cx, |app, _| {
                (app.prefs_seq == seq).then(|| app.prefs.clone())
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move { prefs::save(&home(), &prefs) })
                .await;
        })
        .detach();
    }

    fn show_toast(&mut self, message: Tr, cx: &mut Context<Self>) {
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.toast = Some((message, seq));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(TOAST_MILLIS))
                .await;
            this.update(cx, |app, cx| {
                if app.toast.as_ref().is_some_and(|(_, s)| *s == seq) {
                    app.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.search.update(cx, |search, cx| {
                search.set_access_mode(mode == Mode::Access, cx)
            });
            cx.notify();
        }
    }

    fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match self.mode {
            Mode::Calls => self.calls.focus_handle(cx),
            Mode::Access => self.access.focus_handle(cx),
        };
        window.focus(&focus);
    }

    // -- Commands -------------------------------------------------------------------

    /// Commands that need the window (focus, dialogs).
    fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(cmd, Cmd::Lang(_)) {
            self.modal = Modal::None;
        }
        match cmd {
            Cmd::Run => self.run_tests(window, cx),
            Cmd::EditCommand => self.open_edit_command(window, cx),
            Cmd::FocusSearch => {
                let focus = self.search.read(cx).handle();
                window.focus(&focus);
            }
            Cmd::Lang(lang) => {
                self.modal = Modal::None;
                self.set_lang(lang, window, cx);
            }
            Cmd::About => self.open_modal(Modal::About, window, cx),
            Cmd::Mode(mode) => {
                self.set_mode(mode, cx);
                self.focus_list(window, cx);
            }
            other => self.run_cmd_cx(other, cx),
        }
        cx.notify();
    }

    /// Commands that need no window.
    fn run_cmd_cx(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        match cmd {
            Cmd::Stop => self.stop_run(cx),
            Cmd::OpenProject => self.prompt_open_project(cx),
            Cmd::OpenFile => self.prompt_open_file(cx),
            Cmd::FollowNewest => self.session.update(cx, |s, cx| s.follow_newest(cx)),
            Cmd::OpenRun(path) => {
                let dir = path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| lenslet_core::runs::dir(&home()));
                let source = Source::Runs {
                    dir,
                    pinned: Some(path),
                };
                self.session.update(cx, |s, cx| s.switch(source, cx));
            }
            Cmd::StepRun(delta) => self.session.update(cx, |s, cx| s.step_run(delta, cx)),
            Cmd::Mode(mode) => self.set_mode(mode, cx),
            Cmd::ToggleInspector => {
                self.prefs.inspector_open = !self.prefs.inspector_open;
                self.save_prefs(cx);
            }
            Cmd::ToggleOutput => {
                self.prefs.output_open = !self.prefs.output_open;
                let open = self.prefs.output_open;
                if open {
                    self.set_mode(Mode::Calls, cx);
                }
                self.output.update(cx, |o, cx| o.set_open(open, cx));
                self.save_prefs(cx);
            }
            Cmd::CopyText(label, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                let (en, zh) = (label.en, label.zh);
                self.show_toast(tr!("Copied {en}", "已复制{zh}"), cx);
            }
            Cmd::CopyCurl(call) => {
                let endpoint = self.session.read(cx).header_endpoint().map(str::to_string);
                match to_curl(&call, endpoint.as_deref()) {
                    Some(curl) => {
                        self.run_cmd_cx(Cmd::CopyText(Tr::new("curl", " curl"), curl), cx)
                    }
                    None => self.show_toast(
                        tr!(
                            "This request's body can't be rebuilt as curl",
                            "该请求体无法重建为 curl"
                        ),
                        cx,
                    ),
                }
            }
            Cmd::Accept(entries) => self.session.update(cx, |s, cx| s.accept(entries, cx)),
            Cmd::Remove(access) => self
                .session
                .update(cx, |s, cx| s.remove_from_lock(vec![access], cx)),
            Cmd::TestMark(access, test) => self
                .session
                .update(cx, |s, cx| s.set_test_mark(access, test, cx)),
            Cmd::ShowInAccess(access) => {
                self.set_mode(Mode::Access, cx);
                self.access.update(cx, |view, cx| view.select(access, cx));
            }
            Cmd::Quit => {
                self.stop_all_children();
                cx.quit();
            }
            Cmd::Run | Cmd::EditCommand | Cmd::FocusSearch | Cmd::Lang(_) | Cmd::About => {}
        }
        cx.notify();
    }

    fn prompt_open_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |app, cx| app.open_external_path(path, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn prompt_open_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |app, cx| app.set_project(path, cx)).ok();
            }
        })
        .detach();
    }

    fn call_menu(&self, call: Arc<Trace>, cx: &App) -> Vec<MenuEntry> {
        let ui = Ui::of(cx);
        let mut entries = vec![MenuEntry {
            title: ui.t("Copy as curl", "复制为 curl").into(),
            hint: Some(i18n::shortcut("⇧C")),
            cmd: Cmd::CopyCurl(call.clone()),
        }];
        if let Some(problem) = call.problem.as_ref() {
            entries.push(MenuEntry {
                title: ui.t("Copy error code", "复制错误码").into(),
                hint: None,
                cmd: Cmd::CopyText(tr!("the error code", "错误码"), problem.code.clone()),
            });
        }
        if let Some(access) = call.access().next() {
            entries.push(MenuEntry {
                title: ui
                    .t("Show its permission in Access", "在权限视图中查看")
                    .into(),
                hint: None,
                cmd: Cmd::ShowInAccess(access),
            });
        }
        entries.push(MenuEntry {
            title: ui.t("Copy call ID", "复制调用 ID").into(),
            hint: None,
            cmd: Cmd::CopyText(tr!("the call ID", "调用 ID"), call.id.clone()),
        });
        entries
    }

    fn access_menu(&self, row: &AccessRow, cx: &App) -> Vec<MenuEntry> {
        let ui = Ui::of(cx);
        let access = row.access().clone();
        let mut entries = Vec::new();
        if self.session.read(cx).lock_target().is_some() {
            if row.in_lock() {
                entries.push(MenuEntry {
                    title: ui.t("Remove from lock", "从 lenslet.lock 移除").into(),
                    hint: Some(i18n::key("⌫")),
                    cmd: Cmd::Remove(access.clone()),
                });
            } else {
                entries.push(MenuEntry {
                    title: ui.t("Accept into lock", "写入 lenslet.lock").into(),
                    hint: Some(i18n::key("a")),
                    cmd: Cmd::Accept(vec![(access.clone(), false, row.used.implied)]),
                });
            }
            entries.push(MenuEntry {
                title: if row.test {
                    ui.t("Unmark `# test`", "取消 `# test` 标记").into()
                } else {
                    ui.t("Mark `# test`", "标记 `# test`").into()
                },
                hint: Some(i18n::key("t")),
                cmd: Cmd::TestMark(access.clone(), !row.test),
            });
        }
        entries.push(MenuEntry {
            title: ui.t("Copy action", "复制 Action").into(),
            hint: None,
            cmd: Cmd::CopyText(tr!("the action", "权限名"), access.action.clone()),
        });
        entries.push(MenuEntry {
            title: ui.t("Copy resource ARN", "复制资源 ARN").into(),
            hint: None,
            cmd: Cmd::CopyText(tr!("the ARN", " ARN"), access.resource.clone()),
        });
        entries
    }

    fn select(&mut self, f: impl FnOnce(SelectTarget, &mut App), cx: &mut Context<Self>) {
        if self.modal.is_dialog() {
            return;
        }
        match self.mode {
            Mode::Calls => f(SelectTarget::Calls(self.calls.clone()), cx),
            Mode::Access => f(SelectTarget::Access(self.access.clone()), cx),
        }
    }

    /// Esc: close whatever is on top; else back out of the inspector; else
    /// clear one filter; else clear the search.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_open() {
            self.close_modal(window, cx);
            return;
        }
        let inspector_focused = self.inspector.focus_handle(cx).contains_focused(window, cx)
            || self
                .inspector
                .read(cx)
                .body()
                .focus_handle(cx)
                .is_focused(window);
        if inspector_focused {
            self.focus_list(window, cx);
            cx.notify();
            return;
        }
        let cleared = match self.mode {
            Mode::Calls => self.calls.update(cx, |list, cx| list.clear_one_filter(cx)),
            Mode::Access => self.access.update(cx, |view, cx| view.clear_filter(cx)),
        };
        if !cleared && !self.search.read(cx).query().is_empty() {
            self.search
                .update(cx, |search, cx| search.clear(window, cx));
            self.focus_list(window, cx);
        }
    }

    /// Enter outside text fields.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.modal {
            // A focused dialog button clicks itself on Enter; only act when
            // the dialog itself has focus.
            Modal::Confirm(_) if self.root_focus.is_focused(window) => self.confirm_yes(window, cx),
            Modal::About => self.close_modal(window, cx),
            Modal::None if self.mode == Mode::Calls && self.prefs.inspector_open => {
                let focus = self.inspector.read(cx).body().focus_handle(cx);
                window.focus(&focus);
                cx.notify();
            }
            _ => {}
        }
    }

    /// The access view's keys (a, ⌘⇧A, t, ⌫) act only there.
    fn access_action(
        &mut self,
        f: impl FnOnce(&mut AccessView, &mut Context<AccessView>),
        cx: &mut Context<Self>,
    ) {
        if self.mode == Mode::Access && !self.modal.is_dialog() {
            self.access.update(cx, f);
        }
    }

    fn render_splitter(&self, ui: &Ui, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let t = ui.p;
        div()
            .id("splitter")
            .flex_shrink_0()
            .w(px(theme::SPLITTER_HIT_W))
            .h_full()
            .flex()
            .justify_center()
            .cursor(CursorStyle::ResizeColumn)
            .bg(rgb(t.panel))
            .group("splitter")
            .child(
                div()
                    .w(px(1.0))
                    .h_full()
                    .bg(rgb(t.border))
                    .group_hover("splitter", |s| s.bg(theme::h(t.accent, 0.8))),
            )
            .on_drag(SplitDrag, |_, _, _, cx| cx.new(|_| DragGhost))
            .on_click(cx.listener(|app, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    app.prefs.inspector_width = theme::INSPECTOR_DEFAULT_W;
                    app.save_prefs(cx);
                    cx.notify();
                }
            }))
    }

    fn render_output_splitter(&self, ui: &Ui) -> Stateful<gpui::Div> {
        let t = ui.p;
        div()
            .id("output-splitter")
            .flex_shrink_0()
            .h(px(5.0))
            .w_full()
            .flex()
            .flex_col()
            .justify_center()
            .cursor(CursorStyle::ResizeRow)
            .bg(rgb(t.panel))
            .group("output-splitter")
            .child(
                div()
                    .h(px(1.0))
                    .w_full()
                    .bg(rgb(t.border))
                    .group_hover("output-splitter", |s| s.bg(theme::h(t.accent, 0.8))),
            )
            .on_drag(OutputDrag, |_, _, _, cx| cx.new(|_| DragGhost))
    }

    fn render_toast(&self, ui: &Ui) -> Option<AnyElement> {
        let t = ui.p;
        let (message, _) = self.toast.as_ref()?;
        Some(
            div()
                .absolute()
                .bottom(px(20.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(720.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(rgb(t.border))
                        .bg(rgb(t.panel_raised))
                        .shadow(ui.elevation())
                        .text_size(px(theme::text::MD))
                        .text_color(rgb(t.text_soft))
                        .child(ui.tr(message).to_string()),
                )
                .into_any_element(),
        )
    }
}

enum SelectTarget {
    Calls(Entity<CallsList>),
    Access(Entity<AccessView>),
}

impl SelectTarget {
    fn step(self, delta: isize, cx: &mut App) {
        match self {
            SelectTarget::Calls(list) => list.update(cx, |l, cx| l.move_by(delta, cx)),
            SelectTarget::Access(view) => view.update(cx, |v, cx| v.move_by(delta, cx)),
        }
    }

    fn page(self, down: bool, cx: &mut App) {
        match self {
            SelectTarget::Calls(list) => list.update(cx, |l, cx| l.page(down, cx)),
            SelectTarget::Access(view) => view.update(cx, |v, cx| v.page(down, cx)),
        }
    }

    fn edge(self, last: bool, cx: &mut App) {
        match self {
            SelectTarget::Calls(list) => list.update(cx, |l, cx| l.select_edge(last, cx)),
            SelectTarget::Access(view) => view.update(cx, |v, cx| v.select_edge(last, cx)),
        }
    }
}

impl Render for LensletApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_root_next) {
            window.focus(&self.root_focus);
        }
        crate::fsguard::report_frame();
        crate::fsguard::milestone_once("first frame");
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let viewport = f32::from(window.viewport_size().width);
        let viewport_h = f32::from(window.viewport_size().height);
        let inspector_w = self
            .prefs
            .inspector_width
            .clamp(320.0, (viewport - 420.0).max(320.0));

        let (list, detail) = match self.mode {
            Mode::Calls => (cached(&self.calls), cached(&self.inspector)),
            Mode::Access => (cached(&self.access), cached(&self.access_detail)),
        };
        let mut views = div().flex().flex_row().flex_1().min_h(px(0.0)).child(list);
        if self.prefs.inspector_open {
            views = views.child(self.render_splitter(&ui, cx)).child(
                div()
                    .w(px(inspector_w))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .child(detail),
            );
        }
        let mut main = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(views);
        if self.mode == Mode::Calls && self.prefs.output_open {
            let height = self
                .prefs
                .output_height
                .clamp(OUTPUT_MIN_H, (viewport_h - 260.0).max(OUTPUT_MIN_H));
            main = main.child(self.render_output_splitter(&ui)).child(
                div()
                    .h(px(height))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .child(cached(&self.output)),
            );
        }

        let header = self.render_header(&ui, window, cx);
        let overlay = self.render_overlay(&ui, cx);
        let toast = self.render_toast(&ui);

        div()
            .id("lenslet")
            .key_context("LensletApp")
            .track_focus(&self.root_focus)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(t.canvas))
            .text_color(rgb(t.text))
            .font(fonts::sans_font())
            .on_drop(cx.listener(|app, paths: &ExternalPaths, _, cx| {
                for path in paths.paths().to_vec() {
                    app.open_external_path(path, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &OpenAbout, w, cx| app.run_cmd(Cmd::About, w, cx)))
            .on_action(cx.listener(|app, _: &RunTests, w, cx| app.run_cmd(Cmd::Run, w, cx)))
            .on_action(cx.listener(|app, _: &StopRun, w, cx| app.run_cmd(Cmd::Stop, w, cx)))
            .on_action(
                cx.listener(|app, _: &EditCommand, w, cx| app.run_cmd(Cmd::EditCommand, w, cx)),
            )
            .on_action(
                cx.listener(|app, _: &OpenProject, w, cx| app.run_cmd(Cmd::OpenProject, w, cx)),
            )
            .on_action(cx.listener(|app, _: &OpenFile, w, cx| app.run_cmd(Cmd::OpenFile, w, cx)))
            .on_action(
                cx.listener(|app, _: &FollowNewest, w, cx| app.run_cmd(Cmd::FollowNewest, w, cx)),
            )
            .on_action(
                cx.listener(|app, _: &FocusSearch, w, cx| app.run_cmd(Cmd::FocusSearch, w, cx)),
            )
            .on_action(cx.listener(|app, _: &ToggleInspector, w, cx| {
                app.run_cmd(Cmd::ToggleInspector, w, cx)
            }))
            .on_action(
                cx.listener(|app, _: &ToggleOutput, w, cx| app.run_cmd(Cmd::ToggleOutput, w, cx)),
            )
            .on_action(
                cx.listener(|app, _: &ShowCalls, w, cx| app.run_cmd(Cmd::Mode(Mode::Calls), w, cx)),
            )
            .on_action(
                cx.listener(|app, _: &ShowAccess, w, cx| {
                    app.run_cmd(Cmd::Mode(Mode::Access), w, cx)
                }),
            )
            .on_action(cx.listener(|app, _: &PrevRun, w, cx| app.run_cmd(Cmd::StepRun(-1), w, cx)))
            .on_action(cx.listener(|app, _: &NextRun, w, cx| app.run_cmd(Cmd::StepRun(1), w, cx)))
            .on_action(cx.listener(|app, _: &CopyCurl, w, cx| {
                if let Some(call) = app.inspector.read(cx).call().cloned() {
                    app.run_cmd(Cmd::CopyCurl(call), w, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &AcceptPermission, _, cx| {
                app.access_action(|v, cx| v.accept_selected(cx), cx)
            }))
            .on_action(cx.listener(|app, _: &AcceptAll, _, cx| {
                app.access_action(|v, cx| v.accept_all(cx), cx)
            }))
            .on_action(cx.listener(|app, _: &ToggleTestMark, _, cx| {
                app.access_action(|v, cx| v.toggle_test_selected(cx), cx)
            }))
            .on_action(cx.listener(|app, _: &RemovePermission, _, cx| {
                app.access_action(|v, cx| v.remove_selected(cx), cx)
            }))
            .on_action(cx.listener(|app, _: &UseEnglish, w, cx| app.set_lang(Lang::En, w, cx)))
            .on_action(cx.listener(|app, _: &UseChinese, w, cx| app.set_lang(Lang::Zh, w, cx)))
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ZoomWindow, window, _| window.zoom_window())
            .on_action(|_: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|app, _: &CloseWindow, window, cx| {
                // Closes what is on top, else the window.
                if app.modal.is_open() {
                    app.close_modal(window, cx);
                } else {
                    window.remove_window();
                }
            }))
            .on_action(cx.listener(|app, _: &SelectNext, _, cx| {
                app.select(|target, cx| target.step(1, cx), cx)
            }))
            .on_action(cx.listener(|app, _: &SelectPrev, _, cx| {
                app.select(|target, cx| target.step(-1, cx), cx)
            }))
            .on_action(
                cx.listener(|app, _: &SelectFirst, _, cx| {
                    app.select(|t, cx| t.edge(false, cx), cx)
                }),
            )
            .on_action(
                cx.listener(|app, _: &SelectLast, _, cx| app.select(|t, cx| t.edge(true, cx), cx)),
            )
            .on_action(
                cx.listener(|app, _: &PageDown, _, cx| app.select(|t, cx| t.page(true, cx), cx)),
            )
            .on_action(
                cx.listener(|app, _: &PageUp, _, cx| app.select(|t, cx| t.page(false, cx), cx)),
            )
            .on_action(cx.listener(|app, _: &Confirm, window, cx| app.confirm(window, cx)))
            .on_action(cx.listener(|app, _: &SubmitField, window, cx| {
                if matches!(app.modal, Modal::EditCommand) {
                    app.save_command(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &Cancel, window, cx| app.cancel(window, cx)))
            .on_action(cx.listener(|app, _: &FocusNextControl, window, cx| {
                if app.modal.is_dialog() {
                    window.focus_next();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|app, _: &FocusPrevControl, window, cx| {
                if app.modal.is_dialog() {
                    window.focus_prev();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|app, _: &Copy, _, cx| {
                let body = app.inspector.read(cx).body().clone();
                body.update(cx, |body, cx| body.copy(cx));
            }))
            .on_action(cx.listener(|app, _: &SelectAll, _, cx| {
                let body = app.inspector.read(cx).body().clone();
                body.update(cx, |body, cx| body.select_all(cx));
            }))
            .on_drag_move(
                cx.listener(|app, event: &DragMoveEvent<SplitDrag>, window, cx| {
                    let viewport = f32::from(window.viewport_size().width);
                    let x = f32::from(event.event.position.x);
                    let width = (viewport - x).clamp(320.0, (viewport - 420.0).max(320.0));
                    if (width - app.prefs.inspector_width).abs() >= 1.0 {
                        app.prefs.inspector_width = width;
                        app.save_prefs(cx);
                        cx.notify();
                    }
                }),
            )
            .on_drag_move(
                cx.listener(|app, event: &DragMoveEvent<OutputDrag>, window, cx| {
                    let viewport_h = f32::from(window.viewport_size().height);
                    let y = f32::from(event.event.position.y);
                    let height = (viewport_h - y)
                        .clamp(OUTPUT_MIN_H, (viewport_h - 260.0).max(OUTPUT_MIN_H));
                    if (height - app.prefs.output_height).abs() >= 1.0 {
                        app.prefs.output_height = height;
                        app.save_prefs(cx);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|app, _, _, cx| {
                    if matches!(app.modal, Modal::Menu(..)) {
                        app.modal = Modal::None;
                        cx.notify();
                    }
                }),
            )
            .child(header)
            .child(main)
            .children(overlay)
            .children(toast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogs_take_the_keyboard_and_popovers_do_not() {
        assert!(Modal::EditCommand.is_dialog());
        assert!(Modal::Confirm(Vec::new()).is_dialog());
        assert!(Modal::About.is_dialog());
        for popover in [Modal::RunPicker, Modal::RunMenu, Modal::AppMenu] {
            assert!(popover.is_open() && !popover.is_dialog());
        }
        assert!(!Modal::None.is_open());
    }
}
