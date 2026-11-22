//! The call inspector, one column read top to bottom: what the call was,
//! **why it failed**, the permissions it **needs** (and whether lenslet.lock
//! has them), its bodies (Decoded / Raw, Copy, Save…), then its headers.

use std::sync::Arc;

use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Render, Stateful, Task, Window, div, prelude::*, px, rgb,
};

use super::{
    bodyview::{BodyView, BodyViewEvent},
    session::{Session, SessionEvent},
    widgets,
};
use crate::{
    body::{self, BodyDoc, BodyMode, DocCache, DocKey},
    fonts, glyph,
    i18n::{self, Tr},
    model::{
        Access, Body, Header, Message, Trace, body_size, format_bytes, format_latency, full_time,
        operation_label, status_label, status_text, wire_bytes,
    },
    theme::{self, Ui, text},
    tr,
};

/// Bodies up to this size are built synchronously (no loading flash).
const SYNC_BUILD_BYTES: usize = 48 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Request,
    Response,
}

pub enum InspectorEvent {
    /// Show this permission in the Access view.
    OpenAccess(Access),
    Toast(Tr),
}

pub struct Inspector {
    session: Entity<Session>,
    call: Option<Arc<Trace>>,
    full: Option<Arc<Trace>>,
    hydrating: bool,
    generation: u64,
    side: Side,
    mode: BodyMode,
    headers_open: bool,
    tokens: Vec<String>,
    body: Entity<BodyView>,
    docs: DocCache,
    focus: FocusHandle,
    _build: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<InspectorEvent> for Inspector {}

impl Focusable for Inspector {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Inspector {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let body = cx.new(|cx| BodyView::new("body", cx));
        let subscriptions = vec![
            cx.subscribe(&body, |_, _, event, cx| match event {
                BodyViewEvent::Copied(bytes) => {
                    cx.emit(InspectorEvent::Toast(i18n::copied(&format_bytes(*bytes))));
                }
            }),
            // In-lock badges follow lock edits.
            cx.subscribe(&session, |_, _, event, cx| {
                if matches!(event, SessionEvent::Derived) {
                    cx.notify();
                }
            }),
        ];
        Inspector {
            session,
            call: None,
            full: None,
            hydrating: false,
            generation: 0,
            side: Side::Response,
            mode: BodyMode::Decoded,
            headers_open: false,
            tokens: Vec::new(),
            body,
            docs: DocCache::default(),
            focus: cx.focus_handle(),
            _build: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn body(&self) -> &Entity<BodyView> {
        &self.body
    }

    pub fn call(&self) -> Option<&Arc<Trace>> {
        self.full.as_ref().or(self.call.as_ref())
    }

    /// Show `call`. Reads shed bodies back from the run file in the background.
    pub fn set_call(&mut self, call: Option<Arc<Trace>>, cx: &mut Context<Self>) {
        if self.call.as_ref().map(|c| &c.id) == call.as_ref().map(|c| &c.id) {
            return;
        }
        self.call = call.clone();
        self.generation += 1;
        self.side = default_side(call.as_deref());
        match call {
            Some(call) if call.is_dehydrated() => {
                self.full = None;
                self.hydrating = true;
                self.body.update(cx, |body, cx| body.set_loading(cx));
                let generation = self.generation;
                self._build = Some(cx.spawn(async move |this, cx| {
                    let full = cx
                        .background_executor()
                        .spawn(async move { call.hydrated().map(Arc::new).unwrap_or(call) })
                        .await;
                    this.update(cx, |inspector, cx| {
                        if inspector.generation == generation {
                            inspector.hydrating = false;
                            inspector.full = Some(full);
                            inspector.rebuild(cx);
                        }
                    })
                    .ok();
                }));
            }
            other => {
                self.hydrating = false;
                self.full = other;
                self.rebuild(cx);
            }
        }
        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Vec<String>, cx: &mut Context<Self>) {
        if self.tokens != tokens {
            self.tokens = tokens;
            self.rebuild(cx);
        }
    }

    /// The look changed: cached highlights are stale.
    pub fn restyle(&mut self, cx: &mut Context<Self>) {
        self.docs.clear();
        self.rebuild(cx);
    }

    fn set_side(&mut self, side: Side, cx: &mut Context<Self>) {
        if self.side != side {
            self.side = side;
            self.rebuild(cx);
            cx.notify();
        }
    }

    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            BodyMode::Decoded => BodyMode::Raw,
            BodyMode::Raw => BodyMode::Decoded,
        };
        self.rebuild(cx);
        cx.notify();
    }

