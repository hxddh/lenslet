//! Which local AWS emulator answers a run: finding one that is already
//! running (verified by its protocol, never by an open port alone), and
//! starting and stopping moto when none is.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::RootCertStore;

use crate::proxy::{self, Upstream};

/// A probe gives up connecting after this long...
const PROBE_CONNECT: Duration = Duration::from_millis(500);
/// ...and waiting for the whole answer after this long.
const PROBE_RESPONSE: Duration = Duration::from_millis(1500);
/// At most this much of a probe's answer is read.
const PROBE_BYTES: usize = 16 * 1024;
/// Managed moto must answer within this long of starting.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Managed moto gets this long to exit after `SIGTERM`, then `SIGKILL`.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The message when no emulator is running and moto cannot be started.
pub const NOT_FOUND: &str = "no local AWS emulator found. Start one (AWS Emulator, LocalStack, moto) \
     or install moto (pip install 'moto[server]') and Lenslet will start it for you.";

/// The emulators Lenslet recognizes by their protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Moto,
    LocalStack,
    AwsEmulator,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Moto => "moto",
            Kind::LocalStack => "localstack",
            Kind::AwsEmulator => "aws-emulator",
        }
    }
}

/// An emulator that answered a probe.
#[derive(Clone, Debug)]
pub struct Found {
    pub upstream: Upstream,
    /// `None`: it answers like AWS, but is none of the [`Kind`]s.
    pub kind: Option<Kind>,
    /// How it was found, e.g. `AWS_ENDPOINT_URL` or `port 4566`.
    pub source: &'static str,
}

impl Found {
    /// The run header's `emulator`: the kind, or the URL for anything else.
    pub fn label(&self) -> String {
        self.kind
            .map(|kind| kind.name().to_string())
            .unwrap_or_else(|| self.upstream.base.clone())
    }
}

/// The CA that signs AWS Emulator's certificate on `port`.
pub fn default_ca(home: &Path, port: u16) -> PathBuf {
    home.join(".aws/emulator")
        .join(port.to_string())
        .join("ca.pem")
}

/// `GET path` on the upstream: the status and the lower-cased header lines
/// and start of the body, or `None` when nothing answered in time.
async fn probe(upstream: &Upstream, path: &str, ca: Option<&Path>) -> Option<(u16, String)> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let roots = if upstream.tls {
        proxy::emulator_roots(ca?).ok()?
    } else {
        RootCertStore::empty()
    };
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(PROBE_CONNECT));
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    let request = Request::get(format!("{}{path}", upstream.base))
        .header(hyper::header::CONNECTION, "close")
        .body(Empty::new())
        .ok()?;
    let answer = async {
        let response = client.request(request).await.ok()?;
        let status = response.status().as_u16();
        let mut text = String::new();
        for (name, value) in response.headers() {
            text.push_str(name.as_str());
            text.push_str(": ");
            text.push_str(&String::from_utf8_lossy(value.as_bytes()));
            text.push('\n');
        }
        let mut body = response.into_body();
        let mut read = Vec::new();
        while read.len() < PROBE_BYTES {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        read.extend_from_slice(&data);
                    }
                }
                _ => break,
            }
        }
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&read));
        Some((status, text.to_ascii_lowercase()))
    };
    tokio::time::timeout(PROBE_RESPONSE, answer)
        .await
        .ok()
        .flatten()
}

/// What is answering at `upstream`, by its protocol: `None` when nothing
/// answers like an AWS emulator (an open port alone never counts; port 5000
/// is AirPlay on macOS). An HTTPS upstream counts only when its certificate
/// is signed by `ca` (AWS Emulator's own CA).
pub async fn identify(upstream: &Upstream, ca: Option<&Path>) -> Option<Option<Kind>> {
    if upstream.tls {
        return probe(upstream, "/", ca)
            .await
            .map(|_| Some(Kind::AwsEmulator));
    }
    // Nothing answering this first probe in time means nothing is there.
    let (status, text) = probe(upstream, "/_localstack/health", None).await?;
    if status == 200 && text.contains("\"services\"") {
        return Some(Some(Kind::LocalStack));
    }
    if let Some((200, text)) = probe(upstream, "/moto-api/", None).await
        && text.contains("moto")
    {
        return Some(Some(Kind::Moto));
    }
    let (_, text) = probe(upstream, "/", None).await?;
    if text.contains("airtunes") || text.contains("airplay") {
        return None;
    }
    let aws = [
        "x-amz-request-id",
        "x-amzn-requestid",
        "missingauthenticationtoken",
        "<errorresponse",
        "listallmybucketsresult",
    ];
    aws.iter()
        .any(|marker| text.contains(marker))
        .then_some(None)
}

