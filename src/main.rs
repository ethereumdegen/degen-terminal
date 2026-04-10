mod shell;
mod theme;

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui_hypertile::{EventOutcome, HypertileEvent, KeyChord, KeyCode as HtKeyCode};
use ratatui_hypertile_extras::{
    AnimationConfig, HypertilePlugin, HypertileRuntime, InputMode, ModeIndicator, SplitBehavior,
    WorkspaceRuntime, event_from_crossterm,
};

/// File-based trace log for diagnosing freezes.
fn trace_log(msg: &str) {
    use std::fs::OpenOptions;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/degen-terminal-trace.log")
    {
        use std::time::SystemTime;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs() % 86400;
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        let ms = now.subsec_millis();
        let _ = writeln!(f, "[{:02}:{:02}:{:02}.{:03}] {}", h, m, s, ms, msg);
    }
}

/// Copy text to the system clipboard.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let text = text.to_owned();
    std::thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        #[cfg(target_os = "linux")]
        {
            use arboard::SetExtLinux;
            let _ = clipboard.set().wait().text(text);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = clipboard.set_text(text);
        }
    });
    Ok(())
}

// ── Shared state ──

/// Mouse text selection state
#[derive(Default, Clone)]
struct TextSelection {
    active: bool,
    start: (u16, u16),
    end: (u16, u16),
    selected_text: String,
    pane_rect: Option<Rect>,
}

impl TextSelection {
    fn has_selection(&self) -> bool {
        self.active || !self.selected_text.is_empty()
    }

    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.start.1 < self.end.1
            || (self.start.1 == self.end.1 && self.start.0 <= self.end.0)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

struct SharedState {
    debug_log: Vec<(Instant, String)>,
    selection: TextSelection,
}

pub fn debug_log(msg: impl Into<String>) {
    let msg = msg.into();
    trace_log(&msg);
    if let Ok(mut state) = shared().try_lock() {
        state.debug_log.push((Instant::now(), msg));
        let len = state.debug_log.len();
        if len > 500 {
            state.debug_log.drain(..len - 500);
        }
    }
}

static SHARED: OnceLock<Mutex<SharedState>> = OnceLock::new();
static INPUT_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static NEXT_SESSION_ID: AtomicUsize = AtomicUsize::new(1);
static SESSIONS: OnceLock<Mutex<HashMap<usize, Arc<Mutex<shell::ShellSession>>>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<usize, Arc<Mutex<shell::ShellSession>>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shared() -> &'static Mutex<SharedState> {
    SHARED.get_or_init(|| {
        Mutex::new(SharedState {
            debug_log: vec![(Instant::now(), "Degen Terminal started".into())],
            selection: TextSelection::default(),
        })
    })
}

fn make_tile_block(
    title: impl Into<String>,
    title_color: Color,
    is_focused: bool,
) -> Block<'static> {
    let title = title.into();
    if is_focused {
        Block::default()
            .borders(Borders::ALL)
            .border_set(border::THICK)
            .border_style(
                Style::default()
                    .fg(theme::BORDER_FOCUSED())
                    .add_modifier(Modifier::BOLD),
            )
            .title(title)
            .title_style(
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().bg(theme::bg_primary()))
    } else {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_NORMAL()))
            .title(title)
            .title_style(Style::default().fg(theme::text_secondary()))
            .style(Style::default().bg(theme::bg_primary()))
    }
}

fn create_session() -> usize {
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    // Default size, will be resized on first render
    let session = shell::ShellSession::new(id, 80, 24);
    sessions()
        .lock()
        .unwrap()
        .insert(id, Arc::new(Mutex::new(session)));
    id
}

// ── Convert vt100 colors to ratatui colors ──