    fn message<'a>(&self, call: &'a Trace) -> &'a Message {
        match self.side {
            Side::Request => &call.request,
            Side::Response => &call.response,
        }
    }

    /// Build (or fetch from cache) the document the body view shows.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let ui = Ui::of(cx);
        let Some(call) = self.full.clone() else {
            if !self.hydrating {
                self.body.update(cx, |body, cx| {
                    body.set_doc(Arc::new(BodyDoc::default()), cx);
                    body.set_placeholder("", cx);
                });
            }
            return;
        };
        let placeholder = match self.side {
            Side::Request => ui.t("No request body", "没有请求体"),
            Side::Response => ui.t("No response body", "没有响应体"),
        };
        let key = DocKey {
            call: call.id.clone(),
            side: self.side as u8,
            mode: self.mode,
            query: self.tokens.join(" "),
            look: ui.dark(),
        };
        self.body
            .update(cx, |body, cx| body.set_placeholder(placeholder, cx));
        if let Some(doc) = self.docs.get(&key) {
            self.body.update(cx, |body, cx| body.set_doc(doc, cx));
            return;
        }
        let message = self.message(&call);
        let source: Option<Body> = message.body.clone();
        let content_type = message
            .raw_content_type
            .clone()
            .or_else(|| message.content_type.clone())
            .unwrap_or_default();
        let tokens = self.tokens.clone();
        let mode = self.mode;
        let palette = ui.p;
        let size = source
            .as_ref()
            .map_or(0, |b| b.text.as_ref().map_or(0, String::len));
        if size <= SYNC_BUILD_BYTES {
            let doc = Arc::new(body::build(
                source.as_ref(),
                &content_type,
                mode,
                &tokens,
                &palette,
            ));
            self.docs.insert(key, doc.clone());
            self.body.update(cx, |body, cx| body.set_doc(doc, cx));
            return;
        }
        self.body.update(cx, |body, cx| body.set_loading(cx));
        let generation = self.generation;
        self._build = Some(cx.spawn(async move |this, cx| {
            let doc = cx
                .background_executor()
                .spawn(async move {
                    Arc::new(body::build(
                        source.as_ref(),
                        &content_type,
                        mode,
                        &tokens,
                        &palette,
                    ))
                })
                .await;
            this.update(cx, |inspector, cx| {
                inspector.docs.insert(key.clone(), doc.clone());
                if inspector.generation == generation {
                    inspector.body.update(cx, |body, cx| body.set_doc(doc, cx));
                }
            })
            .ok();
        }));
    }

    /// Save the shown body: the exact bytes when the run file has them,
    /// else the text on screen.
    pub fn save_body(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.full.clone() else {
            return;
        };
        let dir = self
            .session
            .read(cx)
            .project_dir()
            .unwrap_or_else(super::home);
        let side = self.side;
        let mode = self.mode;
        let shown = self.body.read(cx).doc().text.to_string();
        let suggested = match side {
            Side::Request => format!("{}-request.txt", call.id),
            Side::Response => format!("{}-response.txt", call.id),
        };
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dest))) = rx.await else {
                return;
            };
            let target = dest.clone();
            let written = cx
                .background_executor()
                .spawn(async move {
                    crate::fsguard::assert_io_allowed();
                    let message = match side {
                        Side::Request => &call.request,
                        Side::Response => &call.response,
                    };
                    let bytes = message
                        .body
                        .as_ref()
                        .filter(|_| mode == BodyMode::Raw)
                        .and_then(wire_bytes)
                        .unwrap_or_else(|| shown.into_bytes());
                    let len = bytes.len();
                    std::fs::write(&target, bytes).map(|()| len)
                })
                .await;
            this.update(cx, |_, cx| {
                let path = dest.display();
                let message = match written {
                    Ok(n) => {
                        let size = format_bytes(n);
                        tr!("Saved {path} ({size})", "已保存 {path}（{size}）")
                    }
                    Err(e) => tr!("Could not save: {e}", "保存失败：{e}"),
                };
                cx.emit(InspectorEvent::Toast(message));
            })
            .ok();
        })
        .detach();
    }

    // -- Render ------------------------------------------------------------------

    fn render_header(&self, ui: &Ui, call: &Trace) -> Div {
        let t = ui.p;
        let color = ui.service_color(call.service_name());
        let status = ui.status_color(call.status);
        let top = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(20.0))
                    .px(px(6.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme::tint(color, 0.35))
                    .child(glyph::dot(color, 6.0))
                    .child(
                        div()
                            .text_size(px(text::SM))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(call.service_name().to_string()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(text::XL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(t.text))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(operation_label(call).to_string()),
            );
        let target = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_shrink_0()
                    .font(fonts::mono_font())
                    .text_size(px(text::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(t.accent))
                    .child(call.method.to_ascii_uppercase()),
            )
            .child(widgets::mono(ui, call.semantic_target()));
        let meta = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .font(fonts::mono_font())
            .text_size(px(text::SM))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph::dot(status, 6.0))
                    .child(div().text_color(status).child(format!(
                        "{} {}",
                        status_label(call.status),
                        status_text(call.status)
                    ))),
            )
            .child(
                div()
                    .text_color(ui.latency_color(call.ms))
                    .child(format_latency(call.ms)),
            )
            .child(
                div()
                    .text_color(rgb(t.text_faint))
                    .child(full_time(&call.at)),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(16.0))
            .pt(px(12.0))
            .pb(px(10.0))
            .border_b_1()
            .border_color(rgb(t.border_subtle))
            .child(top)
            .child(target)
            .child(meta)
    }

    fn render_why(&self, ui: &Ui, call: &Trace) -> Option<Div> {
        let t = ui.p;
        if call.problem.is_none() && call.error.is_none() {
            return None;
        }
        let mut panel = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme::h(t.red, 0.35))
            .bg(theme::h(t.red, 0.06))
            .child(widgets::label(ui, ui.t("WHY IT FAILED", "失败原因")));
        if let Some(problem) = call.problem.as_ref() {
            panel = panel.child(
                div()
                    .font(fonts::mono_font())
                    .text_size(px(text::LG))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(t.red))
                    .child(problem.code.clone()),
            );
            if let Some(message) = problem.message.as_deref().filter(|m| !m.is_empty()) {
                panel = panel.child(
                    div()
                        .text_size(px(text::MD))
                        .line_height(px(17.0))
                        .text_color(rgb(t.text_soft))
                        .child(message.to_string()),
                );
            }
            if let Some(hint) = problem.hint.as_deref() {
                panel = panel.child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .text_size(px(text::MD))
                        .line_height(px(17.0))
                        .child(div().text_color(rgb(t.accent)).child("→"))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_color(rgb(t.text_body))
                                .child(i18n::hint(hint, ui.lang).to_string()),
                        ),
                );
            }
        }
        if let Some(error) = call.error.as_deref() {
            panel = panel.child(widgets::mono(ui, error.to_string()));
        }
        Some(panel)
    }

    fn render_needs(&self, ui: &Ui, call: &Trace, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let lock = self.session.read(cx).lock.clone();
        let mut list = div().flex().flex_col();
        let entries: Vec<(Access, bool, bool)> = call.access_entries().collect();
        if entries.is_empty() {
            list = list.child(
                div()
                    .text_size(px(text::MD))
                    .text_color(rgb(t.text_muted))
                    .child(ui.t(
                        "No IAM permission Lenslet knows of.",
                        "Lenslet 未识别到此调用需要的 IAM 权限。",
                    )),
            );
        }
        for (index, (access, implied, unverified)) in entries.into_iter().enumerate() {
            let mut name = div().flex().items_center().gap(px(6.0)).child(
                div()
                    .font(fonts::mono_font())
                    .text_size(px(text::MD))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(t.text))
                    .child(access.action.clone()),
            );
            if lock.contains(&access) {
                name = name.child(widgets::badge(ui, ui.t("IN LOCK", "已入锁"), t.green));
            } else {
                name = name.child(widgets::badge(ui, ui.t("NOT IN LOCK", "未入锁"), t.amber));
            }
            if implied {
                name = name.child(widgets::badge(ui, ui.t("IMPLIED", "隐含"), t.text_muted));
            }
            if unverified {
                name = name.child(widgets::badge(ui, ui.t("UNVERIFIED", "未核实"), t.amber));
            }
            let resource = access.resource.clone();
            list = list.child(
                div()
                    .id(ElementId::named_usize("call-access", index))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .py(px(5.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(t.row_hover)))
                    .tooltip(widgets::tip(
                        ui.t("Show in Access", "在权限视图中查看"),
                        Some(i18n::shortcut("2")),
                    ))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(InspectorEvent::OpenAccess(access.clone()));
                    }))
                    .child(name)
                    .child(widgets::mono(ui, resource)),
            );
        }
        if let Some(role) = call.caller_role.as_deref() {
            list = list.child(widgets::meta_row(
                ui,
                ui.t("Assumed role", "调用角色"),
                widgets::mono(ui, role.to_string()),
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::label(ui, ui.t("NEEDS", "所需权限")))
            .child(list)
    }

    fn render_bodies(&self, ui: &Ui, call: &Trace, cx: &mut Context<Self>) -> Div {
        let t = ui.p;
        let entity = cx.entity();
        let sides = widgets::segmented(
            ui,
            vec![
                widgets::Segment {
                    id: "side-req",
                    label: ui.t("Request", "请求").into(),
                    active: self.side == Side::Request,
                    tooltip: None,
                },
                widgets::Segment {
                    id: "side-res",
                    label: ui.t("Response", "响应").into(),
                    active: self.side == Side::Response,
                    tooltip: None,
                },
            ],
            move |index, _, cx| {
                let side = [Side::Request, Side::Response][index];
                entity.update(cx, |inspector, cx| inspector.set_side(side, cx));
            },
        );
        let doc = self.body.read(cx).doc().clone();
        let mut toolbar = div().flex().items_center().gap(px(6.0)).child(sides).child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .font(fonts::mono_font())
                .text_size(px(text::XS))
                .text_color(rgb(t.text_muted))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(body_meta(self.message(call), ui)),
        );
        if doc.decodable {
            let decoded = self.mode == BodyMode::Decoded;
            let label = match (decoded, doc.decoder) {
                (true, Some(decoder)) => format!("{} · {decoder}", ui.t("Decoded", "已解码")),
                (true, None) => ui.t("Decoded", "已解码").to_string(),
                (false, _) => ui.t("Raw", "原始").to_string(),
            };
            toolbar = toolbar.child(
                widgets::button(ui, "mode", label)
                    .tooltip(widgets::tip(
                        if decoded {
                            ui.t("Show the text as recorded", "显示记录的原始文本")
                        } else {
                            ui.t(
                                "Pretty-print and decode AWS encodings",
                                "格式化并解码 AWS 编码",
                            )
                        },
                        None,
                    ))
                    .on_click(cx.listener(|inspector, _, _, cx| inspector.toggle_mode(cx))),
            );
        }
        if !doc.is_empty() {
            let body = self.body.clone();
            toolbar = toolbar
                .child(
                    widgets::button(ui, "copy-body", ui.t("Copy", "复制"))
                        .tooltip(widgets::tip(
                            ui.t(
                                "Copy the selection, or the whole body",
                                "复制选中内容或整个报文",
                            ),
                            Some(i18n::shortcut("C")),
                        ))
                        .on_click(move |_, _, cx| body.update(cx, |body, cx| body.copy(cx))),
                )
                .child(
                    widgets::button(ui, "save-body", ui.t("Save…", "保存…"))
                        .tooltip(widgets::tip(
                            ui.t("Save the body to a file", "将报文保存到文件"),
                            None,
                        ))
                        .on_click(cx.listener(|inspector, _, _, cx| inspector.save_body(cx))),
                );
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(160.0))
            .gap(px(8.0))
            .child(toolbar)
            .child(super::cached(&self.body))
    }

    fn render_headers(&self, ui: &Ui, call: &Trace, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = ui.p;
        let count = call.request.headers.len() + call.response.headers.len();
        let open = self.headers_open;
        let mut section = div()
            .id("headers")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .border_t_1()
            .border_color(rgb(t.border_subtle))
            .child(
                div()
                    .id("headers-toggle")
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(30.0))
                    .px(px(16.0))
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(t.row_hover)))
                    .on_click(cx.listener(|inspector, _, _, cx| {
                        inspector.headers_open = !inspector.headers_open;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(text::XS))
                            .text_color(rgb(t.text_muted))
                            .child(if open { "▾" } else { "▸" }),
                    )
                    .child(widgets::label(ui, ui.t("HEADERS", "请求头与响应头")))
                    .child(
                        div()
                            .font(fonts::mono_font())
                            .text_size(px(text::XS))
                            .text_color(rgb(t.text_muted))
                            .child(count.to_string()),
                    ),
            );
        if open {
            section = section.child(
                div()
                    .id("headers-list")
                    .max_h(px(280.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .px(px(16.0))
                    .pb(px(12.0))
                    .child(self.header_table(
                        ui,
                        ui.t("Request", "请求"),
                        "req",
                        &call.request.headers,
                        cx,
                    ))
                    .child(self.header_table(
                        ui,
                        ui.t("Response", "响应"),
                        "res",
                        &call.response.headers,
                        cx,
                    )),
            );
        }
        section
    }

    fn header_table(
        &self,
        ui: &Ui,
        title: &str,
        prefix: &'static str,
        items: &[Header],
        cx: &mut Context<Self>,
    ) -> Div {
        let t = ui.p;
        let mut table = div()
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(t.border_subtle))
            .bg(rgb(t.code));
        for (ix, h) in items.iter().enumerate() {
            let line = format!("{}: {}", h.name, h.value);
            let name = h.name.clone();
            table = table.child(
                div()
                    .id(ElementId::Name(format!("{prefix}-{ix}").into()))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .min_h(px(24.0))
                    .px(px(10.0))
                    .when(ix > 0, |el| {
                        el.border_t_1().border_color(rgb(t.border_subtle))
                    })
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(t.row_hover)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(line.clone()));
                        cx.emit(InspectorEvent::Toast(i18n::copied(&name)));
                    }))
                    .child(
                        div()
                            .w(px(148.0))
                            .flex_shrink_0()
                            .font(fonts::mono_font())
                            .text_size(px(text::XS))
                            .text_color(rgb(t.accent))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(h.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .font(fonts::mono_font())
                            .text_size(px(text::XS))
                            .text_color(rgb(t.text_body))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(h.value.clone()),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(widgets::label(ui, title.to_string()))
            .child(table)
    }

    fn render_empty(&self, ui: &Ui) -> AnyElement {
        let t = ui.p;
        let key = |k: String, label: &str| {
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .min_w(px(64.0))
                        .text_right()
                        .font(fonts::mono_font())
                        .text_size(px(text::SM))
                        .text_color(rgb(t.text_dim))
                        .child(k),
                )
                .child(
                    div()
                        .text_size(px(text::SM))
                        .text_color(rgb(t.text_muted))
                        .child(label.to_string()),
                )
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(text::LG))
                    .text_color(rgb(t.text_dim))
                    .child(ui.t("Select a call", "选择一个调用")),
            )
            .child(key(
                "j / k".into(),
                ui.t("next / previous", "下一个 / 上一个"),
            ))
            .child(key("/".into(), ui.t("search", "搜索")))
            .child(key(i18n::shortcut("R"), ui.t("run the tests", "运行测试")))
            .into_any_element()
    }
}

