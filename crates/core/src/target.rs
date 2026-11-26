//! What a call is about, in a few words: `table/orders · id="ord-42"`,
//! `s3://shop-assets/receipts/1001.json`, `queue/fulfilment`. The one
//! implementation the terminal and the window share.

use serde_json::Value;

use crate::{trace::Trace, wire};

/// What `trace` targets, read from its request (and, failing that, its
/// first permission's resource); `None` when nothing better than the method
/// and path is known.
pub fn summary(trace: &Trace) -> Option<String> {
    let body_text = trace
        .request
        .body
        .as_ref()
        .and_then(|b| b.text.as_deref())
        .unwrap_or("")
        .trim();
    let json: Option<Value> = if body_text.starts_with('{') {
        serde_json::from_str(body_text).ok()
    } else {
        None
    };

    match trace.service.as_str() {
        "s3" => return Some(s3_target_summary(trace)),
        "dynamodb" | "streams.dynamodb" | "dynamodbstreams" => {
            if let Some(j) = json.as_ref()
                && let Some(summary) = dynamodb_target_summary(&trace.operation, j)
            {
                return Some(summary);
            }
        }
        "sqs" | "sns" => {
            if let Some(summary) = messaging_target(&trace.service, json.as_ref(), body_text) {
                return Some(summary);
            }
        }
        "lambda" => {
            if let Some(summary) = lambda_target_summary(trace, json.as_ref()) {
                return Some(summary);
            }
        }
        "kms" => {
            if let Some(summary) = kms_target_summary(trace, json.as_ref()) {
                return Some(summary);
            }
        }
        "iam" | "sts" => {
            if let Some(summary) = identity_target(json.as_ref(), body_text) {
                return Some(summary);
            }
        }
        "secretsmanager" => {
            if let Some(j) = json.as_ref()
                && let Some(id) = j
                    .get("SecretId")
                    .or_else(|| j.get("Name"))
                    .and_then(Value::as_str)
            {
                let short = id.rsplit(":secret:").next().unwrap_or(id);
                return Some(format!("secret/{short}"));
            }
        }
        "ssm" => {
            if let Some(j) = json.as_ref() {
                if let Some(name) = j.get("Name").and_then(Value::as_str) {
                    return Some(format!("param/{}", name.trim_start_matches('/')));
                }
                if let Some(names) = j.get("Names").and_then(Value::as_array) {
                    let list: Vec<&str> = names.iter().filter_map(Value::as_str).take(2).collect();
                    if !list.is_empty() {
                        return Some(format!(
                            "params: {} ({} total)",
                            list.join(", "),
                            names.len()
                        ));
                    }
                }
            }
        }
        "events" => {
            if let Some(j) = json.as_ref() {
                if let Some(entries) = j.get("Entries").and_then(Value::as_array) {
                    let bus = entries
                        .first()
                        .and_then(|e| e.get("EventBusName"))
                        .and_then(Value::as_str)
                        .unwrap_or("default");
                    return Some(format!("bus/{bus} · {} events", entries.len()));
                }
                if let Some(rule) = j
                    .get("Name")
                    .or_else(|| j.get("Rule"))
                    .and_then(Value::as_str)
                {
                    return Some(format!("rule/{rule}"));
                }
            }
        }
        "states" => {
            if let Some(j) = json.as_ref()
                && let Some(arn) = j
                    .get("stateMachineArn")
                    .or_else(|| j.get("executionArn"))
                    .or_else(|| j.get("name"))
                    .and_then(Value::as_str)
            {
                let short = arn.rsplit(':').next().unwrap_or(arn);
                return Some(format!("sm/{short}"));
            }
        }
        "logs" => {
            if let Some(j) = json.as_ref()
                && let Some(lg) = j.get("logGroupName").and_then(Value::as_str)
            {
                if let Some(ls) = j.get("logStreamName").and_then(Value::as_str) {
                    return Some(format!("{lg} · {ls}"));
                }
                return Some(lg.to_string());
            }
        }
        "bedrock" | "bedrock-runtime" => {
            let bare = trace.path.split('?').next().unwrap_or(&trace.path);
            let segs: Vec<&str> = bare.split('/').filter(|s| !s.is_empty()).collect();
            if segs.first() == Some(&"model")
                && let Some(mid) = segs.get(1)
            {
                return Some(format!("model/{}", wire::percent_decode_path(mid)));
            }
        }
        _ => {}
    }

    // Else the first permission's resource, when it names one.
    let first = trace.access.first().filter(|first| first.resource != "*")?;
    if let Some(s3) = first.resource.strip_prefix("arn:aws:s3:::") {
        return Some(format!("s3://{s3}"));
    }
    first
        .resource
        .splitn(6, ':')
        .nth(5)
        .filter(|rest| !rest.is_empty())
        .map(str::to_string)
}

