//! The IAM golden corpus: recorded SDK calls and the access AWS requires for
//! each, written by hand from the Service Authorization Reference.
//!
//! Each `corpus/*.json` file holds `cases`: the `call` (a run-file `call`
//! line, volatile fields stripped), the footprint `access` / `implied`
//! permissions, and optionally the `concrete` ones (what `lenslet check
//! --policy` evaluates). A derivation change that alters any of them fails
//! here, with every difference listed.

use std::{fs, path::Path};

use lenslet_core::{access, trace::Trace};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    access: Vec<(String, String)>,
    #[serde(default)]
    implied: Vec<(String, String)>,
    #[serde(default)]
    concrete: Option<Concrete>,
    call: Trace,
}

#[derive(Deserialize)]
struct Concrete {
    access: Vec<(String, String)>,
    #[serde(default)]
    implied: Vec<(String, String)>,
}

fn sorted(mut items: Vec<(String, String)>) -> Vec<(String, String)> {
    items.sort();
    items
}

fn pairs(items: &[access::Access]) -> Vec<(String, String)> {
    sorted(
        items
            .iter()
            .map(|item| (item.action.clone(), item.resource.clone()))
            .collect(),
    )
}

#[test]
fn every_recorded_call_derives_exactly_the_expected_access() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("tests/corpus")
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut concrete = 0;
    let mut services = std::collections::BTreeSet::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read fixture");
        let fixture: Fixture = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        for case in fixture.cases {
            cases += 1;
            services.insert(case.call.service.clone());
            let call = case.call;
            let derived = access::derive_call(&call);
            let mut check =
                |what: &str, want: Vec<(String, String)>, got: Vec<(String, String)>| {
                    let want = sorted(want);
                    if want != got {
                        failures.push(format!(
                            "{} ({what}):\n    want {want:?}\n    got  {got:?}",
                            case.name
                        ));
                    }
                };
            check("access", case.access, pairs(&derived.own));
            check("implied", case.implied, pairs(&derived.implied));
            if let Some(expected) = case.concrete {
                concrete += 1;
                check(
                    "concrete access",
                    expected.access,
                    pairs(&derived.concrete_own),
                );
                check(
                    "concrete implied",
                    expected.implied,
                    pairs(&derived.concrete_implied),
                );
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} corpus cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(cases >= 150, "the corpus shrank to {cases} calls");
    assert!(concrete >= 20, "only {concrete} concrete expectations");
    assert!(services.len() >= 15, "only {} services", services.len());
}
