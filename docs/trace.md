# Lenslet run files (format 4)

Each `lenslet run` writes one JSONL file: `~/.lenslet/runs/YYYYMMDD-HHMMSS-PID.jsonl`,
or `--trace PATH` (an existing file there is replaced). Every line is an
object with a `type`:

```text
{"type":"run", ...}      first line: what the run is
{"type":"call", ...}     one per proxied call, written when the call finishes
{"type":"end", ...}      last line, once the run ends: the report
```

Next to it, `<run>.log` holds the test command's combined stdout and stderr
(owner-only), and `<run>.emulator.log` moto's output when Lenslet started moto.

Lenslet computes every fact once, when it writes the line: the error, the
IAM access, whether it is in the lock. The window, `--json` and `lenslet
check` only read those facts, so they always agree. A reader follows a live
run by reading the file as it grows (each line is written whole, with one
write).

Readers refuse any other format: the `run` header and the `end` report carry
`version: 4`, and a file with another version fails with `run file <path> is
format N; this Lenslet reads format 4 — re-run your tests`. Readers ignore
unknown fields.

## `run`

| field | meaning |
| --- | --- |
| `version` | `4` |
| `run` | the file stem, e.g. `20261115-095354-9655` |
| `command` | argv of the test command |
| `cwd` | where `lenslet run` was started (the default lock is `cwd/lenslet.lock`; `lenslet check` defaults to the newest finished run of the current directory) |
| `pid` | the `lenslet run` process. It is a live run while the file has no `end` line and this process is alive. To stop a run, send `SIGTERM` (or `SIGINT`) to its process group: Lenslet stops the test command's process group, writes the `end` report and stops the moto it started |
| `endpoint` | what SDKs point at, e.g. `http://127.0.0.1:37505` |
| `upstream` | the emulator's URL, e.g. `https://127.0.0.1:9298` |
| `emulator` | what answered the calls: `moto (started by Lenslet)` when Lenslet started moto for the run (`--emulator moto`, or no emulator was running), `moto`, `localstack` or `aws-emulator` when one was found running (recognized by its protocol), or the upstream URL for anything else |
| `started` | RFC3339 |
| `lock` | the lock path (relative to `cwd` when inside it) the run is compared with, or `null` when there is no lock |
| `log` | the path of `<run>.log`, the test command's combined stdout and stderr |
| `allow_docker` | `true` when `--allow-docker` let container bridge connections reach the proxy (omitted when false) |

## `call`

| field | meaning |
| --- | --- |
| `id` | unique call id |
| `at` | RFC3339 request-arrival time |
| `method`, `path`, `host` | as sent; `path` includes the query |
| `service` | SigV4 scope service (header or presigned URL); `s3` for SigV2; else `aws` |
| `operation` | from `x-amz-target`, `x-id`, the Query `Action=`, the Smithy RPCv2 path, the S3/Lambda/Bedrock/API Gateway REST shape, or the HTTP method |
| `status`, `ms` | HTTP status and full exchange time (after the response body ended) |
| `authorization` | an Authorization header was present |
| `region` | SigV4 scope region (omitted when unsigned) |
| `invocation_id` | SDK `amz-sdk-invocation-id`, shared across retries of one call |
| `attempt`, `max_attempts` | retry attempt (`> 1`) and configured max from `amz-sdk-request` |
| `error` | proxy-side failure, e.g. the emulator was unreachable (status `502`) |
| `problem` | `{code, message?, hint?}` for status ≥ 400 or a proxy failure; `hint` is the likely cause, including cross-call causes (a resource deleted earlier in the run, created in another region) |
| `access` | `[{action, resource, new?, unverified?}]` this call needs, as the lock records it: usually one; one per table/parameter for batches; each item's action for a DynamoDB transaction; the source read for an S3 copy. `new`: not in the lock (only when there is one) |
| `implied` | `[{action, resource, new?, unverified?}]` needed elsewhere (SSE-KMS key, passed role) |
| `concrete` | `[{action, resource}]`: the exact permissions, own then implied, derived when the call finished from its complete request body (exact S3 keys, the signing region, un-normalized names). What `lenslet check --policy` evaluates; it never re-derives from the stored body |
| `request`, `response` | `{headers, content_type?, body?}` |

