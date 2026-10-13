use crate::backend::{
    Backend, Command, LinkAction, MouseButton, RenderableContent,
};
use crate::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::terminal::{Event, Terminal};
use crate::theme::TerminalStyle;
use alacritty_terminal::index::Point as TerminalGridPoint;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::{cell, TermMode};
use alacritty_terminal::vte::ansi::{self as ansi, NamedColor};
use iced::alignment::Vertical;
use iced::font::{Style as FontStyle, Weight as FontWeight};
use iced::mouse::{Cursor, ScrollDelta};
use iced::widget::canvas::{Path, Text};
use iced::widget::container;
use iced::{Color, Element, Length, Point, Rectangle, Size, Theme};
use iced_core::clipboard::Kind as ClipboardKind;
use iced_core::input_method::{self, InputMethod, Purpose};
use iced_core::keyboard::key::Named;
use iced_core::keyboard::{Key, Modifiers};
use iced_core::mouse::{self, Click};
use iced_core::text::{Alignment, LineHeight, Shaping};
use iced_core::widget::operation::{self, Focusable};
use iced_graphics::core::widget::{tree, Tree};
use iced_graphics::core::Widget;
use iced_graphics::geometry::Stroke;

const DRAG_SCROLL_MAX_LINES: i32 = 8;
const SCROLLBAR_MARGIN: f32 = 4.0;
const SCROLLBAR_MIN_THUMB_HEIGHT: f32 = 24.0;
const SCROLLBAR_RADIUS: f32 = 3.0;
const SCROLLBAR_WIDTH: f32 = 6.0;
const SCROLLBAR_RESERVED_WIDTH: f32 = SCROLLBAR_MARGIN * 2.0 + SCROLLBAR_WIDTH;

pub struct TerminalView<'a> {
    term: &'a Terminal,
}

impl<'a> TerminalView<'a> {
    pub fn show(term: &'a Terminal) -> Element<'a, Event> {
        container(Self { term })
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| term.theme.container_style())
            .into()
    }

