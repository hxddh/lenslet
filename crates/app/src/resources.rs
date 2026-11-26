//! What exists in the local AWS, asked of the emulator itself with each
//! service's own List, Describe and Get operations, called in-process (they
//! never show up as calls): the inventory, refreshed shortly after a call that
//! may have changed something, and one resource's contents when the window
//! opens it.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
    emulator::Emulator,
    session::{Group, Resource, Sent, Session},
};

/// Calls in a burst share one refresh.
const SETTLE: Duration = Duration::from_millis(150);
/// The most items a service lists.
const MAX_ITEMS: usize = 200;
/// The most rows one resource's contents show.
const MAX_ROWS: usize = 500;
/// The most text an object, secret or parameter shows.
const MAX_TEXT: usize = 256 * 1024;

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
            let started = std::time::Instant::now();
            let groups = inventory(&emulator).await;
            // A fresh start during the refresh makes these stale.
            if session.generation() == generation {
                session.set_resources(groups);
            }
            // Under a stream of changes, look again only after resting as
            // long as looking took: the calls themselves keep most of the
            // engine.
            tokio::time::sleep(started.elapsed().min(std::time::Duration::from_secs(5))).await;
        }
    }
}

/// The signing names of the services the inventory lists, in its order.
pub const LISTED: &[&str] = &[
    "s3",
    "dynamodb",
    "sqs",
    "sns",
    "lambda",
    "logs",
    "secretsmanager",
    "ssm",
    "events",
    "states",
    "kinesis",
    "kms",
    "iam",
];

