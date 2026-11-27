//! AWS Emulator, embedded: a fresh in-memory engine behind a router of the
//! services Lenslet serves. Requests are handled in-process; a fresh start
//! swaps in a new engine, which is all a reset is (the emulator keeps its
//! state in memory only).

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use emulator_server::EmulatorService;
use smithy_server_runtime::{MultiServiceRouter, UNSERVED_URL_BASE};

/// Assembling the router needs a deep stack (AWS Emulator's own binary uses
/// the same size); it runs on a thread of its own.
const BUILD_STACK: usize = 64 * 1024 * 1024;

/// The services Lenslet serves, by their signing names, for the window's
/// "not emulated here" answer.
pub const SERVICES: &[&str] = &[
    "s3",
    "dynamodb",
    "sqs",
    "sns",
    "sts",
    "iam",
    "kms",
    "lambda",
    "secretsmanager",
    "ssm",
    "events",
    "logs",
    "states",
    "kinesis",
];

#[derive(Clone)]
pub struct Emulator {
    router: Arc<RwLock<MultiServiceRouter>>,
    endpoint: Arc<str>,
}

impl Emulator {
    /// An emulator whose resources name `endpoint` (queue URLs, presigned URLs).
    pub fn new(endpoint: &str) -> Self {
        Self {
            router: Arc::new(RwLock::new(build())),
            endpoint: endpoint.into(),
        }
    }

    /// A new, empty engine, not yet serving (building it takes a moment).
    pub fn build() -> MultiServiceRouter {
        build()
    }

    /// An engine of its own for `router`, naming the same endpoint: where an
    /// account is built before it is served.
    pub fn offline(&self, router: MultiServiceRouter) -> Emulator {
        Emulator {
            router: Arc::new(RwLock::new(router)),
            endpoint: self.endpoint.clone(),
        }
    }

    /// Which engine this is: what the observer is told each of its calls is
    /// made in.
    pub fn identity(&self) -> u64 {
        self.router
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .identity()
    }

    /// Serve `router` from now on, if `commit` (run while no call can reach
    /// either engine) says so: the account and what is said about it change
    /// over together.
    pub fn swap_with<T>(
        &self,
        router: MultiServiceRouter,
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut serving = self.router.write().unwrap_or_else(|e| e.into_inner());
        let committed = commit()?;
        *serving = router;
        Ok(committed)
    }

    /// Serve the account `offline` holds from now on (see [`Self::swap_with`]).
    pub fn adopt<T>(
        &self,
        offline: &Emulator,
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let router = offline
            .router
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.swap_with(router, commit)
    }

    pub async fn handle(&self, request: http::Request<Bytes>) -> http::Response<Bytes> {
        self.serve(request, None, || ()).await.0
    }

    /// Serve a call from outside with id `id`: the calls it causes inside the
    /// emulator are recorded under it. `pin` is asked as the call takes the
    /// engine, before another can take its place.
    pub async fn handle_traced<T>(
        &self,
        request: http::Request<Bytes>,
        id: u64,
        pin: impl FnOnce() -> T,
    ) -> (http::Response<Bytes>, T) {
        self.serve(request, Some(id), pin).await
    }

    async fn serve<T>(
        &self,
        request: http::Request<Bytes>,
        id: Option<u64>,
        pin: impl FnOnce() -> T,
    ) -> (http::Response<Bytes>, T) {
        let (router, pinned) = {
            let router = self.router.read().unwrap_or_else(|e| e.into_inner());
            (router.clone(), pin())
        };
        let invocation = request.uri().path().ends_with("/invocations");
        let work = async move { router.handle_request(request).await };
        // A panic in a service answers 500 instead of taking the window down.
        let task = match id {
            Some(id) => tokio::spawn(crate::observe::within(id, work)),
            None => tokio::spawn(work),
        };
        let response = match task.await {
            Ok(response) => localize(response, &self.endpoint, invocation),
            Err(_) => http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(Bytes::from_static(
                    br#"{"__type":"InternalFailure","message":"The emulator failed on this request."}"#,
                ))
                .expect("static response"),
        };
        (response, pinned)
    }
}

/// The fields of the emulator's answers that carry an address it made up
/// for a later request: a queue's, an upload's, a function's URL, a
/// presigned download's.
const URL_FIELDS: &[&str] = &[
    "QueueUrl",
    "QueueUrls",
    "Location",
    "FunctionUrl",
    "SubscribeURL",
    "UnsubscribeURL",
    "SigningCertURL",
    "Url",
    "URL",
];

