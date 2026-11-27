//! Building an account again from the calls that made it, on an engine of
//! its own: each call sent again in order, with the ids the first account
//! made up swapped for the new account's, at the pace what it set off needs,
//! and Lenslet's grants sent with their roles.

use crate::{emulator::Emulator, session::Sent};

/// What a rewind sent again: the calls that made the account (Lenslet's own
/// grants, sent with their roles, are not among them), and how many failed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Replayed {
    pub sent: usize,
    pub failed: usize,
}

/// Why a call sent again failed, and whether to send it once more.
#[derive(PartialEq)]
enum Retry {
    /// What it acts on is busy (a function being created): it will be ready.
    Busy,
    /// What it names is not there yet: it may be on its way (a table being
    /// created), unless it was a handle the first account made up.
    Missing,
    /// Any other error is final.
    Never,
}

fn retry(status: u16, code: &str) -> Retry {
    let has = |words: &[&str]| words.iter().any(|w| code.contains(w));
    if matches!(status, 409 | 429 | 503)
        || has(&[
            "InUse",
            "Conflict",
            "Pending",
            "NotReady",
            "Throttl",
            "TooManyRequests",
        ])
    {
        return Retry::Busy;
    }
    // A handle the first account made up (a message's receipt, an upload's
    // id) names nothing in the new one, however long it waits.
    if has(&["ReceiptHandle", "NoSuchUpload", "Invalid"]) {
        return Retry::Never;
    }
    if has(&["NotFound", "NoSuch", "NonExistent", "DoesNotExist"]) {
        return Retry::Missing;
    }
    Retry::Never
}

/// How long a call sent again waits for what it acts on to be ready.
const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long it waits for what it names to appear, once nothing else is
/// going on (the emulator's resources settle in a few seconds).
const APPEAR_WAIT: std::time::Duration = std::time::Duration::from_millis(3500);

/// How far a rebuild is: the calls sent again so far, of how many. `None`
/// while no account is being rebuilt.
pub type Progress = std::sync::Arc<std::sync::Mutex<Option<(usize, usize)>>>;

/// The longest a call waits for what the one before it set off (a delivery,
/// a function's run) to finish before it is sent.
const MAX_PACE: std::time::Duration = std::time::Duration::from_secs(3);
/// What set off is over once AWS has done nothing for this long.
const QUIET: std::time::Duration = std::time::Duration::from_millis(150);

/// Build an account from `log` on a new engine of its own, not yet serving:
/// the engine, and what sending the calls again did. `progress` says how far
/// it is while it builds.
pub(crate) async fn rebuild(
    emulator: &Emulator,
    log: &[Sent],
    progress: &Progress,
) -> Result<(Emulator, Replayed), String> {
    let router = tokio::task::spawn_blocking(Emulator::build)
        .await
        .map_err(|_| "could not start a new engine".to_string())?;
    let built = emulator.offline(router);
    let rebuild = crate::observe::Rebuild::start(built.identity());
    let total = log.iter().filter(|s| s.granted().is_none()).count();
    set(progress, Some((0, total)));
    let replayed = send_again(&built, &rebuild, log, progress).await;
    set(progress, None);
    Ok((built, replayed))
}

fn progress_total(progress: &Progress) -> usize {
    progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map_or(0, |(_, total)| total)
}

fn set(progress: &Progress, to: Option<(usize, usize)>) {
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = to;
}


/// The ids the first account made up, and the new account's for them.
#[derive(Default)]
struct Issued(Vec<(String, String)>);

impl Issued {
    /// `text` naming the new ids, if it named any of the old ones.
    fn swap(&self, text: &str) -> Option<String> {
        self.0
            .iter()
            .any(|(old, _)| text.contains(old.as_str()))
            .then(|| {
                self.0.iter().fold(text.to_string(), |t, (old, new)| {
                    t.replace(old.as_str(), new)
                })
            })
    }