    pub fn focus<Message: 'static>(
        id: iced::widget::Id,
    ) -> iced::Task<Message> {
        iced::widget::operation::focus(id)
    }

    fn is_cursor_in_layout(
        &self,
        cursor: Cursor,
        layout: iced_graphics::core::Layout<'_>,
    ) -> bool {
        if let Some(cursor_position) = cursor.position() {
            let layout_position = layout.position();
            let layout_size = layout.bounds();
            let is_triggered = cursor_position.x >= layout_position.x
                && cursor_position.y >= layout_position.y
                && cursor_position.x < (layout_position.x + layout_size.width)
                && cursor_position.y < (layout_position.y + layout_size.height);

            return is_triggered;
        }

        false
    }

    fn is_cursor_hovered_hyperlink(&self, state: &TerminalViewState) -> bool {
        let content = self.term.backend.renderable_content();
        if let Some(hyperlink_match) = &content.hovered_hyperlink {
            return hyperlink_match.range.contains(&state.mouse_position_on_grid);
        }

        false
    }

    fn handle_resize(
        &mut self,
        state: &mut TerminalViewState,
        layout: iced_graphics::core::Layout<'_>,
        shell: &mut iced_graphics::core::Shell<'_, Event>,
    ) {
        let layout_size = layout.bounds().size();
        let layout_pos = layout.position();
        if state.term_id != self.term.id {
            state.term_id = self.term.id;
            state.size = Size::ZERO;
            state.position = Point::ORIGIN;
        }
        // Invalidate the geometry cache when the widget moves (e.g. pane-grid
        // drag-and-drop swap), because the cache stores terminal cells at
        // absolute screen positions and would otherwise render stale content.
        if state.position != layout_pos {
            state.position = layout_pos;
            self.term.cache.clear();
        }
        if state.size != layout_size {
            state.size = layout_size;
            let mut content_size = layout_size;
            content_size.width =
                (content_size.width - SCROLLBAR_RESERVED_WIDTH).max(0.0);
            let cmd = Command::Resize(
                Some(content_size),
                Some(self.term.font.measure),
            );
            shell.publish(Event::BackendCall(self.term.id, cmd));
        }
    }

    fn handle_focus(
        &self,
        event: &iced_core::Event,
        state: &mut TerminalViewState,
        is_cursor_in_layout: bool,
    ) {
        use iced::Event::Mouse;
        use iced_core::mouse::{Button::Left, Event::ButtonPressed};

        if let Mouse(ButtonPressed(Left)) = event {
            state.focus = is_cursor_in_layout;
        }
    }

    fn handle_mouse_event(
        &self,
        state: &mut TerminalViewState,
        layout_bounds: Rectangle,
        cursor_position: Point,
        event: &iced::mouse::Event,
        clipboard: &mut dyn iced_graphics::core::Clipboard,
    ) -> Vec<Command> {
        let mut commands = Vec::new();
        let terminal_content = self.term.backend.renderable_content();
        let terminal_mode = terminal_content.terminal_mode;
        let layout_position = layout_bounds.position();

        match event {
            iced_core::mouse::Event::ButtonPressed(
                iced_core::mouse::Button::Right,
            ) => {
                if !state.is_focused() {
                    return commands;
                }
                if let Some(mut cmds) = Self::do_paste(
                    clipboard,
                    ClipboardKind::Standard,
                    terminal_mode,
                ) {
                    commands.append(&mut cmds);
                }
                return commands;
            },
            iced_core::mouse::Event::ButtonPressed(
                iced_core::mouse::Button::Middle,
            ) => {
                if !state.is_focused() {
                    return commands;
                }
                if let Some(mut cmds) = Self::do_paste(
                    clipboard,
                    ClipboardKind::Primary,
                    terminal_mode,
                ) {
                    commands.append(&mut cmds);
                }
                return commands;
            },
            iced_core::mouse::Event::ButtonPressed(
                iced_core::mouse::Button::Left,
            ) => {
                if let Some(scrollbar) =
                    Self::scrollbar_geometry(layout_bounds, terminal_content)
                {
                    if Self::contains(scrollbar.track, cursor_position) {
                        let is_thumb_pressed =
                            Self::contains(scrollbar.thumb, cursor_position);
                        let pointer_offset_y = if is_thumb_pressed {
                            cursor_position.y - scrollbar.thumb.y
                        } else {
                            scrollbar.thumb.height * 0.5
                        };
                        let target_display_offset = if is_thumb_pressed {
                            scrollbar.display_offset
                        } else {
                            Self::scrollbar_target_offset(
                                &scrollbar,
                                cursor_position.y,
                            )
                            .unwrap_or(scrollbar.display_offset)
                        };

                        state.is_dragged = false;
                        state.drag_scroll = None;
                        state.scrollbar_drag_dirty = false;
                        state.scrollbar_drag = Some(ScrollbarDragState {
                            pointer_offset_y,
                            target_display_offset,
                        });
                        if let Some(command) = Self::scrollbar_scroll_command(
                            target_display_offset,
                            scrollbar.display_offset,
                        ) {
                            commands.push(command);
                        }
                        return commands;
                    }
                }

                if !state.is_focused() {
                    return Vec::default();
                }

                let has_existing_selection =
                    terminal_content.selectable_range.is_some();
                Self::handle_left_button_pressed(
                    state,
                    &terminal_mode,
                    cursor_position,
                    layout_position,
                    has_existing_selection,
                    &mut commands,
                );
            },
            iced_core::mouse::Event::CursorMoved { position } => {
                if let Some(drag) = &mut state.scrollbar_drag {
                    if let Some(scrollbar) = Self::scrollbar_geometry(
                        layout_bounds,
                        terminal_content,
                    ) {
                        let thumb_center_y = position.y - drag.pointer_offset_y
                            + (scrollbar.thumb.height * 0.5);
                        if let Some(target_display_offset) =
                            Self::scrollbar_target_offset(
                                &scrollbar,
                                thumb_center_y,
                            )
                        {
                            if target_display_offset
                                != drag.target_display_offset
                            {
                                drag.target_display_offset =
                                    target_display_offset;
                                state.scrollbar_drag_dirty = true;
                            }
                        }
                    }
                    return commands;
                }

                if !state.is_focused() {
                    return Vec::default();
                }

                Self::handle_cursor_moved(
                    state,
                    self.term.backend.renderable_content(),
                    position,
                    layout_bounds,
                    &mut commands,
                );
            },
            iced_core::mouse::Event::ButtonReleased(
                iced_core::mouse::Button::Left,
            ) => {
                if state.scrollbar_drag.take().is_some() {
                    return commands;
                }

                if !state.is_focused() {
                    return Vec::default();
                }

                Self::handle_button_released(
                    state,
                    &terminal_mode,
                    &self.term.bindings,
                    &mut commands,
                );
            },
            iced::mouse::Event::WheelScrolled { delta } => {
                Self::handle_wheel_scrolled(
                    state,
                    *delta,
                    &self.term.font.measure,
                    self.term.font.scroll_sensitivity,
                    &mut commands,
                );
            },
            _ => {},
        }

        commands
    }

    fn contains(rectangle: Rectangle, point: Point) -> bool {
        point.x >= rectangle.x
            && point.x <= rectangle.x + rectangle.width
            && point.y >= rectangle.y
            && point.y <= rectangle.y + rectangle.height
    }

    fn scrollbar_geometry(
        layout_bounds: Rectangle,
        terminal_content: &RenderableContent,
    ) -> Option<ScrollbarGeometry> {
        let history_size = terminal_content.history_size;
        if history_size == 0 || layout_bounds.height <= SCROLLBAR_MARGIN * 2.0 {
            return None;
        }

        let screen_lines = terminal_content.screen_lines.max(1);
        let total_lines = terminal_content.total_lines.max(screen_lines);
        let display_offset = terminal_content.display_offset;
        let track_height = layout_bounds.height - (SCROLLBAR_MARGIN * 2.0);
        let thumb_height = ((screen_lines as f32 / total_lines as f32)
            * track_height)
            .clamp(SCROLLBAR_MIN_THUMB_HEIGHT.min(track_height), track_height);
        let max_thumb_y = track_height - thumb_height;
        let offset_ratio = display_offset as f32 / history_size as f32;
        let thumb_y = layout_bounds.y
            + SCROLLBAR_MARGIN
            + ((1.0 - offset_ratio) * max_thumb_y);

        Some(ScrollbarGeometry {
            track: Rectangle {
                x: layout_bounds.x + layout_bounds.width
                    - SCROLLBAR_MARGIN
                    - SCROLLBAR_WIDTH,
                y: layout_bounds.y + SCROLLBAR_MARGIN,
                width: SCROLLBAR_WIDTH,
                height: track_height,
            },
            thumb: Rectangle {
                x: layout_bounds.x + layout_bounds.width
                    - SCROLLBAR_MARGIN
                    - SCROLLBAR_WIDTH,
                y: thumb_y,
                width: SCROLLBAR_WIDTH,
                height: thumb_height,
            },
            history_size,
            display_offset,
        })
    }

    fn scrollbar_target_offset(
        scrollbar: &ScrollbarGeometry,
        thumb_center_y: f32,
    ) -> Option<usize> {
        let max_thumb_y = scrollbar.track.height - scrollbar.thumb.height;
        if max_thumb_y <= 0.0 {
            return None;
        }

        let min_center_y = scrollbar.track.y + (scrollbar.thumb.height * 0.5);
        let max_center_y = scrollbar.track.y + scrollbar.track.height
            - (scrollbar.thumb.height * 0.5);
        let clamped_center_y = thumb_center_y.clamp(min_center_y, max_center_y);
        let top_progress = (clamped_center_y - min_center_y) / max_thumb_y;
        Some(
            ((1.0 - top_progress) * scrollbar.history_size as f32).round()
                as usize,
        )
    }

    fn scrollbar_scroll_command(
        target_display_offset: usize,
        current_display_offset: usize,
    ) -> Option<Command> {
        let delta =
            target_display_offset as i32 - current_display_offset as i32;

        (delta != 0).then_some(Command::Scroll(delta))
    }

    fn with_alpha(mut color: Color, alpha: f32) -> Color {
        color.a = alpha;
        color
    }

    fn handle_left_button_pressed(
        state: &mut TerminalViewState,
        terminal_mode: &TermMode,
        cursor_position: Point,
        layout_position: Point,
        has_existing_selection: bool,
        commands: &mut Vec<Command>,
    ) {
        let cmd = if terminal_mode.intersects(TermMode::MOUSE_MODE) {
            Command::MouseReport(
                MouseButton::LeftButton,
                state.keyboard_modifiers,
                state.mouse_position_on_grid,
                true,
            )
        } else if has_existing_selection
            && state.keyboard_modifiers.contains(Modifiers::SHIFT)
        {
            Command::SelectUpdate((
                cursor_position.x - layout_position.x,
                cursor_position.y - layout_position.y,
            ))
        } else {
            let current_click = Click::new(
                cursor_position,
                mouse::Button::Left,
                state.last_click,
            );
            let selection_type = match current_click.kind() {
                mouse::click::Kind::Single => SelectionType::Simple,
                mouse::click::Kind::Double => SelectionType::Semantic,
                mouse::click::Kind::Triple => SelectionType::Lines,
            };
            state.last_click = Some(current_click);
            Command::SelectStart(
                selection_type,
                (
                    cursor_position.x - layout_position.x,
                    cursor_position.y - layout_position.y,
                ),
            )
        };
        commands.push(cmd);
        state.is_dragged = true;
        state.drag_scroll = None;
    }

    fn handle_cursor_moved(
        state: &mut TerminalViewState,
        terminal_content: &RenderableContent,
        position: &Point,
        layout_bounds: Rectangle,
        commands: &mut Vec<Command>,
    ) {
        let layout_position = layout_bounds.position();
        let cursor_x = position.x - layout_position.x;
        let cursor_y = position.y - layout_position.y;
        state.mouse_position_on_grid = Backend::selection_point(
            cursor_x,
            cursor_y,
            &terminal_content.terminal_size,
            terminal_content.display_offset,
        );

        // Handle command or selection update based on terminal mode and modifiers
        if state.is_dragged {
            let terminal_mode = terminal_content.terminal_mode;
            if terminal_mode.intersects(TermMode::MOUSE_MOTION) {
                state.drag_scroll = None;
                commands.push(Command::MouseReport(
                    MouseButton::LeftMove,
                    state.keyboard_modifiers,
                    state.mouse_position_on_grid,
                    true,
                ));
            } else {
                if let Some(delta) = Self::drag_scroll_delta(
                    cursor_y,
                    layout_bounds.height,
                    terminal_content.terminal_size.cell_height as f32,
                ) {
                    state.drag_scroll = Some(DragScrollState {
                        delta,
                        selection_position: (cursor_x, cursor_y),
                    });
                    commands.push(Command::Scroll(delta));
                } else {
                    state.drag_scroll = None;
                }

                commands.push(Command::SelectUpdate((cursor_x, cursor_y)));
            }
        }

        // Handle link hover when Ctrl is held (for underline and cursor feedback)
        if state.keyboard_modifiers == Modifiers::COMMAND {
            commands.push(Command::ProcessLink(
                LinkAction::Hover,
                state.mouse_position_on_grid,
            ));
        }
    }

    fn drag_scroll_delta(
        cursor_y: f32,
        layout_height: f32,
        cell_height: f32,
    ) -> Option<i32> {
        let distance = if cursor_y < 0.0 {
            -cursor_y
        } else if cursor_y > layout_height {
            cursor_y - layout_height
        } else {
            return None;
        };

        let cell_height = cell_height.max(1.0);
        let lines = (distance / cell_height).ceil() as i32;
        let lines = lines.clamp(1, DRAG_SCROLL_MAX_LINES);

        if cursor_y < 0.0 {
            Some(lines)
        } else {
            Some(-lines)
        }
    }

    fn handle_drag_scroll_frame(
        state: &TerminalViewState,
        commands: &mut Vec<Command>,
    ) {
        if !state.is_dragged {
            return;
        }

        if let Some(drag_scroll) = state.drag_scroll {
            commands.push(Command::Scroll(drag_scroll.delta));
            commands
                .push(Command::SelectUpdate(drag_scroll.selection_position));
        }
    }

    fn handle_scrollbar_drag_frame(
        state: &mut TerminalViewState,
        terminal_content: &RenderableContent,
        commands: &mut Vec<Command>,
    ) {
        if !state.scrollbar_drag_dirty {
            return;
        }

        state.scrollbar_drag_dirty = false;
        if let Some(drag) = state.scrollbar_drag {
            if let Some(command) = Self::scrollbar_scroll_command(
                drag.target_display_offset,
                terminal_content.display_offset,
            ) {
                commands.push(command);
            }
        }
    }

    fn scrollbar_drag_release_command(
        state: &mut TerminalViewState,
        terminal_content: &RenderableContent,
    ) -> Option<Command> {
        let drag = state.scrollbar_drag.take()?;
        let command = state.scrollbar_drag_dirty.then(|| {
            Self::scrollbar_scroll_command(
                drag.target_display_offset,
                terminal_content.display_offset,
            )
        })?;
        state.scrollbar_drag_dirty = false;
        command
    }

    fn handle_button_released(
        state: &mut TerminalViewState,
        terminal_mode: &TermMode,
        bindings: &BindingsLayout, // Use the actual type of your bindings here
        commands: &mut Vec<Command>,
    ) {
        state.is_dragged = false;
        state.drag_scroll = None;

        if terminal_mode.intersects(TermMode::MOUSE_MODE) {
            commands.push(Command::MouseReport(
                MouseButton::LeftButton,
                state.keyboard_modifiers,
                state.mouse_position_on_grid,
                false,
            ));
        }

        if bindings.get_action(
            InputKind::Mouse(iced_core::mouse::Button::Left),
            state.keyboard_modifiers,
            *terminal_mode,
        ) == BindingAction::LinkOpen
        {
            commands.push(Command::ProcessLink(
                LinkAction::Open,
                state.mouse_position_on_grid,
            ));
        }
    }

    fn handle_wheel_scrolled(
        state: &mut TerminalViewState,
        delta: ScrollDelta,
        font_measure: &Size<f32>,
        scroll_sensitivity: f32,
        commands: &mut Vec<Command>,
    ) {
        match delta {
            ScrollDelta::Lines { y, .. } => {
                let lines = y.signum() * y.abs().round() * scroll_sensitivity;
                commands.push(Command::Scroll(lines as i32));
            },
            ScrollDelta::Pixels { y, .. } => {
                state.scroll_pixels -= y * scroll_sensitivity;
                let line_height = font_measure.height;
                let lines = (state.scroll_pixels / line_height).trunc();
                state.scroll_pixels %= line_height;
                if lines != 0.0 {
                    commands.push(Command::Scroll(lines as i32));
                }
            },
        }
    }

    fn do_paste(
        clipboard: &mut dyn iced_graphics::core::Clipboard,
        kind: ClipboardKind,
        terminal_mode: TermMode,
    ) -> Option<Vec<Command>> {
        let data = clipboard.read(kind)?;
        let input: Vec<u8> = data.bytes().collect();
        if terminal_mode.contains(TermMode::BRACKETED_PASTE) {
            Some(vec![
                Command::Write(b"\x1b[200~".to_vec()),
                Command::Write(input),
                Command::Write(b"\x1b[201~".to_vec()),
            ])
        } else {
            Some(vec![Command::Write(input)])
        }
    }

    fn handle_keyboard_event(
        &self,
        state: &mut TerminalViewState,
        clipboard: &mut dyn iced_graphics::core::Clipboard,
        event: &iced::keyboard::Event,
    ) -> Option<Command> {
        let mut binding_action = BindingAction::Ignore;
        let last_content = self.term.backend.renderable_content();
        match event {
            iced::keyboard::Event::ModifiersChanged(m) => {
                state.keyboard_modifiers = *m;
                let action = if state.keyboard_modifiers == Modifiers::COMMAND {
                    LinkAction::Hover
                } else {
                    LinkAction::Clear
                };
                return Some(Command::ProcessLink(
                    action,
                    state.mouse_position_on_grid,
                ));
            },
            iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            } => match &key {
                // Use the physical character key for bindings even when text is None (e.g., Ctrl/Cmd combos)
                Key::Character(k) => {
                    let lower = k.to_ascii_lowercase();
                    binding_action = self.term.bindings.get_action(
                        InputKind::Char(lower),
                        *modifiers,
                        last_content.terminal_mode,
                    );

                    // If no binding matched, only write printable text (when provided).
                    // Only suppress character writes during active IME composition (preedit),
                    // not merely when the IME is open — otherwise typing ASCII digits/chars
                    // while an IME is enabled (but idle) would be blocked.
                    if binding_action == BindingAction::Ignore
                        && state.ime_preedit.is_none()
                    {
                        if let Some(c) = text {
                            return Some(Command::Write(c.as_bytes().to_vec()));
                        }
                    }
                },
                Key::Named(code) => {
                    // Intercept Shift+PageUp/PageDown for scrollback
                    // navigation when not in alt-screen mode.
                    // In alt-screen mode (e.g. vim, less), let the
                    // escape sequence reach the PTY so the application
                    // handles it natively.
                    if !last_content.terminal_mode.contains(TermMode::ALT_SCREEN)
                        && *modifiers == Modifiers::SHIFT
                    {
                        let screen_lines = last_content.screen_lines as i32;
                        if screen_lines > 0 {
                            match code {
                                Named::PageUp => {
                                    return Some(Command::Scroll(screen_lines));
                                },
                                Named::PageDown => {
                                    return Some(Command::Scroll(-screen_lines));
                                },
                                _ => {},
                            }
                        }
                    }

                    binding_action = self.term.bindings.get_action(
                        InputKind::KeyCode(*code),
                        *modifiers,
                        last_content.terminal_mode,
                    );
                },
                _ => {},
            },
            _ => {},
        }

        match binding_action {
            BindingAction::Char(c) => {
                let mut buf = [0, 0, 0, 0];
                let str = c.encode_utf8(&mut buf);
                return Some(Command::Write(str.as_bytes().to_vec()));
            },
            BindingAction::Esc(seq) => {
                return Some(Command::Write(seq.as_bytes().to_vec()));
            },
            BindingAction::Paste => {
                if let Some(data) = clipboard.read(ClipboardKind::Standard) {
                    let input: Vec<u8> = if last_content.terminal_mode.contains(TermMode::BRACKETED_PASTE) {
                        let mut v = Vec::with_capacity(6 + data.len() + 6);
                        v.extend_from_slice(b"\x1b[200~");
                        v.extend(data.bytes());
                        v.extend_from_slice(b"\x1b[201~");
                        v
                    } else {
                        data.bytes().collect()
                    };
                    return Some(Command::Write(input));
                }
            },
            BindingAction::Copy => {
                clipboard.write(
                    ClipboardKind::Standard,
                    self.term.backend.selectable_content(),
                );
            },
            _ => {},
        };

        None
    }

    fn handle_input_method_event(
        &self,
        state: &mut TerminalViewState,
        event: &input_method::Event,
    ) -> Option<Command> {
        match event {
            input_method::Event::Opened => {
                state.ime_active = true;
                state.ime_preedit = None;
                None
            },
            input_method::Event::Preedit(content, selection) => {
                state.ime_active = true;
                state.ime_preedit = if content.is_empty() {
                    None
                } else {
                    Some(input_method::Preedit {
                        content: content.clone(),
                        selection: selection.clone(),
                        text_size: Some(iced_core::Pixels(self.term.font.size)),
                    })
                };
                None
            },
            input_method::Event::Commit(text) => {
                state.ime_active = true;
                state.ime_preedit = None;
                Some(Command::Write(text.as_bytes().to_vec()))
            },
            input_method::Event::Closed => {
                state.ime_active = false;
                state.ime_preedit = None;
                None
            },
        }
    }

    fn input_method<'b>(
        &self,
        state: &'b TerminalViewState,
        layout: iced::advanced::Layout<'_>,
    ) -> InputMethod<&'b str> {
        if !state.is_focused() {
            return InputMethod::Disabled;
        }

        let content = self.term.backend.renderable_content();
        let terminal_size = content.terminal_size;
        let cursor_point = content.cursor_point;
        let display_offset = content.display_offset as f32;
        let cell_width = terminal_size.cell_width as f32;
        let cell_height = terminal_size.cell_height as f32;
        let x = layout.position().x + (cursor_point.column.0 as f32 * cell_width);
        let y = layout.position().y + ((cursor_point.line.0 as f32 + display_offset) * cell_height);

        InputMethod::Enabled {
            cursor: Rectangle::new(
                Point::new(x, y),
                Size::new(cell_width.max(1.0), cell_height.max(1.0)),
            ),
            purpose: Purpose::Terminal,
            preedit: state.ime_preedit.as_ref().map(input_method::Preedit::as_ref),
        }
    }
}

