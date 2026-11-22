//! The Output drawer at the bottom of Calls (⌘J): the run's facts on one line
//! and its stdout/stderr.
//!
//! The highlighted log is built on the background executor, only while the
//! drawer is open, and at most once per [`BUILD_EVERY`]; a closed drawer costs
//! nothing however fast the run writes.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, Window,
    div, prelude::*, px, rgb,
};

use super::{
    bodyview::{BodyView, BodyViewEvent},
    session::{Health, Session, SessionEvent},
    widgets,
};
use crate::{
    body::BodyDoc,
    i18n::{self, Tr},
    model::{format_bytes, format_latency},
    syntax::SyntaxKind,
    theme::{self, Ui, text},
};

/// The log is re-highlighted at most this often.
pub const BUILD_EVERY: Duration = Duration::from_millis(400);

pub enum OutputEvent {
    RevealTrace,
    Toast(Tr),
}

/// When the next build may start, given when the last one did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    pub fn delay(&self, now: Instant, every: Duration) -> Duration {
        match self.last {
            Some(last) => every.saturating_sub(now.saturating_duration_since(last)),
            None => Duration::ZERO,
        }
    }

    pub fn mark(&mut self, at: Instant) {
        self.last = Some(at);
    }
}

pub struct OutputView {
    session: Entity<Session>,
    focus: FocusHandle,
    log_view: Entity<BodyView>,
    tokens: Vec<String>,
    open: bool,
    /// The log text the shown document was built from.
    built_from: Option<Arc<str>>,
    building: bool,
    dirty: bool,
    throttle: Throttle,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<OutputEvent> for OutputView {}

impl Focusable for OutputView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl OutputView {
    pub fn new(session: Entity<Session>, open: bool, cx: &mut Context<Self>) -> Self {
        let log_view = cx.new(|cx| BodyView::new("run-output-log", cx));
        let sub_body = cx.subscribe(&log_view, |_, _, event, cx| match event {
            BodyViewEvent::Copied(bytes) => {
                cx.emit(OutputEvent::Toast(i18n::copied(&format_bytes(*bytes))));
            }
        });
        let sub_session = cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::Log | SessionEvent::RunChanged => this.schedule_build(false, cx),
            SessionEvent::Derived if this.open => cx.notify(),
            _ => {}
        });
        let mut view = Self {
            session,
            focus: cx.focus_handle(),
            log_view,
            tokens: Vec::new(),
            open,
            built_from: None,
            building: false,
            dirty: false,
            throttle: Throttle::default(),
            _subscriptions: vec![sub_body, sub_session],
        };
        view.schedule_build(true, cx);
        view
    }

    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.open != open {
            self.open = open;
            if open {
                self.schedule_build(false, cx);
            }
            cx.notify();
        }
    }

    pub fn set_tokens(&mut self, tokens: Vec<String>, cx: &mut Context<Self>) {
        if self.tokens != tokens {
            self.tokens = tokens;
            self.schedule_build(true, cx);
        }
    }

    pub fn restyle(&mut self, cx: &mut Context<Self>) {
        self.schedule_build(true, cx);
    }

    /// Rebuild the highlighted log off the main thread, throttled, and only
    /// while the drawer is open (a closed drawer just remembers it is stale).
    fn schedule_build(&mut self, force: bool, cx: &mut Context<Self>) {
        if force {
            self.built_from = None;
        }
        if !self.open || self.building {
            self.dirty = true;
            return;
        }
        let log = self.session.read(cx).console_log.clone();
        if self
            .built_from
            .as_ref()
            .is_some_and(|built| Arc::ptr_eq(built, &log))
        {
            self.dirty = false;
            return;
        }
        self.building = true;
        self.dirty = false;
        let wait = self.throttle.delay(Instant::now(), BUILD_EVERY);
        let tokens = self.tokens.clone();
        let palette = Ui::of(cx).p;
        cx.spawn(async move |this, cx| {
            if !wait.is_zero() {
                cx.background_executor().timer(wait).await;
            }
            let text = log.to_string();
            let doc = cx
                .background_executor()
                .spawn(async move {
                    Arc::new(BodyDoc::from_text(
                        text,
                        SyntaxKind::Plain,
                        &tokens,
                        &palette,
                    ))
                })
                .await;
            this.update(cx, |view, cx| {
                view.building = false;
                view.throttle.mark(Instant::now());
                view.built_from = Some(log);
                view.log_view.update(cx, |b, cx| b.set_doc(doc, cx));
                if view.dirty {
                    view.schedule_build(false, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for OutputView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let session = self.session.read(cx);

        let mut facts: Vec<String> = Vec::new();
        if let Some(run) = session.run.as_ref() {
            let h = &run.header;
            match run.report.as_ref() {
                Some(rep) => {
                    facts.push(match rep.exit_code {
                        Some(code) => format!("exit {code}"),
                        None => ui.t("ended", "已结束").into(),
                    });
                    facts.push(format_latency(rep.duration_ms));
                }
                None => facts.push(ui.t("running", "运行中").into()),
            }
            if let Some(emulator) = h.emulator.as_deref() {
                facts.push(emulator.to_string());
            } else if !h.upstream.is_empty() {
                facts.push(h.upstream.clone());
            }
            if h.enforce {
                facts.push(
                    ui.t("blocking calls outside the lock", "拦截 lock 之外的调用")
                        .into(),
                );
            }
            if h.allow_docker {
                facts.push(ui.t("Docker allowed", "允许 Docker").into());
            }
            let stats = session.store.stats();
            facts.push(i18n::calls_count(stats.total, ui.lang));
        }

        let log_view = self.log_view.clone();
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(theme::SUBBAR_H))
            .px(px(theme::ROW_PAD_X))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.toolbar))
            .child(widgets::label(&ui, ui.t("OUTPUT", "输出")))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(text::SM))
                    .text_color(rgb(t.text_muted))
                    .child(facts.join(" · ")),
            );
        if session.path.is_some() {
            bar = bar.child(
                widgets::button(&ui, "output-reveal", ui.t("Show run file", "显示运行文件"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(OutputEvent::RevealTrace))),
            );
        }
        bar = bar.child(
            widgets::button(&ui, "output-copy", ui.t("Copy", "复制"))
                .on_click(move |_, _, cx| log_view.update(cx, |b, cx| b.copy(cx))),
        );

        let empty = session.health == Health::Missing || session.console_log.is_empty();
        let placeholder = ui.t(
            "No output for this run. Runs started with ▶ show their stdout and stderr here.",
            "此运行没有输出。通过 ▶ 启动的运行会在这里显示 stdout 与 stderr。",
        );
        div()
            .id("output-drawer")
            .key_context("Output")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .bg(rgb(t.panel))
            .child(bar)
            .child(if empty {
                div()
                    .flex_1()
                    .p(px(14.0))
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(placeholder)
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .p(px(6.0))
                    .child(super::cached(&self.log_view))
                    .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_spaces_builds_out() {
        let every = Duration::from_millis(400);
        let mut throttle = Throttle::default();
        let start = Instant::now();
        assert_eq!(throttle.delay(start, every), Duration::ZERO);
        throttle.mark(start);
        assert_eq!(throttle.delay(start, every), every);
        assert_eq!(
            throttle.delay(start + Duration::from_millis(150), every),
            Duration::from_millis(250)
        );
        assert_eq!(
            throttle.delay(start + Duration::from_secs(2), every),
            Duration::ZERO
        );
    }
}
