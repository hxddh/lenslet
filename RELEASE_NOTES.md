# Lenslet 15.0.0

Lenslet 15 rebuilds the window to work the way each platform's apps work,
puts a terminal inside it, and shows far more of what your code does.

## Run your code right here

- **A terminal in the window.** It is your own shell, started in the folder
  you opened Lenslet from, with `AWS_ENDPOINT_URL`, the region and
  placeholder keys already set. Type `pytest` or `aws s3 mb s3://demo` and
  watch the calls arrive. `` Ctrl+` `` shows and hides it; the folder button
  starts it somewhere else.
- **One-click examples.** A new account's overview runs a first command for
  you.
- **A free port when 4510 is taken**, instead of refusing to start.
- **Your own credentials stay out.** The terminal drops `AWS_PROFILE`,
  `AWS_SESSION_TOKEN` and the other credential variables, so nothing it runs
  can sign with your real account.

## Native on each platform

- **A full-height sidebar with the system's sidebar material** (macOS,
  Windows 11), and a toolbar over the content.
- **A menu bar on macOS, a main menu (☰) elsewhere.** Every command is
  there with its shortcut, using ⌘ on macOS and Ctrl elsewhere:
  - Find (⌘F)
  - Clear Calls (⌘K)
  - New Empty Account (⇧⌘N)
  - Copy as AWS CLI (⇧⌘C)
  - Export Session (⌘E)
  - Save IAM Policy (⇧⌘S)
  - the views (⌘1 to ⌘3)
- **Context menus** on calls, the endpoint, and everything else that has
  actions.
- **The system's fonts, accent and semantic colors**, following light and
  dark mode.
- **The window remembers** its size and place, and you can drag the panes
  to resize them.
- **A confirmation** before a new empty account replaces the old one.
- **A Dock badge** that counts failed calls.

## See much more

- **Calls are a real table.**
  - Sort by any column, resize and reorder the columns.
  - Move the selection with the arrow keys.
  - Search across operation, resource, error, client and request ID.
  - Filter to failed or not-emulated calls, or to one service.
- **The inspector** has tabs for the summary, the request, the response and
  the permissions. It shows:
  - the client (boto3, AWS CLI, any SDK, with its version);
  - the SDK's retry attempt, the request ID, the sizes and the region;
  - copy as AWS CLI, curl or boto3.
- **Open any resource** to see what it holds, and every call that touched it:
  - S3 objects (double-click one for its text);
  - DynamoDB items;
  - SQS attributes;
  - CloudWatch Logs events;
  - secret and parameter values;
  - EventBridge rules and targets;
  - Step Functions executions;
  - Lambda configuration;
  - IAM roles and their policies.
- **The overview** shows calls, failures, median and 95th-percentile latency
  and retries, broken down by service and by client, with the latest
  failures one click from their details.
- **The permissions view** shows the calls that needed each permission, next
  to the policy, which you can copy or save.
- **Export Session** writes every call (headers, bodies, permissions) to
  JSON.

## Faster

The window takes the session only when it changed, and shares calls rather
than copying their bodies, so it stays fast with thousands of calls.
