//! What went wrong with a call, in one line.
//!
//! AWS protocols report errors in three shapes: a JSON body (`__type` /
//! `message`), an XML body (`<Code>` / `<Message>`, including the Query
//! protocol's `<Error>`), or the `x-amzn-ErrorType` header. This module reads
//! whichever is present and adds a short hint for the handful of failures that
//! have one obvious cause in local development. No per-service tables.

use serde::{Deserialize, Serialize};

use crate::trace::HeaderTrace;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// AWS error code (`NoSuchKey`), or the HTTP reason when none was sent.
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// One-sentence likely cause, when the code has an obvious one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The problem of a completed call, or `None` for a success.
pub fn from_response(status: u16, headers: &[HeaderTrace], body: Option<&str>) -> Option<Problem> {
    if status < 400 {
        return function_error(headers, body.unwrap_or(""));
    }
    let body = body.unwrap_or("");
    let (body_code, message) = from_json(body)
        .or_else(|| from_xml(body))
        .unwrap_or_default();
    let code = body_code
        .or_else(|| header_code(headers))
        .unwrap_or_else(|| reason_code(status));
    let hint = message
        .as_deref()
        .and_then(|m| denial(&code, m))
        .map(|d| d.hint())
        .or_else(|| hint(&code, status))
        .map(str::to_string);
    Some(Problem {
        code,
        message,
        hint,
    })
}

/// A function's own failure: Lambda answers the call 200 and says
/// `X-Amz-Function-Error` (`Unhandled`, `Handled`), the error in the body as
/// the runtime reported it (`errorType`, `errorMessage`, `stackTrace`).
pub fn function_error(headers: &[HeaderTrace], body: &str) -> Option<Problem> {
    let kind = headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("x-amz-function-error"))
        .map(|h| h.value.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let error: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let text = |name: &str| error[name].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let mut message = text("errorMessage");
    // The innermost frames, where the code failed.
    let frames: Vec<String> = error["stackTrace"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(|f| f.trim_end().to_string()))
        .collect();
    if !frames.is_empty() {
        let last = frames[frames.len().saturating_sub(3)..].join("\n");
        message = Some(match message {
            Some(m) => format!("{m}\n{last}"),
            None => last,
        });
    }
    Some(Problem {
        code: text("errorType").unwrap_or_else(|| kind.clone()),
        message,
        hint: Some(format!(
            "The function's own code failed ({kind}): the error is its runtime's, and what it printed is with the call."
        )),
    })
}

/// Why IAM refused a call, from the message AWS (and the emulator) words it
/// with: which policy is in the way, and so whether allowing the caller's
/// role more would help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// No identity-based policy allows it: allowing the role what the call
    /// needs lets it through.
    NotAllowed,
    /// An identity-based policy denies it outright: no Allow overturns that.
    Denied,
    /// The session's own policy (passed to `AssumeRole`) leaves it out.
    Session,
    /// The role's permissions boundary leaves it out.
    Boundary,
    /// The resource's own policy (a bucket's, a key's, a queue's, a
    /// function's) or another policy beside the caller's is in the way.
    Resource,
}

impl Denial {
    /// Whether allowing the denied role what the call needs lets it through.
    pub fn grantable(self) -> bool {
        self == Denial::NotAllowed
    }

    pub fn hint(self) -> &'static str {
        match self {
            Denial::NotAllowed => {
                "No policy of the caller's allows this: allowing its role what the call needs lets it through."
            }
            Denial::Denied => {
                "A policy of the caller's denies this outright: remove that Deny; no Allow overturns it."
            }
            Denial::Session => {
                "The session's own policy (passed when the role was assumed) leaves this out: allow it there."
            }
            Denial::Boundary => {
                "The role's permissions boundary leaves this out: allow it in the boundary."
            }
            Denial::Resource => {
                "The resource's own policy is in the way (a bucket's, a key's, a queue's, a function's): allow the caller there."
            }
        }
    }
}

