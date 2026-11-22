# Lenslet

**See, in every pull request, what AWS access your code needs, and why its
AWS calls failed.**

Lenslet runs your tests against a local AWS emulator: the one you already run
(AWS Emulator, LocalStack, moto), or moto, which Lenslet starts and stops for
you when none is running. It records every AWS call, explains failures, and
works out the IAM permissions the code uses. It keeps those permissions in `lenslet.lock`, a file you commit. When a
pull request widens AWS access, the PR says so, call by call, and CI fails
until someone accepts the change.

AWS Emulator does not evaluate IAM, so code that works locally can still be
denied on AWS. Lenslet covers that gap in code review. With `--enforce` it
also blocks unauthorized calls live at the proxy with real AWS `AccessDenied`
responses, and `lenslet check --policy role.json` tells you, without
re-running anything, whether a recorded run would pass under a real IAM
policy and which statement would deny it.

> **Lenslet · `npm test` passed**
>
> 15 AWS calls · 0 errors · 4 services · against moto
>
> **1 permission not in `lenslet.lock`.** Review it; if it is intended, run `lenslet run --update` and commit `lenslet.lock`.
>
> | | Permission | Resource | Needed by |
> |---|---|---|---|
> | + | `dynamodb:DeleteItem` | `arn:aws:dynamodb:*:*:table/orders` | DYNAMODB DeleteItem |

## Start

```bash
# CLI only (Linux & macOS):
curl -fsSL https://github.com/hxddh/lenslet/releases/latest/download/install.sh | sh

# Or the desktop app and the CLI together:
curl -fsSL https://github.com/hxddh/lenslet/releases/latest/download/install.sh | sh -s -- --app

# No emulator running? Lenslet starts moto for each run:
pip install 'moto[server]'

lenslet pytest          # or: lenslet npm test / lenslet cargo test
lenslet init            # detects your tests, writes lenslet.lock + a GitHub Actions workflow
```

Lenslet uses the emulator that is already running: a loopback
`AWS_ENDPOINT_URL`, AWS Emulator on 9298, LocalStack on 4566 or moto on 5000,
each recognized by its protocol, not just an open port. When none is running
and moto is installed, it starts moto on a free port for the run and stops it
after; when moto is not installed either, it stops with the one-line install
command. Every report says what answered: `· against moto (started by
Lenslet)`.

`lenslet init` detects your test command (`pytest`, `npm`/`pnpm`/`yarn`/`bun`,
`cargo test`, `go test ./...`, `make test`), writes `lenslet.lock` and adds
`.github/workflows/lenslet.yml` (an existing workflow is kept unless you pass
`--force`). Review the lock, mark fixture-only lines with `# test`, and commit
both files.

## In CI

The workflow `lenslet init` writes uses the Lenslet action:

```yaml
permissions:
  contents: read
  pull-requests: write          # one PR comment, updated in place
steps:
  - uses: actions/checkout@v5
  # install your test dependencies
  - uses: hxddh/lenslet@lenslet-v12.0.0
    with:
      command: pytest
      upstream: moto            # the action installs moto; Lenslet starts and stops it
```

The action does five things:

- It installs the pinned Lenslet and checks its checksum.
- It starts the emulator if you give it one, and waits until it answers.
- It runs your command with `--check`.
- It writes the report to the job summary.
- It keeps one PR comment up to date.

Until a `lenslet.lock` exists, it only reports.

| input | default | |
| --- | --- | --- |
| `command` | — | the test command (bash, in `working-directory`) |
| `upstream` | `auto` | the emulator: `auto` (one already running in the job, else moto if installed), `moto` (installed and managed for you), a port (HTTPS on 127.0.0.1) or a URL |
| `emulator` | | `moto`, or a command that starts the emulator in the background |
| `working-directory` | `.` | where to run and find the lock |
| `lock` | `lenslet.lock` | lock path, relative to `working-directory` |
| `policy` | | IAM policy JSON path to check/enforce instead of `lenslet.lock` (supports `Allow`, `Deny`, `NotAction`, `NotResource`, and wire-level IAM `Condition` blocks) |
| `check` | `true` | fail when access grows (exit 3) |
| `strict` | `false` | also fail when the lock lists access the run did not use |
| `enforce` | `false` | actively deny calls outside the lock/policy with AWS `AccessDenied` |
| `comment` | `true` | post or update the PR comment (fork PRs get the job summary only) |
| `version` / `binary` | from the pinned tag | another release, or a binary you built |