/// The places an emulator is looked for, in order.
fn candidates(env_endpoint: Option<&str>) -> Vec<(Upstream, &'static str)> {
    let mut out = Vec::new();
    if let Some(raw) = env_endpoint.map(str::trim).filter(|s| !s.is_empty())
        && let Ok(parsed) = Upstream::parse(raw)
        && parsed.is_loopback()
    {
        out.push((parsed, "AWS_ENDPOINT_URL"));
    }
    for (url, source) in [
        ("https://127.0.0.1:9298", "port 9298"),
        ("http://127.0.0.1:4566", "port 4566"),
        ("http://127.0.0.1:5000", "port 5000"),
    ] {
        if let Ok(parsed) = Upstream::parse(url) {
            out.push((parsed, source));
        }
    }
    out
}

async fn check(upstream: Upstream, source: &'static str, home: &Path) -> Option<Found> {
    let ca = default_ca(home, upstream.port);
    let kind = identify(&upstream, Some(&ca)).await?;
    Some(Found {
        upstream,
        kind,
        source,
    })
}

/// The first running emulator: a loopback `AWS_ENDPOINT_URL`, AWS Emulator
/// on 9298, LocalStack on 4566, moto on 5000.
pub async fn discover(env_endpoint: Option<&str>, home: &Path) -> Option<Found> {
    for (upstream, source) in candidates(env_endpoint) {
        if let Some(found) = check(upstream, source, home).await {
            return Some(found);
        }
    }
    None
}

/// Every recognized emulator that is running now (for `lenslet doctor`).
pub async fn running(env_endpoint: Option<&str>, home: &Path) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for (upstream, source) in candidates(env_endpoint) {
        if out.iter().any(|found| found.upstream.base == upstream.base) {
            continue;
        }
        if let Some(found) = check(upstream, source, home).await
            && found.kind.is_some()
        {
            out.push(found);
        }
    }
    out
}

