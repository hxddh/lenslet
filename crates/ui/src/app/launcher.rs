//! Run options (the header's ▾): the project folder, the test command, the
//! one Enforce toggle, Docker access and the emulator Lenslet will use. It
//! only saves settings; the header's ▶ is the one place that runs.
//!
//! Also the About dialog.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter, FontWeight,
    PathPromptOptions, Render, Window, div, prelude::*, px, rgb,
};

use super::widgets;
use crate::{
    daemon::{self, EmulatorStatus},
    fonts, glyph, icons,
    search::SearchInput,
    theme::{self, Ui, text},
};

/// What the ▶ button runs and how.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSettings {
    pub command: String,
    pub cwd: PathBuf,
    pub upstream: Option<String>,
    pub routes: Vec<String>,
    pub enforce: bool,
    pub allow_docker: bool,
}

pub enum LauncherEvent {
    Close,
    Save(RunSettings),
    OpenTerminal,
    CopyEnv,
    CopyInstall,
}

pub struct RunLauncher {
    cwd: PathBuf,
    recent_workspaces: Vec<PathBuf>,
    presets: Vec<String>,
    enforce: bool,
    allow_docker: bool,
    /// The Enforce setting of the proxy or run that is active now, if any.
    active_enforce: Option<bool>,
    advanced_open: bool,
    status: EmulatorStatus,
    command_input: Entity<SearchInput>,
    upstream_input: Entity<SearchInput>,
    routes_input: Entity<SearchInput>,
}

impl EventEmitter<LauncherEvent> for RunLauncher {}

/// Whether a changed Enforce toggle only takes effect on the next run.
pub fn enforce_applies_next_run(active: Option<bool>, wanted: bool) -> bool {
    active.is_some_and(|active| active != wanted)
}

impl RunLauncher {
    pub fn new(home: PathBuf, cx: &mut Context<Self>) -> Self {
        let command_input =
            cx.new(|cx| SearchInput::new_field("", "pytest / npm test / cargo test", true, cx));
        let upstream_input = cx.new(|cx| {
            SearchInput::new_field("", "http://127.0.0.1:4566 (empty: automatic)", true, cx)
        });
        let routes_input = cx.new(|cx| {
            SearchInput::new_field(
                "",
                "s3=http://127.0.0.1:9000, dynamodb=http://127.0.0.1:8000",
                true,
                cx,
            )
        });
        Self {
            cwd: home,
            recent_workspaces: Vec::new(),
            presets: Vec::new(),
            enforce: false,
            allow_docker: false,
            active_enforce: None,
            advanced_open: false,
            status: EmulatorStatus::Checking,
            command_input,
            upstream_input,
            routes_input,
        }
    }

    pub fn open(
        &mut self,
        settings: RunSettings,
        recent_workspaces: Vec<PathBuf>,
        status: EmulatorStatus,
        active_enforce: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cwd = settings.cwd.clone();
        self.recent_workspaces = recent_workspaces;
        self.enforce = settings.enforce;
        self.allow_docker = settings.allow_docker;
        self.active_enforce = active_enforce;
        self.status = status;
        self.advanced_open = settings
            .upstream
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || !settings.routes.is_empty();
        self.command_input
            .update(cx, |i, cx| i.set_text(settings.command, cx));
        self.upstream_input.update(cx, |i, cx| {
            i.set_text(settings.upstream.unwrap_or_default(), cx)
        });
        self.routes_input
            .update(cx, |i, cx| i.set_text(settings.routes.join(", "), cx));
        self.refresh_presets(settings.cwd, cx);
        let focus = self.command_input.read(cx).handle();
        window.focus(&focus);
        cx.notify();
    }

