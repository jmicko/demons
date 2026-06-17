use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

use crate::config::{
    Config, Layout, Leader, Settings, Task, TaskCommand, parse_file, validate_for_path,
};

pub struct InitResult {
    pub path: PathBuf,
    pub start: bool,
}

trait PromptSource {
    fn prompt(&mut self, output: &mut impl Write, label: &str, default: &str) -> Result<String>;

    fn prompt_with_default(
        &mut self,
        output: &mut impl Write,
        label: &str,
        default: &str,
    ) -> Result<String> {
        self.prompt(output, label, default)
    }
}

struct LinePrompt<'a, R: BufRead> {
    input: &'a mut R,
}

impl<R: BufRead> PromptSource for LinePrompt<'_, R> {
    fn prompt(&mut self, output: &mut impl Write, label: &str, default: &str) -> Result<String> {
        write!(output, "{}", prompt_prefix(label, default))?;
        output.flush()?;

        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("input closed");
        }
        let value = line.trim().to_owned();
        Ok(if value.is_empty() {
            default.to_owned()
        } else {
            value
        })
    }
}

struct TerminalPrompt;

impl PromptSource for TerminalPrompt {
    fn prompt(&mut self, output: &mut impl Write, label: &str, default: &str) -> Result<String> {
        let prefix = prompt_prefix(label, default);
        write!(output, "{prefix}")?;
        output.flush()?;
        let value = read_edited_line(output, &prefix, "")?;
        Ok(if value.trim().is_empty() {
            default.to_owned()
        } else {
            value.trim().to_owned()
        })
    }

    fn prompt_with_default(
        &mut self,
        output: &mut impl Write,
        label: &str,
        default: &str,
    ) -> Result<String> {
        let prefix = prompt_prefix(label, default);
        write!(output, "{prefix}{default}")?;
        output.flush()?;
        let value = read_edited_line(output, &prefix, default)?;
        Ok(if value.trim().is_empty() {
            default.to_owned()
        } else {
            value.trim().to_owned()
        })
    }
}

pub fn run(path: PathBuf) -> Result<InitResult> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("init requires an interactive terminal");
    }

    let mut input = TerminalPrompt;
    let mut output = io::stdout().lock();
    run_flow(path, &mut input, &mut output)
}

