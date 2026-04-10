use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// A real terminal session backed by a PTY + vt100 parser.
#[allow(dead_code)]
pub struct ShellSession {
    pub id: usize,
    pub title: String,
    /// The vt100 screen parser — holds the full terminal state
    pub parser: vt100::Parser,
    /// PTY master — we read output from here and write input to here
    master_writer: Option<Box<dyn Write + Send>>,
    master_reader_thread: Option<std::thread::JoinHandle<()>>,
    /// Buffer of raw bytes received from the PTY, fed into parser on drain
    incoming: Arc<Mutex<Vec<u8>>>,
    /// Whether the child process is still alive
    pub alive: bool,
    /// Scroll offset (lines scrolled up from bottom of scrollback)
    pub scroll_offset: usize,
    /// Current PTY size
    pub pty_cols: u16,
    pub pty_rows: u16,
    /// Master PTY handle (kept alive for resize)
    master_pty: Option<Box<dyn MasterPty + Send>>,
}

impl ShellSession {
    pub fn new(id: usize, cols: u16, rows: u16) -> Self {
        let parser = vt100::Parser::new(rows, cols, 1000); // 1000 lines scrollback

        let mut session = Self {
            id,
            title: format!("Shell {}", id),
            parser,
            master_writer: None,
            master_reader_thread: None,
            incoming: Arc::new(Mutex::new(Vec::new())),
            alive: false,
            scroll_offset: 0,
            pty_cols: cols,
            pty_rows: rows,
            master_pty: None,
        };

        session.spawn_shell();
        session
    }

    fn spawn_shell(&mut self) {
        let pty_system = native_pty_system();

        let pair = match pty_system.openpty(PtySize {
            rows: self.pty_rows,
            cols: self.pty_cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(e) => {
                self.parser.process(format!("Failed to open PTY: {}\r\n", e).as_bytes());
                return;
            }
        };

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        // Start login shell
        cmd.arg("-l");

        if let Err(e) = pair.slave.spawn_command(cmd) {
            self.parser.process(format!("Failed to spawn shell: {}\r\n", e).as_bytes());
            return;
        }

        // Get writer to send input
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                self.parser.process(format!("Failed to get PTY writer: {}\r\n", e).as_bytes());
                return;
            }
        };

        // Get reader for output
        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                self.parser.process(format!("Failed to get PTY reader: {}\r\n", e).as_bytes());
                return;
            }
        };

        self.master_writer = Some(writer);
        self.master_pty = Some(pair.master);
        self.alive = true;

        // Spawn reader thread
        let incoming = Arc::clone(&self.incoming);
        self.master_reader_thread = Some(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut inc) = incoming.lock() {
                            inc.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        }));
    }

    /// Send raw bytes to the PTY (keyboard input).
    pub fn send_bytes(&mut self, data: &[u8]) {
        if let Some(ref mut writer) = self.master_writer {
            let _ = writer.write_all(data);
            let _ = writer.flush();
        }
    }

    /// Send a string to the PTY.
    pub fn send_text(&mut self, text: &str) {
        self.send_bytes(text.as_bytes());
    }

    /// Drain incoming bytes from the reader thread into the vt100 parser.
    /// Returns true if any bytes were processed.
    pub fn drain(&mut self) -> bool {
        let data = {
            let Ok(mut inc) = self.incoming.lock() else {
                return false;
            };
            if inc.is_empty() {
                return false;
            }
            std::mem::take(&mut *inc)
        };

        self.parser.process(&data);

        // Auto-scroll to bottom on new output
        if self.scroll_offset > 0 {
            // Keep scroll position — user is reading history
        }

        true
    }

    /// Resize the PTY and parser.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.pty_cols && rows == self.pty_rows {
            return;
        }
        self.pty_cols = cols;
        self.pty_rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
        if let Some(ref master) = self.master_pty {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Get the current screen contents for rendering.
    /// Returns rows of cells from the vt100 screen.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        // Drop the writer first to signal EOF
        self.master_writer = None;
        self.master_pty = None;
    }
}
