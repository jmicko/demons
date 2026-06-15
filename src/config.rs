use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "demons.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Settings::is_default")]
    pub settings: Settings,
    #[serde(rename = "task")]
    pub tasks: Vec<Task>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub layout: Layout,
    pub leader: Leader,
    #[serde(skip_serializing_if = "is_false")]
    pub logging: bool,
}

impl Settings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            layout: Layout::Grid,
            leader: Leader::AltJ,
            logging: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    #[default]
    Grid,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Leader {
    #[serde(rename = "alt-j")]
    #[default]
    AltJ,
    Tab,
    CtrlB,
    CtrlQ,
    #[serde(rename = "ctrl-\\")]
    CtrlBackslash,
}

impl Leader {
    pub fn label(self) -> &'static str {
        match self {
            Self::AltJ => "Alt-J",
            Self::Tab => "Tab",
            Self::CtrlB => "Ctrl-B",
            Self::CtrlQ => "Ctrl-Q",
            Self::CtrlBackslash => "Ctrl-\\",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub name: String,
    pub command: TaskCommand,
    #[serde(default = "default_cwd", skip_serializing_if = "is_default_cwd")]
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_on_change: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TaskCommand {
    Shell(String),
    Direct(Vec<String>),
}

impl TaskCommand {
    pub fn display(&self) -> String {
        match self {
            Self::Shell(command) => command.clone(),
            Self::Direct(parts) => parts.join(" "),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Shell(command) => command.trim().is_empty(),
            Self::Direct(parts) => parts.is_empty() || parts[0].trim().is_empty(),
        }
    }

    fn contains_nul(&self) -> bool {
        match self {
            Self::Shell(command) => command.contains('\0'),
            Self::Direct(parts) => parts.iter().any(|part| part.contains('\0')),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub root: PathBuf,
    pub config: Config,
}

impl LoadedConfig {
    pub fn load(path: PathBuf) -> Result<Self> {
        let path = absolute_path(path)?;
        let config = parse_file(&path)?;
        let root = path
            .parent()
            .context("config path has no parent directory")?
            .to_path_buf();
        let loaded = Self { path, root, config };
        loaded.validate()?;
        Ok(loaded)
    }

    pub fn validate(&self) -> Result<()> {
        validate_for_path(&self.config, &self.path)
    }

    pub fn task_cwd(&self, task: &Task) -> PathBuf {
        if task.cwd.is_absolute() {
            task.cwd.clone()
        } else {
            self.root.join(&task.cwd)
        }
    }
}

pub fn parse_file(path: &Path) -> Result<Config> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("failed to parse config {}", path.display()))
}

pub fn validate_for_path(config: &Config, path: &Path) -> Result<()> {
    if config.tasks.is_empty() {
        bail!("{} must define at least one [[task]]", path.display());
    }
    if config.settings.logging {
        bail!(
            "{}: settings.logging is reserved for a future release and cannot be enabled",
            path.display()
        );
    }

    let root = path
        .parent()
        .context("config path has no parent directory")?;
    let mut names = HashSet::new();
    for (index, task) in config.tasks.iter().enumerate() {
        let label = format!("task #{}", index + 1);
        if task.name.trim().is_empty() {
            bail!("{}: {label} has an empty name", path.display());
        }
        if task.name.trim() != task.name || task.name.chars().any(char::is_control) {
            bail!(
                "{}: task name {:?} has leading/trailing whitespace or control characters",
                path.display(),
                task.name
            );
        }
        if !names.insert(task.name.as_str()) {
            bail!("{}: duplicate task name {:?}", path.display(), task.name);
        }
        if task.command.is_empty() {
            bail!(
                "{}: task {:?} has an empty command",
                path.display(),
                task.name
            );
        }
        if task.command.contains_nul() {
            bail!(
                "{}: task {:?} command contains a NUL byte",
                path.display(),
                task.name
            );
        }
        for (key, value) in &task.env {
            if key.is_empty()
                || key.contains(['=', '\0'])
                || key.contains(char::is_whitespace)
                || value.contains('\0')
            {
                bail!(
                    "{}: task {:?} has an invalid environment entry for key {:?}",
                    path.display(),
                    task.name,
                    key
                );
            }
        }

        let cwd = if task.cwd.is_absolute() {
            task.cwd.clone()
        } else {
            root.join(&task.cwd)
        };
        if !cwd.is_dir() {
            bail!(
                "{}: cwd for task {:?} is not a directory: {}",
                path.display(),
                task.name,
                cwd.display()
            );
        }

        if task.watch.is_some() || task.run_on_change.is_some() || task.repeat.is_some() {
            bail!(
                "{}: task {:?} uses watch, run_on_change, or repeat; these fields are reserved for \
                 a future release",
                path.display(),
                task.name
            );
        }
    }
    Ok(())
}

pub fn discover(start: &Path) -> Result<Option<PathBuf>> {
    let mut directory = absolute_path(start.to_path_buf())?;
    if directory.is_file() {
        directory.pop();
    }

    loop {
        let candidate = directory.join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        if !directory.pop() {
            return Ok(None);
        }
    }
}

pub fn explicit_or_discover(explicit: Option<PathBuf>, start: &Path) -> Result<Option<PathBuf>> {
    match explicit {
        Some(path) => Ok(Some(absolute_path(path)?)),
        None => discover(start),
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()
            .context("failed to determine current directory")?
            .join(path))
    }
}

fn default_cwd() -> PathBuf {
    PathBuf::from(".")
}

fn is_default_cwd(path: &Path) -> bool {
    path == Path::new(".")
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_closest_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(CONFIG_FILE), "").unwrap();
        fs::write(root.join("a").join(CONFIG_FILE), "").unwrap();

