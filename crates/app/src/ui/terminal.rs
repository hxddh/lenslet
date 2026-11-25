//! The terminal at the bottom: the user's shell, started in their project
//! folder with the variables that point every SDK and the AWS CLI here, so
//! running code against the local account is typing the command.

use std::{cell::RefCell, rc::Rc, time::Duration};

use nsh_base::{Terminal, TerminalApi, TerminalEvent};
use nsh_core::prelude::*;

use super::{Ui, engine, style::*, t, ts};
use crate::shell::{Shell, default_folder};

/// How much of what the shell printed is kept to show again.
const HISTORY: usize = 256 * 1024;

pub fn panel(ui: Ui) -> impl IntoElement {
    let term = component_ref::<Terminal>();
    let shell: Rc<RefCell<Option<Shell>>> = Rc::new(RefCell::new(None));
    let size = Rc::new(RefCell::new((100u16, 16u16)));
    let folder = signal(default_folder());
    let exited = signal(false);
    // What the shell printed lately, shown again when the terminal's view is
    // made anew (hiding the panel takes the view away, not the shell).
    let history = Rc::new(RefCell::new(Vec::<u8>::new()));
    // While the history is shown again, what the view would answer (a
    // cursor position report) is not the user's typing.
    let replaying = Rc::new(std::cell::Cell::new(false));
    // A new view waits for its size to be shown the history: what the shell
    // prints meanwhile is in the history, and would show twice.
    let waiting = Rc::new(std::cell::Cell::new(false));

    let start = {
        let (shell, size) = (shell.clone(), size.clone());
        Rc::new(move || {
            let (cols, rows) = *size.borrow();
            match Shell::start(
                folder.get_untracked(),
                &engine().terminal_vars(),
                cols,
                rows,
                engine().on_mark(),
            ) {
                Ok(s) => {
                    *shell.borrow_mut() = Some(s);
                    exited.set(false);
                }
                Err(e) => {
                    if let Some(api) = term.api::<TerminalApi>() {
                        api.write(format!(
                            "\r\n{}: {e}\r\n",
                            t("Could not start a shell", "无法启动 shell")
                        ));
                    }
                }
            }
        })
    };
    // Output, at the display's pace.
    {
        let (shell, history, waiting) = (shell.clone(), history.clone(), waiting.clone());
        set_interval(Duration::from_millis(16), move || {
            let guard = shell.borrow();
            let Some(s) = guard.as_ref() else { return };
            let bytes = s.drain();
            if s.exited() && !exited.get_untracked() {
                exited.set(true);
            }
            if bytes.is_empty() {
                return;
            }
            {
                let mut history = history.borrow_mut();
                history.extend_from_slice(&bytes);
                if history.len() > HISTORY {
                    trim(&mut history);
                }
            }
            // The view takes the bytes as they come: a character or an
            // escape split across reads is joined there.
            if !waiting.get()
                && let Some(api) = term.api::<TerminalApi>()
            {
                api.write_bytes(&bytes);
            }
        });
    }
    // A command asked for elsewhere (the overview's examples).
    {
        let (shell, start) = (shell.clone(), start.clone());
        effect(move || {
            let Some(line) = ui.run.get() else { return };
            // A shell that exited (`exit`) is started again for it.
            let gone = shell.borrow().as_ref().is_none_or(|s| s.exited());
            if gone {
                *shell.borrow_mut() = None;
                start();
            }
            if let Some(s) = shell.borrow().as_ref() {
                s.run(&line);
            }
            ui.run.set(None);
            if let Some(api) = term.api::<TerminalApi>() {
                api.focus();
            }
        });
    }

    let on_data = {
        let (shell, replaying) = (shell.clone(), replaying.clone());
        move |data: &SharedString| {
            if replaying.get() {
                return;
            }
            if let Some(s) = shell.borrow().as_ref() {
                s.write(data.as_bytes());
            }
        }
    };
    let on_resize = {
        let (shell, size, start) = (shell.clone(), size.clone(), start.clone());
        move |e: &TerminalEvent| {
            if let TerminalEvent::Resize(cols, rows) = e {
                let (cols, rows) = ((*cols).clamp(1, 1000) as u16, (*rows).clamp(1, 1000) as u16);
                // A hidden or not yet laid out panel reports a size of next
                // to nothing: the shell keeps the size it had (or waits for a
                // real one to start), so its lines do not wrap at one column.
                if cols < 10 || rows < 2 {
                    return;
                }
                *size.borrow_mut() = (cols, rows);
                let started = shell.borrow().is_some();
                if started {
                    if let Some(s) = shell.borrow().as_ref() {
                        s.resize(cols, rows);
                    }
                } else {
                    start();
                }
            }
        }
    };
    let restart = {
        let (shell, start, history) = (shell.clone(), start.clone(), history.clone());
        move || {
            *shell.borrow_mut() = None;
            history.borrow_mut().clear();
            if let Some(api) = term.api::<TerminalApi>() {
                api.reset();
            }
            start();
        }
    };
    let choose_folder = {
        let restart = Rc::new(restart.clone());
        move || {
            let restart = restart.clone();
            spawn(async move {
                let dialog =
                    nsh_core::file_dialog::OpenDialog::folders().directory(folder.get_untracked());
                if let Ok(Some(paths)) = pick_paths(dialog).await
                    && let Some(path) = paths.into_iter().next()
                {
                    folder.set(path);
                    restart();
                }
            });
        }
    };

    let header = view()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(30.0))
        .px(px(10.0))
        .bg(chrome_bg())
        .child(icon(Icon::Terminal, 14.0))
        .child(
            text(move || ts("Terminal", "终端"))
                .text_size(small())
                .font_weight(FontWeight::SEMIBOLD)
                .pl(px(4.0)),
        )
        .child(
            nsh_base::button()
                .on_press(choose_folder)
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(6.0))
                .h(px(22.0))
                .rounded(radius())
                .hover(|s| s.bg(hover_bg()))
                .child(icon(Icon::Folder, 13.0))
                .child(
                    text(move || folder.with(|f| tilde(f)))
                        .text_size(small())
                        .font_family(MONO)
                        .text_color(secondary_text()),
                ),
        )
        .child(
            text(move || {
                format!(
                    "AWS_ENDPOINT_URL={}{}",
                    engine().endpoint(),
                    if exited.get() {
                        format!(" · {}", t("exited", "已退出"))
                    } else {
                        String::new()
                    }
                )
            })
            .text_size(small())
            .font_family(MONO)
            .text_color(tertiary_text())
            .flex_1()
            .truncate(),
        )
        .child(tool_button(
            Icon::Restart,
            move || ts("Restart Shell", "重启 shell"),
            || false,
            restart,
        ))
        .child(tool_button(
            Icon::Close,
            move || {
                format!(
                    "{} ({})",
                    t("Hide Terminal", "隐藏终端"),
                    key_label("ctrl+`")
                )
            },
            || false,
            move || ui.terminal.set(false),
        ));

    let on_data = Rc::new(on_data);
    let on_resize = Rc::new(on_resize);
    view()
        .flex_1()
        .bg(AdaptiveColor::rgb(0xffffff, 0x1a1a1a))
        .child(hairline())
        .child(header)
        // Hidden, the view goes (laid out at no size, it would lose its lines);
        // shown again, a new one shows what the shell printed meanwhile.
        .child(when(
            move || ui.terminal.get(),
            move || {
                let (on_data, on_resize) = (on_data.clone(), on_resize.clone());
                let (history, replaying, waiting) =
                    (history.clone(), replaying.clone(), waiting.clone());
                // A new view: once it has a real size, it shows what the shell
                // printed before.
                let fresh = Rc::new(std::cell::Cell::new(true));
                waiting.set(true);
                let on_resize = move |e: &TerminalEvent| {
                    if let TerminalEvent::Resize(cols, rows) = e
                        && *cols >= 10
                        && *rows >= 2
                        && fresh.replace(false)
                        && let Some(api) = term.api::<TerminalApi>()
                    {
                        // Without the questions programs asked the terminal
                        // (where is the cursor?): asked again now, their
                        // answers would reach the shell as typing.
                        let shown = without_queries(&history.borrow());
                        if !shown.is_empty() {
                            replaying.set(true);
                            api.write_bytes(&shown);
                            // The view's events are handled after this:
                            // anything it still answers is dropped too.
                            let replaying = replaying.clone();
                            set_timeout(Duration::from_millis(50), move || replaying.set(false));
                        }
                        waiting.set(false);
                    }
                    on_resize(e);
                };
                nsh_base::terminal()
                    .component_ref(&term)
                    .font_family(SharedString::from(MONO))
                    .text_size(px(12.5))
                    .line_height(px(17.0))
                    .scrollback(10_000u32)
                    .on_data(move |d: &SharedString| on_data(d))
                    .on_resize(on_resize)
                    .flex_1()
                    .px(px(8.0))
                    .py(px(4.0))
            },
        ))
}

fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(std::path::PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// The private modes a full-screen program sets (the alternate screen,
/// application cursor keys, bracketed paste, the cursor shown): kept when
/// the history's start is cut, so what follows shows as it did.
const MODES: &[&[u8]] = &[b"1049", b"47", b"1047", b"1", b"2004", b"25", b"7"];

/// Drop the history's oldest quarter, from a line's start, keeping the
/// modes set in what is dropped.
fn trim(history: &mut Vec<u8>) {
    let from = history.len() - HISTORY * 3 / 4;
    let cut = history[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|n| from + n + 1)
        // No line: before the next escape, never inside one.
        .or_else(|| {
            history[from..]
                .iter()
                .position(|&b| b == 0x1b)
                .map(|n| from + n)
        })
        .unwrap_or(history.len());
    let mut kept: Vec<(&'static [u8], u8)> = Vec::new();
    let dropped = &history[..cut];
    let mut i = 0;
    while let Some(at) = dropped[i..].windows(3).position(|w| w == b"\x1b[?") {
        let start = i + at + 3;
        let end = dropped[start..]
            .iter()
            .position(|b| !(b.is_ascii_digit() || *b == b';'))
            .map(|n| start + n);
        if let Some(end) = end
            && matches!(dropped[end], b'h' | b'l')
        {
            for mode in dropped[start..end].split(|&b| b == b';') {
                if let Some(known) = MODES.iter().find(|m| **m == mode) {
                    kept.retain(|(m, _)| m != known);
                    kept.push((*known, dropped[end]));
                }
            }
        }
        i = start;
    }
    let mut prefix = Vec::new();
    for (mode, set) in kept {
        prefix.extend_from_slice(b"\x1b[?");
        prefix.extend_from_slice(mode);
        prefix.push(set);
    }
    history.splice(..cut, prefix);
}

/// `bytes` without the queries a program sends the terminal (device status
/// and cursor position reports, device attributes): replayed, the view
/// would answer them again.
fn without_queries(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let start = i + 2;
            let end = bytes[start..]
                .iter()
                .position(|b| !(b.is_ascii_digit() || matches!(b, b';' | b'?' | b'>' | b'=')))
                .map(|n| start + n);
            if let Some(end) = end
                && matches!(bytes[end], b'n' | b'c')
            {
                // `ESC[5n`, `ESC[6n`, `ESC[c`, `ESC[>c`…: dropped.
                i = end + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replay_asks_the_terminal_nothing() {
        let shown = without_queries(b"a\x1b[6nb\x1b[>0cc\x1b[31md\x1b[2Je");
        assert_eq!(shown, b"abc\x1b[31md\x1b[2Je");
    }

    #[test]
    fn a_cut_history_keeps_the_modes_set_before_the_cut() {
        let mut history = b"\x1b[?1049h\x1b[?2004h".to_vec();
        history.extend(std::iter::repeat_n(b'x', HISTORY));
        history.extend(b"\nend");
        trim(&mut history);
        assert!(history.starts_with(b"\x1b[?1049h\x1b[?2004h"));
        assert!(history.ends_with(b"end"));
        assert!(history.len() < HISTORY);
    }
}
