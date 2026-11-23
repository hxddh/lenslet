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

pub struct Shell {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    exited: Arc<AtomicBool>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
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
        let (mut command, integrated) = integrate(&program, env).unwrap_or_else(|| {
            let mut command = CommandBuilder::new(&program);
            if cfg!(unix) {
                // A login shell reads the user's profile: their PATH, their tools.
                command.arg("-l");
            }
            (command, false)
        });
        command.cwd(&folder);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("LENSLET", "1");
        for name in DROPPED {
            command.env_remove(name);
        }
        for (name, value) in env {
            command.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let exited = Arc::new(AtomicBool::new(false));
        {
            let (output, exited) = (output.clone(), exited.clone());
            std::thread::Builder::new()
                .name("lenslet-shell".into())
                .spawn(move || {
                    let mut marks = Marks::default();
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let (text, found) = marks.split(&buf[..n]);
                                for mark in found {
                                    on_mark(mark);
                                }
                                output
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .extend_from_slice(&text);
                            }
                        }
                    }
                    exited.store(true, Ordering::SeqCst);
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(Shell {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            output,
            exited,
            child: Mutex::new(child),
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
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
}

/// The user's shell: `$SHELL`, else the platform's.
fn program() -> String {
    if cfg!(windows) {
        return std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into());
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

/// The marks' prefix: an OSC sequence no terminal acts on.
const MARK: &[u8] = b"\x1b]6973;L;";

/// The command line, hex-encoded so it survives any character.
const HEX: &str = "od -An -tx1 | tr -d ' \\n'";

/// The shell's start with its marks, for bash and zsh; `None` for others.
fn integrate(program: &str, env: &[(&str, String)]) -> Option<(CommandBuilder, bool)> {
    if cfg!(windows) {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    let dir = std::env::temp_dir().join(format!("lenslet-shell-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
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
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
fi
{ours}
__lenslet_end() {{ local s=$?; printf '\033]6973;L;E;%s\007' "$s"; return $s; }}
__lenslet_start() {{ HISTTIMEFORMAT= builtin history 1 | sed 's/^ *[0-9]*[* ] *//' | tr -d '\n' | {HEX}; }}
PROMPT_COMMAND="__lenslet_end${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}"
PS0='\[\033]6973;L;S;$(__lenslet_start)\007\]'"$PS0"
"#
            );
            std::fs::write(&rc, script).ok()?;
            let mut command = CommandBuilder::new(program);
            command.args(["--rcfile", rc.to_str()?, "-i"]);
            Some((command, true))
        }
        "zsh" => {
            let zdot = dir.join("zsh");
            std::fs::create_dir_all(&zdot).ok()?;
            let user = std::env::var("ZDOTDIR")
                .ok()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_default();
            for file in [".zshenv", ".zprofile", ".zlogin"] {
                let script = format!("[ -f '{user}/{file}' ] && . '{user}/{file}'\n");
                std::fs::write(zdot.join(file), script).ok()?;
            }
            let script = format!(
                r#"# Lenslet: the user's .zshrc, then marks around each command.
[ -f '{user}/.zshrc' ] && . '{user}/.zshrc'
{ours}
__lenslet_preexec() {{ printf '\033]6973;L;S;%s\007' "$(printf '%s' "$1" | {HEX})"; }}
__lenslet_precmd() {{ local s=$?; printf '\033]6973;L;E;%s\007' "$s"; }}
autoload -Uz add-zsh-hook
add-zsh-hook preexec __lenslet_preexec
add-zsh-hook precmd __lenslet_precmd
"#
            );
            std::fs::write(zdot.join(".zshrc"), script).ok()?;
            let mut command = CommandBuilder::new(program);
            command.arg("-l");
            command.env("ZDOTDIR", zdot.to_str()?);
            Some((command, true))
        }
        _ => None,
    }
}

/// Takes the marks out of the shell's output, across reads.
#[derive(Default)]
pub struct Marks {
    pending: Vec<u8>,
}

impl Marks {
    /// The output without its marks, and the marks found in it.
    pub fn split(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<Mark>) {
        self.pending.extend_from_slice(chunk);
        let mut text = Vec::with_capacity(self.pending.len());
        let mut marks = Vec::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match find(rest, MARK) {
                Some(i) => {
                    text.extend_from_slice(&rest[..i]);
                    let after = &rest[i + MARK.len()..];
                    match after.iter().position(|&b| b == 0x07) {
                        Some(end) => {
                            if let Some(mark) = parse(&after[..end]) {
                                marks.push(mark);
                            }
                            rest = &after[end + 1..];
                        }
                        None => {
                            // The mark continues in the next read.
                            let keep = rest[i..].to_vec();
                            self.pending = keep;
                            return (text, marks);
                        }
                    }
                }
                None => {
                    // The tail may be the start of a mark.
                    let keep = (1..MARK.len().min(rest.len() + 1))
                        .rev()
                        .find(|&n| rest.ends_with(&MARK[..n]))
                        .unwrap_or(0);
                    text.extend_from_slice(&rest[..rest.len() - keep]);
                    let keep = rest[rest.len() - keep..].to_vec();
                    self.pending = keep;
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
    let text = std::str::from_utf8(mark).ok()?;
    let (kind, value) = text.split_once(';')?;
    match kind {
        "S" => {
            let bytes: Option<Vec<u8>> = (0..value.len() / 2)
                .map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok())
                .collect();
            Some(Mark::Start(
                String::from_utf8_lossy(&bytes?).trim().to_string(),
            ))
        }
        "E" => Some(Mark::End(value.trim().parse().unwrap_or(0))),
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

    #[cfg(unix)]
    #[test]
    fn bash_marks_each_command_with_its_line_and_status() {
        use std::time::{Duration, Instant};
        if !Path::new("/bin/bash").exists() {
            return;
        }
        // SAFETY of the test: the shell is chosen by $SHELL; this test runs bash.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let heard = seen.clone();
        let dir = std::env::temp_dir();
        let (mut command, integrated) = integrate(
            "/bin/bash",
            &[("AWS_ENDPOINT_URL", "http://127.0.0.1:4999".into())],
        )
        .expect("bash integrates");
        assert!(integrated);
        command.cwd(&dir);
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
            let mut marks = Marks::default();
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let (_, found) = marks.split(&buf[..n]);
                heard.lock().unwrap().extend(found);
            }
        });
        std::thread::sleep(Duration::from_millis(500));
        writer
            .write_all(b"echo \"$AWS_ENDPOINT_URL\" && false\r")
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
                    Mark::Start("echo \"$AWS_ENDPOINT_URL\" && false".into())
                );
                let _ = child.kill();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        panic!("marks: {:?}", seen.lock().unwrap());
    }
}