/// `queue/{name}` (with a batch's message count) or `topic/{name}`.
fn messaging_target(service: &str, json: Option<&Value>, body_text: &str) -> Option<String> {
    let field = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            json.and_then(|j| j.get(*key))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| form_param(body_text, key))
        })
    };
    if service == "sqs" {
        let queue = field(&["QueueUrl", "QueueName"])?;
        let name = queue
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&queue);
        return Some(
            match json
                .and_then(|j| j.get("Entries"))
                .and_then(Value::as_array)
            {
                Some(entries) => format!("queue/{name} · {} msgs", entries.len()),
                None => format!("queue/{name}"),
            },
        );
    }
    let topic = field(&["TopicArn", "TargetArn", "Name"])?;
    Some(format!(
        "topic/{}",
        topic.rsplit(':').next().unwrap_or(&topic)
    ))
}

/// `fn/{name}` or `fn/{name}:{qualifier}`, from the path or the body.
fn lambda_target_summary(trace: &Trace, json: Option<&Value>) -> Option<String> {
    let bare = trace.path.split('?').next().unwrap_or(&trace.path);
    let segs: Vec<&str> = bare.split('/').filter(|s| !s.is_empty()).collect();
    let raw = segs
        .get(2)
        .filter(|_| segs.get(1) == Some(&"functions"))
        .map(|name| wire::percent_decode_path(name))
        .or_else(|| {
            json.and_then(|j| j.get("FunctionName"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })?;
    let mut name = raw
        .split_once(":function:")
        .map_or(raw.as_str(), |(_, rest)| rest)
        .to_string();
    if !name.contains(':')
        && let Some(qualifier) = wire::query_param(&trace.path, "Qualifier")
    {
        name = format!("{name}:{qualifier}");
    }
    Some(format!("fn/{name}"))
}

/// `alias/{name}` for a key named by alias (the IAM resource is the key it
/// hides), `key/{id}`, or the key spec of a new key.
fn kms_target_summary(trace: &Trace, json: Option<&Value>) -> Option<String> {
    let j = json?;
    if let Some(key) = ["KeyId", "TargetKeyId", "AliasName"]
        .iter()
        .find_map(|field| j.get(*field).and_then(Value::as_str))
    {
        if let Some((_, alias)) = key.split_once("alias/") {
            return Some(format!("alias/{alias}"));
        }
        return Some(format!("key/{}", key.rsplit('/').next().unwrap_or(key)));
    }
    if let Some(spec) = j
        .get("KeySpec")
        .or_else(|| j.get("CustomerMasterKeySpec"))
        .and_then(Value::as_str)
    {
        return Some(spec.to_string());
    }
    (trace.operation == "CreateKey").then(|| "SYMMETRIC_DEFAULT".to_string())
}

/// The role, user, group or policy an IAM or STS call names: an ARN's
/// resource (its path kept), or `{kind}/{name}`.
fn identity_target(json: Option<&Value>, body_text: &str) -> Option<String> {
    [
        ("RoleArn", ""),
        ("RoleName", "role"),
        ("UserName", "user"),
        ("GroupName", "group"),
        ("InstanceProfileName", "instance-profile"),
        ("PolicyArn", ""),
        ("PolicyName", "policy"),
    ]
    .iter()
    .find_map(|(key, kind)| {
        let value = json
            .and_then(|j| j.get(*key))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| form_param(body_text, key))?;
        Some(match value.splitn(6, ':').nth(5) {
            Some(resource) if value.starts_with("arn:") => resource.to_string(),
            _ if kind.is_empty() => value,
            _ => format!("{kind}/{value}"),
        })
    })
}

fn s3_target_summary(trace: &Trace) -> String {
    let (bare, _query) = trace.path.split_once('?').unwrap_or((&trace.path, ""));
    let vhost_bucket = wire::s3_host_bucket(&trace.host);

    let (bucket, key) = if let Some(b) = vhost_bucket {
        (
            b.to_string(),
            wire::percent_decode_path(bare.trim_start_matches('/')),
        )
    } else {
        let trimmed = bare.trim_start_matches('/');
        match trimmed.split_once('/') {
            Some((b, k)) => (wire::percent_decode_path(b), wire::percent_decode_path(k)),
            None => (wire::percent_decode_path(trimmed), String::new()),
        }
    };

    if bucket.is_empty() {
        return "s3://".to_string();
    }
    let mut out = if key.is_empty() {
        format!("s3://{bucket}")
    } else {
        format!("s3://{bucket}/{key}")
    };
    if let Some(prefix) = wire::query_param(&trace.path, "prefix").filter(|p| !p.is_empty()) {
        out.push_str(&format!(" · prefix=\"{prefix}\""));
    }
    out
}

fn dynamodb_target_summary(operation: &str, json: &Value) -> Option<String> {
    if let Some(table) = json.get("TableName").and_then(Value::as_str) {
        let mut out = format!("table/{table}");
        if let Some(idx) = json.get("IndexName").and_then(Value::as_str) {
            out.push_str(&format!("/index/{idx}"));
        }
        if let Some(key_obj) = json
            .get("Key")
            .or_else(|| json.get("Item"))
            .and_then(Value::as_object)
            && let Some((k, v)) = key_obj.iter().next()
            && let Some(scalar) = ddb_scalar_preview(v)
        {
            out.push_str(&format!(" · {k}={scalar}"));
        }
        return Some(out);
    }
    if let Some(req_items) = json.get("RequestItems").and_then(Value::as_object) {
        let tables: Vec<&str> = req_items.keys().map(String::as_str).collect();
        if !tables.is_empty() {
            return Some(format!("tables: {}", tables.join(", ")));
        }
    }
    if let Some(items) = json.get("TransactItems").and_then(Value::as_array) {
        return Some(format!("tx · {} items", items.len()));
    }
    if let Some(stmt) = json.get("Statement").and_then(Value::as_str) {
        let compact: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
        let _ = operation;
        return Some(compact);
    }
    None
}

fn ddb_scalar_preview(attr: &Value) -> Option<String> {
    let obj = attr.as_object()?;
    if let Some(s) = obj.get("S").and_then(Value::as_str) {
        let clipped: String = s.chars().take(24).collect();
        return Some(format!("\"{clipped}\""));
    }
    if let Some(n) = obj.get("N").and_then(Value::as_str) {
        return Some(n.to_string());
    }
    if let Some(b) = obj.get("BOOL").and_then(Value::as_bool) {
        return Some(b.to_string());
    }
    None
}

fn form_param(body: &str, key: &str) -> Option<String> {
    wire::form_field(body, key).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{BodyTrace, Message};

    #[test]
    fn names_what_a_call_targets() {
        let ddb_call = Trace {
            method: "POST".into(),
            path: "/".into(),
            service: "dynamodb".into(),
            operation: "GetItem".into(),
            request: Message {
                headers: Vec::new(),
                content_type: Some("application/x-amz-json-1.0".into()),
                body: Some(BodyTrace {
                    text: Some(r#"{"TableName":"orders","Key":{"orderId":{"S":"ord-42"}}}"#.into()),
                    bytes: 55,
                    ..Default::default()
                }),
            },
            ..Trace::default()
        };
        assert_eq!(
            summary(&ddb_call).as_deref(),
            Some("table/orders · orderId=\"ord-42\"")
        );

        // A qualified function keeps its qualifier; a key named by alias
        // shows the alias (its IAM resource is the key, `key/*`).
        for (path, expected) in [
            ("/2015-03-31/functions/f%3Aprod/invocations", "fn/f:prod"),
            ("/2015-03-31/functions/f/invocations?Qualifier=7", "fn/f:7"),
            (
                "/2015-03-31/functions/arn%3Aaws%3Alambda%3Aus-east-1%3A1%3Afunction%3Af%3Alive/invocations",
                "fn/f:live",
            ),
        ] {
            let invoke = Trace {
                path: path.into(),
                service: "lambda".into(),
                ..Trace::default()
            };
            assert_eq!(summary(&invoke).as_deref(), Some(expected));
        }
        let kms = Trace {
            service: "kms".into(),
            operation: "Encrypt".into(),
            request: Message {
                body: Some(BodyTrace {
                    text: Some(r#"{"KeyId":"arn:aws:kms:us-east-1:1:alias/app"}"#.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Trace::default()
        };
        assert_eq!(summary(&kms).as_deref(), Some("alias/app"));
        let iam = |form: &str| Trace {
            service: "iam".into(),
            operation: "Op".into(),
            request: Message {
                content_type: Some("application/x-www-form-urlencoded; charset=utf-8".into()),
                body: Some(BodyTrace {
                    text: Some(form.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Trace::default()
        };
        for (form, expected) in [
            ("Action=CreateUser&&UserName=bob", "user/bob"),
            ("Action=CreateGroup&GroupName=ops", "group/ops"),
            ("Action=CreatePolicy&PolicyName=read", "policy/read"),
            ("Action=PutRolePolicy&RoleName=fn&PolicyName=p", "role/fn"),
            (
                "Action=AssumeRole&RoleArn=arn%3Aaws%3Aiam%3A%3A0%3Arole%2Fsvc%2Fa%3Ab",
                "role/svc/a:b",
            ),
            (
                "Action=AttachUserPolicy&UserName=bob&PolicyArn=arn%3Aaws%3Aiam%3A%3A0%3Apolicy%2Fp",
                "user/bob",
            ),
        ] {
            assert_eq!(summary(&iam(form)).as_deref(), Some(expected), "{form}");
        }
        let bare = Trace {
            method: "GET".into(),
            path: "/".into(),
            service: "xray".into(),
            ..Trace::default()
        };
        assert_eq!(summary(&bare), None);
    }
}
