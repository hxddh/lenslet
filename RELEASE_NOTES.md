# Lenslet 16.0.0

Lenslet 16 shows what happens inside AWS, not just what your code sends to
it. It organizes everything by the commands you run, and it can put the
account back as it was before any of them.

## What AWS did because of your call

The calls are a tree now. Under each call your code made sit the calls AWS
made because of it:
- an S3 upload, and under it the function its notification invoked;
- under the function, its own call to DynamoDB, and why that call was denied;
- a topic delivering to a queue;
- a rule reaching its target.

Delivered later or right away, each call hangs under its cause. On AWS you
would piece this together from CloudTrail, X-Ray and CloudWatch. The
inspector says when AWS made a call, and links the call that caused it.

## Every command is a run

Each command you run in Lenslet's terminal is a run. The sidebar lists the
runs, newest first, each with how it ended. A run's summary shows:
- how it ended and how long it took;
- its calls, and the calls AWS made because of them;
- the first failure and why, one click from its details;
- what it changed;
- the IAM policy it needed, to copy or save.

Calls from elsewhere are under All Activity. Runs come from bash and zsh;
Lenslet adds marks around each command after your own startup files.

## Fix it, go back, run it again

- **Allow for role.** When a function is denied by its role, the inspector
  offers to allow the role exactly what the call needed.
- **Rewind to before a run** puts the account back as it was when the command
  started. Lenslet sends the calls that changed the account again, in order,
  to a new engine. What AWS does in response (a function's writes) lands
  seconds later, as on AWS.
- **Run Again**, on a run's context menu, repeats its command.

## Simpler

The Overview and Permissions pages are gone: a run's summary says the same
things about the run you care about. The sidebar has two groups, Runs and
Resources.

## Under the hood

Lenslet applies a small patch to AWS Emulator: its router reports every call
it serves to an observer. The submodule stays at the upstream commit. CI and
the release apply `patches/` at build time.
