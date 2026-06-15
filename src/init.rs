use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::config::{
    Config, Layout, Leader, Settings, Task, TaskCommand, parse_file, validate_for_path,
};

pub struct InitResult {
    pub path: PathBuf,
    pub start: bool,
}

pub fn run(path: PathBuf) -> Result<InitResult> {
    if !io::stdin().is_terminal() {
        bail!("init requires an interactive terminal");
    }

    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    run_with_io(path, &mut input, &mut output)
}

fn run_with_io(
    path: PathBuf,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<InitResult> {
    let existing = if path.is_file() {
        Some(parse_file(&path)?)
    } else {
        None
    };

    let config = match existing {
        Some(config) => loop {
            let choice = prompt(
                input,
                output,
                &format!(
                    "A file already exists at {}. [E]dit existing, [F]resh start, [A]bort?",
                    path.display()
                ),
                "E",
            )?;
            match choice.to_ascii_lowercase().as_str() {
                "" | "e" | "edit" => break edit_config(input, output, config)?,
                "f" | "fresh" => break new_config(input, output)?,
                "a" | "abort" => {
                    return Ok(InitResult { path, start: false });
                }
                _ => writeln!(output, "Please choose E, F, or A.")?,
            }
        },
        None => new_config(input, output)?,
    };

    validate_for_path(&config, &path)?;
    let rendered = toml::to_string_pretty(&config).context("failed to render configuration")?;
    writeln!(output, "\n--- {} ---\n{rendered}---", path.display())?;
    if !prompt_yes_no(input, output, "Write this configuration?", true)? {
        return Ok(InitResult { path, start: false });
    }

    write_atomic(&path, rendered.as_bytes())?;
    writeln!(output, "Wrote {}.", path.display())?;
    writeln!(output, "Run 'demons' to start.")?;
    let start = prompt_yes_no(input, output, "Start demons now?", true)?;
    Ok(InitResult { path, start })
}

fn new_config(input: &mut impl BufRead, output: &mut impl Write) -> Result<Config> {
    writeln!(output, "Create a demons configuration.\n")?;
    let settings = prompt_settings(input, output, &Settings::default())?;
    let mut tasks = vec![prompt_task(input, output, None, 1)?];
    while prompt_yes_no(input, output, "Add another task?", true)? {
        let number = tasks.len() + 1;
        tasks.push(prompt_task(input, output, None, number)?);
    }
    Ok(Config { settings, tasks })
}

fn edit_config(
    input: &mut impl BufRead,
    output: &mut impl Write,
    config: Config,
) -> Result<Config> {
    let settings = prompt_settings(input, output, &config.settings)?;
    let mut tasks = Vec::with_capacity(config.tasks.len());
    for (index, task) in config.tasks.iter().enumerate() {
        tasks.push(prompt_task(input, output, Some(task), index + 1)?);
    }
    while prompt_yes_no(input, output, "Add a new task?", true)? {
        let number = tasks.len() + 1;
        tasks.push(prompt_task(input, output, None, number)?);
    }

    if tasks.len() > 1 {
        let names = tasks
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let remove = prompt(
            input,
            output,
            &format!("Remove tasks by name, comma-separated [{names}]"),
            "",
        )?;
        if !remove.trim().is_empty() {
            let removals = remove
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>();
            tasks.retain(|task| !removals.contains(&task.name.as_str()));
        }
    }
    if tasks.is_empty() {
        bail!("a configuration must contain at least one task");
    }
    Ok(Config { settings, tasks })
}

fn prompt_settings(
    input: &mut impl BufRead,
    output: &mut impl Write,
    current: &Settings,
) -> Result<Settings> {
    writeln!(output, "Project settings:")?;
    let layout = prompt(input, output, "Layout", "grid")?;
    if layout != "grid" {
        bail!("layout must be 'grid'");
    }

    let leader_default = match current.leader {
        Leader::AltJ => "alt-j",
        Leader::Tab => "tab",
        Leader::CtrlB => "ctrl-b",
        Leader::CtrlQ => "ctrl-q",
        Leader::CtrlBackslash => "ctrl-\\",
    };
    let leader = match prompt(input, output, "Leader key", leader_default)?.as_str() {
        "alt-j" => Leader::AltJ,
        "tab" => Leader::Tab,
        "ctrl-b" => Leader::CtrlB,
        "ctrl-q" => Leader::CtrlQ,
        "ctrl-\\" => Leader::CtrlBackslash,
        _ => bail!("leader must be one of: alt-j, tab, ctrl-b, ctrl-q, ctrl-\\"),
    };

    Ok(Settings {
        layout: Layout::Grid,
        leader,
        logging: false,
    })
}

fn prompt_task(
    input: &mut impl BufRead,
    output: &mut impl Write,
    current: Option<&Task>,
    number: usize,
) -> Result<Task> {
    writeln!(output, "\nTask {number}:")?;
    let name = required_prompt(
        input,
        output,
        "Name",
        current.map(|task| task.name.as_str()).unwrap_or(""),
    )?;
    let command_default = current
        .map(|task| task.command.display())
        .unwrap_or_default();
    let command = required_prompt(input, output, "Command", &command_default)?;
    let cwd_default = current
        .map(|task| task.cwd.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_owned());
    let cwd = PathBuf::from(prompt(input, output, "Working directory", &cwd_default)?);

    let env_default = current
        .map(|task| format_env(&task.env))
        .unwrap_or_default();
    let env_text = prompt(
        input,
        output,
        "Environment (KEY=value, comma-separated; '-' to clear)",
        &env_default,
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
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    if default.is_empty() {
        write!(output, "{label}: ")?;
    } else {
        write!(output, "{label} [{default}]: ")?;
    }
    output.flush()?;

    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("input closed");
    }
    let value = line.trim().to_owned();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value
    })
}

fn required_prompt(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    loop {
        let value = prompt(input, output, label, default)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }
        writeln!(output, "{label} cannot be empty.")?;
    }
}

pub fn prompt_yes_no(
    input: &mut impl BufRead,
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
}
