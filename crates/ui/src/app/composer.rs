//! Edit & Replay (⇧⌘⏎): change a captured call (method, path, signing
//! service/region, headers, body), re-sign it with `AWS4-HMAC-SHA256` and
//! send it again, strictly over local loopback.

use std::path::PathBuf;

use gpui::{AnyElement, Context, Entity, EventEmitter, Render, Window, div, prelude::*, px, rgb};

use super::widgets;
use crate::{
    i18n::Tr,
    model::{Header, Trace},
    replay::{self, ComposedRequest, ReplayState, ReplayTarget},
    search::SearchInput,
    theme::{self, Ui, text},
};

pub enum ComposerEvent {
    Close,
    Sent(String, ReplayState),
}

pub struct RequestComposer {
    call_id: String,
    host: String,
    encoding: Option<String>,
    base_headers: Vec<Header>,
    via_proxy: bool,
    home: PathBuf,
    resign_sigv4: bool,
    sending: bool,
    error: Option<Tr>,
    target_input: Entity<SearchInput>,
    service_input: Entity<SearchInput>,
    region_input: Entity<SearchInput>,
    method_input: Entity<SearchInput>,
    path_input: Entity<SearchInput>,
    header_input: Entity<SearchInput>,
    body_input: Entity<SearchInput>,
}

impl EventEmitter<ComposerEvent> for RequestComposer {}

impl RequestComposer {
    pub fn new(home: PathBuf, cx: &mut Context<Self>) -> Self {
        let target_input = cx.new(|cx| {
            SearchInput::new_field(
                "http://127.0.0.1:9297",
                "http://127.0.0.1:9297 (loopback only)",
                true,
                cx,
            )
        });
        let service_input =
            cx.new(|cx| SearchInput::new_field("s3", "s3 / dynamodb / sqs", true, cx));
        let region_input = cx.new(|cx| SearchInput::new_field("us-east-1", "us-east-1", true, cx));
        let method_input = cx.new(|cx| SearchInput::new_field("POST", "POST", true, cx));
        let path_input = cx.new(|cx| SearchInput::new_field("/", "/path?query=1", true, cx));
        let header_input = cx.new(|cx| {
            SearchInput::new_field(
                "",
                "x-amz-target: DynamoDB_20120810.GetItem; -x-unwanted-header",
                true,
                cx,
            )
        });
        let body_input = cx.new(|cx| SearchInput::new_field("", "{\"Key\": ...}", true, cx));
        Self {
            call_id: String::new(),
            host: String::new(),
            encoding: None,
            base_headers: Vec::new(),
            via_proxy: true,
            home,
            resign_sigv4: true,
            sending: false,
            error: None,
            target_input,
            service_input,
            region_input,
            method_input,
            path_input,
            header_input,
            body_input,
        }
    }

    pub fn load(
        &mut self,
        call: &Trace,
        target: ReplayTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.call_id = call.id.clone();
        self.error = None;
        self.host = call.host.clone();
        let service = call.service.clone();
        let region = call
            .region
            .clone()
            .unwrap_or_else(|| "us-east-1".to_string());
        self.encoding = call
            .request
            .as_ref()
            .and_then(|r| r.body.as_ref())
            .and_then(|b| b.encoding.clone());
        self.base_headers = call
            .request
            .as_ref()
            .map(|r| r.headers.clone())
            .unwrap_or_default();
        self.via_proxy = target.via_proxy;
        self.resign_sigv4 = call.authorization || !call.presigned();
        self.sending = false;

        let method = call.method.to_ascii_uppercase();
        let path = call.path.clone();
        let target_hdr = self
            .base_headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("x-amz-target"))
            .map(|h| format!("{}: {}", h.name, h.value))
            .unwrap_or_default();
        let raw_body = call
            .request
            .as_ref()
            .and_then(|r| r.body.as_ref())
            .and_then(|b| b.text.as_deref())
            .unwrap_or("");
        let compact_body = serde_json::from_str::<serde_json::Value>(raw_body)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| raw_body.replace(['\r', '\n'], " "));

        self.target_input
            .update(cx, |i, cx| i.set_text(target.url, cx));
        self.service_input
            .update(cx, |i, cx| i.set_text(service, cx));
        self.region_input.update(cx, |i, cx| i.set_text(region, cx));
        self.method_input.update(cx, |i, cx| i.set_text(method, cx));
        self.path_input.update(cx, |i, cx| i.set_text(path, cx));
        self.header_input
            .update(cx, |i, cx| i.set_text(target_hdr, cx));
        self.body_input
            .update(cx, |i, cx| i.set_text(compact_body, cx));
        let focus = self.body_input.read(cx).handle();
        window.focus(&focus);
        cx.notify();
    }

    pub fn send(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let target_url = self.target_input.read(cx).query().trim().to_string();
        if target_url.is_empty() {
            return;
        }
        let target = ReplayTarget {
            url: target_url,
            via_proxy: self.via_proxy,
        };
        let service = self.service_input.read(cx).query().trim().to_string();
        let region = self.region_input.read(cx).query().trim().to_string();
        if self.resign_sigv4
            && let Err(err) = replay::signing_scope(&service, &region)
        {
            self.error = Some(err);
            cx.notify();
            return;
        }
        self.error = None;
        let method = self.method_input.read(cx).query().trim().to_string();
        let path = {
            let p = self.path_input.read(cx).query().trim().to_string();
            if p.is_empty() {
                "/".into()
            } else if p.starts_with('/') {
                p
            } else {
                format!("/{p}")
            }
        };
        let extra_hdr = self.header_input.read(cx).query().trim().to_string();
        let body = self.body_input.read(cx).query().to_string();

        let mut headers = self.base_headers.clone();
        for part in extra_hdr.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(rem) = part.strip_prefix('-') {
                let rem = rem.trim();
                if !rem.is_empty() {
                    headers.retain(|h| !h.name.eq_ignore_ascii_case(rem));
                }
            } else if let Some((name, val)) = part.split_once(':') {
                let name = name.trim();
                let val = val.trim();
                if !name.is_empty() {
                    headers.retain(|h| !h.name.eq_ignore_ascii_case(name));
                    headers.push(Header {
                        name: name.to_string(),
                        value: val.to_string(),
                    });
                }
            }
        }

        let req = ComposedRequest {
            call_id: (!self.call_id.is_empty()).then(|| self.call_id.clone()),
            method,
            path,
            host: self.host.clone(),
            service,
            region,
            encoding: self.encoding.clone(),
            headers,
            body,
            resign_sigv4: self.resign_sigv4,
        };
        let call_id = self.call_id.clone();
        let home = self.home.clone();
        self.sending = true;
        cx.emit(ComposerEvent::Sent(call_id.clone(), ReplayState::Running));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { replay::send_composed(&req, &target, &home) })
                .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                let state = match outcome {
                    Ok(done) => ReplayState::Done(done),
                    Err(err) => ReplayState::Failed(err),
                };
                cx.emit(ComposerEvent::Sent(call_id, state));
                cx.emit(ComposerEvent::Close);
            })
            .ok();
        })
        .detach();
    }

    fn set_method(&mut self, method: &'static str, cx: &mut Context<Self>) {
        self.method_input.update(cx, |i, cx| i.set_text(method, cx));
        cx.notify();
    }
}

