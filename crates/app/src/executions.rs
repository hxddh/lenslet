//! How a state machine's execution ended. Step Functions answers
//! `StartExecution` as soon as the execution starts; when it later fails or
//! times out, Lenslet marks the call that started it failed, with the state
//! that failed and why. (`StartSyncExecution` says so in its own answer.)

use std::time::Duration;

use serde_json::{Value, json};

use crate::{
    emulator::Emulator,
    resources::Ask,
    session::{Call, Failure, Session},
};

/// The execution a call started, if it started one.
pub fn started(call: &Call) -> Option<String> {
    if call.service != "states" || call.operation != "StartExecution" || call.status >= 300 {
        return None;
    }
    let answer: Value = serde_json::from_str(&call.response.body).ok()?;
    answer["executionArn"].as_str().map(str::to_string)
}

/// How a finished execution failed, from its description (and, for the
/// state, its history); `None` while it runs or when it succeeded.
pub fn failure(description: &Value, history: Option<&Value>) -> Option<Failure> {
    let status = description["status"].as_str()?;
    if !matches!(status, "FAILED" | "TIMED_OUT") {
        return None;
    }
    let code = description["error"]
        .as_str()
        .filter(|e| !e.is_empty())
        .unwrap_or(if status == "TIMED_OUT" {
            "States.Timeout"
        } else {
            "States.Failed"
        });
    let state = history.and_then(failed_state);
    let cause = description["cause"].as_str().filter(|c| !c.is_empty());
    let message = match (state, cause) {
        (Some(state), Some(cause)) => Some(format!("{state}: {cause}")),
        (Some(state), None) => Some(state),
        (None, cause) => cause.map(str::to_string),
    };
    Some(Failure {
        code: code.to_string(),
        message,
        hint: Some(FAILED_EXECUTION.into()),
    })
}

/// The last state the execution entered: the one it failed in. The history
/// is newest first.
fn failed_state(history: &Value) -> Option<String> {
    history["events"]
        .as_array()?
        .iter()
        .find_map(|e| e["stateEnteredEventDetails"]["name"].as_str())
        .map(|name| format!("In state “{name}”"))
}

const FAILED_EXECUTION: &str =
    "The execution failed after it was started: open the state machine for its history.";

/// How long an execution is watched for.
const WATCH_FOR: Duration = Duration::from_secs(60 * 60);

/// Watch the executions calls started to their end, and mark a call failed
/// when its execution failed; `StartSyncExecution`'s answer says so at once.
pub async fn watch(emulator: Emulator, session: Session) {
    struct Pending {
        seq: usize,
        execution: String,
        at_ms: u64,
        generation: u64,
    }
    let mut pending: Vec<Pending> = Vec::new();
    let mut seen = 0usize;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        for call in session.calls_from(seen) {
            seen = call.seq + 1;
            if call.failure.is_some() {
                continue;
            }
            if call.service == "states" && call.operation == "StartSyncExecution" {
                if let Some(failure) = serde_json::from_str::<Value>(&call.response.body)
                    .ok()
                    .and_then(|answer| failure(&answer, None))
                {
                    session.fail(call.seq, failure);
                }
            } else if let Some(execution) = started(&call) {
                pending.push(Pending {
                    seq: call.seq,
                    execution,
                    at_ms: call.at_ms,
                    generation: call.generation,
                });
            }
        }
        if pending.is_empty() {
            continue;
        }
        let generation = session.generation();
        let now = crate::session::now_ms();
        let ask = Ask::new(&emulator);
        let mut still = Vec::new();
        for p in pending {
            if p.generation != generation
                || now.saturating_sub(p.at_ms) > WATCH_FOR.as_millis() as u64
            {
                continue;
            }
            let arn = json!({ "executionArn": p.execution });
            let Some(description) = ask
                .json(
                    "states",
                    "AWSStepFunctions.DescribeExecution",
                    "1.0",
                    arn.clone(),
                )
                .await
            else {
                // Gone with its state machine.
                continue;
            };
            match description["status"].as_str() {
                Some("RUNNING") | Some("PENDING_REDRIVE") => still.push(p),
                Some("FAILED") | Some("TIMED_OUT") => {
                    let mut ask_for = arn;
                    ask_for["reverseOrder"] = true.into();
                    ask_for["maxResults"] = 50.into();
                    let history = ask
                        .json(
                            "states",
                            "AWSStepFunctions.GetExecutionHistory",
                            "1.0",
                            ask_for,
                        )
                        .await;
                    if let Some(failure) = failure(&description, history.as_ref()) {
                        session.fail(p.seq, failure);
                    }
                }
                _ => {}
            }
        }
        pending = still;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_which_state_an_execution_failed_in() {
        let description = json!({"status": "FAILED", "error": "Boom", "cause": "it broke"});
        // Newest first, as asked for.
        let history = json!({"events": [
            {"type": "ExecutionFailed"},
            {"type": "TaskStateEntered", "stateEnteredEventDetails": {"name": "Charge"}},
            {"type": "PassStateEntered", "stateEnteredEventDetails": {"name": "Start"}},
        ]});
        let failure = failure(&description, Some(&history)).unwrap();
        assert_eq!(failure.code, "Boom");
        assert!(failure.message.as_deref().unwrap().contains("Charge"));
        assert!(failure.message.as_deref().unwrap().ends_with("it broke"));
        let timed = super::failure(&json!({"status": "TIMED_OUT"}), None).unwrap();
        assert_eq!(timed.code, "States.Timeout");
        assert!(super::failure(&json!({"status": "SUCCEEDED"}), None).is_none());
        assert!(super::failure(&json!({"status": "RUNNING"}), None).is_none());
    }

    #[test]
    fn knows_the_execution_a_call_started() {
        let mut call = Call {
            service: "states".into(),
            operation: "StartExecution".into(),
            status: 200,
            ..Default::default()
        };
        call.response.body = r#"{"executionArn":"arn:aws:states:us-east-1:000000000000:execution:m:e","startDate":1}"#.into();
        assert!(started(&call).unwrap().ends_with(":e"));
        call.status = 400;
        assert!(started(&call).is_none());
    }
}
