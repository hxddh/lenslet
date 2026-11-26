//! A body under its wire encodings: `aws-chunked` framing (the SDKs' streaming
//! uploads, with their trailing checksum headers) and `gzip`. What a call
//! says is read from the payload these leave, never from the frames.

use std::{borrow::Cow, io::Read};

use http::HeaderMap;

/// The most a compressed body is inflated to; past it, it stays as sent.
const MAX_INFLATED: u64 = 64 * 1024 * 1024;

/// A body with its wire encodings undone.
pub struct Decoded<'a> {
    pub bytes: Cow<'a, [u8]>,
    /// The encodings undone, outermost first (`aws-chunked`, `gzip`).
    pub encoding: Vec<&'static str>,
    /// `bytes` is only the first part of the payload (see [`decode_within`]).
    pub truncated: bool,
}

/// `body` without the encodings `headers` say it has: the whole payload. An
/// encoding that does not undo cleanly, or inflates past [`MAX_INFLATED`],
/// is left in place.
pub fn decode<'a>(headers: &HeaderMap, body: &'a [u8]) -> Decoded<'a> {
    let mut decoded = undo(headers, body, MAX_INFLATED);
    if decoded.truncated {
        decoded = Decoded {
            bytes: Cow::Borrowed(body),
            encoding: Vec::new(),
            truncated: false,
        };
    }
    decoded
}

/// `body` without its encodings, as [`decode`], but never more than `max`
/// bytes of payload: what is read of a call stays bounded, however far a
/// small compressed body inflates. A payload cut short is `truncated`.
pub fn decode_within<'a>(headers: &HeaderMap, body: &'a [u8], max: usize) -> Decoded<'a> {
    let mut decoded = undo(headers, body, (max as u64).min(MAX_INFLATED));
    if decoded.bytes.len() > max {
        decoded.bytes = Cow::Owned(decoded.bytes[..max].to_vec());
        decoded.truncated = true;
    }
    decoded
}

/// Undo the encodings, inflating to at most `limit` bytes (a payload past
/// it is cut there, and `truncated`).
fn undo<'a>(headers: &HeaderMap, body: &'a [u8], limit: u64) -> Decoded<'a> {
    let encodings: Vec<String> = headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|e| e.trim().to_ascii_lowercase())
        .collect();
    let streaming = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("STREAMING-"));
    let mut decoded = Decoded {
        bytes: Cow::Borrowed(body),
        encoding: Vec::new(),
        truncated: false,
    };
    if (streaming || encodings.iter().any(|e| e == "aws-chunked"))
        && let Some(payload) = unchunk(&decoded.bytes)
    {
        decoded.bytes = Cow::Owned(payload);
        decoded.encoding.push("aws-chunked");
    }
    if encodings.iter().any(|e| e == "gzip" || e == "x-gzip")
        && let Some((payload, whole)) = inflate(&decoded.bytes, limit)
    {
        decoded.bytes = Cow::Owned(payload);
        decoded.encoding.push("gzip");
        decoded.truncated = !whole;
    }
    decoded
}

/// The payload of an `aws-chunked` body: `<hex size>[;chunk-signature=…]\r\n`
/// and the data, chunk after chunk, to a chunk of size 0 and the trailing
/// headers (`x-amz-checksum-crc32:…`) after it.
pub fn unchunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(body.len());
    let mut rest = body;
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&rest[..end]).ok()?;
        let size = line.split(';').next()?.trim();
        let size = usize::from_str_radix(size, 16).ok()?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Some(payload);
        }
        payload.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size..)?.strip_prefix(b"\r\n")?;
    }
}

/// A gzip body inflated (every member of it), if it is one.
pub fn gunzip(body: &[u8]) -> Option<Vec<u8>> {
    inflate(body, MAX_INFLATED).and_then(|(payload, whole)| whole.then_some(payload))
}

/// A gzip body inflated to at most `limit` bytes, and whether that is all
/// of it. A body cut short stops inflating there.
fn inflate(body: &[u8], limit: u64) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut reader = flate2::read::MultiGzDecoder::new(body).take(limit + 1);
    match reader.read_to_end(&mut out) {
        Ok(_) => {}
        // Cut off inside a member: as much as it holds.
        Err(_) if out.len() as u64 > limit => {}
        Err(_) => return None,
    }
    let whole = out.len() as u64 <= limit;
    out.truncate(limit as usize);
    Some((out, whole))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn unframes_aws_chunked_bodies_with_their_trailers() {
        let signed = b"5;chunk-signature=ab12\r\nhello\r\n6;chunk-signature=cd34\r\n world\r\n0;chunk-signature=ef56\r\n\r\n";
        let map = headers(&[
            ("content-encoding", "aws-chunked"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
        ]);
        let decoded = decode(&map, signed);
        assert_eq!(&*decoded.bytes, b"hello world");
        assert_eq!(decoded.encoding, vec!["aws-chunked"]);

        // Unsigned, with a trailing checksum (the SDKs' default upload).
        let trailer = b"b\r\nhello world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let map = headers(&[
            ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ]);
        assert_eq!(&*decode(&map, trailer).bytes, b"hello world");

        // Not framed after all: left as it is.
        let map = headers(&[("content-encoding", "aws-chunked")]);
        let decoded = decode(&map, b"{\"a\":1}");
        assert_eq!(&*decoded.bytes, b"{\"a\":1}");
        assert!(decoded.encoding.is_empty());
    }

    #[test]
    fn inflates_gzip_bodies_inside_the_framing() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"Action=SendMessage&QueueUrl=q").unwrap();
        let gz = gz.finish().unwrap();
        let map = headers(&[("content-encoding", "gzip")]);
        assert_eq!(&*decode(&map, &gz).bytes, b"Action=SendMessage&QueueUrl=q");

        let mut framed = format!("{:x}\r\n", gz.len()).into_bytes();
        framed.extend_from_slice(&gz);
        framed.extend_from_slice(b"\r\n0\r\n\r\n");
        let map = headers(&[("content-encoding", "aws-chunked, gzip")]);
        let decoded = decode(&map, &framed);
        assert_eq!(&*decoded.bytes, b"Action=SendMessage&QueueUrl=q");
        assert_eq!(decoded.encoding, vec!["aws-chunked", "gzip"]);

        // Said to be gzip, but is not.
        assert_eq!(&*decode(&map, b"plain").bytes, b"plain");
    }

    #[test]
    fn reads_no_more_of_a_body_than_it_may() {
        let big = vec![b'a'; 1_000_000];
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&big).unwrap();
        let gz = gz.finish().unwrap();
        let map = headers(&[("content-encoding", "gzip")]);
        let decoded = decode_within(&map, &gz, 1000);
        assert_eq!(decoded.bytes.len(), 1000);
        assert!(decoded.truncated);
        assert_eq!(decoded.encoding, vec!["gzip"]);
        // All of it fits: whole, as `decode` reads it.
        let whole = decode_within(&map, &gz, 2_000_000);
        assert!(!whole.truncated);
        assert_eq!(whole.bytes.len(), big.len());
        assert_eq!(decode(&map, &gz).bytes.len(), big.len());
        // A plain body past the bound is cut too.
        let plain = decode_within(&HeaderMap::new(), &big, 10);
        assert!(plain.truncated && plain.bytes.len() == 10);
    }
}
