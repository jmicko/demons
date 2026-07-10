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
        validate_config_target(&self.path, false)?;
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
    if let Err(error) = validate_config_target(&path, false) {
        return IntegrationStatus::Invalid(error.to_string());
    }
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

pub fn install(project_root: &Path, config_path: &Path, scope: &str) -> Result<CodexConfigChange> {
    uuid::Uuid::parse_str(scope).context("invalid MCP project scope ID")?;
    if !config_path.is_absolute() {
        bail!("MCP config path must be absolute");
    }
    config_path
        .to_str()
        .context("MCP config path must be valid UTF-8")?;
    edit(project_root, Some((scope, config_path)))
}

pub fn uninstall(project_root: &Path) -> Result<CodexConfigChange> {
    edit(project_root, None)
}

fn edit(project_root: &Path, server: Option<(&str, &Path)>) -> Result<CodexConfigChange> {
    let path = project_config_path(project_root);
    validate_config_target(&path, server.is_some())?;
    let previous = match fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let source = previous
        .as_ref()
        .map(|contents| String::from_utf8(contents.clone()))
        .transpose()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?
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

    match server {
        Some((scope, config_path)) => set_managed_server(&mut document, scope, config_path)?,
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
    validate_config_target(&path, true)?;
    if output.trim().is_empty() {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    } else {
        fs::write(&path, output).with_context(|| format!("failed to write {}", path.display()))?;
    }
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
    let has_marker = table
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| prefix.contains(MANAGED_MARKER));
    has_marker && table.get("command").and_then(Item::as_str) == Some("demons")
}

fn set_managed_server(document: &mut DocumentMut, scope: &str, config_path: &Path) -> Result<()> {
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
    let config_path = config_path
        .to_str()
        .context("MCP config path must be valid UTF-8")?;
    for arg in ["--config", config_path, "mcp", "serve", "--scope", scope] {
        args.push(arg);
    }
    server["args"] = value(args);
    server["default_tools_approval_mode"] = value("writes");
    servers.insert(SERVER_NAME, Item::Table(server));
    Ok(())
}

fn validate_config_target(path: &Path, create_parent: bool) -> Result<()> {
    let parent = path.parent().context("Codex config path has no parent")?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("{} is not a safe config directory", parent.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parent => {
            fs::create_dir(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", parent.display()));
        }
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("{} is not a safe config file", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
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
        let demons_config = temp.path().join("demons.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "model = \"gpt-test\"\n").unwrap();

        assert!(
            install(temp.path(), &demons_config, SCOPE)
                .unwrap()
                .changed()
        );
        assert_eq!(inspect(temp.path()), IntegrationStatus::Managed);
        let installed = fs::read_to_string(&path).unwrap();
        assert!(installed.contains("Managed by Demons"));
        assert!(installed.contains(SCOPE));
        assert!(installed.contains(demons_config.to_str().unwrap()));
        assert!(installed.contains("model = \"gpt-test\""));

        assert!(
            !install(temp.path(), &demons_config, SCOPE)
                .unwrap()
                .changed()
        );
        assert!(uninstall(temp.path()).unwrap().changed());
        assert_eq!(inspect(temp.path()), IntegrationStatus::Missing);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("model = \"gpt-test\"")
        );
    }

    #[test]
    fn removing_the_only_managed_entry_removes_the_config_file() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        let demons_config = temp.path().join("demons.toml");

        assert!(
            install(temp.path(), &demons_config, SCOPE)
                .unwrap()
                .changed()
        );
        assert!(path.is_file());

        assert!(uninstall(temp.path()).unwrap().changed());
        assert!(!path.exists());
        assert_eq!(inspect(temp.path()), IntegrationStatus::Missing);
    }

    #[test]
    fn uninstall_without_a_registration_does_not_create_codex_files() {
        let temp = tempdir().unwrap();

        assert!(!uninstall(temp.path()).unwrap().changed());
        assert!(!temp.path().join(".codex").exists());
    }

    #[test]
    fn refuses_to_replace_user_owned_entry() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        let demons_config = temp.path().join("demons.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[mcp_servers.demons]\ncommand = \"custom-server\"\n").unwrap();

        assert_eq!(inspect(temp.path()), IntegrationStatus::Conflict);
        assert!(install(temp.path(), &demons_config, SCOPE).is_err());
        assert!(uninstall(temp.path()).is_err());
    }

    #[test]
    fn marker_does_not_claim_a_non_demons_entry() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        let demons_config = temp.path().join("demons.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# Managed by Demons\n[mcp_servers.demons]\ncommand = \"custom-server\"\n",
        )
        .unwrap();

        assert_eq!(inspect(temp.path()), IntegrationStatus::Conflict);
        assert!(install(temp.path(), &demons_config, SCOPE).is_err());
    }

    #[test]
    fn refuses_to_rewrite_non_utf8_codex_config() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        let demons_config = temp.path().join("demons.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0xff, 0xfe]).unwrap();

        assert!(install(temp.path(), &demons_config, SCOPE).is_err());
        assert_eq!(fs::read(path).unwrap(), [0xff, 0xfe]);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_codex_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, temp.path().join(".codex")).unwrap();
        let demons_config = temp.path().join("demons.toml");

        assert!(matches!(
            inspect(temp.path()),
            IntegrationStatus::Invalid(_)
        ));
        assert!(install(temp.path(), &demons_config, SCOPE).is_err());
    }

    #[test]
    fn rollback_restores_original_file() {
        let temp = tempdir().unwrap();
        let path = project_config_path(temp.path());
        let demons_config = temp.path().join("demons.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "model = \"before\"\n").unwrap();

        install(temp.path(), &demons_config, SCOPE)
            .unwrap()
            .rollback()
            .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "model = \"before\"\n");
    }
}
