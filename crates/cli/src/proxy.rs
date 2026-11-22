//! The Lenslet proxy: accept SDK calls over plain HTTP (HTTP/1.1 or `h2c`)
//! on loopback, preserve Host/SigV4, forward to the local emulator, and
//! record one run-file line per call when it finishes.
//!
//! Forwarding is streamed, never buffered: request and response bodies pass
//! through a tee unchanged, so SigV4 signatures stay valid and large uploads
//! or downloads are not limited or slowed. The tee keeps a bounded copy: of
//! a request whose access is named in its body, up to [`MAX_INSPECT_BYTES`]
//! (derivation reads the complete body), else up to
//! [`trace::MAX_BODY_TEXT`], which is also all the run file keeps.
//!
//! The run file is a local development artifact and keeps the raw wire
//! values: headers and text bodies exactly as they arrived, owner-only.

use std::{
    convert::Infallible,
    fs,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode, header, service::service_fn};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto as conn_auto,
};
use rustls::RootCertStore;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};

use crate::{
    access::{self, Access},
    capture::{self, Capture, Content, TeeBody},
    problem,
    trace::{self, Message, RunHeader, Trace, TraceSink},
    wire,
};

type Body = BoxBody<Bytes, std::io::Error>;
type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

/// The most of a request body read for access derivation (64 MiB, 4×
/// DynamoDB's 16 MiB batch limit). Only calls whose access is named in the
/// body get this much; a body past it is read as truncated, and what it
/// names past the limit stands as `*`.
const MAX_INSPECT_BYTES: usize = 64 * 1024 * 1024;

/// A client must send a request's headers within this long.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct State {
    client: HttpsClient,
    /// Upstream base URL without a trailing slash, e.g. `https://127.0.0.1:9298`.
    upstream: String,
    /// `--allow-docker`: which peers may send calls (see [`peer_allowed`]).
    allow_docker: bool,
    pub sink: Arc<TraceSink>,
    /// Calls started and not yet recorded.
    in_flight: AtomicUsize,
}

/// Counts a call in flight until dropped.
struct InFlight(Arc<State>);

impl InFlight {
    fn new(state: Arc<State>) -> Self {
        state.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(state)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Unique per proxied call: nanosecond timestamp plus a process-local sequence.
fn next_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{sequence:x}")
}

/// Proxy settings for `lenslet run`.
pub struct Options {
    pub upstream: Upstream,
    /// Whether `--upstream` was given.
    pub upstream_given: bool,
    /// `--emulator moto`: Lenslet starts moto for this run and stops it after.
    pub managed: bool,
    /// Listen on all interfaces and also serve calls from Docker networks
    /// (see [`peer_allowed`]).
    pub allow_docker: bool,
    /// CA that signs the emulator's certificate; derived from the upstream
    /// port when not given (`~/.aws/emulator/PORT/ca.pem`).
    pub emulator_ca: Option<PathBuf>,
    /// Fixed run file; by default each run gets its own in `~/.lenslet/runs`.
    pub trace: Option<PathBuf>,
    pub home: PathBuf,
}

impl Options {
    pub fn new(home: &Path) -> Self {
        Options {
            upstream: Upstream::parse("9298").expect("default upstream"),
            upstream_given: false,
            managed: false,
            allow_docker: false,
            emulator_ca: None,
            trace: None,
            home: home.to_path_buf(),
        }
    }

