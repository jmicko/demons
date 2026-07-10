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
    /// Run the project-scoped Model Context Protocol adapter.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Serve MCP over standard input and output.
    Serve {
        /// Opaque project scope created by the Demons configurator.
        #[arg(long, hide = true)]
        scope: String,
    },
}
