use eframe::egui;
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{forkpty, ForkptyResult, Winsize},
    sys::wait::{waitpid, WaitPidFlag, WaitStatus},
    unistd::Pid,
};

use core::f32;
use std::{
    ffi::CStr,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    process::exit,
};

fn main() {
    let fd: Option<OwnedFd> = unsafe {
        let winsize = Winsize {
            ws_row: 50,
            ws_col: 500,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let res = forkpty(Some(&winsize), None).unwrap();
        match res {
            ForkptyResult::Parent { child, master } => {
                println!("Parent process. Child PID: {} Master FD: Some_value", child);

                // Give child a moment to start
                std::thread::sleep(std::time::Duration::from_millis(100));

                // Check if child is still alive
                match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => println!("Child is still alive"),
                    Ok(status) => println!("Child exited with status: {:?}", status),
                    Err(e) => println!("waitpid error: {}", e),
                }

                // Try to read any initial output from PTY
                let mut initial_buf = vec![0u8; 4096];
                match nix::unistd::read(master.as_raw_fd(), &mut initial_buf) {
                    Ok(n) => {
                        let output = String::from_utf8_lossy(&initial_buf[..n]);
                        println!("Initial PTY output ({} bytes): {:?}", n, output);
                    }
                    Err(e) => println!("Initial read: {}", e),
                }

                // File in non blocking mode to avoid freezing issue
                fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
                    .expect("Failed to set non-blocking mode");
                Some(master) // Return the master file descriptor
            }
            ForkptyResult::Child => {
                let shell_name = CStr::from_bytes_until_nul(b"/bin/bash\0")
                    .expect("Something went wrong in creating the shell_name");
                let arg0 = CStr::from_bytes_until_nul(b"bash\0").unwrap();
                let arg1 = CStr::from_bytes_until_nul(b"--norc\0").unwrap();
                let arg2 = CStr::from_bytes_until_nul(b"--noprofile\0").unwrap();
                let arg3 = CStr::from_bytes_until_nul(b"-i\0").unwrap();
                let args = [arg0, arg1, arg2, arg3];

                // PS1 with colors: blue directory, yellow git branch, green venv
                std::env::remove_var("PROMPT_COMMAND");
                std::env::set_var("PROMPT_DIRTRIM", "2");
                std::env::set_var(
                    "PS1",
                    "\\[\\033[1;34m\\]\\w\\[\\033[1;33m\\]$(git rev-parse --abbrev-ref HEAD 2>/dev/null | sed 's/.*/(&)/')\\[\\033[1;32m\\]$([ -n \"$VIRTUAL_ENV\" ] && echo \"($(basename \"$VIRTUAL_ENV\"))\")\\[\\033[0m\\] $ "
                );

                // Enable colors
                std::env::set_var("TERM", "xterm-256color");

                nix::unistd::execvp(shell_name, &args).unwrap();
                exit(1); // Only reached if execvp fails
            }
        }
    };

    if let Some(fd) = fd {
        println!("Fd read was successful");
        let native_options = eframe::NativeOptions::default();
        let _ = eframe::run_native(
            "Termion",
            native_options,
            Box::new(move |cc| Ok(Box::new(Termion::new(cc, fd)))),
        );
        println!("Completed");
    } else {
        println!("Fd read was unsuccessful");
    }
}

struct Termion {
    fd: OwnedFd,
    buf: Vec<u8>,
    command_history: Vec<String>, // Store all commands TODO: Add delete button, add persistence
    current_command: String,      // Tracks current command pre enter press
    cursor_pos: (usize, usize),   // Window space and scroll back
    prev_cursor_pos: (usize, usize),
    character_size: Option<(f32, f32)>,
}

impl Termion {
    fn new(cc: &eframe::CreationContext<'_>, fd: OwnedFd) -> Self {
        let mut font_id = None;
        cc.egui_ctx.style_mut(|style| {
            style.override_text_style = Some(egui::TextStyle::Monospace);
            font_id = Some(style.text_styles[&egui::TextStyle::Monospace].clone())
        });

        Termion {
            fd,
            buf: Vec::new(),
            command_history: Vec::new(),
            current_command: String::new(),
            cursor_pos: (0, 0),
            prev_cursor_pos: (0, 0),
            character_size: None,
        }
    }
}
fn get_char_size(cc: &egui::Context) -> (f32, f32) {
    let font_id = cc.style().text_styles[&egui::TextStyle::Monospace].clone();
    let (width, height) = cc.fonts(|fonts| {
        let layout = fonts.layout(
            "@".to_string(),
            font_id,
            egui::Color32::default(),
            f32::INFINITY,
        );
        (layout.mesh_bounds.width(), layout.mesh_bounds.height())
    });

    println!("Character dimentions are: {}, {}", width, height);

    return (width, height);
}

