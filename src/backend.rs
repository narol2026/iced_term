use crate::actions::Action;
use crate::settings::BackendSettings;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, cell::{Cell, Flags}, test::TermSize, viewport_to_point, Term, TermMode,
};
use alacritty_terminal::vte::ansi::{self, Rgb};
use alacritty_terminal::{tty, Grid};
use iced::keyboard::Modifiers;
use iced_core::Size;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::{Index, RangeInclusive};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

macro_rules! term_log {
    ($id:expr, $($arg:tt)*) => {
        {
            let message = format!($($arg)*);
            eprintln!("[TERM-{} {:?}] {}", $id, Instant::now(), message);
            crate::diagnostics::log(&format!("TERM-{}", $id), message);
        }
    };
}

/// Regex used to auto-detect bare URLs in terminal text (fallback when a URL
/// is not wrapped in an OSC 8 hyperlink sequence).
const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#;

#[derive(Debug, Clone)]
pub enum Command {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Option<Size<f32>>, Option<Size<f32>>),
    SelectStart(SelectionType, (f32, f32)),
    SelectUpdate((f32, f32)),
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
    ProcessAlacrittyEvent(Event),
    ProcessAlacrittyEvents(Vec<Event>),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    pub layout_width: f32,
    pub layout_height: f32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_width: 80.0,
            layout_height: 50.0,
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub struct Backend {
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    notifier: Notifier,
    last_content: RenderableContent,
    pub(crate) url_regex: RegexSearch,
}

