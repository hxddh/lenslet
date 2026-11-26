//! The built-in terminal's program: the user's own shell in a pseudo-terminal,
//! started in a folder with the variables that point SDKs and the AWS CLI at
//! Lenslet. What it prints collects in a buffer the window drains; what the
//! user types goes to it as is.
//!
//! Bash and zsh also mark where each command starts and ends, as VS Code's
//! shell integration does: the user's own startup files run first, then a
//! hook prints an invisible mark (an OSC sequence Lenslet removes from the
//! output) with the command line before it runs and its exit status after.
//! The marks are what turn a session's calls into runs: the calls a command
//! made between its start and its end.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// What the shell said about its commands.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// A command line starts.
    Start(String),
    /// The command ended with this status.
    End(i32),
}

/// Hears the marks, on the shell's reading thread.
pub type OnMark = Arc<dyn Fn(Mark) + Send + Sync>;

/// The variables a shell must not keep: the user's own credentials and
/// profiles must not reach a request that leaves for real AWS (the AWS CLI v1
/// sends SQS commands to AWS whatever the endpoint says).
const DROPPED: &[&str] = &[
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_ROLE_ARN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
];

/// The variables of the terminal Lenslet was started from.
const FOREIGN: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "KITTY_WINDOW_ID",
    "ALACRITTY_WINDOW_ID",
];

pub struct Shell {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    exited: Arc<AtomicBool>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// The shell is being replaced: its reader no longer reports marks.
    gone: Arc<AtomicBool>,
    on_mark: OnMark,
    /// The startup files Lenslet wrote for it, removed with it.
    dir: Option<PathBuf>,
    pub folder: PathBuf,
    /// The shell marks its commands.
    pub integrated: bool,
}

impl Shell {
    /// The user's shell in `folder`, with `env` set, `cols` by `rows`;
    /// `on_mark` hears where its commands start and end.
    pub fn start(
        folder: PathBuf,
        env: &[(&str, String)],
        cols: u16,
        rows: u16,
        on_mark: OnMark,
    ) -> Result<Shell, String> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let program = program();
        let (mut command, prefix, dir) = match integrate(&program, env) {
            Some(Integration {
                command,
                prefix,
                dir,
            }) => (command, Some(prefix), Some(dir)),
            None => {
                let mut command = CommandBuilder::new(&program);
                if cfg!(unix) {
                    // A login shell reads the user's profile: their PATH, their tools.
                    command.arg("-l");
                }
                (command, None, None)
            }
        };
        let integrated = prefix.is_some();
        command.cwd(&folder);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("LENSLET", "1");
        for name in DROPPED {
            command.env_remove(name);
        }
        // Not the terminal Lenslet was started from: its session files
        // (macOS's /etc/zshrc_Apple_Terminal) are not this shell's.
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && (FOREIGN.contains(&name)
                    || ["ITERM_", "VSCODE_", "KITTY_", "WEZTERM_"]
                        .iter()
                        .any(|p| name.starts_with(p)))
            {
                command.env_remove(name);
            }
        }
        command.env("TERM_PROGRAM", "Lenslet");
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && name.starts_with("AWS_ENDPOINT_URL_")
            {
                command.env_remove(name);
            }
        }
        for (name, value) in env {
            command.env(name, value);
        }
        let child = match pair.slave.spawn_command(command) {
            Ok(child) => child,
            Err(e) => {
                // Its startup files go with it.
                if let Some(dir) = &dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return Err(e.to_string());
            }
        };
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let exited = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        {
            let (output, exited, gone, on_mark) = (
                output.clone(),
                exited.clone(),
                gone.clone(),
                on_mark.clone(),
            );
            // Only an integrated shell's marks: in another shell, text that
            // looks like one is the program's.
            let mut marks = prefix.map(Marks::new);
            std::thread::Builder::new()
                .name("lenslet-shell".into())
                .spawn(move || {
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let (text, found) = match marks.as_mut() {
                                    Some(marks) => marks.split(&buf[..n]),
                                    None => (buf[..n].to_vec(), Vec::new()),
                                };
                                if !gone.load(Ordering::SeqCst) {
                                    for mark in found {
                                        on_mark(mark);
                                    }
                                }
                                output
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .extend_from_slice(&text);
                            }
                        }
                    }
                    exited.store(true, Ordering::SeqCst);
                    // The shell ended (`exit`, a crash) in the middle of a
                    // command, or between them: no command runs now.
                    if !gone.load(Ordering::SeqCst) {
                        on_mark(Mark::End(-1));
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(Shell {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            output,
            exited,
            child: Mutex::new(child),
            gone,
            on_mark,
            dir,
            folder,
            integrated,
        })
    }

    /// What the shell printed since the last call.
    pub fn drain(&self) -> Vec<u8> {
        std::mem::take(&mut *self.output.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Send what the user typed.
    pub fn write(&self, data: &[u8]) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(data);
        let _ = writer.flush();
    }

    /// Run a command line, as if typed.
    pub fn run(&self, line: &str) {
        self.write(format!("{line}\r").as_bytes());
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        let _ = self
            .master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // What the old shell says from now on is not about the new one.
        if !self.gone.swap(true, Ordering::SeqCst) && self.integrated {
            (self.on_mark)(Mark::End(-1));
        }
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The user's shell: `$SHELL`, else the platform's.
fn program() -> String {
    if cfg!(windows) {
        return "powershell.exe".into();
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/bash".into()
            }
        })
}