fn vt100_color_to_ratatui(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

// ── Shell Plugin ──

struct ShellPlugin {
    session_id: usize,
    /// Track inner area size for PTY resize (written in render, read in tick)
    last_cols: std::sync::atomic::AtomicU16,
    last_rows: std::sync::atomic::AtomicU16,
}

impl ShellPlugin {
    fn new() -> Self {
        let id = create_session();
        Self {
            session_id: id,
            last_cols: std::sync::atomic::AtomicU16::new(0),
            last_rows: std::sync::atomic::AtomicU16::new(0),
        }
    }
}

impl HypertilePlugin for ShellPlugin {
    fn render(&self, area: Rect, buf: &mut Buffer, is_focused: bool) {
        let session_arc = {
            let Ok(sess) = sessions().try_lock() else {
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_NORMAL()))
                    .title(format!(" Shell {} ", self.session_id))
                    .style(Style::default().bg(theme::bg_primary()))
                    .render(area, buf);
                return;
            };
            let Some(arc) = sess.get(&self.session_id) else {
                Paragraph::new("Session not found").render(area, buf);
                return;
            };
            Arc::clone(arc)
        };

        let in_input_mode = is_focused && INPUT_MODE_ACTIVE.load(Ordering::Relaxed);

        let Ok(session) = session_arc.try_lock() else {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::BORDER_NORMAL()))
                .title(format!(" Shell {} ", self.session_id))
                .style(Style::default().bg(theme::bg_primary()))
                .render(area, buf);
            return;
        };

        // Build title
        let title_text = session.title.clone();
        let scroll_indicator = if session.scroll_offset > 0 {
            format!(" [+{}]", session.scroll_offset)
        } else {
            String::new()
        };
        let title = format!(" {}{} ", title_text, scroll_indicator);

        let (border_color, tile_bg) = if in_input_mode {
            (theme::GREEN(), theme::bg_primary())
        } else if is_focused {
            (theme::BORDER_FOCUSED(), theme::bg_primary())
        } else {
            (theme::BORDER_NORMAL(), theme::bg_primary())
        };

        let block = if is_focused {
            Block::default()
                .borders(Borders::ALL)
                .border_set(border::THICK)
                .border_style(
                    Style::default()
                        .fg(border_color)
                        .add_modifier(Modifier::BOLD),
                )
                .title(title)
                .title_style(Style::default().fg(if in_input_mode {
                    theme::GREEN()
                } else {
                    theme::CYAN()
                }))
                .style(Style::default().bg(tile_bg))
        } else {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title)
                .title_style(Style::default().fg(theme::text_secondary()))
                .style(Style::default().bg(tile_bg))
        };

        let inner = block.inner(area);
        block.render(area, buf);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Track size for PTY resize on next tick
        self.last_cols.store(inner.width, Ordering::Relaxed);
        self.last_rows.store(inner.height, Ordering::Relaxed);

        // Render the vt100 screen directly into the ratatui buffer
        let screen = session.screen();

        for row in 0..inner.height {
            for col in 0..inner.width {
                let buf_x = inner.x + col;
                let buf_y = inner.y + row;

                let cell = screen.cell(row, col);

                if let Some(cell) = cell {
                    let ch = cell.contents();
                    let ch = if ch.is_empty() { " " } else { &ch };

                    // Build style from vt100 cell attributes
                    let mut style = Style::default();

                    // Foreground
                    if let Some(fg) = vt100_color_to_ratatui(cell.fgcolor()) {
                        style = style.fg(fg);
                    } else {
                        style = style.fg(theme::text_primary());
                    }

                    // Background
                    if let Some(bg) = vt100_color_to_ratatui(cell.bgcolor()) {
                        style = style.bg(bg);
                    } else {
                        style = style.bg(tile_bg);
                    }

                    // Attributes
                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        // Swap fg/bg
                        let fg = style.fg.unwrap_or(Color::White);
                        let bg = style.bg.unwrap_or(Color::Black);
                        style = style.fg(bg).bg(fg);
                    }

                    if let Some(buf_cell) = buf.cell_mut((buf_x, buf_y)) {
                        buf_cell.set_symbol(ch);
                        buf_cell.set_style(style);
                    }
                } else {
                    // Empty cell
                    if let Some(buf_cell) = buf.cell_mut((buf_x, buf_y)) {
                        buf_cell.set_symbol(" ");
                        buf_cell.set_style(Style::default().bg(tile_bg));
                    }
                }
            }
        }

        // Draw cursor if focused and not scrolled
        if is_focused && in_input_mode && session.scroll_offset == 0 {
            let cursor_pos = screen.cursor_position();
            let cx = inner.x + cursor_pos.1;
            let cy = inner.y + cursor_pos.0;
            if cx < inner.x + inner.width && cy < inner.y + inner.height {
                if let Some(cell) = buf.cell_mut((cx, cy)) {
                    // Invert the cursor cell
                    let fg = cell.fg;
                    let bg = cell.bg;
                    cell.set_fg(bg);
                    cell.set_bg(if fg == Color::Reset { theme::text_primary() } else { fg });
                }
            }
        }
    }

    fn on_event(&mut self, event: &HypertileEvent) -> EventOutcome {
        // Tick: drain PTY output
        if matches!(event, HypertileEvent::Tick) {
            let session_arc = {
                let Ok(sess) = sessions().try_lock() else {
                    return EventOutcome::Ignored;
                };
                let Some(arc) = sess.get(&self.session_id) else {
                    return EventOutcome::Ignored;
                };
                Arc::clone(arc)
            };

            let Ok(mut session) = session_arc.try_lock() else {
                return EventOutcome::Ignored;
            };

            // Resize PTY if needed
            let new_cols = self.last_cols.load(Ordering::Relaxed);
            let new_rows = self.last_rows.load(Ordering::Relaxed);
            if new_cols > 0 && new_rows > 0 {
                session.resize(new_cols, new_rows);
            }

            let drained = session.drain();
            if drained {
                return EventOutcome::Consumed;
            }
            return EventOutcome::Ignored;
        }

        let HypertileEvent::Key(key) = event else {
            return EventOutcome::Ignored;
        };

        let session_arc = {
            let Ok(sess) = sessions().try_lock() else {
                return EventOutcome::Ignored;
            };
            let Some(arc) = sess.get(&self.session_id) else {
                return EventOutcome::Ignored;
            };
            Arc::clone(arc)
        };

        let Ok(mut session) = session_arc.try_lock() else {
            return EventOutcome::Ignored;
        };

        // Shift+PageUp/PageDown for scrollback
        if key.modifiers.contains(ratatui_hypertile::Modifiers::SHIFT) {
            match key.code {
                HtKeyCode::PageUp => {
                    let half = session.pty_rows as usize / 2;
                    session.scroll_up(half);
                    return EventOutcome::Consumed;
                }
                HtKeyCode::PageDown => {
                    let half = session.pty_rows as usize / 2;
                    session.scroll_down(half);
                    return EventOutcome::Consumed;
                }
                HtKeyCode::Up => {
                    session.scroll_up(1);
                    return EventOutcome::Consumed;
                }
                HtKeyCode::Down => {
                    session.scroll_down(1);
                    return EventOutcome::Consumed;
                }
                _ => {}
            }
        }

        // Scroll to bottom on any non-shift keypress
        if session.scroll_offset > 0
            && !key.modifiers.contains(ratatui_hypertile::Modifiers::SHIFT)
        {
            session.scroll_to_bottom();
        }

        // Convert key event to terminal escape sequence and send to PTY
        let has_ctrl = key.modifiers.contains(ratatui_hypertile::Modifiers::CTRL);
        let has_alt = key.modifiers.contains(ratatui_hypertile::Modifiers::ALT);

        let bytes: Option<Vec<u8>> = match key.code {
            HtKeyCode::Char(c) => {
                if has_ctrl {
                    // Ctrl+letter -> control character
                    let ctrl_byte = match c {
                        'a'..='z' => Some(c as u8 - b'a' + 1),
                        '@' => Some(0),
                        '[' => Some(27),
                        '\\' => Some(28),
                        ']' => Some(29),
                        '^' => Some(30),
                        '_' => Some(31),
                        _ => None,
                    };
                    ctrl_byte.map(|b| vec![b])
                } else if has_alt {
                    let mut s = String::new();
                    s.push(c);
                    let mut bytes = vec![0x1b]; // ESC prefix for Alt
                    bytes.extend_from_slice(s.as_bytes());
                    Some(bytes)
                } else {
                    let mut s = String::new();
                    s.push(c);
                    Some(s.into_bytes())
                }
            }
            HtKeyCode::Enter => Some(b"\r".to_vec()),
            HtKeyCode::Backspace => Some(b"\x7f".to_vec()),
            HtKeyCode::Tab => Some(b"\t".to_vec()),
            HtKeyCode::Escape => Some(b"\x1b".to_vec()),
            HtKeyCode::Up => Some(b"\x1b[A".to_vec()),
            HtKeyCode::Down => Some(b"\x1b[B".to_vec()),
            HtKeyCode::Right => Some(b"\x1b[C".to_vec()),
            HtKeyCode::Left => Some(b"\x1b[D".to_vec()),
            HtKeyCode::Home => Some(b"\x1b[H".to_vec()),
            HtKeyCode::End => Some(b"\x1b[F".to_vec()),
            HtKeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
            HtKeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
            HtKeyCode::Insert => Some(b"\x1b[2~".to_vec()),
            HtKeyCode::Delete => Some(b"\x1b[3~".to_vec()),
            HtKeyCode::F(n) => {
                let seq = match n {
                    1 => "\x1bOP",
                    2 => "\x1bOQ",
                    3 => "\x1bOR",
                    4 => "\x1bOS",
                    5 => "\x1b[15~",
                    6 => "\x1b[17~",
                    7 => "\x1b[18~",
                    8 => "\x1b[19~",
                    9 => "\x1b[20~",
                    10 => "\x1b[21~",
                    11 => "\x1b[23~",
                    12 => "\x1b[24~",
                    _ => "",
                };
                if seq.is_empty() {
                    None
                } else {
                    Some(seq.as_bytes().to_vec())
                }
            }
            _ => None,
        };

        if let Some(data) = bytes {
            session.send_bytes(&data);
            EventOutcome::Consumed
        } else {
            EventOutcome::Ignored
        }
    }
}

