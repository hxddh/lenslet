//! The Access view: every permission the run needed against lenslet.lock,
//! and the detail pane where they are accepted into the lock (one at a time,
//! or all after a confirmation), removed, or marked `# test`. In-lock state
//! comes from one place ([`crate::store::access_rows`]), so the chips, rows,
//! badges and the Calls view always agree — with no lock file, every used
//! permission is "not in lock".

use std::sync::Arc;

use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy,
    Stateful, UniformListScrollHandle, Window, div, prelude::*, px, rgb, uniform_list,
};

use super::{
    session::{Health, Session, SessionEvent},
    widgets,
};
use crate::{
    fonts, glyph,
    i18n::{self, Tr},
    model::{Access, Trace, ellipsize, operation_label, service_label, status_label},
    store::{AccessFilter, AccessRow, LockState, policy_json},
    theme::{self, Ui, text},
};

const PAGE_ROWS: isize = 20;

pub enum AccessEvent {
    Selected(Option<AccessRow>),
    ContextMenu(Point<Pixels>, AccessRow),
    SelectCall(String),
    /// Ask before accepting every permission not in the lock.
    AcceptAll(Vec<(Access, bool, bool)>),
    Toast(Tr),
}

pub struct AccessView {
    session: Entity<Session>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    filter: AccessFilter,
    tokens: Vec<String>,
    rows: Arc<Vec<AccessRow>>,
    view: Arc<Vec<usize>>,
    selected: Option<Access>,
    /// The viewer picked the selection; until then it is the first row
    /// (what is not in the lock comes first).
    picked: bool,
    _subscription: gpui::Subscription,
}

impl EventEmitter<AccessEvent> for AccessView {}