Outputs: `exit-code`, and `report`, the path to the JSON report.

For AWS Emulator in CI, install it the way its docs describe (on Linux, a
`.deb` or `.rpm`), then set `emulator: aws-emulator --telemetry off --port 9298`.
LocalStack works as an upstream too; `upstream: moto` is the shortest path. Lenslet's own CI runs the action this way
against [examples/python](examples/python).

## What a run shows

```console
$ lenslet run --check -- npm test
...your tests...
lenslet: 24 calls · 2 errors · 4 services (dynamodb, lambda, s3, sqs) · 476 ms · against moto
  ✗ DYNAMODB GetItem  400  ResourceNotFoundException  Requested resource not found
      → Region mismatch: `table/orders` was created in `us-west-2` at call #2 (CreateTable), but this `GetItem` targeted `us-east-1`.
  ! DYNAMODB GetItem ×8 in a serial burst
      → 8 calls on table/orders, one after another in 31 ms: `BatchGetItem` does this in fewer round trips.
  · 2 resources created and not deleted: s3://shop-receipts, SQS fulfilment
  access: 1 permission not in lenslet.lock
    + dynamodb:DeleteItem              arn:aws:dynamodb:*:*:table/orders
  unused: 1 permission in lenslet.lock not exercised by this run
    - dynamodb:GetItem                 arn:aws:dynamodb:*:*:table/orders
```

- **Findings first**: one ranked list of what is worth a look. Failures come
  with their cause, including cross-call correlation (a region mismatch against
  an earlier `CreateTable`/`CreateQueue`, a resource deleted earlier in the
  run, common local setup gaps). Serial bursts that a batch API would collapse
  (N+1), SDK retries, and resources the run left behind are also listed.
  The same list leads the terminal, the PR comment, the JSON report and the
  window.
- **What it needs**: each call becomes an AWS IAM action on a resource ARN,
  with account and region as `*`. Every mapping is checked against a corpus of
  300 recorded SDK calls across 19 services whose expected permissions were
  written by hand from the AWS Service Authorization Reference
  (`crates/core/tests/corpus`). The mapping covers:
  - batches: one entry per table or SSM parameter;
  - transactions: each item's own action;
  - copies & subresources: source reads, `PostObject` → `s3:PutObject`, and
    exact S3 subresource IAM action names (`s3:GetBucketPublicAccessBlock`,
    `s3:GetBucketNotification`, `s3:GetReplicationConfiguration`, …);
  - implied permissions, where the key or role is on the wire: KMS for S3
    SSE-KMS (request or response headers), Secrets Manager and SSM writes that
    name a key, SQS/SNS queues and topics created with a key, DynamoDB
    `SSESpecification` and Kinesis stream encryption; `iam:PassRole` for
    Lambda, Step Functions, ECS (`taskRoleArn` and `executionRoleArn`),
    EventBridge (rules and `Targets[].RoleArn`), Scheduler, Pipes, Firehose
    and Glue. The key of an existing encrypted queue, topic or secret is not
    in the request that uses it, so it is not derived.
  - qualified Lambda functions (`function:fn:prod`), KMS aliases (authorized
    on the key, so `key/*`), Bedrock inference profiles (the profile and its
    foundation model), API Gateway management calls (`apigateway:GET` … on
    `arn:aws:apigateway:*::/restapis/*`) and Secrets Manager
    `BatchGetSecretValue` (plus `GetSecretValue` on each secret);
  - a request whose body is not readable (binary such as CBOR, or over the
    inspection limit) gets its action on `*`, marked unverified, rather than a
    guessed resource.

  Generated names (UUIDs, timestamps) become `*`, so the lock stays stable.
- **What changed**: compared with `lenslet.lock` (added, unused) and with the
  previous run of the same command (new failures, fixed).

This is **observed** access: what your tests exercised, mapped by rules. It
is not a security review. Code paths the tests never reach are not in the lock.

### The lock

```text
# lenslet.lock
dynamodb:CreateTable  arn:aws:dynamodb:*:*:table/orders  # test
dynamodb:PutItem      arn:aws:dynamodb:*:*:table/orders
kms:Decrypt           arn:aws:kms:*:*:key/*  # implied
s3:GetObject          arn:aws:s3:::shop-receipts/*
```

