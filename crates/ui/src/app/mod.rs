//! The Lenslet window: a header (run, ▶ run button, Calls | Access, baseline,
//! search), an optional sidebar (project and runs), the active view with its
//! inspector, the Output drawer under Calls, and at most one [`Modal`]
//! (palette, a dialog, or a popover) on top.
//!
//! The root owns only chrome and child-process handles. Data lives in the
//! [`Session`] model; lists and inspectors are child entities that re-render
//! on their own.

use std::{path::PathBuf, process::Child, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Corner, CursorStyle, Div,
    DragMoveEvent, ElementId, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight,
    MouseButton, PathPromptOptions, Pixels, Point, Render, SharedString, Stateful, Task, Window,
    WindowControlArea, actions, anchored, deferred, div, point, prelude::*, px, rgb,
};

mod access;
mod bodyview;
mod calls;
mod composer;
mod inspector;
mod launcher;
mod output;
mod palette;
mod session;
mod settings;
mod sidebar;
mod widgets;

use self::{
    access::{AccessDetail, AccessEvent, AccessView},
    calls::{CallsEvent, CallsList},
    composer::{ComposerEvent, RequestComposer},
    inspector::{Inspector, InspectorEvent},
    launcher::{LauncherEvent, RunLauncher, RunSettings, render_about_overlay},
    output::{OutputEvent, OutputView},
    session::{Baseline, Health, Session, SessionEvent},
    sidebar::SIDEBAR_W,
};
use crate::{
    daemon::{
        self, CiError, CommandRunOpts, DEFAULT_PROXY_PORT, Doctor, EmulatorStatus, ManagedRun,
        ProxyHealth, ProxyLaunchOpts, ShellFormat,
    },
    feed::{FeedMode, Source},
    fonts, glyph,
    i18n::{self, Tr},
    model::{Access, StatusFilter, Trace, ellipsize, format_bytes, search_tokens, to_curl},
    prefs::{self, AccentColor, Density, Lang, ThemeMode, UiPrefs},
    replay::ReplayTarget,
    search::{SearchEvent, SearchInput},
    store::{AccessRow, policy_json, statement_json},
    theme::{self, Ui, text},
    tr,
};

