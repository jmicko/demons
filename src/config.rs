use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "demons.toml";
pub const CURRENT_SCHEMA_VERSION: u32 = 3;
pub const DEFAULT_MULTI_CLICK_MS: u64 = 500;
pub const MIN_MULTI_CLICK_MS: u64 = 150;
pub const MAX_MULTI_CLICK_MS: u64 = 1000;
pub const MULTI_CLICK_STEP_MS: u64 = 50;
const MAX_RECOVERY_WARNINGS: usize = 6;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    #[serde(rename = "task")]
    pub tasks: Vec<Task>,
    #[serde(default, rename = "terminal", skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalPane>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            settings: Settings::default(),
            tasks: Vec::new(),
            terminals: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub layout: Layout,
    pub leader: Leader,
    #[serde(default = "default_multi_click_ms")]
    pub multi_click_ms: u64,
    pub logging: bool,
    #[serde(default)]
    pub mcp_access: McpAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_scope_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            layout: Layout::Grid,
            leader: Leader::AltJ,
            multi_click_ms: DEFAULT_MULTI_CLICK_MS,
            logging: false,
            mcp_access: McpAccess::Off,
            mcp_scope_id: None,
        }
    }
}

fn default_multi_click_ms() -> u64 {
    DEFAULT_MULTI_CLICK_MS
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAccess {
    #[default]
    Off,
    ReadOnly,
    Full,
}

impl McpAccess {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::ReadOnly => "Read only",
            Self::Full => "Full",
        }
    }

    pub fn allows_read(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::Full)
    }

    fn from_config(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "read_only" => Some(Self::ReadOnly),
            "full" => Some(Self::Full),
            _ => None,
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
    #[serde(rename = "alt-backtick")]
    AltBacktick,
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
            Self::AltBacktick => "Alt-`",
            Self::Tab => "Tab",
            Self::CtrlB => "Ctrl-B",
            Self::CtrlQ => "Ctrl-Q",
            Self::CtrlBackslash => "Ctrl-\\",
        }
    }

    pub fn uses_escape_alt_encoding(self) -> bool {
        matches!(self, Self::AltJ | Self::AltBacktick)
    }

    fn from_config(value: &str) -> Option<Self> {
        match value {
            "alt-j" => Some(Self::AltJ),
            "alt-backtick" => Some(Self::AltBacktick),
            "tab" => Some(Self::Tab),
            "ctrl-b" => Some(Self::CtrlB),
            "ctrl-q" => Some(Self::CtrlQ),
            "ctrl-\\" => Some(Self::CtrlBackslash),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub name: String,
    pub command: TaskCommand,
    #[serde(default = "default_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_delay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_on_change: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TerminalPane {
    pub name: String,
    #[serde(default = "default_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub root: PathBuf,
    pub config: Config,
    pub config_warnings: Vec<String>,
    pub config_problems: Vec<ConfigProblem>,
    pub created_from_missing_file: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigProblem {
    pub severity: ConfigProblemSeverity,
    pub location: ConfigProblemLocation,
    pub message: String,
}

impl ConfigProblem {
    pub fn error(location: ConfigProblemLocation, message: impl Into<String>) -> Self {
        Self {
            severity: ConfigProblemSeverity::Error,
            location,
            message: message.into(),
        }
    }

    pub fn warning(location: ConfigProblemLocation, message: impl Into<String>) -> Self {
        Self {
            severity: ConfigProblemSeverity::Warning,
            location,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigProblemSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigProblemLocation {
    Root,
    Settings,
    Setting(ConfigSettingField),
    Tasks,
    Task {
        index: usize,
        field: Option<ConfigTaskField>,
    },
    Terminal {
        index: usize,
        field: Option<ConfigTerminalField>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigTerminalField {
    Name,
    Cwd,
    Env,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSettingField {
    Layout,
    Leader,
    MultiClick,
    Logging,
    McpAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigTaskField {
    Name,
    Command,
    Cwd,
    Env,
    Dependencies,
    StartDelay,
}

impl LoadedConfig {
    pub fn load(path: PathBuf) -> Result<Self> {
        let path = absolute_path(path)?;
        let parsed = parse_file_with_metadata(&path)?;
        let root = path
            .parent()
            .context("config path has no parent directory")?
            .to_path_buf();
        let loaded = Self {
            path,
            root,
            config: parsed.config,
            config_warnings: parsed.warnings,
            config_problems: parsed.problems,
            created_from_missing_file: false,
        };
        loaded.validate()?;
        if parsed.needs_normalization {
            loaded.save()?;
        }
        Ok(loaded)
    }

    pub fn load_unvalidated_or_default(path: PathBuf) -> Result<Self> {
        let path = absolute_path(path)?;
        let created_from_missing_file = !path.is_file();
        let parsed = if created_from_missing_file {
            ParsedConfig {
                config: Config::default(),
                needs_normalization: false,
                warnings: Vec::new(),
                problems: Vec::new(),
            }
        } else {
            parse_file_for_configurator(&path)?
        };
        let root = path
            .parent()
            .context("config path has no parent directory")?
            .to_path_buf();
        let loaded = Self {
            path,
            root,
            config: parsed.config,
            config_warnings: parsed.warnings,
            config_problems: parsed.problems,
            created_from_missing_file,
        };
        if parsed.needs_normalization && loaded.validate().is_ok() {
            loaded.save()?;
        }
        Ok(loaded)
    }

    pub fn validate(&self) -> Result<()> {
        validate_for_path(&self.config, &self.path)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let text = toml::to_string_pretty(&self.config)
            .with_context(|| format!("failed to serialize config {}", self.path.display()))?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write config {}", self.path.display()))
    }

    pub fn task_cwd(&self, task: &Task) -> PathBuf {
        if task.cwd.is_absolute() {
            task.cwd.clone()
        } else {
            self.root.join(&task.cwd)
        }
    }

    pub fn terminal_cwd(&self, terminal: &TerminalPane) -> PathBuf {
        if terminal.cwd.is_absolute() {
            terminal.cwd.clone()
        } else {
            self.root.join(&terminal.cwd)
        }
    }
}

#[cfg(test)]
pub fn parse_file(path: &Path) -> Result<Config> {
    Ok(parse_file_with_metadata(path)?.config)
}

#[derive(Clone, Debug)]
struct ParsedConfig {
    config: Config,
    needs_normalization: bool,
    warnings: Vec<String>,
    problems: Vec<ConfigProblem>,
}

fn parse_file_with_metadata(path: &Path) -> Result<ParsedConfig> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    parse_source(&source, path)
}

fn parse_source(source: &str, path: &Path) -> Result<ParsedConfig> {
    let schema = detect_schema_version(source, path)?;
    if schema.version > CURRENT_SCHEMA_VERSION {
        bail!(
            "{} uses config schema_version {}, but this demons release supports schema_version {}",
            path.display(),
            schema.version,
            CURRENT_SCHEMA_VERSION
        );
    }
    let mut config: Config = toml::from_str(source)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let needs_normalization = schema.was_missing || schema.version < CURRENT_SCHEMA_VERSION;
    if needs_normalization {
        config.schema_version = CURRENT_SCHEMA_VERSION;
    }
    Ok(ParsedConfig {
        config,
        needs_normalization,
        warnings: Vec::new(),
        problems: Vec::new(),
    })
}

fn parse_file_for_configurator(path: &Path) -> Result<ParsedConfig> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    match parse_source(&source, path) {
        Ok(mut parsed) => {
            let warning_count = parsed.warnings.len();
            repair_config_for_configurator(
                &mut parsed.config,
                path,
                &mut parsed.warnings,
                &mut parsed.problems,
            );
            if parsed.warnings.len() > warning_count {
                parsed.needs_normalization = false;
            }
            Ok(parsed)
        }
        Err(strict_error) => recover_config_source(&source, path, strict_error).or_else(|error| {
            if toml::from_str::<toml::Value>(&source).is_err() {
                Ok(fresh_config_after_unrecoverable_toml(path, error))
            } else {
                Err(error)
            }
        }),
    }
}

fn recover_config_source(
    source: &str,
    path: &Path,
    _strict_error: anyhow::Error,
) -> Result<ParsedConfig> {
    let recovered_source = recover_missing_assignment_values(source);
    let recovery_source = recovered_source.as_deref().unwrap_or(source);
    let value = toml::from_str::<toml::Value>(recovery_source)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let root = value
        .as_table()
        .context("parsed TOML document was not a table")?;
    let schema_warning = recoverable_schema_warning(root.get("schema_version"), path)?;
    let mut warnings = Vec::new();
    let mut problems = Vec::new();
    push_recovery_warning(
        &mut warnings,
        &mut problems,
        ConfigProblemLocation::Root,
        "Recovered config after a parse or schema mismatch.".to_owned(),
    );
    if let Some(warning) = schema_warning {
        push_recovery_warning(
            &mut warnings,
            &mut problems,
            ConfigProblemLocation::Root,
            warning,
        );
    }
    warn_unknown_keys(
        "root",
        root.keys().map(String::as_str),
        &["schema_version", "settings", "task", "terminal"],
        ConfigProblemLocation::Root,
        &mut warnings,
        &mut problems,
    );

    let settings = recover_settings(root.get("settings"), &mut warnings, &mut problems);
    let mut recovered = recover_tasks(root.get("task"), &mut warnings, &mut problems);
    let terminals = recover_terminals(root.get("terminal"), &mut warnings, &mut problems);
    scrub_recovered_dependencies(&mut recovered, &mut warnings, &mut problems);

    let config = Config {
        schema_version: CURRENT_SCHEMA_VERSION,
        settings,
        tasks: recovered.into_iter().map(|task| task.task).collect(),
        terminals,
    };
    problems.extend(config_blocking_problems(&config, path));

    Ok(ParsedConfig {
        config,
        needs_normalization: false,
        warnings,
        problems,
    })
}

fn recoverable_schema_warning(value: Option<&toml::Value>, path: &Path) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(version) = value.as_integer() {
        if version > i64::from(CURRENT_SCHEMA_VERSION) {
            bail!(
                "{} uses config schema_version {}, but this demons release supports schema_version {}",
                path.display(),
                version,
                CURRENT_SCHEMA_VERSION
            );
        }
        if version >= 1 {
            return Ok(None);
        }
    }

    Ok(Some(format!(
        "Recovered invalid schema_version as schema_version {CURRENT_SCHEMA_VERSION}."
    )))
}

fn fresh_config_after_unrecoverable_toml(path: &Path, error: anyhow::Error) -> ParsedConfig {
    let config = Config::default();
    let mut warnings = Vec::new();
    let mut problems = Vec::new();
    push_recovery_warning(
        &mut warnings,
        &mut problems,
        ConfigProblemLocation::Root,
        format!(
            "Could not parse {}; started a fresh draft. Save to overwrite the broken file. ({error:#})",
            path.display()
        ),
    );
    problems.extend(config_blocking_problems(&config, path));
    ParsedConfig {
        config,
        needs_normalization: false,
        warnings,
        problems,
    }
}

fn recover_missing_assignment_values(source: &str) -> Option<String> {
    let mut output = String::with_capacity(source.len());
    let mut changed = false;

    for segment in source.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map(|line| (line, "\n"))
            .unwrap_or((segment, ""));
        let (line, carriage) = line
            .strip_suffix('\r')
            .map(|line| (line, "\r"))
            .unwrap_or((line, ""));

        if let Some(recovered) = recover_missing_assignment_line(line) {
            output.push_str(&recovered);
            changed = true;
        } else {
            output.push_str(line);
        }
        output.push_str(carriage);
        output.push_str(newline);
    }

    changed.then_some(output)
}

fn recover_missing_assignment_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let equals = line.find('=')?;
    let key = line[..equals].trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return None;
    }
    let after = &line[equals + 1..];
    let value = after.trim_start();
    if !(value.is_empty() || value.starts_with('#')) {
        return None;
    }

    let comment = if value.starts_with('#') {
        format!(" {value}")
    } else {
        String::new()
    };
    let recovered = format!("{} \"\"{}", &line[..=equals], comment);
    Some(recovered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SchemaVersion {
    version: u32,
    was_missing: bool,
}

fn detect_schema_version(source: &str, path: &Path) -> Result<SchemaVersion> {
    let value = toml::from_str::<toml::Value>(source)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let Some(version) = value.get("schema_version") else {
        return Ok(SchemaVersion {
            version: CURRENT_SCHEMA_VERSION,
            was_missing: true,
        });
    };
    let Some(version) = version.as_integer() else {
        bail!("{}: schema_version must be an integer", path.display());
    };
    if version < 1 {
        bail!(
            "{}: schema_version must be a positive integer",
            path.display()
        );
    }
    let version = u32::try_from(version)
        .with_context(|| format!("{}: schema_version is too large", path.display()))?;
    Ok(SchemaVersion {
        version,
        was_missing: false,
    })
}

#[derive(Clone, Debug)]
struct RecoveredTask {
    task: Task,
    raw_dependencies: Vec<String>,
}

fn recover_settings(
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> Settings {
    let Some(value) = value else {
        return Settings::default();
    };
    let Some(table) = value.as_table() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Settings,
            "Ignored settings because it was not a table.".to_owned(),
        );
        return Settings::default();
    };

    warn_unknown_keys(
        "settings",
        table.keys().map(String::as_str),
        &[
            "layout",
            "leader",
            "multi_click_ms",
            "logging",
            "mcp_access",
            "mcp_scope_id",
        ],
        ConfigProblemLocation::Settings,
        warnings,
        problems,
    );

    let layout = match table.get("layout") {
        Some(value) => match value.as_str() {
            Some("grid") => Layout::Grid,
            Some(other) => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Setting(ConfigSettingField::Layout),
                    format!("Reset unsupported settings.layout {other:?} to \"grid\"."),
                );
                Layout::Grid
            }
            None => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Setting(ConfigSettingField::Layout),
                    "Reset settings.layout to \"grid\" because it was not a string.".to_owned(),
                );
                Layout::Grid
            }
        },
        None => Layout::Grid,
    };

    let leader = match table.get("leader") {
        Some(value) => match value.as_str().and_then(Leader::from_config) {
            Some(leader) => leader,
            None => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Setting(ConfigSettingField::Leader),
                    "Reset invalid settings.leader to \"alt-j\".".to_owned(),
                );
                Leader::AltJ
            }
        },
        None => Leader::AltJ,
    };

    let multi_click_ms = match table.get("multi_click_ms") {
        Some(value) => match value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
        {
            Some(value) if (MIN_MULTI_CLICK_MS..=MAX_MULTI_CLICK_MS).contains(&value) => value,
            _ => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Setting(ConfigSettingField::MultiClick),
                    format!("Reset invalid settings.multi_click_ms to {DEFAULT_MULTI_CLICK_MS}."),
                );
                DEFAULT_MULTI_CLICK_MS
            }
        },
        None => DEFAULT_MULTI_CLICK_MS,
    };

    if table
        .get("logging")
        .is_some_and(|value| value.as_bool() == Some(true))
    {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::Logging),
            "Ignored settings.logging because it is reserved.".to_owned(),
        );
    } else if table
        .get("logging")
        .is_some_and(|value| value.as_bool().is_none())
    {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::Logging),
            "Reset settings.logging to false because it was not a boolean.".to_owned(),
        );
    }

    let mcp_access = match table.get("mcp_access") {
        Some(value) => match value.as_str().and_then(McpAccess::from_config) {
            Some(access) => access,
            None => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
                    "Reset invalid settings.mcp_access to \"off\".".to_owned(),
                );
                McpAccess::Off
            }
        },
        None => McpAccess::Off,
    };
    let mut mcp_scope_id = table
        .get("mcp_scope_id")
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    if mcp_scope_id
        .as_deref()
        .is_some_and(|scope| uuid::Uuid::parse_str(scope).is_err())
    {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "Replaced invalid settings.mcp_scope_id.".to_owned(),
        );
        mcp_scope_id = None;
    }
    if mcp_access.allows_read() && mcp_scope_id.is_none() {
        let scope = uuid::Uuid::new_v4().to_string();
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "Generated a missing MCP project scope ID.".to_owned(),
        );
        mcp_scope_id = Some(scope);
    }

    Settings {
        layout,
        leader,
        multi_click_ms,
        logging: false,
        mcp_access,
        mcp_scope_id,
    }
}

