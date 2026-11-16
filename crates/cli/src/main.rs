//! Command-line interface for the vault engine.
//!
//! Every command delegates to [`selenite_index::Engine`]; this file only parses arguments and
//! formats output. Note contents are never logged.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use selenite_core::{APP_NAME, VaultPath};
use selenite_index::{Engine, HIGHLIGHT_END, HIGHLIGHT_START};
use selenite_vault::layout;
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "selenite",
    version,
    about = "Local-first notes, workspace and tasks engine"
)]
struct Cli {
    /// Vault folder (default: current directory). Not used by `init`.
    #[arg(long, global = true, value_name = "PATH")]
    vault: Option<PathBuf>,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new vault at PATH.
    Init {
        /// Folder to create the vault in.
        path: PathBuf,
    },
    /// Open the vault and refresh its index.
    Open,
    /// Create a page from a title.
    Add {
        /// Page title; also the file name.
        title: String,
        /// Vault folder to create the page in.
        #[arg(long)]
        folder: Option<String>,
        /// Page body. Without it, stdin is read when piped, else a heading is written.
        #[arg(long)]
        body: Option<String>,
    },
    /// Full-text search. Supports the engine's query syntax (tag:, path:, -term, "phrases").
    Search {
        /// Query words, joined with spaces.
        #[arg(required = true, allow_hyphen_values = true, value_name = "QUERY")]
        query: Vec<String>,
        /// Maximum number of results.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List pages that link to PAGE (a vault path or a link target).
    Backlinks {
        /// Vault path ("Projects/Beta.md") or link target ("Beta").
        page: String,
    },
    /// List outgoing links of PAGE with their resolution.
    Links {
        /// Vault path or link target.
        page: String,
    },
    /// List tags with page counts.
    Tags,
    /// Create (if needed) and print the daily note path.
    Daily {
        /// Date as YYYY-MM-DD (default: today).
        #[arg(long, value_name = "YYYY-MM-DD")]
        date: Option<NaiveDate>,
    },
    /// Drop the index and rebuild it from the vault files.
    RebuildIndex,
    /// Sync with other devices (not available yet).
    Sync,
    /// Import notes from another app (not available yet).
    Import {
        /// Source app name.
        source: String,
        /// Folder to import into.
        path: PathBuf,
    },
}

#[derive(Serialize)]
struct InitOut {
    path: String,
}

#[derive(Serialize)]
struct PathOut {
    path: String,
}

#[derive(Serialize)]
struct OpenOut {
    pages: u64,
    tags: usize,
}

#[derive(Serialize)]
struct SearchOut {
    path: String,
    title: String,
    snippet: String,
    score: f64,
}

#[derive(Serialize)]
struct BacklinkOut {
    source: String,
    source_title: String,
    line: u32,
    context: String,
    embed: bool,
}

#[derive(Serialize)]
struct LinkOut {
    line: u32,
    target: String,
    resolved: Option<String>,
    embed: bool,
}

#[derive(Serialize)]
struct TagOut {
    tag: String,
    count: u32,
}

#[derive(Serialize)]
struct RebuildOut {
    pages: usize,
}

fn main() -> ExitCode {
    init_logging();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

/// Tracing goes to stderr; the level comes from `RUST_LOG` and defaults to `warn`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> Result<ExitCode> {
    let Cli {
        vault,
        json,
        command,
    } = cli;
    let open = || Engine::open_local(&vault.clone().unwrap_or_else(|| PathBuf::from(".")));
    match command {
        Command::Init { path } => {
            if vault.is_some() {
                bail!("`init` takes the folder as its argument; --vault does not apply");
            }
            Engine::init_local(&path)?;
            if json {
                print_json(&InitOut {
                    path: path.display().to_string(),
                })?;
            } else {
                println!("Created vault at {}", path.display());
            }
        }
        Command::Open => {
            let engine = open()?;
            let pages = engine.index().page_count()?;
            let tags = engine.tags()?.len();
            if json {
                print_json(&OpenOut { pages, tags })?;
            } else {
                println!("{pages} pages, {tags} tags");
            }
        }
        Command::Add {
            title,
            folder,
            body,
        } => {
            let mut engine = open()?;
            let piped = match body {
                Some(body) => Some(body),
                None if !std::io::stdin().is_terminal() => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    Some(text).filter(|t| !t.trim().is_empty())
                }
                None => None,
            };
            let body = piped.unwrap_or_else(|| format!("# {title}\n"));
            let path = engine.create_titled_page(&title, folder.as_deref(), &body)?;
            print_path(json, &path)?;
        }
        Command::Search { query, limit } => {
            let engine = open()?;
            cmd_search(&engine, json, &query.join(" "), limit)?;
        }
        Command::Backlinks { page } => {
            let engine = open()?;
            let path = resolve_page(&engine, &page)?;
            let links = engine.backlinks(&path)?;
            if json {
                let out: Vec<BacklinkOut> = links
                    .into_iter()
                    .map(|b| BacklinkOut {
                        source: b.source.to_string(),
                        source_title: b.source_title,
                        line: b.line,
                        context: b.context,
                        embed: b.embed,
                    })
                    .collect();
                print_json(&out)?;
            } else {
                for b in &links {
                    println!("{}:{}  {}", b.source, b.line, b.context);
                }
            }
        }
        Command::Links { page } => {
            let engine = open()?;
            let path = resolve_page(&engine, &page)?;
            let links = engine.outgoing(&path)?;
            if json {
                let out: Vec<LinkOut> = links
                    .into_iter()
                    .map(|l| LinkOut {
                        line: l.line,
                        target: l.target,
                        resolved: l.resolved.map(|p| p.to_string()),
                        embed: l.embed,
                    })
                    .collect();
                print_json(&out)?;
            } else {
                for l in &links {
                    let target = match &l.resolved {
                        Some(p) => p.to_string(),
                        None => "(unresolved)".to_string(),
                    };
                    println!("{}  {} -> {}", l.line, l.target, target);
                }
            }
        }
        Command::Tags => {
            let engine = open()?;
            let tags = engine.tags()?;
            if json {
                let out: Vec<TagOut> = tags
                    .into_iter()
                    .map(|t| TagOut {
                        tag: t.tag,
                        count: t.count,
                    })
                    .collect();
                print_json(&out)?;
            } else {
                for t in &tags {
                    println!("{} ({})", t.tag, t.count);
                }
            }
        }
        Command::Daily { date } => {
            let mut engine = open()?;
            let date = date.unwrap_or_else(|| chrono::Local::now().date_naive());
            let path = engine.daily_note(date)?;
            print_path(json, &path)?;
        }
        Command::RebuildIndex => {
            let mut engine = open()?;
            let pages = engine.rebuild_index()?;
            if json {
                print_json(&RebuildOut { pages })?;
            } else {
                println!("Indexed {pages} pages");
            }
        }
        Command::Sync => {
            eprintln!("error: {APP_NAME} sync is not available until a later phase");
            return Ok(ExitCode::from(2));
        }
        Command::Import { .. } => {
            eprintln!("error: {APP_NAME} import is not available until a later phase");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_search(engine: &Engine, json: bool, query: &str, limit: usize) -> Result<()> {
    let hits = engine.search(query, limit)?;
    if json {
        let out: Vec<SearchOut> = hits
            .into_iter()
            .map(|h| SearchOut {
                path: h.path.to_string(),
                title: h.title,
                snippet: highlight_markup(&h.snippet),
                score: h.score,
            })
            .collect();
        return print_json(&out);
    }
    for h in &hits {
        println!("{} — {}", h.path, h.title);
        for line in h.snippet.lines() {
            println!("    {}", highlight_markup(line));
        }
    }
    Ok(())
}

/// Replace the index's highlight markers with `**` for plain-text output.
fn highlight_markup(snippet: &str) -> String {
    snippet.replace([HIGHLIGHT_START, HIGHLIGHT_END], "**")
}

/// Accept a vault path if it is an indexed page, otherwise resolve it as a link target.
fn resolve_page(engine: &Engine, page: &str) -> Result<VaultPath> {
    if let Ok(path) = VaultPath::new(page) {
        if layout::is_page(&path) && engine.index().page(&path)?.is_some() {
            return Ok(path);
        }
    }
    match engine.resolve(page)? {
        Some(path) => Ok(path),
        None => bail!("no page named {page}"),
    }
}

fn print_path(json: bool, path: &VaultPath) -> Result<()> {
    if json {
        print_json(&PathOut {
            path: path.to_string(),
        })
    } else {
        println!("{path}");
        Ok(())
    }
}

fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