impl Backend {
    pub fn new(
        id: u64,
        pty_event_proxy_sender: mpsc::Sender<Event>,
        settings: BackendSettings,
        font_measure: Size<f32>,
    ) -> Result<Self> {
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.program.clone(), settings.args.clone())),
            working_directory: settings.working_directory,
            env: settings.env,
            ..tty::Options::default()
        };

        term_log!(id, "Backend::new: creating PTY with program={:?} args={:?}", settings.program, settings.args);

        let config = term::Config::default();
        let mut terminal_size = TerminalSize::default();
        if font_measure.width > 0.0 && font_measure.height > 0.0 {
            terminal_size.cell_width = font_measure.width as u16;
            terminal_size.cell_height = font_measure.height as u16;
        }
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;
        term_log!(id, "Backend::new: PTY created successfully");

        // Fix: alacritty_terminal disables ECHOE (erase echo) on the PTY
        // slave during initialisation.  Without ECHOE the line discipline
        // echoes the raw erase character (DEL / ^?) in caret notation
        // instead of the standard backspace-space-backspace sequence, causing
        // visual artifacts when backspace is pressed in cooked mode (e.g.
        // interactive sudo / openconnect prompts).
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = pty.file().as_raw_fd();
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut termios) } == 0 {
                let old_lflag = termios.c_lflag;
                termios.c_lflag |= libc::ECHOE | libc::ECHOK;
                if termios.c_lflag != old_lflag {
                    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
                    term_log!(id, "Backend::new: re-enabled ECHOE/ECHOK on PTY");
                }
            }
        }

        let event_proxy = EventProxy(pty_event_proxy_sender);

        let mut term = Term::new(config, &terminal_size, event_proxy.clone());

        let cursor = term.grid_mut().cursor_cell().clone();

        let initial_content = RenderableContent {
            visible_cells: visible_cells(term.grid()),
            history_size: term.grid().history_size(),
            total_lines: term.grid().total_lines(),
            screen_lines: term.grid().screen_lines(),
            display_offset: term.grid().display_offset(),
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: cursor.clone(),
            cursor_point: term.grid().cursor.point,
            hovered_hyperlink: None,
            osc_hyperlinks: Vec::new(),
            visible_hyperlinks: Vec::new(),
        };

        let term = Arc::new(FairMutex::new(term));

        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;

        let notifier = Notifier(pty_event_loop.channel());

        term_log!(id, "Backend::new: spawning event loop thread");
        let _ = pty_event_loop.spawn();
        term_log!(id, "Backend::new: event loop spawned, returning Backend");

        let mut backend = Self {
            term: term.clone(),
            size: terminal_size,
            notifier,
            last_content: initial_content,
            url_regex: RegexSearch::new(URL_REGEX).expect("invalid url regexp"),
        };
        // Compute visible hyperlinks for the initial content
        {
            let term = backend.term.lock();
            backend.last_content.visible_hyperlinks =
                visible_regex_match_iter(&term, &mut backend.url_regex.clone())
                    .collect();
        }
        Ok(backend)
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        let mut action = Action::default();
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            Command::ProcessAlacrittyEvent(event) => {
                action = self.process_alacritty_event(event);
            },
            Command::ProcessAlacrittyEvents(events) => {
                for event in events {
                    action = merge_actions(
                        action,
                        self.process_alacritty_event(event),
                    );
                }
            },
            Command::Write(input) => {
                self.write(input);
                term.scroll_display(Scroll::Bottom);
            },
            Command::Scroll(delta) => {
                self.scroll(&mut term, delta);
            },
            Command::Resize(layout_size, font_measure) => {
                self.resize(&mut term, layout_size, font_measure);
            },
            Command::SelectStart(selection_type, (x, y)) => {
                self.start_selection(&mut term, selection_type, x, y);
            },
            Command::SelectUpdate((x, y)) => {
                self.update_selection(&mut term, x, y);
            },
            Command::ProcessLink(link_action, point) => {
                self.process_link_action(&term, link_action, point);
            },
            Command::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
            },
        };

        action
    }

    fn process_alacritty_event(&self, event: Event) -> Action {
        match event {
            Event::Exit => {
                let message = "process_alacritty_event: received Event::Exit -> Shutdown";
                eprintln!("[TERM-BACKEND {:?}] {message}", Instant::now());
                crate::diagnostics::log("TERM-BACKEND", message);
                Action::Shutdown
            },
            Event::ChildExit(code) => {
                let message = format!("process_alacritty_event: received Event::ChildExit({code:?}) -> Shutdown");
                eprintln!("[TERM-BACKEND {:?}] {message}", Instant::now());
                crate::diagnostics::log("TERM-BACKEND", message);
                Action::Shutdown
            },
            Event::Title(title) => Action::ChangeTitle(title),
            Event::Osc(message) if !osc_reports_to_app(&message) => Action::Ignore,
            Event::Osc(message) => Action::Osc(message),
            Event::PtyOutput(_) => Action::Ignore,
            Event::PtyWrite(pty) => {
                self.notifier.notify(pty.into_bytes());
                Action::Ignore
            },
            _ => Action::Ignore,
        }
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                // Prioritize OSC 8 hyperlinks, fall back to regex-detected URLs.
                self.last_content.hovered_hyperlink = self
                    .last_content
                    .osc_hyperlinks
                    .iter()
                    .find(|m| m.range.contains(&point))
                    .cloned()
                    .or_else(|| {
                        self.regex_match_at(
                            terminal,
                            point,
                            &mut self.url_regex.clone(),
                        )
                    });
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
            },
            LinkAction::Open => {
                // Resolve the URL to open. Priority:
                // 1. hovered_hyperlink (already detected via Hover)
                // 2. osc_hyperlinks (OSC 8)
                // 3. visible_hyperlinks (regex-detected bare URLs)
                let url_to_open: Option<String> = self
                    .last_content
                    .hovered_hyperlink
                    .as_ref()
                    .and_then(|hm| {
                        hm.url.clone().or_else(|| {
                            // Regex match: extract URL text from grid cells.
                            let start = hm.range.start();
                            let end = hm.range.end();
                            let mut url =
                                String::from(terminal.grid().index(*start).c);
                            for indexed in terminal.grid().iter_from(*start) {
                                url.push(indexed.c);
                                if indexed.point == *end {
                                    break;
                                }
                            }
                            Some(url)
                        })
                    })
                    .or_else(|| {
                        // Fallback: OSC 8 hyperlinks.
                        self.last_content
                            .osc_hyperlinks
                            .iter()
                            .find(|m| m.range.contains(&point))
                            .and_then(|m| m.url.clone())
                    })
                    .or_else(|| {
                        // Last resort: regex-detected bare URLs.
                        self.last_content
                            .visible_hyperlinks
                            .iter()
                            .find(|r| r.contains(&point))
                            .map(|range| {
                                let start = range.start();
                                let end = range.end();
                                let mut url = String::from(
                                    terminal.grid().index(*start).c,
                                );
                                for indexed in
                                    terminal.grid().iter_from(*start)
                                {
                                    url.push(indexed.c);
                                    if indexed.point == *end {
                                        break;
                                    }
                                }
                                url
                            })
                    });

                if let Some(url) = url_to_open {
                    std::thread::spawn(move || {
                        if let Err(e) = open::that(&url) {
                            panic!("termlet: failed to open link `{url}`: {e}");
                        }
                    });
                }
            },
        };
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content.terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.notifier.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.notifier.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Option<Size<f32>>,
        font_measure: Option<Size<f32>>,
    ) {
        if let Some(size) = layout_size {
            self.size.layout_height = size.height;
            self.size.layout_width = size.width;
        };

        if let Some(size) = font_measure {
            self.size.cell_height = size.height as u16;
            self.size.cell_width = size.width as u16;
        }

        let lines = (self.size.layout_height / self.size.cell_height as f32)
            .floor() as u16;
        let cols = (self.size.layout_width / self.size.cell_width as f32)
            .floor() as u16;
        if lines > 0 && cols > 0 {
            self.size.num_lines = lines;
            self.size.num_cols = cols;
            self.notifier.on_resize(self.size.into());
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
        }
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.notifier.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    pub fn selectable_content(&self) -> String {
        let term = self.term.clone();
        let term = term.lock();
        let mut result = String::new();
        if let Some(range) = self.last_content.selectable_range {
            let grid = term.grid();
            let last_column = grid.last_column();
            let mut prev_line: Option<i32> = None;
            let mut line_buf = String::new();
            for line_num in range.start.line.0..=range.end.line.0 {
                let line = Line(line_num);
                line_buf.clear();
                for col_num in 0..=last_column.0 {
                    let point = Point::new(line, Column(col_num));
                    if range.contains(point) {
                        let cell = &grid[point];
                        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                            continue;
                        }
                        let current_line = line.0;
                        if let Some(p) = prev_line {
                            if current_line != p {
                                // Check if the previous line was a visual wrap:
                                // when a line overflows the terminal width it wraps
                                // to the next grid line without a logical newline.
                                // Alacritty marks such lines with WRAPLINE on the
                                // last cell.  If set, don't insert a newline so the
                                // copied text stays on a single line.
                                let prev_last = &grid[Line(p)][last_column];
                                if !prev_last.flags.contains(Flags::WRAPLINE) {
                                    result.push('\n');
                                }
                            }
                        }
                        line_buf.push(cell.c);
                        for c in cell.zerowidth().into_iter().flatten() {
                            line_buf.push(*c);
                        }
                        prev_line = Some(current_line);
                    }
                }
                // Trim trailing spaces from each line to avoid copying
                // empty-cell space characters from the terminal grid.
                let trimmed = line_buf.trim_end();
                result.push_str(trimmed);
            }
        }
        result
    }

    pub fn sync(&mut self) {
        let term = self.term.clone();
        let mut term = term.lock();
        self.internal_sync(&mut term);
    }

    /// Find all matches for the given pattern across the full terminal buffer
    /// (including scrollback history).
    ///
    /// `options` controls whether the pattern is treated as a literal or
    /// regex, and whether matching is case-sensitive and whole-word.
    pub fn search(&self, pattern: &str, options: crate::SearchOptions) -> Vec<Match> {
        let term = self.term.clone();
        let term = term.lock();
        let pattern = build_search_pattern(pattern, options);
        match RegexSearch::with_case_insensitive(&pattern, !options.case_sensitive) {
            Ok(mut regex) => full_regex_match_iter(&term, &mut regex).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Select (highlight) a match and scroll the viewport to show it.
    pub fn select_and_scroll(&mut self, m: &Match) {
        let term = self.term.clone();
        let mut term = term.lock();

        term.selection = Some(Selection::new(
            SelectionType::Simple,
            *m.start(),
            Side::Left,
        ));
        if let Some(ref mut selection) = term.selection {
            selection.update(*m.end(), Side::Right);
        }

        let target_line = m.start().line.0;
        let screen_lines = term.grid().screen_lines() as i32;
        let display_offset = term.grid().display_offset() as i32;

        // Viewport bounds in Point Line coordinates:
        // Line(-display_offset) is the top of the viewport,
        // Line(-display_offset + screen_lines - 1) is the bottom.
        let visible_top = -(display_offset);
        let visible_bottom = visible_top + screen_lines - 1;

        if target_line < visible_top || target_line > visible_bottom {
            let line_offset = (screen_lines / 3).max(1);
            let new_offset = (line_offset - target_line).max(0);
            let delta = new_offset - display_offset;
            term.grid_mut().scroll_display(Scroll::Delta(delta));
        }

        self.internal_sync(&mut term);
    }

    fn internal_sync(&mut self, terminal: &mut Term<EventProxy>) {
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(terminal),
            None => None,
        };

        let cursor_point = terminal.grid().cursor.point;
        let cursor = terminal.grid_mut().cursor_cell().clone();
        self.last_content.visible_cells = visible_cells(terminal.grid());
        highlight_log_levels(&mut self.last_content.visible_cells);
        self.last_content.history_size = terminal.grid().history_size();
        self.last_content.total_lines = terminal.grid().total_lines();
        self.last_content.screen_lines = terminal.grid().screen_lines();
        self.last_content.display_offset = terminal.grid().display_offset();
        self.last_content.selectable_range = selectable_range;
        self.last_content.cursor = cursor.clone();
        self.last_content.cursor_point = cursor_point;
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        self.last_content.osc_hyperlinks =
            visible_osc_hyperlinks(&self.last_content.visible_cells);
        self.last_content.visible_hyperlinks =
            visible_regex_match_iter(terminal, &mut self.url_regex.clone())
                .collect();
    }

    /// Clear the current text selection / highlight.
    pub fn clear_selection(&mut self) {
        let term = self.term.clone();
        let mut term = term.lock();
        term.selection = None;
        self.internal_sync(&mut term);
    }

    pub fn renderable_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the regex match, if the specified point is inside a detected URL.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<HyperlinkMatch> {
        visible_regex_match_iter(terminal, regex)
            .find(|rm| rm.contains(&point))
            .map(|range| HyperlinkMatch {
                range,
                url: None,
            })
    }

}