The lock has one permission per line, sorted. `# test` marks access that only
the tests need (fixtures creating tables or buckets). `--check` still counts
those lines, but `lenslet policy` leaves them out. A lock grants exactly its
lines: a `*` that stands for a generated name is not a pattern over other
names. `lenslet run --update` rewrites the lock, only when the command passed;
the window can accept new lines and toggle marks. Every write is atomic and
keeps the `test` marks and the comment block at the top of the file.

### Checking a real policy

```console
$ lenslet check --policy deploy/role.json
lenslet check: run 20261119-114534-30560 against deploy/role.json · 24 calls · 1 permission denied
  ✗ dynamodb:DeleteItem  arn:aws:dynamodb:*:*:table/orders  (DYNAMODB DeleteItem)
      → denied by statement `ReadOnlyOrders`
```

`lenslet check` re-evaluates a recorded run (the newest by default) against an
IAM policy or a lock, without re-running your tests. It names the statement
that denies each permission, or says that nothing allows it. Exit code 3
means something was denied.

## The window

`lenslet ui` (or `Lenslet.app`) opens the window. It follows the newest run
live: while the run is going it subscribes to the proxy's event stream, so a
call shows up as it happens, and it reads the run file when the run is over.
Everything it shows comes from the same core as the CLI, so the window, the
terminal and the PR comment always agree.

![A run in the Lenslet window](docs/lenslet.png)

One header, two views, one inspector:

- **Header**: the run and what answered it (`against moto`; switch runs with
  `⌘[` / `⌘]`), **▶ your test command** (`⌘R`; `▾` holds the run options:
  the command, *Block calls outside the lock*, *Accept calls from Docker
  containers*), **Calls | Access**, **Compared with ▾** (`lenslet.lock`, the
  previous run, or any recorded run) and search.
- **Calls** (`⌘1`): the run's **Findings** first (click one to jump to its
  calls), then filter chips, the run's timeline and the calls. `⌘J` opens the
  test command's output in a drawer below.
- **Access** (`⌘2`): every permission the run used, marked not in the lock,
  unused, implied or `# test`. New permissions are reviewed here: *Accept into
  lock* one at a time, or *Accept all…* after a confirmation that lists them.
  **Check policy…** simulates an IAM policy; the policy for the lock exports
  as IAM JSON or Terraform.
- **Inspector** (`⌘B`): why a call failed, its SDK retries, the SigV4 scope,
  and the bodies. **Replay** (`⌘⏎`) re-sends the call through the live proxy;
  **Edit & Replay** (`⇧⌘⏎`) changes it first and re-signs it locally.
- **Sidebar** (`⌘0`, closed by default): the project and its runs.

![The Access view](docs/access.png)

Right-click a call or a permission for its actions (copy as curl, copy the
ARN or statement, replay, accept, mark `# test`). `⌘K` opens the command
palette, where every command is, and `⌘F` or `/` searches calls, headers and
bodies. `j`/`k` or the arrow keys move, `Enter` goes into the inspector,
`Esc` closes any dialog or backs out, and `Tab` moves between a dialog's
fields. On Linux every `⌘` shortcut is `Ctrl`. `⌘,` opens Settings:
appearance (System / Light / Dark), accent, density, English or 中文, the
command-line tool, and the space recorded runs take.

The window starts and stops the proxy and your tests itself. It never leaves
them running: quitting stops them without blocking, and after a crash the
next launch cleans up what it had started. The emulator status comes from
`lenslet doctor --json`, so the window and the CLI never disagree about it.

The window never reads the disk while drawing: runs load, search and decode
in the background, and a 5,000-call run is on screen in well under 150 ms.

## For agents

Every run ends with one JSON report, the same object in three places:

- the last line of the run file;
- `lenslet run --json PATH` (`-` for stdout);
- `lenslet last --json`, which reads the newest run without re-running it.

`--markdown PATH` writes the PR-comment version.

| exit code | meaning |
| --- | --- |
| the command's | the command failed |
| `3` | the command passed, but `--check` found access not in the lock (or, with `--strict`, unused); or `lenslet check` found a denied permission |
| `2` | Lenslet could not run |
| `0` | otherwise |

While a run is going, its proxy streams the run file live:
`GET <endpoint>/_lenslet/events` is a server-sent event stream of every line
(backlog first, then each new line, ending after the report), and
`GET <endpoint>/_lenslet/health` says which run it is. The window uses the
same stream.

