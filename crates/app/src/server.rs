//! The endpoint your code talks to: every request goes to the embedded
//! emulator and comes back unchanged, and on the way Lenslet records the call
//! with the facts lenslet-core derives from it.

use std::{convert::Infallible, net::SocketAddr, time::Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use lenslet_core::{
    access, cbor, problem,
    trace::{BodyTrace, HeaderTrace, Message, Trace},
    wire,
};

use crate::{
    emulator::{Emulator, SERVICES},
    resources::Refresher,
    session::{Call, Exchange, Failure, Session, now_ms},
};

/// The most body text a call keeps for the window.
const MAX_BODY: usize = 256 * 1024;

pub async fn serve(
    listener: tokio::net::TcpListener,
    emulator: Emulator,
    session: Session,
    refresher: Refresher,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // Out of file descriptors, say: wait instead of spinning.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        let (emulator, session, refresher) = (emulator.clone(), session.clone(), refresher.clone());
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                handle(
                    request,
                    emulator.clone(),
                    session.clone(),
                    refresher.clone(),
                )
            });
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

pub fn bind(port: u16) -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn handle(
    request: Request<hyper::body::Incoming>,
    emulator: Emulator,
    session: Session,
    refresher: Refresher,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // A browser or a readiness check, not an AWS SDK: say what this is.
    if request.method() == http::Method::GET
        && request.uri().path() == "/"
        && request.uri().query().is_none()
        && !request.headers().contains_key(http::header::AUTHORIZATION)
        && !request
            .headers()
            .keys()
            .any(|k| k.as_str().starts_with("x-amz"))
        && request
            .headers()
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|ua| ua.is_empty() || ua.starts_with("Mozilla") || ua.starts_with("curl"))
    {
        let text = "Lenslet: a local AWS endpoint. Set AWS_ENDPOINT_URL to this address.\n";
        return Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(text.as_bytes())))
            .expect("static response"));
    }
    let started = Instant::now();
    let at_ms = now_ms();
    let (parts, body) = request.into_parts();
    let body = match body.collect().await {
        Ok(body) => body.to_bytes(),
        // The client went away mid-request: nothing was sent to AWS.
        Err(_) => {
            return Ok(Response::builder()
                .status(400)
                .body(Full::new(Bytes::new()))
                .expect("static response"));
        }
    };
    // The calls this one causes inside the emulator hang under its id: on its
    // task, and through the tracing header AWS carries onto its deliveries.
    let id = crate::observe::next_id();
    let mut sent = parts.clone();
    if !sent.headers.contains_key("x-amzn-trace-id")
        && let Ok(value) = http::HeaderValue::from_str(&crate::observe::trace_header(id))
    {
        sent.headers.insert("x-amzn-trace-id", value);
    }
    crate::observe::chain_started(&sent.headers, id);
    let request = http::Request::from_parts(sent, body.clone());
    // The account this call goes to; a new account started meanwhile does not
    // get it in its log.
    let generation = session.generation();
    let response = emulator.handle_traced(request, id).await;
    let (rparts, rbody) = response.into_parts();
    let mut call = record(
        Sent {
            method: &parts.method,
            uri: &parts.uri,
            headers: &parts.headers,
        },
        &body,
        Answered {
            status: rparts.status,
            headers: &rparts.headers,
        },
        &rbody,
        at_ms,
        started.elapsed().as_millis() as u64,
    );
    call.id = id;
    if changes_resources(&call) {
        refresher.poke();
        if !call.unsupported && call.status < 400 {
            session.keep(
                crate::session::Sent {
                    at_ms,
                    method: parts.method.clone(),
                    uri: parts.uri.clone(),
                    headers: parts.headers.clone(),
                    body: body.clone(),
                },
                generation,
            );
        }
    }
    session.record(call);
    Ok(Response::from_parts(rparts, Full::new(rbody)))
}

/// What a request was: its method, address and headers.
pub struct Sent<'a> {
    pub method: &'a http::Method,
    pub uri: &'a http::Uri,
    pub headers: &'a http::HeaderMap,
}

/// What came back: its status and headers.
pub struct Answered<'a> {
    pub status: http::StatusCode,
    pub headers: &'a http::HeaderMap,
}

