# Lenslet 13.0.0

Lenslet 13.0 does one job: in every pull request, what AWS access your code
needs, and why its AWS calls failed. Everything that did not serve that is
gone. The code is 39% smaller (47.6k → 28.9k lines of Rust), the CLI has 6
commands instead of 11, and the window is a viewer with one ▶ for running
your tests.

This release drops compatibility with earlier versions. Run files from
Lenslet 12 and before are not read; re-run your tests.

```bash
curl -fsSL https://github.com/hxddh/lenslet/releases/latest/download/install.sh | sh
lenslet pytest
```

## One check for production: `lenslet check --policy`

- **One way to ask "would this fail in production".** `lenslet check
  --policy role.json` checks the newest run in the current directory against
  an IAM policy, without re-running your tests. `--enforce` and `run --policy`
  are gone.
  - `--enforce` changed how your tests behaved.
  - It missed permissions that only show in a response.
  - It read the same policy file under different rules than `check`.
- **Exact access.** `check` uses the access derived live from each whole
  request body, now stored in the run file (`concrete`). Before, it
  re-derived from the stored body, which was cut at 1 MiB.
- **Conditions are reported, not simulated.** A statement with a `Condition`
  is reported as "conditional, not evaluated" and never counts as allowing.
  The partial condition evaluation, and `--context`, are gone.
- **The right run.** `check` picks the newest run *in this directory*. Before,
  it picked the newest run of any project.
- **CI.** In the action, `policy:` runs `lenslet check --policy` after the
  run. Its annotations no longer suggest `run --update` for a policy failure.

## Removed

- **Commands:** `last`, `runs`, `diff`, `env`, `shell`, and the proxy-only
  mode. Use the window, or `--json`.
- **Run flags:** `--enforce`, `--policy`, `--route`, `--max-body`,
  `--no-env`, `--listen`.
  - Credentials are now always stripped. With `--no-env` they used to reach
    the emulator and the unredacted run file.
- **Findings for hygiene:** N+1 bursts, resources left behind, and retries.
  Findings are failures and new access.
- **`lenslet policy` options:** Terraform output, `--role`, `--region` and
  `--account`. It prints IAM JSON, which every IaC tool accepts.
- **Live events:** `/_lenslet/*` (events, health, shutdown). The window reads
  the run file as it grows.
- **Display-only data:** eventstream, zstd and deflate decoding, body blob
  files, hex previews, and a dozen display-only fields per call.
- **Lock notes:** `# implied` notes. `# test` stays.
- **Pinned runs.** The last 20 finished runs are kept.
- **Compatibility:**
  - the `--sandbox` and `upstream: sandbox` aliases;
  - errors for removed policy formats;
  - fallbacks for older reports;
  - tolerant parsing of older run files.

## Fixed

- **Replays polluted the lock.** A replay from the window counted in the
  report, the lock and `--check`. Replays are gone with Edit & Replay.
- **The window contradicted itself** when there was no lock: "ALLOWED" sat
  next to "Accept into lock", and its counts disagreed. In-lock state now has
  one source.
- **The window opens a run on its first failure**, not the newest call.
- **One test-command detector.** The window and `lenslet init` used to pick
  different test commands. Both now use the CLI's detection, via `lenslet
  doctor --json`.
- **Unbound shortcuts.** Shortcuts the window advertised but never bound are
  bound, and a test checks every advertised shortcut.
- **A complete Chinese UI.** Badges, errors, toasts and menus are now
  translated too.
- **Accept all** lists every permission in its confirmation, and `Enter`
  confirms it.

## The window

- **Header**: the run (`· 2 AWS failures · against moto`), **▶ your test
  command** (`⌘R`; `⌘.` stops), **Calls | Access**, and search: 6 controls,
  down from 8.
- **Calls**: findings first (failures, what is new or fixed since the
  previous run, permissions not in the lock), then the calls. The test
  output is in a drawer (`⌘J`); every run now writes it to `<run>.log`.
- **Inspector**, one column: why the call failed, what it needs, the request
  and response (decoded or raw), and the headers.
- **Access**: accept (`a`), accept all (`⌘⇧A`, with a confirmation),
  remove (`⌫`), `# test` (`t`), and Copy policy (IAM JSON).
- **Removed**:
  - Replay, Edit & Replay and SigV4 signing.
  - The policy simulator and the baseline selector (it always compares with
    `lenslet.lock`, like the PR comment).
  - The timeline and the sidebar.
  - The command palette, demo mode and Settings. The window follows the
    system appearance; English / 中文 is in the menu.
  - Proxies, terminals, Copy Env and CI generation. Use the CLI for those.
  - 11 dependencies, among them ring, rustls, sha2 and zstd.

## Upgrading

- Re-run your tests. Old run files are refused with a message saying so.
- **If you used `--enforce` or `run --policy`**: run your tests, then
  `lenslet check --policy role.json`. In the action, keep `policy:`.
- **`lenslet.lock` files with `# implied` notes still parse.** The notes are
  dropped on the next write.
- **`lenslet last --json`**: use `lenslet run --json PATH`, or the run file's
  last line.
