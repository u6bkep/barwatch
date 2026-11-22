//! Being in one room: register with the server, install what the room's current
//! replay needs (again whenever the room picks another), start the engine when the
//! room runs, and keep the server told (DESIGN.md §9.7).
//!
//! No UI here. A front end (the window, or the `join` command) feeds [`Command`]s in
//! and receives an [`Update`] after every change.

use crate::content;
use crate::datadir::DataDir;
use crate::position::{self, Position};
use anyhow::{bail, ensure, Context, Result};
use barwatch_core::api::{
    self, ApiError, Archived, Control, JoinRequest, JoinResponse, NewRoom, Phase, PickReplay, ReplayInfo, RoomView, ServerInfo,
    TimelineState, TimelineView, ViewerState, ViewerUpdate, API_VERSION,
};
use barwatch_core::engine::{self, Binary, Os};
use barwatch_core::{names, script};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

pub struct Settings {
    /// Name to ask the server for; it may hand back a changed one.
    pub name: String,
    pub data: DataDir,
    /// Run `spring-headless` instead of the game window (for testing).
    pub headless: bool,
}

/// Where this viewer stands with the room's current replay.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Connecting,
    /// The room has no replay yet: nothing to install.
    NoReplay,
    /// Installing the engine, game version or map.
    Preparing {
        what: String,
        fraction: Option<f32>,
    },
    /// Everything is installed; the engine starts as soon as the room runs.
    Ready,
    /// The engine is running.
    Playing,
    /// The engine was closed while the room goes on; [`Command::Rejoin`] restarts it.
    Closed,
    Failed(String),
    /// The room was deleted or given a new link; [`run`] has ended.
    Gone,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub room: Option<RoomView>,
    pub stage: Stage,
    /// The key of the replay `stage` is about (`RoomReplay::key`): the room's
    /// current one as this client last saw it.
    pub replay: Option<String>,
    /// This viewer's name among the room's members.
    pub member: String,
    /// The name this viewer's engine uses in the game.
    pub engine_name: String,
    /// A one-off message for the user (a refused request, for example).
    pub notice: Option<String>,
    /// Where our engine is in the replay, as its reporter widget last said;
    /// [`Position::seconds_at`] gives game seconds for any instant. `None`: no engine,
    /// no report yet, or none for [`position::STALE_AFTER`] (user widgets switched off
    /// in BAR, engine stuck).
    pub position: Option<Position>,
    /// The current replay's statistics as the server last gave them; `None` until it
    /// has answered for this replay.
    pub timeline: Option<Arc<TimelineView>>,
    /// One-off: the server did not take the jump just asked for ([`Command::Jump`]);
    /// `notice` says why.
    pub jump_refused: bool,
    /// The run of the room our engine, while it runs, was started for. After a
    /// replay ends the engine stays open; it is in the room's game only while
    /// this is the room's `run`.
    pub engine_run: Option<u32>,
    /// A replay picked from here is on its way to the server.
    pub picking: bool,
}

/// What to make the room's replay ([`Command::Pick`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A replay file of this computer, uploaded into the room.
    File(PathBuf),
    /// A link to a replay on BAR's site, or its id.
    Bar(String),
    /// A key from the room's history.
    History(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Ask the server to start the room's replay for everyone.
    Start,
    /// Ask the server to stop the run.
    Stop,
    /// Start the engine again after it was closed.
    Rejoin,
    /// Leave the room; ends [`run`].
    Leave,
    /// Ask the server to move every viewer forward to this game time, in seconds.
    Jump(u32),
    /// Make this the room's replay. It does not start.
    Pick(Pick),
}

enum Internal {
    Room(Box<RoomView>),
    Gone,
    /// From the `download`-th download (each replay the room picks gets one).
    Progress {
        download: u64,
        what: String,
        fraction: Option<f32>,
    },
    Provisioned {
        download: u64,
        result: Result<PathBuf, String>,
    },
    /// From the follower of the engine started `launch`-th.
    Position {
        launch: u64,
        position: Option<Position>,
    },
    Timeline(Arc<TimelineView>),
    /// The server did not take the jump asked for, and why.
    JumpRefused(String),
    /// A request made for a command was refused; why, in words for the user.
    Refused(String),
    /// A pick has been answered (taken or refused).
    Picked,
}

