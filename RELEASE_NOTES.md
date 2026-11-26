# Lenslet 17.1.0

Fixes to what Lenslet reads from a call, to rebuilding the account, and to
the terminal; and the end-to-end tests now run in CI.

## Allow for role
- **An allowed role stays allowed.** What Allow for role adds is part of
  the account: it is saved with it, and it stands through Rewind to
  Before. When the role is created again (Run Again after rewinding past
  the run that created it), Lenslet allows it again, so the command
  passes.
- Allowing the same action on another resource adds to the role's policy
  instead of replacing it.
- A role's page lists its inline policies, with their documents, beside
  its managed ones.

## What a call says
- **Bodies are read under their wire encodings.** A streaming upload's
  `aws-chunked` frames (and its trailing checksum) and a `gzip` body are
  undone before Lenslet reads the call: the inspector, the target and the
  IAM resources show the payload, not frames or `*`.
- A Smithy RPCv2 CBOR body that was decoded no longer counts as
  unreadable: its IAM resource is the one it names, not `*`.
- An IAM user, group or policy is named as one (`user/bob`), not as a
  role, and an ARN keeps its whole path.
- A form with an empty field (`a=1&&b=2`) is read past it.
- `s3.localhost` is path style: its bucket is in the path.
  `DeleteBucketLifecycle` and `RestoreObject` are named.
- The hint for unknown credentials no longer mentions `lenslet run`.

## Rewind and Open Account
- **The account is built on an engine of its own** and serves once it is
  done, with the session changing over at the same moment. Until then,
  calls go to the account as it was; none sees it half built.
- What the account in use does meanwhile is still recorded: only the
  rebuild's own calls are left out.
- **Ids the emulator makes up are followed.** A multipart upload's id, a
  KMS key's and the receipt handles of received messages are new in the
  rebuilt account; the calls that name them are sent with the new ones.
  Uploaded objects, keys' aliases and deleted messages come back as they
  were.
- A call whose handle cannot exist any more fails at once instead of
  being retried for ten seconds; a missing resource is waited for only
  while it may still appear.

## The terminal
- **bash with `HISTCONTROL=ignorespace` or `ignoreboth`** (Ubuntu's
  default): a command typed with a space first is its own run, not the
  one before it. History still keeps out what your settings keep out.
- Only keys AWS hands out (STS, a new access key) are the terminal's: an
  `AccessKeyId` in an object or an item is data. A new empty account
  forgets them.
- An unsigned call joins the running command only when a program that
  says what it is sent it (curl, an SDK), not a browser.

## Copy as
- **Copy as AWS CLI and boto3** work for S3, Lambda, the Query services
  (IAM, STS, SNS) and RPCv2 CBOR, as well as the JSON services.
- **Copy as curl** sends a binary, CBOR, framed or compressed body as its
  bytes, never as the text the window shows for it.

## The endpoint
- Only the addresses the emulator made up (queue URLs, `Location`,
  function URLs, presigned downloads) are rewritten to name Lenslet,
  including `[::1]` and a bucket's `.localhost` host. A function's
  answer, objects, items, parameters, secrets and messages come back as
  they were stored.
- Lenslet also listens on `[::1]`, where the machine has IPv6, as its
  `localhost` promises.
- On macOS a function's memory is not measured, so its memory size is not
  enforced; its timeout is. This is now said where it is decided.

## Building
- README: apply the patches before the first build.
- CI runs the end-to-end tests (`chain`, `rewind`) with boto3 on Linux
  and macOS.

## Known limits
- **Other ids made up by the emulator.** Rewinding or opening an account
  follows the ids above; calls that name others it makes anew (a Lambda
  event source mapping's, for one) can fail, and Lenslet reports them as
  failed.
