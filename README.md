# Lenslet

**A local AWS account in a window.**

Lenslet is a desktop app with AWS inside it. Open it and you have an empty
AWS account on `http://127.0.0.1:4510`: S3, DynamoDB, SQS, SNS, Lambda, IAM
and eight more services, emulated in-process by
[AWS Emulator](https://github.com/aws/aws-emulator), and a terminal already
pointed at it. Run your code there, and Lenslet shows what happened, including
inside AWS:

- **Every command is a run.** Each command you run in the terminal (`pytest`,
  `aws s3 cp …`, your script) becomes a run: how it ended, every call it
  made, and the first thing that failed and why. It also shows what the run
  changed and the IAM policy it needed.
- **What AWS did because of it.** The calls are a tree. Under the upload sits
  the function its notification invoked, and under the function its own calls
  to DynamoDB, and why one was denied. Deliveries, triggers and your
  functions' code are all there. On AWS this takes CloudTrail, X-Ray and
  CloudWatch together.
- **Fix it, then go back and run it again.** A function denied by its role?
  One click allows the role what it needed. Rewind puts the account back as
  it was before any run. Run Again repeats the command.
- **Every resource, opened.** Buckets, tables, queues, topics, functions, log
  groups, secrets and parameters, with their counts as they change. Open one
  to see what it holds and every call that touched it.
- **Every call, in full.** Request and response, headers and bodies, the
  client and its retry attempt, the request ID; copy any call as an AWS CLI
  command, curl or boto3.

![Lenslet](docs/lenslet.png)

## Use it

Open Lenslet and run your code in its terminal (`` Ctrl+` `` shows and hides
it). The terminal is your own shell, started in the folder you opened
Lenslet from, with these already set:

```bash
AWS_ENDPOINT_URL=http://127.0.0.1:4510
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=lenslet
AWS_SECRET_ACCESS_KEY=lenslet
```

```bash
aws sqs create-queue --queue-name orders
pytest                # or any program that uses an AWS SDK
```

From any other shell, copy the same variables with **File → Copy
Environment** (or click the endpoint in the sidebar).

Every SDK that honors `AWS_ENDPOINT_URL` (boto3, the AWS SDKs for
JavaScript, Go, Rust, Java and .NET, and the AWS CLI v2) talks to Lenslet
with no code changes. The AWS CLI v1 sends SQS commands to AWS whatever the
endpoint says; the terminal's placeholder keys keep those away from your
account. If port 4510 is taken, Lenslet takes a free port and shows it; set
`LENSLET_PORT` to choose one.

Runs come from the terminal's shell integration (bash and zsh): Lenslet adds
marks around each command, after your own startup files. Calls from other
shells and programs appear under All Activity.

Every command is in the menu bar (macOS) or the main menu (☰), with its
shortcut, and on the context menu of what it acts on: Find (⌘F / Ctrl+F),
Clear Calls (⌘K), New Empty Account (⇧⌘N), Copy as AWS CLI (⇧⌘C), Export
Session (⌘E), Save IAM Policy (⇧⌘S).

## Services

S3, DynamoDB, SQS, SNS, STS, IAM, KMS, Lambda, Secrets Manager, SSM
Parameter Store, EventBridge, CloudWatch Logs, Step Functions and Kinesis.

The account is `000000000000` in `us-east-1`. It lives in memory: quitting
Lenslet or starting a new empty account throws it away. Request signatures are
not checked, so any credentials work.

## Install

Download the build for your system from the
[latest release](https://github.com/hxddh/lenslet/releases/latest): a DMG
for macOS, a tar.gz for Linux (GTK 4.14 or newer).

## Build

```bash
git clone --recurse-submodules --shallow-submodules https://github.com/hxddh/lenslet
cd lenslet
cargo run --release
```

Then apply Lenslet's patches to the emulator:

```bash
git -C third_party/aws-emulator apply ../../patches/aws-emulator-observer.patch
# macOS only: its Lambda service reads per-thread memory usage, which only Linux reports
git -C third_party/aws-emulator apply ../../patches/aws-emulator-macos.patch
```

The observer patch lets Lenslet see the calls AWS makes inside the emulator
(a notification invoking a function, the function's own calls).

The window uses [Native Shell](https://github.com/longbridge/nsh): AppKit on
macOS, GTK 4 on Linux. On Linux, install `libgtk-4-dev` first. The first
build compiles the emulator and takes a while.

## How it works

`crates/app` embeds AWS Emulator's services behind a small HTTP server.
Each request goes to the emulator unchanged and comes back unchanged; on the
way, `crates/core` reads it: which service and operation it is, which IAM
permissions it needs on which resources, and why it failed. The observer
patch reports the calls AWS makes inside the emulator too; Lenslet hangs each
under its cause, on the same task, or through the X-Ray tracing header AWS
carries onto everything a call causes. Rewind sends the calls that changed
the account again, in order, to a new engine. After a call
that changes something, Lenslet asks the emulator for its resources with the
services' own List operations, and opens a resource with their Describe and Get
operations. A new empty account swaps in a new, empty engine. The terminal
is a pseudo-terminal running your shell with the variables above.

## License

Apache-2.0. AWS Emulator is CC0-1.0.
