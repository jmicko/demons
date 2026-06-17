use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    io::{self, Read, Stdout, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

use crate::{
    config::{Leader, LoadedConfig, Task, TaskCommand},
    layout::{Grid, choose_grid, pane_rects},
};

const SCROLLBACK_LINES: usize = 10_000;
const RESTART_GRACE: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const EVENT_INTERVAL: Duration = Duration::from_millis(25);
const ALT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);
const MODE_BUTTON_WIDTH: u16 = 13;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(45);
const NOTICE_DURATION: Duration = Duration::from_secs(3);
const MAX_FULL_HISTORY_OSC52_BYTES: usize = 512 * 1024;

type ProcessRegistry = Arc<Mutex<HashSet<u32>>>;

pub fn run(loaded: LoadedConfig) -> Result<()> {
    let registry = Arc::new(Mutex::new(HashSet::new()));
    install_panic_hook(Arc::clone(&registry));
    let shutdown_requested = register_shutdown_signals()?;

    let mut terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to initialize terminal")?;
    terminal.clear().context("failed to clear terminal")?;

    let (tx, rx) = mpsc::sync_channel(1024);
    let mut app = App::new(loaded, tx, rx, registry);
    let initial_size = terminal.size().context("failed to read terminal size")?;
    let initial_area = Rect::new(0, 0, initial_size.width, initial_size.height);
    app.update_layout(initial_area);
    app.spawn_all();

    let loop_result = run_loop(&mut terminal, &mut app, &shutdown_requested);
    let shutdown_result = app.shutdown();
    terminal.show_cursor().ok();
    terminal_guard.restore();

    loop_result.and(shutdown_result)
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    shutdown_requested: &AtomicBool,
) -> Result<()> {
    let mut dirty = true;
    loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            app.mark_stopping();
            terminal.draw(|frame| app.draw(frame)).ok();
            return Ok(());
        }
        dirty |= app.drain_process_events();
        dirty |= app.tick()?;

        if dirty {
            terminal
                .draw(|frame| app.draw(frame))
                .context("failed to draw terminal UI")?;
            dirty = false;
        }

        if event::poll(EVENT_INTERVAL).context("failed to poll terminal input")? {
            let event = event::read().context("failed to read terminal input")?;
            if app.handle_terminal_event(event)? == Action::Quit {
                app.mark_stopping();
                terminal.draw(|frame| app.draw(frame)).ok();
                return Ok(());
            }
            dirty = true;
        }
    }
}

struct App {
    loaded: LoadedConfig,
    tasks: Vec<TaskRuntime>,
    focus: usize,
    mode: AppMode,
    grid: Grid,
    pane_rects: Vec<Rect>,
    content_rects: Vec<Rect>,
    footer_rect: Option<Rect>,
    mode_button_rect: Option<Rect>,
    tx: SyncSender<ProcessEvent>,
    rx: Receiver<ProcessEvent>,
    registry: ProcessRegistry,
    stopping: bool,
    pending_escape: Option<Instant>,
    selection: Option<Selection>,
    clipboard: String,
    notice: Option<Notice>,
    fullscreen: bool,
    search: Option<SearchState>,
    last_search: Option<String>,
    show_help: bool,
}

impl App {
    fn new(
        loaded: LoadedConfig,
        tx: SyncSender<ProcessEvent>,
        rx: Receiver<ProcessEvent>,
        registry: ProcessRegistry,
    ) -> Self {
        let tasks = loaded
            .config
            .tasks
            .iter()
            .cloned()
            .map(|task| {
                let cwd = loaded.task_cwd(&task);
                TaskRuntime::new(task, cwd)
            })
            .collect();
        Self {
            loaded,
            tasks,
            focus: 0,
            mode: AppMode::Input,
            grid: Grid {
                columns: 1,
                rows: 1,
            },
            pane_rects: Vec::new(),
            content_rects: Vec::new(),
            footer_rect: None,
            mode_button_rect: None,
            tx,
            rx,
            registry,
            stopping: false,
            pending_escape: None,
            selection: None,
            clipboard: String::new(),
            notice: None,
            fullscreen: false,
            search: None,
            last_search: None,
            show_help: false,
        }
    }

    fn spawn_all(&mut self) {
        for index in 0..self.tasks.len() {
            self.spawn(index);
        }
    }

    fn spawn(&mut self, index: usize) {
        let size = self.tasks[index].pty_size;
        if let Err(error) =
            self.tasks[index].spawn(index, size, self.tx.clone(), Arc::clone(&self.registry))
        {
            self.tasks[index].record_spawn_error(&error);
        }
    }