impl Focusable for AccessView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AccessView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&session, |this, _, event, cx| {
            if matches!(event, SessionEvent::RunChanged) {
                this.picked = false;
            }
            if matches!(event, SessionEvent::Derived | SessionEvent::RunChanged) {
                this.refresh(cx);
            }
        });
        let mut view = AccessView {
            session,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            filter: AccessFilter::All,
            tokens: Vec::new(),
            rows: Arc::new(Vec::new()),
            view: Arc::new(Vec::new()),
            selected: None,
            picked: false,
            _subscription: subscription,
        };
        view.refresh(cx);
        view
    }

    pub fn selected(&self) -> Option<AccessRow> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .find(|row| row.access() == selected)
            .cloned()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let rows = self.session.read(cx).access_rows.clone();
        if !Arc::ptr_eq(&rows, &self.rows) {
            self.rows = rows;
            self.recompute(cx);
        }
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        let view: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.filter.matches(row) && row.matches_search(&self.tokens))
            .map(|(index, _)| index)
            .collect();
        self.view = Arc::new(view);
        let still_there = self
            .selected
            .as_ref()
            .is_some_and(|sel| self.view.iter().any(|&ix| self.rows[ix].access() == sel));
        if !still_there || !self.picked {
            self.selected = self.view.first().map(|&ix| self.rows[ix].access().clone());
        }
        cx.emit(AccessEvent::Selected(self.selected()));
        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Vec<String>, cx: &mut Context<Self>) {
        if self.tokens != tokens {
            self.tokens = tokens;
            self.recompute(cx);
        }
    }

    fn set_filter(&mut self, filter: AccessFilter, cx: &mut Context<Self>) {
        self.filter = if self.filter == filter {
            AccessFilter::All
        } else {
            filter
        };
        self.recompute(cx);
    }

    /// Show only the permissions this run used that the lock lacks.
    pub fn show_not_in_lock(&mut self, cx: &mut Context<Self>) {
        self.filter = AccessFilter::NotInLock;
        self.recompute(cx);
    }

    pub fn clear_filter(&mut self, cx: &mut Context<Self>) -> bool {
        if self.filter == AccessFilter::All {
            return false;
        }
        self.filter = AccessFilter::All;
        self.recompute(cx);
        true
    }

    pub fn select(&mut self, access: Access, cx: &mut Context<Self>) {
        if !self.rows.iter().any(|row| *row.access() == access) {
            return;
        }
        if !self
            .view
            .iter()
            .any(|&ix| *self.rows[ix].access() == access)
        {
            self.filter = AccessFilter::All;
            self.tokens.clear();
            self.recompute(cx);
        }
        self.selected = Some(access);
        self.picked = true;
        if let Some(position) = self.position() {
            self.scroll.scroll_to_item(position, ScrollStrategy::Center);
        }
        cx.emit(AccessEvent::Selected(self.selected()));
        cx.notify();
    }

    fn position(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.view
            .iter()
            .position(|&ix| self.rows[ix].access() == selected)
    }

    pub fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() as isize - 1;
        let next = match self.position() {
            Some(position) => (position as isize).saturating_add(delta).clamp(0, last),
            None => 0,
        } as usize;
        self.selected = Some(self.rows[self.view[next]].access().clone());
        self.picked = true;
        self.scroll.scroll_to_item(next, ScrollStrategy::Top);
        cx.emit(AccessEvent::Selected(self.selected()));
        cx.notify();
    }

    pub fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        self.move_by(if down { PAGE_ROWS } else { -PAGE_ROWS }, cx);
    }

    pub fn select_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        self.move_by(if last { isize::MAX } else { isize::MIN }, cx);
    }

    // -- Lock actions (also bound to a, ⌘⇧A, t, ⌫) ------------------------------

    /// Accept the selected permission into the lock.
    pub fn accept_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected().filter(|r| r.state == LockState::NotInLock) {
            let entry = vec![(row.access().clone(), false, row.used.implied)];
            self.session.update(cx, |s, cx| s.accept(entry, cx));
        }
    }

    /// Ask to accept every permission not in the lock.
    pub fn accept_all(&mut self, cx: &mut Context<Self>) {
        let entries = not_in_lock_entries(&self.rows);
        if !entries.is_empty() {
            cx.emit(AccessEvent::AcceptAll(entries));
        }
    }

    pub fn remove_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected().filter(AccessRow::in_lock) {
            let entry = vec![row.access().clone()];
            self.session
                .update(cx, |s, cx| s.remove_from_lock(entry, cx));
        }
    }

    pub fn toggle_test_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected() {
            let (access, test) = (row.access().clone(), !row.test);
            self.session
                .update(cx, |s, cx| s.set_test_mark(access, test, cx));
        }
    }

    pub fn copy_policy(&mut self, cx: &mut Context<Self>) {
        let policy = policy_json(&self.rows);
        cx.write_to_clipboard(ClipboardItem::new_string(policy));
        cx.emit(AccessEvent::Toast(i18n::copied("IAM JSON")));
    }

    // -- Render ------------------------------------------------------------------

    fn render_subbar(&self, ui: &Ui, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let count = |f: AccessFilter| self.rows.iter().filter(|row| f.matches(row)).count();
        let entity = cx.entity();
        let chip = |id: &'static str, label: &str, n: usize, f: AccessFilter, color: u32| {
            let entity = entity.clone();
            widgets::chip(ui, id, label.to_string(), Some(n), self.filter == f, color)
                .on_click(move |_, _, cx| entity.update(cx, |view, cx| view.set_filter(f, cx)))
        };
        let not_in_lock = count(AccessFilter::NotInLock);
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(theme::SUBBAR_H))
            .px(px(theme::ROW_PAD_X - 4.0))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.toolbar))
            .child(chip(
                "a-all",
                ui.t("All", "全部"),
                self.rows.len(),
                AccessFilter::All,
                t.accent,
            ))
            .child(chip(
                "a-not-in-lock",
                ui.t("Not in lock", "未入锁"),
                not_in_lock,
                AccessFilter::NotInLock,
                t.amber,
            ))
            .child(chip(
                "a-unused",
                ui.t("Unused", "未使用"),
                count(AccessFilter::Unused),
                AccessFilter::Unused,
                t.text_muted,
            ))
            .child(
                chip(
                    "a-test",
                    ui.t("Test-only", "仅测试"),
                    count(AccessFilter::Test),
                    AccessFilter::Test,
                    t.text_muted,
                )
                .tooltip(widgets::tip(
                    ui.t(
                        "`# test` lines stay in lenslet.lock but out of the policy",
                        "`# test` 行保留在 lenslet.lock 中，但不进入策略",
                    ),
                    Some(i18n::key("t")),
                )),
            )
            .child(div().flex_1());
        if self.session.read(cx).lock_target().is_some() && not_in_lock > 0 {
            bar = bar.child(
                widgets::primary_button(ui, "accept-all", ui.t("Accept all…", "全部写入…"))
                    .tooltip(widgets::tip(
                        ui.t(
                            "Review the list, then add every permission not in the lock",
                            "确认列表后，写入所有未入锁的权限",
                        ),
                        Some(i18n::shortcut("⇧A")),
                    ))
                    .on_click(cx.listener(|view, _, _, cx| view.accept_all(cx))),
            );
        }
        bar.child(
            widgets::button(
                ui,
                "copy-policy",
                ui.t("Copy policy (IAM JSON)", "复制策略（IAM JSON）"),
            )
            .tooltip(widgets::tip(
                ui.t(
                    "The least-privilege policy for what this run used, without `# test` lines",
                    "本次运行所用权限的最小权限策略（不含 `# test` 行）",
                ),
                None,
            ))
            .on_click(cx.listener(|view, _, _, cx| view.copy_policy(cx))),
        )
    }

    fn render_head(&self, ui: &Ui) -> Div {
        let t = ui.p;
        let head = |label: &str| {
            div()
                .text_size(px(text::XS))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(t.text_muted))
                .child(label.to_string())
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
            .child(div().w(px(14.0)))
            .child(head(ui.t("ACTION", "权限")).w(px(220.0)).flex_shrink_0())
            .child(head(ui.t("RESOURCE", "资源")).flex_1().min_w(px(0.0)))
            .child(
                head(ui.t("CALLS", "调用"))
                    .w(px(56.0))
                    .flex_shrink_0()
                    .justify_end(),
            )
    }
}

