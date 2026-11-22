//! IAM policies: checking recorded access against one (`lenslet check
//! --policy`), and writing one from observed access (`lenslet policy`).
//!
//! The check evaluates `Allow` / `Deny`, `Action` / `NotAction`, `Resource` /
//! `NotResource` and IAM wildcards, failing closed. A statement with a
//! `Condition` (or a `${...}` policy variable) is not evaluated: it never
//! allows anything, and a denial it may decide is reported as `conditional`
//! with the statement that would decide it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{access::Access, lock::Lock, summary::AccessEntry, trace::Trace};

/// A parsed identity policy.
#[derive(Clone, Debug)]
pub struct Policy {
    /// How reports name it (its path relative to the project).
    pub label: String,
    statements: Vec<Statement>,
}

#[derive(Clone, Debug)]
struct Statement {
    /// `Sid`, or `Statement[i]` (0-based) when the statement has none.
    label: String,
    allow: bool,
    actions: Vec<String>,
    not_actions: Vec<String>,
    resources: Vec<String>,
    not_resources: Vec<String>,
    /// Has a `Condition` or a policy variable: matched, never evaluated.
    conditional: bool,
}

/// Why a permission is not allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Allowed,
    /// A `Deny` statement matches.
    ExplicitDeny,
    /// No `Allow` statement grants it.
    NoAllow,
}

/// The policy's verdict on one permission, and the statement that decided it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub allowed: bool,
    pub reason: Reason,
    /// The deciding statement's `Sid` (or `Statement[i]`): the denying one,
    /// the first allowing one, or the conditional one. `None` when no
    /// statement matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The deciding statement has a condition Lenslet does not evaluate: a
    /// conditional `Deny` that may apply, or a conditional `Allow` that is
    /// the only one matching. Never counted as allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conditional: bool,
}

impl Decision {
    fn allowed(by: String) -> Self {
        Decision {
            allowed: true,
            reason: Reason::Allowed,
            by: Some(by),
            conditional: false,
        }
    }

    fn denied(reason: Reason, by: Option<String>, conditional: bool) -> Self {
        Decision {
            allowed: false,
            reason,
            by,
            conditional,
        }
    }
}

