//! The HTTP surface.
//!
//! * `/` and `/api/…`: the web page and its API. No checks of their own: these
//!   belong behind a reverse proxy that authenticates.
//! * `/app/…`: what the desktop app and the public join page use, meant to be let
//!   through the proxy without a login. A session id is the capability to see, join
//!   and control that session; creating one needs a pairing token.

use crate::runner::{self, RunnerCmd};
use crate::source;
use crate::state::{App, Session};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use barwatch_core::api::{
    ApiError, Control, Created, JoinRequest, JoinResponse, Phase, ServerInfo, SessionView, ViewerUpdate, API_VERSION,
};
use barwatch_core::names;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

type Shared = State<Arc<App>>;

pub struct ApiErr(StatusCode, String);

impl ApiErr {
    fn not_found() -> ApiErr {
        ApiErr(StatusCode::NOT_FOUND, "no such session".into())
    }

    fn bad(e: impl std::fmt::Display) -> ApiErr {
        ApiErr(StatusCode::BAD_REQUEST, e.to_string())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

type ApiResult<T> = Result<T, ApiErr>;

/// Body limit of every route except the two that take a replay.
const SMALL_BODY: usize = 16 * 1024;

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
    let web_api = Router::new()
        .route("/info", get(info))
        .route("/sessions", get(list_sessions).post(create_session).layer(DefaultBodyLimit::max(app.cfg.max_upload)))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/control", post(control_session))
        .route("/pair", post(pair))
        .route("/tokens", get(list_tokens))
        .route("/tokens/{id}", delete(revoke_token));
    let app_api = Router::new()
        .route("/info", get(info))
        .route("/sessions", post(create_session_with_token).layer(DefaultBodyLimit::max(app.cfg.max_upload)))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/timeline", get(get_timeline))
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/control", post(control_session))
        .route("/sessions/{id}/viewers", post(join_session))
        .route("/sessions/{id}/viewers/{viewer}", put(report_viewer).delete(leave_session));
    Router::new()
        .route("/", get(index_page))
        .route("/app/join/{id}", get(join_page))
        .nest("/api", web_api)
        .nest(barwatch_core::api::APP_PREFIX, app_api)
        // Everything but a replay upload is a few hundred bytes of JSON.
        .layer(DefaultBodyLimit::max(SMALL_BODY))
        .with_state(app)
}

fn find(app: &App, id: &str) -> ApiResult<Arc<Session>> {
    app.session(id).ok_or_else(ApiErr::not_found)
}

fn view(app: &App, session: &Session) -> SessionView {
    session.view(app.cfg.game_host.as_deref())
}

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo { version: env!("CARGO_PKG_VERSION").into(), api: API_VERSION })
}

async fn list_sessions(State(app): Shared) -> Json<Vec<SessionView>> {
    Json(app.sessions().iter().map(|s| view(&app, s)).collect())
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Answer once the session's `rev` exceeds this…
    rev: Option<u64>,
    /// …or after this many seconds.
    wait: Option<u64>,
}