/// How often the server hears from us; it forgets a viewer after 30 s of silence.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long the engine is given to leave the game when asked through its
/// widget, which looks once a second; then how long after the signal.
const QUIT_BY_WIDGET: Duration = Duration::from_millis(2500);
#[cfg(unix)]
const QUIT_BY_SIGNAL: Duration = Duration::from_millis(2500);
/// A request to leave younger than this is an engine's that is closing now.
const QUIT_FILE_FRESH: Duration = Duration::from_secs(10);

/// What the server says when a room is not there: deleted, or given a new link.
pub const ROOM_GONE: &str = "This room no longer exists: it was deleted or has a new link. Ask for the new link.";

/// A 404 from a room's calls: the room is gone (see [`ROOM_GONE`]).
#[derive(Debug)]
pub struct RoomGone;

impl std::fmt::Display for RoomGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ROOM_GONE)
    }
}

impl std::error::Error for RoomGone {}

/// One room's calls, `<server>/app/v1/rooms/<id>/…`.
#[derive(Clone)]
pub struct RoomApi {
    http: reqwest::Client,
    server: Url,
    room: String,
}

impl RoomApi {
    pub fn new(http: reqwest::Client, server: Url, room: String) -> RoomApi {
        RoomApi { http, server, room }
    }

    /// `<server>/app/v1/rooms/<id><tail>`. The id and server were validated when
    /// the link was parsed.
    fn url(&self, tail: &str) -> Url {
        api::app_url(&self.server, &format!("rooms/{}{tail}", self.room)).expect("validated server URL")
    }

    async fn post(&self, tail: &str) -> Result<RoomView> {
        read_room(self.http.post(self.url(tail)).timeout(Duration::from_secs(30)).send().await).await
    }

    pub async fn start(&self) -> Result<RoomView> {
        self.post("/start").await
    }

    pub async fn stop(&self) -> Result<RoomView> {
        self.post("/stop").await
    }

    async fn control(&self, control: Control) -> Result<RoomView> {
        read_room(self.http.post(self.url("/control")).json(&control).timeout(Duration::from_secs(10)).send().await).await
    }

    /// `PUT rooms/<id>/replay`. Never starts it.
    pub async fn pick(&self, pick: &Pick) -> Result<RoomView> {
        let request = self.http.put(self.url("/replay"));
        let request = match pick {
            Pick::File(path) => {
                let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {}", path.display()))?;
                let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let encoded: String = url::form_urlencoded::byte_serialize(file_name.as_bytes()).collect();
                request
                    .header("content-type", "application/octet-stream")
                    .header("x-barwatch-filename", encoded)
                    .body(bytes)
                    .timeout(Duration::from_secs(300))
            }
            Pick::Bar(link) => request
                .json(&PickReplay { bar_replay: Some(link.trim().to_owned()), key: None })
                .timeout(Duration::from_secs(300)),
            Pick::History(key) => {
                request.json(&PickReplay { bar_replay: None, key: Some(key.clone()) }).timeout(Duration::from_secs(300))
            }
        };
        read_room(request.send().await).await
    }
}