impl Policy {
    pub fn from_file(path: &Path, label: impl Into<String>) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("read policy {}", path.display()))?;
        Self::parse(&text, label).with_context(|| format!("parse IAM policy {}", path.display()))
    }

    pub fn parse(text: &str, label: impl Into<String>) -> Result<Self> {
        let doc: Value = serde_json::from_str(text).context("invalid JSON")?;
        let raw = match doc.get("Statement") {
            Some(Value::Array(items)) => items.clone(),
            Some(single @ Value::Object(_)) => vec![single.clone()],
            _ => anyhow::bail!("IAM policy must contain a `Statement` array or object"),
        };
        let mut statements = Vec::with_capacity(raw.len());
        for (index, item) in raw.into_iter().enumerate() {
            let Some(obj) = item.as_object() else {
                anyhow::bail!("each IAM Statement must be a JSON object");
            };
            if obj.get("NotPrincipal").is_some()
                || obj
                    .get("Principal")
                    .is_some_and(|p| p != "*" && p.get("AWS") != Some(&json!("*")))
            {
                anyhow::bail!(
                    "IAM Statement restricts `Principal`, which Lenslet cannot evaluate locally; pass an identity policy without principal restrictions"
                );
            }
            let effect = obj.get("Effect").and_then(Value::as_str).unwrap_or("");
            let allow = if effect.eq_ignore_ascii_case("allow") {
                true
            } else if effect.eq_ignore_ascii_case("deny") {
                false
            } else {
                anyhow::bail!("IAM Statement Effect must be `Allow` or `Deny`, got `{effect}`");
            };
            let list = |key: &str| -> Result<Vec<String>> {
                match obj.get(key) {
                    None => Ok(Vec::new()),
                    Some(Value::String(s)) => Ok(vec![s.clone()]),
                    Some(Value::Array(arr)) => arr
                        .iter()
                        .map(|v| {
                            v.as_str().map(str::to_string).ok_or_else(|| {
                                anyhow::anyhow!("IAM Statement `{key}` entries must be strings")
                            })
                        })
                        .collect(),
                    _ => {
                        anyhow::bail!("IAM Statement `{key}` must be a string or array of strings")
                    }
                }
            };
            if obj.contains_key("Action") && obj.contains_key("NotAction") {
                anyhow::bail!(
                    "IAM Statement has both `Action` and `NotAction`; AWS accepts only one of them"
                );
            }
            if obj.contains_key("Resource") && obj.contains_key("NotResource") {
                anyhow::bail!(
                    "IAM Statement has both `Resource` and `NotResource`; AWS accepts only one of them"
                );
            }
            let actions = list("Action")?;
            let not_actions = list("NotAction")?;
            let resources = list("Resource")?;
            let not_resources = list("NotResource")?;
            if actions.is_empty() && not_actions.is_empty() {
                anyhow::bail!("IAM Statement must specify `Action` or `NotAction`");
            }
            if resources.is_empty() && not_resources.is_empty() {
                anyhow::bail!("IAM Statement must specify `Resource` or `NotResource`");
            }
            let has_variable = resources
                .iter()
                .chain(&not_resources)
                .any(|resource| resource.contains("${"));
            let conditional = has_variable || obj.get("Condition").is_some_and(|c| !c.is_null());
            // A policy variable stands for any value when matching.
            let resources: Vec<String> = resources.iter().map(|r| without_variables(r)).collect();
            let not_resources: Vec<String> =
                not_resources.iter().map(|r| without_variables(r)).collect();
            let label = obj
                .get("Sid")
                .and_then(Value::as_str)
                .filter(|sid| !sid.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Statement[{index}]"));
            statements.push(Statement {
                label,
                allow,
                actions,
                not_actions,
                resources,
                not_resources,
                conditional,
            });
        }
        Ok(Policy {
            label: label.into(),
            statements,
        })
    }

    /// The verdict on `access`. An explicit (unconditional) `Deny` wins; then
    /// a conditional `Deny` that may apply; then an unconditional `Allow`;
    /// a conditional `Allow` alone allows nothing.
    pub fn decide(&self, access: &Access) -> Decision {
        let mut allowed_by: Option<&str> = None;
        let mut conditional_allow: Option<&str> = None;
        let mut conditional_deny: Option<&str> = None;
        for statement in &self.statements {
            if !statement.matches(access) {
                continue;
            }
            match (statement.allow, statement.conditional) {
                (false, false) => {
                    return Decision::denied(
                        Reason::ExplicitDeny,
                        Some(statement.label.clone()),
                        false,
                    );
                }
                (false, true) => {
                    conditional_deny.get_or_insert(&statement.label);
                }
                (true, false) => {
                    allowed_by.get_or_insert(&statement.label);
                }
                (true, true) => {
                    conditional_allow.get_or_insert(&statement.label);
                }
            }
        }
        if let Some(by) = conditional_deny {
            return Decision::denied(Reason::ExplicitDeny, Some(by.to_string()), true);
        }
        if let Some(by) = allowed_by {
            return Decision::allowed(by.to_string());
        }
        match conditional_allow {
            Some(by) => Decision::denied(Reason::NoAllow, Some(by.to_string()), true),
            None => Decision::denied(Reason::NoAllow, None, false),
        }
    }
}