/// `(access, test, implied)` for every used permission the lock lacks.
pub fn not_in_lock_entries(rows: &[AccessRow]) -> Vec<(Access, bool, bool)> {
    rows.iter()
        .filter(|row| row.state == LockState::NotInLock)
        .map(|row| (row.access().clone(), false, row.used.implied))
        .collect()
}

impl Render for AccessView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let body: AnyElement = if self.view.is_empty() {
            let ready = self.session.read(cx).health == Health::Ready;
            widgets::empty_state(
                &ui,
                if self.rows.is_empty() {
                    ui.t("No permissions yet", "还没有权限记录")
                } else {
                    ui.t("Nothing matches", "没有匹配项")
                },
                if !self.rows.is_empty() {
                    ui.t("Press Esc to clear the filter.", "按 Esc 清除筛选。")
                } else if ready {
                    ui.t(
                        "Permissions appear as the calls that need them arrive.",
                        "需要权限的调用到达后，这里会列出所需权限。",
                    )
                } else {
                    ""
                },
                None,
            )
            .into_any_element()
        } else {
            let rows = self.rows.clone();
            let view = self.view.clone();
            let selected = self.selected.clone();
            let entity = cx.entity().downgrade();
            uniform_list("access", view.len(), move |range, _window, cx| {
                let _scope = crate::fsguard::RenderScope::enter();
                let ui = Ui::of(cx);
                range
                    .map(|position| {
                        let row = &rows[view[position]];
                        let selected = selected.as_ref() == Some(row.access());
                        access_row(&ui, row, position, selected, entity.clone())
                    })
                    .collect()
            })
            .flex_1()
            .min_h(px(0.0))
            .track_scroll(self.scroll.clone())
            .into_any_element()
        };
        div()
            .id("access-list")
            .size_full()
            .key_context("Access")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(rgb(t.panel))
            .child(self.render_subbar(&ui, cx))
            .child(self.render_head(&ui))
            .child(body)
    }
}

