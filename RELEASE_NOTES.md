# Lenslet 12.0.0

Lenslet 12.0 is a release about trust. Lenslet's job is to say, from what it
observed, what AWS access your code needs and why its calls failed. In 12.0,
every line it prints is a true observed fact again. It is also smaller: it
has fewer surfaces, fewer formats and one place for each action.

```bash
curl -fsSL https://github.com/hxddh/lenslet/releases/latest/download/install.sh | sh
pip install 'moto[server]'      # only if you don't run an emulator already
lenslet pytest
```

## A real emulator, always

The built-in sandbox from 11.0 is gone. It answered every operation it did
not implement with an empty `200`, ignored DynamoDB `ConditionExpression`
and most of `UpdateExpression`, never paginated S3 listings, and did no KMS
cryptography. It was also chosen silently, so a PR could report "0 errors"
for calls that would fail anywhere real.

Instead:

- **Lenslet uses the emulator you run.** That is a loopback
  `AWS_ENDPOINT_URL`, AWS Emulator on 9298, LocalStack on 4566 or moto on
  5000. Each is recognized by its protocol, not by an open port; port 5000 is
  AirPlay on every Mac.
- **Or it starts moto for you.** When nothing is running and moto is
  installed, Lenslet starts moto on a free loopback port for the run and stops
  it afterwards, including on Ctrl-C or an error. moto's output goes to
  `~/.lenslet/runs/<run>.emulator.log`.
  - It prints one line saying so, even with `--quiet`.
  - When moto isn't installed either, it exits 2 with the install command.
  - Use `--emulator moto` to always run against a fresh moto.
- **Every report says what answered.** The terminal, the PR comment and the
  JSON report (`emulator`) end their first line with
  `against moto (started by Lenslet)`, `against localstack`, and so on.
- **CI**: the action takes `upstream: moto`. It installs moto, and Lenslet
  starts and stops it.
- **Deprecated aliases**: `--sandbox` and `upstream: sandbox` still work and
  mean moto, with a notice.

## IAM you can rely on

- **A golden corpus.** 301 SDK calls across 19 services, recorded through
  moto (with Bedrock and execute-api calls hand-built). Each has its expected
  permissions written by hand from the AWS Service Authorization Reference. A
  test checks every mapping against it, in both lock and policy mode.
- **Fixed mappings**:
  - **Lambda**: qualified functions (`my-fn:prod`, version ARNs,
    `?Qualifier=`) used to collapse to `function:prod`. They are now
    `function:my-fn:prod`.
  - **KMS**: a key named by alias is authorized on the key (`key/*`), not the
    alias. `CreateAlias`/`DeleteAlias` need both the alias and the key, and
    `ReEncrypt` is `ReEncryptFrom` + `ReEncryptTo`.
  - **API Gateway** (v1 and v2): `apigateway:GET|POST|PUT|PATCH|DELETE` on
    `arn:aws:apigateway:*::/…`, where the account is empty. Calling a deployed
    API is `execute-api:Invoke`. The non-existent `apigateway:GetRestApi` and
    `apigatewayv2:` actions are gone.
  - **Bedrock**: an inference profile also needs its foundation model.
    Foundation-model ARNs have no account.
  - **Secrets Manager**: `BatchGetSecretValue` also needs `GetSecretValue`
    on each secret.
  - **Glue** tables need catalog, database and table.
  - **CloudWatch Logs** reads `logGroupIdentifier`.
  - **STS**: `AssumeRoleWithWebIdentity` and `AssumeRoleWithSAML` need no
    permission from the caller.
  - **Tagging** calls resolve to their real resource.
- **Policy evaluation**:
  - A statement with both `Action` and `NotAction` (or `Resource` and
    `NotResource`) is rejected, as AWS rejects it.
  - `ArnEquals` accepts wildcards, as AWS does.
  - A miscased operator gets a "did you mean" error.

## Policy output: two formats, both valid

`lenslet policy --format iam` (the default) or `--format terraform` (an
`aws_iam_policy_document` data source, escaped correctly).