    /// `sent` naming the new account's ids instead of the first one's: in its
    /// address, and in its body as its protocol writes it.
    fn apply<'a>(&self, sent: &'a Sent) -> std::borrow::Cow<'a, Sent> {
        if self.0.is_empty() {
            return std::borrow::Cow::Borrowed(sent);
        }
        let uri = self.uri(&sent.uri);
        let body = self.body(sent);
        if uri.is_none() && body.is_none() {
            return std::borrow::Cow::Borrowed(sent);
        }
        let mut sent = sent.clone();
        if let Some(uri) = uri {
            sent.uri = uri;
        }
        if let Some(body) = body {
            plain(&mut sent.headers, body.len());
            sent.body = bytes::Bytes::from(body);
        }
        std::borrow::Cow::Owned(sent)
    }

    /// The address with the new ids, each part of it read percent-decoded
    /// (a receipt handle's `+`, `/` and `=` travel as `%2B`, `%2F`, `%3D`).
    fn uri(&self, uri: &http::Uri) -> Option<http::Uri> {
        let mut changed = false;
        let mut swap = |part: &str| match self.swap(&lenslet_core::wire::percent_decode_path(part))
        {
            Some(new) => {
                changed = true;
                encode(&new)
            }
            None => part.to_string(),
        };
        let path: Vec<String> = uri.path().split('/').map(&mut swap).collect();
        let mut local = path.join("/");
        if let Some(query) = uri.query() {
            let pairs: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, value)) => format!("{key}={}", swap(value)),
                    None => pair.to_string(),
                })
                .collect();
            local = format!("{local}?{}", pairs.join("&"));
        }
        changed.then(|| local.parse().ok()).flatten()
    }

    /// The body with the new ids, unframed and inflated: CBOR rewritten item
    /// for item, a Query form field by field, JSON and XML as text. An S3
    /// body is an object's data (S3's ids are in the address): never touched.
    fn body(&self, sent: &Sent) -> Option<Vec<u8>> {
        if sent.body.is_empty() {
            return None;
        }
        let path = sent.uri.path_and_query().map_or("/", |p| p.as_str());
        if lenslet_core::wire::wire_info(&sent.headers, path).service == "s3" {
            return None;
        }
        let payload = lenslet_core::body::decode(&sent.headers, &sent.body).bytes;
        let kind = sent
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if kind.contains("cbor") {
            let out = lenslet_core::cbor::map_text(&payload, &|text| self.swap(text))?;
            return (out != *payload).then_some(out);
        }
        let text = std::str::from_utf8(&payload).ok()?;
        if kind.starts_with("application/x-www-form-urlencoded")
            || lenslet_core::wire::form_action(&payload).is_some()
        {
            let mut changed = false;
            let form: Vec<String> = text
                .split('&')
                .map(|pair| {
                    let swapped = pair.split_once('=').and_then(|(key, value)| {
                        let new = self.swap(&lenslet_core::wire::percent_decode_query(value))?;
                        Some(format!("{key}={}", encode(&new)))
                    });
                    changed |= swapped.is_some();
                    swapped.unwrap_or_else(|| pair.to_string())
                })
                .collect();
            return changed.then(|| form.join("&").into_bytes());
        }
        self.swap(text).map(String::into_bytes)
    }
}

/// Percent-encoding for a URI part or a form value: all but the characters
/// that never need it.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The headers of a request whose body is now its plain payload of `len`
/// bytes: no framing or compression said of it, no checksum of what it was.
fn plain(headers: &mut http::HeaderMap, len: usize) {
    let kept: Vec<String> = headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|e| e.trim().to_string())
        .filter(|e| {
            !["aws-chunked", "gzip", "x-gzip", ""].contains(&e.to_ascii_lowercase().as_str())
        })
        .collect();
    headers.remove(http::header::CONTENT_ENCODING);
    if !kept.is_empty()
        && let Ok(value) = http::HeaderValue::from_str(&kept.join(", "))
    {
        headers.insert(http::header::CONTENT_ENCODING, value);
    }
    if headers
        .get("x-amz-content-sha256")
        .is_some_and(|v| v.as_bytes().starts_with(b"STREAMING-"))
    {
        headers.insert(
            "x-amz-content-sha256",
            http::HeaderValue::from_static("UNSIGNED-PAYLOAD"),
        );
    }
    let stale: Vec<http::HeaderName> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with("x-amz-checksum-")
                || matches!(
                    name,
                    "x-amz-decoded-content-length"
                        | "x-amz-trailer"
                        | "x-amz-sdk-checksum-algorithm"
                        | "content-md5"
                )
        })
        .cloned()
        .collect();
    for name in stale {
        headers.remove(name);
    }
    headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(len));
}

