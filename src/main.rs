use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "llm-index", version)]
struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/llm-index/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, ValueEnum)]
enum Tier {
    Public,
    Private,
}

#[derive(Subcommand)]
enum Command {
    /// Long-running indexing service
    Index,
    /// Sandboxed worker child of `index`
    Worker,
    /// MCP server on stdio (read-only)
    Mcp {
        #[arg(long, value_enum)]
        tier: Tier,
    },
    /// Show index status
    Status,
    /// Print build-time store paths
    #[command(hide = true)]
    Paths,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Paths) {
        println!("pdfium={}", llm_index::pdfium_lib());
        println!("model={}", llm_index::model_dir());
        return ExitCode::SUCCESS;
    }
    eprintln!("not implemented");
    ExitCode::from(2)
}