/// Which policy denied an access-denied call, from its message; `None` for
/// another error, or a denial the message does not explain.
pub fn denial(code: &str, message: &str) -> Option<Denial> {
    let denied = ["AccessDenied", "AccessDeniedException", "UnauthorizedOperation", "Forbidden"]
        .iter()
        .any(|c| code.eq_ignore_ascii_case(c));
    if !denied {
        return None;
    }
    let says = |phrase: &str| message.contains(phrase);
    Some(if says("explicit deny in an identity-based policy") {
        Denial::Denied
    } else if says("session policy") {
        Denial::Session
    } else if says("permissions boundary") {
        Denial::Boundary
    } else if says("resource-based policy")
        || says("VPC endpoint policy")
        || says("service control policy")
        || says("service-based policy")
    {
        Denial::Resource
    } else if says("no identity-based policy allows") {
        Denial::NotAllowed
    } else {
        return None;
    })
}

/// The problem of a call that never reached the emulator.
pub fn unreachable(upstream: &str, error: &str) -> Problem {
    Problem {
        code: "EmulatorUnreachable".to_string(),
        message: Some(error.to_string()),
        hint: Some(format!(
            "Lenslet could not reach the emulator at {upstream}. Is AWS Emulator running on that port?"
        )),
    }
}

fn normalize(code: &str) -> Option<String> {
    let code = code.rsplit('#').next().unwrap_or(code);
    let code = code.split(':').next().unwrap_or(code).trim();
    (!code.is_empty()).then(|| code.to_string())
}

fn from_json(body: &str) -> Option<(Option<String>, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let object = value.as_object()?;
    // JSON-wrapped Query errors nest under "Error".
    let object = object
        .get("Error")
        .and_then(|error| error.as_object())
        .unwrap_or(object);
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(|value| value.as_str()))
            .map(str::to_string)
    };
    let code = text(&["__type", "code", "Code", "errorType"]).and_then(|code| normalize(&code));
    let message = text(&["message", "Message", "errorMessage"]).filter(|m| !m.is_empty());
    (code.is_some() || message.is_some()).then_some((code, message))
}

fn from_xml(body: &str) -> Option<(Option<String>, Option<String>)> {
    let code = xml_element(body, "Code").and_then(|code| normalize(&code));
    let message = xml_element(body, "Message").filter(|m| !m.is_empty());
    (code.is_some() || message.is_some()).then_some((code, message))
}