    fn update_layout(&mut self, terminal_area: Rect) {
        let (pane_area, footer_rect) = if terminal_area.height > 3 {
            (
                Rect::new(
                    terminal_area.x,
                    terminal_area.y,
                    terminal_area.width,
                    terminal_area.height - 1,
                ),
                Some(Rect::new(
                    terminal_area.x,
                    terminal_area.bottom() - 1,
                    terminal_area.width,
                    1,
                )),
            )
        } else {
            (terminal_area, None)
        };
        self.footer_rect = footer_rect;
        self.mode_button_rect = footer_rect.map(|footer| {
            Rect::new(
                footer.x,
                footer.y,
                footer.width.min(MODE_BUTTON_WIDTH),
                footer.height,
            )
        });
        if self.fullscreen {
            self.grid = Grid {
                columns: 1,
                rows: 1,
            };
            self.pane_rects = vec![Rect::default(); self.tasks.len()];
            self.content_rects = vec![Rect::default(); self.tasks.len()];
            if !self.tasks.is_empty() {
                let focus = self.focus.min(self.tasks.len() - 1);
                self.pane_rects[focus] = pane_area;
                self.content_rects[focus] = Rect::new(
                    pane_area.x.saturating_add(1),
                    pane_area.y.saturating_add(1),
                    pane_area.width.saturating_sub(2),
                    pane_area.height.saturating_sub(2),
                );
                let area = self.content_rects[focus];
                self.tasks[focus].resize(area.width, area.height);
            }
        } else {
            self.grid = choose_grid(self.tasks.len(), pane_area);
            self.pane_rects = pane_rects(pane_area, self.grid, self.tasks.len());
            self.content_rects = self
                .pane_rects
                .iter()
                .map(|rect| {
                    Rect::new(
                        rect.x.saturating_add(1),
                        rect.y.saturating_add(1),
                        rect.width.saturating_sub(2),
                        rect.height.saturating_sub(2),
                    )
                })
                .collect();

            for (task, area) in self.tasks.iter_mut().zip(&self.content_rects) {
                task.resize(area.width, area.height);
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let frame_area = frame.area();
        self.update_layout(frame_area);
        let buffer = frame.buffer_mut();

        for index in 0..self.tasks.len() {
            let area = self.pane_rects[index];
            let content = self.content_rects[index];
            if area.width == 0 || area.height == 0 {
                continue;
            }
            let focused = index == self.focus;
            let border_color = match (focused, self.mode) {
                (true, AppMode::Input) => Color::Cyan,
                (true, AppMode::Command) => Color::Yellow,
                (true, AppMode::Search) => Color::Magenta,
                _ => Color::DarkGray,
            };
            let (status, status_color) = self.tasks[index].status_label();
            let title = Line::from(vec![
                Span::raw(" "),
                Span::styled(status, Style::default().fg(status_color)),
                Span::raw(" "),
                Span::styled(
                    self.tasks[index].task.name.as_str(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]);
            let restart = Line::from(Span::styled(
                " [↻] ",
                Style::default().fg(if focused { Color::Yellow } else { Color::Gray }),
            ))
            .right_aligned();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title)
                .title(restart);
            block.render(area, buffer);

            if self.tasks[index].scroll_offset == 0 {
                render_screen(&self.tasks[index].parser, content, buffer);
            } else {
                render_history(&self.tasks[index], content, buffer);
            }
            render_selection(
                self.selection
                    .as_ref()
                    .filter(|selection| selection.pane == index && selection.dragged),
                &self.tasks[index],
                content,
                buffer,
            );
        }

        if let (Some(footer_area), Some(button_area)) = (self.footer_rect, self.mode_button_rect) {
            let help_area = Rect::new(
                button_area.right(),
                footer_area.y,
                footer_area.width.saturating_sub(button_area.width),
                1,
            );
            let (mode_label, mode_color, help) = if let Some(search) = self
                .search
                .as_ref()
                .filter(|_| self.mode == AppMode::Search)
            {
                ("SEARCH", Color::Magenta, search_footer_text(search))
            } else if let Some(notice) = self.active_notice(Instant::now()) {
                let mode_label = match self.mode {
                    AppMode::Input => "INPUT MODE",
                    AppMode::Command => "COMMAND MODE",
                    AppMode::Search => "SEARCH",
                };
                let mode_color = match self.mode {
                    AppMode::Input => Color::Cyan,
                    AppMode::Command => Color::Yellow,
                    AppMode::Search => Color::Magenta,
                };
                (mode_label, mode_color, format!(" {notice} "))
            } else {
                match self.mode {
                    AppMode::Input => (
                        "INPUT MODE",
                        Color::Cyan,
                        format!(
                            " {} | {}: command | drag: select | right-click: copy ",
                            self.tasks[self.focus].task.name,
                            self.loaded.config.settings.leader.label()
                        ),
                    ),
                    AppMode::Command => (
                        "COMMAND MODE",
                        Color::Yellow,
                        format!(
                            " arrows/hjkl: move | f: fullscreen | / n/N: find | y/Y: copy | S: save | r: restart | R: all | c: clear | q: quit | {}: input ",
                            self.loaded.config.settings.leader.label()
                        ),
                    ),
                    AppMode::Search => (
                        "SEARCH",
                        Color::Magenta,
                        " /  Enter: jump | Esc: cancel ".to_owned(),
                    ),
                }
            };
            Paragraph::new(mode_label)
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().fg(Color::Black).bg(mode_color))
                .render(button_area, buffer);
            Paragraph::new(help)
                .style(Style::default().fg(Color::White).bg(Color::DarkGray))
                .render(help_area, buffer);
        }

        if self.show_help {
            render_command_help(
                frame_area,
                buffer,
                self.loaded.config.settings.leader.label(),
            );
        }

        if self.mode == AppMode::Input {
            let task = &self.tasks[self.focus];
            let area = self.content_rects[self.focus];
            if task.scroll_offset == 0 && !task.parser.screen().hide_cursor() {
                let (row, column) = task.parser.screen().cursor_position();
                if row < area.height && column < area.width {
                    frame.set_cursor_position((area.x + column, area.y + row));
                }
            }
        }
    }

    fn handle_terminal_event(&mut self, event: Event) -> Result<Action> {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                match self.mode {
                    AppMode::Input => self.paste_text_to_task(self.focus, &text)?,
                    AppMode::Search => self.insert_search_text(&text),
                    AppMode::Command => {}
                }
                Ok(Action::Continue)
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => Ok(Action::Continue),
            _ => Ok(Action::Continue),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<Action> {
        if is_copy_key(key) {
            self.copy_selection();
            return Ok(Action::Continue);
        }
        if is_paste_key(key) {
            self.paste_clipboard_to_focus()?;
            return Ok(Action::Continue);
        }

        if self.show_help {
            return self.handle_help_key(key);
        }

        let leader = self.loaded.config.settings.leader;
        if leader == Leader::AltJ {
            if let Some(started) = self.pending_escape.take() {
                if started.elapsed() <= ALT_ESCAPE_TIMEOUT && is_legacy_alt_j(key) {
                    self.toggle_mode();
                    return Ok(Action::Continue);
                }
                self.apply_escape()?;
            }
            if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                self.pending_escape = Some(Instant::now());
                return Ok(Action::Continue);
            }
        }

        if is_leader(key, leader) {
            self.toggle_mode();
            return Ok(Action::Continue);
        }

        if self.mode == AppMode::Search {
            return self.handle_search_key(key);
        }

        if self.mode == AppMode::Input {
            let application_cursor = self.tasks[self.focus].parser.screen().application_cursor();
            let bytes = encode_key(key, application_cursor);
            if !bytes.is_empty() {
                self.tasks[self.focus].write_input(&bytes)?;
            }
            return Ok(Action::Continue);
        }

        match key.code {
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Esc => self.mode = AppMode::Input,
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Left | KeyCode::Char('h') => self.move_focus(Direction::Left),
            KeyCode::Right | KeyCode::Char('l') => self.move_focus(Direction::Right),
            KeyCode::Up | KeyCode::Char('k') => self.move_focus(Direction::Up),
            KeyCode::Down | KeyCode::Char('j') => self.move_focus(Direction::Down),
            KeyCode::PageUp => {
                let rows = self.focused_page_rows();
                self.tasks[self.focus].scroll_up(rows);
            }
            KeyCode::PageDown => {
                let rows = self.focused_page_rows();
                self.tasks[self.focus].scroll_down(rows);
            }
            KeyCode::Home => {
                self.tasks[self.focus].scroll_to_top();
            }
            KeyCode::End => {
                self.tasks[self.focus].scroll_to_bottom();
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('f') => self.fullscreen = !self.fullscreen,
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.repeat_search(SearchDirection::Older),
            KeyCode::Char('N') => self.repeat_search(SearchDirection::Newer),
            KeyCode::Char('y') => self.copy_focused_visible(),
            KeyCode::Char('Y') => self.copy_focused_history(),
            KeyCode::Char('S') => self.save_focused_history()?,
            KeyCode::Char('r') => self.request_restart(self.focus),
            KeyCode::Char('R') => {
                for index in 0..self.tasks.len() {
                    self.request_restart(index);
                }
            }
            KeyCode::Char('q') => return Ok(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Action::Quit);
            }
            KeyCode::Char('c') => {
                self.tasks[self.focus].clear();
                self.clear_selection_for(self.focus);
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => self.show_help = false,
            KeyCode::Char('q') => return Ok(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Action::Quit);
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter | KeyCode::Char('\n' | '\r') => self.finish_search(),
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_search()
            }
            KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = 0;
                }
            }
            KeyCode::Char('e' | 'E') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = char_count(&search.query);
                }
            }
            KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    search.query.clear();
                    search.cursor = 0;
                }
            }
            KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    let index = byte_index_for_char(&search.query, search.cursor);
                    search.query.truncate(index);
                }
            }
            KeyCode::Backspace => self.delete_search_char_before_cursor(),
            KeyCode::Delete => self.delete_search_char_at_cursor(),
            KeyCode::Left => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = search.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = (search.cursor + 1).min(char_count(&search.query));
                }
            }
            KeyCode::Home => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(search) = self.search.as_mut() {
                    search.cursor = char_count(&search.query);
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_search_char(character);
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn start_search(&mut self) {
        self.search = Some(SearchState {
            pane: self.focus,
            query: String::new(),
            cursor: 0,
        });
        self.selection = None;
        self.notice = None;
        self.show_help = false;
        self.mode = AppMode::Search;
    }

    fn cancel_search(&mut self) {
        self.search = None;
        self.mode = AppMode::Command;
    }

    fn finish_search(&mut self) {
        let Some(search) = self.search.take() else {
            self.mode = AppMode::Command;
            return;
        };
        self.mode = AppMode::Command;
        let query = search.query.trim().to_owned();
        if query.is_empty() {
            self.set_notice("Search cancelled.".to_owned());
            return;
        }

        let Some(line) = self.tasks[search.pane].history.find_last_line(&query) else {
            self.set_notice(format!("No matches for {query:?}."));
            return;
        };

        self.last_search = Some(query.clone());
        self.jump_to_search_line(search.pane, line, &query);
    }

    fn repeat_search(&mut self, direction: SearchDirection) {
        let Some(query) = self.last_search.clone() else {
            self.set_notice("No previous search.".to_owned());
            return;
        };
        let pane = self.focus;
        let current_line = self.search_anchor_line(pane);
        let history = &self.tasks[pane].history;
        let line = match direction {
            SearchDirection::Older => history
                .find_line_before(&query, current_line)
                .or_else(|| history.find_last_line(&query)),
            SearchDirection::Newer => history
                .find_line_after(&query, current_line)
                .or_else(|| history.find_first_line(&query)),
        };
        let Some(line) = line else {
            self.set_notice(format!("No matches for {query:?}."));
            return;
        };

        self.jump_to_search_line(pane, line, &query);
    }

    fn search_anchor_line(&self, pane: usize) -> u64 {
        if let Some(selection) = self
            .selection
            .as_ref()
            .filter(|selection| selection.pane == pane)
        {
            return selection.ordered_points().0.line;
        }

        let height = self
            .content_rects
            .get(pane)
            .map(|rect| rect.height)
            .unwrap_or(self.tasks[pane].pty_size.rows);
        self.tasks[pane]
            .history
            .visible_start(height, self.tasks[pane].scroll_offset)
            .saturating_add(u64::from(height / 2))
            .min(self.tasks[pane].history.line_count().saturating_sub(1))
    }

    fn jump_to_search_line(&mut self, pane: usize, line: u64, query: &str) {
        self.focus = pane;
        let height = self
            .content_rects
            .get(pane)
            .map(|rect| rect.height)
            .unwrap_or(self.tasks[pane].pty_size.rows);
        self.tasks[pane].scroll_to_history_line(line, height);

        let end_column = self.tasks[pane]
            .history
            .line_char_count(line)
            .unwrap_or(1)
            .saturating_sub(1)
            .min(usize::from(u16::MAX)) as u16;
        self.selection = Some(Selection {
            pane,
            anchor: SelectionPoint { line, column: 0 },
            cursor: SelectionPoint {
                line,
                column: end_column,
            },
            dragging: false,
            dragged: true,
            last_mouse: None,
            last_scroll: Instant::now(),
        });
        self.set_notice(format!(
            "Found {query:?} in {}.",
            self.tasks[pane].task.name
        ));
    }

    fn insert_search_text(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.insert_search_char(character);
        }
    }

    fn insert_search_char(&mut self, character: char) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let index = byte_index_for_char(&search.query, search.cursor);
        search.query.insert(index, character);
        search.cursor += 1;
    }

    fn delete_search_char_before_cursor(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.cursor == 0 {
            return;
        }
        let start = byte_index_for_char(&search.query, search.cursor - 1);
        let end = byte_index_for_char(&search.query, search.cursor);
        search.query.replace_range(start..end, "");
        search.cursor -= 1;
    }

    fn delete_search_char_at_cursor(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.cursor >= char_count(&search.query) {
            return;
        }
        let start = byte_index_for_char(&search.query, search.cursor);
        let end = byte_index_for_char(&search.query, search.cursor + 1);
        search.query.replace_range(start..end, "");
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<Action> {
        if self.show_help {
            self.show_help = false;
            return Ok(Action::Continue);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.mode_button_hit(mouse.column, mouse.row)
        {
            self.toggle_mode();
            return Ok(Action::Continue);
        }

        if self.mode == AppMode::Search {
            self.cancel_search();
        }

        if self.handle_selection_drag(mouse) {
            return Ok(Action::Continue);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) && self.copy_selection() {
            return Ok(Action::Continue);
        }

        let Some(index) = self.pane_at(mouse.column, mouse.row) else {
            return Ok(Action::Continue);
        };

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.restart_hit(index, mouse.column, mouse.row)
        {
            self.focus = index;
            self.selection = None;
            self.request_restart(index);
            return Ok(Action::Continue);
        }

        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.focus = index;
        }

        let content = self.content_rects[index];
        if !contains(content, mouse.column, mouse.row) {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.selection = None;
            }
            return Ok(Action::Continue);
        }

        let mouse_mode = self.tasks[index].parser.screen().mouse_protocol_mode();
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.should_start_selection(index, mouse)
        {
            self.start_selection(index, mouse);
            return Ok(Action::Continue);
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.selection = None;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            && self.paste_clipboard_to_task(index)?
        {
            return Ok(Action::Continue);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Middle))
            && self.paste_clipboard_to_task(index)?
        {
            return Ok(Action::Continue);
        }

        if self.mode == AppMode::Command || mouse_mode == MouseProtocolMode::None {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.tasks[index].scroll_up(3);
                }
                MouseEventKind::ScrollDown => {
                    self.tasks[index].scroll_down(3);
                }
                _ => {}
            }
            return Ok(Action::Continue);
        }

        if should_forward_mouse(mouse.kind, mouse_mode) {
            let local_x = mouse.column - content.x + 1;
            let local_y = mouse.row - content.y + 1;
            let encoding = self.tasks[index].parser.screen().mouse_protocol_encoding();
            let bytes = encode_mouse(mouse, local_x, local_y, encoding);
            self.tasks[index].write_input(&bytes)?;
        }
        Ok(Action::Continue)
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            AppMode::Input => AppMode::Command,
            AppMode::Command | AppMode::Search => {
                self.search = None;
                self.show_help = false;
                AppMode::Input
            }
        };
    }

    fn mode_button_hit(&self, x: u16, y: u16) -> bool {
        self.mode_button_rect
            .is_some_and(|rect| contains(rect, x, y))
    }

    fn pane_at(&self, x: u16, y: u16) -> Option<usize> {
        self.pane_rects
            .iter()
            .position(|rect| contains(*rect, x, y))
    }

    fn restart_hit(&self, index: usize, x: u16, y: u16) -> bool {
        let area = self.pane_rects[index];
        area.width >= 5
            && y == area.y
            && x >= area.right().saturating_sub(4)
            && x < area.right().saturating_sub(1)
    }

    fn should_start_selection(&self, index: usize, mouse: MouseEvent) -> bool {
        self.mode == AppMode::Command
            || mouse.modifiers.contains(KeyModifiers::SHIFT)
            || self.tasks[index].parser.screen().mouse_protocol_mode() == MouseProtocolMode::None
    }

    fn start_selection(&mut self, index: usize, mouse: MouseEvent) {
        let Some(point) = self.selection_point_for_mouse(index, mouse.column, mouse.row) else {
            return;
        };
        self.focus = index;
        self.selection = Some(Selection {
            pane: index,
            anchor: point,
            cursor: point,
            dragging: true,
            dragged: false,
            last_mouse: Some((mouse.column, mouse.row)),
            last_scroll: Instant::now(),
        });
    }

    fn handle_selection_drag(&mut self, mouse: MouseEvent) -> bool {
        if !self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
        {
            return false;
        }

        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.update_selection_from_mouse(mouse.column, mouse.row, true);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.update_selection_from_mouse(mouse.column, mouse.row, false);
                if let Some(selection) = self.selection.as_mut() {
                    if selection.dragged {
                        selection.dragging = false;
                    } else {
                        self.selection = None;
                    }
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.scroll_selection_from_mouse(mouse.column, mouse.row, true);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_selection_from_mouse(mouse.column, mouse.row, false);
                true
            }
            _ => true,
        }
    }

    fn update_selection_from_mouse(&mut self, x: u16, y: u16, dragged: bool) {
        let Some(pane) = self.selection.as_ref().map(|selection| selection.pane) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.last_mouse = Some((x, y));
            selection.dragged |= dragged;
        }
        self.autoscroll_selection(Instant::now(), true);
        let Some(point) = self.selection_point_for_mouse(pane, x, y) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.cursor = point;
        }
    }

    fn scroll_selection_from_mouse(&mut self, x: u16, y: u16, up: bool) {
        let Some(pane) = self.selection.as_ref().map(|selection| selection.pane) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.last_mouse = Some((x, y));
            selection.dragged = true;
        }
        let changed = if up {
            self.tasks[pane].scroll_up(3)
        } else {
            self.tasks[pane].scroll_down(3)
        };
        if changed {
            let Some(point) = self.selection_point_for_mouse(pane, x, y) else {
                return;
            };
            if let Some(selection) = self.selection.as_mut() {
                selection.cursor = point;
                selection.last_scroll = Instant::now();
            }
        }
    }

    fn selection_point_for_mouse(&self, pane: usize, x: u16, y: u16) -> Option<SelectionPoint> {
        let content = *self.content_rects.get(pane)?;
        if content.width == 0 || content.height == 0 {
            return None;
        }
        let column = if x < content.x {
            0
        } else if x >= content.right() {
            content.width.saturating_sub(1)
        } else {
            x - content.x
        };
        let row = if y < content.y {
            0
        } else if y >= content.bottom() {
            content.height.saturating_sub(1)
        } else {
            y - content.y
        };
        Some(SelectionPoint {
            line: self.tasks[pane].history_index_for_visible_row(row, content.height),
            column,
        })
    }

    fn autoscroll_selection(&mut self, now: Instant, force: bool) -> bool {
        let Some((pane, x, y, last_scroll)) = self.selection.as_ref().and_then(|selection| {
            selection
                .last_mouse
                .map(|(x, y)| (selection.pane, x, y, selection.last_scroll))
        }) else {
            return false;
        };
        if !force && now.duration_since(last_scroll) < SELECTION_AUTOSCROLL_INTERVAL {
            return false;
        }
        let content = self.content_rects[pane];
        let step = selection_scroll_step(content, y);
        let changed = if y < content.y {
            self.tasks[pane].scroll_up(step)
        } else if y >= content.bottom() {
            self.tasks[pane].scroll_down(step)
        } else {
            false
        };
        if changed {
            if let Some(selection) = self.selection.as_mut() {
                selection.last_scroll = now;
            }
            if let Some(point) = self.selection_point_for_mouse(pane, x, y) {
                if let Some(selection) = self.selection.as_mut() {
                    selection.cursor = point;
                }
            }
        }
        changed
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        if !selection.dragged {
            return None;
        }
        if let Some(text) = self.visible_selection_text(selection) {
            if !text.is_empty() {
                return Some(text);
            }
        }
        Some(
            self.tasks[selection.pane]
                .history
                .text_between(selection.anchor, selection.cursor),
        )
        .filter(|text| !text.is_empty())
    }

    fn visible_selection_text(&self, selection: &Selection) -> Option<String> {
        let task = self.tasks.get(selection.pane)?;
        if task.scroll_offset != 0 {
            return None;
        }
        let area = *self.content_rects.get(selection.pane)?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let visible_start = task.history.visible_start(area.height, 0);
        let visible_end = visible_start.saturating_add(u64::from(area.height));
        let (start, end) = selection.ordered_points();
        if start.line < visible_start || end.line >= visible_end {
            return None;
        }

        let start_row = (start.line - visible_start) as u16;
        let end_row = (end.line - visible_start) as u16;
        let end_column = end.column.saturating_add(1).min(area.width);
        Some(task.parser.screen().contents_between(
            start_row,
            start.column.min(area.width),
            end_row,
            end_column,
        ))
    }

    fn copy_selection(&mut self) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        let pane = self.selection.as_ref().map(|selection| selection.pane);
        self.clipboard = text.clone();
        let copied_to_terminal = write_osc52_clipboard(&text).is_ok();
        let chars = text.chars().count();
        let suffix = pane
            .and_then(|pane| self.tasks.get(pane))
            .map(|task| format!(" from {}", task.task.name))
            .unwrap_or_default();
        if copied_to_terminal {
            self.set_notice(format!("Copied {chars} characters{suffix}."));
        } else {
            self.set_notice(format!("Copied {chars} characters internally{suffix}."));
        }
        true
    }

    fn copy_focused_visible(&mut self) {
        let Some(text) = self
            .visible_pane_text(self.focus)
            .filter(|text| !text.is_empty())
        else {
            self.set_notice("Focused pane has no visible text.".to_owned());
            return;
        };
        self.clipboard = text.clone();
        let copied_to_terminal = write_osc52_clipboard(&text).is_ok();
        let chars = text.chars().count();
        if copied_to_terminal {
            self.set_notice(format!("Copied {chars} visible characters."));
        } else {
            self.set_notice(format!("Copied {chars} visible characters internally."));
        }
    }

    fn copy_focused_history(&mut self) {
        let Some((task_name, text)) = self
            .tasks
            .get(self.focus)
            .map(|task| (task.task.name.clone(), task.history.all_text()))
        else {
            self.set_notice("Focused pane has no scrollback.".to_owned());
            return;
        };
        if text.is_empty() {
            self.set_notice(format!("{task_name} has no scrollback."));
            return;
        }

        self.clipboard = text.clone();
        let osc52_allowed = text.len() <= MAX_FULL_HISTORY_OSC52_BYTES;
        let copied_to_terminal = osc52_allowed && write_osc52_clipboard(&text).is_ok();
        let chars = text.chars().count();
        if copied_to_terminal {
            self.set_notice(format!(
                "Copied {chars} history characters from {task_name}."
            ));
        } else if osc52_allowed {
            self.set_notice(format!(
                "Copied {chars} history characters from {task_name} internally."
            ));
        } else {
            self.set_notice(format!(
                "Copied {chars} history characters from {task_name} internally; too large for OSC 52."
            ));
        }
    }

    fn save_focused_history(&mut self) -> Result<()> {
        self.save_focused_history_to_dir(&env::temp_dir().join("demons"))
    }

    fn save_focused_history_to_dir(&mut self, dir: &Path) -> Result<()> {
        let Some((task_name, text)) = self
            .tasks
            .get(self.focus)
            .map(|task| (task.task.name.clone(), task.history.all_text()))
        else {
            self.set_notice("Focused pane has no scrollback.".to_owned());
            return Ok(());
        };
        if text.is_empty() {
            self.set_notice(format!("{task_name} has no scrollback."));
            return Ok(());
        }

        let path = write_history_log(dir, &task_name, &text)?;
        let path_text = path.display().to_string();
        self.clipboard = path_text.clone();
        let copied_to_terminal = write_osc52_clipboard(&path_text).is_ok();
        if copied_to_terminal {
            self.set_notice(format!(
                "Saved {task_name} scrollback to {path_text}; path copied."
            ));
        } else {
            self.set_notice(format!(
                "Saved {task_name} scrollback to {path_text}; path copied internally."
            ));
        }
        Ok(())
    }

    fn visible_pane_text(&self, index: usize) -> Option<String> {
        let task = self.tasks.get(index)?;
        let area = *self.content_rects.get(index)?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if task.scroll_offset == 0 {
            return Some(task.parser.screen().contents());
        }

        let start = task.history.visible_start(area.height, task.scroll_offset);
        Some(task.history.text_between(
            SelectionPoint {
                line: start,
                column: 0,
            },
            SelectionPoint {
                line: start.saturating_add(u64::from(area.height.saturating_sub(1))),
                column: area.width.saturating_sub(1),
            },
        ))
    }

    fn paste_clipboard_to_focus(&mut self) -> Result<bool> {
        self.paste_clipboard_to_task(self.focus)
    }

    fn paste_clipboard_to_task(&mut self, index: usize) -> Result<bool> {
        if self.clipboard.is_empty() || self.mode != AppMode::Input {
            return Ok(false);
        }
        let text = self.clipboard.clone();
        self.paste_text_to_task(index, &text)?;
        self.set_notice(format!("Pasted {} characters.", text.chars().count()));
        Ok(true)
    }

    fn paste_text_to_task(&mut self, index: usize, text: &str) -> Result<()> {
        let bracketed = self.tasks[index].parser.screen().bracketed_paste();
        if bracketed {
            self.tasks[index].write_input(b"\x1b[200~")?;
        }
        self.tasks[index].write_input(text.as_bytes())?;
        if bracketed {
            self.tasks[index].write_input(b"\x1b[201~")?;
        }
        Ok(())
    }

    fn set_notice(&mut self, text: String) {
        self.notice = Some(Notice {
            text,
            until: Instant::now() + NOTICE_DURATION,
        });
    }

    fn active_notice(&self, now: Instant) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|notice| notice.until > now)
            .map(|notice| notice.text.as_str())
    }

    fn clear_selection_for(&mut self, index: usize) {
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane == index)
        {
            self.selection = None;
        }
    }

    fn cycle_focus(&mut self, delta: isize) {
        let count = self.tasks.len() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
    }

    fn move_focus(&mut self, direction: Direction) {
        if self.fullscreen {
            match direction {
                Direction::Left | Direction::Up => self.cycle_focus(-1),
                Direction::Right | Direction::Down => self.cycle_focus(1),
            }
            return;
        }

        let row = self.focus / self.grid.columns;
        let column = self.focus % self.grid.columns;
        let next = match direction {
            Direction::Left if column > 0 => Some(self.focus - 1),
            Direction::Right if column + 1 < self.grid.columns => Some(self.focus + 1),
            Direction::Up if row > 0 => Some(self.focus - self.grid.columns),
            Direction::Down => Some(self.focus + self.grid.columns),
            _ => None,
        };
        if let Some(next) = next.filter(|index| *index < self.tasks.len()) {
            self.focus = next;
        }
    }

    fn focused_page_rows(&self) -> usize {
        self.content_rects
            .get(self.focus)
            .map(|rect| usize::from(rect.height.saturating_sub(1).max(1)))
            .unwrap_or(1)
    }

    fn request_restart(&mut self, index: usize) {
        if self.stopping {
            return;
        }
        if let Some(pid) = self.tasks[index].pid {
            self.tasks[index].restart_requested = true;
            self.tasks[index].kill_deadline = Some(Instant::now() + RESTART_GRACE);
            self.tasks[index].status = TaskStatus::Restarting;
            self.tasks[index].message("\r\n\x1b[33m[demons] restarting...\x1b[0m\r\n");
            if signal_process_group(pid, libc::SIGTERM).is_err() {
                self.tasks[index].kill_deadline = Some(Instant::now());
            }
        } else {
            self.spawn(index);
        }
    }

    fn drain_process_events(&mut self) -> bool {
        let mut changed = false;
        // Bound each pass so a process that writes continuously cannot starve
        // keyboard input or screen redraws.
        for _ in 0..256 {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            self.apply_process_event(event);
            changed = true;
        }
        changed
    }

    fn apply_process_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::Output {
                task,
                generation,
                bytes,
            } if self.tasks[task].generation == generation => {
                self.tasks[task].process_output(&bytes);
            }
            ProcessEvent::Exited {
                task,
                generation,
                status,
            } if self.tasks[task].generation == generation => {
                if let Some(pid) = self.tasks[task].pid.take() {
                    registry_remove(&self.registry, pid);
                }
                self.tasks[task].master = None;
                self.tasks[task].writer = None;
                self.tasks[task].kill_deadline = None;
                self.tasks[task].status = TaskStatus::Exited {
                    code: status.exit_code(),
                    success: status.success(),
                    signal: status.signal().map(str::to_owned),
                };
                let reason = match status.signal() {
                    Some(signal) => format!("signal {signal}"),
                    None => format!("code {}", status.exit_code()),
                };
                self.tasks[task].message(&format!(
                    "\r\n\x1b[90m[demons] process exited ({reason})\x1b[0m\r\n"
                ));

                if self.tasks[task].restart_requested && !self.stopping {
                    self.tasks[task].restart_requested = false;
                    self.spawn(task);
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self) -> Result<bool> {
        let now = Instant::now();
        let mut changed = false;
        if self
            .pending_escape
            .is_some_and(|started| now.duration_since(started) >= ALT_ESCAPE_TIMEOUT)
        {
            self.pending_escape = None;
            self.apply_escape()?;
            changed = true;
        }
        if self.autoscroll_selection(now, false) {
            changed = true;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.until <= now)
        {
            self.notice = None;
            changed = true;
        }
        for task in &mut self.tasks {
            if task.kill_deadline.is_some_and(|deadline| deadline <= now) {
                if let Some(pid) = task.pid {
                    signal_process_group(pid, libc::SIGKILL).ok();
                }
                task.kill_deadline = None;
                changed = true;
            }
        }
        Ok(changed)
    }

    fn apply_escape(&mut self) -> Result<()> {
        match self.mode {
            AppMode::Input => self.tasks[self.focus].write_input(b"\x1b")?,
            AppMode::Command => self.mode = AppMode::Input,
            AppMode::Search => self.cancel_search(),
        }
        Ok(())
    }

    fn mark_stopping(&mut self) {
        self.stopping = true;
        for task in &mut self.tasks {
            if task.pid.is_some() {
                task.status = TaskStatus::Stopping;
            }
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stopping = true;
        for task in &mut self.tasks {
            task.restart_requested = false;
            if let Some(pid) = task.pid {
                signal_process_group(pid, libc::SIGTERM).ok();
            }
        }

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while self.tasks.iter().any(|task| task.pid.is_some()) && Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => self.apply_process_event(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        for task in &self.tasks {
            if let Some(pid) = task.pid {
                signal_process_group(pid, libc::SIGKILL).ok();
            }
        }

        while self.tasks.iter().any(|task| task.pid.is_some()) {
            match self.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(event) => self.apply_process_event(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("timed out waiting for child processes to exit");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

struct TaskRuntime {
    task: Task,
    cwd: PathBuf,
    parser: Parser,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    pid: Option<u32>,
    status: TaskStatus,
    generation: u64,
    restart_requested: bool,
    kill_deadline: Option<Instant>,
    pty_size: PtySize,
    scroll_offset: usize,
    history: TextHistory,
}

impl TaskRuntime {
    fn new(task: Task, cwd: PathBuf) -> Self {
        let pty_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        Self {
            task,
            cwd,
            parser: Parser::new(pty_size.rows, pty_size.cols, SCROLLBACK_LINES),
            master: None,
            writer: None,
            pid: None,
            status: TaskStatus::NotStarted,
            generation: 0,
            restart_requested: false,
            kill_deadline: None,
            pty_size,
            scroll_offset: 0,
            history: TextHistory::new(pty_size.cols, SCROLLBACK_LINES),
        }
    }

    fn spawn(
        &mut self,
        task_index: usize,
        size: PtySize,
        tx: SyncSender<ProcessEvent>,
        registry: ProcessRegistry,
    ) -> Result<()> {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.status = TaskStatus::Starting;
        self.scroll_offset = 0;

        let pair = native_pty_system()
            .openpty(size)
            .with_context(|| format!("failed to open PTY for task {:?}", self.task.name))?;
        let mut command = self.command_builder();
        command.cwd(&self.cwd);
        for (key, value) in &self.task.env {
            command.env(key, value);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to spawn task {:?}", self.task.name))?;
        let Some(pid) = child.process_id() else {
            child.kill().ok();
            child.wait().ok();
            anyhow::bail!("spawned process did not report a process ID");
        };
        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                terminate_unmanaged_child(&mut child, pid);
                return Err(error).context("failed to open PTY reader");
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                terminate_unmanaged_child(&mut child, pid);
                return Err(error).context("failed to open PTY writer");
            }
        };

        let output_tx = tx.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("demons-output-{}", self.task.name))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if output_tx
                                .send(ProcessEvent::Output {
                                    task: task_index,
                                    generation,
                                    bytes: buffer[..read].to_vec(),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            })
        {
            terminate_unmanaged_child(&mut child, pid);
            return Err(error).context("failed to start PTY reader thread");
        }

        registry_insert(&registry, pid);
        let child_guard = ChildGuard::new(child, pid, Arc::clone(&registry));
        if let Err(error) = thread::Builder::new()
            .name(format!("demons-wait-{}", self.task.name))
            .spawn(move || {
                let status = child_guard.wait();
                tx.send(ProcessEvent::Exited {
                    task: task_index,
                    generation,
                    status,
                })
                .ok();
            })
        {
            return Err(error).context("failed to start child wait thread");
        }

        self.pid = Some(pid);
        self.writer = Some(writer);
        self.master = Some(pair.master);
        self.status = TaskStatus::Running;
        Ok(())
    }

    fn command_builder(&self) -> CommandBuilder {
        match &self.task.command {
            TaskCommand::Shell(command) => {
                let shell = env::var_os("SHELL")
                    .filter(|shell| !shell.is_empty())
                    .unwrap_or_else(|| "/bin/sh".into());
                let mut builder = CommandBuilder::new(shell);
                builder.arg("-c");
                builder.arg(command);
                builder
            }
            TaskCommand::Direct(parts) => {
                let mut builder = CommandBuilder::new(&parts[0]);
                builder.args(&parts[1..]);
                builder
            }
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        let size = PtySize {
            // vt100 0.15 can underflow while wrapping output on a one-cell
            // screen. Tiny host terminals still need a valid backing screen.
            rows: rows.max(2),
            cols: columns.max(2),
            pixel_width: 0,
            pixel_height: 0,
        };
        if size.rows == self.pty_size.rows && size.cols == self.pty_size.cols {
            return;
        }
        self.pty_size = size;
        self.history.set_width(size.cols);
        self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        self.parser.set_size(size.rows, size.cols);
        if let Some(master) = &self.master {
            master.resize(size).ok();
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let result = writer.write_all(bytes).and_then(|()| writer.flush());
        if let Err(error) = result {
            self.writer = None;
            self.message(&format!(
                "\r\n\x1b[31m[demons] task input closed: {error}\x1b[0m\r\n"
            ));
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.parser = Parser::new(self.pty_size.rows, self.pty_size.cols, SCROLLBACK_LINES);
        self.scroll_offset = 0;
        self.history.clear();
    }

    fn scroll_up(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(rows)
            .min(self.max_scroll_offset());
        self.scroll_offset != previous
    }

    fn scroll_down(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.scroll_offset != previous
    }

    fn scroll_to_top(&mut self) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.max_scroll_offset();
        self.scroll_offset != previous
    }

    fn scroll_to_bottom(&mut self) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = 0;
        self.scroll_offset != previous
    }

    fn scroll_to_history_line(&mut self, line: u64, height: u16) {
        let line_count = self.history.line_count();
        let height = u64::from(height.max(1));
        let max_start = line_count.saturating_sub(height);
        let target_start = line.saturating_sub(height / 2).min(max_start);
        let max_offset = line_count.saturating_sub(height);
        self.scroll_offset = max_offset
            .saturating_sub(target_start)
            .min(usize::MAX as u64) as usize;
    }

    fn max_scroll_offset(&self) -> usize {
        self.history
            .line_count()
            .saturating_sub(u64::from(self.pty_size.rows))
            .min(usize::MAX as u64) as usize
    }

    fn message(&mut self, message: &str) {
        self.scroll_offset = 0;
        self.history.process(message.as_bytes());
        self.parser.process(message.as_bytes());
    }

    fn process_output(&mut self, bytes: &[u8]) {
        let added_rows = self.history.process(bytes);
        self.parser.process(bytes);
        if self.scroll_offset > 0 && added_rows > 0 {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add(added_rows)
                .min(self.max_scroll_offset());
        }
    }

    fn history_index_for_visible_row(&self, row: u16, height: u16) -> u64 {
        self.history
            .visible_start(height, self.scroll_offset)
            .saturating_add(u64::from(row))
    }

    fn record_spawn_error(&mut self, error: &anyhow::Error) {
        self.pid = None;
        self.writer = None;
        self.master = None;
        self.status = TaskStatus::Failed;
        self.message(&format!(
            "\r\n\x1b[31m[demons] failed to start: {error:#}\x1b[0m\r\n"
        ));
    }

    fn status_label(&self) -> (String, Color) {
        match &self.status {
            TaskStatus::NotStarted => ("⏸".to_owned(), Color::DarkGray),
            TaskStatus::Starting => ("…".to_owned(), Color::Yellow),
            TaskStatus::Running => ("●".to_owned(), Color::Green),
            TaskStatus::Restarting => ("↻".to_owned(), Color::Yellow),
            TaskStatus::Stopping => ("■".to_owned(), Color::Yellow),
            TaskStatus::Failed => ("✗".to_owned(), Color::Red),
            TaskStatus::Exited {
                code,
                success,
                signal,
            } => {
                if *success {
                    ("✓".to_owned(), Color::Green)
                } else if let Some(signal) = signal {
                    (format!("✗ {signal}"), Color::Red)
                } else {
                    (format!("✗ {code}"), Color::Red)
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    line: u64,
    column: u16,
}

#[derive(Clone, Debug)]
struct Selection {
    pane: usize,
    anchor: SelectionPoint,
    cursor: SelectionPoint,
    dragging: bool,
    dragged: bool,
    last_mouse: Option<(u16, u16)>,
    last_scroll: Instant,
}

impl Selection {
    fn ordered_points(&self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    fn columns_for_line(&self, line: u64, width: u16) -> Option<(u16, u16)> {
        if !self.dragged || width == 0 {
            return None;
        }
        let (start, end) = self.ordered_points();
        if line < start.line || line > end.line {
            return None;
        }

        let range = if start.line == end.line {
            (
                start.column.min(width),
                end.column.saturating_add(1).min(width),
            )
        } else if line == start.line {
            (start.column.min(width), width)
        } else if line == end.line {
            (0, end.column.saturating_add(1).min(width))
        } else {
            (0, width)
        };
        (range.0 < range.1).then_some(range)
    }
}

#[derive(Clone, Debug)]
struct Notice {
    text: String,
    until: Instant,
}

#[derive(Clone, Debug, Default)]
struct HistoryLine {
    text: String,
    wrapped: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TextParserState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    StringControl,
    StringEscape,
}

#[derive(Clone, Debug)]
struct TextHistory {
    lines: VecDeque<HistoryLine>,
    first_index: u64,
    current: HistoryLine,
    column: usize,
    pending_wrap: bool,
    width: u16,
    max_lines: usize,
    state: TextParserState,
    csi: String,
}

impl TextHistory {
    fn new(width: u16, max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            first_index: 0,
            current: HistoryLine::default(),
            column: 0,
            pending_wrap: false,
            width: width.max(1),
            max_lines,
            state: TextParserState::Ground,
            csi: String::new(),
        }
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.first_index = 0;
        self.current = HistoryLine::default();
        self.column = 0;
        self.pending_wrap = false;
        self.state = TextParserState::Ground;
        self.csi.clear();
    }

    fn set_width(&mut self, width: u16) {
        self.width = width.max(1);
        if self.column >= usize::from(self.width) {
            self.column = usize::from(self.width.saturating_sub(1));
            self.pending_wrap = false;
        }
    }

    fn line_count(&self) -> u64 {
        self.first_index
            .saturating_add(self.lines.len() as u64)
            .saturating_add(1)
    }

    fn visible_start(&self, height: u16, scroll_offset: usize) -> u64 {
        self.line_count()
            .saturating_sub(u64::from(height).saturating_add(scroll_offset as u64))
    }

    fn line(&self, index: u64) -> Option<&HistoryLine> {
        let offset = index.checked_sub(self.first_index)?;
        if offset < self.lines.len() as u64 {
            return self.lines.get(offset as usize);
        }
        (offset == self.lines.len() as u64).then_some(&self.current)
    }

    fn process(&mut self, bytes: &[u8]) -> usize {
        let mut added_rows = 0;
        for character in String::from_utf8_lossy(bytes).chars() {
            match self.state {
                TextParserState::Ground => match character {
                    '\x1b' => self.state = TextParserState::Escape,
                    '\n' => added_rows += self.push_current(false),
                    '\r' => {
                        self.column = 0;
                        self.pending_wrap = false;
                    }
                    '\x08' => {
                        if self.pending_wrap {
                            self.pending_wrap = false;
                        }
                        self.column = self.column.saturating_sub(1);
                    }
                    '\t' => {
                        let spaces = 8 - (self.column % 8);
                        for _ in 0..spaces {
                            added_rows += self.put_char(' ');
                        }
                    }
                    character if character.is_control() => {}
                    character => added_rows += self.put_char(character),
                },
                TextParserState::Escape => match character {
                    '[' => {
                        self.csi.clear();
                        self.state = TextParserState::Csi;
                    }
                    ']' => self.state = TextParserState::Osc,
                    'P' | '^' | '_' => self.state = TextParserState::StringControl,
                    _ => self.state = TextParserState::Ground,
                },
                TextParserState::Csi => {
                    if ('@'..='~').contains(&character) {
                        self.apply_csi(character);
                        self.csi.clear();
                        self.state = TextParserState::Ground;
                    } else if self.csi.len() < 32 {
                        self.csi.push(character);
                    }
                }
                TextParserState::Osc => match character {
                    '\x07' => self.state = TextParserState::Ground,
                    '\x1b' => self.state = TextParserState::OscEscape,
                    _ => {}
                },
                TextParserState::OscEscape => {
                    self.state = if character == '\\' {
                        TextParserState::Ground
                    } else {
                        TextParserState::Osc
                    };
                }
                TextParserState::StringControl => {
                    if character == '\x1b' {
                        self.state = TextParserState::StringEscape;
                    }
                }
                TextParserState::StringEscape => {
                    self.state = if character == '\\' {
                        TextParserState::Ground
                    } else {
                        TextParserState::StringControl
                    };
                }
            }
        }
        added_rows
    }

    fn put_char(&mut self, character: char) -> usize {
        let mut added_rows = 0;
        if self.pending_wrap {
            added_rows += self.push_current(true);
            self.pending_wrap = false;
        }
        while char_count(&self.current.text) < self.column {
            self.current.text.push(' ');
        }

        replace_char(&mut self.current.text, self.column, character);
        self.column += 1;
        if self.column >= usize::from(self.width) {
            self.pending_wrap = true;
        }
        added_rows
    }

    fn push_current(&mut self, wrapped: bool) -> usize {
        let mut line = std::mem::take(&mut self.current);
        line.text.truncate(line.text.trim_end().len());
        line.wrapped = wrapped;
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
            self.first_index = self.first_index.saturating_add(1);
        }
        self.column = 0;
        self.pending_wrap = false;
        1
    }

    fn apply_csi(&mut self, final_byte: char) {
        if final_byte != 'K' {
            return;
        }
        match first_csi_param(&self.csi).unwrap_or(0) {
            0 => truncate_chars(&mut self.current.text, self.column),
            1 => {
                let end = byte_index_for_char(&self.current.text, self.column);
                self.current.text.replace_range(0..end, "");
                self.column = 0;
            }
            2 => {
                self.current.text.clear();
                self.column = 0;
            }
            _ => {}
        }
    }

    fn text_between(&self, anchor: SelectionPoint, cursor: SelectionPoint) -> String {
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        let mut text = String::new();
        for line_index in start.line..=end.line {
            let Some(line) = self.line(line_index) else {
                continue;
            };
            let start_column = if line_index == start.line {
                usize::from(start.column)
            } else {
                0
            };
            let end_column = if line_index == end.line {
                usize::from(end.column.saturating_add(1))
            } else {
                usize::MAX
            };
            text.push_str(&slice_chars(&line.text, start_column, end_column));
            if line_index != end.line && !line.wrapped {
                text.push('\n');
            }
        }
        text
    }

    fn all_text(&self) -> String {
        let end = self.line_count().saturating_sub(1);
        self.text_between(
            SelectionPoint {
                line: self.first_index,
                column: 0,
            },
            SelectionPoint {
                line: end,
                column: u16::MAX,
            },
        )
    }

    fn find_last_line(&self, query: &str) -> Option<u64> {
        let needle = search_needle(query)?;

        (self.first_index..self.line_count())
            .rev()
            .find(|line_index| self.line_matches(*line_index, &needle))
    }

    fn find_first_line(&self, query: &str) -> Option<u64> {
        let needle = search_needle(query)?;

        (self.first_index..self.line_count())
            .find(|line_index| self.line_matches(*line_index, &needle))
    }

    fn find_line_before(&self, query: &str, before: u64) -> Option<u64> {
        let needle = search_needle(query)?;
        let end = before.min(self.line_count());

        (self.first_index..end)
            .rev()
            .find(|line_index| self.line_matches(*line_index, &needle))
    }

    fn find_line_after(&self, query: &str, after: u64) -> Option<u64> {
        let needle = search_needle(query)?;
        let start = after.saturating_add(1).max(self.first_index);

        (start..self.line_count()).find(|line_index| self.line_matches(*line_index, &needle))
    }

    fn line_char_count(&self, index: u64) -> Option<usize> {
        self.line(index).map(|line| char_count(&line.text))
    }

    fn line_matches(&self, index: u64, needle: &str) -> bool {
        self.line(index)
            .is_some_and(|line| line.text.to_ascii_lowercase().contains(needle))
    }
}

enum ProcessEvent {
    Output {
        task: usize,
        generation: u64,
        bytes: Vec<u8>,
    },
    Exited {
        task: usize,
        generation: u64,
        status: ExitStatus,
    },
}

#[derive(Clone, Debug)]
enum TaskStatus {
    NotStarted,
    Starting,
    Running,
    Restarting,
    Stopping,
    Failed,
    Exited {
        code: u32,
        success: bool,
        signal: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppMode {
    Input,
    Command,
    Search,
}

#[derive(Clone, Debug)]
struct SearchState {
    pane: usize,
    query: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchDirection {
    Older,
    Newer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Continue,
    Quit,
}

#[derive(Clone, Copy, Debug)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            disable_raw_mode().ok();
            return Err(error).context("failed to enter terminal UI");
        }
        Ok(Self { active: true })
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        disable_raw_mode().ok();
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .ok();
        self.active = false;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(registry: ProcessRegistry) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let pids = registry
            .lock()
            .map(|pids| pids.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for pid in pids {
            signal_process_group(pid, libc::SIGKILL).ok();
        }
        disable_raw_mode().ok();
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .ok();
        previous(panic);
    }));
}

fn register_shutdown_signals() -> Result<Arc<AtomicBool>> {
    let requested = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&requested))
            .with_context(|| format!("failed to register signal handler for {signal}"))?;
    }
    Ok(requested)
}

fn signal_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if result == 0 {
        return Ok(());
    }
    let group_error = io::Error::last_os_error();
    if group_error.raw_os_error() != Some(libc::ESRCH) {
        return Err(group_error);
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn terminate_unmanaged_child(child: &mut Box<dyn Child + Send + Sync>, pid: u32) {
    signal_process_group(pid, libc::SIGKILL).ok();
    child.kill().ok();
    child.wait().ok();
}

struct ChildGuard {
    child: Option<Box<dyn Child + Send + Sync>>,
    pid: u32,
    registry: ProcessRegistry,
}

impl ChildGuard {
    fn new(child: Box<dyn Child + Send + Sync>, pid: u32, registry: ProcessRegistry) -> Self {
        Self {
            child: Some(child),
            pid,
            registry,
        }
    }

    fn wait(mut self) -> ExitStatus {
        let Some(mut child) = self.child.take() else {
            registry_remove(&self.registry, self.pid);
            return ExitStatus::with_exit_code(1);
        };
        let status = child
            .wait()
            .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
        registry_remove(&self.registry, self.pid);
        status
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            signal_process_group(self.pid, libc::SIGKILL).ok();
            child.kill().ok();
            child.wait().ok();
        }
        registry_remove(&self.registry, self.pid);
    }
}

fn registry_insert(registry: &ProcessRegistry, pid: u32) {
    if let Ok(mut pids) = registry.lock() {
        pids.insert(pid);
    }
}

fn registry_remove(registry: &ProcessRegistry, pid: u32) {
    if let Ok(mut pids) = registry.lock() {
        pids.remove(&pid);
    }
}

fn render_history(task: &TaskRuntime, area: Rect, buffer: &mut Buffer) {
    let start = task.history.visible_start(area.height, task.scroll_offset);
    for row in 0..area.height {
        for column in 0..area.width {
            buffer[(area.x + column, area.y + row)].reset();
        }

        let Some(line) = task.history.line(start.saturating_add(u64::from(row))) else {
            continue;
        };
        for (column, character) in line.text.chars().take(usize::from(area.width)).enumerate() {
            let mut encoded = [0_u8; 4];
            buffer[(area.x + column as u16, area.y + row)]
                .set_symbol(character.encode_utf8(&mut encoded))
                .set_style(Style::default().fg(Color::White));
        }
    }
}

fn render_screen(parser: &Parser, area: Rect, buffer: &mut Buffer) {
    let screen = parser.screen();
    for row in 0..area.height {
        for column in 0..area.width {
            let Some(source) = screen.cell(row, column) else {
                continue;
            };
            if source.is_wide_continuation() {
                continue;
            }
            let symbol = source.contents();
            let symbol = if symbol.is_empty() { " " } else { &symbol };
            buffer[(area.x + column, area.y + row)]
                .set_symbol(symbol)
                .set_style(cell_style(source));
        }
    }
}

fn render_selection(
    selection: Option<&Selection>,
    task: &TaskRuntime,
    area: Rect,
    buffer: &mut Buffer,
) {
    let Some(selection) = selection else {
        return;
    };
    for row in 0..area.height {
        let line = task.history_index_for_visible_row(row, area.height);
        let Some((start, end)) = selection.columns_for_line(line, area.width) else {
            continue;
        };
        for column in start..end {
            buffer[(area.x + column, area.y + row)]
                .set_style(Style::default().fg(Color::Black).bg(Color::White));
        }
    }
}

fn render_command_help(area: Rect, buffer: &mut Buffer, leader: &str) {
    let popup = centered_rect(area, 74, 16);
    Clear.render(popup, buffer);
    let lines = vec![
        Line::styled(
            "Command Help",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("arrows / h j k l       Move focus"),
        Line::raw("Tab / Shift-Tab        Cycle panes"),
        Line::raw("f                      Toggle fullscreen pane"),
        Line::raw("PageUp/PageDown        Scroll focused pane"),
        Line::raw("Home/End               Jump to top/bottom of history"),
        Line::raw("drag / right-click     Select and copy pane text"),
        Line::raw("y / Y                  Copy visible text / full scrollback"),
        Line::raw("S                      Save full scrollback to a temp log"),
        Line::raw("/, n, N                Search, repeat older, repeat newer"),
        Line::raw("r / R                  Restart focused task / all tasks"),
        Line::raw("c                      Clear focused pane"),
        Line::raw("q or Ctrl-C            Quit"),
        Line::raw(format!("{leader} or Esc          Return to input mode")),
        Line::raw("? or Esc               Close this help"),
    ];
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .style(Style::default().fg(Color::White).bg(Color::Black)),
        )
        .wrap(Wrap { trim: false })
        .render(popup, buffer);
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width.saturating_sub(2)).max(1);
    let height = preferred_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(color) = terminal_color(cell.fgcolor()) {
        style = style.fg(color);
    }
    if let Some(color) = terminal_color(cell.bgcolor()) {
        style = style.bg(color);
    }
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
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn terminal_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(red, green, blue) => Some(Color::Rgb(red, green, blue)),
    }
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn selection_scroll_step(rect: Rect, y: u16) -> usize {
    let distance = if y < rect.y {
        rect.y.saturating_sub(y)
    } else if y >= rect.bottom() {
        y.saturating_sub(rect.bottom()).saturating_add(1)
    } else {
        0
    };
    usize::from(1 + distance / 3)
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

fn replace_char(value: &mut String, index: usize, character: char) {
    let mut indices = value.char_indices().map(|(offset, _)| offset);
    let Some(start) = indices.nth(index) else {
        value.push(character);
        return;
    };
    let end = value[start..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| start + offset)
        .unwrap_or(value.len());
    value.replace_range(start..end, character.encode_utf8(&mut [0_u8; 4]));
}

fn truncate_chars(value: &mut String, len: usize) {
    let index = byte_index_for_char(value, len);
    value.truncate(index);
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn slice_chars(value: &str, start: usize, end: usize) -> String {
    value
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn first_csi_param(value: &str) -> Option<usize> {
    value
        .split(';')
        .next()
        .filter(|param| {
            !param.is_empty() && param.chars().all(|character| character.is_ascii_digit())
        })
        .and_then(|param| param.parse().ok())
}

fn search_needle(query: &str) -> Option<String> {
    let needle = query.trim().to_ascii_lowercase();
    (!needle.is_empty()).then_some(needle)
}

fn search_footer_text(search: &SearchState) -> String {
    let mut query = search.query.clone();
    let cursor = byte_index_for_char(&query, search.cursor);
    query.insert(cursor, '|');
    format!(" /{query}  Enter: jump | Esc: cancel ")
}

fn write_history_log(dir: &Path, task_name: &str, text: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create log directory {}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = sanitize_filename(task_name);

    for attempt in 0..1000 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let path = dir.join(format!("{name}-{stamp}{suffix}.log"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to write {}", path.display()));
            }
        }
    }

    anyhow::bail!(
        "failed to create a unique scrollback log in {}",
        dir.display()
    );
}

fn sanitize_filename(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "pane".to_owned()
    } else {
        trimmed.chars().take(64).collect()
    }
}

#[cfg(test)]
fn write_osc52_clipboard(_text: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn write_osc52_clipboard(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b]52;c;")?;
    stdout.write_all(encoded.as_bytes())?;
    stdout.write_all(b"\x07")?;
    stdout.flush()
}

#[cfg(not(test))]
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        encoded.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        encoded.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn is_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::SHIFT)
}

fn is_paste_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v' | 'V'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::SHIFT)
}

fn is_leader(key: KeyEvent, leader: Leader) -> bool {
    match leader {
        Leader::AltJ => {
            matches!(key.code, KeyCode::Char('j' | 'J'))
                && (key.modifiers == KeyModifiers::ALT
                    || key.modifiers == KeyModifiers::ALT | KeyModifiers::SHIFT)
        }
        Leader::Tab => key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::CONTROL),
        Leader::CtrlB => {
            matches!(key.code, KeyCode::Char('b' | 'B'))
                && key.modifiers.contains(KeyModifiers::CONTROL)
        }
        Leader::CtrlQ => {
            matches!(key.code, KeyCode::Char('q' | 'Q'))
                && key.modifiers.contains(KeyModifiers::CONTROL)
        }
        Leader::CtrlBackslash => {
            key.code == KeyCode::Char('\\') && key.modifiers.contains(KeyModifiers::CONTROL)
        }
    }
}

