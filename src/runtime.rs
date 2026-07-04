#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
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
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap},
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

use crate::{
    config::{
        ConfigProblem, ConfigProblemLocation, ConfigProblemSeverity, ConfigSettingField,
        ConfigTaskField, Leader, LoadedConfig, MAX_MULTI_CLICK_MS, MIN_MULTI_CLICK_MS,
        MULTI_CLICK_STEP_MS, Task, TaskCommand, config_blocking_problems, parse_start_delay,
    },
    layout::{Grid, choose_grid, grid_rects, pane_rects},
};

const SCROLLBACK_LINES: usize = 10_000;
const RESTART_GRACE: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const EVENT_INTERVAL: Duration = Duration::from_millis(25);
const ALT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);
const MODE_BUTTON_WIDTH: u16 = 13;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(45);
const SCENE_FRAME_INTERVAL: Duration = Duration::from_millis(350);
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const MAX_FULL_HISTORY_OSC52_BYTES: usize = 512 * 1024;
const DEV_SCENE_ENV: &str = "DEMONS_DEV_SCENE";
const DEV_SCENE_SEED_ENV: &str = "DEMONS_DEV_SCENE_SEED";
const DEV_SCENE_FRAME_ENV: &str = "DEMONS_DEV_SCENE_FRAME";
const THEME_RED: Color = Color::Rgb(132, 22, 36);
const THEME_RED_HOVER: Color = Color::Rgb(168, 44, 55);
const THEME_GREEN: Color = Color::Rgb(44, 107, 78);
const THEME_GREEN_HOVER: Color = Color::Rgb(61, 132, 96);
const THEME_SNOW: Color = Color::Rgb(229, 224, 204);
const THEME_GOLD: Color = Color::Rgb(188, 146, 54);
const THEME_GOLD_HOVER: Color = Color::Rgb(224, 185, 82);
const THEME_SKIN: Color = Color::Rgb(226, 181, 135);
const THEME_COMMAND: Color = THEME_GOLD;
const THEME_HOLLY: Color = Color::Rgb(91, 111, 100);
const THEME_BLACK: Color = Color::Rgb(11, 20, 17);
const THEME_WHITE: Color = Color::Rgb(246, 241, 220);
const THEME_BACKGROUND: Color = Color::Rgb(8, 17, 15);
const THEME_PANEL: Color = Color::Rgb(22, 33, 29);
const THEME_MENU: Color = Color::Rgb(34, 48, 43);
const THEME_FOOTER: Color = Color::Rgb(36, 45, 42);
const THEME_FLAME: Color = Color::Rgb(221, 92, 38);
const THEME_EMBER: Color = Color::Rgb(249, 177, 72);
const THEME_LOG: Color = Color::Rgb(112, 68, 39);
const THEME_LOG_DARK: Color = Color::Rgb(68, 39, 24);
const THEME_ICE: Color = Color::Rgb(104, 164, 166);
const THEME_ICE_DARK: Color = Color::Rgb(64, 115, 123);
const THEME_ACCENT_MARK: &str = "❄";
const SKATING_SNOWBANK_PATTERN: &[u8] =
    b"#__#####______###____########______##_____########______###____###___##____#####";