/// The ids a call's answer made up, in order: the ones each kind of call is
/// known to make up (an S3 multipart upload's, the receipt handles of the
/// messages an SQS receive took), and every UUID in the answer the request
/// did not hold (a KMS key's, an event source mapping's, a subscription's, an
/// execution's): what later calls name, and what the same call sent again
/// makes up anew, in the same places.
pub fn issued_ids(operation: &str, request: &[u8], response: &[u8]) -> Vec<String> {
    let text = match std::str::from_utf8(response) {
        Ok(text) => std::borrow::Cow::Borrowed(text),
        Err(_) => match lenslet_core::cbor::decode_cbor_json(response) {
            Some(json) => std::borrow::Cow::Owned(json),
            None => return Vec::new(),
        },
    };
    let tags = |tag: &str| -> Vec<String> {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        text.split(open.as_str())
            .skip(1)
            .filter_map(|rest| rest.split_once(close.as_str()).map(|(id, _)| id.to_string()))
            .collect()
    };
    let json = || serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
    let mut ids = match operation {
        "CreateMultipartUpload" => tags("UploadId"),
        "ReceiveMessage" if text.starts_with('<') => tags("ReceiptHandle"),
        "ReceiveMessage" => json()["Messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["ReceiptHandle"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    // A message's own content is the code's, not made up: only the kinds above.
    if operation != "ReceiveMessage" {
        let asked = String::from_utf8_lossy(request);
        // The request's own id is in some answers (the Query protocol's
        // `ResponseMetadata`): never named again.
        let answer = match (text.find("<ResponseMetadata>"), text.find("</ResponseMetadata>")) {
            (Some(a), Some(b)) if a < b => format!("{}{}", &text[..a], &text[b..]),
            _ => text.to_string(),
        };
        for id in uuids(&answer) {
            if !asked.contains(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    // Short ids would match more than themselves.
    ids.into_iter().filter(|id| id.len() >= 8).collect()
}

/// The UUIDs (`8-4-4-4-12` hex digits) in `text`, in order.
fn uuids(text: &str) -> Vec<String> {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = text.as_bytes();
    let hex = |b: u8| b.is_ascii_hexdigit();
    let mut found = Vec::new();
    let mut at = 0;
    while at + 36 <= bytes.len() {
        let candidate = &bytes[at..at + 36];
        let mut pos = 0;
        let shaped = SHAPE.iter().enumerate().all(|(i, len)| {
            let part = candidate[pos..pos + len].iter().all(|b| hex(*b));
            pos += len;
            let dash = i == 4 || candidate.get(pos) == Some(&b'-');
            pos += 1;
            part && dash
        });
        let bounded =
            (at == 0 || !hex(bytes[at - 1])) && bytes.get(at + 36).is_none_or(|b| !hex(*b));
        if shaped && bounded {
            found.push(text[at..at + 36].to_string());
            at += 36;
        } else {
            at += 1;
        }
    }
    found
}

/// Send `log` to the engine being built, in order, each call once what the
/// one before set off is over, then let what they set off finish.
async fn send_again(
    emulator: &Emulator,
    rebuild: &crate::observe::Rebuild,
    log: &[Sent],
    progress: &Progress,
) -> Replayed {
    let mut replayed = Replayed::default();
    let mut issued = Issued::default();
    // What Lenslet allowed roles, and the roles the new account has: a grant
    // is sent once its role exists, and once more each time it is created.
    let mut standing: Vec<((String, String), &Sent)> = Vec::new();
    let mut roles = std::collections::HashSet::new();
    let mut previous = None;
    for sent in log {
        if let Some(key) = sent.granted() {
            standing.retain(|(k, _)| *k != key);
            if roles.contains(&key.0) {
                let _ = send_one(emulator, rebuild, sent).await;
            }
            standing.push((key, sent));
            continue;
        }
        // Not the session's pace (the time the code's author took between
        // commands): only until what the call before set off (a delivery, a
        // function's run) is over, and never longer than that call waited.
        if let Some(previous) = previous.replace(sent.at_ms) {
            let gap = std::time::Duration::from_millis(sent.at_ms.saturating_sub(previous));
            let deadline = std::time::Instant::now() + gap.min(MAX_PACE);
            while !rebuild.settled(QUIET.as_millis() as u64) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        replayed.sent += 1;
        set(progress, Some((replayed.sent, progress_total(progress))));
        let sent = issued.apply(sent);
        let Some(response) = send_one(emulator, rebuild, &sent).await else {
            replayed.failed += 1;
            continue;
        };
        if !sent.issued.is_empty() {
            let (_, operation) =
                crate::server::operation(&sent.method, &sent.uri, &sent.headers, &sent.body);
            let new = issued_ids(&operation, &sent.body, response.body());
            issued.0.extend(sent.issued.iter().cloned().zip(new));
        }
        if let Some(role) = sent.created_role() {
            for (_, grant) in standing.iter().filter(|((r, _), _)| *r == role) {
                let _ = send_one(emulator, rebuild, grant).await;
            }
            roles.insert(role);
        } else if let Some(role) = sent.deleted_role() {
            roles.remove(&role);
        } else if let Some(key) = sent.deleted_policy() {
            standing.retain(|(k, _)| *k != key);
        }
    }
    // Let what the calls set off (deliveries, functions) finish: until AWS
    // has been quiet a moment, for at most a while.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while !rebuild.settled(1500) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    replayed
}

/// Send one call again: its answer when it succeeds. Each call succeeded the
/// first time; sent again sooner than it was, it can find what it acts on
/// not ready yet, and waits for it as the code did.
async fn send_one(
    emulator: &Emulator,
    rebuild: &crate::observe::Rebuild,
    sent: &Sent,
) -> Option<http::Response<bytes::Bytes>> {
    let started = std::time::Instant::now();
    loop {
        let id = rebuild.call();
        let mut request = sent.request();
        // What the call sets off is told apart by this call's tracing header.
        if let Ok(value) = http::HeaderValue::from_str(&crate::observe::trace_header(id)) {
            request.headers_mut().insert("x-amzn-trace-id", value);
        }
        crate::observe::chain_started(request.headers(), id);
        let (response, ()) = emulator.handle_traced(request, id, || ()).await;
        let status = response.status().as_u16();
        if status < 400 {
            return Some(response);
        }
        let headers = lenslet_core::trace::headers(response.headers());
        let text = String::from_utf8_lossy(&response.body()[..response.body().len().min(4096)]);
        let code = lenslet_core::problem::from_response(status, &headers, Some(&text))
            .map(|p| p.code)
            .unwrap_or_default();
        let waited = started.elapsed();
        // A grant to a role not there yet is sent again with the role.
        let again = sent.grant.is_none()
            && match retry(status, &code) {
                Retry::Busy => waited < READY_WAIT,
                // Something still under way may be making it; else it has a few
                // seconds to appear.
                Retry::Missing => {
                    waited < READY_WAIT && (!rebuild.settled(500) || waited < APPEAR_WAIT)
                }
                Retry::Never => false,
            };
        if !again {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_only_for_what_can_become_ready() {
        assert!(retry(409, "ResourceConflictException") == Retry::Busy);
        assert!(retry(400, "ResourceInUseException") == Retry::Busy);
        assert!(retry(400, "ResourceNotFoundException") == Retry::Missing);
        assert!(retry(404, "NoSuchBucket") == Retry::Missing);
        // A handle the first account made up never comes.
        assert!(retry(404, "ReceiptHandleIsInvalid") == Retry::Never);
        assert!(retry(404, "NoSuchUpload") == Retry::Never);
        assert!(retry(400, "ValidationException") == Retry::Never);
    }

    #[test]
    fn later_calls_name_the_new_accounts_ids() {
        let sent = |uri: &str, body: &'static str| Sent {
            at_ms: 0,
            method: http::Method::PUT,
            uri: uri.parse().unwrap(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from_static(body.as_bytes()),
            issued: Vec::new(),
            grant: None,
        };
        assert_eq!(
            issued_ids(
                "CreateMultipartUpload",
                b"",
                b"<InitiateMultipartUploadResult><UploadId>old-upload-1</UploadId></InitiateMultipartUploadResult>"
            ),
            vec!["old-upload-1"]
        );
        assert_eq!(
            issued_ids(
                "ReceiveMessage",
                b"",
                br#"{"Messages":[{"ReceiptHandle":"handle-one-1"},{"ReceiptHandle":"handle-two-2"}]}"#
            ),
            vec!["handle-one-1", "handle-two-2"]
        );
        assert!(issued_ids("ReceiveMessage", b"", b"{}").is_empty());
        // Any UUID the answer made up, not the ones the request named, nor
        // the request's own id in the Query protocol's metadata.
        let key = "1234abcd-12ab-34cd-56ef-1234567890ab";
        let named = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let answer = format!(
            r#"<R><SubscriptionArn>arn:aws:sns:us-east-1:0:t:{key}</SubscriptionArn><Of>{named}</Of><ResponseMetadata><RequestId>99999999-8888-7777-6666-555555555555</RequestId></ResponseMetadata></R>"#
        );
        assert_eq!(
            issued_ids("Subscribe", format!("Endpoint={named}").as_bytes(), answer.as_bytes()),
            vec![key]
        );
        assert!(uuids("x1234abcd-12ab-34cd-56ef-1234567890abz").len() == 1);
        assert!(uuids("01234abcd-12ab-34cd-56ef-1234567890ab").is_empty());
        let issued = Issued(vec![("old-upload-1".into(), "new-upload-9".into())]);
        let part = sent("/b/k?partNumber=1&uploadId=old-upload-1", "data");
        assert_eq!(
            issued.apply(&part).uri.to_string(),
            "/b/k?partNumber=1&uploadId=new-upload-9"
        );
        let key = sent("/", r#"{"KeyId":"old-upload-1"}"#);
        assert_eq!(&issued.apply(&key).body[..], br#"{"KeyId":"new-upload-9"}"#);
        let other = sent("/b/other", "x");
        assert!(matches!(
            issued.apply(&other),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// A message's receipt handle, with the characters encodings change.
    const OLD: &str = "AQEB+old/handle==";
    const NEW: &str = "AQEB+new/handle/9=";

    fn request(headers: &[(&'static str, &str)], uri: &str, body: Vec<u8>) -> Sent {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.append(*name, value.parse().unwrap());
        }
        Sent {
            at_ms: 0,
            method: http::Method::POST,
            uri: uri.parse().unwrap(),
            headers: map,
            body: bytes::Bytes::from(body),
            issued: Vec::new(),
            grant: None,
        }
    }

    fn scope(service: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential=k/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
        )
    }

    #[test]
    fn new_ids_are_written_as_each_encoding_writes_them() {
        let issued = Issued(vec![(OLD.into(), NEW.into())]);
        let sqs = scope("sqs");

        // Smithy RPCv2 CBOR: valid CBOR, the new handle at its own length.
        let mut cbor = vec![0xa2, 0x68];
        cbor.extend_from_slice(b"QueueUrl");
        cbor.extend_from_slice(&[0x61, b'q', 0x6d]);
        cbor.extend_from_slice(b"ReceiptHandle");
        cbor.push(0x60 | OLD.len() as u8);
        cbor.extend_from_slice(OLD.as_bytes());
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/cbor"),
                ("smithy-protocol", "rpc-v2-cbor"),
            ],
            "/service/AmazonSQS/operation/DeleteMessage",
            cbor,
        );
        let swapped = issued.apply(&sent);
        let value = lenslet_core::cbor::decode_cbor(&swapped.body).expect("valid CBOR");
        assert_eq!(value["ReceiptHandle"], NEW);
        assert_eq!(value["QueueUrl"], "q");

        // A gzip'd Query form, its handle percent-encoded: sent plain.
        let form = format!(
            "Action=DeleteMessage&QueueUrl=q&ReceiptHandle={}",
            encode(OLD)
        );
        assert!(form.contains("%2Bold%2Fhandle%3D%3D"));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, form.as_bytes()).unwrap();
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/x-www-form-urlencoded"),
                ("content-encoding", "gzip"),
                ("content-length", "999"),
            ],
            "/",
            gz.finish().unwrap(),
        );
        let swapped = issued.apply(&sent);
        let body = std::str::from_utf8(&swapped.body).unwrap();
        assert_eq!(
            lenslet_core::wire::form_field(body, "ReceiptHandle").as_deref(),
            Some(NEW)
        );
        assert!(body.starts_with("Action=DeleteMessage&QueueUrl=q&"));
        assert!(!swapped.headers.contains_key("content-encoding"));
        assert_eq!(swapped.headers["content-length"], body.len().to_string());

        // aws-chunked JSON with a trailing checksum: unframed, its framing
        // and checksum headers gone.
        let json = format!(r#"{{"QueueUrl":"q","ReceiptHandle":"{OLD}"}}"#);
        let framed = format!(
            "{:x}\r\n{json}\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n",
            json.len()
        );
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/x-amz-json-1.0"),
                ("content-encoding", "aws-chunked"),
                ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
                ("x-amz-decoded-content-length", &json.len().to_string()),
                ("x-amz-trailer", "x-amz-checksum-crc32"),
            ],
            "/",
            framed.into_bytes(),
        );
        let swapped = issued.apply(&sent);
        assert_eq!(&swapped.body[..], json.replace(OLD, NEW).as_bytes());
        for gone in [
            "content-encoding",
            "x-amz-decoded-content-length",
            "x-amz-trailer",
        ] {
            assert!(!swapped.headers.contains_key(gone), "{gone}");
        }
        assert_eq!(swapped.headers["x-amz-content-sha256"], "UNSIGNED-PAYLOAD");

        // In the address, percent-encoded; an object's data is never changed.
        let sent = request(
            &[("authorization", &scope("s3"))],
            &format!("/b/k?partNumber=1&uploadId={}", encode(OLD)),
            format!("part data naming {OLD}").into_bytes(),
        );
        let swapped = issued.apply(&sent);
        assert_eq!(
            lenslet_core::wire::query_param(&swapped.uri.to_string(), "uploadId").as_deref(),
            Some(NEW)
        );
        assert_eq!(swapped.body, sent.body);
    }
}