/// Parse ANSI escape codes and return a LayoutJob with colored text
fn parse_ansi_to_layout(text: &str, ctx: &egui::Context) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    use egui::{Color32, FontId, TextStyle};

    let font_id = ctx.style().text_styles[&TextStyle::Monospace].clone();
    let mut job = LayoutJob::default();
    job.wrap = egui::text::TextWrapping {
        max_width: f32::INFINITY,
        ..Default::default()
    };

    let mut current_color = Color32::WHITE;
    let mut bold = false;
    let mut chars = text.chars().peekable();
    let mut current_text = String::new();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Flush current text
            if !current_text.is_empty() {
                job.append(
                    &current_text,
                    0.0,
                    TextFormat {
                        font_id: font_id.clone(),
                        color: current_color,
                        ..Default::default()
                    },
                );
                current_text.clear();
            }

            // Parse escape sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                let mut seq = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_digit() || ch == ';' {
                        seq.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                // Consume the final character (m, H, J, K, etc.)
                if let Some(end_char) = chars.next() {
                    if end_char == 'm' {
                        // SGR (Select Graphic Rendition) - colors
                        for code in seq.split(';') {
                            match code.parse::<u8>() {
                                Ok(0) => {
                                    current_color = Color32::WHITE;
                                    bold = false;
                                }
                                Ok(1) => bold = true,
                                Ok(30) => current_color = if bold { Color32::DARK_GRAY } else { Color32::BLACK },
                                Ok(31) => current_color = if bold { Color32::RED } else { Color32::DARK_RED },
                                Ok(32) => current_color = if bold { Color32::GREEN } else { Color32::DARK_GREEN },
                                Ok(33) => current_color = if bold { Color32::YELLOW } else { Color32::from_rgb(128, 128, 0) },
                                Ok(34) => current_color = if bold { Color32::from_rgb(100, 149, 237) } else { Color32::BLUE },
                                Ok(35) => current_color = if bold { Color32::from_rgb(255, 0, 255) } else { Color32::from_rgb(128, 0, 128) },
                                Ok(36) => current_color = if bold { Color32::from_rgb(0, 255, 255) } else { Color32::from_rgb(0, 128, 128) },
                                Ok(37) => current_color = if bold { Color32::WHITE } else { Color32::LIGHT_GRAY },
                                Ok(39) => current_color = Color32::WHITE, // default fg
                                _ => {}
                            }
                        }
                    }
                    // Ignore other escape sequences (cursor movement, etc.)
                }
            }
        } else if c.is_ascii_graphic() || c.is_ascii_whitespace() {
            current_text.push(c);
        }
        // Skip other control characters
    }

    // Flush remaining text
    if !current_text.is_empty() {
        job.append(
            &current_text,
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color: current_color,
                ..Default::default()
            },
        );
    }

    job
}

fn char_to_cursor_offset(
    character_pos: &(usize, usize),
    character_size: &(f32, f32),
    content: &[u8],
) -> (f32, f32) {
    let content_by_lines: Vec<&[u8]> = content.split(|b| *b == b'\n').collect();
    let num_lines = content_by_lines.len();
    // let last_line = content_by_lines.last().unwrap_or(&[0u8]);
    let x_offset = character_pos.0 as f32 * character_size.0;
    let y_offset = (character_pos.1 as i64 - num_lines as i64) as f32 * character_size.1;
    (x_offset, y_offset)
}

impl eframe::App for Termion {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.character_size.is_none() {
            self.character_size = Some(get_char_size(ctx));
            println!("self.character_size: {:?}", self.character_size);
        }

        let mut buf = vec![0u8; 4096];
        // println!(":");
        match nix::unistd::read(self.fd.as_raw_fd(), &mut buf) {
            Ok(0) => {
                println!("EOF reached");
                return;
            }
            Ok(read_size) => {
                let incoming = &buf[0..read_size];
                let mut i = 0;
                while i < incoming.len() {
                    let c = incoming[i];
                    match c {
                        b'\x1b' => {
                            // Start of escape sequence - add to buffer but don't move cursor
                            self.buf.push(c);
                            i += 1;
                            // Check for CSI sequence (ESC [)
                            if i < incoming.len() && incoming[i] == b'[' {
                                self.buf.push(incoming[i]);
                                i += 1;
                                // Skip until we find the terminating character (letter)
                                while i < incoming.len() {
                                    let seq_char = incoming[i];
                                    self.buf.push(seq_char);
                                    i += 1;
                                    if seq_char.is_ascii_alphabetic() {
                                        break;
                                    }
                                }
                            }
                        }
                        b'\x08' | b'\x7F' => {
                            // Backspace: move cursor back and remove character from buffer
                            if self.cursor_pos.0 > 0 {
                                self.cursor_pos.0 -= 1;
                            }
                            self.buf.pop();
                            i += 1;
                        }
                        b'\n' => {
                            self.cursor_pos = (0, 1 + self.cursor_pos.1);
                            self.buf.push(c);
                            i += 1;
                        }
                        b'\r' => {
                            // Carriage return: move cursor to start of line
                            self.cursor_pos.0 = 0;
                            self.buf.push(c);
                            i += 1;
                        }
                        _ if c.is_ascii_graphic() || c == b' ' || c == b'\t' => {
                            self.cursor_pos = (1 + self.cursor_pos.0, self.cursor_pos.1);
                            self.buf.push(c);
                            i += 1;
                        }
                        _ => {
                            // Skip other control characters
                            i += 1;
                        }
                    }
                }
            }
            Err(e) => {
                if e != Errno::EAGAIN {
                    println!("Read Failed due to: {}", e);
                    // exit(1); // Kill the emulator if there is error;
                } else {
                    // println!("-");
                }
            }
        }

