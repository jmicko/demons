mod cli;
mod config;
mod init;
mod layout;
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
    cli::{Cli, Command},
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

    if matches!(cli.command, Some(Command::Init)) {
        let path = init_path(cli.config, &cwd);
        let result = init::run(path)?;
        if result.start {
            runtime::run(LoadedConfig::load(result.path)?)?;
        }
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
                let result = init::run(cwd.join(CONFIG_FILE))?;
                if !result.start {
                    return Ok(());
                }
                result.path
            } else {
                return Ok(());
            }
        }
        None => {
            bail!("no {CONFIG_FILE} found in {} or its parents", cwd.display())
        }
    };

    let loaded = LoadedConfig::load(path)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("demons requires an interactive terminal");
    }
    runtime::run(loaded)
}

fn init_path(explicit: Option<PathBuf>, cwd: &std::path::Path) -> PathBuf {
    match explicit {
        Some(path) if path.is_absolute() => path,
        Some(path) => cwd.join(path),
        None => cwd.join(CONFIG_FILE),
    }
}