// ── Debug Log Plugin ──

#[derive(Clone, Copy, PartialEq)]
enum LogCategory {
    Error,
    Session,
    Debug,
    Other,
}

impl LogCategory {
    fn classify(msg: &str) -> Self {
        if msg.contains("[error]") {
            LogCategory::Error
        } else if msg.contains("[session]") {
            LogCategory::Session
        } else if msg.contains("[debug]") {
            LogCategory::Debug
        } else {
            LogCategory::Other
        }
    }

    fn badge(self) -> &'static str {
        match self {
            LogCategory::Error => "ERR",
            LogCategory::Session => "SES",
            LogCategory::Debug => "DBG",
            LogCategory::Other => "---",
        }
    }

    fn color(self) -> Color {
        match self {
            LogCategory::Error => theme::RED(),
            LogCategory::Session => theme::CYAN(),
            LogCategory::Debug => theme::ORANGE(),
            LogCategory::Other => theme::text_secondary(),
        }
    }

    fn index(self) -> usize {
        match self {
            LogCategory::Error => 0,
            LogCategory::Session => 1,
            LogCategory::Debug => 2,
            LogCategory::Other => 3,
        }
    }
}

struct DebugPlugin {
    scroll_offset: usize,
    auto_scroll: bool,
    filter_categories: [bool; 4],
    search_active: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    wrap_enabled: bool,
    pending_g: bool,
    show_help: bool,
}