A call has `new` access when any of its `access` or `implied` entries has
`new: true`.

`body` is `{text?, encoding?, bytes, truncated, binary, sha256}`. Headers,
query strings and bodies are preserved verbatim with **zero redaction**.
`text` holds at most 1 MiB of the body as text: `aws-chunked` is unframed,
`gzip` decompressed, and Smithy RPCv2 CBOR decoded into JSON; `encoding`
names what was decoded. Other encodings, and binary payloads, keep only
`bytes` and `sha256` (`binary: true`). `bytes` and `sha256` always cover the
whole wire body; `truncated` means `text` is a prefix. Wire bytes are
forwarded untouched.

### Access derivation

Derivation reads the complete request body (up to 64 MiB) of a call whose
resource is named in its body, whatever the run file keeps.

- **Action**: `{iam prefix}:{operation}`, plus exceptions. The SigV4 names
  `monitoring`, `email` and `bedrock-runtime` become `cloudwatch`, `ses` and
  `bedrock`. S3 `ListObjectsV2` becomes `s3:ListBucket`, `HeadObject` becomes
  `s3:GetObject`, multipart calls become `s3:PutObject`, Lambda `Invoke` becomes
  `lambda:InvokeFunction`, and Bedrock `Converse` becomes `bedrock:InvokeModel`.
  `sts:GetCallerIdentity`, `AssumeRoleWithWebIdentity` and `AssumeRoleWithSAML`
  need nothing from the caller. KMS `ReEncrypt` needs `kms:ReEncryptFrom` on
  the source key and `kms:ReEncryptTo` on the destination key. Secrets Manager
  `BatchGetSecretValue` needs `secretsmanager:BatchGetSecretValue` on `*` and
  `secretsmanager:GetSecretValue` on each secret in `SecretIdList` (on `*` for
  the `Filters` form). API Gateway's management API (v1 and v2 alike) is
  authorized by HTTP verb: `apigateway:GET` on
  `arn:aws:apigateway:*::/restapis/*` (the account field is empty, generated
  ids are `*`, v2 paths drop their `/v2` prefix: `/apis/*`); invoking a
  deployed API is `execute-api:Invoke` on `{api-id}/{stage}/{METHOD}/{path}`.
  `TransactWriteItems` and
  `TransactGetItems` are not IAM actions: each item contributes its own
  (`PutItem`, `UpdateItem`, `DeleteItem`, `ConditionCheckItem`, `GetItem`).
  `CopyObject` and `UploadPartCopy` also need `s3:GetObject` on the source from
  `x-amz-copy-source`.
- **Resource**: read from the request for S3, DynamoDB (tables, indexes, and
  Streams `StreamArn`; one entry per table, since an IAM resource names one
  table), SQS, SNS, Lambda, SSM Parameter Store, CloudWatch Logs, Kinesis,
  Bedrock, Secrets Manager, KMS, STS, IAM, EventBridge (including per-entry
  `PutEvents` `EventBusName`, defaulting omitted entries to `default`), and Step
  Functions (`stateMachineArn`, `CreateStateMachine` `name`, and `executionArn`);
  otherwise `*`. Account and region are `*` (empty where AWS's ARN has no such
  field: S3, IAM, API Gateway, Bedrock foundation models), while `/` paths in
  secrets, parameters and IAM roles are preserved. Some calls need several
  resources:
  - Lambda: a qualified call (`fn:prod`, `123456789012:function:fn:7`, a full
    qualified ARN, or `?Qualifier=prod`) needs the qualified ARN
    `function:fn:prod`; a grant on `function:fn` does not cover it.
  - KMS: a key named by alias (`alias/app` or an alias ARN) is authorized on
    the key, which the request does not name: `arn:aws:kms:*:*:key/*`. `CreateAlias` / `DeleteAlias` / `UpdateAlias`
    need the alias and the key(s).
  - Bedrock: invoking through an inference profile (`us.`, `eu.`, `apac.`,
    `global.` ids or profile ARNs) needs the action on the profile and on the
    foundation model it routes to, in any region
    (`arn:aws:bedrock:*::foundation-model/…`; for an application profile,
    `foundation-model/*`).
  - Glue Data Catalog: the catalog, the database and the table
    (`table/{db}/{name}`, or `table/{db}/*` for `GetTables`).
  - EventBridge rules are `rule/[{bus}/]{name}`; Scheduler schedules
    `schedule/{group}/{name}` (group `default` unless named); CloudWatch Logs
    reads `logGroupName` or `logGroupIdentifier` (a name or an ARN).
