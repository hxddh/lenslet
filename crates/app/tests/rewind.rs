//! Rewinding a run puts the account back as it was before it: run with a
//! Python that has boto3 (`LENSLET_PYTHON=... cargo test -p lenslet --test
//! rewind -- --ignored --nocapture`).

use lenslet::{engine::Engine, shell::Mark};
use lenslet_core::access::Access;

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
    println!("replayed {replayed:?} in {:?}", started.elapsed());
    // Rebuilt at the pace of what the calls set off, not the session's.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "a rewind of {} calls took {:?}",
        replayed.sent,
        started.elapsed()
    );
    assert_eq!(replayed.failed, 0, "every call sent again succeeded");
    // Run two is undone now: it cannot be rewound to again.
    assert!(
        runtime.block_on(engine.rewind(run_two)).is_err(),
        "an undone run cannot be rewound to"
    );
    // What functions do in response lands shortly after, as on AWS.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut rewound = py(&engine, "dump");
    while rewound != before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        rewound = py(&engine, "dump");
    }
    println!("settled {:?} after the rewind returned", started.elapsed());
    assert_eq!(before, rewound, "the account is as it was before run two");

    // A function denied by its role; Lenslet allows the role what the
    // denied call needed, twice on the same action: the grants add up.
    mark(Mark::Start("deny".into()));
    assert_eq!(py(&engine, "deny"), "denied");
    mark(Mark::End(1));
    let run_deny = engine.snapshot().runs.last().unwrap().id;
    let denied = engine
        .snapshot()
        .calls
        .iter()
        .rfind(|c| {
            c.internal
                && c.failure
                    .as_ref()
                    .is_some_and(|f| f.code.contains("AccessDenied"))
        })
        .cloned()
        .expect("the function's call was denied");
    let role = lenslet::resources::denied_role(
        denied.failure.as_ref().unwrap().message.as_deref().unwrap(),
    )
    .expect("the role it ran as");
    assert_eq!(role, "writer-role");
    runtime
        .block_on(engine.grant(role.clone(), denied.access.clone()))
        .expect("grant");
    let carts = Access::new("dynamodb:PutItem", "arn:aws:dynamodb:*:*:table/carts");
    runtime
        .block_on(engine.grant(role.clone(), vec![carts]))
        .expect("grant again");
    let policy = py(&engine, "policy");
    assert!(
        policy.contains("table/orders") && policy.contains("table/carts"),
        "both grants stand: {policy}"
    );
    // The role's page shows the policy Lenslet added.
    let page = runtime.block_on(engine.contents(
        "iam",
        lenslet::session::Resource {
            name: role.clone(),
            id: role.clone(),
            count: None,
        },
    ));
    assert!(
        page.rows
            .iter()
            .any(|r| r[1] == "Inline" && r[2].contains("table/carts")),
        "{page:?}"
    );

    // Back to before the denied run, and run it again: it passes, the role
    // created again with what Lenslet allowed it.
    let replayed = runtime.block_on(engine.rewind(run_deny)).expect("rewind");
    assert_eq!(replayed.failed, 0, "a grant waits for its role");
    mark(Mark::Start("deny".into()));
    assert_eq!(py(&engine, "deny"), "ok", "Run Again passes");
    mark(Mark::End(0));

    // A function that fails: invoked, the call is a failure with the
    // function's error, and what it printed is read from its log; invoked by
    // a bucket's notification, its run is watched and the invoking call
    // marked failed once it is over.
    mark(Mark::Start("crash".into()));
    assert_eq!(py(&engine, "crash"), "Unhandled");
    mark(Mark::End(0));
    let crasher = |internal: bool| {
        engine
            .snapshot()
            .calls
            .iter()
            .rev()
            .find(|c| {
                c.internal == internal
                    && c.operation == "Invoke"
                    && c.target.as_deref() == Some("fn/crasher")
            })
            .cloned()
    };
    let invoked = crasher(false).expect("the invocation");
    assert!(invoked.failed(), "{invoked:?}");
    assert_eq!(invoked.failure.as_ref().unwrap().code, "ValueError");
    let printed = runtime
        .block_on(engine.function_run(&invoked))
        .expect("its run is logged");
    assert!(
        printed.ended && printed.lines.iter().any(|l| l.contains("about to fail")),
        "{printed:?}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let notified = crasher(true);
        if notified.as_ref().is_some_and(|c| c.failed()) {
            assert_eq!(
                notified.unwrap().failure.as_ref().unwrap().code,
                "ValueError"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the notification's run was never marked failed: {notified:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Saved and opened again: the same account, the grants with it.
    let saved = py(&engine, "dump");
    let text = runtime.block_on(engine.save_account()).expect("save");
    let started = std::time::Instant::now();
    let opened = runtime
        .block_on(engine.open_account(text))
        .expect("open the account");
    println!("opened {opened:?} in {:?}", started.elapsed());
    assert_eq!(opened.failed, 0, "every call sent again succeeded");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut reopened = py(&engine, "dump");
    while reopened != saved && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        reopened = py(&engine, "dump");
    }
    assert_eq!(saved, reopened, "the opened account is the saved one");
    assert_eq!(py(&engine, "invoke"), "ok");
    assert!(py(&engine, "policy").contains("table/carts"));
}