        assert_eq!(
            discover(&nested).unwrap(),
            Some(root.join("a").join(CONFIG_FILE))
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        let error = toml::from_str::<Config>(
            r#"
                surprise = true
                [[task]]
                name = "test"
                command = "echo ok"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn parses_shell_and_direct_commands() {
        let config: Config = toml::from_str(
            r#"
                [[task]]
                name = "shell"
                command = "echo shell"

                [[task]]
                name = "direct"
                command = ["echo", "direct"]
            "#,
        )
        .unwrap();

        assert!(matches!(
            config.tasks[0].command,
            TaskCommand::Shell(ref command) if command == "echo shell"
        ));
        assert!(matches!(
            config.tasks[1].command,
            TaskCommand::Direct(ref command) if command == &["echo", "direct"]
        ));
    }

    #[test]
    fn allows_empty_direct_arguments() {
        let config: Config = toml::from_str(
            r#"
                [[task]]
                name = "direct"
                command = ["printf", "%s", ""]
            "#,
        )
        .unwrap();

        assert!(!config.tasks[0].command.is_empty());
    }

    #[test]
    fn validates_relative_working_directories_and_unique_names() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("web")).unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let valid: Config = toml::from_str(
            r#"
                [[task]]
                name = "server"
                command = "echo server"

                [[task]]
                name = "web"
                command = "echo web"
                cwd = "web"
            "#,
        )
        .unwrap();
        validate_for_path(&valid, &path).unwrap();

        let duplicate: Config = toml::from_str(
            r#"
                [[task]]
                name = "server"
                command = "echo one"

                [[task]]
                name = "server"
                command = "echo two"
            "#,
        )
        .unwrap();
        assert!(validate_for_path(&duplicate, &path).is_err());
    }

    #[test]
    fn defaults_to_alt_j_leader() {
        let config: Config = toml::from_str(
            r#"
                [[task]]
                name = "server"
                command = "echo ready"
            "#,
        )
        .unwrap();

        assert_eq!(config.settings.leader, Leader::AltJ);
    }
}