impl DebugPlugin {
    fn new() -> Self {
        debug_log("[debug] Debug panel opened");
        Self {
            scroll_offset: 0,
            auto_scroll: true,
            filter_categories: [true; 4],
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            wrap_enabled: true,
            pending_g: false,
            show_help: false,
        }
    }

    fn filtered_indices(&self, debug_log: &[(Instant, String)]) -> Vec<usize> {
        let query_lower = self.search_query.to_lowercase();
        debug_log
            .iter()
            .enumerate()
            .filter(|(_, (_, msg))| {
                let cat = LogCategory::classify(msg);
                if !self.filter_categories[cat.index()] {
                    return false;
                }
                if self.search_active && !self.search_query.is_empty() {
                    return msg.to_lowercase().contains(&query_lower);
                }
                true
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn update_search_matches(&mut self, debug_log: &[(Instant, String)]) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_cursor = 0;
            return;
        }
        let query_lower = self.search_query.to_lowercase();
        self.search_matches = debug_log
            .iter()
            .enumerate()
            .filter(|(_, (_, msg))| msg.to_lowercase().contains(&query_lower))
            .map(|(i, _)| i)
            .collect();
        if self.search_cursor >= self.search_matches.len() {
            self.search_cursor = 0;
        }
    }
}

impl HypertilePlugin for DebugPlugin {
    fn render(&self, area: Rect, buf: &mut Buffer, is_focused: bool) {
        let Ok(state) = shared().try_lock() else {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::BORDER_NORMAL()))
                .title("  Debug Log  ")
                .style(Style::default().bg(theme::bg_primary()))
                .render(area, buf);
            return;
        };
        let start = Instant::now();

        let filtered = self.filtered_indices(&state.debug_log);
        let total = state.debug_log.len();
        let shown = filtered.len();

        let mut title_parts = format!("  Debug Log ({}/{})  ", shown, total);
        if self.search_active {
            title_parts.push_str(&format!("[/{}] ", self.search_query));
        }
        if self.auto_scroll {
            title_parts.push_str("[tail] ");
        }

        let block = make_tile_block(title_parts, theme::ORANGE(), is_focused);
        let inner = block.inner(area);
        block.render(area, buf);

        let help_height = if is_focused { 1u16 } else { 0u16 };
        let content_height = inner.height.saturating_sub(help_height) as usize;
        let app_start = state.debug_log.first().map(|(t, _)| *t).unwrap_or(start);
        let scroll = if self.auto_scroll { 0 } else { self.scroll_offset };
        let query_lower = self.search_query.to_lowercase();

