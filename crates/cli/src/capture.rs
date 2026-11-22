//! Bounded, content-type- and wire-encoding-aware body capture.
//!
//! [`TeeBody`] wraps a body that is being forwarded and records a bounded
//! prefix plus a SHA-256 of every wire byte as it streams through.
//! Forwarding stays byte-exact (so SigV4 signatures remain valid) and has no
//! size limit. The captured prefix is unframed (`aws-chunked`), decompressed
//! (`gzip`) and, for Smithy RPCv2, decoded from CBOR to JSON, so access
//! derivation reads every protocol the same way.

use std::{
    borrow::Cow,
    io::Read,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use flate2::read::GzDecoder;
use hyper::{
    HeaderMap,
    body::{Body, Frame, SizeHint},
};
use sha2::{Digest, Sha256};

use crate::trace::BodyTrace;

/// The classified `content-type` and wire encoding of a message.
#[derive(Clone, Debug)]
pub struct Content {
    /// Media type without parameters, e.g. `application/xml`.
    pub content_type: Option<String>,
    /// A known text format with a decodable encoding.
    pub text: bool,
    /// Untyped or `octet-stream`: shown as text when it is clean UTF-8.
    pub sniff_utf8: bool,
    /// Framed with AWS SigV4 `aws-chunked` chunks.
    pub aws_chunked: bool,
    /// Smithy RPCv2 CBOR (`application/cbor`).
    pub cbor: bool,
    /// Gzip-compressed (after unframing `aws-chunked`).
    pub gzip: bool,
    /// The encodings are ones Lenslet can decode.
    pub decodable: bool,
    /// The decoded encodings, e.g. `aws-chunked,gzip`.
    pub encoding: Option<String>,
}

/// Classify a message's body from its headers.
pub fn content(headers: &HeaderMap) -> Content {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
    };
    let media = header("content-type")
        .and_then(|value| value.split(';').next().map(|m| m.trim().to_string()))
        .filter(|value| !value.is_empty());
    let mut aws_chunked =
        header("x-amz-content-sha256").is_some_and(|v| v.starts_with("streaming-"));
    let mut gzip = false;
    let mut decodable = true;
    for layer in header("content-encoding")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|layer| !layer.is_empty())
    {
        match layer {
            "identity" => {}
            "aws-chunked" => aws_chunked = true,
            "gzip" | "x-gzip" if !gzip => gzip = true,
            _ => decodable = false,
        }
    }
    let cbor = header("smithy-protocol").is_some_and(|v| v == "rpc-v2-cbor")
        || media.as_deref().is_some_and(|m| {
            m == "application/cbor"
                || m.starts_with("application/x-amz-cbor")
                || m.ends_with("+cbor")
        });
    let mut encodings: Vec<&str> = Vec::new();
    if aws_chunked {
        encodings.push("aws-chunked");
    }
    if gzip {
        encodings.push("gzip");
    }
    if cbor {
        encodings.push("cbor");
    }
    let known_text = media.as_deref().is_some_and(is_text) || cbor;
    let untyped = matches!(
        media.as_deref(),
        None | Some("application/octet-stream" | "binary/octet-stream")
    ) && !cbor;
    Content {
        content_type: media,
        text: known_text && decodable,
        sniff_utf8: untyped && decodable,
        aws_chunked,
        cbor,
        gzip,
        decodable,
        encoding: (!encodings.is_empty()).then(|| encodings.join(",")),
    }
}

fn is_text(media: &str) -> bool {
    media.contains("json")
        || media.contains("xml")
        || media == "application/x-www-form-urlencoded"
        || media.starts_with("text/")
        || matches!(
            media,
            "application/javascript" | "application/yaml" | "application/x-yaml"
        )
}

/// A bounded capture of one body.
pub struct Capture {
    prefix: Vec<u8>,
    limit: usize,
    bytes: u64,
    hasher: Sha256,
}