- **Normalization**: UUIDs, runs of six or more digits, and hex segments of six
  or more characters become `*`, so generated test names do not churn the lock.
- **Unreadable bodies**: when the body that names the resource cannot be read
  (binary, an encoding other than gzip, or longer than 64 MiB), the
  action stands on resource `*` with `unverified: true`. That line is stable
  and can be accepted into the lock; a policy grants it only with
  `Resource: "*"`, and any `Deny` on the action applies to it.
- **Implied permissions** are derived only from what is on the wire: SSE-KMS
  headers of an S3 request or response; `KmsKeyId` of Secrets Manager
  `CreateSecret`/`UpdateSecret`; `KeyId` of SSM `PutParameter`
  (`kms:Encrypt`, or `kms:GenerateDataKey` for the Advanced tier); the
  `KmsMasterKeyId` attribute of SQS/SNS queue and topic creation or update
  (`kms:GenerateDataKey`, what producers will need); DynamoDB
  `SSESpecification.KMSMasterKeyId`; Kinesis `StartStreamEncryption`. The
  key of an existing encrypted queue, topic or secret is not in the request
  that uses it and is not derived. Roles passed to Lambda, Step Functions,
  ECS, EventBridge (`PutRule`, `PutTargets` `Targets[].RoleArn`), Pipes,
  Scheduler (`Target.RoleArn`), Firehose (every `RoleARN`) and Glue
  (`Role`, `JobUpdate.Role`) need `iam:PassRole`.
- A call whose operation is only an HTTP method gets no action. Lenslet does
  not guess, and reports unmapped call counts in `unmapped`.

## `end`

The report: the same object `lenslet run --json PATH` writes.
`lenslet run --markdown PATH` renders it for PR comments and job summaries.

| field | meaning |
| --- | --- |
| `version` | `4` |
| `run`, `command`, `exit_code`, `ended`, `duration_ms` | the run; `exit_code` is the test command's (`128 + N` when signal N ended it, e.g. `143` after `SIGTERM`) |
| `calls`, `errors` | recorded calls, and those with a `problem` |
| `services` | `{service: calls}` |
| `unmapped` | `{service: calls}` with no recognizable operation (not in `access`) |
| `access` | `[{action, resource, calls, errors, implied?, via?, unverified?, new?, unused?}]`: every permission the run used, sorted, then the lock lines it did not use (`unused: true`, `calls: 0`). `new`: used, not in the lock. `via` names the first three calls that needed it, e.g. `DYNAMODB DeleteItem` |
| `findings` | what is worth a look, failures first, each kind in call order; see below |
| `regressions` | `{previous_run, previous_exit_code, new_failures, fixed}` compared with the previous finished run of the same command in the same directory (failure kinds `{service, operation, code}`), or `null` |
| `lock` | the lock file compared with, or `null` |
| `emulator` | what answered the calls, as in the `run` header |
| `trace` | the run file |
| `log` | `<run>.log`, the test command's output |

### Findings

Each finding has `kind`, `title`, `detail?` and `calls` (the ids of the calls
it is about, in call order).

| kind | one finding per | more fields |
| --- | --- | --- |
| `failure` | distinct (service, operation, error code) that failed, with every call that failed that way. A failed SDK attempt that a later attempt of the same invocation recovered from is not a failure | `service`, `operation`, `status` and `message?` (of the latest such call), `code`, `hint?` (the first likely cause among them) |
| `access` | permission not in the lock | `action`, `resource`, `implied?`, `unverified?` |