pub async fn inventory(emulator: &Emulator) -> Vec<Group> {
    let ask = Ask { emulator };
    let mut groups = Vec::new();
    let mut add = |service: &'static str, items: Vec<Resource>| {
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
                .and_then(|xml| tags(&xml, "KeyCount").first()?.parse::<u64>().ok());
            buckets.push(Resource {
                id: name.clone(),
                name,
                count,
            });
        }
    }
    add("s3", buckets);

    // DynamoDB: tables and their item counts.
    let mut tables = Vec::new();
    if let Some(v) = ask
        .json("dynamodb", "DynamoDB_20120810.ListTables", "1.0", json!({}))
        .await
    {
        for name in strings(&v["TableNames"]).into_iter().take(MAX_ITEMS) {
            let count = ask
                .json(
                    "dynamodb",
                    "DynamoDB_20120810.Scan",
                    "1.0",
                    json!({"TableName": name, "Select": "COUNT"}),
                )
                .await
                .and_then(|v| v["Count"].as_u64());
            tables.push(Resource {
                id: name.clone(),
                name,
                count,
            });
        }
    }
    add("dynamodb", tables);

    // SQS: queues and the messages waiting in them.
    let mut queues = Vec::new();
    if let Some(v) = ask
        .json("sqs", "AmazonSQS.ListQueues", "1.0", json!({}))
        .await
    {
        for url in strings(&v["QueueUrls"]).into_iter().take(MAX_ITEMS) {
            let waiting = ask
                .json(
                    "sqs",
                    "AmazonSQS.GetQueueAttributes",
                    "1.0",
                    json!({"QueueUrl": url, "AttributeNames": ["ApproximateNumberOfMessages"]}),
                )
                .await
                .and_then(|v| {
                    v["Attributes"]["ApproximateNumberOfMessages"]
                        .as_str()?
                        .parse::<u64>()
                        .ok()
                });
            let name = url.rsplit('/').next().unwrap_or(&url).to_string();
            queues.push(Resource {
                name,
                id: url,
                count: waiting,
            });
        }
    }
    add("sqs", queues);

    add(
        "sns",
        ask.query("sns", "Action=ListTopics&Version=2010-03-31")
            .await
            .map(|xml| {
                tags(&xml, "TopicArn")
                    .into_iter()
                    .map(|arn| named_by_arn(&arn))
                    .collect()
            })
            .unwrap_or_default(),
    );
    add(
        "lambda",
        ask.rest_json("lambda", "GET", "/2015-03-31/functions/")
            .await
            .map(|v| list(&v["Functions"], "FunctionName", "FunctionName"))
            .unwrap_or_default(),
    );
    add(
        "logs",
        ask.json("logs", "Logs_20140328.DescribeLogGroups", "1.1", json!({}))
            .await
            .map(|v| list(&v["logGroups"], "logGroupName", "logGroupName"))
            .unwrap_or_default(),
    );
    add(
        "secretsmanager",
        ask.json(
            "secretsmanager",
            "secretsmanager.ListSecrets",
            "1.1",
            json!({}),
        )
        .await
        .map(|v| list(&v["SecretList"], "Name", "ARN"))
        .unwrap_or_default(),
    );
    add(
        "ssm",
        ask.json("ssm", "AmazonSSM.DescribeParameters", "1.1", json!({}))
            .await
            .map(|v| list(&v["Parameters"], "Name", "Name"))
            .unwrap_or_default(),
    );
    add(
        "events",
        ask.json("events", "AWSEvents.ListRules", "1.1", json!({}))
            .await
            .map(|v| list(&v["Rules"], "Name", "Name"))
            .unwrap_or_default(),
    );
    add(
        "states",
        ask.json(
            "states",
            "AWSStepFunctions.ListStateMachines",
            "1.0",
            json!({}),
        )
        .await
        .map(|v| list(&v["stateMachines"], "name", "stateMachineArn"))
        .unwrap_or_default(),
    );
    add(
        "kinesis",
        ask.json("kinesis", "Kinesis_20131202.ListStreams", "1.1", json!({}))
            .await
            .map(|v| {
                strings(&v["StreamNames"])
                    .into_iter()
                    .map(|n| Resource {
                        id: n.clone(),
                        name: n,
                        count: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    add(
        "kms",
        ask.json("kms", "TrentService.ListAliases", "1.1", json!({}))
            .await
            .map(|v| {
                v["Aliases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    // AWS's own aliases exist in every account.
                    .filter(|a| {
                        !a["AliasName"]
                            .as_str()
                            .unwrap_or_default()
                            .starts_with("alias/aws/")
                    })
                    .filter_map(|a| {
                        Some(Resource {
                            name: a["AliasName"].as_str()?.to_string(),
                            id: a["TargetKeyId"].as_str().unwrap_or_default().to_string(),
                            count: None,
                        })
                    })
                    .take(MAX_ITEMS)
                    .collect()
            })
            .unwrap_or_default(),
    );
    add(
        "iam",
        ask.query("iam", "Action=ListRoles&Version=2010-05-08")
            .await
            .map(|xml| {
                tags(&xml, "RoleName")
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(|n| Resource {
                        id: n.clone(),
                        name: n,
                        count: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    groups
}

/// One resource's contents, as the window shows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contents {
    /// Name and value pairs: configuration, attributes.
    pub fields: Vec<(String, String)>,
    /// Rows under named columns: objects, items, log events, executions.
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// More rows exist than are shown.
    pub truncated: bool,
    /// A value: a secret's, a parameter's, an object's.
    pub text: Option<String>,
}

/// What `resource` of `service` holds.
pub async fn contents(emulator: &Emulator, service: &str, resource: &Resource) -> Contents {
    let ask = Ask { emulator };
    let id = resource.id.as_str();
    let mut out = Contents::default();
    match service {
        "s3" => {
            if let Some(xml) = ask
                .rest(
                    "s3",
                    "GET",
                    &format!("/{id}?list-type=2&max-keys={MAX_ROWS}"),
                )
                .await
            {
                out.columns = vec!["Key".into(), "Size".into(), "Last modified".into()];
                for object in blocks(&xml, "Contents") {
                    out.rows.push(vec![
                        first_tag(&object, "Key"),
                        first_tag(&object, "Size")
                            .parse::<u64>()
                            .map(bytes_label)
                            .unwrap_or_default(),
                        first_tag(&object, "LastModified"),
                    ]);
                }
                out.truncated = first_tag(&xml, "IsTruncated") == "true";
            }
        }
        "dynamodb" => {
            if let Some(v) = ask
                .json(
                    "dynamodb",
                    "DynamoDB_20120810.DescribeTable",
                    "1.0",
                    json!({"TableName": id}),
                )
                .await
            {
                let table = &v["Table"];
                let keys: Vec<String> = table["KeySchema"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|k| {
                        Some(format!(
                            "{} ({})",
                            k["AttributeName"].as_str()?,
                            k["KeyType"].as_str()?
                        ))
                    })
                    .collect();
                out.fields.push(("Key".into(), keys.join(", ")));
                push_str(&mut out.fields, "Status", &table["TableStatus"]);
                push_str(
                    &mut out.fields,
                    "Billing",
                    &table["BillingModeSummary"]["BillingMode"],
                );
                push_str(&mut out.fields, "ARN", &table["TableArn"]);
                let key_names: Vec<String> = table["KeySchema"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|k| k["AttributeName"].as_str().map(str::to_string))
                    .collect();
                if let Some(scan) = ask
                    .json(
                        "dynamodb",
                        "DynamoDB_20120810.Scan",
                        "1.0",
                        json!({"TableName": id, "Limit": MAX_ROWS}),
                    )
                    .await
                {
                    let items: Vec<&serde_json::Map<String, Value>> = scan["Items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object)
                        .collect();
                    // Key attributes first, then the others in first-seen order.
                    let mut columns = key_names;
                    for item in &items {
                        for name in item.keys() {
                            if !columns.contains(name) {
                                columns.push(name.clone());
                            }
                        }
                    }
                    out.rows = items
                        .iter()
                        .map(|item| {
                            columns
                                .iter()
                                .map(|c| item.get(c).map(attribute_label).unwrap_or_default())
                                .collect()
                        })
                        .collect();
                    out.columns = columns;
                    out.truncated = scan.get("LastEvaluatedKey").is_some();
                }
            }
        }
        "sqs" => {
            if let Some(v) = ask
                .json(
                    "sqs",
                    "AmazonSQS.GetQueueAttributes",
                    "1.0",
                    json!({"QueueUrl": id, "AttributeNames": ["All"]}),
                )
                .await
            {
                out.fields.push(("URL".into(), id.to_string()));
                if let Some(attributes) = v["Attributes"].as_object() {
                    for (name, value) in attributes {
                        out.fields
                            .push((name.clone(), value.as_str().unwrap_or_default().to_string()));
                    }
                }
            }
        }
        "sns" => {
            out.fields.push(("ARN".into(), id.to_string()));
            if let Some(xml) = ask
                .query(
                    "sns",
                    &format!(
                        "Action=ListSubscriptionsByTopic&Version=2010-03-31&TopicArn={}",
                        encode(id)
                    ),
                )
                .await
            {
                out.columns = vec!["Protocol".into(), "Endpoint".into(), "Subscription".into()];
                for member in blocks(&xml, "member") {
                    out.rows.push(vec![
                        first_tag(&member, "Protocol"),
                        first_tag(&member, "Endpoint"),
                        first_tag(&member, "SubscriptionArn"),
                    ]);
                }
            }
        }
        "lambda" => {
            if let Some(v) = ask
                .rest_json("lambda", "GET", &format!("/2015-03-31/functions/{id}"))
                .await
            {
                let c = &v["Configuration"];
                for (label, key) in [
                    ("Runtime", "Runtime"),
                    ("Handler", "Handler"),
                    ("Memory (MB)", "MemorySize"),
                    ("Timeout (s)", "Timeout"),
                    ("Role", "Role"),
                    ("State", "State"),
                    ("Last modified", "LastModified"),
                    ("ARN", "FunctionArn"),
                ] {
                    push_str(&mut out.fields, label, &c[key]);
                }
                if let Some(vars) = c["Environment"]["Variables"].as_object() {
                    for (name, value) in vars {
                        out.fields.push((
                            format!("${name}"),
                            value.as_str().unwrap_or_default().to_string(),
                        ));
                    }
                }
            }
        }
        "logs" => {
            if let Some(v) = ask
                .json(
                    "logs",
                    "Logs_20140328.FilterLogEvents",
                    "1.1",
                    json!({"logGroupName": id, "limit": MAX_ROWS}),
                )
                .await
            {
                out.columns = vec!["Time".into(), "Stream".into(), "Message".into()];
                for e in v["events"].as_array().into_iter().flatten() {
                    out.rows.push(vec![
                        e["timestamp"].as_u64().map(clock).unwrap_or_default(),
                        e["logStreamName"].as_str().unwrap_or_default().to_string(),
                        e["message"]
                            .as_str()
                            .unwrap_or_default()
                            .trim_end()
                            .to_string(),
                    ]);
                }
                out.truncated = v.get("nextToken").is_some_and(|t| !t.is_null());
            }
        }
        "secretsmanager" => {
            if let Some(v) = ask
                .json(
                    "secretsmanager",
                    "secretsmanager.DescribeSecret",
                    "1.1",
                    json!({"SecretId": id}),
                )
                .await
            {
                push_str(&mut out.fields, "ARN", &v["ARN"]);
                push_str(&mut out.fields, "Description", &v["Description"]);
                push_str(&mut out.fields, "KMS key", &v["KmsKeyId"]);
            }
            if let Some(v) = ask
                .json(
                    "secretsmanager",
                    "secretsmanager.GetSecretValue",
                    "1.1",
                    json!({"SecretId": id}),
                )
                .await
            {
                out.text = v["SecretString"].as_str().map(|s| pretty_text(s));
            }
        }
        "ssm" => {
            if let Some(v) = ask
                .json(
                    "ssm",
                    "AmazonSSM.GetParameter",
                    "1.1",
                    json!({"Name": id, "WithDecryption": true}),
                )
                .await
            {
                let p = &v["Parameter"];
                push_str(&mut out.fields, "Type", &p["Type"]);
                push_str(&mut out.fields, "Version", &p["Version"]);
                push_str(&mut out.fields, "ARN", &p["ARN"]);
                out.text = p["Value"].as_str().map(|s| pretty_text(s));
            }
        }
        "events" => {
            if let Some(v) = ask
                .json(
                    "events",
                    "AWSEvents.DescribeRule",
                    "1.1",
                    json!({"Name": id}),
                )
                .await
            {
                push_str(&mut out.fields, "State", &v["State"]);
                push_str(&mut out.fields, "Schedule", &v["ScheduleExpression"]);
                push_str(&mut out.fields, "Event bus", &v["EventBusName"]);
                push_str(&mut out.fields, "ARN", &v["Arn"]);
                out.text = v["EventPattern"].as_str().map(|s| pretty_text(s));
            }
            if let Some(v) = ask
                .json(
                    "events",
                    "AWSEvents.ListTargetsByRule",
                    "1.1",
                    json!({"Rule": id}),
                )
                .await
            {
                out.columns = vec!["Target".into(), "ARN".into()];
                for t in v["Targets"].as_array().into_iter().flatten() {
                    out.rows.push(vec![
                        t["Id"].as_str().unwrap_or_default().to_string(),
                        t["Arn"].as_str().unwrap_or_default().to_string(),
                    ]);
                }
            }
        }
        "states" => {
            out.fields.push(("ARN".into(), id.to_string()));
            if let Some(v) = ask
                .json(
                    "states",
                    "AWSStepFunctions.ListExecutions",
                    "1.0",
                    json!({"stateMachineArn": id, "maxResults": 100}),
                )
                .await
            {
                out.columns = vec!["Execution".into(), "Status".into(), "Started".into()];
                for e in v["executions"].as_array().into_iter().flatten() {
                    out.rows.push(vec![
                        e["name"].as_str().unwrap_or_default().to_string(),
                        e["status"].as_str().unwrap_or_default().to_string(),
                        epoch_label(&e["startDate"]),
                    ]);
                }
            }
        }
        "kinesis" => {
            if let Some(v) = ask
                .json(
                    "kinesis",
                    "Kinesis_20131202.DescribeStreamSummary",
                    "1.1",
                    json!({"StreamName": id}),
                )
                .await
            {
                let d = &v["StreamDescriptionSummary"];
                for (label, key) in [
                    ("Status", "StreamStatus"),
                    ("Open shards", "OpenShardCount"),
                    ("Retention (hours)", "RetentionPeriodHours"),
                    ("ARN", "StreamARN"),
                ] {
                    push_str(&mut out.fields, label, &d[key]);
                }
            }
        }
        "kms" => {
            if let Some(v) = ask
                .json(
                    "kms",
                    "TrentService.DescribeKey",
                    "1.1",
                    json!({"KeyId": id}),
                )
                .await
            {
                let k = &v["KeyMetadata"];
                for (label, key) in [
                    ("Key", "KeyId"),
                    ("State", "KeyState"),
                    ("Usage", "KeyUsage"),
                    ("Spec", "KeySpec"),
                    ("ARN", "Arn"),
                ] {
                    push_str(&mut out.fields, label, &k[key]);
                }
            }
        }
        "iam" => {
            if let Some(xml) = ask
                .query(
                    "iam",
                    &format!("Action=GetRole&Version=2010-05-08&RoleName={}", encode(id)),
                )
                .await
            {
                out.fields.push(("ARN".into(), first_tag(&xml, "Arn")));
                out.fields
                    .push(("Created".into(), first_tag(&xml, "CreateDate")));
                let trust = percent_decode(&first_tag(&xml, "AssumeRolePolicyDocument"));
                if !trust.is_empty() {
                    out.text = Some(pretty_text(&trust));
                }
            }
            if let Some(xml) = ask
                .query(
                    "iam",
                    &format!(
                        "Action=ListAttachedRolePolicies&Version=2010-05-08&RoleName={}",
                        encode(id)
                    ),
                )
                .await
            {
                for member in blocks(&xml, "member") {
                    out.rows.push(vec![
                        first_tag(&member, "PolicyName"),
                        "Managed".into(),
                        first_tag(&member, "PolicyArn"),
                    ]);
                }
            }
            // Its own policies too: what Lenslet allowed it among them.
            if let Some(xml) = ask
                .query(
                    "iam",
                    &format!(
                        "Action=ListRolePolicies&Version=2010-05-08&RoleName={}",
                        encode(id)
                    ),
                )
                .await
            {
                for name in tags(&first_tag_raw(&xml, "PolicyNames"), "member") {
                    let document = ask
                        .query(
                            "iam",
                            &format!(
                                "Action=GetRolePolicy&Version=2010-05-08&RoleName={}&PolicyName={}",
                                encode(id),
                                encode(&name)
                            ),
                        )
                        .await
                        .map(|xml| {
                            compact_json(&percent_decode(&first_tag(&xml, "PolicyDocument")))
                        })
                        .unwrap_or_default();
                    out.rows.push(vec![name, "Inline".into(), document]);
                }
            }
            out.columns = vec!["Policy".into(), "Type".into(), "ARN or document".into()];
        }
        _ => {}
    }
    out.fields.retain(|(_, v)| !v.is_empty());
    out
}

/// Allow the role `role` what `accesses` need: its inline policy for their
/// action, with what it allowed before. Answers the call that did it, to keep
/// in the account's history.
pub async fn grant(
    emulator: &Emulator,
    role: &str,
    accesses: &[lenslet_core::access::Access],
) -> Result<Sent, String> {
    let ask = Ask { emulator };
    // One inline policy per action, so different grants add up.
    let name = accesses
        .first()
        .map(|a| format!("lenslet-{}", a.action.replace([':', '*'], "-")))
        .unwrap_or_else(|| "lenslet-granted".into());
    // The same action allowed on another resource before: both stay.
    let mut allowed = accesses.to_vec();
    if let Some(xml) = ask
        .query(
            "iam",
            &format!(
                "Action=GetRolePolicy&Version=2010-05-08&RoleName={}&PolicyName={}",
                encode(role),
                encode(&name)
            ),
        )
        .await
    {
        let document = percent_decode(&first_tag(&xml, "PolicyDocument"));
        allowed.extend(lenslet_core::policy::allowed(&document));
    }
    let policy = lenslet_core::policy::compile(&allowed);
    let form = format!(
        "Action=PutRolePolicy&Version=2010-05-08&RoleName={}&PolicyName={}&PolicyDocument={}",
        encode(role),
        encode(&name),
        encode(&policy)
    );
    let request = Ask::request("iam", query_request(), form)
        .ok_or_else(|| format!("could not change the role {role}"))?;
    let mut sent = Sent::of(&request, crate::session::now_ms());
    sent.grant = Some(role.to_string());
    if emulator.handle(request).await.status().is_success() {
        Ok(sent)
    } else {
        Err(format!("could not change the role {role}"))
    }
}

/// The role a call denied to an assumed role ran as: `assumed-role/<role>/...`.
pub fn denied_role(message: &str) -> Option<String> {
    let after = message.split("assumed-role/").nth(1)?;
    let role = after.split('/').next()?.trim();
    (!role.is_empty()).then(|| role.to_string())
}

/// An S3 object's text, when it is text.
pub async fn object_text(emulator: &Emulator, bucket: &str, key: &str) -> Option<String> {
    let ask = Ask { emulator };
    let path = format!("/{bucket}/{}", encode_path(key));
    let body = ask.rest("s3", "GET", &path).await?;
    Some(pretty_text(&body))
}

struct Ask<'a> {
    emulator: &'a Emulator,
}

fn query_request() -> http::request::Builder {
    http::Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/x-www-form-urlencoded")
}

impl Ask<'_> {
    fn request(
        service: &str,
        request: http::request::Builder,
        body: String,
    ) -> Option<http::Request<Bytes>> {
        // Auth is off; the credential scope only tells the router the service.
        let scope = format!(
            "AWS4-HMAC-SHA256 Credential=lenslet/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
        );
        request
            .header("authorization", scope)
            .header("host", "localhost")
            .body(Bytes::from(body))
            .ok()
    }

    async fn send(
        &self,
        service: &str,
        request: http::request::Builder,
        body: String,
    ) -> Option<String> {
        let request = Self::request(service, request, body)?;
        let response = self.emulator.handle(request).await;
        if !response.status().is_success() {
            return None;
        }
        String::from_utf8(response.into_body().to_vec()).ok()
    }

    async fn rest(&self, service: &str, method: &str, path: &str) -> Option<String> {
        self.send(
            service,
            http::Request::builder().method(method).uri(path),
            String::new(),
        )
        .await
    }

    async fn rest_json(&self, service: &str, method: &str, path: &str) -> Option<Value> {
        serde_json::from_str(&self.rest(service, method, path).await?).ok()
    }

    async fn json(&self, service: &str, target: &str, version: &str, body: Value) -> Option<Value> {
        let request = http::Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", format!("application/x-amz-json-{version}"))
            .header("x-amz-target", target);
        serde_json::from_str(&self.send(service, request, body.to_string()).await?).ok()
    }

    async fn query(&self, service: &str, form: &str) -> Option<String> {
        self.send(service, query_request(), form.to_string()).await
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

fn list(value: &Value, name: &str, id: &str) -> Vec<Resource> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let name = v[name].as_str()?.to_string();
            let id = v[id]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| name.clone());
            Some(Resource {
                name,
                id,
                count: None,
            })
        })
        .take(MAX_ITEMS)
        .collect()
}

fn named_by_arn(arn: &str) -> Resource {
    Resource {
        name: arn.rsplit(':').next().unwrap_or(arn).to_string(),
        id: arn.to_string(),
        count: None,
    }
}

fn push_str(fields: &mut Vec<(String, String)>, label: &str, value: &Value) {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return,
    };
    fields.push((label.to_string(), text));
}

/// A DynamoDB attribute value as one line: `"o-1"`, `30`, `[2 items]`.
fn attribute_label(value: &Value) -> String {
    let Some((kind, inner)) = value.as_object().and_then(|o| o.iter().next()) else {
        return String::new();
    };
    match kind.as_str() {
        "S" => inner.as_str().unwrap_or_default().to_string(),
        "N" => inner.as_str().unwrap_or_default().to_string(),
        "BOOL" => inner.as_bool().map(|b| b.to_string()).unwrap_or_default(),
        "NULL" => "null".into(),
        "B" => "(binary)".into(),
        "SS" | "NS" | "BS" | "L" => format!("[{}]", inner.as_array().map_or(0, Vec::len)),
        "M" => {
            let fields: Vec<String> = inner
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| format!("{k}: {}", attribute_label(v)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        _ => inner.to_string(),
    }
}

/// Text as it reads best: JSON pretty-printed, capped.
fn pretty_text(text: &str) -> String {
    let mut out = serde_json::from_str::<Value>(text)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_string());
    if out.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

pub fn bytes_label(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / 1024.0 / 1024.0),
    }
}

/// `14:03:07` from milliseconds since the epoch (UTC).
pub fn clock(ms: u64) -> String {
    let s = ms / 1000 % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn epoch_label(value: &Value) -> String {
    value
        .as_f64()
        .map(|s| clock((s * 1000.0) as u64))
        .unwrap_or_default()
}

fn tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        rest = &rest[i + open.len()..];
        let Some(j) = rest.find(&close) else { break };
        out.push(xml_unescape(&rest[..j]));
        rest = &rest[j + close.len()..];
    }
    out
}

fn first_tag(xml: &str, tag: &str) -> String {
    tags(xml, tag).into_iter().next().unwrap_or_default()
}

/// The raw XML inside the first `<tag>…</tag>` block.
fn first_tag_raw(xml: &str, tag: &str) -> String {
    blocks(xml, tag).into_iter().next().unwrap_or_default()
}

/// JSON on one line; other text as it is.
fn compact_json(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| text.to_string())
}

/// The raw XML of each `<tag>…</tag>` block.
fn blocks(xml: &str, tag: &str) -> Vec<String> {
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

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_path(s: &str) -> String {
    s.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (digit(bytes[i + 1]), digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_dynamodb_attributes_on_one_line() {
        assert_eq!(attribute_label(&json!({"S": "o-1"})), "o-1");
        assert_eq!(attribute_label(&json!({"N": "30"})), "30");
        assert_eq!(
            attribute_label(&json!({"L": [{"S": "a"}, {"S": "b"}]})),
            "[2]"
        );
        assert_eq!(attribute_label(&json!({"M": {"a": {"N": "1"}}})), "{a: 1}");
    }

    #[test]
    fn finds_the_role_a_call_was_denied_to() {
        let message = "User: arn:aws:sts::000000000000:assumed-role/resize-role/resize is not authorized to perform: dynamodb:PutItem";
        assert_eq!(denied_role(message).as_deref(), Some("resize-role"));
        assert_eq!(
            denied_role("User: arn:aws:iam::0:user/bob is not authorized"),
            None
        );
    }

    #[test]
    fn reads_xml_blocks_and_escapes() {
        let xml = "<r><member><Key>a&amp;b</Key></member><member><Key>c</Key></member></r>";
        let keys: Vec<String> = blocks(xml, "member")
            .iter()
            .map(|m| first_tag(m, "Key"))
            .collect();
        assert_eq!(keys, vec!["a&b", "c"]);
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D"), "{\"a\":1}");
        assert_eq!(
            encode("arn:aws:sns:us-east-1:0:t"),
            "arn%3Aaws%3Asns%3Aus-east-1%3A0%3At"
        );
        assert_eq!(clock(3_723_000), "01:02:03");
    }
}
