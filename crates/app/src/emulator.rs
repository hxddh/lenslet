//! AWS Emulator, embedded: a fresh in-memory engine behind a router of the
//! services Lenslet serves. Requests are handled in-process; a fresh start
//! swaps in a new engine, which is all a reset is (the emulator keeps its
//! state in memory only).

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use emulator_server::EmulatorService;
use smithy_server_runtime::MultiServiceRouter;

/// Assembling the router needs a deep stack (AWS Emulator's own binary uses
/// the same size); it runs on a thread of its own.
const BUILD_STACK: usize = 64 * 1024 * 1024;

/// The services Lenslet serves, by their signing names, for the window's
/// "not emulated here" answer.
pub const SERVICES: &[&str] = &[
    "s3", "dynamodb", "sqs", "sns", "sts", "iam", "kms", "lambda", "secretsmanager", "ssm",
    "events", "logs", "states", "kinesis",
];

#[derive(Clone)]
pub struct Emulator {
    router: Arc<RwLock<MultiServiceRouter>>,
    endpoint: Arc<str>,
}

impl Emulator {
    /// An emulator whose resources name `endpoint` (queue URLs, presigned URLs).
    pub fn new(endpoint: &str) -> Self {
        Self {
            router: Arc::new(RwLock::new(build(endpoint))),
            endpoint: endpoint.into(),
        }
    }

    /// Throw every resource away: a new, empty AWS account.
    pub fn fresh(&self) {
        let router = build(&self.endpoint);
        *self.router.write().unwrap_or_else(|e| e.into_inner()) = router;
    }

    pub async fn handle(&self, request: http::Request<Bytes>) -> http::Response<Bytes> {
        let router = self.router.read().unwrap_or_else(|e| e.into_inner()).clone();
        // A panic in a service answers 500 instead of taking the window down.
        match tokio::spawn(async move { router.handle_request(request).await }).await {
            Ok(response) => response,
            Err(_) => http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(Bytes::from_static(
                    br#"{"__type":"InternalFailure","message":"The emulator failed on this request."}"#,
                ))
                .expect("static response"),
        }
    }
}

fn build(endpoint: &str) -> MultiServiceRouter {
    let endpoint = endpoint.to_string();
    std::thread::Builder::new()
        .name("lenslet-emulator-build".into())
        .stack_size(BUILD_STACK)
        .spawn(move || {
            let engine = Arc::new(engine::InMemoryEngine::new());
            let mut router = MultiServiceRouter::new();
            macro_rules! serve {
                ($($krate:ident),* $(,)?) => {
                    $(router.register_service($krate::EmulatedService::new(engine.clone()));)*
                };
            }
            serve!(
                service_com_amazonaws_s3_amazons3,
                service_com_amazonaws_dynamodb_dynamodb_20120810,
                service_com_amazonaws_sqs_amazonsqs,
                service_com_amazonaws_sns_amazonsimplenotificationservice,
                service_com_amazonaws_sts_awssecuritytokenservicev20110615,
                service_com_amazonaws_iam_awsidentitymanagementv20100508,
                service_com_amazonaws_kms_trentservice,
                service_com_amazonaws_lambda_awsgirapiservice,
                service_com_amazonaws_secretsmanager_secretsmanager,
                service_com_amazonaws_ssm_amazonssm,
                service_com_amazonaws_eventbridge_awsevents,
                service_com_amazonaws_cloudwatchlogs_logs_20140328,
                service_com_amazonaws_sfn_awsstepfunctions,
                service_com_amazonaws_kinesis_kinesis_20131202,
            );
            // Any credentials work: Lenslet is local, and keys that change on
            // every fresh start would break the code pointed at it.
            router.set_request_authentication(false);
            router.serving_at(&endpoint);
            router
        })
        .expect("spawn the emulator build thread")
        .join()
        .expect("build the emulator")
}