fn recover_tasks(
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<RecoveredTask> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut used_names = HashSet::new();
    match value {
        toml::Value::Array(tasks) => tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| {
                recover_task(index, task, &mut used_names, warnings, problems)
            })
            .collect(),
        toml::Value::Table(_) => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Tasks,
                "Recovered [task] as a single task; use [[task]] for task arrays.".to_owned(),
            );
            recover_task(0, value, &mut used_names, warnings, problems)
                .into_iter()
                .collect()
        }
        _ => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Tasks,
                "Ignored task entries because task was not an array of tables.".to_owned(),
            );
            Vec::new()
        }
    }
}

fn recover_terminals(
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> Vec<TerminalPane> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut used_names = HashSet::new();
    match value {
        toml::Value::Array(terminals) => terminals
            .iter()
            .enumerate()
            .filter_map(|(index, terminal)| {
                recover_terminal(index, terminal, &mut used_names, warnings, problems)
            })
            .collect(),
        toml::Value::Table(_) => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Tasks,
                "Recovered [terminal] as a single terminal; use [[terminal]] for terminal arrays."
                    .to_owned(),
            );
            recover_terminal(0, value, &mut used_names, warnings, problems)
                .into_iter()
                .collect()
        }
        _ => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Tasks,
                "Ignored terminal entries because terminal was not an array of tables.".to_owned(),
            );
            Vec::new()
        }
    }
}

