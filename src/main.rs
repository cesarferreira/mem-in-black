mod config;
mod daemon;
mod distill;
mod eval;
mod hooks;
mod import;
mod install;
mod mcp;
mod observer;
mod preview;
mod store;
mod workspace;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hooks::Host;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};
use store::Store;
use workspace::Workspace;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true, env = "QUIETMEM_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register agent hooks and local MCP servers.
    Install {
        #[arg(long)]
        home: Option<PathBuf>,
        #[arg(long, value_enum)]
        host: Option<install::Target>,
    },
    /// Consume a host hook's JSON.
    Hook { host: Host },
    /// Run the MCP server over stdio.
    Mcp,
    /// Run the worker and local viewer in the foreground.
    Serve,
    /// Stop the background worker.
    Stop,
    /// Inspect the daemon, viewer URL and stored observation counts.
    Status,
    /// Process pending observations once.
    Observe,
    /// Import observations and summaries from a Claude Mem SQLite database.
    Import {
        database: PathBuf,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Export stored records as newline-delimited JSON.
    Export,
    /// Show the recent activity for this worktree.
    Preview {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = preview::Color::Auto)]
        color: preview::Color,
    },
    /// Benchmark retrieval against a Claude Mem database (development tool).
    #[command(hide = true)]
    Eval {
        database: PathBuf,
        #[arg(long, default_value_t = 300)]
        sample: usize,
    },
    /// Delete records by ID, with the summaries built from them.
    Forget {
        #[arg(required = true)]
        ids: Vec<i64>,
    },
    /// Search local memory or list recent observations.
    Search {
        query: Option<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = config::Storage::resolve(cli.data_dir)?;
    match cli.command {
        Command::Install { home, host } => {
            store::prepare_dir(&data.directory)?;
            let directory = data
                .directory_override()
                .map(|path| path.canonicalize())
                .transpose()?;
            install::install(directory.as_deref(), home.as_deref(), host)?;
            println!(
                "Registered QuietMem. Restart the configured harnesses and review the QuietMem hooks if Codex asks."
            );
        }
        Command::Hook { host } => {
            let _ = daemon::start_background(&data);
            match Store::open(&data)
                .context("opening memory store")
                .and_then(|store| hooks::handle(&store, host, io::stdin().lock()))
            {
                Ok(Some(response)) => println!("{response}"),
                Err(error) if std::env::var_os("QUIETMEM_DEBUG").is_some() => {
                    eprintln!("QuietMem hook: {error:#}")
                }
                _ => {}
            }
        }
        Command::Mcp => mcp::run_stdio(data)?,
        Command::Serve => daemon::run(&data)?,
        Command::Stop => daemon::stop(&data)?,
        Command::Status => println!("{}", serde_json::to_string_pretty(&daemon::status(&data)?)?),
        Command::Observe => daemon::observe_once(&data)?,
        Command::Import {
            database,
            workspace,
            project,
        } => {
            let count = import::claude_mem(
                &Store::open(&data)?,
                &database,
                workspace.as_deref(),
                project.as_deref(),
            )?;
            println!("Imported {count} records.");
        }
        Command::Export => Store::open(&data)?.export(io::stdout().lock())?,
        Command::Eval { database, sample } => {
            let temp = tempfile::tempdir()?;
            let (store, queries) = eval::prepare(&database, &temp.path().join("data"), sample)?;
            let report = eval::score(&store, &queries, &eval::lexical_methods())?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Forget { ids } => {
            let removed = Store::open(&data)?.forget(&ids)?;
            println!("Forgot {removed} records.");
        }
        Command::Preview { workspace, color } => {
            let directory = workspace.map(Ok).unwrap_or_else(std::env::current_dir)?;
            let workspace = Workspace::read(&directory);
            let color = color.enabled(io::stdout().is_terminal());
            if let Some(message) = preview::render(
                &Store::open(&data)?,
                &workspace,
                preview::TERMINAL_LIMIT,
                color,
            )? {
                println!("{message}");
            }
        }
        Command::Search {
            query,
            workspace,
            limit,
        } => {
            let store = Store::open(&data)?;
            let root = workspace.map(|path| Workspace::read(&path).root);
            println!(
                "{}",
                serde_json::to_string_pretty(&mcp::search(
                    &store,
                    query.as_deref(),
                    root.as_deref(),
                    limit
                )?)?
            );
        }
    }
    Ok(())
}
