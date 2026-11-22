//! The Calls view: the run's findings first (failures, regressions, what is
//! not in lenslet.lock), filter chips, and a virtualized table. Its view
//! (filtered row indices) is recomputed when inputs change — incrementally
//! for appends, on the background executor for large searches — never in
//! render. A run opens on its first failure.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy, Stateful, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, rgb, uniform_list,
};

use super::{
    session::{Health, Session, SessionEvent},
    widgets,
};
use crate::{
    daemon::{EmulatorStatus, MOTO_INSTALL},
    fonts, glyph,
    i18n::{self, Tr},
    model::{
        CallFilter, FailureKind, Finding, LockView, Regressions, ServiceFacet, StatusFilter, Trace,
        ellipsize, format_latency, format_offset, operation_label, parse_rfc3339_ms, search_tokens,
        short_clock, status_label,
    },
    store::{BACKGROUND_SEARCH_ROWS, Stats, ViewQuery, filter_rows},
    theme::{self, Ui, text},
    tr,
};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(60);
const PAGE_ROWS: isize = 20;
/// Findings shown before "+N more".
const FINDINGS_SHOWN: usize = 5;

pub enum CallsEvent {
    /// The selected call changed.
    Selected(Option<Arc<Trace>>),
    /// Right-click on a row.
    ContextMenu(Point<Pixels>, Arc<Trace>),
    /// "Review": open Access filtered to Not in lock.
    ReviewNotInLock,
    Toast(Tr),
}

pub struct CallsList {
    session: Entity<Session>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    pub filter: CallFilter,
    query: String,
    tokens: Vec<String>,
    rows: Arc<Vec<Arc<Trace>>>,
    view: Arc<Vec<u32>>,
    stats: Stats,
    not_in_lock_calls: usize,
    facets: Vec<ServiceFacet>,
    lock: LockView,
    selected: Option<String>,
    /// The user (or a failure) picked a row; stop following the newest call.
    pinned: bool,
    /// What ▶ will talk to (from `lenslet doctor`), for the empty state.
    emulator: EmulatorStatus,
    view_generation: u64,
    _search_task: Option<Task<()>>,
    _subscription: gpui::Subscription,
}

impl EventEmitter<CallsEvent> for CallsList {}