impl Widget<Event, Theme, iced::Renderer> for TerminalView<'_> {
    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<TerminalViewState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(TerminalViewState::new())
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &iced_core::layout::Limits,
    ) -> iced_core::layout::Node {
        let size = limits.resolve(Length::Fill, Length::Fill, Size::ZERO);
        iced::advanced::layout::Node::new(size)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: iced_core::Layout<'_>,
        _renderer: &iced::Renderer,
        operation: &mut dyn operation::Operation,
    ) {
        let state = tree.state.downcast_mut::<TerminalViewState>();
        let wid = self.term.widget_id();
        operation.focusable(Some(wid), layout.bounds(), state);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout,
        cursor: Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<TerminalViewState>();
        let content = self.term.backend.renderable_content();
        let term_size = content.terminal_size;
        let cell_width = term_size.cell_width as f32;
        let cell_height = term_size.cell_height as f32;
        let font_size = self.term.font.size;
        let font_scale_factor = self.term.font.scale_factor;
        let layout_offset_x = layout.position().x;
        let layout_offset_y = layout.position().y;

        let geom = self.term.cache.draw(renderer, viewport.size(), |frame| {
            // Precompute constants used in the inner loop
            let display_offset = content.display_offset as f32;
            let cell_size = Size::new(cell_width, cell_height);
            let half_w = cell_width * 0.5;
            let half_h = cell_height * 0.5;
            // We use the background pallete color as a default
            // because the widget global background color must be the same
            let default_bg = self
                .term
                .theme
                .get_color(ansi::Color::Named(NamedColor::Background));

            let mut last_line: Option<i32> = None;
            let mut bg_batch_rect = BackgroundRect::default();

            for indexed in &content.visible_cells {
                // Compute per-cell geometry cheaply
                let line = indexed.point.line.0;
                let col = indexed.point.column.0 as f32;
                let cell = &indexed.cell;

                // Resolve position point for this cell
                let x = layout_offset_x + (col * cell_width);
                let y = layout_offset_y
                    + (((line as f32) + display_offset) * cell_height);
                let cell_center_y = y + half_h;
                let cell_center_x = if indexed
                    .cell
                    .flags
                    .contains(cell::Flags::WIDE_CHAR)
                {
                    x + cell_width
                } else {
                    x + half_w
                };

                // Resolve colors for this cell
                let mut fg = self.term.theme.get_color(cell.fg);
                let mut bg = self.term.theme.get_color(cell.bg);

                // If the new line was detected,
                // need to flush pending background rect and init the new one
                if last_line != Some(line) {
                    if bg_batch_rect.can_flush() {
                        let line = last_line.unwrap_or(line);
                        frame.fill(
                            &bg_batch_rect.build(line),
                            bg_batch_rect.color,
                        );
                    }

                    last_line = Some(line);
                    bg_batch_rect = BackgroundRect::default()
                        .with_cell_height(cell_height)
                        .with_display_offset(display_offset)
                        .with_layout_offset_y(layout_offset_y);
                }

                // NOTE: DIM / DIM_BOLD flags are intentionally not handled via
                // foreground alpha reduction here. Reducing alpha blends the
                // text into whatever background is behind it, which produces
                // inconsistent dimming strength depending on the current
                // background color. For a uniform dim appearance, the
                // terminal emulator should instead pick an explicitly darker
                // color from the palette when these flags are set.
                if cell.flags.contains(cell::Flags::INVERSE)
                    || content
                        .selectable_range
                        .is_some_and(|r| r.contains(indexed.point))
                {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Batch draw backgrounds: skip default background (container already paints it)
                if bg != default_bg {
                    if bg_batch_rect.can_extend(bg, x) {
                        // Same color and contiguous: extend current run
                        bg_batch_rect.extend(cell_width);
                    } else {
                        // New colored run (or non-contiguous): flush previous run if any
                        if bg_batch_rect.can_flush() {
                            frame.fill(
                                &bg_batch_rect.build(line),
                                bg_batch_rect.color,
                            );
                        }

                        // Start a new run but do not draw yet; wait for potential extensions
                        bg_batch_rect = BackgroundRect::default()
                            .with_cell_height(cell_height)
                            .with_display_offset(display_offset)
                            .with_layout_offset_y(layout_offset_y)
                            .activate()
                            .with_color(bg)
                            .with_start_x(x)
                            .with_width(cell_width);
                    }
                } else if bg_batch_rect.can_flush() {
                    // Background returns to default, flush current background rect and init the new one
                    frame.fill(&bg_batch_rect.build(line), bg_batch_rect.color);

                    bg_batch_rect = BackgroundRect::default()
                        .with_cell_height(cell_height)
                        .with_display_offset(display_offset)
                        .with_layout_offset_y(layout_offset_y);
                }

                // Draw hovered hyperlink underline (only when Ctrl is held and mouse is over the link)
                if content.hovered_hyperlink.as_ref().is_some_and(|hm| {
                    hm.range.contains(&indexed.point)
                        && hm.range.contains(&state.mouse_position_on_grid)
                }) || cell.flags.contains(cell::Flags::UNDERLINE)
                {
                    let underline_height = y + cell_size.height;
                    let underline = Path::line(
                        Point::new(x, underline_height),
                        Point::new(x + cell_size.width, underline_height),
                    );
                    frame.stroke(
                        &underline,
                        Stroke::default()
                            .with_width(font_size * 0.15)
                            .with_color(fg),
                    );
                }

                // Handle cursor rendering
                if content.cursor_point == indexed.point
                    && content.terminal_mode.contains(TermMode::SHOW_CURSOR)
                {
                    let cursor_color =
                        self.term.theme.get_color(content.cursor.fg);
                    let cursor_rect =
                        Path::rectangle(Point::new(x, y), cell_size);
                    frame.fill(&cursor_rect, cursor_color);
                }

                // Draw text
                if cell.c != ' ' && cell.c != '\t' {
                    if content.cursor_point == indexed.point
                        && content.terminal_mode.contains(TermMode::APP_CURSOR)
                    {
                        fg = bg;
                    }
                    // Resolve font style (bold/italic) from cell flags
                    let mut font = self.term.font.font_type;
                    if cell
                        .flags
                        .intersects(cell::Flags::BOLD | cell::Flags::DIM_BOLD)
                    {
                        font.weight = FontWeight::Bold;
                    }
                    if cell.flags.contains(cell::Flags::ITALIC) {
                        font.style = FontStyle::Italic;
                    }
                    let text = Text {
                        content: cell.c.to_string(),
                        position: Point::new(cell_center_x, cell_center_y),
                        font,
                        size: iced_core::Pixels(font_size),
                        color: fg,
                        align_x: Alignment::Center,
                        align_y: Vertical::Center,
                        shaping: Shaping::Advanced,
                        line_height: LineHeight::Relative(font_scale_factor),
                        ..Default::default()
                    };
                    frame.fill_text(text);
                }
            }

            // Flush any remaining background run at the end
            if bg_batch_rect.can_flush() {
                frame.fill(
                    &bg_batch_rect.build(last_line.unwrap_or(0)),
                    bg_batch_rect.color,
                );
            }

            // Only draw the scrollbar when the host app has it visible
            // (auto-hide timer managed externally).  Always show during an
            // active scrollbar drag so the thumb follows the pointer.
            if self.term.scrollbar_visible.get() || state.scrollbar_drag.is_some() {
                if let Some(scrollbar) =
                    Self::scrollbar_geometry(layout.bounds(), content)
                {
                    let hovered = cursor.position().is_some_and(|position| {
                        Self::contains(scrollbar.track, position)
                    });
                    let active = hovered || state.scrollbar_drag.is_some();
                    let track_color = Self::with_alpha(
                        self.term
                            .theme
                            .get_color(ansi::Color::Named(NamedColor::BrightBlack)),
                        if active { 0.28 } else { 0.18 },
                    );
                    let thumb_color = if state.scrollbar_drag.is_some() {
                        Self::with_alpha(
                            self.term.theme.get_color(ansi::Color::Named(
                                NamedColor::BrightForeground,
                            )),
                            0.82,
                        )
                    } else if hovered {
                        Self::with_alpha(
                            self.term.theme.get_color(ansi::Color::Named(
                                NamedColor::Foreground,
                            )),
                            0.72,
                        )
                    } else {
                        Self::with_alpha(
                            self.term.theme.get_color(ansi::Color::Named(
                                NamedColor::DimForeground,
                            )),
                            0.64,
                        )
                    };

                    let track = Path::rounded_rectangle(
                        Point::new(scrollbar.track.x, scrollbar.track.y),
                        Size::new(scrollbar.track.width, scrollbar.track.height),
                        SCROLLBAR_RADIUS.into(),
                    );
                    frame.fill(&track, track_color);

                    let thumb = Path::rounded_rectangle(
                        Point::new(scrollbar.thumb.x, scrollbar.thumb.y),
                        Size::new(scrollbar.thumb.width, scrollbar.thumb.height),
                        SCROLLBAR_RADIUS.into(),
                    );
                    frame.fill(&thumb, thumb_color);
                }
            }
        });

        use iced::advanced::graphics::geometry::Renderer as _;
        renderer.draw_geometry(geom);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced_core::Event,
        layout: iced_graphics::core::Layout<'_>,
        cursor: Cursor,
        _renderer: &iced::Renderer,
        clipboard: &mut dyn iced_graphics::core::Clipboard,
        shell: &mut iced_graphics::core::Shell<'_, Event>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<TerminalViewState>();
        self.handle_resize(state, layout, shell);

        let is_cursor_in_layout = self.is_cursor_in_layout(cursor, layout);

        // Update focus before dispatching mouse events so the first click on
        // a terminal that is not yet focused can both focus it and start a
        // text selection in the same press. This is especially important when
        // the terminal find bar has focus: clicking terminal text should blur
        // the find bar and begin selecting immediately.
        self.handle_focus(event, state, is_cursor_in_layout);

        if let iced::Event::Mouse(iced_core::mouse::Event::ButtonReleased(
            iced_core::mouse::Button::Left,
        )) = event
        {
            if state.scrollbar_drag.is_some() {
                if let Some(command) = Self::scrollbar_drag_release_command(
                    state,
                    self.term.backend.renderable_content(),
                ) {
                    shell.publish(Event::BackendCall(self.term.id, command));
                }
                shell.capture_event();
                return;
            }
            // Selection drag release when the button is released outside the
            // window: the main dispatch path requires a cursor position and
            // would miss this event, so we clean up the drag state here.
            if state.is_dragged && !is_cursor_in_layout {
                state.is_dragged = false;
                state.drag_scroll = None;
            }
        }

        let commands = match event {
            iced::Event::Mouse(mouse_event)
                if is_cursor_in_layout
                    || state.is_dragged
                    || state.scrollbar_drag.is_some() =>
            {
                match cursor.position() {
                    Some(cursor_position) => self.handle_mouse_event(
                        state,
                        layout.bounds(),
                        cursor_position,
                        mouse_event,
                        clipboard,
                    ),
                    None => {
                        // Clear auto-scroll so we don't keep scrolling while
                        // the cursor is outside the window, but keep is_dragged
                        // so selection can resume when the cursor re-enters.
                        state.drag_scroll = None;
                        Vec::new()
                    },
                }
            },
            iced::Event::Keyboard(keyboard_event) => {
                if !state.is_focused() {
                    return;
                }

                self.handle_keyboard_event(state, clipboard, keyboard_event)
                    .into_iter()
                    .collect()
            },
            iced::Event::InputMethod(input_method_event) => {
                if !state.is_focused() {
                    return;
                }

                self.handle_input_method_event(state, input_method_event)
                    .into_iter()
                    .collect()
            },
            iced::Event::Window(iced::window::Event::RedrawRequested(_)) => {
                shell.request_input_method(&self.input_method(state, layout));
                let mut commands = Vec::new();
                Self::handle_drag_scroll_frame(state, &mut commands);
                Self::handle_scrollbar_drag_frame(
                    state,
                    self.term.backend.renderable_content(),
                    &mut commands,
                );
                commands
            },
            _ => Vec::new(),
        };

        if state.drag_scroll.is_some() || state.scrollbar_drag_dirty {
            shell.request_redraw();
        }

        if !commands.is_empty() {
            shell.capture_event();
        }

        for cmd in commands {
            shell.publish(Event::BackendCall(self.term.id, cmd));
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: iced_core::Layout<'_>,
        cursor: iced_core::mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> iced_core::mouse::Interaction {
        let state = tree.state.downcast_ref::<TerminalViewState>();
        let mut cursor_mode = iced_core::mouse::Interaction::Idle;
        let terminal_mode =
            self.term.backend.renderable_content().terminal_mode;
        if self.is_cursor_in_layout(cursor, layout)
            && !terminal_mode.contains(TermMode::SGR_MOUSE)
        {
            cursor_mode = iced_core::mouse::Interaction::Text;
        }

        if let Some(cursor_position) = cursor.position() {
            if let Some(scrollbar) = Self::scrollbar_geometry(
                layout.bounds(),
                self.term.backend.renderable_content(),
            ) {
                if Self::contains(scrollbar.track, cursor_position) {
                    cursor_mode = iced_core::mouse::Interaction::Pointer;
                }
            }
        }

        if self.is_cursor_hovered_hyperlink(state) {
            cursor_mode = iced_core::mouse::Interaction::Pointer;
        }

        cursor_mode
    }
}

impl<'a> From<TerminalView<'a>> for Element<'a, Event, Theme, iced::Renderer> {
    fn from(widget: TerminalView<'a>) -> Self {
        Self::new(widget)
    }
}

#[derive(Debug, Clone)]
struct TerminalViewState {
    focus: bool,
    is_dragged: bool,
    drag_scroll: Option<DragScrollState>,
    scrollbar_drag: Option<ScrollbarDragState>,
    scrollbar_drag_dirty: bool,
    last_click: Option<mouse::Click>,
    scroll_pixels: f32,
    keyboard_modifiers: Modifiers,
    size: Size<f32>,
    /// The absolute position of the widget within the window.
    /// Used to detect pane movement (e.g. pane-grid drag-and-drop)
    /// and invalidate the geometry cache.
    position: Point,
    mouse_position_on_grid: TerminalGridPoint,
    /// The terminal ID this state belongs to.
    /// Used to detect terminal switches and force a resize.
    term_id: u64,
    ime_active: bool,
    ime_preedit: Option<input_method::Preedit>,
}

impl TerminalViewState {
    fn new() -> Self {
        Self {
            focus: false,
            is_dragged: false,
            drag_scroll: None,
            scrollbar_drag: None,
            scrollbar_drag_dirty: false,
            last_click: None,
            scroll_pixels: 0.0,
            keyboard_modifiers: Modifiers::empty(),
            size: Size::from([0.0, 0.0]),
            position: Point::ORIGIN,
            mouse_position_on_grid: TerminalGridPoint::default(),
            term_id: 0,
            ime_active: false,
            ime_preedit: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DragScrollState {
    delta: i32,
    selection_position: (f32, f32),
}

#[derive(Debug, Clone, Copy)]
struct ScrollbarDragState {
    pointer_offset_y: f32,
    target_display_offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct ScrollbarGeometry {
    track: Rectangle,
    thumb: Rectangle,
    history_size: usize,
    display_offset: usize,
}

impl Default for TerminalViewState {
    fn default() -> Self {
        Self::new()
    }
}

impl operation::Focusable for TerminalViewState {
    fn is_focused(&self) -> bool {
        self.focus
    }

    fn focus(&mut self) {
        self.focus = true;
    }

    fn unfocus(&mut self) {
        self.focus = false;
    }
}

#[derive(Default)]
struct BackgroundRect {
    display_offset: f32,
    cell_height: f32,
    layout_offset_y: f32,
    is_active: bool,
    color: Color,
    start_x: f32,
    width: f32,
}

impl BackgroundRect {
    fn with_display_offset(mut self, value: f32) -> Self {
        self.display_offset = value;
        self
    }

    fn with_cell_height(mut self, value: f32) -> Self {
        self.cell_height = value;
        self
    }

    fn with_layout_offset_y(mut self, value: f32) -> Self {
        self.layout_offset_y = value;
        self
    }

    fn with_width(mut self, value: f32) -> Self {
        self.width = value;
        self
    }

    fn with_start_x(mut self, value: f32) -> Self {
        self.start_x = value;
        self
    }

    fn with_color(mut self, value: Color) -> Self {
        self.color = value;
        self
    }

    fn activate(mut self) -> Self {
        self.is_active = true;
        self
    }

    fn build(&self, line: i32) -> Path {
        let flush_y = self.layout_offset_y
            + ((line as f32 + self.display_offset) * self.cell_height);
        Path::rectangle(
            Point::new(self.start_x, flush_y),
            Size::new(self.width, self.cell_height),
        )
    }

    fn can_flush(&self) -> bool {
        self.is_active && self.width > 0.0
    }

    fn can_extend(&self, bg: Color, x: f32) -> bool {
        self.is_active
            && bg == self.color
            && (self.start_x + self.width - x).abs() < f32::EPSILON
    }

    fn extend(&mut self, value: f32) {
        self.width += value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod handle_left_button_pressed_tests {
        use super::*;
        use alacritty_terminal::index::{Column, Line};

        #[test]
        fn handles_mouse_mode_with_left_click() {
            let mut state = TerminalViewState::new();
            let terminal_mode = TermMode::MOUSE_MODE;
            let layout_position = Point { x: 5.0, y: 5.0 };
            let cursor_position = Point { x: 100.0, y: 150.0 };
            let mut commands = Vec::new();
            let _modifiers = Modifiers::empty();

            TerminalView::handle_left_button_pressed(
                &mut state,
                &terminal_mode,
                cursor_position,
                layout_position,
                false,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::MouseReport(
                    MouseButton::LeftButton,
                    _modifiers,
                    TerminalGridPoint {
                        line: Line(0),
                        column: Column(0),
                    },
                    true,
                )
            ));
            assert!(state.is_dragged);
        }

        #[test]
        fn starts_simple_selection_with_left_click() {
            let terminal_mode = TermMode::SGR_MOUSE;
            let cursor_position = Point { x: 200.0, y: 150.0 };
            let layout_position = Point { x: 50.0, y: 50.0 };

            let cases = vec![
                SelectionType::Simple,
                SelectionType::Semantic,
                SelectionType::Lines,
            ];

            for _selection_type in cases {
                let mut state = TerminalViewState::new();
                state.keyboard_modifiers = Modifiers::SHIFT;
                let mut commands = Vec::new();

                TerminalView::handle_left_button_pressed(
                    &mut state,
                    &terminal_mode,
                    cursor_position,
                    layout_position,
                    false,
                    &mut commands,
                );

                assert_eq!(commands.len(), 1);
                assert!(matches!(
                    commands[0],
                    Command::SelectStart(_selection_type, (150.0, 100.0))
                ),);
                assert!(state.is_dragged);
            }
        }
    }

    mod handle_cursor_moved_tests {
        use alacritty_terminal::index::{Column, Line};

        use super::*;

        #[test]
        fn updates_mouse_position_on_grid() {
            let mut state = TerminalViewState::new();
            let terminal_content = RenderableContent::default();
            let mut commands = Vec::new();
            let cases = vec![
                (
                    Point { x: 0.0, y: 0.0 },
                    Point { x: 1.0, y: 1.0 },
                    TerminalGridPoint {
                        line: Line(1),
                        column: Column(1),
                    },
                ),
                (
                    Point { x: 0.0, y: 0.0 },
                    Point { x: 2.0, y: 2.0 },
                    TerminalGridPoint {
                        line: Line(2),
                        column: Column(2),
                    },
                ),
                (
                    Point { x: 0.0, y: 0.0 },
                    Point { x: 30.0, y: 2.0 },
                    TerminalGridPoint {
                        line: Line(2),
                        column: Column(30),
                    },
                ),
                (
                    Point { x: 10.0, y: 0.0 },
                    Point { x: 30.0, y: 2.0 },
                    TerminalGridPoint {
                        line: Line(2),
                        column: Column(20),
                    },
                ),
                (
                    Point { x: 10.0, y: 10.0 },
                    Point { x: 30.0, y: 2.0 },
                    TerminalGridPoint {
                        line: Line(0),
                        column: Column(20),
                    },
                ),
            ];

            for (layout_position, cursor_position, expected) in cases {
                let layout_bounds = Rectangle {
                    x: layout_position.x,
                    y: layout_position.y,
                    width: 80.0,
                    height: 50.0,
                };
                TerminalView::handle_cursor_moved(
                    &mut state,
                    &terminal_content,
                    &cursor_position,
                    layout_bounds,
                    &mut commands,
                );

                assert_eq!(state.mouse_position_on_grid, expected);
            }
        }

        #[test]
        fn generates_drag_update_command_when_dragged() {
            let mut state = TerminalViewState::new();
            state.is_dragged = true; // Simulate an ongoing drag operation
            let terminal_content = RenderableContent::default();
            let layout_bounds = Rectangle {
                x: 5.0,
                y: 5.0,
                width: 80.0,
                height: 200.0,
            };
            let cursor_position = Point { x: 100.0, y: 150.0 };
            let mut commands = Vec::new();

            TerminalView::handle_cursor_moved(
                &mut state,
                &terminal_content,
                &cursor_position,
                layout_bounds,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::SelectUpdate((95.0, 145.0))
            ));
        }

        #[test]
        fn generates_drag_update_command_when_dragged_in_mouse_motion_mode() {
            let mut state = TerminalViewState::new();
            state.is_dragged = true; // Simulate an ongoing drag operation
            let mut terminal_content = RenderableContent::default();
            terminal_content.terminal_mode = TermMode::MOUSE_MOTION;
            let layout_bounds = Rectangle {
                x: 5.0,
                y: 5.0,
                width: 80.0,
                height: 200.0,
            };
            let cursor_position = Point { x: 100.0, y: 150.0 };
            let mut commands = Vec::new();
            let _modifiers = Modifiers::empty();

            TerminalView::handle_cursor_moved(
                &mut state,
                &terminal_content,
                &cursor_position,
                layout_bounds,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::MouseReport(
                    MouseButton::LeftMove,
                    _modifiers,
                    TerminalGridPoint {
                        line: Line(49),
                        column: Column(79),
                    },
                    true,
                )
            ));
        }

        #[test]
        fn generates_drag_update_command_when_dragged_in_srg_mode_with_key_mods(
        ) {
            let mut state = TerminalViewState::new();
            state.keyboard_modifiers = Modifiers::SHIFT;
            state.is_dragged = true; // Simulate an ongoing drag operation
            let mut terminal_content = RenderableContent::default();
            terminal_content.terminal_mode = TermMode::SGR_MOUSE;
            let layout_bounds = Rectangle {
                x: 5.0,
                y: 5.0,
                width: 80.0,
                height: 200.0,
            };
            let cursor_position = Point { x: 100.0, y: 150.0 };
            let mut commands = Vec::new();

            TerminalView::handle_cursor_moved(
                &mut state,
                &terminal_content,
                &cursor_position,
                layout_bounds,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::SelectUpdate((95.0, 145.0))
            ));
        }

        #[test]
        fn generates_drag_update_and_link_open() {
            let mut state = TerminalViewState::new();
            state.keyboard_modifiers = Modifiers::COMMAND;
            state.is_dragged = true; // Simulate an ongoing drag operation
            let mut terminal_content = RenderableContent::default();
            terminal_content.terminal_mode = TermMode::SGR_MOUSE;
            let layout_bounds = Rectangle {
                x: 5.0,
                y: 5.0,
                width: 80.0,
                height: 200.0,
            };
            let cursor_position = Point { x: 100.0, y: 150.0 };
            let mut commands = Vec::new();

            TerminalView::handle_cursor_moved(
                &mut state,
                &terminal_content,
                &cursor_position,
                layout_bounds,
                &mut commands,
            );

            assert_eq!(commands.len(), 2);
            assert!(matches!(
                commands[0],
                Command::SelectUpdate((95.0, 145.0))
            ));
            assert!(matches!(
                commands[1],
                Command::ProcessLink(
                    LinkAction::Hover,
                    TerminalGridPoint {
                        line: Line(49),
                        column: Column(79),
                    },
                )
            ));
        }

        #[test]
        fn generates_scroll_before_drag_update_outside_layout() {
            let mut state = TerminalViewState::new();
            state.is_dragged = true;
            let terminal_content = RenderableContent::default();
            let layout_bounds = Rectangle {
                x: 5.0,
                y: 5.0,
                width: 80.0,
                height: 50.0,
            };
            let cursor_position = Point { x: 100.0, y: 80.0 };
            let mut commands = Vec::new();

            TerminalView::handle_cursor_moved(
                &mut state,
                &terminal_content,
                &cursor_position,
                layout_bounds,
                &mut commands,
            );

            assert_eq!(commands.len(), 2);
            assert!(matches!(commands[0], Command::Scroll(-8)));
            assert!(matches!(commands[1], Command::SelectUpdate((95.0, 75.0))));
            assert!(matches!(
                state.drag_scroll,
                Some(DragScrollState {
                    delta: -8,
                    selection_position: (95.0, 75.0),
                })
            ));
        }

        #[test]
        fn continues_drag_scroll_on_redraw_frame() {
            let mut state = TerminalViewState::new();
            state.is_dragged = true;
            state.drag_scroll = Some(DragScrollState {
                delta: -3,
                selection_position: (12.0, 60.0),
            });
            let mut commands = Vec::new();

            TerminalView::handle_drag_scroll_frame(&state, &mut commands);

            assert_eq!(commands.len(), 2);
            assert!(matches!(commands[0], Command::Scroll(-3)));
            assert!(matches!(commands[1], Command::SelectUpdate((12.0, 60.0))));
        }
    }

    mod handle_button_released_tests {
        use super::*;
        use alacritty_terminal::index::{Column, Line};

        #[test]
        fn mouse_mode_activated() {
            let mut state = TerminalViewState::new();
            let terminal_mode = TermMode::MOUSE_MODE;
            let bindings = BindingsLayout::new();
            let mut commands = Vec::new();
            let _modifiers = Modifiers::empty();

            TerminalView::handle_button_released(
                &mut state,
                &terminal_mode,
                &bindings,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::MouseReport(
                    MouseButton::LeftButton,
                    _modifiers,
                    TerminalGridPoint {
                        line: Line(0),
                        column: Column(0)
                    },
                    false
                )
            ));
        }

        #[test]
        fn link_open_on_button_release() {
            let mut state = TerminalViewState::new();
            state.keyboard_modifiers = Modifiers::COMMAND;
            let terminal_mode = TermMode::MOUSE_MODE;
            let bindings = BindingsLayout::new();
            let mut commands = Vec::new();
            let _modifiers = Modifiers::empty();

            TerminalView::handle_button_released(
                &mut state,
                &terminal_mode,
                &bindings,
                &mut commands,
            );

            assert_eq!(commands.len(), 2);
            assert!(matches!(
                commands[0],
                Command::MouseReport(
                    MouseButton::LeftButton,
                    _modifiers,
                    TerminalGridPoint {
                        line: Line(0),
                        column: Column(0)
                    },
                    false
                )
            ));
            assert!(matches!(
                commands[1],
                Command::ProcessLink(
                    LinkAction::Open,
                    TerminalGridPoint {
                        line: Line(0),
                        column: Column(0)
                    }
                ),
            ));
        }

        #[test]
        fn link_open_on_button_release_in_non_mouse_mode() {
            let mut state = TerminalViewState::new();
            state.keyboard_modifiers = Modifiers::COMMAND;
            state.mouse_position_on_grid = TerminalGridPoint {
                line: Line(4),
                column: Column(10),
            };
            let terminal_mode = TermMode::empty(); // Assume SGR_MOUSE mode doesn't affect link opening
            let bindings = BindingsLayout::new();
            let mut commands = Vec::new();

            TerminalView::handle_button_released(
                &mut state,
                &terminal_mode,
                &bindings,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0],
                Command::ProcessLink(
                    LinkAction::Open,
                    TerminalGridPoint {
                        line: Line(4),
                        column: Column(10)
                    }
                ),
            ));
        }
    }

    mod handle_input_method_event_tests {
        use super::*;

        #[test]
        fn opens_and_closes_ime_state() {
            let terminal = Terminal::new(0, crate::settings::Settings::default())
                .expect("terminal created");
            let view = TerminalView { term: &terminal };
            let mut state = TerminalViewState::new();

            assert!(!state.ime_active);
            assert!(state.ime_preedit.is_none());

            let opened = view.handle_input_method_event(
                &mut state,
                &input_method::Event::Opened,
            );
            assert!(opened.is_none());
            assert!(state.ime_active);

            let closed = view.handle_input_method_event(
                &mut state,
                &input_method::Event::Closed,
            );
            assert!(closed.is_none());
            assert!(!state.ime_active);
            assert!(state.ime_preedit.is_none());
        }

        #[test]
        fn stores_preedit_without_writing() {
            let terminal = Terminal::new(0, crate::settings::Settings::default())
                .expect("terminal created");
            let view = TerminalView { term: &terminal };
            let mut state = TerminalViewState::new();

            let result = view.handle_input_method_event(
                &mut state,
                &input_method::Event::Preedit("ni".into(), Some(0..2)),
            );

            assert!(result.is_none());
            assert!(state.ime_active);
            let preedit = state.ime_preedit.expect("preedit stored");
            assert_eq!(preedit.content, "ni");
            assert_eq!(preedit.selection, Some(0..2));
            assert_eq!(preedit.text_size, Some(iced_core::Pixels(terminal.font.size)));
        }

        #[test]
        fn clears_preedit_on_empty_preedit() {
            let terminal = Terminal::new(0, crate::settings::Settings::default())
                .expect("terminal created");
            let view = TerminalView { term: &terminal };
            let mut state = TerminalViewState::new();
            state.ime_preedit = Some(input_method::Preedit {
                content: "ni".into(),
                selection: Some(0..2),
                text_size: Some(iced_core::Pixels(terminal.font.size)),
            });

            let result = view.handle_input_method_event(
                &mut state,
                &input_method::Event::Preedit(String::new(), None),
            );

            assert!(result.is_none());
            assert!(state.ime_preedit.is_none());
        }

        #[test]
        fn commits_utf8_text_once() {
            let terminal = Terminal::new(0, crate::settings::Settings::default())
                .expect("terminal created");
            let view = TerminalView { term: &terminal };
            let mut state = TerminalViewState::new();
            state.ime_preedit = Some(input_method::Preedit {
                content: "ni".into(),
                selection: Some(0..2),
                text_size: Some(iced_core::Pixels(terminal.font.size)),
            });

            let result = view.handle_input_method_event(
                &mut state,
                &input_method::Event::Commit("你".into()),
            );

            assert!(matches!(result, Some(Command::Write(bytes)) if bytes == "你".as_bytes().to_vec()));
            assert!(state.ime_active);
            assert!(state.ime_preedit.is_none());
        }
    }

    mod handle_wheel_scrolled_tests {
        use super::*;
        use crate::font::TermFont;
        use crate::settings::FontSettings;

        #[test]
        fn scroll_with_lines_downward() {
            let mut state = TerminalViewState::new();
            let font = TermFont::new(FontSettings::default());
            let mut commands = Vec::new();

            TerminalView::handle_wheel_scrolled(
                &mut state,
                ScrollDelta::Lines { y: 3.0, x: 0.0 }, // Scroll down 3 lines
                &font.measure,
                1.0,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(commands[0], Command::Scroll(3)));
        }

        #[test]
        fn scroll_with_lines_upward() {
            let mut state = TerminalViewState::new();
            let font = TermFont::new(FontSettings::default());
            let mut commands = Vec::new();

            TerminalView::handle_wheel_scrolled(
                &mut state,
                ScrollDelta::Lines { y: -2.0, x: 0.0 },
                &font.measure,
                1.0,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(commands[0], Command::Scroll(-2)));
        }

        #[test]
        fn scroll_with_pixels_accumulating_downward() {
            let mut state = TerminalViewState::new();
            let font = TermFont::new(FontSettings::default());
            let mut commands = Vec::new();

            TerminalView::handle_wheel_scrolled(
                &mut state,
                ScrollDelta::Pixels { y: 45.0, x: 0.0 },
                &font.measure,
                1.0,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(commands[0], Command::Scroll(-2)));
            assert_eq!(state.scroll_pixels, -8.600002);
        }

        #[test]
        fn scroll_with_pixels_accumulating_upward() {
            let mut state = TerminalViewState::new();
            let font = TermFont::new(FontSettings::default());
            let mut commands = Vec::new();

            TerminalView::handle_wheel_scrolled(
                &mut state,
                ScrollDelta::Pixels { y: -60.0, x: 0.0 },
                &font.measure,
                1.0,
                &mut commands,
            );

            assert_eq!(commands.len(), 1);
            assert!(matches!(commands[0], Command::Scroll(3)));
            assert_eq!(state.scroll_pixels, 5.4000034);
        }
    }
}