    pub fn set_status(&mut self, status: EmulatorStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    fn set_cwd(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.cwd = dir.clone();
        self.refresh_presets(dir, cx);
        cx.notify();
    }

    fn refresh_presets(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let detected = cx
                .background_executor()
                .spawn(async move { daemon::detect_project_commands(&dir) })
                .await;
            this.update(cx, |launcher, cx| {
                let empty = launcher.command_input.read(cx).query().trim().is_empty();
                if empty && let Some(first) = detected.first().cloned() {
                    launcher
                        .command_input
                        .update(cx, |i, cx| i.set_text(first, cx));
                }
                launcher.presets = detected;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn prompt_folder(&mut self, cx: &mut Context<Self>) {
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
                this.update(cx, |launcher, cx| launcher.set_cwd(dir, cx))
                    .ok();
            }
        })
        .detach();
    }

    pub fn settings(&self, cx: &App) -> RunSettings {
        let upstream = self.upstream_input.read(cx).query().trim().to_string();
        RunSettings {
            command: self.command_input.read(cx).query().trim().to_string(),
            cwd: self.cwd.clone(),
            upstream: (!upstream.is_empty()).then_some(upstream),
            routes: self
                .routes_input
                .read(cx)
                .query()
                .split([',', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            enforce: self.enforce,
            allow_docker: self.allow_docker,
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        cx.emit(LauncherEvent::Save(settings));
    }
}

fn section(ui: &Ui, title: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::label(ui, title.to_string()))
}

impl Render for RunLauncher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;

        let current = self.command_input.read(cx).query().trim().to_string();
        let mut presets = div().flex().flex_wrap().items_center().gap(px(6.0));
        for (ix, cmd) in self.presets.iter().take(6).enumerate() {
            let value = cmd.clone();
            presets = presets.child(
                widgets::tabbable(
                    &ui,
                    widgets::chip(
                        &ui,
                        ElementId::named_usize("preset-cmd", ix),
                        cmd.clone(),
                        None,
                        current == *cmd,
                        t.accent,
                    ),
                )
                .on_click(cx.listener(move |launcher, _, _, cx| {
                    let value = value.clone();
                    launcher
                        .command_input
                        .update(cx, |i, cx| i.set_text(value, cx));
                    cx.notify();
                })),
            );
        }

        let mut recent = div().flex().flex_wrap().items_center().gap(px(6.0));
        for (ix, dir) in self
            .recent_workspaces
            .iter()
            .filter(|d| **d != self.cwd)
            .take(4)
            .enumerate()
        {
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("folder")
                .to_string();
            let target = dir.clone();
            recent = recent.child(
                widgets::tabbable(
                    &ui,
                    widgets::chip(
                        &ui,
                        ElementId::named_usize("recent-ws", ix),
                        name,
                        None,
                        false,
                        t.text_muted,
                    ),
                )
                .tooltip(widgets::tip(dir.display().to_string(), None))
                .on_click(cx.listener(move |launcher, _, _, cx| {
                    launcher.set_cwd(target.clone(), cx);
                })),
            );
        }

        let status_color = if self.status.ok() { t.green } else { t.amber };
        let mut status_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(glyph::dot(theme::h(status_color, 1.0), 7.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_soft))
                    .child(ui.tr(&self.status.message()).to_string()),
            );
        if self.status == EmulatorStatus::Missing {
            status_row = status_row.child(
                widgets::tabbable(
                    &ui,
                    widgets::button(&ui, "copy-install", ui.t("Copy", "复制")),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::CopyInstall))),
            );
        }

        let pending = enforce_applies_next_run(self.active_enforce, self.enforce);

        let advanced_toggle = widgets::tabbable(
            &ui,
            div()
                .id("launcher-adv-toggle")
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(2.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme::h(t.border, 0.0))
                .cursor_pointer()
                .text_size(px(text::SM))
                .text_color(rgb(t.text_muted))
                .hover(move |s| s.text_color(rgb(t.text)))
                .child(if self.advanced_open { "▾" } else { "▸" })
                .child(ui.t("Use a specific emulator", "指定模拟器")),
        )
        .on_click(cx.listener(|l, _, _, cx| {
            l.advanced_open = !l.advanced_open;
            cx.notify();
        }));

        let card = widgets::dialog_card(
            &ui,
            "run-launcher-modal",
            600.0,
            ui.t("Run options", "运行选项"),
        )
        .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::Close)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .p(px(18.0))
                .child(
                    section(&ui, ui.t("PROJECT FOLDER", "项目目录"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .h(px(28.0))
                                        .px(px(10.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .rounded(px(6.0))
                                        .border_1()
                                        .border_color(rgb(t.border))
                                        .bg(rgb(t.code))
                                        .child(icons::icon(
                                            "icons/folder.svg",
                                            13.0,
                                            theme::h(t.accent, 0.9),
                                        ))
                                        .child(widgets::mono(&ui, self.cwd.display().to_string())),
                                )
                                .child(
                                    widgets::tabbable(
                                        &ui,
                                        widgets::button(
                                            &ui,
                                            "pick-workspace-btn",
                                            ui.t("Choose…", "选择…"),
                                        ),
                                    )
                                    .on_click(cx.listener(|l, _, _, cx| l.prompt_folder(cx))),
                                ),
                        )
                        .when(
                            self.recent_workspaces.iter().any(|d| *d != self.cwd),
                            |el| el.child(recent),
                        ),
                )
                .child(
                    section(&ui, ui.t("TEST COMMAND", "测试命令"))
                        .child(self.command_input.clone())
                        .when(!self.presets.is_empty(), |el| el.child(presets)),
                )
                .child(
                    section(&ui, ui.t("WHILE IT RUNS", "运行时"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    widgets::checkbox(
                                        &ui,
                                        "opt-enforce",
                                        ui.t(
                                            "Block calls outside the lock",
                                            "拦截 lock 之外的调用",
                                        ),
                                        self.enforce,
                                    )
                                    .on_click(cx.listener(
                                        |l, _, _, cx| {
                                            l.enforce = !l.enforce;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .when(pending, |el| {
                                    el.child(
                                        div()
                                            .text_size(px(text::SM))
                                            .text_color(rgb(t.amber))
                                            .child(ui.t("applies on next run", "下次运行生效")),
                                    )
                                }),
                        )
                        .child(
                            widgets::checkbox(
                                &ui,
                                "opt-docker",
                                ui.t(
                                    "Accept calls from Docker containers",
                                    "接受来自 Docker 容器的调用",
                                ),
                                self.allow_docker,
                            )
                            .on_click(cx.listener(|l, _, _, cx| {
                                l.allow_docker = !l.allow_docker;
                                cx.notify();
                            })),
                        ),
                )
                .child(
                    section(&ui, ui.t("EMULATOR", "模拟器"))
                        .child(status_row)
                        .child(advanced_toggle)
                        .when(self.advanced_open, |el| {
                            el.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .text_size(px(text::SM))
                                            .text_color(rgb(t.text_muted))
                                            .child(ui.t(
                                                "Emulator address (empty: the one above)",
                                                "模拟器地址（留空则使用上面的）",
                                            )),
                                    )
                                    .child(self.upstream_input.clone())
                                    .child(
                                        div()
                                            .text_size(px(text::SM))
                                            .text_color(rgb(t.text_muted))
                                            .child(ui.t(
                                                "Send some services elsewhere (optional)",
                                                "将部分服务发往其他地址（可选）",
                                            )),
                                    )
                                    .child(self.routes_input.clone()),
                            )
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(18.0))
                .py(px(12.0))
                .border_t_1()
                .border_color(rgb(t.border_subtle))
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::button(
                            &ui,
                            "launcher-terminal",
                            ui.t("Open Terminal", "打开终端"),
                        ),
                    )
                    .tooltip(widgets::tip(
                        ui.t(
                            "A terminal whose AWS calls go through Lenslet",
                            "打开一个 AWS 调用经过 Lenslet 的终端",
                        ),
                        None,
                    ))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::OpenTerminal))),
                )
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::button(&ui, "launcher-env", ui.t("Copy Env", "复制环境变量")),
                    )
                    .tooltip(widgets::tip(
                        ui.t(
                            "AWS_ENDPOINT_URL and local keys for your own shell",
                            "复制 AWS_ENDPOINT_URL 与本地密钥，用于你自己的终端",
                        ),
                        None,
                    ))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::CopyEnv))),
                )
                .child(div().flex_1())
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::button(&ui, "launcher-cancel", ui.t("Cancel", "取消")),
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::Close))),
                )
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::primary_button(&ui, "launcher-save", ui.t("Save", "保存")),
                    )
                    .on_click(cx.listener(|l, _, _, cx| l.save(cx))),
                ),
        );

        widgets::scrim(&ui).child(widgets::appear(&ui, "launcher-anim", card))
    }
}

