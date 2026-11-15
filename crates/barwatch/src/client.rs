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
    self, ApiError, Created, JoinRequest, JoinResponse, Phase, ServerInfo, SessionView, ViewerState, ViewerUpdate, API_VERSION,
};
use barwatch_core::engine::{self, Binary, Os};
use barwatch_core::{names, script};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Ask the server to start the replay for everyone.
    Start,
    /// Start the engine again after it was closed.
    Rejoin,
    /// Leave the party; ends [`run`].
    Leave,
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
}

/// How often the server hears from us; it forgets a viewer after 30 s of silence.
const HEARTBEAT: Duration = Duration::from_secs(10);

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
    let mut update =
        Update { session: None, stage: Stage::Connecting, name: client.settings.name.clone(), notice: None, position: None };
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

    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                Internal::Session(view) => {
                    // The run our engine was started for is over (stopped, or a new
                    // one began): the engine, if still up, is in a game that is gone.
                    if launched_run.is_some_and(|run| run != view.run || view.phase != Phase::Running) {
                        stop_following(&mut positions, &mut update.position);
                    }
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
            },
            command = commands.recv() => match command {
                Some(Command::Start) => {
                    if let Err(e) = client.start().await {
                        update.notice = Some(format!("{e:#}"));
                    }
                }
                Some(Command::Rejoin) => launched_run = None,
                Some(Command::Leave) | None => break,
            },
            _ = heartbeat.tick() => reported = None,
            _ = wait(&mut child), if child.is_some() => {
                child = None;
                update.stage = Stage::Closed;
                stop_following(&mut positions, &mut update.position);
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
    }

    follower.abort();
    provisioner.abort();
    stop_following(&mut positions, &mut update.position);
    if let Some(mut c) = child {
        let _ = c.start_kill();
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
        let binary = which.find(engine_dir).with_context(|| format!("no engine executable in {}", engine_dir.display()))?;
        let log_path = self.engine_log();
        let log = std::fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
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
}