fn body_meta(message: &Message, ui: &Ui) -> String {
    let mut parts = Vec::new();
    if let Some(body) = message.body.as_ref() {
        let size = body_size(body);
        match body.decoded_bytes.map(|d| d as usize) {
            Some(decoded) if decoded != size => {
                let (wire, decoded) = (format_bytes(size), format_bytes(decoded));
                parts.push(
                    ui.tr(&tr!(
                        "{wire} on the wire → {decoded} decoded",
                        "传输 {wire} → 解码后 {decoded}"
                    ))
                    .to_string(),
                );
            }
            _ => parts.push(format_bytes(size)),
        }
        if let Some(encoding) = body.encoding.as_deref() {
            parts.push(encoding.to_string());
        }
        if body.truncated {
            parts.push(ui.t("truncated", "已截断").into());
        }
        if body.binary {
            parts.push(ui.t("binary", "二进制").into());
        }
    }
    if let Some(ct) = message
        .raw_content_type
        .as_deref()
        .or(message.content_type.as_deref())
    {
        parts.push(ct.to_string());
    }
    parts.join(" · ")
}

fn default_side(call: Option<&Trace>) -> Side {
    let has = |message: &Message| {
        message
            .body
            .as_ref()
            .is_some_and(|body| body_size(body) > 0)
    };
    match call {
        Some(call) if !call.is_error() && !has(&call.response) && has(&call.request) => {
            Side::Request
        }
        _ => Side::Response,
    }
}

impl Render for Inspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let container = div()
            .id("inspector")
            .key_context("Inspector")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(0.0))
            .bg(rgb(t.panel_raised));
        let Some(call) = self.call().cloned() else {
            return container.child(self.render_empty(&ui));
        };
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap(px(14.0))
            .p(px(16.0))
            .children(self.render_why(&ui, &call))
            .child(self.render_needs(&ui, &call, cx))
            .child(self.render_bodies(&ui, &call, cx));
        container
            .child(self.render_header(&ui, &call))
            .child(body)
            .child(self.render_headers(&ui, &call, cx))
    }
}
