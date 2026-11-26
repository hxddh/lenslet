//! A recorded call as something to paste: an AWS CLI command, a curl command,
//! or a boto3 call. The CLI and boto3 forms come from the operation's input,
//! read back from the request: the body of the JSON and CBOR protocols, the
//! form of the Query protocol, and the path, query and headers (and body) of
//! S3's and Lambda's REST APIs.

use lenslet_core::wire;
use serde_json::{Map, Value};

use crate::session::Call;

/// The AWS CLI's name for a service, by signing name.
fn cli_service(service: &str) -> &str {
    match service {
        "states" => "stepfunctions",
        "s3" => "s3api",
        "monitoring" => "cloudwatch",
        other => other,
    }
}

/// The request's header `name`.
fn header<'a>(call: &'a Call, name: &str) -> Option<&'a str> {
    call.request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The protocols whose body is the operation's input: AWS JSON and Smithy
/// RPCv2 CBOR (its body shown decoded, as JSON).
fn json_protocol(call: &Call) -> bool {
    header(call, "x-amz-target").is_some()
        || header(call, "smithy-protocol").is_some()
        || call
            .request
            .content_type
            .as_deref()
            .is_some_and(|t| t.contains("cbor"))
}

fn query_protocol(call: &Call) -> bool {
    call.request
        .content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("application/x-www-form-urlencoded"))
        || call.request.body.starts_with("Action=")
}

