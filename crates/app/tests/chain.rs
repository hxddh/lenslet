//! A serverless app's chain of calls, as Lenslet records it: run with a
//! Python that has boto3 (`LENSLET_PYTHON=/path/to/python cargo test -p
//! lenslet --test chain -- --ignored --nocapture`).

use lenslet::engine::Engine;

#[test]
#[ignore = "needs LENSLET_PYTHON with boto3"]
fn records_what_aws_does_inside_under_what_caused_it() {
    let python = std::env::var("LENSLET_PYTHON").expect("LENSLET_PYTHON");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/chain.py");
    let engine = Engine::start(0).expect("start the engine");
    let out = std::process::Command::new(python)
        .arg(script)
        .env("AWS_ENDPOINT_URL", engine.endpoint())
        .output()
        .expect("run the app");
    println!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());

    let calls = engine.snapshot().calls;
    let by_id: std::collections::HashMap<u64, _> = calls.iter().map(|c| (c.id, c.clone())).collect();
    let depth = |mut c: &std::sync::Arc<lenslet::session::Call>| {
        let mut d = 0;
        while let Some(p) = c.parent.and_then(|p| by_id.get(&p)) {
            d += 1;
            c = p;
        }
        d
    };
    for c in &calls {
        println!(
            "{}{} {} {} {} parent={:?} {}",
            "    ".repeat(depth(c)),
            if c.internal { "↳" } else { "•" },
            c.service,
            c.operation,
            c.status,
            c.parent,
            c.target.clone().unwrap_or_default()
        );
    }
    // The upload's notification invoked the function, and the function's own
    // write to DynamoDB hangs under it.
    let put = calls.iter().find(|c| c.operation == "PutObject" && !c.internal).expect("the upload");
    let invoke = calls
        .iter()
        .find(|c| c.internal && c.service == "lambda")
        .expect("the function's invocation");
    let write = calls
        .iter()
        .find(|c| c.internal && c.service == "dynamodb" && c.operation == "PutItem")
        .expect("the function's write");
    println!("invoke parent {:?} (put {}), write parent {:?} (invoke {})", invoke.parent, put.id, write.parent, invoke.id);
}