fn xml_element(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = start + body[start..].find(&close)?;
    Some(
        body[start..end]
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

fn header_code(headers: &[HeaderTrace]) -> Option<String> {
    headers.iter().find_map(|header| {
        let name = header.name.to_ascii_lowercase();
        match name.as_str() {
            "x-amzn-errortype" => normalize(&header.value),
            // `AWS.SimpleQueueService.NonExistentQueue;Sender`
            "x-amzn-query-error" => normalize(header.value.split(';').next().unwrap_or("")),
            _ => None,
        }
    })
}

fn reason_code(status: u16) -> String {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .map(|reason| reason.replace([' ', '-'], ""))
        .unwrap_or_else(|| format!("Http{status}"))
}

/// The one obvious cause for common local-development failures.
pub fn hint(code: &str, status: u16) -> Option<&'static str> {
    let is = |names: &[&str]| names.iter().any(|name| code.eq_ignore_ascii_case(name));
    if is(&[
        "SignatureDoesNotMatch",
        "InvalidSignatureException",
        "IncompleteSignature",
        "IncompleteSignatureException",
    ]) {
        return Some(
            "The SigV4 signature did not verify: the request changed after signing, or the secret key is not the emulator's.",
        );
    }
    if is(&[
        "UnrecognizedClientException",
        "InvalidClientTokenId",
        "InvalidAccessKeyId",
        "AuthFailure",
        "ExpiredToken",
        "ExpiredTokenException",
        "MissingAuthenticationToken",
        "MissingAuthenticationTokenException",
    ]) {
        return Some(
            "The emulator does not know these credentials. Use the variables Lenslet's terminal has (File → Copy Environment), not real AWS keys.",
        );
    }
    if is(&[
        "AuthorizationHeaderMalformed",
        "IllegalLocationConstraintException",
        "InvalidLocationConstraint",
        "PermanentRedirect",
    ]) {
        return Some(
            "Region mismatch: the request is signed for a different region than the resource.",
        );
    }
    if is(&[
        "AccessDenied",
        "AccessDeniedException",
        "UnauthorizedOperation",
        "Forbidden",
    ]) {
        return Some("Denied by the emulator's IAM evaluation for this principal.");
    }
    if is(&[
        "NotImplemented",
        "UnknownOperationException",
        "InvalidAction",
        "UnsupportedOperation",
        "UnsupportedOperationException",
    ]) || status == 501
    {
        return Some(
            "The emulator does not implement this operation, or the SDK sent an unknown action.",
        );
    }
    let lower = code.to_ascii_lowercase();
    if lower.starts_with("nosuch")
        || lower.ends_with("notfound")
        || lower.ends_with("notfoundexception")
        || lower.contains("nonexistent")
        || lower.contains("doesnotexist")
    {
        return Some(
            "The resource does not exist in the emulator. Was it created in this run, in the same region?",
        );
    }
    if lower.contains("alreadyexists")
        || lower.contains("alreadyowned")
        || is(&[
            "ResourceInUseException",
            "ConflictException",
            "EntityAlreadyExists",
        ])
    {
        return Some("The resource already exists, probably left over from an earlier run.");
    }
    if is(&[
        "ThrottlingException",
        "Throttling",
        "TooManyRequestsException",
        "SlowDown",
        "RequestLimitExceeded",
        "ProvisionedThroughputExceededException",
    ]) {
        return Some("Throttled. Retry with backoff.");
    }
    if is(&[
        "ValidationException",
        "ValidationError",
        "InvalidParameterValue",
        "InvalidParameterValueException",
        "InvalidParameterException",
        "InvalidParameterCombination",
        "MissingParameter",
        "InvalidRequest",
        "InvalidArgument",
        "MalformedXML",
        "SerializationException",
    ]) {
        return Some("The emulator rejected the request parameters; the message names the field.");
    }
    if status >= 500 {
        return Some(
            "The emulator failed internally: an emulator bug or unsupported input, not a client error.",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> HeaderTrace {
        HeaderTrace {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn a_function_that_failed_is_a_failure_though_lambda_answered_200() {
        let body = r#"{"errorMessage":"boom","errorType":"ValueError","stackTrace":["  File \"/var/task/app.py\", line 2, in handler\n    raise ValueError('boom')\n"]}"#;
        let problem = from_response(200, &[header("X-Amz-Function-Error", "Unhandled")], Some(body))
            .expect("a failure");
        assert_eq!(problem.code, "ValueError");
        let message = problem.message.unwrap();
        assert!(message.starts_with("boom\n") && message.contains("app.py\", line 2"), "{message}");
        assert!(problem.hint.unwrap().contains("Unhandled"));
        // A function that returned: no failure.
        assert!(from_response(200, &[], Some(r#"{"ok":true}"#)).is_none());
        // Not JSON: the kind is the code.
        let problem = from_response(200, &[header("x-amz-function-error", "Handled")], Some("oops"));
        assert_eq!(problem.unwrap().code, "Handled");
    }

    #[test]
    fn tells_which_policy_denied_a_call() {
        let base = "User: arn:aws:sts::000000000000:assumed-role/r/s is not authorized to perform: dynamodb:PutItem on resource: t ";
        for (tail, expected) in [
            ("because no identity-based policy allows the dynamodb:PutItem action", Denial::NotAllowed),
            ("with an explicit deny in an identity-based policy: arn:aws:iam::0:policy/p", Denial::Denied),
            ("because no session policy allows the dynamodb:PutItem action", Denial::Session),
            ("because no permissions boundary allows the dynamodb:PutItem action", Denial::Boundary),
            ("with an explicit deny in a resource-based policy", Denial::Resource),
            ("because no VPC endpoint policy allows the s3:PutObject action", Denial::Resource),
        ] {
            let message = format!("{base}{tail}");
            assert_eq!(denial("AccessDeniedException", &message), Some(expected), "{tail}");
            let problem = from_response(
                400,
                &[],
                Some(&format!(r#"{{"__type":"AccessDeniedException","message":"{message}"}}"#)),
            )
            .unwrap();
            assert_eq!(problem.hint.as_deref(), Some(expected.hint()));
        }
        assert!(Denial::NotAllowed.grantable() && !Denial::Resource.grantable());
        assert_eq!(denial("AccessDenied", "Access Denied"), None);
        assert_eq!(denial("NoSuchKey", "because no identity-based policy allows"), None);
    }

    fn header(name: &str, value: &str) -> HeaderTrace {
        HeaderTrace {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn successes_have_no_problem() {
        assert_eq!(from_response(200, &[], Some("{}")), None);
        assert_eq!(from_response(304, &[], None), None);
    }

    #[test]
    fn reads_json_protocol_errors() {
        let problem = from_response(
            400,
            &[],
            Some(r#"{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException","message":"Requested resource not found"}"#),
        )
        .unwrap();
        assert_eq!(problem.code, "ResourceNotFoundException");
        assert_eq!(
            problem.message.as_deref(),
            Some("Requested resource not found")
        );
        assert!(problem.hint.unwrap().contains("does not exist"));
    }

    #[test]
    fn reads_xml_and_query_errors() {
        let s3 = r#"<?xml version="1.0"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#;
        let problem = from_response(404, &[], Some(s3)).unwrap();
        assert_eq!(problem.code, "NoSuchKey");
        assert_eq!(
            problem.message.as_deref(),
            Some("The specified key does not exist.")
        );

        let query = r#"<ErrorResponse><Error><Type>Sender</Type><Code>InvalidParameterValue</Code><Message>Value &lt;x&gt; is invalid</Message></Error></ErrorResponse>"#;
        let problem = from_response(400, &[], Some(query)).unwrap();
        assert_eq!(problem.code, "InvalidParameterValue");
        assert_eq!(problem.message.as_deref(), Some("Value <x> is invalid"));
        assert!(problem.hint.is_some());
    }

    #[test]
    fn falls_back_to_headers_then_status() {
        let problem = from_response(
            400,
            &[header(
                "X-Amzn-ErrorType",
                "ValidationException:http://internal.amazon.com/",
            )],
            Some(""),
        )
        .unwrap();
        assert_eq!(problem.code, "ValidationException");

        let problem = from_response(
            400,
            &[header(
                "x-amzn-query-error",
                "AWS.SimpleQueueService.NonExistentQueue;Sender",
            )],
            None,
        )
        .unwrap();
        assert_eq!(problem.code, "AWS.SimpleQueueService.NonExistentQueue");
        assert!(problem.hint.unwrap().contains("does not exist"));

        // S3 HEAD 404 has no body at all.
        let problem = from_response(404, &[], None).unwrap();
        assert_eq!(problem.code, "NotFound");
        assert!(problem.hint.is_some());
        assert_eq!(from_response(418, &[], None).unwrap().hint, None);
    }

    #[test]
    fn hints_cover_the_common_local_failures() {
        for (code, status, needle) in [
            ("SignatureDoesNotMatch", 403, "signature"),
            ("UnrecognizedClientException", 400, "credentials"),
            ("AuthorizationHeaderMalformed", 400, "Region"),
            ("BucketAlreadyOwnedByYou", 409, "already exists"),
            ("ThrottlingException", 400, "Throttled"),
            ("InternalError", 500, "internally"),
            ("Anything", 501, "implement"),
        ] {
            let hint = hint(code, status).unwrap_or_default();
            assert!(hint.contains(needle), "{code}: {hint}");
        }
    }

    #[test]
    fn truncated_bodies_do_not_panic() {
        assert!(from_response(400, &[], Some(r#"{"__type":"Valid"#)).is_some());
        assert!(from_response(400, &[], Some("<Error><Code>NoSu")).is_some());
    }
}
