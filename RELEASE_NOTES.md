# Lenslet 17.0.1

A bug-fix release, from a deep review of 17.0.

## Dark mode
- **Text shows in dark mode.** Text without a color of its own stayed
  black on the dark backgrounds, so most of the window was unreadable:
  the sidebar, the calls, buttons and the terminal's prompt.
- Faint text (counts, hints, the terminal's endpoint) is now readable on
  every background.
- Toolbar tooltips have a box of their own, in both modes.
- On macOS, a selected failed call is tinted rather than solid blue, so
  its red status stays readable.

## Runs
- **The calls AWS makes inside a command's call are in its run.** For
  example, a function's own calls during `aws lambda invoke`: they were
  recorded before the call that caused them, and so were left out of the
  run, its summary and its IAM policy.
- A run with no calls says so ("This command made no AWS calls") instead
  of blaming the filters.

## The account
- **New Account is refused while a rewind or an opened account is being
  rebuilt.** It used to let the replay go on filling the new account.
- The new engine is built before the account changes over, so no call
  lands between the account's history and the engine.
- The history counts headers and calls, not only bodies, so a long
  polling loop no longer grows it without bound.
- Save Account writes on the engine's threads, and the window no longer
  freezes on a large account.
- Header values that are not plain text survive a save.

## Security
- The endpoint answers only requests addressed to this machine
  (127.0.0.1, ::1, localhost). A web page whose name was made to point at
  127.0.0.1 (DNS rebinding) can no longer read or change the account.

## The terminal
- **Showing the terminal again no longer types into the shell.** Programs
  had asked the terminal where the cursor is, and those questions were
  answered again into the prompt.
- After a long full-screen program (vim, htop), showing the terminal again
  keeps its screen.
- Output that arrived while the terminal was reappearing was shown twice;
  it now shows once.
- **bash**
  - A profile with `set -u` or `set -e`, or a prompt command ending in
    `;`, keeps its runs, and a failing command inside `&&` no longer ends
    the shell.
  - A profile with only `~/.bashrc` is read.
  - A multi-line command keeps its lines.
- **zsh on macOS keeps your history.** macOS's `/etc/zshrc` put the
  history file in Lenslet's temporary folder.
- The variables of the terminal Lenslet was started from (`TERM_PROGRAM`
  and the like) are dropped.
- A profile's `ignore_configured_endpoint_urls` can no longer send the
  terminal's calls to AWS.
- Run Again and the welcome examples start a new shell if the old one
  exited.

## The window
- **Window size**
  - On Linux the window no longer shrinks a little at every launch.
  - The smallest window is now 960×600, so the calls keep room beside the
    inspector.
- **A deleted resource's page** says the resource no longer exists, and
  its old contents go away.
- **The inspector**
  - It no longer shows an empty shell for a call that left the list.
  - Show Resource appears as soon as the resource exists.
  - A call selected on another run's page is no longer kept.
- **Shortcuts**
  - The menu's own items (Undo, Copy, Quit) follow the in-app language
    switch.
  - Off macOS, Sidebar and Inspector use Ctrl+Shift+B and Ctrl+Shift+I.
    On Windows, Ctrl+Alt is AltGr and typed letters.
- **Faster with many calls**
  - The calls list is rebuilt only when the calls or the filters change,
    and not while a resource page is on show.
  - Show Resource no longer copies the whole session.
- Clear Calls also clears the service filter.
- Wording fixes in Chinese.

## Known limits
- **Ids made up by the emulator.** Rewinding or opening an account cannot
  rebuild resources whose ids the emulator makes up anew, such as KMS keys
  and Lambda event source mappings. Later calls that name them fail, and
  Lenslet reports them as failed.
- **SQS messages.** Messages a program received and deleted come back
  after a rewind.