/// The emulator names only `https://` URLs: [`UNSERVED_URL_BASE`] when
/// embedded, or `https://` and the request's host (`127.0.0.1`, `[::1]`,
/// `localhost`, a bucket's `shop.localhost`). Lenslet serves plain HTTP on
/// loopback, so the URLs it hands out are rewritten to name Lenslet: in the
/// `Location` header, and in the fields of its answers that carry one
/// ([`URL_FIELDS`]). Nothing else is touched: what your code stored (an
/// object, an item, a parameter, a secret, a message) and what a function
/// answered (`invocation`) come back as they went in.
fn localize(
    response: http::Response<Bytes>,
    endpoint: &str,
    invocation: bool,
) -> http::Response<Bytes> {
    let (mut parts, body) = response.into_parts();
    let port = endpoint.rsplit(':').next().unwrap_or_default();
    for name in [http::header::LOCATION, http::header::CONTENT_LOCATION] {
        if let Some(local) = parts
            .headers
            .get(&name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| local_url(v, endpoint, port))
            .and_then(|v| http::HeaderValue::from_str(&v).ok())
        {
            parts.headers.insert(name, local);
        }
    }
    // Only service documents, never object data (an object, even one that is
    // JSON or XML, comes with its ETag) or a function's answer.
    let kind = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let document = !invocation && !parts.headers.contains_key(http::header::ETAG);
    let text = std::str::from_utf8(&body).ok().filter(|_| document);
    let local = match text {
        Some(text) if kind.contains("json") => localize_json(text, endpoint, port).map(Bytes::from),
        Some(text) if kind.contains("xml") => localize_xml(text, endpoint, port).map(Bytes::from),
        // Smithy RPCv2 CBOR: the same fields, each string at its new length.
        _ if document && kind.contains("cbor") => {
            lenslet_core::cbor::map_values(&body, &|key, text| {
                key.filter(|k| URL_FIELDS.contains(k))
                    .and_then(|_| local_url(text, endpoint, port))
            })
            .filter(|out| *out != body)
            .map(Bytes::from)
        }
        _ => None,
    };
    let body = match local {
        Some(local) => {
            parts.headers.remove(http::header::CONTENT_LENGTH);
            local
        }
        None => body,
    };
    http::Response::from_parts(parts, body)
}

/// `url` naming Lenslet, when the emulator made it up: [`UNSERVED_URL_BASE`],
/// or `https://` and a loopback host on Lenslet's `port`.
fn local_url(url: &str, endpoint: &str, port: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix(UNSERVED_URL_BASE)
        && (rest.is_empty() || rest.starts_with(['/', '?']))
    {
        return Some(format!("{endpoint}{rest}"));
    }
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let (_, on) = authority.rsplit_once(':')?;
    let host = http::HeaderValue::from_str(authority).ok()?;
    (on == port && crate::server::loopback_host(Some(&host))).then(|| format!("http://{rest}"))
}

fn localize_json(text: &str, endpoint: &str, port: &str) -> Option<String> {
    use serde_json::Value;
    fn walk(value: &mut Value, url: bool, f: &dyn Fn(&str) -> Option<String>) -> bool {
        match value {
            Value::String(s) if url => f(s).map(|local| *s = local).is_some(),
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, v| walk(v, url, f) | changed),
            Value::Object(fields) => fields.iter_mut().fold(false, |changed, (k, v)| {
                walk(v, URL_FIELDS.contains(&k.as_str()), f) | changed
            }),
            _ => false,
        }
    }
    let mut value: Value = serde_json::from_str(text).ok()?;
    walk(&mut value, false, &|s| local_url(s, endpoint, port))
        .then(|| serde_json::to_string(&value).ok())
        .flatten()
}

