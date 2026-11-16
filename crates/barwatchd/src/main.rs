//! barwatchd: hosts Beyond All Reason replays for a group of viewers.
//!
//! It serves a small web page and an HTTP API (HTTP/1 and cleartext HTTP/2 on the
//! same port, for a reverse proxy to speak either), and runs one `spring-dedicated`
//! per running session on a UDP port from a configured range. It has no login of its
//! own: put the web page behind a reverse proxy that does, and let `/app/` through
//! for the desktop app (see the README).

mod autohost;
mod chat;
mod http;
mod runner;
mod source;
mod state;
mod stats_client;
mod timeline;

// The fake stats service of `examples/fake_stats`, for the tests that follow it.
#[cfg(test)]
#[path = "../examples/fake_stats/service.rs"]
mod fake_service;
#[cfg(test)]
mod stats_tests;

use anyhow::{Context, Result};
use barwatch_core::api::Phase;
use clap::Parser;
use state::{App, Config};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address the web page and API listen on (HTTP/1 and h2c).
    #[arg(long, env = "BARWATCH_LISTEN", default_value = "0.0.0.0:8780")]
    listen: SocketAddr,
    /// Directory for replays, engines and state.
    #[arg(long, env = "BARWATCH_DATA", default_value = "data")]
    data: PathBuf,
    /// UDP ports for hosting, one per running session, as FIRST-LAST. Viewers'
    /// engines connect to these directly, so they must be reachable from outside.
    #[arg(long, env = "BARWATCH_UDP_PORTS", default_value = "8480-8489", value_parser = parse_ports)]
    udp_ports: std::ops::RangeInclusive<u16>,
    /// Host name or address viewers' engines connect to. Default: the host of the
    /// URL each viewer reached this server by.
    #[arg(long, env = "BARWATCH_GAME_HOST")]
    game_host: Option<String>,
    /// Largest replay accepted, in MiB.
    #[arg(long, env = "BARWATCH_MAX_UPLOAD_MB", default_value_t = 256)]
    max_upload_mb: usize,
    /// Sessions older than this many days are deleted.
    #[arg(long, env = "BARWATCH_SESSION_DAYS", default_value_t = 14)]
    session_days: u64,
    /// Where the pages send people for the desktop app.
    #[arg(long, env = "BARWATCH_DOWNLOAD_URL", default_value = "https://github.com/u6bkep/barwatch/releases/latest")]
    download_url: String,
    /// Base address of the stats service (bar-replays), e.g. http://bar-replays:8770/.
    /// Unset: no statistics timeline.
    #[arg(long, env = "BARWATCH_STATS_URL")]
    stats_url: Option<String>,
    /// A `client` token of the stats service. Prefer the environment variable: a
    /// command-line flag is visible to every user of the machine.
    #[arg(long, env = "BARWATCH_STATS_TOKEN", hide_env_values = true)]
    stats_token: Option<String>,
    /// A file holding the `client` token (e.g. a Docker secret), instead of
    /// BARWATCH_STATS_TOKEN.
    #[arg(long, env = "BARWATCH_STATS_TOKEN_FILE", conflicts_with = "stats_token")]
    stats_token_file: Option<PathBuf>,
}

fn parse_ports(s: &str) -> Result<std::ops::RangeInclusive<u16>, String> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let first: u16 = a.trim().parse().map_err(|_| format!("bad port {a:?}"))?;
    let last: u16 = b.trim().parse().map_err(|_| format!("bad port {b:?}"))?;
    if first == 0 || last < first {
        return Err(format!("bad port range {s:?}"));
    }
    Ok(first..=last)
}

pub fn log(message: &str) {
    eprintln!("[{}] {message}", state::unix_now());
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.data).with_context(|| format!("creating {}", args.data.display()))?;
    let data = args.data.canonicalize().with_context(|| format!("resolving {}", args.data.display()))?;
    if let Some(host) = &args.game_host {
        anyhow::ensure!(barwatch_core::names::valid_host(host), "--game-host {host:?} is not a host name or address");
    }
    let cfg = Config {
        data,
        udp_ports: args.udp_ports,
        game_host: args.game_host,
        max_upload: args.max_upload_mb << 20,
        session_days: args.session_days,
        download_url: args.download_url,
        archive: source::Archive::default(),
    };
    let stats_token = match (&args.stats_token, &args.stats_token_file) {
        (Some(t), _) => Some(t.clone()),
        (None, Some(f)) => Some(
            std::fs::read_to_string(f)
                .with_context(|| format!("reading BARWATCH_STATS_TOKEN_FILE {}", f.display()))?
                .trim()
                .to_owned(),
        ),
        (None, None) => None,
    };
    let stats = match (args.stats_url.as_deref().filter(|u| !u.trim().is_empty()), stats_token.as_deref()) {
        (None, _) => None,
        (Some(_), None) => {
            anyhow::bail!("BARWATCH_STATS_URL is set but neither BARWATCH_STATS_TOKEN nor BARWATCH_STATS_TOKEN_FILE is")
        }
        (Some(url), Some(token)) => {
            Some(stats_client::StatsClient::new(url, token).context("BARWATCH_STATS_URL / BARWATCH_STATS_TOKEN")?)
        }
    };
    match &stats {
        Some(c) => log(&format!("stats service at {}", c.base())),
        None => log("no stats service configured; parties have no statistics timeline"),
    }
    let stats = timeline::Service::new(stats, timeline::Tuning::default());
    let app = Arc::new(App::new(cfg, barwatch_core::http_client()?, stats)?);

    tokio::spawn(load_maps(app.clone()));
    tokio::spawn(housekeeping(app.clone()));

    let listener = tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("listening on {}", args.listen))?;
    log(&format!(
        "barwatchd {} listening on http://{} (game ports udp {}-{})",
        env!("CARGO_PKG_VERSION"),
        args.listen,
        app.cfg.udp_ports.start(),
        app.cfg.udp_ports.end()
    ));
    axum::serve(listener, http::router(app)).with_graceful_shutdown(shutdown()).await?;
    // Dropping the runtime kills any hosting server still running.
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    log("shutting down");
}

/// Fetches the map list (for pictures) and fills in sessions created before it came.
async fn load_maps(app: Arc<App>) {
    let index = barwatch_core::maps::MapIndex::load(&app.http, &app.cfg.data.join("cache/maps.json")).await;
    if index.is_empty() {
        log("map list unavailable; map pictures use guessed names");
    }
    *app.maps.write().unwrap() = index;
    for session in app.sessions() {
        let file = app.maps.read().unwrap().file_name(&session.read(|st| st.replay.map.clone()));
        session.mutate(|st| st.replay.map_file = Some(file));
    }
}

async fn housekeeping(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let limit = state::unix_now().saturating_sub(app.cfg.session_days * 24 * 3600);
        for session in app.sessions() {
            session.expire_viewers();
            // Viewers count as demand for the statistics (DESIGN §4.1).
            if session.read(|st| !st.viewers.is_empty() || !st.in_game.is_empty()) {
                timeline::ensure(&app, &session, false, false);
            }
            let (created, phase) = session.read(|st| (st.created_unix, st.phase));
            if created < limit && matches!(phase, Phase::Idle | Phase::Failed) {
                log(&format!("session {} expired", session.id));
                app.remove_session(&session.id);
            }
        }
        timeline::evict(&app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges() {
        assert_eq!(parse_ports("8480-8489"), Ok(8480..=8489));
        assert_eq!(parse_ports("9000"), Ok(9000..=9000));
        assert!(parse_ports("9-1").is_err());
        assert!(parse_ports("x").is_err());
    }
}
