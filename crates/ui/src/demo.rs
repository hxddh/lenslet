//! Demo data for screenshots and `LENSLET_DEMO=1`: a real run of a small shop
//! script against a local emulator, recorded by `lenslet run --check` (paths
//! scrubbed). It is only used when demo mode is explicitly requested.

use crate::model::{self as trace, RunInfo, Trace};

const RUN: &str = include_str!("demo.jsonl");

/// The demo run's stdout/stderr.
pub const LOG: &str = "$ pytest tests/test_checkout.py -q\n\
             collected 4 items\n\
             tests/test_checkout.py ...F                                              [100%]\n\n\
             FAILED tests/test_checkout.py::test_place_order - ClientError: An error occurred (ResourceNotFoundException) when calling the GetItem operation: Requested resource not found\n";

pub fn demo_rows() -> Vec<Trace> {
    trace::parse_lines(RUN)
}

pub fn demo_run() -> RunInfo {
    trace::parse_run(RUN)
}