impl Statement {
    /// Whether this statement applies to `access`. An `Allow` must cover
    /// every resource the (possibly partly unknown) ARN stands for; a `Deny`
    /// applies when it may match one of them.
    fn matches(&self, access: &Access) -> bool {
        let action_ok = if !self.actions.is_empty() {
            self.actions
                .iter()
                .any(|p| action_matches(p, &access.action))
        } else {
            !self
                .not_actions
                .iter()
                .any(|p| action_matches(p, &access.action))
        };
        if !action_ok {
            return false;
        }
        type Matcher = fn(&str, &str) -> bool;
        if !self.resources.is_empty() {
            let matches: Matcher = if self.allow { arn_covers } else { arn_overlaps };
            self.resources.iter().any(|p| matches(p, &access.resource))
        } else {
            let excluded: Matcher = if self.allow { arn_overlaps } else { arn_covers };
            !self
                .not_resources
                .iter()
                .any(|p| excluded(p, &access.resource))
        }
    }
}

/// `home/${aws:username}/*` → `home/*/*`.
fn without_variables(resource: &str) -> String {
    let mut out = String::new();
    let mut rest = resource;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => {
                out.push('*');
                rest = &rest[start + end + 1..];
            }
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn action_matches(pattern: &str, actual: &str) -> bool {
    wildcard_match(
        &pattern.to_ascii_lowercase(),
        &actual.to_ascii_lowercase(),
        false,
    )
}

/// Whether IAM ARN pattern `pattern` matches every resource `actual` can
/// stand for. A `*` in `actual` is an unknown part, so only a `*` in the
/// pattern covers it: an Allow pinned to a region does not cover a region
/// Lenslet does not know.
pub fn arn_covers(pattern: &str, actual: &str) -> bool {
    arn_compare(pattern, actual, glob)
}

/// Whether IAM ARN pattern `pattern` matches at least one resource `actual`
/// can stand for (a `*` in `actual` is an unknown part).
pub fn arn_overlaps(pattern: &str, actual: &str) -> bool {
    arn_compare(pattern, actual, globs_intersect)
}

/// Compare ARNs part by part. The account is never known locally (it is
/// always `*` in recorded access), so any account in a policy matches; the
/// partition likewise. Service, region and resource compare as patterns.
fn arn_compare(pattern: &str, actual: &str, matches: impl Fn(&str, &str) -> bool) -> bool {
    if pattern == "*" {
        return true;
    }
    if let (Some(p), Some(a)) = (split_arn(pattern), split_arn(actual)) {
        let relaxed = |p: &str, a: &str| p == "*" || a == "*" || glob(p, a);
        return relaxed(p[0], a[0])
            && matches(&p[1].to_ascii_lowercase(), &a[1].to_ascii_lowercase())
            && matches(p[2], a[2])
            && relaxed(p[3], a[3])
            && matches(p[4], a[4]);
    }
    matches(pattern, actual)
}

fn split_arn(arn: &str) -> Option<[&str; 5]> {
    let rest = arn.strip_prefix("arn:")?;
    let (partition, rest) = rest.split_once(':')?;
    let (service, rest) = rest.split_once(':')?;
    let (region, rest) = rest.split_once(':')?;
    let (account, resource) = rest.split_once(':')?;
    Some([partition, service, region, account, resource])
}

/// IAM wildcard matching of a resource. A `*` in `text` is an unknown run of
/// characters: only a pattern `*` absorbs it.
fn glob(pattern: &str, text: &str) -> bool {
    wildcard_match(pattern, text, true)
}

fn wildcard_match(pattern: &str, text: &str, star_is_unknown: bool) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if pi < p.len()
            && !(star_is_unknown && t[ti] == '*')
            && (p[pi] == '?' || p[pi] == t[ti])
        {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether two IAM wildcard patterns match at least one common string: a
/// search over the product of their automata, O(len(a) × len(b)).
fn globs_intersect(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let width = b.len() + 1;
    let mut seen = vec![false; (a.len() + 1) * width];
    let mut stack = vec![(0usize, 0usize)];
    while let Some((i, j)) = stack.pop() {
        if std::mem::replace(&mut seen[i * width + j], true) {
            continue;
        }
        if i == a.len() && j == b.len() {
            return true;
        }
        let star_a = a.get(i) == Some(&'*');
        let star_b = b.get(j) == Some(&'*');
        // A `*` may match nothing.
        if star_a {
            stack.push((i + 1, j));
        }
        if star_b {
            stack.push((i, j + 1));
        }
        // Both consume one common character.
        if let (Some(&x), Some(&y)) = (a.get(i), b.get(j)) {
            let any_x = x == '*' || x == '?';
            let any_y = y == '*' || y == '?';
            if any_x || any_y || x == y {
                let next_i = if star_a { i } else { i + 1 };
                let next_j = if star_b { j } else { j + 1 };
                if (next_i, next_j) != (i, j) {
                    stack.push((next_i, next_j));
                }
            }
        }
    }
    false
}

/// One permission of one recorded call that a policy does not allow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denial {
    /// The call's id.
    pub call: String,
    pub service: String,
    pub operation: String,
    pub action: String,
    pub resource: String,
    /// Needed elsewhere (a KMS key, a passed role) rather than by the call itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub implied: bool,
    /// The resource is `*` because the request body could not be read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
    #[serde(flatten)]
    pub decision: Decision,
}

/// Check recorded calls against `policy`: each call's `concrete` access,
/// derived live from its complete request body, never re-derived from the
/// stored (possibly truncated) one.
pub fn check(calls: &[Trace], policy: &Policy) -> Vec<Denial> {
    let mut out = Vec::new();
    for call in calls {
        for item in &call.concrete {
            let decision = policy.decide(item);
            if decision.allowed {
                continue;
            }
            // The footprint entry this permission falls under says whether
            // it is implied, and whether its resource could be read.
            let footprint = call
                .access
                .iter()
                .map(|entry| (entry, false))
                .chain(call.implied.iter().map(|entry| (entry, true)))
                .find(|(entry, _)| {
                    entry.action == item.action && arn_covers(&entry.resource, &item.resource)
                });
            out.push(Denial {
                call: call.id.clone(),
                service: call.service.clone(),
                operation: call.operation.clone(),
                action: item.action.clone(),
                resource: item.resource.clone(),
                implied: footprint.is_some_and(|(_, implied)| implied),
                unverified: footprint.is_some_and(|(entry, _)| entry.unverified),
                decision,
            });
        }
    }
    out
}

/// The permissions a lock grants, without its `# test` lines.
pub fn from_lock(lock: &Lock) -> Vec<Access> {
    lock.access.difference(&lock.test).cloned().collect()
}

/// The permissions a run used (not the lock lines it left unused).
pub fn from_entries(entries: &[AccessEntry]) -> Vec<Access> {
    entries
        .iter()
        .filter(|entry| !entry.unused)
        .map(|entry| Access::new(entry.action.clone(), entry.resource.clone()))
        .collect()
}

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
    use super::*;

    fn access(action: &str, resource: &str) -> Access {
        Access::new(action, resource)
    }

    fn policy(json: &str) -> Policy {
        Policy::parse(json, "policy.json").unwrap()
    }

    fn allows(policy: &Policy, action: &str, resource: &str) -> bool {
        policy.decide(&access(action, resource)).allowed
    }

    #[test]
    fn evaluates_allow_deny_wildcards_and_concrete_arns() {
        let policy = policy(
            r#"{"Version":"2012-10-17","Statement":[
                {"Effect":"Allow","Action":["s3:Get*","dynamodb:PutItem"],"Resource":[
                    "arn:aws:s3:::assets/*",
                    "arn:aws:dynamodb:us-east-1:123456789012:table/orders"]},
                {"Sid":"NoAcl","Effect":"Deny","Action":"s3:GetObjectAcl","Resource":"*"}
            ]}"#,
        );
        assert!(allows(&policy, "s3:GetObject", "arn:aws:s3:::assets/a.png"));
        assert!(allows(
            &policy,
            "dynamodb:PutItem",
            "arn:aws:dynamodb:us-east-1:*:table/orders"
        ));
        assert_eq!(
            policy.decide(&access("s3:GetObjectAcl", "arn:aws:s3:::assets/a")),
            Decision::denied(Reason::ExplicitDeny, Some("NoAcl".into()), false),
            "explicit Deny overrides Allow"
        );
        assert_eq!(
            policy.decide(&access(
                "dynamodb:DeleteTable",
                "arn:aws:dynamodb:us-east-1:*:table/orders"
            )),
            Decision::denied(Reason::NoAllow, None, false)
        );
    }

    #[test]
    fn an_allow_pinned_to_a_region_covers_that_region_only() {
        let policy = policy(
            r#"{"Statement":[
                {"Effect":"Allow","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:us-east-1:*:jobs"},
                {"Sid":"NoEu","Effect":"Deny","Action":"sqs:*","Resource":"arn:aws:sqs:eu-west-1:*:*"}
            ]}"#,
        );
        assert!(allows(
            &policy,
            "sqs:SendMessage",
            "arn:aws:sqs:us-east-1:*:jobs"
        ));
        assert!(!allows(
            &policy,
            "sqs:SendMessage",
            "arn:aws:sqs:eu-west-1:*:jobs"
        ));
        // An unknown region could be any: the pinned Allow does not cover
        // it, and the Deny may apply.
        assert_eq!(
            policy.decide(&access("sqs:SendMessage", "arn:aws:sqs:*:*:jobs")),
            Decision::denied(Reason::ExplicitDeny, Some("NoEu".into()), false)
        );
        let any = super::tests::policy(
            r#"{"Statement":[{"Effect":"Allow","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:*:*:jobs"}]}"#,
        );
        assert!(allows(&any, "sqs:SendMessage", "arn:aws:sqs:*:*:jobs"));
        assert!(allows(
            &any,
            "sqs:SendMessage",
            "arn:aws:sqs:us-east-1:*:jobs"
        ));
        // The account is never known locally: a concrete one matches.
        let account = super::tests::policy(
            r#"{"Statement":[{"Effect":"Allow","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:us-east-1:123456789012:jobs"}]}"#,
        );
        assert!(allows(
            &account,
            "sqs:SendMessage",
            "arn:aws:sqs:us-east-1:*:jobs"
        ));
    }

    #[test]
    fn conditional_statements_are_reported_and_never_allow() {
        let policy = policy(
            r#"{"Statement":[
                {"Sid":"TaggedOnly","Effect":"Allow","Action":"dynamodb:GetItem","Resource":"*",
                 "Condition":{"StringEquals":{"aws:ResourceTag/env":"dev"}}},
                {"Sid":"Puts","Effect":"Allow","Action":"dynamodb:PutItem","Resource":"*"},
                {"Sid":"NotFromEu","Effect":"Deny","Action":"dynamodb:PutItem","Resource":"*",
                 "Condition":{"StringEquals":{"aws:RequestedRegion":"eu-west-1"}}},
                {"Sid":"Home","Effect":"Allow","Action":"s3:GetObject",
                 "Resource":"arn:aws:s3:::home/${aws:username}/*"}
            ]}"#,
        );
        assert_eq!(
            policy.decide(&access(
                "dynamodb:GetItem",
                "arn:aws:dynamodb:us-east-1:*:table/t"
            )),
            Decision::denied(Reason::NoAllow, Some("TaggedOnly".into()), true)
        );
        assert_eq!(
            policy.decide(&access(
                "dynamodb:PutItem",
                "arn:aws:dynamodb:us-east-1:*:table/t"
            )),
            Decision::denied(Reason::ExplicitDeny, Some("NotFromEu".into()), true)
        );
        assert_eq!(
            policy.decide(&access("s3:GetObject", "arn:aws:s3:::home/ann/a")),
            Decision::denied(Reason::NoAllow, Some("Home".into()), true)
        );
        assert!(
            Policy::parse(
                r#"{"Statement":{"Effect":"Allow","Action":"*","Resource":"*","Condition":{"DateLessThan":{"aws:CurrentTime":"2030-01-01T00:00:00Z"}}}}"#,
                "p"
            )
            .is_ok(),
            "any condition parses; none is evaluated"
        );
    }

    #[test]
    fn an_unknown_resource_fails_closed_for_deny_and_open_only_for_allow_star() {
        let policy = policy(
            r#"{"Statement":[
                {"Effect":"Allow","Action":"dynamodb:*","Resource":"*"},
                {"Sid":"KeepAudit","Effect":"Deny","Action":"dynamodb:DeleteItem","Resource":"arn:aws:dynamodb:*:*:table/audit"}
            ]}"#,
        );
        assert_eq!(
            policy.decide(&access("dynamodb:DeleteItem", "*")),
            Decision::denied(Reason::ExplicitDeny, Some("KeepAudit".into()), false)
        );
        assert!(allows(
            &policy,
            "dynamodb:DeleteItem",
            "arn:aws:dynamodb:us-east-1:*:table/orders-*"
        ));
        assert!(!allows(
            &policy,
            "dynamodb:DeleteItem",
            "arn:aws:dynamodb:us-east-1:*:table/aud*"
        ));

        let scoped = super::tests::policy(
            r#"{"Statement":[{"Effect":"Allow","Action":"dynamodb:GetItem","Resource":[
                "arn:aws:dynamodb:*:*:table/orders",
                "arn:aws:dynamodb:*:*:table/carts-*"
            ]}]}"#,
        );
        assert!(!allows(&scoped, "dynamodb:GetItem", "*"));
        assert!(!allows(
            &scoped,
            "dynamodb:GetItem",
            "arn:aws:dynamodb:*:*:table/orders*"
        ));
        assert!(allows(
            &scoped,
            "dynamodb:GetItem",
            "arn:aws:dynamodb:*:*:table/carts-*"
        ));

        let not_resource = super::tests::policy(
            r#"{"Statement":[
                {"Effect":"Allow","Action":"s3:GetObject","NotResource":"arn:aws:s3:::vault/*"},
                {"Effect":"Deny","Action":"s3:GetObject","NotResource":"arn:aws:s3:::public/*"}
            ]}"#,
        );
        assert_eq!(
            not_resource
                .decide(&access("s3:GetObject", "*"))
                .by
                .as_deref(),
            Some("Statement[1]")
        );
        assert!(allows(
            &not_resource,
            "s3:GetObject",
            "arn:aws:s3:::public/a"
        ));
        assert_eq!(
            not_resource
                .decide(&access("s3:PutObject", "arn:aws:s3:::public/a"))
                .reason,
            Reason::NoAllow
        );
        let not_action = super::tests::policy(
            r#"{"Statement":[{"Effect":"Allow","NotAction":"iam:*","Resource":"*"}]}"#,
        );
        assert!(allows(&not_action, "s3:GetObject", "*"));
        assert!(!allows(&not_action, "iam:PassRole", "*"));
    }

    #[test]
    fn check_evaluates_the_stored_concrete_access() {
        use crate::access::CallAccess;
        let call = Trace {
            id: "c1".into(),
            service: "s3".into(),
            operation: "PutObject".into(),
            // What the footprint says, and what the live derivation saw.
            access: vec![CallAccess {
                action: "s3:PutObject".into(),
                resource: "arn:aws:s3:::uploads/*".into(),
                new: false,
                unverified: false,
            }],
            implied: vec![CallAccess {
                action: "kms:GenerateDataKey".into(),
                resource: "arn:aws:kms:*:*:key/*".into(),
                new: false,
                unverified: false,
            }],
            concrete: vec![
                access("s3:PutObject", "arn:aws:s3:::uploads/private/a.txt"),
                access("kms:GenerateDataKey", "arn:aws:kms:us-east-1:*:key/k1"),
            ],
            ..Trace::default()
        };
        let policy = policy(
            r#"{"Statement":[
                {"Effect":"Allow","Action":"s3:PutObject","Resource":"arn:aws:s3:::uploads/*"},
                {"Sid":"NoPrivate","Effect":"Deny","Action":"s3:PutObject","Resource":"arn:aws:s3:::uploads/private/*"}
            ]}"#,
        );
        let denied = check(&[call], &policy);
        assert_eq!(denied.len(), 2, "{denied:?}");
        assert_eq!(denied[0].resource, "arn:aws:s3:::uploads/private/a.txt");
        assert_eq!(denied[0].decision.by.as_deref(), Some("NoPrivate"));
        assert!(!denied[0].implied);
        assert_eq!(denied[1].action, "kms:GenerateDataKey");
        assert!(denied[1].implied);
        assert_eq!(denied[1].decision.reason, Reason::NoAllow);
        let json = serde_json::to_value(&denied[1]).unwrap();
        assert_eq!(json["call"], "c1");
        assert_eq!(json["reason"], "no_allow");
    }

    #[test]
    fn wildcard_patterns_compare_as_sets() {
        assert!(arn_overlaps("table/a*", "table/*b"));
        assert!(!arn_overlaps("table/a?", "table/abc*"));
        assert!(arn_overlaps("table/a?", "table/a*"));
        assert!(!arn_covers("table/a?", "table/a*"));
        assert!(arn_covers("table/*", "table/a*"));
        assert!(arn_covers("*a", "*ba"));
        assert!(!arn_covers("*b", "*"));
        assert!(arn_covers("arn:aws:s3:::b/?", "arn:aws:s3:::b/x"));
        assert!(action_matches("s3:Get*", "s3:GetObject"));
    }

    #[test]
    fn policies_aws_would_reject_are_rejected_with_a_reason() {
        for (statement, needle) in [
            (
                r#"{"Effect":"Allow","Action":"s3:GetObject","NotAction":"s3:PutObject","Resource":"*"}"#,
                "both `Action` and `NotAction`",
            ),
            (
                r#"{"Effect":"Allow","Action":"s3:GetObject","Resource":"*","NotResource":"arn:aws:s3:::b/*"}"#,
                "both `Resource` and `NotResource`",
            ),
            (
                r#"{"Effect":"Maybe","Action":"s3:GetObject","Resource":"*"}"#,
                "Effect must be",
            ),
            (
                r#"{"Effect":"Allow","Principal":{"AWS":"arn:aws:iam::1:root"},"Action":"s3:GetObject","Resource":"*"}"#,
                "Principal",
            ),
            (
                r#"{"Effect":"Allow","Resource":"*"}"#,
                "`Action` or `NotAction`",
            ),
        ] {
            let error = Policy::parse(
                &format!(r#"{{"Version":"2012-10-17","Statement":[{statement}]}}"#),
                "p",
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(needle), "{error:#}");
        }
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

        // A generated policy passes a check of the access it was made from.
        let generated = Policy::parse(&text, "generated").unwrap();
        for item in golden_access() {
            assert!(generated.decide(&item).allowed, "{item:?}");
        }
    }

    #[test]
    fn a_lock_compiles_without_its_test_lines() {
        let lock = crate::lock::parse(
            "sqs:SendMessage  arn:aws:sqs:*:*:jobs\n\
             sqs:ReceiveMessage  arn:aws:sqs:*:*:jobs\n\
             s3:GetObject  arn:aws:s3:::assets/*\n\
             s3:CreateBucket  arn:aws:s3:::assets  # test\n",
        );
        let doc = compile_value(&from_lock(&lock));
        let statements = doc["Statement"].as_array().unwrap();
        assert_eq!(statements.len(), 2, "{doc}");
        assert_eq!(statements[0]["Resource"], "arn:aws:s3:::assets/*");
        assert_eq!(
            statements[1]["Action"],
            json!(["sqs:ReceiveMessage", "sqs:SendMessage"])
        );
    }
}