        let lines: Vec<Line> = filtered
            .iter()
            .rev()
            .skip(scroll)
            .take(content_height)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|&idx| {
                let (timestamp, msg) = &state.debug_log[idx];
                let elapsed = timestamp.duration_since(app_start);
                let secs = elapsed.as_secs();
                let ms = elapsed.subsec_millis();
                let time_str = format!("{:>4}.{:03}", secs, ms);
                let cat = LogCategory::classify(msg);
                let msg_color = cat.color();

                let mut spans = vec![
                    Span::styled(format!(" {} ", time_str), Style::default().fg(theme::text_muted())),
                    Span::styled(format!("{} ", cat.badge()), Style::default().fg(Color::Black).bg(cat.color())),
                ];

                if self.search_active && !self.search_query.is_empty() {
                    let msg_lower = msg.to_lowercase();
                    let mut pos = 0;
                    while pos < msg.len() {
                        if let Some(found) = msg_lower[pos..].find(&query_lower) {
                            let abs = pos + found;
                            if abs > pos {
                                spans.push(Span::styled(&msg[pos..abs], Style::default().fg(msg_color)));
                            }
                            spans.push(Span::styled(
                                &msg[abs..abs + query_lower.len()],
                                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
                            ));
                            pos = abs + query_lower.len();
                        } else {
                            spans.push(Span::styled(&msg[pos..], Style::default().fg(msg_color)));
                            break;
                        }
                    }
                    if pos >= msg.len() && pos == 0 {
                        spans.push(Span::styled(msg.as_str(), Style::default().fg(msg_color)));
                    }
                } else {
                    spans.push(Span::styled(msg.as_str(), Style::default().fg(msg_color)));
                }

                Line::from(spans)
            })
            .collect();

        let content_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: content_height as u16 };
        let mut para = Paragraph::new(lines).style(Style::default().bg(theme::bg_primary()));
        if self.wrap_enabled { para = para.wrap(Wrap { trim: false }); }
        para.render(content_area, buf);

        if is_focused && inner.height > 1 {
            let help_area = Rect { x: inner.x, y: inner.y + inner.height - 1, width: inner.width, height: 1 };
            let help_text = if self.search_active {
                "type to search | Enter/Esc:exit | n/N:next/prev"
            } else if self.show_help {
                "j/k:scroll G:bottom gg:top /:search F:tail w:wrap ?:help"
            } else {
                "j/k:scroll /:search F:tail ?:help"
            };
            Paragraph::new(Line::from(Span::styled(help_text, Style::default().fg(theme::text_muted()).add_modifier(Modifier::DIM))))
                .style(Style::default().bg(theme::bg_secondary()))
                .render(help_area, buf);
        }
    }

    fn on_event(&mut self, event: &HypertileEvent) -> EventOutcome {
        let HypertileEvent::Key(key) = event else { return EventOutcome::Ignored; };

        if self.search_active {
            match key.code {
                HtKeyCode::Escape | HtKeyCode::Enter => { self.search_active = false; return EventOutcome::Consumed; }
                HtKeyCode::Backspace => {
                    self.search_query.pop();
                    let log = shared().lock().unwrap();
                    let dl = log.debug_log.clone();
                    drop(log);
                    self.update_search_matches(&dl);
                    return EventOutcome::Consumed;
                }
                HtKeyCode::Char(c) => {
                    self.search_query.push(c);
                    let log = shared().lock().unwrap();
                    let dl = log.debug_log.clone();
                    drop(log);
                    self.update_search_matches(&dl);
                    return EventOutcome::Consumed;
                }
                _ => return EventOutcome::Consumed,
            }
        }

        if self.pending_g {
            self.pending_g = false;
            if key.code == HtKeyCode::Char('g') {
                let state = shared().lock().unwrap();
                let filtered = self.filtered_indices(&state.debug_log);
                drop(state);
                self.scroll_offset = filtered.len().saturating_sub(1);
                self.auto_scroll = false;
                return EventOutcome::Consumed;
            }
        }

        match key.code {
            HtKeyCode::Char('j') => { if self.scroll_offset > 0 { self.scroll_offset -= 1; } self.auto_scroll = false; EventOutcome::Consumed }
            HtKeyCode::Char('k') => { self.scroll_offset += 1; self.auto_scroll = false; EventOutcome::Consumed }
            HtKeyCode::Char('G') => { self.scroll_offset = 0; self.auto_scroll = true; EventOutcome::Consumed }
            HtKeyCode::Char('g') => { self.pending_g = true; EventOutcome::Consumed }
            HtKeyCode::Char('/') => { self.search_active = true; self.search_query.clear(); EventOutcome::Consumed }
            HtKeyCode::Char('n') => {
                if !self.search_matches.is_empty() {
                    self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
                    let state = shared().lock().unwrap();
                    let filtered = self.filtered_indices(&state.debug_log);
                    drop(state);
                    let target = self.search_matches[self.search_cursor];
                    if let Some(pos) = filtered.iter().position(|&i| i == target) {
                        self.scroll_offset = filtered.len().saturating_sub(1) - pos;
                        self.auto_scroll = false;
                    }
                }
                EventOutcome::Consumed
            }
            HtKeyCode::Char('F') => { self.auto_scroll = !self.auto_scroll; if self.auto_scroll { self.scroll_offset = 0; } EventOutcome::Consumed }
            HtKeyCode::Char('w') => { self.wrap_enabled = !self.wrap_enabled; EventOutcome::Consumed }
            HtKeyCode::Char('?') => { self.show_help = !self.show_help; EventOutcome::Consumed }
            _ => EventOutcome::Ignored,
        }
    }
}

// ── Theme Selector Plugin ──

struct ThemeMenuPlugin {
    selected: usize,
}

impl ThemeMenuPlugin {
    fn new() -> Self {
        Self { selected: theme::active_index() }
    }
}