fn recover_terminal(
    index: usize,
    value: &toml::Value,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> Option<TerminalPane> {
    let Some(table) = value.as_table() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Tasks,
            format!(
                "Ignored terminal #{} because it was not a table.",
                index + 1
            ),
        );
        return None;
    };
    let label = format!("terminal #{}", index + 1);
    warn_unknown_keys(
        &label,
        table.keys().map(String::as_str),
        &["name", "cwd", "env"],
        ConfigProblemLocation::Terminal { index, field: None },
        warnings,
        problems,
    );
    let name = recover_terminal_name(index, table.get("name"), used_names, warnings, problems);
    let cwd = recover_terminal_cwd(&name, table.get("cwd"), warnings, problems, index);
    let env = recover_terminal_env(&name, table.get("env"), warnings, problems, index);
    Some(TerminalPane { name, cwd, env })
}

fn recover_terminal_name(
    index: usize,
    value: Option<&toml::Value>,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> String {
    let fallback = || {
        if index == 0 {
            "terminal".to_owned()
        } else {
            format!("terminal{}", index + 1)
        }
    };
    let candidate = match value.and_then(toml::Value::as_str) {
        Some(name) => {
            let trimmed = name.trim();
            if trimmed != name {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Terminal {
                        index,
                        field: Some(ConfigTerminalField::Name),
                    },
                    format!("Trimmed whitespace from terminal name {name:?}."),
                );
            }
            if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Terminal {
                        index,
                        field: Some(ConfigTerminalField::Name),
                    },
                    format!("Replaced invalid terminal name {name:?}."),
                );
                fallback()
            } else {
                trimmed.to_owned()
            }
        }
        None => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Name),
                },
                format!("Filled missing terminal #{} name.", index + 1),
            );
            fallback()
        }
    };
    unique_recovered_terminal_name(candidate, used_names, warnings, problems, index)
}

fn unique_recovered_terminal_name(
    candidate: String,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> String {
    if used_names.insert(candidate.clone()) {
        return candidate;
    }
    for suffix in 2.. {
        let renamed = format!("{candidate}{suffix}");
        if used_names.insert(renamed.clone()) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Name),
                },
                format!("Renamed duplicate terminal {candidate:?} to {renamed:?}."),
            );
            return renamed;
        }
    }
    unreachable!("unbounded suffix search always returns");
}

fn recover_terminal_cwd(
    terminal_name: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> PathBuf {
    match value.and_then(toml::Value::as_str) {
        Some(cwd) => PathBuf::from(cwd),
        None if value.is_some() => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Cwd),
                },
                format!(
                    "Reset cwd for terminal {terminal_name:?} to \".\" because it was not a string."
                ),
            );
            PathBuf::from(".")
        }
        None => PathBuf::from("."),
    }
}

fn recover_terminal_env(
    terminal_name: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> BTreeMap<String, String> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Some(table) = value.as_table() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Terminal {
                index,
                field: Some(ConfigTerminalField::Env),
            },
            format!("Ignored env for terminal {terminal_name:?} because it was not a table."),
        );
        return BTreeMap::new();
    };
    let mut env = BTreeMap::new();
    for (key, value) in table {
        if key.is_empty() || key.contains(['=', '\0']) || key.contains(char::is_whitespace) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Env),
                },
                format!("Ignored invalid env key {key:?} for terminal {terminal_name:?}."),
            );
            continue;
        }
        let Some(value) = value.as_str() else {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Env),
                },
                format!(
                    "Ignored env key {key:?} for terminal {terminal_name:?}; value was not a string."
                ),
            );
            continue;
        };
        if value.contains('\0') {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Env),
                },
                format!(
                    "Ignored env key {key:?} for terminal {terminal_name:?}; value contained a NUL byte."
                ),
            );
            continue;
        }
        env.insert(key.clone(), value.to_owned());
    }
    env
}

fn recover_task(
    index: usize,
    value: &toml::Value,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> Option<RecoveredTask> {
    let Some(table) = value.as_table() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Tasks,
            format!("Ignored task #{} because it was not a table.", index + 1),
        );
        return None;
    };
    let label = format!("task #{}", index + 1);
    warn_unknown_keys(
        &label,
        table.keys().map(String::as_str),
        &[
            "name",
            "command",
            "cwd",
            "env",
            "depends_on",
            "start_delay",
            "watch",
            "run_on_change",
            "repeat",
        ],
        ConfigProblemLocation::Task { index, field: None },
        warnings,
        problems,
    );

    let name = recover_task_name(index, table.get("name"), used_names, warnings, problems);
    let command = recover_task_command(table.get("command"));
    let cwd = recover_task_cwd(&name, table.get("cwd"), warnings, problems, index);
    let env = recover_task_env(&name, table.get("env"), warnings, problems, index);
    let raw_dependencies =
        recover_task_dependencies(&name, table.get("depends_on"), warnings, problems, index);
    let start_delay = recover_optional_string(
        &name,
        "start_delay",
        table.get("start_delay"),
        warnings,
        problems,
        index,
    );
    warn_reserved_task_fields(&name, table, warnings, problems, index);

    Some(RecoveredTask {
        task: Task {
            name,
            command,
            cwd,
            env,
            depends_on: Vec::new(),
            start_delay,
            watch: None,
            run_on_change: None,
            repeat: None,
        },
        raw_dependencies,
    })
}

fn recover_task_name(
    index: usize,
    value: Option<&toml::Value>,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) -> String {
    let fallback = || {
        if index == 0 {
            "task".to_owned()
        } else {
            format!("task{}", index + 1)
        }
    };
    let candidate = match value.and_then(toml::Value::as_str) {
        Some(name) => {
            let trimmed = name.trim();
            if trimmed != name {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Name),
                    },
                    format!("Trimmed whitespace from task name {name:?}."),
                );
            }
            if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Name),
                    },
                    format!("Replaced invalid task name {name:?}."),
                );
                fallback()
            } else {
                trimmed.to_owned()
            }
        }
        None => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Name),
                },
                format!("Filled missing task #{} name.", index + 1),
            );
            fallback()
        }
    };
    unique_recovered_name(candidate, used_names, warnings, problems, index)
}

fn unique_recovered_name(
    candidate: String,
    used_names: &mut HashSet<String>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> String {
    if used_names.insert(candidate.clone()) {
        return candidate;
    }
    for suffix in 2.. {
        let renamed = format!("{candidate}{suffix}");
        if used_names.insert(renamed.clone()) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Name),
                },
                format!("Renamed duplicate task {candidate:?} to {renamed:?}."),
            );
            return renamed;
        }
    }
    unreachable!("unbounded suffix search always returns");
}