        // Side panel remains the same...
        egui::SidePanel::right("history_panel")
            .min_width(100.0)
            .show(ctx, |ui| {
                ui.heading("Command History");
                ui.separator();
                for cmd in &self.command_history {
                    if ui.button(cmd).clicked() {
                        println!("Clicked:: {}", cmd);
                        self.current_command.clear();
                        let cmd_with_newline = format!("{}\n", cmd);
                        let bytes = cmd_with_newline.as_bytes();
                        let mut to_write: &[u8] = &bytes;
                        while to_write.len() > 0 {
                            match nix::unistd::write(self.fd.as_fd(), to_write) {
                                Ok(written) => to_write = &to_write[written..],
                                Err(e) => {
                                    println!("Failed to write command to terminal: {}", e);
                                    break;
                                }
                            }
                        }
                        println!("Executed command from sidepanel: {}", cmd);
                    }
                }
            });

        // Convert buffer to string, keeping escape sequences for color parsing
        let raw_output = String::from_utf8_lossy(&self.buf);
        // Remove bracketed paste mode sequences
        let cleaned_output = raw_output
            .replace("\x1b[?2004h", "")
            .replace("\x1b[?2004l", "");

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::both()
                .auto_shrink([false; 2]) // Prevent shrinking; ensures resizing works
                .stick_to_bottom(true) // For large commands, helps keep ip part in focus
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                .show(ui, |ui| {
                    ui.input(|input_state| {
                        for event in &input_state.events {
                            let text = match event {
                                egui::Event::Text(text) => {
                                    self.current_command.push_str(text);
                                    text
                                }
                                egui::Event::Key { key, pressed, .. } => match key {
                                    egui::Key::Enter => {
                                        if *pressed {
                                            if !self.current_command.trim().is_empty() {
                                                self.command_history.push(self.current_command.clone());
                                            }
                                            self.current_command.clear();
                                            "\n"
                                        } else {
                                            ""
                                        }
                                    }
                                    egui::Key::Backspace => {
                                        if *pressed && !self.current_command.is_empty() {
                                            self.current_command.pop();
                                            let _ = nix::unistd::write(self.fd.as_fd(), b"\x7F");
                                        }
                                        ""
                                    }
                                    _ => "",
                                },
                                _ => "",
                            };

                            // let temp_text = &text.replace("[?2004h", "").replace("[?2004l", "");
                            let temp_text = &text;
                            let bytes = temp_text.as_bytes();

                            let mut to_write: &[u8] = &bytes;
                            while !to_write.is_empty() {
                                match nix::unistd::write(self.fd.as_fd(), to_write) {
                                    Ok(written) => to_write = &to_write[written..],
                                    Err(e) => {
                                        if e != Errno::EPIPE {
                                            println!("Write error: {}", e);
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    let layout_job = parse_ansi_to_layout(&cleaned_output, ctx);
                    let response = ui.add(egui::Label::new(layout_job).wrap_mode(egui::TextWrapMode::Extend));

                    let left = response.rect.left();
                    let bottom = response.rect.bottom();

                    let painter = ui.painter();
                    let character_size = self.character_size.as_ref().unwrap();
                    let (x_offset, y_offset) =
                        char_to_cursor_offset(&self.cursor_pos, character_size, &self.buf);

                    let cursor_rect = egui::Rect::from_min_size(
                        egui::pos2(left + x_offset, bottom + y_offset),
                        egui::vec2(character_size.0, character_size.1),
                    );
                    painter.rect_filled(cursor_rect, 0.0, egui::Color32::GREEN);

                    // Auto-scroll to keep cursor visible only when cursor moves
                    if self.cursor_pos != self.prev_cursor_pos {
                        ui.scroll_to_rect(cursor_rect, Some(egui::Align::Max));
                        self.prev_cursor_pos = self.cursor_pos;
                    }
                    ctx.request_repaint(); // Explicitly request a repaint
                });
        });
    }
}
