//! REST services: naming the operation of REST-JSON / REST-XML calls whose
//! SDKs send neither `x-amz-target` nor `?x-id=` (API Gateway v1/v2,
//! CloudFront, Route 53, EKS, ECS, S3 Control, OpenSearch, Batch, SES v2,
//! Backup, SageMaker Runtime, Bedrock Agent, ...), and the resource ARNs of
//! services beyond the core set. Part of the one derivation surface,
//! [`crate::access`].

use http::HeaderMap;
use serde_json::Value;

use crate::{trace::Trace, wire};

/// Resolve a REST-JSON or REST-XML operation name from `(service, method, path, headers)`
/// across 40+ AWS REST services when neither `x-amz-target` nor `?x-id=` was sent.
pub fn resolve_rest_operation(
    service: &str,
    method: &str,
    path: &str,
    _headers: &HeaderMap,
) -> Option<String> {
    let bare = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = bare.split('/').filter(|s| !s.is_empty()).collect();
    management_operation(service, method, &segs)
        .or_else(|| application_operation(service, method, &segs, bare))
        .map(str::to_string)
}

/// Control-plane REST APIs: API Gateway, EKS, Batch, CloudFront, Route 53,
/// S3 Control, SES v2.
fn management_operation(service: &str, method: &str, segs: &[&str]) -> Option<&'static str> {
    let op = match service {
        // API Gateway v1 (`apigateway`)
        "apigateway" => match (method, segs) {
            ("POST", ["restapis"]) => "CreateRestApi",
            ("GET", ["restapis"]) => "GetRestApis",
            ("GET", ["restapis", _]) => "GetRestApi",
            ("DELETE", ["restapis", _]) => "DeleteRestApi",
            ("GET", ["restapis", _, "resources"]) => "GetResources",
            ("POST", ["restapis", _, "resources", _]) => "CreateResource",
            ("PUT", ["restapis", _, "resources", _, "methods", _]) => "PutMethod",
            ("PUT", ["restapis", _, "resources", _, "methods", _, "integration"]) => {
                "PutIntegration"
            }
            ("POST", ["restapis", _, "deployments"]) => "CreateDeployment",
            ("GET", ["restapis", _, "stages"]) => "GetStages",
            ("GET", ["restapis", _, "stages", _]) => "GetStage",
            ("PATCH", ["restapis", _, "stages", _]) => "UpdateStage",
            ("DELETE", ["restapis", _, "stages", _]) => "DeleteStage",
            ("PATCH", ["restapis", _]) => "UpdateRestApi",
            ("POST", ["apikeys"]) => "CreateApiKey",
            ("GET", ["apikeys"]) => "GetApiKeys",
            ("POST", ["domainnames"]) => "CreateDomainName",
            ("GET", ["domainnames"]) => "GetDomainNames",
            // HTTP and WebSocket APIs (v2) sign as `apigateway` too.
            _ if segs.first() == Some(&"v2") => {
                return management_operation("apigatewayv2", method, segs);
            }
            _ => return None,
        },
        // API Gateway v2 (`apigatewayv2`)
        "apigatewayv2" => match (method, segs) {
            ("POST", ["v2", "apis"]) => "CreateApi",
            ("GET", ["v2", "apis"]) => "GetApis",
            ("GET", ["v2", "apis", _]) => "GetApi",
            ("PATCH", ["v2", "apis", _]) => "UpdateApi",
            ("DELETE", ["v2", "apis", _]) => "DeleteApi",
            ("POST", ["v2", "apis", _, "routes"]) => "CreateRoute",
            ("GET", ["v2", "apis", _, "routes"]) => "GetRoutes",
            ("DELETE", ["v2", "apis", _, "routes", _]) => "DeleteRoute",
            ("POST", ["v2", "apis", _, "integrations"]) => "CreateIntegration",
            ("GET", ["v2", "apis", _, "integrations"]) => "GetIntegrations",
            ("POST", ["v2", "apis", _, "stages"]) => "CreateStage",
            ("GET", ["v2", "apis", _, "stages"]) => "GetStages",
            ("POST", ["v2", "domainnames"]) => "CreateDomainName",
            ("GET", ["v2", "domainnames"]) => "GetDomainNames",
            _ => return None,
        },
        // Amazon EKS (`eks`)
        "eks" => match (method, segs) {
            ("POST", ["clusters"]) => "CreateCluster",
            ("GET", ["clusters"]) => "ListClusters",
            ("GET", ["clusters", _]) => "DescribeCluster",
            ("DELETE", ["clusters", _]) => "DeleteCluster",
            ("POST", ["clusters", _, "node-groups"]) => "CreateNodegroup",
            ("GET", ["clusters", _, "node-groups"]) => "ListNodegroups",
            ("GET", ["clusters", _, "node-groups", _]) => "DescribeNodegroup",
            ("DELETE", ["clusters", _, "node-groups", _]) => "DeleteNodegroup",
            ("POST", ["clusters", _, "addons"]) => "CreateAddon",
            ("GET", ["clusters", _, "addons"]) => "ListAddons",
            _ => return None,
        },
        // AWS Batch (`batch`)
        "batch" => match (method, segs) {
            ("POST", ["v1", "submitjob"]) => "SubmitJob",
            ("POST", ["v1", "describejobs"]) => "DescribeJobs",
            ("POST", ["v1", "listjobs"]) => "ListJobs",
            ("POST", ["v1", "terminatejob"]) => "TerminateJob",
            ("POST", ["v1", "canceljob"]) => "CancelJob",
            ("POST", ["v1", "registerjobdefinition"]) => "RegisterJobDefinition",
            ("POST", ["v1", "createjobqueue"]) => "CreateJobQueue",
            ("POST", ["v1", "createcomputeenvironment"]) => "CreateComputeEnvironment",
            _ => return None,
        },
        // CloudFront (`cloudfront`)
        "cloudfront" => match (method, segs) {
            ("POST", [_, "distribution"]) => "CreateDistribution",
            ("GET", [_, "distribution"]) => "ListDistributions",
            ("GET", [_, "distribution", _]) => "GetDistribution",
            ("GET", [_, "distribution", _, "config"]) => "GetDistributionConfig",
            ("PUT", [_, "distribution", _, "config"]) => "UpdateDistribution",
            ("DELETE", [_, "distribution", _]) => "DeleteDistribution",
            ("POST", [_, "distribution", _, "invalidation"]) => "CreateInvalidation",
            ("GET", [_, "distribution", _, "invalidation"]) => "ListInvalidations",
            _ => return None,
        },
        // Route 53 (`route53`)
        "route53" => match (method, segs) {
            ("POST", [_, "hostedzone"]) => "CreateHostedZone",
            ("GET", [_, "hostedzone"]) => "ListHostedZones",
            ("GET", [_, "hostedzone", _]) => "GetHostedZone",
            ("DELETE", [_, "hostedzone", _]) => "DeleteHostedZone",
            ("POST", [_, "hostedzone", _, "rrset"]) => "ChangeResourceRecordSets",
            ("GET", [_, "hostedzone", _, "rrset"]) => "ListResourceRecordSets",
            ("GET", [_, "change", _]) => "GetChange",
            _ => return None,
        },
        // S3 Control (`s3-control`)
        "s3-control" => match (method, segs) {
            ("PUT", ["v20180820", "configuration", "publicAccessBlock"]) => "PutPublicAccessBlock",
            ("GET", ["v20180820", "configuration", "publicAccessBlock"]) => "GetPublicAccessBlock",
            ("DELETE", ["v20180820", "configuration", "publicAccessBlock"]) => {
                "DeletePublicAccessBlock"
            }
            ("PUT", ["v20180820", "accesspoint", _]) => "CreateAccessPoint",
            ("GET", ["v20180820", "accesspoint", _]) => "GetAccessPoint",
            ("GET", ["v20180820", "accesspoint"]) => "ListAccessPoints",
            ("DELETE", ["v20180820", "accesspoint", _]) => "DeleteAccessPoint",
            ("POST", ["v20180820", "jobs"]) => "CreateJob",
            ("GET", ["v20180820", "jobs"]) => "ListJobs",
            _ => return None,
        },
        // SES v2 (`sesv2` / `email` REST v2 paths)
        "sesv2" | "email" if segs.first() == Some(&"v2") => match (method, segs) {
            ("POST", ["v2", "email", "outbound-emails"]) => "SendEmail",
            ("POST", ["v2", "email", "outbound-bulk-emails"]) => "SendBulkEmail",
            ("POST", ["v2", "email", "identities"]) => "CreateEmailIdentity",
            ("GET", ["v2", "email", "identities"]) => "ListEmailIdentities",
            ("GET", ["v2", "email", "identities", _]) => "GetEmailIdentity",
            ("DELETE", ["v2", "email", "identities", _]) => "DeleteEmailIdentity",
            ("POST", ["v2", "email", "configuration-sets"]) => "CreateConfigurationSet",
            ("GET", ["v2", "email", "configuration-sets"]) => "ListConfigurationSets",
            ("GET", ["v2", "email", "account"]) => "GetAccount",
            _ => return None,
        },
        _ => return None,
    };
    Some(op)
}

