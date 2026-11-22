//! Local AWS SigV4 request re-signer for the Edit-and-Replay Request Composer.
//!
//! When a developer edits a captured request's JSON/XML/Query body, URI path, or
//! headers in the Composer, the original `Authorization` signature would fail
//! `SignatureDoesNotMatch` on strict emulators (AWS Emulator, LocalStack Pro).
//! This module computes a fresh RFC-compliant `AWS4-HMAC-SHA256` signature
//! locally in-memory using `ring::hmac` and `sha2::Sha256`.

use std::collections::BTreeMap;

use chrono::Utc;
use ring::hmac;
use sha2::{Digest, Sha256};

use crate::model::Header;

pub struct SignParams<'a> {
    pub method: &'a str,
    pub path_and_query: &'a str,
    pub host: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
}

/// Replace `x-amz-date`, `x-amz-content-sha256`, `host`, and `authorization` on
/// `headers` with a freshly signed `AWS4-HMAC-SHA256` header over `body`.
pub fn sign_headers(headers: &mut Vec<Header>, body: &[u8], params: &SignParams<'_>) {
    let now = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    sign_headers_at(headers, body, params, &amz_date, &date_stamp);
}

pub fn sign_headers_at(
    headers: &mut Vec<Header>,
    body: &[u8],
    params: &SignParams<'_>,
    amz_date: &str,
    date_stamp: &str,
) {
    let payload_hash = hex_sha256(body);

    // Remove stale signing / framing headers before re-signing.
    headers.retain(|h| {
        let low = h.name.to_ascii_lowercase();
        !matches!(
            low.as_str(),
            "authorization"
                | "x-amz-date"
                | "date"
                | "x-amz-content-sha256"
                | "content-length"
                | "transfer-encoding"
                | "expect"
        )
    });

    if !headers.iter().any(|h| h.name.eq_ignore_ascii_case("host")) && !params.host.is_empty() {
        headers.insert(
            0,
            Header {
                name: "host".into(),
                value: params.host.to_string(),
            },
        );
    }
    headers.push(Header {
        name: "x-amz-date".into(),
        value: amz_date.to_string(),
    });
    headers.push(Header {
        name: "x-amz-content-sha256".into(),
        value: payload_hash.clone(),
    });
    if let Some(tok) = params.session_token.filter(|s| !s.is_empty())
        && !headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("x-amz-security-token"))
    {
        headers.push(Header {
            name: "x-amz-security-token".into(),
            value: tok.to_string(),
        });
    }

    let (raw_path, raw_query) = params
        .path_and_query
        .split_once('?')
        .unwrap_or((params.path_and_query, ""));
    let names_values: Vec<(&str, &str)> = headers
        .iter()
        .map(|h| (h.name.as_str(), h.value.as_str()))
        .collect();
    let (canonical_request, signed_headers) = canonical_request(
        params.method,
        raw_path,
        raw_query,
        &names_values,
        &payload_hash,
        params.service == "s3",
    );
    let scope = format!(
        "{date_stamp}/{}/{}/aws4_request",
        params.region, params.service
    );
    let sts = string_to_sign(amz_date, &scope, &canonical_request);
    let signature = signature(
        params.secret_key,
        date_stamp,
        params.region,
        params.service,
        &sts,
    );

    headers.push(Header {
        name: "authorization".into(),
        value: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            params.access_key
        ),
    });
}

/// The canonical request and its `SignedHeaders` list. `raw_path` is the path
/// as it goes on the wire (already percent-encoded once).
pub fn canonical_request(
    method: &str,
    raw_path: &str,
    raw_query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    is_s3: bool,
) -> (String, String) {
    let canonical_uri = canonical_uri(raw_path, is_s3);
    let canonical_query = canonicalize_query(raw_query);

    let mut canonical_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        let low = name.trim().to_ascii_lowercase();
        if low.is_empty() || matches!(low.as_str(), "connection" | "user-agent") {
            continue;
        }
        canonical_map
            .entry(low)
            .or_default()
            .push(collapse_ws(value.trim()));
    }
    let mut canonical_headers = String::new();
    let mut signed_names: Vec<&str> = Vec::with_capacity(canonical_map.len());
    for (name, vals) in &canonical_map {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&vals.join(","));
        canonical_headers.push('\n');
        signed_names.push(name.as_str());
    }
    let signed_headers = signed_names.join(";");
    (
        format!(
            "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            method.trim().to_ascii_uppercase()
        ),
        signed_headers,
    )
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    )
}

pub fn signature(
    secret_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    string_to_sign: &str,
) -> String {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&signing_key, msg).as_ref().to_vec()
}

fn hex_sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.push(ch);
        }
    }
    out
}

