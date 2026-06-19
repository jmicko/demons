use std::{
    collections::{BTreeMap, HashSet, VecDeque},
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
    config::{Leader, LoadedConfig, Task, TaskCommand, parse_start_delay},
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
const THEME_RED: Color = Color::Red;
const THEME_GREEN: Color = Color::Green;
const THEME_SNOW: Color = Color::Gray;
const THEME_GOLD: Color = Color::Yellow;
const THEME_COMMAND: Color = THEME_GOLD;
const THEME_HOLLY: Color = Color::DarkGray;
const THEME_BLACK: Color = Color::Black;
const THEME_WHITE: Color = Color::White;

type ProcessRegistry = Arc<Mutex<HashSet<u32>>>;

pub fn run(loaded: LoadedConfig) -> Result<()> {
    run_with_options(
        loaded,
        RunOptions {
            start_tasks: true,
            open_menu: false,
            quit_when_menu_closes: false,
        },
    )
}

pub fn configure(loaded: LoadedConfig) -> Result<()> {
    run_with_options(
        loaded,
        RunOptions {
            start_tasks: false,
            open_menu: true,
            quit_when_menu_closes: true,
        },
    )
}

fn run_with_options(loaded: LoadedConfig, options: RunOptions) -> Result<()> {
    let registry = Arc::new(Mutex::new(HashSet::new()));
    install_panic_hook(Arc::clone(&registry));
    let shutdown_requested = register_shutdown_signals()?;

    let mut terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to initialize terminal")?;
    terminal.clear().context("failed to clear terminal")?;

    let (tx, rx) = mpsc::sync_channel(1024);
    let mut app = App::new(loaded, tx, rx, registry, options.quit_when_menu_closes);
    if options.open_menu {
        app.open_menu(MenuTab::Tasks);
    }
    let initial_size = terminal.size().context("failed to read terminal size")?;
    let initial_area = Rect::new(0, 0, initial_size.width, initial_size.height);
    app.update_layout(initial_area);
    if options.start_tasks {
        app.spawn_all();
    }

    let loop_result = run_loop(&mut terminal, &mut app, &shutdown_requested);
    let shutdown_result = app.shutdown();
    terminal.show_cursor().ok();
    terminal_guard.restore();

    loop_result.and(shutdown_result)
}

#[derive(Clone, Copy, Debug)]
struct RunOptions {
    start_tasks: bool,
    open_menu: bool,
    quit_when_menu_closes: bool,
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
                if app.tasks_started {
                    terminal.draw(|frame| app.draw(frame)).ok();
                }
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
    footer_hits: Vec<FooterHit>,
    mouse_position: Option<(u16, u16)>,
    tx: SyncSender<ProcessEvent>,
    rx: Receiver<ProcessEvent>,
    registry: ProcessRegistry,
    dependency_indexes: Vec<Vec<usize>>,
    dependent_indexes: Vec<Vec<usize>>,
    stopping: bool,
    pending_escape: Option<Instant>,
    selection: Option<Selection>,
    clipboard: String,
    notice: Option<Notice>,
    fullscreen: bool,
    search: Option<SearchState>,
    menu: Option<MenuState>,
    confirm_quit: bool,
    quit_when_menu_closes: bool,
    tasks_started: bool,
    countdown_snapshot: Vec<Option<u64>>,
}

impl App {
    fn new(
        loaded: LoadedConfig,
        tx: SyncSender<ProcessEvent>,
        rx: Receiver<ProcessEvent>,
        registry: ProcessRegistry,
        quit_when_menu_closes: bool,
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
        let (dependency_indexes, dependent_indexes) = dependency_graph(&loaded.config.tasks);
        Self {
            loaded,
            tasks,
            focus: 0,
            mode: AppMode::Command,
            grid: Grid {
                columns: 1,
                rows: 1,
            },
            pane_rects: Vec::new(),
            content_rects: Vec::new(),
            footer_rect: None,
            mode_button_rect: None,
            footer_hits: Vec::new(),
            mouse_position: None,
            tx,
            rx,
            registry,
            dependency_indexes,
            dependent_indexes,
            stopping: false,
            pending_escape: None,
            selection: None,
            clipboard: String::new(),
            notice: None,
            fullscreen: false,
            search: None,
            menu: None,
            confirm_quit: false,
            quit_when_menu_closes,
            tasks_started: false,
            countdown_snapshot: Vec::new(),
        }
    }

    fn spawn_all(&mut self) {
        self.tasks_started = true;
        for index in 0..self.tasks.len() {
            self.tasks[index].start_requested = true;
        }
        self.tick_dependency_starts(Instant::now());
    }

    fn spawn(&mut self, index: usize) {
        if index >= self.tasks.len() {
            return;
        }
        self.tasks[index].pending_start = None;
        self.tasks[index].start_requested = false;
        let size = self.tasks[index].pty_size;
        if let Err(error) =
            self.tasks[index].spawn(index, size, self.tx.clone(), Arc::clone(&self.registry))
        {
            self.tasks[index].record_spawn_error(&error);
        }
    }

    fn update_layout(&mut self, terminal_area: Rect) {
        let footer_height = self.footer_height(terminal_area.width, terminal_area.height);
        let (pane_area, footer_rect) = if footer_height > 0 {
            (
                Rect::new(
                    terminal_area.x,
                    terminal_area.y,
                    terminal_area.width,
                    terminal_area.height.saturating_sub(footer_height),
                ),
                Some(Rect::new(
                    terminal_area.x,
                    terminal_area.bottom().saturating_sub(footer_height),
                    terminal_area.width,
                    footer_height,
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
        let now = Instant::now();
        let frame_area = frame.area();
        self.update_layout(frame_area);
        let buffer = frame.buffer_mut();
        self.footer_hits.clear();

        for index in 0..self.tasks.len() {
            let area = self.pane_rects[index];
            let content = self.content_rects[index];
            if area.width == 0 || area.height == 0 {
                continue;
            }
            let focused = index == self.focus;
            let border_color = match (focused, self.mode) {
                (true, AppMode::Input) => THEME_GREEN,
                (true, AppMode::Command) => THEME_COMMAND,
                (true, AppMode::Search) => THEME_GOLD,
                _ => THEME_HOLLY,
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
                Style::default().fg(if focused { THEME_GOLD } else { THEME_SNOW }),
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
            render_waiting_countdown(&self.tasks[index], content, now, buffer);
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
                footer_area.height,
            );
            let (mode_label, mode_color, items) = self.footer_parts(now);
            let mode_hovered = self
                .mouse_position
                .is_some_and(|(x, y)| contains(button_area, x, y));
            Paragraph::new(mode_label)
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().fg(THEME_BLACK).bg(if mode_hovered {
                    mode_hover_color(mode_color)
                } else {
                    mode_color
                }))
                .render(button_area, buffer);
            self.footer_hits = render_footer_items(&items, help_area, buffer, self.mouse_position);
        }

        if let Some(menu) = self.menu.as_mut() {
            render_menu(
                frame_area,
                buffer,
                menu,
                self.loaded.config.settings.leader.label(),
                self.quit_when_menu_closes,
                self.tasks_started,
                self.mouse_position,
            );
        }

        if self.confirm_quit {
            render_quit_confirm(frame_area, buffer);
        }

        if self.mode == AppMode::Input && self.menu.is_none() && !self.tasks.is_empty() {
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
                if self.menu.is_some() {
                    self.insert_menu_edit_text(&text);
                    return Ok(Action::Continue);
                }
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
        if self.confirm_quit {
            return self.handle_quit_confirm_key(key);
        }

        if self.menu.is_some() {
            return self.handle_menu_key(key);
        }

        if is_copy_key(key) {
            self.copy_selection();
            return Ok(Action::Continue);
        }
        if is_paste_key(key) {
            self.paste_clipboard_to_focus()?;
            return Ok(Action::Continue);
        }

        let leader = self.loaded.config.settings.leader;
        if leader.uses_escape_alt_encoding() {
            if let Some(started) = self.pending_escape.take() {
                if started.elapsed() <= ALT_ESCAPE_TIMEOUT && is_legacy_alt_leader(key, leader) {
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
            if is_quit_key(key) && !self.focused_task_accepts_input() {
                return Ok(self.request_quit());
            }
            if self.tasks.is_empty() {
                return Ok(Action::Continue);
            }
            let application_cursor = self.tasks[self.focus].parser.screen().application_cursor();
            let bytes = encode_key(key, application_cursor);
            if !bytes.is_empty() {
                self.tasks[self.focus].write_input(&bytes)?;
            }
            return Ok(Action::Continue);
        }

        match key.code {
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Left | KeyCode::Char('h') => self.move_focus(Direction::Left),
            KeyCode::Right | KeyCode::Char('l') => self.move_focus(Direction::Right),
            KeyCode::Up | KeyCode::Char('k') => self.move_focus(Direction::Up),
            KeyCode::Down | KeyCode::Char('j') => self.move_focus(Direction::Down),
            KeyCode::PageUp => {
                if self.tasks.is_empty() {
                    return Ok(Action::Continue);
                }
                let rows = self.focused_page_rows();
                self.tasks[self.focus].scroll_up(rows);
            }
            KeyCode::PageDown => {
                if self.tasks.is_empty() {
                    return Ok(Action::Continue);
                }
                let rows = self.focused_page_rows();
                self.tasks[self.focus].scroll_down(rows);
            }
            KeyCode::Home => {
                if let Some(task) = self.tasks.get_mut(self.focus) {
                    task.scroll_to_top();
                }
            }
            KeyCode::End => {
                if let Some(task) = self.tasks.get_mut(self.focus) {
                    task.scroll_to_bottom();
                }
            }
            KeyCode::Char('?') => self.open_menu(MenuTab::Help),
            KeyCode::Char('f') => self.fullscreen = !self.fullscreen,
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('y') => self.copy_focused_visible(),
            KeyCode::Char('Y') => self.copy_focused_history(),
            KeyCode::Char('S') => self.save_focused_history()?,
            KeyCode::Char('r') => self.request_restart(self.focus),
            KeyCode::Char('R') => self.request_restart_all(),
            KeyCode::Char('q') => return Ok(self.request_quit()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(self.request_quit());
            }
            KeyCode::Char('c') => {
                if let Some(task) = self.tasks.get_mut(self.focus) {
                    task.clear();
                    self.clear_selection_for(self.focus);
                }
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn request_quit(&mut self) -> Action {
        if self.confirm_quit {
            return Action::Quit;
        }
        self.confirm_quit = true;
        self.search = None;
        self.notice = None;
        Action::Continue
    }

    fn handle_quit_confirm_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => self.confirm_quit = false,
            KeyCode::Char('q') => return Ok(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Action::Quit);
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn open_menu(&mut self, tab: MenuTab) {
        self.menu = Some(MenuState::new(self.loaded.config.clone(), tab));
        self.search = None;
        self.notice = None;
        self.confirm_quit = false;
    }

    fn handle_menu_key(&mut self, key: KeyEvent) -> Result<Action> {
        if self
            .menu
            .as_ref()
            .and_then(|menu| menu.edit.as_ref())
            .is_some()
        {
            return self.handle_menu_edit_key(key);
        }
        if self
            .menu
            .as_ref()
            .and_then(|menu| menu.dependency_task)
            .is_some()
        {
            return self.handle_menu_dependency_key(key);
        }
        if self.menu.as_ref().is_some_and(|menu| menu.leader_picker) {
            return self.handle_menu_leader_key(key);
        }

        match key.code {
            KeyCode::Esc => return Ok(self.menu_back_or_close()),
            KeyCode::Left => self.cycle_menu_tab(-1),
            KeyCode::Right | KeyCode::Tab => self.cycle_menu_tab(1),
            KeyCode::BackTab => self.cycle_menu_tab(-1),
            KeyCode::Up | KeyCode::Char('k') => self.move_menu_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_menu_cursor(1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.activate_selected_menu_item();
            }
            KeyCode::Char('?') => return Ok(self.menu_back_or_close()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(self.request_quit());
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn handle_menu_dependency_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.dependency_task = None;
                    menu.dependency_cursor = 0;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_menu_dependency_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_menu_dependency_cursor(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected_dependency(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(self.request_quit());
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn handle_menu_leader_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.leader_picker = false;
                    menu.leader_cursor = 0;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_menu_leader_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_menu_leader_cursor(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.select_menu_leader(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(self.request_quit());
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn handle_menu_edit_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.edit = None;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(self.request_quit());
            }
            KeyCode::Enter => self.submit_menu_edit(),
            KeyCode::Tab => self.complete_menu_cwd(),
            KeyCode::Backspace => self.delete_menu_edit_char_before_cursor(),
            KeyCode::Delete => self.delete_menu_edit_char_at_cursor(),
            KeyCode::Left => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = edit.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = (edit.cursor + 1).min(char_count(&edit.value));
                }
            }
            KeyCode::Home => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = char_count(&edit.value);
                }
            }
            KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = 0;
                }
            }
            KeyCode::Char('e' | 'E') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.cursor = char_count(&edit.value);
                }
            }
            KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.value.clear();
                    edit.cursor = 0;
                }
            }
            KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    let index = byte_index_for_char(&edit.value, edit.cursor);
                    edit.value.truncate(index);
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_menu_edit_char(character);
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn cycle_menu_tab(&mut self, delta: isize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let index = menu.tab.index() as isize;
        let next = (index + delta).rem_euclid(MenuTab::ALL.len() as isize) as usize;
        menu.tab = MenuTab::ALL[next];
        menu.cursor = 0;
        menu.task_detail = None;
        menu.dependency_task = None;
    }

    fn move_menu_cursor(&mut self, delta: isize) {
        let configure_only = self.quit_when_menu_closes || !self.tasks_started;
        let count = self
            .menu
            .as_ref()
            .map(|menu| menu_item_count(menu, configure_only))
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.cursor = (menu.cursor as isize + delta).rem_euclid(count as isize) as usize;
            if menu.tab == MenuTab::Tasks && menu.task_detail.is_none() {
                menu.task_list_cursor = menu.cursor;
            }
        }
    }

    fn move_menu_dependency_cursor(&mut self, delta: isize) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let Some(task) = menu.dependency_task else {
            return;
        };
        let count = dependency_candidates(menu, task).len();
        if count == 0 {
            return;
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.dependency_cursor =
                (menu.dependency_cursor as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn move_menu_leader_cursor(&mut self, delta: isize) {
        let count = all_leaders().len();
        if let Some(menu) = self.menu.as_mut() {
            menu.leader_cursor =
                (menu.leader_cursor as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn select_menu_leader(&mut self) {
        let Some(index) = self.menu.as_ref().map(|menu| menu.leader_cursor) else {
            return;
        };
        let Some(&leader) = all_leaders().get(index) else {
            return;
        };
        self.set_menu_leader(leader);
    }

    fn activate_selected_menu_item(&mut self) -> Result<Action> {
        let Some(action) = self.selected_menu_action() else {
            return Ok(Action::Continue);
        };
        self.apply_menu_action(action)
    }

    fn selected_menu_action(&self) -> Option<MenuAction> {
        let menu = self.menu.as_ref()?;
        match menu.tab {
            MenuTab::Help => None,
            MenuTab::Tasks => {
                if menu.task_detail.is_some() {
                    return task_detail_fields()
                        .get(menu.cursor)
                        .copied()
                        .map(MenuAction::TaskField);
                }
                if menu.cursor < menu.draft.tasks.len() {
                    Some(MenuAction::OpenTask(menu.cursor))
                } else {
                    Some(MenuAction::AddTask)
                }
            }
            MenuTab::Settings => Some(MenuAction::OpenLeaderPicker),
            MenuTab::Exit => exit_actions(self.quit_when_menu_closes || !self.tasks_started)
                .get(menu.cursor)
                .copied()
                .map(MenuAction::Exit),
        }
    }

    fn apply_menu_action(&mut self, action: MenuAction) -> Result<Action> {
        match action {
            MenuAction::Tab(tab) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.edit = None;
                    menu.tab = tab;
                    menu.cursor = 0;
                    menu.task_detail = None;
                    menu.dependency_task = None;
                    menu.leader_picker = false;
                }
            }
            MenuAction::Close => return Ok(self.menu_back_or_close()),
            MenuAction::OpenTask(index) => {
                if let Some(menu) = self.menu.as_mut() {
                    if index < menu.draft.tasks.len() {
                        menu.task_list_cursor = index;
                        menu.task_detail = Some(index);
                        menu.cursor = 0;
                    }
                }
            }
            MenuAction::AddTask => self.add_menu_task(),
            MenuAction::TaskField(field) => self.activate_task_field(field),
            MenuAction::ToggleDependency(candidate) => self.toggle_dependency(candidate),
            MenuAction::OpenLeaderPicker => self.open_menu_leader_picker(),
            MenuAction::SelectLeader(leader) => self.set_menu_leader(leader),
            MenuAction::Exit(action) => return self.handle_menu_exit_action(action),
        }
        Ok(Action::Continue)
    }

    fn menu_back_or_close(&mut self) -> Action {
        let Some(menu) = self.menu.as_mut() else {
            return Action::Continue;
        };
        if menu.edit.is_some() {
            menu.edit = None;
            return Action::Continue;
        }
        if menu.dependency_task.is_some() {
            menu.dependency_task = None;
            menu.dependency_cursor = 0;
            return Action::Continue;
        }
        if menu.leader_picker {
            menu.leader_picker = false;
            menu.leader_cursor = 0;
            return Action::Continue;
        }
        if menu.task_detail.is_some() {
            menu.task_detail = None;
            menu.cursor = task_list_cursor(menu);
            return Action::Continue;
        }
        if menu.dirty() {
            menu.tab = MenuTab::Exit;
            menu.cursor = 0;
            self.set_notice("Use the Exit tab to save or discard menu changes.".to_owned());
            return Action::Continue;
        }
        self.menu = None;
        if self.quit_when_menu_closes {
            Action::Quit
        } else {
            Action::Continue
        }
    }

    fn add_menu_task(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let name = unique_task_name(&menu.draft, "task");
        menu.draft.tasks.push(Task {
            name,
            command: TaskCommand::Shell("echo ready".to_owned()),
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            depends_on: Vec::new(),
            start_delay: None,
            watch: None,
            run_on_change: None,
            repeat: None,
        });
        let index = menu.draft.tasks.len() - 1;
        menu.task_list_cursor = index;
        menu.task_detail = Some(index);
        menu.cursor = 0;
    }

    fn activate_task_field(&mut self, field: TaskField) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let Some(task) = menu.task_detail else {
            return;
        };
        match field {
            TaskField::Name
            | TaskField::Command
            | TaskField::Cwd
            | TaskField::Env
            | TaskField::StartDelay => {
                self.start_menu_edit(task, field);
            }
            TaskField::Dependencies => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.dependency_task = Some(task);
                    menu.dependency_cursor = 0;
                }
            }
            TaskField::Delete => self.delete_menu_task(task),
            TaskField::Back => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.task_detail = None;
                    menu.cursor = task_list_cursor(menu);
                }
            }
        }
    }

    fn start_menu_edit(&mut self, task_index: usize, field: TaskField) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task) = menu.draft.tasks.get(task_index) else {
            return;
        };
        let value = match field {
            TaskField::Name => task.name.clone(),
            TaskField::Command => task.command.display(),
            TaskField::Cwd => task.cwd.to_string_lossy().into_owned(),
            TaskField::Env => format_env_inline(&task.env),
            TaskField::StartDelay => task.start_delay.clone().unwrap_or_default(),
            _ => return,
        };
        menu.edit = Some(MenuEdit {
            task: task_index,
            field,
            cursor: char_count(&value),
            value,
        });
    }

    fn submit_menu_edit(&mut self) {
        let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.take()) else {
            return;
        };
        let value = edit.value.trim().to_owned();
        let result = self.apply_menu_edit(&edit, value);
        if let Err(error) = result {
            self.set_notice(format!("Edit not applied: {error:#}"));
            if let Some(menu) = self.menu.as_mut() {
                menu.edit = Some(edit);
            }
        }
    }

    fn apply_menu_edit(&mut self, edit: &MenuEdit, value: String) -> Result<()> {
        let root = self.loaded.root.clone();
        let Some(menu) = self.menu.as_mut() else {
            return Ok(());
        };
        let Some(task) = menu.draft.tasks.get_mut(edit.task) else {
            return Ok(());
        };
        match edit.field {
            TaskField::Name => {
                if value.is_empty() {
                    anyhow::bail!("task name cannot be empty");
                }
                task.name = value;
                scrub_missing_dependencies(&mut menu.draft);
            }
            TaskField::Command => {
                if value.is_empty() {
                    anyhow::bail!("command cannot be empty");
                }
                task.command = TaskCommand::Shell(value);
            }
            TaskField::Cwd => task.cwd = validate_menu_cwd(&root, &value)?,
            TaskField::Env => task.env = parse_env_inline(&value)?,
            TaskField::StartDelay => {
                if value.is_empty() {
                    task.start_delay = None;
                } else {
                    parse_start_delay(&value)?;
                    task.start_delay = Some(value);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn complete_menu_cwd(&mut self) {
        let root = self.loaded.root.clone();
        let Some((value, cursor)) = self.menu.as_ref().and_then(|menu| {
            let edit = menu.edit.as_ref()?;
            (edit.field == TaskField::Cwd).then(|| (edit.value.clone(), edit.cursor))
        }) else {
            return;
        };

        match complete_directory(&root, &value, cursor) {
            Ok(DirectoryCompletion::Updated { value, cursor }) => {
                if let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) {
                    edit.value = value;
                    edit.cursor = cursor;
                }
            }
            Ok(DirectoryCompletion::NoMatches) => {
                self.set_notice("No matching directories.".to_owned());
            }
            Ok(DirectoryCompletion::Ambiguous { matches }) => {
                self.set_notice(format!("{matches} matching directories. Keep typing."));
            }
            Err(error) => {
                self.set_notice(format!("Completion failed: {error:#}"));
            }
        }
    }

    fn delete_menu_task(&mut self, task_index: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if task_index >= menu.draft.tasks.len() {
            return;
        }
        let name = menu.draft.tasks[task_index].name.clone();
        menu.draft.tasks.remove(task_index);
        for task in &mut menu.draft.tasks {
            task.depends_on.retain(|dependency| dependency != &name);
        }
        menu.task_detail = None;
        menu.cursor = if menu.draft.tasks.is_empty() {
            0
        } else {
            task_index.min(menu.draft.tasks.len() - 1)
        };
        menu.task_list_cursor = menu.cursor;
    }

    fn toggle_selected_dependency(&mut self) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let Some(task) = menu.dependency_task else {
            return;
        };
        let candidates = dependency_candidates(menu, task);
        let Some(&candidate) = candidates.get(menu.dependency_cursor) else {
            return;
        };
        self.toggle_dependency(candidate);
    }

    fn toggle_dependency(&mut self, candidate: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task_index) = menu.dependency_task.or(menu.task_detail) else {
            return;
        };
        if task_index == candidate
            || task_index >= menu.draft.tasks.len()
            || candidate >= menu.draft.tasks.len()
        {
            return;
        }
        let name = menu.draft.tasks[candidate].name.clone();
        let dependencies = &mut menu.draft.tasks[task_index].depends_on;
        if let Some(position) = dependencies
            .iter()
            .position(|dependency| dependency == &name)
        {
            dependencies.remove(position);
        } else {
            dependencies.push(name);
            dependencies.sort();
        }
    }

    fn open_menu_leader_picker(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let leaders = all_leaders();
        let current = menu.draft.settings.leader;
        menu.leader_cursor = leaders
            .iter()
            .position(|leader| *leader == current)
            .unwrap_or(0);
        menu.leader_picker = true;
    }

    fn set_menu_leader(&mut self, leader: Leader) {
        if let Some(menu) = self.menu.as_mut() {
            menu.draft.settings.leader = leader;
            menu.leader_picker = false;
        }
        self.loaded.config.settings.leader = leader;
    }

    fn handle_menu_exit_action(&mut self, action: MenuExitAction) -> Result<Action> {
        match action {
            MenuExitAction::SaveAffected => self.save_menu_config(RestartMode::Affected),
            MenuExitAction::SaveAll => self.save_menu_config(RestartMode::All),
            MenuExitAction::SaveOnly => self.save_menu_config(RestartMode::None),
            MenuExitAction::Discard => {
                if let Some(menu) = self.menu.take() {
                    self.loaded.config = menu.original;
                    if !self.tasks_started {
                        self.rebuild_unstarted_tasks();
                    }
                }
                if self.quit_when_menu_closes {
                    Ok(Action::Quit)
                } else {
                    Ok(Action::Continue)
                }
            }
            MenuExitAction::Close => {
                if self.menu.as_ref().is_some_and(MenuState::dirty) {
                    self.set_notice("Save or discard changes before closing the menu.".to_owned());
                    Ok(Action::Continue)
                } else {
                    self.menu = None;
                    if self.quit_when_menu_closes {
                        Ok(Action::Quit)
                    } else {
                        Ok(Action::Continue)
                    }
                }
            }
        }
    }

    fn save_menu_config(&mut self, restart: RestartMode) -> Result<Action> {
        let Some(menu) = self.menu.as_ref() else {
            return Ok(Action::Continue);
        };
        let draft = menu.draft.clone();
        let loaded = LoadedConfig {
            path: self.loaded.path.clone(),
            root: self.loaded.root.clone(),
            config: draft.clone(),
        };
        if let Err(error) = loaded.save() {
            self.set_notice(format!("Config not saved: {error:#}"));
            return Ok(Action::Continue);
        }

        let old = self.loaded.config.clone();
        self.menu = None;
        self.apply_saved_config(old, draft, restart);
        if self.quit_when_menu_closes {
            Ok(Action::Quit)
        } else {
            Ok(Action::Continue)
        }
    }

    fn apply_saved_config(
        &mut self,
        old: crate::config::Config,
        new: crate::config::Config,
        restart: RestartMode,
    ) {
        if !self.tasks_started {
            self.loaded.config = new;
            self.rebuild_unstarted_tasks();
            self.set_notice(format!("Saved {}.", self.loaded.path.display()));
            return;
        }

        let same_runtime_shape = old.tasks.len() == new.tasks.len()
            && old
                .tasks
                .iter()
                .zip(&new.tasks)
                .all(|(old, new)| old.name == new.name);

        if same_runtime_shape {
            let changed = old
                .tasks
                .iter()
                .zip(&new.tasks)
                .enumerate()
                .filter_map(|(index, (old, new))| (old != new).then_some(index))
                .collect::<Vec<_>>();
            self.loaded.config = new;
            for index in 0..self.tasks.len() {
                let task = self.loaded.config.tasks[index].clone();
                self.tasks[index].cwd = self.loaded.task_cwd(&task);
                self.tasks[index].start_delay = task
                    .start_delay
                    .as_deref()
                    .and_then(|delay| parse_start_delay(delay).ok())
                    .unwrap_or_default();
                self.tasks[index].task = task;
            }
            self.rebuild_dependency_graph_from_runtime();
            match restart {
                RestartMode::All => self.request_restart_all(),
                RestartMode::Affected => {
                    let mut indexes = Vec::new();
                    for index in changed {
                        indexes.extend(self.restart_closure(index));
                    }
                    indexes.sort_unstable();
                    indexes.dedup();
                    self.request_restart_set(&indexes);
                }
                RestartMode::None => {}
            }
            self.set_notice(format!("Saved {}.", self.loaded.path.display()));
        } else {
            self.loaded.config = new;
            self.rebuild_dependency_graph_from_runtime();
            self.set_notice(format!(
                "Saved {}; restart Demons to use added, removed, or renamed tasks.",
                self.loaded.path.display()
            ));
        }
    }

    fn rebuild_unstarted_tasks(&mut self) {
        self.tasks = self
            .loaded
            .config
            .tasks
            .iter()
            .cloned()
            .map(|task| {
                let cwd = self.loaded.task_cwd(&task);
                TaskRuntime::new(task, cwd)
            })
            .collect();
        self.focus = self.focus.min(self.tasks.len().saturating_sub(1));
        self.rebuild_dependency_graph_from_runtime();
    }

    fn rebuild_dependency_graph_from_runtime(&mut self) {
        let tasks = self
            .tasks
            .iter()
            .map(|runtime| runtime.task.clone())
            .collect::<Vec<_>>();
        let (dependency_indexes, dependent_indexes) = dependency_graph(&tasks);
        self.dependency_indexes = dependency_indexes;
        self.dependent_indexes = dependent_indexes;
    }

    fn insert_menu_edit_char(&mut self, character: char) {
        let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) else {
            return;
        };
        if character.is_control() {
            return;
        }
        let index = byte_index_for_char(&edit.value, edit.cursor);
        edit.value.insert(index, character);
        edit.cursor += 1;
    }

    fn insert_menu_edit_text(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.insert_menu_edit_char(character);
        }
    }

    fn delete_menu_edit_char_before_cursor(&mut self) {
        let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) else {
            return;
        };
        if edit.cursor == 0 {
            return;
        }
        let start = byte_index_for_char(&edit.value, edit.cursor - 1);
        let end = byte_index_for_char(&edit.value, edit.cursor);
        edit.value.replace_range(start..end, "");
        edit.cursor -= 1;
    }

    fn delete_menu_edit_char_at_cursor(&mut self) {
        let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.as_mut()) else {
            return;
        };
        if edit.cursor >= char_count(&edit.value) {
            return;
        }
        let start = byte_index_for_char(&edit.value, edit.cursor);
        let end = byte_index_for_char(&edit.value, edit.cursor + 1);
        edit.value.replace_range(start..end, "");
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter | KeyCode::Char('\n' | '\r')
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.submit_search(SearchDirection::Newer)
            }
            KeyCode::Enter | KeyCode::Char('\n' | '\r') => {
                self.submit_search(SearchDirection::Older)
            }
            KeyCode::Tab => self.cycle_search_pane(1),
            KeyCode::BackTab => self.cycle_search_pane(-1),
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
                self.refresh_search_results(None);
            }
            KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = self.search.as_mut() {
                    let index = byte_index_for_char(&search.query, search.cursor);
                    search.query.truncate(index);
                }
                self.refresh_search_results(None);
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
        if self.tasks.is_empty() {
            self.set_notice("No task panes are configured.".to_owned());
            return;
        }
        self.search = Some(SearchState {
            pane: self.focus,
            query: String::new(),
            cursor: 0,
            current: None,
            current_index: None,
            match_count: 0,
            message: None,
        });
        self.selection = None;
        self.notice = None;
        self.mode = AppMode::Search;
    }

    fn cancel_search(&mut self) {
        self.search = None;
        self.mode = AppMode::Command;
    }

    fn submit_search(&mut self, direction: SearchDirection) {
        self.move_search_result(direction);
    }

    fn refresh_search_results(&mut self, empty_message: Option<&'static str>) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let pane = search.pane;
        let query = search.query.trim().to_owned();
        if query.is_empty() {
            self.clear_search_results(empty_message.map(str::to_owned));
            return;
        }
        let history = &self.tasks[pane].history;
        let matches = history.matching_lines(&query);
        if matches.is_empty() {
            self.clear_search_results(Some("0 matches".to_owned()));
            return;
        }

        self.select_search_match(pane, &matches, matches.len() - 1);
    }

    fn move_search_result(&mut self, direction: SearchDirection) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let pane = search.pane;
        let query = search.query.trim().to_owned();
        let current = search.current;
        let stored_index = search.current_index;
        if query.is_empty() {
            self.clear_search_results(Some("type a query".to_owned()));
            return;
        }

        let matches = self.tasks[pane].history.matching_lines(&query);
        if matches.is_empty() {
            self.clear_search_results(Some("0 matches".to_owned()));
            return;
        }

        let current_index = current
            .and_then(|line| matches.iter().position(|candidate| *candidate == line))
            .or_else(|| stored_index.filter(|index| *index < matches.len()));
        let next_index = match (direction, current_index) {
            (SearchDirection::Older, Some(0)) => matches.len() - 1,
            (SearchDirection::Older, Some(index)) => index - 1,
            (SearchDirection::Newer, Some(index)) => (index + 1) % matches.len(),
            (_, None) => 0,
        };

        self.select_search_match(pane, &matches, next_index);
    }

    fn select_search_match(&mut self, pane: usize, matches: &[u64], index: usize) {
        let Some(&line) = matches.get(index) else {
            return;
        };

        self.jump_to_search_line(pane, line);
        if let Some(search) = self.search.as_mut() {
            search.current = Some(line);
            search.current_index = Some(index);
            search.match_count = matches.len();
            search.message = None;
        }
    }

    fn clear_search_results(&mut self, message: Option<String>) {
        if let Some(search) = self.search.as_mut() {
            search.current = None;
            search.current_index = None;
            search.match_count = 0;
            search.message = message;
        }
        self.selection = None;
    }

    fn jump_to_search_line(&mut self, pane: usize, line: u64) {
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
            history_backed: true,
            dragging: false,
            dragged: true,
            last_mouse: None,
            last_scroll: Instant::now(),
        });
    }

    fn insert_search_text(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.insert_search_char_raw(character);
        }
        self.refresh_search_results(None);
    }

    fn insert_search_char(&mut self, character: char) {
        self.insert_search_char_raw(character);
        self.refresh_search_results(None);
    }

    fn insert_search_char_raw(&mut self, character: char) {
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
        self.refresh_search_results(None);
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
        self.refresh_search_results(None);
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<Action> {
        self.mouse_position = Some((mouse.column, mouse.row));

        if self.confirm_quit {
            return Ok(Action::Continue);
        }

        if self.menu.is_some() {
            return self.handle_menu_mouse(mouse);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.mode_button_hit(mouse.column, mouse.row)
        {
            self.toggle_mode();
            return Ok(Action::Continue);
        }

        if let Some(action) = self.footer_action_at(mouse) {
            return self.apply_footer_action(action);
        }

        if self
            .footer_rect
            .is_some_and(|rect| contains(rect, mouse.column, mouse.row))
        {
            return Ok(Action::Continue);
        }

        if self.mode == AppMode::Search {
            return self.handle_search_mouse(mouse);
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

    fn handle_search_mouse(&mut self, mouse: MouseEvent) -> Result<Action> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) && self.copy_selection() {
            return Ok(Action::Continue);
        }

        let Some(index) = self.pane_at(mouse.column, mouse.row) else {
            return Ok(Action::Continue);
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = index;
                self.set_search_pane(index);
            }
            MouseEventKind::Down(_) => {
                self.focus = index;
            }
            MouseEventKind::ScrollUp => {
                self.tasks[index].scroll_up(3);
            }
            MouseEventKind::ScrollDown => {
                self.tasks[index].scroll_down(3);
            }
            _ => {}
        }

        Ok(Action::Continue)
    }

    fn handle_menu_mouse(&mut self, mouse: MouseEvent) -> Result<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.dependency_task.is_some())
                {
                    self.move_menu_dependency_cursor(-1);
                } else if self.menu.as_ref().is_some_and(|menu| menu.leader_picker) {
                    self.move_menu_leader_cursor(-1);
                } else {
                    self.move_menu_cursor(-1);
                }
                return Ok(Action::Continue);
            }
            MouseEventKind::ScrollDown => {
                if self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.dependency_task.is_some())
                {
                    self.move_menu_dependency_cursor(1);
                } else if self.menu.as_ref().is_some_and(|menu| menu.leader_picker) {
                    self.move_menu_leader_cursor(1);
                } else {
                    self.move_menu_cursor(1);
                }
                return Ok(Action::Continue);
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return Ok(Action::Continue),
        }
        let action = self.menu.as_ref().and_then(|menu| {
            menu.hits
                .iter()
                .find(|hit| contains(hit.rect, mouse.column, mouse.row))
                .map(|hit| hit.action)
        });
        let Some(action) = action else {
            return Ok(Action::Continue);
        };
        self.apply_menu_action(action)
    }

    fn footer_action_at(&self, mouse: MouseEvent) -> Option<FooterAction> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        self.footer_hits
            .iter()
            .find(|hit| contains(hit.rect, mouse.column, mouse.row))
            .map(|hit| hit.action)
    }

    fn apply_footer_action(&mut self, action: FooterAction) -> Result<Action> {
        match action {
            FooterAction::ToggleFullscreen => self.fullscreen = !self.fullscreen,
            FooterAction::StartSearch => self.start_search(),
            FooterAction::SearchOlder => self.submit_search(SearchDirection::Older),
            FooterAction::SearchNewer => self.submit_search(SearchDirection::Newer),
            FooterAction::SearchNextPane => self.cycle_search_pane(1),
            FooterAction::SearchDone => self.cancel_search(),
            FooterAction::CopyVisible => self.copy_focused_visible(),
            FooterAction::CopyHistory => self.copy_focused_history(),
            FooterAction::SaveHistory => self.save_focused_history()?,
            FooterAction::ShowMenu => self.open_menu(MenuTab::Help),
            FooterAction::RestartFocused => self.request_restart(self.focus),
            FooterAction::RestartAll => self.request_restart_all(),
            FooterAction::ClearFocused => {
                if let Some(task) = self.tasks.get_mut(self.focus) {
                    task.clear();
                    self.clear_selection_for(self.focus);
                }
            }
            FooterAction::Quit => return Ok(self.request_quit()),
        }
        Ok(Action::Continue)
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            AppMode::Input => AppMode::Command,
            AppMode::Command | AppMode::Search => {
                self.search = None;
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
            history_backed: false,
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
        if !selection.history_backed {
            if let Some(text) = self.visible_selection_text(selection) {
                if !text.is_empty() {
                    return Some(text);
                }
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
        if self.clipboard.is_empty() || self.mode != AppMode::Input || index >= self.tasks.len() {
            return Ok(false);
        }
        let text = self.clipboard.clone();
        self.paste_text_to_task(index, &text)?;
        self.set_notice(format!("Pasted {} characters.", text.chars().count()));
        Ok(true)
    }

    fn paste_text_to_task(&mut self, index: usize, text: &str) -> Result<()> {
        if index >= self.tasks.len() {
            return Ok(());
        }
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
        if self.tasks.is_empty() {
            self.focus = 0;
            return;
        }
        let count = self.tasks.len() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(count) as usize;
    }

    fn move_focus(&mut self, direction: Direction) {
        if self.tasks.is_empty() {
            return;
        }
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

    fn set_search_pane(&mut self, pane: usize) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.pane != pane {
            search.pane = pane;
            search.current = None;
            search.current_index = None;
            search.match_count = 0;
            search.message = None;
            self.selection = None;
            self.notice = None;
            self.refresh_search_results(None);
        }
    }

    fn cycle_search_pane(&mut self, delta: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let count = self.tasks.len() as isize;
        let current = self
            .search
            .as_ref()
            .map(|search| search.pane)
            .unwrap_or(self.focus);
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.focus = next;
        self.set_search_pane(next);
    }

    fn focused_page_rows(&self) -> usize {
        self.content_rects
            .get(self.focus)
            .map(|rect| usize::from(rect.height.saturating_sub(1).max(1)))
            .unwrap_or(1)
    }

    fn footer_height(&self, terminal_width: u16, terminal_height: u16) -> u16 {
        if terminal_height <= 3 || terminal_width == 0 {
            return 0;
        }
        let button_width = terminal_width.min(MODE_BUTTON_WIDTH);
        let help_width = terminal_width.saturating_sub(button_width);
        if help_width == 0 {
            return 1;
        }
        let (_, _, items) = self.footer_parts(Instant::now());
        let line_count = footer_line_count(&items, help_width);
        line_count.min(terminal_height.saturating_sub(3).max(1))
    }

    fn footer_parts(&self, now: Instant) -> (&'static str, Color, Vec<FooterItem>) {
        if let Some(search) = self
            .search
            .as_ref()
            .filter(|_| self.mode == AppMode::Search)
        {
            return ("SEARCH", THEME_GOLD, search_footer_items(search));
        }

        if let Some(notice) = self.active_notice(now) {
            let (label, color) = self.mode_label_color();
            return (label, color, vec![footer_status(notice.to_string())]);
        }

        match self.mode {
            AppMode::Input => (
                "INPUT MODE",
                THEME_GREEN,
                vec![
                    footer_status(
                        self.tasks
                            .get(self.focus)
                            .map(|task| task.task.name.clone())
                            .unwrap_or_else(|| "no tasks configured".to_owned()),
                    ),
                    footer_status("drag select"),
                    footer_status("right-click copy"),
                ],
            ),
            AppMode::Command => ("COMMAND MODE", THEME_COMMAND, command_footer_items()),
            AppMode::Search => ("SEARCH", THEME_GOLD, search_placeholder_footer_items()),
        }
    }

    fn mode_label_color(&self) -> (&'static str, Color) {
        match self.mode {
            AppMode::Input => ("INPUT MODE", THEME_GREEN),
            AppMode::Command => ("COMMAND MODE", THEME_COMMAND),
            AppMode::Search => ("SEARCH", THEME_GOLD),
        }
    }

    fn focused_task_accepts_input(&self) -> bool {
        self.tasks
            .get(self.focus)
            .is_some_and(|task| task.writer.is_some())
    }

    fn request_restart(&mut self, index: usize) {
        if index >= self.tasks.len() {
            return;
        }
        let indexes = self.restart_closure(index);
        self.request_restart_set(&indexes);
    }

    fn request_restart_all(&mut self) {
        let indexes = (0..self.tasks.len()).collect::<Vec<_>>();
        self.request_restart_set(&indexes);
    }

    fn request_restart_set(&mut self, indexes: &[usize]) {
        if self.stopping {
            return;
        }
        let indexes = self.restart_order(indexes);
        let now = Instant::now();
        for &index in &indexes {
            if index >= self.tasks.len() {
                continue;
            }
            self.tasks[index].start_requested = true;
            self.tasks[index].pending_start = None;
            if self.tasks[index].pid.is_none() {
                self.tasks[index].restart_requested = false;
            }
        }
        for &index in indexes.iter().rev() {
            if index >= self.tasks.len() {
                continue;
            }
            if let Some(pid) = self.tasks[index].pid {
                self.tasks[index].restart_requested = true;
                self.tasks[index].kill_deadline = Some(now + RESTART_GRACE);
                self.tasks[index].status = TaskStatus::Restarting;
                self.tasks[index].message("\r\n\x1b[33m[demons] restarting...\x1b[0m\r\n");
                if signal_process_group(pid, libc::SIGTERM).is_err() {
                    self.tasks[index].kill_deadline = Some(now);
                }
            }
        }
        self.tick_dependency_starts(now);
    }

    fn restart_order(&self, indexes: &[usize]) -> Vec<usize> {
        let mut requested = vec![false; self.tasks.len()];
        for &index in indexes {
            if index < requested.len() {
                requested[index] = true;
            }
        }

        let mut seen = vec![false; self.tasks.len()];
        let mut visiting = vec![false; self.tasks.len()];
        let mut ordered = Vec::new();
        for &index in indexes {
            self.collect_restart_order(index, &requested, &mut seen, &mut visiting, &mut ordered);
        }
        ordered
    }

    fn collect_restart_order(
        &self,
        index: usize,
        requested: &[bool],
        seen: &mut [bool],
        visiting: &mut [bool],
        ordered: &mut Vec<usize>,
    ) {
        if index >= requested.len() || !requested[index] || seen[index] || visiting[index] {
            return;
        }

        visiting[index] = true;
        for &dependency in self.dependency_indexes.get(index).into_iter().flatten() {
            self.collect_restart_order(dependency, requested, seen, visiting, ordered);
        }
        visiting[index] = false;
        seen[index] = true;
        ordered.push(index);
    }

    fn restart_closure(&self, index: usize) -> Vec<usize> {
        let mut seen = vec![false; self.tasks.len()];
        let mut ordered = Vec::new();
        self.collect_dependents(index, &mut seen, &mut ordered);
        ordered
    }

    fn collect_dependents(&self, index: usize, seen: &mut [bool], ordered: &mut Vec<usize>) {
        if index >= seen.len() || seen[index] {
            return;
        }
        seen[index] = true;
        ordered.push(index);
        for &dependent in self.dependent_indexes.get(index).into_iter().flatten() {
            self.collect_dependents(dependent, seen, ordered);
        }
    }

    fn dependencies_ready(&self, index: usize) -> bool {
        self.dependency_indexes
            .get(index)
            .into_iter()
            .flatten()
            .all(|&dependency| {
                self.tasks.get(dependency).is_some_and(|task| {
                    task.pid.is_some()
                        && !task.restart_requested
                        && matches!(task.status, TaskStatus::Starting | TaskStatus::Running)
                })
            })
    }

    fn tick_dependency_starts(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for index in 0..self.tasks.len() {
            if !self.tasks[index].start_requested
                || self.tasks[index].pid.is_some()
                || self.tasks[index].restart_requested
            {
                continue;
            }
            if !self.dependencies_ready(index) {
                if self.tasks[index].status != TaskStatus::Waiting {
                    self.tasks[index].status = TaskStatus::Waiting;
                    changed = true;
                }
                self.tasks[index].pending_start = None;
                continue;
            }
            let deadline = match self.tasks[index].pending_start {
                Some(deadline) => deadline,
                None => {
                    let deadline = now + self.tasks[index].start_delay;
                    self.tasks[index].pending_start = Some(deadline);
                    if self.tasks[index].start_delay > Duration::ZERO {
                        self.tasks[index].status = TaskStatus::Waiting;
                        changed = true;
                    }
                    deadline
                }
            };
            if deadline <= now {
                self.spawn(index);
                changed = true;
            }
        }
        changed
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
                    self.tasks[task].start_requested = true;
                    self.tasks[task].pending_start = None;
                }
                self.tick_dependency_starts(Instant::now());
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
        if self.tick_dependency_starts(now) {
            changed = true;
        }
        let countdown_snapshot = self.waiting_countdown_snapshot(now);
        if countdown_snapshot != self.countdown_snapshot {
            self.countdown_snapshot = countdown_snapshot;
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

    fn waiting_countdown_snapshot(&self, now: Instant) -> Vec<Option<u64>> {
        self.tasks
            .iter()
            .map(|task| {
                task.pending_start
                    .map(|deadline| countdown_seconds(deadline, now))
            })
            .collect()
    }

    fn apply_escape(&mut self) -> Result<()> {
        match self.mode {
            AppMode::Input => {
                if let Some(task) = self.tasks.get_mut(self.focus) {
                    task.write_input(b"\x1b")?;
                }
            }
            AppMode::Command => {}
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
    start_requested: bool,
    pending_start: Option<Instant>,
    start_delay: Duration,
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
        let start_delay = task
            .start_delay
            .as_deref()
            .and_then(|delay| parse_start_delay(delay).ok())
            .unwrap_or_default();
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
            start_requested: false,
            pending_start: None,
            start_delay,
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
            TaskStatus::NotStarted => ("⏸".to_owned(), THEME_HOLLY),
            TaskStatus::Waiting => ("⏱".to_owned(), THEME_GOLD),
            TaskStatus::Starting => ("…".to_owned(), THEME_GOLD),
            TaskStatus::Running => ("●".to_owned(), THEME_GREEN),
            TaskStatus::Restarting => ("↻".to_owned(), THEME_GOLD),
            TaskStatus::Stopping => ("■".to_owned(), THEME_GOLD),
            TaskStatus::Failed => ("✗".to_owned(), THEME_RED),
            TaskStatus::Exited {
                code,
                success,
                signal,
            } => {
                if *success {
                    ("✓".to_owned(), THEME_GREEN)
                } else if let Some(signal) = signal {
                    (format!("✗ {signal}"), THEME_RED)
                } else {
                    (format!("✗ {code}"), THEME_RED)
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
    history_backed: bool,
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

    fn matching_lines(&self, query: &str) -> Vec<u64> {
        let Some(needle) = search_needle(query) else {
            return Vec::new();
        };

        (self.first_index..self.line_count())
            .filter(|line_index| self.line_matches(*line_index, &needle))
            .collect()
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum TaskStatus {
    NotStarted,
    Waiting,
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
    current: Option<u64>,
    current_index: Option<usize>,
    match_count: usize,
    message: Option<String>,
}

#[derive(Clone, Debug)]
struct MenuState {
    tab: MenuTab,
    cursor: usize,
    task_list_cursor: usize,
    task_detail: Option<usize>,
    dependency_task: Option<usize>,
    dependency_cursor: usize,
    leader_picker: bool,
    leader_cursor: usize,
    edit: Option<MenuEdit>,
    draft: crate::config::Config,
    original: crate::config::Config,
    hits: Vec<MenuHit>,
}

impl MenuState {
    fn new(config: crate::config::Config, tab: MenuTab) -> Self {
        Self {
            tab,
            cursor: 0,
            task_list_cursor: 0,
            task_detail: None,
            dependency_task: None,
            dependency_cursor: 0,
            leader_picker: false,
            leader_cursor: 0,
            edit: None,
            draft: config.clone(),
            original: config,
            hits: Vec::new(),
        }
    }

    fn dirty(&self) -> bool {
        self.draft != self.original
    }
}

#[derive(Clone, Debug)]
struct MenuEdit {
    task: usize,
    field: TaskField,
    value: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuTab {
    Help,
    Tasks,
    Settings,
    Exit,
}

impl MenuTab {
    const ALL: [Self; 4] = [Self::Help, Self::Tasks, Self::Settings, Self::Exit];

    fn label(self) -> &'static str {
        match self {
            Self::Help => "Help",
            Self::Tasks => "Tasks",
            Self::Settings => "Settings",
            Self::Exit => "Exit",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskField {
    Name,
    Command,
    Cwd,
    Env,
    Dependencies,
    StartDelay,
    Delete,
    Back,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuExitAction {
    SaveAffected,
    SaveAll,
    SaveOnly,
    Discard,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartMode {
    None,
    Affected,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Tab(MenuTab),
    Close,
    OpenTask(usize),
    AddTask,
    TaskField(TaskField),
    ToggleDependency(usize),
    OpenLeaderPicker,
    SelectLeader(Leader),
    Exit(MenuExitAction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryCompletion {
    Updated { value: String, cursor: usize },
    NoMatches,
    Ambiguous { matches: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MenuHit {
    rect: Rect,
    action: MenuAction,
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

fn dependency_graph(tasks: &[Task]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let names = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.name.as_str(), index))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut dependencies = vec![Vec::new(); tasks.len()];
    let mut dependents = vec![Vec::new(); tasks.len()];
    for (index, task) in tasks.iter().enumerate() {
        for dependency in &task.depends_on {
            let Some(&dependency_index) = names.get(dependency.as_str()) else {
                continue;
            };
            dependencies[index].push(dependency_index);
            dependents[dependency_index].push(index);
        }
    }
    (dependencies, dependents)
}

fn countdown_seconds(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return 0;
    }
    remaining
        .as_millis()
        .saturating_add(999)
        .saturating_div(1000)
        .min(u128::from(u64::MAX)) as u64
}

fn render_menu(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    leader: &str,
    configure_only: bool,
    tasks_started: bool,
    hover_position: Option<(u16, u16)>,
) {
    let popup = centered_rect(area, 92, 26);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    menu.hits.clear();
    Clear.render(popup, buffer);
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME_RED))
        .style(Style::default().fg(THEME_SNOW).bg(THEME_BLACK))
        .title(Line::styled(
            " Demons Menu ",
            Style::default()
                .fg(THEME_GREEN)
                .add_modifier(Modifier::BOLD),
        ))
        .render(popup, buffer);

    if popup.width >= 8 {
        let close = Rect::new(popup.right().saturating_sub(5), popup.y, 3, 1);
        let close_hovered = hover_position.is_some_and(|(x, y)| contains(close, x, y));
        render_text(
            buffer,
            close,
            " x ",
            Style::default()
                .fg(THEME_BLACK)
                .bg(if close_hovered { THEME_RED } else { THEME_SNOW }),
        );
        menu.hits.push(MenuHit {
            rect: close,
            action: MenuAction::Close,
        });
    }

    let inner = inset_rect(popup, 2, 1);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut tab_x = inner.x;
    for (index, tab) in MenuTab::ALL.iter().enumerate() {
        let label = format!(" {} ", tab.label());
        let width = char_count(&label).min(usize::from(u16::MAX)) as u16;
        if tab_x.saturating_add(width) > inner.right() {
            break;
        }
        let selected = *tab == menu.tab;
        let rect = Rect::new(tab_x, inner.y, width, 1);
        let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
        let style = if selected {
            Style::default().fg(THEME_BLACK).bg(THEME_GREEN)
        } else if hovered {
            Style::default().fg(THEME_BLACK).bg(THEME_GOLD)
        } else if index % 2 == 0 {
            Style::default().fg(THEME_BLACK).bg(THEME_SNOW)
        } else {
            Style::default().fg(THEME_BLACK).bg(THEME_RED)
        };
        render_text(buffer, rect, &label, style);
        menu.hits.push(MenuHit {
            rect,
            action: MenuAction::Tab(*tab),
        });
        tab_x = tab_x.saturating_add(width);
    }
    if menu.dirty() && inner.width > 10 {
        let text = " unsaved ";
        let width = char_count(text) as u16;
        let rect = Rect::new(inner.right().saturating_sub(width), inner.y, width, 1);
        render_text(
            buffer,
            rect,
            text,
            Style::default().fg(THEME_BLACK).bg(THEME_GOLD),
        );
    }

    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    clear_rect(
        buffer,
        body,
        Style::default().fg(THEME_SNOW).bg(THEME_BLACK),
    );

    if let Some(edit) = menu.edit.as_ref() {
        render_menu_edit(body, buffer, edit);
        return;
    }
    if let Some(task) = menu.dependency_task {
        render_menu_dependencies(body, buffer, menu, task, hover_position);
        return;
    }
    if menu.leader_picker {
        render_menu_leaders(body, buffer, menu, hover_position);
        return;
    }

    match menu.tab {
        MenuTab::Help => render_menu_help(body, buffer, leader),
        MenuTab::Tasks => render_menu_tasks(body, buffer, menu, hover_position),
        MenuTab::Settings => render_menu_settings(body, buffer, menu, hover_position),
        MenuTab::Exit => render_menu_exit(
            body,
            buffer,
            menu,
            configure_only,
            tasks_started,
            hover_position,
        ),
    }
}

fn render_menu_help(area: Rect, buffer: &mut Buffer, leader: &str) {
    let lines = vec![
        "Command mode".to_owned(),
        "arrows / h j k l       Move focus".to_owned(),
        "Tab / Shift-Tab        Cycle panes".to_owned(),
        "f                      Toggle fullscreen pane".to_owned(),
        "PageUp/PageDown        Scroll focused pane".to_owned(),
        "Home/End               Jump to top/bottom of history".to_owned(),
        "drag / right-click     Select and copy pane text".to_owned(),
        "y / Y                  Copy visible text / full scrollback".to_owned(),
        "S                      Save full scrollback to a temp log".to_owned(),
        "/                      Search focused pane".to_owned(),
        "Enter / Shift-Enter    Previous / next search match".to_owned(),
        "Tab / Shift-Tab        Change searched pane while searching".to_owned(),
        "r                      Restart focused task and dependents".to_owned(),
        "R                      Restart every task".to_owned(),
        "c                      Clear focused pane".to_owned(),
        "?                      Open this menu".to_owned(),
        "q or Ctrl-C            Close Demons with confirmation".to_owned(),
        format!("{leader}                 Return to input mode outside the menu"),
        "".to_owned(),
        "Menu".to_owned(),
        "arrows / wheel         Move through visible options".to_owned(),
        "Enter / click          Activate an option".to_owned(),
        "Space                  Toggle dependency checkboxes".to_owned(),
        "Esc                    Back out one level".to_owned(),
    ];
    for (row, line) in lines.iter().enumerate() {
        if row >= usize::from(area.height) {
            break;
        }
        let style = if line == "Command mode" || line == "Menu" {
            Style::default()
                .fg(THEME_GREEN)
                .bg(THEME_BLACK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME_SNOW).bg(THEME_BLACK)
        };
        render_text(
            buffer,
            Rect::new(area.x, area.y + row as u16, area.width, 1),
            line,
            style,
        );
    }
}

fn render_menu_tasks(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    hover_position: Option<(u16, u16)>,
) {
    if let Some(task_index) = menu.task_detail {
        render_menu_task_detail(area, buffer, menu, task_index, hover_position);
        return;
    }
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        "Tasks",
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let rows = area.height.saturating_sub(1);
    let count = menu.draft.tasks.len() + 1;
    let start = scroll_start(menu.cursor, count, usize::from(rows));
    for row in 0..rows {
        let index = start + usize::from(row);
        let y = area.y + row + 1;
        let (text, action) = if index < menu.draft.tasks.len() {
            let task = &menu.draft.tasks[index];
            (
                format!("{}  {}", task.name, task.command.display()),
                MenuAction::OpenTask(index),
            )
        } else if index == menu.draft.tasks.len() {
            ("+ Add task".to_owned(), MenuAction::AddTask)
        } else {
            break;
        };
        render_menu_row(
            buffer,
            Rect::new(area.x, y, area.width, 1),
            &text,
            index == menu.cursor,
            Some(action),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_task_detail(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    task_index: usize,
    hover_position: Option<(u16, u16)>,
) {
    let Some(task) = menu.draft.tasks.get(task_index) else {
        return;
    };
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        &format!("Task: {}", task.name),
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let fields = task_detail_fields();
    for (row, field) in fields.iter().enumerate() {
        if row + 1 >= usize::from(area.height) {
            break;
        }
        let text = task_field_text(task, *field);
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
            &text,
            row == menu.cursor,
            Some(MenuAction::TaskField(*field)),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_dependencies(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    task: usize,
    hover_position: Option<(u16, u16)>,
) {
    let Some(task_name) = menu.draft.tasks.get(task).map(|task| task.name.clone()) else {
        return;
    };
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        &format!("Dependencies for {task_name}"),
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let candidates = dependency_candidates(menu, task);
    if candidates.is_empty() {
        render_text(
            buffer,
            Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
            "No other tasks are configured.",
            Style::default().fg(THEME_SNOW).bg(THEME_BLACK),
        );
        return;
    }
    let rows = area.height.saturating_sub(1);
    let start = scroll_start(menu.dependency_cursor, candidates.len(), usize::from(rows));
    for row in 0..rows {
        let Some(candidate) = candidates.get(start + usize::from(row)) else {
            break;
        };
        let candidate_task = &menu.draft.tasks[*candidate];
        let checked = menu.draft.tasks[task]
            .depends_on
            .iter()
            .any(|dependency| dependency == &candidate_task.name);
        let text = format!(
            "[{}] {}",
            if checked { "x" } else { " " },
            candidate_task.name
        );
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row + 1, area.width, 1),
            &text,
            start + usize::from(row) == menu.dependency_cursor,
            Some(MenuAction::ToggleDependency(*candidate)),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_settings(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    hover_position: Option<(u16, u16)>,
) {
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        "Settings",
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    render_menu_row(
        buffer,
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        &format!("Leader key: {}", menu.draft.settings.leader.label()),
        menu.cursor == 0,
        Some(MenuAction::OpenLeaderPicker),
        &mut menu.hits,
        hover_position,
    );
}

fn render_menu_leaders(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    hover_position: Option<(u16, u16)>,
) {
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        "Leader key",
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let leaders = all_leaders();
    let rows = area.height.saturating_sub(1);
    let start = scroll_start(menu.leader_cursor, leaders.len(), usize::from(rows));
    for row in 0..rows {
        let index = start + usize::from(row);
        let Some(&leader) = leaders.get(index) else {
            break;
        };
        let selected = leader == menu.draft.settings.leader;
        let text = format!("[{}] {}", if selected { "x" } else { " " }, leader.label());
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row + 1, area.width, 1),
            &text,
            index == menu.leader_cursor,
            Some(MenuAction::SelectLeader(leader)),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_exit(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    configure_only: bool,
    tasks_started: bool,
    hover_position: Option<(u16, u16)>,
) {
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        if menu.dirty() {
            "Exit - unsaved changes"
        } else {
            "Exit"
        },
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let actions = exit_actions(configure_only || !tasks_started);
    for (row, action) in actions.iter().enumerate() {
        if row + 1 >= usize::from(area.height) {
            break;
        }
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
            exit_action_label(*action, configure_only || !tasks_started),
            row == menu.cursor,
            Some(MenuAction::Exit(*action)),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_edit(area: Rect, buffer: &mut Buffer, edit: &MenuEdit) {
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        &format!("Editing {}", task_field_name(edit.field)),
        Style::default()
            .fg(THEME_GREEN)
            .bg(THEME_BLACK)
            .add_modifier(Modifier::BOLD),
    );
    let mut value = edit.value.clone();
    let cursor = byte_index_for_char(&value, edit.cursor);
    value.insert(cursor, '|');
    render_text(
        buffer,
        Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
        &value,
        Style::default().fg(THEME_BLACK).bg(THEME_SNOW),
    );
    render_text(
        buffer,
        Rect::new(area.x, area.y.saturating_add(4), area.width, 1),
        "Enter saves this field. Esc cancels.",
        Style::default().fg(THEME_SNOW).bg(THEME_BLACK),
    );
}

fn render_menu_row(
    buffer: &mut Buffer,
    rect: Rect,
    text: &str,
    selected: bool,
    action: Option<MenuAction>,
    hits: &mut Vec<MenuHit>,
    hover_position: Option<(u16, u16)>,
) {
    let hovered = action.is_some() && hover_position.is_some_and(|(x, y)| contains(rect, x, y));
    let style = if selected {
        Style::default().fg(THEME_BLACK).bg(THEME_SNOW)
    } else if hovered {
        Style::default().fg(THEME_BLACK).bg(THEME_GOLD)
    } else {
        Style::default().fg(THEME_SNOW).bg(THEME_BLACK)
    };
    render_text(buffer, rect, text, style);
    if let Some(action) = action {
        hits.push(MenuHit { rect, action });
    }
}

fn render_text(buffer: &mut Buffer, rect: Rect, text: &str, style: Style) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    for column in 0..rect.width {
        buffer[(rect.x + column, rect.y)]
            .set_symbol(" ")
            .set_style(style);
    }
    for (column, character) in text.chars().take(usize::from(rect.width)).enumerate() {
        let mut encoded = [0_u8; 4];
        buffer[(rect.x + column as u16, rect.y)]
            .set_symbol(character.encode_utf8(&mut encoded))
            .set_style(style);
    }
}

fn clear_rect(buffer: &mut Buffer, rect: Rect, style: Style) {
    for row in 0..rect.height {
        for column in 0..rect.width {
            buffer[(rect.x + column, rect.y + row)]
                .set_symbol(" ")
                .set_style(style);
        }
    }
}

fn inset_rect(rect: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect::new(
        rect.x.saturating_add(horizontal),
        rect.y.saturating_add(vertical),
        rect.width.saturating_sub(horizontal.saturating_mul(2)),
        rect.height.saturating_sub(vertical.saturating_mul(2)),
    )
}

fn menu_item_count(menu: &MenuState, configure_only: bool) -> usize {
    match menu.tab {
        MenuTab::Help => 0,
        MenuTab::Tasks if menu.task_detail.is_some() => task_detail_fields().len(),
        MenuTab::Tasks => menu.draft.tasks.len() + 1,
        MenuTab::Settings => 1,
        MenuTab::Exit => exit_actions(configure_only).len(),
    }
}

fn task_list_cursor(menu: &MenuState) -> usize {
    menu.task_list_cursor.min(menu.draft.tasks.len())
}

fn task_detail_fields() -> &'static [TaskField] {
    &[
        TaskField::Name,
        TaskField::Command,
        TaskField::Cwd,
        TaskField::Env,
        TaskField::Dependencies,
        TaskField::StartDelay,
        TaskField::Delete,
        TaskField::Back,
    ]
}

fn task_field_text(task: &Task, field: TaskField) -> String {
    match field {
        TaskField::Name => format!("Name: {}", task.name),
        TaskField::Command => format!("Command: {}", task.command.display()),
        TaskField::Cwd => format!("Working directory: {}", task.cwd.display()),
        TaskField::Env => {
            let value = format_env_inline(&task.env);
            format!(
                "Environment: {}",
                if value.is_empty() { "(none)" } else { &value }
            )
        }
        TaskField::Dependencies => {
            let value = task.depends_on.join(", ");
            format!(
                "Dependencies: {}",
                if value.is_empty() { "(none)" } else { &value }
            )
        }
        TaskField::StartDelay => format!(
            "Start delay: {}",
            task.start_delay.as_deref().unwrap_or("(none)")
        ),
        TaskField::Delete => "Delete task".to_owned(),
        TaskField::Back => "Back to task list".to_owned(),
    }
}

fn task_field_name(field: TaskField) -> &'static str {
    match field {
        TaskField::Name => "name",
        TaskField::Command => "command",
        TaskField::Cwd => "working directory",
        TaskField::Env => "environment",
        TaskField::Dependencies => "dependencies",
        TaskField::StartDelay => "start delay",
        TaskField::Delete => "delete",
        TaskField::Back => "back",
    }
}

fn dependency_candidates(menu: &MenuState, task: usize) -> Vec<usize> {
    (0..menu.draft.tasks.len())
        .filter(|candidate| *candidate != task)
        .collect()
}

fn scroll_start(selected: usize, count: usize, visible: usize) -> usize {
    if visible == 0 || count <= visible {
        return 0;
    }
    selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(count - visible)
}

fn exit_actions(configure_only: bool) -> &'static [MenuExitAction] {
    if configure_only {
        &[
            MenuExitAction::SaveOnly,
            MenuExitAction::Discard,
            MenuExitAction::Close,
        ]
    } else {
        &[
            MenuExitAction::SaveAffected,
            MenuExitAction::SaveAll,
            MenuExitAction::SaveOnly,
            MenuExitAction::Discard,
            MenuExitAction::Close,
        ]
    }
}

fn exit_action_label(action: MenuExitAction, configure_only: bool) -> &'static str {
    match (action, configure_only) {
        (MenuExitAction::SaveOnly, true) => "Save config and close",
        (MenuExitAction::SaveOnly, false) => "Save without restarting",
        (MenuExitAction::SaveAffected, _) => "Save and restart affected",
        (MenuExitAction::SaveAll, _) => "Save and restart all",
        (MenuExitAction::Discard, true) => "Discard and close",
        (MenuExitAction::Discard, false) => "Discard changes",
        (MenuExitAction::Close, _) => "Close menu",
    }
}

fn all_leaders() -> &'static [Leader] {
    &[
        Leader::AltJ,
        Leader::AltBacktick,
        Leader::Tab,
        Leader::CtrlB,
        Leader::CtrlQ,
        Leader::CtrlBackslash,
    ]
}

fn unique_task_name(config: &crate::config::Config, base: &str) -> String {
    if !config.tasks.iter().any(|task| task.name == base) {
        return base.to_owned();
    }
    for number in 2..1000 {
        let candidate = format!("{base}{number}");
        if !config.tasks.iter().any(|task| task.name == candidate) {
            return candidate;
        }
    }
    format!("{base}{}", config.tasks.len() + 1)
}

fn scrub_missing_dependencies(config: &mut crate::config::Config) {
    let names = config
        .tasks
        .iter()
        .map(|task| task.name.clone())
        .collect::<HashSet<_>>();
    for task in &mut config.tasks {
        task.depends_on
            .retain(|dependency| names.contains(dependency) && dependency != &task.name);
    }
}

fn format_env_inline(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_env_inline(text: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for entry in text
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((key, value)) = entry.split_once('=') else {
            anyhow::bail!("environment entries must be KEY=value");
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || key.contains(['=', '\0'])
            || key.contains(char::is_whitespace)
            || value.contains('\0')
        {
            anyhow::bail!("invalid environment entry for key {key:?}");
        }
        env.insert(key.to_owned(), value.to_owned());
    }
    Ok(env)
}

fn validate_menu_cwd(root: &Path, value: &str) -> Result<PathBuf> {
    let cwd = PathBuf::from(if value.is_empty() { "." } else { value });
    let resolved = if cwd.is_absolute() {
        cwd.clone()
    } else {
        root.join(&cwd)
    };
    if !resolved.is_dir() {
        anyhow::bail!(
            "working directory is not a directory: {}",
            resolved.display()
        );
    }
    Ok(cwd)
}

fn complete_directory(root: &Path, value: &str, cursor: usize) -> Result<DirectoryCompletion> {
    let cursor_byte = byte_index_for_char(value, cursor);
    let before = &value[..cursor_byte];
    let after = &value[cursor_byte..];
    let (parent_text, display_parent, prefix) = split_directory_completion_prefix(before);
    let parent = if parent_text.is_empty() {
        root.to_path_buf()
    } else {
        let parent = Path::new(parent_text);
        if parent.is_absolute() {
            parent.to_path_buf()
        } else {
            root.join(parent)
        }
    };

    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DirectoryCompletion::NoMatches);
        }
        Err(error) => return Err(error.into()),
    };

    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !prefix.starts_with('.') && name.starts_with('.') {
            continue;
        }
        if name.starts_with(prefix) {
            matches.push(name);
        }
    }
    matches.sort();

    match matches.len() {
        0 => Ok(DirectoryCompletion::NoMatches),
        1 => {
            let mut completed = format!("{display_parent}{}/", matches[0]);
            completed.push_str(after);
            let cursor = char_count(display_parent) + char_count(&matches[0]) + 1;
            Ok(DirectoryCompletion::Updated {
                value: completed,
                cursor,
            })
        }
        count => {
            let common = common_prefix(&matches);
            if common != prefix {
                let mut completed = format!("{display_parent}{common}");
                completed.push_str(after);
                let cursor = char_count(display_parent) + char_count(&common);
                Ok(DirectoryCompletion::Updated {
                    value: completed,
                    cursor,
                })
            } else {
                Ok(DirectoryCompletion::Ambiguous { matches: count })
            }
        }
    }
}

fn split_directory_completion_prefix(input: &str) -> (&str, &str, &str) {
    if let Some(index) = input.rfind('/') {
        let parent = if index == 0 { "/" } else { &input[..index] };
        let display_parent = &input[..=index];
        let prefix = &input[index + 1..];
        (parent, display_parent, prefix)
    } else {
        ("", "", input)
    }
}

fn common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut common = String::new();
    for (index, character) in first.chars().enumerate() {
        if values
            .iter()
            .all(|value| value.chars().nth(index) == Some(character))
        {
            common.push(character);
        } else {
            break;
        }
    }
    common
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

fn render_waiting_countdown(task: &TaskRuntime, area: Rect, now: Instant, buffer: &mut Buffer) {
    let Some(deadline) = task.pending_start else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }

    let seconds = countdown_seconds(deadline, now);
    let text = if seconds == 0 {
        "[demons] starting...".to_owned()
    } else {
        format!("[demons] starting in {seconds}s...")
    };
    let width = char_count(&text).min(usize::from(area.width)) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height / 2;
    render_text(
        buffer,
        Rect::new(x, y, width, 1),
        &text,
        Style::default().fg(THEME_GOLD),
    );
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
    if selection.history_backed
        && task.scroll_offset == 0
        && render_live_history_selection(selection, task, area, buffer)
    {
        return;
    }
    render_history_selection(selection, task, area, buffer);
}

fn render_history_selection(
    selection: &Selection,
    task: &TaskRuntime,
    area: Rect,
    buffer: &mut Buffer,
) {
    for row in 0..area.height {
        let line = task.history_index_for_visible_row(row, area.height);
        let Some((start, end)) = selection.columns_for_line(line, area.width) else {
            continue;
        };
        paint_selection_row(area, buffer, row, start, end);
    }
}

fn render_live_history_selection(
    selection: &Selection,
    task: &TaskRuntime,
    area: Rect,
    buffer: &mut Buffer,
) -> bool {
    let (start, end) = selection.ordered_points();
    if start.line != end.line || area.width == 0 {
        return false;
    }

    let Some(line) = task.history.line(start.line) else {
        return false;
    };
    let expected = slice_chars(&line.text, 0, usize::from(area.width));
    let expected = expected.trim_end();
    if expected.is_empty() {
        return false;
    }

    for row in 0..area.height {
        if screen_row_text(&task.parser, row, area.width).trim_end() == expected {
            let start_column = start.column.min(area.width);
            let end_column = end.column.saturating_add(1).min(area.width);
            if start_column < end_column {
                paint_selection_row(area, buffer, row, start_column, end_column);
                return true;
            }
        }
    }

    false
}

fn screen_row_text(parser: &Parser, row: u16, width: u16) -> String {
    let screen = parser.screen();
    let mut text = String::new();
    for column in 0..width {
        let Some(cell) = screen.cell(row, column) else {
            text.push(' ');
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let contents = cell.contents();
        if contents.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&contents);
        }
    }
    text
}

fn paint_selection_row(area: Rect, buffer: &mut Buffer, row: u16, start: u16, end: u16) {
    for column in start..end {
        buffer[(area.x + column, area.y + row)]
            .set_style(Style::default().fg(Color::Black).bg(Color::White));
    }
}

fn render_footer_items(
    items: &[FooterItem],
    area: Rect,
    buffer: &mut Buffer,
    hover_position: Option<(u16, u16)>,
) -> Vec<FooterHit> {
    for row in 0..area.height {
        for column in 0..area.width {
            buffer[(area.x + column, area.y + row)]
                .set_symbol(" ")
                .set_style(Style::default().fg(THEME_SNOW).bg(THEME_BLACK));
        }
    }

    let mut hits = Vec::new();
    let mut row = 0_u16;
    let mut column = 0_u16;
    for item in items {
        if row >= area.height {
            break;
        }
        if column > 0 && column.saturating_add(item.width) > area.width {
            row += 1;
            column = 0;
        }
        if row >= area.height {
            break;
        }

        let start_row = row;
        let start_column = column;
        let rect = Rect::new(
            area.x + start_column,
            area.y + start_row,
            item.width.min(area.width),
            1,
        );
        let hovered =
            item.action.is_some() && hover_position.is_some_and(|(x, y)| contains(rect, x, y));
        let style = item.style(hovered);
        for character in item.text.chars() {
            if column >= area.width {
                row += 1;
                column = 0;
                if row >= area.height {
                    break;
                }
            }
            let mut encoded = [0_u8; 4];
            buffer[(area.x + column, area.y + row)]
                .set_symbol(character.encode_utf8(&mut encoded))
                .set_style(style);
            column += 1;
        }
        if let Some(hit) = item.hit_rect(area, start_row, start_column, row, column) {
            hits.push(hit);
        }
    }
    hits
}

fn render_quit_confirm(area: Rect, buffer: &mut Buffer) {
    let popup = centered_rect(area, 56, 7);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    Clear.render(popup, buffer);
    Paragraph::new(vec![
        Line::styled(
            "Close Demons?",
            Style::default().fg(THEME_RED).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("Press Ctrl-C or q again to close Demons."),
        Line::raw("Press Esc to keep the panes running."),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(THEME_RED))
            .style(Style::default().fg(THEME_SNOW).bg(THEME_BLACK)),
    )
    .wrap(Wrap { trim: false })
    .render(popup, buffer);
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
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
    format!("/{query}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FooterItem {
    text: String,
    width: u16,
    action: Option<FooterAction>,
    style: FooterItemStyle,
}

impl FooterItem {
    fn new(text: String, action: Option<FooterAction>, style: FooterItemStyle) -> Self {
        Self {
            width: char_count(&text).min(usize::from(u16::MAX)) as u16,
            text,
            action,
            style,
        }
    }

    fn style(&self, hovered: bool) -> Style {
        let (fg, bg) = if hovered {
            (self.style.hover_fg, self.style.hover_bg)
        } else {
            (self.style.fg, self.style.bg)
        };
        Style::default().fg(fg).bg(bg)
    }

    fn hit_rect(
        &self,
        area: Rect,
        start_row: u16,
        start_column: u16,
        end_row: u16,
        end_column: u16,
    ) -> Option<FooterHit> {
        let action = self.action?;
        if start_row != end_row || end_column <= start_column {
            return None;
        }
        Some(FooterHit {
            rect: Rect::new(
                area.x + start_column,
                area.y + start_row,
                end_column - start_column,
                1,
            ),
            action,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FooterItemStyle {
    fg: Color,
    bg: Color,
    hover_fg: Color,
    hover_bg: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FooterHit {
    rect: Rect,
    action: FooterAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterAction {
    ToggleFullscreen,
    StartSearch,
    SearchOlder,
    SearchNewer,
    SearchNextPane,
    SearchDone,
    CopyVisible,
    CopyHistory,
    SaveHistory,
    ShowMenu,
    RestartFocused,
    RestartAll,
    ClearFocused,
    Quit,
}

fn footer_status(label: impl Into<String>) -> FooterItem {
    FooterItem::new(
        format!(" {} ", label.into()),
        None,
        FooterItemStyle {
            fg: THEME_BLACK,
            bg: THEME_SNOW,
            hover_fg: THEME_BLACK,
            hover_bg: THEME_WHITE,
        },
    )
}

fn command_footer_items() -> Vec<FooterItem> {
    [
        ("f fullscreen", FooterAction::ToggleFullscreen),
        ("/ search", FooterAction::StartSearch),
        ("y copy", FooterAction::CopyVisible),
        ("Y copy all", FooterAction::CopyHistory),
        ("S save", FooterAction::SaveHistory),
        ("r restart", FooterAction::RestartFocused),
        ("R restart all", FooterAction::RestartAll),
        ("c clear", FooterAction::ClearFocused),
        ("q quit", FooterAction::Quit),
        ("? menu", FooterAction::ShowMenu),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (label, action))| footer_command_button(label, action, index))
    .collect()
}

fn search_footer_items(search: &SearchState) -> Vec<FooterItem> {
    let mut items = search_placeholder_footer_items();
    items[0] = footer_status(search_footer_text(search));
    if let Some(status) = search_result_status(search) {
        items.push(footer_status(status));
    }
    items
}

fn search_placeholder_footer_items() -> Vec<FooterItem> {
    vec![
        footer_status("/"),
        footer_command_button("Enter previous", FooterAction::SearchOlder, 1),
        footer_command_button("Shift+Enter next", FooterAction::SearchNewer, 2),
        footer_command_button("Tab pane", FooterAction::SearchNextPane, 3),
        footer_command_button("Esc done", FooterAction::SearchDone, 4),
    ]
}

fn search_result_status(search: &SearchState) -> Option<String> {
    if let (Some(index), count) = (search.current_index, search.match_count) {
        if count > 0 {
            let noun = if count == 1 { "match" } else { "matches" };
            return Some(format!("{}/{} {noun}", index + 1, count));
        }
    }
    search.message.clone()
}

fn footer_command_button(
    label: impl Into<String>,
    action: FooterAction,
    index: usize,
) -> FooterItem {
    let style = christmas_style_for_index(index);
    FooterItem::new(format!(" {} ", label.into()), Some(action), style)
}

fn christmas_style_for_index(index: usize) -> FooterItemStyle {
    if index % 2 == 0 {
        footer_style(THEME_BLACK, THEME_SNOW, THEME_BLACK, THEME_WHITE)
    } else {
        footer_style(THEME_BLACK, THEME_RED, THEME_BLACK, Color::LightRed)
    }
}

fn footer_style(fg: Color, bg: Color, hover_fg: Color, hover_bg: Color) -> FooterItemStyle {
    FooterItemStyle {
        fg,
        bg,
        hover_fg,
        hover_bg,
    }
}

fn mode_hover_color(color: Color) -> Color {
    match color {
        THEME_GREEN => Color::LightGreen,
        THEME_RED => Color::LightRed,
        THEME_GOLD => Color::LightYellow,
        _ => THEME_WHITE,
    }
}

fn footer_line_count(items: &[FooterItem], width: u16) -> u16 {
    let mut lines = 1_u16;
    let mut column = 0_u16;
    for item in items {
        if column > 0 && column.saturating_add(item.width) > width {
            lines = lines.saturating_add(1);
            column = 0;
        }
        if item.width > width {
            let extra = item.width.saturating_sub(1) / width.max(1);
            lines = lines.saturating_add(extra);
            column = item.width % width.max(1);
        } else {
            column = column.saturating_add(item.width);
        }
    }
    lines.max(1)
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

fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q'))
        || (matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_leader(key: KeyEvent, leader: Leader) -> bool {
    match leader {
        Leader::AltJ => {
            matches!(key.code, KeyCode::Char('j' | 'J'))
                && (key.modifiers == KeyModifiers::ALT
                    || key.modifiers == KeyModifiers::ALT | KeyModifiers::SHIFT)
        }
        Leader::AltBacktick => key.code == KeyCode::Char('`') && key.modifiers == KeyModifiers::ALT,
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

fn is_legacy_alt_leader(key: KeyEvent, leader: Leader) -> bool {
    match leader {
        Leader::AltJ => {
            matches!(key.code, KeyCode::Char('j' | 'J'))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        }
        Leader::AltBacktick => key.code == KeyCode::Char('`') && key.modifiers.is_empty(),
        _ => false,
    }
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

    use crate::config::{CONFIG_FILE, Config, Settings};
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
        assert!(is_leader(
            key(KeyCode::Char('`'), KeyModifiers::ALT),
            Leader::AltBacktick
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
        app.mode = AppMode::Input;

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Command);
        assert!(app.pending_escape.is_none());
    }

    #[test]
    fn recognizes_legacy_escape_encoded_alt_backtick() {
        let mut app = test_app();
        app.mode = AppMode::Input;
        app.loaded.config.settings.leader = Leader::AltBacktick;

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('`'), KeyModifiers::NONE))
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
    fn starts_in_command_mode() {
        let app = test_app();

        assert_eq!(app.mode, AppMode::Command);
    }

    #[test]
    fn esc_does_not_leave_command_mode() {
        let mut app = test_app();
        app.loaded.config.settings.leader = Leader::CtrlB;

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Command);
    }

    #[test]
    fn command_mode_question_mark_opens_menu_help_tab() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;

        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.menu.as_ref().unwrap().tab, MenuTab::Help);

        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.menu.is_some());
        assert!(!app.tasks[0].restart_requested);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.menu.is_none());
        assert_eq!(app.mode, AppMode::Command);
    }

    #[test]
    fn mouse_movement_does_not_close_menu() {
        let mut app = test_app();
        app.open_menu(MenuTab::Help);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert!(app.menu.is_some());
        assert_eq!(app.mouse_position, Some((12, 4)));
    }

    #[test]
    fn mouse_movement_does_not_leave_search_mode() {
        let mut app = test_app();
        app.mode = AppMode::Command;
        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.mode, AppMode::Search);
        assert!(app.search.is_some());
        assert_eq!(app.mouse_position, Some((12, 4)));
    }

    #[test]
    fn footer_height_wraps_long_command_buttons() {
        let mut app = test_app();
        app.mode = AppMode::Command;

        assert_eq!(app.footer_height(240, 24), 1);
        assert!(app.footer_height(45, 24) > 1);
    }

    #[test]
    fn footer_items_keep_buttons_together_when_wrapping() {
        let items = vec![
            footer_command_button("a one", FooterAction::ClearFocused, 0),
            footer_command_button("? menu", FooterAction::ShowMenu, 1),
            footer_command_button("q quit", FooterAction::Quit, 2),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 3));

        let hits = render_footer_items(&items, Rect::new(0, 0, 14, 3), &mut buffer, None);

        assert_eq!(footer_line_count(&items, 14), 3);
        assert_eq!(hits[0].rect.y, 0);
        assert_eq!(hits[1].rect.y, 1);
        assert_eq!(hits[2].rect.y, 2);
    }

    #[test]
    fn command_footer_splits_paired_actions_into_buttons() {
        let items = command_footer_items();

        assert!(
            items
                .iter()
                .any(|item| item.text == " y copy "
                    && item.action == Some(FooterAction::CopyVisible))
        );
        assert!(
            items.iter().any(|item| item.text == " Y copy all "
                && item.action == Some(FooterAction::CopyHistory))
        );
        assert!(
            items.iter().any(|item| item.text == " r restart "
                && item.action == Some(FooterAction::RestartFocused))
        );
        assert!(
            items.iter().any(|item| item.text == " R restart all "
                && item.action == Some(FooterAction::RestartAll))
        );
        assert_eq!(items.last().unwrap().action, Some(FooterAction::ShowMenu));
    }

    #[test]
    fn command_footer_uses_candy_cane_button_contrast() {
        let items = command_footer_items();

        for item in &items {
            assert_eq!(item.style.fg, THEME_BLACK);
        }
        assert_eq!(items[0].style.bg, THEME_SNOW);
        assert_eq!(items[1].style.bg, THEME_RED);
        assert_eq!(items[2].style.bg, THEME_SNOW);
        assert_eq!(items[3].style.bg, THEME_RED);
    }

    #[test]
    fn command_mode_uses_gold_mode_color() {
        let mut app = test_app();
        app.mode = AppMode::Command;

        assert_eq!(app.mode_label_color(), ("COMMAND MODE", THEME_COMMAND));
    }

    #[test]
    fn centered_rect_clamps_to_small_areas() {
        assert_eq!(
            centered_rect(Rect::new(0, 0, 10, 5), 74, 16),
            Rect::new(1, 1, 8, 3)
        );
        assert_eq!(
            centered_rect(Rect::new(0, 0, 0, 0), 74, 16),
            Rect::new(0, 0, 0, 0)
        );
    }

    #[test]
    fn fullscreen_layout_only_resizes_focused_pane() {
        let mut app = test_app();
        app.mode = AppMode::Input;
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
    fn text_history_finds_matching_lines_case_insensitively() {
        let mut history = TextHistory::new(80, 100);
        history.process(b"alpha\nerror one\nERROR two\n");

        assert_eq!(history.matching_lines("error"), vec![1, 2]);
        assert!(history.matching_lines("missing").is_empty());
    }

    #[test]
    fn search_footer_marks_the_edit_cursor() {
        let search = SearchState {
            pane: 0,
            query: "eror".to_owned(),
            cursor: 2,
            current: None,
            current_index: None,
            match_count: 0,
            message: None,
        };

        assert_eq!(search_footer_text(&search), "/er|or");
        let items = search_footer_items(&search);
        assert!(
            items.iter().any(|item| item.text == " Tab pane "
                && item.action == Some(FooterAction::SearchNextPane))
        );
        assert_eq!(items[1].text, " Enter previous ");
        assert_eq!(items[2].text, " Shift+Enter next ");
        assert_eq!(items[1].style.bg, THEME_RED);
        assert_eq!(items[2].style.bg, THEME_SNOW);
        assert_eq!(items[3].style.bg, THEME_RED);
        assert_eq!(items[4].style.bg, THEME_SNOW);
    }

    #[test]
    fn footer_copy_buttons_click_visible_and_full_history_actions() {
        let mut app = test_app();
        app.mode = AppMode::Input;
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        for line in 0..20 {
            app.tasks[0].process_output(format!("line {line}\n").as_bytes());
        }
        app.tasks[0].scroll_up(10);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 2));
        let items = command_footer_items();
        app.footer_hits = render_footer_items(&items, Rect::new(0, 0, 120, 2), &mut buffer, None);
        let copy_visible = app
            .footer_hits
            .iter()
            .find(|hit| hit.action == FooterAction::CopyVisible)
            .unwrap()
            .rect;
        let copy_history = app
            .footer_hits
            .iter()
            .find(|hit| hit.action == FooterAction::CopyHistory)
            .unwrap()
            .rect;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy_visible.x,
            row: copy_visible.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        assert!(app.clipboard.contains("line 6"));
        assert!(!app.clipboard.contains("line 19"));

        app.clipboard.clear();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy_history.x,
            row: copy_history.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        assert_eq!(app.clipboard.lines().next(), Some("line 0"));
        assert!(app.clipboard.contains("line 19"));
    }

    #[test]
    fn footer_restart_all_button_clicks_and_right_click_is_ignored() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 120, 20));
        app.mode = AppMode::Command;
        for (index, task) in app.tasks.iter_mut().enumerate() {
            task.pid = Some(1000 + index as u32);
        }

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 2));
        let items = command_footer_items();
        app.footer_hits = render_footer_items(&items, Rect::new(0, 0, 120, 2), &mut buffer, None);
        let restart = app
            .footer_hits
            .iter()
            .find(|hit| hit.action == FooterAction::RestartAll)
            .unwrap()
            .rect;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: restart.x,
            row: restart.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert!(app.tasks.iter().all(|task| task.restart_requested));

        for task in &mut app.tasks {
            task.restart_requested = false;
        }
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: restart.x,
            row: restart.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert!(app.tasks.iter().all(|task| !task.restart_requested));
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
            history_backed: false,
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
        app.mode = AppMode::Input;
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
    fn command_mode_quit_requires_confirmation() {
        let mut app = test_app();
        app.mode = AppMode::Command;

        assert_eq!(
            app.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
                .unwrap(),
            Action::Continue
        );
        assert!(app.confirm_quit);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(!app.confirm_quit);

        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.confirm_quit);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .unwrap(),
            Action::Quit
        );
    }

    #[test]
    fn quit_confirmation_ignores_mouse_events_behind_it() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.confirm_quit = true;
        let second = app.content_rects[1];

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second.x,
            row: second.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.focus, 0);
        assert!(app.confirm_quit);
    }

    #[test]
    fn input_mode_quit_keys_confirm_only_when_focused_pane_cannot_accept_input() {
        let mut app = test_app();
        app.mode = AppMode::Input;

        app.tasks[0].writer = Some(Box::new(io::sink()));
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.confirm_quit);

        app.tasks[0].writer = None;
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.confirm_quit);
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

        assert_eq!(app.mode, AppMode::Search);
        assert_eq!(app.search.as_ref().unwrap().current, Some(5));
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
    fn search_accepts_n_in_query_text() {
        let mut app = test_app();
        app.mode = AppMode::Command;

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "node".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }

        let search = app.search.as_ref().unwrap();
        assert_eq!(search.query, "node");
        assert_eq!(search.cursor, 4);
        assert!(search.current.is_none());
    }

    #[test]
    fn search_updates_matches_while_typing_and_reports_position() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"alpha\nbeta one\nbeta two\n");

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "beta".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }

        let search = app.search.as_ref().unwrap();
        assert_eq!(search.current_index, Some(1));
        assert_eq!(search.match_count, 2);
        assert_eq!(search_result_status(search).as_deref(), Some("2/2 matches"));
        assert_eq!(app.selected_text().unwrap(), "beta two");

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let search = app.search.as_ref().unwrap();
        assert_eq!(search.current_index, Some(0));
        assert_eq!(search_result_status(search).as_deref(), Some("1/2 matches"));
        assert_eq!(app.selected_text().unwrap(), "beta one");
    }

    #[test]
    fn search_clicking_pane_retargets_active_search() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"http://wrong.example\n");
        app.tasks[1].process_output(b"http://right.example\n");

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "http".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }

        let second = app.content_rects[1];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: second.x,
            row: second.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.focus, 1);
        assert_eq!(app.search.as_ref().unwrap().pane, 1);
        assert_eq!(app.search.as_ref().unwrap().current_index, Some(0));
        assert_eq!(app.search.as_ref().unwrap().match_count, 1);
        assert_eq!(app.selection.as_ref().unwrap().pane, 1);
        assert_eq!(app.selected_text().unwrap(), "http://right.example");
    }

    #[test]
    fn search_tab_retargets_active_search() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"http://first.example\n");
        app.tasks[1].process_output(b"http://second.example\n");

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "http".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.selected_text().unwrap(), "http://first.example");

        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, 1);
        assert_eq!(app.search.as_ref().unwrap().pane, 1);
        assert_eq!(app.search.as_ref().unwrap().query, "http");
        assert_eq!(app.search.as_ref().unwrap().current_index, Some(0));
        assert_eq!(app.search.as_ref().unwrap().match_count, 1);
        assert_eq!(app.selected_text().unwrap(), "http://second.example");

        app.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.focus, 0);
        assert_eq!(app.search.as_ref().unwrap().pane, 0);
        assert_eq!(app.selected_text().unwrap(), "http://first.example");
    }

    #[test]
    fn search_selection_copies_history_text_without_forcing_scrollback() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 8));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"http://localhost\x1b[1GXY");

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "http".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.tasks[0].scroll_offset, 0);
        assert_eq!(app.selected_text().unwrap(), "http://localhostXY");
    }

    #[test]
    fn search_selection_tracks_live_screen_after_cursor_movement() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.focus = 1;
        app.tasks[1].process_output(
            format!(
                "{}\x1b[30A  Network: http://localhost:5173/\n",
                "\n".repeat(30)
            )
            .as_bytes(),
        );

        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for character in "Network".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let area = app.content_rects[1];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
        render_screen(&app.tasks[1].parser, area, &mut buffer);
        render_selection(app.selection.as_ref(), &app.tasks[1], area, &mut buffer);

        let screen_row = (0..area.height)
            .find(|row| screen_row_text(&app.tasks[1].parser, *row, area.width).contains("Network"))
            .unwrap();
        let selection = app.selection.as_ref().unwrap();
        let history_row = selection
            .anchor
            .line
            .saturating_sub(
                app.tasks[1]
                    .history
                    .visible_start(area.height, app.tasks[1].scroll_offset),
            )
            .min(u64::from(area.height.saturating_sub(1))) as u16;

        assert_ne!(screen_row, history_row);
        assert_eq!(buffer[(area.x, area.y + screen_row)].bg, Color::White);
        assert_ne!(buffer[(area.x, area.y + history_row)].bg, Color::White);
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

        assert_eq!(app.mode, AppMode::Search);
        assert!(app.selection.is_none());
        assert_eq!(
            app.search.as_ref().unwrap().message.as_deref(),
            Some("0 matches")
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
        assert_eq!(app.selected_text().unwrap(), "error 25");

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 15");

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 5");

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "error 25");

        app.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT))
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
            history_backed: false,
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
        app.mode = AppMode::Input;
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.tasks[0].parser.process(b"\x1b[?1000h");
        app.selection = Some(Selection {
            pane: 0,
            anchor: SelectionPoint { line: 0, column: 0 },
            cursor: SelectionPoint { line: 0, column: 3 },
            history_backed: false,
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

    #[test]
    fn dependency_graph_links_tasks_by_name() {
        let mut server = test_task("server");
        let mut web = test_task("web");
        web.depends_on = vec!["server".to_owned()];
        web.start_delay = Some("3s".to_owned());
        let app = test_app_with_tasks(vec![server.clone(), web.clone()]);

        assert_eq!(app.dependency_indexes[1], vec![0]);
        assert_eq!(app.dependent_indexes[0], vec![1]);
        assert_eq!(app.restart_closure(0), vec![0, 1]);
        assert_eq!(app.restart_closure(1), vec![1]);

        server.depends_on = vec!["web".to_owned()];
        let (dependencies, dependents) = dependency_graph(&[server, web]);
        assert_eq!(dependencies[0], vec![1]);
        assert_eq!(dependents[1], vec![0]);
    }

    #[test]
    fn restart_order_starts_dependencies_before_dependents() {
        let mut web = test_task("web");
        web.depends_on = vec!["server".to_owned()];
        let server = test_task("server");
        let app = test_app_with_tasks(vec![web, server]);

        assert_eq!(app.restart_order(&[0, 1]), vec![1, 0]);
        assert_eq!(app.restart_order(&[0]), vec![0]);
        assert_eq!(app.restart_closure(1), vec![1, 0]);
    }

    #[test]
    fn menu_scroll_start_keeps_cursor_visible() {
        assert_eq!(scroll_start(0, 10, 4), 0);
        assert_eq!(scroll_start(3, 10, 4), 0);
        assert_eq!(scroll_start(4, 10, 4), 1);
        assert_eq!(scroll_start(9, 10, 4), 6);
        assert_eq!(scroll_start(9, 3, 4), 0);
    }

    #[test]
    fn dependent_start_waits_for_dependencies_and_delay() {
        let server = test_task("server");
        let mut web = test_task("web");
        web.depends_on = vec!["server".to_owned()];
        web.start_delay = Some("3s".to_owned());
        let mut app = test_app_with_tasks(vec![server, web]);
        let now = Instant::now();

        app.tasks[1].start_requested = true;
        assert!(app.tick_dependency_starts(now));
        assert_eq!(app.tasks[1].status, TaskStatus::Waiting);
        assert!(app.tasks[1].pending_start.is_none());

        app.tasks[0].pid = Some(1234);
        app.tasks[0].status = TaskStatus::Running;
        assert!(app.tick_dependency_starts(now));
        assert_eq!(app.tasks[1].status, TaskStatus::Waiting);
        assert_eq!(
            app.tasks[1].pending_start,
            Some(now + Duration::from_secs(3))
        );
        assert!(app.tasks[1].pid.is_none());
    }

    #[test]
    fn waiting_status_label_uses_icon() {
        let now = Instant::now();
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.status = TaskStatus::Waiting;
        task.pending_start = Some(now + Duration::from_millis(3200));

        assert_eq!(task.status_label().0, "⏱");
    }

    #[test]
    fn waiting_countdown_renders_in_pane_body() {
        let now = Instant::now();
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.status = TaskStatus::Waiting;
        task.pending_start = Some(now + Duration::from_millis(3200));
        let area = Rect::new(0, 0, 40, 5);
        let mut buffer = Buffer::empty(area);

        render_waiting_countdown(&task, area, now, &mut buffer);

        assert!(buffer_line(&buffer, 2, 40).contains("[demons] starting in 4s..."));
    }

    #[test]
    fn waiting_countdown_snapshot_tracks_visible_seconds() {
        let now = Instant::now();
        let mut app = test_app();

        assert_eq!(app.waiting_countdown_snapshot(now), vec![None, None]);

        app.tasks[1].pending_start = Some(now + Duration::from_millis(2100));

        assert_eq!(app.waiting_countdown_snapshot(now), vec![None, Some(3)]);
        assert_eq!(
            app.waiting_countdown_snapshot(now + Duration::from_millis(1200)),
            vec![None, Some(1)]
        );
    }

    #[test]
    fn menu_dependency_picker_toggles_dependencies() {
        let mut app = test_app_with_tasks(vec![
            test_task("server"),
            test_task("web"),
            test_task("worker"),
        ]);
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(1)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Dependencies))
            .unwrap();

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.menu.as_ref().unwrap().draft.tasks[1].depends_on,
            vec!["server"]
        );

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.menu.as_ref().unwrap().draft.tasks[1].depends_on,
            vec!["server", "worker"]
        );
    }

    #[test]
    fn task_detail_back_restores_task_list_cursor() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);
        app.move_menu_cursor(1);

        app.apply_menu_action(MenuAction::OpenTask(1)).unwrap();
        assert_eq!(app.menu.as_ref().unwrap().cursor, 0);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert!(menu.task_detail.is_none());
        assert_eq!(menu.cursor, 1);
    }

    #[test]
    fn deleted_task_selects_neighboring_task() {
        let mut app =
            test_app_with_tasks(vec![test_task("one"), test_task("two"), test_task("three")]);
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(1)).unwrap();

        app.apply_menu_action(MenuAction::TaskField(TaskField::Delete))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert!(menu.task_detail.is_none());
        assert_eq!(menu.cursor, 1);
        assert_eq!(menu.draft.tasks[1].name, "three");

        app.apply_menu_action(MenuAction::OpenTask(1)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Delete))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.cursor, 0);
        assert_eq!(menu.draft.tasks[0].name, "one");
    }

    #[test]
    fn cwd_edit_validates_directory_before_apply() {
        let temp = tempdir().unwrap();
        std::fs::create_dir(temp.path().join("web")).unwrap();
        let mut app = test_app();
        app.loaded.root = temp.path().to_path_buf();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Cwd))
            .unwrap();

        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "missing".to_owned();
            edit.cursor = char_count(&edit.value);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.menu.as_ref().unwrap().edit.is_some());
        assert_eq!(
            app.menu.as_ref().unwrap().draft.tasks[0].cwd,
            PathBuf::from(".")
        );

        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "web".to_owned();
            edit.cursor = char_count(&edit.value);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert!(menu.edit.is_none());
        assert_eq!(menu.draft.tasks[0].cwd, PathBuf::from("web"));
    }

    #[test]
    fn cwd_edit_tab_completes_directories() {
        let temp = tempdir().unwrap();
        std::fs::create_dir(temp.path().join("frontend_client")).unwrap();
        let mut app = test_app();
        app.loaded.root = temp.path().to_path_buf();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Cwd))
            .unwrap();
        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "front".to_owned();
            edit.cursor = char_count(&edit.value);
        }

        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();

        let edit = app.menu.as_ref().unwrap().edit.as_ref().unwrap();
        assert_eq!(edit.value, "frontend_client/");
        assert_eq!(edit.cursor, char_count("frontend_client/"));
    }

    #[test]
    fn menu_add_task_and_save_writes_config_in_configure_mode() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let loaded = LoadedConfig {
            path: path.clone(),
            root: temp.path().to_path_buf(),
            config: Config::default(),
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut app = App::new(loaded, tx, rx, Arc::new(Mutex::new(HashSet::new())), true);

        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::AddTask).unwrap();
        assert_eq!(
            app.handle_menu_exit_action(MenuExitAction::SaveOnly)
                .unwrap(),
            Action::Quit
        );

        let saved = std::fs::read_to_string(path).unwrap();
        assert!(saved.contains("[[task]]"));
        assert!(saved.contains("command = \"echo ready\""));
    }

    #[test]
    fn menu_discard_reverts_live_leader_change() {
        let mut app = test_app();
        app.open_menu(MenuTab::Settings);

        app.apply_menu_action(MenuAction::OpenLeaderPicker).unwrap();
        app.apply_menu_action(MenuAction::SelectLeader(Leader::AltBacktick))
            .unwrap();
        assert_eq!(app.loaded.config.settings.leader, Leader::AltBacktick);
        assert!(!app.menu.as_ref().unwrap().leader_picker);

        app.handle_menu_exit_action(MenuExitAction::Discard)
            .unwrap();
        assert_eq!(app.loaded.config.settings.leader, Leader::AltJ);
        assert!(app.menu.is_none());
    }

    #[test]
    fn settings_leader_picker_selects_with_keyboard() {
        let mut app = test_app();
        app.open_menu(MenuTab::Settings);

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.menu.as_ref().unwrap().leader_picker);

        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.draft.settings.leader, Leader::AltBacktick);
        assert_eq!(app.loaded.config.settings.leader, Leader::AltBacktick);
        assert!(!menu.leader_picker);
    }

    #[test]
    fn menu_tab_click_cancels_active_text_edit() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Name))
            .unwrap();
        assert!(app.menu.as_ref().unwrap().edit.is_some());

        app.apply_menu_action(MenuAction::Tab(MenuTab::Settings))
            .unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.tab, MenuTab::Settings);
        assert!(menu.edit.is_none());
        assert!(menu.task_detail.is_none());
    }

    #[test]
    fn menu_text_edit_supports_cursor_movement() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Name))
            .unwrap();

        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('X'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.menu.as_ref().unwrap().draft.tasks[0].name, "onXe");
    }

    #[test]
    fn ctrl_c_from_menu_text_edit_opens_quit_confirmation() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Name))
            .unwrap();

        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();

        assert!(app.confirm_quit);
    }

    #[test]
    fn mouse_wheel_moves_menu_cursor() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();

        assert_eq!(app.menu.as_ref().unwrap().cursor, 1);
    }

    #[test]
    fn menu_save_affected_updates_task_and_requests_restart() {
        let temp = tempdir().unwrap();
        let mut app = test_app();
        app.loaded.path = temp.path().join(CONFIG_FILE);
        app.loaded.root = temp.path().to_path_buf();
        app.tasks_started = true;
        app.tasks[0].pid = Some(4242);
        app.tasks[0].status = TaskStatus::Running;
        app.open_menu(MenuTab::Tasks);
        {
            let menu = app.menu.as_mut().unwrap();
            menu.draft.tasks[0].command = TaskCommand::Shell("echo changed".to_owned());
        }

        app.handle_menu_exit_action(MenuExitAction::SaveAffected)
            .unwrap();

        assert_eq!(app.tasks[0].task.command.display(), "echo changed");
        assert!(app.tasks[0].restart_requested);
    }

    #[test]
    fn menu_save_affected_schedules_dependents() {
        let temp = tempdir().unwrap();
        let server = test_task("server");
        let mut web = test_task("web");
        web.depends_on = vec!["server".to_owned()];
        let mut app = test_app_with_tasks(vec![server, web]);
        app.loaded.path = temp.path().join(CONFIG_FILE);
        app.loaded.root = temp.path().to_path_buf();
        app.tasks_started = true;
        app.open_menu(MenuTab::Tasks);
        {
            let menu = app.menu.as_mut().unwrap();
            menu.draft.tasks[0].command = TaskCommand::Shell("echo changed".to_owned());
        }

        app.handle_menu_exit_action(MenuExitAction::SaveAffected)
            .unwrap();

        assert!(app.tasks[0].start_requested || app.tasks[0].pid.is_some());
        assert!(app.tasks[1].start_requested || app.tasks[1].pid.is_some());
    }

    fn test_task(name: &str) -> Task {
        Task {
            name: name.to_owned(),
            command: TaskCommand::Shell("echo ready".to_owned()),
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            depends_on: Vec::new(),
            start_delay: None,
            watch: None,
            run_on_change: None,
            repeat: None,
        }
    }

    fn test_app() -> App {
        test_app_with_tasks(vec![test_task("one"), test_task("two")])
    }

    fn test_app_with_tasks(tasks: Vec<Task>) -> App {
        let loaded = LoadedConfig {
            path: PathBuf::from("/tmp/demons.toml"),
            root: PathBuf::from("."),
            config: Config {
                settings: Settings::default(),
                tasks,
            },
        };
        let (tx, rx) = mpsc::sync_channel(8);
        App::new(loaded, tx, rx, Arc::new(Mutex::new(HashSet::new())), false)
    }

    fn buffer_line(buffer: &Buffer, row: u16, width: u16) -> String {
        let mut line = String::new();
        for column in 0..width {
            line.push_str(buffer[(column, row)].symbol());
        }
        line
    }
}