fn recover_task_command(value: Option<&toml::Value>) -> TaskCommand {
    match value {
        Some(toml::Value::String(command)) if !command.trim().is_empty() => {
            TaskCommand::Shell(command.clone())
        }
        Some(toml::Value::String(command)) => TaskCommand::Shell(command.clone()),
        Some(toml::Value::Array(parts)) => {
            let mut command = Vec::new();
            for part in parts {
                let Some(part) = part.as_str() else {
                    return TaskCommand::Shell(String::new());
                };
                command.push(part.to_owned());
            }
            if command.first().is_some_and(|part| !part.trim().is_empty()) {
                TaskCommand::Direct(command)
            } else {
                TaskCommand::Shell(String::new())
            }
        }
        _ => TaskCommand::Shell(String::new()),
    }
}

fn recover_task_cwd(
    task_name: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> PathBuf {
    match value.and_then(toml::Value::as_str) {
        Some(cwd) => PathBuf::from(cwd),
        None if value.is_some() => {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Cwd),
                },
                format!("Reset cwd for task {task_name:?} to \".\" because it was not a string."),
            );
            PathBuf::from(".")
        }
        None => PathBuf::from("."),
    }
}

fn recover_task_env(
    task_name: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> BTreeMap<String, String> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Some(table) = value.as_table() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Task {
                index,
                field: Some(ConfigTaskField::Env),
            },
            format!("Ignored env for task {task_name:?} because it was not a table."),
        );
        return BTreeMap::new();
    };
    let mut env = BTreeMap::new();
    for (key, value) in table {
        if key.is_empty() || key.contains(['=', '\0']) || key.contains(char::is_whitespace) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Env),
                },
                format!("Ignored invalid env key {key:?} for task {task_name:?}."),
            );
            continue;
        }
        let Some(value) = value.as_str() else {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Env),
                },
                format!("Ignored env key {key:?} for task {task_name:?}; value was not a string."),
            );
            continue;
        };
        if value.contains('\0') {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Env),
                },
                format!(
                    "Ignored env key {key:?} for task {task_name:?}; value contained a NUL byte."
                ),
            );
            continue;
        }
        env.insert(key.clone(), value.to_owned());
    }
    env
}

fn recover_task_dependencies(
    task_name: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(values) = value.as_array() else {
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Task {
                index,
                field: Some(ConfigTaskField::Dependencies),
            },
            format!(
                "Ignored dependencies for task {task_name:?} because depends_on was not an array."
            ),
        );
        return Vec::new();
    };
    let mut dependencies = Vec::new();
    for value in values {
        let Some(dependency) = value.as_str() else {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Dependencies),
                },
                format!("Ignored non-string dependency for task {task_name:?}."),
            );
            continue;
        };
        let dependency = dependency.trim();
        if dependency.is_empty() || dependency.chars().any(char::is_control) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Dependencies),
                },
                format!("Ignored invalid dependency for task {task_name:?}."),
            );
            continue;
        }
        dependencies.push(dependency.to_owned());
    }
    dependencies
}

fn recover_optional_string(
    task_name: &str,
    field: &str,
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) -> Option<String> {
    match value {
        Some(value) => match value.as_str() {
            Some(value) if !value.trim().is_empty() => Some(value.to_owned()),
            Some(_) => None,
            None => {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::StartDelay),
                    },
                    format!("Ignored {field} for task {task_name:?} because it was not a string."),
                );
                None
            }
        },
        None => None,
    }
}

fn warn_reserved_task_fields(
    task_name: &str,
    table: &toml::map::Map<String, toml::Value>,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    index: usize,
) {
    for field in ["watch", "run_on_change", "repeat"] {
        if table.contains_key(field) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task { index, field: None },
                format!("Ignored reserved field {field} for task {task_name:?}."),
            );
        }
    }
}

fn scrub_recovered_dependencies(
    tasks: &mut [RecoveredTask],
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    let names = tasks
        .iter()
        .map(|task| task.task.name.clone())
        .collect::<HashSet<_>>();
    for (index, task) in tasks.iter_mut().enumerate() {
        let mut dependencies = HashSet::new();
        for dependency in &task.raw_dependencies {
            if dependency == &task.task.name {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Dependencies),
                    },
                    format!("Ignored self-dependency on task {:?}.", task.task.name),
                );
                continue;
            }
            if !names.contains(dependency) {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Dependencies),
                    },
                    format!(
                        "Ignored unknown dependency {dependency:?} for task {:?}.",
                        task.task.name
                    ),
                );
                continue;
            }
            if !dependencies.insert(dependency.clone()) {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Dependencies),
                    },
                    format!(
                        "Ignored duplicate dependency {dependency:?} for task {:?}.",
                        task.task.name
                    ),
                );
                continue;
            }
            task.task.depends_on.push(dependency.clone());
        }
    }
}

fn warn_unknown_keys<'a>(
    label: &str,
    keys: impl Iterator<Item = &'a str>,
    known: &[&str],
    location: ConfigProblemLocation,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    for key in keys {
        if !known.contains(&key) {
            push_recovery_warning(
                warnings,
                problems,
                location.clone(),
                format!("Ignored unknown {label} key {key:?}."),
            );
        }
    }
}

fn push_recovery_warning(
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
    location: ConfigProblemLocation,
    warning: String,
) {
    if warnings.len() < MAX_RECOVERY_WARNINGS {
        warnings.push(warning.clone());
        problems.push(ConfigProblem::warning(location, warning));
    } else if warnings.len() == MAX_RECOVERY_WARNINGS {
        let warning = "Additional config recovery warnings omitted.".to_owned();
        warnings.push(warning.clone());
        problems.push(ConfigProblem::warning(ConfigProblemLocation::Root, warning));
    }
}

fn repair_config_for_configurator(
    config: &mut Config,
    path: &Path,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    if config
        .settings
        .mcp_scope_id
        .as_deref()
        .is_some_and(|scope| uuid::Uuid::parse_str(scope).is_err())
    {
        config.settings.mcp_scope_id = None;
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "Replaced invalid settings.mcp_scope_id.".to_owned(),
        );
    }
    if config.settings.mcp_access.allows_read() && config.settings.mcp_scope_id.is_none() {
        config.settings.mcp_scope_id = Some(uuid::Uuid::new_v4().to_string());
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "Generated a missing MCP project scope ID.".to_owned(),
        );
    }
    if config.settings.logging {
        config.settings.logging = false;
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::Logging),
            "Disabled settings.logging because it is reserved.".to_owned(),
        );
    }
    if !(MIN_MULTI_CLICK_MS..=MAX_MULTI_CLICK_MS).contains(&config.settings.multi_click_ms) {
        config.settings.multi_click_ms = DEFAULT_MULTI_CLICK_MS;
        push_recovery_warning(
            warnings,
            problems,
            ConfigProblemLocation::Setting(ConfigSettingField::MultiClick),
            format!("Reset invalid settings.multi_click_ms to {DEFAULT_MULTI_CLICK_MS}."),
        );
    }
    if config.settings.mcp_access.allows_read()
        && config
            .settings
            .mcp_scope_id
            .as_deref()
            .is_none_or(|scope| uuid::Uuid::parse_str(scope).is_err())
    {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "MCP access requires a valid project scope ID.",
        ));
    }

    repair_task_names(config, warnings, problems);
    repair_terminal_names(config, warnings, problems);
    repair_task_env(config, warnings, problems);
    repair_terminal_env(config, warnings, problems);
    repair_task_dependencies(config, warnings, problems);
    repair_reserved_task_fields(config, warnings, problems);
    problems.extend(config_blocking_problems(config, path));
}

fn repair_task_names(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    let mut used_names = HashSet::new();
    for (index, task) in config.tasks.iter_mut().enumerate() {
        let original = task.name.clone();
        let trimmed = original.trim();
        let candidate = if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task {
                    index,
                    field: Some(ConfigTaskField::Name),
                },
                format!("Replaced invalid task name {original:?}."),
            );
            if index == 0 {
                "task".to_owned()
            } else {
                format!("task{}", index + 1)
            }
        } else {
            if trimmed != original {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Name),
                    },
                    format!("Trimmed whitespace from task name {original:?}."),
                );
            }
            trimmed.to_owned()
        };
        let repaired = unique_recovered_name(candidate, &mut used_names, warnings, problems, index);
        task.name = repaired;
    }
}

