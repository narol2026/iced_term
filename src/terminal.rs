use crate::actions::Action;
use crate::backend;
use crate::bindings::{Binding, BindingAction, BindingsLayout, InputKind};
use crate::font::TermFont;
use crate::settings::{FontSettings, Settings, ThemeSettings};
use crate::theme::{ColorPalette, Theme};
use crate::AlacrittyEvent;
use iced::futures::stream::BoxStream;
use iced::futures::{SinkExt, StreamExt};
use iced::widget::canvas::Cache;
use iced::Subscription;
use std::hash::{Hash, Hasher};
use std::io::Result;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::Mutex;
use tokio::time::{self, Duration};

const MAX_BACKEND_EVENTS_PER_BATCH: usize = 4096;
const BACKEND_EVENT_BATCH_INTERVAL: Duration = Duration::from_millis(16);

/// Global counter so each Terminal instance gets a unique generation number,
/// even when recreated with the same session_id (e.g. SSH reconnect).
static TERMINAL_GENERATION: AtomicU64 = AtomicU64::new(0);

macro_rules! sub_log {
    ($id:expr, $($arg:tt)*) => {
        {
            let message = format!($($arg)*);
            eprintln!("[TERM-SUB-{} {:?}] {}", $id, Instant::now(), message);
            crate::diagnostics::log(&format!("TERM-SUB-{}", $id), message);
        }
    };
}

#[derive(Debug, Clone)]
pub enum Event {
    BackendCall(u64, backend::Command),
}

#[derive(Debug, Clone)]
pub enum Command {
    ChangeTheme(Box<ColorPalette>),
    ChangeFont(FontSettings),
    AddBindings(Vec<(Binding<InputKind>, BindingAction)>),
    ProxyToBackend(backend::Command),
}

pub struct Terminal {
    pub id: u64,
    generation: u64,
    widget_id: iced::widget::Id,
    pub(crate) font: TermFont,
    pub(crate) theme: Theme,
    pub(crate) cache: Cache,
    pub(crate) bindings: BindingsLayout,
    pub(crate) backend: backend::Backend,
    backend_event_rx: Arc<Mutex<Receiver<AlacrittyEvent>>>,
    /// Whether to draw the scrollbar.  The host application controls
    /// auto-hide via [`Self::set_scrollbar_visible`].
    pub scrollbar_visible: std::cell::Cell<bool>,
}

/// Options controlling how a terminal search pattern is interpreted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// Match upper- and lowercase letters exactly as typed.
    pub case_sensitive: bool,
    /// Match only whole words instead of substrings.
    pub whole_word: bool,
    /// Interpret the query as a regular expression instead of a literal.
    pub regex: bool,
}

impl Terminal {
    pub fn new(id: u64, settings: Settings) -> Result<Self> {
        let (backend_event_tx, backend_event_rx) = mpsc::channel(100);
        let theme = Theme::new(settings.theme);
        let font = TermFont::new(settings.font);
        let generation = TERMINAL_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let font_measure = font.measure;
        Ok(Self {
            id,
            generation,
            widget_id: iced::widget::Id::unique(),
            font,
            theme,
            bindings: BindingsLayout::default(),
            cache: Cache::default(),
            backend: backend::Backend::new(
                id,
                backend_event_tx,
                settings.backend,
                font_measure,
            )?,
            backend_event_rx: Arc::new(Mutex::new(backend_event_rx)),
            scrollbar_visible: std::cell::Cell::new(false),
        })
    }

    /// Control whether the scrollbar is drawn.  Callers should set this to
    /// `true` on scroll activity and back to `false` after an idle timeout.
    ///
    /// The terminal canvas cache is cleared so the next frame picks up the
    /// new visibility immediately.
    pub fn set_scrollbar_visible(&self, visible: bool) {
        self.scrollbar_visible.set(visible);
        self.cache.clear();
    }