impl Focusable for CallsList {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CallsList {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::RunChanged => this.on_run_changed(cx),
            SessionEvent::Rows(delta) => this.on_rows(delta.appended_from, cx),
            SessionEvent::Derived => this.recompute(cx),
            SessionEvent::Runs | SessionEvent::Log | SessionEvent::Toast(_) => {}
        });
        let mut list = CallsList {
            session,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            filter: CallFilter::default(),
            query: String::new(),
            tokens: Vec::new(),
            rows: Arc::new(Vec::new()),
            view: Arc::new(Vec::new()),
            stats: Stats::default(),
            not_in_lock_calls: 0,
            facets: Vec::new(),
            lock: LockView::default(),
            selected: None,
            pinned: false,
            emulator: EmulatorStatus::Checking,
            view_generation: 0,
            _search_task: None,
            _subscription: subscription,
        };
        list.on_run_changed(cx);
        list
    }

    pub fn set_emulator_status(&mut self, status: EmulatorStatus, cx: &mut Context<Self>) {
        if self.emulator != status {
            self.emulator = status;
            cx.notify();
        }
    }

    pub fn selected(&self) -> Option<Arc<Trace>> {
        let id = self.selected.as_deref()?;
        self.rows.iter().find(|row| row.id == id).cloned()
    }

    fn on_run_changed(&mut self, cx: &mut Context<Self>) {
        self.filter = CallFilter::default();
        self.pinned = false;
        if self.selected.take().is_some() {
            cx.emit(CallsEvent::Selected(None));
        }
        self.recompute(cx);
        self.select_first_failure(cx);
    }

    /// Open on the first failure: it is what the run is about.
    fn select_first_failure(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = first_failure(&self.rows, &self.view) {
            self.select_id(Some(id), false, cx);
            self.pinned = true;
            self.reveal(ScrollStrategy::Center);
        }
    }

    fn snapshot(&mut self, cx: &App) {
        let session = self.session.read(cx);
        self.rows = session.store.rows().clone();
        self.stats = session.store.stats();
        self.facets = session.store.facets();
        self.lock = session.lock.clone();
        if let Some(service) = self.filter.service.as_deref()
            && !self.facets.iter().any(|facet| facet.id == service)
        {
            self.filter.service = None;
        }
    }

    fn query(&self) -> ViewQuery {
        ViewQuery {
            filter: self.filter.clone(),
            tokens: self.tokens.clone(),
            lock: self.lock.clone(),
        }
    }

    fn on_rows(&mut self, appended_from: Option<usize>, cx: &mut Context<Self>) {
        let Some(from) = appended_from else {
            self.recompute(cx);
            return;
        };
        let follow = !self.pinned && self.near_bottom();
        self.snapshot(cx);
        let query = self.query();
        let mut view = (*self.view).clone();
        for (index, row) in self.rows.iter().enumerate().skip(from) {
            self.not_in_lock_calls += usize::from(row.not_in_lock(&self.lock));
            if query.matches(row) {
                view.push(index as u32);
            }
        }
        self.view = Arc::new(view);
        self.after_view_change(follow, cx);
    }

    /// Recompute the view from scratch: synchronously when cheap, on the
    /// background executor for a search over many rows.
    pub fn recompute(&mut self, cx: &mut Context<Self>) {
        self.snapshot(cx);
        self.view_generation += 1;
        let generation = self.view_generation;
        let query = self.query();
        let rows = self.rows.clone();
        let follow = !self.pinned && self.near_bottom();
        if query.tokens.is_empty() || rows.len() <= BACKGROUND_SEARCH_ROWS {
            let (view, not_in_lock) = compute_view(&rows, &query);
            self.view = Arc::new(view);
            self.not_in_lock_calls = not_in_lock;
            self.after_view_change(follow, cx);
            return;
        }
        self._search_task = Some(cx.spawn(async move |this, cx| {
            let (view, not_in_lock) = cx
                .background_executor()
                .spawn(async move { compute_view(&rows, &query) })
                .await;
            this.update(cx, |list, cx| {
                if list.view_generation == generation {
                    list.view = Arc::new(view);
                    list.not_in_lock_calls = not_in_lock;
                    list.after_view_change(follow, cx);
                }
            })
            .ok();
        }));
    }

    fn after_view_change(&mut self, follow: bool, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            if self.selected.take().is_some() {
                cx.emit(CallsEvent::Selected(None));
            }
        } else if !self.pinned {
            // While live and nothing is picked: the first failure wins,
            // otherwise follow the newest call.
            if let Some(id) = first_failure(&self.rows, &self.view) {
                self.select_id(Some(id), false, cx);
                self.pinned = true;
                self.reveal(ScrollStrategy::Center);
            } else {
                let newest = self
                    .view
                    .last()
                    .map(|&ix| self.rows[ix as usize].id.clone());
                self.select_id(newest, false, cx);
                if follow && !self.view.is_empty() {
                    self.scroll
                        .scroll_to_item(self.view.len() - 1, ScrollStrategy::Bottom);
                }
            }
        } else if self
            .selected
            .as_deref()
            .is_none_or(|id| !self.view_contains(id))
            && let Some(&first) = self.view.first()
        {
            // The selection was filtered out: move to the first match.
            let id = self.rows[first as usize].id.clone();
            self.select_id(Some(id), false, cx);
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn view_contains(&self, id: &str) -> bool {
        self.view.iter().any(|&ix| self.rows[ix as usize].id == id)
    }

    fn near_bottom(&self) -> bool {
        let state = self.scroll.0.borrow();
        let max = state.base_handle.max_offset().height;
        if max <= px(1.0) {
            return true;
        }
        let offset = state.base_handle.offset().y;
        (-offset + px(48.0)) >= max
    }

    // -- Query and filters ---------------------------------------------------

    /// Debounced: typing does not filter on every keystroke.
    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.view_generation += 1;
        let generation = self.view_generation;
        self._search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |list, cx| {
                if list.view_generation == generation {
                    list.tokens = search_tokens(&list.query);
                    list.recompute(cx);
                }
            })
            .ok();
        }));
    }

    pub fn set_filter(&mut self, filter: CallFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.recompute(cx);
            self.reveal(ScrollStrategy::Center);
        }
    }

    pub fn set_status(&mut self, status: StatusFilter, cx: &mut Context<Self>) {
        let mut filter = self.filter.clone();
        filter.status = if filter.status == status {
            StatusFilter::All
        } else {
            status
        };
        self.set_filter(filter, cx);
    }

    pub fn set_service(&mut self, service: Option<String>, cx: &mut Context<Self>) {
        let mut filter = self.filter.clone();
        filter.service = if filter.service == service {
            None
        } else {
            service
        };
        self.set_filter(filter, cx);
    }

    /// Escape: drop the most specific filter first. Returns whether one was cleared.
    pub fn clear_one_filter(&mut self, cx: &mut Context<Self>) -> bool {
        let mut filter = self.filter.clone();
        let cleared = filter.ids.take().is_some()
            || filter.service.take().is_some()
            || std::mem::take(&mut filter.status) != StatusFilter::All;
        if cleared {
            self.set_filter(filter, cx);
        }
        cleared
    }

    /// A finding was clicked: select its first call.
    pub fn show_finding(&mut self, finding: &Finding, cx: &mut Context<Self>) {
        let Some(first) = finding.calls.first().cloned() else {
            return;
        };
        if !self.view_contains(&first) {
            self.set_filter(CallFilter::default(), cx);
        }
        self.select_id(Some(first), true, cx);
        self.reveal(ScrollStrategy::Center);
    }

    // -- Selection -----------------------------------------------------------

    pub fn select_id(&mut self, id: Option<String>, by_user: bool, cx: &mut Context<Self>) {
        if by_user {
            self.pinned = true;
        }
        if self.selected != id {
            self.selected = id;
            let row = self.selected();
            cx.emit(CallsEvent::Selected(row));
            cx.notify();
        }
    }

    fn position(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.view
            .iter()
            .position(|&ix| self.rows[ix as usize].id == id)
    }

    pub fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() as isize - 1;
        let next = match self.position() {
            Some(position) => (position as isize + delta).clamp(0, last),
            None if delta > 0 => 0,
            None => last,
        } as usize;
        let id = self.rows[self.view[next] as usize].id.clone();
        self.select_id(Some(id), true, cx);
        self.scroll.scroll_to_item(next, ScrollStrategy::Top);
    }

    pub fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        self.move_by(if down { PAGE_ROWS } else { -PAGE_ROWS }, cx);
    }

    pub fn select_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        if self.view.is_empty() {
            return;
        }
        let index = if last { self.view.len() - 1 } else { 0 };
        let id = self.rows[self.view[index] as usize].id.clone();
        self.select_id(Some(id), true, cx);
        // Jumping to the end resumes following new calls.
        if last {
            self.pinned = false;
        }
        self.scroll.scroll_to_item(index, ScrollStrategy::Bottom);
    }

    fn reveal(&self, strategy: ScrollStrategy) {
        if let Some(position) = self.position() {
            self.scroll.scroll_to_item(position, strategy);
        }
    }

    // -- Render --------------------------------------------------------------

    fn render_chips(&self, ui: &Ui, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let status = self.filter.status;
        let entity = cx.entity();
        let status_chip = |id: &'static str, label: &str, count: usize, value, color| {
            let entity = entity.clone();
            widgets::chip(
                ui,
                id,
                label.to_string(),
                Some(count),
                status == value,
                color,
            )
            .on_click(move |_, _, cx| entity.update(cx, |list, cx| list.set_status(value, cx)))
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(theme::SUBBAR_H))
            .px(px(theme::ROW_PAD_X - 4.0))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.toolbar))
            .overflow_hidden()
            .child(status_chip(
                "f-all",
                ui.t("All", "全部"),
                self.stats.total,
                StatusFilter::All,
                t.accent,
            ));
        if self.stats.errors > 0 || status == StatusFilter::Errors {
            bar = bar.child(status_chip(
                "f-errors",
                ui.t("Errors", "错误"),
                self.stats.errors,
                StatusFilter::Errors,
                t.red,
            ));
        }
        if self.not_in_lock_calls > 0 || status == StatusFilter::NotInLock {
            bar = bar.child(
                status_chip(
                    "f-not-in-lock",
                    ui.t("Not in lock", "未入锁"),
                    self.not_in_lock_calls,
                    StatusFilter::NotInLock,
                    t.amber,
                )
                .tooltip(widgets::tip(
                    ui.t(
                        "Calls that need a permission lenslet.lock does not have",
                        "需要 lenslet.lock 中没有的权限的调用",
                    ),
                    None,
                )),
            );
        }
        if self.facets.len() > 1 {
            bar = bar.child(div().w(px(1.0)).h(px(16.0)).bg(rgb(t.border)).mx(px(2.0)));
            for (index, facet) in self.facets.iter().enumerate() {
                let active = self.filter.service.as_deref() == Some(facet.id.as_str());
                let color = color_hex(ui.service_color(&facet.label));
                let id = facet.id.clone();
                let entity = entity.clone();
                bar = bar.child(
                    widgets::chip(
                        ui,
                        ElementId::named_usize("svc", index),
                        ellipsize(&facet.label, 14),
                        Some(facet.count),
                        active,
                        color,
                    )
                    .on_click(move |_, _, cx| {
                        let id = id.clone();
                        entity.update(cx, |list, cx| list.set_service(Some(id), cx));
                    }),
                );
            }
        }
        let session = self.session.read(cx);
        let (evicted, skipped) = (session.evicted, session.skipped);
        let max = crate::model::MAX_ROWS;
        bar = bar.child(div().flex_1());
        if evicted > 0 {
            bar = bar.child(
                widgets::badge(
                    ui,
                    ui.tr(&tr!("newest {max} shown", "仅显示最新 {max} 条"))
                        .to_string(),
                    t.text_muted,
                )
                .id("evicted")
                .tooltip(widgets::tip(
                    ui.tr(&tr!(
                        "{evicted} earlier calls are not shown; the Access view still counts them",
                        "另有 {evicted} 条更早的调用未显示；权限视图仍会统计它们"
                    ))
                    .to_string(),
                    None,
                )),
            );
        }
        if skipped > 0 {
            bar = bar.child(widgets::badge(
                ui,
                ui.tr(&tr!("{skipped} unreadable lines", "{skipped} 行无法解析"))
                    .to_string(),
                t.red,
            ));
        }
        bar.when(!self.tokens.is_empty() || self.filter.is_active(), |bar| {
            bar.child(
                div()
                    .flex_shrink_0()
                    .font_family(fonts::mono())
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_muted))
                    .child(format!("{} / {}", self.view.len(), self.rows.len())),
            )
        })
    }

    fn render_findings(&self, ui: &Ui, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let t = ui.p;
        let session = self.session.read(cx);
        let items = strip_items(
            &session.failures,
            &self.rows,
            session.run.as_ref().and_then(|r| r.regressions()),
            session.not_in_lock,
            session.lock.exists(),
        );
        if items.is_empty() {
            return None;
        }
        let mut strip = div()
            .id("findings")
            .flex()
            .flex_col()
            .py(px(4.0))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.panel));
        let item = |id: ElementId, color: u32, glyph_text: &'static str, label: String| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(24.0))
                .px(px(theme::ROW_PAD_X))
                .child(
                    div()
                        .w(px(12.0))
                        .text_size(px(9.0))
                        .text_color(rgb(color))
                        .child(glyph_text),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(text::MD))
                        .text_color(rgb(t.text_soft))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(label),
                )
        };
        for (index, entry) in items.into_iter().enumerate() {
            let id = ElementId::named_usize("finding", index);
            let row = match entry {
                StripItem::Failure {
                    finding,
                    title,
                    new,
                } => {
                    let calls = finding.calls.len();
                    item(id, t.red, "●", ui.tr(&title).to_string())
                        .cursor_pointer()
                        .hover(move |s| s.bg(rgb(t.row_hover)))
                        .on_click(
                            cx.listener(move |list, _, _, cx| list.show_finding(&finding, cx)),
                        )
                        .when(new, |row| {
                            row.child(widgets::badge(ui, ui.t("NEW", "新"), t.red))
                        })
                        .when(calls > 1, |row| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .font(fonts::mono_font())
                                    .text_size(px(text::XS))
                                    .text_color(rgb(t.text_muted))
                                    .child(i18n::calls_count(calls, ui.lang)),
                            )
                        })
                }
                StripItem::More(n) => item(id, t.text_muted, "", i18n::more_count(n, ui.lang)),
                StripItem::Fixed(text) => item(id, t.green, "✓", ui.tr(&text).to_string()),
                StripItem::NotInLock(text) => item(id, t.amber, "▲", ui.tr(&text).to_string())
                    .child(
                        widgets::button(ui, "finding-review", ui.t("Review →", "查看 →")).on_click(
                            cx.listener(|_, _, _, cx| cx.emit(CallsEvent::ReviewNotInLock)),
                        ),
                    ),
            };
            strip = strip.child(row);
        }
        Some(strip)
    }

    fn render_head(&self, ui: &Ui) -> Div {
        let t = ui.p;
        let cell = |label: &str, width: Option<f32>, right: bool| {
            let cell = div()
                .flex()
                .items_center()
                .when(right, |el| el.justify_end())
                .text_size(px(text::XS))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(t.text_muted))
                .child(label.to_string());
            match width {
                Some(w) => cell.w(px(w)).flex_shrink_0(),
                None => cell.flex_1().min_w(px(0.0)),
            }
        };
        div()
            .flex()
            .items_center()
            .gap(px(theme::COL_GAP))
            .h(px(theme::TABLE_HEAD_H))
            .px(px(theme::ROW_PAD_X))
            .bg(rgb(t.table_head))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .child(cell(ui.t("TIME", "时间"), Some(theme::COL_TIME), false))
            .child(cell(
                ui.t("SERVICE", "服务"),
                Some(theme::COL_SERVICE),
                false,
            ))
            .child(cell(ui.t("OPERATION", "操作"), None, false))
            .child(cell(ui.t("STATUS", "状态"), Some(theme::COL_STATUS), false))
            .child(cell(
                ui.t("DURATION", "耗时"),
                Some(theme::COL_LATENCY),
                true,
            ))
    }

    fn render_empty(&self, ui: &Ui, health: &Health, cx: &mut Context<Self>) -> AnyElement {
        match health {
            Health::Loading => widgets::skeleton_rows(ui, 10).into_any_element(),
            Health::Missing => self.render_welcome(ui, cx),
            Health::Error(error) => widgets::empty_state(
                ui,
                ui.t("Can't show this run", "无法显示此运行"),
                ui.tr(error).to_string(),
                None,
            )
            .into_any_element(),
            Health::Ready if self.rows.is_empty() => widgets::empty_state(
                ui,
                ui.t("Waiting for the first call", "等待第一个调用"),
                ui.t(
                    "The tests are running. Every AWS call they make shows up here as it happens.",
                    "测试正在运行。它们发出的每个 AWS 调用都会实时显示在这里。",
                ),
                None,
            )
            .into_any_element(),
            Health::Ready => {
                let entity = cx.entity();
                widgets::empty_state(
                    ui,
                    ui.t("No calls match", "没有匹配的调用"),
                    ui.t(
                        "Nothing matches the search and filters. Press Esc to clear them one at a time.",
                        "没有符合搜索与筛选条件的调用。按 Esc 逐个清除。",
                    ),
                    Some(
                        widgets::button(ui, "clear-filters", ui.t("Clear filters", "清除筛选"))
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |list, cx| {
                                    list.set_filter(CallFilter::default(), cx)
                                })
                            })
                            .into_any_element(),
                    ),
                )
                .into_any_element()
            }
        }
    }

    fn render_welcome(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let status = &self.emulator;
        let color = if status.ok() { t.green } else { t.amber };
        let mut status_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(glyph::dot(theme::h(color, 1.0), 7.0))
            .child(
                div()
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_soft))
                    .child(ui.tr(&status.message()).to_string()),
            );
        if *status == EmulatorStatus::Missing {
            let weak = cx.entity().downgrade();
            status_row = status_row.child(widgets::mono(ui, MOTO_INSTALL)).child(
                widgets::button(ui, "welcome-copy-install", ui.t("Copy", "复制")).on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            MOTO_INSTALL.to_string(),
                        ));
                        weak.update(cx, |_, cx| {
                            cx.emit(CallsEvent::Toast(i18n::copied(MOTO_INSTALL)))
                        })
                        .ok();
                    },
                ),
            );
        }
        widgets::empty_state(
            ui,
            ui.t(
                "See what AWS access your code needs, and why its calls failed",
                "看清代码需要哪些 AWS 权限，以及调用为何失败",
            ),
            ui.t(
                "Open your project and press ▶ to run its tests against a local emulator — or run them from a terminal with the command below.",
                "打开项目并按 ▶，即可针对本地模拟器运行测试 — 也可以在终端中运行下面的命令。",
            ),
            Some(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        widgets::primary_button(
                            ui,
                            "empty-open-project",
                            ui.t("Open Project Folder…", "打开项目目录…"),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(super::OpenProject), cx)
                        }),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(rgb(t.code))
                            .font(fonts::mono_font())
                            .text_size(px(text::MD))
                            .text_color(rgb(t.text_body))
                            .child(ui.t(
                                "lenslet run --ui -- <your tests>",
                                "lenslet run --ui -- <你的测试命令>",
                            )),
                    )
                    .child(status_row)
                    .into_any_element(),
            ),
        )
        .into_any_element()
    }
}

