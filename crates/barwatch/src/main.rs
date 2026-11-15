//! barwatch: join a watch party of a Beyond All Reason replay.
//!
//! Started by a `barwatch://` link (or with one pasted in), it installs the engine,
//! game version and map the replay needs into BAR's own data directory and starts
//! the game as a spectator of the party's server.

// No console window behind the app on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod client;
mod config;
mod content;
mod datadir;
mod fixtures;
mod instance;
mod mapimg;
mod position;
mod replays;
mod timeline;
mod ui;
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
    /// PROTOTYPE: open the timeline on a replay saved by a game client.
    #[command(hide = true)]
    Timeline {
        replay: PathBuf,
        /// Game time to start at, in seconds.
        #[arg(long, default_value_t = 0.0)]
        at: f32,
        /// Start with the stand-in game clock paused.
        #[arg(long)]
        paused: bool,
        /// Add made-up milestones, flagged as samples, to try the event lane.
        #[arg(long)]
        sample_markers: bool,
        /// DEVELOPMENT: a Beyond-All-Reason checkout to read marker pictures from.
        #[arg(long)]
        game_art: Option<PathBuf>,
        /// Pretend statistics are worked out only up to this game time (seconds).
        #[arg(long)]
        computed_to: Option<f32>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load();
    let data = datadir::find(cli.data_dir.as_deref().or(cfg.data_dir.as_deref()));
    match cli.command {
        Some(Cmd::Register) => {
            uri::register()?;
            println!("barwatch:// links now open barwatch");
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
        Some(Cmd::Timeline { replay, at, paused, sample_markers, game_art, computed_to }) => {
            use damascene_core::prelude::Rect;
            let opts = timeline::ProtoOptions { start_s: at, sample_markers, game_art, computed_to, paused };
            let proto = timeline::Proto::open(&replay, opts)?;
            damascene_winit_wgpu::run("barwatch timeline (prototype)", Rect::new(0.0, 0.0, 1180.0, 900.0), proto)
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        None => run_window(cfg, data, cli.link),
    }
}

/// Opens the window, on `link` if one was given (that is how a browser starts us).
fn run_window(cfg: config::Config, data: datadir::DataDir, link: Option<String>) -> Result<()> {
    use damascene_core::prelude::Rect;
    let viewport = Rect::new(0.0, 0.0, 980.0, 700.0);
    // A canned scene, for screenshots and layout review.
    if let Ok(scene) = std::env::var("BARWATCH_FIXTURE") {
        let ui = fixtures::scene(&scene)
            .with_context(|| format!("unknown fixture {scene:?}; known: {}", fixtures::SCENES.join(", ")))?;
        return damascene_winit_wgpu::run("barwatch", viewport, ui).map_err(|e| anyhow::anyhow!("{e}"));
    }

    // One window per user: a link opened while it is up goes to that window.
    if instance::hand_over(link.as_deref()) {
        return Ok(());
    }

    let opened = link.and_then(|text| match Link::parse_any(&text) {
        Ok(link) => Some(link),
        Err(e) => {
            eprintln!("ignoring {text:?}: {e:#}");
            None
        }
    });
    // First run of a bare binary: make links open it. A packaged install already is.
    if uri::status() == uri::Status::Missing {
        if let Err(e) = uri::register() {
            eprintln!("could not register barwatch:// links: {e:#}");
        }
    }
    let wake = backend::WakeSlot::default();
    let (backend, to_window) = backend::Backend::spawn(wake.clone());
    let _listening = instance::listen(move |link| to_window.post(backend::Msg::Opened(link)));
    let ui = ui::Ui::new(backend, cfg, data, opened);
    let host = damascene_winit_wgpu::HostConfig::default()
        .with_app_id("barwatch")
        .with_external_wakeup(move |wakeup| *wake.lock().unwrap() = Some(wakeup));
    damascene_winit_wgpu::run_with_config("barwatch", viewport, ui, host).map_err(|e| anyhow::anyhow!("{e}"))
}

/// `barwatch join`: the session client driven from a terminal.
fn join_in_terminal(link: &str, settings: client::Settings) -> Result<()> {
    let Link::Join { server, session } = Link::parse_any(link)? else { bail!("that is a pairing link, not an invite") };
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let last = std::sync::Mutex::new(String::new());
        // The game position, printed once a second while the engine runs.
        let shown = std::sync::Arc::new(std::sync::Mutex::new((false, None::<position::Position>)));
        let latest = shown.clone();
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
            let mut latest = latest.lock().unwrap();
            if latest.1.is_some() != update.position.is_some() {
                eprintln!("{}", position_line(update.position.as_ref()));
            }
            *latest = (update.stage == client::Stage::Playing, update.position);
        });
        let ticker = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tick.tick().await;
                let (playing, position) = *shown.lock().unwrap();
                if playing || position.is_some() {
                    eprintln!("{}", position_line(position.as_ref()));
                }
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
        ticker.abort();
        Ok(())
    })
}

/// `16:47:01.234 game 5:00.4 (f=9012 1.00x running, report 0.3 s old)`.
fn position_line(position: Option<&position::Position>) -> String {
    let clock = chrono::Local::now().format("%H:%M:%S%.3f");
    let now = std::time::Instant::now();
    match position.and_then(|p| Some((p, p.seconds_at(now)?))) {
        Some((p, seconds)) => {
            let r = p.report;
            format!(
                "{clock} game {}:{:04.1} (f={} {:.2}x {}, report {:.1} s old)",
                (seconds / 60.0) as u32,
                seconds % 60.0,
                r.frame,
                r.speed,
                if r.paused { "paused" } else { "running" },
                now.saturating_duration_since(p.seen).as_secs_f32()
            )
        }
        None => format!("{clock} game position unknown"),
    }
}
