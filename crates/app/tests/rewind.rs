//! Rewinding a run puts the account back as it was before it: run with a
//! Python that has boto3 (`LENSLET_PYTHON=... cargo test -p lenslet --test
//! rewind -- --ignored --nocapture`).

use lenslet::{engine::Engine, shell::Mark};

fn py(engine: &Engine, phase: &str) -> String {
    let python = std::env::var("LENSLET_PYTHON").expect("LENSLET_PYTHON");
    let out = std::process::Command::new(python)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/rewind.py"))
        .arg(phase)
        .env("AWS_ENDPOINT_URL", engine.endpoint())
        .output()
        .expect("run python");
    assert!(
        out.status.success(),
        "{phase}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
#[ignore = "needs LENSLET_PYTHON with boto3"]
fn rewinding_a_run_puts_the_account_back() {
    let engine = Engine::start(0).expect("start the engine");
    let mark = engine.on_mark();
    mark(Mark::Start("one".into()));
    py(&engine, "one");
    mark(Mark::End(0));
    let before = py(&engine, "dump");
    mark(Mark::Start("two".into()));
    py(&engine, "two");
    mark(Mark::End(0));
    let after_two = py(&engine, "dump");
    assert_ne!(before, after_two, "phase two changed the account");
    let run_two = engine
        .snapshot()
        .runs
        .iter()
        .find(|r| r.command == "two")
        .unwrap()
        .id;

    let started = std::time::Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let replayed = runtime.block_on(engine.rewind(run_two)).expect("rewind");
    println!("replayed {replayed} calls in {:?}", started.elapsed());
    // What functions do in response lands shortly after, as on AWS.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut rewound = py(&engine, "dump");
    while rewound != before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        rewound = py(&engine, "dump");
    }
    println!("settled {:?} after the rewind returned", started.elapsed());
    assert_eq!(before, rewound, "the account is as it was before run two");
}