The format is in [docs/trace.md](docs/trace.md).

## Commands

```text
lenslet COMMAND [ARGS...]                      shorthand: run COMMAND under Lenslet (`lenslet pytest`)
lenslet init [--emulator moto | --upstream URL] [--force] [-- COMMAND]
                                               onboarding: lock + CI workflow
lenslet shell [options]                        interactive shell pre-configured with AWS_ENDPOINT_URL
lenslet doctor [--json]                        check CLI, desktop app, emulators (running, and moto), lock, CI
lenslet run [options] [--] COMMAND             run under Lenslet; report at the end
lenslet run [options]                          no command: a proxy on 127.0.0.1:9297
lenslet env [--shell sh|fish|pwsh|dotenv|docker] [--listen PORT]
                                               print SDK environment snippet for a running proxy
lenslet last [--json]                          the newest run's report
lenslet runs [--json] [--limit N]              list recorded runs newest-first
lenslet diff [BASE] [TARGET] [--json] [--markdown PATH]
                                               compare two runs (default: previous vs newest)
lenslet check [--policy PATH | --lock PATH] [--context K=V] [RUN] [--json] [--markdown PATH]
                                               would a recorded run pass under this policy/lock?
lenslet policy [--lock PATH | --run RUN] [--format iam|terraform] [--role ROLE]
                                               an IAM policy (JSON or Terraform), without `# test` lines
lenslet ui                                     the window

run: --emulator moto  --enforce  --check  --strict  --update  --lock PATH  --policy PATH
     --max-body SIZE  --json PATH  --markdown PATH  --quiet  --ui  --no-env
     --listen PORT  --upstream ADDRESS  --route SERVICE=ADDRESS  --allow-docker
     --emulator-ca PATH  --trace PATH
```

Without `--upstream` or `--emulator`, `lenslet run` finds the emulator as
described in [Start](#start). For the child process, `lenslet run` sets:

- `AWS_ENDPOINT_URL`;
- the emulator's credentials and config, when you have not set your own;
- placeholder keys, when the emulator has no credentials file.

It always removes ambient cloud credentials (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_PROFILE`,
`AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN`, `AWS_CONTAINER_CREDENTIALS_*`,
`AWS_BEARER_TOKEN_BEDROCK`), and the service-specific `AWS_ENDPOINT_URL_*`
variables that would send calls around the proxy, from the environment it
hands over, so real
credentials never reach the emulator or the unredacted local trace. `--no-env`
sets only the endpoint. Both `127.0.0.1` and `localhost` work on IPv4 and IPv6
loopback.

## Install

| | |
| --- | --- |
| any Linux, macOS (CLI) | `curl -fsSL https://github.com/hxddh/lenslet/releases/latest/download/install.sh \| sh` |
| macOS (app + CLI) | `Lenslet-X.Y.Z.dmg`: universal (Apple silicon and Intel). The CLI is inside the app at `Lenslet.app/Contents/MacOS/lenslet`. |
| Linux (CLI + window) | `lenslet-X.Y.Z-{x86_64,aarch64}-linux.tar.gz` (glibc ≥ 2.35) |
| CI images, Alpine | `lenslet-cli-X.Y.Z-{x86_64,aarch64}-linux-musl.tar.gz`: static |

Every asset is listed in the release's `SHA256SUMS`, and `install.sh` checks
the file it downloads against it. With `--app` on macOS it installs
`Lenslet.app` (an existing one is moved aside, not deleted) and prints the
command that clears the quarantine flag; it does not clear it for you,
because the app is not notarized. To uninstall, delete the binary (or the app)
and `~/.lenslet`. That directory holds raw local traces, including request
headers.

## Files

| path | what |
| --- | --- |
| `lenslet.lock` | the access your project needs; commit it |
| `.github/workflows/lenslet.yml` | written by `lenslet init` |
| `~/.lenslet/runs/*.jsonl` | one file per run (header, calls, report); the last 20 finished runs are kept |
| `~/.lenslet/runs/*.emulator.log` | moto's output, when Lenslet started moto for the run |

Run files are a local development artifact. They hold raw headers and text
bodies, and are written owner-only (`0600`). Do not commit them.

## Build from source

You need Rust 1.88+. The window also needs the GPUI Linux libraries
(`libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libwayland-dev`,
`libx11-xcb-dev`, `libxcb1-dev`, `libvulkan-dev`, `libfontconfig1-dev`,
`libfreetype-dev`, `pkg-config`, `cmake`, `clang`) and a Vulkan driver.

