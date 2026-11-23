//! A recorded call as something to paste: an AWS CLI command, a curl command,
//! or a boto3 call. The CLI and boto3 forms exist for the JSON protocols,
//! whose request body is the operation's input as the API defines it.

use crate::session::Call;

/// The AWS CLI's name for a service, by signing name.
fn cli_service(service: &str) -> &str {
    match service {
        "states" => "stepfunctions",
        "s3" => "s3api",
        other => other,
    }
}

fn json_protocol(call: &Call) -> bool {
    call.request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("x-amz-target"))
}

/// `PutItem` as the CLI writes it: `put-item`.
pub fn kebab(operation: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = operation.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev_lower = chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev_lower || (next_lower && chars[i - 1].is_uppercase()) {
                out.push('-');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// `PutItem` as boto3 writes it: `put_item`.
pub fn snake(operation: &str) -> String {
    kebab(operation).replace('-', "_")
}

/// Shell single-quoting.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// The call as an AWS CLI command, for the services whose body is the input.
pub fn aws_cli(call: &Call) -> Option<String> {
    if !json_protocol(call) {
        return None;
    }
    let body = compact(&call.request.body)?;
    let mut command = format!(
        "aws {} {}",
        cli_service(&call.service),
        kebab(&call.operation)
    );
    if body != "{}" {
        command.push_str(&format!(" --cli-input-json {}", quote(&body)));
    }
    Some(command)
}

/// The call as boto3, for the services whose body is the input.
pub fn boto3(call: &Call) -> Option<String> {
    if !json_protocol(call) {
        return None;
    }
    let body: serde_json::Value = serde_json::from_str(&call.request.body).ok()?;
    let args = serde_json::to_string_pretty(&body).ok()?;
    let args = python_literals(&args);
    let method = snake(&call.operation);
    let client = format!("boto3.client({:?})", call.service);
    Some(if body.as_object().is_some_and(|o| o.is_empty()) {
        format!("{client}.{method}()")
    } else {
        format!("{client}.{method}(**{args})")
    })
}

/// The exact request, replayable with curl.
pub fn curl(call: &Call, endpoint: &str) -> String {
    let mut command = format!(
        "curl -sS -X {} {}",
        call.method,
        quote(&format!("{endpoint}{}", call.path))
    );
    const SKIP: &[&str] = &[
        "host",
        "content-length",
        "connection",
        "accept-encoding",
        "transfer-encoding",
    ];
    for (name, value) in &call.request.headers {
        if SKIP.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            continue;
        }
        command.push_str(&format!(" \\\n  -H {}", quote(&format!("{name}: {value}"))));
    }
    if !call.request.body.is_empty() && call.request.body.len() == call.request.bytes {
        command.push_str(&format!(
            " \\\n  --data-binary {}",
            quote(&call.request.body)
        ));
    }
    command
}

fn compact(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return Some("{}".into());
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    serde_json::to_string(&value).ok()
}

/// JSON's `true`, `false` and `null` as Python's, outside strings.
fn python_literals(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(match word.as_str() {
            "true" => "True",
            "false" => "False",
            "null" => "None",
            other => other,
        });
        word.clear();
    };
    for c in json.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c.is_ascii_alphabetic() {
            word.push(c);
            continue;
        }
        flush(&mut word, &mut out);
        if c == '"' {
            in_string = true;
        }
        out.push(c);
    }
    flush(&mut word, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Exchange;

    #[test]
    fn writes_operations_as_the_cli_and_boto3_do() {
        assert_eq!(kebab("PutItem"), "put-item");
        assert_eq!(kebab("ListObjectsV2"), "list-objects-v2");
        assert_eq!(kebab("GetQueueURL"), "get-queue-url");
        assert_eq!(kebab("DescribeDBInstances"), "describe-db-instances");
        assert_eq!(snake("SendMessage"), "send_message");
    }

    #[test]
    fn json_calls_become_cli_boto3_and_curl() {
        let call = json_call(r#"{"TableName":"orders","ConsistentRead":true,"X":null}"#);
        assert_eq!(
            aws_cli(&call).unwrap(),
            r#"aws dynamodb put-item --cli-input-json '{"TableName":"orders","ConsistentRead":true,"X":null}'"#
        );
        let py = boto3(&call).unwrap();
        assert!(
            py.starts_with("boto3.client(\"dynamodb\").put_item(**{"),
            "{py}"
        );
        assert!(
            py.contains("\"ConsistentRead\": True") && py.contains("\"X\": None"),
            "{py}"
        );
        let curl = curl(&call, "http://127.0.0.1:4510");
        assert!(
            curl.starts_with("curl -sS -X POST 'http://127.0.0.1:4510/'"),
            "{curl}"
        );
        assert!(
            curl.contains("-H 'x-amz-target: DynamoDB_20120810.PutItem'"),
            "{curl}"
        );
        assert!(curl.contains("--data-binary '{\"TableName\""), "{curl}");
    }

    #[test]
    fn rest_calls_only_become_curl() {
        let mut call = json_call("");
        call.request.headers.clear();
        assert!(aws_cli(&call).is_none() && boto3(&call).is_none());
    }

    fn json_call(body: &str) -> Call {
        Call {
            seq: 0,
            generation: 0,
            at_ms: 0,
            service: "dynamodb".into(),
            operation: "PutItem".into(),
            method: "POST".into(),
            path: "/".into(),
            status: 200,
            ms: 1,
            region: None,
            target: None,
            failure: None,
            unsupported: false,
            access: vec![],
            client: None,
            attempt: None,
            max_attempts: None,
            invocation: None,
            request_id: None,
            request: Exchange {
                headers: vec![
                    ("host".into(), "127.0.0.1:4510".into()),
                    ("x-amz-target".into(), "DynamoDB_20120810.PutItem".into()),
                ],
                content_type: Some("application/x-amz-json-1.0".into()),
                body: body.into(),
                bytes: body.len(),
            },
            response: Exchange::default(),
        }
    }
}
