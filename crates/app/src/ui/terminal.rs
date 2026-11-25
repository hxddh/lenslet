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
        let (shell, history) = (shell.clone(), history.clone());
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
                    // From a line's start, so no sequence is cut in two.
                    let from = history.len() - HISTORY * 3 / 4;
                    let cut = history[from..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(from, |n| from + n + 1);
                    history.drain(..cut);
                }
            }
            // The view takes the bytes as they come: a character or an
            // escape split across reads is joined there.
            if let Some(api) = term.api::<TerminalApi>() {
                api.write_bytes(&bytes);
            }
        });
    }
    // A command asked for elsewhere (the overview's examples).
    {
        let (shell, start) = (shell.clone(), start.clone());
        effect(move || {
            let Some(line) = ui.run.get() else { return };
            if shell.borrow().is_none() {
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
                let (history, replaying) = (history.clone(), replaying.clone());
                // A new view: once it has a real size, it shows what the shell
                // printed before.
                let fresh = Rc::new(std::cell::Cell::new(true));
                let on_resize = move |e: &TerminalEvent| {
                    if let TerminalEvent::Resize(cols, rows) = e
                        && *cols >= 10
                        && *rows >= 2
                        && fresh.replace(false)
                        && let Some(api) = term.api::<TerminalApi>()
                    {
                        let shown = history.borrow().clone();
                        if !shown.is_empty() {
                            replaying.set(true);
                            api.write_bytes(&shown);
                            replaying.set(false);
                        }
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
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let shown = path.display().to_string();
    match home.map(|h| std::path::PathBuf::from(h).display().to_string()) {
        Some(h) if shown.starts_with(&h) => format!("~{}", &shown[h.len()..]),
        _ => shown,
    }
}
