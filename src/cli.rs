use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "demons",
    version,
    about = "Run your project's development commands side-by-side"
)]
pub struct Cli {
    /// Read configuration from this file instead of searching parent directories.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the configurator without starting tasks.
    Init,
}