- **Formats removed:** CloudFormation, SAM and CDK. The CDK output didn't
  compile, none of them escaped strings, and every IaC tool accepts IAM JSON.
- **Same input, same policy:** `--lock` and `--run` now produce the same
  statements for the same access. For that reason, generated policies no
  longer include `iam:PassedToService` / `kms:ViaService` conditions. Add
  them by hand if you want them.

## The window, simplified

- **Two views again**, Calls and Access. The test output is a drawer in
  Calls (`⌘J`).
- **Findings first** in Calls again.
  - The "+N not in lock" banner now leads to a review in Access.
  - Accepting all new permissions asks first and lists them; it is no longer
    a one-click button in Calls.
- **One place for each action.** Run is the header's ▶. There is one
  "Block calls outside the lock" switch, in the run options, and it says when
  it applies to the next run.
- **Fewer controls everywhere.** Header controls went from 13 to 8, commands
  from 49 to 30 and shortcuts from 34 to 29. The sidebar is closed by
  default and holds only the project and its runs. Settings no longer shows
  command-line flags.
- **No default test command.** Without a detected command the button reads
  "Set test command…" instead of guessing `pytest`.
- **Composing requests from scratch is gone.** Edit & Replay of a recorded
  call stays.
- **Keyboard.** `Esc` closes every dialog, even from a text field, and `Tab`
  moves between fields.
- **Safe processes.**
  - The window stops the run it started, not whichever run is on screen.
  - Starting twice is refused. A proxy counts as started only when its health
    reply carries the spawned process's pid.
  - Quitting sends a stop signal and returns at once instead of blocking.
  - Children die with the window on Linux. After a crash, the next launch
    cleans up what it had started.
  - The CI workflow is never overwritten without asking.
  - On Linux, Ctrl+W closes the window instead of quitting.
- **Emulator status** comes from `lenslet doctor --json`, so the window and
  the CLI agree. The window no longer probes ports itself.
- **SigV4 re-signing** encodes non-S3 paths twice, as AWS requires. It is
  tested against the AWS SigV4 test-suite vectors.
- **Off the main thread**: the test output is highlighted in the background,
  only while the drawer is open. Error messages and toasts are translated.

## Security

- **Live events are loopback-only.** `/_lenslet/*` (live events, health,
  shutdown) answers only loopback. In 11.0, `--allow-docker` exposed the live
  trace, with captured secrets, to anyone on the LAN. It now serves only
  Docker networks.
- **The local-only check works.** The upstream and `--route` hosts are
  resolved and must be loopback or private. AWS domains are refused, trailing
  dot included.
- **Environment.** The service-specific `AWS_ENDPOINT_URL_*` variables, which
  bypass the proxy, are removed from the tests' environment. `lenslet env`
  uses the same list as `lenslet run`. `lenslet shell` warns when your rc
  files set AWS credentials.
- **`install.sh --app`** no longer strips extended attributes or deletes an
  existing app. It moves the old app aside and prints the command that clears
  the quarantine flag.
- **Truncation is marked.** A decompressed body that hit the capture limit is
  marked `truncated`.

## Corrections to earlier notes

- **No Windows support.** 10.0 said "Linux & Windows" conformance. There is
  no Windows build and no Windows support.
- **The sandbox was not "protocol-accurate"** (11.0).
- **Generated policies were not always valid.** 9.0's "least-privilege …
  compiles to CDK" and its API Gateway mappings were wrong, as described above.

## Upgrading

- **If you used the sandbox**, run `pip install 'moto[server]'` locally, and
  use `upstream: moto` in the action (`sandbox` keeps working, with a
  notice). moto implements far more of AWS than the sandbox did, so tests
  that passed against the sandbox can now fail for real reasons. That is the
  point.
- **Locks with Lambda, KMS, API Gateway, Bedrock, Secrets Manager or Glue
  lines may change.** Run `lenslet run --update -- <tests>` once, review the
  diff, and commit.
- **`lenslet policy --format cfn|sam|cdk`** now exits 2. Use `--format iam`
  or `--format terraform`.
- **`lenslet init`** no longer overwrites an existing workflow without
  `--force`.
