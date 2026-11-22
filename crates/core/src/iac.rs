//! IAM policies compiled from observed access (excluding `# test` lines).
//!
//! Two formats, one code path:
//! - `iam`: an AWS IAM policy JSON document (any IaC tool accepts it);
//! - `terraform`: a `data "aws_iam_policy_document" "lenslet"` data source.
//!
//! The lock and a run's report compile through the same [`PolicyInput`]s, so
//! the same access always gives the same statements. ARNs keep the empty
//! region / account fields AWS uses (S3, Route 53, IAM, API Gateway,
//! Bedrock foundation models); only `*` fields are filled with the region
//! and account given (or, for Terraform, data-source references).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::{
    access::{self, Access},
    lock::Lock,
    summary::AccessEntry,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PolicyFormat {
    #[default]
    IamJson,
    Terraform,
}

impl PolicyFormat {
    pub const ALL: [PolicyFormat; 2] = [PolicyFormat::IamJson, PolicyFormat::Terraform];

    /// The format named on the command line, or why it is not one.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "iam" | "json" => Ok(Self::IamJson),
            "terraform" | "tf" => Ok(Self::Terraform),
            removed @ ("cfn" | "cloudformation" | "sam" | "cdk") => Err(format!(
                "--format {removed} was removed in 12.0; use --format iam (any IaC tool accepts IAM JSON)"
            )),
            other => Err(format!(
                "unknown --format `{other}`; expected iam or terraform"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::IamJson => "IAM JSON",
            Self::Terraform => "Terraform",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::IamJson => "json",
            Self::Terraform => "tf",
        }
    }
}

/// One permission fed into the compiler.
#[derive(Clone, Debug, Default)]
pub struct PolicyInput {
    pub access: Access,
    pub implied: bool,
    /// The SigV4 services of *every* call that needed this permission (see
    /// [`crate::summary::AccessUse::services`]). With them, an implied
    /// `iam:PassRole` gets an `iam:PassedToService` condition and an implied
    /// KMS permission a `kms:ViaService` one. Empty: no condition.
    pub services: Vec<String>,
}

/// The permissions a lock grants, without its `# test` lines. A lock does
/// not record which services needed a permission, so these carry none.
pub fn inputs_from_lock(lock: &Lock) -> Vec<PolicyInput> {
    lock.access
        .difference(&lock.test)
        .map(|access| PolicyInput {
            access: access.clone(),
            implied: lock.implied.contains(access),
            services: Vec::new(),
        })
        .collect()
}

