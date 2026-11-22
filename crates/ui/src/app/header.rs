//! The header (☰ · the run · ▶ the tests · Calls | Access · search) and
//! everything that opens from it: the run picker, the ▾ run menu, the ☰
//! menu, right-click menus and the dialogs.

use gpui::{
    AnyElement, Context, Corner, Div, ElementId, FontWeight, Pixels, Point, Stateful, Window,
    WindowControlArea, anchored, deferred, div, point, prelude::*, px, rgb,
};

use super::{Cmd, LensletApp, MenuEntry, Modal, Mode, session::Health, widgets};
use crate::{
    fonts, glyph, i18n,
    model::{ellipsize, emulator_name},
    prefs::Lang,
    theme::{self, Ui, text},
    tr,
};

impl LensletApp {
    pub(super) fn render_header(
        &self,
        ui: &Ui,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = ui.p;
        let mac = cfg!(target_os = "macos");
        let compact = window.viewport_size().width < px(1_220.0);
        let not_in_lock = self.session.read(cx).not_in_lock;
        let permissions = i18n::permissions(not_in_lock);
        let entity = cx.entity();
        let access_label = if not_in_lock > 0 {
            format!("{} · {not_in_lock}", ui.t("Access", "权限"))
        } else {
            ui.t("Access", "权限").to_string()
        };
        let modes = widgets::segmented(
            ui,
            vec![
                widgets::Segment {
                    id: "mode-calls",
                    label: ui.t("Calls", "调用").into(),
                    active: self.mode == Mode::Calls,
                    tooltip: Some((
                        ui.t("Every AWS call", "全部 AWS 调用").into(),
                        Some(i18n::shortcut("1")),
                    )),
                },
                widgets::Segment {
                    id: "mode-access",
                    label: access_label.into(),
                    active: self.mode == Mode::Access,
                    tooltip: Some((
                        if not_in_lock > 0 {
                            ui.tr(&tr!(
                                "{not_in_lock} {permissions} not in lenslet.lock",
                                "{not_in_lock} 项权限不在 lenslet.lock 中"
                            ))
                            .to_string()
                        } else {
                            ui.t("IAM permissions the run needed", "本次运行所需的 IAM 权限")
                                .to_string()
                        },
                        Some(i18n::shortcut("2")),
                    )),
                },
            ],
            move |index, window, cx| {
                let mode = [Mode::Calls, Mode::Access][index];
                entity.update(cx, |app, cx| app.run_cmd(Cmd::Mode(mode), window, cx));
            },
        );

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
            let open = matches!(self.modal, Modal::AppMenu);
            let button = widgets::icon_button(
                ui,
                "app-menu-btn",
                open,
                glyph::menu_icon(theme::h(t.text_dim, 1.0)),
            )
            .tooltip(widgets::tip(ui.t("Menu", "菜单"), None))
            .on_click(cx.listener(|app, _, _, cx| app.toggle_popover(Modal::AppMenu, cx)));
            bar = bar.child(popover_anchor(
                button.into_any_element(),
                open.then(|| self.render_app_menu(ui, cx)),
            ));
        }
        bar.child(self.render_run_picker(ui, cx))
            .child(self.render_run_button(ui, compact, cx))
            .child(modes)
            .child(div().flex_1())
            .child(
                div()
                    .w(px(if compact { 184.0 } else { 276.0 }))
                    .child(self.search.clone()),
            )
    }

    /// `python demo · exit 1 · 2 failed · against moto`, opening the picker.
    fn render_run_picker(&self, ui: &Ui, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let session = self.session.read(cx);
        let mut parts: Vec<(String, u32)> = Vec::new();
        let title = match (&session.health, session.run.as_ref()) {
            (Health::Loading, _) => ui.t("Loading…", "加载中…").to_string(),
            (Health::Missing, _) => ui.t("No runs yet", "还没有运行").to_string(),
            (Health::Error(_), _) => ui.t("Can't show this run", "无法显示此运行").to_string(),
            (Health::Ready, None) => String::new(),
            (Health::Ready, Some(run)) => {
                match run.report.as_ref().map(|r| r.exit_code) {
                    Some(Some(code)) => parts.push((
                        ui.tr(&tr!("exit {code}", "退出码 {code}")).to_string(),
                        if code == 0 { t.green } else { t.red },
                    )),
                    Some(None) => parts.push((ui.t("ended", "已结束").into(), t.text_muted)),
                    None => parts.push((ui.t("running", "运行中").into(), t.amber)),
                }
                let failed = session.failures.len();
                if failed > 0 {
                    parts.push((
                        ui.tr(&tr!("{failed} failed", "{failed} 个失败"))
                            .to_string(),
                        t.red,
                    ));
                }
                if let Some(name) = emulator_name(&run.header) {
                    parts.push((
                        ui.tr(&tr!("against {name}", "模拟器：{name}")).to_string(),
                        t.text_muted,
                    ));
                }
                ellipsize(&run.command_line(), 32)
            }
        };
        let open = matches!(self.modal, Modal::RunPicker);
        let mut button = div()
            .id("run-picker")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(28.0))
            .min_w(px(0.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .when(open, |el| el.bg(theme::h(t.text, 0.08)))
            .cursor_pointer()
            .hover(move |s| s.bg(theme::h(t.text, 0.06)))
            .tooltip(widgets::tip(
                ui.t("Switch run", "切换运行"),
                Some(format!("{} {}", i18n::shortcut("["), i18n::shortcut("]"))),
            ))
            .on_click(cx.listener(|app, _, _, cx| {
                app.session.update(cx, |s, cx| s.refresh_runs(cx));
                app.toggle_popover(Modal::RunPicker, cx);
            }))
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
            );
        for (part, color) in parts {
            button = button
                .child(div().text_color(rgb(t.text_faint)).child("·"))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(text::SM))
                        .text_color(rgb(color))
                        .child(part),
                );
        }
        button = button.child(glyph::chevron_down(theme::h(t.text_muted, 1.0)));
        popover_anchor(
            button.into_any_element(),
            open.then(|| self.render_run_popover(ui, cx)),
        )
        .flex_shrink()
        .min_w(px(0.0))
    }

    fn render_run_popover(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let session = self.session.read(cx);
        let (runs, groups, current_path, follows) = (
            session.runs.clone(),
            session.run_groups.clone(),
            session.path.clone(),
            session.source.follows_latest(),
        );
        let mut card = widgets::popover_card(ui, "runs-popover", 480.0)
            .max_h(px(440.0))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            }));
        card = card
            .child(self.menu_row(
                ui,
                "follow-newest",
                ui.t("Follow newest", "跟随最新运行"),
                None,
                follows,
                Cmd::FollowNewest,
                cx,
            ))
            .child(self.menu_row(
                ui,
                "open-run-file",
                ui.t("Open run file…", "打开运行文件…"),
                Some(i18n::shortcut("O")),
                false,
                Cmd::OpenFile,
                cx,
            ))
            .child(
                div().flex().gap(px(12.0)).px(px(10.0)).py(px(2.0)).child(
                    div()
                        .text_size(px(text::XS))
                        .text_color(rgb(t.text_muted))
                        .child({
                            let (older, newer) = (i18n::shortcut("["), i18n::shortcut("]"));
                            ui.tr(&tr!(
                                "{older} older · {newer} newer",
                                "{older} 更早 · {newer} 更新"
                            ))
                            .to_string()
                        }),
                ),
            );
        if runs.is_empty() {
            card = card.child(widgets::separator(ui)).child(
                div()
                    .p(px(12.0))
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t("No recorded runs", "没有运行记录")),
            );
        }
        const SHOWN: usize = 40;
        let mut shown = 0;
        for group in groups.iter() {
            if shown >= SHOWN {
                break;
            }
            card = card.child(widgets::separator(ui)).child(
                div()
                    .px(px(10.0))
                    .pt(px(4.0))
                    .pb(px(2.0))
                    .child(widgets::label(ui, group.project.clone())),
            );
            for &index in group.runs.iter().take(SHOWN - shown) {
                shown += 1;
                let run = &runs[index];
                let current = current_path.as_ref() == Some(&run.path);
                let (status, color) = if run.outdated() {
                    (ui.t("older format", "旧格式").to_string(), t.text_muted)
                } else {
                    match run.report.as_ref().map(|r| r.exit_code) {
                        Some(Some(0)) => (ui.t("passed", "通过").to_string(), t.green),
                        Some(Some(code)) => (
                            ui.tr(&tr!("exit {code}", "退出码 {code}")).to_string(),
                            t.red,
                        ),
                        Some(None) => (ui.t("ended", "已结束").into(), t.text_muted),
                        None => (ui.t("running", "运行中").into(), t.amber),
                    }
                };
                let label = run
                    .header
                    .as_ref()
                    .map(|h| h.command.join(" "))
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| run.id.clone());
                let mut detail = vec![run.id.clone()];
                if let Some(report) = run.report.as_ref() {
                    detail.push(i18n::calls_count(report.calls, ui.lang));
                }
                if let Some(name) = run.header.as_ref().and_then(emulator_name) {
                    detail.push(ui.tr(&tr!("against {name}", "模拟器：{name}")).to_string());
                }
                let path = run.path.clone();
                card = card.child(
                    div()
                        .id(ElementId::named_usize("run", index))
                        .w_full()
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
                            app.run_cmd(Cmd::OpenRun(path.clone()), window, cx)
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
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .font(fonts::mono_font())
                                        .text_size(px(text::XS))
                                        .text_color(rgb(t.text_muted))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(detail.join(" · ")),
                                ),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(text::SM))
                                .text_color(rgb(color))
                                .child(status),
                        ),
                );
            }
        }
        widgets::appear(ui, "runs-popover-anim", card)
    }

    /// ▶ <test command> (■ Stop while running) and ▾ for "Edit test command…".
    fn render_run_button(&self, ui: &Ui, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let main = if self.run.is_some() {
            widgets::danger_button(ui, "hdr-stop", ui.t("■ Stop", "■ 停止"))
                .tooltip(widgets::tip(
                    ui.t("Stop the tests", "停止测试"),
                    Some(i18n::shortcut(".")),
                ))
                .on_click(cx.listener(|app, _, window, cx| app.run_cmd(Cmd::Stop, window, cx)))
        } else {
            let (label, tip) = match self.test_command(cx) {
                _ if self.starting => (
                    ui.t("Starting…", "正在启动…").to_string(),
                    ui.t("Starting the tests", "正在启动测试").to_string(),
                ),
                Some(command) => (
                    format!("▶ {}", ellipsize(&command, if compact { 14 } else { 22 })),
                    format!(
                        "{} · {}",
                        ui.tr(&tr!("Run {command}", "运行 {command}")),
                        ui.tr(&self.emulator_status().message())
                    ),
                ),
                None => (
                    ui.t("▶ Set test command…", "▶ 设置测试命令…").to_string(),
                    ui.t(
                        "Lenslet did not find this project's tests",
                        "Lenslet 没有找到此项目的测试命令",
                    )
                    .to_string(),
                ),
            };
            widgets::primary_button(ui, "hdr-run", label)
                .tooltip(widgets::tip(tip, Some(i18n::shortcut("R"))))
                .on_click(cx.listener(|app, _, window, cx| app.run_cmd(Cmd::Run, window, cx)))
        };
        let open = matches!(self.modal, Modal::RunMenu);
        let more = widgets::button(ui, "hdr-run-menu", "▾")
            .px(px(6.0))
            .on_click(cx.listener(|app, _, _, cx| app.toggle_popover(Modal::RunMenu, cx)));
        let menu = open.then(|| {
            let card = widgets::popover_card(ui, "run-menu", 220.0)
                .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                    app.modal = Modal::None;
                    cx.notify();
                }))
                .child(self.menu_row(
                    ui,
                    "edit-command",
                    ui.t("Edit test command…", "编辑测试命令…"),
                    None,
                    false,
                    Cmd::EditCommand,
                    cx,
                ));
            widgets::appear(ui, "run-menu-anim", card)
        });
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.0))
            .child(main)
            .child(popover_anchor(more.into_any_element(), menu))
            .into_any_element()
    }

    fn render_app_menu(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let mut card = widgets::popover_card(ui, "app-menu", 260.0).on_mouse_down_out(cx.listener(
            |app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            },
        ));
        let lang = self.prefs.lang;
        let items: [MenuLine; 11] = [
            Some((
                ui.t("Open Project Folder…", "打开项目目录…"),
                Some(i18n::shortcut("⇧O")),
                false,
                Cmd::OpenProject,
            )),
            Some((
                ui.t("Open Run File…", "打开运行文件…"),
                Some(i18n::shortcut("O")),
                false,
                Cmd::OpenFile,
            )),
            None,
            Some((
                ui.t("Run Tests", "运行测试"),
                Some(i18n::shortcut("R")),
                false,
                Cmd::Run,
            )),
            Some((
                ui.t("Edit Test Command…", "编辑测试命令…"),
                None,
                false,
                Cmd::EditCommand,
            )),
            None,
            Some((
                "Language: English",
                None,
                lang == Lang::En,
                Cmd::Lang(Lang::En),
            )),
            Some((
                "Language: 中文",
                None,
                lang == Lang::Zh,
                Cmd::Lang(Lang::Zh),
            )),
            None,
            Some((
                ui.t("About Lenslet", "关于 Lenslet"),
                None,
                false,
                Cmd::About,
            )),
            Some((
                ui.t("Quit Lenslet", "退出 Lenslet"),
                Some(i18n::shortcut("Q")),
                false,
                Cmd::Quit,
            )),
        ];
        for (index, item) in items.into_iter().enumerate() {
            card = match item {
                None => card.child(widgets::separator(ui)),
                Some((title, hint, checked, cmd)) => card.child(self.menu_row(
                    ui,
                    ElementId::named_usize("app-menu", index),
                    title,
                    hint,
                    checked,
                    cmd,
                    cx,
                )),
            };
        }
        widgets::appear(ui, "app-menu-anim", card)
    }

    #[allow(clippy::too_many_arguments)]
    fn menu_row(
        &self,
        ui: &Ui,
        id: impl Into<ElementId>,
        title: &str,
        hint: Option<String>,
        selected: bool,
        cmd: Cmd,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        widgets::menu_item(ui, id, title.to_string(), hint, selected, false)
            .on_click(cx.listener(move |app, _, window, cx| app.run_cmd(cmd.clone(), window, cx)))
    }

    /// The right-click menu, dialogs and the about card.
    pub(super) fn render_overlay(&self, ui: &Ui, cx: &mut Context<Self>) -> Option<AnyElement> {
        match &self.modal {
            Modal::Menu(position, entries) => Some(self.render_menu(ui, *position, entries, cx)),
            Modal::EditCommand => Some(self.render_edit_command(ui, cx)),
            Modal::Confirm(entries) => Some(self.render_confirm(ui, entries, cx)),
            Modal::About => Some(self.render_about(ui, cx)),
            Modal::None | Modal::AppMenu | Modal::RunPicker | Modal::RunMenu => None,
        }
    }

    fn render_menu(
        &self,
        ui: &Ui,
        position: Point<Pixels>,
        entries: &[MenuEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = widgets::popover_card(ui, "context-menu", 260.0).on_mouse_down_out(
            cx.listener(|app, _, _, cx| {
                app.modal = Modal::None;
                cx.notify();
            }),
        );
        for (index, entry) in entries.iter().enumerate() {
            card = card.child(self.menu_row(
                ui,
                ElementId::named_usize("menu", index),
                &entry.title,
                entry.hint.clone(),
                false,
                entry.cmd.clone(),
                cx,
            ));
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

    fn render_edit_command(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let project = self
            .project_dir(cx)
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let from_doctor = match self.doctor.as_ref() {
            Some(Ok(doctor)) => doctor.test_command.clone(),
            _ => None,
        };
        let hint = match from_doctor {
            Some(found) => ui
                .tr(&tr!(
                    "Lenslet found: {found}. It runs as `lenslet run -- <command>` in {project}.",
                    "Lenslet 找到的命令：{found}。它会在 {project} 中以 `lenslet run -- <命令>` 运行。"
                ))
                .to_string(),
            None => ui
                .tr(&tr!(
                    "It runs as `lenslet run -- <command>` in {project}.",
                    "它会在 {project} 中以 `lenslet run -- <命令>` 运行。"
                ))
                .to_string(),
        };
        let card =
            widgets::dialog_card(ui, "edit-command", 520.0, ui.t("Test command", "测试命令"))
                .on_mouse_down_out(cx.listener(|app, _, window, cx| app.close_modal(window, cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .p(px(18.0))
                        .child(self.command_field.clone())
                        .child(
                            div()
                                .text_size(px(text::SM))
                                .line_height(px(16.0))
                                .text_color(rgb(t.text_muted))
                                .child(hint),
                        ),
                )
                .child(dialog_buttons(
                    ui,
                    widgets::tabbable(
                        ui,
                        widgets::button(ui, "command-cancel", ui.t("Cancel", "取消")),
                    )
                    .on_click(cx.listener(|app, _, window, cx| app.close_modal(window, cx))),
                    widgets::tabbable(
                        ui,
                        widgets::primary_button(ui, "command-save", ui.t("Save", "保存")),
                    )
                    .on_click(cx.listener(|app, _, window, cx| app.save_command(window, cx))),
                ));
        widgets::scrim(ui)
            .child(widgets::appear(ui, "edit-command-anim", card))
            .into_any_element()
    }

    fn render_confirm(
        &self,
        ui: &Ui,
        entries: &[(crate::model::Access, bool, bool)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui.p;
        let n = entries.len();
        let permissions = i18n::permissions(n);
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
        for (access, _, implied) in entries {
            list = list.child(
                widgets::mono(
                    ui,
                    format!(
                        "{}  {}{}",
                        access.action,
                        access.resource,
                        if *implied { "  # implied" } else { "" }
                    ),
                )
                .text_color(rgb(t.text_soft)),
            );
        }
        let card = widgets::dialog_card(
            ui,
            "confirm",
            560.0,
            ui.tr(&tr!(
                "Accept {n} {permissions} into lenslet.lock?",
                "将 {n} 项权限写入 lenslet.lock？"
            ))
            .to_string(),
        )
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
                        .child(ui.t(
                            "Your tests will be allowed to use these from now on:",
                            "之后测试将被允许使用以下权限：",
                        )),
                )
                .child(list),
        )
        .child(dialog_buttons(
            ui,
            widgets::tabbable(
                ui,
                widgets::button(ui, "confirm-no", ui.t("Cancel", "取消")),
            )
            .on_click(cx.listener(|app, _, window, cx| app.close_modal(window, cx))),
            widgets::tabbable(
                ui,
                widgets::primary_button(
                    ui,
                    "confirm-yes",
                    ui.tr(&tr!("Accept all {n}", "全部写入（{n}）")).to_string(),
                ),
            )
            .on_click(cx.listener(|app, _, window, cx| app.confirm_yes(window, cx))),
        ));
        widgets::scrim(ui)
            .child(widgets::appear(ui, "confirm-anim", card))
            .into_any_element()
    }

    fn render_about(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let card = div()
            .id("about")
            .w(px(400.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .p(px(24.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(rgb(t.border))
            .bg(rgb(t.panel_raised))
            .shadow(ui.elevation())
            .on_mouse_down_out(cx.listener(|app, _, window, cx| app.close_modal(window, cx)))
            .child(glyph::lens_mark(&t, 52.0))
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(px(20.0))
                    .font_weight(FontWeight::BOLD)
                    .child("Lenslet"),
            )
            .child(
                div()
                    .font(fonts::mono_font())
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_muted))
                    .child(format!(
                        "{} {}",
                        ui.t("Version", "版本"),
                        env!("CARGO_PKG_VERSION")
                    )),
            )
            .child(
                div()
                    .text_center()
                    .text_size(px(text::MD))
                    .line_height(px(18.0))
                    .text_color(rgb(t.text_soft))
                    .child(ui.t(
                        "See what AWS access your code needs, and why its AWS calls failed.",
                        "看清代码需要哪些 AWS 权限，以及它的 AWS 调用为何失败。",
                    )),
            )
            .child(
                div()
                    .text_center()
                    .text_size(px(text::XS))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t(
                        "Local only: talks to emulators, never to AWS. No telemetry.",
                        "仅限本地：只连接模拟器，从不连接 AWS。无遥测。",
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        widgets::tabbable(
                            ui,
                            widgets::button(ui, "about-docs", ui.t("Documentation", "使用文档")),
                        )
                        .on_click(|_, _, cx| cx.open_url(crate::DOCS_URL)),
                    )
                    .child(
                        widgets::tabbable(
                            ui,
                            widgets::primary_button(ui, "about-ok", ui.t("OK", "好")),
                        )
                        .on_click(cx.listener(|app, _, window, cx| app.close_modal(window, cx))),
                    ),
            );
        widgets::scrim(ui)
            .child(widgets::appear(ui, "about-anim", card))
            .into_any_element()
    }
}

/// A ☰ menu line: title, shortcut, checked, command (`None`: a separator).
type MenuLine<'a> = Option<(&'a str, Option<String>, bool, Cmd)>;

/// A button with its popover hanging below it.
fn popover_anchor(button: AnyElement, popover: Option<AnyElement>) -> Div {
    let mut wrap = div().relative().child(button);
    if let Some(popover) = popover {
        wrap = wrap.child(deferred(
            anchored()
                .anchor(Corner::TopLeft)
                .offset(point(px(0.0), px(32.0)))
                .snap_to_window_with_margin(px(8.0))
                .child(popover),
        ));
    }
    wrap
}

fn dialog_buttons(ui: &Ui, cancel: Stateful<Div>, ok: Stateful<Div>) -> Div {
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .px(px(18.0))
        .py(px(12.0))
        .border_t_1()
        .border_color(rgb(ui.p.border_subtle))
        .child(cancel)
        .child(ok)
}