/// What a call asked for, as the operation's input.
struct Input {
    params: Map<String, Value>,
    /// A blob parameter's text: S3's `Body`, Lambda's `Payload`.
    blob: Option<(&'static str, String)>,
    /// The file the CLI writes the answer to (`get-object`, `invoke`).
    outfile: Option<String>,
}

impl Input {
    fn of(params: Map<String, Value>) -> Self {
        Input {
            params,
            blob: None,
            outfile: None,
        }
    }
}

/// The input of a call made over the Query, S3 or Lambda REST protocols.
fn input(call: &Call) -> Option<Input> {
    if !call.request.complete {
        // Its body is data, or only part of it is kept.
        return None;
    }
    match call.service.as_str() {
        "s3" => s3_input(call),
        "lambda" => lambda_input(call),
        _ if query_protocol(call) => Some(Input::of(form_input(&call.request.body))),
        _ => None,
    }
}

/// A Query form as the input it encodes: `Tags.member.1.Key=k` is
/// `{"Tags": [{"Key": "k"}]}`, `Attributes.entry.1.key=a&…value=b` is
/// `{"Attributes": {"a": "b"}}`.
fn form_input(body: &str) -> Map<String, Value> {
    #[derive(Default)]
    struct Node {
        leaf: Option<String>,
        children: std::collections::BTreeMap<String, Node>,
    }
    fn value(name: &str, node: Node) -> Value {
        if node.children.is_empty() {
            return scalar(name, node.leaf.unwrap_or_default());
        }
        let mut children = node.children;
        // `member` wraps a list's items, `entry` a map's.
        let mut wrapper = None;
        for kind in ["member", "entry"] {
            if children.len() == 1
                && let Some(inner) = children.remove(kind)
            {
                children = inner.children;
                wrapper = Some(kind);
            }
        }
        let numbered = children.keys().all(|k| k.parse::<u32>().is_ok());
        if !numbered {
            return Value::Object(
                children
                    .into_iter()
                    .map(|(k, v)| (k.clone(), value(&k, v)))
                    .collect(),
            );
        }
        let mut items: Vec<(u32, Node)> = children
            .into_iter()
            .map(|(k, v)| (k.parse().unwrap_or_default(), v))
            .collect();
        items.sort_by_key(|(i, _)| *i);
        // A map's entries: `entry.N.key`/`value`, or a flattened map's
        // `Name`/`Value` pairs (SQS's `Attribute.N.Name`).
        let pair = |n: &Node, key: &str| {
            n.children.len() == 2
                && n.children.contains_key(key)
                && n.children.keys().any(|c| c.eq_ignore_ascii_case("value"))
        };
        let map = match wrapper {
            Some("entry") => items.iter().all(|(_, n)| pair(n, "key")),
            Some(_) => false,
            None => items.iter().all(|(_, n)| pair(n, "Name")),
        };
        if !items.is_empty() && map {
            return Value::Object(
                items
                    .into_iter()
                    .map(|(_, n)| {
                        let mut key = String::new();
                        let mut val = Value::Null;
                        for (k, v) in n.children {
                            if k.eq_ignore_ascii_case("value") {
                                val = value(name, v);
                            } else {
                                key = v.leaf.unwrap_or_default();
                            }
                        }
                        (key, val)
                    })
                    .collect(),
            );
        }
        Value::Array(items.into_iter().map(|(_, n)| value(name, n)).collect())
    }
    let mut root = Node::default();
    for pair in body.split('&') {
        let Some((key, raw)) = pair.split_once('=') else {
            continue;
        };
        let key = wire::percent_decode_query(key);
        if key == "Action" || key == "Version" {
            continue;
        }
        let mut node = &mut root;
        for part in key.split('.') {
            node = node.children.entry(part.to_string()).or_default();
        }
        node.leaf = Some(wire::percent_decode_query(raw));
    }
    root.children
        .into_iter()
        .map(|(k, v)| (k.clone(), value(&k, v)))
        .collect()
}

/// A form value as its type: `true`, and a count (`MaxItems=100`) as a
/// number; anything else stays text.
fn scalar(name: &str, text: String) -> Value {
    if text == "true" || text == "false" {
        return Value::Bool(text == "true");
    }
    let counted = name.starts_with("Max")
        || [
            "Seconds", "Size", "Count", "Limit", "Items", "Days", "Number",
        ]
        .iter()
        .any(|s| name.ends_with(s));
    match text.parse::<u64>() {
        Ok(n) if counted && (text == "0" || !text.starts_with('0')) => Value::from(n),
        _ => Value::String(text),
    }
}

/// S3's REST input: the bucket and key from the host or path, the query's
/// parameters, the headers that carry input, and the object's body.
fn s3_input(call: &Call) -> Option<Input> {
    let (path, query) = call.path.split_once('?').unwrap_or((&call.path, ""));
    let path = path.trim_start_matches('/');
    let host = header(call, "host").unwrap_or_default();
    let (bucket, key) = match wire::s3_host_bucket(host) {
        Some(bucket) => (bucket.to_string(), wire::percent_decode_path(path)),
        None => match path.split_once('/') {
            Some((b, k)) => (wire::percent_decode_path(b), wire::percent_decode_path(k)),
            None => (wire::percent_decode_path(path), String::new()),
        },
    };
    let mut params = Map::new();
    if !bucket.is_empty() {
        params.insert("Bucket".into(), bucket.into());
    }
    if !key.is_empty() {
        params.insert("Key".into(), key.clone().into());
    }
    const QUERY: &[(&str, &str)] = &[
        ("prefix", "Prefix"),
        ("delimiter", "Delimiter"),
        ("max-keys", "MaxKeys"),
        ("continuation-token", "ContinuationToken"),
        ("start-after", "StartAfter"),
        ("marker", "Marker"),
        ("encoding-type", "EncodingType"),
        ("fetch-owner", "FetchOwner"),
        ("uploadId", "UploadId"),
        ("partNumber", "PartNumber"),
        ("versionId", "VersionId"),
        ("max-uploads", "MaxUploads"),
        ("max-parts", "MaxParts"),
        ("key-marker", "KeyMarker"),
        ("upload-id-marker", "UploadIdMarker"),
        ("version-id-marker", "VersionIdMarker"),
        ("part-number-marker", "PartNumberMarker"),
    ];
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, raw) = pair.split_once('=').unwrap_or((pair, ""));
        if let Some((_, param)) = QUERY.iter().find(|(q, _)| *q == name) {
            params.insert(
                param.to_string(),
                scalar(param, wire::percent_decode_query(raw)),
            );
        }
    }
    const HEADERS: &[(&str, &str)] = &[
        ("content-type", "ContentType"),
        ("cache-control", "CacheControl"),
        ("content-disposition", "ContentDisposition"),
        ("content-language", "ContentLanguage"),
        ("x-amz-copy-source", "CopySource"),
        ("x-amz-storage-class", "StorageClass"),
        ("x-amz-server-side-encryption", "ServerSideEncryption"),
        ("x-amz-tagging", "Tagging"),
        ("x-amz-acl", "ACL"),
        ("range", "Range"),
    ];
    let uploads = matches!(
        call.operation.as_str(),
        "PutObject" | "CopyObject" | "CreateMultipartUpload"
    );
    let mut metadata = Map::new();
    for (name, value) in &call.request.headers {
        let name = name.to_ascii_lowercase();
        if let Some(meta) = name.strip_prefix("x-amz-meta-") {
            metadata.insert(meta.to_string(), value.clone().into());
        } else if let Some((_, param)) = HEADERS.iter().find(|(h, _)| *h == name)
            && (uploads || !matches!(*param, "ContentType" | "CacheControl"))
        {
            params.insert(param.to_string(), value.clone().into());
        }
    }
    if !metadata.is_empty() {
        params.insert("Metadata".into(), metadata.into());
    }
    let mut input = Input::of(params);
    let body = &call.request.body;
    match call.operation.as_str() {
        "PutObject" | "UploadPart" if !body.is_empty() => {
            input.blob = Some(("Body", body.clone()));
        }
        "PutBucketPolicy" => {
            input.params.insert("Policy".into(), body.clone().into());
        }
        "GetObject" => {
            let file = key.rsplit('/').next().filter(|f| !f.is_empty());
            input.outfile = Some(file.unwrap_or("object").to_string());
        }
        // Input S3 takes as an XML document is not read back.
        _ if !body.is_empty() => return None,
        _ => {}
    }
    Some(input)
}

