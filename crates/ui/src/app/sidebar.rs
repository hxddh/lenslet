//! The sidebar (⌘0, collapsed by default): the project folder with a switch
//! button, and the recorded runs grouped by project. The grouping is computed
//! with the run list in the background; render only walks it.

use gpui::{AnyElement, Context, ElementId, FontWeight, MouseButton, div, prelude::*, px, rgb};

use super::{Cmd, LensletApp, RunNav, widgets};
use crate::{
    fonts, glyph, icons,
    theme::{self, Ui, text},
    tr,
};

pub const SIDEBAR_W: f32 = 252.0;
/// Runs listed before "N more…".
pub const RUNS_PAGE: usize = 60;

impl LensletApp {
    pub(super) fn render_sidebar(&self, ui: &Ui, cx: &mut Context<Self>) -> AnyElement {
        let t = ui.p;
        let session = self.session.read(cx);

        let ws_path = self.active_workspace_dir(cx);
        let ws_name = ws_path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("project")
            .to_string();
        let project = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .p(px(10.0))
            .mx(px(8.0))
            .mt(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.panel_raised))
            .child(icons::icon(
                "icons/folder.svg",
                13.0,
                theme::h(t.accent, 0.95),
            ))
            .child(
                div()
                    .id("sb-ws-name")
                    .flex_1()
                    .min_w(px(0.0))
                    .font(fonts::mono_font())
                    .text_size(px(text::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(t.text))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .tooltip(widgets::tip(ws_path.display().to_string(), None))
                    .child(ws_name),
            )
            .child(
                widgets::button(ui, "sb-pick-ws", ui.t("Switch…", "切换…"))
                    .px(px(6.0))
                    .tooltip(widgets::tip(
                        ui.t("Open a project folder", "打开项目目录"),
                        None,
                    ))
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.run_cmd(Cmd::OpenWorkspace, window, cx)
                    })),
            );

        let mut runs_list = div()
            .id("sidebar-runs")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(6.0))
            .pb(px(10.0))
            .gap(px(2.0));

        if session.source.runs_dir().is_some() {
            let following = session.source.follows_latest();
            runs_list = runs_list.child(
                div()
                    .id("sb-follow-latest")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(26.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .when(following, |el| el.bg(theme::h(t.accent, 0.14)))
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme::h(t.text, 0.05)))
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.run_cmd(Cmd::Runs(RunNav::Latest), window, cx)
                    }))
                    .child(glyph::dot(
                        theme::h(if following { t.green } else { t.text_muted }, 1.0),
                        6.0,
                    ))
                    .child(
                        div()
                            .text_size(px(text::SM))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(if following { t.text } else { t.text_soft }))
                            .child(ui.t("Follow newest run", "跟随最新运行")),
                    ),
            );
        }

        let mut shown = 0usize;
        'groups: for group in session.run_groups.iter() {
            if shown >= self.sidebar_runs_shown {
                break;
            }
            runs_list = runs_list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .px(px(6.0))
                    .child(icons::icon(
                        "icons/folder.svg",
                        12.0,
                        theme::h(t.text_muted, 0.9),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(text::XS))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(t.text_muted))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(group.project.clone()),
                    ),
            );
            for &index in &group.runs {
                if shown >= self.sidebar_runs_shown {
                    break 'groups;
                }
                let Some(run) = session.runs.get(index) else {
                    continue;
                };
                shown += 1;
                let current = session.path.as_ref() == Some(&run.path);
                let dot_color = match run.report.as_ref() {
                    Some(report) => match report.exit_code {
                        Some(0) => t.green,
                        Some(_) => t.red,
                        None => t.text_muted,
                    },
                    None => t.amber,
                };
                let calls = run
                    .report
                    .as_ref()
                    .map(|r| r.calls.to_string())
                    .unwrap_or_else(|| "…".into());
                let path = run.path.clone();
                let run_id = run.id.clone();
                let pinned = run.pinned;
                let session_ent = self.session.clone();
                runs_list = runs_list.child(
                    div()
                        .id(ElementId::named_usize("sb-run", index))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(28.0))
                        .flex_shrink_0()
                        .px(px(8.0))
                        .rounded(px(5.0))
                        .when(current, |el| el.bg(theme::h(t.accent, 0.15)))
                        .cursor_pointer()
                        .hover(move |s| s.bg(theme::h(t.text, 0.06)))
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.run_cmd(Cmd::Runs(RunNav::Open(path.clone())), window, cx)
                        }))
                        .child(glyph::dot(theme::h(dot_color, 1.0), 6.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .font(fonts::mono_font())
                                .text_size(px(text::SM))
                                .text_color(rgb(if current { t.text } else { t.text_soft }))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(run.label()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .font(fonts::mono_font())
                                .text_size(px(text::XS))
                                .text_color(rgb(t.text_faint))
                                .child(calls),
                        )
                        .child(
                            div()
                                .id(ElementId::named_usize("sb-pin", index))
                                .flex_shrink_0()
                                .size(px(18.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .text_size(px(11.0))
                                .text_color(if pinned {
                                    theme::h(t.amber, 1.0)
                                } else {
                                    theme::h(t.text_faint, 0.65)
                                })
                                .hover(move |s| {
                                    s.bg(theme::h(t.text, 0.08)).text_color(rgb(t.amber))
                                })
                                .tooltip(widgets::tip(
                                    if pinned {
                                        ui.t("Unpin run", "取消置顶保留")
                                    } else {
                                        ui.t("Pin run (keep it)", "置顶保留（不被自动清理）")
                                    },
                                    None,
                                ))
                                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                    cx.stop_propagation();
                                    let id = run_id.clone();
                                    session_ent.update(cx, |s, cx| {
                                        s.toggle_pin_run(id, !pinned, cx);
                                    });
                                })
                                .child(if pinned { "★" } else { "☆" }),
                        ),
                );
            }
        }
        let hidden = session.runs.len().saturating_sub(shown);
        if hidden > 0 {
            runs_list = runs_list.child(
                div()
                    .id("sb-more-runs")
                    .h(px(26.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme::h(t.text, 0.05)))
                    .text_size(px(text::SM))
                    .text_color(rgb(t.accent))
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.sidebar_runs_shown += RUNS_PAGE;
                        cx.notify();
                    }))
                    .child(
                        ui.tr(&tr!("{hidden} more…", "还有 {hidden} 个…"))
                            .to_string(),
                    ),
            );
        }

        div()
            .id("workspace-sidebar")
            .w(px(SIDEBAR_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(t.toolbar))
            .border_r_1()
            .border_color(rgb(t.border))
            .child(project)
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .child(widgets::label(ui, ui.t("RUNS", "运行"))),
            )
            .child(runs_list)
            .into_any_element()
    }
}