/// The folder the shell starts in: the one Lenslet was started in when that
/// is a project (not `/`), else the home folder.
pub fn default_folder() -> PathBuf {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match std::env::current_dir() {
        Ok(dir) if dir.parent().is_some() && Some(&dir) != home.as_ref() => dir,
        _ => home.unwrap_or_else(|| PathBuf::from(".")),
    }
}

// -- Shell integration -------------------------------------------------------------

/// The marks' prefix: an OSC sequence no terminal acts on. Each shell's marks
/// also carry a key of its own, so a program that prints the prefix (`cat`
/// of a file) cannot fake a run.
const MARK: &[u8] = b"\x1b]6973;L;";

/// The longest a mark can be; past it, it was not one.
const MAX_MARK: usize = 64 * 1024;

/// The command line, hex-encoded so it survives any character.
const HEX: &str = "od -An -tx1 | tr -d ' \\n'";

struct Integration {
    command: CommandBuilder,
    /// What this shell's marks start with.
    prefix: Vec<u8>,
    dir: PathBuf,
}

/// A random number, from the standard library's hasher keys.
fn random() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now())
}

/// A new folder only this user can read, which did not exist before.
fn private_dir() -> Option<PathBuf> {
    for _ in 0..16 {
        let dir = std::env::temp_dir().join(format!("lenslet-shell-{:016x}", random()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Some(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Write a new file (never through a link left in its place).
fn write_new(path: &Path, text: &str) -> Option<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .ok()?;
    file.write_all(text.as_bytes()).ok()
}

/// The shell's start with its marks, for bash and zsh; `None` for others.
fn integrate(program: &str, env: &[(&str, String)]) -> Option<Integration> {
    if cfg!(windows) {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    if name != "bash" && name != "zsh" {
        return None;
    }
    let dir = private_dir()?;
    let made = integrate_in(&dir, name, program, env);
    if made.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    made
}

fn integrate_in(
    dir: &Path,
    name: &str,
    program: &str,
    env: &[(&str, String)],
) -> Option<Integration> {
    let key = format!("{:08x}", random() as u32);
    let prefix = [MARK, key.as_bytes(), b";"].concat();
    let mark = format!(r"\033]6973;L;{key};");
    // After the user's files: Lenslet's variables win over anything they set.
    let mut ours = String::new();
    for name in DROPPED {
        ours.push_str(&format!("unset {name}\n"));
    }
    for (name, value) in env {
        ours.push_str(&format!(
            "export {name}='{}'\n",
            value.replace('\'', r"'\''")
        ));
    }
    match name {
        "bash" => {
            let rc = dir.join("bashrc");
            let script = format!(
                r#"# Lenslet: the user's login files, then marks around each command.
# Every variable is read with a default: a profile with `set -u` keeps the
# marks. Prompt commands are joined by lines, not `;` (one of the user's may
# end in `;`), and `&& :` keeps the status for the user's own without
# tripping `set -e` or an ERR trap.
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
elif [ -f ~/.bashrc ]; then . ~/.bashrc
fi
{ours}
for __lenslet_v in ${{!AWS_ENDPOINT_URL_*}}; do unset "$__lenslet_v"; done; unset __lenslet_v
__lenslet_nl=$'\n'
# HISTCONTROL's ignorespace and ignoredups, and HISTIGNORE, keep a line out
# of history, where the start mark reads it (a line typed with a space first
# would be read as the one before it). Lenslet takes them over: every line
# goes into history, and the ones they would keep out are taken out again
# as the prompt comes back.
__lenslet_hc=; __lenslet_hi=
__lenslet_take() {{
  local w c=
  if [ -n "${{HISTIGNORE-}}" ]; then __lenslet_hi=$HISTIGNORE; HISTIGNORE=; fi
  case ":${{HISTCONTROL-}}:" in *:ignore*) ;; *) return 0 ;; esac
  __lenslet_hc=
  for w in ${{HISTCONTROL//:/ }}; do
    case $w in ignorespace|ignoredups|ignoreboth) __lenslet_hc="$__lenslet_hc:$w" ;; *) c="$c:$w" ;; esac
  done
  HISTCONTROL=${{c#:}}
  return 0
}}
__lenslet_last() {{ local h; h=$(HISTTIMEFORMAT= builtin history 1); h=${{h#"${{h%%[![:space:]]*}}"}}; printf '%s' "${{h%%[!0-9]*}}"; }}
__lenslet_forget() {{
  shopt -oq history || return 0
  local n line prev= p out=; local -a pats=()
  n=$(__lenslet_last)
  if [ -z "$n" ] || [ "$n" = "${{__lenslet_seen-}}" ]; then return 0; fi
  line=$(HISTTIMEFORMAT= builtin history 1 | sed '1s/^ *[0-9]*[* ] //')
  prev=$(HISTTIMEFORMAT= builtin history 2 | sed -e "/^ *$n[* ] /,\$d" -e '1s/^ *[0-9]*[* ] //')
  case ":$__lenslet_hc:" in *:ignorespace:*|*:ignoreboth:*) case $line in [[:space:]]*) out=1 ;; esac ;; esac
  case ":$__lenslet_hc:" in *:ignoredups:*|*:ignoreboth:*) if [ "$line" = "$prev" ]; then out=1; fi ;; esac
  if [ -n "$__lenslet_hi" ]; then
    IFS=: read -r -a pats <<< "$__lenslet_hi"
    for p in "${{pats[@]}}"; do
      if [ "$p" = "&" ]; then [ "$line" = "$prev" ] && out=1; elif [[ $line == $p ]]; then out=1; fi
    done
  fi
  if [ -n "$out" ]; then builtin history -d "$n"; fi
  __lenslet_seen=$(__lenslet_last)
  return 0
}}
__lenslet_take
__lenslet_seen=$(__lenslet_last)
__lenslet_end() {{ local s=$?; __lenslet_armed=; __lenslet_take || :; __lenslet_forget || :; printf '{mark}E;%s\007' "$s"; return $s; }}
# The line just entered, as history keeps it (none with history off).
__lenslet_start() {{ shopt -oq history || return 0; HISTTIMEFORMAT= builtin history 1 | sed '1s/^ *[0-9]*[* ] //' | {HEX}; }}
if [ -z "${{__LENSLET_DEBUG_TRAP-}}" ] && {{ [ "${{BASH_VERSINFO[0]}}" -gt 4 ] || {{ [ "${{BASH_VERSINFO[0]}}" -eq 4 ] && [ "${{BASH_VERSINFO[1]}}" -ge 4 ]; }}; }}; then
  PROMPT_COMMAND="__lenslet_end && :${{PROMPT_COMMAND:+$__lenslet_nl$PROMPT_COMMAND}}"
  PS0='{mark}S;$(__lenslet_start)\007'"${{PS0-}}"
else
  # Bash before 4.4 (macOS's /bin/bash) has no PS0: mark the start from the
  # DEBUG trap, armed as the prompt shows (after the user's own prompt
  # commands), once per command line.
  __lenslet_armed=
  __lenslet_arm() {{ __lenslet_armed=1; }}
  __lenslet_debug() {{
    [ -n "${{__lenslet_armed-}}" ] || return 0
    [ -n "${{COMP_LINE-}}" ] && return 0
    case "$BASH_COMMAND" in __lenslet_*|:) return 0 ;; esac
    __lenslet_armed=
    printf '{mark}S;%s\007' "$(__lenslet_start)"
  }}
  PROMPT_COMMAND="__lenslet_end && :${{PROMPT_COMMAND:+$__lenslet_nl$PROMPT_COMMAND}}${{__lenslet_nl}}__lenslet_arm"
  trap '__lenslet_debug' DEBUG
fi
"#
            );
            write_new(&rc, &script)?;
            let mut command = CommandBuilder::new(program);
            command.args(["--rcfile", rc.to_str()?, "-i"]);
            Some(Integration {
                command,
                prefix,
                dir: dir.to_path_buf(),
            })
        }
        "zsh" => {
            // As VS Code does: zsh reads Lenslet's startup files, each of
            // which reads the user's own with ZDOTDIR as the user has it, and
            // the last one puts the user's ZDOTDIR back (their history, their
            // completion cache, and the shells started from this one).
            let zdot = dir.join("zsh");
            std::fs::create_dir(&zdot).ok()?;
            let source = |file: &str| {
                format!(
                    r#"if [ -f "$__lenslet_user/{file}" ]; then
  ZDOTDIR="$__lenslet_user"; . "$__lenslet_user/{file}"; __lenslet_user="$ZDOTDIR"; ZDOTDIR="$__lenslet_zdot"
fi
"#
                )
            };
            let zshenv = format!(
                r#"__lenslet_zdot="$ZDOTDIR"
__lenslet_user="${{LENSLET_USER_ZDOTDIR:-$HOME}}"
unset LENSLET_USER_ZDOTDIR
{}"#,
                source(".zshenv")
            );
            write_new(&zdot.join(".zshenv"), &zshenv)?;
            write_new(&zdot.join(".zprofile"), &source(".zprofile"))?;
            let script = format!(
                r#"# Lenslet: the user's .zshrc, then marks around each command.
{}
# A history file the system's zshrc put under Lenslet's ZDOTDIR (macOS's
# does: HISTFILE=${{ZDOTDIR:-$HOME}}/.zsh_history) is the user's.
if [ "${{HISTFILE-}}" = "$__lenslet_zdot/.zsh_history" ]; then HISTFILE="$__lenslet_user/.zsh_history"; fi
if [ "$__lenslet_user" != "$HOME" ]; then ZDOTDIR="$__lenslet_user"; else unset ZDOTDIR; fi
unset __lenslet_zdot __lenslet_user
__lenslet_env() {{
{ours}
unset -m 'AWS_ENDPOINT_URL_*'
}}
__lenslet_env
__lenslet_preexec() {{ printf '{mark}S;%s\007' "$(printf '%s' "$1" | {HEX})"; }}
__lenslet_precmd() {{ local s=$?; printf '{mark}E;%s\007' "$s"; }}
# The user's .zlogin runs after this file: Lenslet's variables again, once.
__lenslet_first() {{ __lenslet_env; add-zsh-hook -d precmd __lenslet_first; unset -f __lenslet_first; }}
autoload -Uz add-zsh-hook
add-zsh-hook preexec __lenslet_preexec
add-zsh-hook precmd __lenslet_precmd
add-zsh-hook precmd __lenslet_first
"#,
                source(".zshrc")
            );
            write_new(&zdot.join(".zshrc"), &script)?;
            let mut command = CommandBuilder::new(program);
            command.arg("-l");
            command.env("ZDOTDIR", zdot.to_str()?);
            if let Some(user) = std::env::var_os("ZDOTDIR") {
                command.env("LENSLET_USER_ZDOTDIR", user);
            }
            Some(Integration {
                command,
                prefix,
                dir: dir.to_path_buf(),
            })
        }
        _ => None,
    }
}

/// Takes the marks out of the shell's output, across reads.
pub struct Marks {
    prefix: Vec<u8>,
    pending: Vec<u8>,
}

impl Default for Marks {
    fn default() -> Self {
        Self::new(MARK.to_vec())
    }
}

impl Marks {
    /// Marks that start with `prefix`.
    fn new(prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            pending: Vec::new(),
        }
    }

    /// The output without its marks, and the marks found in it.
    pub fn split(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<Mark>) {
        self.pending.extend_from_slice(chunk);
        let pending = std::mem::take(&mut self.pending);
        let prefix = &self.prefix;
        let mut text = Vec::with_capacity(pending.len());
        let mut marks = Vec::new();
        let mut rest: &[u8] = &pending;
        loop {
            match find(rest, prefix) {
                Some(i) => {
                    text.extend_from_slice(&rest[..i]);
                    let after = &rest[i + prefix.len()..];
                    match after.iter().position(|&b| b == 0x07 || b == 0x1b) {
                        Some(end) if after[end] == 0x07 => {
                            if let Some(mark) = parse(&after[..end]) {
                                marks.push(mark);
                            }
                            rest = &after[end + 1..];
                        }
                        // Another escape before the end: it was not a mark.
                        Some(_) => {
                            text.extend_from_slice(&rest[i..i + prefix.len()]);
                            rest = after;
                        }
                        None if after.len() > MAX_MARK => {
                            text.extend_from_slice(&rest[i..i + prefix.len()]);
                            rest = after;
                        }
                        None => {
                            // The mark continues in the next read.
                            self.pending = rest[i..].to_vec();
                            return (text, marks);
                        }
                    }
                }
                None => {
                    // The tail may be the start of a mark.
                    let keep = (1..prefix.len().min(rest.len() + 1))
                        .rev()
                        .find(|&n| rest.ends_with(&prefix[..n]))
                        .unwrap_or(0);
                    text.extend_from_slice(&rest[..rest.len() - keep]);
                    self.pending = rest[rest.len() - keep..].to_vec();
                    return (text, marks);
                }
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse(mark: &[u8]) -> Option<Mark> {
    let at = mark.iter().position(|&b| b == b';')?;
    let (kind, value) = (&mark[..at], &mark[at + 1..]);
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    match kind {
        b"S" => {
            let bytes: Option<Vec<u8>> = value
                .chunks(2)
                .filter(|pair| pair.len() == 2)
                .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
                .collect();
            Some(Mark::Start(
                String::from_utf8_lossy(&bytes?).trim().to_string(),
            ))
        }
        b"E" => Some(Mark::End(
            std::str::from_utf8(value).ok()?.trim().parse().unwrap_or(0),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_marks_out_of_the_output_across_reads() {
        let mut marks = Marks::default();
        let (a, m) = marks.split(b"$ \x1b]6973;L;S;6c73\x07file\n\x1b]69");
        assert_eq!(a, b"$ file\n");
        assert_eq!(m, vec![Mark::Start("ls".into())]);
        let (b, m) = marks.split(b"73;L;E;1\x07$ ");
        assert_eq!(b, b"$ ");
        assert_eq!(m, vec![Mark::End(1)]);
        let (c, m) = marks.split(b"plain \x1b[0m text");
        assert_eq!(c, b"plain \x1b[0m text");
        assert!(m.is_empty());
    }

    #[test]
    fn what_only_looks_like_a_mark_stays_text() {
        let mut marks = Marks::default();
        // Not hex, not UTF-8 at a byte boundary: no mark, and no panic.
        let (a, m) = marks.split("\x1b]6973;L;S;aé\x07!".as_bytes());
        assert_eq!(a, b"!");
        assert!(m.is_empty());
        // A mark cut off by another escape gives its text back.
        let (b, m) = marks.split(b"\x1b]6973;L;S;6c\x1b[0m ok");
        assert_eq!(b, b"\x1b]6973;L;S;6c\x1b[0m ok");
        assert!(m.is_empty());
        // Another shell's key is not this one's.
        let mut keyed = Marks::new(b"\x1b]6973;L;abcd;".to_vec());
        let (c, m) = keyed.split(b"\x1b]6973;L;S;6c73\x07\x1b]6973;L;abcd;E;0\x07");
        assert_eq!(c, b"\x1b]6973;L;S;6c73\x07");
        assert_eq!(m, vec![Mark::End(0)]);
    }

    #[cfg(unix)]
    #[test]
    fn bash_marks_each_command_with_its_line_and_status() {
        shell_marks("/bin/bash", false);
    }

    #[cfg(unix)]
    #[test]
    fn zsh_marks_each_command_and_gives_zdotdir_back() {
        shell_marks("/bin/zsh", false);
    }

    /// The DEBUG-trap way, which bash before 4.4 (macOS's) uses.
    #[cfg(unix)]
    #[test]
    fn bash_without_ps0_marks_each_command_too() {
        shell_marks("/bin/bash", true);
    }

    /// bash with a strict profile: `set -u`, `set -e`, and a prompt
    /// command that ends in `;`. The marks still come, with each command's
    /// line and status, and a command that fails inside `&&` does not end
    /// the shell (as it does not in bash itself).
    #[cfg(unix)]
    #[test]
    fn bash_marks_with_a_strict_profile() {
        for debug_trap in [false, true] {
            let Some((marks, _)) = bash_session(
                "set -u\nset -e\nPROMPT_COMMAND=\"history -a;\"\n",
                &["false && true", "echo alive"],
                debug_trap,
            ) else {
                return;
            };
            assert_eq!(
                marks,
                vec![
                    Mark::Start("false && true".into()),
                    Mark::End(1),
                    Mark::Start("echo alive".into()),
                    Mark::End(0),
                ],
                "debug trap: {debug_trap}"
            );
        }
    }

    /// With `HISTCONTROL=ignoreboth` (Ubuntu's default) a line typed with a
    /// space first, or typed twice, is still the line that runs, never the
    /// one before it; and history keeps out what the user's settings keep
    /// out.
    #[cfg(unix)]
    #[test]
    fn bash_marks_lines_history_keeps_out() {
        for debug_trap in [false, true] {
            let Some((marks, out)) = bash_session(
                "set -u\nHISTCONTROL=ignoreboth\nHISTIGNORE='ls*'\n",
                &[
                    "echo one",
                    " echo secret",
                    "echo one",
                    "ls -d /",
                    "history | grep -c 'sec[r]et\\|ls -[d]\\|echo [o]ne'",
                ],
                debug_trap,
            ) else {
                return;
            };
            let starts: Vec<&Mark> = marks
                .iter()
                .filter(|m| matches!(m, Mark::Start(_)))
                .collect();
            assert_eq!(
                starts,
                [
                    "echo one",
                    "echo secret",
                    "echo one",
                    "ls -d /",
                    "history | grep -c 'sec[r]et\\|ls -[d]\\|echo [o]ne'"
                ]
                .map(|l| Mark::Start(l.into()))
                .iter()
                .collect::<Vec<_>>(),
                "debug trap: {debug_trap}"
            );
            // Only the first `echo one` is in history.
            assert!(out.contains("\r1\r\n"), "debug trap: {debug_trap}: {out:?}");
        }
    }

    /// Run `lines` in an integrated bash whose profile is `profile`: the
    /// marks from the first command on, and what it printed. `None` without
    /// bash.
    #[cfg(unix)]
    fn bash_session(
        profile: &str,
        lines: &[&str],
        debug_trap: bool,
    ) -> Option<(Vec<Mark>, String)> {
        use std::time::{Duration, Instant};
        if !Path::new("/bin/bash").exists() {
            return None;
        }
        let home = std::env::temp_dir().join(format!("lenslet-home-{:x}", random()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".bash_profile"), profile).unwrap();
        let Integration {
            mut command,
            prefix,
            dir: rc,
        } = integrate("/bin/bash", &[]).expect("bash integrates");
        command.cwd(&home);
        command.env("HOME", &home);
        command.env("HISTFILE", home.join(".bash_history"));
        if debug_trap {
            command.env("__LENSLET_DEBUG_TRAP", "1");
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let printed = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (heard, shown) = (seen.clone(), printed.clone());
        std::thread::spawn(move || {
            let mut marks = Marks::new(prefix);
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let (text, found) = marks.split(&buf[..n]);
                shown.lock().unwrap().extend(text);
                heard.lock().unwrap().extend(found);
            }
        });
        let ends = |marks: &[Mark]| marks.iter().filter(|m| matches!(m, Mark::End(_))).count();
        // The first prompt's end mark (no command yet) comes first.
        let deadline = Instant::now() + Duration::from_secs(10);
        while ends(&seen.lock().unwrap()) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        for (i, line) in lines.iter().enumerate() {
            writer.write_all(format!("{line}\r").as_bytes()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while ends(&seen.lock().unwrap()) < i + 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&rc);
        let _ = std::fs::remove_dir_all(&home);
        let marks = seen.lock().unwrap().clone();
        let from = marks
            .iter()
            .position(|m| matches!(m, Mark::Start(_)))
            .unwrap_or(marks.len());
        let out = String::from_utf8_lossy(&printed.lock().unwrap()).into_owned();
        Some((marks[from..].to_vec(), out))
    }

    #[cfg(unix)]
    fn shell_marks(program: &str, debug_trap: bool) {
        use std::time::{Duration, Instant};
        if !Path::new(program).exists() {
            return;
        }
        // SAFETY of the test: the shell is chosen by $SHELL; this test runs bash.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let heard = seen.clone();
        let printed = Arc::new(Mutex::new(Vec::<u8>::new()));
        let shown = printed.clone();
        let dir = std::env::temp_dir();
        let Integration {
            mut command,
            prefix,
            dir: rc,
        } = integrate(
            program,
            &[("AWS_ENDPOINT_URL", "http://127.0.0.1:4999".into())],
        )
        .expect("the shell integrates");
        command.cwd(&dir);
        if debug_trap {
            command.env("__LENSLET_DEBUG_TRAP", "1");
        }
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        std::thread::spawn(move || {
            let mut marks = Marks::new(prefix);
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let (text, found) = marks.split(&buf[..n]);
                shown.lock().unwrap().extend(text);
                heard.lock().unwrap().extend(found);
            }
        });
        std::thread::sleep(Duration::from_millis(500));
        writer
            .write_all(b"echo \"$AWS_ENDPOINT_URL\" \"${ZDOTDIR-}\" && false\r")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let marks = seen.lock().unwrap().clone();
            let start = marks.iter().position(|m| matches!(m, Mark::Start(_)));
            if let Some(i) = start
                && marks[i + 1..].contains(&Mark::End(1))
            {
                assert_eq!(
                    marks[i],
                    Mark::Start("echo \"$AWS_ENDPOINT_URL\" \"${ZDOTDIR-}\" && false".into())
                );
                let _ = child.kill();
                let _ = std::fs::remove_dir_all(&rc);
                // Lenslet's endpoint, and ZDOTDIR as the user had it (unset).
                let out = String::from_utf8_lossy(&printed.lock().unwrap()).into_owned();
                assert!(out.contains("http://127.0.0.1:4999 \r\n"), "{out:?}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        panic!("marks: {:?}", seen.lock().unwrap());
    }
}
