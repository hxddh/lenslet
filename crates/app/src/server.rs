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

/// The largest body read as text (to find what a call names); larger ones
/// are data, as an object's.
const MAX_TEXT: usize = 32 * 1024 * 1024;

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

/// The same port on IPv6's loopback (`[::1]`, and `localhost` where it names
/// that first), when the machine has one.
pub fn bind_v6(port: u16) -> Option<std::net::TcpListener> {
    let listener =
        std::net::TcpListener::bind(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)))
            .ok()?;
    listener.set_nonblocking(true).ok()?;
    Some(listener)
}

async fn handle(
    request: Request<hyper::body::Incoming>,
    emulator: Emulator,
    session: Session,
    refresher: Refresher,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Only this machine's names: a web page whose name was made to point at
    // 127.0.0.1 (DNS rebinding) must not reach the account.
    if !loopback_host(request.headers().get(http::header::HOST)) {
        return Ok(Response::builder()
            .status(403)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                b"Lenslet serves this machine only: use 127.0.0.1 or localhost.\n",
            )))
            .expect("static response"));
    }
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
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let key = lenslet_core::wire::wire_info(&parts.headers, path).access_key_id;
    let client = header(&parts.headers, "user-agent").and_then(|ua| crate::session::client_of(&ua));
    session.arrive(id, key.as_deref(), client.as_deref());
    let request = http::Request::from_parts(sent, body.clone());
    // The account this call goes to, told as it takes the engine: an account
    // that takes its place meanwhile does not get it in its log.
    let (response, generation) = emulator
        .handle_traced(request, id, || session.generation())
        .await;
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
    let issued = crate::engine::issued_ids(&call.operation, &rbody);
    // A receive that took messages changed the queue, and the handles it got
    // are what the deletes after it name.
    let received = call.operation == "ReceiveMessage" && !issued.is_empty();
    if changes_resources(&call) || received {
        refresher.poke();
        if !call.unsupported && call.status < 400 {
            let sent = crate::session::Sent {
                at_ms,
                method: parts.method.clone(),
                uri: parts.uri.clone(),
                headers: parts.headers.clone(),
                body: body.clone(),
                issued,
                grant: None,
            };
            // A role created again gets what Lenslet allowed it before.
            if let Some(role) = sent.created_role() {
                for grant in session.grants_for(&role) {
                    emulator.handle(grant.request()).await;
                }
            }
            session.keep(sent, generation);
        }
    }
    session.record(call);
    Ok(Response::from_parts(rparts, Full::new(rbody)))
}

/// Whether a request's `Host` names this machine: 127.0.0.1, ::1,
/// localhost or a name under it (an S3 bucket's virtual host). A request
/// with no `Host` (HTTP/1.0) comes from a local client.
pub fn loopback_host(host: Option<&http::HeaderValue>) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok()) else {
        return host.is_none();
    };
    let name = match host.strip_prefix('[') {
        // [::1]:4510
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name == "::1"
        || name
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    let info = wire::wire_info(request.headers, &path);
    let content_type = header(request.headers, "content-type");
    let (_, operation) = self::operation(request.method, request.uri, request.headers, body);
    let status = response.status.as_u16();
    let request_text = text_of(request.headers, body, content_type.as_deref());
    let response_type = header(&response.headers, "content-type");
    let response_text = text_of(response.headers, rbody, response_type.as_deref());

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
    trace.problem =
        problem::from_response(status, &trace.response.headers, Some(&response_text.shown));
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
        request: exchange(&trace.request.headers, content_type, request_text, body),
        response: exchange(&trace.response.headers, response_type, response_text, rbody),
    }
}

/// The service and operation a request is for.
pub fn operation(
    method: &http::Method,
    uri: &http::Uri,
    headers: &http::HeaderMap,
    body: &[u8],
) -> (String, String) {
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let info = wire::wire_info(headers, path);
    // Query-protocol services (SNS, IAM, STS) name the action in the body,
    // which may be framed or compressed.
    let operation = info
        .operation
        .clone()
        .or_else(|| wire::form_action(&lenslet_core::body::decode(headers, body).bytes))
        .unwrap_or_else(|| wire::fallback_operation(&info.service, method.as_str(), path, headers));
    (info.service, operation)
}