impl HypertilePlugin for ThemeMenuPlugin {
    fn render(&self, area: Rect, buf: &mut Buffer, is_focused: bool) {
        let t = theme::active();
        let themes = theme::all_themes();
        let current = theme::active_index();

        let title = format!("  Themes ({})  ", themes.len());
        let block = make_tile_block(title, t.magenta, is_focused);
        let inner = block.inner(area);
        block.render(area, buf);

        let visible_height = inner.height as usize;
        let mut lines: Vec<Line> = Vec::new();

        lines.push(Line::from(vec![
            Span::styled("     Theme Name              ", Style::default().fg(t.text_primary).add_modifier(Modifier::BOLD)),
            Span::styled("Preview", Style::default().fg(t.text_primary).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(Span::styled(
            " -".to_string() + &"-".repeat(inner.width.saturating_sub(3) as usize),
            Style::default().fg(t.border_normal),
        )));

        let list_height = visible_height.saturating_sub(5);
        let scroll = if self.selected >= list_height { self.selected - list_height + 1 } else { 0 };

        for (idx, theme_entry) in themes.iter().enumerate().skip(scroll).take(list_height) {
            let is_selected = is_focused && idx == self.selected;
            let is_active = idx == current;
            let row_bg = if is_selected { t.bg_secondary } else { t.bg_primary };
            let indicator = if is_selected && is_active { " >*" } else if is_selected { " > " } else if is_active { "  *" } else { "   " };
            let indicator_color = if is_active { t.green } else { t.cyan };

            lines.push(Line::from(vec![
                Span::styled(indicator, Style::default().fg(indicator_color).bg(row_bg)),
                Span::styled(format!("{:<26}", theme_entry.name), Style::default().fg(if is_active { t.green } else { t.text_primary }).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.blue).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.green).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.red).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.yellow).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.cyan).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.magenta).bg(row_bg)),
                Span::styled("##", Style::default().fg(theme_entry.orange).bg(row_bg)),
            ]));
        }

        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("  j/k", Style::default().fg(t.green).add_modifier(Modifier::BOLD)),
            Span::styled(":navigate  ", Style::default().fg(t.text_muted)),
            Span::styled("Enter", Style::default().fg(t.cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":apply  ", Style::default().fg(t.text_muted)),
            Span::styled("*", Style::default().fg(t.green)),
            Span::styled("=active", Style::default().fg(t.text_muted)),
        ]));

        Paragraph::new(lines).style(Style::default().bg(t.bg_primary)).render(inner, buf);
    }

    fn on_event(&mut self, event: &HypertileEvent) -> EventOutcome {
        let HypertileEvent::Key(key) = event else { return EventOutcome::Ignored; };
        let theme_count = theme::all_themes().len();
        match key.code {
            HtKeyCode::Char('j') => { if theme_count > 0 && self.selected < theme_count - 1 { self.selected += 1; } EventOutcome::Consumed }
            HtKeyCode::Char('k') => { if self.selected > 0 { self.selected -= 1; } EventOutcome::Consumed }
            HtKeyCode::Enter => {
                theme::set_active(self.selected);
                theme::save_current();
                debug_log(format!("[theme] Switched to: {} (saved)", theme::all_themes()[self.selected].name));
                EventOutcome::Consumed
            }
            _ => EventOutcome::Ignored,
        }
    }
}

// ── Main ──

fn build_runtime() -> HypertileRuntime {
    let mut rt = HypertileRuntime::builder()
        .with_split_behavior(SplitBehavior::DefaultPlugin)
        .with_default_split_plugin("shell")
        .with_animation_config(AnimationConfig {
            enabled: true,
            ..AnimationConfig::default()
        })
        .build();

    rt.register_plugin_type("shell", ShellPlugin::new);
    rt.register_plugin_type("debug", DebugPlugin::new);
    rt.register_plugin_type("themes", ThemeMenuPlugin::new);

    rt
}

fn restore_terminal() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
}