/// Lambda's REST input: the function (alias, mapping, statement) the path
/// names, its query, and the JSON body; `Invoke`'s body is its payload.
fn lambda_input(call: &Call) -> Option<Input> {
    let (path, query) = call.path.split_once('?').unwrap_or((&call.path, ""));
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .skip(1)
        .map(wire::percent_decode_path)
        .collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let mut params = Map::new();
    let mut put = |name: &str, value: &str| {
        params.insert(name.to_string(), value.into());
    };
    match segments.as_slice() {
        ["functions", name, rest @ ..] => {
            put("FunctionName", name);
            match rest {
                ["aliases", alias] => put("Name", alias),
                ["policy", sid] => put("StatementId", sid),
                _ => {}
            }
        }
        ["event-source-mappings", uuid] => put("UUID", uuid),
        ["tags", arn] => put("Resource", arn),
        _ => {}
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, raw) = pair.split_once('=').unwrap_or((pair, ""));
        if let Some(param) = [
            "Qualifier",
            "Marker",
            "MaxItems",
            "FunctionVersion",
            "EventSourceArn",
        ]
        .iter()
        .find(|p| **p == name)
        {
            params.insert(
                param.to_string(),
                scalar(param, wire::percent_decode_query(raw)),
            );
        }
    }
    let body = &call.request.body;
    if call.operation == "Invoke" {
        for (name, param) in [
            ("x-amz-invocation-type", "InvocationType"),
            ("x-amz-log-type", "LogType"),
            ("x-amz-client-context", "ClientContext"),
        ] {
            if let Some(value) = header(call, name) {
                params.insert(param.into(), value.into());
            }
        }
        let mut input = Input::of(params);
        if !body.is_empty() {
            input.blob = Some(("Payload", body.clone()));
        }
        input.outfile = Some("response.json".into());
        return Some(input);
    }
    if !body.trim().is_empty() {
        let Value::Object(fields) = serde_json::from_str(body).ok()? else {
            return None;
        };
        params.extend(fields);
    }
    Some(Input::of(params))
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

/// The call as an AWS CLI command.
pub fn aws_cli(call: &Call) -> Option<String> {
    let mut command = format!(
        "aws {} {}",
        cli_service(&call.service),
        kebab(&call.operation)
    );
    if json_protocol(call) {
        if !call.request.complete {
            return None;
        }
        let body = compact(&call.request.body)?;
        if body != "{}" {
            command.push_str(&format!(" --cli-input-json {}", quote(&body)));
        }
        return Some(command);
    }
    let input = input(call)?;
    for (name, value) in &input.params {
        let flag = kebab(name);
        match value {
            Value::Bool(true) => command.push_str(&format!(" --{flag}")),
            Value::Bool(false) => command.push_str(&format!(" --no-{flag}")),
            Value::String(text) => command.push_str(&format!(" --{flag} {}", quote(text))),
            Value::Number(n) => command.push_str(&format!(" --{flag} {n}")),
            other => command.push_str(&format!(" --{flag} {}", quote(&other.to_string()))),
        }
    }
    match &input.blob {
        // The CLI reads an object's body from a file: the one it came from.
        Some(("Body", _)) => {
            let key = input.params.get("Key").and_then(Value::as_str);
            let file = key.and_then(|k| k.rsplit('/').next()).unwrap_or("body");
            command.push_str(&format!(" --body {}", quote(file)));
        }
        Some((_, payload)) => command.push_str(&format!(
            " --cli-binary-format raw-in-base64-out --payload {}",
            quote(payload)
        )),
        None => {}
    }
    if let Some(file) = &input.outfile {
        command.push_str(&format!(" {}", quote(file)));
    }
    Some(command)
}