type ProcessRegistry = Arc<Mutex<HashSet<u32>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SceneKind {
    Fireplace,
    Snow,
    Tree,
    Santa,
    Jack,
    Skating,
    Sleigh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SceneState {
    kind: SceneKind,
    seed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FireGeometry {
    log_x: u16,
    log_y: u16,
    log_width: u16,
    flame_height: u16,
}

fn app_style() -> Style {
    Style::default().fg(THEME_WHITE).bg(THEME_BACKGROUND)
}

fn pane_style() -> Style {
    Style::default().fg(THEME_WHITE).bg(THEME_PANEL)
}

fn menu_style() -> Style {
    Style::default().fg(THEME_SNOW).bg(THEME_MENU)
}

fn menu_heading_style() -> Style {
    menu_style()
        .fg(THEME_GOLD_HOVER)
        .add_modifier(Modifier::BOLD)
}

fn footer_base_style() -> Style {
    Style::default().fg(THEME_SNOW).bg(THEME_FOOTER)
}

fn app_scene_seed(loaded: &LoadedConfig) -> u64 {
    if let Some(seed) = dev_scene_seed_override() {
        return seed;
    }
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    mix_scene_seed(
        clock,
        hash_text(&loaded.path.display().to_string()),
        0x00a1_1ce5_u64,
    )
}

fn hash_text(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn mix_scene_seed(seed: u64, value: u64, salt: u64) -> u64 {
    let mut mixed = seed ^ value.rotate_left(17) ^ salt.rotate_right(7);
    mixed = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

fn scene_state_for_area(
    seed: u64,
    area: Rect,
    override_kind: Option<SceneKind>,
) -> Option<SceneState> {
    if let Some(kind) = override_kind.filter(|kind| scene_fits(*kind, area)) {
        return Some(SceneState { kind, seed });
    }
    let kinds = fitting_scene_kinds(area);
    if kinds.is_empty() {
        return None;
    }
    let kind = *kinds.get((seed % kinds.len() as u64) as usize)?;
    Some(SceneState { kind, seed })
}

fn fitting_scene_kinds(area: Rect) -> Vec<SceneKind> {
    [
        SceneKind::Fireplace,
        SceneKind::Snow,
        SceneKind::Tree,
        SceneKind::Santa,
        SceneKind::Jack,
        SceneKind::Skating,
        SceneKind::Sleigh,
    ]
    .into_iter()
    .filter(|kind| scene_fits(*kind, area))
    .collect()
}

fn scene_fits(kind: SceneKind, area: Rect) -> bool {
    let (width, height) = scene_min_size(kind);
    area.width >= width && area.height >= height
}

fn scene_min_size(kind: SceneKind) -> (u16, u16) {
    match kind {
        SceneKind::Fireplace => (18, 4),
        SceneKind::Snow => (18, 7),
        SceneKind::Tree => (18, 7),
        SceneKind::Santa => (34, 14),
        SceneKind::Jack => (20, 7),
        SceneKind::Skating => (28, 8),
        SceneKind::Sleigh => (34, 8),
    }
}

fn dev_scene_kind_override() -> Option<SceneKind> {
    env::var(DEV_SCENE_ENV)
        .ok()
        .and_then(|value| parse_dev_scene_kind(&value))
}

fn dev_scene_seed_override() -> Option<u64> {
    env::var(DEV_SCENE_SEED_ENV)
        .ok()
        .and_then(|value| parse_dev_u64(&value))
}

fn dev_scene_frame_override() -> Option<u64> {
    env::var(DEV_SCENE_FRAME_ENV)
        .ok()
        .and_then(|value| parse_dev_u64(&value))
}

fn parse_dev_scene_kind(value: &str) -> Option<SceneKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fire" | "fireplace" => Some(SceneKind::Fireplace),
        "snow" | "snowman" => Some(SceneKind::Snow),
        "tree" => Some(SceneKind::Tree),
        "santa" | "rooftop" => Some(SceneKind::Santa),
        "jack" | "jack-in-the-box" | "jack_in_the_box" => Some(SceneKind::Jack),
        "skate" | "skating" | "lake" | "pond" => Some(SceneKind::Skating),
        "sleigh" | "reindeer" | "rudolph" => Some(SceneKind::Sleigh),
        _ => None,
    }
}

fn parse_dev_u64(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        value.parse().ok()
    }
}

pub fn run(loaded: LoadedConfig) -> Result<()> {
    run_with_options(
        loaded,
        RunOptions {
            start_tasks: true,
            open_menu: false,
            quit_when_menu_closes: false,
            start_after_config_save: false,
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
            start_after_config_save: false,
        },
    )
}

pub fn recover_then_run(loaded: LoadedConfig) -> Result<()> {
    run_with_options(
        loaded,
        RunOptions {
            start_tasks: false,
            open_menu: true,
            quit_when_menu_closes: false,
            start_after_config_save: true,
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
    terminal.clear().ok();

    let (tx, rx) = mpsc::sync_channel(1024);
    let mut app = App::new(
        loaded,
        tx,
        rx,
        registry,
        options.quit_when_menu_closes,
        options.start_after_config_save,
    );
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
    start_after_config_save: bool,
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
    slot_rects: Vec<Rect>,
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
    last_click: Option<ClickState>,
    clipboard: String,
    notice: Option<Notice>,
    fullscreen: bool,
    search: Option<SearchState>,
    menu: Option<MenuState>,
    confirm_quit: bool,
    quit_when_menu_closes: bool,
    start_after_config_save: bool,
    welcome_intro: bool,
    welcome_intro_seen: bool,
    problem_intro: bool,
    problem_intro_seen: bool,
    tasks_started: bool,
    countdown_snapshot: Vec<Option<u64>>,
    scene_seed: u64,
    scene_override: Option<SceneKind>,
    scene_frame: u64,
    scene_frame_override: Option<u64>,
    last_scene_frame: Instant,
}

impl App {
    fn new(
        loaded: LoadedConfig,
        tx: SyncSender<ProcessEvent>,
        rx: Receiver<ProcessEvent>,
        registry: ProcessRegistry,
        quit_when_menu_closes: bool,
        start_after_config_save: bool,
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
        let scene_seed = app_scene_seed(&loaded);
        let scene_override = dev_scene_kind_override();
        let scene_frame_override = dev_scene_frame_override();
        Self {
            loaded,
            tasks,
            focus: 0,
            mode: AppMode::Command,
            grid: Grid {
                columns: 1,
                rows: 1,
            },
            slot_rects: Vec::new(),
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
            last_click: None,
            clipboard: String::new(),
            notice: None,
            fullscreen: false,
            search: None,
            menu: None,
            confirm_quit: false,
            quit_when_menu_closes,
            start_after_config_save,
            welcome_intro: false,
            welcome_intro_seen: false,
            problem_intro: false,
            problem_intro_seen: false,
            tasks_started: false,
            countdown_snapshot: Vec::new(),
            scene_seed,
            scene_override,
            scene_frame: 0,
            scene_frame_override,
            last_scene_frame: Instant::now(),
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
            self.slot_rects = vec![Rect::default(); self.tasks.len()];
            self.pane_rects = vec![Rect::default(); self.tasks.len()];
            self.content_rects = vec![Rect::default(); self.tasks.len()];
            if !self.tasks.is_empty() {
                let focus = self.focus.min(self.tasks.len() - 1);
                self.slot_rects[focus] = pane_area;
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
            self.slot_rects = grid_rects(pane_area, self.grid);
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

    fn task_output_rect(&self, index: usize) -> Option<Rect> {
        let content = *self.content_rects.get(index)?;
        let Some(scene) = self.exited_scene_rect(index) else {
            return Some(content);
        };
        Some(Rect::new(
            content.x,
            content.y,
            content.width,
            content.height.saturating_sub(scene.height),
        ))
    }

    fn task_parser_row_offset(&self, index: usize) -> u16 {
        let Some(content) = self.content_rects.get(index) else {
            return 0;
        };
        let Some(output) = self.task_output_rect(index) else {
            return 0;
        };
        let Some(task) = self.tasks.get(index) else {
            return 0;
        };
        let available_shift = content.height.saturating_sub(output.height);
        let needed_shift = task
            .history
            .visible_line_count()
            .saturating_sub(u64::from(output.height));
        needed_shift.min(u64::from(available_shift)) as u16
    }

    fn exited_scene_rect(&self, index: usize) -> Option<Rect> {
        if self.mode == AppMode::Search {
            return None;
        }
        let task = self.tasks.get(index)?;
        if !matches!(task.status, TaskStatus::Exited { .. }) || task.scroll_offset != 0 {
            return None;
        }
        reserved_scene_rect(*self.content_rects.get(index)?)
    }

    fn task_scene_state(&self, index: usize, area: Rect) -> Option<SceneState> {
        let seed = mix_scene_seed(self.scene_seed, index as u64, 0x007e_17ed_u64);
        scene_state_for_area(seed, area, self.scene_override)
    }

    fn empty_slot_scene_state(&self, slot: usize, area: Rect) -> Option<SceneState> {
        let seed = mix_scene_seed(self.scene_seed, slot as u64, 0xe4d7_5107_u64);
        scene_state_for_area(seed, area, self.scene_override)
    }

    fn active_scene_frame(&self) -> u64 {
        self.scene_frame_override.unwrap_or(self.scene_frame)
    }

    fn scenes_visible(&self) -> bool {
        if !self.fullscreen && self.slot_rects.len() > self.tasks.len() {
            return true;
        }
        (0..self.tasks.len()).any(|index| self.exited_scene_rect(index).is_some())
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let now = Instant::now();
        let frame_area = frame.area();
        self.update_layout(frame_area);
        let buffer = frame.buffer_mut();
        self.footer_hits.clear();
        clear_rect(buffer, frame_area, app_style());

        if !self.fullscreen && self.slot_rects.len() > self.tasks.len() {
            let scene_frame = self.active_scene_frame();
            for slot in self.tasks.len()..self.slot_rects.len() {
                let area = self.slot_rects[slot];
                if area.width == 0 || area.height == 0 {
                    continue;
                }
                let scene_area = inset_rect(area, 1, 1);
                if let Some(scene) = self.empty_slot_scene_state(slot, scene_area) {
                    render_scene_slot(area, scene, scene_frame, buffer);
                }
            }
        }

        for index in 0..self.tasks.len() {
            let area = self.pane_rects[index];
            let Some(content) = self.task_output_rect(index) else {
                continue;
            };
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
                Span::styled(status, pane_style().fg(status_color)),
                Span::raw(" "),
                Span::styled(
                    self.tasks[index].task.name.as_str(),
                    pane_style()
                        .fg(if focused {
                            THEME_GOLD_HOVER
                        } else {
                            THEME_HOLLY
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if focused { " ✦ " } else { " " },
                    pane_style().fg(if focused {
                        THEME_GOLD_HOVER
                    } else {
                        THEME_HOLLY
                    }),
                ),
            ]);
            let restart = Line::from(Span::styled(
                " [↻] ",
                pane_style().fg(if focused {
                    THEME_GOLD_HOVER
                } else {
                    THEME_HOLLY
                }),
            ))
            .right_aligned();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(pane_style().fg(border_color))
                .style(pane_style())
                .title(title)
                .title(restart);
            block.render(area, buffer);

            let parser_row_offset = self.task_parser_row_offset(index);
            if self.tasks[index].renders_with_terminal_parser(content.height) {
                render_screen(
                    &self.tasks[index].parser,
                    content,
                    parser_row_offset,
                    buffer,
                );
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
                parser_row_offset,
                buffer,
            );
            if let Some(scene) = self.exited_scene_rect(index)
                && let Some(state) = self.task_scene_state(index, scene)
            {
                render_scene(scene, state, self.active_scene_frame(), buffer);
            }
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
            let exit_mode = menu_exit_mode(
                self.quit_when_menu_closes,
                self.start_after_config_save,
                self.tasks_started,
            );
            render_menu(
                frame_area,
                buffer,
                menu,
                self.loaded.config.settings.leader.label(),
                exit_mode,
                self.mouse_position,
            );
        }

        if self.welcome_intro {
            render_welcome_intro(frame_area, buffer);
        }

        if self.problem_intro {
            render_problem_intro(frame_area, buffer);
        }

        if self.confirm_quit {
            render_quit_confirm(frame_area, buffer);
        }

        if let Some(notice) = self.active_notice(now) {
            render_notice(frame_area, self.footer_rect, notice, buffer);
        }

        if self.mode == AppMode::Input && self.menu.is_none() && !self.tasks.is_empty() {
            let task = &self.tasks[self.focus];
            let area = self.task_output_rect(self.focus).unwrap_or_default();
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
        if self.welcome_intro || self.problem_intro {
            match key.code {
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Esc => {
                    self.welcome_intro = false;
                    self.problem_intro = false;
                }
                _ => {}
            }
            return Ok(Action::Continue);
        }

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
        let problems = self.loaded.config_problems.clone();
        self.menu = Some(MenuState::new(
            self.loaded.config.clone(),
            self.loaded.path.clone(),
            problems.clone(),
            tab,
        ));
        self.search = None;
        self.confirm_quit = false;
        let visible_problems = self.menu.as_ref().map(menu_problems).unwrap_or_default();
        let first_run_empty_config = self.loaded.created_from_missing_file
            && self.loaded.config.tasks.is_empty()
            && self.loaded.config_problems.is_empty();
        if first_run_empty_config && !self.welcome_intro_seen {
            self.welcome_intro = true;
            self.welcome_intro_seen = true;
        } else if !visible_problems.is_empty() && !self.problem_intro_seen {
            self.problem_intro = true;
            self.problem_intro_seen = true;
        }
        if visible_problems.is_empty() || first_run_empty_config {
            self.notice = None;
        } else {
            self.set_notice("Config problems found; use Exit > Problems before saving.".to_owned());
        }
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
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.env_task.is_some())
        {
            return self.handle_menu_env_key(key);
        }
        if self.menu.as_ref().is_some_and(|menu| menu.leader_picker) {
            return self.handle_menu_leader_key(key);
        }

        match key.code {
            KeyCode::Esc => return Ok(self.menu_back_or_close()),
            KeyCode::Left if self.selected_menu_slider() => {
                self.adjust_menu_multi_click(-(MULTI_CLICK_STEP_MS as i64));
            }
            KeyCode::Right if self.selected_menu_slider() => {
                self.adjust_menu_multi_click(MULTI_CLICK_STEP_MS as i64);
            }
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

    fn handle_menu_env_key(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Esc => return Ok(self.menu_back_or_close()),
            KeyCode::Up | KeyCode::Char('k') => self.move_menu_env_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_menu_env_cursor(1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.activate_selected_menu_item();
            }
            KeyCode::Delete => self.delete_selected_env_var(),
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
        menu.env_task = None;
        menu.env_detail_key = None;
        menu.env_cursor = 0;
    }

    fn move_menu_cursor(&mut self, delta: isize) {
        let exit_mode = self.menu_exit_mode();
        let count = self
            .menu
            .as_ref()
            .map(|menu| menu_item_count(menu, exit_mode))
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

    fn move_menu_env_cursor(&mut self, delta: isize) {
        let count = self.menu.as_ref().map(env_item_count).unwrap_or(0);
        if count == 0 {
            return;
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.env_cursor =
                (menu.env_cursor as isize + delta).rem_euclid(count as isize) as usize;
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
                if menu.env_task.is_some() {
                    return selected_env_action(menu);
                }
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
            MenuTab::Settings => match menu.cursor {
                1 => Some(MenuAction::OpenLeaderPicker),
                2 => Some(MenuAction::AdjustMultiClick(MULTI_CLICK_STEP_MS as i64)),
                _ => None,
            },
            MenuTab::Exit => {
                let actions = exit_actions(self.menu_exit_mode());
                if let Some(action) = actions.get(menu.cursor).copied() {
                    return Some(MenuAction::Exit(action));
                }
                let problem_index = menu.cursor.saturating_sub(actions.len());
                (problem_index < menu_problems(menu).len())
                    .then_some(MenuAction::Problem(problem_index))
            }
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
                    menu.env_task = None;
                    menu.env_detail_key = None;
                    menu.env_cursor = 0;
                    menu.leader_picker = false;
                }
            }
            MenuAction::Close => return Ok(self.menu_back_or_close()),
            MenuAction::OpenTask(index) => {
                if let Some(menu) = self.menu.as_mut()
                    && index < menu.draft.tasks.len()
                {
                    menu.task_list_cursor = index;
                    menu.task_detail = Some(index);
                    menu.cursor = 0;
                    menu.env_task = None;
                    menu.env_detail_key = None;
                    menu.env_cursor = 0;
                }
            }
            MenuAction::AddTask => self.add_menu_task(),
            MenuAction::TaskField(field) => self.activate_task_field(field),
            MenuAction::ToggleDependency(candidate) => self.toggle_dependency(candidate),
            MenuAction::OpenEnvEntry(index) => self.open_env_entry(index),
            MenuAction::AddEnvVar => self.start_new_env_var(),
            MenuAction::EnvField(field) => self.activate_env_field(field),
            MenuAction::DeleteEnvVar => self.delete_selected_env_var(),
            MenuAction::BackEnv => self.back_env_menu(),
            MenuAction::OpenLeaderPicker => self.open_menu_leader_picker(),
            MenuAction::SelectLeader(leader) => self.set_menu_leader(leader),
            MenuAction::AdjustMultiClick(delta) => self.adjust_menu_multi_click(delta),
            MenuAction::SetMultiClick(value) => self.set_menu_multi_click(value),
            MenuAction::Exit(action) => return self.handle_menu_exit_action(action),
            MenuAction::Problem(index) => self.jump_to_menu_problem(index),
        }
        Ok(Action::Continue)
    }

    fn jump_to_menu_problem(&mut self, problem_index: usize) {
        let Some(problem) = self
            .menu
            .as_ref()
            .and_then(|menu| menu_problems(menu).get(problem_index).cloned())
        else {
            return;
        };
        let exit_mode = self.menu_exit_mode();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        menu.edit = None;
        menu.dependency_task = None;
        menu.env_task = None;
        menu.env_detail_key = None;
        menu.env_cursor = 0;
        menu.leader_picker = false;
        match problem.location {
            ConfigProblemLocation::Root => {
                menu.tab = MenuTab::Exit;
                menu.cursor = exit_actions(exit_mode).len() + problem_index;
            }
            ConfigProblemLocation::Settings => {
                menu.tab = MenuTab::Settings;
                menu.cursor = 0;
                menu.task_detail = None;
            }
            ConfigProblemLocation::Setting(field) => {
                menu.tab = MenuTab::Settings;
                menu.cursor = setting_cursor(field);
                menu.task_detail = None;
            }
            ConfigProblemLocation::Tasks => {
                menu.tab = MenuTab::Tasks;
                menu.task_detail = None;
                menu.cursor = task_list_cursor(menu).min(menu.draft.tasks.len());
                menu.task_list_cursor = menu.cursor;
            }
            ConfigProblemLocation::Task { index, field } => {
                menu.tab = MenuTab::Tasks;
                let task_index = index.min(menu.draft.tasks.len().saturating_sub(1));
                if menu.draft.tasks.is_empty() {
                    menu.task_detail = None;
                    menu.cursor = 0;
                    menu.task_list_cursor = 0;
                    return;
                }
                menu.task_detail = Some(task_index);
                menu.task_list_cursor = task_index;
                menu.cursor = field
                    .and_then(config_task_field_cursor)
                    .unwrap_or(0)
                    .min(task_detail_fields().len().saturating_sub(1));
            }
        }
    }

    fn menu_exit_mode(&self) -> MenuExitMode {
        menu_exit_mode(
            self.quit_when_menu_closes,
            self.start_after_config_save,
            self.tasks_started,
        )
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
        if menu.env_detail_key.is_some() {
            menu.env_detail_key = None;
            menu.env_cursor = 0;
            return Action::Continue;
        }
        if menu.env_task.is_some() {
            menu.env_task = None;
            menu.env_cursor = 0;
            menu.cursor = config_task_field_cursor(ConfigTaskField::Env).unwrap_or(menu.cursor);
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
            TaskField::Name | TaskField::Command | TaskField::Cwd | TaskField::StartDelay => {
                self.start_menu_edit(task, field);
            }
            TaskField::Env => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.env_task = Some(task);
                    menu.env_detail_key = None;
                    menu.env_cursor = 0;
                }
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
            TaskField::StartDelay => task.start_delay.clone().unwrap_or_default(),
            _ => return,
        };
        menu.edit = Some(MenuEdit {
            target: MenuEditTarget::TaskField {
                task: task_index,
                field,
            },
            cursor: char_count(&value),
            value,
        });
    }

    fn submit_menu_edit(&mut self) {
        let Some(edit) = self.menu.as_mut().and_then(|menu| menu.edit.take()) else {
            return;
        };
        let result = self.apply_menu_edit(&edit);
        if let Err(error) = result {
            self.set_notice(format!("Edit not applied: {error:#}"));
            if let Some(menu) = self.menu.as_mut() {
                menu.edit = Some(edit);
            }
        }
    }

    fn apply_menu_edit(&mut self, edit: &MenuEdit) -> Result<()> {
        let root = self.loaded.root.clone();
        let Some(menu) = self.menu.as_mut() else {
            return Ok(());
        };
        match &edit.target {
            MenuEditTarget::TaskField { task, field } => {
                let value = edit.value.trim().to_owned();
                let Some(task) = menu.draft.tasks.get_mut(*task) else {
                    return Ok(());
                };
                match field {
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
                        if task.command.display() != value {
                            task.command = TaskCommand::Shell(value);
                        }
                    }
                    TaskField::Cwd => task.cwd = validate_menu_cwd(&root, &value)?,
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
            }
            MenuEditTarget::EnvKey { task, original_key } => {
                let key = edit.value.trim().to_owned();
                validate_env_key(&key)?;
                let Some(task_config) = menu.draft.tasks.get_mut(*task) else {
                    return Ok(());
                };
                if original_key
                    .as_ref()
                    .is_some_and(|original| original == &key)
                {
                    menu.env_detail_key = Some(key);
                    return Ok(());
                }
                if task_config.env.contains_key(&key) {
                    anyhow::bail!("environment key {key:?} already exists");
                }
                let value = original_key
                    .as_ref()
                    .and_then(|original| task_config.env.remove(original))
                    .unwrap_or_default();
                task_config.env.insert(key.clone(), value);
                menu.env_detail_key = Some(key);
                menu.env_cursor = 1;
            }
            MenuEditTarget::EnvValue { task, key } => {
                validate_env_value(&edit.value)?;
                let Some(task_config) = menu.draft.tasks.get_mut(*task) else {
                    return Ok(());
                };
                if !task_config.env.contains_key(key) {
                    anyhow::bail!("environment key {key:?} no longer exists");
                }
                task_config.env.insert(key.clone(), edit.value.clone());
            }
        }
        Ok(())
    }

    fn complete_menu_cwd(&mut self) {
        let root = self.loaded.root.clone();
        let Some((value, cursor)) = self.menu.as_ref().and_then(|menu| {
            let edit = menu.edit.as_ref()?;
            matches!(
                &edit.target,
                MenuEditTarget::TaskField {
                    field: TaskField::Cwd,
                    ..
                }
            )
            .then(|| (edit.value.clone(), edit.cursor))
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
        menu.env_task = None;
        menu.env_detail_key = None;
        menu.env_cursor = 0;
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

    fn open_env_entry(&mut self, entry_index: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task) = menu.env_task else {
            return;
        };
        let Some(key) = menu
            .draft
            .tasks
            .get(task)
            .and_then(|task| env_keys(task).get(entry_index).cloned())
        else {
            return;
        };
        menu.env_detail_key = Some(key);
        menu.env_cursor = 1;
    }

    fn start_new_env_var(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task) = menu.env_task else {
            return;
        };
        let Some(task_config) = menu.draft.tasks.get(task) else {
            return;
        };
        let key = unique_env_key(&task_config.env);
        menu.edit = Some(MenuEdit {
            target: MenuEditTarget::EnvKey {
                task,
                original_key: None,
            },
            cursor: char_count(&key),
            value: key,
        });
    }

    fn activate_env_field(&mut self, field: EnvField) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task) = menu.env_task else {
            return;
        };
        let Some(key) = menu.env_detail_key.clone() else {
            return;
        };
        let Some(task_config) = menu.draft.tasks.get(task) else {
            return;
        };
        if !task_config.env.contains_key(&key) {
            menu.env_detail_key = None;
            menu.env_cursor = 0;
            return;
        }
        let value = match field {
            EnvField::Key => key.clone(),
            EnvField::Value => task_config.env.get(&key).cloned().unwrap_or_default(),
        };
        let target = match field {
            EnvField::Key => MenuEditTarget::EnvKey {
                task,
                original_key: Some(key),
            },
            EnvField::Value => MenuEditTarget::EnvValue { task, key },
        };
        menu.edit = Some(MenuEdit {
            target,
            cursor: char_count(&value),
            value,
        });
    }

    fn delete_selected_env_var(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let Some(task) = menu.env_task else {
            return;
        };
        let Some(key) = menu.env_detail_key.clone() else {
            return;
        };
        if let Some(task_config) = menu.draft.tasks.get_mut(task) {
            task_config.env.remove(&key);
        }
        menu.env_detail_key = None;
        menu.env_cursor = 0;
    }

    fn back_env_menu(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if menu.env_detail_key.is_some() {
            menu.env_detail_key = None;
            menu.env_cursor = 0;
        } else {
            menu.env_task = None;
            menu.env_cursor = 0;
            menu.cursor = config_task_field_cursor(ConfigTaskField::Env).unwrap_or(menu.cursor);
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

    fn selected_menu_slider(&self) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|menu| menu.tab == MenuTab::Settings && menu.cursor == 2)
    }

    fn adjust_menu_multi_click(&mut self, delta: i64) {
        let current = self
            .menu
            .as_ref()
            .map(|menu| menu.draft.settings.multi_click_ms)
            .unwrap_or(self.loaded.config.settings.multi_click_ms);
        let next = (current as i64)
            .saturating_add(delta)
            .clamp(MIN_MULTI_CLICK_MS as i64, MAX_MULTI_CLICK_MS as i64) as u64;
        self.set_menu_multi_click(next);
    }

    fn set_menu_multi_click(&mut self, value: u64) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let next = rounded_multi_click_ms(value);
        menu.draft.settings.multi_click_ms = next;
        self.loaded.config.settings.multi_click_ms = next;
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
        let problems = menu_problems(menu);
        if problems
            .iter()
            .any(|problem| problem.severity == ConfigProblemSeverity::Error)
        {
            if let Some(menu) = self.menu.as_mut() {
                menu.tab = MenuTab::Exit;
                menu.cursor = 0;
            }
            self.set_notice("Fix red config problems before saving.".to_owned());
            return Ok(Action::Continue);
        }
        let draft = menu.draft.clone();
        let loaded = LoadedConfig {
            path: self.loaded.path.clone(),
            root: self.loaded.root.clone(),
            config: draft.clone(),
            config_warnings: Vec::new(),
            config_problems: Vec::new(),
            created_from_missing_file: false,
        };
        if let Err(error) = loaded.save() {
            self.set_notice(format!("Config not saved: {error:#}"));
            return Ok(Action::Continue);
        }

        let old = self.loaded.config.clone();
        self.menu = None;
        self.apply_saved_config(old, draft, restart);
        self.loaded.config_problems.clear();
        if self.start_after_config_save && !self.tasks_started {
            self.start_after_config_save = false;
            self.spawn_all();
            self.set_notice(format!(
                "Saved {}; starting tasks.",
                self.loaded.path.display()
            ));
        }
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
        self.loaded.config_warnings.clear();
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
            self.stop_tasks_for_rebuild();
            self.loaded.config = new;
            self.rebuild_unstarted_tasks();
            self.spawn_all();
            self.set_notice(format!(
                "Saved {}; restarted tasks.",
                self.loaded.path.display()
            ));
        }
    }

    fn stop_tasks_for_rebuild(&mut self) {
        for task in &mut self.tasks {
            task.restart_requested = false;
            task.start_requested = false;
            task.pending_start = None;
            task.kill_deadline = None;
            if let Some(pid) = task.pid {
                task.status = TaskStatus::Stopping;
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

        let deadline = Instant::now() + Duration::from_millis(500);
        while self.tasks.iter().any(|task| task.pid.is_some()) && Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => self.apply_process_event(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        for task in &mut self.tasks {
            if let Some(pid) = task.pid.take() {
                registry_remove(&self.registry, pid);
            }
            task.master = None;
            task.writer = None;
            task.kill_deadline = None;
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
            .task_output_rect(pane)
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
            granularity: SelectionGranularity::Character,
            origin: None,
            history_backed: true,
            parser_anchor: None,
            parser_cursor: None,
            parser_origin: None,
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

        if self.welcome_intro || self.problem_intro {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.welcome_intro = false;
                self.problem_intro = false;
            }
            return Ok(Action::Continue);
        }

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

        let Some(content) = self.task_output_rect(index) else {
            return Ok(Action::Continue);
        };
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
            if self.handle_multi_click_selection(index, mouse) {
                return Ok(Action::Continue);
            }
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
                } else if self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.env_task.is_some())
                {
                    self.move_menu_env_cursor(-1);
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
                } else if self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.env_task.is_some())
                {
                    self.move_menu_env_cursor(1);
                } else if self.menu.as_ref().is_some_and(|menu| menu.leader_picker) {
                    self.move_menu_leader_cursor(1);
                } else {
                    self.move_menu_cursor(1);
                }
                return Ok(Action::Continue);
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {}
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
        let parser_point = self.parser_selection_point_for_mouse(index, mouse.column, mouse.row);
        self.focus = index;
        self.selection = Some(Selection {
            pane: index,
            anchor: point,
            cursor: point,
            granularity: SelectionGranularity::Character,
            origin: None,
            history_backed: false,
            parser_anchor: parser_point,
            parser_cursor: parser_point,
            parser_origin: None,
            dragging: true,
            dragged: false,
            last_mouse: Some((mouse.column, mouse.row)),
            last_scroll: Instant::now(),
        });
    }

    fn handle_multi_click_selection(&mut self, index: usize, mouse: MouseEvent) -> bool {
        let click_count = self.register_left_click(index, mouse);
        match click_count {
            2 => self.start_granular_selection(index, mouse, SelectionGranularity::Word),
            3.. => self.start_granular_selection(index, mouse, SelectionGranularity::Line),
            _ => false,
        }
    }

    fn register_left_click(&mut self, index: usize, mouse: MouseEvent) -> u8 {
        let now = Instant::now();
        let threshold = Duration::from_millis(self.loaded.config.settings.multi_click_ms);
        let count = self
            .last_click
            .filter(|click| {
                click.pane == index
                    && click.x == mouse.column
                    && click.y == mouse.row
                    && now.duration_since(click.at) <= threshold
            })
            .map(|click| click.count.saturating_add(1).min(3))
            .unwrap_or(1);

        self.last_click = Some(ClickState {
            pane: index,
            x: mouse.column,
            y: mouse.row,
            at: now,
            count,
        });
        count
    }

    fn start_granular_selection(
        &mut self,
        index: usize,
        mouse: MouseEvent,
        granularity: SelectionGranularity,
    ) -> bool {
        let Some(span) = self.selection_span_for_mouse(index, mouse.column, mouse.row, granularity)
        else {
            return false;
        };
        let parser_span =
            self.parser_selection_span_for_mouse(index, mouse.column, mouse.row, granularity);
        self.focus = index;
        self.selection = Some(Selection {
            pane: index,
            anchor: span.start,
            cursor: span.end,
            granularity,
            origin: Some(span),
            history_backed: false,
            parser_anchor: parser_span.map(|span| span.start),
            parser_cursor: parser_span.map(|span| span.end),
            parser_origin: parser_span,
            dragging: true,
            dragged: true,
            last_mouse: Some((mouse.column, mouse.row)),
            last_scroll: Instant::now(),
        });
        true
    }

    fn selection_span_for_mouse(
        &self,
        index: usize,
        x: u16,
        y: u16,
        granularity: SelectionGranularity,
    ) -> Option<SelectionSpan> {
        let point = self.selection_point_for_mouse(index, x, y)?;
        if granularity == SelectionGranularity::Character {
            return Some(SelectionSpan::single(point));
        }

        let text = self.selection_text_for_mouse(index, x, y)?;
        self.selection_span_for_point(index, point, &text, granularity)
    }

    fn selection_span_for_point(
        &self,
        index: usize,
        point: SelectionPoint,
        text: &str,
        granularity: SelectionGranularity,
    ) -> Option<SelectionSpan> {
        match granularity {
            SelectionGranularity::Character => Some(SelectionSpan::single(point)),
            SelectionGranularity::Word => {
                let (start, end) =
                    word_columns_at(text, point.column).unwrap_or((point.column, point.column));
                Some(SelectionSpan {
                    start: SelectionPoint {
                        line: point.line,
                        column: start,
                    },
                    end: SelectionPoint {
                        line: point.line,
                        column: end,
                    },
                })
            }
            SelectionGranularity::Line => {
                let width = self
                    .task_output_rect(index)
                    .map(|rect| rect.width)
                    .unwrap_or_default();
                let end = width.saturating_sub(1);
                Some(SelectionSpan {
                    start: SelectionPoint {
                        line: point.line,
                        column: 0,
                    },
                    end: SelectionPoint {
                        line: point.line,
                        column: end,
                    },
                })
            }
        }
    }

    fn selection_text_for_mouse(&self, index: usize, x: u16, y: u16) -> Option<String> {
        let point = self.selection_point_for_mouse(index, x, y)?;
        let content = self.task_output_rect(index)?;
        let row = y
            .saturating_sub(content.y)
            .min(content.height.saturating_sub(1));
        Some(
            if self.tasks[index].renders_with_terminal_parser(content.height) {
                screen_row_text(&self.tasks[index].parser, row, content.width)
            } else {
                self.tasks[index]
                    .history
                    .line(point.line)
                    .map(|line| line.text.clone())
                    .unwrap_or_default()
            },
        )
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
        self.update_selection_cursor_from_mouse(pane, x, y);
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
            if let Some(selection) = self.selection.as_mut() {
                selection.last_scroll = Instant::now();
            }
            self.update_selection_cursor_from_mouse(pane, x, y);
        }
    }

    fn update_selection_cursor_from_mouse(&mut self, pane: usize, x: u16, y: u16) {
        let Some(granularity) = self
            .selection
            .as_ref()
            .map(|selection| selection.granularity)
        else {
            return;
        };
        let Some(span) = self.selection_span_for_mouse(pane, x, y, granularity) else {
            return;
        };
        let parser_span = self.parser_selection_span_for_mouse(pane, x, y, granularity);
        if let Some(selection) = self.selection.as_mut() {
            selection.set_cursor_span(span);
            selection.set_parser_cursor_span(parser_span);
        }
    }

    fn selection_point_for_mouse(&self, pane: usize, x: u16, y: u16) -> Option<SelectionPoint> {
        let (content, column, row) = self.selection_mouse_cell(pane, x, y)?;
        let line = self.tasks[pane].history_index_for_visible_row(row, content.height);
        Some(SelectionPoint { line, column })
    }

    fn parser_selection_point_for_mouse(
        &self,
        pane: usize,
        x: u16,
        y: u16,
    ) -> Option<SelectionPoint> {
        let (content, column, row) = self.selection_mouse_cell(pane, x, y)?;
        let task = &self.tasks[pane];
        if !task.renders_with_terminal_parser(content.height) {
            return None;
        }
        Some(SelectionPoint {
            line: task.parser_index_for_visible_row(self.task_parser_row_offset(pane) + row),
            column,
        })
    }

    fn parser_selection_span_for_mouse(
        &self,
        index: usize,
        x: u16,
        y: u16,
        granularity: SelectionGranularity,
    ) -> Option<SelectionSpan> {
        let point = self.parser_selection_point_for_mouse(index, x, y)?;
        if granularity == SelectionGranularity::Character {
            return Some(SelectionSpan::single(point));
        }

        let text = self.selection_text_for_mouse(index, x, y)?;
        self.selection_span_for_point(index, point, &text, granularity)
    }

    fn selection_mouse_cell(&self, pane: usize, x: u16, y: u16) -> Option<(Rect, u16, u16)> {
        let content = self.task_output_rect(pane)?;
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
        Some((content, column, row))
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
        let Some(content) = self.task_output_rect(pane) else {
            return false;
        };
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
            self.update_selection_cursor_from_mouse(pane, x, y);
        }
        changed
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        if !selection.dragged {
            return None;
        }
        if !selection.history_backed
            && let Some(text) = self.visible_selection_text(selection)
            && !text.is_empty()
        {
            return Some(text);
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
        let area = self.task_output_rect(selection.pane)?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if !task.renders_with_terminal_parser(area.height) {
            return None;
        }
        let row_offset = self.task_parser_row_offset(selection.pane);
        let (start, end, visible_start) =
            if let Some((start, end)) = selection.parser_ordered_points() {
                (start, end, task.parser_index_for_visible_row(row_offset))
            } else {
                let (start, end) = selection.ordered_points();
                (
                    start,
                    end,
                    task.history.visible_start(area.height, task.scroll_offset),
                )
            };
        let visible_end = visible_start.saturating_add(u64::from(area.height));
        if start.line < visible_start || end.line >= visible_end {
            return None;
        }

        let start_row = (start.line - visible_start) as u16;
        let end_row = (end.line - visible_start) as u16;
        let end_column = end.column.saturating_add(1).min(area.width);
        Some(task.parser.screen().contents_between(
            row_offset + start_row,
            start.column.min(area.width),
            row_offset + end_row,
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
        self.save_focused_history_to_dir(&default_history_dir())
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
        let area = self.task_output_rect(index)?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if task.renders_with_terminal_parser(area.height) {
            let row_offset = self.task_parser_row_offset(index);
            if row_offset == 0 {
                return Some(task.parser.screen().contents());
            }
            return Some(task.parser.screen().contents_between(
                row_offset,
                0,
                row_offset + area.height.saturating_sub(1),
                area.width,
            ));
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
        self.task_output_rect(self.focus)
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

    fn footer_parts(&self, _now: Instant) -> (&'static str, Color, Vec<FooterItem>) {
        if let Some(search) = self
            .search
            .as_ref()
            .filter(|_| self.mode == AppMode::Search)
        {
            return ("❄ SEARCH ❄", THEME_GOLD, search_footer_items(search));
        }

        match self.mode {
            AppMode::Input => (
                "❄ INPUT ❄",
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
            AppMode::Command => ("❄ COMMAND ❄", THEME_COMMAND, command_footer_items()),
            AppMode::Search => ("❄ SEARCH ❄", THEME_GOLD, search_placeholder_footer_items()),
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
            } => {
                let Some(runtime) = self.tasks.get_mut(task) else {
                    return;
                };
                if runtime.generation != generation {
                    return;
                }
                runtime.process_output(&bytes);
            }
            ProcessEvent::Exited {
                task,
                generation,
                status,
            } => {
                let Some(runtime) = self.tasks.get_mut(task) else {
                    return;
                };
                if runtime.generation != generation {
                    return;
                }
                if let Some(pid) = runtime.pid.take() {
                    registry_remove(&self.registry, pid);
                }
                runtime.master = None;
                runtime.writer = None;
                runtime.kill_deadline = None;
                runtime.status = TaskStatus::Exited {
                    code: status.exit_code(),
                    success: status.success(),
                    signal: status.signal().map(str::to_owned),
                };
                let reason = match status.signal() {
                    Some(signal) => format!("signal {signal}"),
                    None => format!("code {}", status.exit_code()),
                };
                runtime.message(&format!(
                    "\r\n\x1b[90m[demons] process exited ({reason})\x1b[0m\r\n"
                ));

                if runtime.restart_requested && !self.stopping {
                    runtime.restart_requested = false;
                    runtime.start_requested = true;
                    runtime.pending_start = None;
                }
                self.tick_dependency_starts(Instant::now());
            }
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
        if self.scene_frame_override.is_none()
            && self.scenes_visible()
            && now.duration_since(self.last_scene_frame) >= SCENE_FRAME_INTERVAL
        {
            self.scene_frame = self.scene_frame.wrapping_add(1);
            self.last_scene_frame = now;
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
    terminal_scrollback_len: usize,
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
            terminal_scrollback_len: 0,
            history: TextHistory::new(pty_size.cols, pty_size.rows, SCROLLBACK_LINES),
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
        self.sync_parser_scrollback();

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
        self.history.set_size(size.cols, size.rows);
        self.parser.set_size(size.rows, size.cols);
        self.refresh_terminal_scrollback_len();
        self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        self.sync_parser_scrollback();
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
        self.terminal_scrollback_len = 0;
        self.history.clear();
    }

    fn scroll_up(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(rows)
            .min(self.max_scroll_offset());
        self.sync_parser_scrollback();
        self.scroll_offset != previous
    }

    fn scroll_down(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.sync_parser_scrollback();
        self.scroll_offset != previous
    }

    fn scroll_to_top(&mut self) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.max_scroll_offset();
        self.sync_parser_scrollback();
        self.scroll_offset != previous
    }

    fn scroll_to_bottom(&mut self) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = 0;
        self.sync_parser_scrollback();
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
        self.sync_parser_scrollback();
    }

    fn max_scroll_offset(&self) -> usize {
        let history_scrollback = self
            .history
            .line_count()
            .saturating_sub(u64::from(self.pty_size.rows))
            .min(usize::MAX as u64) as usize;
        history_scrollback.max(self.parser_scrollback_limit(self.pty_size.rows))
    }

    fn renders_with_terminal_parser(&self, height: u16) -> bool {
        self.scroll_offset <= self.parser_scrollback_limit(height)
    }

    fn parser_scrollback_limit(&self, height: u16) -> usize {
        self.terminal_scrollback_len.min(usize::from(height))
    }

    fn sync_parser_scrollback(&mut self) {
        self.parser.set_scrollback(
            self.scroll_offset
                .min(self.parser_scrollback_limit(self.pty_size.rows)),
        );
    }

    fn refresh_terminal_scrollback_len(&mut self) {
        let current = self.parser.screen().scrollback();
        self.parser.set_scrollback(usize::MAX);
        self.terminal_scrollback_len = self.parser.screen().scrollback();
        self.parser
            .set_scrollback(current.min(self.terminal_scrollback_len));
    }

    fn message(&mut self, message: &str) {
        self.scroll_offset = 0;
        self.sync_parser_scrollback();
        self.history.process(message.as_bytes());
        self.parser.process(message.as_bytes());
        self.refresh_terminal_scrollback_len();
        self.sync_parser_scrollback();
    }

    fn process_output(&mut self, bytes: &[u8]) {
        let parser_offset = self.parser.screen().scrollback();
        let added_rows = self.history.process(bytes);
        self.parser.process(bytes);
        let parser_added_rows = self
            .parser
            .screen()
            .scrollback()
            .saturating_sub(parser_offset);
        self.refresh_terminal_scrollback_len();
        if self.scroll_offset > 0 {
            let added_rows = added_rows.max(parser_added_rows);
            if added_rows > 0 {
                self.scroll_offset = self.scroll_offset.saturating_add(added_rows);
            }
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
        self.sync_parser_scrollback();
    }

    fn history_index_for_visible_row(&self, row: u16, height: u16) -> u64 {
        self.history
            .visible_start(height, self.scroll_offset)
            .saturating_add(u64::from(row))
    }

    fn parser_index_for_visible_row(&self, row: u16) -> u64 {
        let scrollback = self.parser.screen().scrollback();
        (self.terminal_scrollback_len.saturating_sub(scrollback) as u64)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionSpan {
    start: SelectionPoint,
    end: SelectionPoint,
}

impl SelectionSpan {
    fn single(point: SelectionPoint) -> Self {
        Self {
            start: point,
            end: point,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionGranularity {
    Character,
    Word,
    Line,
}

#[derive(Clone, Debug)]
struct Selection {
    pane: usize,
    anchor: SelectionPoint,
    cursor: SelectionPoint,
    granularity: SelectionGranularity,
    origin: Option<SelectionSpan>,
    history_backed: bool,
    parser_anchor: Option<SelectionPoint>,
    parser_cursor: Option<SelectionPoint>,
    parser_origin: Option<SelectionSpan>,
    dragging: bool,
    dragged: bool,
    last_mouse: Option<(u16, u16)>,
    last_scroll: Instant,
}

#[derive(Clone, Copy, Debug)]
struct ClickState {
    pane: usize,
    x: u16,
    y: u16,
    at: Instant,
    count: u8,
}

impl Selection {
    fn ordered_points(&self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    fn parser_ordered_points(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let anchor = self.parser_anchor?;
        let cursor = self.parser_cursor?;
        Some(if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    fn set_cursor_span(&mut self, span: SelectionSpan) {
        let Some(origin) = self.origin else {
            self.cursor = span.end;
            return;
        };

        if span.end < origin.start {
            self.anchor = origin.end;
            self.cursor = span.start;
        } else {
            self.anchor = origin.start;
            self.cursor = span.end;
        }
    }

    fn set_parser_cursor_span(&mut self, span: Option<SelectionSpan>) {
        let Some(span) = span else {
            self.parser_cursor = None;
            return;
        };
        if self.parser_anchor.is_none() {
            self.parser_anchor = Some(span.start);
        }

        let Some(origin) = self.parser_origin else {
            self.parser_cursor = Some(span.end);
            return;
        };

        if span.end < origin.start {
            self.parser_anchor = Some(origin.end);
            self.parser_cursor = Some(span.start);
        } else {
            self.parser_anchor = Some(origin.start);
            self.parser_cursor = Some(span.end);
        }
    }

    fn columns_for_line(&self, line: u64, width: u16) -> Option<(u16, u16)> {
        if !self.dragged || width == 0 {
            return None;
        }
        let (start, end) = self.ordered_points();
        columns_for_ordered_points(start, end, line, width)
    }

    fn columns_for_parser_line(&self, line: u64, width: u16) -> Option<(u16, u16)> {
        if !self.dragged || width == 0 {
            return None;
        }
        let (start, end) = self.parser_ordered_points()?;
        columns_for_ordered_points(start, end, line, width)
    }
}

fn columns_for_ordered_points(
    start: SelectionPoint,
    end: SelectionPoint,
    line: u64,
    width: u16,
) -> Option<(u16, u16)> {
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
    cursor_home: bool,
    pending_wrap: bool,
    width: u16,
    height: u16,
    max_lines: usize,
    state: TextParserState,
    csi: String,
}

impl TextHistory {
    fn new(width: u16, height: u16, max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            first_index: 0,
            current: HistoryLine::default(),
            column: 0,
            cursor_home: false,
            pending_wrap: false,
            width: width.max(1),
            height: height.max(1),
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
        self.cursor_home = false;
        self.pending_wrap = false;
        self.state = TextParserState::Ground;
        self.csi.clear();
    }

    fn clear_visible_screen(&mut self) {
        let finalized_rows_to_remove = usize::from(self.height.saturating_sub(1));
        for _ in 0..finalized_rows_to_remove.min(self.lines.len()) {
            self.lines.pop_back();
        }
        self.current = HistoryLine::default();
        self.column = 0;
        self.cursor_home = false;
        self.pending_wrap = false;
    }

    fn set_size(&mut self, width: u16, height: u16) {
        self.width = width.max(1);
        self.height = height.max(1);
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

    fn visible_line_count(&self) -> u64 {
        let has_trailing_empty_line =
            self.current.text.is_empty() && self.column == 0 && !self.pending_wrap;
        self.line_count()
            .saturating_sub(u64::from(has_trailing_empty_line))
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
                    '\n' => {
                        self.cursor_home = false;
                        added_rows += self.push_current(false);
                    }
                    '\r' => {
                        self.cursor_home = false;
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
                        self.cursor_home = false;
                        let spaces = 8 - (self.column % 8);
                        for _ in 0..spaces {
                            added_rows += self.put_char(' ');
                        }
                    }
                    character if character.is_control() => {}
                    character => {
                        self.cursor_home = false;
                        added_rows += self.put_char(character);
                    }
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
        match final_byte {
            'H' | 'f' => self.apply_cursor_position(),
            'J' => self.apply_erase_display(),
            'K' => self.apply_erase_line(),
            _ => self.cursor_home = false,
        }
    }

    fn apply_cursor_position(&mut self) {
        let row = csi_param(&self.csi, 0, 1).max(1);
        let column = csi_param(&self.csi, 1, 1).max(1);
        self.column = column
            .saturating_sub(1)
            .min(usize::from(self.width.saturating_sub(1)));
        self.pending_wrap = false;
        self.cursor_home = row == 1 && column == 1;
    }

    fn apply_erase_display(&mut self) {
        let mode = first_csi_param(&self.csi).unwrap_or(0);
        if mode == 3 {
            self.clear();
            return;
        }
        if mode == 2 || (mode == 0 && self.cursor_home) {
            self.clear_visible_screen();
            return;
        }
        if mode == 0 {
            self.apply_erase_line();
        }
        self.cursor_home = false;
    }

    fn apply_erase_line(&mut self) {
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
        self.cursor_home = false;
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
    env_task: Option<usize>,
    env_detail_key: Option<String>,
    env_cursor: usize,
    leader_picker: bool,
    leader_cursor: usize,
    edit: Option<MenuEdit>,
    path: PathBuf,
    initial_problems: Vec<ConfigProblem>,
    draft: crate::config::Config,
    original: crate::config::Config,
    hits: Vec<MenuHit>,
}

impl MenuState {
    fn new(
        config: crate::config::Config,
        path: PathBuf,
        initial_problems: Vec<ConfigProblem>,
        tab: MenuTab,
    ) -> Self {
        Self {
            tab,
            cursor: 0,
            task_list_cursor: 0,
            task_detail: None,
            dependency_task: None,
            dependency_cursor: 0,
            env_task: None,
            env_detail_key: None,
            env_cursor: 0,
            leader_picker: false,
            leader_cursor: 0,
            edit: None,
            path,
            initial_problems,
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
    target: MenuEditTarget,
    value: String,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MenuEditTarget {
    TaskField {
        task: usize,
        field: TaskField,
    },
    EnvKey {
        task: usize,
        original_key: Option<String>,
    },
    EnvValue {
        task: usize,
        key: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvField {
    Key,
    Value,
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
enum MenuExitMode {
    ConfigureOnly,
    StartAfterSave,
    Runtime,
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
    OpenEnvEntry(usize),
    AddEnvVar,
    EnvField(EnvField),
    DeleteEnvVar,
    BackEnv,
    OpenLeaderPicker,
    SelectLeader(Leader),
    AdjustMultiClick(i64),
    SetMultiClick(u64),
    Exit(MenuExitAction),
    Problem(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProblemBadges {
    error: bool,
    warning: bool,
}

impl ProblemBadges {
    fn add(&mut self, severity: ConfigProblemSeverity) {
        match severity {
            ConfigProblemSeverity::Error => self.error = true,
            ConfigProblemSeverity::Warning => self.warning = true,
        }
    }

    fn any(self) -> bool {
        self.error || self.warning
    }
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
    exit_mode: MenuExitMode,
    hover_position: Option<(u16, u16)>,
) {
    let popup = centered_rect(area, 92, 26);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    menu.hits.clear();
    let problems = menu_problems(menu);
    Clear.render(popup, buffer);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(menu_style().fg(THEME_GOLD))
        .style(menu_style())
        .title(Line::styled(
            format!(" {THEME_ACCENT_MARK} Demons Menu {THEME_ACCENT_MARK} "),
            menu_style()
                .fg(THEME_GOLD_HOVER)
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
            Style::default().fg(THEME_BLACK).bg(if close_hovered {
                THEME_RED_HOVER
            } else {
                THEME_SNOW
            }),
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
    for tab in &MenuTab::ALL {
        let badges = tab_problem_badges(&problems, *tab);
        let label = format!(" {} ", tab_text_with_problem_badges(tab.label(), badges));
        let width = char_count(&label).min(usize::from(u16::MAX)) as u16;
        if tab_x.saturating_add(width) > inner.right() {
            break;
        }
        let selected = *tab == menu.tab;
        let rect = Rect::new(tab_x, inner.y, width, 1);
        let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
        let style = if selected {
            Style::default()
                .fg(THEME_BLACK)
                .bg(THEME_SNOW)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default().fg(THEME_BLACK).bg(THEME_GOLD_HOVER)
        } else {
            menu_style()
        };
        render_text(buffer, rect, &label, style);
        render_tab_problem_badges(buffer, rect, tab.label(), badges, selected, hovered);
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
    if inner.height > 1 {
        render_ribbon(
            buffer,
            Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
        );
    }

    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    clear_rect(buffer, body, menu_style());
    if let Some(edit) = menu.edit.as_ref() {
        render_menu_edit(body, buffer, edit);
        return;
    }
    if let Some(task) = menu.dependency_task {
        render_menu_dependencies(body, buffer, menu, task, hover_position);
        return;
    }
    if let Some(task) = menu.env_task {
        render_menu_env(body, buffer, menu, task, hover_position);
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
        MenuTab::Exit => render_menu_exit(body, buffer, menu, exit_mode, hover_position),
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
        "drag                   Select text".to_owned(),
        "double-click/drag      Select whole words".to_owned(),
        "triple-click/drag      Select whole lines".to_owned(),
        "right-click            Copy selection or paste".to_owned(),
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
        "Left / Right           Adjust slider settings".to_owned(),
        "Enter / click          Activate an option".to_owned(),
        "Space                  Toggle dependency checkboxes".to_owned(),
        "Esc                    Back out one level".to_owned(),
    ];
    for (row, line) in lines.iter().enumerate() {
        if row >= usize::from(area.height) {
            break;
        }
        let style = if line == "Command mode" || line == "Menu" {
            menu_heading_style()
        } else {
            menu_style()
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
        menu_heading_style(),
    );
    let rows = area.height.saturating_sub(1);
    let count = menu.draft.tasks.len() + 1;
    let start = scroll_start(menu.cursor, count, usize::from(rows));
    let problems = menu_problems(menu);
    for row in 0..rows {
        let index = start + usize::from(row);
        let y = area.y + row + 1;
        let (text, action, badges) = if index < menu.draft.tasks.len() {
            let task = &menu.draft.tasks[index];
            let badges = task_problem_badges(&problems, index);
            (
                format!("{}  {}", task.name, task.command.display()),
                MenuAction::OpenTask(index),
                badges,
            )
        } else if index == menu.draft.tasks.len() {
            (
                "+ Add task".to_owned(),
                MenuAction::AddTask,
                ProblemBadges::default(),
            )
        } else {
            break;
        };
        let rect = Rect::new(area.x, y, area.width, 1);
        let selected = index == menu.cursor;
        let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
        let text = text_with_problem_badges(&text, badges);
        render_menu_row(
            buffer,
            rect,
            &text,
            selected,
            Some(action),
            &mut menu.hits,
            hover_position,
        );
        render_problem_badges(buffer, rect, badges, row_bg_color(selected, hovered));
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
        menu_heading_style(),
    );
    let fields = task_detail_fields();
    let problems = menu_problems(menu);
    for (row, field) in fields.iter().enumerate() {
        if row + 1 >= usize::from(area.height) {
            break;
        }
        let badges = task_field_problem_badges(&problems, task_index, *field);
        let text = text_with_problem_badges(&task_field_text(task, *field), badges);
        let rect = Rect::new(area.x, area.y + row as u16 + 1, area.width, 1);
        let selected = row == menu.cursor;
        let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
        render_menu_row(
            buffer,
            rect,
            &text,
            selected,
            Some(MenuAction::TaskField(*field)),
            &mut menu.hits,
            hover_position,
        );
        render_problem_badges(buffer, rect, badges, row_bg_color(selected, hovered));
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
        menu_heading_style(),
    );
    let candidates = dependency_candidates(menu, task);
    if candidates.is_empty() {
        render_text(
            buffer,
            Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
            "No other tasks are configured.",
            menu_style(),
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

fn render_menu_env(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    task_index: usize,
    hover_position: Option<(u16, u16)>,
) {
    if menu.env_detail_key.is_some() {
        render_menu_env_detail(area, buffer, menu, task_index, hover_position);
    } else {
        render_menu_env_list(area, buffer, menu, task_index, hover_position);
    }
}

fn render_menu_env_list(
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
        &format!("Environment for {}", task.name),
        menu_heading_style(),
    );

    let rows = area.height.saturating_sub(1);
    let count = task.env.len() + 2;
    let start = scroll_start(menu.env_cursor, count, usize::from(rows));
    for row in 0..rows {
        let index = start + usize::from(row);
        let rect = Rect::new(area.x, area.y + row + 1, area.width, 1);
        let (text, action) = if index == 0 {
            ("+ Add variable".to_owned(), MenuAction::AddEnvVar)
        } else if index <= task.env.len() {
            let keys = env_keys(task);
            let key = &keys[index - 1];
            let value = task.env.get(key).map(String::as_str).unwrap_or_default();
            (
                format!("{key} = {}", display_env_value(value)),
                MenuAction::OpenEnvEntry(index - 1),
            )
        } else if index == task.env.len() + 1 {
            ("Back to task".to_owned(), MenuAction::BackEnv)
        } else {
            break;
        };
        render_menu_row(
            buffer,
            rect,
            &text,
            index == menu.env_cursor,
            Some(action),
            &mut menu.hits,
            hover_position,
        );
    }
}

fn render_menu_env_detail(
    area: Rect,
    buffer: &mut Buffer,
    menu: &mut MenuState,
    task_index: usize,
    hover_position: Option<(u16, u16)>,
) {
    let Some(key) = menu.env_detail_key.clone() else {
        return;
    };
    let Some(task) = menu.draft.tasks.get(task_index) else {
        return;
    };
    let Some(value) = task.env.get(&key) else {
        return;
    };
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        &format!("Environment variable: {key}"),
        menu_heading_style(),
    );

    let rows = [
        (format!("Key: {key}"), MenuAction::EnvField(EnvField::Key)),
        (
            format!("Value: {}", display_env_value(value)),
            MenuAction::EnvField(EnvField::Value),
        ),
        ("Delete variable".to_owned(), MenuAction::DeleteEnvVar),
        ("Back to environment".to_owned(), MenuAction::BackEnv),
    ];
    for (row, (text, action)) in rows.iter().enumerate() {
        if row + 1 >= usize::from(area.height) {
            break;
        }
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
            text,
            row == menu.env_cursor,
            Some(*action),
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
        menu_heading_style(),
    );
    let problems = menu_problems(menu);
    render_menu_static_setting_row(
        buffer,
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        "Layout: grid",
        menu.cursor == 0,
        setting_problem_badges(&problems, ConfigSettingField::Layout),
        hover_position,
    );
    let leader_rect = Rect::new(area.x, area.y.saturating_add(2), area.width, 1);
    let leader_hovered = hover_position.is_some_and(|(x, y)| contains(leader_rect, x, y));
    let leader_badges = setting_problem_badges(&problems, ConfigSettingField::Leader);
    render_menu_row(
        buffer,
        leader_rect,
        &text_with_problem_badges(
            &format!("Leader key: {}", menu.draft.settings.leader.label()),
            leader_badges,
        ),
        menu.cursor == 1,
        Some(MenuAction::OpenLeaderPicker),
        &mut menu.hits,
        hover_position,
    );
    render_problem_badges(
        buffer,
        leader_rect,
        leader_badges,
        row_bg_color(menu.cursor == 1, leader_hovered),
    );
    render_menu_multi_click_row(
        buffer,
        Rect::new(area.x, area.y.saturating_add(3), area.width, 1),
        menu,
        &problems,
        hover_position,
    );
    render_menu_static_setting_row(
        buffer,
        Rect::new(area.x, area.y.saturating_add(4), area.width, 1),
        &format!(
            "Logging: {}",
            if menu.draft.settings.logging {
                "enabled"
            } else {
                "disabled"
            }
        ),
        menu.cursor == 3,
        setting_problem_badges(&problems, ConfigSettingField::Logging),
        hover_position,
    );
}

fn render_menu_static_setting_row(
    buffer: &mut Buffer,
    rect: Rect,
    text: &str,
    selected: bool,
    badges: ProblemBadges,
    hover_position: Option<(u16, u16)>,
) {
    let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
    let mut ignored_hits = Vec::new();
    render_menu_row(
        buffer,
        rect,
        &text_with_problem_badges(text, badges),
        selected,
        None,
        &mut ignored_hits,
        hover_position,
    );
    render_problem_badges(buffer, rect, badges, row_bg_color(selected, hovered));
}

fn render_menu_multi_click_row(
    buffer: &mut Buffer,
    rect: Rect,
    menu: &mut MenuState,
    problems: &[ConfigProblem],
    hover_position: Option<(u16, u16)>,
) {
    if rect.width == 0 {
        return;
    }

    let value = menu.draft.settings.multi_click_ms;
    let bar_width = multi_click_slider_width();
    let bar = slider_bar(value, MIN_MULTI_CLICK_MS, MAX_MULTI_CLICK_MS, bar_width);
    let prefix = format!("Multi-click timing: {value}ms  ");
    let badges = setting_problem_badges(problems, ConfigSettingField::MultiClick);
    let text = text_with_problem_badges(&format!("{prefix}< [{bar}] >"), badges);
    let hovered = hover_position.is_some_and(|(x, y)| contains(rect, x, y));
    let style = if menu.cursor == 2 {
        Style::default().fg(THEME_BLACK).bg(THEME_SNOW)
    } else if hovered {
        Style::default().fg(THEME_BLACK).bg(THEME_GOLD_HOVER)
    } else {
        menu_style()
    };
    render_text(buffer, rect, &text, style);
    render_problem_badges(
        buffer,
        rect,
        badges,
        row_bg_color(menu.cursor == 2, hovered),
    );

    let badge_offset = if badges.any() {
        to_u16(problem_badge_width(badges) + 1)
    } else {
        0
    };
    let minus_x = rect
        .x
        .saturating_add(badge_offset)
        .saturating_add(to_u16(char_count(&prefix)));
    if minus_x < rect.right() {
        menu.hits.push(MenuHit {
            rect: Rect::new(minus_x, rect.y, 1, 1),
            action: MenuAction::AdjustMultiClick(-(MULTI_CLICK_STEP_MS as i64)),
        });
    }

    let slider_x = minus_x.saturating_add(to_u16(char_count("< [")));
    for index in 0..bar_width {
        let cell_x = slider_x.saturating_add(to_u16(index));
        if cell_x >= rect.right() {
            break;
        }
        menu.hits.push(MenuHit {
            rect: Rect::new(cell_x, rect.y, 1, 1),
            action: MenuAction::SetMultiClick(multi_click_value_for_slider_index(index, bar_width)),
        });
    }

    let plus_x = slider_x
        .saturating_add(to_u16(bar_width))
        .saturating_add(to_u16(char_count("] ")));
    if plus_x < rect.right() {
        menu.hits.push(MenuHit {
            rect: Rect::new(plus_x, rect.y, 1, 1),
            action: MenuAction::AdjustMultiClick(MULTI_CLICK_STEP_MS as i64),
        });
    }
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
        menu_heading_style(),
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
    exit_mode: MenuExitMode,
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
        menu_heading_style(),
    );
    let actions = exit_actions(exit_mode);
    let problems = menu_problems(menu);
    for (row, action) in actions.iter().enumerate() {
        if row + 1 >= usize::from(area.height) {
            break;
        }
        render_menu_row(
            buffer,
            Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
            exit_action_label(*action, exit_mode),
            row == menu.cursor,
            Some(MenuAction::Exit(*action)),
            &mut menu.hits,
            hover_position,
        );
    }

    let mut y = area
        .y
        .saturating_add(1)
        .saturating_add(to_u16(actions.len()))
        .saturating_add(1);
    if y >= area.bottom() {
        return;
    }
    render_text(
        buffer,
        Rect::new(area.x, y, area.width, 1),
        "Problems",
        menu_heading_style(),
    );
    y = y.saturating_add(1);
    if problems.is_empty() {
        if y < area.bottom() {
            render_text(
                buffer,
                Rect::new(area.x, y, area.width, 1),
                "No config problems.",
                menu_style(),
            );
        }
        return;
    }

    let problem_start = actions.len();
    let visible_problem_count = usize::from(area.bottom().saturating_sub(y));
    let start = scroll_start(
        menu.cursor.saturating_sub(problem_start),
        problems.len(),
        visible_problem_count,
    );
    for problem_index in start..problems.len() {
        let Some(problem) = problems.get(problem_index) else {
            break;
        };
        let selected = menu.cursor == problem_start + problem_index;
        for line in wrap_line(&problem_line(problem), usize::from(area.width)) {
            if y >= area.bottom() {
                return;
            }
            let rect = Rect::new(area.x, y, area.width, 1);
            render_menu_row(
                buffer,
                rect,
                &line,
                selected,
                Some(MenuAction::Problem(problem_index)),
                &mut menu.hits,
                hover_position,
            );
            y = y.saturating_add(1);
        }
    }
}

fn render_menu_edit(area: Rect, buffer: &mut Buffer, edit: &MenuEdit) {
    render_text(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        &format!("Editing {}", menu_edit_title(edit)),
        menu_heading_style(),
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
        menu_style(),
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
        Style::default().fg(THEME_BLACK).bg(THEME_GOLD_HOVER)
    } else {
        menu_style()
    };
    render_text(buffer, rect, text, style);
    if let Some(action) = action {
        hits.push(MenuHit { rect, action });
    }
}

fn row_bg_color(selected: bool, hovered: bool) -> Color {
    if selected {
        THEME_SNOW
    } else if hovered {
        THEME_GOLD_HOVER
    } else {
        THEME_MENU
    }
}

fn tab_bg_color(selected: bool, hovered: bool) -> Color {
    if selected {
        THEME_SNOW
    } else if hovered {
        THEME_GOLD_HOVER
    } else {
        THEME_MENU
    }
}

fn tab_text_with_problem_badges(text: &str, badges: ProblemBadges) -> String {
    if !badges.any() {
        return text.to_owned();
    }
    let mut value = text.to_owned();
    if badges.error {
        value.push_str(" !");
    }
    if badges.warning {
        value.push_str(" !");
    }
    value
}

fn text_with_problem_badges(text: &str, badges: ProblemBadges) -> String {
    let width = problem_badge_width(badges);
    if width == 0 {
        text.to_owned()
    } else {
        format!("{}{text}", " ".repeat(width + 1))
    }
}

fn render_tab_problem_badges(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    badges: ProblemBadges,
    selected: bool,
    hovered: bool,
) {
    if !badges.any() {
        return;
    }
    let bg = tab_bg_color(selected, hovered);
    let mut x = rect
        .x
        .saturating_add(1)
        .saturating_add(to_u16(char_count(label)))
        .saturating_add(1);
    if badges.error {
        render_problem_badge(buffer, x, rect.y, ConfigProblemSeverity::Error, bg);
        x = x.saturating_add(2);
    }
    if badges.warning {
        render_problem_badge(buffer, x, rect.y, ConfigProblemSeverity::Warning, bg);
    }
}

fn render_problem_badges(buffer: &mut Buffer, rect: Rect, badges: ProblemBadges, bg: Color) {
    if rect.width == 0 || !badges.any() {
        return;
    }
    let mut x = rect.x;
    if badges.error {
        render_problem_badge(buffer, x, rect.y, ConfigProblemSeverity::Error, bg);
        x = x.saturating_add(1);
    }
    if badges.warning {
        render_problem_badge(buffer, x, rect.y, ConfigProblemSeverity::Warning, bg);
    }
}

fn render_problem_badge(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    severity: ConfigProblemSeverity,
    bg: Color,
) {
    buffer[(x, y)].set_symbol("!").set_style(
        Style::default()
            .fg(problem_color(severity))
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
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
        let symbol = if character.is_control() {
            " "
        } else {
            character.encode_utf8(&mut encoded)
        };
        buffer[(rect.x + column as u16, rect.y)]
            .set_symbol(symbol)
            .set_style(style);
    }
}

fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        let word_width = char_count(word);
        let line_width = char_count(&line);
        if line.is_empty() {
            if word_width <= width {
                line.push_str(word);
            } else {
                lines.extend(wrap_long_word(word, width));
            }
        } else if line_width + 1 + word_width <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(line);
            line = String::new();
            if word_width <= width {
                line.push_str(word);
            } else {
                lines.extend(wrap_long_word(word, width));
            }
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_long_word(word: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut remaining = word;
    while !remaining.is_empty() {
        let take = remaining
            .char_indices()
            .nth(width)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        lines.push(remaining[..take].to_owned());
        remaining = &remaining[take..];
    }
    lines
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

fn render_ribbon(buffer: &mut Buffer, rect: Rect) {
    for column in 0..rect.width {
        let color = match column % 8 {
            0..=2 => THEME_RED,
            3..=5 => THEME_SNOW,
            _ => THEME_GOLD,
        };
        let fg = if color == THEME_RED {
            THEME_WHITE
        } else {
            THEME_BLACK
        };
        buffer[(rect.x + column, rect.y)]
            .set_symbol(" ")
            .set_style(Style::default().fg(fg).bg(color));
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

fn menu_item_count(menu: &MenuState, exit_mode: MenuExitMode) -> usize {
    match menu.tab {
        MenuTab::Help => 0,
        MenuTab::Tasks if menu.env_task.is_some() => env_item_count(menu),
        MenuTab::Tasks if menu.task_detail.is_some() => task_detail_fields().len(),
        MenuTab::Tasks => menu.draft.tasks.len() + 1,
        MenuTab::Settings => 4,
        MenuTab::Exit => exit_actions(exit_mode).len() + menu_problems(menu).len(),
    }
}

fn menu_problems(menu: &MenuState) -> Vec<ConfigProblem> {
    let mut problems = Vec::new();
    for problem in &menu.initial_problems {
        if problem.severity == ConfigProblemSeverity::Warning
            && warning_still_applies(menu, problem)
        {
            push_unique_problem(&mut problems, problem.clone());
        }
    }
    for problem in config_blocking_problems(&menu.draft, &menu.path) {
        push_unique_problem(&mut problems, problem);
    }
    problems
}

fn push_unique_problem(problems: &mut Vec<ConfigProblem>, problem: ConfigProblem) {
    if !problems.contains(&problem) {
        problems.push(problem);
    }
}

fn warning_still_applies(menu: &MenuState, problem: &ConfigProblem) -> bool {
    match &problem.location {
        ConfigProblemLocation::Root => !is_generic_root_recovery_warning(problem),
        ConfigProblemLocation::Settings | ConfigProblemLocation::Tasks => true,
        ConfigProblemLocation::Setting(field) => setting_value_unchanged(menu, *field),
        ConfigProblemLocation::Task { index, field } => task_value_unchanged(menu, *index, *field),
    }
}

fn is_generic_root_recovery_warning(problem: &ConfigProblem) -> bool {
    problem.severity == ConfigProblemSeverity::Warning
        && matches!(problem.location, ConfigProblemLocation::Root)
        && (problem.message == "Recovered config after a parse or schema mismatch."
            || problem.message == "Additional config recovery warnings omitted.")
}

fn setting_value_unchanged(menu: &MenuState, field: ConfigSettingField) -> bool {
    match field {
        ConfigSettingField::Layout => menu.draft.settings.layout == menu.original.settings.layout,
        ConfigSettingField::Leader => menu.draft.settings.leader == menu.original.settings.leader,
        ConfigSettingField::MultiClick => {
            menu.draft.settings.multi_click_ms == menu.original.settings.multi_click_ms
        }
        ConfigSettingField::Logging => {
            menu.draft.settings.logging == menu.original.settings.logging
        }
    }
}

fn task_value_unchanged(menu: &MenuState, index: usize, field: Option<ConfigTaskField>) -> bool {
    let Some(current) = menu.draft.tasks.get(index) else {
        return false;
    };
    let Some(original) = menu.original.tasks.get(index) else {
        return false;
    };
    match field {
        Some(ConfigTaskField::Name) => current.name == original.name,
        Some(ConfigTaskField::Command) => current.command == original.command,
        Some(ConfigTaskField::Cwd) => current.cwd == original.cwd,
        Some(ConfigTaskField::Env) => current.env == original.env,
        Some(ConfigTaskField::Dependencies) => current.depends_on == original.depends_on,
        Some(ConfigTaskField::StartDelay) => current.start_delay == original.start_delay,
        None => current == original,
    }
}

fn tab_problem_badges(problems: &[ConfigProblem], tab: MenuTab) -> ProblemBadges {
    let mut badges = ProblemBadges::default();
    for problem in problems {
        let belongs = match (&problem.location, tab) {
            (_, MenuTab::Exit | MenuTab::Help) => false,
            (
                ConfigProblemLocation::Settings | ConfigProblemLocation::Setting(_),
                MenuTab::Settings,
            ) => true,
            (ConfigProblemLocation::Tasks | ConfigProblemLocation::Task { .. }, MenuTab::Tasks) => {
                true
            }
            (ConfigProblemLocation::Root, _) => false,
            _ => false,
        };
        if belongs {
            badges.add(problem.severity);
        }
    }
    badges
}

fn task_problem_badges(problems: &[ConfigProblem], task_index: usize) -> ProblemBadges {
    let mut badges = ProblemBadges::default();
    for problem in problems {
        if let ConfigProblemLocation::Task { index, .. } = &problem.location
            && *index == task_index
        {
            badges.add(problem.severity);
        }
    }
    badges
}

fn task_field_problem_badges(
    problems: &[ConfigProblem],
    task_index: usize,
    field: TaskField,
) -> ProblemBadges {
    let Some(config_field) = task_field_to_config_field(field) else {
        return ProblemBadges::default();
    };
    let mut badges = ProblemBadges::default();
    for problem in problems {
        if let ConfigProblemLocation::Task {
            index,
            field: Some(problem_field),
        } = &problem.location
            && *index == task_index
            && *problem_field == config_field
        {
            badges.add(problem.severity);
        }
    }
    badges
}

fn setting_problem_badges(problems: &[ConfigProblem], field: ConfigSettingField) -> ProblemBadges {
    let mut badges = ProblemBadges::default();
    for problem in problems {
        if let ConfigProblemLocation::Setting(problem_field) = &problem.location
            && *problem_field == field
        {
            badges.add(problem.severity);
        }
    }
    badges
}

fn problem_badge_width(badges: ProblemBadges) -> usize {
    usize::from(badges.warning) + usize::from(badges.error)
}

fn problem_color(severity: ConfigProblemSeverity) -> Color {
    match severity {
        ConfigProblemSeverity::Error => THEME_RED_HOVER,
        ConfigProblemSeverity::Warning => THEME_GOLD_HOVER,
    }
}

fn problem_line(problem: &ConfigProblem) -> String {
    let prefix = match problem.severity {
        ConfigProblemSeverity::Error => "red",
        ConfigProblemSeverity::Warning => "gold",
    };
    format!(
        "{prefix} !  {}: {}",
        problem_location_label(&problem.location),
        problem.message
    )
}

fn problem_location_label(location: &ConfigProblemLocation) -> String {
    match location {
        ConfigProblemLocation::Root => "Config".to_owned(),
        ConfigProblemLocation::Settings => "Settings".to_owned(),
        ConfigProblemLocation::Setting(field) => {
            format!("Settings > {}", setting_field_label(*field))
        }
        ConfigProblemLocation::Tasks => "Tasks".to_owned(),
        ConfigProblemLocation::Task { index, field } => match field {
            Some(field) => format!("Task #{} > {}", index + 1, config_task_field_label(*field)),
            None => format!("Task #{}", index + 1),
        },
    }
}

fn task_list_cursor(menu: &MenuState) -> usize {
    menu.task_list_cursor.min(menu.draft.tasks.len())
}

fn env_item_count(menu: &MenuState) -> usize {
    if menu.env_detail_key.is_some() {
        4
    } else {
        menu.env_task
            .and_then(|task| menu.draft.tasks.get(task))
            .map(|task| task.env.len() + 2)
            .unwrap_or(0)
    }
}

fn selected_env_action(menu: &MenuState) -> Option<MenuAction> {
    if menu.env_detail_key.is_some() {
        return match menu.env_cursor {
            0 => Some(MenuAction::EnvField(EnvField::Key)),
            1 => Some(MenuAction::EnvField(EnvField::Value)),
            2 => Some(MenuAction::DeleteEnvVar),
            3 => Some(MenuAction::BackEnv),
            _ => None,
        };
    }
    let task = menu.env_task?;
    let env_len = menu.draft.tasks.get(task)?.env.len();
    match menu.env_cursor {
        0 => Some(MenuAction::AddEnvVar),
        cursor if cursor <= env_len => Some(MenuAction::OpenEnvEntry(cursor - 1)),
        cursor if cursor == env_len + 1 => Some(MenuAction::BackEnv),
        _ => None,
    }
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
        TaskField::Env => format!("Environment: {}", env_summary(&task.env)),
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

fn menu_edit_title(edit: &MenuEdit) -> &'static str {
    match &edit.target {
        MenuEditTarget::TaskField { field, .. } => task_field_name(*field),
        MenuEditTarget::EnvKey { .. } => "environment key",
        MenuEditTarget::EnvValue { .. } => "environment value",
    }
}

fn task_field_to_config_field(field: TaskField) -> Option<ConfigTaskField> {
    match field {
        TaskField::Name => Some(ConfigTaskField::Name),
        TaskField::Command => Some(ConfigTaskField::Command),
        TaskField::Cwd => Some(ConfigTaskField::Cwd),
        TaskField::Env => Some(ConfigTaskField::Env),
        TaskField::Dependencies => Some(ConfigTaskField::Dependencies),
        TaskField::StartDelay => Some(ConfigTaskField::StartDelay),
        TaskField::Delete | TaskField::Back => None,
    }
}

fn config_task_field_cursor(field: ConfigTaskField) -> Option<usize> {
    task_detail_fields()
        .iter()
        .position(|candidate| task_field_to_config_field(*candidate) == Some(field))
}

fn setting_cursor(field: ConfigSettingField) -> usize {
    match field {
        ConfigSettingField::Layout => 0,
        ConfigSettingField::Leader => 1,
        ConfigSettingField::MultiClick => 2,
        ConfigSettingField::Logging => 3,
    }
}

fn setting_field_label(field: ConfigSettingField) -> &'static str {
    match field {
        ConfigSettingField::Layout => "layout",
        ConfigSettingField::Leader => "leader",
        ConfigSettingField::MultiClick => "multi-click timing",
        ConfigSettingField::Logging => "logging",
    }
}

fn config_task_field_label(field: ConfigTaskField) -> &'static str {
    match field {
        ConfigTaskField::Name => "name",
        ConfigTaskField::Command => "command",
        ConfigTaskField::Cwd => "working directory",
        ConfigTaskField::Env => "environment",
        ConfigTaskField::Dependencies => "dependencies",
        ConfigTaskField::StartDelay => "start delay",
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

fn slider_bar(value: u64, min: u64, max: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 || max <= min {
        return "|".to_owned();
    }
    let clamped = value.clamp(min, max);
    let position = ((clamped - min) as usize * (width - 1)) / (max - min) as usize;
    (0..width)
        .map(|index| if index == position { '|' } else { '-' })
        .collect()
}

fn multi_click_slider_width() -> usize {
    ((MAX_MULTI_CLICK_MS - MIN_MULTI_CLICK_MS) / MULTI_CLICK_STEP_MS) as usize + 1
}

fn multi_click_value_for_slider_index(index: usize, width: usize) -> u64 {
    if width <= 1 {
        return MIN_MULTI_CLICK_MS;
    }
    let steps = ((MAX_MULTI_CLICK_MS - MIN_MULTI_CLICK_MS) / MULTI_CLICK_STEP_MS) as usize;
    let step_index = (index.min(width - 1) * steps + (width - 1) / 2) / (width - 1);
    MIN_MULTI_CLICK_MS + step_index as u64 * MULTI_CLICK_STEP_MS
}

fn rounded_multi_click_ms(value: u64) -> u64 {
    let clamped = value.clamp(MIN_MULTI_CLICK_MS, MAX_MULTI_CLICK_MS);
    let offset = clamped - MIN_MULTI_CLICK_MS;
    let rounded_steps = (offset + MULTI_CLICK_STEP_MS / 2) / MULTI_CLICK_STEP_MS;
    (MIN_MULTI_CLICK_MS + rounded_steps * MULTI_CLICK_STEP_MS).min(MAX_MULTI_CLICK_MS)
}

fn menu_exit_mode(
    quit_when_menu_closes: bool,
    start_after_config_save: bool,
    tasks_started: bool,
) -> MenuExitMode {
    if quit_when_menu_closes {
        MenuExitMode::ConfigureOnly
    } else if start_after_config_save && !tasks_started {
        MenuExitMode::StartAfterSave
    } else {
        MenuExitMode::Runtime
    }
}

fn exit_actions(mode: MenuExitMode) -> &'static [MenuExitAction] {
    match mode {
        MenuExitMode::ConfigureOnly | MenuExitMode::StartAfterSave => &[
            MenuExitAction::SaveOnly,
            MenuExitAction::Discard,
            MenuExitAction::Close,
        ],
        MenuExitMode::Runtime => &[
            MenuExitAction::SaveAffected,
            MenuExitAction::SaveAll,
            MenuExitAction::SaveOnly,
            MenuExitAction::Discard,
            MenuExitAction::Close,
        ],
    }
}

fn exit_action_label(action: MenuExitAction, mode: MenuExitMode) -> &'static str {
    match (action, mode) {
        (MenuExitAction::SaveOnly, MenuExitMode::ConfigureOnly) => "Save config and close",
        (MenuExitAction::SaveOnly, MenuExitMode::StartAfterSave) => "Save config and start tasks",
        (MenuExitAction::SaveOnly, MenuExitMode::Runtime) => "Save without restarting",
        (MenuExitAction::SaveAffected, _) => "Save and restart affected",
        (MenuExitAction::SaveAll, _) => "Save and restart all",
        (MenuExitAction::Discard, MenuExitMode::ConfigureOnly) => "Discard and close",
        (MenuExitAction::Discard, _) => "Discard changes",
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

fn env_summary(env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return "(none)".to_owned();
    }
    let keys = env.keys().take(3).cloned().collect::<Vec<_>>();
    let remaining = env.len().saturating_sub(keys.len());
    if remaining == 0 {
        keys.join(", ")
    } else {
        format!("{} +{remaining} more", keys.join(", "))
    }
}

fn display_env_value(value: &str) -> &str {
    if value.is_empty() { "(empty)" } else { value }
}

fn env_keys(task: &Task) -> Vec<String> {
    task.env.keys().cloned().collect()
}

fn unique_env_key(env: &BTreeMap<String, String>) -> String {
    if !env.contains_key("NEW_VAR") {
        return "NEW_VAR".to_owned();
    }
    for number in 2..1000 {
        let key = format!("NEW_VAR_{number}");
        if !env.contains_key(&key) {
            return key;
        }
    }
    format!("NEW_VAR_{}", env.len() + 1)
}

fn validate_env_key(key: &str) -> Result<()> {
    if key.is_empty() {
        anyhow::bail!("environment key cannot be empty");
    }
    if key.contains(['=', '\0']) || key.contains(char::is_whitespace) {
        anyhow::bail!("environment key {key:?} cannot contain whitespace, '=', or NUL");
    }
    Ok(())
}

fn validate_env_value(value: &str) -> Result<()> {
    if value.contains('\0') {
        anyhow::bail!("environment values cannot contain NUL");
    }
    Ok(())
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

fn reserved_scene_rect(content: Rect) -> Option<Rect> {
    if content.width < 18 {
        return None;
    }
    let max_scene_height = content.height.saturating_sub(3);
    if max_scene_height < 4 {
        return None;
    }
    let preferred = ((u32::from(content.height) * 3).div_ceil(4)) as u16;
    let height = preferred.clamp(4, max_scene_height);
    Some(Rect::new(
        content.x,
        content.bottom().saturating_sub(height),
        content.width,
        height,
    ))
}

fn render_scene_slot(area: Rect, scene: SceneState, frame: u64, buffer: &mut Buffer) {
    clear_rect(buffer, area, app_style());
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(pane_style().fg(THEME_HOLLY))
        .style(pane_style())
        .title(Line::styled(
            format!(" {THEME_ACCENT_MARK} "),
            pane_style().fg(THEME_GOLD_HOVER),
        ))
        .render(area, buffer);
    render_scene(inset_rect(area, 1, 1), scene, frame, buffer);
}

fn render_scene(area: Rect, scene: SceneState, frame: u64, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear_rect(buffer, area, pane_style());
    match scene.kind {
        SceneKind::Fireplace => render_fireplace(area, scene.seed, frame, buffer),
        SceneKind::Snow => render_snow_scene(area, scene.seed, frame / 2, buffer),
        SceneKind::Tree => render_tree_scene(area, scene.seed, frame / 2, buffer),
        SceneKind::Santa => render_santa_scene(area, frame / 2, buffer),
        SceneKind::Jack => render_jack_scene(area, scene.seed, frame, buffer),
        SceneKind::Skating => render_skating_scene(area, scene.seed, frame, buffer),
        SceneKind::Sleigh => render_sleigh_scene(area, scene.seed, frame, buffer),
    }
}

fn render_fireplace(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Fireplace, area) {
        return;
    }
    let log_width = ((u32::from(area.width) * 2) / 5)
        .clamp(10, 30)
        .min(u32::from(area.width.saturating_sub(4))) as u16;
    let log_height = if area.height >= 6 { 2 } else { 1 };
    let fire = FireGeometry {
        log_x: area.x + area.width.saturating_sub(log_width) / 2,
        log_y: area.bottom().saturating_sub(log_height),
        log_width,
        flame_height: area
            .bottom()
            .saturating_sub(log_height)
            .saturating_sub(area.y)
            .clamp(1, 4),
    };

    render_log(buffer, fire.log_x, fire.log_y, fire.log_width, log_height);
    render_fire(buffer, area, seed, frame, fire);
    render_log_nub(buffer, fire.log_x, fire.log_y, fire.log_width);
}

fn render_snow_scene(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Snow, area) {
        return;
    }
    let ground_y = area.bottom().saturating_sub(1);
    let sky_height = ground_y.saturating_sub(area.y).max(1);
    let snowman = snowman_rect(area, ground_y);
    let flakes = usize::from((area.width / 4).clamp(4, 24));
    for index in 0..flakes {
        let value = mix_scene_seed(seed, index as u64, 0x5107_u64);
        let fall = (value / 17).wrapping_add(frame) % u64::from(sky_height);
        let drift = ((frame / 2 + index as u64) % 3) as i16 - 1;
        let slot_x = ((index as u64 * u64::from(area.width)) / flakes as u64) as i16;
        let slot_width = (usize::from(area.width) / flakes).max(1) as i16;
        let jitter = (value % slot_width as u64) as i16;
        let base_x = slot_x + jitter;
        let x =
            area.x + ((base_x + drift).rem_euclid(i16::try_from(area.width).unwrap_or(1))) as u16;
        let y = area.y + fall as u16;
        if snowman.is_some_and(|snowman| contains(snowman, x, y)) {
            continue;
        }
        let symbol = if value & 1 == 0 { "·" } else { "❄" };
        buffer[(x, y)]
            .set_symbol(symbol)
            .set_style(pane_style().fg(THEME_SNOW));
    }

    for column in 0..area.width {
        buffer[(area.x + column, ground_y)]
            .set_symbol(" ")
            .set_style(Style::default().fg(THEME_BLACK).bg(THEME_SNOW));
    }
    for column in 0..area.width {
        buffer[(area.x + column, ground_y.saturating_sub(1))]
            .set_symbol("▁")
            .set_style(pane_style().fg(THEME_SNOW));
    }

    if area.width >= 18 && area.height >= 5 {
        render_snowman(area, ground_y, buffer);
    }
}

fn render_tree_scene(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Tree, area) {
        return;
    }

    let ground_y = area.bottom().saturating_sub(1);
    render_snow_ground(area, ground_y, buffer);

    let trunk_y = ground_y.saturating_sub(1);
    let tree_rows = area.height.saturating_sub(3).clamp(4, 10);
    let tree_top = trunk_y.saturating_sub(tree_rows);
    let center = area.x + area.width / 2;
    let max_half_width = ((area.width.saturating_sub(3)) / 2).min((tree_rows * 2).clamp(5, 12));

    paint_scene_cell(
        buffer,
        i32::from(center),
        tree_top.saturating_sub(1),
        "✦",
        pane_style()
            .fg(THEME_GOLD_HOVER)
            .add_modifier(Modifier::BOLD),
    );

    for row in 0..tree_rows {
        let y = tree_top + row;
        if y < area.y || y >= trunk_y {
            continue;
        }
        let half_width = (((u32::from(row) + 1) * u32::from(max_half_width)) / u32::from(tree_rows))
            .max(1) as u16;
        for offset in -(i32::from(half_width))..=i32::from(half_width) {
            let x = i32::from(center) + offset;
            let leaf_seed = mix_scene_seed(seed, row.into(), offset as u64);
            let green = if leaf_seed & 1 == 0 {
                THEME_GREEN_HOVER
            } else {
                THEME_GREEN
            };
            paint_scene_cell(
                buffer,
                x,
                y,
                "█",
                pane_style().fg(green).add_modifier(Modifier::BOLD),
            );

            if should_draw_tree_light(row, offset, leaf_seed) {
                let lit = ((frame + leaf_seed % 4) % 4) < 2;
                let (symbol, color) = tree_light(leaf_seed, lit);
                paint_scene_cell(
                    buffer,
                    x,
                    y,
                    symbol,
                    Style::default()
                        .fg(color)
                        .bg(green)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
    }

    for offset in -1..=1 {
        paint_scene_cell(
            buffer,
            i32::from(center) + offset,
            trunk_y,
            " ",
            Style::default().fg(THEME_LOG_DARK).bg(THEME_LOG),
        );
    }

    render_tree_presents(area, center, ground_y, buffer);
}

fn render_snow_ground(area: Rect, ground_y: u16, buffer: &mut Buffer) {
    for column in 0..area.width {
        buffer[(area.x + column, ground_y)]
            .set_symbol(" ")
            .set_style(Style::default().fg(THEME_BLACK).bg(THEME_SNOW));
    }
    for column in 0..area.width {
        buffer[(area.x + column, ground_y.saturating_sub(1))]
            .set_symbol("▁")
            .set_style(pane_style().fg(THEME_SNOW));
    }
}

fn should_draw_tree_light(row: u16, offset: i32, seed: u64) -> bool {
    row > 0 && (offset + i32::from(row)).rem_euclid(3) == 0 && seed.is_multiple_of(2)
}

fn tree_light(seed: u64, lit: bool) -> (&'static str, Color) {
    if !lit {
        return ("•", THEME_SNOW);
    }
    match seed % 3 {
        0 => ("●", THEME_RED_HOVER),
        1 => ("◆", THEME_GOLD_HOVER),
        _ => ("•", THEME_SNOW),
    }
}

fn render_tree_presents(area: Rect, center: u16, ground_y: u16, buffer: &mut Buffer) {
    if area.width < 24 || area.height < 8 || ground_y <= area.y {
        return;
    }
    let y = ground_y.saturating_sub(1);
    let left_x = center.saturating_sub(8);
    let right_x = center.saturating_add(5);
    render_present(buffer, left_x, y, THEME_RED_HOVER);
    render_present(buffer, right_x, y, THEME_GOLD_HOVER);
}

fn render_present(buffer: &mut Buffer, x: u16, y: u16, color: Color) {
    let ribbon = Style::default().fg(THEME_SNOW).bg(color);
    let wrap = Style::default().fg(color).bg(color);
    for offset in 0..3 {
        paint_scene_cell(buffer, i32::from(x + offset), y, " ", wrap);
    }
    paint_scene_cell(buffer, i32::from(x + 1), y, "╋", ribbon);
}

fn render_santa_scene(area: Rect, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Santa, area) {
        return;
    }

    let roof_y = area.bottom().saturating_sub(3);
    render_santa_sky(area, roof_y, frame, buffer);
    render_rooftop(area, roof_y, buffer);

    let chimney_width = 11;
    let chimney_x = area.x + area.width.saturating_sub(chimney_width) / 2;
    let chimney_top = roof_y.saturating_sub(4);
    render_santa(
        buffer,
        i32::from(chimney_x + chimney_width / 2),
        chimney_top,
        frame,
    );
    render_chimney(buffer, chimney_x, chimney_width, chimney_top, roof_y);
}

fn render_santa_sky(area: Rect, roof_y: u16, frame: u64, buffer: &mut Buffer) {
    let sky_height = roof_y.saturating_sub(area.y);
    if sky_height < 5 {
        return;
    }

    let flakes = usize::from((area.width / 16).clamp(2, 6));
    for index in 0..flakes {
        let x = area.x + ((index as u16 * 17 + 5) % area.width);
        let y = area.y + 1 + ((index as u16 * 5 + 2) % sky_height.saturating_sub(1));
        let symbol = if (frame + index as u64).is_multiple_of(2) {
            "*"
        } else {
            "·"
        };
        let color = if index % 3 == 0 {
            THEME_GOLD_HOVER
        } else {
            THEME_SNOW
        };
        paint_scene_cell(buffer, i32::from(x), y, symbol, pane_style().fg(color));
    }
}

fn render_rooftop(area: Rect, roof_y: u16, buffer: &mut Buffer) {
    let snow_pattern = b"#___######____####_______######___";
    for column in 0..area.width {
        let x = area.x + column;
        let snow = snow_pattern
            .get(usize::from(column) % snow_pattern.len())
            .copied()
            .unwrap_or(b'_');
        let snow = if snow == b'#' { "█" } else { "▄" };
        paint_scene_cell(
            buffer,
            i32::from(x),
            roof_y,
            snow,
            pane_style().fg(THEME_SNOW),
        );

        if roof_y + 1 < area.bottom() {
            let color = if column % 2 == 0 {
                THEME_RED
            } else {
                THEME_RED_HOVER
            };
            paint_scene_cell(
                buffer,
                i32::from(x),
                roof_y + 1,
                " ",
                Style::default().fg(color).bg(color),
            );
        }
        if roof_y + 2 < area.bottom() {
            let color = if column % 4 < 2 {
                THEME_RED
            } else {
                THEME_LOG_DARK
            };
            paint_scene_cell(
                buffer,
                i32::from(x),
                roof_y + 2,
                "▄",
                Style::default().fg(color).bg(THEME_RED),
            );
        }
    }
}

fn render_chimney(buffer: &mut Buffer, x: u16, width: u16, top: u16, roof_y: u16) {
    let cap_style = Style::default().fg(THEME_SNOW).bg(THEME_LOG);
    for offset in 0..width.saturating_add(6) {
        paint_scene_cell(
            buffer,
            i32::from(x + offset).saturating_sub(3),
            top,
            "▀",
            cap_style,
        );
    }

    for y in top.saturating_add(1)..=roof_y.saturating_add(1) {
        let row = y.saturating_sub(top);
        let base = if row.is_multiple_of(2) {
            THEME_LOG
        } else {
            THEME_LOG_DARK
        };
        for offset in 0..width {
            paint_scene_cell(
                buffer,
                i32::from(x + offset),
                y,
                " ",
                Style::default().fg(base).bg(base),
            );
        }
        let mortar = Style::default().fg(THEME_BLACK).bg(base);
        let stagger = if row.is_multiple_of(2) { 2 } else { 4 };
        for divider in (stagger..usize::from(width)).step_by(5) {
            paint_scene_cell(buffer, i32::from(x) + divider as i32, y, "▏", mortar);
        }
    }
}

fn render_santa(buffer: &mut Buffer, center: i32, chimney_top: u16, frame: u64) {
    let top = i32::from(chimney_top).saturating_sub(7);
    let left = center - 13;
    let hand_out = frame.is_multiple_of(2);
    let red = pane_style()
        .fg(THEME_RED_HOVER)
        .add_modifier(Modifier::BOLD);
    let snow = pane_style().fg(THEME_SNOW).add_modifier(Modifier::BOLD);
    let gold = pane_style()
        .fg(THEME_GOLD_HOVER)
        .add_modifier(Modifier::BOLD);
    let face = Style::default()
        .fg(THEME_BLACK)
        .bg(THEME_SKIN)
        .add_modifier(Modifier::BOLD);
    let brim_on_face = Style::default()
        .fg(THEME_SNOW)
        .bg(THEME_SKIN)
        .add_modifier(Modifier::BOLD);

    render_scene_text_clipped(buffer, left + 6, top, "◢█████████◣", red);
    render_scene_text_clipped(buffer, left + 17, top, "●", snow);
    render_scene_text_clipped(buffer, left + 4, top + 1, "▔▔▔", snow);
    render_scene_text_clipped(buffer, left + 7, top + 1, "▔▔▔▔▔▔▔", brim_on_face);
    render_scene_text_clipped(buffer, left + 14, top + 1, "▔▔▔▔", snow);
    render_scene_text_clipped(buffer, left + 7, top + 2, " ●   ● ", face);
    render_scene_text_clipped(buffer, left + 6, top + 3, "╭  ‿  ╮", face);
    render_scene_text_clipped(buffer, left + 5, top + 3, "╭", snow);
    render_scene_text_clipped(buffer, left + 14, top + 3, "╮", snow);
    render_scene_text_clipped(buffer, left + 5, top + 4, "╭████████╮", snow);
    render_scene_text_clipped(buffer, left + 3, top + 5, "╭╯████████╰╮", snow);
    render_scene_text_clipped(buffer, left + 1, top + 6, "╭████████████████╮", red);
    render_scene_text_clipped(buffer, left + 9, top + 6, "╋", gold);
    render_scene_text_clipped(buffer, left + 10, top + 6, "╋", gold);
    render_scene_text_clipped(buffer, left + 4, top + 7, "██████████████", red);

    render_scene_text_clipped(buffer, left + 18, top + 5, "◢◤", red);
    render_scene_text_clipped(buffer, left + 18, top + 6, "◤", red);
    if hand_out {
        render_scene_text_clipped(buffer, left + 21, top + 3, "●", snow);
        render_scene_text_clipped(buffer, left + 19, top + 4, "◢◤", red);
    } else {
        render_scene_text_clipped(buffer, left + 19, top + 3, "●", snow);
        render_scene_text_clipped(buffer, left + 19, top + 4, "█", red);
    }
}

fn render_skating_scene(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Skating, area) {
        return;
    }

    let lake_height = if area.height >= 18 {
        ((u32::from(area.height) * 2) / 5).clamp(7, 10) as u16
    } else if area.height >= 13 {
        6
    } else if area.height >= 10 {
        5
    } else {
        4
    };
    let lake_y = area.bottom().saturating_sub(lake_height);
    let snowbank_y = lake_y.saturating_sub(1);

    render_skating_sky(area, snowbank_y, seed, frame, buffer);
    render_skating_snow(area, snowbank_y, buffer);
    render_skating_pines(area, snowbank_y, seed, buffer);
    render_frozen_lake(area, lake_y, seed, buffer);
    render_skating_tracks(area, lake_y, seed, buffer);
    render_skaters(area, lake_y, seed, frame, buffer);
}

fn render_skating_sky(area: Rect, snowbank_y: u16, seed: u64, frame: u64, buffer: &mut Buffer) {
    if snowbank_y <= area.y + 1 {
        return;
    }

    let sky_height = snowbank_y.saturating_sub(area.y);
    let flakes = usize::from((area.width / 12).clamp(3, 12));
    for index in 0..flakes {
        let value = mix_scene_seed(seed, index as u64, 0x5e7a_51de_u64);
        let slot_x = ((index as u64 * u64::from(area.width)) / flakes as u64) as i16;
        let drift = ((frame / 3 + value / 13) % 3) as i16 - 1;
        let x = area.x + (slot_x + drift).rem_euclid(i16::try_from(area.width).unwrap_or(1)) as u16;
        let y = area.y + ((value / 29 + frame / 2) % u64::from(sky_height)) as u16;
        let symbol = if value & 1 == 0 { "·" } else { "*" };
        paint_scene_cell(buffer, i32::from(x), y, symbol, pane_style().fg(THEME_SNOW));
    }
}

fn render_skating_snow(area: Rect, y: u16, buffer: &mut Buffer) {
    if y < area.y {
        return;
    }
    for column in 0..area.width {
        let x = area.x + column;
        let symbol = SKATING_SNOWBANK_PATTERN
            .get(usize::from(column) % SKATING_SNOWBANK_PATTERN.len())
            .copied()
            .unwrap_or(b'_');
        let symbol = if symbol == b'#' { "█" } else { "▄" };
        paint_scene_cell(buffer, i32::from(x), y, symbol, pane_style().fg(THEME_SNOW));
    }
}

fn render_skating_pines(area: Rect, snowbank_y: u16, seed: u64, buffer: &mut Buffer) {
    if area.height < 11 || snowbank_y <= area.y + 3 {
        return;
    }

    let count = usize::from((area.width / 18).clamp(2, 5));
    for index in 0..count {
        let value = mix_scene_seed(seed, index as u64, 0x0051_ca7e_u64);
        let slot = u64::from(area.width) / count as u64;
        let x =
            area.x + ((index as u64 * slot + value % slot.max(1)) % u64::from(area.width)) as u16;
        let height = 2 + (value % 2) as u16;
        let y = snowbank_y.saturating_sub(height);
        if y <= area.y {
            continue;
        }
        render_scene_text_clipped(
            buffer,
            i32::from(x).saturating_sub(1),
            i32::from(y),
            "▲",
            pane_style().fg(THEME_GREEN_HOVER),
        );
        if height >= 3 {
            render_scene_text_clipped(
                buffer,
                i32::from(x).saturating_sub(2),
                i32::from(y + 1),
                "▲▲▲",
                pane_style().fg(THEME_GREEN),
            );
        }
        render_scene_text_clipped(
            buffer,
            i32::from(x).saturating_sub(1),
            i32::from(y + height.saturating_sub(1)),
            "▐▌",
            pane_style().fg(THEME_LOG),
        );
    }
}

fn render_frozen_lake(area: Rect, lake_y: u16, seed: u64, buffer: &mut Buffer) {
    for y in lake_y.saturating_add(1)..area.bottom() {
        let snow_width = skating_snow_width(area, lake_y, y);
        for column in 0..area.width {
            let x = area.x + column;
            let row = y.saturating_sub(lake_y);
            let value = mix_scene_seed(
                seed,
                u64::from(row) << 32 | u64::from(column),
                0x1ce_5ca7e_u64,
            );
            let diagonal = (u64::from(column) * 3 + u64::from(row) + seed % 17) % 23 == 0;
            let ice = if row > 0 && column > snow_width && diagonal && value % 5 < 2 {
                THEME_ICE
            } else {
                THEME_ICE_DARK
            };
            if row > 0 && column < snow_width {
                paint_scene_cell(
                    buffer,
                    i32::from(x),
                    y,
                    " ",
                    Style::default().fg(THEME_BLACK).bg(THEME_SNOW),
                );
            } else if row > 0 && column == snow_width && snow_width > 0 {
                paint_scene_cell(
                    buffer,
                    i32::from(x),
                    y,
                    "▌",
                    Style::default().fg(THEME_SNOW).bg(ice),
                );
            } else {
                paint_scene_cell(
                    buffer,
                    i32::from(x),
                    y,
                    " ",
                    Style::default().fg(THEME_BLACK).bg(ice),
                );
            }
        }
    }

    let top_snow_width = area.width / 4;
    for column in 0..area.width {
        let x = area.x + column;
        if column < top_snow_width {
            paint_scene_cell(
                buffer,
                i32::from(x),
                lake_y,
                " ",
                Style::default().fg(THEME_BLACK).bg(THEME_SNOW),
            );
        } else if column == top_snow_width && top_snow_width > 0 {
            paint_scene_cell(
                buffer,
                i32::from(x),
                lake_y,
                "▌",
                Style::default().fg(THEME_SNOW).bg(THEME_ICE_DARK),
            );
        } else {
            paint_scene_cell(
                buffer,
                i32::from(x),
                lake_y,
                " ",
                Style::default().fg(THEME_BLACK).bg(THEME_ICE_DARK),
            );
        }
    }
}

fn skating_snow_width(area: Rect, lake_y: u16, y: u16) -> u16 {
    if y <= lake_y {
        return 0;
    }
    let max_width = area.width / 4;
    if max_width == 0 {
        return 0;
    }
    let lake_rows = area
        .bottom()
        .saturating_sub(lake_y)
        .saturating_sub(1)
        .max(1);
    let row = y.saturating_sub(lake_y).min(lake_rows);
    let curve_rows = lake_rows.saturating_sub(1).max(1);
    let curve_row = row.saturating_sub(1).min(curve_rows);
    let midpoint = u32::from(curve_rows);
    let distance_from_middle = u32::from(curve_row.saturating_mul(2)).abs_diff(midpoint);
    let max_width = u32::from(max_width);
    let curved = 1 + distance_from_middle
        .saturating_mul(distance_from_middle)
        .saturating_mul(max_width.saturating_sub(1))
        / midpoint.saturating_mul(midpoint).max(1);
    let curved = if row == 1 && lake_rows > 2 {
        curved.saturating_sub(1)
    } else {
        curved
    };
    u16::try_from(curved).unwrap_or(u16::MAX)
}

fn skating_ice_start(area: Rect) -> u16 {
    let snow_width = area.width / 4;
    area.x
        .saturating_add(snow_width)
        .saturating_add(2)
        .min(area.right().saturating_sub(1))
}

fn render_skating_tracks(area: Rect, lake_y: u16, seed: u64, buffer: &mut Buffer) {
    let available_height = area.bottom().saturating_sub(lake_y);
    let ice_start = skating_ice_start(area);
    let ice_width = area.right().saturating_sub(ice_start);
    if available_height < 3 || ice_width < 12 {
        return;
    }

    let track_count = usize::from((ice_width / 16).clamp(2, 6));
    for index in 0..track_count {
        let value = mix_scene_seed(seed, index as u64, 0x1ced_1a4e_u64);
        let y = lake_y + 1 + (value % u64::from(available_height.saturating_sub(1))) as u16;
        let x = ice_start + (value % u64::from(ice_width.saturating_sub(4))) as u16;
        render_scene_text_clipped(
            buffer,
            i32::from(x),
            i32::from(y),
            "·",
            Style::default().fg(THEME_ICE),
        );
        if x + 3 < area.right() {
            render_scene_text_clipped(
                buffer,
                i32::from(x + 2),
                i32::from(y),
                "·",
                Style::default().fg(THEME_ICE),
            );
        }
    }
}

fn render_skaters(area: Rect, lake_y: u16, seed: u64, frame: u64, buffer: &mut Buffer) {
    let available_height = area.bottom().saturating_sub(lake_y);
    let ice_start = skating_ice_start(area);
    let ice_width = area.right().saturating_sub(ice_start);
    if available_height < 3 || ice_width < 12 {
        return;
    }

    let first_lane = lake_y.saturating_add(3);
    let last_lane = area.bottom().saturating_sub(1);
    if first_lane > last_lane {
        return;
    }
    let lane_slots = usize::from((last_lane - first_lane) / 2 + 1);
    let width_slots = usize::from((ice_width / 18).clamp(1, 3));
    let skater_count = width_slots.min(lane_slots).max(1);
    let path_span = i32::from(ice_width.saturating_sub(6).max(1));
    let path_period = (path_span * 2).max(1);
    let mut skaters = Vec::with_capacity(skater_count);
    for index in 0..skater_count {
        let value = mix_scene_seed(seed, index as u64, 0x5ca7_1ace_u64);
        let foot_y = first_lane + index as u16 * 2;
        let phase = frame + index as u64;
        let spacing = path_period / i32::try_from(skater_count).unwrap_or(1).max(1);
        let drift = (value % 5) as i32 - 2;
        let offset = index as i32 * spacing + drift;
        let progress = ((frame as i32 + offset).rem_euclid(path_period)).min(path_period);
        let local_x = if progress <= path_span {
            progress
        } else {
            path_period - progress
        };
        let x = i32::from(ice_start) + 1 + local_x;
        let direction = if progress <= path_span { 1 } else { -1 };
        skaters.push((foot_y, x, phase, direction));
    }

    skaters.sort_by_key(|(foot_y, ..)| *foot_y);
    for (foot_y, x, phase, direction) in skaters {
        render_skater(buffer, x, foot_y, phase, direction);
    }
}

fn render_skater(buffer: &mut Buffer, x: i32, foot_y: u16, phase: u64, direction: i32) {
    let head_y = i32::from(foot_y).saturating_sub(2);
    let body_y = i32::from(foot_y).saturating_sub(1);
    let foot_y = i32::from(foot_y);
    let red = Style::default()
        .fg(THEME_RED_HOVER)
        .add_modifier(Modifier::BOLD);
    let gold = Style::default()
        .fg(THEME_GOLD_HOVER)
        .add_modifier(Modifier::BOLD);
    let snow = Style::default().fg(THEME_SNOW).add_modifier(Modifier::BOLD);

    let scarf_x = if direction >= 0 { x - 1 } else { x + 3 };
    render_scene_text_clipped(buffer, scarf_x, body_y, "~", red);
    let body = if direction >= 0 { "/█>" } else { "<█\\" };
    render_scene_text_clipped(buffer, x + 1, head_y, "o", snow);
    render_scene_text_clipped(buffer, x, body_y, body, gold);

    let (leg_x, stride) = if phase.is_multiple_of(2) {
        (x + 1, "║")
    } else if direction >= 0 {
        (x, "/|")
    } else {
        (x + 1, "|\\")
    };
    render_scene_text_clipped(
        buffer,
        leg_x,
        foot_y,
        stride,
        Style::default().fg(THEME_SNOW),
    );
}

fn render_sleigh_scene(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Sleigh, area) {
        return;
    }

    render_sleigh_sky(area, seed, frame, buffer);
    render_sleigh_moon(area, buffer);
    render_sleigh_team(area, seed, frame, buffer);
}

fn render_sleigh_sky(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    let stars = usize::from((area.width / 7).clamp(5, 18));
    let width = i16::try_from(area.width).unwrap_or(1).max(1);
    for index in 0..stars {
        let value = mix_scene_seed(seed, index as u64, 0x51e1_6a00_u64);
        let slot_x = ((index as u64 * u64::from(area.width)) / stars as u64) as i16;
        let jitter = (value % 5) as i16 - 2;
        let x = area.x + (slot_x + jitter).rem_euclid(width) as u16;
        let y = area.y + ((value / 23) % u64::from(area.height)) as u16;
        let bright = (frame / 2 + value).is_multiple_of(4);
        let symbol = if bright { "✦" } else { "·" };
        let color = if bright { THEME_GOLD_HOVER } else { THEME_SNOW };
        paint_scene_cell(buffer, i32::from(x), y, symbol, pane_style().fg(color));
    }
}

fn render_sleigh_moon(area: Rect, buffer: &mut Buffer) {
    if area.width < 40 || area.height < 10 {
        return;
    }

    let x = i32::from(area.right().saturating_sub(9));
    let y = i32::from(area.y + 1);
    let style = pane_style().fg(THEME_GOLD_HOVER);
    render_scene_text_clipped(buffer, x + 1, y, "▄██▄", style);
    render_scene_text_clipped(buffer, x, y + 1, "██████", style);
    render_scene_text_clipped(buffer, x + 1, y + 2, "▀██▀", style);
}

fn render_sleigh_team(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    const SLEIGH_SPRITE_WIDTH: i32 = 34;

    let travel = u64::from(area.width) + SLEIGH_SPRITE_WIDTH as u64;
    let local = ((frame.wrapping_mul(3) + seed % travel) % travel) as i32;
    let wave_amplitude: u16 = if area.height >= 14 { 2 } else { 1 };
    let wave_offset = triangular_wave((local as u64) / 6, u64::from(wave_amplitude * 2 + 1)) as i32
        - i32::from(wave_amplitude);
    let base_top = i32::from(area.y + area.height.saturating_sub(5) / 2);
    let top = (base_top + wave_offset).clamp(
        i32::from(area.y),
        i32::from(area.bottom().saturating_sub(5)),
    ) as u16;
    let left = i32::from(area.right()) - local;

    render_sleigh_sprite(buffer, area, left, top, frame);
}

fn render_sleigh_sprite(buffer: &mut Buffer, clip: Rect, x: i32, top: u16, frame: u64) {
    let y = i32::from(top);
    let rein_style = pane_style().fg(THEME_SNOW);
    let sleigh_style = pane_style()
        .fg(THEME_RED_HOVER)
        .add_modifier(Modifier::BOLD);
    let runner_style = pane_style().fg(THEME_GOLD_HOVER);

    render_scene_text_in_rect(buffer, clip, x + 11, y + 1, "───", rein_style);
    render_scene_text_in_rect(buffer, clip, x + 22, y + 1, "────╮", rein_style);

    render_reindeer(buffer, clip, x + 3, y, true, frame);
    render_reindeer(buffer, clip, x + 14, y, false, frame + 1);

    render_scene_text_in_rect(buffer, clip, x + 27, y + 1, "__◢██◣", runner_style);
    render_scene_text_in_rect(buffer, clip, x + 27, y + 2, "◥████◤", sleigh_style);
}

fn render_reindeer(buffer: &mut Buffer, clip: Rect, x: i32, top: i32, red_nose: bool, frame: u64) {
    let antler_style = pane_style().fg(THEME_GOLD_HOVER);
    let body_style = pane_style().fg(THEME_LOG).add_modifier(Modifier::BOLD);
    let leg_style = pane_style().fg(THEME_LOG);
    let nose_style = pane_style()
        .fg(if red_nose {
            THEME_RED_HOVER
        } else {
            THEME_SNOW
        })
        .add_modifier(Modifier::BOLD);

    render_scene_text_in_rect(buffer, clip, x + 2, top, "Y Y", antler_style);
    render_scene_text_in_rect(
        buffer,
        clip,
        x,
        top + 1,
        if red_nose { "●" } else { "o" },
        nose_style,
    );
    render_scene_text_in_rect(buffer, clip, x + 1, top + 1, "<(•)==", body_style);
    render_scene_text_in_rect(buffer, clip, x + 3, top + 2, "║ ║", leg_style);

    let legs = if frame.is_multiple_of(2) {
        "╱  ╲"
    } else {
        "╲  ╱"
    };
    render_scene_text_in_rect(buffer, clip, x + 2, top + 3, legs, leg_style);
}

fn render_jack_scene(area: Rect, seed: u64, frame: u64, buffer: &mut Buffer) {
    if !scene_fits(SceneKind::Jack, area) {
        return;
    }

    let ground_y = area.bottom().saturating_sub(1);
    render_snow_ground(area, ground_y, buffer);

    let box_width = if area.width >= 36 {
        ((u32::from(area.width) * 3) / 10).clamp(13, 26) as u16
    } else {
        area.width.saturating_sub(8).clamp(9, 13)
    };
    let box_height = if area.height >= 16 {
        4
    } else if area.height >= 9 {
        3
    } else {
        2
    };
    let box_x = area.x + area.width.saturating_sub(box_width) / 2;
    let box_y = ground_y.saturating_sub(box_height);
    let center = box_x + box_width / 2;
    let large = area.width >= 32 && area.height >= 12;

    render_jack_confetti(area, seed, frame, center, box_y, buffer);
    render_jack_box(buffer, box_x, box_y, box_width, box_height, frame);
    render_jack(buffer, i32::from(center), box_y, frame, large);
}

fn render_jack_box(buffer: &mut Buffer, x: u16, y: u16, width: u16, height: u16, frame: u64) {
    let phase = frame % 10;
    let box_style = Style::default().fg(THEME_RED_HOVER).bg(THEME_RED_HOVER);
    let ribbon_style = Style::default().fg(THEME_GOLD_HOVER).bg(THEME_GOLD_HOVER);
    let center = width / 2;

    for row in 0..height {
        for column in 0..width {
            let style = if column == center || row == 0 {
                ribbon_style
            } else {
                box_style
            };
            paint_scene_cell(buffer, i32::from(x + column), y + row, " ", style);
        }
    }

    if phase == 0 || phase >= 8 {
        for column in 0..width {
            let style = if column == center {
                ribbon_style
            } else {
                Style::default().fg(THEME_RED).bg(THEME_RED)
            };
            paint_scene_cell(
                buffer,
                i32::from(x + column),
                y.saturating_sub(1),
                "▀",
                style,
            );
        }
    } else {
        let lid_y = y.saturating_sub(1);
        let left_width = center.saturating_sub(1).max(3);
        let right_width = width.saturating_sub(center + 2).max(3);
        let left_flap = format!("╲{}", "▄".repeat(usize::from(left_width.saturating_sub(1))));
        let right_flap = format!(
            "{}╱",
            "▄".repeat(usize::from(right_width.saturating_sub(1)))
        );
        let left = i32::from(x);
        let right = i32::from(x + width.saturating_sub(right_width));
        render_scene_text_clipped(
            buffer,
            left,
            i32::from(lid_y),
            &left_flap,
            pane_style().fg(THEME_GOLD_HOVER),
        );
        render_scene_text_clipped(
            buffer,
            right,
            i32::from(lid_y),
            &right_flap,
            pane_style().fg(THEME_GOLD_HOVER),
        );
    }
}

fn render_jack_confetti(
    area: Rect,
    seed: u64,
    frame: u64,
    center: u16,
    box_y: u16,
    buffer: &mut Buffer,
) {
    let phase = frame % 10;
    if !(2..=6).contains(&phase) || box_y <= area.y + 2 {
        return;
    }

    let top = area.y + 1;
    let bottom = box_y.saturating_sub(2);
    if bottom <= top {
        return;
    }
    let height = bottom - top + 1;
    let spread = area.width.saturating_sub(4).min((area.width / 2).max(14));
    let left = i32::from(center) - i32::from(spread / 2);
    let count = usize::from((area.width / 7).clamp(6, 22));

    for index in 0..count {
        let value = mix_scene_seed(seed, index as u64, 0x0b0c_51de_u64);
        let x = left
            + i32::try_from(value % u64::from(spread.max(1))).unwrap_or_default()
            + ((frame + index as u64) % 3) as i32
            - 1;
        let y = top + ((value / 17 + frame + index as u64) % u64::from(height)) as u16;
        let symbol = match value % 4 {
            0 => "✦",
            1 => "·",
            2 => "*",
            _ => "•",
        };
        let color = match value % 3 {
            0 => THEME_GOLD_HOVER,
            1 => THEME_RED_HOVER,
            _ => THEME_SNOW,
        };
        paint_scene_cell(buffer, x, y, symbol, pane_style().fg(color));
    }
}

fn render_jack(buffer: &mut Buffer, center: i32, box_y: u16, frame: u64, large: bool) {
    let phase = frame % 10;
    if phase == 0 || phase >= 8 {
        return;
    }

    let lid_y = i32::from(box_y).saturating_sub(1);
    if large {
        render_large_jack(buffer, center, lid_y, phase);
    } else {
        render_small_jack(buffer, center, lid_y, phase);
    }
}

fn render_large_jack(buffer: &mut Buffer, center: i32, lid_y: i32, phase: u64) {
    let left = center - 3;
    let spring_style = pane_style().fg(THEME_GOLD_HOVER);
    if phase == 1 || phase == 7 {
        render_scene_text_clipped(buffer, left, lid_y - 2, "  ╱╲   ", spring_style);
        render_scene_text_clipped(buffer, left, lid_y - 1, "  ╲╱   ", spring_style);
        render_scene_text_clipped(buffer, left, lid_y, "  ╱╲   ", spring_style);
        return;
    }

    let arms = if phase.is_multiple_of(2) {
        "\\ ███ /"
    } else {
        "/ ███ \\"
    };
    let top_shift = if phase == 4 { 1 } else { 0 };
    let red = pane_style()
        .fg(THEME_RED_HOVER)
        .add_modifier(Modifier::BOLD);
    let snow = pane_style().fg(THEME_SNOW).add_modifier(Modifier::BOLD);
    let gold = pane_style()
        .fg(THEME_GOLD_HOVER)
        .add_modifier(Modifier::BOLD);

    render_scene_text_clipped(buffer, left, lid_y - 7 + top_shift, "   ▲   ", gold);
    render_scene_text_clipped(buffer, left, lid_y - 6 + top_shift, "  ▔▔▔  ", snow);
    render_scene_text_clipped(buffer, left, lid_y - 5 + top_shift, " (o o) ", snow);
    render_scene_text_clipped(buffer, left, lid_y - 4 + top_shift, arms, red);
    render_scene_text_clipped(buffer, left, lid_y - 3 + top_shift, "  ███  ", red);
    render_scene_text_clipped(buffer, left, lid_y - 2, "  ╱╲   ", spring_style);
    render_scene_text_clipped(buffer, left, lid_y - 1, "  ╲╱   ", spring_style);
    render_scene_text_clipped(buffer, left, lid_y, "  ╱╲   ", spring_style);
}

fn render_small_jack(buffer: &mut Buffer, center: i32, lid_y: i32, phase: u64) {
    let left = center - 2;
    let spring_style = pane_style().fg(THEME_GOLD_HOVER);
    if phase == 1 || phase == 7 {
        render_scene_text_clipped(buffer, left, lid_y - 1, " ╱╲ ", spring_style);
        render_scene_text_clipped(buffer, left, lid_y, " ╲╱ ", spring_style);
        return;
    }

    let arms = if phase.is_multiple_of(2) {
        "\\ | /"
    } else {
        "/ | \\"
    };
    let red = pane_style()
        .fg(THEME_RED_HOVER)
        .add_modifier(Modifier::BOLD);
    let snow = pane_style().fg(THEME_SNOW).add_modifier(Modifier::BOLD);
    let gold = pane_style()
        .fg(THEME_GOLD_HOVER)
        .add_modifier(Modifier::BOLD);

    render_scene_text_clipped(buffer, left, lid_y - 4, "  ▲  ", gold);
    render_scene_text_clipped(buffer, left, lid_y - 3, " (☺) ", snow);
    render_scene_text_clipped(buffer, left, lid_y - 2, arms, red);
    render_scene_text_clipped(buffer, left, lid_y - 1, " ███ ", red);
    render_scene_text_clipped(buffer, left, lid_y, "╱╲╱╲", spring_style);
}

fn render_fire(buffer: &mut Buffer, area: Rect, seed: u64, frame: u64, fire: FireGeometry) {
    if fire.flame_height == 0 || fire.log_y <= area.y {
        return;
    }

    let ember_y = fire.log_y.saturating_sub(1);
    render_flame_bed(buffer, seed, frame, fire, ember_y);

    let flame_count = (fire.log_width / 5).clamp(3, 6);
    let span = fire.log_width.saturating_sub(4).max(1);
    for index in 0..flame_count {
        let value = mix_scene_seed(seed, u64::from(index), 0xf1a6_u64);
        let phase = frame.wrapping_add(value % 11);
        let wave = triangular_wave(phase, u64::from(fire.flame_height));
        let height = if fire.flame_height >= 2 {
            2 + (wave as u16 % fire.flame_height.saturating_sub(1))
        } else {
            1
        }
        .min(fire.flame_height);
        let center = if flame_count == 1 {
            fire.log_x + fire.log_width / 2
        } else {
            fire.log_x + 2 + (index * span) / flame_count.saturating_sub(1)
        };
        let wobble = (phase % 3) as i32 - 1;
        let center = i32::from(center) + wobble;
        render_flame_tongue(buffer, area, center, ember_y, height, phase);
    }

    if fire.flame_height >= 3 {
        for index in 0..2 {
            let value = mix_scene_seed(seed, frame.wrapping_add(index), 0x5aa7_u64);
            let x = fire.log_x + 2 + (value % u64::from(span)) as u16;
            let y_offset =
                1 + ((value / 13) % u64::from(fire.flame_height.saturating_sub(1))) as u16;
            let y = ember_y.saturating_sub(y_offset);
            paint_scene_cell(buffer, i32::from(x), y, "·", pane_style().fg(THEME_EMBER));
        }
    }
}

fn render_flame_bed(buffer: &mut Buffer, seed: u64, frame: u64, fire: FireGeometry, y: u16) {
    for column in 1..fire.log_width.saturating_sub(1) {
        let value = mix_scene_seed(seed, u64::from(column), frame);
        let symbol = match value % 5 {
            0 => "▆",
            1 => "▄",
            _ => "█",
        };
        let color = if value & 1 == 0 {
            THEME_FLAME
        } else {
            THEME_EMBER
        };
        paint_scene_cell(
            buffer,
            i32::from(fire.log_x + column),
            y,
            symbol,
            pane_style().fg(color).add_modifier(Modifier::BOLD),
        );
    }
}

fn render_flame_tongue(
    buffer: &mut Buffer,
    area: Rect,
    center: i32,
    base_y: u16,
    height: u16,
    phase: u64,
) {
    for level in 0..height {
        let y = base_y.saturating_sub(level);
        if y < area.y || y >= area.bottom() {
            continue;
        }
        let rows_above = height.saturating_sub(level + 1);
        let half_width = rows_above.min(2);
        for offset in -(i32::from(half_width))..=i32::from(half_width) {
            let edge = offset.unsigned_abs() as u16 == half_width;
            let symbol = if rows_above == 0 {
                "▲"
            } else if level == 0 || phase & 1 == 0 || !edge {
                "█"
            } else {
                "▌"
            };
            let color = if rows_above == 0 || offset == 0 {
                THEME_EMBER
            } else {
                THEME_FLAME
            };
            paint_scene_cell(
                buffer,
                center + offset,
                y,
                symbol,
                pane_style().fg(color).add_modifier(Modifier::BOLD),
            );
        }
    }
}

fn triangular_wave(value: u64, max: u64) -> u64 {
    if max <= 1 {
        return 0;
    }
    let period = max * 2 - 2;
    let value = value % period;
    if value < max { value } else { period - value }
}

fn render_log(buffer: &mut Buffer, x: u16, y: u16, width: u16, height: u16) {
    let height = height.max(1);
    for row in 0..height {
        for column in 0..width {
            let style = Style::default().fg(THEME_LOG_DARK).bg(THEME_LOG);
            buffer[(x + column, y + row)]
                .set_symbol(" ")
                .set_style(style);
        }
    }

    if width >= 8 {
        let ring_y = y + height / 2;
        render_scene_text(
            buffer,
            x + 1,
            ring_y,
            "◉",
            Style::default().fg(THEME_EMBER).bg(THEME_LOG),
        );
        render_scene_text(
            buffer,
            x + width.saturating_sub(3),
            ring_y,
            "◌",
            Style::default().fg(THEME_LOG_DARK).bg(THEME_LOG),
        );
    }

    if width >= 12 {
        let stripe_y = y + height.saturating_sub(1);
        for column in 4..width.saturating_sub(4) {
            if column % 3 == 0 {
                buffer[(x + column, stripe_y)]
                    .set_symbol("╱")
                    .set_style(Style::default().fg(THEME_LOG_DARK).bg(THEME_LOG));
            }
        }
    }
}

fn render_log_nub(buffer: &mut Buffer, x: u16, y: u16, width: u16) {
    if width < 10 || y == 0 {
        return;
    }
    let nub_x = x + width.saturating_sub(3);
    render_scene_text(buffer, nub_x, y - 1, "▗●▖", pane_style().fg(THEME_LOG));
}

fn render_snowman(area: Rect, ground_y: u16, buffer: &mut Buffer) {
    let Some(rect) = snowman_rect(area, ground_y) else {
        return;
    };
    let x = rect.x;
    let y = rect.y;
    let rows = rect.height;

    render_scene_text(buffer, x, y, "  _Π_  ", pane_style().fg(THEME_SNOW));
    render_scene_text(buffer, x + 2, y + 1, "(•)", pane_style().fg(THEME_SNOW));
    render_scene_text(buffer, x, y + 1, "\\ ", pane_style().fg(THEME_LOG));
    render_scene_text(buffer, x + 5, y + 1, " /", pane_style().fg(THEME_LOG));
    if rows == 4 {
        render_scene_text(buffer, x, y + 2, " ( : ) ", pane_style().fg(THEME_SNOW));
        render_scene_text(buffer, x, y + 3, " (___) ", pane_style().fg(THEME_SNOW));
    } else {
        render_scene_text(buffer, x, y + 2, " (___) ", pane_style().fg(THEME_SNOW));
    }
}

fn snowman_rect(area: Rect, ground_y: u16) -> Option<Rect> {
    if area.width < 18 || area.height < 5 {
        return None;
    }
    let rows = if area.height >= 7 { 4 } else { 3 };
    let width = 7_u16;
    let x = area.x + area.width.saturating_sub(width + 2);
    let y = ground_y.saturating_sub(rows);
    if y < area.y {
        return None;
    }
    Some(Rect::new(x, y, width, rows))
}

fn paint_scene_cell(buffer: &mut Buffer, x: i32, y: u16, symbol: &str, style: Style) {
    if x < 0 {
        return;
    }
    let x = x as u16;
    if !contains(buffer.area, x, y) {
        return;
    }
    buffer[(x, y)].set_symbol(symbol).set_style(style);
}

fn render_scene_text_clipped(buffer: &mut Buffer, x: i32, y: i32, text: &str, style: Style) {
    if y < 0 {
        return;
    }
    let y = y as u16;
    for (offset, character) in text.chars().enumerate() {
        let cell_x = x + offset as i32;
        if cell_x < 0 {
            continue;
        }
        let cell_x = cell_x as u16;
        if !contains(buffer.area, cell_x, y) {
            continue;
        }
        let mut encoded = [0_u8; 4];
        let symbol = if character.is_control() {
            " "
        } else {
            character.encode_utf8(&mut encoded)
        };
        buffer[(cell_x, y)].set_symbol(symbol).set_style(style);
    }
}

fn render_scene_text_in_rect(
    buffer: &mut Buffer,
    clip: Rect,
    x: i32,
    y: i32,
    text: &str,
    style: Style,
) {
    if y < 0 {
        return;
    }
    let y = y as u16;
    for (offset, character) in text.chars().enumerate() {
        let cell_x = x + offset as i32;
        if cell_x < 0 {
            continue;
        }
        let cell_x = cell_x as u16;
        if !contains(clip, cell_x, y) || !contains(buffer.area, cell_x, y) {
            continue;
        }
        let mut encoded = [0_u8; 4];
        let symbol = if character.is_control() {
            " "
        } else {
            character.encode_utf8(&mut encoded)
        };
        buffer[(cell_x, y)].set_symbol(symbol).set_style(style);
    }
}

fn render_scene_text(buffer: &mut Buffer, x: u16, y: u16, text: &str, style: Style) {
    for (offset, character) in text.chars().enumerate() {
        let cell_x = x.saturating_add(offset as u16);
        if !contains(buffer.area, cell_x, y) {
            continue;
        }
        let mut encoded = [0_u8; 4];
        let symbol = if character.is_control() {
            " "
        } else {
            character.encode_utf8(&mut encoded)
        };
        buffer[(cell_x, y)].set_symbol(symbol).set_style(style);
    }
}

fn render_history(task: &TaskRuntime, area: Rect, buffer: &mut Buffer) {
    let start = task.history.visible_start(area.height, task.scroll_offset);
    clear_rect(buffer, area, pane_style());
    for row in 0..area.height {
        let Some(line) = task.history.line(start.saturating_add(u64::from(row))) else {
            continue;
        };
        for (column, character) in line.text.chars().take(usize::from(area.width)).enumerate() {
            let mut encoded = [0_u8; 4];
            let symbol = if character.is_control() {
                " "
            } else {
                character.encode_utf8(&mut encoded)
            };
            buffer[(area.x + column as u16, area.y + row)]
                .set_symbol(symbol)
                .set_style(pane_style());
        }
    }
}

fn render_screen(parser: &Parser, area: Rect, row_offset: u16, buffer: &mut Buffer) {
    clear_rect(buffer, area, pane_style());
    let screen = parser.screen();
    for row in 0..area.height {
        for column in 0..area.width {
            let Some(source) = screen.cell(row_offset.saturating_add(row), column) else {
                continue;
            };
            if source.is_wide_continuation() {
                continue;
            }
            let symbol = source.contents();
            let symbol = if symbol.is_empty() || symbol.chars().any(char::is_control) {
                " "
            } else {
                &symbol
            };
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
    parser_row_offset: u16,
    buffer: &mut Buffer,
) {
    let Some(selection) = selection else {
        return;
    };
    if selection.history_backed
        && task.renders_with_terminal_parser(area.height)
        && render_live_history_selection(selection, task, area, parser_row_offset, buffer)
    {
        return;
    }
    if selection.parser_ordered_points().is_some() && task.renders_with_terminal_parser(area.height)
    {
        render_parser_selection(selection, task, area, parser_row_offset, buffer);
        return;
    }
    render_history_selection(selection, task, area, buffer);
}

fn render_parser_selection(
    selection: &Selection,
    task: &TaskRuntime,
    area: Rect,
    parser_row_offset: u16,
    buffer: &mut Buffer,
) {
    for row in 0..area.height {
        let line = task.parser_index_for_visible_row(parser_row_offset.saturating_add(row));
        let Some((start, end)) = selection.columns_for_parser_line(line, area.width) else {
            continue;
        };
        paint_selection_row(area, buffer, row, start, end);
    }
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
    parser_row_offset: u16,
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
        let screen_row = parser_row_offset.saturating_add(row);
        if screen_row_text(&task.parser, screen_row, area.width).trim_end() == expected {
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
                .set_style(footer_base_style());
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
            let symbol = if character.is_control() {
                " "
            } else {
                character.encode_utf8(&mut encoded)
            };
            buffer[(area.x + column, area.y + row)]
                .set_symbol(symbol)
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
            menu_style()
                .fg(THEME_RED_HOVER)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("Press Ctrl-C or q again to close Demons."),
        Line::raw("Press Esc to keep the panes running."),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(THEME_RED))
            .style(menu_style()),
    )
    .wrap(Wrap { trim: false })
    .render(popup, buffer);
}

fn render_notice(area: Rect, footer_rect: Option<Rect>, notice: &str, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let y = footer_rect
        .map(|footer| footer.y)
        .unwrap_or_else(|| area.bottom())
        .saturating_sub(1);
    if y < area.y {
        return;
    }
    let notice_area = Rect::new(area.x, y, area.width, 1);
    Paragraph::new(Line::from(vec![
        Span::styled(" notice ", Style::default().fg(THEME_BLACK).bg(THEME_GOLD)),
        Span::styled(
            format!(" {notice} "),
            Style::default().fg(THEME_WHITE).bg(THEME_FOOTER),
        ),
    ]))
    .style(footer_base_style())
    .render(notice_area, buffer);
}

fn render_problem_intro(area: Rect, buffer: &mut Buffer) {
    let popup = centered_rect(area, 66, 11);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    Clear.render(popup, buffer);
    Paragraph::new(vec![
        Line::styled(
            format!("{THEME_ACCENT_MARK} Config Problems {THEME_ACCENT_MARK}"),
            menu_heading_style(),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "Red !",
                menu_style()
                    .fg(THEME_RED_HOVER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" marks settings that must be fixed before saving or running."),
        ]),
        Line::from(vec![
            Span::styled(
                "Gold !",
                menu_style()
                    .fg(THEME_GOLD_HOVER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" marks settings Demons recovered; review them before saving."),
        ]),
        Line::raw("Follow the markers through the tabs, or use Exit > Problems."),
        Line::raw(""),
        Line::styled(
            "  Got it  ",
            Style::default().fg(THEME_BLACK).bg(THEME_SNOW),
        ),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(THEME_GOLD))
            .style(menu_style()),
    )
    .wrap(Wrap { trim: false })
    .render(popup, buffer);
}

fn render_welcome_intro(area: Rect, buffer: &mut Buffer) {
    let popup = centered_rect(area, 68, 11);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    Clear.render(popup, buffer);
    Paragraph::new(vec![
        Line::styled(
            format!("{THEME_ACCENT_MARK} Welcome to Demons {THEME_ACCENT_MARK}"),
            menu_heading_style(),
        ),
        Line::raw(""),
        Line::raw("This project does not have a demons.toml yet."),
        Line::raw("Use Tasks to add the commands you run for this project."),
        Line::raw("Each task gets its own pane when Demons starts."),
        Line::raw("When you are done, use Exit to save the new config."),
        Line::raw(""),
        Line::styled(
            "  Got it  ",
            Style::default().fg(THEME_BLACK).bg(THEME_SNOW),
        ),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(THEME_GOLD))
            .style(menu_style()),
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
    let mut style = pane_style();
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

fn word_columns_at(text: &str, column: u16) -> Option<(u16, u16)> {
    let chars = text.chars().collect::<Vec<_>>();
    let index = usize::from(column);
    if index >= chars.len() || chars[index].is_whitespace() {
        return None;
    }

    let mut start = index;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
        end += 1;
    }

    Some((
        start.min(usize::from(u16::MAX)) as u16,
        end.min(usize::from(u16::MAX)) as u16,
    ))
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

fn to_u16(value: usize) -> u16 {
    value.min(usize::from(u16::MAX)) as u16
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

fn csi_param(value: &str, index: usize, default: usize) -> usize {
    value
        .split(';')
        .nth(index)
        .and_then(|param| {
            if param.is_empty() {
                Some(default)
            } else {
                param.parse().ok()
            }
        })
        .unwrap_or(default)
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
            hover_bg: THEME_GOLD_HOVER,
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
    if let (Some(index), count) = (search.current_index, search.match_count)
        && count > 0
    {
        let noun = if count == 1 { "match" } else { "matches" };
        return Some(format!("{}/{} {noun}", index + 1, count));
    }
    search.message.clone()
}

fn footer_command_button(
    label: impl Into<String>,
    action: FooterAction,
    index: usize,
) -> FooterItem {
    let style = footer_palette_for_index(index);
    FooterItem::new(format!(" {} ", label.into()), Some(action), style)
}

fn footer_palette_for_index(index: usize) -> FooterItemStyle {
    match index % 5 {
        0 => footer_style(THEME_BLACK, THEME_SNOW, THEME_BLACK, THEME_GOLD_HOVER),
        1 => footer_style(THEME_WHITE, THEME_RED, THEME_WHITE, THEME_RED_HOVER),
        2 => footer_style(THEME_BLACK, THEME_SNOW, THEME_BLACK, THEME_GOLD_HOVER),
        3 => footer_style(THEME_WHITE, THEME_RED, THEME_WHITE, THEME_RED_HOVER),
        _ => footer_style(THEME_BLACK, THEME_GOLD, THEME_BLACK, THEME_GOLD_HOVER),
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
    if color == THEME_GREEN {
        THEME_GREEN_HOVER
    } else if color == THEME_RED {
        THEME_RED_HOVER
    } else if color == THEME_GOLD {
        THEME_GOLD_HOVER
    } else {
        THEME_WHITE
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
    prepare_history_log_dir(dir)?;
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

fn default_history_dir() -> PathBuf {
    #[cfg(unix)]
    {
        // Use a per-user directory under the system temp dir so scrollback logs
        // cannot be redirected through a predictable shared /tmp/demons path.
        let uid = unsafe { libc::geteuid() };
        env::temp_dir().join(format!("demons-{uid}"))
    }
    #[cfg(not(unix))]
    {
        env::temp_dir().join("demons")
    }
}

#[cfg(unix)]
fn prepare_history_log_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => validate_history_log_dir(dir, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let create_result = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
            match create_result {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create log directory {}", dir.display())
                    });
                }
            }
            let metadata = fs::symlink_metadata(dir)
                .with_context(|| format!("failed to inspect log directory {}", dir.display()))?;
            validate_history_log_dir(dir, &metadata)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect log directory {}", dir.display()))
        }
    }
}

#[cfg(unix)]
fn validate_history_log_dir(dir: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        anyhow::bail!("log directory {} must not be a symlink", dir.display());
    }
    if !metadata.is_dir() {
        anyhow::bail!("log path {} is not a directory", dir.display());
    }

    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        anyhow::bail!(
            "log directory {} is owned by uid {}, not current uid {uid}",
            dir.display(),
            metadata.uid()
        );
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict log directory {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_history_log_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create log directory {}", dir.display()))
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

    use crate::config::{CONFIG_FILE, Config, DEFAULT_MULTI_CLICK_MS, Settings};
    use tempfile::tempdir;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
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
    fn notice_does_not_replace_command_footer_buttons() {
        let mut app = test_app();
        app.mode = AppMode::Command;
        app.set_notice("Edit not applied.".to_owned());

        let (_, _, items) = app.footer_parts(Instant::now());

        assert!(items.iter().any(|item| item.text == " / search "));
        assert!(items.iter().any(|item| item.text == " ? menu "));
        assert!(
            !items
                .iter()
                .any(|item| item.text.contains("Edit not applied"))
        );
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
    fn render_text_replaces_control_characters() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));

        render_text(
            &mut buffer,
            Rect::new(0, 0, 8, 1),
            "a\u{1b}b",
            Style::default(),
        );

        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "b");
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
    fn command_footer_uses_alternating_button_contrast() {
        let items = command_footer_items();

        assert_eq!(items[0].style.fg, THEME_BLACK);
        assert_eq!(items[1].style.fg, THEME_WHITE);
        assert_eq!(items[2].style.fg, THEME_BLACK);
        assert_eq!(items[3].style.fg, THEME_WHITE);
        assert_eq!(items[0].style.bg, THEME_SNOW);
        assert_eq!(items[1].style.bg, THEME_RED);
        assert_eq!(items[2].style.bg, THEME_SNOW);
        assert_eq!(items[3].style.bg, THEME_RED);
        assert_eq!(items[4].style.bg, THEME_GOLD);
    }

    #[test]
    fn command_mode_uses_gold_mode_color() {
        let mut app = test_app();
        app.mode = AppMode::Command;

        let (label, color, _) = app.footer_parts(Instant::now());
        assert_eq!((label, color), ("❄ COMMAND ❄", THEME_COMMAND));
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
        let mut history = TextHistory::new(5, 24, 100);
        history.process(b"alpha\nbravocharlie\nz");

        let text = history.text_between(
            SelectionPoint { line: 1, column: 0 },
            SelectionPoint { line: 3, column: 1 },
        );

        assert_eq!(text, "bravocharlie");
    }

    #[test]
    fn text_history_honors_erase_line_for_progress_output() {
        let mut history = TextHistory::new(80, 24, 100);
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
    fn text_history_honors_home_clear_redraws() {
        let mut history = TextHistory::new(80, 24, 100);
        history.process(
            b"\r\n> vashti-web@0.1.0 dev\r\n> vite --host 127.0.0.1\r\n\r\n\r\n\
              \x1b[1;1H\x1b[0J\r\n  VITE v6.4.2  ready in 103 ms\r\n\r\n  -> Local: http://localhost:5173/\r\n",
        );

        let text = history.all_text();
        assert!(!text.contains("vashti-web"));
        assert!(!text.contains("vite --host"));
        assert!(text.contains("VITE v6.4.2"));
        assert!(text.contains("Local: http://localhost:5173/"));
    }

    #[test]
    fn vite_home_clear_output_does_not_create_fake_scrollback() {
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.resize(80, 20);

        task.process_output(
            b"\r\n> vashti-web@0.1.0 dev\r\n> vite --host 127.0.0.1\r\n\r\n\r\n\
              \x1b[1;1H\x1b[0J\r\n  VITE v6.4.2  ready in 103 ms\r\n\r\n  -> Local: http://localhost:5173/\r\n",
        );

        assert_eq!(task.max_scroll_offset(), 0);
        assert!(!task.scroll_up(3));
    }

    #[test]
    fn home_clear_redraw_clamps_existing_scroll_offset() {
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.resize(80, 10);
        for line in 0..15 {
            task.process_output(format!("line {line}\r\n").as_bytes());
        }
        task.scroll_to_top();
        assert!(task.scroll_offset > 0);

        task.process_output(b"\x1b[1;1H\x1b[0Jnew screen\r\n");

        let max_scroll_offset = task.max_scroll_offset();
        assert_eq!(task.scroll_offset, max_scroll_offset);
    }

    #[test]
    fn text_history_home_clear_preserves_scrolled_off_history() {
        let mut history = TextHistory::new(80, 3, 100);
        history.process(b"old one\r\nold two\r\nvisible one\r\nvisible two\r\nvisible three");

        history.process(b"\x1b[1;1H\x1b[0Jnew screen\r\n");

        let text = history.all_text();
        assert!(text.contains("old one"));
        assert!(text.contains("old two"));
        assert!(!text.contains("visible one"));
        assert!(!text.contains("visible two"));
        assert!(!text.contains("visible three"));
        assert!(text.contains("new screen"));
    }

    #[test]
    fn text_history_all_text_preserves_scrollback_text() {
        let mut history = TextHistory::new(80, 24, 100);
        history.process(b"alpha\nbravo");

        assert_eq!(history.all_text(), "alpha\nbravo");

        history.process(b"\n");
        assert_eq!(history.all_text(), "alpha\nbravo\n");
    }

    #[test]
    fn text_history_finds_matching_lines_case_insensitively() {
        let mut history = TextHistory::new(80, 24, 100);
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
        assert_eq!(items[4].style.bg, THEME_GOLD);
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

    #[cfg(unix)]
    #[test]
    fn history_log_rejects_symlink_directory() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("logs");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = write_history_log(&link, "web", "secret").unwrap_err();

        assert!(error.to_string().contains("must not be a symlink"));
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
            granularity: SelectionGranularity::Character,
            origin: None,
            history_backed: false,
            parser_anchor: None,
            parser_cursor: None,
            parser_origin: None,
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
    fn parser_backed_drag_selection_stays_attached_while_autoscrolling() {
        let mut app = test_app_with_tasks(vec![test_task("one")]);
        app.update_layout(Rect::new(0, 0, 60, 8));
        app.mode = AppMode::Command;
        let area = app.content_rects[0];
        for line in 0..(area.height + 4) {
            app.tasks[0].process_output(format!("line {line}\r\n").as_bytes());
        }
        assert!(app.tasks[0].terminal_scrollback_len > 0);

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.bottom() - 1,
        ))
        .unwrap();
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 5,
            area.y.saturating_sub(1),
        ))
        .unwrap();

        assert!(app.tasks[0].scroll_offset > 0);
        assert!(
            app.selection
                .as_ref()
                .unwrap()
                .parser_ordered_points()
                .is_some()
        );

        app.tasks[0].history.clear();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 8));
        render_screen(&app.tasks[0].parser, area, 0, &mut buffer);
        render_selection(app.selection.as_ref(), &app.tasks[0], area, 0, &mut buffer);

        assert_eq!(buffer[(area.x + 5, area.y)].bg, Color::White);
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
    fn double_click_selects_word_and_triple_click_selects_line() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"alpha beta/gamma\nsecond line\n");

        let first = app.content_rects[0];
        let column = first.x + 8;
        let row = first.y;

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
            .unwrap();
        assert!(app.selection.is_none());

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
            .unwrap();

        let selection = app.selection.as_ref().unwrap();
        assert!(!selection.dragging);
        assert_eq!(app.selected_text().unwrap(), "beta/gamma");

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
            .unwrap();

        assert_eq!(app.selected_text().unwrap(), "alpha beta/gamma");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
        render_screen(&app.tasks[0].parser, first, 0, &mut buffer);
        render_selection(app.selection.as_ref(), &app.tasks[0], first, 0, &mut buffer);
        assert_eq!(buffer[(first.right() - 1, row)].bg, Color::White);
    }

    #[test]
    fn double_click_drag_expands_selection_by_whole_words() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"alpha beta gamma delta\n");

        let first = app.content_rects[0];
        let beta = first.x + 8;
        let gamma = first.x + 13;
        let row = first.y;

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), beta, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), beta, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), beta, row))
            .unwrap();

        assert_eq!(
            app.selection.as_ref().unwrap().granularity,
            SelectionGranularity::Word
        );
        assert!(app.selection.as_ref().unwrap().dragging);
        assert_eq!(app.selected_text().unwrap(), "beta");

        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), gamma, row))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "beta gamma");

        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), gamma, row))
            .unwrap();
        assert!(!app.selection.as_ref().unwrap().dragging);
        assert_eq!(app.selected_text().unwrap(), "beta gamma");
    }

    #[test]
    fn double_click_blank_space_still_enters_word_selection_mode() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"alpha   beta gamma\n");

        let first = app.content_rects[0];
        let blank = first.x + 6;
        let beta = first.x + 10;
        let row = first.y;

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), blank, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), blank, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), blank, row))
            .unwrap();

        assert_eq!(
            app.selection.as_ref().unwrap().granularity,
            SelectionGranularity::Word
        );
        assert!(app.selection.as_ref().unwrap().dragging);

        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), beta, row))
            .unwrap();
        assert_eq!(app.selected_text().unwrap(), "  beta");
    }

    #[test]
    fn double_click_drag_left_keeps_the_anchor_word_complete() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"alpha beta gamma delta\n");

        let first = app.content_rects[0];
        let beta = first.x + 8;
        let gamma = first.x + 13;
        let row = first.y;

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), gamma, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), gamma, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), gamma, row))
            .unwrap();
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), beta, row))
            .unwrap();

        assert_eq!(app.selected_text().unwrap(), "beta gamma");
    }

    #[test]
    fn triple_click_drag_expands_selection_by_whole_lines() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.tasks[0].process_output(b"line one\r\nline two\r\nline three\r\n");

        let first = app.content_rects[0];
        let column = first.x + 1;
        let first_row = first.y;
        let third_row = first.y + 2;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            first_row,
        ))
        .unwrap();
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            column,
            first_row,
        ))
        .unwrap();
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            first_row,
        ))
        .unwrap();
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            column,
            first_row,
        ))
        .unwrap();
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            first_row,
        ))
        .unwrap();

        assert_eq!(
            app.selection.as_ref().unwrap().granularity,
            SelectionGranularity::Line
        );
        assert_eq!(app.selected_text().unwrap(), "line one");

        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            column,
            third_row,
        ))
        .unwrap();
        assert_eq!(
            app.selected_text().unwrap(),
            "line one\nline two\nline three"
        );

        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            column,
            third_row,
        ))
        .unwrap();
        assert!(!app.selection.as_ref().unwrap().dragging);
        assert_eq!(
            app.selected_text().unwrap(),
            "line one\nline two\nline three"
        );
    }

    #[test]
    fn stale_click_does_not_count_as_double_click() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 100, 20));
        app.mode = AppMode::Command;
        app.loaded.config.settings.multi_click_ms = 150;
        app.tasks[0].process_output(b"alpha beta\n");

        let first = app.content_rects[0];
        let column = first.x + 8;
        let row = first.y;
        app.last_click = Some(ClickState {
            pane: 0,
            x: column,
            y: row,
            at: Instant::now() - Duration::from_millis(151),
            count: 1,
        });

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row))
            .unwrap();

        let selection = app.selection.as_ref().unwrap();
        assert!(selection.dragging);
        assert!(!selection.dragged);
        assert!(app.selected_text().is_none());
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
    fn first_scrollback_page_renders_from_terminal_parser() {
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.resize(20, 4);
        task.process_output(
            b"\x1b[32mgreen row\x1b[0m\r\nplain one\r\nplain two\r\nplain three\r\nplain four\r\n",
        );

        assert!(task.scroll_up(2));

        let area = Rect::new(0, 0, 20, 4);
        let mut buffer = Buffer::empty(area);
        render_screen(&task.parser, area, 0, &mut buffer);

        assert_eq!(buffer_line(&buffer, 0, 20).trim_end(), "green row");
        assert_eq!(buffer[(0, 0)].fg, Color::Indexed(2));
    }

    #[test]
    fn scroll_up_uses_terminal_parser_scrollback_when_text_history_lags() {
        let mut task = TaskRuntime::new(test_task("web"), PathBuf::from("."));
        task.resize(20, 4);
        task.process_output(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
        assert!(task.terminal_scrollback_len > 0);

        task.history.clear();

        assert!(task.scroll_up(1));
        assert_eq!(task.scroll_offset, 1);
        assert_eq!(task.parser.screen().scrollback(), 1);
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
        render_screen(&app.tasks[1].parser, area, 0, &mut buffer);
        render_selection(app.selection.as_ref(), &app.tasks[1], area, 0, &mut buffer);

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
            granularity: SelectionGranularity::Character,
            origin: None,
            history_backed: false,
            parser_anchor: None,
            parser_cursor: None,
            parser_origin: None,
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
            granularity: SelectionGranularity::Character,
            origin: None,
            history_backed: false,
            parser_anchor: None,
            parser_cursor: None,
            parser_origin: None,
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

        assert!(task.scroll_offset <= task.max_scroll_offset());
        assert!(task.renders_with_terminal_parser(40));
        assert_eq!(task.parser.screen().scrollback(), task.scroll_offset);
    }

    #[test]
    fn exited_pane_reserves_scene_only_at_bottom() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 80, 24));
        app.tasks[0].status = TaskStatus::Exited {
            code: 0,
            success: true,
            signal: None,
        };

        let full = app.content_rects[0];
        let scene = app.exited_scene_rect(0).unwrap();
        let output = app.task_output_rect(0).unwrap();

        assert!(scene.height > 0);
        assert_eq!(output.height + scene.height, full.height);

        app.tasks[0].scroll_offset = 1;
        assert!(app.exited_scene_rect(0).is_none());
        assert_eq!(app.task_output_rect(0), Some(full));

        app.tasks[0].scroll_offset = 0;
        app.mode = AppMode::Search;
        assert!(app.exited_scene_rect(0).is_none());
        assert_eq!(app.task_output_rect(0), Some(full));
    }

    #[test]
    fn exited_scene_uses_most_of_pane_but_keeps_output_tail() {
        let content = Rect::new(0, 0, 80, 20);
        let scene = reserved_scene_rect(content).unwrap();

        assert_eq!(scene.height, 15);
        assert_eq!(content.height - scene.height, 5);

        let compact = Rect::new(0, 0, 80, 8);
        let scene = reserved_scene_rect(compact).unwrap();
        assert_eq!(compact.height - scene.height, 3);

        assert!(reserved_scene_rect(Rect::new(0, 0, 80, 6)).is_none());
    }

    #[test]
    fn scene_rendering_does_not_enter_visible_text() {
        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 80, 24));
        app.tasks[0].process_output(b"alpha output\n");
        app.tasks[0].status = TaskStatus::Exited {
            code: 0,
            success: true,
            signal: None,
        };

        let text = app.visible_pane_text(0).unwrap();

        assert!(text.contains("alpha output"));
        assert!(!text.contains('╱'));
        assert!(!text.contains('☃'));
        assert!(!text.contains(THEME_ACCENT_MARK));
    }

    #[test]
    fn exited_scene_preserves_bottom_parser_view() {
        let mut app = test_app_with_tasks(vec![test_task("long_done")]);
        app.update_layout(Rect::new(0, 0, 80, 24));
        for line in 1..=80 {
            app.tasks[0].process_output(format!("long line {line:03}\n").as_bytes());
        }
        app.tasks[0].status = TaskStatus::Exited {
            code: 0,
            success: true,
            signal: None,
        };

        assert!(app.exited_scene_rect(0).is_some());
        assert!(app.task_parser_row_offset(0) > 0);

        let text = app.visible_pane_text(0).unwrap();

        assert!(text.contains("long line 080"));
        assert!(!text.contains("long line 064"));
        assert!(!text.contains(THEME_ACCENT_MARK));
    }

    #[test]
    fn snow_scene_flakes_fall_between_frames() {
        let area = Rect::new(0, 0, 18, 7);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_snow_scene(area, 42, 0, &mut first);
        render_snow_scene(area, 42, 1, &mut second);

        assert_ne!(
            snowflake_positions(&first, area),
            snowflake_positions(&second, area)
        );
    }

    #[test]
    fn snowman_uses_full_body_in_reserved_scene_height() {
        let reserved = Rect::new(0, 0, 24, 7);
        let compact = Rect::new(0, 0, 24, 5);

        assert_eq!(
            snowman_rect(reserved, reserved.bottom() - 1)
                .unwrap()
                .height,
            4
        );
        assert_eq!(
            snowman_rect(compact, compact.bottom() - 1).unwrap().height,
            3
        );
    }

    #[test]
    fn tree_scene_lights_blink_between_frames() {
        let area = Rect::new(0, 0, 28, 8);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_tree_scene(area, 42, 0, &mut first);
        render_tree_scene(area, 42, 2, &mut second);

        assert_ne!(
            tree_light_positions(&first, area),
            tree_light_positions(&second, area)
        );
    }

    #[test]
    fn skating_scene_skaters_glide_between_frames() {
        let area = Rect::new(0, 0, 34, 8);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_skating_scene(area, 42, 0, &mut first);
        render_skating_scene(area, 42, 3, &mut second);

        assert_ne!(
            skater_head_positions(&first, area),
            skater_head_positions(&second, area)
        );
    }

    #[test]
    fn skating_scene_draws_lake_and_skaters() {
        let area = Rect::new(0, 0, 34, 8);
        let mut buffer = Buffer::empty(area);

        render_skating_scene(area, 42, 0, &mut buffer);

        let text = buffer_text(&buffer, area);
        assert!(text.contains('o'));
        assert!(text.contains('█') || text.contains('▄'));
        assert!(text.contains('║') || text.contains("/|") || text.contains("|\\"));
    }

    #[test]
    fn skating_tracks_do_not_look_like_extra_legs() {
        let area = Rect::new(0, 0, 34, 8);
        let lake_y = 4;
        let mut buffer = Buffer::empty(area);

        render_frozen_lake(area, lake_y, 42, &mut buffer);
        render_skating_tracks(area, lake_y, 42, &mut buffer);

        let text = buffer_text(&buffer, area);
        assert!(text.contains('·'));
        assert!(!text.contains('╱'));
        assert!(!text.contains('╲'));
    }

    #[test]
    fn skater_sprite_preserves_existing_backgrounds() {
        let area = Rect::new(0, 0, 12, 6);
        let mut buffer = Buffer::empty(area);

        buffer[(6, 2)].set_bg(THEME_PANEL);
        for x in 5..=7 {
            buffer[(x, 3)].set_bg(THEME_ICE);
        }
        buffer[(6, 4)].set_bg(THEME_ICE);

        render_skater(&mut buffer, 5, 4, 0, 1);

        assert_eq!(buffer[(6, 2)].symbol(), "o");
        assert_eq!(buffer[(6, 2)].bg, THEME_PANEL);
        for x in 5..=7 {
            assert_ne!(buffer[(x, 3)].symbol(), " ");
            assert_eq!(buffer[(x, 3)].bg, THEME_ICE);
        }
        assert_eq!(buffer[(6, 4)].symbol(), "║");
        assert_eq!(buffer[(6, 4)].bg, THEME_ICE);
    }

    #[test]
    fn skating_tracks_preserve_ice_backgrounds() {
        let area = Rect::new(0, 0, 34, 8);
        let lake_y = 4;
        let mut buffer = Buffer::empty(area);

        render_frozen_lake(area, lake_y, 42, &mut buffer);
        let mut before = Vec::new();
        for y in lake_y..area.bottom() {
            for x in area.x..area.right() {
                before.push((x, y, buffer[(x, y)].bg));
            }
        }

        render_skating_tracks(area, lake_y, 42, &mut buffer);

        for (x, y, bg) in before {
            assert_eq!(
                buffer[(x, y)].bg,
                bg,
                "track changed ice background at ({x}, {y})"
            );
        }
    }

    #[test]
    fn frozen_lake_uses_sparse_seeded_ice_highlights() {
        let area = Rect::new(0, 0, 58, 20);
        let lake_y = 12;
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_frozen_lake(area, lake_y, 42, &mut first);
        render_frozen_lake(area, lake_y, 99, &mut second);

        let first_highlights = ice_highlight_positions(&first, area, lake_y);
        let second_highlights = ice_highlight_positions(&second, area, lake_y);
        let lake_cells =
            usize::from(area.width) * usize::from(area.bottom().saturating_sub(lake_y + 1));

        assert!(first_highlights.len() < lake_cells / 30);
        assert_ne!(first_highlights, second_highlights);
    }

    #[test]
    fn frozen_lake_snowbank_stays_on_left_side_under_quarter_width() {
        let area = Rect::new(0, 0, 60, 20);
        let lake_y = 12;
        let mut buffer = Buffer::empty(area);

        render_frozen_lake(area, lake_y, 42, &mut buffer);

        let max_snow_width = area.width / 4;
        for y in lake_y..area.bottom() {
            let snow_cells = (area.x..area.right())
                .filter(|&x| buffer[(x, y)].bg == THEME_SNOW)
                .count();
            assert!(
                snow_cells <= usize::from(max_snow_width),
                "row {y} used {snow_cells} snow cells"
            );
            let snow_width = if y == lake_y {
                area.width / 4
            } else {
                skating_snow_width(area, lake_y, y)
            };
            let edge = area.x + snow_width;
            assert_eq!(buffer[(edge, y)].symbol(), "▌");
        }
    }

    #[test]
    fn frozen_lake_snowbank_uses_concave_curve() {
        let area = Rect::new(0, 0, 60, 20);
        let lake_y = 12;
        let top_shelf_width = area.width / 4;
        let first_curve_width = skating_snow_width(area, lake_y, lake_y + 1);
        let mid_width = skating_snow_width(area, lake_y, lake_y + 4);
        let bottom_width = skating_snow_width(area, lake_y, area.bottom() - 1);

        assert_eq!(first_curve_width + 1, top_shelf_width);
        assert_eq!(bottom_width, area.width / 4);
        assert!(
            mid_width < top_shelf_width / 2,
            "middle width {mid_width} should pinch below half of top shelf width {top_shelf_width}"
        );
        assert!(
            mid_width < bottom_width / 2,
            "middle width {mid_width} should pinch below half of bottom width {bottom_width}"
        );
    }

    #[test]
    fn skating_lanes_stay_on_ice_side_of_snowbank() {
        let area = Rect::new(0, 0, 58, 20);
        let lake_y = 12;
        let mut buffer = Buffer::empty(area);

        render_frozen_lake(area, lake_y, 42, &mut buffer);
        render_skating_tracks(area, lake_y, 42, &mut buffer);
        render_skaters(area, lake_y, 42, 15, &mut buffer);

        for y in lake_y + 1..area.bottom() {
            for x in area.x..skating_ice_start(area) {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    !matches!(
                        symbol,
                        "o" | "~" | "/" | "\\" | "|" | "█" | ">" | "<" | "║" | "·"
                    ),
                    "skating lane marker {symbol:?} entered snowbank at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn skating_scene_keeps_heads_visible_while_skaters_pass() {
        let area = Rect::new(0, 0, 58, 20);

        for frame in 0..120 {
            let mut buffer = Buffer::empty(area);
            render_skating_scene(area, 42, frame, &mut buffer);

            assert_eq!(
                skater_head_positions(&buffer, area).len(),
                2,
                "frame {frame} lost a skater head"
            );
        }
    }

    #[test]
    fn skating_scene_keeps_heads_on_ice() {
        let area = Rect::new(0, 0, 58, 20);
        let mut buffer = Buffer::empty(area);

        render_skating_scene(area, 42, 0, &mut buffer);

        for (x, y) in skater_head_positions(&buffer, area) {
            assert!(
                matches!(buffer[(x, y)].bg, THEME_ICE | THEME_ICE_DARK),
                "skater head at ({x}, {y}) was not on ice"
            );
        }
    }

    #[test]
    fn sleigh_scene_team_moves_between_frames() {
        let area = Rect::new(0, 0, 44, 10);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_sleigh_scene(area, 0, 10, &mut first);
        render_sleigh_scene(area, 0, 12, &mut second);

        let first = sleigh_nose_positions(&first, area);
        let second = sleigh_nose_positions(&second, area);

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(second[0].0 < first[0].0);
    }

    #[test]
    fn sleigh_scene_stars_twinkle_without_drifting() {
        let area = Rect::new(0, 0, 44, 10);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);

        render_sleigh_sky(area, 42, 0, &mut first);
        render_sleigh_sky(area, 42, 2, &mut second);

        assert_eq!(
            sleigh_star_positions(&first, area),
            sleigh_star_positions(&second, area)
        );
        assert_ne!(
            sleigh_star_symbols(&first, area),
            sleigh_star_symbols(&second, area)
        );
    }

    #[test]
    fn sleigh_scene_draws_reindeer_and_sleigh() {
        let area = Rect::new(0, 0, 44, 10);
        let mut buffer = Buffer::empty(area);

        render_sleigh_scene(area, 0, 12, &mut buffer);

        let text = buffer_text(&buffer, area);
        assert!(text.contains('●'));
        assert!(text.contains("<(•)=="));
        assert!(text.contains("────╮"));
        assert!(text.contains("__◢██◣"));
        assert!(text.contains("◥████◤"));
        assert!(text.contains("███"));
    }

    #[test]
    fn sleigh_scene_clips_team_to_scene_area() {
        let scene_area = Rect::new(0, 0, 44, 10);
        let buffer_area = Rect::new(0, 0, 88, 10);
        let mut buffer = Buffer::empty(buffer_area);

        render_sleigh_scene(scene_area, 0, 0, &mut buffer);

        for y in buffer_area.y..buffer_area.bottom() {
            for x in scene_area.right()..buffer_area.right() {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    " ",
                    "sleigh scene leaked into neighboring area at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn santa_scene_waves_between_frames() {
        let area = Rect::new(0, 0, 38, 14);
        let mut low_wave = Buffer::empty(area);
        let mut high_wave = Buffer::empty(area);

        render_santa_scene(area, 0, &mut high_wave);
        render_santa_scene(area, 1, &mut low_wave);

        let high_text = buffer_text(&high_wave, area);
        let low_text = buffer_text(&low_wave, area);
        assert!(high_text.contains("●   ●"));
        assert!(high_text.contains("╭  ‿  ╮"));
        assert!(high_text.contains("◢◤"));
        assert!(low_text.contains("●   ●"));
        assert!(low_text.contains("╭  ‿  ╮"));
        assert!(low_text.contains("◢◤"));
        assert_ne!(high_text, low_text);
    }

    #[test]
    fn santa_scene_draws_fat_santa_over_small_chimney() {
        let area = Rect::new(0, 0, 38, 14);
        let mut buffer = Buffer::empty(area);

        render_santa_scene(area, 0, &mut buffer);

        let text = buffer_text(&buffer, area);
        assert!(text.contains("◢█████████◣"));
        assert!(text.contains("╭████████"));
        assert!(text.contains("▀▀▀▀▀"));
        assert_eq!(buffer[(12, 1)].bg, THEME_SKIN);
        assert_eq!(buffer[(15, 1)].bg, THEME_SKIN);
        assert_eq!(buffer[(18, 1)].bg, THEME_SKIN);
        assert_ne!(buffer[(11, 1)].bg, THEME_SKIN);
    }

    #[test]
    fn jack_scene_pops_out_between_frames() {
        let area = Rect::new(0, 0, 24, 8);
        let mut closed = Buffer::empty(area);
        let mut open = Buffer::empty(area);

        render_jack_scene(area, 42, 0, &mut closed);
        render_jack_scene(area, 42, 3, &mut open);

        assert!(!buffer_text(&closed, area).contains('☺'));
        assert!(buffer_text(&open, area).contains('☺'));
        assert_ne!(buffer_text(&closed, area), buffer_text(&open, area));
    }

    #[test]
    fn jack_scene_uses_spring_phase_before_full_sprite() {
        let area = Rect::new(0, 0, 24, 8);
        let mut spring = Buffer::empty(area);
        let mut open = Buffer::empty(area);

        render_jack_scene(area, 42, 1, &mut spring);
        render_jack_scene(area, 42, 2, &mut open);

        let spring_text = buffer_text(&spring, area);
        let open_text = buffer_text(&open, area);
        assert!(spring_text.contains('╱'));
        assert!(spring_text.contains('╲'));
        assert!(!spring_text.contains('☺'));
        assert!(open_text.contains('☺'));
    }

    #[test]
    fn jack_scene_keeps_open_sprite_attached_to_box() {
        let area = Rect::new(0, 0, 24, 8);
        let mut buffer = Buffer::empty(area);
        render_jack_scene(area, 42, 3, &mut buffer);

        let ground_y = area.bottom().saturating_sub(1);
        let box_width = area.width.saturating_sub(8).clamp(9, 13);
        let box_x = area.x + area.width.saturating_sub(box_width) / 2;
        let box_y = ground_y.saturating_sub(2);
        let center = box_x + box_width / 2;

        for row in box_y.saturating_sub(5)..box_y {
            assert_ne!(
                buffer[(center, row)].symbol(),
                " ",
                "jack should not float above the present at row {row}"
            );
        }

        let lid_y = box_y.saturating_sub(1);
        assert_eq!(buffer[(box_x, lid_y)].symbol(), "╲");
        assert_eq!(buffer[(box_x + box_width - 1, lid_y)].symbol(), "╱");

        let rendered = buffer_text(&buffer, area);
        assert!(rendered.contains("╲▄▄▄"));
        assert!(rendered.contains("▄▄▄╱"));
    }

    #[test]
    fn empty_grid_slots_get_seeded_scene_choices() {
        let mut app =
            test_app_with_tasks(vec![test_task("one"), test_task("two"), test_task("three")]);
        app.scene_seed = 0;
        app.update_layout(Rect::new(0, 0, 80, 40));

        assert_eq!(app.pane_rects.len(), 3);
        assert!(app.slot_rects.len() > app.pane_rects.len());

        let choices = (0..64)
            .filter_map(|slot| app.empty_slot_scene_state(slot, Rect::new(0, 0, 80, 40)))
            .map(|scene| scene.kind)
            .collect::<HashSet<_>>();
        assert!(choices.contains(&SceneKind::Fireplace));
        assert!(choices.contains(&SceneKind::Snow));
        assert!(choices.contains(&SceneKind::Tree));
        assert!(choices.contains(&SceneKind::Santa));
        assert!(choices.contains(&SceneKind::Jack));
        assert!(choices.contains(&SceneKind::Skating));
        assert!(choices.contains(&SceneKind::Sleigh));
    }

    #[test]
    fn scene_selection_uses_current_area_size() {
        let compact = Rect::new(0, 0, 24, 4);
        let roomy = Rect::new(0, 0, 40, 14);
        let too_small = Rect::new(0, 0, 17, 7);

        assert!(scene_fits(SceneKind::Fireplace, compact));
        assert!(!scene_fits(SceneKind::Snow, compact));
        assert!(!scene_fits(SceneKind::Santa, compact));
        assert!(scene_state_for_area(0, too_small, None).is_none());
        assert_eq!(
            scene_state_for_area(0, compact, Some(SceneKind::Snow))
                .unwrap()
                .kind,
            SceneKind::Fireplace
        );

        let choices = (0..64)
            .filter_map(|seed| scene_state_for_area(seed, roomy, None))
            .map(|scene| scene.kind)
            .collect::<HashSet<_>>();
        assert!(choices.contains(&SceneKind::Fireplace));
        assert!(choices.contains(&SceneKind::Snow));
        assert!(choices.contains(&SceneKind::Tree));
        assert!(choices.contains(&SceneKind::Santa));
        assert!(choices.contains(&SceneKind::Jack));
        assert!(choices.contains(&SceneKind::Skating));
        assert!(choices.contains(&SceneKind::Sleigh));
    }

    #[test]
    fn dev_scene_override_parses_private_scene_names() {
        assert_eq!(
            parse_dev_scene_kind("fireplace"),
            Some(SceneKind::Fireplace)
        );
        assert_eq!(parse_dev_scene_kind("fire"), Some(SceneKind::Fireplace));
        assert_eq!(parse_dev_scene_kind("snow"), Some(SceneKind::Snow));
        assert_eq!(parse_dev_scene_kind("snowman"), Some(SceneKind::Snow));
        assert_eq!(parse_dev_scene_kind("tree"), Some(SceneKind::Tree));
        assert_eq!(parse_dev_scene_kind("santa"), Some(SceneKind::Santa));
        assert_eq!(parse_dev_scene_kind("rooftop"), Some(SceneKind::Santa));
        assert_eq!(parse_dev_scene_kind("jack"), Some(SceneKind::Jack));
        assert_eq!(
            parse_dev_scene_kind("jack-in-the-box"),
            Some(SceneKind::Jack)
        );
        assert_eq!(parse_dev_scene_kind("skating"), Some(SceneKind::Skating));
        assert_eq!(parse_dev_scene_kind("lake"), Some(SceneKind::Skating));
        assert_eq!(parse_dev_scene_kind("sleigh"), Some(SceneKind::Sleigh));
        assert_eq!(parse_dev_scene_kind("reindeer"), Some(SceneKind::Sleigh));
    }

    #[test]
    fn dev_scene_numeric_overrides_parse_decimal_and_hex() {
        assert_eq!(parse_dev_u64("42"), Some(42));
        assert_eq!(parse_dev_u64("0x2a"), Some(42));
        assert_eq!(parse_dev_u64(""), None);
        assert_eq!(parse_dev_u64("nope"), None);
    }

    #[test]
    fn scene_override_forces_empty_and_exited_scene_kinds() {
        let mut app =
            test_app_with_tasks(vec![test_task("one"), test_task("two"), test_task("three")]);
        app.scene_override = Some(SceneKind::Snow);
        app.update_layout(Rect::new(0, 0, 80, 40));

        assert_eq!(
            app.task_scene_state(0, Rect::new(0, 0, 80, 7))
                .unwrap()
                .kind,
            SceneKind::Snow
        );
        assert_eq!(
            app.empty_slot_scene_state(3, Rect::new(0, 0, 80, 7))
                .unwrap()
                .kind,
            SceneKind::Snow
        );
    }

    #[test]
    fn scene_animation_ticks_only_when_visible() {
        let mut app =
            test_app_with_tasks(vec![test_task("one"), test_task("two"), test_task("three")]);
        let now = Instant::now();
        app.update_layout(Rect::new(0, 0, 80, 40));
        app.countdown_snapshot = app.waiting_countdown_snapshot(now);
        app.last_scene_frame = now - SCENE_FRAME_INTERVAL;

        assert!(app.tick().unwrap());
        assert_eq!(app.scene_frame, 1);

        let mut app = test_app();
        app.update_layout(Rect::new(0, 0, 80, 40));
        app.countdown_snapshot = app.waiting_countdown_snapshot(now);
        app.last_scene_frame = now - SCENE_FRAME_INTERVAL;

        assert!(!app.tick().unwrap());
        assert_eq!(app.scene_frame, 0);
    }

    #[test]
    fn scene_frame_override_freezes_animation_tick() {
        let mut app =
            test_app_with_tasks(vec![test_task("one"), test_task("two"), test_task("three")]);
        let now = Instant::now();
        app.update_layout(Rect::new(0, 0, 80, 40));
        app.countdown_snapshot = app.waiting_countdown_snapshot(now);
        app.scene_frame_override = Some(7);
        app.last_scene_frame = now - SCENE_FRAME_INTERVAL;

        assert!(!app.tick().unwrap());
        assert_eq!(app.active_scene_frame(), 7);
        assert_eq!(app.scene_frame, 0);
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
            config_warnings: Vec::new(),
            config_problems: Vec::new(),
            created_from_missing_file: true,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut app = App::new(
            loaded,
            tx,
            rx,
            Arc::new(Mutex::new(HashSet::new())),
            true,
            false,
        );

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
    fn first_run_empty_config_shows_welcome_intro() {
        let temp = tempdir().unwrap();
        let loaded = LoadedConfig {
            path: temp.path().join(CONFIG_FILE),
            root: temp.path().to_path_buf(),
            config: Config::default(),
            config_warnings: Vec::new(),
            config_problems: Vec::new(),
            created_from_missing_file: true,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut app = App::new(
            loaded,
            tx,
            rx,
            Arc::new(Mutex::new(HashSet::new())),
            true,
            false,
        );

        app.open_menu(MenuTab::Tasks);

        assert!(app.welcome_intro);
        assert!(!app.problem_intro);
        assert!(app.notice.is_none());
        assert!(
            menu_problems(app.menu.as_ref().unwrap())
                .iter()
                .any(|problem| problem.message == "At least one task is required.")
        );

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_welcome_intro(Rect::new(0, 0, 120, 40), &mut buffer);
        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Welcome to Demons"));
        assert!(rendered.contains("Use Tasks to add the commands"));
        assert!(!rendered.contains("Config Problems"));
    }

    #[test]
    fn recovery_exit_tab_saves_and_starts_instead_of_closing() {
        let mut app = test_app();
        app.start_after_config_save = true;
        app.tasks_started = false;
        app.open_menu(MenuTab::Exit);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let exit_mode = app.menu_exit_mode();
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            exit_mode,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Save config and start tasks"));
        assert!(!rendered.contains("Save config and close"));
        assert_eq!(
            app.selected_menu_action(),
            Some(MenuAction::Exit(MenuExitAction::SaveOnly))
        );
    }

    #[test]
    fn menu_shows_recovered_config_warnings() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Task {
                index: 0,
                field: Some(ConfigTaskField::Command),
            },
            "Recovered command for task \"server\".",
        )];
        app.open_menu(MenuTab::Exit);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::ConfigureOnly,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Problems"));
        assert!(rendered.contains("Recovered command"));
    }

    #[test]
    fn root_recovery_notice_is_not_a_menu_problem() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Root,
            "Recovered config after a parse or schema mismatch.",
        )];
        app.open_menu(MenuTab::Exit);

        assert!(!app.problem_intro);
        assert!(app.notice.is_none());
        assert!(menu_problems(app.menu.as_ref().unwrap()).is_empty());

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::StartAfterSave,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("No config problems."));
        assert!(!rendered.contains("Recovered config"));
    }

    #[test]
    fn specific_root_recovery_warning_remains_a_menu_problem() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Root,
            "Ignored unknown root key \"surprise\".",
        )];
        app.open_menu(MenuTab::Exit);

        assert!(app.problem_intro);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("Config problems found"))
        );
        assert_eq!(menu_problems(app.menu.as_ref().unwrap()).len(), 1);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::StartAfterSave,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Ignored unknown root key"));
        assert!(!rendered.contains("No config problems."));
    }

    #[test]
    fn fresh_draft_root_warning_remains_a_menu_problem() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Root,
            "Could not parse /tmp/demons.toml; started a fresh draft. Save to overwrite the broken file.",
        )];
        app.open_menu(MenuTab::Exit);

        assert!(app.problem_intro);
        assert_eq!(menu_problems(app.menu.as_ref().unwrap()).len(), 1);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::StartAfterSave,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("started a fresh draft"));
    }

    #[test]
    fn exit_problem_messages_wrap_instead_of_clipping() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Root,
            "Could not parse /tmp/demons.toml; started a fresh draft. Save to overwrite the broken file.",
        )];
        app.open_menu(MenuTab::Exit);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 78, 24));
        render_menu(
            Rect::new(0, 0, 78, 24),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::StartAfterSave,
            None,
        );

        let rendered = (0..24)
            .map(|row| buffer_line(&buffer, row, 78))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("started a fresh draft"));
        assert!(rendered.contains("Save to overwrite the broken file."));
    }

    #[test]
    fn menu_problem_badges_render_on_left_side_of_rows() {
        let mut app = test_app();
        app.loaded.config.tasks[0].command = TaskCommand::Shell(String::new());
        app.open_menu(MenuTab::Tasks);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::ConfigureOnly,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("! one"));
    }

    #[test]
    fn recovered_field_warning_disappears_after_field_changes() {
        let mut app = test_app();
        app.loaded.config_problems = vec![ConfigProblem::warning(
            ConfigProblemLocation::Task {
                index: 0,
                field: Some(ConfigTaskField::Command),
            },
            "Recovered command for task \"one\".",
        )];
        app.open_menu(MenuTab::Tasks);
        assert!(
            menu_problems(app.menu.as_ref().unwrap())
                .iter()
                .any(|problem| problem.severity == ConfigProblemSeverity::Warning)
        );

        app.menu.as_mut().unwrap().draft.tasks[0].command =
            TaskCommand::Shell("echo fixed".to_owned());

        assert!(
            !menu_problems(app.menu.as_ref().unwrap())
                .iter()
                .any(|problem| problem.severity == ConfigProblemSeverity::Warning)
        );
    }

    #[test]
    fn menu_blocks_save_until_red_problems_are_fixed() {
        let temp = tempdir().unwrap();
        let mut app = test_app();
        app.loaded.path = temp.path().join(CONFIG_FILE);
        app.loaded.root = temp.path().to_path_buf();
        app.tasks[0].task.command = TaskCommand::Shell(String::new());
        app.loaded.config.tasks[0].command = TaskCommand::Shell(String::new());
        app.open_menu(MenuTab::Tasks);

        app.handle_menu_exit_action(MenuExitAction::SaveOnly)
            .unwrap();

        assert!(app.menu.is_some());
        assert!(!app.loaded.path.exists());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("Fix red config problems"))
        );
    }

    #[test]
    fn menu_problem_action_jumps_to_task_field() {
        let mut app = test_app();
        app.loaded.config.tasks[0].command = TaskCommand::Shell(String::new());
        app.open_menu(MenuTab::Exit);

        app.apply_menu_action(MenuAction::Problem(0)).unwrap();

        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.tab, MenuTab::Tasks);
        assert_eq!(menu.task_detail, Some(0));
        assert_eq!(
            task_field_to_config_field(task_detail_fields()[menu.cursor]),
            Some(ConfigTaskField::Command)
        );
    }

    #[test]
    fn menu_problem_action_jumps_to_setting_rows() {
        let mut app = test_app();
        app.loaded.config_problems = vec![
            ConfigProblem::warning(
                ConfigProblemLocation::Setting(ConfigSettingField::Layout),
                "Reset unsupported settings.layout \"tabs\" to \"grid\".",
            ),
            ConfigProblem::warning(
                ConfigProblemLocation::Setting(ConfigSettingField::Logging),
                "Reset unsupported settings.logging value.",
            ),
        ];
        app.open_menu(MenuTab::Exit);

        app.apply_menu_action(MenuAction::Problem(0)).unwrap();
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.tab, MenuTab::Settings);
        assert_eq!(menu.cursor, 0);

        app.apply_menu_action(MenuAction::Problem(1)).unwrap();
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.tab, MenuTab::Settings);
        assert_eq!(menu.cursor, 3);
    }

    #[test]
    fn settings_problem_badges_render_on_visible_rows() {
        let mut app = test_app();
        app.loaded.config_problems = vec![
            ConfigProblem::warning(
                ConfigProblemLocation::Setting(ConfigSettingField::Layout),
                "Reset unsupported settings.layout \"tabs\" to \"grid\".",
            ),
            ConfigProblem::warning(
                ConfigProblemLocation::Setting(ConfigSettingField::Logging),
                "Reset unsupported settings.logging value.",
            ),
        ];
        app.open_menu(MenuTab::Settings);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::Runtime,
            None,
        );
        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("! Layout: grid"));
        assert!(rendered.contains("! Logging: disabled"));
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
    fn settings_multi_click_adjusts_with_keyboard_and_discard_reverts() {
        let mut app = test_app();
        app.open_menu(MenuTab::Settings);
        app.move_menu_cursor(2);

        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.menu.as_ref().unwrap().tab, MenuTab::Settings);
        assert_eq!(
            app.menu.as_ref().unwrap().draft.settings.multi_click_ms,
            DEFAULT_MULTI_CLICK_MS + MULTI_CLICK_STEP_MS
        );
        assert_eq!(
            app.loaded.config.settings.multi_click_ms,
            DEFAULT_MULTI_CLICK_MS + MULTI_CLICK_STEP_MS
        );

        app.apply_menu_action(MenuAction::SetMultiClick(MAX_MULTI_CLICK_MS + 37))
            .unwrap();
        assert_eq!(
            app.menu.as_ref().unwrap().draft.settings.multi_click_ms,
            MAX_MULTI_CLICK_MS
        );

        app.handle_menu_exit_action(MenuExitAction::Discard)
            .unwrap();
        assert_eq!(
            app.loaded.config.settings.multi_click_ms,
            DEFAULT_MULTI_CLICK_MS
        );
        assert!(app.menu.is_none());
    }

    #[test]
    fn settings_multi_click_slider_accepts_mouse_drag() {
        let mut app = test_app();
        app.open_menu(MenuTab::Settings);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::Runtime,
            None,
        );
        let target = app
            .menu
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|hit| matches!(hit.action, MenuAction::SetMultiClick(MAX_MULTI_CLICK_MS)))
            .unwrap()
            .rect;

        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            target.x,
            target.y,
        ))
        .unwrap();

        assert_eq!(
            app.menu.as_ref().unwrap().draft.settings.multi_click_ms,
            MAX_MULTI_CLICK_MS
        );
        assert_eq!(
            app.loaded.config.settings.multi_click_ms,
            MAX_MULTI_CLICK_MS
        );
    }

    #[test]
    fn settings_leader_picker_selects_with_keyboard() {
        let mut app = test_app();
        app.open_menu(MenuTab::Settings);
        app.move_menu_cursor(1);

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
    fn env_editor_adds_value_without_comma_parsing() {
        let mut app = test_app();
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Env))
            .unwrap();
        assert_eq!(app.menu.as_ref().unwrap().env_task, Some(0));

        app.apply_menu_action(MenuAction::AddEnvVar).unwrap();
        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "API_TOKEN".to_owned();
            edit.cursor = char_count(&edit.value);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.menu.as_ref().unwrap().env_detail_key.as_deref(),
            Some("API_TOKEN")
        );
        assert_eq!(app.menu.as_ref().unwrap().env_cursor, 1);

        app.apply_menu_action(MenuAction::EnvField(EnvField::Value))
            .unwrap();
        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "alpha, beta = ok".to_owned();
            edit.cursor = char_count(&edit.value);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(
            app.menu.as_ref().unwrap().draft.tasks[0]
                .env
                .get("API_TOKEN"),
            Some(&"alpha, beta = ok".to_owned())
        );
    }

    #[test]
    fn env_editor_renames_and_deletes_variables() {
        let mut task = test_task("one");
        task.env.insert("RUST_LOG".to_owned(), "debug".to_owned());
        let mut app = test_app_with_tasks(vec![task]);
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Env))
            .unwrap();
        app.apply_menu_action(MenuAction::OpenEnvEntry(0)).unwrap();

        app.apply_menu_action(MenuAction::EnvField(EnvField::Key))
            .unwrap();
        {
            let edit = app.menu.as_mut().unwrap().edit.as_mut().unwrap();
            edit.value = "RUST_LOG_LEVEL".to_owned();
            edit.cursor = char_count(&edit.value);
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(
            !app.menu.as_ref().unwrap().draft.tasks[0]
                .env
                .contains_key("RUST_LOG")
        );
        assert_eq!(
            app.menu.as_ref().unwrap().draft.tasks[0]
                .env
                .get("RUST_LOG_LEVEL"),
            Some(&"debug".to_owned())
        );

        app.apply_menu_action(MenuAction::DeleteEnvVar).unwrap();

        assert!(app.menu.as_ref().unwrap().draft.tasks[0].env.is_empty());
        assert!(app.menu.as_ref().unwrap().env_detail_key.is_none());
    }

    #[test]
    fn env_editor_rows_render_as_clickable_menu_items() {
        let mut task = test_task("one");
        task.env.insert("BROWSER".to_owned(), "none".to_owned());
        let mut app = test_app_with_tasks(vec![task]);
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Env))
            .unwrap();

        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        render_menu(
            Rect::new(0, 0, 120, 40),
            &mut buffer,
            app.menu.as_mut().unwrap(),
            "Alt+J",
            MenuExitMode::Runtime,
            None,
        );

        let rendered = (0..40)
            .map(|row| buffer_line(&buffer, row, 120))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Environment for one"));
        assert!(rendered.contains("+ Add variable"));
        assert!(rendered.contains("BROWSER = none"));
        assert!(
            app.menu
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .any(|hit| { matches!(hit.action, MenuAction::OpenEnvEntry(0)) })
        );
    }

    #[test]
    fn unchanged_direct_command_edit_preserves_direct_command() {
        let mut app = test_app_with_tasks(vec![Task {
            command: TaskCommand::Direct(vec!["cargo".to_owned(), "run".to_owned()]),
            ..test_task("one")
        }]);
        app.open_menu(MenuTab::Tasks);
        app.apply_menu_action(MenuAction::OpenTask(0)).unwrap();
        app.apply_menu_action(MenuAction::TaskField(TaskField::Command))
            .unwrap();

        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(matches!(
            app.menu.as_ref().unwrap().draft.tasks[0].command,
            TaskCommand::Direct(ref parts) if parts == &["cargo", "run"]
        ));
        assert!(!app.menu.as_ref().unwrap().dirty());
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
                schema_version: crate::config::CURRENT_SCHEMA_VERSION,
                settings: Settings::default(),
                tasks,
            },
            config_warnings: Vec::new(),
            config_problems: Vec::new(),
            created_from_missing_file: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        App::new(
            loaded,
            tx,
            rx,
            Arc::new(Mutex::new(HashSet::new())),
            false,
            false,
        )
    }

    fn buffer_line(buffer: &Buffer, row: u16, width: u16) -> String {
        let mut line = String::new();
        for column in 0..width {
            line.push_str(buffer[(column, row)].symbol());
        }
        line
    }

    fn buffer_text(buffer: &Buffer, area: Rect) -> String {
        let mut text = String::new();
        for row in area.y..area.bottom() {
            if row > area.y {
                text.push('\n');
            }
            for column in area.x..area.right() {
                text.push_str(buffer[(column, row)].symbol());
            }
        }
        text
    }

    fn snowflake_positions(buffer: &Buffer, area: Rect) -> Vec<(u16, u16, String)> {
        let mut positions = Vec::new();
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                let symbol = buffer[(column, row)].symbol();
                if symbol == "·" || symbol == "❄" {
                    positions.push((column, row, symbol.to_owned()));
                }
            }
        }
        positions
    }

    fn tree_light_positions(buffer: &Buffer, area: Rect) -> Vec<(u16, u16, String)> {
        let mut positions = Vec::new();
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                let symbol = buffer[(column, row)].symbol();
                if symbol == "●" || symbol == "◆" || symbol == "•" {
                    positions.push((column, row, symbol.to_owned()));
                }
            }
        }
        positions
    }

    fn skater_head_positions(buffer: &Buffer, area: Rect) -> Vec<(u16, u16)> {
        let mut positions = Vec::new();
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                if buffer[(column, row)].symbol() == "o" {
                    positions.push((column, row));
                }
            }
        }
        positions
    }

    fn ice_highlight_positions(buffer: &Buffer, area: Rect, lake_y: u16) -> Vec<(u16, u16)> {
        let mut positions = Vec::new();
        for row in lake_y.saturating_add(1)..area.bottom() {
            for column in area.x..area.right() {
                if buffer[(column, row)].bg == THEME_ICE {
                    positions.push((column, row));
                }
            }
        }
        positions
    }

    fn sleigh_nose_positions(buffer: &Buffer, area: Rect) -> Vec<(u16, u16)> {
        let mut positions = Vec::new();
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                if buffer[(column, row)].symbol() == "●" {
                    positions.push((column, row));
                }
            }
        }
        positions
    }

    fn sleigh_star_positions(buffer: &Buffer, area: Rect) -> Vec<(u16, u16)> {
        sleigh_star_symbols(buffer, area)
            .into_iter()
            .map(|(column, row, _)| (column, row))
            .collect()
    }

    fn sleigh_star_symbols(buffer: &Buffer, area: Rect) -> Vec<(u16, u16, String)> {
        let mut positions = Vec::new();
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                let symbol = buffer[(column, row)].symbol();
                if symbol == "✦" || symbol == "·" {
                    positions.push((column, row, symbol.to_owned()));
                }
            }
        }
        positions
    }
}
