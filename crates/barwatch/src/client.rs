//! Taking part in one watch party: register with the server, install what the
//! replay needs, start the engine when the party runs, and keep the server told.
//!
//! No UI here. A front end (the window, or the `join` command) feeds [`Command`]s in
//! and receives an [`Update`] after every change.

use crate::content;
use crate::datadir::DataDir;
use crate::position::{self, Position};
use anyhow::{bail, ensure, Context, Result};
use barwatch_core::api::{
    self, ApiError, Control, Created, JoinRequest, JoinResponse, Phase, ServerInfo, SessionView, TimelineState, TimelineView,
    ViewerState, ViewerUpdate, API_VERSION,
};
use barwatch_core::engine::{self, Binary, Os};
use barwatch_core::{names, script};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use url::Url;

pub struct Settings {
    /// Name to ask the server for; it may hand back a changed one.
    pub name: String,
    pub data: DataDir,
    /// Run `spring-headless` instead of the game window (for testing).
    pub headless: bool,
}

/// Where this viewer is on the way into the party.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Connecting,
    /// Installing the engine, game version or map.
    Preparing {
        what: String,
        fraction: Option<f32>,
    },
    /// Everything is installed; the engine starts as soon as the party runs.
    Ready,
    /// The engine is running.
    Playing,
    /// The engine was closed while the party goes on; [`Command::Rejoin`] restarts it.
    Closed,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Update {
    pub session: Option<SessionView>,
    pub stage: Stage,
    /// The name the engine uses for this viewer.
    pub name: String,
    /// A one-off message for the user (a refused request, for example).
    pub notice: Option<String>,
    /// Where our engine is in the replay, as its reporter widget last said;
    /// [`Position::seconds_at`] gives game seconds for any instant. `None`: no engine,
    /// no report yet, or none for [`position::STALE_AFTER`] (user widgets switched off
    /// in BAR, engine stuck).
    pub position: Option<Position>,
    /// The party's statistics as the server last gave them; `None` until it has
    /// answered. A server without the timeline call reads as "unavailable".
    pub timeline: Option<Arc<TimelineView>>,
    /// One-off: the server did not take the jump just asked for ([`Command::Jump`]);
    /// `notice` says why.
    pub jump_refused: bool,
    /// The run of the party our engine, while it runs, was started for. After a
    /// replay ends the engine stays open; it is in the party's game only while
    /// this is the party's `run`.
    pub engine_run: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Ask the server to start the replay for everyone.
    Start,
    /// Start the engine again after it was closed.
    Rejoin,
    /// Leave the party; ends [`run`].
    Leave,
    /// Ask the server to move every viewer forward to this game time, in seconds.
    Jump(u32),
}

enum Internal {
    Session(Box<SessionView>),
    Gone,
    Progress(String, Option<f32>),
    Provisioned(Result<PathBuf, String>),
    /// From the follower of the engine started `launch`-th.
    Position {
        launch: u64,
        position: Option<Position>,
    },
    Timeline(Arc<TimelineView>),
    /// The server did not take the jump asked for, and why.
    JumpRefused(String),
}

/// How often the server hears from us; it forgets a viewer after 30 s of silence.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long the engine is given to leave the game when asked through its
/// widget, which looks once a second; then how long after the signal.
const QUIT_BY_WIDGET: Duration = Duration::from_millis(2500);
#[cfg(unix)]
const QUIT_BY_SIGNAL: Duration = Duration::from_millis(2500);

struct Client {
    http: reqwest::Client,
    server: Url,
    session_id: String,
    settings: Settings,
}