fn localize_xml(text: &str, endpoint: &str, port: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let field = URL_FIELDS.iter().find(|f| {
            rest[1..]
                .strip_prefix(**f)
                .is_some_and(|r| r.starts_with('>'))
        });
        let close = field.map(|f| format!("</{f}>"));
        let found = field.zip(close.as_ref()).and_then(|(f, close)| {
            let open = f.len() + 2;
            rest[open..].find(close.as_str()).map(|end| (open, end))
        });
        let Some((open, end)) = found else {
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        let value = &rest[open..open + end];
        out.push_str(&rest[..open]);
        match local_url(value, endpoint, port) {
            Some(local) => {
                out.push_str(&local);
                changed = true;
            }
            None => out.push_str(value),
        }
        rest = &rest[open + end..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

fn build() -> MultiServiceRouter {
    std::thread::Builder::new()
        .name("lenslet-emulator-build".into())
        .stack_size(BUILD_STACK)
        .spawn(move || {
            let engine = Arc::new(engine::InMemoryEngine::new());
            let mut router = MultiServiceRouter::new();
            macro_rules! serve {
                ($($krate:ident),* $(,)?) => {
                    $(router.register_service($krate::EmulatedService::new(engine.clone()));)*
                };
            }
            serve!(
                service_com_amazonaws_s3_amazons3,
                service_com_amazonaws_dynamodb_dynamodb_20120810,
                service_com_amazonaws_sqs_amazonsqs,
                service_com_amazonaws_sns_amazonsimplenotificationservice,
                service_com_amazonaws_sts_awssecuritytokenservicev20110615,
                service_com_amazonaws_iam_awsidentitymanagementv20100508,
                service_com_amazonaws_kms_trentservice,
                service_com_amazonaws_lambda_awsgirapiservice,
                service_com_amazonaws_secretsmanager_secretsmanager,
                service_com_amazonaws_ssm_amazonssm,
                service_com_amazonaws_eventbridge_awsevents,
                service_com_amazonaws_cloudwatchlogs_logs_20140328,
                service_com_amazonaws_sfn_awsstepfunctions,
                service_com_amazonaws_kinesis_kinesis_20131202,
            );
            // Any credentials work: Lenslet is local, and keys that change on
            // every fresh start would break the code pointed at it.
            router.set_request_authentication(false);
            router
        })
        .expect("spawn the emulator build thread")
        .join()
        .expect("build the emulator")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(kind: &str, body: &str) -> http::Response<Bytes> {
        http::Response::builder()
            .header("content-type", kind)
            .header("location", "https://shop.localhost:4510/a.txt")
            .body(Bytes::from(body.to_string()))
            .unwrap()
    }

    #[test]
    fn names_lenslet_only_in_the_urls_the_emulator_made_up() {
        let endpoint = "http://127.0.0.1:4510";
        let r = localize(
            response(
                "application/x-amz-json-1.0",
                r#"{"QueueUrl":"https://[::1]:4510/000000000000/jobs","Note":"https://127.0.0.1:4510/x"}"#,
            ),
            endpoint,
            false,
        );
        assert_eq!(r.headers()["location"], "http://shop.localhost:4510/a.txt");
        assert_eq!(
            r.body(),
            r#"{"QueueUrl":"http://[::1]:4510/000000000000/jobs","Note":"https://127.0.0.1:4510/x"}"#
        );
        let r = localize(
            response(
                "text/xml",
                "<R><QueueUrls>https://emulator.invalid/0/q</QueueUrls><QueueUrl>https://localhost:4510/0/q</QueueUrl><Body>https://localhost:4510/0/q</Body></R>",
            ),
            endpoint,
            false,
        );
        assert_eq!(
            r.body(),
            "<R><QueueUrls>http://127.0.0.1:4510/0/q</QueueUrls><QueueUrl>http://localhost:4510/0/q</QueueUrl><Body>https://localhost:4510/0/q</Body></R>"
        );
        // CBOR: the URL rewritten in place, at its new length.
        let url = "https://localhost:4510/000000000000/jobs";
        let mut cbor = vec![0xa1, 0x68];
        cbor.extend_from_slice(b"QueueUrl");
        cbor.extend_from_slice(&[0x78, url.len() as u8]);
        cbor.extend_from_slice(url.as_bytes());
        let r = localize(
            http::Response::builder()
                .header("content-type", "application/cbor")
                .body(Bytes::from(cbor))
                .unwrap(),
            endpoint,
            false,
        );
        assert_eq!(
            lenslet_core::cbor::decode_cbor(r.body()).unwrap()["QueueUrl"],
            "http://localhost:4510/000000000000/jobs"
        );
        // What your code stored or a function answered is left as it is.
        for (kind, body, invocation) in [
            (
                "application/json",
                r#"{"QueueUrl":"https://localhost:4510/0/q"}"#,
                true,
            ),
            (
                "application/x-amz-json-1.1",
                r#"{"Parameter":{"Value":"https://localhost:4510/0/q"}}"#,
                false,
            ),
            (
                "application/x-amz-json-1.0",
                r#"{"Item":{"QueueUrl":{"S":"https://localhost:4510/0/q"}}}"#,
                false,
            ),
            // Another port, or not this machine: not the emulator's.
            (
                "application/json",
                r#"{"Url":"https://localhost:9999/x"}"#,
                false,
            ),
            (
                "application/json",
                r#"{"Url":"https://example.com:4510/x"}"#,
                false,
            ),
        ] {
            let r = localize(response(kind, body), endpoint, invocation);
            assert_eq!(r.body(), body);
        }
    }
}