impl Capture {
    /// Keep the first `limit` wire bytes (and hash all of them).
    pub fn new(limit: usize) -> Self {
        Capture {
            prefix: Vec::new(),
            limit,
            bytes: 0,
            hasher: Sha256::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.bytes = self.bytes.saturating_add(data.len() as u64);
        self.hasher.update(data);
        let room = self.limit.saturating_sub(self.prefix.len());
        self.prefix.extend_from_slice(&data[..data.len().min(room)]);
    }

    /// The retained prefix, unframed and decoded, for reading an AWS Query
    /// `Action=` before the body is complete.
    pub fn decoded_prefix(&self, content: &Content) -> Cow<'_, [u8]> {
        if !content.aws_chunked && !content.gzip && !content.cbor {
            return Cow::Borrowed(&self.prefix);
        }
        decode(&self.prefix, content, self.limit)
            .map(|(bytes, _)| Cow::Owned(bytes))
            .unwrap_or(Cow::Borrowed(&[]))
    }

    /// The trace body, or `None` when the body was empty. `sha256` and
    /// `bytes` cover every wire byte, not just the retained prefix.
    pub fn body(&self, content: &Content) -> Option<BodyTrace> {
        if self.bytes == 0 {
            return None;
        }
        let wire_truncated = self.bytes > self.prefix.len() as u64;
        let mut body = BodyTrace {
            text: None,
            encoding: content.encoding.clone(),
            bytes: self.bytes,
            truncated: false,
            binary: true,
            sha256: hex(&self.hasher.clone().finalize()),
        };
        if !content.decodable {
            return Some(body);
        }
        // A CBOR document cut short cannot be decoded at all.
        if content.cbor && wire_truncated {
            body.truncated = true;
            return Some(body);
        }
        let Some((decoded, capped)) = decode(&self.prefix, content, self.limit) else {
            return Some(body);
        };
        let truncated = wire_truncated || capped;
        let text = if content.text {
            Some(lossy_utf8(&decoded, truncated))
        } else if content.sniff_utf8 {
            clean_utf8(&decoded, truncated)
        } else {
            None
        };
        if let Some(text) = text {
            body.text = Some(text);
            body.binary = false;
            body.truncated = truncated;
        }
        Some(body)
    }
}

/// Unframe `aws-chunked`, gunzip, and decode CBOR, keeping at most `limit`
/// bytes; `true` when that cut the payload short.
fn decode(prefix: &[u8], content: &Content, limit: usize) -> Option<(Vec<u8>, bool)> {
    let (unframed, chunk_capped): (Cow<'_, [u8]>, bool) = if content.aws_chunked {
        let (bytes, capped) = decode_aws_chunked(prefix, limit)?;
        (Cow::Owned(bytes), capped)
    } else {
        (Cow::Borrowed(prefix), false)
    };
    let (plain, capped): (Cow<'_, [u8]>, bool) = if content.gzip {
        let (bytes, capped) = gunzip(&unframed, limit)?;
        (Cow::Owned(bytes), chunk_capped || capped)
    } else {
        (unframed, chunk_capped)
    };
    if content.cbor {
        if capped {
            return None;
        }
        return Some((
            lenslet_core::cbor::decode_cbor_json(&plain)?.into_bytes(),
            false,
        ));
    }
    Some((plain.into_owned(), capped))
}

/// Strip `aws-chunked` chunk headers (`<hex-size>[;chunk-signature=...]\r\n
/// <data>\r\n`) and the trailing checksum headers, keeping at most `limit`
/// payload bytes.
fn decode_aws_chunked(input: &[u8], limit: usize) -> Option<(Vec<u8>, bool)> {
    let mut pos = 0usize;
    let mut out = Vec::new();
    while pos < input.len() {
        let rest = &input[pos..];
        let Some(end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return (!out.is_empty()).then_some((out, true));
        };
        let header = &rest[..end];
        let size = header
            .split(|&b| b == b';')
            .next()
            .unwrap_or(header)
            .trim_ascii();
        let size = usize::from_str_radix(std::str::from_utf8(size).ok()?, 16).ok()?;
        pos += end + 2;
        if size == 0 {
            return Some((out, false));
        }
        let available = input.len().saturating_sub(pos).min(size);
        let room = limit.saturating_sub(out.len());
        if available > room {
            out.extend_from_slice(&input[pos..pos + room]);
            return Some((out, true));
        }
        out.extend_from_slice(&input[pos..pos + available]);
        if available < size {
            return Some((out, true));
        }
        pos += size;
        if input.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        }
    }
    Some((out, false))
}

