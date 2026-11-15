//! barwatch: join a watch party of a Beyond All Reason replay.
//!
//! Started by a `barwatch://` link (or with one pasted in), it installs the engine,
//! game version and map the replay needs into BAR's own data directory and starts
//! the game as a spectator of the party's server.

// No console window behind the app on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod client;
mod config;
mod content;
mod datadir;
mod uri;

use anyhow::{bail, Context, Result};
use barwatch_core::api::Link;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// A barwatch:// link or an invite page address to open.
    link: Option<String>,
    /// BAR's data directory (default: detected; also $BAR_DATA_DIR).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make barwatch:// links open this program.
    Register,
    /// Undo `register`.
    Unregister,
    /// Join a watch party without the window, printing progress (for testing).
    Join {
        link: String,
        /// Name to show other viewers.
        #[arg(long)]
        name: Option<String>,
        /// Run the windowless engine build instead of the game.
        #[arg(long)]
        headless: bool,
    },
    /// Show the detected data directory and installed engines.
    Info,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load();
    let data = datadir::find(cli.data_dir.as_deref().or(cfg.data_dir.as_deref()));
    match cli.command {
        Some(Cmd::Register) => {
            uri::register()?;
            println!("barwatch:// links now open {}", std::env::current_exe()?.display());
            Ok(())
        }
        Some(Cmd::Unregister) => uri::unregister(),
        Some(Cmd::Info) => {
            println!("data directory: {}{}", data.root.display(), if data.own { " (no BAR installation found)" } else { "" });
            println!("viewer name:    {}", if cfg.name.is_empty() { datadir::default_name(&data) } else { cfg.name.clone() });
            println!("link handler:   {:?}", uri::status());
            for name in barwatch_core::engine::installed(&data.engines()) {
                println!("engine:         {name}");
            }
            Ok(())
        }
        Some(Cmd::Join { link, name, headless }) => {
            let name = name.unwrap_or_else(|| if cfg.name.is_empty() { datadir::default_name(&data) } else { cfg.name.clone() });
            join_in_terminal(&link, client::Settings { name, data, headless })
        }
        None => bail!("the window is not built yet; use `barwatch join <link>`"),
    }
}

/// `barwatch join`: the session client driven from a terminal.
fn join_in_terminal(link: &str, settings: client::Settings) -> Result<()> {
    let Link::Join { server, session } = Link::parse_any(link)? else { bail!("that is a pairing link, not an invite") };
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let last = std::sync::Mutex::new(String::new());
        let run = client::run(barwatch_core::http_client()?, server, session, settings, rx, move |update| {
            let phase = update.session.as_ref().map(|s| format!("{:?}", s.phase)).unwrap_or_default();
            let line = format!("[{}] {:?} (party: {phase})", update.name, update.stage);
            let mut last = last.lock().unwrap();
            if *last != line {
                eprintln!("{line}");
                *last = line;
            }
            if let Some(notice) = &update.notice {
                eprintln!("note: {notice}");
            }
        });
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => {}
            _ = tokio::signal::ctrl_c() => {
                // Let the client leave properly: it closes the engine and tells the server.
                let _ = tx.send(client::Command::Leave);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(8), run).await;
            }
        }
        Ok(())
    })
}