#[cfg(test)]
fn run_with_io(
    path: PathBuf,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<InitResult> {
    let mut input = LinePrompt { input };
    run_flow(path, &mut input, output)
}

fn run_flow(
    path: PathBuf,
    input: &mut impl PromptSource,
    output: &mut impl Write,
) -> Result<InitResult> {
    let existing = if path.is_file() {
        Some(parse_file(&path)?)
    } else {
        None
    };

    let config = match existing {
        Some(config) => loop {
            let choice = prompt_choice(
                input,
                output,
                &format!("A file already exists at {}.", path.display()),
                &[("e", "Edit existing"), ("f", "Fresh start"), ("a", "Abort")],
                0,
            )?;
            match choice.to_ascii_lowercase().as_str() {
                "e" => break edit_config(input, output, config, config_root(&path))?,
                "f" => break new_config(input, output, config_root(&path))?,
                "a" => {
                    return Ok(InitResult { path, start: false });
                }
                _ => writeln!(output, "Please choose E, F, or A.")?,
            }
        },
        None => new_config(input, output, config_root(&path))?,
    };

    validate_for_path(&config, &path)?;
    let rendered = toml::to_string_pretty(&config).context("failed to render configuration")?;
    writeln!(output, "\n--- {} ---\n{rendered}---", path.display())?;
    if !prompt_yes_no_with(input, output, "Write this configuration?", true)? {
        return Ok(InitResult { path, start: false });
    }

    write_atomic(&path, rendered.as_bytes())?;
    writeln!(output, "Wrote {}.", path.display())?;
    writeln!(output, "Run 'demons' to start.")?;
    let start = prompt_yes_no_with(input, output, "Start demons now?", true)?;
    Ok(InitResult { path, start })
}

fn new_config(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    root: &Path,
) -> Result<Config> {
    writeln!(output, "Create a demons configuration.\n")?;
    let settings = prompt_settings(input, output, &Settings::default())?;
    let starters = detected_task_templates(root);
    let first_task = prompt_detected_task(input, output, starters.clone())?;
    let mut tasks = vec![prompt_task(input, output, first_task.as_ref(), 1)?];
    while prompt_yes_no_with(input, output, "Add another task?", true)? {
        let number = tasks.len() + 1;
        let detected = starters
            .iter()
            .filter(|candidate| !tasks.iter().any(|task| task.name == candidate.name))
            .cloned()
            .collect();
        let task = prompt_detected_task(input, output, detected)?;
        tasks.push(prompt_task(input, output, task.as_ref(), number)?);
    }
    Ok(Config { settings, tasks })
}

fn edit_config(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    config: Config,
    root: &Path,
) -> Result<Config> {
    let settings = prompt_settings(input, output, &config.settings)?;
    let mut tasks = Vec::with_capacity(config.tasks.len());
    for (index, task) in config.tasks.iter().enumerate() {
        tasks.push(prompt_task(input, output, Some(task), index + 1)?);
    }
    while prompt_yes_no_with(input, output, "Add a new task?", true)? {
        let number = tasks.len() + 1;
        let detected = detected_task_templates(root)
            .into_iter()
            .filter(|candidate| !tasks.iter().any(|task| task.name == candidate.name))
            .collect();
        let task = prompt_detected_task(input, output, detected)?;
        tasks.push(prompt_task(input, output, task.as_ref(), number)?);
    }

    if tasks.len() > 1 {
        writeln!(output, "\nRemove tasks:")?;
        for (index, task) in tasks.iter().enumerate() {
            writeln!(output, "  {}. {}", index + 1, task.name)?;
        }
        let remove = prompt(
            input,
            output,
            "Remove task numbers/names, comma-separated; blank to keep all",
            "",
        )?;
        if !remove.trim().is_empty() {
            let removals = parse_task_removals(&remove, &tasks)?;
            tasks.retain(|task| !removals.contains(&task.name));
        }
    }
    if tasks.is_empty() {
        bail!("a configuration must contain at least one task");
    }
    Ok(Config { settings, tasks })
}

fn config_root(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

fn detected_task_templates(root: &Path) -> Vec<Task> {
    let mut tasks = Vec::new();
    if root.join("Cargo.toml").is_file() {
        tasks.push(template_task("server", "cargo run"));
    }

    tasks.extend(detected_package_tasks(root));

    if root.join("Makefile").is_file() || root.join("makefile").is_file() {
        tasks.push(template_task("make", "make"));
    }
    tasks
}

#[derive(serde::Deserialize)]
struct PackageJson {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

fn detected_package_tasks(root: &Path) -> Vec<Task> {
    let package_json = root.join("package.json");
    let Ok(source) = fs::read_to_string(package_json) else {
        return Vec::new();
    };
    let Ok(package) = serde_json::from_str::<PackageJson>(&source) else {
        return Vec::new();
    };

    [
        ("dev", "web"),
        ("start", "start"),
        ("serve", "serve"),
        ("watch", "watch"),
    ]
    .into_iter()
    .filter(|(script, _)| package.scripts.contains_key(*script))
    .map(|(script, name)| template_task(name, &package_script_command(root, script)))
    .collect()
}

fn package_script_command(root: &Path, script: &str) -> String {
    if root.join("pnpm-lock.yaml").is_file() {
        format!("pnpm run {script}")
    } else if root.join("yarn.lock").is_file() {
        format!("yarn {script}")
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        format!("bun run {script}")
    } else {
        format!("npm run {script}")
    }
}

fn template_task(name: &str, command: &str) -> Task {
    Task {
        name: name.to_owned(),
        command: TaskCommand::Shell(command.to_owned()),
        cwd: PathBuf::from("."),
        env: BTreeMap::new(),
        watch: None,
        run_on_change: None,
        repeat: None,
    }
}

fn prompt_detected_task(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    tasks: Vec<Task>,
) -> Result<Option<Task>> {
    if tasks.is_empty() {
        return Ok(None);
    }

    writeln!(output, "Detected task starters:")?;
    for (index, task) in tasks.iter().enumerate() {
        writeln!(
            output,
            "  {}. {} ({})",
            index + 1,
            task.name,
            task.command.display()
        )?;
    }
    writeln!(output, "  {}. Custom task", tasks.len() + 1)?;

    loop {
        let answer = prompt(input, output, "Choose first task", "1")?;
        if let Ok(number) = answer.parse::<usize>() {
            if (1..=tasks.len()).contains(&number) {
                return Ok(Some(tasks[number - 1].clone()));
            }
            if number == tasks.len() + 1 {
                return Ok(None);
            }
        }
        writeln!(output, "Please choose one of the listed task starters.")?;
    }
}

fn prompt_settings(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    current: &Settings,
) -> Result<Settings> {
    writeln!(output, "Project settings:")?;
    let layout = prompt_choice(input, output, "Layout", &[("grid", "grid")], 0)?;
    if layout != "grid" {
        bail!("layout must be 'grid'");
    }

    let leader_default = match current.leader {
        Leader::AltJ => 0,
        Leader::AltBacktick => 1,
        Leader::Tab => 2,
        Leader::CtrlB => 3,
        Leader::CtrlQ => 4,
        Leader::CtrlBackslash => 5,
    };
    let leader_choice = prompt_choice(
        input,
        output,
        "Leader key",
        &[
            ("alt-j", "Alt-J"),
            ("alt-backtick", "Alt-`"),
            ("tab", "Tab"),
            ("ctrl-b", "Ctrl-B"),
            ("ctrl-q", "Ctrl-Q"),
            ("ctrl-\\", "Ctrl-\\"),
        ],
        leader_default,
    )?;
    let leader = match leader_choice.as_str() {
        "alt-j" => Leader::AltJ,
        "alt-backtick" => Leader::AltBacktick,
        "tab" => Leader::Tab,
        "ctrl-b" => Leader::CtrlB,
        "ctrl-q" => Leader::CtrlQ,
        "ctrl-\\" => Leader::CtrlBackslash,
        _ => bail!("leader must be one of: alt-j, alt-backtick, tab, ctrl-b, ctrl-q, ctrl-\\"),
    };

    Ok(Settings {
        layout: Layout::Grid,
        leader,
        logging: false,
    })
}

fn prompt_task(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    current: Option<&Task>,
    number: usize,
) -> Result<Task> {
    writeln!(output, "\nTask {number}:")?;
    let name_default = current.map(|task| task.name.as_str()).unwrap_or("");
    let name = required_task_prompt(input, output, "Name", name_default, current.is_some())?;
    let command_default = current
        .map(|task| task.command.display())
        .unwrap_or_default();
    let command = required_task_prompt(
        input,
        output,
        "Command",
        &command_default,
        current.is_some(),
    )?;
    let cwd_default = current
        .map(|task| task.cwd.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_owned());
    let cwd = PathBuf::from(task_prompt(
        input,
        output,
        "Working directory",
        &cwd_default,
        current.is_some(),
    )?);

    let env_default = current
        .map(|task| format_env(&task.env))
        .unwrap_or_default();
    let env_text = task_prompt(
        input,
        output,
        "Environment (KEY=value, comma-separated; '-' to clear)",
        &env_default,
        current.is_some() && !env_default.is_empty(),
    )?;
    let env = if env_text == "-" {
        BTreeMap::new()
    } else {
        parse_env(&env_text)?
    };

    Ok(Task {
        name,
        command: match current.map(|task| &task.command) {
            Some(TaskCommand::Direct(parts)) if command == command_default => {
                TaskCommand::Direct(parts.clone())
            }
            _ => TaskCommand::Shell(command),
        },
        cwd,
        env,
        watch: None,
        run_on_change: None,
        repeat: None,
    })
}

fn prompt(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    input.prompt(output, label, default)
}

fn task_prompt(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    label: &str,
    default: &str,
    prefill_default: bool,
) -> Result<String> {
    if prefill_default {
        input.prompt_with_default(output, label, default)
    } else {
        prompt(input, output, label, default)
    }
}

fn required_task_prompt(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    label: &str,
    default: &str,
    prefill_default: bool,
) -> Result<String> {
    loop {
        let value = task_prompt(input, output, label, default, prefill_default)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }
        writeln!(output, "{label} cannot be empty.")?;
    }
}

fn prompt_choice(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    label: &str,
    choices: &[(&str, &str)],
    default_index: usize,
) -> Result<String> {
    if choices.is_empty() {
        bail!("choice prompt requires at least one option");
    }
    writeln!(output, "{label}")?;
    for (index, (_, description)) in choices.iter().enumerate() {
        writeln!(output, "  {}. {}", index + 1, description)?;
    }

    loop {
        let default = (default_index + 1).min(choices.len()).to_string();
        let answer = prompt(input, output, "Choose", &default)?;
        let normalized = answer.trim().to_ascii_lowercase();
        if let Ok(number) = normalized.parse::<usize>() {
            if (1..=choices.len()).contains(&number) {
                return Ok(choices[number - 1].0.to_owned());
            }
        }
        if let Some((value, _)) = choices.iter().find(|(value, description)| {
            normalized == *value || normalized == description.to_ascii_lowercase()
        }) {
            return Ok((*value).to_owned());
        }
        writeln!(output, "Please choose one of the listed options.")?;
    }
}

fn parse_task_removals(value: &str, tasks: &[Task]) -> Result<Vec<String>> {
    let mut removals = Vec::new();
    for token in value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        if let Ok(number) = token.parse::<usize>() {
            let Some(task) = number.checked_sub(1).and_then(|index| tasks.get(index)) else {
                bail!("no task numbered {number}");
            };
            removals.push(task.name.clone());
        } else if tasks.iter().any(|task| task.name == token) {
            removals.push(token.to_owned());
        } else {
            bail!("no task named {token:?}");
        }
    }
    removals.sort();
    removals.dedup();
    Ok(removals)
}

pub fn prompt_yes_no(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default_yes: bool,
) -> Result<bool> {
    let mut input = LinePrompt { input };
    prompt_yes_no_with(&mut input, output, label, default_yes)
}

fn prompt_yes_no_with(
    input: &mut impl PromptSource,
    output: &mut impl Write,
    label: &str,
    default_yes: bool,
) -> Result<bool> {
    loop {
        let suffix = if default_yes { "Y/n" } else { "y/N" };
        let answer = prompt(input, output, &format!("{label} [{suffix}]"), "")?;
        match answer.to_ascii_lowercase().as_str() {
            "" => return Ok(default_yes),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Please answer yes or no.")?,
        }
    }
}

fn prompt_prefix(label: &str, default: &str) -> String {
    if default.is_empty() {
        format!("{label}: ")
    } else {
        format!("{label} [{default}]: ")
    }
}

fn read_edited_line(output: &mut impl Write, prefix: &str, initial: &str) -> Result<String> {
    let _guard = RawModeGuard::enter()?;
    let mut buffer = initial.to_owned();
    let mut cursor = char_len(&buffer);

    loop {
        let event = event::read().context("failed to read terminal input")?;
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        match key.code {
            KeyCode::Enter | KeyCode::Char('\n' | '\r') => {
                writeln!(output, "\r")?;
                output.flush()?;
                return Ok(buffer);
            }
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                bail!("input cancelled");
            }
            KeyCode::Char('d' | 'D')
                if key.modifiers.contains(KeyModifiers::CONTROL) && buffer.is_empty() =>
            {
                bail!("input closed");
            }
            KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                cursor = 0;
            }
            KeyCode::Char('e' | 'E') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                cursor = char_len(&buffer);
            }
            KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                buffer.clear();
                cursor = 0;
            }
            KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let index = byte_index(&buffer, cursor);
                buffer.truncate(index);
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let index = byte_index(&buffer, cursor);
                buffer.insert(index, character);
                cursor += 1;
            }
            KeyCode::Backspace if cursor > 0 => {
                let start = byte_index(&buffer, cursor - 1);
                let end = byte_index(&buffer, cursor);
                buffer.replace_range(start..end, "");
                cursor -= 1;
            }
            KeyCode::Delete if cursor < char_len(&buffer) => {
                let start = byte_index(&buffer, cursor);
                let end = byte_index(&buffer, cursor + 1);
                buffer.replace_range(start..end, "");
            }
            KeyCode::Left => cursor = cursor.saturating_sub(1),
            KeyCode::Right => cursor = (cursor + 1).min(char_len(&buffer)),
            KeyCode::Home => cursor = 0,
            KeyCode::End => cursor = char_len(&buffer),
            _ => {}
        }

        redraw_line(output, prefix, &buffer, cursor)?;
    }
}