    /// Consume one proxy option. Returns `Ok(false)` when `arg` is not one.
    pub fn accept(&mut self, arg: &str, value: &mut dyn FnMut() -> Result<String>) -> Result<bool> {
        match arg {
            "--emulator" => {
                let raw = value()?;
                if raw.trim() != "moto" {
                    bail!(
                        "--emulator `{raw}`: Lenslet can start `moto`; for any other emulator, start it and pass --upstream"
                    );
                }
                if self.upstream_given {
                    bail!("use --upstream or --emulator moto, not both");
                }
                self.managed = true;
            }
            "--upstream" => {
                if self.managed {
                    bail!("use --upstream or --emulator moto, not both");
                }
                self.upstream = Upstream::parse(&value()?)?;
                self.upstream.ensure_local_boundary("--upstream")?;
                self.upstream_given = true;
            }
            "--allow-docker" => self.allow_docker = true,
            "--emulator-ca" => self.emulator_ca = Some(PathBuf::from(value()?)),
            "--trace" => self.trace = Some(PathBuf::from(value()?)),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The emulator's own directory: credentials, config and CA.
    pub fn emulator_dir(&self) -> PathBuf {
        self.home
            .join(".aws/emulator")
            .join(self.upstream.port.to_string())
    }

    pub fn emulator_ca(&self) -> PathBuf {
        self.emulator_ca
            .clone()
            .unwrap_or_else(|| self.emulator_dir().join("ca.pem"))
    }
}

fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Loopback, RFC1918 (`10/8`, `172.16/12`, `192.168/16`), link-local, or
/// IPv6 ULA (`fc00::/7`) / link-local: addresses on this machine or a private
/// container network, never the public internet.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Who may send calls through the proxy: loopback only. With
/// `--allow-docker` (the proxy then listens on all interfaces) also Docker
/// bridge networks (`172.16.0.0/12`) and the Docker Desktop VM
/// (`192.168.65.0/24`); never the rest of the LAN.
pub(crate) fn peer_allowed(ip: IpAddr, allow_docker: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        other => other,
    };
    if is_loopback_ip(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) if allow_docker => {
            let [a, b, c, _] = v4.octets();
            (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168 && c == 65)
        }
        _ => false,
    }
}

/// Real AWS endpoints, which Lenslet never proxies to.
fn is_aws_domain(host: &str) -> bool {
    [
        "amazonaws.com",
        "amazonaws.com.cn",
        "api.aws",
        "amazonaws.eu",
    ]
    .iter()
    .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// Where the proxy sends requests: `PORT` means `https://127.0.0.1:PORT`.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// Base URL without a trailing slash, e.g. `https://127.0.0.1:9298`.
    pub base: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Upstream {
    pub fn parse(value: &str) -> Result<Self> {
        let text = if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) {
            format!("https://127.0.0.1:{value}")
        } else {
            value.to_string()
        };
        let uri: hyper::Uri = text.parse().with_context(|| {
            format!("--upstream expects a port or an http(s) URL, got `{value}`")
        })?;
        let scheme = uri.scheme_str().unwrap_or_default();
        if scheme != "http" && scheme != "https" {
            bail!("--upstream `{value}` must use http:// or https://");
        }
        let authority = uri
            .authority()
            .with_context(|| format!("--upstream `{value}` needs a host and port"))?;
        if !uri.path().is_empty() && uri.path() != "/" {
            bail!("--upstream `{value}` must not include a path");
        }
        if uri.query().is_some() {
            bail!("--upstream `{value}` must not include a query");
        }
        let port = authority
            .port_u16()
            .with_context(|| format!("--upstream `{value}` needs an explicit port"))?;
        if port == 0 {
            bail!("--upstream `{value}` needs a port between 1 and 65535");
        }
        Ok(Upstream {
            base: format!("{scheme}://{authority}"),
            host: authority.host().trim_matches(['[', ']']).to_string(),
            port,
            tls: scheme == "https",
        })
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    /// Enforce Lenslet's strict local boundary: never proxy traffic to real
    /// AWS or the public internet. The host is resolved, and every address it
    /// resolves to must be loopback or a private (container) network.
    pub fn ensure_local_boundary(&self, flag: &str) -> Result<()> {
        let host = self.host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            bail!("{flag} `{}` needs a host", self.base);
        }
        if is_aws_domain(&host) {
            bail!(
                "{flag} `{}` points at real AWS cloud; Lenslet only proxies to local emulators",
                self.base
            );
        }
        use std::net::ToSocketAddrs as _;
        let addresses: Vec<SocketAddr> = (host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("{flag} `{}`: cannot resolve `{host}`", self.base))?
            .collect();
        if addresses.is_empty() {
            bail!("{flag} `{}`: `{host}` resolves to no address", self.base);
        }
        if let Some(public) = addresses.iter().find(|addr| !is_private_ip(addr.ip())) {
            bail!(
                "{flag} `{}` resolves to {}, which is not loopback or a private network; Lenslet only proxies to local emulators",
                self.base,
                public.ip()
            );
        }
        Ok(())
    }

    /// Whether something accepts TCP connections at the upstream address
    /// (trying normal resolution first, so IPv6-only `localhost` works too).
    pub async fn reachable(&self) -> bool {
        let primary = tokio::time::timeout(
            Duration::from_millis(400),
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .is_ok_and(|connected| connected.is_ok());
        if primary || self.host != "localhost" {
            return primary;
        }
        tokio::time::timeout(
            Duration::from_millis(250),
            TcpStream::connect(("127.0.0.1", self.port)),
        )
        .await
        .is_ok_and(|connected| connected.is_ok())
    }
}

pub(crate) fn emulator_roots(path: &Path) -> Result<RootCertStore> {
    let pem = fs::read(path).with_context(|| {
        format!(
            "read emulator CA {} (start AWS Emulator first, or pass --emulator-ca)",
            path.display()
        )
    })?;
    let mut roots = RootCertStore::empty();
    let mut count = 0;
    for cert in rustls_pemfile::certs(&mut &pem[..]) {
        roots
            .add(cert.with_context(|| format!("parse emulator CA {}", path.display()))?)
            .with_context(|| format!("trust emulator CA {}", path.display()))?;
        count += 1;
    }
    if count == 0 {
        bail!("no certificates in emulator CA {}", path.display());
    }
    Ok(roots)
}

fn client(roots: RootCertStore) -> HttpsClient {
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

/// Remove hop-by-hop headers (and any the `Connection` header names): they
/// describe one connection, not the request or response, and are not
/// forwarded. The run file still records them as sent.
fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    let named: Vec<header::HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

/// One proxied call while it is being handled: its trace entry so far and
/// what is needed to finish it.
struct Call {
    trace: Trace,
    started: Instant,
    /// The operation the wire protocol advertised; when `None`, the request
    /// body may still supply one (AWS Query `Action=...`).
    advertised_operation: Option<String>,
    request_capture: Arc<Mutex<Capture>>,
    request_content: Content,
    in_flight: InFlight,
}

impl Call {
    /// The trace entry for a request that has just arrived, and the request
    /// body wrapped to be captured as it is forwarded.
    fn begin<B>(req: Request<B>, state: &Arc<State>) -> (Call, hyper::http::request::Parts, Body)
    where
        B: hyper::body::Body<Data = Bytes> + Send + Sync + Unpin + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let started = Instant::now();
        let (parts, body) = req.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map(|path| path.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        let host = parts
            .headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .or_else(|| parts.uri.authority().map(|authority| authority.as_str()))
            .unwrap_or("")
            .to_string();
        let wire::Wire {
            service,
            region,
            operation: advertised_operation,
            invocation_id,
            attempt,
            max_attempts,
            ..
        } = wire::wire_info(&parts.headers, &path);
        let attempt = attempt.filter(|n| *n > 1);
        let operation = advertised_operation.clone().unwrap_or_else(|| {
            wire::fallback_operation(&service, parts.method.as_str(), &path, &parts.headers)
        });
        let request_content = capture::content(&parts.headers);
        // A call whose access is named in its body is read in full (up to
        // the inspection limit) for derivation.
        let limit = if access::needs_request_body(
            &service,
            advertised_operation.as_deref(),
            &operation,
            &path,
        ) {
            MAX_INSPECT_BYTES
        } else {
            trace::MAX_BODY_TEXT
        };
        let request_capture = Arc::new(Mutex::new(Capture::new(limit)));
        let trace = Trace {
            id: next_id(),
            at: Utc::now().to_rfc3339(),
            method: parts.method.to_string(),
            path,
            host,
            service,
            operation,
            authorization: parts.headers.contains_key("authorization"),
            region,
            invocation_id,
            attempt,
            max_attempts: max_attempts.filter(|_| attempt.is_some()),
            request: Message {
                headers: trace::headers(&parts.headers),
                content_type: request_content.content_type.clone(),
                body: None,
            },
            ..Trace::default()
        };
        let body = TeeBody::new(body, request_capture.clone())
            .map_err(|error| std::io::Error::other(error.into()))
            .boxed();
        let call = Call {
            trace,
            started,
            advertised_operation,
            request_capture,
            request_content,
            in_flight: InFlight::new(state.clone()),
        };
        (call, parts, body)
    }

    /// Record the call with what its complete request needs.
    fn finish(mut self, response: Option<(&Mutex<Capture>, &Content)>) {
        self.trace.ms = self.started.elapsed().as_millis() as u64;
        if let Ok(capture) = self.request_capture.lock() {
            if self.advertised_operation.is_none()
                && let Some(action) = query_action(&capture, &self.request_content)
            {
                self.trace.operation = action;
            }
            self.trace.request.body = capture.body(&self.request_content);
        }
        if let Some((capture, content)) = response {
            self.trace.response.body = capture.lock().ok().and_then(|c| c.body(content));
            self.trace.problem = problem::from_response(
                self.trace.status,
                &self.trace.response.headers,
                self.trace
                    .response
                    .body
                    .as_ref()
                    .and_then(|body| body.text.as_deref()),
            );
        }
        let derived = access::derive_call(&self.trace);
        let state = self.in_flight.0.clone();
        state.sink.record(self.trace, &derived);
    }
}

/// `Action=` from a captured form body, if the request is a query-protocol call.
fn query_action(capture: &Capture, content: &Content) -> Option<String> {
    let prefix = capture.decoded_prefix(content);
    let form = content.content_type.as_deref() == Some("application/x-www-form-urlencoded")
        || prefix.trim_ascii_start().starts_with(b"Action=");
    form.then(|| wire::form_action(&prefix)).flatten()
}

async fn handle<B>(req: Request<B>, state: Arc<State>) -> Result<Response<Body>, Infallible>
where
    B: hyper::body::Body<Data = Bytes> + Send + Sync + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (mut call, mut parts, body) = Call::begin(req, &state);
    let Some(forward_path) = prepare_forward(&mut parts.headers, &call.trace) else {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "invalid request path",
        ));
    };
    let uri = format!("{}{forward_path}", state.upstream);
    match forward(&state, parts, uri, body).await {
        Ok((status, mut headers, response_body)) => {
            let content = capture::content(&headers);
            call.trace.status = status.as_u16();
            call.trace.response = Message {
                headers: trace::headers(&headers),
                content_type: content.content_type.clone(),
                body: None,
            };
            strip_hop_by_hop(&mut headers);
            let capture = Arc::new(Mutex::new(Capture::new(trace::MAX_BODY_TEXT)));
            let tee_capture = capture.clone();
            // The call is recorded when the response body ends (or the
            // client goes away), so `ms` covers the whole exchange.
            let body = TeeBody::new(response_body, tee_capture)
                .on_end(move || call.finish(Some((&capture, &content))))
                .boxed();
            let mut out = Response::new(body);
            *out.status_mut() = status;
            *out.headers_mut() = headers;
            Ok(out)
        }
        Err(detail) => {
            call.trace.status = StatusCode::BAD_GATEWAY.as_u16();
            call.trace.problem = Some(problem::unreachable(&state.upstream, &detail));
            call.trace.error = Some(detail);
            call.finish(None);
            Ok(error_response(StatusCode::BAD_GATEWAY, "upstream error"))
        }
    }
}

