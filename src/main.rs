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

#[derive(Clone, ValueEnum)]
enum WorkerRole {
    Scan,
    Extract,
    Watch,
    Validate,
}

#[derive(Subcommand)]
enum Command {
    /// Long-running indexing service
    Index,
    /// Sandboxed worker child of `index`
    Worker {
        #[arg(long, value_enum)]
        role: WorkerRole,
        #[arg(long)]
        root: Option<String>,
    },
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
    if let Command::Worker { role, root } = cli.command {
        let role = match role {
            WorkerRole::Scan => "scan",
            WorkerRole::Extract => "extract",
            WorkerRole::Watch => "watch",
            WorkerRole::Validate => "validate",
        };
        let config = match cli
            .config
            .map(Ok)
            .unwrap_or_else(llm_index::config::default_path)
        {
            Ok(path) => path,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
        return match llm_index::worker::run(&config, role, root.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(2)
            }
        };
    }
    eprintln!("not implemented");
    ExitCode::from(2)
}