fn repair_task_env(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    for (index, task) in config.tasks.iter_mut().enumerate() {
        let task_name = task.name.clone();
        task.env.retain(|key, value| {
            let keep = !key.is_empty()
                && !key.contains(['=', '\0'])
                && !key.contains(char::is_whitespace)
                && !value.contains('\0');
            if !keep {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Env),
                    },
                    format!("Ignored invalid env key {key:?} for task {task_name:?}."),
                );
            }
            keep
        });
    }
}

fn repair_terminal_names(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    let mut used_names = config
        .tasks
        .iter()
        .map(|task| task.name.clone())
        .collect::<HashSet<_>>();
    for (index, terminal) in config.terminals.iter_mut().enumerate() {
        let original = terminal.name.clone();
        let trimmed = original.trim();
        let candidate = if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Terminal {
                    index,
                    field: Some(ConfigTerminalField::Name),
                },
                format!("Replaced invalid terminal name {original:?}."),
            );
            if index == 0 {
                "terminal".to_owned()
            } else {
                format!("terminal{}", index + 1)
            }
        } else {
            if trimmed != original {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Terminal {
                        index,
                        field: Some(ConfigTerminalField::Name),
                    },
                    format!("Trimmed whitespace from terminal name {original:?}."),
                );
            }
            trimmed.to_owned()
        };
        let repaired =
            unique_recovered_terminal_name(candidate, &mut used_names, warnings, problems, index);
        terminal.name = repaired;
    }
}

fn repair_terminal_env(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    for (index, terminal) in config.terminals.iter_mut().enumerate() {
        let terminal_name = terminal.name.clone();
        terminal.env.retain(|key, value| {
            let keep = !key.is_empty()
                && !key.contains(['=', '\0'])
                && !key.contains(char::is_whitespace)
                && !value.contains('\0');
            if !keep {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Terminal {
                        index,
                        field: Some(ConfigTerminalField::Env),
                    },
                    format!("Ignored invalid env key {key:?} for terminal {terminal_name:?}."),
                );
            }
            keep
        });
    }
}

fn repair_task_dependencies(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    let names = config
        .tasks
        .iter()
        .map(|task| task.name.clone())
        .collect::<HashSet<_>>();
    for (index, task) in config.tasks.iter_mut().enumerate() {
        let mut seen = HashSet::new();
        let task_name = task.name.clone();
        task.depends_on.retain(|dependency| {
            let keep = dependency != &task_name
                && names.contains(dependency)
                && dependency.trim() == dependency
                && !dependency.is_empty()
                && !dependency.chars().any(char::is_control)
                && seen.insert(dependency.clone());
            if !keep {
                push_recovery_warning(
                    warnings,
                    problems,
                    ConfigProblemLocation::Task {
                        index,
                        field: Some(ConfigTaskField::Dependencies),
                    },
                    format!("Ignored invalid dependency {dependency:?} for task {task_name:?}."),
                );
            }
            keep
        });
    }
}

fn repair_reserved_task_fields(
    config: &mut Config,
    warnings: &mut Vec<String>,
    problems: &mut Vec<ConfigProblem>,
) {
    for (index, task) in config.tasks.iter_mut().enumerate() {
        if task.watch.take().is_some() {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task { index, field: None },
                format!("Ignored reserved field watch for task {:?}.", task.name),
            );
        }
        if task.run_on_change.take().is_some() {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task { index, field: None },
                format!(
                    "Ignored reserved field run_on_change for task {:?}.",
                    task.name
                ),
            );
        }
        if task.repeat.take().is_some() {
            push_recovery_warning(
                warnings,
                problems,
                ConfigProblemLocation::Task { index, field: None },
                format!("Ignored reserved field repeat for task {:?}.", task.name),
            );
        }
    }
}

pub fn config_blocking_problems(config: &Config, path: &Path) -> Vec<ConfigProblem> {
    let mut problems = Vec::new();
    let mut can_check_cycles = true;
    if config.schema_version != CURRENT_SCHEMA_VERSION {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Root,
            format!(
                "schema_version {} is not supported by this demons release",
                config.schema_version
            ),
        ));
    }
    if config.tasks.is_empty() && config.terminals.is_empty() {
        can_check_cycles = false;
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Tasks,
            "At least one task or terminal is required.",
        ));
    }
    if config.settings.logging {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Setting(ConfigSettingField::Logging),
            "settings.logging is reserved and cannot be enabled.",
        ));
    }
    if !(MIN_MULTI_CLICK_MS..=MAX_MULTI_CLICK_MS).contains(&config.settings.multi_click_ms) {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Setting(ConfigSettingField::MultiClick),
            format!(
                "settings.multi_click_ms must be between {MIN_MULTI_CLICK_MS} and {MAX_MULTI_CLICK_MS}."
            ),
        ));
    }
    if config.settings.mcp_access.allows_read()
        && config
            .settings
            .mcp_scope_id
            .as_deref()
            .is_none_or(|scope| uuid::Uuid::parse_str(scope).is_err())
    {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Setting(ConfigSettingField::McpAccess),
            "MCP access requires a valid project scope ID.",
        ));
    }

    let root = path.parent().unwrap_or_else(|| Path::new("."));
    let mut names = HashSet::new();
    let mut name_to_index = BTreeMap::new();
    for (index, task) in config.tasks.iter().enumerate() {
        if task.name.trim().is_empty() {
            can_check_cycles = false;
            problems.push(task_problem(
                index,
                ConfigTaskField::Name,
                "Task name is required.",
            ));
        } else if task.name.trim() != task.name || task.name.chars().any(char::is_control) {
            can_check_cycles = false;
            problems.push(task_problem(
                index,
                ConfigTaskField::Name,
                "Task name cannot have leading/trailing whitespace or control characters.",
            ));
        } else if !names.insert(task.name.as_str()) {
            can_check_cycles = false;
            problems.push(task_problem(
                index,
                ConfigTaskField::Name,
                format!("Duplicate task name {:?}.", task.name),
            ));
        } else {
            name_to_index.insert(task.name.as_str(), index);
        }

        if task.command.is_empty() {
            problems.push(task_problem(
                index,
                ConfigTaskField::Command,
                "Command is required.",
            ));
        } else if task.command.contains_nul() {
            problems.push(task_problem(
                index,
                ConfigTaskField::Command,
                "Command cannot contain a NUL byte.",
            ));
        }

        let cwd = if task.cwd.is_absolute() {
            task.cwd.clone()
        } else {
            root.join(&task.cwd)
        };
        if !cwd.is_dir() {
            problems.push(task_problem(
                index,
                ConfigTaskField::Cwd,
                format!("Working directory is not a directory: {}.", cwd.display()),
            ));
        }

        for (key, value) in &task.env {
            if key.is_empty()
                || key.contains(['=', '\0'])
                || key.contains(char::is_whitespace)
                || value.contains('\0')
            {
                problems.push(task_problem(
                    index,
                    ConfigTaskField::Env,
                    format!("Environment entry {key:?} is invalid."),
                ));
            }
        }

        let mut dependencies = HashSet::new();
        for dependency in &task.depends_on {
            if dependency.trim().is_empty()
                || dependency.trim() != dependency
                || dependency.chars().any(char::is_control)
            {
                can_check_cycles = false;
                problems.push(task_problem(
                    index,
                    ConfigTaskField::Dependencies,
                    format!("Dependency name {dependency:?} is invalid."),
                ));
            } else if dependency == &task.name {
                can_check_cycles = false;
                problems.push(task_problem(
                    index,
                    ConfigTaskField::Dependencies,
                    "Task cannot depend on itself.",
                ));
            } else if !dependencies.insert(dependency.as_str()) {
                can_check_cycles = false;
                problems.push(task_problem(
                    index,
                    ConfigTaskField::Dependencies,
                    format!("Dependency {dependency:?} is repeated."),
                ));
            } else if !config
                .tasks
                .iter()
                .any(|candidate| &candidate.name == dependency)
            {
                can_check_cycles = false;
                problems.push(task_problem(
                    index,
                    ConfigTaskField::Dependencies,
                    format!("Dependency {dependency:?} does not match a task."),
                ));
            }
        }

        if let Some(delay) = task.start_delay.as_deref()
            && let Err(error) = parse_start_delay(delay)
        {
            problems.push(task_problem(
                index,
                ConfigTaskField::StartDelay,
                format!("Start delay {delay:?} is invalid: {error:#}."),
            ));
        }

        if task.watch.is_some() || task.run_on_change.is_some() || task.repeat.is_some() {
            problems.push(ConfigProblem::error(
                ConfigProblemLocation::Task { index, field: None },
                "Task uses reserved watch, run_on_change, or repeat fields.",
            ));
        }
    }

    for (index, terminal) in config.terminals.iter().enumerate() {
        if terminal.name.trim().is_empty() {
            problems.push(terminal_problem(
                index,
                ConfigTerminalField::Name,
                "Terminal name is required.",
            ));
        } else if terminal.name.trim() != terminal.name
            || terminal.name.chars().any(char::is_control)
        {
            problems.push(terminal_problem(
                index,
                ConfigTerminalField::Name,
                "Terminal name cannot have leading/trailing whitespace or control characters.",
            ));
        } else if !names.insert(terminal.name.as_str()) {
            problems.push(terminal_problem(
                index,
                ConfigTerminalField::Name,
                format!("Duplicate pane name {:?}.", terminal.name),
            ));
        }

        let cwd = if terminal.cwd.is_absolute() {
            terminal.cwd.clone()
        } else {
            root.join(&terminal.cwd)
        };
        if !cwd.is_dir() {
            problems.push(terminal_problem(
                index,
                ConfigTerminalField::Cwd,
                format!("Working directory is not a directory: {}.", cwd.display()),
            ));
        }

        for (key, value) in &terminal.env {
            if key.is_empty()
                || key.contains(['=', '\0'])
                || key.contains(char::is_whitespace)
                || value.contains('\0')
            {
                problems.push(terminal_problem(
                    index,
                    ConfigTerminalField::Env,
                    format!("Environment entry {key:?} is invalid."),
                ));
            }
        }
    }

    if can_check_cycles && let Err(error) = reject_dependency_cycles(config, &name_to_index, path) {
        problems.push(ConfigProblem::error(
            ConfigProblemLocation::Tasks,
            format!("{error:#}"),
        ));
    }
    problems
}

