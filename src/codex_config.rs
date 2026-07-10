use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Table, value};

const SERVER_NAME: &str = "demons";
const MANAGED_MARKER: &str = "Managed by Demons";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationStatus {
    Missing,
    Managed,
    Conflict,
    Invalid(String),
}

impl IntegrationStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Missing => "not installed",
            Self::Managed => "installed",
            Self::Conflict => "conflicting entry",
            Self::Invalid(_) => "invalid Codex config",
        }
    }
}

#[derive(Debug)]
pub struct CodexConfigChange {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    changed: bool,
}

impl CodexConfigChange {
    pub fn changed(&self) -> bool {
        self.changed
    }

    pub fn rollback(self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        match self.previous {
            Some(contents) => fs::write(&self.path, contents)
                .with_context(|| format!("failed to restore {}", self.path.display())),
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("failed to remove {}", self.path.display()))
                }
            },
        }
    }
}

pub fn project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".codex").join("config.toml")
}

pub fn inspect(project_root: &Path) -> IntegrationStatus {
    let path = project_config_path(project_root);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return IntegrationStatus::Missing;
        }
        Err(error) => return IntegrationStatus::Invalid(error.to_string()),
    };
    let document = match source.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(error) => return IntegrationStatus::Invalid(error.to_string()),
    };
    match server_item(&document) {
        None => IntegrationStatus::Missing,
        Some(item) if is_managed(item) => IntegrationStatus::Managed,
        Some(_) => IntegrationStatus::Conflict,
    }
}

pub fn install(project_root: &Path, scope: &str) -> Result<CodexConfigChange> {
    uuid::Uuid::parse_str(scope).context("invalid MCP project scope ID")?;
    edit(project_root, Some(scope))
}

pub fn uninstall(project_root: &Path) -> Result<CodexConfigChange> {
    edit(project_root, None)
}

fn edit(project_root: &Path, scope: Option<&str>) -> Result<CodexConfigChange> {
    let path = project_config_path(project_root);
    let previous = match fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let source = previous
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let mut document = source
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let existing = server_item(&document);
    if existing.is_some_and(|item| !is_managed(item)) {
        bail!(
            "{} already contains a user-owned [mcp_servers.{SERVER_NAME}] entry",
            path.display()
        );
    }

    match scope {
        Some(scope) => set_managed_server(&mut document, scope)?,
        None => remove_managed_server(&mut document),
    }

    let output = document.to_string();
    if previous.as_deref() == Some(output.as_bytes())
        || (previous.is_none() && output.trim().is_empty())
    {
        return Ok(CodexConfigChange {
            path,
            previous,
            changed: false,
        });
    }
    if output.trim().is_empty() && previous.is_none() {
        return Ok(CodexConfigChange {
            path,
            previous,
            changed: false,
        });
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, output).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(CodexConfigChange {
        path,
        previous,
        changed: true,
    })
}

fn server_item(document: &DocumentMut) -> Option<&Item> {
    document
        .get("mcp_servers")?
        .as_table_like()?
        .get(SERVER_NAME)
}

fn is_managed(item: &Item) -> bool {
    let Some(table) = item.as_table() else {
        return false;
    };
    table
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| prefix.contains(MANAGED_MARKER))
}

fn set_managed_server(document: &mut DocumentMut, scope: &str) -> Result<()> {
    if document.get("mcp_servers").is_none() {
        document["mcp_servers"] = Item::Table(Table::new());
    }
    let servers = document["mcp_servers"]
        .as_table_mut()
        .context("mcp_servers must be a TOML table")?;
    let mut server = Table::new();
    server
        .decor_mut()
        .set_prefix(format!("\n# {MANAGED_MARKER}\n"));
    server["command"] = value("demons");
    let mut args = Array::new();
    for arg in ["mcp", "serve", "--scope", scope] {
        args.push(arg);
    }
    server["args"] = value(args);
    server["default_tools_approval_mode"] = value("writes");
    servers.insert(SERVER_NAME, Item::Table(server));
    Ok(())
}

fn remove_managed_server(document: &mut DocumentMut) {
    let Some(servers) = document.get_mut("mcp_servers").and_then(Item::as_table_mut) else {
        return;
    };
    if servers.get(SERVER_NAME).is_some_and(is_managed) {
        servers.remove(SERVER_NAME);
    }
    if servers.is_empty() {
        document.remove("mcp_servers");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SCOPE: &str = "3f4a7f63-2492-477a-ae7f-92bffab78fa4";

    #[test]
    fn installs_updates_and_removes_only_managed_entry() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "model = \"gpt-test\"\n").unwrap();

        assert!(install(temp.path(), SCOPE).unwrap().changed());
        assert_eq!(inspect(temp.path()), IntegrationStatus::Managed);
        let installed = fs::read_to_string(&path).unwrap();
        assert!(installed.contains("Managed by Demons"));
        assert!(installed.contains(SCOPE));
        assert!(installed.contains("model = \"gpt-test\""));

        assert!(!install(temp.path(), SCOPE).unwrap().changed());
        assert!(uninstall(temp.path()).unwrap().changed());
        assert_eq!(inspect(temp.path()), IntegrationStatus::Missing);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("model = \"gpt-test\"")
        );
    }

    #[test]
    fn refuses_to_replace_user_owned_entry() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[mcp_servers.demons]\ncommand = \"custom-server\"\n").unwrap();

        assert_eq!(inspect(temp.path()), IntegrationStatus::Conflict);
        assert!(install(temp.path(), SCOPE).is_err());
        assert!(uninstall(temp.path()).is_err());
    }

    #[test]
    fn rollback_restores_original_file() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "model = \"before\"\n").unwrap();

        install(temp.path(), SCOPE).unwrap().rollback().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "model = \"before\"\n");
    }
}