/// Make the request's headers ready to forward, and return the path to
/// forward to (`None` for a path Lenslet cannot forward).
fn prepare_forward(headers: &mut hyper::HeaderMap, trace: &Trace) -> Option<String> {
    strip_hop_by_hop(headers);
    // HTTP/2 `h2c` clients send `:authority` in the URI instead of a `Host`
    // header; populate `Host` so HTTP/1.1 emulators validate SigV4 signatures.
    if !headers.contains_key(header::HOST)
        && !trace.host.is_empty()
        && let Ok(value) = header::HeaderValue::from_str(&trace.host)
    {
        headers.insert(header::HOST, value);
    }
    if trace.path.starts_with('/') {
        Some(trace.path.clone())
    } else if trace.path == "*" {
        Some("/".to_string())
    } else {
        None
    }
}

/// Send the request upstream. End-to-end headers (including Host and
/// Authorization) are forwarded verbatim so SigV4 signatures remain valid.
async fn forward(
    state: &State,
    parts: hyper::http::request::Parts,
    uri: String,
    body: Body,
) -> Result<(StatusCode, hyper::HeaderMap, Body), String> {
    let mut upstream = Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(body)
        .map_err(|error| error.to_string())?;
    *upstream.headers_mut() = parts.headers;
    state
        .client
        .request(upstream)
        .await
        .map(|response| {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.into_body().map_err(std::io::Error::other).boxed();
            (status, headers, body)
        })
        .map_err(|e| error_chain(&e))
}