/// The call as boto3.
pub fn boto3(call: &Call) -> Option<String> {
    let mut params = if json_protocol(call) {
        if !call.request.complete {
            return None;
        }
        match serde_json::from_str(&call.request.body).ok()? {
            Value::Object(params) => params,
            _ => return None,
        }
    } else {
        let input = input(call)?;
        let mut params = input.params;
        if let Some((name, text)) = input.blob {
            params.insert(name.into(), text.into());
        }
        params
    };
    params.retain(|_, v| !v.is_null() || json_protocol(call));
    let method = snake(&call.operation);
    let client = format!(
        "boto3.client({:?})",
        cli_service(&call.service).replace("s3api", "s3")
    );
    if params.is_empty() {
        return Some(format!("{client}.{method}()"));
    }
    let args = serde_json::to_string_pretty(&Value::Object(params)).ok()?;
    Some(format!("{client}.{method}(**{})", python_literals(&args)))
}

/// The exact request, replayable with curl: a body that is not text (binary,
/// CBOR, framed or compressed) goes as its bytes, never as what the window
/// shows for it.
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
    if call.request.bytes == 0 {
        return command;
    }
    if call.request.verbatim {
        command.push_str(&format!(
            " \\\n  --data-binary {}",
            quote(&call.request.body)
        ));
        return command;
    }
    match &call.request.raw {
        Some(raw) => format!(
            "printf {} | {command} \\\n  --data-binary @-",
            quote(&printf_escaped(raw))
        ),
        None => format!(
            "# The request's {}-byte body is not kept: add it with --data-binary @FILE.\n{command}",
            call.request.bytes
        ),
    }
}

