//! The IAM policy for a set of permissions: one statement per resource, its
//! actions sorted, with the ARN fields AWS leaves empty kept empty.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::access::Access;

/// An IAM policy JSON document granting `access`: one statement per
/// resource, actions sorted. ARNs keep the empty region / account fields AWS
/// uses (S3, Route 53, IAM, API Gateway, Bedrock foundation models).
pub fn compile(access: &[Access]) -> String {
    let mut text =
        serde_json::to_string_pretty(&compile_value(access)).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    text
}

/// [`compile`], as a value.
pub fn compile_value(access: &[Access]) -> Value {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in access {
        groups
            .entry(aws_arn(&item.resource))
            .or_default()
            .insert(item.action.clone());
    }
    let mut used_sids: BTreeSet<String> = BTreeSet::new();
    let statements: Vec<Value> = groups
        .into_iter()
        .map(|(resource, actions)| {
            let actions: Vec<String> = actions.into_iter().collect();
            let base = make_sid(&actions, &resource);
            let mut sid = base.clone();
            let mut seq = 2usize;
            while !used_sids.insert(sid.clone()) {
                sid = format!("{base}{seq}");
                seq += 1;
            }
            json!({ "Sid": sid, "Effect": "Allow", "Action": actions, "Resource": resource })
        })
        .collect();
    json!({ "Version": "2012-10-17", "Statement": statements })
}

/// Which fields of a service's ARNs are empty in AWS (region, account).
fn empty_fields(service: &str, resource: &str) -> (bool, bool) {
    match service {
        "s3" | "route53" => (true, true),
        "iam" | "sts" | "cloudfront" => (true, false),
        "apigateway" => (false, true),
        "bedrock" if resource.starts_with("foundation-model/") => (false, true),
        _ => (false, false),
    }
}

/// The ARN as AWS writes it: a field the service leaves empty stays empty.
fn aws_arn(arn: &str) -> String {
    let mut fields = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some(service), Some(region), Some(account), Some(rest)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return arn.to_string();
    };
    let (no_region, no_account) = empty_fields(service, rest);
    let region = if no_region { "" } else { region };
    let account = if no_account { "" } else { account };
    format!("arn:{partition}:{service}:{region}:{account}:{rest}")
}

fn make_sid(actions: &[String], resource: &str) -> String {
    let prefix = actions
        .first()
        .and_then(|action| action.split(':').next())
        .unwrap_or("Aws");
    // The resource part of the ARN (after the account), by its last named
    // `/` segment: `table/orders` → `Orders`, `function:f:prod` → `FunctionFProd`.
    let rest = resource.splitn(6, ':').nth(5).unwrap_or(resource);
    let tail = rest
        .rsplit('/')
        .map(|segment| segment.trim_end_matches(":*"))
        .find(|segment| !segment.is_empty() && *segment != "*")
        .map(pascal_case)
        .filter(|tail| !tail.is_empty())
        .unwrap_or_else(|| "Access".to_string());
    let clean: String = format!("{}{tail}", pascal_case(prefix))
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    if clean.is_empty() {
        "AllowAccess".into()
    } else {
        clean
    }
}

fn pascal_case(input: &str) -> String {
    let mut out = String::new();
    for part in input.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    fn access(action: &str, resource: &str) -> Access {
        Access::new(action, resource)
    }

    fn golden_access() -> Vec<Access> {
        vec![
            access("dynamodb:GetItem", "arn:aws:dynamodb:*:*:table/orders"),
            access("dynamodb:PutItem", "arn:aws:dynamodb:*:*:table/orders"),
            access("s3:GetObject", "arn:aws:s3:::shop-assets/*"),
            access("s3:ListBucket", "arn:aws:s3:::shop-assets"),
            access("sqs:SendMessage", "arn:aws:sqs:*:*:jobs"),
            access(
                "lambda:InvokeFunction",
                "arn:aws:lambda:*:*:function:thumbnailer:prod",
            ),
            access("apigateway:GET", "arn:aws:apigateway:*::/restapis/*"),
            access(
                "bedrock:InvokeModel",
                "arn:aws:bedrock:*::foundation-model/anthropic.claude-3-haiku-20240307-v1:0",
            ),
            access(
                "bedrock:InvokeModel",
                "arn:aws:bedrock:*:*:inference-profile/us.anthropic.claude-3-haiku-20240307-v1:0",
            ),
            access("iam:PassRole", "arn:aws:iam::*:role/service-role/worker"),
            access("kms:Decrypt", "arn:aws:kms:*:*:key/*"),
            access("route53:GetHostedZone", "arn:aws:route53:::hostedzone/Z123"),
            access("sts:GetSessionToken", "*"),
        ]
    }

    #[test]
    fn iam_json_matches_its_golden_file_and_keeps_empty_arn_fields() {
        let text = compile(&golden_access());
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/policy.json");
        if std::env::var_os("LENSLET_BLESS").is_some() {
            fs::write(&path, &text).expect("bless golden file");
        }
        assert_eq!(text, fs::read_to_string(&path).expect("golden file"));
        let doc: Value = serde_json::from_str(&text).expect("valid JSON");
        let resources: Vec<&str> = doc["Statement"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["Resource"].as_str().unwrap())
            .collect();
        assert!(resources.contains(&"arn:aws:s3:::shop-assets/*"));
        assert!(resources.contains(&"arn:aws:apigateway:*::/restapis/*"));
        assert!(resources.contains(&"arn:aws:iam::*:role/service-role/worker"));
    }
}
