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
  functions' code are all there: a function that failed is a failed call,
  with its error and what it printed, whether you invoked it or a
  notification did. On AWS this takes CloudTrail, X-Ray and CloudWatch
  together.
- **Your code or the emulator?** When a call fails, Lenslet says whether
  AWS Emulator is known to differ from AWS there, from the emulator's own
  list of what it does not do yet, with a link to the issue.
- **Fix it, then go back and run it again.** A function denied by its role?
  One click allows the role what it needed (and when a Deny, a boundary or
  the resource's own policy is in the way, Lenslet says which). Rewind puts
  the account back as it was before any run, in seconds. Run Again repeats
  the command, and the run says what changed since it last ran.
- **Keep the account.** Save the account to a file and open it again later,
  or on a teammate's machine: Lenslet builds it again from the calls that
  made it.
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
AWS_ACCESS_KEY_ID=lenslet-terminal
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
marks around each command, after your own startup files. A run holds only
the terminal's calls, told by the access key they are signed with (and the
keys AWS hands the terminal's programs, as an assumed role's). Calls from
other shells and programs appear under All Activity.

Every command is in the menu bar (macOS) or the main menu (☰), with its
shortcut, and on the context menu of what it acts on: Find (⌘F), Clear Calls
(⌘K), New Empty Account (⇧⌘N), Open Account (⌘O), Save Account (⌘S), Copy as
AWS CLI (⇧⌘C), Export Session (⌘E), Save IAM Policy (⇧⌘S). On Linux and
Windows the shortcuts that the terminal's shell needs take Ctrl+Shift
instead (Find is Ctrl+Shift+F, Save Account Ctrl+Shift+S), so Ctrl+R, Ctrl+K
and the rest still reach your shell.

## Services

S3, DynamoDB, SQS, SNS, STS, IAM, KMS, Lambda, Secrets Manager, SSM
Parameter Store, EventBridge, CloudWatch Logs, Step Functions and Kinesis.

The account is `000000000000` in `us-east-1`. It lives in memory: quitting
Lenslet or starting a new empty account throws it away, unless you save it
(File → Save Account). Request signatures are not checked, so any
credentials work.

## Install

Download the build for your system from the
[latest release](https://github.com/hxddh/lenslet/releases/latest): a DMG
for macOS, a tar.gz for Linux (GTK 4.14 or newer).

## Build

```bash
git clone --recurse-submodules --shallow-submodules https://github.com/hxddh/lenslet
cd lenslet
```

Apply Lenslet's patches to the emulator before the first build:

```bash
git -C third_party/aws-emulator apply ../../patches/aws-emulator-observer.patch
# macOS only: its Lambda service reads per-thread memory usage, which only Linux reports
git -C third_party/aws-emulator apply ../../patches/aws-emulator-macos.patch
```

Then build and run it:

```bash
cargo run --release
```

The observer patch lets Lenslet see the calls AWS makes inside the emulator
(a notification invoking a function, the function's own calls). On macOS a
function's memory is not measured, so its memory size is not enforced; its
timeout is.

Format Lenslet's own crates with `cargo fmt -p lenslet -p lenslet-core`:
`cargo fmt --all` reaches the emulator's crates too, and rewrites them.

The tests that drive a whole app with boto3 (`chain`, `rewind`) need a
Python with boto3:

```bash
LENSLET_PYTHON=python3 cargo test -p lenslet --test chain --test rewind -- --ignored
```

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
carries onto everything a call causes. A function's run is read from its
log group by the invocation's request id. Rewind sends the calls that
changed the account again, in order, to a new engine of its own, each once
what the one before set off is over, with the ids the emulator made up the
first time swapped for the new ones; a saved account is those calls in a
file, and opening it sends them the same way. The build indexes
AWS Emulator's `issues/things-that-do-not-work/` by service and operation
for the known differences a failed call shows. After a call
that changes something, Lenslet asks the emulator for its resources with the
services' own List operations, and opens a resource with their Describe and Get
operations. A new empty account swaps in a new, empty engine. The terminal
is a pseudo-terminal running your shell with the variables above.

## License

Apache-2.0. AWS Emulator is CC0-1.0.
