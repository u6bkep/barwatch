//! One run of a session: launches `spring-dedicated` on the replay, follows it over
//! the autohost interface, relays playback commands and tidies up when it ends.
//!
//! The dedicated server only streams the recording: it simulates nothing and needs
//! neither the game nor the map, just the engine build the replay was made with.

use crate::autohost::{Event, Notice};
use crate::chat::{self, Command};
use crate::state::{App, Session};
use anyhow::{bail, Context, Result};
use barwatch_core::api::{Control, JumpView, Phase};
use barwatch_core::engine::{self, Binary, Os};
use barwatch_core::script;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub enum RunnerCmd {
    Control(Control),
    Stop,
}

/// The server must report that it listens within this long of being launched.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// A run with nobody registered or connected for this long is stopped.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(180);
/// How long the server gets to exit after `/kill` before it is killed.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Unpaused playback time that must pass before any skip is sent. The engine's skip
/// never returns if it runs before the server has consumed the first chunk of the
/// recording, which it does 0.09 s of game time into playback (`CGameServer::SkipTo`
/// advances to the next chunk's own time, while `CDemoReader` holds the first chunk
/// back until then). 1.5 s is well past that even at the slowest speed.
const SKIP_SAFE_AFTER: Duration = Duration::from_millis(1500);
/// How long the engine itself waits for a first viewer. Longer than
/// `EMPTY_TIMEOUT`, so it is barwatch that decides. The engine's default of 30 s is
/// shorter than a viewer's first launch can take.
const ENGINE_FIRST_VIEWER_WAIT_S: u32 = 600;

/// Makes sure the engine for `session` is installed. Installs are serialized; a
/// party whose engine is already there never waits for another party's download.
pub async fn ensure_engine(app: &App, session: &Session) -> Result<std::path::PathBuf> {
    let version = &session.demo.engine_version;
    if let Some(dir) = engine::find(&app.engines_dir(), version, Binary::Dedicated) {
        return Ok(dir);
    }
    let _guard = app.engine_lock.lock().await;
    engine::ensure(&app.http, &app.engines_dir(), version, Os::Linux, Binary::Dedicated, &|_, _| {}).await
}

/// Resolves when the run is told to stop (or its session is gone).
async fn stop_requested(cmds: &mut mpsc::UnboundedReceiver<RunnerCmd>) {
    loop {
        match cmds.recv().await {
            Some(RunnerCmd::Stop) | None => return,
            // Nothing to control yet.
            Some(RunnerCmd::Control(_)) => {}
        }
    }
}

/// Owns one run from launch to exit and leaves the session `Idle` (or `Failed`).
pub async fn run(app: Arc<App>, session: Arc<Session>, mut cmds: mpsc::UnboundedReceiver<RunnerCmd>) {
    let outcome = serve(&app, &session, &mut cmds).await;
    session.mutate(|st| {
        st.runner = None;
        st.game_port = None;
        st.in_game.clear();
        st.paused = false;
        match outcome {
            Ok(reason) => {
                crate::log(&format!("session {}: run {} ended: {reason}", session.id, st.run));
                st.phase = Phase::Idle;
                st.note = Some(reason);
            }
            Err(e) => {
                crate::log(&format!("session {}: run {} failed: {e:#}", session.id, st.run));
                st.phase = Phase::Failed;
                st.note = Some(format!("{e:#}"));
            }
        }
    });
}

