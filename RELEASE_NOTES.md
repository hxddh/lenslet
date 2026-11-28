# Lenslet 18.1.0

What CloudWatch and X-Ray would tell you about a run, in its summary; the
loop after a fix in one click; and fish.

## How a run spent its time
- **Time.** How long the run took and how much of it was spent waiting on
  AWS calls, its three slowest calls, and the longest AWS took to act on
  something the run did (an upload to the function its notification
  invoked). Click any of them to open the call.
- **Services.** Each service the run called: calls, failures, p95. Click
  one to list its calls.
- **Retries.** How many retries the SDKs made, how many calls came through
  in the end, and how many answers said slow down. Click to list every
  attempt of those calls (a new Retried filter, also in the View menu).
- **Functions.** What a function's runs spent, from Lambda's own `REPORT`
  line: duration and billed duration, cold starts, memory used of memory
  size. The run's summary adds them up and warns when a run came near its
  timeout or its memory. The inspector shows each invocation's figures,
  and a call AWS made shows how long after its cause it began.

## The loop, closed
- **A Step Functions execution that fails is a failed call.**
  `StartExecution` is answered before the execution runs; Lenslet follows
  it to the end and marks the call failed with the state it failed in and
  why. (`StartSyncExecution` says so at once.)
- **Rewind and Run Again**, one action: after a fix, back to before the run
  and run the command again. In the run's summary, its menu and the main
  menu.
- **A resource's own policy in the way?** The inspector offers the
  statement that policy needs for the caller, ready to paste. Lenslet does
  not change the policy for you.
- **Run comparisons stay true.** The list keeps the latest 20,000 calls;
  a run whose first calls were let go now says so, and is not compared,
  where it used to show failures it had lost as fixed.

## Easier
- **fish marks its commands** as bash and zsh do: its commands become runs.
  A shell that does not is named in the terminal, with where its calls go.

## Under the hood
- **AWS Emulator** is at its latest, with fixes in DynamoDB (parallel
  scans), Lambda (the Python runtime's bundled SDK), SQS and CloudWatch
  Logs, and its list of what it does not do yet, which Lenslet quotes for a
  failed call, is current.
- Function runs are read once a second per function, for all of its runs
  together, not once per run: hundreds of notifications no longer load the
  emulator.
- Every Rust dependency is at its latest release; CI runs Python 3.14.

## Known limits
- On macOS a function's memory is not measured, so its memory size is not
  enforced or shown; its timeout is.

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