/// Runs until the user leaves, the party disappears, or `commands` closes.
pub async fn run(
    http: reqwest::Client,
    server: Url,
    session_id: String,
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    notify: impl Fn(Update) + Send + Sync + 'static,
) {
    let client = Client { http, server, session_id, settings };
    let mut update = Update {
        session: None,
        stage: Stage::Connecting,
        name: client.settings.name.clone(),
        notice: None,
        position: None,
        timeline: None,
        jump_refused: false,
        engine_run: None,
    };
    notify(update.clone());

    let joined = match client.register(None).await {
        Ok(j) => j,
        Err(e) => {
            update.stage = Stage::Failed(format!("{e:#}"));
            notify(update);
            return;
        }
    };
    let mut viewer_id = joined.viewer_id;
    update.name = joined.name;
    update.session = Some(joined.session.clone());
    notify(update.clone());

    let (tx, mut events) = mpsc::unbounded_channel();
    let to_self = tx.clone();
    let follower = tokio::spawn(follow(client.http.clone(), client.session_url(""), joined.session.rev, tx.clone()));
    let statistics = {
        let (http, url, tx) = (client.http.clone(), client.session_url("/timeline"), tx.clone());
        tokio::spawn(follow_timeline(http, url, TimelinePoll::default(), move |view| tx.send(Internal::Timeline(view)).is_ok()))
    };
    let provisioner = tokio::spawn(provision(
        client.http.clone(),
        client.settings.data.clone(),
        client.settings.headless,
        joined.session.replay.clone(),
        tx,
    ));

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut engine_dir: Option<PathBuf> = None;
    let mut child: Option<tokio::process::Child> = None;
    // The run of the party our engine was last started for.
    let mut launched_run: Option<u32> = None;
    let mut reported: Option<(ViewerState, String)> = None;
    // Reads our engine's position out of its log; one per engine started.
    let mut positions: Option<tokio::task::JoinHandle<()>> = None;
    let mut launches: u64 = 0;
    // Our engine is still open in a run that is over while the party runs a new one.
    let mut stale_engine = false;

    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                Internal::Session(view) => {
                    // The run our engine was started for is over (stopped, or a new
                    // one began): the engine, if still up, is in a game that is gone.
                    if launched_run.is_some_and(|run| run != view.run || view.phase != Phase::Running) {
                        stop_following(&mut positions, &mut update.position);
                    }
                    stale_engine = launched_run.is_some_and(|run| run != view.run) && view.phase == Phase::Running;
                    update.session = Some(*view);
                }
                Internal::Gone => {
                    update.stage = Stage::Failed("This watch party no longer exists.".into());
                    notify(update.clone());
                    break;
                }
                Internal::Progress(what, fraction) => {
                    if matches!(update.stage, Stage::Connecting | Stage::Preparing { .. }) {
                        update.stage = Stage::Preparing { what, fraction };
                    }
                }
                Internal::Provisioned(Ok(dir)) => {
                    engine_dir = Some(dir);
                    update.stage = Stage::Ready;
                }
                Internal::Provisioned(Err(e)) => update.stage = Stage::Failed(e),
                Internal::Position { launch, position } => {
                    if launch != launches || positions.is_none() {
                        continue; // from an engine we no longer follow
                    }
                    update.position = position;
                }
                Internal::Timeline(view) => update.timeline = Some(view),
                Internal::JumpRefused(why) => {
                    update.notice = Some(format!("The jump did not happen: {why}"));
                    update.jump_refused = true;
                }
            },
            command = commands.recv() => match command {
                Some(Command::Start) => {
                    if let Err(e) = client.start().await {
                        update.notice = Some(format!("{e:#}"));
                    }
                }
                Some(Command::Rejoin) => launched_run = None,
                // On its own: a slow server must not hold up the game position.
                Some(Command::Jump(seconds)) => {
                    let (http, url, tx) = (client.http.clone(), client.session_url("/control"), to_self.clone());
                    tokio::spawn(async move {
                        if let Err(e) = control(&http, url, Control::Skip { seconds, relative: false }).await {
                            let _ = tx.send(Internal::JumpRefused(format!("{e:#}")));
                        }
                    });
                }
                Some(Command::Leave) | None => break,
            },
            _ = heartbeat.tick() => reported = None,
            _ = wait(&mut child), if child.is_some() => {
                child = None;
                update.stage = Stage::Closed;
                update.engine_run = None;
                stop_following(&mut positions, &mut update.position);
            }
        }

        // A new run started while our engine is still open in the last one (it
        // stays open after a replay ends): close it, and the new one is joined
        // below once it has gone.
        if std::mem::take(&mut stale_engine) {
            if let Some(c) = &mut child {
                let _ = c.start_kill();
            }
        }

        // Start the engine when everything is installed and the party runs a run we
        // have not been in yet.
        if let (Some(dir), Some(session), None) = (&engine_dir, &update.session, &child) {
            let due = matches!(update.stage, Stage::Ready | Stage::Closed)
                && session.phase == Phase::Running
                && launched_run != Some(session.run);
            if due {
                launched_run = Some(session.run);
                match client.launch(dir, session, &update.name) {
                    Ok(c) => {
                        child = Some(c);
                        update.stage = Stage::Playing;
                        update.engine_run = Some(session.run);
                        stop_following(&mut positions, &mut update.position);
                        launches += 1;
                        let (launch, tx) = (launches, to_self.clone());
                        let log = client.engine_log();
                        positions = Some(tokio::spawn(position::follow(log, move |position| {
                            let _ = tx.send(Internal::Position { launch, position });
                        })));
                    }
                    Err(e) => update.stage = Stage::Failed(format!("{e:#}")),
                }
            }
        }

        let report = report_of(&update.stage);
        if reported.as_ref() != Some(&report) {
            match client.report(&viewer_id, &report).await {
                Ok(true) => reported = Some(report),
                // The server forgot us (it restarted, or we were silent too long).
                Ok(false) => {
                    let playing_as = child.is_some().then(|| update.name.clone());
                    if let Ok(again) = client.register(playing_as).await {
                        viewer_id = again.viewer_id;
                        if child.is_none() {
                            update.name = again.name;
                        }
                    }
                }
                Err(_) => {} // unreachable server: the next heartbeat tries again
            }
        }
        notify(update.clone());
        update.notice = None;
        update.jump_refused = false;
    }

    follower.abort();
    statistics.abort();
    provisioner.abort();
    stop_following(&mut positions, &mut update.position);
    if let Some(c) = child {
        close_engine(c, position::quit_file().as_deref()).await;
    }
    let _ = client.http.delete(client.session_url(&format!("/viewers/{viewer_id}"))).timeout(Duration::from_secs(5)).send().await;
}

