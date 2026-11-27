# Lenslet 18.0.0

The same loop (run your code, see what happened, fix it, go back, run it
again), with no step left to guess: every failure has its reason, every
rebuild is exact, and the loop does not keep you waiting. And a new mark.

## Every failure has its reason
- **A function that failed is a failed call.** Lambda answers a crashed
  function 200 and says so in a header; Lenslet counted it a success. Now
  the call fails with the function's own error (`ValueError: boom`) and
  where it was raised, and a run's first failure is that function.
- **The same for a function a notification invoked.** Its call was
  answered before it ran; Lenslet now follows the run to its end and marks
  the call failed when the run was.
- **What the function printed, with the call.** The inspector shows an
  invocation's log, read by its request id; when there is none, it says
  why (the function's role may not write logs).
- **Which policy denied a call.** Lenslet tells a role that no policy
  allows from a Deny, a session policy, a permissions boundary and the
  resource's own policy, and offers Allow for role only where it lets the
  call through.

## Every rebuild is exact
- **Ids the emulator makes up are all followed.** Beyond multipart
  uploads and receipt handles, any id an answer made up (a KMS key's, an
  SNS subscription's, an event source mapping's, a Step Functions
  execution's) is swapped for the new account's in the calls that name it.
  The known limit is gone; each kind is tested end to end.
- **Another account's background work stays out.** Each call is told by
  the engine that made it: what a replaced account still does is no
  account's, and several engines in one process each keep their own.

## The loop does not keep you waiting
- **Rewind in seconds.** The account is rebuilt at the pace of what the
  calls set off (a delivery, a function's run), not the time you took
  between commands, and the window says how far it is.
- **Run Again says whether the fix worked.** A run's summary compares it
  with the last run of the same command: what is fixed, what is new, what
  still fails.

## Smaller
- Queue URLs and other addresses in Smithy RPCv2 CBOR answers name Lenslet,
  as they do in JSON and XML.

## A new mark
- A call and the call it caused, an L seen through a lens: the app icon,
  and the window. Linux gets a 512 × 512 icon and a scalable one.

## Under the hood
- **AWS Emulator** is at its latest; the macOS patch now sits in its shared
  Python substrate.
- **Native Shell 0.7.2** redraws only what changed, and pans on the GPU:
  long call lists and the terminal draw less.
- base64 0.23 (one copy in the build, not two), hyper 1.13.
- CI on the current GitHub Actions (Node 24), Python 3.13; artifacts are
  checked against their digests before a release is published.
- Replaying moved to its own module; `cargo fmt -p lenslet -p lenslet-core`
  is how to format (README), as `--all` reaches the emulator.

## Known limits
- On macOS a function's memory is not measured, so its memory size is not
  enforced; its timeout is.