fn main() -> std::io::Result<()> {
    theme::load_saved();

    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;

    // Install panic hook so the terminal is always restored, even on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut workspace = WorkspaceRuntime::new(build_runtime);

    // Start with a single shell pane
    let rt = workspace.active_runtime_mut();
    let _ = rt.replace_focused_plugin("shell");

    let result = run(&mut terminal, &mut workspace);

    // Clean up all shell sessions before restoring terminal
    if let Ok(mut sess_map) = sessions().lock() {
        sess_map.clear();
    }

    restore_terminal();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    workspace: &mut WorkspaceRuntime,
) -> std::io::Result<()> {
    let tick_rate = Duration::from_millis(50); // fast for responsive terminal
    let mut last_tick = Instant::now();

    // Dedicated event reader thread
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(ev) => {
                if event_tx.send(ev).is_err() { break; }
            }
            Err(_) => break,
        }
    });

    loop {
        if QUIT.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mode = workspace.active_runtime().mode();
        INPUT_MODE_ACTIVE.store(mode == InputMode::PluginInput, Ordering::Relaxed);

        terminal.draw(|frame| {
            let [tabs, body, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(frame.area());

            render_tabs(workspace, tabs, frame.buffer_mut());


            workspace.render(body, frame.buffer_mut());

            // Footer
            let rt = workspace.active_runtime();
            let [mode_area, hint_area] =
                Layout::horizontal([Constraint::Length(10), Constraint::Min(0)]).areas(footer);
            ModeIndicator::new(rt.mode()).render(mode_area, frame.buffer_mut());

            Paragraph::new(Line::from(vec![
                Span::styled("  s/v", Style::default().fg(theme::GREEN()).add_modifier(Modifier::BOLD)),
                Span::styled(":split  ", Style::default().fg(theme::text_muted())),
                Span::styled("d", Style::default().fg(theme::RED()).add_modifier(Modifier::BOLD)),
                Span::styled(":close  ", Style::default().fg(theme::text_muted())),
                Span::styled("hjkl", Style::default().fg(theme::YELLOW()).add_modifier(Modifier::BOLD)),
                Span::styled(":nav  ", Style::default().fg(theme::text_muted())),
                Span::styled("i", Style::default().fg(theme::CYAN()).add_modifier(Modifier::BOLD)),
                Span::styled(":input  ", Style::default().fg(theme::text_muted())),
                Span::styled("t", Style::default().fg(theme::BLUE()).add_modifier(Modifier::BOLD)),
                Span::styled(":themes  ", Style::default().fg(theme::text_muted())),
                Span::styled("Ctrl+t/w", Style::default().fg(theme::ORANGE()).add_modifier(Modifier::BOLD)),
                Span::styled(":tab  ", Style::default().fg(theme::text_muted())),
                Span::styled("Ctrl+c", Style::default().fg(theme::RED()).add_modifier(Modifier::BOLD)),
                Span::styled(":quit  ", Style::default().fg(theme::text_muted())),
                Span::styled("Esc", Style::default().fg(theme::YELLOW()).add_modifier(Modifier::BOLD)),
                Span::styled(":layout", Style::default().fg(theme::text_muted())),
            ]))
            .style(Style::default().bg(theme::bg_panel()))
            .render(hint_area, frame.buffer_mut());

            // Selection highlight
            if let Ok(state) = shared().try_lock() {
                let sel = &state.selection;
                if sel.has_selection() {
                    let (start, end) = sel.ordered();
                    let sel_area = sel.pane_rect.unwrap_or(frame.area());
                    let mut selected_text = String::new();
                    let buf = frame.buffer_mut();
                    for row in start.1..=end.1 {
                        if row < sel_area.y || row >= sel_area.y + sel_area.height { continue; }
                        let col_start = if row == start.1 { start.0 } else { sel_area.x };
                        let col_end = if row == end.1 { end.0 } else { sel_area.x + sel_area.width - 1 };
                        let mut row_text = String::new();
                        for col in col_start..=col_end {
                            if col < sel_area.x || col >= sel_area.x + sel_area.width { continue; }
                            if let Some(cell) = buf.cell_mut((col, row)) {
                                cell.set_fg(Color::White);
                                cell.set_bg(theme::BLUE());
                                row_text.push_str(cell.symbol());
                            }
                        }
                        if !selected_text.is_empty() { selected_text.push('\n'); }
                        selected_text.push_str(row_text.trim_end());
                    }
                    drop(state);
                    if !selected_text.is_empty() {
                        if let Ok(mut state) = shared().try_lock() {
                            state.selection.selected_text = selected_text;
                        }
                    }
                }
            }
        })?;

        let timeout = workspace.next_frame_in().map_or_else(
            || tick_rate.saturating_sub(last_tick.elapsed()),
            |frame| frame.min(tick_rate.saturating_sub(last_tick.elapsed())),
        );

        let poll_deadline = Instant::now() + timeout;
        let mut maybe_event = None;
        while Instant::now() < poll_deadline {
            match event_rx.try_recv() {
                Ok(ev) => { maybe_event = Some(ev); break; }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
            if QUIT.load(Ordering::Relaxed) { return Ok(()); }
        }

        if let Some(ev) = maybe_event {
            match ev {
                Event::Key(key) => {
                    let is_ctrl_shift_c = key.modifiers.contains(KeyModifiers::CONTROL)
                        && (key.code == KeyCode::Char('C')
                            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::SHIFT)));
                    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL;

                    if is_ctrl_shift_c || is_ctrl_c {
                        let mut state = match shared().try_lock() {
                            Ok(s) => s,
                            Err(_) => {
                                if is_ctrl_c { QUIT.store(true, Ordering::Relaxed); return Ok(()); }
                                continue;
                            }
                        };
                        if !state.selection.selected_text.is_empty() {
                            let text = state.selection.selected_text.clone();
                            state.selection = TextSelection::default();
                            drop(state);
                            let _ = copy_to_clipboard(&text);
                            continue;
                        } else if is_ctrl_c {
                            drop(state);
                            let mode = workspace.active_runtime().mode();
                            if mode == InputMode::PluginInput {
                                // Forward Ctrl+C to shell (sends SIGINT via PTY)
                                if let Some(ev) = event_from_crossterm(key) {
                                    workspace.handle_event(ev);
                                }
                            } else {
                                QUIT.store(true, Ordering::Relaxed);
                                return Ok(());
                            }
                            continue;
                        } else {
                            continue;
                        }
                    }

                    // Ctrl+Shift+V = paste from clipboard
                    let is_ctrl_shift_v = key.modifiers.contains(KeyModifiers::CONTROL)
                        && (key.code == KeyCode::Char('V')
                            || (key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::SHIFT)));
                    if is_ctrl_shift_v {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            if let Ok(text) = clipboard.get_text() {
                                // Send paste text to the focused shell
                                let Ok(sess_map) = sessions().try_lock() else { continue; };
                                // Find the focused pane's session
                                // We'll send to all sessions as a simplification
                                // Actually, we should send char-by-char through hypertile
                                drop(sess_map);
                                for ch in text.chars() {
                                    workspace.handle_event(HypertileEvent::Key(KeyChord::new(HtKeyCode::Char(ch))));
                                }
                            }
                        }
                        continue;
                    }

                    // 't' in layout mode opens theme selector
                    if key.code == KeyCode::Char('t')
                        && key.modifiers == KeyModifiers::NONE
                        && workspace.active_runtime().mode() == InputMode::Layout
                    {
                        let rt = workspace.active_runtime_mut();
                        let _ = rt.split_focused(Direction::Vertical, "themes");
                        rt.set_mode(InputMode::PluginInput);
                    } else if let Some(ev) = event_from_crossterm(key) {
                        workspace.handle_event(ev);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        let rt = workspace.active_runtime_mut();
                        let panes = rt.panes();
                        let mut clicked_rect = None;
                        for pane in &panes {
                            if mouse.column >= pane.rect.x
                                && mouse.column < pane.rect.x + pane.rect.width
                                && mouse.row >= pane.rect.y
                                && mouse.row < pane.rect.y + pane.rect.height
                            {
                                clicked_rect = Some(pane.rect);
                                let _ = rt.focus_pane(pane.id);
                                rt.set_mode(InputMode::PluginInput);
                                break;
                            }
                        }
                        if let Ok(mut state) = shared().try_lock() {
                            state.selection = TextSelection {
                                active: true,
                                start: (mouse.column, mouse.row),
                                end: (mouse.column, mouse.row),
                                selected_text: String::new(),
                                pane_rect: clicked_rect,
                            };
                        }
                    }
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        if let Ok(mut state) = shared().try_lock() {
                            if state.selection.active {
                                let (col, row) = if let Some(r) = state.selection.pane_rect {
                                    (
                                        mouse.column.max(r.x).min(r.x + r.width.saturating_sub(1)),
                                        mouse.row.max(r.y).min(r.y + r.height.saturating_sub(1)),
                                    )
                                } else {
                                    (mouse.column, mouse.row)
                                };
                                state.selection.end = (col, row);
                            }
                        }
                    }
                    MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        if let Ok(mut state) = shared().try_lock() {
                            if state.selection.active {
                                state.selection.end = (mouse.column, mouse.row);
                                state.selection.active = false;
                                if state.selection.start == state.selection.end {
                                    state.selection.selected_text.clear();
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let prev_mode = workspace.active_runtime().mode();
                        workspace.active_runtime_mut().set_mode(InputMode::PluginInput);
                        let key = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                            HtKeyCode::PageUp
                        } else {
                            HtKeyCode::PageDown
                        };
                        let mut chord = KeyChord::new(key);
                        chord.modifiers = ratatui_hypertile::Modifiers::SHIFT;
                        workspace.handle_event(HypertileEvent::Key(chord));
                        workspace.active_runtime_mut().set_mode(prev_mode);
                    }
                    _ => {}
                },
                Event::Paste(text) => {
                    // Bracketed paste: send raw to the PTY
                    // We need to get the focused shell session and send directly
                    // For now, send char-by-char through the plugin system
                    for ch in text.chars() {
                        workspace.handle_event(HypertileEvent::Key(KeyChord::new(HtKeyCode::Char(ch))));
                    }
                }
                Event::FocusGained | Event::Resize(_, _) => {
                    terminal.clear()?;
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= tick_rate {
            workspace.handle_event(HypertileEvent::Tick);
            last_tick = Instant::now();
        }
    }
}

fn render_tabs(workspace: &WorkspaceRuntime, area: Rect, buf: &mut Buffer) {
    let spans: Vec<Span> = workspace
        .tab_labels()
        .enumerate()
        .flat_map(|(i, (label, active))| {
            let sep = if i > 0 { vec![Span::raw(" ")] } else { vec![] };
            let tab = if active {
                Span::styled(
                    format!(" {} ", label),
                    Style::default().fg(theme::bg_primary()).bg(theme::CYAN()).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {} ", label),
                    Style::default().fg(theme::text_secondary()).bg(theme::bg_panel()),
                )
            };
            sep.into_iter().chain(std::iter::once(tab))
        })
        .collect();
    Line::from(spans).render(area, buf);
}