pub fn render_launcher_overlay(launcher: &Entity<RunLauncher>) -> AnyElement {
    launcher.clone().into_any_element()
}

// -- About -----------------------------------------------------------------------

pub fn render_about_overlay(
    ui: &Ui,
    on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
) -> AnyElement {
    let t = ui.p;
    let close_out = on_close.clone();
    let close_btn = on_close;
    let card = div()
        .id("about-modal")
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
        .on_mouse_down_out(move |_, window, cx| close_out(window, cx))
        .child(glyph::lens_mark(&t, 52.0))
        .child(
            div()
                .mt(px(4.0))
                .text_size(px(20.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(t.text))
                .child("Lenslet"),
        )
        .child(
            div()
                .font(fonts::mono_font())
                .text_size(px(text::SM))
                .text_color(rgb(t.text_muted))
                .child(format!(
                    "{} {} ({}-{})",
                    ui.t("Version", "版本"),
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::ARCH,
                    std::env::consts::OS
                )),
        )
        .child(
            div()
                .text_center()
                .text_size(px(text::MD))
                .line_height(px(18.0))
                .text_color(rgb(t.text_soft))
                .child(ui.t(
                    "See every AWS call your tests make against a local emulator, and the IAM permissions they need.",
                    "查看测试对本地模拟器发出的每个 AWS 调用，以及它们需要的 IAM 权限。",
                )),
        )
        .child(
            div()
                .w_full()
                .p(px(10.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme::h(t.green, 0.35))
                .bg(theme::h(t.green, 0.06))
                .text_center()
                .text_size(px(text::XS))
                .text_color(rgb(t.text_body))
                .child(ui.t(
                    "Local only: refuses *.amazonaws.com and public addresses. No telemetry.",
                    "仅限本地：拒绝 *.amazonaws.com 与公网地址。无遥测。",
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .pt(px(6.0))
                .child(
                    widgets::tabbable(ui, widgets::button(ui, "about-github", "GitHub"))
                        .on_click(|_, _, cx| cx.open_url("https://github.com/hxddh/lenslet")),
                )
                .child(
                    widgets::tabbable(
                        ui,
                        widgets::button(ui, "about-issues", ui.t("Report Issue", "反馈问题")),
                    )
                    .on_click(|_, _, cx| cx.open_url("https://github.com/hxddh/lenslet/issues")),
                )
                .child(
                    widgets::tabbable(ui, widgets::primary_button(ui, "about-close", ui.t("OK", "好")))
                        .px(px(16.0))
                        .on_click(move |_, window, cx| close_btn(window, cx)),
                ),
        );
    widgets::scrim(ui)
        .child(widgets::appear(ui, "about-anim", card))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforce_change_is_flagged_only_against_a_different_active_setting() {
        assert!(!enforce_applies_next_run(None, true));
        assert!(!enforce_applies_next_run(Some(true), true));
        assert!(enforce_applies_next_run(Some(false), true));
        assert!(enforce_applies_next_run(Some(true), false));
    }
}