fn task_problem(index: usize, field: ConfigTaskField, message: impl Into<String>) -> ConfigProblem {
    ConfigProblem::error(
        ConfigProblemLocation::Task {
            index,
            field: Some(field),
        },
        message,
    )
}

fn terminal_problem(
    index: usize,
    field: ConfigTerminalField,
    message: impl Into<String>,
) -> ConfigProblem {
    ConfigProblem::error(
        ConfigProblemLocation::Terminal {
            index,
            field: Some(field),
        },
        message,
    )
}

pub fn validate_for_path(config: &Config, path: &Path) -> Result<()> {
    if config.schema_version != CURRENT_SCHEMA_VERSION {
        bail!(
            "{} uses config schema_version {}, but this demons release supports schema_version {}",
            path.display(),
            config.schema_version,
            CURRENT_SCHEMA_VERSION
        );
    }
    if config.tasks.is_empty() && config.terminals.is_empty() {
        bail!(
            "{} must define at least one [[task]] or [[terminal]]",
            path.display()
        );
    }
    if config.settings.logging {
        bail!(
            "{}: settings.logging is reserved for a future release and cannot be enabled",
            path.display()
        );
    }
    if !(MIN_MULTI_CLICK_MS..=MAX_MULTI_CLICK_MS).contains(&config.settings.multi_click_ms) {
        bail!(
            "{}: settings.multi_click_ms must be between {MIN_MULTI_CLICK_MS} and {MAX_MULTI_CLICK_MS}",
            path.display()
        );
    }
    if config.settings.mcp_access.allows_read()
        && config
            .settings
            .mcp_scope_id
            .as_deref()
            .is_none_or(|scope| uuid::Uuid::parse_str(scope).is_err())
    {
        bail!(
            "{}: MCP access requires a valid settings.mcp_scope_id UUID",
            path.display()
        );
    }

    let root = path
        .parent()
        .context("config path has no parent directory")?;
    let mut names = HashSet::new();
    let mut name_to_index = BTreeMap::new();
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
        name_to_index.insert(task.name.as_str(), index);
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

        let mut dependencies = HashSet::new();
        for dependency in &task.depends_on {
            if dependency.trim().is_empty()
                || dependency.trim() != dependency
                || dependency.chars().any(char::is_control)
            {
                bail!(
                    "{}: task {:?} has an invalid dependency name {:?}",
                    path.display(),
                    task.name,
                    dependency
                );
            }
            if dependency == &task.name {
                bail!(
                    "{}: task {:?} cannot depend on itself",
                    path.display(),
                    task.name
                );
            }
            if !dependencies.insert(dependency.as_str()) {
                bail!(
                    "{}: task {:?} repeats dependency {:?}",
                    path.display(),
                    task.name,
                    dependency
                );
            }
        }
        if let Some(delay) = task.start_delay.as_deref() {
            parse_start_delay(delay).with_context(|| {
                format!(
                    "{}: task {:?} has invalid start_delay {:?}",
                    path.display(),
                    task.name,
                    delay
                )
            })?;
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
    for task in &config.tasks {
        for dependency in &task.depends_on {
            if !name_to_index.contains_key(dependency.as_str()) {
                bail!(
                    "{}: task {:?} depends on unknown task {:?}",
                    path.display(),
                    task.name,
                    dependency
                );
            }
        }
    }
    for (index, terminal) in config.terminals.iter().enumerate() {
        let label = format!("terminal #{}", index + 1);
        if terminal.name.trim().is_empty() {
            bail!("{}: {label} has an empty name", path.display());
        }
        if terminal.name.trim() != terminal.name || terminal.name.chars().any(char::is_control) {
            bail!(
                "{}: terminal name {:?} has leading/trailing whitespace or control characters",
                path.display(),
                terminal.name
            );
        }
        if !names.insert(terminal.name.as_str()) {
            bail!(
                "{}: duplicate pane name {:?}",
                path.display(),
                terminal.name
            );
        }
        for (key, value) in &terminal.env {
            if key.is_empty()
                || key.contains(['=', '\0'])
                || key.contains(char::is_whitespace)
                || value.contains('\0')
            {
                bail!(
                    "{}: terminal {:?} has an invalid environment entry for key {:?}",
                    path.display(),
                    terminal.name,
                    key
                );
            }
        }

        let cwd = if terminal.cwd.is_absolute() {
            terminal.cwd.clone()
        } else {
            root.join(&terminal.cwd)
        };
        if !cwd.is_dir() {
            bail!(
                "{}: cwd for terminal {:?} is not a directory: {}",
                path.display(),
                terminal.name,
                cwd.display()
            );
        }
    }
    reject_dependency_cycles(config, &name_to_index, path)?;
    Ok(())
}

pub fn parse_start_delay(value: &str) -> Result<Duration> {
    let value = value.trim();
    if value.is_empty() {
        bail!("delay cannot be empty");
    }
    let (number, unit) = split_duration(value)?;
    let amount: u64 = number
        .parse()
        .with_context(|| format!("invalid delay amount {number:?}"))?;
    let millis_per_unit = match unit {
        "" | "s" => 1_000,
        "ms" => 1,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => bail!("delay unit must be one of ms, s, m, h"),
    };
    let millis = amount
        .checked_mul(millis_per_unit)
        .context("delay is too large")?;
    let duration = Duration::from_millis(millis);
    Instant::now()
        .checked_add(duration)
        .context("delay is too large to schedule on this platform")?;
    Ok(duration)
}

fn split_duration(value: &str) -> Result<(&str, &str)> {
    let unit_start = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = &value[..unit_start];
    let unit = &value[unit_start..];
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        bail!("delay must start with a number");
    }
    if unit.is_empty()
        || unit
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        Ok((number, unit))
    } else {
        bail!("delay unit must use letters only");
    }
}

