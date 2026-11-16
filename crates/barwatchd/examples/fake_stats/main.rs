//! A fake stats service for developing and testing barwatch against: the calls of
//! bar-replays' HTTP API v1 that barwatch uses (its DESIGN.md §9.5) and the record
//! stream (§9.8), for one game whose raw output file it is given.
//!
//! ```text
//! cargo run -p barwatchd --example fake_stats -- --output <actions.jsonl[.zst]> \
//!     --replay <game.sdfz> [--listen 127.0.0.1:8792] [--token fake-token] [--rate 30]
//! ```
//!
//! Then start barwatchd with `BARWATCH_STATS_URL=http://127.0.0.1:8792/` and
//! `BARWATCH_STATS_TOKEN=fake-token`.
//!
//! Calls (all need `Authorization: Bearer <token>`, else 401; `priority`, where a
//! call takes one, must be `interactive`, `batch` or absent, else 400):
//! - `POST /api/v1/games {game_id, priority}`: registers the game (201 the first
//!   time, then 200). Another game id answers 404, as for a game the service cannot
//!   find; so does submitting by `file_name`.
//! - `PUT /api/v1/replays?priority=`: any replay is accepted (its header's game id
//!   is the game). Ours starts its simulation; any other stays `fetched` for ever,
//!   with an empty stream.
//! - `GET /api/v1/games/{id}[?wait=&rev=]`: status, long-polling for a change of
//!   `rev`. States: `fetched`, `provisioned` (queued), `simulating` (with
//!   `progress.frame`), `ingested`, or `failed` with `error`. 404 before the game is
//!   registered (unless `--known`).
//! - `GET /api/v1/games/{id}/replay`: the `--replay` file.
//! - `GET /api/v1/games/{id}/records?actions=a,b&from=<n>&follow=<0|1>`: the
//!   output's lines (unfiltered positions for `from` and `next`), each batch ending
//!   with `{"action":"_cursor","next":n,"frame":f,"complete":b}`; with `follow=1`
//!   new lines as the simulated clock releases them, a cursor at least every two
//!   seconds, closing after `complete:true`. A line is released once the clock
//!   passes the highest frame of any record up to it (so `team_stats` and
//!   `bs_team_stats` lines, which carry their history entry's frame, come when
//!   they were written).
//!
//! Flags:
//! - `--output <file>` raw output, `.jsonl` or `.jsonl.zst` (required).
//! - `--replay <file>` the game's `.sdfz`; its header gives the game id.
//! - `--game-id <hex>` the game id, if there is no `--replay`.
//! - `--listen <addr>` default `127.0.0.1:8792`. `--token <t>` default `fake-token`
//!   (or `FAKE_STATS_TOKEN`).
//! - `--rate <x>` game seconds simulated per second; `--instant` (default) makes
//!   the whole output available at once.
//! - `--queued <s>` seconds the game stays queued after registration (default 0).
//! - `--known` the game is registered from launch.
//! - `--no-resolve` `POST /api/v1/games` answers 404 for our game too (so a client
//!   must upload).
//! - `--fail-at <game s>` the simulation fails there (state `failed`; the stream
//!   stops growing and stays open).
//! - `--no-records` the record stream answers 404 "no such API call", as
//!   bar-replays before §9.8.
//! - `--drop-after-bytes <n>` every records response is cut after n body bytes,
//!   breaking the connection mid-transfer; with `--drop-clean` the response ends
//!   properly instead (no `complete`). `--drop-times <k>` cuts only the first k
//!   responses.
//! - `--stall-after-bytes <n>` every records response stops sending after n body
//!   bytes, for `--stall <s>` seconds (default: until the client gives up);
//!   `--stall-times <k>` only the first k responses.
//! - `--error-every <k>` every k-th API call answers 503.
//! - `--fail <calls>` these calls always answer 500, comma-separated: `submit`,
//!   `upload`, `status`, `replay`, `records`.
//! - `--garbage-every <m>` a garbage line (not JSON, wrong types, a cursor going
//!   back, a 1.5 MiB line, invalid UTF-8, …) after every m records sent.
//!
//! It does not synthesise companion records: use an output that has them (one
//! from a bar-replays build with `barstats.lua`), such as
//! `/data/projects/bar-replays-dev/outputs/duel-1v1.actions.jsonl` with
//! `demos/duel-1v1.sdfz` on the development machine.
//!
//! `service.rs` is also compiled into barwatchd's tests (`src/stats_tests.rs`),
//! which drive it through [`service::Options`] instead of these flags.

