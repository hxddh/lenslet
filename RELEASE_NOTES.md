# Lenslet 17.0.0

Lenslet 17 makes what it shows trustworthy and the account worth keeping.

## Runs hold only your command's calls

The terminal now signs its calls with an access key of its own. A run
holds only the terminal's calls, plus the keys AWS hands the terminal's
programs (an assumed role's). Your IDE, a second terminal or a test
runner elsewhere no longer lands in the run that happens to be going.
Their calls stay under All Activity.

## Your code or the emulator?

When a call fails, its details say whether AWS Emulator is known to differ
from AWS there, with a link to the issue. Lenslet reads this from the
emulator's own list of what it does not do yet, indexed for Lenslet's
services when it is built. An operation the emulator has not implemented
says so plainly. Otherwise Lenslet names the known difference for that
operation and error, for you to weigh.

## Keep the account

**File → Save Account** writes the calls that made the account to a file.
**Open Account** builds it again in a new engine, the way a rewind does:
- on your machine tomorrow;
- on a teammate's, to reproduce what you see;
- as a starting point you return to.

An opened account saves and rewinds like any other.

## One window, everywhere

Lenslet is built on Native Shell 0.7. Its window model keeps the system's
host window hidden and shows only the window Lenslet declares, so there is
exactly one window, on macOS too. Closing it quits Lenslet: the account
lives in memory, unless you save it.

Also from Native Shell 0.7:
- **Toolbar.** It keeps clear of the window's controls only where it meets
  them.
- **Calls table.** The Resource or Error column fills the room that is
  left.
- **Terminal.** It takes the shell's bytes as they come, and draws at once
  on Linux, with no delay at launch.
- **Copy.** It says so only when the system took it.

## Faster with many calls

- The session keeps its counts as calls arrive: failures per run,
  services, the permissions shown. The window no longer recounts every call
  on each change.
- A large session redraws at a calmer pace.
- A run that changed thousands of objects summarizes them in linear time;
  before, it took quadratic time.
- The resource inventory rests between looks while calls stream in.

With 20,000 calls the window answers at once, and a debug build serves
about half as many more calls a second.

## Shortcuts

Open Account is ⌘O (Ctrl+Shift+O) and Save Account is ⌘S (Ctrl+Shift+S).
Save IAM Policy keeps ⇧⌘S on macOS and has no shortcut elsewhere.