fn reject_dependency_cycles(
    config: &Config,
    name_to_index: &BTreeMap<&str, usize>,
    path: &Path,
) -> Result<()> {
    let mut states = vec![VisitState::Unvisited; config.tasks.len()];
    let mut stack = Vec::new();
    for index in 0..config.tasks.len() {
        visit_dependency(index, config, name_to_index, &mut states, &mut stack, path)?;
    }
    Ok(())
}

fn visit_dependency(
    index: usize,
    config: &Config,
    name_to_index: &BTreeMap<&str, usize>,
    states: &mut [VisitState],
    stack: &mut Vec<usize>,
    path: &Path,
) -> Result<()> {
    match states[index] {
        VisitState::Visited => return Ok(()),
        VisitState::Visiting => {
            let task = &config.tasks[index];
            bail!(
                "{}: task dependency cycle includes {:?}",
                path.display(),
                task.name
            );
        }
        VisitState::Unvisited => {}
    }
    states[index] = VisitState::Visiting;
    stack.push(index);
    for dependency in &config.tasks[index].depends_on {
        let Some(&dependency_index) = name_to_index.get(dependency.as_str()) else {
            continue;
        };
        if states[dependency_index] == VisitState::Visiting {
            let start = stack
                .iter()
                .position(|candidate| *candidate == dependency_index)
                .unwrap_or(0);
            let mut cycle = stack[start..]
                .iter()
                .map(|candidate| config.tasks[*candidate].name.as_str())
                .collect::<Vec<_>>();
            cycle.push(config.tasks[dependency_index].name.as_str());
            bail!(
                "{}: task dependency cycle: {}",
                path.display(),
                cycle.join(" -> ")
            );
        }
        visit_dependency(dependency_index, config, name_to_index, states, stack, path)?;
    }
    stack.pop();
    states[index] = VisitState::Visited;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisitState {
    Unvisited,
    Visiting,
    Visited,
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

fn current_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn shipped_config_examples_use_current_schema() {
        for (name, source) in [
            ("example", include_str!("../examples/demons.toml")),
            (
                "smoke fixture",
                include_str!("../tests/fixtures/smoke.toml"),
            ),
        ] {
            let config: Config = toml::from_str(source)
                .unwrap_or_else(|error| panic!("{name} config did not parse: {error}"));
            assert_eq!(
                config.schema_version, CURRENT_SCHEMA_VERSION,
                "{name} config uses a stale schema"
            );
        }
    }

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
    fn load_normalizes_unversioned_config_after_validation() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            r#"
                [[task]]
                name = "server"
                command = "echo ok"
            "#,
        )
        .unwrap();

        let loaded = LoadedConfig::load(path.clone()).unwrap();

        assert_eq!(loaded.config.schema_version, CURRENT_SCHEMA_VERSION);
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("schema_version = 3"));
        assert!(saved.contains("[settings]"));
        assert!(saved.contains("layout = \"grid\""));
        assert!(saved.contains("leader = \"alt-j\""));
        assert!(saved.contains("multi_click_ms = 500"));
        assert!(saved.contains("logging = false"));
        assert!(saved.contains("cwd = \".\""));
        assert!(saved.contains("depends_on = []"));
        assert!(saved.contains("[task.env]"));
    }

    #[test]
    fn load_does_not_rewrite_invalid_unversioned_config() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = "# no tasks yet\n";
        fs::write(&path, original).unwrap();

        let error = LoadedConfig::load(path.clone()).unwrap_err().to_string();

        assert!(error.contains("must define at least one [[task]] or [[terminal]]"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn unvalidated_load_normalizes_valid_unversioned_config() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            r#"
                [[task]]
                name = "server"
                command = "echo ok"
            "#,
        )
        .unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert_eq!(loaded.config.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("schema_version = 3")
        );
    }

    #[test]
    fn unvalidated_load_keeps_invalid_unversioned_config_editable_without_rewrite() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = "# no tasks yet\n";
        fs::write(&path, original).unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert!(loaded.config.tasks.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn unvalidated_load_recovers_schema_invalid_config_without_rewrite() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = r#"
            surprise = true

            [settings]
            leader = "not-a-leader"
            multi_click_ms = 20
            extra = true

            [[task]]
            name = " server "
            cwd = 123
            env = { OK = "yes", BAD = 42 }
            depends_on = ["server", "missing", 7]
            unknown = true

            [[task]]
            name = "server"
            command = ["cargo", "run"]
        "#;
        fs::write(&path, original).unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!loaded.config_warnings.is_empty());
        assert!(
            loaded
                .config_warnings
                .iter()
                .any(|warning| warning.contains("Recovered config"))
        );
        assert_eq!(loaded.config.settings.leader, Leader::AltJ);
        assert_eq!(
            loaded.config.settings.multi_click_ms,
            DEFAULT_MULTI_CLICK_MS
        );
        assert_eq!(loaded.config.tasks.len(), 2);
        assert_eq!(loaded.config.tasks[0].name, "server");
        assert!(matches!(
            loaded.config.tasks[0].command,
            TaskCommand::Shell(ref command) if command.is_empty()
        ));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Error
                && matches!(
                    problem.location,
                    ConfigProblemLocation::Task {
                        index: 0,
                        field: Some(ConfigTaskField::Command)
                    }
                )
        }));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Warning
                && matches!(problem.location, ConfigProblemLocation::Root)
                && problem.message.contains("unknown root key")
        }));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Warning
                && matches!(problem.location, ConfigProblemLocation::Settings)
                && problem.message.contains("unknown settings key")
        }));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Warning
                && matches!(
                    problem.location,
                    ConfigProblemLocation::Task {
                        index: 0,
                        field: None
                    }
                )
                && problem.message.contains("unknown task #1 key")
        }));
        assert_eq!(loaded.config.tasks[0].cwd, PathBuf::from("."));
        assert_eq!(
            loaded.config.tasks[0].env.get("OK"),
            Some(&"yes".to_owned())
        );
        assert!(!loaded.config.tasks[0].env.contains_key("BAD"));
        assert!(loaded.config.tasks[0].depends_on.is_empty());
        assert_eq!(loaded.config.tasks[1].name, "server2");
        assert!(matches!(
            loaded.config.tasks[1].command,
            TaskCommand::Direct(ref command) if command == &["cargo", "run"]
        ));
        assert!(validate_for_path(&loaded.config, &path).is_err());
    }

    #[test]
    fn unvalidated_load_repairs_validation_invalid_config_without_rewrite() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = r#"
            [settings]
            logging = true

            [[task]]
            name = "server"
            command = "echo one"
            depends_on = ["missing", "server"]
            watch = ["src/**/*.rs"]

            [[task]]
            name = "server"
            command = "echo two"
            repeat = "1s"
        "#;
        fs::write(&path, original).unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!loaded.config_warnings.is_empty());
        assert!(!loaded.config.settings.logging);
        assert_eq!(loaded.config.tasks[0].name, "server");
        assert_eq!(loaded.config.tasks[1].name, "server2");
        assert!(loaded.config.tasks[0].depends_on.is_empty());
        assert!(loaded.config.tasks[0].watch.is_none());
        assert!(loaded.config.tasks[1].repeat.is_none());
        validate_for_path(&loaded.config, &path).unwrap();
    }

    #[test]
    fn unvalidated_load_does_not_recover_future_schema() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            r#"
                schema_version = 4
                future_setting = true
            "#,
        )
        .unwrap();

        let error = LoadedConfig::load_unvalidated_or_default(path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("uses config schema_version 4"));
    }

    #[test]
    fn unvalidated_load_salvages_malformed_schema_declarations_without_rewrite() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        for declaration in [
            "schema_version = 0",
            "schema_version = -1",
            "schema_version = \"2\"",
            "schema_version = \"3\"",
            "schema_version = true",
            "schema_version =",
        ] {
            let original =
                format!("{declaration}\n\n[[task]]\nname = \"server\"\ncommand = \"echo ok\"\n");
            fs::write(&path, &original).unwrap();

            let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            assert_eq!(loaded.config.schema_version, CURRENT_SCHEMA_VERSION);
            assert_eq!(loaded.config.tasks.len(), 1);
            assert_eq!(loaded.config.tasks[0].name, "server");
            assert!(loaded.config_problems.iter().any(|problem| {
                problem.severity == ConfigProblemSeverity::Warning
                    && matches!(problem.location, ConfigProblemLocation::Root)
                    && problem.message.contains("Recovered invalid schema_version")
            }));
            validate_for_path(&loaded.config, &path).unwrap();
        }
    }

    #[test]
    fn unvalidated_load_recovers_missing_assignment_value_without_rewrite() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = r#"
            [settings]
            layout = "grid"
            leader = "ctrl-b"

            [[task]]
            name = "backend_server"
            command = "cargo run -p vashti"

            [[task]]
            name = "frontend_client"
            command = "npm run dev -- --host 0.0.0.0"
            cwd = "./apps/vashti/web"
            depends_on = ["backend_server"]
            start_delay = "1"

            [[task]]
            name = "test"
            command =
        "#;
        fs::write(&path, original).unwrap();
        fs::create_dir_all(temp.path().join("apps/vashti/web")).unwrap();

        let strict_error = LoadedConfig::load(path.clone()).unwrap_err().to_string();
        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert!(strict_error.contains("failed to parse config"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(loaded.config.tasks.len(), 3);
        assert!(matches!(
            loaded.config.tasks[2].command,
            TaskCommand::Shell(ref command) if command.is_empty()
        ));
        assert!(!loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Warning
                && matches!(
                    problem.location,
                    ConfigProblemLocation::Task {
                        index: 2,
                        field: Some(ConfigTaskField::Command)
                    }
                )
        }));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Error
                && matches!(
                    problem.location,
                    ConfigProblemLocation::Task {
                        index: 2,
                        field: Some(ConfigTaskField::Command)
                    }
                )
        }));
        assert!(validate_for_path(&loaded.config, &path).is_err());
    }

    #[test]
    fn unvalidated_load_starts_fresh_after_unrecoverable_malformed_toml() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = "[[task]\nname = \"server\"\n";
        fs::write(&path, original).unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(loaded.config.tasks.is_empty());
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Warning
                && matches!(problem.location, ConfigProblemLocation::Root)
                && problem.message.contains("started a fresh draft")
        }));
        assert!(loaded.config_problems.iter().any(|problem| {
            problem.severity == ConfigProblemSeverity::Error
                && matches!(problem.location, ConfigProblemLocation::Tasks)
        }));
    }

    #[test]
    fn load_does_not_rewrite_current_version_config() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = r#"schema_version = 3