mod service;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    replay: Option<PathBuf>,
    #[arg(long)]
    game_id: Option<String>,
    #[arg(long, default_value = "127.0.0.1:8792")]
    listen: std::net::SocketAddr,
    #[arg(long, env = "FAKE_STATS_TOKEN", default_value = "fake-token", hide_env_values = true)]
    token: String,
    #[arg(long, conflicts_with = "instant")]
    rate: Option<f64>,
    #[arg(long)]
    instant: bool,
    #[arg(long, default_value_t = 0.0)]
    queued: f64,
    #[arg(long)]
    known: bool,
    #[arg(long)]
    no_resolve: bool,
    #[arg(long)]
    fail_at: Option<f64>,
    #[arg(long)]
    no_records: bool,
    #[arg(long)]
    drop_after_bytes: Option<usize>,
    #[arg(long)]
    drop_clean: bool,
    #[arg(long)]
    drop_times: Option<u32>,
    #[arg(long)]
    stall_after_bytes: Option<usize>,
    #[arg(long)]
    stall: Option<f64>,
    #[arg(long)]
    stall_times: Option<u32>,
    #[arg(long)]
    error_every: Option<u64>,
    #[arg(long, value_delimiter = ',')]
    fail: Vec<String>,
    #[arg(long)]
    garbage_every: Option<usize>,
}

fn read_output(path: &std::path::Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut raw = Vec::new();
    if path.extension().is_some_and(|e| e == "zst") {
        ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(file)).context("not zstd")?.read_to_end(&mut raw)?;
    } else {
        std::io::BufReader::new(file).read_to_end(&mut raw)?;
    }
    Ok(raw)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let lines = service::split_lines(&read_output(&args.output)?);
    let replay = args.replay.as_ref().map(std::fs::read).transpose().context("reading --replay")?;
    let game_id = match (&args.game_id, &replay) {
        (Some(id), _) => id.to_ascii_lowercase(),
        (None, Some(bytes)) => service::header_game_id(bytes).context("--replay has no game id in its header")?,
        (None, None) => bail!("give --replay or --game-id"),
    };
    let mut opts = service::Options::new(&args.token, lines, &game_id);
    if let (Some(path), Some(bytes)) = (&args.replay, replay) {
        opts.replay_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(opts.replay_name);
        opts.replay = Some(bytes);
    }
    opts.rate = args.rate.filter(|r| *r > 0.0);
    opts.queued_s = args.queued;
    opts.known_at_launch = args.known;
    opts.no_resolve = args.no_resolve;
    opts.fail_at_frame = args.fail_at.map(|s| (s * 30.0) as u32);
    opts.no_records = args.no_records;
    opts.drop_after_bytes = args.drop_after_bytes;
    opts.drop_clean = args.drop_clean;
    opts.drop_times = args.drop_times;
    opts.stall_times = args.stall_times;
    opts.stall_after_bytes = args.stall_after_bytes;
    opts.stall_s = args.stall;
    opts.error_every = args.error_every;
    opts.fail_calls = args.fail.into_iter().collect();
    opts.garbage_every = args.garbage_every;
    let n = opts.lines.len();
    let fake = service::Fake::new(opts);
    let listener = tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("listening on {}", args.listen))?;
    eprintln!("fake stats service on http://{} for game {game_id} ({n} lines)", args.listen);
    let server = fake.serve(listener);
    tokio::signal::ctrl_c().await?;
    server.stop().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}