```bash
cargo build --release -p lenslet          # CLI: no graphics libraries
cargo build --release -p lenslet-ui       # the window
cargo test --workspace
```

```text
crates/core    lenslet-core: the facts — run-file schema, access derivation, lock
               and policy gate, failure causes, findings, reports, run diffs
crates/cli     lenslet: the proxy, the run file and live events, commands
crates/ui      lenslet-ui: the GPUI window, on lenslet-core
action.yml     the GitHub Action
install.sh     the installer
examples/      a project the action is tested against
```

## Limits

- Local only. An upstream (or `--route`) must resolve to loopback or a private
  container network; AWS domains are refused. The proxy serves loopback only;
  `--allow-docker` also serves Docker networks (172.16.0.0/12,
  192.168.65.0/24). `/_lenslet/*` (live events, health, shutdown) is always
  loopback-only. Both HTTP/1.1 and HTTP/2 (`h2c` and upstream ALPN) are
  supported.
- What Lenslet reports about responses is what your emulator answered. moto,
  LocalStack and AWS Emulator each implement AWS to a different depth; the
  report names the one that answered.
- `--enforce` stops a call before it reaches the emulator, except when the
  denied permission only shows in the response (the SSE-KMS key of an S3
  bucket's default encryption): the emulator has then already run the call,
  and the trace marks it `blocked_phase: "response"`.
- Capture keeps the first 1 MiB of each body by default (`--max-body` or
  `--max-body unlimited` to change), unframing `aws-chunked` and
  `application/vnd.amazon.eventstream` streams, decompressing `gzip`, `deflate`
  and `zstd`, and decoding Smithy RPCv2 / AWS CBOR payloads into readable JSON
  while forwarding wire bytes untouched and unredacted.
- Resources are derived for S3, DynamoDB (tables,
  indexes, and Streams), SQS, SNS, Lambda, SSM Parameter Store, CloudWatch Logs,
  Kinesis, Bedrock, Secrets Manager, KMS, STS, IAM, EventBridge (including
  per-entry `PutEvents` bus names and Pipes), Step Functions, Cognito IDP, ECR,
  ECS, EKS, CloudFormation, Glue, Athena, RDS & RDS Data API, API Gateway v1/v2,
  CloudFront, Route 53, SES/SESv2, SageMaker, OpenSearch, Backup, Firehose, and
  Scheduler. Other services get `*`. Calls with unrecognized operations are
  reported as unmapped rather than guessed.
- `--policy` evaluates wire-observable IAM `Condition` keys (`aws:RequestedRegion`,
  `aws:RequestTag/*`, `aws:TagKeys`, `dynamodb:LeadingKeys`,
  `kms:EncryptionContext:*`, `kms:EncryptionContextKeys`, `iam:PassedToService`,
  `kms:ViaService`, `s3:prefix`, `s3:delimiter`, `s3:max-keys`, `s3:versionid`,
  `s3:x-amz-*`) across `String*`, `Arn*`, `Numeric*`, `Bool`, `Null`,
  `ForAnyValue:`, `ForAllValues:`, and `...IfExists`. Cloud-only runtime keys
  (such as `aws:PrincipalOrgID`) can be supplied with `lenslet check --context KEY=VAL`
  and otherwise report `missing_context` / fail closed.
- No Windows build. The macOS app is not notarized.

Earlier versions: [lenslet-v11.0.0](https://github.com/hxddh/lenslet/tree/lenslet-v11.0.0),
[lenslet-v8.0.0](https://github.com/hxddh/lenslet/tree/lenslet-v8.0.0),
[lenslet-v7.0.0](https://github.com/hxddh/lenslet/tree/lenslet-v7.0.0),
[lenslet-v6.0.0](https://github.com/hxddh/lenslet/tree/lenslet-v6.0.0),
[lenslet-v5.0.0](https://github.com/hxddh/lenslet/tree/lenslet-v5.0.0),
[lenslet-v4.1.0](https://github.com/hxddh/lenslet/tree/lenslet-v4.1.0).
This repository used to be `celagent`; that project is at
[v4.1.0](https://github.com/hxddh/lenslet/tree/v4.1.0).

## License

Apache-2.0. The bundled fonts are under the SIL Open Font License; see
`assets/fonts/`.
