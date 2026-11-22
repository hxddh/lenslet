//! What exists in the local AWS, asked of the emulator itself: each service's
//! own List/Describe operations, called in-process (they never show up as
//! calls). Refreshed shortly after a call that may have changed something.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use serde_json::Value;
use tokio::sync::Notify;

use crate::{
    emulator::Emulator,
    session::{Group, Session},
};

/// Calls in a burst share one refresh.
const SETTLE: Duration = Duration::from_millis(150);
/// The most items a service lists; the window says when there are more.
const MAX_ITEMS: usize = 50;

#[derive(Clone, Default)]
pub struct Refresher {
    notify: Arc<Notify>,
}

impl Refresher {
    pub fn poke(&self) {
        self.notify.notify_one();
    }

    pub async fn run(self, emulator: Emulator, session: Session) {
        loop {
            self.notify.notified().await;
            tokio::time::sleep(SETTLE).await;
            let generation = session.generation();
            let groups = inventory(&emulator).await;
            // A fresh start during the refresh makes these stale.
            if session.generation() == generation {
                session.set_resources(groups);
            }
        }
    }
}

pub async fn inventory(emulator: &Emulator) -> Vec<Group> {
    let ask = Ask { emulator };
    let mut groups = Vec::new();
    let mut add = |service: &'static str, items: Vec<(String, Option<String>)>| {
        if !items.is_empty() {
            groups.push(Group { service, items });
        }
    };

    // S3: buckets and how many objects each holds.
    let mut buckets = Vec::new();
    if let Some(xml) = ask.rest("s3", "GET", "/").await {
        for name in tags(&xml, "Name").into_iter().take(MAX_ITEMS) {
            let count = ask
                .rest("s3", "GET", &format!("/{name}?list-type=2&max-keys=1000"))
                .await
                .and_then(|xml| tags(&xml, "KeyCount").first()?.parse::<usize>().ok());
            buckets.push((name, count.map(|n| objects(n))));
        }
    }
    add("S3", buckets);

    // DynamoDB: tables and their item counts.
    let mut tables = Vec::new();
    if let Some(v) = ask.json("dynamodb", "DynamoDB_20120810.ListTables", "1.0", "{}").await {
        for name in strings(&v["TableNames"]).into_iter().take(MAX_ITEMS) {
            let body = serde_json::json!({"TableName": name, "Select": "COUNT"}).to_string();
            let count = ask
                .json("dynamodb", "DynamoDB_20120810.Scan", "1.0", &body)
                .await
                .and_then(|v| v["Count"].as_u64());
            tables.push((name, count.map(|n| items(n as usize))));
        }
    }
    add("DynamoDB", tables);

    // SQS: queues and the messages waiting in them.
    let mut queues = Vec::new();
    if let Some(v) = ask.json("sqs", "AmazonSQS.ListQueues", "1.0", "{}").await {
        for url in strings(&v["QueueUrls"]).into_iter().take(MAX_ITEMS) {
            let body = serde_json::json!({"QueueUrl": url, "AttributeNames": ["ApproximateNumberOfMessages"]}).to_string();
            let waiting = ask
                .json("sqs", "AmazonSQS.GetQueueAttributes", "1.0", &body)
                .await
                .and_then(|v| v["Attributes"]["ApproximateNumberOfMessages"].as_str()?.parse::<usize>().ok());
            let name = url.rsplit('/').next().unwrap_or(&url).to_string();
            queues.push((name, waiting.map(|n| messages(n))));
        }
    }
    add("SQS", queues);

    add("SNS", names(ask.query("sns", "Action=ListTopics&Version=2010-03-31").await, "TopicArn", true));
    add(
        "Lambda",
        ask.rest_json("lambda", "GET", "/2015-03-31/functions/")
            .await
            .map(|v| field_list(&v["Functions"], "FunctionName"))
            .unwrap_or_default(),
    );
    add(
        "Secrets Manager",
        ask.json("secretsmanager", "secretsmanager.ListSecrets", "1.1", "{}")
            .await
            .map(|v| field_list(&v["SecretList"], "Name"))
            .unwrap_or_default(),
    );
    add(
        "SSM",
        ask.json("ssm", "AmazonSSM.DescribeParameters", "1.1", "{}")
            .await
            .map(|v| field_list(&v["Parameters"], "Name"))
            .unwrap_or_default(),
    );
    add(
        "EventBridge",
        ask.json("events", "AWSEvents.ListRules", "1.1", "{}")
            .await
            .map(|v| field_list(&v["Rules"], "Name"))
            .unwrap_or_default(),
    );
    add(
        "Step Functions",
        ask.json("states", "AWSStepFunctions.ListStateMachines", "1.0", "{}")
            .await
            .map(|v| field_list(&v["stateMachines"], "name"))
            .unwrap_or_default(),
    );
    add(
        "Kinesis",
        ask.json("kinesis", "Kinesis_20131202.ListStreams", "1.1", "{}")
            .await
            .map(|v| strings(&v["StreamNames"]).into_iter().map(|n| (n, None)).collect())
            .unwrap_or_default(),
    );
    add(
        "CloudWatch Logs",
        ask.json("logs", "Logs_20140328.DescribeLogGroups", "1.1", "{}")
            .await
            .map(|v| field_list(&v["logGroups"], "logGroupName"))
            .unwrap_or_default(),
    );
    add(
        "KMS",
        ask.json("kms", "TrentService.ListAliases", "1.1", "{}")
            .await
            .map(|v| field_list(&v["Aliases"], "AliasName"))
            .unwrap_or_default(),
    );
    add("IAM", names(ask.query("iam", "Action=ListRoles&Version=2010-05-08").await, "RoleName", false));
    groups
}