fn redraw_line(
    output: &mut impl Write,
    prefix: &str,
    buffer: &str,
    cursor: usize,
) -> io::Result<()> {
    write!(output, "\r\x1b[2K{prefix}{buffer}")?;
    let distance_from_end = char_len(buffer).saturating_sub(cursor);
    if distance_from_end > 0 {
        write!(output, "\x1b[{distance_from_end}D")?;
    }
    output.flush()
}

fn char_len(value: &str) -> usize {
    value.chars().count()
}

fn byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        Ok(Self { active: true })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            disable_raw_mode().ok();
            self.active = false;
        }
    }
}

fn parse_env(value: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    if value.trim().is_empty() {
        return Ok(env);
    }
    for pair in value.split(',') {
        let (key, value) = pair
            .trim()
            .split_once('=')
            .with_context(|| format!("environment entry {:?} must use KEY=value", pair.trim()))?;
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            bail!("invalid environment variable name {key:?}");
        }
        env.insert(key.to_owned(), value.trim().to_owned());
    }
    Ok(env)
}

fn format_env(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("config path has no parent directory")?;
    if !parent.is_dir() {
        bail!("config directory does not exist: {}", parent.display());
    }
    let temporary = parent.join(format!(".{}.tmp", crate::config::CONFIG_FILE));
    fs::write(&temporary, contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_environment_entries() {
        let env = parse_env("RUST_LOG=debug, BROWSER=none").unwrap();
        assert_eq!(env["RUST_LOG"], "debug");
        assert_eq!(env["BROWSER"], "none");
    }

    #[test]
    fn rejects_environment_without_equals() {
        assert!(parse_env("RUST_LOG").is_err());
    }

    #[test]
    fn parses_task_removals_by_number_and_name() {
        let tasks = vec![
            Task {
                name: "api".to_owned(),
                command: TaskCommand::Shell("echo api".to_owned()),
                cwd: PathBuf::from("."),
                env: BTreeMap::new(),
                watch: None,
                run_on_change: None,
                repeat: None,
            },
            Task {
                name: "web".to_owned(),
                command: TaskCommand::Shell("echo web".to_owned()),
                cwd: PathBuf::from("."),
                env: BTreeMap::new(),
                watch: None,
                run_on_change: None,
                repeat: None,
            },
        ];

        assert_eq!(
            parse_task_removals("1, web", &tasks).unwrap(),
            ["api", "web"]
        );
        assert!(parse_task_removals("3", &tasks).is_err());
    }

    #[test]
    fn creates_a_config_from_line_input() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(crate::config::CONFIG_FILE);
        let answers = b"\n\nserver\necho ready\n\n\nn\n\nn\n";
        let mut input = Cursor::new(answers);
        let mut output = Vec::new();

        let result = run_with_io(path.clone(), &mut input, &mut output).unwrap();

        assert!(!result.start);
        let config = parse_file(&path).unwrap();
        assert_eq!(config.tasks.len(), 1);
        assert_eq!(config.tasks[0].name, "server");
        assert!(matches!(
            config.tasks[0].command,
            TaskCommand::Shell(ref command) if command == "echo ready"
        ));
    }

    #[test]
    fn creates_a_config_with_numbered_setting_choices() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(crate::config::CONFIG_FILE);
        let answers = b"\n4\nserver\necho ready\n\n\nn\n\nn\n";
        let mut input = Cursor::new(answers);
        let mut output = Vec::new();

        run_with_io(path.clone(), &mut input, &mut output).unwrap();

        let config = parse_file(&path).unwrap();
        assert_eq!(config.settings.leader, Leader::CtrlB);
    }

    #[test]
    fn offers_detected_cargo_task_as_first_task_default() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        let path = temp.path().join(crate::config::CONFIG_FILE);
        let answers = b"\n\n\n\n\n\n\nn\n\nn\n";
        let mut input = Cursor::new(answers);
        let mut output = Vec::new();

        run_with_io(path.clone(), &mut input, &mut output).unwrap();

        let config = parse_file(&path).unwrap();
        assert_eq!(config.tasks[0].name, "server");
        assert!(matches!(
            config.tasks[0].command,
            TaskCommand::Shell(ref command) if command == "cargo run"
        ));
    }

    #[test]
    fn offers_remaining_detected_tasks_when_adding_to_new_config() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        let path = temp.path().join(crate::config::CONFIG_FILE);
        let answers = [
            "", "", "", "", "", "", "", "", "", "", "", "", "", "n", "", "n",
        ]
        .join("\n")
            + "\n";
        let mut input = Cursor::new(answers.into_bytes());
        let mut output = Vec::new();

        run_with_io(path.clone(), &mut input, &mut output).unwrap();

        let config = parse_file(&path).unwrap();
        assert_eq!(config.tasks.len(), 2);
        assert_eq!(config.tasks[0].name, "server");
        assert_eq!(config.tasks[1].name, "web");
        assert!(matches!(
            config.tasks[1].command,
            TaskCommand::Shell(ref command) if command == "npm run dev"
        ));
    }

    #[test]
    fn detects_package_manager_dev_script_starter() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        fs::write(temp.path().join("pnpm-lock.yaml"), "").unwrap();

        let templates = detected_task_templates(temp.path());

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "web");
        assert!(matches!(
            templates[0].command,
            TaskCommand::Shell(ref command) if command == "pnpm run dev"
        ));
    }

    #[test]
    fn package_detection_requires_actual_script_entries() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"dependencies":{"dev":"latest"}}"#,
        )
        .unwrap();

        assert!(detected_task_templates(temp.path()).is_empty());
    }
}