fn merge_actions(current: Action, next: Action) -> Action {
    match (current, next) {
        (Action::Shutdown, _) | (_, Action::Shutdown) => Action::Shutdown,
        (action, Action::ChangeTitle(_)) => action,
        (action, Action::Osc(message)) if !osc_reports_to_app(&message) => action,
        (_, next @ Action::Osc(_)) => next,
        (action, Action::Ignore) => action,
    }
}

fn osc_reports_to_app(message: &crate::OscMessage) -> bool {
    !matches!(
        message,
        crate::OscMessage::CommandStart | crate::OscMessage::CommandLine(_)
    )
}

fn escape_regex_literal(pattern: &str) -> String {
    const SPECIAL: &[char] = &['.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\'];
    let mut out = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build the regex passed to [`RegexSearch`] from a find-bar query.
fn build_search_pattern(pattern: &str, options: crate::SearchOptions) -> String {
    let body = if options.regex {
        pattern.to_owned()
    } else {
        escape_regex_literal(pattern)
    };

    if options.whole_word {
        // Match VS Code's whole-word behaviour: only require a boundary on
        // the sides of the query that actually end on a word character.
        // Otherwise a query like `foo!` would never match because `!` is
        // followed by a non-word character.
        let left_boundary = if pattern.chars().next().is_some_and(is_word_char) {
            r"(?-u:\b)"
        } else {
            ""
        };
        let right_boundary = if pattern.chars().next_back().is_some_and(is_word_char) {
            r"(?-u:\b)"
        } else {
            ""
        };
        format!("{left_boundary}(?:{body}){right_boundary}")
    } else {
        body
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Iterate over all regex matches in the visible viewport (plus a small buffer).
fn visible_regex_match_iter<'a, T: EventListener>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

/// Iterate over all regex matches across the entire terminal buffer
/// (scrollback history + visible screen).
fn full_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let start = term.line_search_left(Point::new(term.topmost_line(), Column(0)));
    let end = term.line_search_right(Point::new(term.bottommost_line(), Column(0)));

    RegexIter::new(start, end, Direction::Right, term, regex)
}

pub struct RenderableCell {
    pub point: Point,
    pub cell: Cell,
}

/// A detected hyperlink range.
///
/// For OSC 8 hyperlinks, `url` is `Some(uri)` read from the cell's hyperlink
/// data. For regex-detected bare URLs, `url` is `None` and the URL text is
/// extracted from the cell characters in the range when opened.
#[derive(Debug, Clone)]
pub struct HyperlinkMatch {
    pub range: RangeInclusive<Point>,
    pub url: Option<String>,
}

pub struct RenderableContent {
    pub visible_cells: Vec<RenderableCell>,
    pub history_size: usize,
    pub total_lines: usize,
    pub screen_lines: usize,
    pub display_offset: usize,
    pub hovered_hyperlink: Option<HyperlinkMatch>,
    /// OSC 8 hyperlinks detected from per-cell hyperlink data.
    pub osc_hyperlinks: Vec<HyperlinkMatch>,
    /// Regex-detected bare URL ranges (fallback for URLs without OSC 8).
    pub visible_hyperlinks: Vec<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub cursor_point: Point,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            visible_cells: Vec::new(),
            history_size: 0,
            total_lines: 0,
            screen_lines: 0,
            display_offset: 0,
            hovered_hyperlink: None,
            osc_hyperlinks: Vec::new(),
            visible_hyperlinks: Vec::new(),
            selectable_range: None,
            cursor: Cell::default(),
            cursor_point: Point::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

fn visible_cells(grid: &Grid<Cell>) -> Vec<RenderableCell> {
    let last_column = grid.last_column();
    let mut point =
        Point::new(Line(-(grid.display_offset() as i32) - 1), last_column);
    let end_line =
        min(point.line + grid.screen_lines(), grid.bottommost_line());
    let end = Point::new(end_line, last_column);
    let mut cells = Vec::with_capacity(grid.screen_lines() * grid.columns());

    while point < end {
        if point.column >= last_column {
            point.column = Column(0);
            point.line += 1;
        } else {
            point.column += Column(1);
        }

        cells.push(RenderableCell {
            point,
            cell: grid.index(point).clone(),
        });
    }

    cells
}

/// Scan visible cells for contiguous OSC 8 hyperlink regions.
///
/// Cells sharing the same hyperlink ID are grouped into a single [`HyperlinkMatch`]
/// with a pre-resolved URL from the cell's hyperlink data.
fn visible_osc_hyperlinks(cells: &[RenderableCell]) -> Vec<HyperlinkMatch> {
    let mut results = Vec::new();
    if cells.is_empty() {
        return results;
    }

    let mut current_id: Option<String> = None;
    let mut start_point: Option<Point> = None;
    let mut current_uri: Option<String> = None;
    let mut end_point: Option<Point> = None;

    for rc in cells {
        match rc.cell.hyperlink() {
            Some(hl) => {
                let hl_id = hl.id().to_owned();
                if current_id.as_ref() == Some(&hl_id) {
                    // Continue existing range — consecutive cells share the same ID
                    end_point = Some(rc.point);
                } else {
                    // Finish previous range
                    if let (Some(start), Some(end), Some(uri)) =
                        (start_point, end_point, current_uri.take())
                    {
                        results.push(HyperlinkMatch {
                            range: start..=end,
                            url: Some(uri),
                        });
                    }
                    // Start new range
                    current_id = Some(hl_id);
                    start_point = Some(rc.point);
                    end_point = Some(rc.point);
                    current_uri = Some(hl.uri().to_owned());
                }
            },
            None => {
                // Finish current range
                if let (Some(start), Some(end), Some(uri)) =
                    (start_point, end_point, current_uri.take())
                {
                    results.push(HyperlinkMatch {
                        range: start..=end,
                        url: Some(uri),
                    });
                }
                current_id = None;
                start_point = None;
                end_point = None;
            },
        }
    }

    // Finish last range
    if let (Some(start), Some(end), Some(uri)) = (start_point, end_point, current_uri.take()) {
        results.push(HyperlinkMatch {
            range: start..=end,
            url: Some(uri),
        });
    }

    results
}

fn cell_char_at(cells: &[RenderableCell], pos: usize, line: Line) -> Option<char> {
    if pos >= cells.len() || cells[pos].point.line != line {
        return None;
    }
    Some(cells[pos].cell.c)
}

/// Returns keyword length if cells at `start` match the keyword (case-insensitive, same line).
fn match_keyword(cells: &[RenderableCell], start: usize, keyword: &str) -> Option<usize> {
    let line = cells[start].point.line;
    let kw_chars: Vec<char> = keyword.chars().collect();
    for (k, &kw_char) in kw_chars.iter().enumerate() {
        let pos = start + k;
        if pos >= cells.len() || cells[pos].point.line != line {
            return None;
        }
        if !cells[pos].cell.c.eq_ignore_ascii_case(&kw_char) {
            return None;
        }
    }
    Some(kw_chars.len())
}

fn highlight_log_levels(cells: &mut [RenderableCell]) {
    // Longest keywords first so "WARNING" takes priority over "WARN".
    // Only match [KEYWORD] bracketed form — colon and bare-word patterns
    // are too ambiguous and cause false positives in message content.
    const LOG_LEVELS: &[(&str, Rgb)] = &[
        ("WARNING", Rgb { r: 0xff, g: 0xc0, b: 0x40 }),
        ("ERROR",   Rgb { r: 0xff, g: 0x50, b: 0x50 }),
        ("WARN",    Rgb { r: 0xff, g: 0xc0, b: 0x40 }),
        ("INFO",    Rgb { r: 0x50, g: 0xb8, b: 0xff }),
        ("DEBUG",   Rgb { r: 0xa0, g: 0xa0, b: 0xa0 }),
        ("TRACE",   Rgb { r: 0x78, g: 0x78, b: 0x78 }),
    ];

    let mut i = 0;
    while i < cells.len() {
        let mut matched = false;
        for &(keyword, rgb) in LOG_LEVELS {
            let Some(kw_len) = match_keyword(cells, i, keyword) else {
                continue;
            };
            let line = cells[i].point.line;
            let color = ansi::Color::Spec(rgb);
            let keyword_end = i + kw_len;

            // [KEYWORD] — bracketed log level, color brackets and keyword
            let has_open = i > 0 && cell_char_at(cells, i - 1, line) == Some('[');
            let has_close = cell_char_at(cells, keyword_end, line) == Some(']');
            if has_open && has_close {
                cells[i - 1].cell.fg = color;
                for k in 0..kw_len {
                    cells[i + k].cell.fg = color;
                }
                cells[keyword_end].cell.fg = color;
                i = keyword_end + 1;
                matched = true;
                break;
            }
        }
        if !matched {
            i += 1;
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let message = "Drop: sending Msg::Shutdown to event loop";
        eprintln!("[TERM-BACKEND {:?}] {message}", Instant::now());
        crate::diagnostics::log("TERM-BACKEND", message);
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match &event {
            Event::Exit | Event::ChildExit(_) => {
                let message = format!("send_event: {event:?}");
                eprintln!("[TERM-PROXY {:?}] {message}", Instant::now());
                crate::diagnostics::log("TERM-PROXY", message);
            },
            Event::PtyWrite(text) => {
                let preview: String = text.chars().take(300).collect();
                let message = format!(
                    "send_event: PtyWrite(len={}, preview=\"{}\")",
                    text.len(),
                    preview.escape_debug()
                );
                eprintln!("[TERM-PROXY {:?}] {message}", Instant::now());
                crate::diagnostics::log("TERM-PROXY", message);
            },
            _ => {},
        }
        let result = self.0.blocking_send(event);
        if result.is_err() {
            let message = "send_event: blocking_send FAILED (channel closed)";
            eprintln!("[TERM-PROXY {:?}] {message}", Instant::now());
            crate::diagnostics::log("TERM-PROXY", message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    struct MockListener;

    impl EventListener for MockListener {
        fn send_event(&self, _event: Event) {}
    }

    /// Feed raw byte sequences through the VTE parser into a fresh terminal.
    fn term_with_input(bytes: &[u8]) -> Term<MockListener> {
        let size = TermSize::new(60, 10);
        let mut term = Term::new(term::Config::default(), &size, MockListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    #[test]
    fn detects_single_osc8_hyperlink() {
        let term = term_with_input(
            b"\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\",
        );

        let cells = visible_cells(term.grid());
        let links = visible_osc_hyperlinks(&cells);

        assert_eq!(links.len(), 1, "expected one OSC 8 hyperlink");
        assert_eq!(links[0].url.as_deref(), Some("https://example.com"));
        assert_eq!(links[0].range.start().line, Line(0));
        assert_eq!(links[0].range.start().column, Column(0));
        assert_eq!(links[0].range.end().line, Line(0));
        assert_eq!(links[0].range.end().column, Column(4), "covers 'click'");
    }

    #[test]
    fn detects_multiple_osc8_hyperlinks() {
        let term = term_with_input(
            b"\x1b]8;;https://one.example.com\x1b\\one\x1b]8;;\x1b\\ \
             \x1b]8;;https://two.example.com\x1b\\two\x1b]8;;\x1b\\",
        );

        let cells = visible_cells(term.grid());
        let links = visible_osc_hyperlinks(&cells);

        assert_eq!(links.len(), 2, "expected two separate OSC 8 hyperlinks");
        assert_eq!(links[0].url.as_deref(), Some("https://one.example.com"));
        assert_eq!(links[1].url.as_deref(), Some("https://two.example.com"));
        // First link covers "one" at columns 0..=2, second covers "two" after a space.
        assert_eq!(links[0].range.end().column, Column(2));
        assert_eq!(links[1].range.start().column, Column(4));
        assert_eq!(links[1].range.end().column, Column(6));
    }

    #[test]
    fn plain_text_without_osc8_has_no_hyperlinks() {
        let term = term_with_input(b"just some plain text with no links");

        let cells = visible_cells(term.grid());
        let links = visible_osc_hyperlinks(&cells);

        assert!(links.is_empty(), "no OSC 8 sequences should yield no links");
    }

    #[test]
    fn cell_without_hyperlink_breaks_range() {
        // "abc" is linked, then a gap, then "def" linked again under the same URI
        // (different ID, so it must be treated as a separate range).
        let term = term_with_input(
            b"\x1b]8;id=a;https://example.com\x1b\\abc\x1b]8;;\x1b\\ GAP \
             \x1b]8;id=b;https://example.com\x1b\\def\x1b]8;;\x1b\\",
        );

        let cells = visible_cells(term.grid());
        let links = visible_osc_hyperlinks(&cells);

        assert_eq!(links.len(), 2, "gap must split the hyperlink ranges");
        assert_eq!(links[0].range.end().column, Column(2), "covers 'abc'");
        assert_eq!(links[1].range.start().column, Column(8), "covers 'def'");
    }

    #[test]
    fn regex_fallback_detects_bare_url() {
        // A bare URL without any OSC 8 wrapping must still be detected via regex.
        let term = term_with_input(b"visit https://qq.com now");

        let cells = visible_cells(term.grid());
        let osc_links = visible_osc_hyperlinks(&cells);
        assert!(
            osc_links.is_empty(),
            "bare URL should not produce OSC 8 links"
        );

        let mut regex = RegexSearch::new(URL_REGEX).expect("invalid url regexp");
        let matches: Vec<Match> =
            visible_regex_match_iter(&term, &mut regex).collect();
        assert_eq!(matches.len(), 1, "regex should detect one bare URL");
        // The URL starts at column 6 ("https://qq.com"), 14 chars wide.
        assert_eq!(matches[0].start().column, Column(6));
        assert_eq!(matches[0].end().column, Column(19), "covers https://qq.com");
    }

    #[test]
    fn osc8_takes_priority_over_regex() {
        // When both OSC 8 metadata and URL-looking text are present, the
        // hover detection must prefer the OSC 8 hyperlink.
        let term = term_with_input(
            b"\x1b]8;;https://internal.example.com\x1b\\https://qq.com\x1b]8;;\x1b\\",
        );
        let cells = visible_cells(term.grid());

        // OSC 8 hyperlink exists...
        let osc_links = visible_osc_hyperlinks(&cells);
        assert_eq!(osc_links.len(), 1);
        assert_eq!(osc_links[0].url.as_deref(), Some("https://internal.example.com"));

        // ...and the regex also matches the displayed text.
        let mut regex = RegexSearch::new(URL_REGEX).expect("invalid url regexp");
        let matches: Vec<Match> =
            visible_regex_match_iter(&term, &mut regex).collect();
        assert!(!matches.is_empty(), "displayed URL text is regex-detectable");

        // Hover resolution must pick the OSC 8 link (url: Some).
        let hovered = osc_links
            .iter()
            .find(|m| m.range.contains(&Point::new(Line(0), Column(2))))
            .cloned();
        assert!(hovered.is_some());
        assert!(hovered.unwrap().url.is_some());
    }

    #[test]
    fn search_pattern_builds_literal_and_whole_word_forms() {
        let literal = crate::SearchOptions {
            case_sensitive: false,
            whole_word: false,
            regex: false,
        };
        assert_eq!(build_search_pattern("a.b", literal), r"a\.b");

        let whole_word = crate::SearchOptions {
            case_sensitive: false,
            whole_word: true,
            regex: false,
        };
        assert_eq!(
            build_search_pattern("cat", whole_word),
            r"(?-u:\b)(?:cat)(?-u:\b)"
        );
        assert_eq!(
            build_search_pattern("foo!", whole_word),
            r"(?-u:\b)(?:foo!)"
        );

        let regex = crate::SearchOptions {
            case_sensitive: true,
            whole_word: false,
            regex: true,
        };
        assert_eq!(build_search_pattern("^cat|dog$", regex), "^cat|dog$");
    }

    #[test]
    fn whole_word_regex_matches_term_text() {
        let term = term_with_input(b"termlet");
        let mut regex = RegexSearch::with_case_insensitive(
            r"(?-u:\b)(?:termlet)(?-u:\b)",
            true,
        )
        .expect("valid whole-word regex");

        let matches: Vec<Match> =
            visible_regex_match_iter(&term, &mut regex).collect();

        assert_eq!(matches.len(), 1, "whole-word 'termlet' should match");
    }

    /// A batch is collapsed to a single action, keeping only the last
    /// reporting event.  A fast command's full OSC 633 cycle (C → D → A → OSC 7)
    /// lands in one batch, so `CommandExecuted` is shadowed by the trailing
    /// `WorkingDirectory` — this is why the app records executed commands by
    /// scanning the raw batch, not the merged action (see
    /// `terminal/update.rs` `IcedTermEvent`).
    #[test]
    fn merge_actions_drops_command_executed_in_full_cycle_batch() {
        let mut action = Action::default();
        let batch = [
            Action::Osc(crate::OscMessage::CommandStart),
            Action::Osc(crate::OscMessage::CommandExecuted("ls".into())),
            Action::Osc(crate::OscMessage::CommandFinished(Some(0))),
            Action::Osc(crate::OscMessage::PromptStart),
            Action::Osc(crate::OscMessage::WorkingDirectory(
                "/home/user".into(),
            )),
        ];
        for next in batch {
            action = merge_actions(action, next);
        }
        assert_eq!(
            action,
            Action::Osc(crate::OscMessage::WorkingDirectory(
                "/home/user".into()
            ))
        );
    }

    /// A slow command emits OSC 633 ; C alone in its batch (no D/A/OSC 7 within
    /// the 16 ms window), so the merged action does carry the command.
    #[test]
    fn merge_actions_keeps_command_executed_alone_in_batch() {
        let action = merge_actions(
            Action::default(),
            Action::Osc(crate::OscMessage::CommandExecuted("sleep 5".into())),
        );
        assert_eq!(
            action,
            Action::Osc(crate::OscMessage::CommandExecuted("sleep 5".into()))
        );
    }
}