[[task]]
name = "server"
command = "echo ok"
"#;
        fs::write(&path, original).unwrap();

        LoadedConfig::load(path.clone()).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn rejects_unsupported_schema_versions_before_current_schema_parse() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            r#"
                schema_version = 4
                future_setting = true
            "#,
        )
        .unwrap();

        let error = parse_file(&path).unwrap_err().to_string();

        assert!(error.contains("uses config schema_version 4"));
        assert!(error.contains("supports schema_version 3"));
    }

    #[test]
    fn rejects_invalid_schema_version_values() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(&path, "schema_version = \"1\"\n").unwrap();

        let error = parse_file(&path).unwrap_err().to_string();

        assert!(error.contains("schema_version must be an integer"));
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
    fn validates_terminal_only_config_and_shared_pane_names() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("scratch")).unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let terminal_only: Config = toml::from_str(
            r#"
                [[terminal]]
                name = "scratch"
                cwd = "scratch"
            "#,
        )
        .unwrap();
        validate_for_path(&terminal_only, &path).unwrap();

        let duplicate: Config = toml::from_str(
            r#"
                [[task]]
                name = "scratch"
                command = "echo task"

                [[terminal]]
                name = "scratch"
                cwd = "."
            "#,
        )
        .unwrap();
        let error = validate_for_path(&duplicate, &path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate pane name"));
    }

    #[test]
    fn validates_dependencies_and_start_delay() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let valid: Config = toml::from_str(
            r#"
                [[task]]
                name = "server"
                command = "echo server"

                [[task]]
                name = "web"
                command = "echo web"
                depends_on = ["server"]
                start_delay = "3s"
            "#,
        )
        .unwrap();

        validate_for_path(&valid, &path).unwrap();
        assert_eq!(
            parse_start_delay("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_start_delay("2").unwrap(), Duration::from_secs(2));
        for value in ["0", "0ms", "0s", "0m", "0h"] {
            assert_eq!(parse_start_delay(value).unwrap(), Duration::ZERO);
        }
        assert!(parse_start_delay("0anything").is_err());
        assert!(parse_start_delay("18446744073709551615h").is_err());
    }

    #[test]
    fn rejects_unknown_dependency_and_cycles() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let unknown: Config = toml::from_str(
            r#"
                [[task]]
                name = "web"
                command = "echo web"
                depends_on = ["server"]
            "#,
        )
        .unwrap();
        assert!(validate_for_path(&unknown, &path).is_err());

        let cycle: Config = toml::from_str(
            r#"
                [[task]]
                name = "api"
                command = "echo api"
                depends_on = ["web"]

                [[task]]
                name = "web"
                command = "echo web"
                depends_on = ["api"]
            "#,
        )
        .unwrap();
        let error = validate_for_path(&cycle, &path).unwrap_err().to_string();
        assert!(error.contains("dependency cycle"));
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

        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(config.settings.leader, Leader::AltJ);
        assert_eq!(config.settings.multi_click_ms, DEFAULT_MULTI_CLICK_MS);
    }

    #[test]
    fn parses_alt_backtick_leader() {
        let config: Config = toml::from_str(
            r#"
                [settings]
                leader = "alt-backtick"

                [[task]]
                name = "server"
                command = "echo ready"
            "#,
        )
        .unwrap();

        assert_eq!(config.settings.leader, Leader::AltBacktick);
        assert_eq!(config.settings.leader.label(), "Alt-`");
    }

    #[test]
    fn validates_multi_click_timing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let config: Config = toml::from_str(
            r#"
                [settings]
                multi_click_ms = 400

                [[task]]
                name = "server"
                command = "echo ready"
            "#,
        )
        .unwrap();
        validate_for_path(&config, &path).unwrap();

        let invalid: Config = toml::from_str(
            r#"
                [settings]
                multi_click_ms = 20

                [[task]]
                name = "server"
                command = "echo ready"
            "#,
        )
        .unwrap();
        assert!(validate_for_path(&invalid, &path).is_err());
    }

    #[test]
    fn schema_v2_migrates_to_v3_with_mcp_off() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            r#"
                schema_version = 2

                [[task]]
                name = "server"
                command = "echo ready"
            "#,
        )
        .unwrap();

        let loaded = LoadedConfig::load(path.clone()).unwrap();

        assert_eq!(loaded.config.schema_version, 3);
        assert_eq!(loaded.config.settings.mcp_access, McpAccess::Off);
        assert!(loaded.config.settings.mcp_scope_id.is_none());
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("mcp_access = \"off\""));
    }

    #[test]
    fn enabled_mcp_requires_a_valid_scope_id() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let mut config = Config::default();
        config.tasks.push(Task {
            name: "server".to_owned(),
            command: TaskCommand::Shell("echo ready".to_owned()),
            cwd: PathBuf::from("."),
            env: BTreeMap::new(),
            depends_on: Vec::new(),
            start_delay: None,
            watch: None,
            run_on_change: None,
            repeat: None,
        });
        config.settings.mcp_access = McpAccess::ReadOnly;

        assert!(validate_for_path(&config, &path).is_err());
        config.settings.mcp_scope_id = Some(uuid::Uuid::new_v4().to_string());
        validate_for_path(&config, &path).unwrap();
    }

    #[test]
    fn configurator_recovers_missing_enabled_mcp_scope_without_rewriting() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        let original = r#"
            schema_version = 3

            [settings]
            mcp_access = "read_only"

            [[task]]
            name = "server"
            command = "echo ready"
        "#;
        fs::write(&path, original).unwrap();

        let loaded = LoadedConfig::load_unvalidated_or_default(path.clone()).unwrap();

        assert!(loaded.config.settings.mcp_scope_id.is_some());
        assert!(
            loaded
                .config_warnings
                .iter()
                .any(|warning| warning.contains("scope ID"))
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}