/// `client error (Connect): tcp connect error: Connection refused` instead of
/// just the outermost `client error (Connect)`.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = Full::new(Bytes::from(message.to_string()))
        .map_err(|never| match never {})
        .boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// A bound, running proxy.
pub struct Server {
    pub addr: SocketAddr,
    pub state: Arc<State>,
    shutdown: watch::Sender<bool>,
    /// One accept loop per listener; each owns its connections.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Validate everything, bind a free loopback port (IPv4 + best-effort
    /// IPv6 so `localhost` works on either stack), write the run header, and
    /// start serving in the background. `locked` is the lock's lines, when
    /// there is a lock.
    pub async fn start(
        options: &Options,
        trace_path: PathBuf,
        mut header: RunHeader,
        locked: Option<std::collections::BTreeSet<Access>>,
    ) -> Result<Server> {
        options.upstream.ensure_local_boundary("--upstream")?;
        // rustls needs a crypto provider before any TLS config is built.
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let roots = if options.upstream.tls {
            emulator_roots(&options.emulator_ca())?
        } else {
            RootCertStore::empty()
        };
        let bind_v4 = if options.allow_docker {
            IpAddr::from([0, 0, 0, 0])
        } else {
            IpAddr::from([127, 0, 0, 1])
        };
        let listener = TcpListener::bind((bind_v4, 0))
            .await
            .with_context(|| format!("bind {bind_v4}"))?;
        let port = listener
            .local_addr()
            .context("read the bound address")?
            .port();
        // Host processes use `127.0.0.1:PORT`; Docker containers reach
        // `0.0.0.0:PORT` via `host.docker.internal` or `172.17.0.1`.
        let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
        let v6_listener = TcpListener::bind((IpAddr::V6(Ipv6Addr::LOCALHOST), port))
            .await
            .ok();
        header.endpoint = format!("http://{addr}");
        header.upstream = options.upstream.base.clone();
        let sink = Arc::new(TraceSink::new(trace_path, &header, locked)?);
        let state = Arc::new(State {
            client: client(roots),
            upstream: options.upstream.base.clone(),
            allow_docker: options.allow_docker,
            sink,
            in_flight: AtomicUsize::new(0),
        });
        let shutdown = watch::Sender::new(false);
        let tasks = std::iter::once(listener)
            .chain(v6_listener)
            .map(|listener| tokio::spawn(serve(listener, state.clone(), shutdown.subscribe())))
            .collect();
        Ok(Server {
            addr,
            state,
            shutdown,
            tasks,
        })
    }