/// Runs until the user leaves, the room disappears, or `commands` closes.
pub async fn run(
    http: reqwest::Client,
    server: Url,
    room_id: String,
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    notify: impl Fn(Update) + Send + Sync + 'static,
) {
    let api = RoomApi::new(http, server, room_id);
    let (tx, mut events) = mpsc::unbounded_channel();
    let mut seat = Seat {
        update: Update {
            room: None,
            stage: Stage::Connecting,
            replay: None,
            member: settings.name.clone(),
            engine_name: settings.name.clone(),
            notice: None,
            position: None,
            timeline: None,
            jump_refused: false,
            engine_run: None,
            picking: false,
        },
        api,
        settings,
        tx,
        viewer_id: String::new(),
        engine_dir: None,
        provisioner: None,
        downloads: 0,
        closing: Vec::new(),
        statistics: None,
        child: None,
        launched_run: None,
        launched_as: None,
        reported: None,
        positions: None,
        launches: 0,
        stale_engine: false,
        picks: 0,
    };
    notify(seat.update.clone());

    let joined = match seat.register(None).await {
        Ok(j) => j,
        Err(e) => {
            seat.update.stage = if e.is::<RoomGone>() { Stage::Gone } else { Stage::Failed(format!("{e:#}")) };
            notify(seat.update);
            return;
        }
    };
    seat.viewer_id = joined.viewer_id;
    seat.update.member = joined.name;
    let follower = tokio::spawn(follow(seat.api.clone(), joined.room.rev, seat.tx.clone()));
    seat.on_room(joined.room);
    notify(seat.update.clone());

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                Internal::Room(view) => seat.on_room(*view),
                Internal::Gone => {
                    seat.update.stage = Stage::Gone;
                    notify(seat.update.clone());
                    break;
                }
                // From a download that has since been replaced (the room went from A to
                // B and back to A, say), whatever it is about, is no word on this one.
                Internal::Progress { download, what, fraction } => {
                    if download == seat.downloads && matches!(seat.update.stage, Stage::Preparing { .. }) {
                        seat.update.stage = Stage::Preparing { what, fraction };
                    }
                }
                Internal::Provisioned { download, result } => {
                    if download == seat.downloads && seat.provisioner.is_some() {
                        seat.provisioner = None;
                        match result {
                            Ok(dir) => {
                                seat.engine_dir = Some(dir);
                                seat.update.stage = Stage::Ready;
                            }
                            Err(e) => seat.update.stage = Stage::Failed(e),
                        }
                    }
                }
                Internal::Position { launch, position } => {
                    if launch != seat.launches || seat.positions.is_none() {
                        continue; // from an engine we no longer follow
                    }
                    seat.update.position = position;
                }
                // A view of another replay than the room's current one is from a
                // follower that has since been replaced.
                Internal::Timeline(view) => {
                    if view.replay == seat.update.replay {
                        seat.update.timeline = Some(view);
                    }
                }
                Internal::JumpRefused(why) => {
                    seat.update.notice = Some(format!("The jump did not happen: {why}"));
                    seat.update.jump_refused = true;
                }
                Internal::Refused(why) => seat.update.notice = Some(why),
                Internal::Picked => {
                    seat.picks = seat.picks.saturating_sub(1);
                    seat.update.picking = seat.picks > 0;
                }
            },
            command = commands.recv() => match command {
                Some(Command::Leave) | None => break,
                Some(command) => seat.command(command),
            },
            _ = heartbeat.tick() => seat.reported = None,
            _ = wait(&mut seat.child), if seat.child.is_some() => {
                seat.child = None;
                seat.launched_as = None;
                seat.update.stage = Stage::Closed;
                seat.update.engine_run = None;
                stop_following(&mut seat.positions, &mut seat.update.position);
            }
        }

        // A new run started while our engine is still open in the last one (it
        // stays open after a replay ends): close it, and the new one is joined
        // below once it has gone.
        if std::mem::take(&mut seat.stale_engine) {
            if let Some(c) = &mut seat.child {
                let _ = c.start_kill();
            }
        }
        seat.launch_if_due();
        seat.report().await;
        notify(seat.update.clone());
        seat.update.notice = None;
        seat.update.jump_refused = false;
    }

    follower.abort();
    seat.leave().await;
}

/// What a viewer last told the server: its state, in words, and the replay it is about.
type Report = (ViewerState, String, Option<String>);

/// The client's side of being in the room.
struct Seat {
    update: Update,
    api: RoomApi,
    settings: Settings,
    /// Back into the loop, from the tasks this starts.
    tx: mpsc::UnboundedSender<Internal>,
    viewer_id: String,
    /// The engine installed for the current replay, once everything is there.
    engine_dir: Option<PathBuf>,
    /// Installing what the current replay needs.
    provisioner: Option<JoinHandle<()>>,
    /// Counts downloads started, so that only the latest one's word is taken.
    downloads: u64,
    /// Engines closing after the room changed its replay; leaving waits for them.
    closing: Vec<JoinHandle<()>>,
    /// Long-polls the current replay's statistics.
    statistics: Option<JoinHandle<()>>,
    child: Option<tokio::process::Child>,
    /// The run of the room our engine was last started for.
    launched_run: Option<u32>,
    /// The name our engine, while it runs, is in the game under.
    launched_as: Option<String>,
    reported: Option<Report>,
    /// Reads our engine's position out of its log; one per engine started.
    positions: Option<JoinHandle<()>>,
    launches: u64,
    /// Our engine is still open in a run that is over while the room runs a new one.
    stale_engine: bool,
    /// Picks sent and not answered yet.
    picks: u32,
}