impl Render for RequestComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::fsguard::RenderScope::enter();
        let ui = Ui::of(cx);
        let t = ui.p;
        let cur_method = self.method_input.read(cx).query().to_ascii_uppercase();

        let mut method_chips = div().flex().items_center().gap(px(4.0));
        for m in ["POST", "GET", "PUT", "DELETE", "HEAD"] {
            method_chips = method_chips.child(
                widgets::tabbable(
                    &ui,
                    widgets::chip(&ui, m, m, None, cur_method == m, t.accent),
                )
                .on_click(cx.listener(move |comp, _, _, cx| comp.set_method(m, cx))),
            );
        }

        let card = widgets::dialog_card(
            &ui,
            "composer-modal",
            680.0,
            ui.t("Edit & Replay", "编辑并重放"),
        )
        .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Close)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .p(px(18.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().child(field_block(
                            &ui,
                            ui.t("SEND TO (LOOPBACK ONLY)", "发送到（仅本机回环）"),
                            self.target_input.clone(),
                        )))
                        .child(div().w(px(120.0)).child(field_block(
                            &ui,
                            "SERVICE",
                            self.service_input.clone(),
                        )))
                        .child(div().w(px(120.0)).child(field_block(
                            &ui,
                            "REGION",
                            self.region_input.clone(),
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(88.0)).child(self.method_input.clone()))
                        .child(method_chips),
                )
                .child(field_block(
                    &ui,
                    ui.t("PATH & QUERY", "路径与查询参数"),
                    self.path_input.clone(),
                ))
                .child(field_block(
                    &ui,
                    ui.t(
                        "HEADERS (Key: Value; Key2: Value2; -RemoveKey)",
                        "Headers（用 ; 分隔，-Header 表示移除）",
                    ),
                    self.header_input.clone(),
                ))
                .child(field_block(
                    &ui,
                    ui.t("BODY", "请求体"),
                    self.body_input.clone(),
                ))
                .when_some(self.error.as_ref(), |el, err| {
                    el.child(
                        div()
                            .p(px(8.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme::h(t.red, 0.4))
                            .bg(theme::h(t.red, 0.08))
                            .text_size(px(text::SM))
                            .text_color(rgb(t.red))
                            .child(ui.tr(err).to_string()),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(18.0))
                .py(px(12.0))
                .border_t_1()
                .border_color(rgb(t.border_subtle))
                .child(
                    widgets::checkbox(
                        &ui,
                        "toggle-sigv4",
                        ui.t("Sign with AWS SigV4", "使用 AWS SigV4 签名"),
                        self.resign_sigv4,
                    )
                    .on_click(cx.listener(|comp, _, _, cx| {
                        comp.resign_sigv4 = !comp.resign_sigv4;
                        comp.error = None;
                        cx.notify();
                    })),
                )
                .child(div().flex_1())
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::button(&ui, "close-composer", ui.t("Cancel", "取消")),
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Close))),
                )
                .child(
                    widgets::tabbable(
                        &ui,
                        widgets::primary_button(
                            &ui,
                            "composer-send",
                            if self.sending {
                                ui.t("Sending…", "发送中…")
                            } else {
                                ui.t("Send", "发送")
                            },
                        ),
                    )
                    .on_click(cx.listener(|comp, _, _, cx| comp.send(cx))),
                ),
        );

        widgets::scrim(&ui).child(widgets::appear(&ui, "composer-anim", card))
    }
}

fn field_block(ui: &Ui, title: &str, input: Entity<SearchInput>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(widgets::label(ui, title.to_string()))
        .child(input)
}

pub fn render_composer_overlay(composer: &Entity<RequestComposer>) -> AnyElement {
    composer.clone().into_any_element()
}