fn objects(n: usize) -> String {
    if n == 1 { "1 object".into() } else { format!("{n} objects") }
}
fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}
fn messages(n: usize) -> String {
    if n == 1 { "1 message".into() } else { format!("{n} messages") }
}

struct Ask<'a> {
    emulator: &'a Emulator,
}

impl Ask<'_> {
    async fn send(&self, service: &str, request: http::request::Builder, body: &str) -> Option<String> {
        // Auth is off; the credential scope only tells the router the service.
        let scope = format!(
            "AWS4-HMAC-SHA256 Credential=lenslet/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
        );
        let request = request
            .header("authorization", scope)
            .header("host", "localhost")
            .body(Bytes::from(body.to_string()))
            .ok()?;
        let response = self.emulator.handle(request).await;
        if !response.status().is_success() {
            return None;
        }
        String::from_utf8(response.into_body().to_vec()).ok()
    }

    async fn rest(&self, service: &str, method: &str, path: &str) -> Option<String> {
        self.send(service, http::Request::builder().method(method).uri(path), "").await
    }

    async fn rest_json(&self, service: &str, method: &str, path: &str) -> Option<Value> {
        serde_json::from_str(&self.rest(service, method, path).await?).ok()
    }

    async fn json(&self, service: &str, target: &str, version: &str, body: &str) -> Option<Value> {
        let request = http::Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", format!("application/x-amz-json-{version}"))
            .header("x-amz-target", target);
        serde_json::from_str(&self.send(service, request, body).await?).ok()
    }

    async fn query(&self, service: &str, form: &str) -> Option<String> {
        let request = http::Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/x-www-form-urlencoded");
        self.send(service, request, form).await
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn field_list(value: &Value, field: &str) -> Vec<(String, Option<String>)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v[field].as_str().map(|s| (s.to_string(), None)))
        .take(MAX_ITEMS)
        .collect()
}

fn names(xml: Option<String>, tag: &str, arn_tail: bool) -> Vec<(String, Option<String>)> {
    xml.map(|xml| {
        tags(&xml, tag)
            .into_iter()
            .take(MAX_ITEMS)
            .map(|name| {
                let name = if arn_tail { name.rsplit(':').next().unwrap_or(&name).to_string() } else { name };
                (name, None)
            })
            .collect()
    })
    .unwrap_or_default()
}

fn tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        rest = &rest[i + open.len()..];
        let Some(j) = rest.find(&close) else { break };
        out.push(rest[..j].to_string());
        rest = &rest[j + close.len()..];
    }
    out
}