async fn serve(app: &Arc<App>, session: &Arc<Session>, cmds: &mut mpsc::UnboundedReceiver<RunnerCmd>) -> Result<String> {
    // This can be a download; Stop must still work while it runs.
    let engine_dir = tokio::select! {
        dir = ensure_engine(app, session) => dir?,
        _ = stop_requested(cmds) => return Ok("stopped".into()),
    };
    let binary = Binary::Dedicated.find(&engine_dir).context("engine has no dedicated server")?;
    let lease = app.take_port().context("every game port is in use; stop another session first")?;
    let autohost = UdpSocket::bind(("127.0.0.1", 0)).await.context("opening the control socket")?;
    let autohost_port = autohost.local_addr()?.port();

    let run_dir = app.run_dir(&session.id);
    let _ = tokio::fs::remove_dir_all(&run_dir).await;
    tokio::fs::create_dir_all(&run_dir).await.with_context(|| format!("creating {}", run_dir.display()))?;
    let script_path = run_dir.join("script.txt");
    let config_path = run_dir.join("springsettings.cfg");
    let script = script::dedicated_script(
        &session.demo.script,
        &session.demo.game_data,
        &session.replay_path(),
        lease.port,
        autohost_port,
    )?;
    tokio::fs::write(&script_path, script).await?;
    tokio::fs::write(&config_path, format!("InitialNetworkTimeout = {ENGINE_FIRST_VIEWER_WAIT_S}\n")).await?;
    let log = std::fs::File::create(run_dir.join("server.log"))?;

    // `-isolation` makes the engine directory the only data directory, which is where
    // the engine's own base content lives; everything it writes goes to `run_dir`.
    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&engine_dir)
        .env("SPRING_WRITEDIR", &run_dir)
        .arg("-nocolor")
        .arg("-isolation")
        .arg("-config")
        .arg(&config_path)
        .arg(&script_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    crate::log(&format!("session {}: server launched on udp {}", session.id, lease.port));

    let launched = Instant::now();
    let mut link = Link { socket: autohost, peer: None };
    let mut listening = false;
    let mut last_presence = Instant::now();
    let mut stopping: Option<(Instant, String)> = None;
    // Unpaused time since the replay stream began; `None` before it has.
    let mut played: Option<Duration> = None;
    let mut pregame_skipped = false;
    let mut clock = GameClock::default();
    let tick_every = Duration::from_millis(250);
    let mut tick = tokio::time::interval(tick_every);
    let mut buf = vec![0u8; 65536];

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                if !listening {
                    bail!("the server exited before it was ready ({status}){}", log_tail(&run_dir));
                }
                return Ok(match stopping {
                    Some((_, reason)) => reason,
                    None if session.read(|st| st.ended) => "the replay ended".into(),
                    None => "the server closed".into(),
                });
            }
            received = link.socket.recv_from(&mut buf) => {
                let (len, from) = received?;
                // The server we launched is the first to speak; nobody else on this
                // machine gets to feed events in or have commands sent to them.
                if *link.peer.get_or_insert(from) != from {
                    continue;
                }
                match Event::parse(&buf[..len]) {
                    Some(Event::Started) => {
                        listening = true;
                        session.mutate(|st| {
                            st.phase = Phase::Running;
                            st.game_port = Some(lease.port);
                        });
                    }
                    Some(Event::Message(text)) => match Notice::parse(&text) {
                        Some(Notice::PlaybackBegan) => played = Some(Duration::ZERO),
                        Some(Notice::Paused(p)) => session.mutate(|st| st.paused = p),
                        Some(Notice::ReplayEnded) => session.mutate(|st| st.ended = true),
                        None => {}
                    },
                    Some(Event::PlayerJoined { number, name }) => session.mutate(|st| {
                        st.in_game.insert(number, name);
                    }),
                    Some(Event::PlayerLeft { number }) => session.mutate(|st| {
                        st.in_game.remove(&number);
                    }),
                    Some(Event::Chat { text, .. }) => match chat::parse(&text) {
                        Some(Ok(Command::Control(control))) => {
                            let reply = apply(&link, session, control, pregame_skipped, &mut clock).await;
                            link.send(&reply).await;
                        }
                        Some(Ok(Command::Help)) => link.send(chat::HELP).await,
                        Some(Err(usage)) => link.send(&usage).await,
                        None => {}
                    },
                    Some(Event::Quit | Event::GameOver | Event::Other(_)) | None => {}
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(RunnerCmd::Control(control)) => {
                    let reply = apply(&link, session, control, pregame_skipped, &mut clock).await;
                    link.send(&reply).await;
                }
                Some(RunnerCmd::Stop) | None => {
                    begin_stop(&link, &mut child, &mut stopping, "stopped").await;
                }
            },
            _ = tick.tick() => {
                if let Some(time) = &mut played {
                    let (paused, speed) = session.read(|st| (st.paused, st.speed));
                    if !paused {
                        *time += tick_every;
                        if pregame_skipped {
                            clock.advance(tick_every, speed);
                        }
                    }
                    // The recording opens with the original lobby wait; nobody wants
                    // to sit through it again.
                    if !pregame_skipped && *time >= SKIP_SAFE_AFTER {
                        pregame_skipped = true;
                        link.send("/skip 1").await;
                        clock.skipped_to(1);
                    }
                }
                if !listening && launched.elapsed() > START_TIMEOUT {
                    let _ = child.start_kill();
                    bail!("the server did not come up within {} s{}", START_TIMEOUT.as_secs(), log_tail(&run_dir));
                }
                if session.read(|st| !st.in_game.is_empty() || !st.viewers.is_empty()) {
                    last_presence = Instant::now();
                } else if last_presence.elapsed() > EMPTY_TIMEOUT {
                    begin_stop(&link, &mut child, &mut stopping, "everyone left").await;
                }
                if stopping.as_ref().is_some_and(|(since, _)| since.elapsed() > KILL_GRACE) {
                    let _ = child.start_kill();
                }
            }
        }
    }
}

/// The control socket and the address the server talks to us from.
pub(crate) struct Link {
    socket: UdpSocket,
    peer: Option<SocketAddr>,
}

impl Link {
    /// Sends a command (`/skip 60`) or, without a leading slash, a chat line from the
    /// server. Dropped silently while the server has not spoken yet.
    async fn send(&self, text: &str) {
        if let Some(peer) = self.peer {
            let _ = self.socket.send_to(text.as_bytes(), peer).await;
        }
    }
}

async fn begin_stop(link: &Link, child: &mut tokio::process::Child, stopping: &mut Option<(Instant, String)>, reason: &str) {
    if stopping.is_some() {
        return;
    }
    if link.peer.is_some() {
        link.send("/kill").await;
    } else {
        let _ = child.start_kill();
    }
    *stopping = Some((Instant::now(), reason.to_string()));
}