/// Application-facing REST APIs: Scheduler, Pipes, RDS Data, OpenSearch,
/// IoT, SageMaker Runtime, Bedrock Agent Runtime, Backup, X-Ray, GuardDuty,
/// Glacier, S3 Object Lambda.
fn application_operation(
    service: &str,
    method: &str,
    segs: &[&str],
    bare: &str,
) -> Option<&'static str> {
    let op = match service {
        // EventBridge Scheduler (`scheduler`)
        "scheduler" => match (method, segs) {
            ("POST", ["schedules", _]) => "CreateSchedule",
            ("GET", ["schedules", _]) => "GetSchedule",
            ("PUT", ["schedules", _]) => "UpdateSchedule",
            ("DELETE", ["schedules", _]) => "DeleteSchedule",
            ("GET", ["schedules"]) => "ListSchedules",
            ("POST", ["schedule-groups", _]) => "CreateScheduleGroup",
            ("GET", ["schedule-groups"]) => "ListScheduleGroups",
            _ => return None,
        },
        // EventBridge Pipes (`pipes`)
        "pipes" => match (method, segs) {
            ("POST", ["v1", "pipes", _]) => "CreatePipe",
            ("GET", ["v1", "pipes", _]) => "DescribePipe",
            ("PUT", ["v1", "pipes", _]) => "UpdatePipe",
            ("DELETE", ["v1", "pipes", _]) => "DeletePipe",
            ("GET", ["v1", "pipes"]) => "ListPipes",
            ("POST", ["v1", "pipes", _, "start"]) => "StartPipe",
            ("POST", ["v1", "pipes", _, "stop"]) => "StopPipe",
            _ => return None,
        },
        // RDS Data API (`rds-data`)
        "rds-data" => match (method, segs) {
            ("POST", ["Execute"]) => "ExecuteStatement",
            ("POST", ["BatchExecute"]) => "BatchExecuteStatement",
            ("POST", ["BeginTransaction"]) => "BeginTransaction",
            ("POST", ["CommitTransaction"]) => "CommitTransaction",
            ("POST", ["RollbackTransaction"]) => "RollbackTransaction",
            _ => return None,
        },
        // OpenSearch (`es` / `opensearch`)
        "es" | "opensearch" => match (method, segs) {
            ("POST", ["2021-01-01", "opensearch", "domain"]) => "CreateDomain",
            ("GET", ["2021-01-01", "opensearch", "domain"]) => "ListDomainNames",
            ("GET", ["2021-01-01", "opensearch", "domain", _]) => "DescribeDomain",
            ("DELETE", ["2021-01-01", "opensearch", "domain", _]) => "DeleteDomain",
            _ => return None,
        },
        // AWS IoT Core & IoT Data (`iot` / `iotdata`)
        "iot" => match (method, segs) {
            ("POST", ["things", _]) => "CreateThing",
            ("GET", ["things", _]) => "DescribeThing",
            ("DELETE", ["things", _]) => "DeleteThing",
            ("GET", ["things"]) => "ListThings",
            ("POST", ["policies", _]) => "CreatePolicy",
            ("GET", ["policies"]) => "ListPolicies",
            _ => return None,
        },
        "iotdata" | "iot-data" => match (method, segs) {
            ("POST", ["topics", ..]) => "Publish",
            ("GET", ["things", _, "shadow"]) => "GetThingShadow",
            ("POST", ["things", _, "shadow"]) => "UpdateThingShadow",
            ("DELETE", ["things", _, "shadow"]) => "DeleteThingShadow",
            _ => return None,
        },
        // SageMaker Runtime (`sagemaker` / `sagemaker-runtime`)
        "sagemaker-runtime" | "sagemaker" if segs.first() == Some(&"endpoints") => {
            match (method, segs) {
                ("POST", ["endpoints", _, "invocations"]) => "InvokeEndpoint",
                ("POST", ["endpoints", _, "invocations-response-stream"]) => {
                    "InvokeEndpointWithResponseStream"
                }
                ("POST", ["endpoints", _, "async-invocations"]) => "InvokeEndpointAsync",
                _ => return None,
            }
        }
        // Bedrock Agent Runtime (`bedrock-agent-runtime` / `bedrock-agent`)
        "bedrock-agent-runtime" => match (method, segs) {
            ("POST", ["knowledgebases", _, "retrieve"]) => "Retrieve",
            ("POST", ["knowledgebases", _, "retrieveAndGenerate"]) => "RetrieveAndGenerate",
            ("POST", ["agents", _, "agentAliases", _, "sessions", _, "text"]) => "InvokeAgent",
            _ => return None,
        },
        // AWS Backup (`backup`)
        "backup" => match (method, segs) {
            ("PUT", ["backup-vaults", _]) => "CreateBackupVault",
            ("GET", ["backup-vaults", _]) => "DescribeBackupVault",
            ("GET", ["backup-vaults"]) => "ListBackupVaults",
            ("DELETE", ["backup-vaults", _]) => "DeleteBackupVault",
            ("PUT", ["backup", "plans"]) => "CreateBackupPlan",
            ("GET", ["backup", "plans"]) => "ListBackupPlans",
            _ => return None,
        },
        // AWS X-Ray (`xray`)
        "xray" => match (method, segs) {
            ("POST", ["TraceSegments"]) => "PutTraceSegments",
            ("POST", ["TelemetryRecords"]) => "PutTelemetryRecords",
            ("POST", ["Traces"]) => "BatchGetTraces",
            ("POST", ["TraceSummaries"]) => "GetTraceSummaries",
            ("POST", ["ServiceGraph"]) => "GetServiceGraph",
            _ => return None,
        },
        // GuardDuty (`guardduty`)
        "guardduty" => match (method, segs) {
            ("POST", ["detector"]) => "CreateDetector",
            ("GET", ["detector"]) => "ListDetectors",
            ("GET", ["detector", _]) => "GetDetector",
            ("DELETE", ["detector", _]) => "DeleteDetector",
            ("POST", ["detector", _, "findings"]) => "ListFindings",
            ("POST", ["detector", _, "findings", "get"]) => "GetFindings",
            _ => return None,
        },
        // Glacier (`glacier`)
        "glacier" => match (method, segs) {
            ("PUT", [_, "vaults", _]) => "CreateVault",
            ("GET", [_, "vaults", _]) => "DescribeVault",
            ("GET", [_, "vaults"]) => "ListVaults",
            ("DELETE", [_, "vaults", _]) => "DeleteVault",
            ("POST", [_, "vaults", _, "archives"]) => "UploadArchive",
            _ => return None,
        },
        // S3 Object Lambda (`s3-object-lambda`)
        "s3-object-lambda" if method == "POST" && bare.contains("WriteGetObjectResponse") => {
            "WriteGetObjectResponse"
        }
        _ => return None,
    };
    Some(op)
}