actions!(
    lenslet,
    [
        OpenAbout,
        RunTests,
        StopRun,
        RunOptions,
        OpenTerminalAction,
        ToggleDemoAction,
        OpenWorkspace,
        OpenPalette,
        FocusSearch,
        OpenSettings,
        ToggleSidebar,
        ToggleInspector,
        ToggleOutput,
        OpenFile,
        ExportReport,
        InitCiWorkflow,
        NextMatch,
        PrevMatch,
        Replay,
        EditReplay,
        ShowCalls,
        ShowAccess,
        PrevRun,
        NextRun,
        CopyCurl,
        InstallCliAction,
        OpenDocs,
        OpenIssues,
        HideApp,
        HideOthers,
        ShowAll,
        MinimizeWindow,
        ZoomWindow,
        BringAllToFront,
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
/// How often the managed children and the proxy's health are checked.
const CHILD_POLL: Duration = Duration::from_secs(2);
/// How often `lenslet doctor --json` runs while the window is active.
const DOCTOR_POLL: Duration = Duration::from_secs(10);
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(5);
/// Children get this long after `SIGTERM` before `SIGKILL` on quit.
const QUIT_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_MIN_H: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Calls,
    Access,
}

/// Call-list filters a command can set.
#[derive(Clone, Debug)]
pub enum FilterCmd {
    Status(StatusFilter),
    Service(String),
    Access(Access),
    Clear,
}

/// Edits to `lenslet.lock`, and the way to review them.
#[derive(Clone, Debug)]
pub enum LockCmd {
    Accept(Vec<(Access, bool)>),
    Remove(Vec<Access>),
    TestMark(Access, bool),
    /// Switch to Access, filtered to the permissions not in the lock.
    ReviewNotInLock,
}

/// Which run is shown.
#[derive(Clone, Debug)]
pub enum RunNav {
    Prev,
    Next,
    Latest,
    Open(PathBuf),
    Reload,
    LoadAll,
}

#[derive(Clone, Copy, Debug)]
pub enum PrefCmd {
    Theme(ThemeMode),
    Accent(AccentColor),
    Density(Density),
    Lang(Lang),
}

/// Everything the window can do: palette entries, menus and context-menu
/// items are all lists of these. Keep it under 30 variants.
#[derive(Clone, Debug)]
pub enum Cmd {
    Mode(Mode),
    Run,
    StopRun,
    RunOptions,
    OpenTerminal,
    CopyEnv(ShellFormat),
    Proxy(bool),
    Demo(bool),
    OpenWorkspace,
    OpenFile,
    ExportReport,
    InitCi,
    ToggleSidebar,
    ToggleInspector,
    ToggleOutput,
    FocusSearch,
    OpenSettings,
    OpenAbout,
    Replay,
    EditReplay,
    CopyCurl(Arc<Trace>),
    CopyText(&'static str, String),
    CopyPolicy,
    SavePayload,
    Filter(FilterCmd),
    Lock(LockCmd),
    Runs(RunNav),
    SetBaseline(Baseline),
    Pref(PrefCmd),
    Quit,
}

#[derive(Clone)]
pub struct MenuEntry {
    pub title: SharedString,
    pub hint: Option<String>,
    pub cmd: Cmd,
}

/// A small menu anchored to a control or the pointer.
#[derive(Clone)]
pub enum Popover {
    AppMenu,
    Runs,
    Baseline,
    Menu(Point<Pixels>, Vec<MenuEntry>),
}

/// A question that needs a yes before anything is written.
#[derive(Clone, Debug)]
pub enum Question {
    /// Accept every permission not in the lock (`(access, test, implied)`).
    AcceptAll(Vec<(Access, bool, bool)>),
    /// Replace an existing `.github/workflows/lenslet.yml`.
    OverwriteCi {
        path: PathBuf,
        command: String,
        upstream: Option<String>,
    },
}

/// Whatever sits on top of the window. There is at most one.
#[derive(Clone, Default)]
pub enum Modal {
    #[default]
    None,
    Palette,
    Settings,
    /// Run options (the header's ▾).
    Launcher,
    /// Edit & Replay of a captured call.
    Editor,
    About,
    Confirm(Question),
    Popover(Popover),
}

impl Modal {
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }

    /// A dialog takes the keyboard: Tab cycles its controls, lists don't move.
    pub fn is_dialog(&self) -> bool {
        matches!(
            self,
            Modal::Settings | Modal::Launcher | Modal::Editor | Modal::About | Modal::Confirm(_)
        )
    }

    /// Whether list navigation keys should be ignored.
    pub fn blocks_list(&self) -> bool {
        self.is_dialog()
    }

    /// Esc: close whatever is open. Returns whether something was.
    pub fn escape(&mut self) -> bool {
        let was_open = self.is_open();
        *self = Modal::None;
        was_open
    }
}

/// What ▶ would start, or `None` when there is no command yet (▶ then reads
/// "Set test command…" and opens Run options).
pub fn plan_run(settings: &RunSettings, runs_dir: PathBuf) -> Option<CommandRunOpts> {
    let command = settings.command.trim();
    if command.is_empty() {
        return None;
    }
    Some(CommandRunOpts {
        command_line: command.to_string(),
        cwd: settings.cwd.clone(),
        upstream: settings.upstream.clone(),
        routes: settings.routes.clone(),
        enforce: settings.enforce,
        allow_docker: settings.allow_docker,
        runs_dir,
    })
}

/// Start requests in flight, so a double click never starts two.
#[derive(Clone, Copy, Debug, Default)]
pub struct InFlight {
    run: bool,
    proxy: bool,
}

impl InFlight {
    /// Claim the run slot; false when a run is starting or already running.
    pub fn begin_run(&mut self, running: bool) -> bool {
        if self.run || running {
            return false;
        }
        self.run = true;
        true
    }

    pub fn end_run(&mut self) {
        self.run = false;
    }

    pub fn begin_proxy(&mut self, running: bool) -> bool {
        if self.proxy || running {
            return false;
        }
        self.proxy = true;
        true
    }

    pub fn end_proxy(&mut self) {
        self.proxy = false;
    }

    pub fn starting_run(&self) -> bool {
        self.run
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
    launcher: Entity<RunLauncher>,
    composer: Entity<RequestComposer>,
    search: Entity<SearchInput>,
    palette_input: Entity<SearchInput>,
    root_focus: FocusHandle,

    mode: Mode,
    prefs: UiPrefs,
    modal: Modal,
    palette_index: usize,
    palette_entries: Vec<MenuEntry>,
    storage: Option<(usize, u64)>,
    proxy: ProxyHealth,
    doctor: Option<Result<Doctor, Tr>>,
    doctor_busy: bool,
    window_active: bool,
    managed_proxy: Option<Child>,
    managed_run: Option<ManagedRun>,
    /// The Enforce setting the managed run was started with.
    run_enforce: Option<bool>,
    in_flight: InFlight,
    proxy_port: u16,
    detected_cmd: Option<String>,
    /// How many runs the sidebar lists before "N more…".
    sidebar_runs_shown: usize,
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

/// A child view that keeps its last frame until it notifies. It lays out as
/// a flexible column that fills its parent's main axis.
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

fn trace_source() -> Source {
    match std::env::var_os("LENSLET_TRACE") {
        Some(path) => Source::File(PathBuf::from(path)),
        None => Source::Runs {
            dir: lenslet_core::runs::dir(&home()),
            pinned: None,
        },
    }
}

impl LensletApp {
    pub fn new(
        prefs: UiPrefs,
        initial_paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Ui::install(cx, Ui::new(&prefs, window.appearance()));
        let demo = std::env::var_os("LENSLET_DEMO").is_some_and(|v| v == "1");
        let h = home();
        let workspace = prefs.workspace_dir.as_deref().map(PathBuf::from);
        let session = cx.new(|cx| {
            let mut s = Session::new(trace_source(), demo, cx);
            if let Some(ws) = workspace.filter(|p| !p.as_os_str().is_empty()) {
                s.set_workspace_fallback(Some(ws), cx);
            }
            s
        });
        let calls = cx.new(|cx| CallsList::new(session.clone(), cx));
        let inspector = cx.new(|cx| Inspector::new(session.clone(), h.clone(), cx));
        let access = cx.new(|cx| AccessView::new(session.clone(), cx));
        let access_detail = cx.new(|cx| AccessDetail::new(session.clone(), cx));
        let output_open = prefs.output_open;
        let output = cx.new(|cx| OutputView::new(session.clone(), output_open, cx));
        let launcher = cx.new(|cx| RunLauncher::new(h.clone(), cx));
        let composer = cx.new(|cx| RequestComposer::new(h.clone(), cx));
        let search = cx.new(SearchInput::new);
        let palette_input = cx.new(SearchInput::new_palette);

        let mut subscriptions = vec![
            cx.subscribe(&session, |this, _, event, cx| match event {
                SessionEvent::Toast(message) => this.show_toast(message.clone(), cx),
                SessionEvent::RunChanged | SessionEvent::Runs | SessionEvent::Derived => {
                    cx.notify()
                }
                SessionEvent::Rows(_) | SessionEvent::Log => {}
            }),
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.subscribe(&calls, |this, _, event, cx| match event {
                CallsEvent::Selected(call) => {
                    let call = call.clone();
                    this.inspector
                        .update(cx, |inspector, cx| inspector.set_call(call, cx));
                }
                CallsEvent::ContextMenu(position, call) => {
                    let entries = this.call_menu(call.clone(), cx);
                    this.open_popover(Popover::Menu(*position, entries), cx);
                }
                CallsEvent::ReviewNotInLock => {
                    this.run_cmd_cx(Cmd::Lock(LockCmd::ReviewNotInLock), cx)
                }
                CallsEvent::Toast(message) => this.show_toast(message.clone(), cx),
            }),
            cx.subscribe_in(
                &inspector,
                window,
                |this, _, event, window, cx| match event {
                    InspectorEvent::OpenAccess(access) => {
                        let access = access.clone();
                        this.set_mode(Mode::Access, cx);
                        this.access.update(cx, |view, cx| view.select(access, cx));
                    }
                    InspectorEvent::Select(id) => {
                        let id = id.clone();
                        this.calls
                            .update(cx, |list, cx| list.select_id(Some(id), true, cx));
                    }
                    InspectorEvent::OpenComposer(call, target) => {
                        let call = call.clone();
                        let target = target.clone();
                        this.open_editor(&call, target, window, cx);
                    }
                    InspectorEvent::Toast(message) => this.show_toast(message.clone(), cx),
                },
            ),
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    ComposerEvent::Close => this.close_modal(window, cx),
                    ComposerEvent::Sent(call_id, state) => {
                        let call_id = call_id.clone();
                        let state = state.clone();
                        this.inspector.update(cx, |insp, cx| {
                            insp.apply_composed_replay(call_id, state, cx);
                        });
                    }
                },
            ),
            cx.subscribe_in(
                &launcher,
                window,
                |this, _, event, window, cx| match event {
                    LauncherEvent::Close => this.close_modal(window, cx),
                    LauncherEvent::Save(settings) => {
                        this.save_run_settings(settings.clone(), cx);
                        this.close_modal(window, cx);
                    }
                    LauncherEvent::OpenTerminal => this.open_configured_terminal(cx),
                    LauncherEvent::CopyEnv => this.copy_env(ShellFormat::Sh, cx),
                    LauncherEvent::CopyInstall => {
                        this.copy("pip install", daemon::MOTO_INSTALL.to_string(), cx)
                    }
                },
            ),
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
            cx.observe(&palette_input, |this, _, cx| {
                this.palette_index = 0;
                this.refresh_palette(cx);
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.restyle(window, cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                this.window_active = window.is_window_active();
                if this.window_active {
                    this.refresh_doctor(cx);
                }
            }),
        ];
        subscriptions.push(cx.observe_window_bounds(window, |this, window, cx| {
            let size = window.viewport_size();
            let (w, h) = (f32::from(size.width), f32::from(size.height));
            if w >= 960.0 && h >= 640.0 {
                this.prefs.window_width = Some(w.round());
                this.prefs.window_height = Some(h.round());
                this.save_prefs(cx);
            }
        }));

        let mut tasks = Vec::new();
        if !demo {
            tasks.push(Self::spawn_child_poller(cx));
            let h = home();
            cx.background_executor()
                .spawn(async move {
                    daemon::cleanup_stale_children(&h);
                })
                .detach();
        }
        tasks.push(Self::spawn_doctor_poller(cx));

        let proxy_port = prefs.proxy_port.unwrap_or(DEFAULT_PROXY_PORT);
        let detected_cmd = prefs.last_command.clone().filter(|s| !s.trim().is_empty());
        let root_focus = cx.focus_handle();
        let mut app = LensletApp {
            session,
            calls,
            inspector,
            access,
            access_detail,
            output,
            launcher,
            composer,
            search,
            palette_input,
            root_focus,
            mode: Mode::Calls,
            prefs,
            modal: Modal::None,
            palette_index: 0,
            palette_entries: Vec::new(),
            storage: None,
            proxy: ProxyHealth::default(),
            doctor: None,
            doctor_busy: false,
            window_active: true,
            managed_proxy: None,
            managed_run: None,
            run_enforce: None,
            in_flight: InFlight::default(),
            proxy_port,
            detected_cmd,
            sidebar_runs_shown: sidebar::RUNS_PAGE,
            toast: None,
            toast_seq: 0,
            prefs_seq: 0,
            _tasks: tasks,
            _subscriptions: subscriptions,
        };
        for path in initial_paths {
            app.open_external_path(path, cx);
        }
        let ws_init = app.active_workspace_dir(cx);
        app.refresh_detected_command(ws_init, false, cx);
        let selected = app.calls.read(cx).selected();
        app.inspector
            .update(cx, |inspector, cx| inspector.set_call(selected, cx));
        let permission = app.access.read(cx).selected();
        app.access_detail
            .update(cx, |detail, cx| detail.set_row(permission, cx));
        let focus = app.calls.focus_handle(cx);
        window.focus(&focus);
        app
    }

    /// `SIGTERM` every managed child and return at once; a background thread
    /// reaps them (and `SIGKILL`s stragglers). Never blocks the main thread.
    pub fn stop_all_children(&mut self) {
        let mut children = Vec::new();
        if let Some(child) = self.managed_proxy.take() {
            children.push(child);
        }
        if let Some(run) = self.managed_run.take() {
            children.push(run.child);
        }
        daemon::terminate_and_reap(children, QUIT_GRACE);
    }

    /// A path dropped on the window or passed on the command line: a folder
    /// becomes the project, a `.json` a policy to check, anything else a run
    /// file. The folder check runs in the background.
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
            this.update(cx, |app, cx| app.open_classified_path(path, is_dir, cx))
                .ok();
        })
        .detach();
    }

    fn open_classified_path(&mut self, path: PathBuf, is_dir: bool, cx: &mut Context<Self>) {
        if self.session.read(cx).demo {
            self.session.update(cx, |s, cx| s.exit_demo(cx));
        }
        if is_dir {
            self.record_workspace(path.clone(), cx);
            let dir = path.display();
            self.show_toast(tr!("Project folder: {dir}", "项目目录：{dir}"), cx);
            return;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "json" {
            self.set_mode(Mode::Access, cx);
            self.session.update(cx, |s, cx| s.load_sim_policy(path, cx));
            return;
        }
        cx.add_recent_document(&path);
        self.session
            .update(cx, |s, cx| s.switch(Source::File(path), cx));
    }

    pub(super) fn active_workspace_dir(&self, cx: &App) -> PathBuf {
        if let Some(ws) = self
            .prefs
            .workspace_dir
            .as_deref()
            .map(PathBuf::from)
            .filter(|d| !d.as_os_str().is_empty())
        {
            return ws;
        }
        if let Some(cwd) = self
            .session
            .read(cx)
            .run
            .as_ref()
            .filter(|r| !r.header.cwd.is_empty())
            .map(|r| PathBuf::from(&r.header.cwd))
        {
            return cwd;
        }
        std::env::current_dir().unwrap_or_else(|_| home())
    }

    fn record_workspace(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let dir_str = dir.to_string_lossy().into_owned();
        let changed = self.prefs.workspace_dir.as_deref() != Some(dir_str.as_str());
        self.prefs.workspace_dir = Some(dir_str.clone());
        self.prefs.recent_workspaces.retain(|d| d != &dir_str);
        self.prefs.recent_workspaces.insert(0, dir_str);
        self.prefs.recent_workspaces.truncate(6);
        self.session
            .update(cx, |s, cx| s.set_workspace_fallback(Some(dir.clone()), cx));
        if changed {
            self.refresh_detected_command(dir, true, cx);
        }
        self.save_prefs(cx);
        cx.notify();
    }

    /// Detect the project's test command in the background; a command the
    /// user saved wins unless the project changed.
    fn refresh_detected_command(
        &mut self,
        dir: PathBuf,
        project_changed: bool,
        cx: &mut Context<Self>,
    ) {
        if !project_changed && self.detected_cmd.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { daemon::detect_project_commands(&dir) })
                .await;
            this.update(cx, |app, cx| {
                app.detected_cmd = found.into_iter().next();
                if project_changed {
                    app.prefs.last_command = app.detected_cmd.clone();
                    app.save_prefs(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The settings ▶ uses.
    fn run_settings(&self, cx: &App) -> RunSettings {
        RunSettings {
            command: self.detected_cmd.clone().unwrap_or_default(),
            cwd: self.active_workspace_dir(cx),
            upstream: self.prefs.proxy_upstream.clone(),
            routes: self.prefs.proxy_routes.clone(),
            enforce: self.prefs.proxy_enforce,
            allow_docker: self.prefs.proxy_allow_docker,
        }
    }

    fn save_run_settings(&mut self, settings: RunSettings, cx: &mut Context<Self>) {
        let command = settings.command.trim().to_string();
        self.detected_cmd = (!command.is_empty()).then(|| command.clone());
        self.prefs.last_command = self.detected_cmd.clone();
        self.prefs.proxy_upstream = settings.upstream;
        self.prefs.proxy_routes = settings.routes;
        self.prefs.proxy_enforce = settings.enforce;
        self.prefs.proxy_allow_docker = settings.allow_docker;
        let cwd_changed =
            self.prefs.workspace_dir.as_deref() != Some(&*settings.cwd.to_string_lossy());
        if cwd_changed {
            let dir_str = settings.cwd.to_string_lossy().into_owned();
            self.prefs.workspace_dir = Some(dir_str.clone());
            self.prefs.recent_workspaces.retain(|d| d != &dir_str);
            self.prefs.recent_workspaces.insert(0, dir_str);
            self.prefs.recent_workspaces.truncate(6);
            self.session
                .update(cx, |s, cx| s.set_workspace_fallback(Some(settings.cwd), cx));
        }
        self.save_prefs(cx);
        cx.notify();
    }

    /// The Enforce setting of whatever is capturing right now, if anything.
    fn active_enforce(&self) -> Option<bool> {
        if self.managed_run.is_some() {
            return self.run_enforce;
        }
        self.proxy.running.then_some(self.proxy.enforce)
    }

    fn emulator_status(&self) -> EmulatorStatus {
        EmulatorStatus::from_doctor(self.doctor.as_ref())
    }

    // -- Children ----------------------------------------------------------------

    /// ▶: run the test command, or ask for one.
    fn run_tests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let runs_dir = lenslet_core::runs::dir(&home());
        let Some(opts) = plan_run(&self.run_settings(cx), runs_dir) else {
            self.open_launcher(window, cx);
            return;
        };
        self.launch_command_run(opts, cx);
    }

    fn launch_command_run(&mut self, opts: CommandRunOpts, cx: &mut Context<Self>) {
        if !self.in_flight.begin_run(self.managed_run.is_some()) {
            self.show_toast(
                tr!(
                    "A run is already going; stop it first",
                    "已有运行正在进行，请先停止"
                ),
                cx,
            );
            return;
        }
        if self.session.read(cx).demo {
            self.session.update(cx, |s, cx| s.exit_demo(cx));
        }
        let enforce = opts.enforce;
        let h = home();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let run = daemon::spawn_command_run(&opts)?;
                    daemon::record_child(&h, run.child.id());
                    Ok::<_, Tr>(run)
                })
                .await;
            this.update(cx, |app, cx| {
                app.in_flight.end_run();
                match res {
                    Ok(run) => {
                        let trace_path = run.trace_path.clone();
                        let cmd = run.command_line.clone();
                        app.managed_run = Some(run);
                        app.run_enforce = Some(enforce);
                        let dir = lenslet_core::runs::dir(&home());
                        app.session.update(cx, |s, cx| {
                            s.switch(
                                Source::Runs {
                                    dir,
                                    pinned: Some(trace_path),
                                },
                                cx,
                            );
                        });
                        app.show_toast(tr!("Started {cmd}", "已启动 {cmd}"), cx);
                    }
                    Err(err) => app.show_toast(err, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stop the managed run (on its own proxy port), else the managed proxy.
    fn stop_run(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.managed_run.take() {
            self.run_enforce = None;
            let pid = run.child.id();
            let h = home();
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .spawn(async move {
                        daemon::stop_managed_run(run);
                        daemon::forget_children(&h, &[pid]);
                    })
                    .await;
                this.update(cx, |app, cx| {
                    app.session.update(cx, |s, cx| {
                        s.refresh_console_log(cx);
                        s.reload(cx);
                    });
                    app.show_toast(tr!("Stopped the run", "已停止运行"), cx);
                })
                .ok();
            })
            .detach();
            cx.notify();
        } else if self.managed_proxy.is_some() {
            self.stop_proxy(cx);
        }
    }

    fn proxy_opts(&self, cx: &App) -> ProxyLaunchOpts {
        ProxyLaunchOpts {
            port: self.proxy_port,
            upstream: self.prefs.proxy_upstream.clone(),
            routes: self.prefs.proxy_routes.clone(),
            enforce: self.prefs.proxy_enforce,
            allow_docker: self.prefs.proxy_allow_docker,
            cwd: Some(self.active_workspace_dir(cx)),
        }
    }

    /// Start the managed proxy, then (optionally) open a terminal wired to it.
    fn start_proxy(&mut self, then_terminal: bool, cx: &mut Context<Self>) {
        let running = self.managed_proxy.is_some() || self.proxy.running;
        if running && then_terminal {
            self.spawn_terminal(cx);
            return;
        }
        if !self.in_flight.begin_proxy(running) {
            return;
        }
        let opts = self.proxy_opts(cx);
        let port = self.proxy_port;
        let h = home();
        let cwd = self.active_workspace_dir(cx);
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let child = daemon::spawn_proxy_verified(&opts)?;
                    daemon::record_child(&h, child.id());
                    let terminal =
                        then_terminal.then(|| daemon::open_configured_terminal(&h, &cwd, port));
                    Ok::<_, Tr>((child, terminal))
                })
                .await;
            this.update(cx, |app, cx| {
                app.in_flight.end_proxy();
                match res {
                    Ok((child, terminal)) => {
                        app.managed_proxy = Some(child);
                        app.proxy.running = true;
                        app.proxy.enforce = app.prefs.proxy_enforce;
                        app.follow_latest(cx);
                        match terminal {
                            Some(result) => app.terminal_opened(result, cx),
                            None => app.show_toast(
                                tr!(
                                    "Proxy listening on http://127.0.0.1:{port}",
                                    "本地代理已在 http://127.0.0.1:{port} 监听"
                                ),
                                cx,
                            ),
                        }
                    }
                    Err(err) => app.show_toast(err, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    fn stop_proxy(&mut self, cx: &mut Context<Self>) {
        if let Some(child) = self.managed_proxy.take() {
            let port = self.proxy_port;
            let pid = child.id();
            let h = home();
            cx.background_executor()
                .spawn(async move {
                    daemon::stop_gracefully(child, Some(port), Duration::from_millis(2_500));
                    daemon::forget_children(&h, &[pid]);
                })
                .detach();
            self.proxy.running = false;
            self.show_toast(tr!("Stopped the proxy", "已停止本地代理"), cx);
        }
    }

    fn open_configured_terminal(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).demo {
            self.session.update(cx, |s, cx| s.exit_demo(cx));
        }
        self.start_proxy(true, cx);
    }

    fn spawn_terminal(&mut self, cx: &mut Context<Self>) {
        let (h, cwd, port) = (home(), self.active_workspace_dir(cx), self.proxy_port);
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { daemon::open_configured_terminal(&h, &cwd, port) })
                .await;
            this.update(cx, |app, cx| app.terminal_opened(res, cx)).ok();
        })
        .detach();
    }

    fn terminal_opened(&mut self, result: Result<(), Tr>, cx: &mut Context<Self>) {
        let port = self.proxy_port;
        match result {
            Ok(()) => self.show_toast(
                tr!(
                    "Opened a terminal (AWS_ENDPOINT_URL=http://127.0.0.1:{port})",
                    "已打开终端 (AWS_ENDPOINT_URL=http://127.0.0.1:{port})"
                ),
                cx,
            ),
            Err(_) => {
                let snippet = daemon::shell_env_snippet_for(port, ShellFormat::Sh);
                cx.write_to_clipboard(ClipboardItem::new_string(snippet));
                self.show_toast(
                    tr!(
                        "No terminal app found; copied the environment to the clipboard",
                        "未找到终端程序；已将环境变量复制到剪贴板"
                    ),
                    cx,
                );
            }
        }
    }

    fn copy_env(&mut self, fmt: ShellFormat, cx: &mut Context<Self>) {
        let snippet = daemon::shell_env_snippet_for(self.proxy_port, fmt);
        cx.write_to_clipboard(ClipboardItem::new_string(snippet));
        let label = fmt.label();
        let msg = if self.proxy.running {
            tr!("Copied the {label} environment", "已复制 {label} 环境变量")
        } else {
            tr!(
                "Copied the {label} environment; start the proxy before using it",
                "已复制 {label} 环境变量；使用前请先启动本地代理"
            )
        };
        self.show_toast(msg, cx);
    }

    /// Every [`CHILD_POLL`]: notice managed children that exited and read the
    /// managed proxy's health (our own port only; emulators come from doctor).
    fn spawn_child_poller(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok((port, exited)) = this.update(cx, |app, cx| {
                    let mut exited = Vec::new();
                    if let Some(child) = app.managed_proxy.as_mut()
                        && matches!(child.try_wait(), Ok(Some(_)))
                    {
                        exited.push(child.id());
                        app.managed_proxy = None;
                    }
                    if let Some(run) = app.managed_run.as_mut()
                        && let Ok(Some(status)) = run.child.try_wait()
                    {
                        exited.push(run.child.id());
                        let code = status.code().unwrap_or(1);
                        let cmd = run.command_line.clone();
                        app.managed_run = None;
                        app.run_enforce = None;
                        app.session.update(cx, |s, cx| {
                            s.refresh_console_log(cx);
                            s.refresh_lock(cx);
                            s.refresh_runs(cx);
                        });
                        app.show_toast(
                            tr!(
                                "{cmd} exited with code {code}",
                                "{cmd} 已退出 (exit {code})"
                            ),
                            cx,
                        );
                    }
                    (app.proxy_port, exited)
                }) else {
                    break;
                };
                let h = home();
                let probe = cx
                    .background_executor()
                    .spawn(async move {
                        if !exited.is_empty() {
                            daemon::forget_children(&h, &exited);
                        }
                        daemon::probe_proxy(port)
                    })
                    .await;
                let gone = this
                    .update(cx, |app, cx| {
                        if app.proxy != probe {
                            app.proxy = probe;
                            cx.notify();
                        }
                    })
                    .is_err();
                if gone {
                    break;
                }
                cx.background_executor().timer(CHILD_POLL).await;
            }
        })
    }

    /// `lenslet doctor --json` on start and every [`DOCTOR_POLL`] while the
    /// window is active (and again whenever it becomes active).
    fn spawn_doctor_poller(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this
                    .update(cx, |app, cx| {
                        if app.window_active {
                            app.refresh_doctor(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(DOCTOR_POLL).await;
            }
        })
    }

    fn refresh_doctor(&mut self, cx: &mut Context<Self>) {
        if self.doctor_busy {
            return;
        }
        self.doctor_busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { daemon::run_doctor(DOCTOR_TIMEOUT) })
                .await;
            this.update(cx, |app, cx| {
                app.doctor_busy = false;
                if app.doctor.as_ref() != Some(&result) {
                    app.doctor = Some(result);
                    let status = app.emulator_status();
                    app.calls
                        .update(cx, |c, cx| c.set_emulator_status(status.clone(), cx));
                    app.launcher.update(cx, |l, cx| l.set_status(status, cx));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
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

    fn open_popover(&mut self, popover: Popover, cx: &mut Context<Self>) {
        self.modal = Modal::Popover(popover);
        cx.notify();
    }

    pub(super) fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.escape() {
            self.focus_list(window, cx);
            cx.notify();
        }
    }

    pub(super) fn open_launcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.run_settings(cx);
        let recent: Vec<PathBuf> = self
            .prefs
            .recent_workspaces
            .iter()
            .map(PathBuf::from)
            .collect();
        let status = self.emulator_status();
        let active = self.active_enforce();
        self.modal = Modal::Launcher;
        self.launcher.update(cx, |l, cx| {
            l.open(settings, recent, status, active, window, cx);
        });
        self.refresh_doctor(cx);
        cx.notify();
    }

    fn open_editor(
        &mut self,
        call: &Trace,
        target: ReplayTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.modal = Modal::Editor;
        self.composer
            .update(cx, |comp, cx| comp.load(call, target, window, cx));
        cx.notify();
    }

    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_modal(Modal::Settings, window, cx);
        self.storage = None;
        if let Some(dir) = self.session.read(cx).runs_dir() {
            cx.spawn(async move |this, cx| {
                let stats = cx
                    .background_executor()
                    .spawn(async move { crate::feed::storage_stats(&dir) })
                    .await;
                this.update(cx, |app, cx| {
                    app.storage = Some(stats);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    fn confirm_yes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Modal::Confirm(confirm) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.focus_list(window, cx);
        match confirm {
            Question::AcceptAll(entries) => {
                self.session.update(cx, |s, cx| s.accept(entries, cx));
            }
            Question::OverwriteCi {
                path,
                command,
                upstream,
            } => {
                let cwd = path
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.active_workspace_dir(cx));
                self.write_ci(cwd, command, upstream, true, cx);
            }
        }
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
                let entries = self.access_menu(row, cx);
                self.open_popover(Popover::Menu(*position, entries), cx);
            }
            AccessEvent::FilterCalls(access) => {
                self.run_cmd_cx(Cmd::Filter(FilterCmd::Access(access.clone())), cx);
            }
            AccessEvent::SelectCall(id) => {
                let id = id.clone();
                self.set_mode(Mode::Calls, cx);
                self.calls
                    .update(cx, |list, cx| list.select_id(Some(id), true, cx));
            }
            AccessEvent::AcceptAll(entries) => {
                self.modal = Modal::Confirm(Question::AcceptAll(entries.clone()));
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

    /// Appearance or accent changed: publish the new look and re-highlight.
    fn restyle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if Ui::install(cx, Ui::new(&self.prefs, window.appearance())) {
            self.inspector
                .update(cx, |inspector, cx| inspector.restyle(cx));
            self.access_detail
                .update(cx, |detail, cx| detail.restyle(cx));
            self.output.update(cx, |output, cx| output.restyle(cx));
            cx.refresh_windows();
        }
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

    pub(super) fn update_prefs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut UiPrefs),
    ) {
        let prev_lang = self.prefs.lang;
        f(&mut self.prefs);
        if self.prefs.lang != prev_lang && cfg!(target_os = "macos") {
            cx.set_menus(crate::menus(self.prefs.lang));
            cx.set_dock_menu(crate::dock_menu(self.prefs.lang));
        }
        self.restyle(window, cx);
        self.save_prefs(cx);
        cx.notify();
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

    fn copy(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.show_toast(i18n::copied(label), cx);
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

    pub(super) fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match self.mode {
            Mode::Calls => self.calls.focus_handle(cx),
            Mode::Access => self.access.focus_handle(cx),
        };
        window.focus(&focus);
    }

    fn toggle_output(&mut self, cx: &mut Context<Self>) {
        self.prefs.output_open = !self.prefs.output_open;
        let open = self.prefs.output_open;
        if open {
            self.set_mode(Mode::Calls, cx);
        }
        self.output.update(cx, |o, cx| o.set_open(open, cx));
        self.save_prefs(cx);
        cx.notify();
    }

    // -- Commands -----------------------------------------------------------

    /// Commands that need no window.
    fn run_cmd_cx(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        match cmd {
            Cmd::StopRun => self.stop_run(cx),
            Cmd::OpenTerminal => self.open_configured_terminal(cx),
            Cmd::CopyEnv(fmt) => self.copy_env(fmt, cx),
            Cmd::Proxy(true) => self.start_proxy(false, cx),
            Cmd::Proxy(false) => self.stop_proxy(cx),
            Cmd::Demo(true) => self.session.update(cx, |s, cx| s.enter_demo(cx)),
            Cmd::Demo(false) => self.session.update(cx, |s, cx| s.exit_demo(cx)),
            Cmd::OpenWorkspace => self.prompt_open_workspace(cx),
            Cmd::OpenFile => self.prompt_open_file(cx),
            Cmd::ExportReport => self.prompt_export_report(cx),
            Cmd::InitCi => self.init_ci_workflow(cx),
            Cmd::ToggleSidebar => {
                self.prefs.sidebar_open = !self.prefs.sidebar_open;
                self.save_prefs(cx);
                cx.notify();
            }
            Cmd::ToggleInspector => {
                self.prefs.inspector_open = !self.prefs.inspector_open;
                self.save_prefs(cx);
                cx.notify();
            }
            Cmd::ToggleOutput => self.toggle_output(cx),
            Cmd::Replay => self
                .inspector
                .update(cx, |inspector, cx| inspector.replay(cx)),
            Cmd::EditReplay => self
                .inspector
                .update(cx, |inspector, cx| inspector.open_composer(cx)),
            Cmd::CopyText(label, text) => self.copy(label, text, cx),
            Cmd::CopyCurl(call) => {
                let endpoint = self.session.read(cx).header_endpoint().map(str::to_string);
                match to_curl(&call, endpoint.as_deref()) {
                    Some(curl) => self.copy("curl", curl, cx),
                    None => self.show_toast(
                        tr!(
                            "This request's body can't be rebuilt as curl",
                            "该请求体无法重建为 curl"
                        ),
                        cx,
                    ),
                }
            }
            Cmd::CopyPolicy => {
                let rows = self.session.read(cx).access_rows.clone();
                self.copy("policy JSON", policy_json(&rows), cx);
            }
            Cmd::SavePayload => self.inspector.update(cx, |insp, cx| insp.save_payload(cx)),
            Cmd::Filter(filter) => self.apply_filter(filter, cx),
            Cmd::Lock(lock) => self.apply_lock(lock, cx),
            Cmd::Runs(nav) => self.navigate_runs(nav, cx),
            Cmd::SetBaseline(baseline) => {
                self.session
                    .update(cx, |session, cx| session.set_baseline(baseline, cx));
            }
            Cmd::Mode(mode) => self.set_mode(mode, cx),
            Cmd::Quit => {
                self.stop_all_children();
                cx.quit();
            }
            Cmd::Run
            | Cmd::RunOptions
            | Cmd::FocusSearch
            | Cmd::OpenSettings
            | Cmd::OpenAbout
            | Cmd::Pref(_) => {}
        }
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Modal::Popover(_) | Modal::Palette) {
            self.modal = Modal::None;
        }
        match cmd {
            Cmd::Run => self.run_tests(window, cx),
            Cmd::RunOptions => self.open_launcher(window, cx),
            Cmd::FocusSearch => {
                let focus = self.search.read(cx).handle();
                window.focus(&focus);
            }
            Cmd::OpenSettings => self.open_settings(window, cx),
            Cmd::OpenAbout => self.open_modal(Modal::About, window, cx),
            Cmd::Pref(pref) => self.update_prefs(window, cx, |p| match pref {
                PrefCmd::Theme(mode) => p.theme = mode,
                PrefCmd::Accent(accent) => p.accent = accent,
                PrefCmd::Density(density) => p.density = density,
                PrefCmd::Lang(lang) => p.lang = lang,
            }),
            Cmd::Mode(mode) => {
                self.set_mode(mode, cx);
                self.focus_list(window, cx);
            }
            other => self.run_cmd_cx(other, cx),
        }
        cx.notify();
    }

    fn apply_filter(&mut self, filter: FilterCmd, cx: &mut Context<Self>) {
        match filter {
            FilterCmd::Status(status) => {
                self.set_mode(Mode::Calls, cx);
                self.calls
                    .update(cx, |list, cx| list.set_status(status, cx));
            }
            FilterCmd::Service(service) => {
                self.set_mode(Mode::Calls, cx);
                self.calls
                    .update(cx, |list, cx| list.set_service(Some(service), cx));
            }
            FilterCmd::Access(access) => {
                self.set_mode(Mode::Calls, cx);
                self.calls.update(cx, |list, cx| {
                    let mut filter = list.filter.clone();
                    filter.access = Some(access);
                    list.set_filter(filter, cx);
                });
            }
            FilterCmd::Clear => {
                self.calls.update(cx, |list, cx| {
                    list.set_filter(crate::model::CallFilter::default(), cx)
                });
                self.access.update(cx, |view, cx| {
                    view.clear_filter(cx);
                });
            }
        }
    }

    fn apply_lock(&mut self, lock: LockCmd, cx: &mut Context<Self>) {
        match lock {
            LockCmd::Accept(entries) => {
                let entries = entries
                    .into_iter()
                    .map(|(a, implied)| (a, false, implied))
                    .collect();
                self.session
                    .update(cx, |session, cx| session.accept(entries, cx));
            }
            LockCmd::Remove(entries) => {
                self.session
                    .update(cx, |session, cx| session.remove_from_lock(entries, cx));
            }
            LockCmd::TestMark(access, test) => {
                self.session
                    .update(cx, |session, cx| session.set_test_mark(access, test, cx));
            }
            LockCmd::ReviewNotInLock => {
                self.set_mode(Mode::Access, cx);
                self.access.update(cx, |view, cx| view.show_not_in_lock(cx));
            }
        }
    }

    fn navigate_runs(&mut self, nav: RunNav, cx: &mut Context<Self>) {
        match nav {
            RunNav::Prev => self.session.update(cx, |s, cx| s.step_run(-1, cx)),
            RunNav::Next => self.session.update(cx, |s, cx| s.step_run(1, cx)),
            RunNav::Latest => self.follow_latest(cx),
            RunNav::Open(path) => self.pin_run(path, cx),
            RunNav::Reload => {
                if self.session.read(cx).demo {
                    self.show_toast(tr!("Demo data is static", "演示数据为静态快照"), cx);
                } else {
                    self.session.update(cx, |session, cx| session.reload(cx));
                }
            }
            RunNav::LoadAll => self.session.update(cx, |s, cx| s.load_all_calls(cx)),
        }
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

    fn prompt_open_workspace(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(dir) = paths.into_iter().next()
            {
                this.update(cx, |app, cx| app.open_classified_path(dir, true, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn init_ci_workflow(&mut self, cx: &mut Context<Self>) {
        let command = self.detected_cmd.clone().or_else(|| {
            self.session
                .read(cx)
                .run
                .as_ref()
                .filter(|r| !r.header.command.is_empty())
                .map(|r| r.command_line())
        });
        let Some(command) = command else {
            self.show_toast(
                tr!(
                    "Set a test command first (▾ next to ▶)",
                    "请先设置测试命令（▶ 旁的 ▾）"
                ),
                cx,
            );
            return;
        };
        let upstream = self.prefs.proxy_upstream.clone();
        let cwd = self.active_workspace_dir(cx);
        self.write_ci(cwd, command, upstream, false, cx);
    }

    fn write_ci(
        &mut self,
        cwd: PathBuf,
        command: String,
        upstream: Option<String>,
        overwrite: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let (cmd, up) = (command.clone(), upstream.clone());
            let res =
                cx.background_executor()
                    .spawn(async move {
                        daemon::generate_ci_workflow(&cwd, &cmd, up.as_deref(), overwrite)
                    })
                    .await;
            this.update(cx, |app, cx| match res {
                Ok(path) => {
                    let p = path.display();
                    app.show_toast(tr!("Wrote {p}", "已生成 {p}"), cx);
                }
                Err(CiError::Exists(path)) => {
                    app.modal = Modal::Confirm(Question::OverwriteCi {
                        path,
                        command,
                        upstream,
                    });
                    cx.notify();
                }
                Err(CiError::Failed(err)) => app.show_toast(err, cx),
            })
            .ok();
        })
        .detach();
    }

    fn prompt_export_report(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let report = session
            .run
            .as_ref()
            .and_then(|r| r.report.clone())
            .unwrap_or_else(|| {
                let run_id = session
                    .run
                    .as_ref()
                    .map(|r| r.header.run.clone())
                    .unwrap_or_default();
                session.store.live_report(&run_id)
            });
        let Ok(json) = serde_json::to_string_pretty(&report) else {
            return;
        };
        let dir = self.active_workspace_dir(cx);
        let rx = cx.prompt_for_new_path(&dir, Some("lenslet-report.json"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let target = path.clone();
                let res = cx
                    .background_executor()
                    .spawn(async move {
                        crate::fsguard::assert_io_allowed();
                        std::fs::write(&target, json)
                    })
                    .await;
                this.update(cx, |app, cx| {
                    let p = path.display();
                    let msg = match res {
                        Ok(()) => tr!("Saved {p}", "已保存 {p}"),
                        Err(e) => tr!("Could not save {p}: {e}", "无法保存 {p}：{e}"),
                    };
                    app.show_toast(msg, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn install_cli(&mut self, cx: &mut Context<Self>) {
        let h = home();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { daemon::install_cli_to_path(&h) })
                .await;
            this.update(cx, |app, cx| {
                let msg = match res {
                    Ok(dest) => {
                        let d = dest.display();
                        tr!("Installed lenslet to {d}", "已将 lenslet 安装至 {d}")
                    }
                    Err(err) => err,
                };
                app.show_toast(msg, cx);
            })
            .ok();
        })
        .detach();
    }

    fn follow_latest(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).demo {
            self.session.update(cx, |s, cx| s.exit_demo(cx));
        }
        let dir = self
            .session
            .read(cx)
            .source
            .runs_dir()
            .map(|d| d.to_path_buf());
        if let Some(dir) = dir {
            self.session.update(cx, |session, cx| {
                session.switch(Source::Runs { dir, pinned: None }, cx)
            });
        }
    }

    fn pin_run(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.session.read(cx).demo {
            self.session.update(cx, |s, cx| s.exit_demo(cx));
        }
        let dir = self
            .session
            .read(cx)
            .source
            .runs_dir()
            .map(|d| d.to_path_buf());
        let newest = self
            .session
            .read(cx)
            .runs
            .first()
            .map(|run| run.path.clone());
        if let Some(dir) = dir {
            let pinned = (newest.as_ref() != Some(&path)).then_some(path);
            self.session.update(cx, |session, cx| {
                session.switch(Source::Runs { dir, pinned }, cx)
            });
        } else {
            self.session
                .update(cx, |session, cx| session.switch(Source::File(path), cx));
        }
    }

    fn call_menu(&self, call: Arc<Trace>, cx: &App) -> Vec<MenuEntry> {
        let ui = Ui::of(cx);
        let mut entries = vec![MenuEntry {
            title: ui.t("Copy as curl", "复制为 curl").into(),
            hint: Some(i18n::shortcut("⇧C")),
            cmd: Cmd::CopyCurl(call.clone()),
        }];
        entries.push(MenuEntry {
            title: ui.t("Save Payload…", "保存报文…").into(),
            hint: None,
            cmd: Cmd::SavePayload,
        });
        if let Some(first) = call.access.first().or(call.implied.first()) {
            entries.push(MenuEntry {
                title: ui.t("Copy permission", "复制权限").into(),
                hint: None,
                cmd: Cmd::CopyText("permission", format!("{} {}", first.action, first.resource)),
            });
            entries.push(MenuEntry {
                title: ui.t("Copy resource ARN", "复制资源 ARN").into(),
                hint: None,
                cmd: Cmd::CopyText("ARN", first.resource.clone()),
            });
        }
        entries.push(MenuEntry {
            title: ui.t("Copy call id", "复制调用 ID").into(),
            hint: None,
            cmd: Cmd::CopyText("id", call.id.clone()),
        });
        if self.inspector.read(cx).can_replay(cx) {
            entries.push(MenuEntry {
                title: ui.t("Replay", "重放").into(),
                hint: Some(i18n::shortcut("⏎")),
                cmd: Cmd::Replay,
            });
            entries.push(MenuEntry {
                title: ui.t("Edit & Replay…", "编辑并重放…").into(),
                hint: Some(i18n::shortcut("⇧⏎")),
                cmd: Cmd::EditReplay,
            });
        }
        entries.push(MenuEntry {
            title: format!("{} {}", ui.t("Only", "仅显示"), call.service_name()).into(),
            hint: None,
            cmd: Cmd::Filter(FilterCmd::Service(call.service_key().to_string())),
        });
        entries
    }

    fn access_menu(&self, row: &AccessRow, cx: &App) -> Vec<MenuEntry> {
        let ui = Ui::of(cx);
        let editable = self.session.read(cx).can_edit_lock();
        let mut entries = Vec::new();
        if editable && row.used && !row.in_lock {
            entries.push(MenuEntry {
                title: ui.t("Accept into lock", "写入 lock").into(),
                hint: None,
                cmd: Cmd::Lock(LockCmd::Accept(vec![(row.access.clone(), row.implied)])),
            });
        }
        if editable && row.in_lock {
            entries.push(MenuEntry {
                title: ui.t("Remove from lock", "从 lock 移除").into(),
                hint: None,
                cmd: Cmd::Lock(LockCmd::Remove(vec![row.access.clone()])),
            });
        }
        if editable && (row.in_lock || row.used) {
            entries.push(MenuEntry {
                title: if row.test {
                    ui.t("Unmark # test", "取消 # test").into()
                } else {
                    ui.t("Mark # test", "标记 # test").into()
                },
                hint: None,
                cmd: Cmd::Lock(LockCmd::TestMark(row.access.clone(), !row.test)),
            });
        }
        entries.push(MenuEntry {
            title: ui.t("Copy statement JSON", "复制 Statement JSON").into(),
            hint: None,
            cmd: Cmd::CopyText("statement", statement_json(&row.access)),
        });
        entries.push(MenuEntry {
            title: ui.t("Copy resource ARN", "复制资源 ARN").into(),
            hint: None,
            cmd: Cmd::CopyText("ARN", row.access.resource.clone()),
        });
        if row.used {
            entries.push(MenuEntry {
                title: ui.t("Show calls", "查看调用").into(),
                hint: None,
                cmd: Cmd::Filter(FilterCmd::Access(row.access.clone())),
            });
        }
        entries
    }

    // -- Keyboard -------------------------------------------------------------

    fn select(&mut self, f: impl FnOnce(SelectTarget, &mut App), cx: &mut Context<Self>) {
        if self.modal.blocks_list() {
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
            Modal::Palette => self.palette_confirm(window, cx),
            // A focused dialog button clicks itself on Enter; only act when
            // the dialog itself has focus.
            Modal::Confirm(_) if self.root_focus.is_focused(window) => self.confirm_yes(window, cx),
            Modal::None if self.mode == Mode::Calls && self.prefs.inspector_open => {
                let focus = self.inspector.read(cx).body().focus_handle(cx);
                window.focus(&focus);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Enter inside a dialog's text field.
    fn submit_field(&mut self, cx: &mut Context<Self>) {
        match self.modal {
            Modal::Launcher => self.launcher.update(cx, |l, cx| l.save(cx)),
            Modal::Editor => self.composer.update(cx, |c, cx| c.send(cx)),
            _ => {}
        }
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

// -- Render ----------------------------------------------------------------------

impl LensletApp {
    fn render_header(&self, ui: &Ui, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = ui.p;
        let session = self.session.read(cx);
        let mac = cfg!(target_os = "macos");
        let mode = self.mode;
        let entity = cx.entity();
        let modes = widgets::segmented(
            ui,
            vec![
                widgets::Segment {
                    id: "mode-calls",
                    label: ui.t("Calls", "请求").into(),
                    active: mode == Mode::Calls,
                    tooltip: Some((
                        ui.t("Every AWS call", "全部 AWS 调用").into(),
                        Some(i18n::shortcut("1")),
                    )),
                },
                widgets::Segment {
                    id: "mode-access",
                    label: ui.t("Access", "权限").into(),
                    active: mode == Mode::Access,
                    tooltip: Some((
                        ui.t("IAM permissions the run needed", "本次运行所需的 IAM 权限")
                            .into(),
                        Some(i18n::shortcut("2")),
                    )),
                },
            ],
            move |index, window, cx| {
                let mode = [Mode::Calls, Mode::Access][index];
                entity.update(cx, |app, cx| app.run_cmd(Cmd::Mode(mode), window, cx));
            },
        );

        let compact = window.viewport_size().width < px(1_220.0);
        let app_menu_open = matches!(self.modal, Modal::Popover(Popover::AppMenu));

        let mut bar = div()
            .id("titlebar")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(8.0))
            .h(px(theme::HEADER_H))
            .pl(px(if mac { 80.0 } else { 10.0 }))
            .pr(px(10.0))
            .bg(rgb(t.header))
            .border_b_1()
            .border_color(rgb(t.border));

        if mac {
            bar = bar
                .window_control_area(WindowControlArea::Drag)
                .on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                });
        } else {
            let mut menu_wrap = div().relative().child(
                widgets::icon_button(
                    ui,
                    "app-menu-btn",
                    app_menu_open,
                    glyph::menu_icon(theme::h(t.text_dim, 1.0)),
                )
                .tooltip(widgets::tip(ui.t("Menu", "菜单"), None))
                .on_click(cx.listener(|app, _, _, cx| {
                    if matches!(app.modal, Modal::Popover(Popover::AppMenu)) {
                        app.modal = Modal::None;
                        cx.notify();
                    } else {
                        app.open_popover(Popover::AppMenu, cx);
                    }
                })),
            );
            if app_menu_open {
                menu_wrap = menu_wrap.child(deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .offset(point(px(0.0), px(32.0)))
                        .snap_to_window_with_margin(px(8.0))
                        .child(self.render_app_menu_popover(ui, cx)),
                ));
            }
            bar = bar.child(menu_wrap);
        }

        bar.child(
            widgets::icon_button(
                ui,
                "toggle-sidebar",
                self.prefs.sidebar_open,
                glyph::left_sidebar_icon(theme::h(t.text_dim, 1.0), self.prefs.sidebar_open),
            )
            .tooltip(widgets::tip(
                ui.t("Project & runs", "项目与运行"),
                Some(i18n::shortcut("0")),
            ))
            .on_click(
                cx.listener(|app, _, window, cx| app.run_cmd(Cmd::ToggleSidebar, window, cx)),
            ),
        )
        .child(self.render_run_picker(ui, session, cx))
        .child(self.render_run_button(ui, compact, cx))
        .child(modes)
        .child(div().flex_1())
        .child(self.render_baseline_button(ui, session, cx))
        .child(
            div()
                .w(px(if compact { 184.0 } else { 276.0 }))
                .child(self.search.clone()),
        )
    }

    /// The one place that runs: ▶ <command> (or ■ Stop) and ▾ for options.
    fn render_run_button(&self, ui: &Ui, compact: bool, cx: &Context<Self>) -> AnyElement {
        if self.managed_run.is_some() {
            return widgets::danger_button(ui, "hdr-stop-run", ui.t("■ Stop", "■ 停止"))
                .tooltip(widgets::tip(
                    ui.t("Stop the run", "停止运行"),
                    Some(i18n::shortcut(".")),
                ))
                .on_click(cx.listener(|app, _, window, cx| app.run_cmd(Cmd::StopRun, window, cx)))
                .into_any_element();
        }
        let status = self.emulator_status();
        let (label, tooltip) = match self.detected_cmd.as_deref() {
            _ if self.in_flight.starting_run() => (
                ui.t("Starting…", "正在启动…").to_string(),
                ui.t("Starting the run", "正在启动运行").to_string(),
            ),
            Some(cmd) => (
                format!("▶ {}", ellipsize(cmd, if compact { 12 } else { 18 })),
                format!(
                    "{} {cmd} · {}",
                    ui.t("Run", "运行"),
                    ui.tr(&status.message())
                ),
            ),
            None => (
                ui.t("Set test command…", "设置测试命令…").to_string(),
                ui.t("Choose the command ▶ runs", "选择 ▶ 要运行的命令")
                    .to_string(),
            ),
        };
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.0))
            .child(
                widgets::primary_button(ui, "hdr-run", label)
                    .tooltip(widgets::tip(tooltip, Some(i18n::shortcut("R"))))
                    .on_click(cx.listener(|app, _, window, cx| app.run_cmd(Cmd::Run, window, cx))),
            )
            .child(
                widgets::button(ui, "hdr-run-options", "▾")
                    .px(px(6.0))
                    .tooltip(widgets::tip(
                        ui.t("Run options…", "运行选项…"),
                        Some(i18n::shortcut("N")),
                    ))
                    .on_click(
                        cx.listener(|app, _, window, cx| app.run_cmd(Cmd::RunOptions, window, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_app_menu_popover(&self, ui: &Ui, cx: &Context<Self>) -> AnyElement {
        let mut card = widgets::popover_card(ui, "app-menu-popover", 248.0).on_mouse_down_out(
            cx.listener(|app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            }),
        );
        let items: Vec<Option<MenuEntry>> = vec![
            Some(MenuEntry {
                title: ui.t("Run Options…", "运行选项…").into(),
                hint: Some(i18n::shortcut("N")),
                cmd: Cmd::RunOptions,
            }),
            Some(MenuEntry {
                title: ui.t("Open Project Folder…", "打开项目目录…").into(),
                hint: None,
                cmd: Cmd::OpenWorkspace,
            }),
            Some(MenuEntry {
                title: ui.t("Open Run File…", "打开运行文件…").into(),
                hint: Some(i18n::shortcut("O")),
                cmd: Cmd::OpenFile,
            }),
            Some(MenuEntry {
                title: ui.t("Export Run Summary…", "导出运行摘要…").into(),
                hint: Some(i18n::shortcut("S")),
                cmd: Cmd::ExportReport,
            }),
            None,
            Some(MenuEntry {
                title: ui.t("Commands…", "命令…").into(),
                hint: Some(i18n::shortcut("K")),
                cmd: Cmd::FocusSearch,
            }),
            Some(MenuEntry {
                title: ui.t("Settings…", "设置…").into(),
                hint: Some(i18n::shortcut(",")),
                cmd: Cmd::OpenSettings,
            }),
            Some(MenuEntry {
                title: ui.t("About Lenslet", "关于 Lenslet").into(),
                hint: None,
                cmd: Cmd::OpenAbout,
            }),
            None,
            Some(MenuEntry {
                title: ui.t("Quit Lenslet", "退出 Lenslet").into(),
                hint: Some(i18n::shortcut("Q")),
                cmd: Cmd::Quit,
            }),
        ];
        for (ix, item) in items.into_iter().enumerate() {
            match item {
                None => card = card.child(widgets::separator(ui)),
                Some(entry) => {
                    let cmd = entry.cmd.clone();
                    let palette = ix == 5;
                    card = card.child(
                        widgets::menu_item(
                            ui,
                            ElementId::named_usize("app-m", ix),
                            entry.title,
                            entry.hint,
                            false,
                            false,
                        )
                        .on_click(cx.listener(
                            move |app, _, window, cx| {
                                if palette {
                                    app.modal = Modal::None;
                                    app.toggle_palette(window, cx);
                                } else {
                                    app.run_cmd(cmd.clone(), window, cx)
                                }
                            },
                        )),
                    );
                }
            }
        }
        widgets::appear(ui, "app-menu-anim", card)
    }

    fn render_run_picker(&self, ui: &Ui, session: &Session, cx: &Context<Self>) -> Div {
        let t = ui.p;
        let mut against: Option<String> = None;
        let (title, status, status_color) = match (&session.health, session.run.as_ref()) {
            (Health::Loading, _) => (
                ui.t("Loading…", "加载中…").to_string(),
                String::new(),
                t.text_muted,
            ),
            (Health::Missing, _) => (
                ui.t("No runs yet", "还没有运行").to_string(),
                String::new(),
                t.text_muted,
            ),
            (Health::Error(_), _) => (
                ui.t("Can't read run", "无法读取运行").to_string(),
                String::new(),
                t.red,
            ),
            (Health::Ready, Some(run)) => {
                against = run
                    .header
                    .emulator_name()
                    .map(|name| ui.tr(&tr!("against {name}", "使用 {name}")).to_string());
                let title = if run.header.command.is_empty() {
                    ui.t("proxy session", "代理会话").to_string()
                } else {
                    run.command_line()
                };
                match (&run.report, session.feed_mode) {
                    (Some(report), _) => match report.exit_code {
                        Some(0) => (title, "exit 0".into(), t.green),
                        Some(code) => (title, format!("exit {code}"), t.red),
                        None => (title, ui.t("ended", "已结束").into(), t.text_muted),
                    },
                    (None, Some(FeedMode::Push)) => (title, ui.t("live", "实时").into(), t.green),
                    (None, Some(FeedMode::Tail)) => {
                        (title, ui.t("live · tail", "实时 · 轮询").into(), t.amber)
                    }
                    (None, _) => (title, ui.t("unfinished", "未结束").into(), t.amber),
                }
            }
            (Health::Ready, None) => (
                ui.t("Demo", "演示").to_string(),
                String::new(),
                t.text_muted,
            ),
        };
        let position = session
            .run_position()
            .map(|(i, n)| format!("{i}/{n}"))
            .unwrap_or_default();
        let open = matches!(self.modal, Modal::Popover(Popover::Runs));
        let button = div()
            .id("run-switcher")
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(28.0))
            .max_w(px(360.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .when(open, |el| el.bg(theme::h(t.text, 0.08)))
            .cursor_pointer()
            .hover(move |s| s.bg(theme::h(t.text, 0.06)))
            .tooltip(widgets::tip(
                ui.t("Switch run", "切换运行"),
                Some(format!("{} / {}", i18n::shortcut("["), i18n::shortcut("]"))),
            ))
            .on_click(cx.listener(|app, _, _, cx| {
                if matches!(app.modal, Modal::Popover(Popover::Runs)) {
                    app.modal = Modal::None;
                    cx.notify();
                } else {
                    app.session.update(cx, |s, cx| s.refresh_runs(cx));
                    app.open_popover(Popover::Runs, cx);
                }
            }))
            .when(!status.is_empty(), |el| {
                el.child(glyph::dot(theme::h(status_color, 1.0), 7.0))
            })
            .child(
                div()
                    .min_w(px(0.0))
                    .font(fonts::mono_font())
                    .text_size(px(text::MD))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(t.text))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title),
            )
            .when(!status.is_empty(), |el| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(text::SM))
                        .text_color(rgb(status_color))
                        .child(status),
                )
            })
            .when_some(against, |el, against| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(text::SM))
                        .text_color(rgb(t.text_muted))
                        .child(against),
                )
            })
            .when(!position.is_empty(), |el| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .font(fonts::mono_font())
                        .text_size(px(text::XS))
                        .text_color(rgb(t.text_muted))
                        .child(position),
                )
            })
            .child(glyph::chevron_down(theme::h(t.text_muted, 1.0)));
        let mut wrapper = div().relative().flex_shrink().min_w(px(0.0)).child(button);
        if open {
            wrapper = wrapper.child(deferred(
                anchored()
                    .anchor(Corner::TopLeft)
                    .offset(point(px(0.0), px(32.0)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(self.render_runs_popover(ui, session, cx)),
            ));
        }
        wrapper
    }

    fn render_runs_popover(&self, ui: &Ui, session: &Session, cx: &Context<Self>) -> AnyElement {
        let t = ui.p;
        let mut card = widgets::popover_card(ui, "runs-popover", 480.0)
            .max_h(px(420.0))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            }));
        card = card.child(
            widgets::menu_item(
                ui,
                "open-trace-file",
                ui.t("Open Run File…", "打开运行文件…"),
                Some(i18n::shortcut("O")),
                false,
                false,
            )
            .on_click(cx.listener(|app, _, window, cx| app.run_cmd(Cmd::OpenFile, window, cx))),
        );
        if session.source.runs_dir().is_some() {
            card = card.child(
                widgets::menu_item(
                    ui,
                    "follow-latest",
                    ui.t("Follow the newest run", "跟随最新运行"),
                    None,
                    session.source.follows_latest(),
                    false,
                )
                .on_click(cx.listener(|app, _, window, cx| {
                    app.run_cmd(Cmd::Runs(RunNav::Latest), window, cx)
                })),
            );
        }
        card = card.child(widgets::separator(ui));
        if session.runs.is_empty() {
            card = card.child(
                div()
                    .p(px(12.0))
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t("No recorded runs", "没有运行记录")),
            );
        }
        const SHOWN: usize = 40;
        for (index, run) in session.runs.iter().take(SHOWN).enumerate() {
            let current = session.path.as_ref() == Some(&run.path);
            let (status, color) = match run.report.as_ref() {
                Some(report) => match report.exit_code {
                    Some(0) => (ui.t("pass", "通过").to_string(), t.green),
                    Some(code) => (format!("exit {code}"), t.red),
                    None => (ui.t("ended", "已结束").into(), t.text_muted),
                },
                None => (ui.t("live", "实时").into(), t.amber),
            };
            let calls = run
                .report
                .as_ref()
                .map(|r| r.calls.to_string())
                .unwrap_or_default();
            let path = run.path.clone();
            let pin_prefix = if run.pinned { "★ " } else { "" };
            let mut detail = vec![run.project_name(), run.id.clone()];
            if let Some(name) = run.emulator() {
                detail.push(ui.tr(&tr!("against {name}", "使用 {name}")).to_string());
            }
            detail.push(format_bytes(run.bytes as usize));
            card = card.child(
                div()
                    .id(ElementId::named_usize("run", index))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(36.0))
                    .flex_shrink_0()
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .when(current, |el| el.bg(theme::h(t.accent, 0.14)))
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme::h(t.accent, 0.10)))
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.run_cmd(Cmd::Runs(RunNav::Open(path.clone())), window, cx)
                    }))
                    .child(glyph::dot(theme::h(color, 1.0), 6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font(fonts::mono_font())
                                    .text_size(px(text::MD))
                                    .text_color(rgb(t.text))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(format!("{pin_prefix}{}", run.label())),
                            )
                            .child(
                                div()
                                    .font(fonts::mono_font())
                                    .text_size(px(text::XS))
                                    .text_color(rgb(t.text_muted))
                                    .child(detail.join(" · ")),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font(fonts::mono_font())
                            .text_size(px(text::SM))
                            .text_color(rgb(t.text_muted))
                            .child(calls),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(48.0))
                            .text_right()
                            .text_size(px(text::SM))
                            .text_color(rgb(color))
                            .child(status),
                    ),
            );
        }
        if session.runs.len() > SHOWN {
            let more = session.runs.len() - SHOWN;
            card = card.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_muted))
                    .child(
                        ui.tr(&tr!("{more} more in the sidebar…", "侧栏中还有 {more} 个…"))
                            .to_string(),
                    ),
            );
        }
        widgets::appear(ui, "runs-popover-anim", card)
    }

    fn render_baseline_button(&self, ui: &Ui, session: &Session, cx: &Context<Self>) -> Div {
        let t = ui.p;
        let label: String = match (&session.baseline, session.baseline_run.as_ref()) {
            (Baseline::Lock, _) if session.lock_path.is_some() && session.lock.is_none() => ui
                .t("lenslet.lock (none yet)", "lenslet.lock（尚无）")
                .into(),
            (Baseline::Lock, _) => "lenslet.lock".into(),
            (Baseline::Previous, Some(_)) => ui.t("previous run", "上一次运行").into(),
            (Baseline::Previous, None) => ui.t("previous run (none)", "上一次运行（无）").into(),
            (Baseline::Run(path), _) => lenslet_core::runs::run_id(path),
        };
        let summary: Option<(String, u32)> = match session.diff.as_ref() {
            Some(diff) => {
                let mut parts = Vec::new();
                let n = diff.new_failures.len();
                if n > 0 {
                    parts.push(ui.tr(&tr!("{n} new fail", "{n} 新增失败")).to_string());
                }
                let n = diff.fixed_failures.len();
                if n > 0 {
                    parts.push(ui.tr(&tr!("{n} fixed", "{n} 已修复")).to_string());
                }
                let n = diff.access_added.len();
                if n > 0 {
                    parts.push(ui.tr(&tr!("+{n} access", "+{n} 新增权限")).to_string());
                }
                if !diff.access_removed.is_empty() {
                    parts.push(format!("−{}", diff.access_removed.len()));
                }
                (!parts.is_empty()).then(|| {
                    (
                        parts.join(" · "),
                        if diff.new_failures.is_empty() {
                            t.amber
                        } else {
                            t.red
                        },
                    )
                })
            }
            None => {
                let added = session
                    .access_rows
                    .iter()
                    .filter(|row| row.delta == crate::store::AccessDelta::Added)
                    .count();
                (added > 0).then(|| (i18n::not_in_lock_badge(added, ui.lang), t.amber))
            }
        };
        let open = matches!(self.modal, Modal::Popover(Popover::Baseline));
        let button = div()
            .id("baseline")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .when(open, |el| el.bg(theme::h(t.text, 0.08)))
            .cursor_pointer()
            .hover(move |s| s.bg(theme::h(t.text, 0.06)))
            .tooltip(widgets::tip(
                ui.t(
                    "What this run is compared with: badges, the Changed filter and Access +/−",
                    "本次运行的对比基线：徽标、“有变化”筛选与权限 +/−",
                ),
                None,
            ))
            .on_click(cx.listener(|app, _, _, cx| {
                if matches!(app.modal, Modal::Popover(Popover::Baseline)) {
                    app.modal = Modal::None;
                    cx.notify();
                } else {
                    app.open_popover(Popover::Baseline, cx);
                }
            }))
            .child(
                div()
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t("Compared with", "对比")),
            )
            .child(
                div()
                    .font(fonts::mono_font())
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_soft))
                    .child(label),
            )
            .when_some(summary, |el, (summary, color)| {
                el.child(
                    div()
                        .font(fonts::mono_font())
                        .text_size(px(text::XS))
                        .text_color(rgb(color))
                        .child(summary),
                )
            })
            .child(glyph::chevron_down(theme::h(t.text_muted, 1.0)));
        let mut wrapper = div().relative().flex_shrink_0().child(button);
        if open {
            let mut card = widgets::popover_card(ui, "baseline-popover", 320.0)
                .max_h(px(360.0))
                .overflow_y_scroll()
                .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                    app.modal = Modal::None;
                    cx.notify();
                }));
            let mut options = vec![
                (Baseline::Lock, "lenslet.lock".to_string()),
                (
                    Baseline::Previous,
                    ui.t("Previous run", "上一次运行").to_string(),
                ),
            ];
            for run in session
                .runs
                .iter()
                .filter(|r| r.ended() && Some(&r.path) != session.path.as_ref())
                .take(12)
            {
                options.push((
                    Baseline::Run(run.path.clone()),
                    format!("{} · {}", run.id, run.label()),
                ));
            }
            for (index, (baseline, title)) in options.into_iter().enumerate() {
                let selected = session.baseline == baseline;
                card = card.child(
                    widgets::menu_item(
                        ui,
                        ElementId::named_usize("baseline-opt", index),
                        title,
                        None,
                        selected,
                        false,
                    )
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.run_cmd(Cmd::SetBaseline(baseline.clone()), window, cx)
                    })),
                );
                if index == 1 {
                    card = card.child(widgets::separator(ui));
                }
            }
            wrapper = wrapper.child(deferred(
                anchored()
                    .anchor(Corner::TopRight)
                    .offset(point(px(0.0), px(32.0)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(widgets::appear(ui, "baseline-anim", card)),
            ));
        }
        wrapper
    }

    fn render_menu(
        &self,
        ui: &Ui,
        position: Point<Pixels>,
        entries: &[MenuEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = widgets::popover_card(ui, "context-menu", 240.0).on_mouse_down_out(
            cx.listener(|app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            }),
        );
        for (index, entry) in entries.iter().enumerate() {
            let cmd = entry.cmd.clone();
            card = card.child(
                widgets::menu_item(
                    ui,
                    ElementId::named_usize("menu", index),
                    entry.title.clone(),
                    entry.hint.clone(),
                    false,
                    false,
                )
                .on_click(
                    cx.listener(move |app, _, window, cx| app.run_cmd(cmd.clone(), window, cx)),
                ),
            );
        }
        deferred(
            anchored()
                .position(position)
                .snap_to_window_with_margin(px(8.0))
                .child(widgets::appear(ui, "menu-anim", card)),
        )
        .with_priority(2)
        .into_any_element()
    }

    fn render_confirm(&self, ui: &Ui, confirm: &Question, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let (title, detail, items, yes): (String, String, Vec<String>, String) = match confirm {
            Question::AcceptAll(entries) => {
                let n = entries.len();
                (
                    ui.tr(&tr!(
                        "Accept {n} permissions into lenslet.lock?",
                        "将 {n} 项权限写入 lenslet.lock？"
                    ))
                    .to_string(),
                    ui.t(
                        "Your tests will be allowed to use these from now on:",
                        "之后测试将被允许使用以下权限：",
                    )
                    .to_string(),
                    entries
                        .iter()
                        .map(|(a, _, implied)| {
                            format!(
                                "{} {}{}",
                                a.action,
                                a.resource,
                                if *implied { "  # implied" } else { "" }
                            )
                        })
                        .collect(),
                    ui.tr(&tr!("Accept all {n}", "全部写入（{n}）")).to_string(),
                )
            }
            Question::OverwriteCi { path, .. } => (
                ui.t("Replace the CI workflow?", "替换 CI 工作流？")
                    .to_string(),
                {
                    let p = path.display();
                    ui.tr(&tr!(
                        "{p} already exists. Replace it with Lenslet's workflow?",
                        "{p} 已存在。要用 Lenslet 的工作流替换它吗？"
                    ))
                    .to_string()
                },
                Vec::new(),
                ui.t("Replace", "替换").to_string(),
            ),
        };
        let mut list = div()
            .id("confirm-list")
            .flex()
            .flex_col()
            .max_h(px(260.0))
            .overflow_y_scroll()
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.code))
            .p(px(8.0));
        for item in &items {
            list = list.child(
                div()
                    .font(fonts::mono_font())
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_soft))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(item.clone()),
            );
        }
        let card = widgets::dialog_card(ui, "confirm-modal", 520.0, title)
            .on_mouse_down_out(cx.listener(|app, _, window, cx| app.close_modal(window, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(18.0))
                    .child(
                        div()
                            .text_size(px(text::MD))
                            .text_color(rgb(t.text_soft))
                            .child(detail),
                    )
                    .when(!items.is_empty(), |el| el.child(list)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .px(px(18.0))
                    .py(px(12.0))
                    .border_t_1()
                    .border_color(rgb(t.border_subtle))
                    .child(
                        widgets::tabbable(
                            ui,
                            widgets::button(ui, "confirm-no", ui.t("Cancel", "取消")),
                        )
                        .on_click(cx.listener(|app, _, window, cx| app.close_modal(window, cx))),
                    )
                    .child(
                        widgets::tabbable(ui, widgets::primary_button(ui, "confirm-yes", yes))
                            .on_click(
                                cx.listener(|app, _, window, cx| app.confirm_yes(window, cx)),
                            ),
                    ),
            );
        widgets::scrim(ui)
            .child(widgets::appear(ui, "confirm-anim", card))
            .into_any_element()
    }

    fn render_splitter(&self, ui: &Ui, cx: &mut Context<Self>) -> Stateful<Div> {
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

    fn render_output_splitter(&self, ui: &Ui) -> Stateful<Div> {
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
                        .text_size(px(text::MD))
                        .text_color(rgb(t.text_soft))
                        .child(ui.tr(message).to_string()),
                )
                .into_any_element(),
        )
    }
}

