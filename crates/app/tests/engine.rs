//! The engine end to end: real HTTP into the embedded emulator, the call it
//! records, the resources it finds, and a fresh start.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use lenslet::engine::Engine;

/// One HTTP/1.1 request; returns the status and the body.
fn send(
    port: u16,
    service: &str,
    target: Option<&str>,
    content_type: &str,
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut request = format!(
        "POST / HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\ncontent-type: {content_type}\r\n\
         authorization: AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0\r\n\
         content-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    if let Some(target) = target {
        request.push_str(&format!("x-amz-target: {target}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn sqs(port: u16, op: &str, body: &str) -> (u16, String) {
    send(
        port,
        "sqs",
        Some(&format!("AmazonSQS.{op}")),
        "application/x-amz-json-1.0",
        body,
    )
}

fn ddb(port: u16, op: &str, body: &str) -> (u16, String) {
    send(
        port,
        "dynamodb",
        Some(&format!("DynamoDB_20120810.{op}")),
        "application/x-amz-json-1.0",
        body,
    )
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn records_calls_finds_resources_explains_failures_and_starts_fresh() {
    let engine = Engine::start(0).expect("start");
    let port = engine.port;

    // A call is recorded with its service, operation and IAM access.
    let (status, _) = sqs(port, "CreateQueue", r#"{"QueueName":"orders"}"#);
    assert_eq!(status, 200);
    let call = engine.snapshot().calls[0].clone();
    assert_eq!(
        (call.service.as_str(), call.operation.as_str()),
        ("sqs", "CreateQueue")
    );
    assert_eq!(call.access[0].action, "sqs:CreateQueue");
    assert!(
        call.access[0].resource.ends_with(":orders"),
        "{:?}",
        call.access
    );

    // The emulator itself says the queue exists, and its URL names Lenslet.
    wait_for("the queue in the resources", || {
        engine
            .snapshot()
            .resources
            .iter()
            .any(|g| g.service == "sqs" && g.items.iter().any(|r| r.name == "orders"))
    });
    let (_, body) = sqs(port, "GetQueueUrl", r#"{"QueueName":"orders"}"#);
    assert!(
        body.contains(&format!("\"http://127.0.0.1:{port}/")),
        "{body}"
    );

    // A failure comes with its code; a conditional write is really checked.
    ddb(
        port,
        "CreateTable",
        r#"{"TableName":"carts","KeySchema":[{"AttributeName":"id","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"id","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"}"#,
    );
    wait_for("the table to become active", || {
        ddb(port, "DescribeTable", r#"{"TableName":"carts"}"#)
            .1
            .contains("ACTIVE")
    });
    let item = r#"{"TableName":"carts","Item":{"id":{"S":"a"}},"ConditionExpression":"attribute_not_exists(id)"}"#;
    assert_eq!(ddb(port, "PutItem", item).0, 200);
    assert_eq!(ddb(port, "PutItem", item).0, 400);
    let failed = engine.snapshot().calls.last().cloned().unwrap();
    assert_eq!(
        failed.failure.clone().unwrap().code,
        "ConditionalCheckFailedException"
    );
    assert!(!failed.unsupported);

    // A service Lenslet does not include is marked as such.
    let (status, _) = send(
        port,
        "ec2",
        None,
        "application/x-www-form-urlencoded",
        "Action=DescribeInstances&Version=2016-11-15",
    );
    assert!(status >= 400);
    assert!(engine.snapshot().calls.last().unwrap().unsupported);

    // The session's permissions compile to a policy.
    assert!(engine.policy().contains("sqs:CreateQueue"));

    // A fresh start: an empty account, the calls kept and marked.
    let before = engine.snapshot().calls.len();
    engine.fresh();
    assert_eq!(engine.snapshot().generation, 1);
    assert!(engine.snapshot().permissions.is_empty());
    let (_, body) = sqs(port, "ListQueues", "{}");
    assert!(!body.contains("orders"), "{body}");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.calls.len(), before + 1);
    assert_eq!(snapshot.calls.last().unwrap().generation, 1);
    wait_for("the resources to empty", || {
        engine.snapshot().resources.is_empty()
    });
}

#[test]
fn opens_a_resource_and_counts_what_it_holds() {
    let engine = Engine::start(0).expect("start the engine");
    let port = engine.port;
    let (status, _) = sqs(port, "CreateQueue", r#"{"QueueName":"jobs"}"#);
    assert_eq!(status, 200);
    let url = format!("http://127.0.0.1:{port}/000000000000/jobs");
    let (status, _) = sqs(port, "SendMessage", &format!(r#"{{"QueueUrl":"{url}","MessageBody":"hi"}}"#));
    assert_eq!(status, 200);
    engine.refresh();
    wait_for("the queue's waiting message", || {
        engine
            .snapshot()
            .resources
            .iter()
            .any(|g| g.service == "sqs" && g.items.iter().any(|r| r.name == "jobs" && r.count == Some(1)))
    });
    let queue = engine.snapshot().resources.iter().find(|g| g.service == "sqs").unwrap().items[0].clone();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let contents = runtime.block_on(engine.contents("sqs", queue));
    assert!(
        contents.fields.iter().any(|(k, v)| k == "ApproximateNumberOfMessages" && v == "1"),
        "{:?}",
        contents.fields
    );
    // Each call knows what sent it.
    let call = engine.snapshot().calls.last().cloned().unwrap();
    assert_eq!(call.operation, "SendMessage");
    assert!(call.touches("sqs", "jobs"));
}
