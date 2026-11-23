//! The built-in terminal's program: the user's own shell in a pseudo-terminal,
//! started in a folder with the variables that point SDKs and the AWS CLI at
//! Lenslet. What it prints collects in a buffer the window drains; what the
//! user types goes to it as is.

use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

pub struct Shell {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    exited: Arc<AtomicBool>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    pub folder: PathBuf,
}

impl Shell {
    /// The user's shell in `folder`, with `env` set, `cols` by `rows`.
    pub fn start(
        folder: PathBuf,
        env: &[(&str, String)],
        cols: u16,
        rows: u16,
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
        let mut command = CommandBuilder::new(program());
        if cfg!(unix) {
            // A login shell reads the user's profile: their PATH, their tools.
            command.arg("-l");
        }
        command.cwd(&folder);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("LENSLET", "1");
        // Only Lenslet's placeholder keys: the user's own credentials and
        // profiles must not reach a request that leaves for real AWS (the AWS
        // CLI v1 sends SQS commands to AWS whatever the endpoint says).
        for name in [
            "AWS_SESSION_TOKEN",
            "AWS_SECURITY_TOKEN",
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
            "AWS_ROLE_ARN",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        ] {
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
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => output
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .extend_from_slice(&buf[..n]),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn runs_commands_with_the_lenslet_environment() {
        let dir = std::env::temp_dir();
        let shell = Shell::start(
            dir,
            &[("AWS_ENDPOINT_URL", "http://127.0.0.1:4999".into())],
            80,
            24,
        )
        .expect("start a shell");
        shell.run("echo endpoint=$AWS_ENDPOINT_URL; exit");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            seen.extend(shell.drain());
            if String::from_utf8_lossy(&seen).contains("endpoint=http://127.0.0.1:4999") {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no output: {}", String::from_utf8_lossy(&seen));
    }
}