/// Decompress up to `limit` bytes of a gzip stream.
fn gunzip(input: &[u8], limit: usize) -> Option<(Vec<u8>, bool)> {
    if input.is_empty() {
        return Some((Vec::new(), false));
    }
    let mut reader = GzDecoder::new(input);
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut cut = false;
    while out.len() <= limit {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            // A cut or corrupt stream: what was decoded is only a prefix.
            Err(_) => {
                cut = true;
                break;
            }
        }
    }
    if out.is_empty() && cut {
        return None;
    }
    let capped = out.len() > limit || cut;
    out.truncate(limit);
    Some((out, capped))
}

/// Declared text, dropping an incomplete trailing UTF-8 sequence when the
/// bytes were cut mid-character.
fn lossy_utf8(bytes: &[u8], truncated: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(valid) => valid.to_string(),
        Err(err) if truncated && err.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..err.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// An untyped payload, as text, when it is readable UTF-8 (no control bytes
/// other than whitespace).
fn clean_utf8(bytes: &[u8], truncated: bool) -> Option<String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(valid) => valid,
        Err(err) if truncated && err.error_len().is_none() && err.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    let clean = !text
        .bytes()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F));
    clean.then(|| text.to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Wraps a body that is being forwarded and captures it on the way through.
///
/// The wrapper never fails a transfer: a poisoned capture mutex only costs
/// trace detail. `on_end` runs exactly once, when the body ends, errors, or
/// is dropped.
pub struct TeeBody<B> {
    inner: B,
    capture: Arc<Mutex<Capture>>,
    on_end: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<B> TeeBody<B> {
    pub fn new(inner: B, capture: Arc<Mutex<Capture>>) -> Self {
        TeeBody {
            inner,
            capture,
            on_end: None,
        }
    }

    /// Run `hook` once, when the body is finished or dropped.
    pub fn on_end(mut self, hook: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.on_end = Some(Box::new(hook));
        self
    }

    fn end(&mut self) {
        if let Some(hook) = self.on_end.take() {
            hook();
        }
    }
}

impl<B> Drop for TeeBody<B> {
    fn drop(&mut self) {
        self.end();
    }
}

impl<B> Body for TeeBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && let Ok(mut capture) = this.capture.lock()
                {
                    capture.feed(data);
                }
            }
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => this.end(),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use http_body_util::{BodyExt, Full};
    use hyper::header::HeaderValue;
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn capture(limit: usize, data: &[u8], pairs: &[(&str, &str)]) -> BodyTrace {
        let mut capture = Capture::new(limit);
        capture.feed(data);
        capture.body(&content(&headers(pairs))).unwrap()
    }

    #[test]
    fn classifies_content_types_and_encodings() {
        let json = content(&headers(&[("content-type", "application/x-amz-json-1.0")]));
        assert!(json.text && json.decodable && json.encoding.is_none());
        let chunked = content(&headers(&[
            ("content-type", "text/plain"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("content-encoding", "gzip"),
        ]));
        assert!(chunked.aws_chunked && chunked.gzip);
        assert_eq!(chunked.encoding.as_deref(), Some("aws-chunked,gzip"));
        let cbor = content(&headers(&[("smithy-protocol", "rpc-v2-cbor")]));
        assert!(cbor.cbor && cbor.text && !cbor.sniff_utf8);
        let zstd = content(&headers(&[
            ("content-type", "application/json"),
            ("content-encoding", "zstd"),
        ]));
        assert!(!zstd.decodable && !zstd.text, "only gzip is decoded");
        let untyped = content(&HeaderMap::new());
        assert!(untyped.sniff_utf8 && !untyped.text);
    }

    #[test]
    fn keeps_text_verbatim_truncates_long_bodies_and_hashes_everything() {
        let body = capture(
            1024,
            br#"{"TableName":"orders"}"#,
            &[("content-type", "application/json")],
        );
        assert_eq!(body.text.as_deref(), Some(r#"{"TableName":"orders"}"#));
        assert!(!body.binary && !body.truncated);
        assert_eq!(body.bytes, 22);

        let long = "x".repeat(100);
        let body = capture(10, long.as_bytes(), &[("content-type", "text/plain")]);
        assert_eq!(body.text.as_deref(), Some("xxxxxxxxxx"));
        assert!(body.truncated);
        assert_eq!(body.bytes, 100);
        assert_eq!(body.sha256, hex(&Sha256::digest(long.as_bytes())));

        assert!(Capture::new(10).body(&content(&HeaderMap::new())).is_none());
    }

    #[test]
    fn unframes_aws_chunked_gunzips_and_sniffs_untyped_utf8() {
        let wire = b"5;chunk-signature=abc\r\nhello\r\n0;chunk-signature=def\r\nx-amz-checksum-crc32:AAAA\r\n\r\n";
        let body = capture(
            1024,
            wire,
            &[("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")],
        );
        assert_eq!(body.text.as_deref(), Some("hello"));
        assert_eq!(body.encoding.as_deref(), Some("aws-chunked"));

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(br#"{"QueueName":"jobs"}"#).unwrap();
        let body = capture(
            1024,
            &gz.finish().unwrap(),
            &[
                ("content-type", "application/json"),
                ("content-encoding", "gzip"),
            ],
        );
        assert_eq!(body.text.as_deref(), Some(r#"{"QueueName":"jobs"}"#));

        let binary = capture(1024, &[0, 1, 2, 0xff], &[]);
        assert!(binary.binary && binary.text.is_none());
    }

    #[test]
    fn decodes_smithy_rpcv2_cbor_to_json() {
        let mut frame = vec![0xa1, 0x69];
        frame.extend_from_slice(b"TableName");
        frame.push(0x66);
        frame.extend_from_slice(b"orders");
        let body = capture(1024, &frame, &[("content-type", "application/cbor")]);
        assert_eq!(body.text.as_deref(), Some(r#"{"TableName":"orders"}"#));
        assert_eq!(body.encoding.as_deref(), Some("cbor"));
        // Cut short, it cannot be read.
        let cut = capture(4, &frame, &[("content-type", "application/cbor")]);
        assert!(cut.text.is_none() && cut.truncated);
    }

    #[tokio::test]
    async fn tee_body_forwards_every_byte_and_runs_the_hook_once() {
        let capture = Arc::new(Mutex::new(Capture::new(4)));
        let ended = Arc::new(AtomicUsize::new(0));
        let counter = ended.clone();
        let body = TeeBody::new(
            Full::new(Bytes::from_static(b"hello world")),
            capture.clone(),
        )
        .on_end(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(&collected[..], b"hello world");
        assert_eq!(ended.load(Ordering::SeqCst), 1);
        let captured = capture
            .lock()
            .unwrap()
            .body(&content(&HeaderMap::new()))
            .unwrap();
        assert_eq!(captured.bytes, 11);
        assert_eq!(captured.text.as_deref(), Some("hell"));
    }

    #[test]
    fn tee_body_hook_runs_when_the_body_is_dropped_early() {
        let ended = Arc::new(AtomicUsize::new(0));
        let counter = ended.clone();
        let body = TeeBody::new(
            Full::new(Bytes::from_static(b"x")),
            Arc::new(Mutex::new(Capture::new(1))),
        )
        .on_end(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        drop(body);
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }
}