async fn get_session(State(app): Shared, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Json<SessionView>> {
    let session = find(&app, &id)?;
    if let Some(rev) = q.rev {
        session.changed_since(rev, Duration::from_secs(q.wait.unwrap_or(25).min(55))).await;
    }
    Ok(Json(view(&app, &session)))
}

/// `GET /app/v1/sessions/{id}/timeline[?rev=&wait=]`: `api::TimelineView`,
/// long-polling as the session call does. Asking is demand: it keeps the session's
/// statistics followed (`timeline.rs`).
async fn get_timeline(State(app): Shared, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> ApiResult<Response> {
    let session = find(&app, &id)?;
    crate::timeline::ensure(&app, &session, true, false);
    if let Some(rev) = q.rev {
        session.stats.changed_since(rev, Duration::from_secs(q.wait.unwrap_or(25).min(55))).await;
    }
    let body = session.stats.view_json(app.stats.client.is_some()).await;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

#[derive(Deserialize)]
struct FromBar {
    /// A replay id or a link to the replay on BAR's site.
    bar_replay: String,
}

/// Body: the replay file itself (name in `x-barwatch-filename`, percent-encoded), or
/// JSON `{"bar_replay": "<id or link>"}` to fetch one from BAR's archive.
async fn create_session(State(app): Shared, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Created>> {
    let is_json =
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    let (name, bytes, bar_replay) = if is_json {
        let req: FromBar = serde_json::from_slice(&body).map_err(ApiErr::bad)?;
        let id = source::replay_id(&req.bar_replay).ok_or_else(|| ApiErr::bad("that is not a BAR replay link or id"))?;
        // Through the stats service's mirror when there is one (DESIGN §4.2); BAR's
        // store directly when there is not or it fails.
        let via_service = match &app.stats.client {
            Some(client) => match source::through_service(client, id, app.cfg.max_upload).await {
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
            None => source::fetch(&app.http, &app.cfg.archive, id, app.cfg.max_upload)
                .await
                .map_err(|e| ApiErr::bad(format!("{e:#}")))?,
        };
        (name, Bytes::from(bytes), Some(id.to_ascii_lowercase()))
    } else {
        let name = headers
            .get("x-barwatch-filename")
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                url::form_urlencoded::parse(format!("n={v}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
            })
            .unwrap_or_default();
        (name, body, None)
    };
    if bytes.is_empty() {
        return Err(ApiErr::bad("no replay was sent"));
    }
    let session = app.create_session(&name, &bytes, bar_replay.as_deref()).map_err(|e| ApiErr::bad(format!("{e:#}")))?;
    crate::log(&format!("session {} created from {:?}", session.id, session.read(|st| st.replay.file_name.clone())));
    tokio::spawn(prepare(app.clone(), session.clone()));
    // Hands the game to the stats service in the background (DESIGN §4.1).
    crate::timeline::ensure(&app, &session, false, true);
    Ok(Json(Created { id: session.id.clone() }))
}

async fn create_session_with_token(_: Paired, state: Shared, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Created>> {
    create_session(state, headers, body).await
}

/// Fetches the engine for a new session so that starting it later is instant.
async fn prepare(app: Arc<App>, session: Arc<Session>) {
    let result = runner::ensure_engine(&app, &session).await;
    session.mutate(|st| {
        if st.phase != Phase::Preparing {
            return;
        }
        match result {
            Ok(_) => st.phase = Phase::Idle,
            Err(e) => {
                crate::log(&format!("session {}: engine unavailable: {e:#}", session.id));
                st.phase = Phase::Failed;
                st.note = Some(format!("{e:#}"));
            }
        }
    });
}

async fn start_session(State(app): Shared, Path(id): Path<String>) -> ApiResult<Json<SessionView>> {
    let session = find(&app, &id)?;
    let launch = session.mutate(|st| match st.phase {
        Phase::Starting | Phase::Running => Ok(None),
        Phase::Preparing => Err(ApiErr(StatusCode::CONFLICT, "the server is still fetching the engine for this replay".into())),
        Phase::Idle | Phase::Failed => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            st.phase = Phase::Starting;
            st.note = None;
            st.ended = false;
            st.paused = false;
            st.speed = 1.0;
            // Never a number a previous daemon handed out for this party: an app that
            // was in an earlier run must be able to tell this one apart.
            st.run = (st.run + 1).max(crate::state::unix_now() as u32);
            st.runner = Some(tx);
            st.jump = None;
            Ok(Some(rx))
        }
    })?;
    if let Some(rx) = launch {
        tokio::spawn(runner::run(app.clone(), session.clone(), rx));
    }
    Ok(Json(view(&app, &session)))
}

async fn stop_session(State(app): Shared, Path(id): Path<String>) -> ApiResult<Json<SessionView>> {
    let session = find(&app, &id)?;
    session.read(|st| st.runner.as_ref().map(|tx| tx.send(RunnerCmd::Stop)));
    Ok(Json(view(&app, &session)))
}

async fn control_session(
    State(app): Shared,
    Path(id): Path<String>,
    Json(control): Json<Control>,
) -> ApiResult<Json<SessionView>> {
    let session = find(&app, &id)?;
    match control {
        Control::Speed { speed } if !(crate::chat::MIN_SPEED..=crate::chat::MAX_SPEED).contains(&speed) => {
            return Err(ApiErr::bad("speed out of range"));
        }
        Control::Skip { seconds, .. } if seconds > crate::chat::MAX_SKIP_S => {
            return Err(ApiErr::bad("that is further than any replay goes"));
        }
        _ => {}
    }
    let sent = session.read(|st| match (&st.runner, st.phase) {
        (Some(tx), Phase::Running) => tx.send(RunnerCmd::Control(control)).is_ok(),
        _ => false,
    });
    if !sent {
        return Err(ApiErr(StatusCode::CONFLICT, "the session is not running".into()));
    }
    Ok(Json(view(&app, &session)))
}

async fn delete_session(State(app): Shared, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let session = find(&app, &id)?;
    session.read(|st| st.runner.as_ref().map(|tx| tx.send(RunnerCmd::Stop)));
    tokio::spawn(async move {
        // Let a running server exit before its replay is deleted from under it.
        for _ in 0..40 {
            if session.read(|st| st.runner.is_none()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        app.remove_session(&session.id);
        crate::log(&format!("session {} deleted", session.id));
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn join_session(State(app): Shared, Path(id): Path<String>, Json(req): Json<JoinRequest>) -> ApiResult<Json<JoinResponse>> {
    let session = find(&app, &id)?;
    let (viewer_id, name) = session
        .join(&req.name, req.playing_as.as_deref())
        .ok_or_else(|| ApiErr(StatusCode::TOO_MANY_REQUESTS, "this watch party is full".into()))?;
    crate::timeline::ensure(&app, &session, false, false);
    Ok(Json(JoinResponse { viewer_id, name, session: view(&app, &session) }))
}

async fn report_viewer(
    State(app): Shared,
    Path((id, viewer)): Path<(String, String)>,
    Json(update): Json<ViewerUpdate>,
) -> ApiResult<StatusCode> {
    let session = find(&app, &id)?;
    if session.report(&viewer, update.state, &update.detail) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // The app registers again when it sees this.
        Err(ApiErr(StatusCode::GONE, "viewer registration expired".into()))
    }
}

async fn leave_session(State(app): Shared, Path((id, viewer)): Path<(String, String)>) -> ApiResult<StatusCode> {
    find(&app, &id)?.leave(&viewer);
    Ok(StatusCode::NO_CONTENT)
}

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
    let token = app.mint_token(&label).map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
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
        Err(e) => Err(ApiErr(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
    }
}

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