impl Seat {
    /// `playing_as`: the name our engine is in the game under, when there is one.
    async fn register(&self, playing_as: Option<String>) -> Result<JoinResponse> {
        let http = &self.api.http;
        let info: ServerInfo =
            read(http.get(api::app_url(&self.api.server, "info")?).timeout(Duration::from_secs(20)).send().await)
                .await
                .context("reaching the server")?;
        ensure!(
            info.api == API_VERSION,
            "the server (version {}) and this app ({}) do not match; update the older one",
            info.version,
            env!("CARGO_PKG_VERSION")
        );
        let body = JoinRequest { name: self.settings.name.clone(), app_version: env!("CARGO_PKG_VERSION").into(), playing_as };
        let resp = http.post(self.api.url("/viewers")).json(&body).timeout(Duration::from_secs(20)).send().await;
        read_room(resp).await.context("joining the room")
    }

    /// Takes in a view of the room: a new run, a stopped one, another replay.
    fn on_room(&mut self, view: RoomView) {
        // The run our engine was started for is over (stopped, or a new one
        // began): the engine, if still up, is in a game that is gone.
        if self.launched_run.is_some_and(|run| run != view.run || view.phase != Phase::Running) {
            stop_following(&mut self.positions, &mut self.update.position);
        }
        self.stale_engine = self.launched_run.is_some_and(|run| run != view.run) && view.phase == Phase::Running;
        let key = view.replay.as_ref().map(|r| r.key.clone());
        let info = view.replay.as_ref().map(|r| r.info.clone());
        let me = view.members.iter().find(|m| m.name == self.update.member).map(|m| m.engine_name.clone());
        self.update.room = Some(view);
        // First: another replay closes an engine still open on the one before.
        if key != self.update.replay || self.update.stage == Stage::Connecting {
            self.follow_replay(key, info);
        }
        // The name the server gives this member for the current replay, unless an
        // engine is in the game under the one it had.
        if let (None, Some(name)) = (&self.child, me) {
            self.update.engine_name = name;
        }
    }

    /// The room's replay is now `key` (or none): whatever was under way for the
    /// one before is dropped, and everything starts over for this one.
    fn follow_replay(&mut self, key: Option<String>, info: Option<ReplayInfo>) {
        if let Some(task) = self.provisioner.take() {
            task.abort();
        }
        self.engine_dir = None;
        // An engine still open on the old replay: its game is gone (the server
        // stops a run before it changes the replay).
        if let Some(child) = self.child.take() {
            let quit = position::quit_file();
            self.closing.retain(|task| !task.is_finished());
            self.closing.push(tokio::spawn(async move { close_engine(child, quit.as_deref()).await }));
        }
        stop_following(&mut self.positions, &mut self.update.position);
        self.launched_run = None;
        self.launched_as = None;
        self.stale_engine = false;
        self.update.engine_run = None;
        self.update.replay = key.clone();
        if self.update.timeline.as_ref().is_some_and(|t| t.replay != key) {
            self.update.timeline = None;
        }
        // The statistics of the old replay are no use: asked afresh, the server
        // answers at once for the new one.
        if let Some(task) = self.statistics.take() {
            task.abort();
        }
        let tx = self.tx.clone();
        self.statistics = Some(tokio::spawn(follow_timeline(
            self.api.http.clone(),
            self.api.url("/timeline"),
            key.clone(),
            TimelinePoll::default(),
            move |view| tx.send(Internal::Timeline(view)).is_ok(),
        )));
        match (key, info) {
            (Some(_), Some(info)) => {
                self.update.stage = Stage::Preparing { what: "Getting ready".into(), fraction: None };
                self.downloads += 1;
                self.provisioner = Some(tokio::spawn(provision(
                    self.api.http.clone(),
                    self.settings.data.clone(),
                    self.settings.headless,
                    self.downloads,
                    info,
                    self.tx.clone(),
                )));
            }
            _ => self.update.stage = Stage::NoReplay,
        }
    }