async fn wait(child: &mut Option<tokio::process::Child>) {
    match child {
        Some(c) => {
            let _ = c.wait().await;
        }
        None => std::future::pending().await,
    }
}

/// Whether `child` ends within `within`.
async fn gone(child: &mut tokio::process::Child, within: Duration) -> bool {
    tokio::time::timeout(within, child.wait()).await.is_ok()
}

/// Closes our engine so that it leaves the game first: killed outright, the
/// dedicated server lists it as in the game until its connection times out
/// (about two minutes), while an engine that quits tells the server so.
///
/// In turn: the widget the app installs quits as the game's menu does when it
/// sees `quit_file` ([`position::QUIT_FILE`]; every platform, but only once the game has loaded and with
/// user widgets on); on Unix, SIGTERM, which the windowed engine's SDL takes as
/// the window being closed (the headless engine has no handler and just ends);
/// and last, a kill.
async fn close_engine(mut child: tokio::process::Child, quit_file: Option<&Path>) {
    let asked = quit_file.is_some_and(|f| std::fs::write(f, b"barwatch: leave the game\n").is_ok());
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut closed = asked && gone(&mut child, QUIT_BY_WIDGET).await;
    #[cfg(unix)]
    if !closed {
        if let Some(pid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: a plain signal to our own child, which has not been
            // waited for yet, so its pid is still its own.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            closed = gone(&mut child, QUIT_BY_SIGNAL).await;
        }
    }
    if !closed {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    if let Some(f) = quit_file {
        let _ = std::fs::remove_file(f);
    }
}

/// Forgets our engine's position and stops reading its log.
fn stop_following(follower: &mut Option<tokio::task::JoinHandle<()>>, position: &mut Option<Position>) {
    if let Some(task) = follower.take() {
        task.abort();
    }
    *position = None;
}

fn report_of(stage: &Stage) -> (ViewerState, String) {
    match stage {
        Stage::Connecting => (ViewerState::Preparing, String::new()),
        Stage::Preparing { what, fraction } => {
            let detail = match fraction {
                Some(f) => format!("{what} {:.0}%", f * 100.0),
                None => what.clone(),
            };
            (ViewerState::Preparing, detail)
        }
        Stage::Ready => (ViewerState::Ready, String::new()),
        Stage::Playing => (ViewerState::Playing, String::new()),
        Stage::Closed => (ViewerState::Ready, "closed the game".into()),
        Stage::Failed(e) => (ViewerState::Error, e.clone()),
    }
}

impl Client {
    /// `<server>/app/v1/sessions/<id><tail>`. The id and server were validated when
    /// the link was parsed.
    fn session_url(&self, tail: &str) -> Url {
        api::app_url(&self.server, &format!("sessions/{}{tail}", self.session_id)).expect("validated server URL")
    }

    /// `playing_as`: the name our engine is in the game under, when there is one.
    async fn register(&self, playing_as: Option<String>) -> Result<JoinResponse> {
        let info: ServerInfo =
            read(self.http.get(api::app_url(&self.server, "info")?).timeout(Duration::from_secs(20)).send().await)
                .await
                .context("reaching the server")?;
        ensure!(
            info.api == API_VERSION,
            "the server (version {}) and this app ({}) do not match; update the older one",
            info.version,
            env!("CARGO_PKG_VERSION")
        );
        let body = JoinRequest { name: self.settings.name.clone(), app_version: env!("CARGO_PKG_VERSION").into(), playing_as };
        read(self.http.post(self.session_url("/viewers")).json(&body).timeout(Duration::from_secs(20)).send().await)
            .await
            .context("joining the watch party")
    }

    /// `Ok(false)` when the server no longer knows this viewer.
    async fn report(&self, viewer_id: &str, (state, detail): &(ViewerState, String)) -> Result<bool> {
        let body = ViewerUpdate { state: *state, detail: detail.clone() };
        let resp = self
            .http
            .put(self.session_url(&format!("/viewers/{viewer_id}")))
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        Ok(!matches!(resp.status(), StatusCode::GONE | StatusCode::NOT_FOUND))
    }

    async fn start(&self) -> Result<()> {
        let _: SessionView =
            read(self.http.post(self.session_url("/start")).timeout(Duration::from_secs(20)).send().await).await?;
        Ok(())
    }

    /// Where the engine's output goes.
    fn engine_log(&self) -> PathBuf {
        self.settings.data.root.join("barwatch_engine.log")
    }


    /// Starts the engine as a spectator of the running party.
    fn launch(&self, engine_dir: &Path, session: &SessionView, name: &str) -> Result<tokio::process::Child> {
        let port = session.game_port.context("the server did not say which port to connect to")?;
        let host = match &session.game_host {
            Some(h) => h.clone(),
            None => self.server.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string(),
        };
        // Both end up inside a start script.
        ensure!(names::valid_host(&host), "the server named an odd game host: {host:?}");
        let name = names::sanitize(name);

        let data = &self.settings.data.root;
        let script_path = data.join("barwatch_script.txt");
        std::fs::write(&script_path, script::join_script(&host, port, &name))
            .with_context(|| format!("writing {}", script_path.display()))?;
        let which = if self.settings.headless { Binary::Headless } else { Binary::Spring };
        let binary = match stand_in_engine() {
            Some(program) => program,
            None => which.find(engine_dir).with_context(|| format!("no engine executable in {}", engine_dir.display()))?,
        };
        let log_path = self.engine_log();
        let log = std::fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
        // A request to leave left over from an engine before (the app was
        // killed while closing it) would end this one at once.
        if let Some(quit) = position::quit_file() {
            match std::fs::remove_file(&quit) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => bail!("removing {}: {e}", quit.display()),
                _ => {}
            }
        }
        if let Some(program) = stand_in_engine() {
            return tokio::process::Command::new(&program)
                .arg(self.session_url("").as_str())
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("starting {}", program.display()));
        }
        let mut command = tokio::process::Command::new(&binary);
        // The position reporter: a folder of our own, never BAR's. Without it the
        // game runs as before, only its position is unknown.
        match position::widget_dir().map(|dir| position::install(&dir)) {
            Some(Ok(Some(datadirs))) => {
                command.env("SPRING_DATADIR", datadirs);
            }
            Some(Ok(None)) => eprintln!("barwatch: the reporter folder's path cannot be given to the engine"),
            Some(Err(e)) => eprintln!("barwatch: could not write the position reporter: {e}"),
            None => eprintln!("barwatch: no local data folder for the position reporter"),
        }
        command
            .current_dir(engine_dir)
            .arg("--write-dir")
            .arg(data)
            .arg("--isolation")
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))
    }
}

