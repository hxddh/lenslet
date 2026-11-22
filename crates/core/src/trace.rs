//! One recorded call: the request, the response, and the facts derived from
//! them (the IAM access it needs, why it failed).

use http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::{
    access::{Access, CallAccess},
    problem::Problem,
};

/// One finished call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Trace {
    /// Unique per call within (and across) runs.
    pub id: String,
    /// RFC3339 timestamp of when the request arrived.
    pub at: String,
    pub method: String,
    /// Path and query exactly as sent.
    pub path: String,
    pub host: String,
    pub service: String,
    pub operation: String,
    pub status: u16,
    /// Wall time for the complete exchange, measured after the body finished.
    pub ms: u64,
    pub authorization: bool,
    /// SigV4 signing region, when the request was signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// SDK invocation identifier (`amz-sdk-invocation-id`), constant across retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// Retry attempt number (`> 1`) from `amz-sdk-request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Maximum attempts configured for this SDK invocation, when retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Set when the proxy could not complete the exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Error code, message and likely cause of a failed call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
    /// The IAM permissions this call needs (`s3:GetObject` on a bucket), as
    /// the lock records them: usually one; several for batches, transactions
    /// and copies; none when the operation is unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<CallAccess>,
    /// Permissions the call implies elsewhere (SSE-KMS key, passed role).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implied: Vec<CallAccess>,
    /// The exact permissions (own, then implied) derived live from the
    /// complete request body: what `lenslet check --policy` evaluates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concrete: Vec<Access>,
    pub request: Message,
    pub response: Message,
}

impl Trace {
    /// Whether any of this call's access is not in the lock.
    pub fn is_new(&self) -> bool {
        self.access.iter().chain(&self.implied).any(|item| item.new)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Message {
    /// Raw header values in map order, duplicates preserved.
    pub headers: Vec<HeaderTrace>,
    /// Media type without parameters, e.g. `application/xml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Absent when the body was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyTrace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderTrace {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BodyTrace {
    /// The body as text (unframed, decompressed, CBOR as JSON), at most
    /// [`MAX_BODY_TEXT`] bytes; absent for binary bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Wire encodings decoded for `text` (`aws-chunked`, `gzip`, `cbor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Total wire bytes of the body.
    pub bytes: u64,
    /// `text` holds only a prefix of the body.
    pub truncated: bool,
    pub binary: bool,
    /// SHA-256 of every wire byte.
    pub sha256: String,
}

/// The most body text a run file keeps per message (1 MiB).
pub const MAX_BODY_TEXT: usize = 1024 * 1024;

impl BodyTrace {
    /// Keep at most `max` bytes of text (on a character boundary).
    pub fn cap(&mut self, max: usize) {
        if let Some(text) = self.text.as_mut()
            && text.len() > max
        {
            let mut cut = max;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            self.truncated = true;
        }
    }
}

/// Raw headers in a stable order with duplicates preserved.
pub fn headers(headers: &HeaderMap) -> Vec<HeaderTrace> {
    headers
        .iter()
        .map(|(name, value)| HeaderTrace {
            name: name.as_str().to_string(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })
        .collect()
}

/// Every permission one call accounts for: its own, then the implied ones.
pub fn trace_access(trace: &Trace) -> impl Iterator<Item = Access> + '_ {
    trace
        .access
        .iter()
        .chain(trace.implied.iter())
        .map(CallAccess::key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capping_keeps_whole_characters() {
        let mut body = BodyTrace {
            text: Some("aé".repeat(3)),
            ..BodyTrace::default()
        };
        body.cap(4);
        assert_eq!(body.text.as_deref(), Some("aéa"));
        assert!(body.truncated);
    }
}