/// The permissions in a run report's access list. Like the lock, they carry
/// no services, so `--run` and `--lock` compile the same access to the same
/// statements.
pub fn inputs_from_entries(entries: &[AccessEntry]) -> Vec<PolicyInput> {
    entries
        .iter()
        .map(|entry| PolicyInput {
            access: Access::new(entry.action.clone(), entry.resource.clone()),
            implied: entry.used.implied,
            services: Vec::new(),
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct CompileOptions {
    pub format: PolicyFormat,
    /// Region (e.g. `us-east-1`) for the `*` region of regional ARNs.
    pub region: Option<String>,
    /// 12-digit account id for the `*` account of ARNs that have one.
    pub account: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Condition {
    None,
    /// `StringEquals` `iam:PassedToService`.
    PassedToService(Vec<String>),
    /// `kms:ViaService`: service names, the region filled in at render time.
    KmsViaService(Vec<String>),
}

/// A piece of a rendered string: literal text, or a reference Terraform
/// interpolates (`data.aws_region.current.region`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Text(String),
    Ref(&'static str),
}

const PARTITION_REF: &str = "data.aws_partition.current.partition";
const REGION_REF: &str = "data.aws_region.current.region";
const ACCOUNT_REF: &str = "data.aws_caller_identity.current.account_id";

#[derive(Clone, Debug)]
struct Statement {
    sid: String,
    actions: Vec<String>,
    resource: Vec<Part>,
    condition: Condition,
}

/// Compile a single permission (the window's preview).
pub fn compile_single(input: &PolicyInput, format: PolicyFormat) -> String {
    compile(
        std::slice::from_ref(input),
        &CompileOptions {
            format,
            ..CompileOptions::default()
        },
    )
}

/// Compile permissions into the chosen format.
pub fn compile(inputs: &[PolicyInput], opts: &CompileOptions) -> String {
    let statements = build_statements(inputs, opts);
    match opts.format {
        PolicyFormat::IamJson => {
            let mut text = serde_json::to_string_pretty(&render_iam_json(&statements, opts))
                .unwrap_or_else(|_| "{}".to_string());
            text.push('\n');
            text
        }
        PolicyFormat::Terraform => render_terraform(&statements, opts),
    }
}

/// The IAM JSON document as a value.
pub fn compile_iam_value(inputs: &[PolicyInput], opts: &CompileOptions) -> Value {
    let opts = CompileOptions {
        format: PolicyFormat::IamJson,
        ..opts.clone()
    };
    render_iam_json(&build_statements(inputs, &opts), &opts)
}

fn build_statements(inputs: &[PolicyInput], opts: &CompileOptions) -> Vec<Statement> {
    // One statement per (resource, condition), actions sorted: compact and sound.
    let mut groups: BTreeMap<(Vec<Part>, Condition), BTreeSet<String>> = BTreeMap::new();
    for item in inputs {
        let resource = parameterize_arn(&item.access.resource, opts);
        groups
            .entry((resource, condition(item)))
            .or_default()
            .insert(item.access.action.clone());
    }
    let mut used_sids: BTreeSet<String> = BTreeSet::new();
    groups
        .into_iter()
        .map(|((resource, condition), actions)| {
            let actions: Vec<String> = actions.into_iter().collect();
            let base = make_sid(&actions, &item_text(&resource), &condition);
            let mut sid = base.clone();
            let mut seq = 2usize;
            while !used_sids.insert(sid.clone()) {
                sid = format!("{base}{seq}");
                seq += 1;
            }
            Statement {
                sid,
                actions,
                resource,
                condition,
            }
        })
        .collect()
}

fn condition(item: &PolicyInput) -> Condition {
    let services: BTreeSet<&str> = item
        .services
        .iter()
        .map(String::as_str)
        .filter(|service| !service.is_empty())
        .collect();
    if services.is_empty() || !item.implied {
        return Condition::None;
    }
    let action = item.access.action.to_ascii_lowercase();
    if action == "iam:passrole" {
        let principals: BTreeSet<String> = services
            .iter()
            .filter(|service| **service != "iam")
            .map(|service| access::passed_to_service(service))
            .collect();
        if !principals.is_empty() {
            return Condition::PassedToService(principals.into_iter().collect());
        }
    } else if action.starts_with("kms:") {
        let via: BTreeSet<String> = services
            .iter()
            .filter(|service| **service != "kms")
            .map(|service| access::kms_via_service_name(service).to_string())
            .collect();
        if !via.is_empty() {
            return Condition::KmsViaService(via.into_iter().collect());
        }
    }
    Condition::None
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

/// Fill an ARN's `*` partition-free fields: the region and account given, or
/// Terraform references. Empty fields stay empty; a Bedrock foundation model
/// keeps region `*` (an inference profile may route to any region).
fn parameterize_arn(arn: &str, opts: &CompileOptions) -> Vec<Part> {
    let mut fields = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some(service), Some(region), Some(account), Some(rest)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return vec![Part::Text(arn.to_string())];
    };
    let (no_region, no_account) = empty_fields(service, rest);
    let terraform = opts.format == PolicyFormat::Terraform;
    let keep_region_star = service == "bedrock" && rest.starts_with("foundation-model/");
    let mut parts = vec![Part::Text("arn:".into())];
    if terraform && partition == "aws" {
        parts.push(Part::Ref(PARTITION_REF));
    } else {
        parts.push(Part::Text(partition.into()));
    }
    parts.push(Part::Text(format!(":{service}:")));
    if no_region {
        // `*` and empty both stand for the empty field.
    } else if region == "*" && !keep_region_star {
        match (&opts.region, terraform) {
            (Some(region), _) => parts.push(Part::Text(region.clone())),
            (None, true) => parts.push(Part::Ref(REGION_REF)),
            (None, false) => parts.push(Part::Text("*".into())),
        }
    } else {
        parts.push(Part::Text(region.into()));
    }
    parts.push(Part::Text(":".into()));
    if no_account {
    } else if account == "*" {
        match (&opts.account, terraform) {
            (Some(account), _) => parts.push(Part::Text(account.clone())),
            (None, true) => parts.push(Part::Ref(ACCOUNT_REF)),
            (None, false) => parts.push(Part::Text("*".into())),
        }
    } else {
        parts.push(Part::Text(account.into()));
    }
    parts.push(Part::Text(format!(":{rest}")));
    merge_text(parts)
}

fn merge_text(parts: Vec<Part>) -> Vec<Part> {
    let mut out: Vec<Part> = Vec::new();
    for part in parts {
        match (out.last_mut(), part) {
            (Some(Part::Text(previous)), Part::Text(text)) => previous.push_str(&text),
            (_, part) => out.push(part),
        }
    }
    out
}

/// The parts as plain text (references as `${…}`), for Sids and IAM JSON.
fn item_text(parts: &[Part]) -> String {
    parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => text.clone(),
            Part::Ref(reference) => format!("${{{reference}}}"),
        })
        .collect()
}

fn make_sid(actions: &[String], resource: &str, condition: &Condition) -> String {
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
        .find(|segment| !segment.is_empty() && *segment != "*" && !segment.contains("${"))
        .map(pascal_case)
        .filter(|tail| !tail.is_empty())
        .unwrap_or_else(|| "Access".to_string());
    let base = match condition {
        Condition::PassedToService(_) => format!("PassRole{tail}"),
        Condition::KmsViaService(_) => "KmsViaService".to_string(),
        Condition::None => format!("{}{tail}", pascal_case(prefix)),
    };
    let clean: String = base.chars().filter(char::is_ascii_alphanumeric).collect();
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

fn single_or_list(items: &[String]) -> Value {
    if items.len() == 1 {
        Value::String(items[0].clone())
    } else {
        json!(items)
    }
}

/// `kms:ViaService` values for a region (`*` when none is known, matched
/// with `StringLike`).
fn via_values(services: &[String], region: &str) -> Vec<String> {
    services
        .iter()
        .map(|service| format!("{service}.{region}.amazonaws.com"))
        .collect()
}

fn render_iam_json(statements: &[Statement], opts: &CompileOptions) -> Value {
    let list: Vec<Value> = statements
        .iter()
        .map(|statement| {
            let mut obj = serde_json::Map::new();
            obj.insert("Sid".into(), Value::String(statement.sid.clone()));
            obj.insert("Effect".into(), Value::String("Allow".into()));
            obj.insert("Action".into(), single_or_list(&statement.actions));
            obj.insert(
                "Resource".into(),
                Value::String(item_text(&statement.resource)),
            );
            match &statement.condition {
                Condition::None => {}
                Condition::PassedToService(principals) => {
                    obj.insert(
                        "Condition".into(),
                        json!({ "StringEquals": { "iam:PassedToService": single_or_list(principals) } }),
                    );
                }
                Condition::KmsViaService(services) => {
                    let region = opts.region.as_deref().unwrap_or("*");
                    let op = if region == "*" {
                        "StringLike"
                    } else {
                        "StringEquals"
                    };
                    obj.insert(
                        "Condition".into(),
                        json!({ op: { "kms:ViaService": single_or_list(&via_values(services, region)) } }),
                    );
                }
            }
            Value::Object(obj)
        })
        .collect();
    json!({ "Version": "2012-10-17", "Statement": list })
}

/// An HCL quoted string: `\`, `"` and control characters escaped, and the
/// template sequences `${` / `%{` doubled so they stay literal.
fn hcl_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' | '%' if chars.peek() == Some(&'{') => {
                out.push(ch);
                out.push(ch);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn hcl_string(parts: &[Part]) -> String {
    let mut out = String::from("\"");
    for part in parts {
        match part {
            Part::Text(text) => out.push_str(&hcl_literal(text)),
            Part::Ref(reference) => out.push_str(&format!("${{{reference}}}")),
        }
    }
    out.push('"');
    out
}

fn hcl_list(items: &[Vec<Part>]) -> String {
    let inner: Vec<String> = items.iter().map(|item| hcl_string(item)).collect();
    format!("[{}]", inner.join(", "))
}

fn text(value: &str) -> Vec<Part> {
    vec![Part::Text(value.to_string())]
}

fn render_terraform(statements: &[Statement], opts: &CompileOptions) -> String {
    let mut refs: BTreeSet<&'static str> = BTreeSet::new();
    let mut body = String::new();
    for statement in statements {
        for part in &statement.resource {
            if let Part::Ref(reference) = part {
                refs.insert(reference);
            }
        }
        let actions: Vec<Vec<Part>> = statement.actions.iter().map(|a| text(a)).collect();
        body.push_str("  statement {\n");
        body.push_str(&format!(
            "    sid       = {}\n",
            hcl_string(&text(&statement.sid))
        ));
        body.push_str("    effect    = \"Allow\"\n");
        body.push_str(&format!("    actions   = {}\n", hcl_list(&actions)));
        body.push_str(&format!(
            "    resources = {}\n",
            hcl_list(std::slice::from_ref(&statement.resource))
        ));
        let condition = match &statement.condition {
            Condition::None => None,
            Condition::PassedToService(principals) => Some((
                "StringEquals",
                "iam:PassedToService",
                principals.iter().map(|p| text(p)).collect::<Vec<_>>(),
            )),
            Condition::KmsViaService(services) => {
                let values = services
                    .iter()
                    .map(|service| match &opts.region {
                        Some(region) => text(&format!("{service}.{region}.amazonaws.com")),
                        None => {
                            refs.insert(REGION_REF);
                            vec![
                                Part::Text(format!("{service}.")),
                                Part::Ref(REGION_REF),
                                Part::Text(".amazonaws.com".into()),
                            ]
                        }
                    })
                    .collect();
                Some(("StringEquals", "kms:ViaService", values))
            }
        };
        if let Some((test, variable, values)) = condition {
            body.push('\n');
            body.push_str("    condition {\n");
            body.push_str(&format!("      test     = {}\n", hcl_string(&text(test))));
            body.push_str(&format!(
                "      variable = {}\n",
                hcl_string(&text(variable))
            ));
            body.push_str(&format!("      values   = {}\n", hcl_list(&values)));
            body.push_str("    }\n");
        }
        body.push_str("  }\n");
    }
    let mut out = String::new();
    for (reference, source) in [
        (PARTITION_REF, "data \"aws_partition\" \"current\" {}\n"),
        (REGION_REF, "data \"aws_region\" \"current\" {}\n"),
        (ACCOUNT_REF, "data \"aws_caller_identity\" \"current\" {}\n"),
    ] {
        if refs.contains(reference) {
            out.push_str(source);
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("data \"aws_iam_policy_document\" \"lenslet\" {\n");
    out.push_str(&body);
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(action: &str, resource: &str) -> PolicyInput {
        PolicyInput {
            access: Access::new(action, resource),
            ..PolicyInput::default()
        }
    }

    fn golden_inputs() -> Vec<PolicyInput> {
        vec![
            input("dynamodb:GetItem", "arn:aws:dynamodb:*:*:table/orders"),
            input("dynamodb:PutItem", "arn:aws:dynamodb:*:*:table/orders"),
            input("s3:GetObject", "arn:aws:s3:::shop-assets/*"),
            input("s3:ListBucket", "arn:aws:s3:::shop-assets"),
            input("sqs:SendMessage", "arn:aws:sqs:*:*:jobs"),
            input(
                "lambda:InvokeFunction",
                "arn:aws:lambda:*:*:function:thumbnailer:prod",
            ),
            input("apigateway:GET", "arn:aws:apigateway:*::/restapis/*"),
            input(
                "bedrock:InvokeModel",
                "arn:aws:bedrock:*::foundation-model/anthropic.claude-3-haiku-20240307-v1:0",
            ),
            input(
                "bedrock:InvokeModel",
                "arn:aws:bedrock:*:*:inference-profile/us.anthropic.claude-3-haiku-20240307-v1:0",
            ),
            PolicyInput {
                implied: true,
                ..input("iam:PassRole", "arn:aws:iam::*:role/service-role/worker")
            },
            PolicyInput {
                implied: true,
                ..input("kms:Decrypt", "arn:aws:kms:*:*:key/*")
            },
            input("route53:GetHostedZone", "arn:aws:route53:::hostedzone/Z123"),
            input("sts:GetSessionToken", "*"),
        ]
    }

    fn golden(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        if std::env::var_os("LENSLET_BLESS").is_some() {
            std::fs::write(&path, actual).expect("bless golden file");
        }
        let expected = std::fs::read_to_string(&path).expect("golden file");
        assert_eq!(actual, expected, "{name} differs from its golden file");
    }

    #[test]
    fn iam_json_matches_its_golden_file_and_keeps_empty_arn_fields() {
        let text = compile(&golden_inputs(), &CompileOptions::default());
        golden("policy.json", &text);
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

        let explicit = compile(
            &golden_inputs(),
            &CompileOptions {
                region: Some("us-east-1".into()),
                account: Some("123456789012".into()),
                ..CompileOptions::default()
            },
        );
        golden("policy-us-east-1.json", &explicit);
        for (needle, present) in [
            ("arn:aws:dynamodb:us-east-1:123456789012:table/orders", true),
            ("arn:aws:s3:::shop-assets", true),
            ("arn:aws:iam::123456789012:role/service-role/worker", true),
            ("arn:aws:apigateway:us-east-1::/restapis/*", true),
            ("arn:aws:route53:::hostedzone/Z123", true),
            // A foundation model keeps any region and no account.
            (
                "arn:aws:bedrock:*::foundation-model/anthropic.claude-3-haiku-20240307-v1:0",
                true,
            ),
            (
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/",
                true,
            ),
            (":123456789012:foundation-model", false),
            ("s3:us-east-1", false),
        ] {
            assert_eq!(explicit.contains(needle), present, "{needle}");
        }
    }

    #[test]
    fn terraform_matches_its_golden_file_and_is_well_formed_hcl() {
        let text = compile(
            &golden_inputs(),
            &CompileOptions {
                format: PolicyFormat::Terraform,
                ..CompileOptions::default()
            },
        );
        golden("policy.tf", &text);
        assert!(text.contains("data \"aws_iam_policy_document\" \"lenslet\" {"));
        assert!(text.contains("${data.aws_region.current.region}"));
        assert!(!text.contains("aws_region.current.name"));
        assert!(
            text.contains("\"arn:${data.aws_partition.current.partition}:s3:::shop-assets/*\"")
        );
        // Structural check: balanced braces and brackets outside strings,
        // every string closed, every statement with the four attributes.
        let (mut depth, mut brackets, mut in_string, mut escape) = (0i32, 0i32, false, false);
        for ch in text.chars() {
            if in_string {
                match (escape, ch) {
                    (true, _) => escape = false,
                    (false, '\\') => escape = true,
                    (false, '"') => in_string = false,
                    (false, '\n') => panic!("newline inside an HCL string"),
                    _ => {}
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                '[' => brackets += 1,
                ']' => brackets -= 1,
                _ => {}
            }
            assert!(depth >= 0 && brackets >= 0);
        }
        assert_eq!((depth, brackets, in_string), (0, 0, false));
        let statements = text.matches("  statement {\n").count();
        for attribute in [
            "sid       = ",
            "effect    = ",
            "actions   = ",
            "resources = ",
        ] {
            assert_eq!(text.matches(attribute).count(), statements, "{attribute}");
        }
    }

    #[test]
    fn hcl_strings_escape_quotes_backslashes_and_template_sequences() {
        assert_eq!(
            hcl_string(&text(r#"arn:aws:s3:::b/${aws:username}/%{x}/"q"\n"#)),
            r#""arn:aws:s3:::b/$${aws:username}/%%{x}/\"q\"\\n""#
        );
        assert_eq!(hcl_string(&text("a\nb")), r#""a\nb""#);
        // A literal `$` not followed by `{` stays as it is.
        assert_eq!(hcl_string(&text("fn:$LATEST")), r#""fn:$LATEST""#);
    }

    #[test]
    fn removed_formats_say_so_and_unknown_formats_list_the_choices() {
        assert_eq!(PolicyFormat::parse("IAM"), Ok(PolicyFormat::IamJson));
        assert_eq!(
            PolicyFormat::parse("terraform"),
            Ok(PolicyFormat::Terraform)
        );
        assert_eq!(
            PolicyFormat::parse("cdk").unwrap_err(),
            "--format cdk was removed in 12.0; use --format iam (any IaC tool accepts IAM JSON)"
        );
        for removed in ["cfn", "sam", "cloudformation"] {
            assert!(
                PolicyFormat::parse(removed)
                    .unwrap_err()
                    .contains("removed in 12.0")
            );
        }
        assert!(
            PolicyFormat::parse("yaml")
                .unwrap_err()
                .contains("iam or terraform")
        );
    }

    #[test]
    fn conditions_use_every_service_and_the_lock_and_report_paths_agree() {
        // Four services passed the same role: all four are allowed.
        let passrole = PolicyInput {
            implied: true,
            services: vec![
                "lambda".into(),
                "states".into(),
                "ecs".into(),
                "scheduler".into(),
            ],
            ..input("iam:PassRole", "arn:aws:iam::*:role/app")
        };
        let doc = compile_iam_value(std::slice::from_ref(&passrole), &CompileOptions::default());
        assert_eq!(
            doc["Statement"][0]["Condition"]["StringEquals"]["iam:PassedToService"],
            json!([
                "ecs-tasks.amazonaws.com",
                "lambda.amazonaws.com",
                "scheduler.amazonaws.com",
                "states.amazonaws.com"
            ])
        );
        let kms = PolicyInput {
            implied: true,
            services: vec!["s3".into(), "streams.dynamodb".into()],
            ..input("kms:Decrypt", "arn:aws:kms:*:*:key/*")
        };
        let doc = compile_iam_value(std::slice::from_ref(&kms), &CompileOptions::default());
        assert_eq!(
            doc["Statement"][0]["Condition"]["StringLike"]["kms:ViaService"],
            json!(["dynamodb.*.amazonaws.com", "s3.*.amazonaws.com"])
        );

        // The same access from a lock and from a report: the same statements.
        let mut lock = Lock::default();
        let mut entries = Vec::new();
        for item in golden_inputs() {
            lock.access.insert(item.access.clone());
            if item.implied {
                lock.implied.insert(item.access.clone());
            }
            entries.push(AccessEntry {
                action: item.access.action.clone(),
                resource: item.access.resource.clone(),
                used: crate::summary::AccessUse {
                    implied: item.implied,
                    via: vec!["LAMBDA CreateFunction".into()],
                    services: vec!["lambda".into()],
                    ..Default::default()
                },
                new: false,
            });
        }
        let test_only = Access::new("dynamodb:CreateTable", "arn:aws:dynamodb:*:*:table/orders");
        lock.access.insert(test_only.clone());
        lock.test.insert(test_only);
        for format in PolicyFormat::ALL {
            let opts = CompileOptions {
                format,
                ..CompileOptions::default()
            };
            let from_lock = compile(&inputs_from_lock(&lock), &opts);
            assert_eq!(from_lock, compile(&inputs_from_entries(&entries), &opts));
            assert!(
                !from_lock.contains("dynamodb:CreateTable"),
                "test lines stay out"
            );
        }
    }
}
