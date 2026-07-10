mod capture;
mod cli;
mod codex_config;
mod config;
#[cfg(unix)]
mod control;
mod init;
mod layout;
#[cfg(unix)]
mod mcp_server;
mod runtime;

use std::{
    env,
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{
    cli::{Cli, Command, McpCommand},
    config::{CONFIG_FILE, LoadedConfig, explicit_or_discover},
};

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir().context("failed to determine current directory")?;

    if let Some(Command::Mcp {
        command: McpCommand::Serve { scope },
    }) = &cli.command
    {
        let config_path = cli
            .config
            .as_deref()
            .context("demons mcp serve requires --config")?;
        let config_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            cwd.join(config_path)
        };
        #[cfg(unix)]
        return mcp_server::serve(scope.clone(), config_path);
        #[cfg(not(unix))]
        bail!("the Demons MCP adapter currently requires Unix domain sockets");
    }

    if matches!(cli.command, Some(Command::Init)) {
        let path = init_path(cli.config, &cwd)?;
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("demons init requires an interactive terminal");
        }
        runtime::configure(LoadedConfig::load_unvalidated_or_default(path)?)?;
        return Ok(());
    }

    let path = match explicit_or_discover(cli.config, &cwd)? {
        Some(path) => path,
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            let should_init = {
                let mut input = io::stdin().lock();
                let mut output = io::stdout().lock();
                let question = format!(
                    "No {CONFIG_FILE} found in {} or its parents. Run 'demons init' here?",
                    cwd.display()
                );
                init::prompt_yes_no(&mut input, &mut output, &question, true)?
            };
            if should_init {
                runtime::configure(LoadedConfig::load_unvalidated_or_default(
                    cwd.join(CONFIG_FILE),
                )?)?;
                return Ok(());
            } else {
                return Ok(());
            }
        }
        None => {
            bail!("no {CONFIG_FILE} found in {} or its parents", cwd.display())
        }
    };

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        LoadedConfig::load(path)?;
        bail!("demons requires an interactive terminal");
    }

    let loaded = match LoadedConfig::load(path.clone()) {
        Ok(loaded) => loaded,
        Err(error) => match LoadedConfig::load_unvalidated_or_default(path) {
            Ok(loaded) if !loaded.config_problems.is_empty() => {
                return runtime::recover_then_run(loaded);
            }
            _ => return Err(error),
        },
    };
    runtime::run(loaded)
}

fn init_path(explicit: Option<PathBuf>, cwd: &std::path::Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    }

    Ok(explicit_or_discover(None, cwd)?.unwrap_or_else(|| cwd.join(CONFIG_FILE)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn init_path_uses_nearest_existing_config_without_explicit_path() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("app/src");
        fs::create_dir_all(&nested).unwrap();
        let config = root.join(CONFIG_FILE);
        fs::write(&config, "").unwrap();

        assert_eq!(init_path(None, &nested).unwrap(), config);
    }

    #[test]
    fn init_path_creates_in_current_directory_when_no_config_exists() {
        let temp = tempdir().unwrap();

        assert_eq!(
            init_path(None, temp.path()).unwrap(),
            temp.path().join(CONFIG_FILE)
        );
    }
}
