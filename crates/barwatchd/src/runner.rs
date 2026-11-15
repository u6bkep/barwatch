//! One run of a session: launches `spring-dedicated` on the replay, follows it over
//! the autohost interface, relays playback commands and tidies up when it ends.
//!
//! The dedicated server only streams the recording: it simulates nothing and needs
//! neither the game nor the map, just the engine build the replay was made with.

use crate::autohost::{Event, Notice};
use crate::chat::{self, Command};
use crate::state::{App, Session};
use anyhow::{bail, Context, Result};
use barwatch_core::api::{Control, Phase};
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
/// Wait between the replay stream beginning and skipping its recorded lobby wait.
/// The engine's skip never returns if it runs before the server has consumed the
/// first chunk of the recording, which it does about 0.1 s into playback
/// (`CGameServer::SkipTo` advances to the next chunk's own time, while
/// `CDemoReader` holds the first chunk back until 0.09 s).
const PREGAME_SKIP_DELAY: Duration = Duration::from_millis(1500);
/// How long the engine itself waits for a first viewer. Longer than
/// `EMPTY_TIMEOUT`, so it is barwatch that decides. The engine's default of 30 s is
/// shorter than a viewer's first launch can take.
const ENGINE_FIRST_VIEWER_WAIT_S: u32 = 600;

/// Makes sure the engine for `session` is installed. Serialized across sessions.
pub async fn ensure_engine(app: &App, session: &Session) -> Result<std::path::PathBuf> {
    let _guard = app.engine_lock.lock().await;
    engine::ensure(
        &app.http,
        &app.engines_dir(),
        &session.demo.engine_version,
        Os::Linux,
        Binary::Dedicated,
        &|_, _| {},
    )
    .await
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
    let engine_dir = ensure_engine(app, session).await?;
    let binary = Binary::Dedicated.find(&engine_dir).context("engine has no dedicated server")?;
    let lease = app.take_port().context("every game port is in use; stop another session first")?;
    let autohost = UdpSocket::bind(("127.0.0.1", 0)).await.context("opening the control socket")?;
    let autohost_port = autohost.local_addr()?.port();

    let run_dir = app.run_dir(&session.id);
    let _ = tokio::fs::remove_dir_all(&run_dir).await;
    tokio::fs::create_dir_all(&run_dir).await.with_context(|| format!("creating {}", run_dir.display()))?;
    let script_path = run_dir.join("script.txt");
    let config_path = run_dir.join("springsettings.cfg");
    let script = script::dedicated_script(&session.demo.game_data, &session.replay_path(), lease.port, autohost_port);
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
    let mut pregame_skip_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
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
                link.peer = Some(from);
                match Event::parse(&buf[..len]) {
                    Some(Event::Started) => {
                        listening = true;
                        session.mutate(|st| {
                            st.phase = Phase::Running;
                            st.game_port = Some(lease.port);
                        });
                    }
                    Some(Event::Message(text)) => match Notice::parse(&text) {
                        // The recording opens with the original lobby wait; nobody
                        // wants to sit through it again.
                        Some(Notice::PlaybackBegan) => pregame_skip_at = Some(Instant::now() + PREGAME_SKIP_DELAY),
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
                            let reply = apply(&link, session, control).await;
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
                    let reply = apply(&link, session, control).await;
                    link.send(&reply).await;
                }
                Some(RunnerCmd::Stop) | None => {
                    begin_stop(&link, &mut child, &mut stopping, "stopped").await;
                }
            },
            _ = tick.tick() => {
                if pregame_skip_at.is_some_and(|at| Instant::now() >= at) {
                    pregame_skip_at = None;
                    link.send("/skip 1").await;
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
struct Link {
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

/// Carries out a playback command; returns the line to announce in game chat.
async fn apply(link: &Link, session: &Session, control: Control) -> String {
    match control {
        Control::Skip { seconds, relative } => {
            // Exact on the server; each viewer's engine then replays the gap as fast
            // as it can. A target already passed is ignored by the engine.
            link.send(&format!("/skip {}{seconds}", if relative { "+" } else { "" })).await;
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
    fn clock_format() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(840), "14:00");
        assert_eq!(clock(3723), "1:02:03");
    }
}