fn is_legacy_alt_j(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('j' | 'J'))
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let cursor_key = matches!(
        key.code,
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left | KeyCode::Home | KeyCode::End
    );
    let sequence = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let upper = character.to_ascii_uppercase() as u32;
            if (64..=95).contains(&upper) {
                vec![(upper as u8) & 0x1f]
            } else if character == '?' {
                vec![0x7f]
            } else if character == ' ' {
                vec![0]
            } else {
                return Vec::new();
            }
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_sequence(b'A', application_cursor, key.modifiers),
        KeyCode::Down => cursor_sequence(b'B', application_cursor, key.modifiers),
        KeyCode::Right => cursor_sequence(b'C', application_cursor, key.modifiers),
        KeyCode::Left => cursor_sequence(b'D', application_cursor, key.modifiers),
        KeyCode::Home => cursor_sequence(b'H', application_cursor, key.modifiers),
        KeyCode::End => cursor_sequence(b'F', application_cursor, key.modifiers),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(number) => function_key_sequence(number),
        _ => Vec::new(),
    };

    if key.modifiers.contains(KeyModifiers::ALT) && !cursor_key && !sequence.is_empty() {
        let mut with_alt = Vec::with_capacity(sequence.len() + 1);
        with_alt.push(0x1b);
        with_alt.extend(sequence);
        with_alt
    } else {
        sequence
    }
}