impl Render for LensletApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::fsguard::report_frame();
        crate::fsguard::milestone_once("first frame");
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let viewport = f32::from(window.viewport_size().width);
        let viewport_h = f32::from(window.viewport_size().height);
        let sidebar_offset = if self.prefs.sidebar_open {
            SIDEBAR_W
        } else {
            0.0
        };
        let show_inspector = self.prefs.inspector_open;
        let inspector_w = self
            .prefs
            .inspector_width
            .clamp(320.0, (viewport - sidebar_offset - 420.0).max(320.0));

        let mut views = div().flex().flex_row().flex_1().min_h(px(0.0));
        match self.mode {
            Mode::Calls => {
                views = views.child(cached(&self.calls));
                if show_inspector {
                    views = views.child(self.render_splitter(&ui, cx)).child(
                        div()
                            .w(px(inspector_w))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .child(cached(&self.inspector)),
                    );
                }
            }
            Mode::Access => {
                views = views.child(cached(&self.access));
                if show_inspector {
                    views = views.child(self.render_splitter(&ui, cx)).child(
                        div()
                            .w(px(inspector_w))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .child(cached(&self.access_detail)),
                    );
                }
            }
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
        let mut workspace = div().flex().flex_row().flex_1().min_h(px(0.0));
        if self.prefs.sidebar_open {
            workspace = workspace.child(self.render_sidebar(&ui, cx));
        }
        workspace = workspace.child(main);

        let header = self.render_header(&ui, window, cx);
        let modal = self.modal.clone();
        let overlay: Option<AnyElement> = match &modal {
            Modal::None | Modal::Popover(Popover::AppMenu | Popover::Runs | Popover::Baseline) => {
                None
            }
            Modal::Popover(Popover::Menu(position, entries)) => {
                Some(self.render_menu(&ui, *position, entries, cx))
            }
            Modal::Palette => Some(self.render_palette(&ui, cx)),
            Modal::Settings => Some(self.render_settings(&ui, cx)),
            Modal::Launcher => Some(launcher::render_launcher_overlay(&self.launcher)),
            Modal::Editor => Some(composer::render_composer_overlay(&self.composer)),
            Modal::About => {
                let ent = cx.entity();
                Some(render_about_overlay(&ui, move |window, cx| {
                    ent.update(cx, |app, cx| app.close_modal(window, cx));
                }))
            }
            Modal::Confirm(confirm) => Some(self.render_confirm(&ui, confirm, cx)),
        };
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
            .on_action(
                cx.listener(|app, _: &OpenAbout, window, cx| {
                    app.run_cmd(Cmd::OpenAbout, window, cx)
                }),
            )
            .on_action(
                cx.listener(|app, _: &RunTests, window, cx| app.run_cmd(Cmd::Run, window, cx)),
            )
            .on_action(
                cx.listener(|app, _: &StopRun, window, cx| app.run_cmd(Cmd::StopRun, window, cx)),
            )
            .on_action(cx.listener(|app, _: &RunOptions, window, cx| {
                app.run_cmd(Cmd::RunOptions, window, cx)
            }))
            .on_action(cx.listener(|app, _: &OpenTerminalAction, window, cx| {
                app.run_cmd(Cmd::OpenTerminal, window, cx)
            }))
            .on_action(cx.listener(|app, _: &ToggleDemoAction, window, cx| {
                let demo = app.session.read(cx).demo;
                app.run_cmd(Cmd::Demo(!demo), window, cx)
            }))
            .on_action(cx.listener(|app, _: &OpenWorkspace, window, cx| {
                app.run_cmd(Cmd::OpenWorkspace, window, cx)
            }))
            .on_action(
                cx.listener(|app, _: &OpenPalette, window, cx| app.toggle_palette(window, cx)),
            )
            .on_action(cx.listener(|app, _: &FocusSearch, window, cx| {
                app.run_cmd(Cmd::FocusSearch, window, cx)
            }))
            .on_action(cx.listener(|app, _: &OpenSettings, window, cx| {
                if matches!(app.modal, Modal::Settings) {
                    app.close_modal(window, cx);
                } else {
                    app.run_cmd(Cmd::OpenSettings, window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &ToggleSidebar, window, cx| {
                app.run_cmd(Cmd::ToggleSidebar, window, cx)
            }))
            .on_action(cx.listener(|app, _: &ToggleInspector, window, cx| {
                app.run_cmd(Cmd::ToggleInspector, window, cx)
            }))
            .on_action(cx.listener(|app, _: &ToggleOutput, window, cx| {
                app.run_cmd(Cmd::ToggleOutput, window, cx)
            }))
            .on_action(
                cx.listener(|app, _: &OpenFile, window, cx| app.run_cmd(Cmd::OpenFile, window, cx)),
            )
            .on_action(cx.listener(|app, _: &ExportReport, window, cx| {
                app.run_cmd(Cmd::ExportReport, window, cx)
            }))
            .on_action(cx.listener(|app, _: &InitCiWorkflow, window, cx| {
                app.run_cmd(Cmd::InitCi, window, cx)
            }))
            .on_action(cx.listener(|app, _: &InstallCliAction, _, cx| app.install_cli(cx)))
            .on_action(cx.listener(|app, _: &NextMatch, _, cx| {
                app.inspector.update(cx, |insp, cx| insp.step_match(1, cx));
            }))
            .on_action(cx.listener(|app, _: &PrevMatch, _, cx| {
                app.inspector.update(cx, |insp, cx| insp.step_match(-1, cx));
            }))
            .on_action(
                cx.listener(|app, _: &Replay, window, cx| app.run_cmd(Cmd::Replay, window, cx)),
            )
            .on_action(cx.listener(|app, _: &EditReplay, window, cx| {
                app.run_cmd(Cmd::EditReplay, window, cx)
            }))
            .on_action(cx.listener(|app, _: &ShowCalls, window, cx| {
                app.run_cmd(Cmd::Mode(Mode::Calls), window, cx)
            }))
            .on_action(cx.listener(|app, _: &ShowAccess, window, cx| {
                app.run_cmd(Cmd::Mode(Mode::Access), window, cx)
            }))
            .on_action(cx.listener(|app, _: &PrevRun, window, cx| {
                app.run_cmd(Cmd::Runs(RunNav::Prev), window, cx)
            }))
            .on_action(cx.listener(|app, _: &NextRun, window, cx| {
                app.run_cmd(Cmd::Runs(RunNav::Next), window, cx)
            }))
            .on_action(cx.listener(|app, _: &CopyCurl, window, cx| {
                if let Some(call) = app.inspector.read(cx).call().cloned() {
                    app.run_cmd(Cmd::CopyCurl(call), window, cx);
                }
            }))
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ZoomWindow, window, _| window.zoom_window())
            .on_action(|_: &BringAllToFront, _, cx| cx.activate(true))
            .on_action(|_: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|app, _: &CloseWindow, window, cx| {
                // Closes what is on top, else the window. On Linux closing
                // the last window ends the app; Quit is only ⌃Q.
                if app.modal.is_open() {
                    app.close_modal(window, cx);
                } else {
                    window.remove_window();
                }
            }))
            .on_action(cx.listener(|app, _: &SelectNext, _, cx| {
                if matches!(app.modal, Modal::Palette) {
                    app.palette_step(1, cx);
                } else {
                    app.select(|target, cx| target.step(1, cx), cx);
                }
            }))
            .on_action(cx.listener(|app, _: &SelectPrev, _, cx| {
                if matches!(app.modal, Modal::Palette) {
                    app.palette_step(-1, cx);
                } else {
                    app.select(|target, cx| target.step(-1, cx), cx);
                }
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
            .on_action(cx.listener(|app, _: &SubmitField, _, cx| app.submit_field(cx)))
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
                    let sidebar_offset = if app.prefs.sidebar_open {
                        SIDEBAR_W
                    } else {
                        0.0
                    };
                    let x = f32::from(event.event.position.x);
                    let width =
                        (viewport - x).clamp(320.0, (viewport - sidebar_offset - 420.0).max(320.0));
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
                    if matches!(app.modal, Modal::Popover(Popover::Menu(..))) {
                        app.modal = Modal::None;
                        cx.notify();
                    }
                }),
            )
            .child(header)
            .child(workspace)
            .children(overlay)
            .children(toast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_modal() -> Vec<Modal> {
        vec![
            Modal::Palette,
            Modal::Settings,
            Modal::Launcher,
            Modal::Editor,
            Modal::About,
            Modal::Confirm(Question::AcceptAll(Vec::new())),
            Modal::Confirm(Question::OverwriteCi {
                path: PathBuf::from("/p/.github/workflows/lenslet.yml"),
                command: "pytest".into(),
                upstream: None,
            }),
            Modal::Popover(Popover::AppMenu),
            Modal::Popover(Popover::Runs),
            Modal::Popover(Popover::Baseline),
            Modal::Popover(Popover::Menu(Point::default(), Vec::new())),
        ]
    }

    #[test]
    fn escape_closes_every_modal() {
        for mut modal in every_modal() {
            assert!(modal.is_open());
            assert!(modal.escape(), "Esc must close an open modal");
            assert!(!modal.is_open());
            assert!(!modal.escape(), "a second Esc has nothing to close");
        }
    }

    #[test]
    fn dialogs_take_the_keyboard_and_popovers_do_not() {
        for modal in every_modal() {
            let dialog = matches!(
                modal,
                Modal::Settings
                    | Modal::Launcher
                    | Modal::Editor
                    | Modal::About
                    | Modal::Confirm(_)
            );
            assert_eq!(modal.is_dialog(), dialog);
            assert_eq!(modal.blocks_list(), dialog);
        }
        assert!(!Modal::None.is_dialog());
    }

    #[test]
    fn run_uses_the_saved_command_and_the_one_enforce_toggle() {
        let settings = RunSettings {
            command: "  pytest -q ".into(),
            cwd: PathBuf::from("/p"),
            upstream: None,
            routes: Vec::new(),
            enforce: true,
            allow_docker: true,
        };
        let opts = plan_run(&settings, PathBuf::from("/runs")).unwrap();
        assert_eq!(opts.command_line, "pytest -q");
        assert!(opts.enforce, "Block calls outside the lock reaches the run");
        assert!(opts.allow_docker);
        let off = plan_run(
            &RunSettings {
                enforce: false,
                ..settings.clone()
            },
            PathBuf::from("/runs"),
        )
        .unwrap();
        assert!(!off.enforce);

        // No command: ▶ asks for one instead of guessing `pytest`.
        let empty = RunSettings {
            command: "  ".into(),
            ..settings
        };
        assert!(plan_run(&empty, PathBuf::from("/runs")).is_none());
    }

    #[test]
    fn a_second_start_is_refused_while_one_is_in_flight() {
        let mut flight = InFlight::default();
        assert!(flight.begin_run(false));
        assert!(flight.starting_run());
        assert!(!flight.begin_run(false), "double click while starting");
        flight.end_run();
        assert!(!flight.begin_run(true), "a run is already running");
        assert!(flight.begin_run(false));

        assert!(flight.begin_proxy(false));
        assert!(!flight.begin_proxy(false));
        flight.end_proxy();
        assert!(!flight.begin_proxy(true), "a proxy already answers");
    }
}
