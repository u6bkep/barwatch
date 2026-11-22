//! The HTTP surface (DESIGN §9.6).
//!
//! * `/` and `/api/…`: the web page and its API. No checks of their own: these
//!   belong behind a reverse proxy that authenticates.
//! * `/app/…`: what the desktop app and the public join page use, meant to be let
//!   through the proxy without a login. A room's link id is the capability to see,
//!   join and control that room, and to pick its replay; creating, deleting and
//!   re-linking rooms and sending a replay to the archive need a pairing token.

use crate::runner::{self, RunnerCmd};
use crate::source;
use crate::state::{App, Replay, Room};
use crate::stats_client::{Priority, SvcError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use barwatch_core::api::{
    ApiError, Archived, Control, JoinRequest, JoinResponse, NewRoom, Phase, PickReplay, RoomView, ServerInfo, TimelineState,
    TimelineView, ViewerUpdate, API_VERSION,
};
use barwatch_core::names;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::OwnedMutexGuard;

type Shared = State<Arc<App>>;

pub struct ApiErr(StatusCode, String);

impl ApiErr {
    fn not_found() -> ApiErr {
        ApiErr(StatusCode::NOT_FOUND, "no such room: it was deleted or has a new link".into())
    }

    fn bad(e: impl std::fmt::Display) -> ApiErr {
        ApiErr(StatusCode::BAD_REQUEST, e.to_string())
    }

    fn internal(e: impl std::fmt::Display) -> ApiErr {
        ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

type ApiResult<T> = Result<T, ApiErr>;

/// Body limit of every route except those that take a replay.
const SMALL_BODY: usize = 16 * 1024;
/// How long picking a replay or deleting a room waits for a running server to exit.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// Proof that the request carries a valid pairing token. Extracted before the body,
/// so an upload without one is refused without being read.
struct Paired;

impl axum::extract::FromRequestParts<Arc<App>> for Paired {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, app: &Arc<App>) -> Result<Self, ApiErr> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if app.token_valid(token) {
            Ok(Paired)
        } else {
            Err(ApiErr(StatusCode::UNAUTHORIZED, "this app is not paired with the server (or was unpaired)".into()))
        }
    }
}

pub fn router(app: Arc<App>) -> Router {
    let big = DefaultBodyLimit::max(app.cfg.max_upload);
    let web_api = Router::new()
        .route("/info", get(info))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/{id}", get(get_room).delete(delete_room))
        .route("/rooms/{id}/relink", post(relink_room))
        .route("/rooms/{id}/replay", put(pick_replay).layer(big))
        .route("/rooms/{id}/start", post(start_room))
        .route("/rooms/{id}/stop", post(stop_room))
        .route("/rooms/{id}/control", post(control_room))
        .route("/pair", post(pair))
        .route("/tokens", get(list_tokens))
        .route("/tokens/{id}", delete(revoke_token));
    let app_api = Router::new()
        .route("/info", get(info))
        .route("/rooms", get(list_rooms_paired).post(create_room_paired))
        .route("/rooms/{id}", get(get_room).delete(delete_room_paired))
        .route("/rooms/{id}/relink", post(relink_room_paired))
        .route("/rooms/{id}/timeline", get(get_timeline))
        .route("/rooms/{id}/replay", put(pick_replay).layer(big))
        .route("/rooms/{id}/start", post(start_room))
        .route("/rooms/{id}/stop", post(stop_room))
        .route("/rooms/{id}/control", post(control_room))
        .route("/rooms/{id}/viewers", post(join_room))
        .route("/rooms/{id}/viewers/{viewer}", put(report_viewer).delete(leave_room))
        .route("/archive", post(archive).layer(big));
    Router::new()
        .route("/", get(index_page))
        .route("/app/join/{id}", get(join_page))
        .nest("/api", web_api)
        .nest(barwatch_core::api::APP_PREFIX, app_api)
        // Everything but a replay upload is a few hundred bytes of JSON.
        .layer(DefaultBodyLimit::max(SMALL_BODY))
        .with_state(app)
}

fn find(app: &App, id: &str) -> ApiResult<Arc<Room>> {
    app.room(id).ok_or_else(ApiErr::not_found)
}

fn view(app: &App, room: &Room) -> RoomView {
    room.view(app)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"))
}

/// The upload's file name, from `x-barwatch-filename` (percent-encoded).
fn file_name(headers: &HeaderMap) -> String {
    headers
        .get("x-barwatch-filename")
        .and_then(|v| v.to_str().ok())
        .map(|v| url::form_urlencoded::parse(format!("n={v}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default())
        .unwrap_or_default()
}

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo { version: env!("CARGO_PKG_VERSION").into(), api: API_VERSION })
}

// -- rooms -----------------------------------------------------------------------

async fn list_rooms(State(app): Shared) -> Json<Vec<RoomView>> {
    Json(app.rooms().iter().map(|r| view(&app, r)).collect())
}

async fn list_rooms_paired(_: Paired, state: Shared) -> Json<Vec<RoomView>> {
    list_rooms(state).await
}

/// Body: `{"name": …}` (or nothing, for a room called "Room").
async fn create_room(State(app): Shared, body: Bytes) -> ApiResult<Json<RoomView>> {
    let req: NewRoom =
        if body.is_empty() { NewRoom { name: String::new() } } else { serde_json::from_slice(&body).map_err(ApiErr::bad)? };
    let room = app.create_room(&req.name).map_err(|e| ApiErr::internal(format!("{e:#}")))?;
    crate::log(&format!("room {} created", room.uid));
    Ok(Json(view(&app, &room)))
}

async fn create_room_paired(_: Paired, state: Shared, body: Bytes) -> ApiResult<Json<RoomView>> {
    create_room(state, body).await
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Answer once the `rev` differs from this…
    rev: Option<u64>,
    /// …or after this many seconds.
    wait: Option<u64>,
}

impl WaitQuery {
    fn wait(&self) -> Duration {
        Duration::from_secs(self.wait.unwrap_or(25).min(55))
    }
}

async fn get_room(State(app): Shared, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    if let Some(rev) = q.rev {
        room.changed_since(rev, q.wait()).await;
    }
    // Deleted or re-linked while the call waited.
    find(&app, &id)?;
    Ok(Json(view(&app, &room)))
}

/// Asks the room's server to stop and waits up to `limit` for it to exit; `false`
/// if it is still running.
async fn stop_and_wait(room: &Room, limit: Duration) -> bool {
    room.read(|st| st.runner.as_ref().map(|tx| tx.send(RunnerCmd::Stop)));
    let until = Instant::now() + limit;
    while room.read(|st| st.runner.is_some()) {
        if Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

async fn delete_room(State(app): Shared, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let room = find(&app, &id)?;
    app.close_room(&room);
    crate::log(&format!("room {} deleted", room.uid));
    tokio::spawn(async move {
        // A pick in flight finishes (and sees the room is gone) first; a running
        // server exits before the room's replays can go from under it.
        let _picking = room.pick_lock.lock().await;
        // Never before the server is gone: forgetting the room releases its replay
        // files, and the server may still be streaming one.
        while !stop_and_wait(&room, STOP_WAIT).await {
            crate::log(&format!("room {}: its server has not stopped yet; still waiting", room.uid));
        }
        app.forget_room(&room);
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_room_paired(_: Paired, state: Shared, id: Path<String>) -> ApiResult<StatusCode> {
    delete_room(state, id).await
}

async fn relink_room(State(app): Shared, Path(id): Path<String>) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    app.relink(&room).map_err(|_| ApiErr::not_found())?;
    crate::log(&format!("room {}: new link", room.uid));
    Ok(Json(view(&app, &room)))
}

async fn relink_room_paired(_: Paired, state: Shared, id: Path<String>) -> ApiResult<Json<RoomView>> {
    relink_room(state, id).await
}

// -- picking a replay --------------------------------------------------------------

/// `PUT rooms/{id}/replay`. Body: the replay file itself (name in
/// `x-barwatch-filename`, percent-encoded), or JSON `api::PickReplay`: a BAR link or
/// id to fetch, or a key from the room's history. Never starts the replay.
async fn pick_replay(State(app): Shared, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    let (replay, held) = if is_json(&headers) {
        let req: PickReplay = serde_json::from_slice(&body).map_err(ApiErr::bad)?;
        match (req.bar_replay, req.key) {
            (Some(link), None) => from_bar(&app, &link).await?,
            (None, Some(key)) => {
                if room.current().is_some_and(|r| r.key == key) {
                    return Ok(Json(view(&app, &room)));
                }
                from_history(&app, &room, &key).await?
            }
            _ => return Err(ApiErr::bad("give either bar_replay or key")),
        }
    } else {
        if body.is_empty() {
            return Err(ApiErr::bad("no replay was sent"));
        }
        app.replay_for(&file_name(&headers), &body, None).await.map_err(|e| ApiErr::bad(format!("{e:#}")))?
    };
    switch(&app, &room, replay, held).await?;
    Ok(Json(view(&app, &room)))
}

/// A replay by BAR link or id: the record this server has, if it has the file;
/// else fetched through the stats service's mirror (DESIGN §4.2), or from BAR's
/// archive directly when there is no service or it fails.
async fn from_bar(app: &Arc<App>, link: &str) -> ApiResult<(Arc<Replay>, OwnedMutexGuard<()>)> {
    let id = source::replay_id(link).ok_or_else(|| ApiErr::bad("that is not a BAR replay link or id"))?.to_ascii_lowercase();
    if let Some(replay) = app.replay(&id) {
        let held = replay.file_lock.clone().lock_owned().await;
        if !replay.is_closed() && replay.has_file() {
            return Ok((replay, held));
        }
    }
    let via_service = match &app.stats.client {
        Some(client) => match source::through_service(client, &id, app.cfg.max_upload).await {
            Ok(got) => {
                crate::log(&format!("replay {id} came through the stats service"));
                Some(got)
            }
            Err(e) => {
                crate::log(&format!("replay {id} through the stats service: {e:#}; downloading it directly"));
                None
            }
        },
        None => None,
    };
    let (name, bytes) = match via_service {
        Some(got) => got,
        None => source::fetch(&app.http, &app.cfg.archive, &id, app.cfg.max_upload)
            .await
            .map_err(|e| ApiErr::bad(format!("{e:#}")))?,
    };
    app.replay_for(&name, &bytes, Some(&id)).await.map_err(|e| ApiErr::bad(format!("{e:#}")))
}

/// A replay from the room's history, its file fetched again from the stats
/// service if the cache let it go (DESIGN §9.5).
async fn from_history(app: &Arc<App>, room: &Room, key: &str) -> ApiResult<(Arc<Replay>, OwnedMutexGuard<()>)> {
    let not_there = || ApiErr::bad("that replay is not in this room's history");
    let replay =
        room.read(|st| st.history.iter().find(|p| p.replay.key == key).map(|p| p.replay.clone())).ok_or_else(not_there)?;
    let held = replay.file_lock.clone().lock_owned().await;
    if replay.is_closed() {
        return Err(not_there());
    }
    if replay.has_file() {
        return Ok((replay, held));
    }
    let (Some(client), Some(game_id)) = (&app.stats.client, replay.game_id()) else {
        return Err(ApiErr(
            StatusCode::CONFLICT,
            "the server no longer keeps this replay's file and cannot fetch it again; pick it from the file or a BAR link".into(),
        ));
    };
    let (_, bytes) = source::through_service(client, &game_id, app.cfg.max_upload)
        .await
        .map_err(|e| ApiErr(StatusCode::BAD_GATEWAY, format!("fetching the replay from the stats service: {e:#}")))?;
    replay.store_file(&bytes, None).map_err(|e| ApiErr::internal(format!("{e:#}")))?;
    crate::log(&format!("replay {key}: file fetched again from the stats service"));
    Ok((replay, held))
}

/// Makes `replay` (whose file lock the caller holds) the room's current one: stops
/// a run first and waits for its server to exit, then sets it, prepares its engine
/// and hands the game to the stats service.
async fn switch(app: &Arc<App>, room: &Arc<Room>, replay: Arc<Replay>, held: OwnedMutexGuard<()>) -> ApiResult<()> {
    let _picking = room.pick_lock.lock().await;
    if room.is_closed() {
        return Err(ApiErr::not_found());
    }
    if room.current().is_some_and(|r| Arc::ptr_eq(&r, &replay)) {
        // Already playing it. If its file had gone missing (the room failed for
        // that), it is back now: prepare again.
        let retry = room.mutate(|st| {
            let missing = st.phase == Phase::Failed && st.runner.is_none() && replay.has_file();
            if missing {
                st.phase = Phase::Preparing;
                st.note = None;
            }
            missing
        });
        if retry {
            tokio::spawn(prepare(app.clone(), room.clone(), replay.clone()));
        }
        return Ok(());
    }
    if !stop_and_wait(room, STOP_WAIT).await {
        return Err(ApiErr(StatusCode::CONFLICT, "the room's server did not stop in time; try again".into()));
    }
    app.set_current(room, replay.clone());
    // The room names it now: the cache keeps it from here on.
    drop(held);
    crate::log(&format!("room {}: playing replay {} ({:?})", room.uid, replay.key, replay.info().file_name));
    tokio::spawn(prepare(app.clone(), room.clone(), replay.clone()));
    // Hands the game to the stats service in the background (DESIGN §4.1).
    crate::timeline::ensure(app, &replay, false, true);
    app.trim_cache();
    Ok(())
}

/// Fetches the engine for a room's new replay so that starting it later is instant.
async fn prepare(app: Arc<App>, room: Arc<Room>, replay: Arc<Replay>) {
    let result = match replay.demo() {
        Some(demo) => runner::ensure_engine(&app, &demo.engine_version).await,
        None => Err(anyhow::anyhow!("the server does not have this replay's file")),
    };
    room.mutate(|st| {
        if st.phase != Phase::Preparing || !st.replay.as_ref().is_some_and(|r| Arc::ptr_eq(r, &replay)) {
            return;
        }
        match result {
            Ok(_) => st.phase = Phase::Idle,
            Err(e) => {
                crate::log(&format!("room {}: engine unavailable: {e:#}", room.uid));
                st.phase = Phase::Failed;
                st.note = Some(format!("{e:#}"));
            }
        }
    });
}

// -- playing ---------------------------------------------------------------------

async fn start_room(State(app): Shared, Path(id): Path<String>) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    // Not while a replay is being picked: the run would be of the old one.
    let _picking = room.pick_lock.lock().await;
    let launch = room.mutate(|st| match (st.phase, st.replay.clone()) {
        (_, _) if st.closed => Err(ApiErr::not_found()),
        (Phase::Starting | Phase::Running, _) => Ok(None),
        (Phase::Empty, _) | (_, None) => Err(ApiErr(StatusCode::CONFLICT, "this room has no replay yet; pick one first".into())),
        (Phase::Preparing, _) => {
            Err(ApiErr(StatusCode::CONFLICT, "the server is still fetching the engine for this replay".into()))
        }
        (Phase::Idle | Phase::Failed, Some(replay)) => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            st.phase = Phase::Starting;
            st.note = None;
            st.ended = false;
            st.paused = false;
            st.speed = 1.0;
            // Never a number a previous daemon handed out for this room: an app that
            // was in an earlier run must be able to tell this one apart.
            st.run = (st.run + 1).max(crate::state::unix_now() as u32);
            st.runner = Some(tx);
            st.jump = None;
            Ok(Some((rx, replay)))
        }
    })?;
    if let Some((rx, replay)) = launch {
        replay.touch_now();
        room.touch_used();
        tokio::spawn(runner::run(app.clone(), room.clone(), replay, rx));
    }
    Ok(Json(view(&app, &room)))
}

async fn stop_room(State(app): Shared, Path(id): Path<String>) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    room.read(|st| st.runner.as_ref().map(|tx| tx.send(RunnerCmd::Stop)));
    Ok(Json(view(&app, &room)))
}

async fn control_room(State(app): Shared, Path(id): Path<String>, Json(control): Json<Control>) -> ApiResult<Json<RoomView>> {
    let room = find(&app, &id)?;
    match control {
        Control::Speed { speed } if !(crate::chat::MIN_SPEED..=crate::chat::MAX_SPEED).contains(&speed) => {
            return Err(ApiErr::bad("speed out of range"));
        }
        Control::Skip { seconds, .. } if seconds > crate::chat::MAX_SKIP_S => {
            return Err(ApiErr::bad("that is further than any replay goes"));
        }
        _ => {}
    }
    let sent = room.read(|st| match (&st.runner, st.phase) {
        (Some(tx), Phase::Running) => tx.send(RunnerCmd::Control(control)).is_ok(),
        _ => false,
    });
    if !sent {
        return Err(ApiErr(StatusCode::CONFLICT, "the room is not running".into()));
    }
    Ok(Json(view(&app, &room)))
}

/// `GET rooms/{id}/timeline[?rev=&wait=]`: the current replay's `api::TimelineView`,
/// long-polling for a change of it or of the room's replay. Asking is demand: it
/// keeps the replay's statistics followed (`timeline.rs`).
async fn get_timeline(State(app): Shared, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Response> {
    let room = find(&app, &id)?;
    let picks = room.picks();
    if let Some(rev) = q.rev {
        match room.current() {
            Some(replay) => {
                crate::timeline::ensure(&app, &replay, true, false);
                tokio::select! {
                    _ = replay.stats.changed_since(rev, q.wait()) => {}
                    _ = room.picked_since(picks, q.wait()) => {}
                }
            }
            None => room.picked_since(picks, q.wait()).await,
        }
    }
    // Whatever the room plays now, which may not be what it played when asked.
    let body = match room.current() {
        Some(replay) => {
            crate::timeline::ensure(&app, &replay, true, false);
            replay.stats.view_json(app.stats.client.is_some()).await
        }
        None => {
            let none = TimelineView {
                rev: 0,
                state: TimelineState::Unavailable,
                note: Some("This room has no replay yet.".into()),
                doc: None,
                replay: None,
            };
            Bytes::from(serde_json::to_vec(&none).expect("a timeline view serializes"))
        }
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

// -- members ---------------------------------------------------------------------

async fn join_room(State(app): Shared, Path(id): Path<String>, Json(req): Json<JoinRequest>) -> ApiResult<Json<JoinResponse>> {
    let room = find(&app, &id)?;
    let (viewer_id, name) = room
        .join(&req.name, req.playing_as.as_deref())
        .ok_or_else(|| ApiErr(StatusCode::TOO_MANY_REQUESTS, "this room is full".into()))?;
    if let Some(replay) = room.current() {
        crate::timeline::ensure(&app, &replay, false, false);
    }
    Ok(Json(JoinResponse { viewer_id, name, room: view(&app, &room) }))
}

async fn report_viewer(
    State(app): Shared,
    Path((id, viewer)): Path<(String, String)>,
    Json(update): Json<ViewerUpdate>,
) -> ApiResult<StatusCode> {
    let room = find(&app, &id)?;
    if room.report(&viewer, update.state, &update.detail, update.replay.as_deref()) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // The app registers again when it sees this.
        Err(ApiErr(StatusCode::GONE, "viewer registration expired".into()))
    }
}

async fn leave_room(State(app): Shared, Path((id, viewer)): Path<(String, String)>) -> ApiResult<StatusCode> {
    find(&app, &id)?.leave(&viewer);
    Ok(StatusCode::NO_CONTENT)
}

// -- the archive -----------------------------------------------------------------

/// `POST archive`: the body, a replay file, goes to the stats service at `batch`
/// priority, without watching it. Nothing is kept here.
async fn archive(_: Paired, State(app): Shared, body: Bytes) -> ApiResult<Json<Archived>> {
    let Some(client) = &app.stats.client else {
        return Err(ApiErr(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no replay archive: no statistics service is configured".into(),
        ));
    };
    if body.is_empty() {
        return Err(ApiErr::bad("no replay was sent"));
    }
    barwatch_core::demo::Demo::from_gz_bytes(&body).map_err(|_| ApiErr::bad("this is not a readable replay"))?;
    let id = barwatch_core::demo::game_id_from_gz(&body)
        .ok_or_else(|| ApiErr::bad("this replay carries no game id, so the archive cannot take it"))?;
    let gateway = |words: String| ApiErr(StatusCode::BAD_GATEWAY, words);
    match client.upload(body.to_vec(), Priority::Batch).await {
        Ok(st) if st.game_id == id => {
            crate::log(&format!("replay {id} sent to the archive ({})", st.state));
            Ok(Json(Archived { match_url: app.match_url(&id), game_id: id }))
        }
        Ok(st) => Err(gateway(format!("the archive answered for another game ({})", st.game_id))),
        Err(SvcError::Unauthorized) => Err(gateway("the archive does not accept this server".into())),
        Err(SvcError::Refused(w) | SvcError::NotFound(w)) => {
            Err(ApiErr::bad(format!("the archive would not take this replay: {w}")))
        }
        Err(e) => Err(gateway(e.to_string())),
    }
}

// -- pairing ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct PairRequest {
    #[serde(default)]
    label: String,
}

#[derive(Serialize)]
struct PairResponse {
    id: String,
    token: String,
}

async fn pair(State(app): Shared, headers: HeaderMap, body: Bytes) -> ApiResult<Json<PairResponse>> {
    let req: PairRequest =
        if body.is_empty() { PairRequest::default() } else { serde_json::from_slice(&body).map_err(ApiErr::bad)? };
    // A proxy that authenticates usually names the user; it makes a better label.
    let user = ["x-authentik-username", "x-forwarded-user", "remote-user"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()));
    let label = match (req.label.trim(), user) {
        ("", Some(user)) => format!("{user}'s desktop app"),
        ("", None) => "desktop app".to_string(),
        (label, Some(user)) => format!("{label} ({user})"),
        (label, None) => label.to_string(),
    };
    let token = app.mint_token(&label).map_err(|e| ApiErr::internal(format!("{e:#}")))?;
    crate::log(&format!("paired: {label:?} ({})", token.id));
    Ok(Json(PairResponse { id: token.id, token: token.secret }))
}

#[derive(Serialize)]
struct TokenView {
    id: String,
    label: String,
    created_unix: u64,
}

async fn list_tokens(State(app): Shared) -> Json<Vec<TokenView>> {
    Json(app.tokens().into_iter().map(|t| TokenView { id: t.id, label: t.label, created_unix: t.created_unix }).collect())
}

async fn revoke_token(State(app): Shared, Path(id): Path<String>) -> ApiResult<StatusCode> {
    match app.revoke_token(&id) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(ApiErr(StatusCode::NOT_FOUND, "no such token".into())),
        Err(e) => Err(ApiErr::internal(format!("{e:#}"))),
    }
}

// -- pages -----------------------------------------------------------------------

const STYLE: &str = include_str!("web/style.css");
const COMMON_JS: &str = include_str!("web/common.js");

fn page(template: &str, script: &str, app: &App) -> Html<String> {
    Html(
        template
            .replace("{{STYLE}}", STYLE)
            .replace("{{SCRIPT}}", &format!("{COMMON_JS}\n{script}"))
            .replace("{{DOWNLOAD_URL}}", &html_escape(&app.cfg.download_url))
            .replace("{{VERSION}}", env!("CARGO_PKG_VERSION")),
    )
}

async fn index_page(State(app): Shared) -> Html<String> {
    page(include_str!("web/index.html"), include_str!("web/index.js"), &app)
}

async fn join_page(State(app): Shared, Path(id): Path<String>) -> Response {
    if !names::valid_id(&id) {
        return ApiErr::not_found().into_response();
    }
    page(include_str!("web/join.html"), include_str!("web/join.js"), &app).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