    /// Returns `true` when the terminal emulator is in alternate screen mode
    /// (e.g. vim, nano, less, htop).  The host application can use this to
    /// decide whether local keyboard shortcuts should be suppressed so that
    /// keystrokes pass through to the remote application unmodified.
    pub fn is_alt_screen(&self) -> bool {
        self.backend
            .renderable_content()
            .terminal_mode
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    pub fn widget_id(&self) -> &iced::widget::Id {
        &self.widget_id
    }

    pub fn subscription(&self) -> Subscription<Event> {
        let data = TerminalSubscriptionData {
            id: self.id,
            generation: self.generation,
            event_receiver: self.backend_event_rx.clone(),
        };

        Subscription::run_with(data, terminal_subscription_stream)
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        let mut action = Action::default();

        match cmd {
            Command::ChangeTheme(color_pallete) => {
                self.theme = Theme::new(ThemeSettings::new(color_pallete));
            },
            Command::ChangeFont(font_settings) => {
                self.font = TermFont::new(font_settings);
                self.backend.handle(backend::Command::Resize(
                    None,
                    Some(self.font.measure),
                ));
            },
            Command::AddBindings(bindings) => {
                self.bindings.add_bindings(bindings);
            },
            Command::ProxyToBackend(cmd) => {
                action = self.backend.handle(cmd);
            },
        };

        self.sync_and_redraw();
        action
    }

    fn sync_and_redraw(&mut self) {
        self.backend.sync();
        self.redraw();
    }

    pub fn clear_selection(&mut self) {
        self.backend.clear_selection();
        self.cache.clear();
    }

    /// Returns the terminal cursor position in grid coordinates (column, line)
    /// and the cell size in pixels (width, height). Useful for positioning
    /// popups relative to the text cursor.
    pub fn cursor_screen_position(&self) -> (f32, f32, f32, f32) {
        let content = self.backend.renderable_content();
        let col = content.cursor_point.column.0 as f32;
        let line = content.cursor_point.line.0 as f32;
        let cell_w = content.terminal_size.cell_width as f32;
        let cell_h = content.terminal_size.cell_height as f32;
        let display_offset = content.display_offset as f32;
        // Cursor x, y in pixels (relative to terminal layout origin)
        let cursor_x = col * cell_w;
        let cursor_y = (line as f32 + display_offset) * cell_h;
        (cursor_x, cursor_y, cell_w, cell_h)
    }

    /// Returns the pixel dimensions of the terminal content area (width, height).
    /// This is the actual layout size of the terminal widget.
    pub fn content_area_size(&self) -> (f32, f32) {
        let content = self.backend.renderable_content();
        (
            content.terminal_size.layout_width,
            content.terminal_size.layout_height,
        )
    }

    /// Reads the text on the terminal's current line, from column 0 up to the
    /// cursor position. This captures the full line as displayed on screen
    /// (including the shell prompt), regardless of how it was entered — typed,
    /// pasted, or edited with backspace / arrow keys.
    ///
    /// Returns an empty string if no visible cells exist on the cursor's line.
    pub fn current_line_text(&self) -> String {
        let content = self.backend.renderable_content();
        let cursor_line = content.cursor_point.line;
        let cursor_col = content.cursor_point.column;

        let mut chars: Vec<(usize, char)> = content
            .visible_cells
            .iter()
            .filter(|cell| {
                cell.point.line == cursor_line && cell.point.column < cursor_col
            })
            .map(|cell| (cell.point.column.0, cell.cell.c))
            .collect();

        chars.sort_by_key(|(col, _)| *col);
        chars.into_iter().map(|(_, c)| c).collect()
    }

    /// Search for a pattern, return all matches and jump to the one at `index`.
    /// The current match is highlighted and the viewport scrolls to show it.
    /// `options` controls case sensitivity, whole-word matching, and regex mode.
    /// Returns (all_matches, effective_index_wrapped).
    pub fn search_jump(
        &mut self,
        query: &str,
        index: usize,
        options: SearchOptions,
    ) -> (Vec<crate::Match>, Option<usize>) {
        self.backend.sync();
        let matches = self.backend.search(query, options);
        if matches.is_empty() {
            self.cache.clear();
            return (matches, None);
        }
        let idx = index % matches.len();
        self.backend.select_and_scroll(&matches[idx]);
        self.cache.clear();
        (matches, Some(idx))
    }

    fn redraw(&mut self) {
        self.cache.clear();
    }
}

#[derive(Clone)]
struct TerminalSubscriptionData {
    id: u64,
    generation: u64,
    event_receiver: Arc<Mutex<Receiver<AlacrittyEvent>>>,
}

impl Hash for TerminalSubscriptionData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.generation.hash(state);
    }
}

fn terminal_subscription_stream(
    data: &TerminalSubscriptionData,
) -> BoxStream<'static, Event> {
    let id = data.id;
    let event_receiver = data.event_receiver.clone();
    iced::stream::channel(1000, async move |mut output| {
        let mut shutdown = false;
        sub_log!(id, "subscription stream started, waiting for events");
        loop {
            let events = {
                let mut event_receiver = event_receiver.lock().await;
                let Some(first_event) = event_receiver.recv().await else {
                    if !shutdown {
                        sub_log!(
                            id,
                            "terminal event channel closed unexpectedly (session dropped, treating as shutdown)"
                        );
                    }
                    break;
                };

                sub_log!(id, "received first event: {:?}", first_event);

                let mut events = vec![first_event];
                let batch_deadline = time::sleep(BACKEND_EVENT_BATCH_INTERVAL);
                tokio::pin!(batch_deadline);

                while events.len() < MAX_BACKEND_EVENTS_PER_BATCH {
                    tokio::select! {
                        _ = &mut batch_deadline => break,
                        event = event_receiver.recv() => {
                            match event {
                                Some(event) => events.push(event),
                                None => break,
                            }
                        }
                    }
                }

                events
            };

            sub_log!(
                id,
                "batch ({} events): {:?}",
                events.len(),
                events.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>()
            );

            if events.iter().any(|event| matches!(event, AlacrittyEvent::Exit)) {
                sub_log!(
                    id,
                    "detected Exit event in batch ({} events), setting shutdown",
                    events.len()
                );
                shutdown = true;
            }

            if output
                .send(Event::BackendCall(
                    id,
                    backend::Command::ProcessAlacrittyEvents(events),
                ))
                .await
                .is_err()
            {
                sub_log!(
                    id,
                    "output channel closed, exiting subscription"
                );
                break;
            }
        }
    })
    .boxed()
}