/// Extract extended service resource ARNs for services beyond the core 18.
/// `norm` normalizes generated names in Footprint mode and leaves them intact in Concrete mode.
pub fn derive_extra_resources(
    prefix: &str,
    operation: &str,
    trace: &Trace,
    json: Option<&Value>,
    form: Option<&[(String, String)]>,
    arn: impl Fn(&str, &str) -> String,
    norm: impl Fn(&str) -> String,
) -> Option<Vec<String>> {
    let bare_path = trace.path.split('?').next().unwrap_or(&trace.path);
    let segs: Vec<&str> = bare_path.split('/').filter(|s| !s.is_empty()).collect();
    let j_str = |key: &str| -> Option<String> {
        json.and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let f_str = |key: &str| -> Option<String> {
        form.and_then(|items| items.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
            .filter(|s| !s.is_empty())
    };
    let field = |key: &str| j_str(key).or_else(|| f_str(key));

    let single = |res: String| Some(vec![res]);

    match prefix {
        "ecs" => ecs_resource(&field, &arn, &norm).map(|res| vec![res]),
        "cloudformation" => {
            let stack = field("StackName")?;
            if stack.starts_with("arn:") {
                return single(stack);
            }
            single(arn("cloudformation", &format!("stack/{}/*", norm(&stack))))
        }
        "glue" => glue_resources(operation, json, &field, &arn, &norm),
        "athena" => {
            let wg = field("WorkGroup").unwrap_or_else(|| "primary".into());
            single(arn("athena", &format!("workgroup/{}", norm(&wg))))
        }
        "rds" => {
            if let Some(cluster) = field("DBClusterIdentifier") {
                return single(arn("rds", &format!("cluster:{}", norm(&cluster))));
            }
            if let Some(db) = field("DBInstanceIdentifier") {
                return single(arn("rds", &format!("db:{}", norm(&db))));
            }
            None
        }
        _ => rest_path_resource(prefix, &segs, &field, &arn, &norm).map(|res| vec![res]),
    }
}

/// ECS task definitions, services and clusters.
fn ecs_resource(
    field: &dyn Fn(&str) -> Option<String>,
    arn: &dyn Fn(&str, &str) -> String,
    norm: &dyn Fn(&str) -> String,
) -> Option<String> {
    let single = Some;
    if let Some(td) = field("taskDefinition") {
        if td.starts_with("arn:") {
            return single(td);
        }
        return single(arn("ecs", &format!("task-definition/{}", norm(&td))));
    }
    if let Some(fam) = field("family") {
        return single(arn("ecs", &format!("task-definition/{}:*", norm(&fam))));
    }
    if let Some(svc) = field("service").or_else(|| field("serviceName")) {
        let cluster = field("cluster")
            .map(|c| norm(c.rsplit('/').next().unwrap_or(&c)))
            .unwrap_or_else(|| "default".into());
        return single(arn(
            "ecs",
            &format!(
                "service/{cluster}/{}",
                norm(svc.rsplit('/').next().unwrap_or(&svc))
            ),
        ));
    }
    if let Some(cluster) = field("cluster").or_else(|| field("clusterName")) {
        if cluster.starts_with("arn:") {
            return single(cluster);
        }
        return single(arn("ecs", &format!("cluster/{}", norm(&cluster))));
    }
    None
}

/// Services that name their resource in the REST path (or the body):
/// EKS, CloudFront, Route 53, SES, SageMaker, OpenSearch, Pipes, Backup.
fn rest_path_resource(
    prefix: &str,
    segs: &[&str],
    field: &dyn Fn(&str) -> Option<String>,
    arn: &dyn Fn(&str, &str) -> String,
    norm: &dyn Fn(&str) -> String,
) -> Option<String> {
    let single = Some;
    match prefix {
        "eks" => {
            if let Some(name) = segs
                .get(1)
                .filter(|_| segs.first() == Some(&"clusters"))
                .copied()
                .map(wire::percent_decode_path)
                .or_else(|| field("name"))
                .or_else(|| field("clusterName"))
            {
                return single(arn("eks", &format!("cluster/{}", norm(&name))));
            }
            None
        }
        "cloudfront" => {
            if let Some(dist) = segs
                .get(2)
                .filter(|_| segs.get(1) == Some(&"distribution"))
                .copied()
            {
                return single(format!("arn:aws:cloudfront::*:distribution/{}", norm(dist)));
            }
            None
        }
        "route53" => {
            if let Some(zone) = segs
                .get(2)
                .filter(|_| segs.get(1) == Some(&"hostedzone"))
                .copied()
            {
                return single(format!("arn:aws:route53:::hostedzone/{}", norm(zone)));
            }
            None
        }
        "ses" => {
            if let Some(id) = field("FromEmailAddress")
                .or_else(|| field("Source"))
                .or_else(|| field("EmailIdentity"))
                .or_else(|| {
                    segs.get(3)
                        .filter(|_| segs.get(2) == Some(&"identities"))
                        .map(|s| wire::percent_decode_path(s))
                })
            {
                return single(arn("ses", &format!("identity/{id}")));
            }
            None
        }
        "sagemaker" => {
            if let Some(ep) = segs
                .get(1)
                .filter(|_| segs.first() == Some(&"endpoints"))
                .map(|s| wire::percent_decode_path(s))
                .or_else(|| field("EndpointName"))
            {
                return single(arn("sagemaker", &format!("endpoint/{}", norm(&ep))));
            }
            None
        }
        "es" | "opensearch" => {
            if let Some(dom) = segs
                .get(3)
                .filter(|_| segs.get(2) == Some(&"domain"))
                .map(|s| wire::percent_decode_path(s))
                .or_else(|| field("DomainName"))
            {
                return single(arn("es", &format!("domain/{}", norm(&dom))));
            }
            None
        }
        "pipes" => {
            if let Some(pipe) = segs
                .get(2)
                .filter(|_| segs.get(1) == Some(&"pipes"))
                .map(|s| wire::percent_decode_path(s))
                .or_else(|| field("Name"))
            {
                return single(arn("pipes", &format!("pipe/{}", norm(&pipe))));
            }
            None
        }
        "backup" => {
            if let Some(vault) = segs
                .get(1)
                .filter(|_| segs.first() == Some(&"backup-vaults"))
                .map(|s| wire::percent_decode_path(s))
                .or_else(|| field("BackupVaultName"))
            {
                return single(arn("backup", &format!("backup-vault:{}", norm(&vault))));
            }
            None
        }
        _ => None,
    }
}
/// Glue Data Catalog calls are authorized on the catalog, the database and
/// the table together (`catalog`, `database/{db}`, `table/{db}/{table}`);
/// jobs on `job/{name}`.
fn glue_resources(
    operation: &str,
    json: Option<&Value>,
    field: &dyn Fn(&str) -> Option<String>,
    arn: &dyn Fn(&str, &str) -> String,
    norm: &dyn Fn(&str) -> String,
) -> Option<Vec<String>> {
    let nested = |outer: &str, key: &str| {
        json.and_then(|v| v.get(outer))
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let catalog = || arn("glue", "catalog");
    let database = |db: &str| arn("glue", &format!("database/{}", norm(db)));
    let table = |db: &str, tbl: &str| arn("glue", &format!("table/{}/{}", norm(db), norm(tbl)));
    match operation {
        "CreateDatabase" => {
            let db = nested("DatabaseInput", "Name")?;
            Some(vec![catalog(), database(&db)])
        }
        "GetDatabase" | "UpdateDatabase" | "DeleteDatabase" => {
            let db = field("Name")?;
            Some(vec![catalog(), database(&db)])
        }
        "GetDatabases" => Some(vec![catalog(), arn("glue", "database/*")]),
        "CreateTable" | "UpdateTable" => {
            let db = field("DatabaseName")?;
            let tbl = nested("TableInput", "Name")?;
            Some(vec![catalog(), database(&db), table(&db, &tbl)])
        }
        "GetTable" | "DeleteTable" => {
            let db = field("DatabaseName")?;
            let tbl = field("Name")?;
            Some(vec![catalog(), database(&db), table(&db, &tbl)])
        }
        "GetTables" => {
            let db = field("DatabaseName")?;
            Some(vec![
                catalog(),
                database(&db),
                arn("glue", &format!("table/{}/*", norm(&db))),
            ])
        }
        _ if field("DatabaseName").is_some() => {
            let db = field("DatabaseName")?;
            Some(match field("TableName") {
                Some(tbl) => vec![catalog(), database(&db), table(&db, &tbl)],
                None => vec![catalog(), database(&db)],
            })
        }
        _ => {
            let job = field("JobName").or_else(|| {
                matches!(operation, "CreateJob" | "GetJob" | "DeleteJob")
                    .then(|| field("Name"))
                    .flatten()
            })?;
            Some(vec![arn("glue", &format!("job/{}", norm(&job)))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_rest_operations() {
        let headers = HeaderMap::new();
        assert_eq!(
            resolve_rest_operation("apigatewayv2", "POST", "/v2/apis/abc123/routes", &headers)
                .as_deref(),
            Some("CreateRoute")
        );
        assert_eq!(
            resolve_rest_operation("eks", "GET", "/clusters/prod-eks/node-groups", &headers)
                .as_deref(),
            Some("ListNodegroups")
        );
        assert_eq!(
            resolve_rest_operation("sesv2", "POST", "/v2/email/outbound-emails", &headers)
                .as_deref(),
            Some("SendEmail")
        );
    }
}
