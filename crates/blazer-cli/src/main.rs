use std::{
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use blazer_core::{
    Config, DownloadId, DownloadRecord, Engine, Event, MediaFormat, NewDownload, Progress, Status,
};
use clap::{Parser, Subcommand};
use tokio::sync::broadcast::error::RecvError;

#[derive(Parser)]
#[command(name = "blazer", version, about = "InternetBlazer command-line downloader")]
struct Cli {
    /// Path of the state database.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL.
    Get {
        url: String,
        /// Directory to save into. Defaults to your Downloads folder.
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
        /// File name to save as, instead of the one the server suggests.
        #[arg(short = 'o', long)]
        output: Option<String>,
        /// Parallel connections.
        #[arg(short = 'c', long)]
        connections: Option<usize>,
        /// Speed limit for this download, such as 500K or 2M (bytes per second).
        #[arg(short = 'l', long, value_parser = parse_rate)]
        limit: Option<u64>,
        /// Download the video on the page with yt-dlp: `best`, a height such as `720`, or
        /// `audio` for the sound alone.
        #[arg(short = 'm', long, value_parser = parse_media)]
        media: Option<MediaFormat>,
    },
    /// Resume a paused or failed download.
    Resume { id: u64 },
    /// Download a file again from the start.
    Restart { id: u64 },
    /// Give a paused or failed download a new address for the same file, such as a fresh link
    /// after the old one expired. The progress is kept.
    SetUrl { id: u64, url: String },
    /// List all downloads.
    List,
    /// Show or set the speed limit for all downloads, shared with the desktop app.
    Limit {
        /// Bytes per second, such as 500K or 2M, or `off`.
        rate: Option<String>,
    },
    /// Show the programs video downloads use: yt-dlp and ffmpeg.
    Tools {
        /// Install the latest yt-dlp, or update the copy InternetBlazer installed.
        #[arg(long)]
        update: bool,
    },
    /// Remove a download from the list.
    Remove {
        id: u64,
        /// Also delete the downloaded file or partial data.
        #[arg(long)]
        delete_files: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let db = match cli.db {
        Some(db) => db,
        None => dirs::data_local_dir()
            .context("no local data directory; pass --db")?
            .join("InternetBlazer")
            .join("state.redb"),
    };
    let tools_dir = db.parent().map(|dir| dir.join("tools"));
    let engine = Engine::open(&db, Config { tools_dir, ..Config::default() })
        .with_context(|| format!("opening {}", db.display()))?;

    let code = match cli.command {
        Command::Get { url, dir, output, connections, limit, media } => {
            if media.is_some() && engine.media_tools().await.ytdlp.is_none() {
                eprintln!("Installing yt-dlp, which video downloads need...");
                let tool = engine.install_ytdlp().await?;
                eprintln!("Installed yt-dlp {}", tool.version);
            }
            let dir = match dir {
                Some(dir) => dir,
                None => dirs::download_dir().map_or_else(std::env::current_dir, Ok)?,
            };
            let mut new = NewDownload::new(url, dir);
            new.file_name = output;
            new.connections = connections;
            new.speed_limit = limit;
            new.media = media;
            let id = engine.add(new).await?;
            follow(&engine, id).await?
        }
        Command::Resume { id } => {
            engine.start(DownloadId(id))?;
            follow(&engine, DownloadId(id)).await?
        }
        Command::Restart { id } => {
            engine.restart(DownloadId(id)).await?;
            follow(&engine, DownloadId(id)).await?
        }
        Command::SetUrl { id, url } => {
            engine.set_url(DownloadId(id), &url).await?;
            println!("Updated the address. Resume with: blazer resume {id}");
            ExitCode::SUCCESS
        }
        Command::List => {
            print_list(&engine.list()?);
            ExitCode::SUCCESS
        }
        Command::Limit { rate } => {
            if let Some(rate) = rate {
                let limit = match rate.as_str() {
                    "off" | "none" => None,
                    rate => Some(parse_rate(rate).map_err(anyhow::Error::msg)?),
                };
                engine.set_speed_limit(limit)?;
            }
            match engine.settings()?.speed_limit {
                Some(limit) => println!("Speed limit: {}/s", bytes(limit)),
                None => println!("No speed limit"),
            }
            ExitCode::SUCCESS
        }
        Command::Tools { update } => {
            if update {
                let tool = engine.install_ytdlp().await?;
                println!("Installed yt-dlp {}", tool.version);
            }
            let tools = engine.media_tools().await;
            match tools.ytdlp {
                Some(t) => println!("yt-dlp {} at {}", t.version, t.path.display()),
                None => println!("yt-dlp: not installed; install it with: blazer tools --update"),
            }
            match tools.ffmpeg {
                Some(path) => println!("ffmpeg at {}", path.display()),
                None => println!(
                    "ffmpeg: not found; without it, many sites only offer low qualities or none"
                ),
            }
            ExitCode::SUCCESS
        }
        Command::Remove { id, delete_files } => {
            engine.remove(DownloadId(id), delete_files).await?;
            println!("Removed download {id}");
            ExitCode::SUCCESS
        }
    };
    engine.shutdown().await;
    Ok(code)
}

/// Shows progress until the download stops. Ctrl+C pauses it.
async fn follow(engine: &Engine, id: DownloadId) -> anyhow::Result<ExitCode> {
    let mut events = engine.subscribe();
    let record = engine.get(id)?;
    println!("[{id}] {} -> {}", record.url, record.path().display());

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut pausing = false;
    // Waiting to retry, or for its turn in the queue, still counts as going.
    let going = || -> anyhow::Result<bool> {
        let status = engine.get(id)?.status;
        Ok(engine.is_active(id) || matches!(status, Status::Queued | Status::Retrying))
    };
    while going()? {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Progress(p)) if p.id == id => print_progress(&p),
                Ok(Event::StatusChanged { id: changed, status: Status::Retrying, error })
                    if changed == id =>
                {
                    let wait = engine.get(id)?.retry.map(|r| r.at.saturating_sub(now()));
                    eprint!(
                        "\r{}; retrying in {}\x1b[K",
                        error.as_deref().unwrap_or("failed"),
                        duration(wait.unwrap_or(0))
                    );
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            result = &mut ctrl_c, if !pausing => {
                result?;
                pausing = true;
                eprint!("\nPausing...");
                engine.pause(id);
            }
        }
    }
    eprintln!();

    let record = engine.wait(id).await?;
    Ok(match record.status {
        Status::Completed => {
            println!("Saved {} ({})", record.path().display(), bytes(record.downloaded()));
            ExitCode::SUCCESS
        }
        Status::Failed => {
            eprintln!("Failed: {}", record.error.as_deref().unwrap_or("unknown error"));
            eprintln!("Retry with: blazer resume {id}");
            ExitCode::FAILURE
        }
        _ => {
            println!("Paused at {}. Resume with: blazer resume {id}", bytes(record.downloaded()));
            ExitCode::SUCCESS
        }
    })
}

fn print_progress(p: &Progress) {
    if let Some(problem) = &p.problem {
        eprint!("\r{}  {problem}; reconnecting...\x1b[K", bytes(p.downloaded));
        return;
    }
    let line = match p.total {
        Some(total) if total > 0 => {
            let percent = p.downloaded as f64 * 100.0 / total as f64;
            let eta = match p.speed {
                0 => "--".to_string(),
                speed => duration(total.saturating_sub(p.downloaded) / speed),
            };
            format!(
                "{percent:5.1}%  {} / {}  {}/s  {} conn  ETA {eta}",
                bytes(p.downloaded),
                bytes(total),
                bytes(p.speed),
                p.connections
            )
        }
        _ => format!("{}  {}/s", bytes(p.downloaded), bytes(p.speed)),
    };
    eprint!("\r{line}\x1b[K");
}

fn print_list(records: &[DownloadRecord]) {
    if records.is_empty() {
        println!("No downloads.");
        return;
    }
    println!("{:>4}  {:<11}  {:>7}  {:>10}  NAME", "ID", "STATUS", "DONE", "SIZE");
    for r in records {
        let done = match r.total {
            Some(0) => "100%".to_string(),
            Some(total) if r.status == Status::Completed || total > 0 => {
                let downloaded = if r.status == Status::Completed { total } else { r.downloaded() };
                format!("{:.1}%", downloaded as f64 * 100.0 / total as f64)
            }
            _ => "-".to_string(),
        };
        let size = r.total.map_or_else(|| "?".to_string(), bytes);
        println!(
            "{:>4}  {:<11}  {done:>7}  {size:>10}  {}",
            r.id,
            format!("{:?}", r.status),
            r.file_name
        );
    }
}

/// Parses a rate like `800`, `500K` or `2.5M`, in bytes per second with binary units.
fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (number, unit) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => s.split_at(i),
        None => (s, ""),
    };
    let multiplier = match unit.to_ascii_lowercase().trim_end_matches("/s").trim_end_matches('b') {
        "" => 1.0,
        "k" | "ki" => 1024.0,
        "m" | "mi" => 1024.0 * 1024.0,
        "g" | "gi" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("unknown unit `{unit}`; use K, M or G")),
    };
    let number: f64 = number.trim().parse().map_err(|_| format!("`{s}` is not a rate"))?;
    if !number.is_finite() || number <= 0.0 {
        return Err("the rate must be above zero".into());
    }
    Ok((number * multiplier) as u64)
}

fn parse_media(s: &str) -> Result<MediaFormat, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "best" => Ok(MediaFormat::Video { max_height: None }),
        "audio" => Ok(MediaFormat::Audio),
        height => {
            let height = height.trim_end_matches('p').parse().map_err(|_| {
                format!("`{s}` is not a quality; use best, audio or a height such as 720")
            })?;
            Ok(MediaFormat::Video { max_height: Some(height) })
        }
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

fn duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
