//! Command-line interface for the vault engine.
//!
//! Every command delegates to [`selenite_index::Engine`]; this file only parses arguments and
//! formats output. Note contents are never logged.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use clap::{Parser, Subcommand, ValueEnum};
use selenite_core::{Due, Priority, TaskFields, VaultPath};
use selenite_index::{Engine, HIGHLIGHT_END, HIGHLIGHT_START, TaskItem};
use selenite_reminders::{PlannerInput, WINDOW_DAYS, plan};
use selenite_storage::oauth::default_open_browser;
use selenite_sync::{Provider, RemoteConfig, client_config, keychain_store};
use selenite_vault::{SavedView, layout};
use serde::Serialize;

mod remotes;

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
    /// Serve the vault to AI agents over MCP (stdio). Logs go to stderr.
    Mcp,
    /// Sync the vault with its cloud remotes (exit 0 ok, 1 error, 3 offline).
    Sync {
        /// Only this remote (default: all). See `remote list`.
        #[arg(long, value_name = "ID")]
        remote: Option<String>,
    },
    /// Manage cloud remotes (Google Drive, OneDrive).
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Import notes or tasks from another app into the vault.
    Import {
        /// Where the data comes from.
        source: ImportSource,
        /// Obsidian vault folder, Notion export (.zip or folder), or Todoist CSV (file or folder).
        path: PathBuf,
        /// Vault folder to import into (default: Imported/<Source> <date>).
        #[arg(long, value_name = "FOLDER")]
        into: Option<String>,
        /// Report what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Task commands (tasks are Markdown rows in the Tasks database).
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Open tasks per day for the next N days, plus overdue tasks.
    Agenda {
        /// Number of days, starting today.
        #[arg(long, default_value_t = 7)]
        days: u32,
    },
    /// Reminder tools.
    Reminders {
        #[command(subcommand)]
        command: RemindersCommand,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Add a task from one line, e.g. "pay rent every 1st at 9am p1 #home".
    Add {
        /// Task text, joined with spaces.
        #[arg(required = true, allow_hyphen_values = true, value_name = "TEXT")]
        text: Vec<String>,
    },
    /// List tasks. Names: inbox, today, upcoming, completed; anything else is a filter.
    List {
        /// Filter expression (default: all open tasks).
        #[arg(allow_hyphen_values = true, value_name = "FILTER")]
        filter: Vec<String>,
        /// Append each task's vault path.
        #[arg(long)]
        paths: bool,
    },
    /// Complete a task. Recurring tasks move to their next occurrence.
    Done {
        /// Vault path ("Tasks/pay rent.md") or exact title.
        task: String,
    },
    /// Reopen a completed task.
    Reopen {
        /// Vault path or exact title.
        task: String,
    },
    /// Move a task to the vault trash.
    Archive {
        /// Vault path or exact title.
        task: String,
    },
    /// List projects with open-task counts.
    Projects,
    /// List labels in use.
    Labels,
    /// List saved views of the Tasks database.
    Views,
    /// Save a view (replaces a view with the same name).
    ViewSave {
        /// View name.
        name: String,
        /// Filter expression.
        #[arg(required = true, allow_hyphen_values = true, value_name = "FILTER")]
        filter: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportSource {
    Obsidian,
    Notion,
    Todoist,
}

fn cmd_import(
    engine: &mut Engine,
    json: bool,
    source: ImportSource,
    path: &std::path::Path,
    into: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let mut opts = selenite_import::ImportOptions::new(now());
    opts.into = into;
    opts.dry_run = dry_run;
    let report = match source {
        ImportSource::Obsidian => selenite_import::import_obsidian(engine, path, &opts)?,
        ImportSource::Notion => selenite_import::import_notion(engine, path, &opts)?,
        ImportSource::Todoist => selenite_import::import_todoist(engine, path, &opts)?,
    };
    if json {
        return print_json(&report);
    }
    let verb = if report.dry_run {
        "Would import"
    } else {
        "Imported"
    };
    println!(
        "{verb} into {}: {} pages, {} rows, {} tasks, {} attachments, {} skipped",
        report.target,
        report.pages,
        report.rows,
        report.tasks,
        report.attachments,
        report.skipped.len()
    );
    for (p, reason) in &report.skipped {
        println!("  skipped {p}: {reason}");
    }
    Ok(())
}

#[derive(Clone, Copy, ValueEnum)]
enum ProviderArg {
    #[value(name = "google-drive")]
    GoogleDrive,
    #[value(name = "onedrive")]
    Onedrive,
}

impl From<ProviderArg> for Provider {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::GoogleDrive => Provider::GoogleDrive,
            ProviderArg::Onedrive => Provider::OneDrive,
        }
    }
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Sign in to a provider in your browser and add it as a remote.
    Add {
        /// Cloud provider.
        provider: ProviderArg,
        /// Remote folder name (default: the vault folder's name). OneDrive uses its app folder.
        #[arg(long)]
        folder: Option<String>,
    },
    /// List configured remotes.
    List,
    /// Remove a remote and its stored sign-in. Files in the cloud are never deleted.
    Remove {
        /// Remote id, e.g. gdrive-1.
        id: String,
    },
}

#[derive(Subcommand)]
enum RemindersCommand {
    /// Dry run: print the reminders the app would register for the next 7 days.
    Plan,
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

#[derive(Serialize)]
struct AddOut {
    path: String,
    fields: TaskFields,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ArchiveOut {
    path: String,
    trashed: String,
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
    let vault_root = || vault.clone().unwrap_or_else(|| PathBuf::from("."));
    let open = || Engine::open_local(&vault_root());
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
        Command::Mcp => {
            let engine = open()?;
            selenite_mcp::run_stdio(engine)?;
        }
        Command::Sync { remote } => {
            let open_remote =
                |r: &RemoteConfig| selenite_sync::open_remote(r, keychain_store(r.provider, &r.id));
            let code = remotes::sync_cmd(
                &vault_root(),
                remote.as_deref(),
                json,
                &open_remote,
                &mut std::io::stdout(),
            )?;
            return Ok(ExitCode::from(code));
        }
        Command::Remote { command } => {
            let root = vault_root();
            let store_for = |p: Provider, id: &str| keychain_store(p, id);
            let mut out = std::io::stdout();
            match command {
                RemoteCommand::Add { provider, folder } => {
                    let provider = Provider::from(provider);
                    remotes::remote_add(
                        &root,
                        provider,
                        folder,
                        client_config(provider),
                        &store_for,
                        &default_open_browser,
                        json,
                        &mut out,
                    )?;
                }
                RemoteCommand::List => remotes::remote_list(&root, json, &mut out)?,
                RemoteCommand::Remove { id } => {
                    remotes::remote_remove(&root, &id, &store_for, json, &mut out)?;
                }
            }
        }
        Command::Import {
            source,
            path,
            into,
            dry_run,
        } => {
            let mut engine = open()?;
            cmd_import(&mut engine, json, source, &path, into, dry_run)?;
        }
        Command::Task { command } => {
            let mut engine = open()?;
            cmd_task(&mut engine, json, command)?;
        }
        Command::Agenda { days } => {
            let engine = open()?;
            cmd_agenda(&engine, json, days)?;
        }
        Command::Reminders { command } => {
            let engine = open()?;
            match command {
                RemindersCommand::Plan => cmd_reminders_plan(&engine, json)?,
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Local wall-clock time. Only the CLI reads the clock; the engine takes explicit times.
fn now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

fn cmd_task(engine: &mut Engine, json: bool, command: TaskCommand) -> Result<()> {
    let today = chrono::Local::now().date_naive();
    match command {
        TaskCommand::Add { text } => {
            let (path, warnings) = engine.quick_add(&text.join(" "), now())?;
            if json {
                let fields = engine.task(&path)?;
                print_json(&AddOut {
                    path: path.to_string(),
                    fields,
                    warnings,
                })?;
            } else {
                println!("{path}");
                for w in &warnings {
                    eprintln!("warning: {w}");
                }
            }
        }
        TaskCommand::List { filter, paths } => {
            let items = engine.query_tasks(&list_filter(&filter), today)?;
            if json {
                print_json(&items)?;
            } else {
                for item in &items {
                    println!("{}", task_line(item, paths));
                }
            }
        }
        TaskCommand::Done { task } => {
            let path = resolve_task(engine, &task)?;
            let fields = engine.complete_task(&path, now())?;
            let next = if fields.done { None } else { fields.due };
            if json {
                print_json(&TaskItem { path, fields })?;
            } else if let Some(due) = next {
                println!("next: {}", fmt_due(&due));
            } else {
                println!("completed");
            }
        }
        TaskCommand::Reopen { task } => {
            let path = resolve_task(engine, &task)?;
            let fields = engine.reopen_task(&path)?;
            if json {
                print_json(&TaskItem { path, fields })?;
            } else {
                println!("reopened {path}");
            }
        }
        TaskCommand::Archive { task } => {
            let path = resolve_task(engine, &task)?;
            let trashed = engine.archive_task(&path)?;
            if json {
                print_json(&ArchiveOut {
                    path: path.to_string(),
                    trashed: trashed.to_string(),
                })?;
            } else {
                println!("archived to {trashed}");
            }
        }
        TaskCommand::Projects => {
            let projects = engine.projects()?;
            if json {
                print_json(&projects)?;
            } else {
                for p in &projects {
                    println!("{} ({})", p.project, p.open);
                }
            }
        }
        TaskCommand::Labels => {
            let labels = engine.labels()?;
            if json {
                print_json(&labels)?;
            } else {
                for label in &labels {
                    println!("{label}");
                }
            }
        }
        TaskCommand::Views => {
            let views = engine.saved_views()?;
            if json {
                print_json(&views)?;
            } else {
                for v in &views {
                    println!("{} ({}): {}", v.name, v.layout, v.filter);
                }
            }
        }
        TaskCommand::ViewSave { name, filter } => {
            let view = SavedView {
                name,
                filter: filter.join(" "),
                layout: "list".into(),
            };
            engine.save_view(view.clone(), today)?;
            if json {
                print_json(&view)?;
            } else {
                println!("saved view {}", view.name);
            }
        }
    }
    Ok(())
}

/// Map the named task views onto filter expressions; anything else is passed through.
fn list_filter(words: &[String]) -> String {
    let text = words.join(" ");
    let text = text.trim();
    match text.to_lowercase().as_str() {
        "" => "open".into(),
        "inbox" => "inbox & open".into(),
        "today" => "today & open".into(),
        "upcoming" => "next 7 days & open".into(),
        "completed" => "done".into(),
        _ => text.to_string(),
    }
}

/// Resolve a vault path or an exact (case-insensitive) title to one task's path.
fn resolve_task(engine: &Engine, task: &str) -> Result<VaultPath> {
    let needle = task.trim();
    let tasks = engine.all_tasks()?;
    if let Some(item) = tasks.iter().find(|t| t.path.to_string() == needle) {
        return Ok(item.path.clone());
    }
    let lower = needle.to_lowercase();
    let matches: Vec<&TaskItem> = tasks
        .iter()
        .filter(|t| t.fields.title.to_lowercase() == lower)
        .collect();
    match matches.as_slice() {
        [] => bail!("no task matches {needle:?} (use a vault path or an exact title)"),
        [one] => Ok(one.path.clone()),
        many => bail!(
            "{} tasks match {needle:?}: {}",
            many.len(),
            many.iter()
                .map(|t| t.path.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `2026-12-01` or `2026-12-01 09:00`.
fn fmt_due(due: &Due) -> String {
    match due.time {
        Some(t) => format!("{} {}", due.date.format("%Y-%m-%d"), t.format("%H:%M")),
        None => due.date.format("%Y-%m-%d").to_string(),
    }
}

/// One task as a text line: `[ ] p1 2026-12-01 09:00  pay rent  #home @label  ↻`.
/// The default priority (p4) and empty parts are omitted.
fn task_line(item: &TaskItem, show_path: bool) -> String {
    let f = &item.fields;
    let mut head = vec![if f.done { "[x]" } else { "[ ]" }.to_string()];
    if f.priority != Priority::P4 {
        head.push(f.priority.to_string());
    }
    if let Some(due) = &f.due {
        head.push(fmt_due(due));
    }
    let mut parts = vec![head.join(" "), f.title.clone()];
    let mut tags = Vec::new();
    if let Some(project) = &f.project {
        tags.push(format!("#{project}"));
    }
    tags.extend(f.labels.iter().map(|l| format!("@{l}")));
    if !tags.is_empty() {
        parts.push(tags.join(" "));
    }
    if f.recurrence.is_some() {
        parts.push("↻".into());
    }
    if show_path {
        parts.push(format!("({})", item.path));
    }
    parts.join("  ")
}

fn cmd_agenda(engine: &Engine, json: bool, days: u32) -> Result<()> {
    let today = chrono::Local::now().date_naive();
    let agenda = engine.agenda(today, days)?;
    if json {
        return print_json(&agenda);
    }
    if !agenda.overdue.is_empty() {
        println!("Overdue");
        for item in &agenda.overdue {
            println!("  {}", task_line(item, false));
        }
    }
    for day in agenda.days.iter().filter(|d| !d.tasks.is_empty()) {
        println!("{}", day.date.format("%a %d %b"));
        for item in &day.tasks {
            println!("  {}", task_line(item, false));
        }
    }
    Ok(())
}

/// Dry run of the reminder planner: what the desktop app would register for the window.
fn cmd_reminders_plan(engine: &Engine, json: bool) -> Result<()> {
    let tasks = engine.all_tasks()?;
    let keys: Vec<String> = tasks.iter().map(|t| t.path.to_string()).collect();
    let inputs = tasks.iter().zip(&keys).map(|(t, key)| PlannerInput {
        task_key: key.as_str(),
        fields: &t.fields,
    });
    let planned = plan(inputs, now(), Duration::days(i64::from(WINDOW_DAYS)));
    if json {
        return print_json(&planned);
    }
    for o in &planned {
        println!(
            "{}  {}  ({})",
            o.fire_at.format("%Y-%m-%d %H:%M"),
            o.title,
            o.task_key
        );
    }
    Ok(())
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