    /// Asks the server for what the user wants, each on its own so that a slow
    /// server holds up nothing else.
    fn command(&mut self, command: Command) {
        let (api, tx) = (self.api.clone(), self.tx.clone());
        match command {
            Command::Start => {
                tokio::spawn(async move {
                    if let Err(e) = api.start().await {
                        let _ = tx.send(Internal::Refused(format!("Not started: {e:#}")));
                    }
                });
            }
            Command::Stop => {
                tokio::spawn(async move {
                    if let Err(e) = api.stop().await {
                        let _ = tx.send(Internal::Refused(format!("Not stopped: {e:#}")));
                    }
                });
            }
            Command::Rejoin => self.launched_run = None,
            Command::Jump(seconds) => {
                tokio::spawn(async move {
                    if let Err(e) = api.control(Control::Skip { seconds, relative: false }).await {
                        let _ = tx.send(Internal::JumpRefused(format!("{e:#}")));
                    }
                });
            }
            Command::Pick(pick) => {
                self.picks += 1;
                self.update.picking = true;
                tokio::spawn(async move {
                    if let Err(e) = api.pick(&pick).await {
                        let _ = tx.send(Internal::Refused(format!("That replay was not taken: {e:#}")));
                    }
                    let _ = tx.send(Internal::Picked);
                });
            }
            Command::Leave => {}
        }
    }

    /// Starts the engine when everything is installed and the room runs a run we
    /// have not been in yet.
    fn launch_if_due(&mut self) {
        let (Some(dir), Some(room), None) = (&self.engine_dir, &self.update.room, &self.child) else { return };
        let due = matches!(self.update.stage, Stage::Ready | Stage::Closed)
            && room.phase == Phase::Running
            && launched_for(room, self.update.replay.as_deref())
            && self.launched_run != Some(room.run);
        if !due {
            return;
        }
        let run = room.run;
        self.launched_run = Some(run);
        let name = self.update.engine_name.clone();
        match self.launch(dir, room, &name) {
            Ok(c) => {
                self.child = Some(c);
                self.launched_as = Some(name);
                self.update.stage = Stage::Playing;
                self.update.engine_run = Some(run);
                stop_following(&mut self.positions, &mut self.update.position);
                self.launches += 1;
                let (launch, tx) = (self.launches, self.tx.clone());
                let log = self.engine_log();
                self.positions = Some(tokio::spawn(position::follow(log, move |position| {
                    let _ = tx.send(Internal::Position { launch, position });
                })));
            }
            Err(e) => self.update.stage = Stage::Failed(format!("{e:#}")),
        }
    }

    /// Tells the server where this viewer stands, if that changed or a heartbeat is due.
    async fn report(&mut self) {
        let (state, detail) = report_of(&self.update.stage);
        let report: Report = (state, detail, self.update.replay.clone());
        if self.reported.as_ref() == Some(&report) {
            return;
        }
        let body = ViewerUpdate { state: report.0, detail: report.1.clone(), replay: report.2.clone() };
        let resp = self
            .api
            .http
            .put(self.api.url(&format!("/viewers/{}", self.viewer_id)))
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        match resp.map(|r| r.status()) {
            // The server forgot us (it restarted, or we were silent too long).
            Ok(StatusCode::GONE) => {
                if let Ok(again) = self.register(self.launched_as.clone()).await {
                    self.viewer_id = again.viewer_id;
                    self.update.member = again.name;
                    self.on_room(again.room);
                }
            }
            // The room is gone: its follower says so too.
            Ok(StatusCode::NOT_FOUND) => {}
            Ok(_) => self.reported = Some(report),
            Err(_) => {} // unreachable server: the next heartbeat tries again
        }
    }

    /// Where the engine's output goes.
    fn engine_log(&self) -> PathBuf {
        self.settings.data.root.join("barwatch_engine.log")
    }