fn uri_encode(input: &str, encode_slash: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved || (!encode_slash && b == b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The canonical URI for a wire path (already percent-encoded once).
///
/// S3 signs the path as sent: decoded and re-encoded once. Every other
/// service normalizes the path (`.`/`..` segments, repeated slashes) and then
/// encodes the wire path again, so each segment ends up encoded twice.
fn canonical_uri(raw: &str, is_s3: bool) -> String {
    let ensured = if raw.is_empty() { "/" } else { raw };
    if is_s3 {
        return uri_encode(&lenslet_core::wire::percent_decode_path(ensured), false);
    }
    uri_encode(&normalize_path(ensured), false)
}

/// RFC 3986 dot-segment removal plus collapsing empty segments.
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let trailing = path.len() > 1
        && (path.ends_with('/') || path.ends_with("/.") || path.ends_with("/.."))
        && !out.is_empty();
    let mut joined = format!("/{}", out.join("/"));
    if trailing {
        joined.push('/');
    }
    joined
}

/// Percent-encode what may not appear raw in a request line (spaces, non-ASCII,
/// control bytes) while keeping existing `%XX` escapes and reserved characters.
pub fn wire_path(path: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if b.is_ascii_graphic() && b != b'"' && b != b'<' && b != b'>' && b != b'\\' {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn canonicalize_query(raw_query: &str) -> String {
    if raw_query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k_dec = lenslet_core::wire::percent_decode_query(k);
            let v_dec = lenslet_core::wire::percent_decode_query(v);
            (uri_encode(&k_dec, true), uri_encode(&v_dec, true))
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AWS SigV4 test suite (`aws-sig-v4-test-suite`) constants.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";
    const SCOPE: &str = "20150830/us-east-1/service/aws4_request";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const BASE_HEADERS: [(&str, &str); 2] =
        [("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)];

    /// (canonical request, its SHA-256, signature) for one suite case.
    fn suite_case(
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload: &[u8],
    ) -> (String, String, String) {
        let (creq, _) =
            canonical_request(method, path, query, headers, &hex_sha256(payload), false);
        let sts = string_to_sign(DATE, SCOPE, &creq);
        let hash = sts.lines().last().unwrap().to_string();
        let sig = signature(SECRET, "20150830", "us-east-1", "service", &sts);
        (creq, hash, sig)
    }

    #[test]
    fn suite_get_vanilla() {
        let (creq, hash, sig) = suite_case("GET", "/", "", &BASE_HEADERS, b"");
        assert_eq!(
            creq,
            format!(
                "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:{DATE}\n\nhost;x-amz-date\n{EMPTY}"
            )
        );
        assert_eq!(
            hash,
            "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(
            string_to_sign(DATE, SCOPE, &creq),
            format!("AWS4-HMAC-SHA256\n{DATE}\n{SCOPE}\n{hash}")
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn suite_get_vanilla_query_order_key() {
        let (creq, _, sig) = suite_case(
            "GET",
            "/",
            "Param2=value2&Param1=value1",
            &BASE_HEADERS,
            b"",
        );
        assert!(
            creq.starts_with("GET\n/\nParam1=value1&Param2=value2\n"),
            "{creq}"
        );
        assert_eq!(
            sig,
            "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        );
    }

    #[test]
    fn suite_post_x_www_form_urlencoded() {
        let headers = [
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", DATE),
        ];
        let (creq, _, sig) = suite_case("POST", "/", "", &headers, b"Param1=value1");
        assert!(
            creq.ends_with(
                "\n\ncontent-type;host;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e"
            ),
            "{creq}"
        );
        assert_eq!(
            sig,
            "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    #[test]
    fn suite_get_utf8_and_space() {
        let (creq, _, sig) = suite_case("GET", "/ሴ", "", &BASE_HEADERS, b"");
        assert!(creq.starts_with("GET\n/%E1%88%B4\n\n"), "{creq}");
        assert_eq!(
            sig,
            "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85"
        );
        let (creq, _, sig) = suite_case("GET", "/example space/", "", &BASE_HEADERS, b"");
        assert!(creq.starts_with("GET\n/example%20space/\n\n"), "{creq}");
        assert_eq!(
            sig,
            "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741"
        );
    }

    #[test]
    fn suite_path_normalization() {
        // get-slash-dot-slash, get-slashes, get-relative-relative, get-slash-pointless-dot.
        assert_eq!(canonical_uri("/./", false), "/");
        assert_eq!(canonical_uri("//example//", false), "/example/");
        assert_eq!(canonical_uri("/example1/example2/../..", false), "/");
        assert_eq!(canonical_uri("/./example", false), "/example");
        // get-slash-dot-slash signs exactly like get-vanilla.
        let (_, _, sig) = suite_case("GET", "/./", "", &BASE_HEADERS, b"");
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn non_s3_paths_are_encoded_twice_and_s3_once() {
        assert_eq!(canonical_uri("/a%20b/c", false), "/a%2520b/c");
        assert_eq!(canonical_uri("/a%20b/c", true), "/a%20b/c");
        assert_eq!(canonical_uri("/bucket//key/./x", true), "/bucket//key/./x");
        assert_eq!(wire_path("/a b/ሴ"), "/a%20b/%E1%88%B4");
        assert_eq!(wire_path("/a%20b"), "/a%20b");
    }

    #[test]
    fn signs_aws_sigv4_headers_deterministically() {
        let mut headers = vec![Header {
            name: "x-amz-target".into(),
            value: "DynamoDB_20120810.GetItem".into(),
        }];
        sign_headers_at(
            &mut headers,
            br#"{"TableName":"orders"}"#,
            &SignParams {
                method: "POST",
                path_and_query: "/",
                host: "127.0.0.1:9297",
                service: "dynamodb",
                region: "us-east-1",
                access_key: "lenslet-local",
                secret_key: "lenslet-local",
                session_token: None,
            },
            "20261120T000000Z",
            "20261120",
        );
        let auth = headers.iter().find(|h| h.name == "authorization").unwrap();
        assert!(auth.value.starts_with(
            "AWS4-HMAC-SHA256 Credential=lenslet-local/20261120/us-east-1/dynamodb/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-target, Signature="
        ));
        assert_eq!(auth.value.split("Signature=").nth(1).unwrap().len(), 64);
    }
}
