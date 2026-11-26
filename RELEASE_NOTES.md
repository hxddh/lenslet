# Lenslet 17.1.1

Fixes where 17.1.0's parts meet: ids followed through every encoding,
copying a call that was framed or compressed, and the account in use while
another is rebuilt.

## Rewind and Open Account
- **New ids are written as the call's encoding writes them.** A receipt
  handle, upload id or key id is swapped in a CBOR body (with its new
  length), in a Query form (`%2B`, `%2F`, `%3D` and all), in the address,
  and in a gzip'd or `aws-chunked` body, which is then sent plain, with
  its headers to match. An S3 body is an object's data and is never
  changed.
- **The account in use is recorded while another is rebuilt.** Each call
  is told by the engine that made it, so a queue's poller invoking a
  function in the account in use stays in the list, and never holds up
  the rebuild.
- Each grant is sent once its role exists, not once at its place and
  again with the role, and is not counted among the calls sent again.

## Allow for role
- Allowing a role again keeps one grant for its policy, the latest, not
  every one before it.
- A policy Lenslet added and your code then deleted stays deleted when
  the role is created again.
- A `CreateRole` sent gzip'd gets the role's grants too.

## Copy as
- **A framed or compressed call copies as AWS CLI and boto3.** An SDK's
  streaming `PutObject` (`aws-chunked`) and a gzip'd Query or Lambda call
  copy from what they say; curl still sends their bytes.

## The endpoint
- **A port another program holds on `[::1]` is passed over.** If
  `127.0.0.1:4510` is free but `[::1]:4510` is not, Lenslet takes a port
  free on both, so `localhost` never reaches the other program. On a
  machine without IPv6 it serves `127.0.0.1` alone. A port set with
  `LENSLET_PORT` is not changed: Lenslet says it is taken.
- A body that inflates past 4 MB is read only that far, and said to be
  cut short.
- A call's permissions are worked out once, not once more for a policy
  evaluation the window does not make.

## The window
- A deleted resource's page says so ("This resource no longer exists.")
  instead of "Loading…", and an empty one says it is empty.

## The terminal
- **bash:** clearing `HISTCONTROL` after starting with `ignoreboth` now
  puts lines typed with a space first back into history. While Lenslet
  applies the rules, `HISTCONTROL` holds the word `lenslet`, which bash
  passes over.

## Known limits
- **Other ids made up by the emulator.** Rewinding or opening an account
  follows multipart upload ids, KMS key ids and SQS receipt handles;
  calls that name others it makes anew (a Lambda event source mapping's,
  for one) can fail, and Lenslet reports them as failed.