/// Bytes as a `printf` format that prints exactly them: octal escapes for
/// all but plain printable ASCII.
fn printf_escaped(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            b'%' | b'\\' | b'\'' => out.push_str(&format!("\\{b:03o}")),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out
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

    fn rest_call(service: &str, operation: &str, method: &str, path: &str, body: &str) -> Call {
        let mut call = json_call(body);
        call.service = service.into();
        call.operation = operation.into();
        call.method = method.into();
        call.path = path.into();
        call.request.headers = vec![("host".into(), "127.0.0.1:4510".into())];
        call.request.content_type = None;
        call
    }

    #[test]
    fn s3_calls_become_cli_and_boto3() {
        let mut put = rest_call(
            "s3",
            "PutObject",
            "PUT",
            "/shop/docs/a.txt?x-id=PutObject",
            "hi",
        );
        put.request
            .headers
            .push(("content-type".into(), "text/plain".into()));
        put.request
            .headers
            .push(("x-amz-meta-owner".into(), "ann".into()));
        assert_eq!(
            aws_cli(&put).unwrap(),
            "aws s3api put-object --bucket 'shop' --key 'docs/a.txt' --content-type 'text/plain' --metadata '{\"owner\":\"ann\"}' --body 'a.txt'"
        );
        let py = boto3(&put).unwrap();
        assert!(
            py.starts_with("boto3.client(\"s3\").put_object(**{"),
            "{py}"
        );
        assert!(
            py.contains("\"Body\": \"hi\"") && py.contains("\"Key\": \"docs/a.txt\""),
            "{py}"
        );

        let mut get = rest_call("s3", "GetObject", "GET", "/a.txt", "");
        get.request.headers = vec![("host".into(), "shop.localhost:4510".into())];
        assert_eq!(
            aws_cli(&get).unwrap(),
            "aws s3api get-object --bucket 'shop' --key 'a.txt' 'a.txt'"
        );
        let list = rest_call(
            "s3",
            "ListObjectsV2",
            "GET",
            "/shop?list-type=2&prefix=docs%2F&max-keys=10",
            "",
        );
        assert_eq!(
            aws_cli(&list).unwrap(),
            "aws s3api list-objects-v2 --bucket 'shop' --prefix 'docs/' --max-keys 10"
        );
        assert_eq!(
            boto3(&list).unwrap(),
            "boto3.client(\"s3\").list_objects_v2(**{\n  \"Bucket\": \"shop\",\n  \"Prefix\": \"docs/\",\n  \"MaxKeys\": 10\n})"
        );
        // An input S3 takes as XML is not read back.
        let versioning = rest_call(
            "s3",
            "PutBucketVersioning",
            "PUT",
            "/shop?versioning",
            "<VersioningConfiguration/>",
        );
        assert!(aws_cli(&versioning).is_none() && boto3(&versioning).is_none());
    }

    #[test]
    fn lambda_and_query_calls_become_cli_and_boto3() {
        let mut invoke = rest_call(
            "lambda",
            "Invoke",
            "POST",
            "/2015-03-31/functions/resize/invocations?Qualifier=live",
            r#"{"key":"cat.jpg"}"#,
        );
        invoke
            .request
            .headers
            .push(("x-amz-invocation-type".into(), "Event".into()));
        assert_eq!(
            aws_cli(&invoke).unwrap(),
            "aws lambda invoke --function-name 'resize' --qualifier 'live' --invocation-type 'Event' --cli-binary-format raw-in-base64-out --payload '{\"key\":\"cat.jpg\"}' 'response.json'"
        );
        assert!(
            boto3(&invoke)
                .unwrap()
                .contains("\"Payload\": \"{\\\"key\\\":\\\"cat.jpg\\\"}\"")
        );
        let config = rest_call(
            "lambda",
            "UpdateFunctionConfiguration",
            "PUT",
            "/2015-03-31/functions/resize/configuration",
            r#"{"Timeout":30}"#,
        );
        assert_eq!(
            aws_cli(&config).unwrap(),
            "aws lambda update-function-configuration --function-name 'resize' --timeout 30"
        );

        let mut role = rest_call(
            "iam",
            "CreateRole",
            "POST",
            "/",
            "Action=CreateRole&Version=2010-05-08&RoleName=app&&Tags.member.1.Key=team&Tags.member.1.Value=web&MaxSessionDuration=3600",
        );
        role.request.content_type = Some("application/x-www-form-urlencoded; charset=utf-8".into());
        assert_eq!(
            aws_cli(&role).unwrap(),
            "aws iam create-role --max-session-duration 3600 --role-name 'app' --tags '[{\"Key\":\"team\",\"Value\":\"web\"}]'"
        );
        let py = boto3(&role).unwrap();
        assert!(
            py.contains("\"MaxSessionDuration\": 3600") && py.contains("\"RoleName\": \"app\""),
            "{py}"
        );
        let topic = rest_call(
            "sns",
            "CreateTopic",
            "POST",
            "/",
            "Action=CreateTopic&Name=news&Attributes.entry.1.key=DisplayName&Attributes.entry.1.value=News",
        );
        assert_eq!(
            aws_cli(&topic).unwrap(),
            "aws sns create-topic --attributes '{\"DisplayName\":\"News\"}' --name 'news'"
        );
    }

    #[test]
    fn cbor_calls_become_cli_and_curl_sends_their_bytes() {
        let raw: &[u8] = &[
            0xa1, 0x68, b'Q', b'u', b'e', b'u', b'e', b'U', b'r', b'l', 0x61, b'q',
        ];
        let mut call = rest_call(
            "sqs",
            "GetQueueAttributes",
            "POST",
            "/service/AmazonSQS/operation/GetQueueAttributes",
            r#"{"QueueUrl":"q"}"#,
        );
        call.request
            .headers
            .push(("smithy-protocol".into(), "rpc-v2-cbor".into()));
        call.request.content_type = Some("application/cbor".into());
        call.request.verbatim = false;
        call.request.bytes = raw.len();
        call.request.raw = Some(bytes::Bytes::from_static(raw));
        assert_eq!(
            aws_cli(&call).unwrap(),
            r#"aws sqs get-queue-attributes --cli-input-json '{"QueueUrl":"q"}'"#
        );
        let sent = curl(&call, "http://127.0.0.1:4510");
        assert!(
            sent.starts_with(r"printf '\241hQueueUrlaq' | curl -sS -X POST"),
            "{sent}"
        );
        assert!(
            sent.ends_with("--data-binary @-") && !sent.contains("QueueUrl\":"),
            "{sent}"
        );

        // Binary data whose note is as long as the data: never sent as the note.
        let mut binary = rest_call(
            "s3",
            "PutObject",
            "PUT",
            "/b/k",
            "(25 bytes of binary data)",
        );
        binary.request.verbatim = false;
        binary.request.complete = false;
        binary.request.bytes = 25;
        binary.request.raw = None;
        let sent = curl(&binary, "http://127.0.0.1:4510");
        assert!(
            !sent.contains("binary data") && sent.starts_with("# The request's 25-byte body"),
            "{sent}"
        );
        assert!(aws_cli(&binary).is_none() && boto3(&binary).is_none());
    }

    fn json_call(body: &str) -> Call {
        Call {
            seq: 0,
            id: 0,
            parent: None,
            internal: false,
            run: None,
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
            key: None,
            request_id: None,
            request: Exchange {
                headers: vec![
                    ("host".into(), "127.0.0.1:4510".into()),
                    ("x-amz-target".into(), "DynamoDB_20120810.PutItem".into()),
                ],
                content_type: Some("application/x-amz-json-1.0".into()),
                body: body.into(),
                bytes: body.len(),
                complete: true,
                verbatim: true,
                raw: None,
            },
            response: Exchange::default(),
        }
    }
}
