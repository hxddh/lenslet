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
}

/// `body` without the encodings `headers` say it has. An encoding that does
/// not undo cleanly is left in place.
pub fn decode<'a>(headers: &HeaderMap, body: &'a [u8]) -> Decoded<'a> {
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
    };
    if (streaming || encodings.iter().any(|e| e == "aws-chunked"))
        && let Some(payload) = unchunk(&decoded.bytes)
    {
        decoded.bytes = Cow::Owned(payload);
        decoded.encoding.push("aws-chunked");
    }
    if encodings.iter().any(|e| e == "gzip" || e == "x-gzip")
        && let Some(payload) = gunzip(&decoded.bytes)
    {
        decoded.bytes = Cow::Owned(payload);
        decoded.encoding.push("gzip");
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
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(body)
        .take(MAX_INFLATED + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() as u64 <= MAX_INFLATED).then_some(out)
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
}
