//! The command palette (⌘K / Ctrl+K): every command, run and preference in
//! one filterable list. Entries are built from state already in memory when
//! the palette opens or its query changes — never from disk.

use gpui::{AnyElement, Context, ElementId, FontWeight, Window, div, prelude::*, px, rgb};

use super::{
    Cmd, FilterCmd, LensletApp, LockCmd, MenuEntry, Modal, Mode, PrefCmd, RunNav,
    session::Baseline, widgets,
};
use crate::{
    daemon::ShellFormat,
    i18n,
    model::StatusFilter,
    prefs::{AccentColor, Density, Lang, ThemeMode},
    theme::{self, Ui, text},
};

impl LensletApp {
    pub(super) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Modal::Palette) {
            self.close_palette(window, cx);
            return;
        }
        self.modal = Modal::Palette;
        self.palette_index = 0;
        self.palette_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.refresh_palette(cx);
        cx.notify();
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
    }

    pub(super) fn palette_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.palette_entries.len();
        if count > 0 {
            self.palette_index =
                (self.palette_index as isize + delta).clamp(0, count as isize - 1) as usize;
            cx.notify();
        }
    }

    pub(super) fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.palette_entries.get(self.palette_index).cloned();
        self.close_palette(window, cx);
        if let Some(entry) = entry {
            self.run_cmd(entry.cmd, window, cx);
        }
    }

    /// Rebuild the filtered entries (on open and on each query change).
    pub(super) fn refresh_palette(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::Palette) {
            return;
        }
        let query = self.palette_input.read(cx).query().to_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        let entries = self.palette_all(cx);
        self.palette_entries = entries
            .into_iter()
            .filter(|entry| {
                let hay = entry.title.to_lowercase();
                tokens.iter().all(|token| hay.contains(token))
            })
            .take(60)
            .collect();
        self.palette_index = self
            .palette_index
            .min(self.palette_entries.len().saturating_sub(1));
        cx.notify();
    }

    fn palette_all(&self, cx: &Context<Self>) -> Vec<MenuEntry> {
        let ui = Ui::of(cx);
        let session = self.session.read(cx);
        let mut items: Vec<MenuEntry> = Vec::with_capacity(64);
        let mut add = |title: String, hint: Option<String>, cmd: Cmd| {
            items.push(MenuEntry {
                title: title.into(),
                hint,
                cmd,
            });
        };
        if self.managed_run.is_some() {
            add(
                ui.t("Stop the run", "停止运行").into(),
                Some(i18n::shortcut(".")),
                Cmd::StopRun,
            );
        } else {
            add(
                ui.t("Run tests", "运行测试").into(),
                Some(i18n::shortcut("R")),
                Cmd::Run,
            );
        }
        add(
            ui.t("Run options…", "运行选项…").into(),
            Some(i18n::shortcut("N")),
            Cmd::RunOptions,
        );
        add(
            ui.t("Open a terminal through Lenslet", "打开经过 Lenslet 的终端")
                .into(),
            None,
            Cmd::OpenTerminal,
        );
        for fmt in ShellFormat::ALL {
            add(
                format!(
                    "{} ({})",
                    ui.t("Copy environment", "复制环境变量"),
                    fmt.label()
                ),
                None,
                Cmd::CopyEnv(fmt),
            );
        }
        if self.managed_proxy.is_some() {
            add(
                ui.t("Stop the proxy", "停止本地代理").into(),
                None,
                Cmd::Proxy(false),
            );
        } else if !self.proxy.running {
            let port = self.proxy_port;
            add(
                ui.tr(&crate::tr!(
                    "Start the proxy on :{port}",
                    "在 :{port} 启动本地代理"
                ))
                .into(),
                None,
                Cmd::Proxy(true),
            );
        }
        add(
            if session.demo {
                ui.t("Exit the demo", "退出演示").into()
            } else {
                ui.t("Explore the demo", "浏览演示").into()
            },
            None,
            Cmd::Demo(!session.demo),
        );
        add(
            ui.t("Open project folder…", "打开项目目录…").into(),
            None,
            Cmd::OpenWorkspace,
        );
        add(
            ui.t("Open run file…", "打开运行文件…").into(),
            Some(i18n::shortcut("O")),
            Cmd::OpenFile,
        );
        add(
            ui.t("Export run summary…", "导出运行摘要…").into(),
            Some(i18n::shortcut("S")),
            Cmd::ExportReport,
        );
        add(
            ui.t(
                "Generate GitHub Actions workflow…",
                "生成 GitHub Actions 工作流…",
            )
            .into(),
            None,
            Cmd::InitCi,
        );
        add(
            ui.t("Show Calls", "显示请求").into(),
            Some(i18n::shortcut("1")),
            Cmd::Mode(Mode::Calls),
        );
        add(
            ui.t("Show Access", "显示权限").into(),
            Some(i18n::shortcut("2")),
            Cmd::Mode(Mode::Access),
        );
        add(
            ui.t("Toggle Output", "显示/隐藏输出").into(),
            Some(i18n::shortcut("J")),
            Cmd::ToggleOutput,
        );
        add(
            ui.t("Toggle sidebar", "显示/隐藏侧栏").into(),
            Some(i18n::shortcut("0")),
            Cmd::ToggleSidebar,
        );
        add(
            ui.t("Toggle inspector", "显示/隐藏详情栏").into(),
            Some(i18n::shortcut("B")),
            Cmd::ToggleInspector,
        );
        add(
            ui.t("Search", "搜索").into(),
            Some("/".into()),
            Cmd::FocusSearch,
        );
        if session.evicted > 0 && !session.store.is_unbounded() {
            let n = session.evicted;
            add(
                ui.tr(&crate::tr!("Load all calls (+{n})", "加载全部调用（+{n}）"))
                    .into(),
                None,
                Cmd::Runs(RunNav::LoadAll),
            );
        }
        if let Some(call) = self.inspector.read(cx).call().cloned() {
            if self.inspector.read(cx).can_replay(cx) {
                add(
                    ui.t("Replay the call", "重放请求").into(),
                    Some(i18n::shortcut("⏎")),
                    Cmd::Replay,
                );
                add(
                    ui.t("Edit & Replay…", "编辑并重放…").into(),
                    Some(i18n::shortcut("⇧⏎")),
                    Cmd::EditReplay,
                );
            }
            add(
                ui.t("Copy the call as curl", "将请求复制为 curl").into(),
                Some(i18n::shortcut("⇧C")),
                Cmd::CopyCurl(call),
            );
            add(
                ui.t("Save payload…", "保存报文…").into(),
                None,
                Cmd::SavePayload,
            );
        }
        add(
            ui.t("Copy IAM policy JSON", "复制 IAM 策略 JSON").into(),
            None,
            Cmd::CopyPolicy,
        );
        let not_in_lock = session
            .access_rows
            .iter()
            .filter(|row| row.used && !row.in_lock)
            .count();
        if not_in_lock > 0 {
            let n = not_in_lock;
            add(
                ui.tr(&crate::tr!(
                    "Review {n} permissions not in the lock",
                    "查看 {n} 项未入锁权限"
                ))
                .into(),
                None,
                Cmd::Lock(LockCmd::ReviewNotInLock),
            );
        }
        for (status, en, zh) in [
            (StatusFilter::Errors, "Filter: errors", "筛选：错误"),
            (
                StatusFilter::Changed,
                "Filter: changed vs baseline",
                "筛选：与基线不同",
            ),
            (StatusFilter::Retries, "Filter: retries", "筛选：重试"),
            (StatusFilter::Slow, "Filter: slow", "筛选：慢请求"),
            (StatusFilter::Blocked, "Filter: blocked", "筛选：已拦截"),
        ] {
            add(
                ui.t(en, zh).into(),
                None,
                Cmd::Filter(FilterCmd::Status(status)),
            );
        }
        for facet in session.store.facets() {
            add(
                format!("{} {}", ui.t("Filter: service", "筛选：服务"), facet.label),
                None,
                Cmd::Filter(FilterCmd::Service(facet.id)),
            );
        }
        add(
            ui.t("Clear filters", "清除筛选").into(),
            Some("Esc".into()),
            Cmd::Filter(FilterCmd::Clear),
        );
        add(
            ui.t("Compare with lenslet.lock", "对比 lenslet.lock")
                .into(),
            None,
            Cmd::SetBaseline(Baseline::Lock),
        );
        add(
            ui.t("Compare with the previous run", "对比上一次运行")
                .into(),
            None,
            Cmd::SetBaseline(Baseline::Previous),
        );
        if session.source.runs_dir().is_some() {
            add(
                ui.t("Previous run", "上一个运行").into(),
                Some(i18n::shortcut("[")),
                Cmd::Runs(RunNav::Prev),
            );
            add(
                ui.t("Next run", "下一个运行").into(),
                Some(i18n::shortcut("]")),
                Cmd::Runs(RunNav::Next),
            );
            add(
                ui.t("Follow the newest run", "跟随最新运行").into(),
                None,
                Cmd::Runs(RunNav::Latest),
            );
        }
        add(
            ui.t("Reload the run", "重新加载运行").into(),
            None,
            Cmd::Runs(RunNav::Reload),
        );
        for run in session.runs.iter().take(20) {
            add(
                format!(
                    "{} {} · {}",
                    ui.t("Open run", "打开运行"),
                    run.id,
                    run.label()
                ),
                None,
                Cmd::Runs(RunNav::Open(run.path.clone())),
            );
        }
        for run in session.runs.iter().filter(|r| r.ended()).take(10) {
            add(
                format!("{} {}", ui.t("Compare with run", "对比运行"), run.id),
                None,
                Cmd::SetBaseline(Baseline::Run(run.path.clone())),
            );
        }
        for mode in ThemeMode::ALL {
            add(
                format!(
                    "{} {}",
                    ui.t("Appearance:", "外观："),
                    i18n::theme_label(mode, ui.lang)
                ),
                None,
                Cmd::Pref(PrefCmd::Theme(mode)),
            );
        }
        for accent in AccentColor::ALL {
            add(
                format!(
                    "{} {}",
                    ui.t("Accent:", "强调色："),
                    i18n::accent_label(accent, ui.lang)
                ),
                None,
                Cmd::Pref(PrefCmd::Accent(accent)),
            );
        }
        for density in [Density::Comfortable, Density::Compact] {
            add(
                format!(
                    "{} {}",
                    ui.t("Density:", "密度："),
                    i18n::density_label(density, ui.lang)
                ),
                None,
                Cmd::Pref(PrefCmd::Density(density)),
            );
        }
        add(
            "Language: English".into(),
            None,
            Cmd::Pref(PrefCmd::Lang(Lang::En)),
        );
        add(
            "语言：中文".into(),
            None,
            Cmd::Pref(PrefCmd::Lang(Lang::Zh)),
        );
        add(
            ui.t("Settings…", "设置…").into(),
            Some(i18n::shortcut(",")),
            Cmd::OpenSettings,
        );
        add(
            ui.t("About Lenslet", "关于 Lenslet").into(),
            None,
            Cmd::OpenAbout,
        );
        add(
            ui.t("Quit Lenslet", "退出 Lenslet").into(),
            Some(i18n::shortcut("Q")),
            Cmd::Quit,
        );
        items
    }

    pub(super) fn render_palette(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .max_h(px(380.0))
            .overflow_y_scroll()
            .p(px(4.0));
        if self.palette_entries.is_empty() {
            list = list.child(
                div()
                    .p(px(12.0))
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t("No matching command", "没有匹配的命令")),
            );
        }
        for (index, entry) in self.palette_entries.iter().enumerate() {
            let cmd = entry.cmd.clone();
            list = list.child(
                widgets::menu_item(
                    ui,
                    ElementId::named_usize("palette-item", index),
                    entry.title.clone(),
                    entry.hint.clone(),
                    index == self.palette_index,
                    false,
                )
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.close_palette(window, cx);
                    app.run_cmd(cmd.clone(), window, cx);
                })),
            );
        }
        let card = div()
            .id("palette")
            .w(px(560.0))
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .border_1()
            .border_color(rgb(t.border))
            .bg(rgb(t.panel_raised))
            .shadow(ui.elevation())
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|app, _, window, cx| app.close_palette(window, cx)))
            .child(self.palette_input.clone())
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .h(px(28.0))
                    .px(px(12.0))
                    .border_t_1()
                    .border_color(rgb(t.border_subtle))
                    .text_size(px(text::XS))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(t.text_muted))
                    .child(format!("↑↓  {}", ui.t("navigate", "选择")))
                    .child(format!("{}  {}", i18n::key("⏎"), ui.t("run", "执行")))
                    .child(format!("Esc  {}", ui.t("close", "关闭"))),
            );
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(88.0))
            .bg(theme::h(0x000000, if ui.dark() { 0.32 } else { 0.12 }))
            .child(widgets::appear(ui, "palette-anim", card))
            .into_any_element()
    }
}