fn cursor_sequence(final_byte: u8, application_cursor: bool, modifiers: KeyModifiers) -> Vec<u8> {
    let modifiers = modifiers & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
    if modifiers.is_empty() {
        return vec![
            0x1b,
            if application_cursor { b'O' } else { b'[' },
            final_byte,
        ];
    }

    let mut modifier = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        modifier += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        modifier += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        modifier += 4;
    }
    format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
}

fn function_key_sequence(number: u8) -> Vec<u8> {
    match number {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    }
}

fn should_forward_mouse(kind: MouseEventKind, mode: MouseProtocolMode) -> bool {
    match kind {
        MouseEventKind::Moved => mode == MouseProtocolMode::AnyMotion,
        MouseEventKind::Drag(_) => matches!(
            mode,
            MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
        ),
        MouseEventKind::Up(_) => mode != MouseProtocolMode::Press,
        _ => true,
    }
}

fn encode_mouse(mouse: MouseEvent, x: u16, y: u16, encoding: MouseProtocolEncoding) -> Vec<u8> {
    let base_code = match mouse.kind {
        MouseEventKind::Down(button) | MouseEventKind::Up(button) => mouse_button_code(button),
        MouseEventKind::Drag(button) => mouse_button_code(button) + 32,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    let mut modifier_code = 0;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        modifier_code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        modifier_code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        modifier_code += 16;
    }

    if encoding == MouseProtocolEncoding::Sgr {
        let code = base_code + modifier_code;
        let final_byte = if matches!(mouse.kind, MouseEventKind::Up(_)) {
            'm'
        } else {
            'M'
        };
        format!("\x1b[<{code};{x};{y}{final_byte}").into_bytes()
    } else {
        let code = if matches!(mouse.kind, MouseEventKind::Up(_)) {
            3 + modifier_code
        } else {
            base_code + modifier_code
        };
        let values = [code + 32, u32::from(x) + 32, u32::from(y) + 32];
        let mut bytes = b"\x1b[M".to_vec();
        if encoding == MouseProtocolEncoding::Utf8 {
            for value in values {
                let character = char::from_u32(value).unwrap_or('\u{fffd}');
                let mut encoded = [0_u8; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
        } else {
            bytes.extend(values.map(|value| value.min(255) as u8));
        }
        bytes
    }
}

fn mouse_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::config::{Config, Settings};
    use tempfile::tempdir;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn recognizes_configured_leaders() {
        assert!(is_leader(
            key(KeyCode::Char('j'), KeyModifiers::ALT),
            Leader::AltJ
        ));
        assert!(is_leader(
            key(KeyCode::Char('J'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            Leader::AltJ
        ));
        assert!(!is_leader(
            key(
                KeyCode::Char('j'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            ),
            Leader::AltJ
        ));
        assert!(is_leader(
            key(KeyCode::Tab, KeyModifiers::NONE),
            Leader::Tab
        ));
        assert!(is_leader(
            key(KeyCode::Char('b'), KeyModifiers::CONTROL),
            Leader::CtrlB
        ));
        assert!(!is_leader(
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            Leader::CtrlB
        ));
    }

    #[test]
    fn recognizes_legacy_escape_encoded_alt_j() {
        let mut app = test_app();

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Command);
        assert!(app.pending_escape.is_none());
    }

    #[test]
    fn encodes_basic_terminal_keys() {
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            vec![3]
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), false),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::CONTROL), false),
            b"\x1b[1;5D"
        );
    }

    #[test]
    fn encodes_sgr_mouse_events() {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse(event, 3, 4, MouseProtocolEncoding::Sgr),
            b"\x1b[<0;3;4M"
        );
    }

    #[test]
    fn encodes_utf8_mouse_coordinates() {
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 100,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse(event, 100, 4, MouseProtocolEncoding::Utf8),
            [b"\x1b[M".as_slice(), &[97], "\u{84}".as_bytes(), &[36]].concat()
        );
    }

    #[test]
    fn clear_blanks_the_visible_screen() {
        let mut task = TaskRuntime::new(test_task("one"), PathBuf::from("."));
        task.parser.process(b"visible output");
        assert!(task.parser.screen().cell(0, 0).unwrap().has_contents());

        task.clear();

        assert!(!task.parser.screen().cell(0, 0).unwrap().has_contents());
    }

    #[test]
    fn pane_clicks_preserve_mode_and_footer_button_toggles_it() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;

        let second = app.content_rects[1];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second.x,
            row: second.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.focus, 1);
        assert_eq!(app.mode, AppMode::Command);

        let button = app.mode_button_rect.unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.x,
            row: button.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.mode, AppMode::Input);
    }

    #[test]
    fn command_mode_question_mark_opens_and_closes_help() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;

        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.show_help);

        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.show_help);
        assert!(!app.tasks[0].restart_requested);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(!app.show_help);
        assert_eq!(app.mode, AppMode::Command);
    }

    #[test]
    fn centered_rect_clamps_to_small_areas() {
        assert_eq!(
            centered_rect(Rect::new(0, 0, 10, 5), 74, 16),
            Rect::new(1, 1, 8, 3)
        );
    }

    #[test]
    fn fullscreen_layout_only_resizes_focused_pane() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        let hidden_size = app.tasks[0].pty_size;
        app.focus = 1;
        app.fullscreen = true;

        app.update_layout(Rect::new(0, 0, 100, 20));

        assert_eq!(app.pane_rects[0], Rect::default());
        assert_eq!(app.pane_rects[1], Rect::new(0, 0, 100, 19));
        assert_eq!(app.tasks[0].pty_size.rows, hidden_size.rows);
        assert_eq!(app.tasks[0].pty_size.cols, hidden_size.cols);
        assert_eq!(app.tasks[1].pty_size.rows, 17);
        assert_eq!(app.tasks[1].pty_size.cols, 98);
    }

    #[test]
    fn command_mode_f_toggles_fullscreen_and_arrows_cycle_inside_it() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;

        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.fullscreen);

        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, 1);

        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.fullscreen);
    }

    #[test]
    fn text_history_copies_wrapped_rows_without_extra_newlines() {
        let mut history = TextHistory::new(5, 100);
        history.process(b"alpha\nbravocharlie\nz");

        let text = history.text_between(
            SelectionPoint { line: 1, column: 0 },
            SelectionPoint { line: 3, column: 1 },
        );

        assert_eq!(text, "bravocharlie");
    }

    #[test]
    fn text_history_honors_erase_line_for_progress_output() {
        let mut history = TextHistory::new(80, 100);
        history.process(b"old progress text\r\x1b[Kdone\n");

        let text = history.text_between(
            SelectionPoint { line: 0, column: 0 },
            SelectionPoint {
                line: 0,
                column: 20,
            },
        );

        assert_eq!(text, "done");
    }

    #[test]
    fn text_history_all_text_preserves_scrollback_text() {
        let mut history = TextHistory::new(80, 100);
        history.process(b"alpha\nbravo");

        assert_eq!(history.all_text(), "alpha\nbravo");

        history.process(b"\n");
        assert_eq!(history.all_text(), "alpha\nbravo\n");
    }

    #[test]
    fn text_history_finds_latest_matching_line_case_insensitively() {
        let mut history = TextHistory::new(80, 100);
        history.process(b"alpha\nerror one\nERROR two\n");

        assert_eq!(history.find_first_line("error"), Some(1));
        assert_eq!(history.find_last_line("error"), Some(2));
        assert_eq!(history.find_line_before("error", 2), Some(1));
        assert_eq!(history.find_line_after("error", 1), Some(2));
        assert_eq!(history.find_last_line("missing"), None);
    }

    #[test]
    fn search_footer_marks_the_edit_cursor() {
        let search = SearchState {
            pane: 0,
            query: "eror".to_owned(),
            cursor: 2,
        };

        assert_eq!(
            search_footer_text(&search),
            " /er|or  Enter: jump | Esc: cancel "
        );
    }

    #[test]
    fn history_log_sanitizes_name_and_writes_contents() {
        let temp = tempdir().unwrap();

        let path = write_history_log(temp.path(), "web/dev:1", "alpha\nbeta").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("web-dev-1-")
        );
    }

    #[test]
    fn command_mode_save_history_writes_log_and_copies_path() {
        let temp = tempdir().unwrap();
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"important output\n");

        app.save_focused_history_to_dir(temp.path()).unwrap();

        let path = PathBuf::from(&app.clipboard);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "important output\n"
        );
        assert!(path.starts_with(temp.path()));
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("path copied"))
        );
    }

    #[test]
    fn visible_selection_uses_terminal_screen_contents() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.tasks[0].process_output(b"hello\x1b[1GXY");
        app.selection = Some(Selection {
            pane: 0,
            anchor: SelectionPoint { line: 0, column: 0 },
            cursor: SelectionPoint { line: 0, column: 4 },
            dragging: false,
            dragged: true,
            last_mouse: None,
            last_scroll: Instant::now(),
        });

        assert_eq!(app.selected_text().unwrap(), "XYllo");
    }

    #[test]
    fn drag_selection_stays_with_original_pane() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.tasks[0].process_output(b"line one\nline two\n");
        app.tasks[1].process_output(b"other pane\n");

        let first = app.content_rects[0];
        let second = app.content_rects[1];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first.x,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: second.x + 5,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: second.x + 5,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        let selection = app.selection.as_ref().unwrap();
        assert_eq!(selection.pane, 0);
        assert_eq!(app.selected_text().unwrap(), "line one");
    }

    #[test]
    fn selection_autoscrolls_only_the_original_pane() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        for line in 0..40 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }
        for line in 0..40 {
            app.tasks[1].process_output(format!("other {line}\n").as_bytes());
        }

        let first = app.content_rects[0];
        let second = app.content_rects[1];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first.x,
            row: first.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: second.x,
            row: first.y.saturating_sub(2),
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.selection.as_ref().unwrap().pane, 0);
        assert!(app.tasks[0].scroll_offset > 0);
        assert_eq!(app.tasks[1].scroll_offset, 0);
    }

    #[test]
    fn wheel_during_selection_scrolls_original_pane() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        for line in 0..40 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }

        let first = app.content_rects[0];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first.x,
            row: first.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: first.x + 2,
            row: first.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: first.x + 2,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.selection.as_ref().unwrap().pane, 0);
        assert!(app.tasks[0].scroll_offset > 0);
    }

    #[test]
    fn command_mode_page_keys_scroll_focused_pane() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        for line in 0..40 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }

        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE))
            .unwrap();
        let page_offset = app.tasks[0].scroll_offset;
        assert!(page_offset > 0);

        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        assert!(app.tasks[0].scroll_offset >= page_offset);

        app.handle_key(key(KeyCode::End, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tasks[0].scroll_offset, 0);
    }

    #[test]
    fn command_mode_y_copies_focused_visible_screen() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"hello\x1b[1GXY");

        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.clipboard, "XYllo");
    }

    #[test]
    fn command_mode_y_copies_scrolled_history_window() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        for line in 0..20 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }

        app.tasks[0].scroll_up(10);
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();

        assert!(app.clipboard.contains("line 6"));
        assert!(app.clipboard.contains("line 10"));
        assert!(!app.clipboard.contains("line 19"));
    }

    #[test]
    fn command_mode_shift_y_copies_full_focused_history() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        for line in 0..20 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }
        app.tasks[0].scroll_up(10);

        app.handle_key(key(KeyCode::Char('Y'), KeyModifiers::SHIFT))
            .unwrap();

        assert_eq!(app.clipboard.lines().next(), Some("line 0"));
        assert!(app.clipboard.contains("line 19"));
    }

    #[test]
    fn command_mode_search_jumps_to_matching_history_line() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        for line in 0..40 {
            if line == 5 {
                app.tasks[0].process_output(b"ERROR target\n");
            } else {
                app.tasks[0].process_output(format!("line {line}\n").as_bytes());
            }
        }

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "eror".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Command);
        assert!(app.tasks[0].scroll_offset > 0);
        assert_eq!(app.selection.as_ref().unwrap().pane, 0);
        assert_eq!(app.selected_text().unwrap(), "ERROR target");
    }

    #[test]
    fn command_mode_search_does_not_cross_panes() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"ERROR wrong pane\n");
        for line in 0..30 {
            if line == 10 {
                app.tasks[1].process_output(b"error right pane\n");
            } else {
                app.tasks[1].process_output(format!("other {line}\n").as_bytes());
            }
        }
        app.focus = 1;

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "error".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.focus, 1);
        assert_eq!(app.selection.as_ref().unwrap().pane, 1);
        assert!(app.selected_text().unwrap().contains("right pane"));
        assert_eq!(app.tasks[0].scroll_offset, 0);
    }

    #[test]
    fn command_mode_search_reports_no_match() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"ordinary output\n");

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "error".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Command);
        assert!(app.selection.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("No matches"))
        );
    }

    #[test]
    fn command_mode_repeats_previous_search_with_wraparound() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        for line in 0..35 {
            if matches!(line, 5 | 15 | 25) {
                app.tasks[0].process_output(format!("error {line}\n").as_bytes());
            } else {
                app.tasks[0].process_output(format!("line {line}\n").as_bytes());
            }
        }

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "error".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 25");

        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 15");

        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 5");

        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 25");

        app.handle_key(key(KeyCode::Char('N'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 5");
    }

    #[test]
    fn clearing_focused_pane_clears_its_selection() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.selection = Some(Selection {
            pane: 0,
            anchor: SelectionPoint { line: 0, column: 0 },
            cursor: SelectionPoint { line: 0, column: 3 },
            dragging: false,
            dragged: true,
            last_mouse: None,
            last_scroll: Instant::now(),
        });

        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::NONE))
            .unwrap();

        assert!(app.selection.is_none());
    }

    #[test]
    fn plain_child_mouse_click_clears_existing_selection() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.tasks[0].parser.process(b"\x1b[?1000h");
        app.selection = Some(Selection {
            pane: 0,
            anchor: SelectionPoint { line: 0, column: 0 },
            cursor: SelectionPoint { line: 0, column: 3 },
            dragging: false,
            dragged: true,
            last_mouse: None,
            last_scroll: Instant::now(),
        });

        let first = app.content_rects[0];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first.x,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert!(app.selection.is_none());
    }

    #[test]
    fn resize_clamps_scroll_offset_to_available_history() {
        let mut task = TaskRuntime::new(test_task("one"), PathBuf::from("."));
        task.resize(40, 5);
        for line in 0..20 {
            task.process_output(format!("line {line}\n").as_bytes());
        }
        task.scroll_to_top();
        assert!(task.scroll_offset > 0);

        task.resize(40, 40);

        assert_eq!(task.scroll_offset, 0);
    }

    fn test_task(name: &str) -> Task {
        Task {
            name: name.to_owned(),
            command: TaskCommand::Shell("echo ready".to_owned()),
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            watch: None,
            run_on_change: None,
            repeat: None,
        }
    }

    fn test_app() -> App {
        let loaded = LoadedConfig {
            path: PathBuf::from("/tmp/demons.toml"),
            root: PathBuf::from("."),
            config: Config {
                settings: Settings::default(),
                tasks: vec![test_task("one"), test_task("two")],
            },
        };
        let (tx, rx) = mpsc::sync_channel(8);
        App::new(loaded, tx, rx, Arc::new(Mutex::new(HashSet::new())))
    }
}
