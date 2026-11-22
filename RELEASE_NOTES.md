# Lenslet 14.0.0

Lenslet 14 is a new product: **a local AWS account in a window.**

Earlier versions recorded your tests' AWS calls against an emulator you ran
yourself and checked IAM access in pull requests. Lenslet 14 has the
emulator inside it. Open the app and you have an empty AWS account on
`http://127.0.0.1:4510`, emulated in-process by
[AWS Emulator](https://github.com/aws/aws-emulator). Point an SDK or the AWS
CLI at it and Lenslet shows what your code does.

## What you see

- **Calls, live.** Every call with its service, operation, status and time;
  the request and response bodies, pretty-printed and selectable.
- **Resources.** S3 buckets with object counts, DynamoDB tables with item
  counts, SQS queues with waiting messages, and SNS topics, Lambda
  functions, secrets, parameters, rules, state machines, streams, log
  groups, keys and roles. New ones are marked as they appear.
- **Failures, explained.** The first failing call is selected for you, with
  the error in plain words. Calls the emulator does not support are marked
  "not emulated", so a gap in the emulator does not look like a bug in your
  code.
- **The IAM policy.** Every permission the session used, as one
  least-privilege policy, one click to copy.
- **Fresh start.** A new, empty account at once.
- English and Chinese.

## Services

S3, DynamoDB, SQS, SNS, STS, IAM, KMS, Lambda, Secrets Manager, SSM,
EventBridge, CloudWatch Logs, Step Functions and Kinesis.

## Gone

The `lenslet` CLI, `lenslet.lock`, the GitHub Action, PR comments, run
files and `install.sh` are gone, along with support for LocalStack and moto
as upstreams. Lenslet 13 remains available for pull-request checks.

## Install

macOS: open the DMG and drag Lenslet to Applications.
Linux: unpack the tar.gz and run `bin/lenslet` (GTK 4.14 or newer).