/// One line of the Findings strip.
#[derive(Clone, Debug, PartialEq)]
pub enum StripItem {
    /// A failure (the first call it names is selected on click).
    Failure {
        finding: Finding,
        title: Tr,
        new: bool,
    },
    /// Failures beyond the first few.
    More(usize),
    /// Failure kinds fixed since the previous run.
    Fixed(Tr),
    /// Permissions not in lenslet.lock, with a Review button.
    NotInLock(Tr),
}

/// The Findings strip: failures (new ones marked), what was fixed since the
/// previous run, and how many permissions lenslet.lock lacks.
pub fn strip_items(
    failures: &[Finding],
    rows: &[Arc<Trace>],
    regressions: Option<&Regressions>,
    not_in_lock: usize,
    lock_exists: bool,
) -> Vec<StripItem> {
    let new_failures: BTreeSet<&FailureKind> = regressions
        .map(|r| r.new_failures.iter().collect())
        .unwrap_or_default();
    let mut items: Vec<StripItem> = failures
        .iter()
        .take(FINDINGS_SHOWN)
        .map(|finding| {
            let call = finding
                .calls
                .first()
                .and_then(|id| rows.iter().find(|row| &row.id == id));
            let title = match call {
                Some(call) => {
                    let service = call.service_name();
                    let op = operation_label(call);
                    let code = call.problem.as_ref().map_or("", |p| p.code.as_str());
                    tr!(
                        "{service} {op} failed: {code}",
                        "{service} {op} 失败：{code}"
                    )
                }
                None => Tr::new(finding.title.clone(), finding.title.clone()),
            };
            let new = call
                .and_then(|c| c.failure_kind())
                .is_some_and(|kind| new_failures.contains(&kind));
            StripItem::Failure {
                finding: finding.clone(),
                title,
                new,
            }
        })
        .collect();
    if failures.len() > FINDINGS_SHOWN {
        items.push(StripItem::More(failures.len() - FINDINGS_SHOWN));
    }
    if let Some(regressions) = regressions.filter(|r| !r.fixed.is_empty()) {
        let names = regressions
            .fixed
            .iter()
            .map(|k| {
                format!(
                    "{} {} ({})",
                    k.service.to_ascii_uppercase(),
                    k.operation,
                    k.code
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        items.push(StripItem::Fixed(tr!(
            "Fixed since the previous run: {names}",
            "相比上次运行已修复：{names}"
        )));
    }
    if not_in_lock > 0 {
        let n = not_in_lock;
        let permissions = i18n::permissions(n);
        items.push(StripItem::NotInLock(if lock_exists {
            tr!(
                "{n} {permissions} not in lenslet.lock",
                "{n} 项权限不在 lenslet.lock 中"
            )
        } else {
            tr!(
                "No lenslet.lock yet — {n} {permissions} not in lock",
                "还没有 lenslet.lock — {n} 项权限未入锁"
            )
        }));
    }
    items
}

/// The first failing call in the view.
pub fn first_failure(rows: &[Arc<Trace>], view: &[u32]) -> Option<String> {
    view.iter()
        .map(|&ix| &rows[ix as usize])
        .find(|row| row.is_error())
        .map(|row| row.id.clone())
}

fn color_hex(color: gpui::Rgba) -> u32 {
    let r = (color.r * 255.0).round() as u32;
    let g = (color.g * 255.0).round() as u32;
    let b = (color.b * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

/// The view and how many calls need a permission not in the lock.
fn compute_view(rows: &[Arc<Trace>], query: &ViewQuery) -> (Vec<u32>, usize) {
    let not_in_lock = rows
        .iter()
        .filter(|row| row.not_in_lock(&query.lock))
        .count();
    (filter_rows(rows, query), not_in_lock)
}

impl Render for CallsList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let session = self.session.read(cx);
        let health = session.health.clone();
        let start_ms = session
            .run
            .as_ref()
            .and_then(|run| parse_rfc3339_ms(&run.header.started))
            .or_else(|| self.rows.first().and_then(|row| row.at_ms()));
        let new_failures: Arc<BTreeSet<FailureKind>> = Arc::new(
            session
                .run
                .as_ref()
                .and_then(|r| r.regressions())
                .map(|r| r.new_failures.iter().cloned().collect())
                .unwrap_or_default(),
        );

        let body: AnyElement = if self.view.is_empty() {
            self.render_empty(&ui, &health, cx)
        } else {
            crate::fsguard::milestone_once("first frame with rows");
            let rows = self.rows.clone();
            let view = self.view.clone();
            let selected = self.selected.clone();
            let lock = self.lock.clone();
            let entity = cx.entity().downgrade();
            uniform_list("calls", view.len(), move |range, _window, cx| {
                let _scope = crate::fsguard::RenderScope::enter();
                let ui = Ui::of(cx);
                range
                    .map(|position| {
                        let row = &rows[view[position] as usize];
                        let badges = RowBadges {
                            not_in_lock: row.not_in_lock(&lock),
                            new_failure: row
                                .failure_kind()
                                .is_some_and(|kind| new_failures.contains(&kind)),
                        };
                        call_row(
                            &ui,
                            row,
                            position,
                            selected.as_deref() == Some(row.id.as_str()),
                            badges,
                            start_ms,
                            entity.clone(),
                        )
                    })
                    .collect()
            })
            .flex_1()
            .min_h(px(0.0))
            .track_scroll(self.scroll.clone())
            .into_any_element()
        };

        let has_run = health == Health::Ready;
        div()
            .id("calls-list")
            .size_full()
            .key_context("Calls")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(t.panel))
            .children(has_run.then(|| self.render_findings(&ui, cx)).flatten())
            .when(has_run, |el| {
                el.child(self.render_chips(&ui, cx))
                    .child(self.render_head(&ui))
            })
            .child(body)
    }
}

#[derive(Clone, Copy)]
struct RowBadges {
    not_in_lock: bool,
    new_failure: bool,
}

fn call_row(
    ui: &Ui,
    trace: &Arc<Trace>,
    position: usize,
    selected: bool,
    badges: RowBadges,
    run_start_ms: Option<i64>,
    list: gpui::WeakEntity<CallsList>,
) -> Stateful<Div> {
    let t = ui.p;
    let service_color = ui.service_color(trace.service_name());
    let rel = match (trace.at_ms(), run_start_ms) {
        (Some(at), Some(start)) => format_offset(at - start),
        _ => short_clock(&trace.at),
    };
    let id = trace.id.clone();
    let menu_trace = trace.clone();
    let click_list = list.clone();
    let status_color = ui.status_color(trace.status);
    let semantic = trace.semantic_target();

    let mut title = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(text::LG))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(t.text))
                .whitespace_nowrap()
                .child(operation_label(trace).to_string()),
        );
    if badges.new_failure {
        title = title.child(widgets::badge(ui, ui.t("NEW FAILURE", "新失败"), t.red));
    }
    if badges.not_in_lock {
        title = title.child(widgets::badge(ui, ui.t("NOT IN LOCK", "未入锁"), t.amber));
    }
    if let Some(role) = trace.caller_role.as_deref() {
        let short_role = role.rsplit('/').next().unwrap_or(role);
        title = title.child(widgets::badge(
            ui,
            format!("role/{}", ellipsize(short_role, 18)),
            t.text_muted,
        ));
    }

    let line = |s: String, color: u32| {
        div()
            .flex_1()
            .min_w(px(0.0))
            .text_color(rgb(color))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis()
            .child(s)
    };
    let detail = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .text_size(px(text::SM));
    let detail = match trace.problem.as_ref() {
        Some(problem) => detail
            .child(
                div()
                    .flex_shrink_0()
                    .font(fonts::mono_font())
                    .text_color(status_color)
                    .child(ellipsize(&problem.code, 40)),
            )
            .child(line(
                problem
                    .message
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| semantic.clone()),
                t.text_dim,
            )),
        None => detail
            .font(fonts::mono_font())
            .child(line(semantic, t.text_soft)),
    };

    div()
        .id(ElementId::named_usize("call", position))
        .relative()
        .flex()
        .items_center()
        .gap(px(theme::COL_GAP))
        .w_full()
        .h(px(ui.row_height()))
        .px(px(theme::ROW_PAD_X))
        .border_b_1()
        .border_color(rgb(t.border_subtle))
        .bg(if selected {
            rgb(t.row_selected)
        } else {
            rgb(t.row)
        })
        .hover(move |s| {
            s.bg(if selected {
                rgb(t.row_selected_hover)
            } else {
                rgb(t.row_hover)
            })
        })
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let id = id.clone();
            click_list
                .update(cx, |list, cx| {
                    window.focus(&list.focus);
                    list.select_id(Some(id), true, cx);
                })
                .ok();
        })
        .on_mouse_down(
            MouseButton::Right,
            move |event: &MouseDownEvent, window, cx| {
                let trace = menu_trace.clone();
                list.update(cx, |list, cx| {
                    window.focus(&list.focus);
                    list.select_id(Some(trace.id.clone()), true, cx);
                    cx.emit(CallsEvent::ContextMenu(event.position, trace));
                })
                .ok();
            },
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(2.0))
                .when(selected, |el| el.bg(rgb(t.accent)))
                .when(!selected && trace.is_error(), |el| {
                    el.bg(theme::h(t.red, 0.7))
                }),
        )
        .child(
            div()
                .w(px(theme::COL_TIME))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .font_family(fonts::mono())
                .child(
                    div()
                        .text_size(px(text::SM))
                        .text_color(rgb(t.text_dim))
                        .child(rel),
                )
                .child(
                    div()
                        .text_size(px(text::XS))
                        .text_color(rgb(t.text_faint))
                        .child(short_clock(&trace.at)),
                ),
        )
        .child(
            div()
                .w(px(theme::COL_SERVICE))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .child(glyph::dot(service_color, 6.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .text_size(px(text::SM))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(service_color)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(trace.service_name().to_string()),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(title)
                .child(detail),
        )
        .child(
            div()
                .w(px(theme::COL_STATUS))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(glyph::dot(status_color, 6.0))
                .child(
                    div()
                        .font_family(fonts::mono())
                        .text_size(px(text::SM))
                        .text_color(if trace.is_error() {
                            status_color
                        } else {
                            rgb(t.text_body)
                        })
                        .child(status_label(trace.status)),
                ),
        )
        .child(
            div()
                .w(px(theme::COL_LATENCY))
                .flex_shrink_0()
                .text_right()
                .font_family(fonts::mono())
                .text_size(px(text::SM))
                .text_color(ui.latency_color(trace.ms))
                .child(format_latency(trace.ms)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::parse_run_text, testdata::RUN_V4};

    #[test]
    fn a_run_opens_on_its_first_failure_not_its_newest_call() {
        let (_, calls) = parse_run_text(RUN_V4);
        let rows: Vec<Arc<Trace>> = calls.into_iter().map(Arc::new).collect();
        let view: Vec<u32> = (0..rows.len() as u32).collect();
        let first = first_failure(&rows, &view).unwrap();
        let call = rows.iter().find(|r| r.id == first).unwrap();
        assert_eq!(call.operation, "GetObject");
        assert_eq!(call.status, 404);
        assert_ne!(Some(&first), rows.last().map(|r| &r.id));
        // A filtered view picks its own first failure.
        let sqs: Vec<u32> = (0..rows.len() as u32)
            .filter(|&i| rows[i as usize].service == "sqs")
            .collect();
        let call_id = first_failure(&rows, &sqs).unwrap();
        assert!(
            rows.iter()
                .any(|r| r.id == call_id && r.operation == "GetQueueUrl")
        );
        assert_eq!(first_failure(&rows, &[]), None);
    }

    #[test]
    fn the_findings_strip_lists_failures_regressions_and_the_lock() {
        let (run, calls) = parse_run_text(RUN_V4);
        let rows: Vec<Arc<Trace>> = calls.into_iter().map(Arc::new).collect();
        let failures: Vec<Finding> = run.failures().into_iter().cloned().collect();
        assert_eq!(failures.len(), 2);
        let items = strip_items(&failures, &rows, run.regressions(), 1, true);
        assert_eq!(items.len(), 4, "{items:?}");
        let StripItem::Failure { title, new, .. } = &items[0] else {
            panic!("{:?}", items[0]);
        };
        assert_eq!(title.en, "S3 GetObject failed: NoSuchKey");
        assert_eq!(title.zh, "S3 GetObject 失败：NoSuchKey");
        assert!(!new);
        let StripItem::Failure { new, .. } = &items[1] else {
            panic!("{:?}", items[1]);
        };
        assert!(new, "GetQueueUrl is a new failure since the previous run");
        assert!(matches!(&items[2], StripItem::Fixed(t) if t.en.contains("DYNAMODB Query")));
        assert!(
            matches!(&items[3], StripItem::NotInLock(t) if t.en == "1 permission not in lenslet.lock")
        );

        // No lock file: the strip says so; a clean run shows nothing.
        let items = strip_items(&[], &rows, None, 17, false);
        assert!(
            matches!(&items[..], [StripItem::NotInLock(t)] if t.en.starts_with("No lenslet.lock yet"))
        );
        assert!(strip_items(&[], &rows, None, 0, true).is_empty());
        let many = vec![failures[0].clone(); FINDINGS_SHOWN + 2];
        let items = strip_items(&many, &rows, None, 0, true);
        assert_eq!(items.last(), Some(&StripItem::More(2)));
    }
}