    /// The endpoint SDKs should use.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Give in-flight calls up to `grace` to finish, then stop accepting
    /// connections and close idle ones. A call that finishes after the run
    /// is sealed is not recorded.
    pub async fn stop(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while self.state.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.shutdown.send_replace(true);
    }

    /// After [`Server::stop`]: wait up to `grace` for open connections to
    /// finish, then drop the rest.
    pub async fn join(self, grace: Duration) {
        let deadline = Instant::now() + grace;
        for mut task in self.tasks {
            let left = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(left, &mut task).await.is_err() {
                task.abort();
            }
        }
    }
}

/// Accept connections until shut down, then wait for the open ones (each
/// shut down gracefully). Dropping this future drops every connection.
async fn serve(listener: TcpListener, state: Arc<State>, mut shutdown: watch::Receiver<bool>) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, peer)) = accepted {
                    // Loopback only; with `--allow-docker` also Docker networks.
                    if !peer_allowed(peer.ip(), state.allow_docker) {
                        drop(stream);
                        continue;
                    }
                    connections.spawn(serve_connection(stream, state.clone(), shutdown.clone()));
                }
            }
            () = stopped(&mut shutdown) => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
}

/// Resolves once the server is stopping (or gone).
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn serve_connection(
    stream: TcpStream,
    state: Arc<State>,
    mut shutdown: watch::Receiver<bool>,
) {
    let service = service_fn(move |req| handle(req, state.clone()));
    let mut builder = conn_auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder.http2().timer(TokioTimer::new());
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    let result = tokio::select! {
        result = connection.as_mut() => result,
        () = stopped(&mut shutdown) => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        let msg = error.to_string();
        if !msg.contains("incomplete message") && !msg.contains("timed out") {
            eprintln!("lenslet: connection error: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{HeaderMap, body::Incoming};
    use tokio::sync::mpsc;

    fn options(arguments: &[&str]) -> Result<Options> {
        let mut options = Options::new(Path::new("/home/tester"));
        let mut it = arguments.iter().map(|argument| argument.to_string());
        while let Some(arg) = it.next() {
            let mut value = || it.next().context("needs a value");
            if !options.accept(&arg, &mut value)? {
                bail!("unknown {arg}");
            }
        }
        Ok(options)
    }

    #[test]
    fn validates_upstream_addresses_and_emulator_choices() {
        let parsed = options(&["--upstream", "9400"]).unwrap();
        assert_eq!(parsed.upstream.base, "https://127.0.0.1:9400");
        assert_eq!(
            parsed.emulator_ca(),
            PathBuf::from("/home/tester/.aws/emulator/9400/ca.pem"),
            "the emulator CA follows the upstream port"
        );
        assert!(options(&["--upstream", "0"]).is_err());
        assert!(options(&["--upstream", "ftp://127.0.0.1:21"]).is_err());
        assert!(options(&["--upstream", "https://127.0.0.1"]).is_err());
        let plain = options(&["--upstream", "http://127.0.0.1:8080"]).unwrap();
        assert!(!plain.upstream.tls);
        assert!(options(&["--emulator", "moto"]).unwrap().managed);
        assert!(options(&["--emulator", "localstack"]).is_err());
        assert!(options(&["--emulator", "moto", "--upstream", "4566"]).is_err());
        assert!(options(&["--upstream", "4566", "--emulator", "moto"]).is_err());
        for removed in [
            "--sandbox",
            "--listen",
            "--route",
            "--max-body",
            "--enforce",
        ] {
            assert!(options(&[removed, "x"]).is_err(), "{removed}");
        }
        for removed in ["sandbox", "built-in"] {
            assert!(options(&["--upstream", removed]).is_err(), "{removed}");
        }
    }

    #[test]
    fn the_local_boundary_resolves_names_and_rejects_public_and_aws_hosts() {
        for ok in [
            "http://localhost:4566",
            "http://127.0.0.1:4566",
            "http://[::1]:4566",
            "http://172.17.0.1:4566",
            "http://10.1.2.3:4566",
        ] {
            assert!(options(&["--upstream", ok]).is_ok(), "{ok}");
        }
        for aws in [
            "https://s3.amazonaws.com:443",
            "https://s3.amazonaws.com.:443",
            "https://sts.cn-north-1.amazonaws.com.cn:443",
            "https://bedrock.us-east-1.api.aws:443",
        ] {
            let error = options(&["--upstream", aws]).err().unwrap().to_string();
            assert!(error.contains("real AWS"), "{aws}: {error}");
        }
        for public in [
            "http://8.8.8.8:80",
            "http://[2606:4700::1111]:80",
            "http://0.0.0.0:80",
        ] {
            let error = options(&["--upstream", public]).err().unwrap().to_string();
            assert!(
                error.contains("not loopback or a private network"),
                "{public}: {error}"
            );
        }
        assert!(options(&["--upstream", "http://lenslet-no-such-host.invalid:80"]).is_err());
    }

    #[test]
    fn allow_docker_serves_loopback_and_docker_networks_only() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        for loopback in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(peer_allowed(ip(loopback), false), "{loopback}");
        }
        for docker in ["172.17.0.2", "172.31.255.254", "192.168.65.3"] {
            assert!(!peer_allowed(ip(docker), false), "{docker}");
            assert!(peer_allowed(ip(docker), true), "{docker}");
        }
        for lan in [
            "192.168.1.20",
            "10.0.0.7",
            "172.32.0.1",
            "8.8.8.8",
            "fd00::1",
        ] {
            assert!(!peer_allowed(ip(lan), true), "{lan}");
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            TempDir(std::env::temp_dir().join(format!(
                "lenslet-proxy-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed),
            )))
        }

        fn trace(&self) -> PathBuf {
            self.0.join("trace.jsonl")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A plain-HTTP upstream that records what it received and answers with
    /// JSON, a 404 for paths ending in `/missing`, or more than 1 MiB of
    /// text for `/big`.
    async fn spawn_upstream() -> (u16, mpsc::Receiver<(HeaderMap, Bytes)>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let tx = tx.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let path = parts.uri.path().to_string();
                            let received = body
                                .collect()
                                .await
                                .map(|collected| collected.to_bytes())
                                .unwrap_or_default();
                            let _ = tx.send((parts.headers, received)).await;
                            let (status, content_type, body) = if path.ends_with("/missing") {
                                (
                                    StatusCode::NOT_FOUND,
                                    "application/xml",
                                    "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>".to_string(),
                                )
                            } else if path.ends_with("/big") {
                                (
                                    StatusCode::OK,
                                    "text/plain",
                                    "x".repeat(trace::MAX_BODY_TEXT + 10),
                                )
                            } else {
                                (
                                    StatusCode::OK,
                                    "application/json",
                                    r#"{"ok":true}"#.to_string(),
                                )
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("content-type", content_type)
                                    .header("connection", "keep-alive")
                                    .header("keep-alive", "timeout=5")
                                    .body(Full::new(Bytes::from(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = conn_auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (port, rx)
    }

    async fn server(upstream: u16, temp: &TempDir) -> Server {
        let mut options = Options::new(&temp.0);
        options.upstream = Upstream::parse(&format!("http://127.0.0.1:{upstream}")).unwrap();
        Server::start(&options, temp.trace(), RunHeader::default(), None)
            .await
            .unwrap()
    }

    fn traces(path: &Path) -> Vec<Trace> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| match serde_json::from_str(line).expect("a record") {
                trace::Record::Call(trace) => Some(*trace),
                _ => None,
            })
            .collect()
    }

    fn request(method: &str, uri: &str, service: &str) -> hyper::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(
                "authorization",
                format!(
                    "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261115/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=deadbeef"
                ),
            )
    }

    async fn send(
        sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        req: Request<Full<Bytes>>,
    ) -> (StatusCode, Bytes) {
        let response = sender.send_request(req).await.unwrap();
        let status = response.status();
        (
            status,
            response.into_body().collect().await.unwrap().to_bytes(),
        )
    }

    #[tokio::test]
    async fn forwards_byte_exact_and_records_one_line_per_finished_call() {
        let (port, mut upstream) = spawn_upstream().await;
        let temp = TempDir::new("forward");
        let server = server(port, &temp).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);

        let body = br#"{"password":"hunter2"}"#.to_vec();
        let req = request("PUT", "/bucket/key?x-id=PutObject", "s3")
            .header("host", "127.0.0.1:9297")
            .header("content-type", "application/json")
            .header("connection", "keep-alive")
            .body(Full::new(Bytes::from(body.clone())))
            .unwrap();
        let (status, answer) = send(&mut sender, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&answer[..], br#"{"ok":true}"#);
        let (headers, forwarded) = upstream.recv().await.unwrap();
        assert_eq!(forwarded, body, "request body is byte exact");
        assert!(
            headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("deadbeef")
        );
        assert!(
            headers.get("connection").is_none(),
            "hop-by-hop headers are not forwarded"
        );

        let req = request("GET", "/bucket/big", "s3")
            .header("host", "127.0.0.1:9297")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let (_, big) = send(&mut sender, req).await;
        assert_eq!(
            big.len(),
            trace::MAX_BODY_TEXT + 10,
            "large responses are not cut"
        );
        let req = request("GET", "/bucket/missing", "s3")
            .header("host", "127.0.0.1:9297")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let (status, _) = send(&mut sender, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        server.stop(Duration::from_secs(1)).await;
        let written = traces(&temp.trace());
        assert_eq!(written.len(), 3);
        let put = &written[0];
        assert_eq!(
            (put.service.as_str(), put.operation.as_str()),
            ("s3", "PutObject")
        );
        assert_eq!(
            put.request.body.as_ref().unwrap().text.as_deref(),
            Some(r#"{"password":"hunter2"}"#)
        );
        assert!(
            put.request.headers.iter().any(|h| h.name == "connection"),
            "recorded as sent"
        );
        assert_eq!(put.access[0].action, "s3:PutObject");
        assert_eq!(put.concrete[0].resource, "arn:aws:s3:::bucket/key");
        let big = written[1].response.body.as_ref().unwrap();
        assert!(big.truncated);
        assert_eq!(big.text.as_deref().unwrap().len(), trace::MAX_BODY_TEXT);
        assert_eq!(big.bytes, (trace::MAX_BODY_TEXT + 10) as u64);
        let missing = written[2].problem.as_ref().unwrap();
        assert_eq!(missing.code, "NoSuchKey");
        server.join(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn derivation_reads_the_complete_body_and_cbor() {
        let (port, _upstream) = spawn_upstream().await;
        let temp = TempDir::new("derive");
        let server = server(port, &temp).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
                .await
                .unwrap();
        tokio::spawn(conn);

        // A batch whose second table is named past the stored 1 MiB.
        let pad = "x".repeat(trace::MAX_BODY_TEXT + 100);
        let batch = format!(
            r#"{{"RequestItems":{{"aaa":[{{"PutRequest":{{"Item":{{"p":{{"S":"{pad}"}}}}}}}}],"zzz":[{{"DeleteRequest":{{"Key":{{"k":{{"S":"1"}}}}}}}}]}}}}"#
        );
        let req = request(
            "POST",
            format!("http://{}/", server.addr).as_str(),
            "dynamodb",
        )
        .header("x-amz-target", "DynamoDB_20120810.BatchWriteItem")
        .header("content-type", "application/x-amz-json-1.0")
        .body(Full::new(Bytes::from(batch)))
        .unwrap();
        let response = sender.send_request(req).await.unwrap();
        response.into_body().collect().await.unwrap();

        // Smithy RPCv2 CBOR: {"TableName": "orders"}.
        let mut frame = vec![0xa1, 0x69];
        frame.extend_from_slice(b"TableName");
        frame.push(0x66);
        frame.extend_from_slice(b"orders");
        let req = request(
            "POST",
            format!(
                "http://{}/service/DynamoDB_20120810/operation/GetItem",
                server.addr
            )
            .as_str(),
            "dynamodb",
        )
        .header("smithy-protocol", "rpc-v2-cbor")
        .header("content-type", "application/cbor")
        .body(Full::new(Bytes::from(frame)))
        .unwrap();
        let response = sender.send_request(req).await.unwrap();
        response.into_body().collect().await.unwrap();

        server.stop(Duration::from_secs(1)).await;
        let written = traces(&temp.trace());
        assert_eq!(written.len(), 2);
        let tables: Vec<&str> = written[0]
            .access
            .iter()
            .map(|a| a.resource.as_str())
            .collect();
        assert_eq!(
            tables,
            [
                "arn:aws:dynamodb:*:*:table/aaa",
                "arn:aws:dynamodb:*:*:table/zzz"
            ]
        );
        assert!(written[0].request.body.as_ref().unwrap().truncated);
        assert_eq!(written[1].operation, "GetItem");
        assert_eq!(
            written[1].concrete,
            [Access::new(
                "dynamodb:GetItem",
                "arn:aws:dynamodb:us-east-1:*:table/orders"
            )]
        );
        server.join(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn records_an_unreachable_upstream_as_a_502() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let temp = TempDir::new("unreachable");
        let server = server(port, &temp).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        let req = request("POST", "/", "sqs")
            .header("host", "127.0.0.1:9297")
            .header("x-amz-target", "AmazonSQS.SendMessage")
            .header("content-type", "application/x-amz-json-1.0")
            .body(Full::new(Bytes::from_static(
                br#"{"QueueUrl":"http://q/1/jobs"}"#,
            )))
            .unwrap();
        let (status, body) = send(&mut sender, req).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(&body[..], b"upstream error");
        server.stop(Duration::from_secs(1)).await;
        let written = traces(&temp.trace());
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].status, 502);
        assert_eq!(written[0].operation, "SendMessage");
        assert_eq!(
            written[0].problem.as_ref().unwrap().code,
            "EmulatorUnreachable"
        );
        assert!(written[0].error.as_deref().unwrap().contains("onnect"));
        server.join(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn lenslet_paths_are_forwarded_like_any_other() {
        let (port, mut upstream) = spawn_upstream().await;
        let temp = TempDir::new("no-endpoints");
        let server = server(port, &temp).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        for path in ["/_lenslet/events", "/_lenslet/health"] {
            let req = Request::builder()
                .uri(path)
                .header("host", "127.0.0.1")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let (status, _) = send(&mut sender, req).await;
            assert_eq!(status, StatusCode::OK);
            assert!(upstream.recv().await.is_some(), "{path} went upstream");
        }
        server.stop(Duration::from_secs(1)).await;
        server.join(Duration::from_secs(1)).await;
    }
}