/// The badge for where a permission stands against the lock.
pub fn state_badge(ui: &Ui, state: LockState) -> Div {
    let t = ui.p;
    match state {
        LockState::NotInLock => widgets::badge(ui, ui.t("NOT IN LOCK", "未入锁"), t.amber),
        LockState::InLock => widgets::badge(ui, ui.t("IN LOCK", "已入锁"), t.green),
        LockState::Unused => widgets::badge(ui, ui.t("UNUSED", "未使用"), t.text_muted),
    }
}

fn access_row(
    ui: &Ui,
    row: &AccessRow,
    position: usize,
    selected: bool,
    view: gpui::WeakEntity<AccessView>,
) -> Stateful<Div> {
    let t = ui.p;
    let (glyph_text, color) = match row.state {
        LockState::NotInLock => ("+", t.amber),
        LockState::Unused => ("−", t.text_muted),
        LockState::InLock => ("", t.text_muted),
    };
    let access = row.access().clone();
    let service = access.action.split(':').next().unwrap_or("");
    let service_color = ui.service_color(&service_label(service));
    let menu_row = row.clone();
    let click_view = view.clone();
    let mut flags = div().flex().items_center().gap(px(4.0)).flex_shrink_0();
    if row.state == LockState::NotInLock {
        flags = flags.child(state_badge(ui, row.state));
    }
    if row.used.implied {
        flags = flags.child(widgets::badge(ui, ui.t("IMPLIED", "隐含"), t.text_muted));
    }
    if row.test {
        flags = flags.child(widgets::badge(ui, "# test", t.text_muted));
    }
    if row.used.errors > 0 {
        let n = row.used.errors;
        flags = flags.child(widgets::badge(
            ui,
            ui.tr(&crate::tr!("{n} failed", "{n} 次失败")).to_string(),
            t.red,
        ));
    }
    div()
        .id(ElementId::named_usize("access-row", position))
        .w_full()
        .flex()
        .items_center()
        .gap(px(theme::COL_GAP))
        .h(px(32.0))
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
        .when(row.state == LockState::Unused, |el| el.opacity(0.65))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let access = access.clone();
            click_view
                .update(cx, |view, cx| {
                    window.focus(&view.focus);
                    view.select(access, cx);
                })
                .ok();
        })
        .on_mouse_down(
            MouseButton::Right,
            move |event: &MouseDownEvent, window, cx| {
                let row = menu_row.clone();
                view.update(cx, |view, cx| {
                    window.focus(&view.focus);
                    view.select(row.access().clone(), cx);
                    cx.emit(AccessEvent::ContextMenu(event.position, row));
                })
                .ok();
            },
        )
        .child(
            div()
                .w(px(14.0))
                .flex_shrink_0()
                .font(fonts::mono_font())
                .text_size(px(text::MD))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color))
                .child(glyph_text),
        )
        .child(
            div()
                .w(px(220.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .child(glyph::dot(service_color, 6.0))
                .child(widgets::mono(ui, row.access().action.clone()).text_color(rgb(t.text))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    widgets::mono(ui, row.access().resource.clone())
                        .flex_1()
                        .text_color(rgb(t.text_dim)),
                )
                .child(flags),
        )
        .child(
            div()
                .w(px(56.0))
                .flex_shrink_0()
                .text_right()
                .font(fonts::mono_font())
                .text_size(px(text::SM))
                .text_color(rgb(t.text_muted))
                .child(if row.state == LockState::Unused {
                    "—".to_string()
                } else {
                    row.used.calls.to_string()
                }),
        )
}

/// The right-hand pane of the Access view: where a permission stands, the
/// lock actions, and the calls that needed it.
pub struct AccessDetail {
    session: Entity<Session>,
    row: Option<AccessRow>,
    calls: Vec<Arc<Trace>>,
    _subscription: gpui::Subscription,
}

impl EventEmitter<AccessEvent> for AccessDetail {}

impl AccessDetail {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&session, |_, _, event, cx| {
            if matches!(event, SessionEvent::Derived) {
                cx.notify();
            }
        });
        AccessDetail {
            session,
            row: None,
            calls: Vec::new(),
            _subscription: subscription,
        }
    }

    pub fn set_row(&mut self, row: Option<AccessRow>, cx: &mut Context<Self>) {
        if self.row == row {
            return;
        }
        self.calls = match row.as_ref() {
            Some(row) => self
                .session
                .read(cx)
                .store
                .rows()
                .iter()
                .filter(|call| call.uses(row.access()))
                .take(40)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        self.row = row;
        cx.notify();
    }

    fn render_actions(&self, ui: &Ui, row: &AccessRow, cx: &mut Context<Self>) -> Div {
        let can_edit = self.session.read(cx).lock_target().is_some();
        let mut actions = div().flex().flex_wrap().items_center().gap(px(8.0));
        if !can_edit {
            return actions.child(
                widgets::primary_button(
                    ui,
                    "choose-project",
                    ui.t("Open Project Folder…", "打开项目目录…"),
                )
                .tooltip(widgets::tip(
                    ui.t(
                        "Choose the project whose lenslet.lock to update",
                        "选择要更新其 lenslet.lock 的项目",
                    ),
                    Some(i18n::shortcut("⇧O")),
                ))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(super::OpenProject), cx)),
            );
        }
        let session = self.session.clone();
        let access = row.access().clone();
        match row.state {
            LockState::NotInLock => {
                let implied = row.used.implied;
                actions = actions.child(
                    widgets::primary_button(
                        ui,
                        "accept",
                        ui.t("Accept into lock", "写入 lenslet.lock"),
                    )
                    .tooltip(widgets::tip(
                        ui.t("Add it to lenslet.lock", "将其加入 lenslet.lock"),
                        Some(i18n::key("a")),
                    ))
                    .on_click(move |_, _, cx| {
                        let entries = vec![(access.clone(), false, implied)];
                        session.update(cx, |session, cx| session.accept(entries, cx));
                    }),
                );
            }
            LockState::InLock | LockState::Unused => {
                actions = actions.child(
                    widgets::danger_button(
                        ui,
                        "remove-lock",
                        ui.t("Remove from lock", "从 lenslet.lock 移除"),
                    )
                    .tooltip(widgets::tip(
                        ui.t(
                            "Delete its line from lenslet.lock",
                            "从 lenslet.lock 删除这一行",
                        ),
                        Some(i18n::key("⌫")),
                    ))
                    .on_click(move |_, _, cx| {
                        let entries = vec![access.clone()];
                        session.update(cx, |s, cx| s.remove_from_lock(entries, cx));
                    }),
                );
            }
        }
        let session = self.session.clone();
        let access = row.access().clone();
        let test = !row.test;
        actions.child(
            widgets::button(
                ui,
                "mark-test",
                if row.test {
                    ui.t("Unmark `# test`", "取消 `# test` 标记")
                } else {
                    ui.t("Mark `# test`", "标记 `# test`")
                },
            )
            .tooltip(widgets::tip(
                ui.t(
                    "Test-only permissions stay in the lock but out of the policy",
                    "仅测试需要的权限保留在 lock 中，但不进入策略",
                ),
                Some(i18n::key("t")),
            ))
            .on_click(move |_, _, cx| {
                let access = access.clone();
                session.update(cx, |session, cx| session.set_test_mark(access, test, cx));
            }),
        )
    }

    fn render_calls(&self, ui: &Ui, row: &AccessRow, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let mut calls = div().flex().flex_col();
        for (index, call) in self.calls.iter().enumerate() {
            let id = call.id.clone();
            calls = calls.child(
                div()
                    .id(ElementId::named_usize("access-call", index))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(t.row_hover)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(AccessEvent::SelectCall(id.clone()));
                    }))
                    .child(
                        div()
                            .w(px(36.0))
                            .font(fonts::mono_font())
                            .text_size(px(text::SM))
                            .text_color(ui.status_color(call.status))
                            .child(status_label(call.status)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(text::SM))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(t.text_soft))
                            .child(format!("{} {}", call.service_name(), operation_label(call))),
                    )
                    .child(
                        widgets::mono(ui, ellipsize(&call.semantic_target(), 44))
                            .flex_1()
                            .text_size(px(text::XS))
                            .text_color(rgb(t.text_muted)),
                    ),
            );
        }
        if self.calls.is_empty() {
            calls = calls.child(
                div()
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(if row.state == LockState::Unused {
                        ui.t("No call in this run needed it.", "本次运行没有调用需要它。")
                    } else {
                        ui.t(
                            "The calls that needed it are no longer in view.",
                            "相关调用已不在当前列表中。",
                        )
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(widgets::label(ui, ui.t("CALLS", "调用")))
            .child(calls)
    }
}

impl Render for AccessDetail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let container = div()
            .id("access-detail")
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(t.panel_raised));
        let Some(row) = self.row.clone() else {
            return container.child(widgets::empty_state(
                &ui,
                ui.t("Select a permission", "选择一项权限"),
                "",
                None,
            ));
        };
        let (service, action) = row
            .access()
            .action
            .split_once(':')
            .unwrap_or(("", row.access().action.as_str()));
        let service_color = ui.service_color(&service_label(service));
        let mut badges = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .child(glyph::dot(service_color, 7.0))
            .child(
                div()
                    .text_size(px(text::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(service_color)
                    .child(service_label(service)),
            )
            .child(state_badge(&ui, row.state));
        if row.used.implied {
            badges = badges.child(widgets::badge(&ui, ui.t("IMPLIED", "隐含"), t.accent));
        }
        if row.test {
            badges = badges.child(widgets::badge(&ui, "# test", t.text_muted));
        }
        if row.used.unverified {
            badges = badges.child(widgets::badge(&ui, ui.t("UNVERIFIED", "未核实"), t.amber));
        }
        let explain = match row.state {
            LockState::NotInLock if !self.session.read(cx).lock.exists() => ui.t(
                "There is no lenslet.lock yet: accepting creates it.",
                "还没有 lenslet.lock：写入时会创建它。",
            ),
            LockState::NotInLock => ui.t(
                "This run needed it and lenslet.lock does not have it.",
                "本次运行需要它，但 lenslet.lock 中没有。",
            ),
            LockState::InLock => ui.t("lenslet.lock already has it.", "lenslet.lock 中已有它。"),
            LockState::Unused => ui.t(
                "lenslet.lock has it, but no call in this run needed it.",
                "lenslet.lock 中有它，但本次运行没有调用需要它。",
            ),
        };
        let via = row.used.via.join(", ");
        container.child(
            div()
                .id("access-detail-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .gap(px(16.0))
                .p(px(16.0))
                .child(badges)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .font(fonts::mono_font())
                                .text_size(px(text::XL))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(t.text))
                                .child(action.to_string()),
                        )
                        .child(
                            div()
                                .font(fonts::mono_font())
                                .text_size(px(text::MD))
                                .line_height(px(18.0))
                                .text_color(rgb(t.text_dim))
                                .child(row.access().resource.clone()),
                        ),
                )
                .child(
                    div()
                        .text_size(px(text::MD))
                        .text_color(rgb(t.text_soft))
                        .child(explain),
                )
                .child(self.render_actions(&ui, &row, cx))
                .when(!via.is_empty(), |el| {
                    el.child(widgets::meta_row(
                        &ui,
                        ui.t("Needed by", "调用方"),
                        widgets::mono(&ui, via.clone()),
                    ))
                })
                .child(self.render_calls(&ui, &row, cx)),
        )
    }
}