/// DEVELOPMENT, debug builds only: a program to run in place of the game, named by
/// `BARWATCH_STAND_IN_ENGINE`. Nothing is installed for it, and it is started with
/// the party's address as its one argument; whatever it writes to standard output is
/// read as the engine's would be, so it can report a game position (`position.rs`)
/// without a game. For driving the window in tests without an engine.
fn stand_in_engine() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os("BARWATCH_STAND_IN_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from)
    } else {
        None
    }
}

/// A playback command for the whole party, to `<session>/control`.
async fn control(http: &reqwest::Client, url: Url, control: Control) -> Result<()> {
    let _: SessionView = read(http.post(url).json(&control).timeout(Duration::from_secs(10)).send().await).await?;
    Ok(())
}

/// Decodes a JSON reply, turning the server's `{"error": …}` into the error text.
async fn read<T: serde::de::DeserializeOwned>(resp: reqwest::Result<reqwest::Response>) -> Result<T> {
    let resp = resp?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<ApiError>(&text) {
            Ok(e) => bail!("{}", e.error),
            // Not ours: a proxy's login page, most likely.
            Err(_) => bail!("the server answered {status}; is /app/ let through its proxy?"),
        }
    }
    resp.json().await.context("unexpected reply from the server")
}

/// Long-polls the session and forwards every change.
async fn follow(http: reqwest::Client, url: Url, mut rev: u64, tx: mpsc::UnboundedSender<Internal>) {
    loop {
        let resp = http
            .get(url.clone())
            .query(&[("rev", rev.to_string()), ("wait", "25".to_string())])
            .timeout(Duration::from_secs(40))
            .send()
            .await;
        match resp {
            Ok(r) if r.status() == StatusCode::NOT_FOUND => {
                let _ = tx.send(Internal::Gone);
                return;
            }
            Ok(r) if r.status().is_success() => match r.json::<SessionView>().await {
                Ok(view) => {
                    rev = view.rev;
                    if tx.send(Internal::Session(Box::new(view))).is_err() {
                        return;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
            },
            _ => tokio::time::sleep(Duration::from_secs(3)).await,
        }
    }
}

/// How the timeline call is polled. The defaults are the app's; tests shorten them.
#[derive(Clone, Copy)]
struct TimelinePoll {
    /// Largest answer read. The document of a long 8v8 comes to a few MiB.
    max_bytes: usize,
    /// How long the server may hold an answer back waiting for a change, in seconds.
    wait_s: u32,
    /// Pause after a failed request, or after an answer that came back at once
    /// with nothing new.
    retry: Duration,
    /// Pause after a 404 (a server older than the timeline), and after an
    /// answer this app cannot read (a newer one): neither changes soon.
    absent_retry: Duration,
    /// Least time between two answers while the statistics are being worked
    /// out: the server's count moves with nearly every record, and each
    /// answer is the whole document.
    building_gap: Duration,
}

impl Default for TimelinePoll {
    fn default() -> Self {
        TimelinePoll {
            max_bytes: 16 << 20,
            wait_s: 25,
            retry: Duration::from_secs(3),
            absent_retry: Duration::from_secs(600),
            building_gap: Duration::from_secs(3),
        }
    }
}

/// What a server that has no timeline call amounts to.
fn no_timeline() -> TimelineView {
    unavailable(0, "This server does not offer statistics: it is older than they are.")
}

fn unavailable(rev: u64, note: &str) -> TimelineView {
    TimelineView { rev, state: TimelineState::Unavailable, note: Some(note.into()), doc: None }
}

/// One answer of the timeline call.
enum Answer {
    View(TimelineView),
    /// 404: the server has no such call, or the party is gone (which the
    /// session follower finds out).
    Absent,
    /// An answer this app cannot read, with the server's count when that much
    /// of it could be read, and why in words for the viewer.
    Unreadable(Option<u64>, &'static str),
}

/// Long-polls the party's statistics and hands on every change, until `deliver`
/// says nobody listens any more. The server's bytes are taken for no more than
/// what the types make of them, and no more than `max_bytes` of them are read.
async fn follow_timeline(
    http: reqwest::Client,
    url: Url,
    poll: TimelinePoll,
    deliver: impl Fn(Arc<TimelineView>) -> bool,
) {
    // The `rev` of the view delivered last; the server's own count, which starts
    // over when the server does.
    let mut rev: Option<u64> = None;
    // Something was delivered: from then on a 404 is not an old server.
    let mut delivered = false;
    // What was said last when there was nothing to show, not to say it twice.
    let mut said: Option<&'static str> = None;
    loop {
        let mut request = http.get(url.clone()).timeout(Duration::from_secs(u64::from(poll.wait_s) + 15));
        if let Some(rev) = rev {
            request = request.query(&[("rev", rev.to_string()), ("wait", poll.wait_s.to_string())]);
        }
        let asked = Instant::now();
        match fetch_timeline(request, poll.max_bytes).await {
            Ok(Answer::View(view)) => {
                said = None;
                if rev == Some(view.rev) {
                    // The wait ran out with nothing new. A server that answers
                    // at once instead of waiting must not be asked in a loop.
                    if asked.elapsed() < Duration::from_secs(1) {
                        tokio::time::sleep(poll.retry).await;
                    }
                    continue;
                }
                rev = Some(view.rev);
                let building = view.state == TimelineState::Building;
                delivered = true;
                if !deliver(Arc::new(view)) {
                    return;
                }
                if building {
                    tokio::time::sleep(poll.building_gap.saturating_sub(asked.elapsed())).await;
                }
            }
            // Once there were statistics a 404 is the party going, or a passing
            // fault: keep what was shown and ask again later.
            Ok(Answer::Absent) if delivered => tokio::time::sleep(poll.retry * 10).await,
            Ok(Answer::Absent) => {
                if said != Some("absent") && !deliver(Arc::new(no_timeline())) {
                    return;
                }
                said = Some("absent");
                rev = None;
                tokio::time::sleep(poll.absent_retry).await;
            }
            Ok(Answer::Unreadable(at, why)) => {
                if said != Some(why) && !deliver(Arc::new(unavailable(at.unwrap_or(0), why))) {
                    return;
                }
                said = Some(why);
                delivered = true;
                // Asked again from where it was, it would answer the same at once.
                rev = at.or(rev);
                tokio::time::sleep(poll.absent_retry).await;
            }
            Err(_) => tokio::time::sleep(poll.retry).await,
        }
    }
}

const TOO_LARGE: &str = "This server's statistics are too large for barwatch to take.";
const UNREADABLE: &str = "This server's statistics are in a form this barwatch cannot read: update barwatch to see them.";

/// One answer of the timeline call. `Err` for a request that failed on the way
/// (worth asking again soon); what came back and cannot be used is an answer.
async fn fetch_timeline(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<Answer> {
    let mut resp = request.send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(Answer::Absent);
    }
    ensure!(resp.status().is_success(), "the server answered {}", resp.status());
    if resp.content_length().is_some_and(|n| n > max_bytes as u64) {
        return Ok(Answer::Unreadable(None, TOO_LARGE));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max_bytes {
            return Ok(Answer::Unreadable(None, TOO_LARGE));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(read_timeline(&body))
}

/// The answer's bytes as this app takes them. A later server may add a state
/// or a kind of series this app does not know; then only the count is read,
/// so that the same answer is not fetched over and over.
fn read_timeline(body: &[u8]) -> Answer {
    if let Ok(view) = serde_json::from_slice::<TimelineView>(body) {
        return Answer::View(view);
    }
    #[derive(serde::Deserialize)]
    struct Count {
        rev: u64,
    }
    Answer::Unreadable(serde_json::from_slice::<Count>(body).ok().map(|c| c.rev), UNREADABLE)
}

/// Installs the engine, game version and map the replay needs.
async fn provision(
    http: reqwest::Client,
    data: DataDir,
    headless: bool,
    replay: api::ReplayInfo,
    tx: mpsc::UnboundedSender<Internal>,
) {
    let progress = |what: String, fraction: Option<f32>| {
        let _ = tx.send(Internal::Progress(what, fraction));
    };
    let result: Result<PathBuf> = async {
        std::fs::create_dir_all(&data.root).with_context(|| format!("creating {}", data.root.display()))?;
        if stand_in_engine().is_some() {
            return Ok(data.root.clone());
        }
        progress(format!("Checking engine {}", replay.engine), None);
        let what = format!("Downloading engine {}", replay.engine);
        let needs = if headless { Binary::Headless } else { Binary::Spring };
        // One event per percent, not one per network chunk.
        let last_percent = std::sync::atomic::AtomicU32::new(u32::MAX);
        let on_bytes = |done: u64, total: Option<u64>| {
            let fraction = total.filter(|t| *t > 0).map(|t| done as f32 / t as f32);
            let percent = fraction.map_or(0, |f| (f * 100.0) as u32);
            if last_percent.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
                progress(what.clone(), fraction);
            }
        };
        let dir = engine::ensure(&http, &data.engines(), &replay.engine, Os::current(), needs, &on_bytes).await?;
        progress(format!("Checking game version {}", short_game(&replay.game)), None);
        content::ensure_game(&dir, &data.root, &replay.game).await?;
        progress(format!("Checking map {}", replay.map), None);
        content::ensure_map(&http, &dir, &data.root, &replay.map).await?;
        Ok(dir)
    }
    .await;
    let _ = tx.send(Internal::Provisioned(result.map_err(|e| format!("{e:#}"))));
}

/// "Beyond All Reason test-31516-95f3e26" → "test-31516-95f3e26".
fn short_game(game: &str) -> &str {
    game.strip_prefix("Beyond All Reason ").unwrap_or(game)
}

/// Uploads a local replay to a paired server; returns the new party's id.
pub async fn create_session(http: &reqwest::Client, server: &Url, token: &str, replay: &Path) -> Result<String> {
    let bytes = tokio::fs::read(replay).await.with_context(|| format!("reading {}", replay.display()))?;
    let file_name = replay.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let encoded: String = url::form_urlencoded::byte_serialize(file_name.as_bytes()).collect();
    let resp = http
        .post(api::app_url(server, "sessions")?)
        .bearer_auth(token)
        .header("content-type", "application/octet-stream")
        .header("x-barwatch-filename", encoded)
        .body(bytes)
        .timeout(Duration::from_secs(300))
        .send()
        .await;
    let created: Created = read(resp).await.context("uploading the replay")?;
    ensure!(names::valid_id(&created.id), "the server answered with a malformed id");
    Ok(created.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine stand-in that leaves when the widget's file appears is let
    /// go at once and the file cleared; one that does not is signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn leaving_asks_the_engine_first_and_signals_it_only_then() {
        let dir = std::env::temp_dir().join(format!("barwatch-close-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let quit = dir.join("barwatch_quit");
        let script = format!("while [ ! -e '{}' ]; do sleep 0.05; done", quit.display());
        let child = tokio::process::Command::new("sh").arg("-c").arg(script).spawn().unwrap();
        let started = Instant::now();
        close_engine(child, Some(&quit)).await;
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(!quit.exists(), "the request does not outlive the engine");
        // Deaf to the file (user widgets off): SIGTERM, which the game takes as
        // its window closing.
        let child = tokio::process::Command::new("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        close_engine(child, Some(&quit)).await;
        let took = started.elapsed();
        assert!(took >= QUIT_BY_WIDGET && took < QUIT_BY_WIDGET + QUIT_BY_SIGNAL, "{took:?}");
        // Deaf to both: killed.
        let child = tokio::process::Command::new("sh").arg("-c").arg("trap '' TERM; sleep 30").spawn().unwrap();
        // Time for the shell to set its trap before the signal comes.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        close_engine(child, None).await;
        let took = started.elapsed();
        assert!(took >= QUIT_BY_SIGNAL && took < QUIT_BY_SIGNAL + Duration::from_secs(2), "{took:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_follow_the_stage() {
        assert_eq!(
            report_of(&Stage::Preparing { what: "Downloading engine 2026.07.04".into(), fraction: Some(0.426) }),
            (ViewerState::Preparing, "Downloading engine 2026.07.04 43%".into())
        );
        assert_eq!(report_of(&Stage::Ready), (ViewerState::Ready, String::new()));
        assert_eq!(report_of(&Stage::Failed("no space".into())), (ViewerState::Error, "no space".into()));
        assert_eq!(short_game("Beyond All Reason test-1-abc"), "test-1-abc");
    }

    /// The timeline call as a tiny server answers it, one connection per answer.
    mod timeline_follower {
        use super::*;
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        enum Reply {
            View(TimelineView),
            Status(u16),
            /// A body this long, said up front or sent in chunks without saying.
            Huge { len: usize, chunked: bool },
            /// A JSON body as it is.
            Raw(&'static str),
        }

        fn view(rev: u64, state: TimelineState) -> Reply {
            Reply::View(TimelineView { rev, state, note: None, doc: None })
        }

        /// Asked paths (with their queries) and when, in order.
        type Asked = Arc<Mutex<Vec<(String, Instant)>>>;

        /// Serves `replies` in order; after the last it holds every request
        /// open without answering.
        async fn serve(replies: Vec<Reply>) -> (Url, Asked) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = Url::parse(&format!("http://{}/app/v1/sessions/abc/timeline", listener.local_addr().unwrap())).unwrap();
            let asked: Asked = Arc::default();
            let log = asked.clone();
            tokio::spawn(async move {
                let mut replies = replies.into_iter();
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && socket.read(&mut byte).await.unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let line = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_owned();
                    log.lock().unwrap().push((line.split(' ').nth(1).unwrap_or("").to_owned(), Instant::now()));
                    let Some(reply) = replies.next() else {
                        // Held open: the client waits on it until the test ends.
                        tokio::spawn(async move {
                            let _keep = socket;
                            std::future::pending::<()>().await;
                        });
                        continue;
                    };
                    let response = match reply {
                        Reply::View(v) => {
                            let body = serde_json::to_vec(&v).unwrap();
                            let mut r = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
                            r.extend(body);
                            r
                        }
                        Reply::Raw(body) => {
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
                        }
                        Reply::Status(code) => {
                            format!("HTTP/1.1 {code} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
                        }
                        Reply::Huge { len, chunked: false } => {
                            let mut r = format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n").into_bytes();
                            r.extend(std::iter::repeat_n(b' ', len));
                            r
                        }
                        Reply::Huge { len, chunked: true } => {
                            let mut r = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
                            for _ in 0..len / 1000 {
                                r.extend(b"3e8\r\n");
                                r.extend(std::iter::repeat_n(b' ', 1000));
                                r.extend(b"\r\n");
                            }
                            r.extend(b"0\r\n\r\n");
                            r
                        }
                    };
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                }
            });
            (url, asked)
        }

        const POLL: TimelinePoll = TimelinePoll {
            max_bytes: 64 << 10,
            wait_s: 1,
            retry: Duration::from_millis(100),
            absent_retry: Duration::from_millis(600),
            building_gap: Duration::ZERO,
        };

        /// Follows `url` until `want` views have come, or two seconds pass.
        async fn follow_for(url: Url, poll: TimelinePoll, want: usize) -> Vec<TimelineView> {
            let got: Arc<Mutex<Vec<TimelineView>>> = Arc::default();
            let sink = got.clone();
            let follow = follow_timeline(reqwest::Client::new(), url, poll, move |v| {
                let mut got = sink.lock().unwrap();
                got.push((*v).clone());
                got.len() < want
            });
            let _ = tokio::time::timeout(Duration::from_secs(2), follow).await;
            let got = got.lock().unwrap();
            got.clone()
        }

        fn paths(asked: &Asked) -> Vec<String> {
            asked.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
        }

        #[tokio::test]
        async fn hands_on_each_change_once_and_waits_from_the_last() {
            let (url, asked) = serve(vec![
                view(1, TimelineState::Queued),
                // The wait ran out: nothing new.
                view(1, TimelineState::Queued),
                view(2, TimelineState::Building),
                // The server started over: its count with it.
                view(1, TimelineState::Building),
            ])
            .await;
            let got = follow_for(url, POLL, 3).await;
            let revs: Vec<(u64, TimelineState)> = got.iter().map(|v| (v.rev, v.state)).collect();
            assert_eq!(revs, [(1, TimelineState::Queued), (2, TimelineState::Building), (1, TimelineState::Building)]);
            let paths = paths(&asked);
            assert_eq!(paths[0], "/app/v1/sessions/abc/timeline", "the first answer comes at once");
            assert_eq!(paths[1], "/app/v1/sessions/abc/timeline?rev=1&wait=1");
            assert_eq!(paths[3], "/app/v1/sessions/abc/timeline?rev=2&wait=1");
        }

        #[tokio::test]
        async fn a_server_without_the_call_is_told_once_and_asked_rarely() {
            let (url, asked) =
                serve(vec![Reply::Status(404), Reply::Status(404), view(5, TimelineState::Complete)]).await;
            let got = follow_for(url, POLL, 2).await;
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].state, TimelineState::Unavailable);
            assert!(got[0].note.as_deref().is_some_and(|n| n.contains("older")));
            // The second 404 says nothing new; the server, upgraded, then has it.
            assert_eq!((got[1].rev, got[1].state), (5, TimelineState::Complete));
            let asked = asked.lock().unwrap();
            assert!(asked[1].1 - asked[0].1 >= POLL.absent_retry, "not asked again at once");
            // Asked afresh after the 404s: no stale rev.
            assert_eq!(asked[2].0, "/app/v1/sessions/abc/timeline");
        }

        #[tokio::test]
        async fn answers_too_large_are_said_once_and_not_fetched_in_a_loop() {
            let (url, asked) = serve(vec![
                Reply::Huge { len: 100_000, chunked: false },
                Reply::Huge { len: 100_000, chunked: true },
                Reply::Status(502),
                view(3, TimelineState::Building),
            ])
            .await;
            let got = follow_for(url, POLL, 2).await;
            let seen: Vec<(u64, TimelineState)> = got.iter().map(|v| (v.rev, v.state)).collect();
            assert_eq!(seen, [(0, TimelineState::Unavailable), (3, TimelineState::Building)]);
            assert!(got[0].note.as_deref().is_some_and(|n| n.contains("too large")));
            let asked = asked.lock().unwrap();
            assert_eq!(asked.len(), 4);
            assert!(asked[1].1 - asked[0].1 >= POLL.absent_retry, "not fetched again soon");
            assert!(asked[3].1 - asked[2].1 >= POLL.retry, "a pause after a failure");
        }

        #[tokio::test]
        async fn an_answer_from_a_later_server_is_said_once_and_waited_on() {
            // A state this app does not know: the whole answer cannot be read.
            let later = r#"{"rev": 9, "state": "archived", "doc": null}"#;
            let (url, asked) = serve(vec![Reply::Raw(later), Reply::Raw(later), view(10, TimelineState::Complete)]).await;
            let poll = TimelinePoll { absent_retry: Duration::from_millis(300), ..POLL };
            let got = follow_for(url, poll, 2).await;
            let seen: Vec<(u64, TimelineState)> = got.iter().map(|v| (v.rev, v.state)).collect();
            assert_eq!(seen, [(9, TimelineState::Unavailable), (10, TimelineState::Complete)]);
            assert!(got[0].note.as_deref().is_some_and(|n| n.contains("update barwatch")));
            // Asked again from its count, so a server that waits for a change does.
            assert_eq!(paths(&asked)[1], "/app/v1/sessions/abc/timeline?rev=9&wait=1");
        }

        #[tokio::test]
        async fn a_404_after_statistics_keeps_them() {
            let (url, _) = serve(vec![view(4, TimelineState::Complete), Reply::Status(404)]).await;
            let got = follow_for(url, POLL, 2).await;
            assert_eq!(got.iter().map(|v| v.rev).collect::<Vec<_>>(), [4], "no word of an old server");
        }

        #[tokio::test]
        async fn answers_come_no_closer_than_the_gap_while_building() {
            let replies: Vec<Reply> = (1..20).map(|rev| view(rev, TimelineState::Building)).collect();
            let (url, _) = serve(replies).await;
            let poll = TimelinePoll { building_gap: Duration::from_millis(500), ..POLL };
            let got = follow_for(url, poll, 20).await;
            assert!((3..=5).contains(&got.len()), "{} answers in two seconds", got.len());
        }

        #[tokio::test]
        async fn a_server_that_answers_at_once_is_not_asked_in_a_loop() {
            let same: Vec<Reply> = (0..50).map(|_| view(7, TimelineState::Building)).collect();
            let (url, asked) = serve(same).await;
            let poll = TimelinePoll { retry: Duration::from_millis(300), ..POLL };
            let got = follow_for(url, poll, 2).await;
            assert_eq!(got.len(), 1);
            // Two seconds at one ask per 300 ms, not fifty asks.
            let n = asked.lock().unwrap().len();
            assert!((5..=9).contains(&n), "{n} asks");
        }

        #[tokio::test]
        async fn stops_when_nobody_listens() {
            let (url, _) = serve(vec![view(1, TimelineState::Queued), view(2, TimelineState::Queued)]).await;
            let follow = follow_timeline(reqwest::Client::new(), url, POLL, |_| false);
            assert!(tokio::time::timeout(Duration::from_secs(2), follow).await.is_ok());
        }
    }
}
