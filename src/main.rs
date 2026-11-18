mod config;
mod daemon;
mod distill;
mod embed;
mod eval;
mod hooks;
mod import;
mod install;
mod mcp;
mod observer;
mod preview;
mod stats;
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
        /// Comma-separated embedding models to compare, downloaded on first use.
        #[arg(long, value_delimiter = ',')]
        models: Vec<String>,
    },
    /// Show how QuietMem has been used and an estimate of the tokens it saved.
    Stats {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Manage local semantic search.
    Embeddings {
        #[command(subcommand)]
        action: EmbeddingsAction,
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

#[derive(Subcommand)]
enum EmbeddingsAction {
    /// Download and verify a local model, then use it for search and recall.
    Enable {
        #[arg(long, default_value = embed::DEFAULT_MODEL)]
        model: String,
    },
    /// Return to keyword-only search. Downloaded models stay on disk.
    Disable,
    /// Show the configured model and how many records it has embedded.
    Status,
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
            let model = embed::configured(&data.directory).ok().flatten();
            let semantic = |prompt: &str, project: &str| {
                let spec = model?;
                let hits = daemon::semantic_search(
                    &data,
                    prompt,
                    &embed::Scope::Project(project),
                    12,
                    std::time::Duration::from_millis(900),
                )?;
                Some(
                    hits.into_iter()
                        .filter(|(_, score)| *score >= spec.assisted_floor)
                        .map(|(id, score)| (id, score >= spec.recall_floor))
                        .collect(),
                )
            };
            match Store::open(&data)
                .context("opening memory store")
                .and_then(|store| {
                    hooks::handle(
                        &store,
                        host,
                        io::stdin().lock(),
                        model.is_some().then_some(&semantic),
                    )
                }) {
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
        Command::Eval {
            database,
            sample,
            models,
        } => {
            let temp = tempfile::tempdir()?;
            let (store, queries) = eval::prepare(&database, &temp.path().join("data"), sample)?;
            let mut report = eval::score(&store, &queries, &eval::lexical_methods())?;
            for name in models {
                let spec = embed::spec(&name)?;
                embed::install(&data.directory, spec, io::stderr().is_terminal())?;
                let embedder = embed::Embedder::load(&data.directory, spec)?;
                let mut index = embed::Index::default();
                eval::embed_all(&store, &embedder, &mut index)?;
                let scored = eval::score(
                    &store,
                    &queries,
                    &eval::embedding_methods(&embedder, &index),
                )?;
                for (method, value) in scored.as_object().into_iter().flatten() {
                    report[format!("{method}:{name}")] = value.clone();
                }
                report[format!("calibration:{name}")] =
                    eval::calibrate(&queries, &embedder, &index)?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Stats { workspace, json } => {
            let root = workspace.map(|path| Workspace::read(&path).root);
            let stats = Store::open(&data)?.stats(root.as_deref())?;
            if json {
                let mut value = serde_json::to_value(&stats)?;
                value["estimated_tokens_saved"] = stats.estimated_tokens_saved().into();
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", stats::text(&stats));
            }
        }
        Command::Embeddings { action } => match action {
            EmbeddingsAction::Enable { model } => {
                let spec = embed::spec(&model)?;
                let size: u64 = spec.files.iter().map(|(_, _, size)| size).sum();
                eprintln!(
                    "Downloading {} ({} MB) from huggingface.co/{} at {}…",
                    spec.name,
                    size / 1_000_000,
                    spec.repo,
                    &spec.revision[..12]
                );
                embed::install(&data.directory, spec, io::stderr().is_terminal())?;
                eprintln!("Verified. Checking that the model loads…");
                embed::Embedder::load(&data.directory, spec)?.embed_query("check")?;
                embed::configure(&data.directory, Some(spec))?;
                let _ = daemon::stop(&data);
                println!(
                    "Enabled {}. The worker embeds existing memory in the background; check progress with `quietmem embeddings status`.",
                    spec.name
                );
            }
            EmbeddingsAction::Disable => {
                embed::configure(&data.directory, None)?;
                let _ = daemon::stop(&data);
                println!("Disabled semantic search. Keyword search stays available.");
            }
            EmbeddingsAction::Status => {
                let store = Store::open(&data)?;
                let spec = embed::configured(&data.directory)?;
                let records: i64 = store.counts()?.values().sum();
                let embedded = match spec {
                    Some(spec) => store.vectors(spec.name)?.len(),
                    None => 0,
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "model": spec.map(|spec| spec.name),
                        "installed": spec.map(|spec| embed::installed(&data.directory, spec)),
                        "records": records,
                        "embedded": embedded,
                    }))?
                );
            }
        },
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
            let semantic =
                mcp::worker_semantic(&data, root.as_deref(), std::time::Duration::from_secs(5));
            println!(
                "{}",
                serde_json::to_string_pretty(&mcp::search(
                    &store,
                    query.as_deref(),
                    root.as_deref(),
                    limit,
                    Some(&semantic),
                )?)?
            );
        }
    }
    Ok(())
}