## `lenslet check`

`lenslet check --policy PATH [RUN] [--json] [--markdown PATH]` evaluates the
`concrete` access of every call of a finished run against an IAM identity
policy. `RUN` is a run file path, a run id or a unique prefix of one, or
`latest`; the default is the newest finished run whose `cwd` is the current
directory (an error when there is none).

The policy supports `Allow` and `Deny`, `Action` / `NotAction`, `Resource` /
`NotResource` and IAM wildcards, and fails closed: an `Allow` must cover
every resource a partly unknown ARN (`*`) could stand for, a `Deny` applies
when it may match one. The account is never known locally, so any account
in the policy matches; an `Allow` pinned to a region covers that region
only, not an unknown (`*`) one. A statement with a `Condition` (or a
`${...}` policy variable) is **not evaluated**: it never allows anything; a
denial it may decide is reported with `conditional: true` and the statement.
A statement that restricts `Principal` is refused.

`--json` prints `{run, policy, calls, denied: [{call, service, operation,
action, resource, implied?, unverified?, allowed, reason, by?,
conditional?}]}`; `reason` is `explicit_deny` (with `by`, the statement's
`Sid` or `Statement[i]`) or `no_allow`. Exit 3 when anything is denied, 2 on
errors, else 0.

## `lenslet policy`

`lenslet policy [--lock PATH | --run RUN]` prints an IAM policy JSON document
for the lock (without `# test` lines) or a finished run's access: one
statement per resource, actions sorted. ARN fields AWS leaves empty (S3,
Route 53, IAM, API Gateway, Bedrock foundation models) stay empty.

## `lenslet doctor --json`

```json
{
  "version": "13.0.0",
  "cli": "/usr/local/bin/lenslet",
  "emulators": [{"name": "localstack", "url": "http://127.0.0.1:4566"}],
  "managed": {"name": "moto", "available": true, "command": ["moto_server"]},
  "test_command": "pytest",
  "lock": "/work/app/lenslet.lock",
  "ci_workflow": true
}
```

`emulators` are the ones running now, recognized by their protocol (a
loopback `AWS_ENDPOINT_URL`, AWS Emulator on 9298, LocalStack on 4566, moto
on 5000). `managed` is the moto Lenslet starts when none is running
(`command: null` when it is not installed). `test_command` is the project's
test command as `lenslet init` detects it, or `null`. `lock` is
`./lenslet.lock` when it exists, else `null`.

## The emulator log

When Lenslet starts moto for a run, moto's own output goes to
`~/.lenslet/runs/<run>.emulator.log` (owner-only), not the terminal. moto
runs in its own process group on a free loopback port and is stopped
(`SIGTERM`, then `SIGKILL` after 2 s) when the run ends, also on Ctrl-C,
`SIGTERM` or an error.

## `lenslet.lock`

```text
# AWS access observed by `lenslet run`: action, resource.
dynamodb:CreateTable  arn:aws:dynamodb:*:*:table/orders  # test
dynamodb:GetItem      arn:aws:dynamodb:*:*:table/orders
kms:Decrypt           arn:aws:kms:*:*:key/*
```

The file has one `action  resource` pair per line, sorted. Text after `#` is
a comment. A comment containing the word `test` marks a line that only the
tests need: `--check` still counts it, but `lenslet policy` leaves it out. A
lock grants exactly its lines: a `*` in a line (a generated name) is not a
pattern over other names. `lenslet run --update` writes the file, only when
the command succeeded; the window can add lines, remove them and toggle
`test` marks. Every write is atomic (a temporary file renamed over the lock,
permissions kept) and canonical: it keeps the `test` marks and the full-line
comments at the top of the file; other comments are not kept. `--lock PATH`
uses another file.

## Retention

Lenslet keeps the newest 20 finished runs on the machine (with their logs);
a run that is still going is never deleted.