/// The call as lenslet-core sees it, and the facts it derives.
pub fn record(
    request: Sent<'_>,
    body: &Bytes,
    response: Answered<'_>,
    rbody: &Bytes,
    at_ms: u64,
    ms: u64,
) -> Call {
    let path = request
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let info = wire::wire_info(&request.headers, &path);
    let content_type = header(&request.headers, "content-type");
    // Query-protocol services (SNS, IAM, STS) name the action in the body.
    let operation = info
        .operation
        .clone()
        .or_else(|| wire::form_action(body))
        .unwrap_or_else(|| {
            wire::fallback_operation(
                &info.service,
                request.method.as_str(),
                &path,
                &request.headers,
            )
        });
    let status = response.status.as_u16();
    let request_text = text_of(body, content_type.as_deref());
    let response_type = header(&response.headers, "content-type");
    let response_text = text_of(rbody, response_type.as_deref());

    let mut trace = Trace {
        method: request.method.to_string(),
        path: path.clone(),
        host: header(&request.headers, "host").unwrap_or_default(),
        service: info.service.clone(),
        operation: operation.clone(),
        status,
        region: info.region.clone(),
        request: Message {
            headers: lenslet_core::trace::headers(&request.headers),
            content_type: content_type.clone(),
            body: body_trace(body, &request_text),
        },
        response: Message {
            headers: lenslet_core::trace::headers(&response.headers),
            content_type: response_type.clone(),
            body: body_trace(rbody, &response_text),
        },
        ..Default::default()
    };
    let (own, implied) = access::derive(&trace);
    trace.problem = problem::from_response(status, &trace.response.headers, Some(&response_text));
    let target = lenslet_core::target::summary(&trace);
    let failure = trace.problem.as_ref().map(|p| Failure {
        code: p.code.clone(),
        message: p.message.clone(),
        hint: p.hint.clone(),
    });
    let unsupported = status == 501
        || failure.as_ref().is_some_and(|f| f.code == "UnknownService")
        || (info.service != "aws" && !SERVICES.contains(&info.service.as_str()) && status >= 400);
    Call {
        seq: 0,
        id: 0,
        parent: None,
        internal: false,
        run: None,
        generation: 0,
        at_ms,
        service: info.service,
        operation,
        method: request.method.to_string(),
        path,
        status,
        ms,
        region: info.region.clone(),
        target,
        failure,
        unsupported,
        client: header(&request.headers, "user-agent")
            .and_then(|ua| crate::session::client_of(&ua)),
        attempt: info.attempt,
        max_attempts: info.max_attempts,
        invocation: info.invocation_id.clone(),
        key: info.access_key_id.clone(),
        request_id: ["x-amzn-requestid", "x-amz-request-id", "x-amz-id-2"]
            .iter()
            .find_map(|name| header(&response.headers, name)),
        access: own.into_iter().chain(implied).collect(),
        request: exchange(
            &trace.request.headers,
            content_type,
            request_text,
            body.len(),
        ),
        response: exchange(
            &trace.response.headers,
            response_type,
            response_text,
            rbody.len(),
        ),
    }
}

fn header(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The body as text: CBOR decoded to JSON, UTF-8 as is, binary as a note.
fn text_of(body: &Bytes, content_type: Option<&str>) -> String {
    if body.is_empty() {
        return String::new();
    }
    if content_type.is_some_and(|t| t.contains("cbor"))
        && let Some(json) = cbor::decode_cbor_json(body)
    {
        return json;
    }
    match std::str::from_utf8(body) {
        Ok(text) => text.to_string(),
        Err(_) => format!("({} bytes of binary data)", body.len()),
    }
}

fn body_trace(body: &Bytes, text: &str) -> Option<BodyTrace> {
    (!body.is_empty()).then(|| BodyTrace {
        text: std::str::from_utf8(body)
            .is_ok()
            .then(|| text.to_string())
            .or_else(|| {
                // CBOR decoded to JSON still names the resources.
                text.starts_with('{').then(|| text.to_string())
            }),
        bytes: body.len() as u64,
        binary: std::str::from_utf8(body).is_err(),
        ..Default::default()
    })
}

fn exchange(
    headers: &[HeaderTrace],
    content_type: Option<String>,
    mut body: String,
    bytes: usize,
) -> Exchange {
    if body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n…");
    }
    Exchange {
        headers: headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect(),
        content_type,
        body,
        bytes,
    }
}

/// Whether a call may have created, changed or removed a resource (anything
/// but a read that succeeded or failed).
pub fn changes_resources(call: &Call) -> bool {
    let op = call.operation.as_str();
    // An operation Lenslet could not name is its HTTP method.
    let raw_read = matches!(op, "GET" | "HEAD" | "OPTIONS");
    let read = raw_read
        || [
            "Get",
            "List",
            "Describe",
            "Head",
            "Query",
            "Scan",
            "BatchGet",
            "Select",
            "Search",
            "Lookup",
            "Filter",
            "Receive",
            "Decrypt",
            "Encrypt",
            "ReEncrypt",
            "GenerateDataKey",
            "GenerateRandom",
            "GenerateMac",
            "VerifyMac",
            "Sign",
            "Verify",
            "Simulate",
            "Test",
            "Validate",
            "Preview",
            "Estimate",
            "Check",
        ]
        .iter()
        .any(|prefix| {
            // A whole word: "Tag…" and "Test…" are not the same.
            op.strip_prefix(prefix).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase())
            })
        });
    !read && call.status < 400
}