fn header(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// A body as the window and lenslet-core read it.
struct Text {
    /// What the window shows: the text, or a note for data.
    shown: String,
    /// `shown` is what the body says (not a note about it).
    readable: bool,
    /// The encodings undone to read it (`aws-chunked`, `gzip`, `cbor`).
    encoding: Vec<&'static str>,
}

/// The body as text: its wire encodings undone (`aws-chunked` framing,
/// gzip), CBOR decoded to JSON, UTF-8 as is, binary as a note.
fn text_of(headers: &http::HeaderMap, body: &Bytes, content_type: Option<&str>) -> Text {
    let note = |shown: String| Text {
        shown,
        readable: false,
        encoding: Vec::new(),
    };
    if body.is_empty() {
        return Text {
            shown: String::new(),
            readable: true,
            encoding: Vec::new(),
        };
    }
    // An object's data, not a request to read: never copied whole.
    if body.len() > MAX_TEXT {
        return note(format!("({} bytes)", body.len()));
    }
    let decoded = lenslet_core::body::decode(headers, body);
    let mut encoding = decoded.encoding;
    if content_type.is_some_and(|t| t.contains("cbor"))
        && let Some(json) = cbor::decode_cbor_json(&decoded.bytes)
    {
        encoding.push("cbor");
        return Text {
            shown: json,
            readable: true,
            encoding,
        };
    }
    match std::str::from_utf8(&decoded.bytes) {
        Ok(text) => Text {
            shown: text.to_string(),
            readable: true,
            encoding,
        },
        Err(_) => note(format!("({} bytes of binary data)", body.len())),
    }
}

fn body_trace(body: &Bytes, text: &Text) -> Option<BodyTrace> {
    (!body.is_empty()).then(|| BodyTrace {
        // What lenslet-core reads: the payload, never the frames or a note.
        text: text.readable.then(|| text.shown.clone()),
        encoding: (!text.encoding.is_empty()).then(|| text.encoding.join(", ")),
        bytes: body.len() as u64,
        binary: !text.readable,
        ..Default::default()
    })
}

/// The most of a body that is not its own text (binary, CBOR, framed,
/// compressed) a call keeps, to copy it as curl.
const MAX_RAW: usize = 64 * 1024;

fn exchange(
    headers: &[HeaderTrace],
    content_type: Option<String>,
    text: Text,
    raw: &Bytes,
) -> Exchange {
    let mut body = text.shown;
    let mut verbatim = text.readable && text.encoding.is_empty();
    if body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n…");
        verbatim = false;
    }
    Exchange {
        headers: headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect(),
        content_type,
        body,
        bytes: raw.len(),
        verbatim,
        raw: (!verbatim && !raw.is_empty() && raw.len() <= MAX_RAW).then(|| raw.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(headers: &[(&str, &str)], method: &str, path: &str, body: &[u8]) -> Call {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let uri: http::Uri = path.parse().unwrap();
        let method: http::Method = method.parse().unwrap();
        record(
            Sent {
                method: &method,
                uri: &uri,
                headers: &map,
            },
            &Bytes::copy_from_slice(body),
            Answered {
                status: http::StatusCode::OK,
                headers: &http::HeaderMap::new(),
            },
            &Bytes::new(),
            0,
            1,
        )
    }

    fn scope(service: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential=AKID/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
        )
    }

    #[test]
    fn reads_what_calls_say_under_their_wire_encodings() {
        // An SDK's streaming upload: the object, not its frames.
        let call = recorded(
            &[
                ("authorization", &scope("s3")),
                ("host", "127.0.0.1:4510"),
                ("content-encoding", "aws-chunked"),
                ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
                ("x-amz-trailer", "x-amz-checksum-crc32"),
            ],
            "PUT",
            "/shop/notes.txt?x-id=PutObject",
            b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:NhCmhg==\r\n\r\n",
        );
        assert_eq!(call.request.body, "hello");
        assert!(!call.request.verbatim && call.request.raw.is_some());
        assert_eq!(call.target.as_deref(), Some("s3://shop/notes.txt"));

        // A compressed Query form: its action and what it names.
        let form = b"Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-1%3A000000000000%3Anews&Message=hi";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, form).unwrap();
        let call = recorded(
            &[
                ("authorization", &scope("sns")),
                (
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                ),
                ("content-encoding", "gzip"),
            ],
            "POST",
            "/",
            &gz.finish().unwrap(),
        );
        assert_eq!(call.operation, "Publish");
        assert_eq!(call.target.as_deref(), Some("topic/news"));
        assert_eq!(
            call.access.first().map(|a| a.resource.as_str()),
            Some("arn:aws:sns:*:*:news")
        );

        // Smithy RPCv2 CBOR: `{"QueueUrl": ".../jobs", "MessageBody": "x"}`,
        // whose bytes are not text, reads as its JSON.
        let mut cbor = vec![0xa2, 0x68];
        cbor.extend_from_slice(b"QueueUrl");
        let url = b"http://127.0.0.1:4510/000000000000/jobs";
        cbor.extend_from_slice(&[0x78, url.len() as u8]);
        cbor.extend_from_slice(url);
        cbor.push(0x6b);
        cbor.extend_from_slice(b"MessageBody");
        cbor.extend_from_slice(&[0x61, b'x', 0xff]);
        let call = recorded(
            &[
                ("authorization", &scope("sqs")),
                ("smithy-protocol", "rpc-v2-cbor"),
                ("content-type", "application/cbor"),
            ],
            "POST",
            "/service/AmazonSQS/operation/SendMessage",
            &cbor[..cbor.len() - 1],
        );
        assert_eq!(call.operation, "SendMessage");
        assert!(
            call.request.body.contains("\"QueueUrl\""),
            "{}",
            call.request.body
        );
        assert_eq!(
            call.access
                .iter()
                .map(|a| a.resource.as_str())
                .collect::<Vec<_>>(),
            vec!["arn:aws:sqs:*:*:jobs"],
            "no `*` for a body that was read"
        );
        assert_eq!(call.target.as_deref(), Some("queue/jobs"));
    }

    #[test]
    fn only_this_machines_names_are_served() {
        let ok = |h: &str| loopback_host(Some(&http::HeaderValue::from_str(h).unwrap()));
        assert!(ok("127.0.0.1:4510"));
        assert!(ok("localhost:4510"));
        assert!(ok("shop.localhost:4510"));
        assert!(ok("[::1]:4510"));
        assert!(ok("127.0.0.1"));
        assert!(!ok("evil.example:4510"));
        assert!(!ok("192.168.1.5:4510"));
        assert!(!ok("localhost.evil.example"));
        assert!(loopback_host(None));
    }
}