    /// Starts the engine as a spectator of the running room.
    fn launch(&self, engine_dir: &Path, room: &RoomView, name: &str) -> Result<tokio::process::Child> {
        let port = room.game_port.context("the server did not say which port to connect to")?;
        let host = match &room.game_host {
            Some(h) => h.clone(),
            None => self.api.server.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string(),
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
        // killed while closing it) would end this one once it has loaded. A
        // fresh one belongs to the engine of a room just left or a replay just
        // changed, closing now: `close_engine` takes it away within seconds, long
        // before this engine has loaded, and taken away now that engine would be killed.
        if let Some(quit) = position::quit_file() {
            let fresh = std::fs::metadata(&quit)
                .and_then(|m| m.modified())
                .is_ok_and(|at| at.elapsed().is_ok_and(|age| age < QUIT_FILE_FRESH));
            if !fresh {
                match std::fs::remove_file(&quit) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => bail!("removing {}: {e}", quit.display()),
                    _ => {}
                }
            }
        }
        if let Some(program) = stand_in_engine() {
            return tokio::process::Command::new(&program)
                .arg(self.api.url("").as_str())
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
        // A group of its own: a Ctrl+C in the terminal the app was started
        // from reaches the app, which closes the engine properly, and not the
        // engine, which (headless) would die without leaving the game.
        #[cfg(unix)]
        command.process_group(0);
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

    /// Leaves: the engine closed properly, the server told.
    async fn leave(mut self) {
        for task in [self.provisioner.take(), self.statistics.take()].into_iter().flatten() {
            task.abort();
        }
        stop_following(&mut self.positions, &mut self.update.position);
        if let Some(c) = self.child.take() {
            close_engine(c, position::quit_file().as_deref()).await;
        }
        // Engines of a replay the room left, still closing.
        for task in self.closing.drain(..) {
            let _ = task.await;
        }
        if !self.viewer_id.is_empty() {
            let url = self.api.url(&format!("/viewers/{}", self.viewer_id));
            let _ = self.api.http.delete(url).timeout(Duration::from_secs(5)).send().await;
        }
    }
}

/// Whether the room's run is of the replay this client installed for. Always
/// so in practice (the server stops a run before it changes the replay, and the
/// client starts over when it sees the change); checked so that an engine is never
/// started with what another replay needs.
fn launched_for(room: &RoomView, replay: Option<&str>) -> bool {
    room.replay.as_ref().map(|r| r.key.as_str()) == replay
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
fn stop_following(follower: &mut Option<JoinHandle<()>>, position: &mut Option<Position>) {
    if let Some(task) = follower.take() {
        task.abort();
    }
    *position = None;
}

fn report_of(stage: &Stage) -> (ViewerState, String) {
    match stage {
        Stage::Connecting => (ViewerState::Preparing, String::new()),
        // Nothing to get ready for.
        Stage::NoReplay => (ViewerState::Ready, String::new()),
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
        Stage::Gone => (ViewerState::Error, ROOM_GONE.into()),
    }
}

/// DEVELOPMENT, debug builds only: a program to run in place of the game, named by
/// `BARWATCH_STAND_IN_ENGINE`. Nothing is installed for it, and it is started with
/// the room's address as its one argument; whatever it writes to standard output is
/// read as the engine's would be, so it can report a game position (`position.rs`)
/// without a game. For driving the window in tests without an engine.
fn stand_in_engine() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os("BARWATCH_STAND_IN_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from)
    } else {
        None
    }
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

/// [`read`] for a room's calls, where a 404 means the room is gone ([`RoomGone`]).
async fn read_room<T: serde::de::DeserializeOwned>(resp: reqwest::Result<reqwest::Response>) -> Result<T> {
    match resp {
        Ok(r) if r.status() == StatusCode::NOT_FOUND => Err(RoomGone.into()),
        other => read(other).await,
    }
}

/// Long-polls the room and forwards every change.
async fn follow(api: RoomApi, mut rev: u64, tx: mpsc::UnboundedSender<Internal>) {
    loop {
        let resp = api
            .http
            .get(api.url(""))
            .query(&[("rev", rev.to_string()), ("wait", "25".to_string())])
            .timeout(Duration::from_secs(40))
            .send()
            .await;
        match resp {
            Ok(r) if r.status() == StatusCode::NOT_FOUND => {
                let _ = tx.send(Internal::Gone);
                return;
            }
            Ok(r) if r.status().is_success() => match r.json::<RoomView>().await {
                Ok(view) => {
                    rev = view.rev;
                    if tx.send(Internal::Room(Box::new(view))).is_err() {
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
    /// Pause after an answer this app cannot read (a newer server): it does not
    /// change soon.
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

fn unavailable(rev: u64, note: &str, replay: Option<String>) -> TimelineView {
    TimelineView { rev, state: TimelineState::Unavailable, note: Some(note.into()), doc: None, replay }
}

/// One answer of the timeline call.
enum Answer {
    View(TimelineView),
    /// 404: the room is gone, which the room's follower finds out.
    Absent,
    /// An answer this app cannot read, with the server's count and the replay it
    /// is about when that much of it could be read, and why in words for the viewer.
    Unreadable(Option<(u64, Option<String>)>, &'static str),
}

/// Long-polls the room's statistics and hands on every change, until `deliver`
/// says nobody listens any more. `about` is the replay the room had when this
/// began: what is said of an answer that could not be read is about it. A change is a new count or another replay (each
/// replay has a count of its own, so the two together say what was seen). The
/// server's bytes are taken for no more than what the types make of them, and no
/// more than `max_bytes` of them are read.
async fn follow_timeline(
    http: reqwest::Client,
    url: Url,
    about: Option<String>,
    poll: TimelinePoll,
    deliver: impl Fn(Arc<TimelineView>) -> bool,
) {
    // What was delivered last: the server's count (its own, which starts over
    // when the server does) and the replay it was about.
    let mut seen: Option<(u64, Option<String>)> = None;
    // What was said last when there was nothing to show, not to say it twice.
    let mut said: Option<&'static str> = None;
    loop {
        let mut request = http.get(url.clone()).timeout(Duration::from_secs(u64::from(poll.wait_s) + 15));
        if let Some((rev, _)) = &seen {
            request = request.query(&[("rev", rev.to_string()), ("wait", poll.wait_s.to_string())]);
        }
        let asked = Instant::now();
        match fetch_timeline(request, poll.max_bytes).await {
            Ok(Answer::View(view)) => {
                said = None;
                let now = (view.rev, view.replay.clone());
                if seen.as_ref() == Some(&now) {
                    // The wait ran out with nothing new. A server that answers
                    // at once instead of waiting must not be asked in a loop.
                    if asked.elapsed() < Duration::from_secs(1) {
                        tokio::time::sleep(poll.retry).await;
                    }
                    continue;
                }
                seen = Some(now);
                let building = view.state == TimelineState::Building;
                if !deliver(Arc::new(view)) {
                    return;
                }
                if building {
                    tokio::time::sleep(poll.building_gap.saturating_sub(asked.elapsed())).await;
                }
            }
            // The room is gone (its follower ends the client), or a passing fault:
            // keep what was shown and ask again later.
            Ok(Answer::Absent) => tokio::time::sleep(poll.retry * 10).await,
            Ok(Answer::Unreadable(at, why)) => {
                let replay = at.as_ref().and_then(|(_, r)| r.clone()).or_else(|| about.clone());
                if said != Some(why) && !deliver(Arc::new(unavailable(at.as_ref().map_or(0, |a| a.0), why, replay))) {
                    return;
                }
                said = Some(why);
                // Asked again from where it was, it would answer the same at once.
                seen = at.or(seen);
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
/// or a kind of series this app does not know; then only the count and the
/// replay are read, so that the same answer is not fetched over and over.
fn read_timeline(body: &[u8]) -> Answer {
    if let Ok(view) = serde_json::from_slice::<TimelineView>(body) {
        return Answer::View(view);
    }
    #[derive(serde::Deserialize)]
    struct Count {
        rev: u64,
        #[serde(default)]
        replay: Option<String>,
    }
    Answer::Unreadable(serde_json::from_slice::<Count>(body).ok().map(|c| (c.rev, c.replay)), UNREADABLE)
}

/// Installs the engine, game version and map `replay` needs; the `download`-th
/// download of this client.
async fn provision(
    http: reqwest::Client,
    data: DataDir,
    headless: bool,
    download: u64,
    replay: ReplayInfo,
    tx: mpsc::UnboundedSender<Internal>,
) {
    let progress = |what: String, fraction: Option<f32>| {
        let _ = tx.send(Internal::Progress { download, what, fraction });
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
    let _ = tx.send(Internal::Provisioned { download, result: result.map_err(|e| format!("{e:#}")) });
}

/// "Beyond All Reason test-31516-95f3e26" → "test-31516-95f3e26".
fn short_game(game: &str) -> &str {
    game.strip_prefix("Beyond All Reason ").unwrap_or(game)
}

/// The calls of a paired app: they carry its pairing token.
#[derive(Clone)]
pub struct Paired {
    pub http: reqwest::Client,
    pub server: Url,
    pub token: String,
}

impl Paired {
    /// The server's rooms, most recently used first.
    pub async fn rooms(&self) -> Result<Vec<RoomView>> {
        let resp = self.http.get(api::app_url(&self.server, "rooms")?).bearer_auth(&self.token).timeout(Duration::from_secs(20));
        read(resp.send().await).await.context("listing the server's rooms")
    }

    pub async fn create_room(&self, name: &str) -> Result<RoomView> {
        let resp = self
            .http
            .post(api::app_url(&self.server, "rooms")?)
            .bearer_auth(&self.token)
            .json(&NewRoom { name: name.trim().to_owned() })
            .timeout(Duration::from_secs(20));
        let room: RoomView = read(resp.send().await).await.context("making a room")?;
        ensure!(names::valid_id(&room.id), "the server answered with a malformed room id");
        Ok(room)
    }

    /// Sends a replay to the server's archive (bar-replays) without watching it.
    pub async fn archive(&self, replay: &Path) -> Result<Archived> {
        let bytes = tokio::fs::read(replay).await.with_context(|| format!("reading {}", replay.display()))?;
        let resp = self
            .http
            .post(api::app_url(&self.server, "archive")?)
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .timeout(Duration::from_secs(300));
        read(resp.send().await).await.context("sending the replay to the archive")
    }
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
            Huge {
                len: usize,
                chunked: bool,
            },
            /// A JSON body as it is.
            Raw(&'static str),
        }

        fn view(rev: u64, state: TimelineState) -> Reply {
            of_replay(rev, state, "k1")
        }

        fn of_replay(rev: u64, state: TimelineState, replay: &str) -> Reply {
            Reply::View(TimelineView { rev, state, note: None, doc: None, replay: Some(replay.into()) })
        }

        /// Asked paths (with their queries) and when, in order.
        type Asked = Arc<Mutex<Vec<(String, Instant)>>>;

        /// Serves `replies` in order; after the last it holds every request
        /// open without answering.
        async fn serve(replies: Vec<Reply>) -> (Url, Asked) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = Url::parse(&format!("http://{}/app/v1/rooms/abc/timeline", listener.local_addr().unwrap())).unwrap();
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
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                                .into_bytes()
                        }
                        Reply::Status(code) => {
                            format!("HTTP/1.1 {code} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
                        }
                        Reply::Huge { len, chunked: false } => {
                            let mut r =
                                format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n").into_bytes();
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
            let follow = follow_timeline(reqwest::Client::new(), url, Some("k1".into()), poll, move |v| {
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
            assert_eq!(paths[0], "/app/v1/rooms/abc/timeline", "the first answer comes at once");
            assert_eq!(paths[1], "/app/v1/rooms/abc/timeline?rev=1&wait=1");
            assert_eq!(paths[3], "/app/v1/rooms/abc/timeline?rev=2&wait=1");
        }

        /// The room picked another replay: its view is new even with the count
        /// the old one had, and waiting goes on from it.
        #[tokio::test]
        async fn another_replay_is_a_change_whatever_its_count() {
            let (url, asked) = serve(vec![
                of_replay(3, TimelineState::Complete, "k1"),
                of_replay(3, TimelineState::Queued, "k2"),
                of_replay(3, TimelineState::Queued, "k2"),
                of_replay(4, TimelineState::Building, "k2"),
            ])
            .await;
            let got = follow_for(url, POLL, 3).await;
            let seen: Vec<(u64, Option<String>)> = got.iter().map(|v| (v.rev, v.replay.clone())).collect();
            assert_eq!(seen, [(3, Some("k1".into())), (3, Some("k2".into())), (4, Some("k2".into()))]);
            assert_eq!(paths(&asked)[2], "/app/v1/rooms/abc/timeline?rev=3&wait=1");
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
            assert_eq!(got[0].replay.as_deref(), Some("k1"), "about the replay followed, so the window shows it");
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
            assert_eq!(paths(&asked)[1], "/app/v1/rooms/abc/timeline?rev=9&wait=1");
        }

        /// A 404 is the room going, which its own follower handles: what was
        /// shown stays, and nothing is said of it here.
        #[tokio::test]
        async fn a_404_keeps_what_was_shown() {
            let (url, _) = serve(vec![view(4, TimelineState::Complete), Reply::Status(404)]).await;
            let got = follow_for(url, POLL, 2).await;
            assert_eq!(got.iter().map(|v| v.rev).collect::<Vec<_>>(), [4]);
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
            let follow = follow_timeline(reqwest::Client::new(), url, None, POLL, |_| false);
            assert!(tokio::time::timeout(Duration::from_secs(2), follow).await.is_ok());
        }
    }
}
