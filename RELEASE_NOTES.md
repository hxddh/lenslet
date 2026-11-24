# Lenslet 16.0.1

A bug-fix release.

## Window
- **One window on launch.** On macOS Lenslet opened a second, empty
  window next to the main one. Now the workbench is the main window.
- **Shortcuts no longer take keys from the terminal** on Linux and Windows.
  Ctrl+R, Ctrl+K, Ctrl+F, Ctrl+E and Ctrl+Shift+C reach your shell again.
  Lenslet's commands use Ctrl+Shift there instead: Find is Ctrl+Shift+F,
  Clear Calls Ctrl+Shift+K, Refresh Ctrl+Shift+R, Export Ctrl+Shift+E.
- **The list stays where you scrolled.** While calls arrived, it kept
  jumping back to the selected call.
- Rewinding, exporting, saving a policy and allowing a role from a context
  menu now finish. Before, closing the menu could cancel them.
- A message no longer clears a newer one early, and "Rewinding…" always
  goes away.
- **Resource pages.**
  - A resource page shows the resource you opened; it could briefly show
    the previous one.
  - It reloads when the account changes, not on every call.
  - A bucket's calls no longer include calls to other buckets that have a
    key of the same name.
- Showing a call that the filters hide turns the filters off.
- Clearing the calls goes back to All Activity if the run on show is gone.

## Runs and the terminal
- **The terminal survives hiding it.** Hiding and showing the terminal
  (Ctrl+`) re-wrapped its lines to one column and lost what it showed. Now
  it shows what the shell printed, and the command keeps running.
- On Linux the terminal could stay blank after launch. It now draws once
  the window has its size.
- A long command line no longer pushes the toolbar's search field and
  buttons out of the window.
- A run whose shell ended mid-command says "Ended with the shell", not
  "Exited with -1".
- **A run ends when its shell does.** Typing `exit`, restarting the shell or
  choosing another folder in the middle of a command used to leave its run
  open forever, and every later call went to it.
- With macOS's bash and a prompt command of your own, an empty Enter or
  Ctrl-C no longer starts a phantom run.
- **zsh keeps your history.** The integration no longer moves `ZDOTDIR`,
  so these work as they do in your own terminal:
  - `HISTFILE`, the completion cache and a `ZDOTDIR` you set in `.zshenv`;
  - zsh started inside the terminal.
- **Variables.** `AWS_ENDPOINT_URL_<SERVICE>` variables from your
  environment or startup files no longer send calls past Lenslet. Your
  `.zlogin` cannot override Lenslet's variables either.
- **Startup files.** The shell's startup files go in a new private folder,
  removed when the shell ends. Before, another user on the machine could
  plant one on Linux.
- **Marks.** Printing text that looks like a mark (such as `cat` of a
  binary file) no longer stops the terminal, swallows output or fakes a run.
- A character split across reads no longer shows as two replacement
  characters.

## Rewind
- **Only rewinds that make sense.** A run whose effects were already undone
  (by a rewind or a new account) cannot be rewound to again, and is dimmed.
  Neither can a run still going, nor two rewinds at once.
- **Retries only what was not ready.** Rewind resends a call only when the
  resource it needs is not ready yet. It reports how many calls failed
  instead of retrying every error for 15 seconds.
- A call that raced a new account no longer leaks into the new account's
  history.
- A session that sent more than 512 MB keeps working; rewinding is off
  until the next new account.

## Engine
- New empty account and rewind build the engine off the window's thread.
  The window no longer stalls while they run.
- **What counts as a change.** Reads like KMS `Encrypt`, `Sign`, S3
  `SelectObjectContent` and `BatchGet…` are no longer treated as changes.
  They do not trigger resource refreshes and are not replayed on rewind.
- An S3 object whose content is JSON or XML is returned unchanged; before,
  Lenslet could rewrite URLs inside it.
- **Requests.** A request whose body did not arrive is answered 400 and not
  recorded. The plain-text greeting is only for browsers and curl, never
  for an SDK.
- **Memory.** The call list keeps the latest 20,000 calls. The tables that
  tie AWS's own calls to their causes no longer grow without bound.
- A call inside the emulator that panics no longer holds rewind's wait for
  20 seconds.
- If you set `LENSLET_PORT`, Lenslet uses that port or says why it cannot.
  It no longer silently takes another one.