/// How to start moto here: `moto_server` on PATH, else `python3 -m
/// moto.server` when that module imports; `None` when moto is not installed.
pub fn moto_command() -> Option<Vec<String>> {
    if crate::command_exists_on_path("moto_server") {
        return Some(vec!["moto_server".to_string()]);
    }
    let imports = Command::new("python3")
        .args(["-c", "import moto.server"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    imports.then(|| {
        ["python3", "-m", "moto.server"]
            .map(str::to_string)
            .to_vec()
    })
}

/// A moto server Lenslet started for one run. It runs in its own process
/// group and is stopped (the whole group) by [`Managed::stop`] or on drop.
pub struct Managed {
    child: Option<Child>,
    pub upstream: Upstream,
    pub port: u16,
    pub log: PathBuf,
}

impl Managed {
    /// Start `command` on a free loopback port, its output going to `log`
    /// (0600), and wait until it answers.
    #[cfg(test)]
    pub async fn start(command: &[String], log: &Path) -> Result<Managed> {
        let mut managed = Managed::spawn(command, log)?;
        managed.ready().await?;
        Ok(managed)
    }

    /// Start `command` on a free loopback port, its output going to `log`
    /// (0600). It is not answering yet: see [`Managed::ready`].
    pub fn spawn(command: &[String], log: &Path) -> Result<Managed> {
        let port = free_port()?;
        if let Some(parent) = log.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            lenslet_core::runs::set_mode(parent, 0o700)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let out = options
            .open(log)
            .with_context(|| format!("open {}", log.display()))?;
        let err = out
            .try_clone()
            .with_context(|| format!("open {}", log.display()))?;
        let mut spawn = Command::new(&command[0]);
        spawn
            .args(&command[1..])
            .args(["-H", "127.0.0.1", "-p", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            spawn.process_group(0);
        }
        let child = spawn
            .spawn()
            .with_context(|| format!("start `{}`", command.join(" ")))?;
        let upstream = Upstream::parse(&format!("http://127.0.0.1:{port}"))?;
        Ok(Managed {
            child: Some(child),
            upstream,
            port,
            log: log.to_path_buf(),
        })
    }

    /// Wait until moto answers (`GET /moto-api/` is 200), for at most
    /// [`START_TIMEOUT`]; it is stopped when it does not.
    pub async fn ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                self.child = None;
                bail!(
                    "moto exited ({status}) before answering on 127.0.0.1:{}; see {}",
                    self.port,
                    self.log.display()
                );
            }
            if let Some((200, _)) = probe(&self.upstream, "/moto-api/", None).await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.stop();
                bail!(
                    "moto did not answer on 127.0.0.1:{} within {} s; see {}",
                    self.port,
                    START_TIMEOUT.as_secs(),
                    self.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop moto and everything it started: `SIGTERM` to its process group,
    /// then `SIGKILL` after [`STOP_GRACE`]. Safe to call more than once.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        crate::signal_process_group(pid, "TERM");
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                // The leader is gone; make sure nothing it started lingers.
                crate::signal_process_group(pid, "KILL");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        crate::signal_process_group(pid, "KILL");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The process id of moto (the process group leader), while it runs.
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        self.stop();
    }
}

fn free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("find a free loopback port")?;
    Ok(listener.local_addr().context("read the bound port")?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain HTTP server that answers every request with `reply`.
    async fn serve(reply: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        port
    }

    fn http(port: u16) -> Upstream {
        Upstream::parse(&format!("http://127.0.0.1:{port}")).unwrap()
    }

    #[tokio::test]
    async fn an_open_port_is_not_an_emulator() {
        let airplay = serve(
            "HTTP/1.1 403 Forbidden\r\nServer: AirTunes/770.8.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(identify(&http(airplay), None).await, None);
        let web = serve(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .await;
        assert_eq!(identify(&http(web), None).await, None);
        let silent = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            std::mem::forget(listener);
            port
        };
        let started = Instant::now();
        assert_eq!(identify(&http(silent), None).await, None);
        assert!(started.elapsed() < Duration::from_secs(10));
        // HTTPS without AWS Emulator's CA cannot be verified.
        let tls = Upstream::parse(&format!("https://127.0.0.1:{web}")).unwrap();
        assert_eq!(identify(&tls, None).await, None);
    }

    #[tokio::test]
    async fn recognizes_localstack_and_aws_shaped_answers() {
        let localstack = serve(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: close\r\n\r\n{\"services\": {\"s3\": \"available\"}}",
        )
        .await;
        assert_eq!(
            identify(&http(localstack), None).await,
            Some(Some(Kind::LocalStack))
        );
        let generic = serve(
            "HTTP/1.1 400 Bad Request\r\nx-amzn-RequestId: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(identify(&http(generic), None).await, Some(None));
        let found = Found {
            upstream: http(generic),
            kind: None,
            source: "AWS_ENDPOINT_URL",
        };
        assert_eq!(found.label(), format!("http://127.0.0.1:{generic}"));
    }

    #[test]
    fn discovery_order_starts_with_a_loopback_endpoint_url() {
        let order: Vec<String> = candidates(Some("http://localhost:4510"))
            .into_iter()
            .map(|(upstream, _)| upstream.base)
            .collect();
        assert_eq!(
            order,
            [
                "http://localhost:4510",
                "https://127.0.0.1:9298",
                "http://127.0.0.1:4566",
                "http://127.0.0.1:5000"
            ]
        );
        // A non-loopback AWS_ENDPOINT_URL is never used.
        assert_eq!(candidates(Some("http://10.0.0.5:4566")).len(), 3);
    }

    /// Managed moto: starts on a free port, answers, records to its log, and
    /// is gone (with its process group) after `stop`. Skipped, with a note,
    /// when moto is not installed.
    #[tokio::test]
    async fn managed_moto_starts_answers_and_stops() {
        let Some(command) = moto_command() else {
            eprintln!("note: moto is not installed; skipping the managed moto test");
            return;
        };
        let dir = std::env::temp_dir().join(format!("lenslet-moto-{}", std::process::id()));
        let log = dir.join("run.emulator.log");
        let mut moto = Managed::start(&command, &log).await.expect("start moto");
        assert_eq!(identify(&moto.upstream, None).await, Some(Some(Kind::Moto)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&log).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let pid = moto.pid().unwrap();
        let upstream = moto.upstream.clone();
        let started = Instant::now();
        moto.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(identify(&upstream, None).await, None, "moto still answers");
        #[cfg(target_os = "linux")]
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists()
                || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|stat| stat.contains(") Z ")),
            "moto (pid {pid}) is still running"
        );
        let _ = pid;
        let _ = std::fs::remove_dir_all(dir);
    }
}