/// Where the hosting server is in the game, in game seconds, as far as the daemon
/// can tell. The engine reports no position over the autohost interface, so this
/// counts unpaused time at the speed the daemon last set, from the pre-game skip
/// (game second 1) on, and never stays behind a skip target. It runs off when
/// viewers change the speed with their own keys (the daemon is not told), and it
/// is only used to turn a relative skip into the absolute target `jump` reports.
#[derive(Default)]
pub(crate) struct GameClock {
    pos_s: f64,
    /// The furthest target skipped to in this run: the server stands at or past it,
    /// so any skip to an earlier time is ignored by the engine.
    furthest: Option<u32>,
}

impl GameClock {
    pub(crate) fn advance(&mut self, dt: Duration, speed: f32) {
        self.pos_s += dt.as_secs_f64() * f64::from(speed.max(0.0));
    }

    pub(crate) fn skipped_to(&mut self, target_s: u32) {
        self.pos_s = self.pos_s.max(f64::from(target_s));
    }

    /// The absolute game second a skip lands on; for a relative one, from the
    /// estimate (the engine itself adds to its exact frame).
    fn target(&self, seconds: u32, relative: bool) -> u32 {
        if relative {
            (self.pos_s.max(0.0).min(f64::from(u32::MAX)) as u32).saturating_add(seconds)
        } else {
            seconds
        }
    }

    /// Records a skip; `true` if it goes further than any before in this run (an
    /// earlier target is ignored by the engine, and must not replace a jump that
    /// viewers may still be catching up to).
    fn skip(&mut self, target_s: u32) -> bool {
        let further = self.furthest.is_none_or(|f| target_s > f);
        if further {
            self.furthest = Some(target_s);
        }
        self.skipped_to(target_s);
        further
    }
}

/// Carries out a playback command; returns the line to announce in game chat.
pub(crate) async fn apply(link: &Link, session: &Session, control: Control, skips_safe: bool, game: &mut GameClock) -> String {
    match control {
        Control::Skip { .. } if !skips_safe => "barwatch: the replay is only just starting; try again in a moment".into(),
        Control::Skip { seconds, relative } => {
            // Exact on the server; each viewer's engine then replays the gap as fast
            // as it can. A target already passed is ignored by the engine.
            link.send(&format!("/skip {}{seconds}", if relative { "+" } else { "" })).await;
            // The engine stops at the end of the recording.
            let length = session.read(|st| st.replay.duration_s);
            let mut target_s = game.target(seconds, relative);
            if length > 0 {
                target_s = target_s.min(length);
            }
            if game.skip(target_s) {
                session.mutate(|st| st.jump = Some(JumpView { target_s, at_unix: crate::state::unix_now() }));
            }
            if relative {
                format!("barwatch: jumping ahead {}", clock(seconds))
            } else {
                format!("barwatch: jumping to {}", clock(seconds))
            }
        }
        Control::Pause { paused } => {
            link.send(if paused { "/pause 1" } else { "/pause 0" }).await;
            session.mutate(|st| st.paused = paused);
            format!("barwatch: {}", if paused { "paused" } else { "resumed" })
        }
        Control::Speed { speed } => {
            let speed = speed.clamp(chat::MIN_SPEED, chat::MAX_SPEED);
            // The server only changes speed by clamping it into [min, max]: narrow the
            // range onto the wanted speed, then open it again so viewers' own speed
            // keys keep working.
            for cmd in [
                "/setminspeed 0.1".to_string(),
                format!("/setmaxspeed {speed}"),
                format!("/setminspeed {speed}"),
                "/setminspeed 0.1".to_string(),
                format!("/setmaxspeed {}", chat::MAX_SPEED),
            ] {
                link.send(&cmd).await;
            }
            session.mutate(|st| st.speed = speed);
            format!("barwatch: speed {speed}x")
        }
    }
}

fn clock(seconds: u32) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// The last error lines of the server's own log, for a failure message.
fn log_tail(run_dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(run_dir.join("infolog.txt")) else { return String::new() };
    let errors: Vec<&str> = text.lines().filter(|l| l.contains("Error")).collect();
    match errors.last() {
        Some(last) => format!(": {}", last.chars().take(300).collect::<String>()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_targets_are_absolute() {
        let mut c = GameClock::default();
        c.skipped_to(1);
        c.advance(Duration::from_secs(60), 2.0);
        assert_eq!(c.target(120, true), 241);
        assert_eq!(c.target(600, false), 600);
        assert!(c.skip(600));
        assert_eq!(c.target(30, true), 630);
        // Behind the furthest jump: the engine ignores it, and the jump stands.
        assert!(!c.skip(300));
        assert!(c.skip(c.target(30, true)));
        c.advance(Duration::from_secs(10), 0.5);
        assert_eq!(c.target(0, true), 635);
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(840), "14:00");
        assert_eq!(clock(3723), "1:02:03");
    }
}
