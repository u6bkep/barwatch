//! The fake stats service itself (see `main.rs` for the command line). Also
//! compiled into barwatchd's tests, so it uses nothing but external crates.
//!
//! It knows one game with output: the configured raw output file, released as a
//! simulation at a chosen pace would produce it. Any other uploaded replay is
//! accepted and stays queued forever with an empty stream.

// Parts are used only by barwatchd's tests, others only by the command.
#![allow(dead_code)]

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// How the fake behaves. See `main.rs` for what each switch means.
#[derive(Clone, Debug)]
pub struct Options {
    pub token: String,
    /// The game's raw output, one record per entry, without line ends.
    pub lines: Vec<Vec<u8>>,
    pub game_id: String,
    pub replay: Option<Vec<u8>>,
    pub replay_name: String,
    /// Game seconds simulated per wall-clock second; `None` for all at once.
    pub rate: Option<f64>,
    /// Seconds the game stays queued after it is registered.
    pub queued_s: f64,
    /// The game is known (and its clock running) from launch, not only after a
    /// `POST /api/v1/games` or upload.
    pub known_at_launch: bool,
    /// `POST /api/v1/games {game_id}` answers 404, as for a game the service cannot
    /// find in BAR's store.
    pub no_resolve: bool,
    /// The simulation fails on reaching this frame.
    pub fail_at_frame: Option<u32>,
    /// The record stream answers 404 "no such API call", as bar-replays before §9.8.
    pub no_records: bool,
    /// Every records response is cut after this many bytes of its body.
    pub drop_after_bytes: Option<usize>,
    /// The cut ends the response properly instead of breaking the connection.
    pub drop_clean: bool,
    /// Only this many responses are cut (`None`: all).
    pub drop_times: Option<u32>,
    /// Every records response stops sending after this many bytes of its body…
    pub stall_after_bytes: Option<usize>,
    /// …for this long (`None`: forever).
    pub stall_s: Option<f64>,
    /// Only this many responses stall (`None`: all).
    pub stall_times: Option<u32>,
    /// Every K-th API call answers 503.
    pub error_every: Option<u64>,
    /// These calls always answer 500: `submit`, `upload`, `status`, `replay`, `records`.
    pub fail_calls: HashSet<String>,
    /// A garbage line after every M records sent.
    pub garbage_every: Option<usize>,
    /// How often a following stream looks for new lines.
    pub poll: Duration,
    /// A following stream sends a `_cursor` at least this often.
    pub cursor_every: Duration,
}

impl Options {
    pub fn new(token: &str, lines: Vec<Vec<u8>>, game_id: &str) -> Options {
        Options {
            token: token.to_owned(),
            lines,
            game_id: game_id.to_ascii_lowercase(),
            replay: None,
            replay_name: format!("{game_id}.sdfz"),
            rate: None,
            queued_s: 0.0,
            known_at_launch: false,
            no_resolve: false,
            fail_at_frame: None,
            no_records: false,
            drop_after_bytes: None,
            drop_clean: false,
            drop_times: None,
            stall_after_bytes: None,
            stall_s: None,
            stall_times: None,
            error_every: None,
            fail_calls: HashSet::new(),
            garbage_every: None,
            poll: Duration::from_millis(200),
            cursor_every: Duration::from_secs(2),
        }
    }
}

/// Splits raw output into lines, dropping empty ones.
pub fn split_lines(raw: &[u8]) -> Vec<Vec<u8>> {
    raw.split(|&b| b == b'\n').filter(|l| !l.trim_ascii().is_empty()).map(<[u8]>::to_vec).collect()
}

/// A game id from a `.sdfz` header.
pub fn header_game_id(sdfz: &[u8]) -> Option<String> {
    barwatch_core::demo::game_id_from_gz(sdfz)
}

struct Line {
    bytes: Vec<u8>,
    action: String,
    /// The highest frame of any record up to this one: when the simulation wrote it.
    written: u32,
}

#[derive(Default)]
struct Sim {
    /// When the game was registered (its clock started).
    since: Option<Instant>,
    /// Registered games without output.
    others: HashSet<String>,
    /// Number of registrations per game, for 201 vs 200.
    seen: HashMap<String, u32>,
}

pub struct Fake {
    opts: Options,
    lines: Vec<Line>,
    /// The last record's frame: where the simulation ends.
    end_frame: u32,
    sim: Mutex<Sim>,
    calls: AtomicU64,
    /// Counts of calls by kind, for tests.
    counts: Mutex<HashMap<&'static str, u64>>,
    /// Bumped to break every open stream (a service going away).
    epoch: watch::Sender<u64>,
    /// Cuts and stalls still to make, when limited.
    drops_left: Mutex<(Option<u32>, Option<u32>)>,
    /// The `priority` of every registration, in order (`-` when none was given).
    priorities: Mutex<Vec<String>>,
}

/// What the simulation stands at.
struct Now {
    state: &'static str,
    frame: u32,
    /// Lines released so far.
    available: usize,
    complete: bool,
    error: Option<String>,
}

impl Fake {
    pub fn new(opts: Options) -> Arc<Fake> {
        let mut max = 0u32;
        let lines = opts
            .lines
            .iter()
            .map(|l| {
                let v: Option<Value> = serde_json::from_slice(l).ok();
                let frame = v.as_ref().and_then(|v| v.get("frame")).and_then(Value::as_u64).unwrap_or(0) as u32;
                max = max.max(frame);
                let action = v.as_ref().and_then(|v| v.get("action")).and_then(Value::as_str).unwrap_or("").to_owned();
                Line { bytes: l.clone(), action, written: max }
            })
            .collect();
        let mut sim = Sim::default();
        if opts.known_at_launch {
            sim.since = Some(Instant::now());
        }
        Arc::new(Fake {
            lines,
            end_frame: max,
            sim: Mutex::new(sim),
            calls: AtomicU64::new(0),
            counts: Mutex::new(HashMap::new()),
            drops_left: Mutex::new((opts.drop_times, opts.stall_times)),
            epoch: watch::channel(0).0,
            priorities: Mutex::new(Vec::new()),
            opts,
        })
    }

    /// How many calls of a kind were answered (`submit`, `upload`, `status`,
    /// `replay`, `records`).
    pub fn count(&self, kind: &str) -> u64 {
        self.counts.lock().unwrap().get(kind).copied().unwrap_or(0)
    }

    /// The `priority` of every registration so far (`-` when none was given).
    pub fn priorities(&self) -> Vec<String> {
        self.priorities.lock().unwrap().clone()
    }

    /// Checks and records a registration's priority, as the real service does: the
    /// answer to send if it is not one.
    fn refuse_priority(&self, p: Option<&str>) -> Option<Response> {
        match p {
            None | Some("batch" | "interactive") => {
                self.priorities.lock().unwrap().push(p.unwrap_or("-").to_owned());
                None
            }
            Some(other) => Some(err(StatusCode::BAD_REQUEST, &format!("priority must be interactive or batch, not {other:?}"))),
        }
    }

    /// Whether the game was registered.
    pub fn registered(&self) -> bool {
        self.sim.lock().unwrap().since.is_some()
    }

    /// Breaks every open record stream.
    pub fn break_streams(&self) {
        self.epoch.send_modify(|e| *e += 1);
    }

    /// Forgets every registration, as a service whose store was reset.
    pub fn forget(&self) {
        *self.sim.lock().unwrap() = Sim::default();
        self.break_streams();
    }

    fn now(&self) -> Option<Now> {
        let since = self.sim.lock().unwrap().since?;
        let e = since.elapsed().as_secs_f64();
        if e < self.opts.queued_s {
            let state = if e < self.opts.queued_s / 2.0 { "fetched" } else { "provisioned" };
            return Some(Now { state, frame: 0, available: 0, complete: false, error: None });
        }
        let sim_frame = match self.opts.rate {
            Some(r) => ((e - self.opts.queued_s) * r * 30.0).min(u32::MAX as f64) as u32,
            None => u32::MAX,
        };
        let released = |frame: u32| self.lines.partition_point(|l| l.written <= frame);
        if let Some(fail) = self.opts.fail_at_frame.filter(|f| sim_frame >= *f) {
            return Some(Now {
                state: "failed",
                frame: fail,
                available: released(fail),
                complete: false,
                error: Some("simulation: the engine crashed (fake)".into()),
            });
        }
        if sim_frame >= self.end_frame {
            return Some(Now { state: "ingested", frame: self.end_frame, available: self.lines.len(), complete: true, error: None });
        }
        Some(Now { state: "simulating", frame: sim_frame, available: released(sim_frame), complete: false, error: None })
    }

    fn status_json(&self, id: &str) -> Option<Value> {
        let (state, frame, error) = if id == self.opts.game_id {
            let n = self.now()?;
            (n.state, n.frame, n.error)
        } else if self.sim.lock().unwrap().others.contains(id) {
            ("fetched", 0, None)
        } else {
            return None;
        };
        let source = if self.opts.known_at_launch { "bucket" } else { "upload" };
        let mut v = json!({
            "game_id": id,
            "state": state,
            "source": source,
            "replay_file_name": format!("upload/{id}.sdfz"),
            "progress": {"frame": frame, "of_frames": self.end_frame},
            "error": error,
        });
        // Like the real one: changes whenever anything above does (progress in
        // steps of five game seconds).
        let quantized = json!([state, frame / 150, &v["error"]]).to_string();
        let rev = quantized.bytes().fold(1469598103934665603u64, |h, b| (h ^ u64::from(b)).wrapping_mul(1099511628211)) >> 16;
        v["rev"] = rev.into();
        Some(v)
    }

    fn register(&self, id: &str) -> bool {
        let mut sim = self.sim.lock().unwrap();
        if id == self.opts.game_id {
            sim.since.get_or_insert_with(Instant::now);
        } else {
            sim.others.insert(id.to_owned());
        }
        let n = sim.seen.entry(id.to_owned()).or_default();
        *n += 1;
        *n == 1
    }

    /// Serves until the listener is dropped; open streams end when
    /// [`break_streams`](Self::break_streams) is called.
    pub fn router(self: &Arc<Self>) -> Router {
        let api = Router::new()
            .route("/api/v1/games", post(submit))
            .route("/api/v1/replays", put(upload).layer(DefaultBodyLimit::max(512 << 20)))
            .route("/api/v1/games/{id}", get(status))
            .route("/api/v1/games/{id}/replay", get(replay))
            .route("/api/v1/games/{id}/records", get(records))
            .route("/api/v1/{*rest}", any(|| async { err(StatusCode::NOT_FOUND, "no such API call") }))
            .route_layer(axum::middleware::from_fn_with_state(self.clone(), guard));
        api.with_state(self.clone())
    }

    /// Serves on `listener` until the returned handle is dropped or stopped.
    pub fn serve(self: &Arc<Self>, listener: tokio::net::TcpListener) -> Server {
        let (stop, mut stopped) = watch::channel(false);
        let router = self.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|s| *s).await;
                })
                .await;
        });
        Server { fake: self.clone(), stop, task: Some(task) }
    }
}

/// A running fake; [`Server::stop`] takes it down (streams broken, port closed).
pub struct Server {
    fake: Arc<Fake>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    pub async fn stop(mut self) {
        self.shut();
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
    }

    fn shut(&mut self) {
        let _ = self.stop.send(true);
        self.fake.break_streams();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut();
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], json!({ "error": msg }).to_string()).into_response()
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn kind_of(path: &str) -> &'static str {
    if path.ends_with("/records") {
        "records"
    } else if path.ends_with("/replay") {
        "replay"
    } else if path == "/api/v1/games" {
        "submit"
    } else if path == "/api/v1/replays" {
        "upload"
    } else if path.starts_with("/api/v1/games/") {
        "status"
    } else {
        "other"
    }
}

/// Token check, call counting and induced failures.
async fn guard(State(fake): State<Arc<Fake>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if token != Some(fake.opts.token.as_str()) {
        return err(StatusCode::UNAUTHORIZED, "missing or unknown token");
    }
    let kind = kind_of(req.uri().path());
    *fake.counts.lock().unwrap().entry(kind).or_default() += 1;
    let n = fake.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if fake.opts.error_every.is_some_and(|k| k > 0 && n.is_multiple_of(k)) {
        return err(StatusCode::SERVICE_UNAVAILABLE, "fake: induced failure");
    }
    if fake.opts.fail_calls.contains(kind) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "fake: this call always fails");
    }
    next.run(req).await
}

fn parse_id(id: &str) -> Option<String> {
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

#[derive(Deserialize)]
struct SubmitBody {
    file_name: Option<String>,
    game_id: Option<String>,
    priority: Option<String>,
}

async fn submit(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<SubmitBody>(&body) else { return err(StatusCode::BAD_REQUEST, "request body") };
    if let Some(e) = fake.refuse_priority(body.priority.as_deref()) {
        return e;
    }
    let id = match (body.game_id, body.file_name) {
        (Some(id), None) => match parse_id(&id) {
            Some(id) => id,
            None => return err(StatusCode::BAD_REQUEST, "not a game id"),
        },
        (None, Some(_)) => return err(StatusCode::NOT_FOUND, "fake: submitting by file name is not supported"),
        _ => return err(StatusCode::BAD_REQUEST, "give exactly one of file_name and game_id"),
    };
    if id != fake.opts.game_id || fake.opts.no_resolve {
        return err(StatusCode::NOT_FOUND, &format!("no replay known for game {id}"));
    }
    let created = fake.register(&id);
    let v = fake.status_json(&id).expect("registered");
    json_response(if created { StatusCode::CREATED } else { StatusCode::OK }, &v)
}

#[derive(Deserialize)]
struct UploadQuery {
    priority: Option<String>,
}

async fn upload(State(fake): State<Arc<Fake>>, Query(q): Query<UploadQuery>, body: Bytes) -> Response {
    if let Some(e) = fake.refuse_priority(q.priority.as_deref()) {
        return e;
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty body");
    }
    let Some(id) = header_game_id(&body) else { return err(StatusCode::BAD_REQUEST, "not a gzip-compressed demo") };
    let created = fake.register(&id);
    let v = fake.status_json(&id).expect("registered");
    json_response(if created { StatusCode::CREATED } else { StatusCode::OK }, &v)
}

#[derive(Deserialize)]
struct StatusQuery {
    wait: Option<f64>,
    rev: Option<u64>,
}

async fn status(State(fake): State<Arc<Fake>>, Path(id): Path<String>, Query(q): Query<StatusQuery>) -> Response {
    let Some(id) = parse_id(&id) else { return err(StatusCode::BAD_REQUEST, "not a game id") };
    let deadline = Instant::now() + Duration::from_secs_f64(q.wait.unwrap_or(0.0).clamp(0.0, 60.0));
    loop {
        let Some(v) = fake.status_json(&id) else { return err(StatusCode::NOT_FOUND, &format!("unknown game {id}")) };
        if q.rev.is_none() || v["rev"].as_u64() != q.rev || Instant::now() >= deadline {
            return json_response(StatusCode::OK, &v);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn replay(State(fake): State<Arc<Fake>>, Path(id): Path<String>) -> Response {
    let Some(id) = parse_id(&id) else { return err(StatusCode::BAD_REQUEST, "not a game id") };
    match &fake.opts.replay {
        Some(bytes) if id == fake.opts.game_id => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", fake.opts.replay_name)),
            ],
            bytes.clone(),
        )
            .into_response(),
        _ => err(StatusCode::NOT_FOUND, &format!("no replay known for game {id}")),
    }
}

#[derive(Deserialize)]
struct RecordsQuery {
    actions: Option<String>,
    from: Option<u64>,
    follow: Option<u8>,
}

/// Garbage lines, sent in turn.
fn garbage(i: usize) -> Vec<u8> {
    match i % 9 {
        0 => b"this is not json".to_vec(),
        1 => br#"{"action":"team_stats","teamID":"zero","frame":-1,"metalProduced":"lots"}"#.to_vec(),
        2 => br#"{"action":"_cursor","next":0,"frame":0,"complete":false}"#.to_vec(),
        3 => b"[1,2,3]".to_vec(),
        4 => br#"{"action":"unit_finished"}"#.to_vec(),
        5 => vec![b'x'; 3 << 19],
        6 => vec![0xff, 0xfe, b'{', 0x80],
        7 => br#"{"action":"_cursor","next":"many","complete":"yes"}"#.to_vec(),
        _ => br#"{"action":"unit_created","frame":1e300,"unitID":-4,"teamID":99999999999,"defID":1}"#.to_vec(),
    }
}

async fn records(State(fake): State<Arc<Fake>>, Path(id): Path<String>, Query(q): Query<RecordsQuery>) -> Response {
    let Some(id) = parse_id(&id) else { return err(StatusCode::BAD_REQUEST, "not a game id") };
    if fake.opts.no_records {
        return err(StatusCode::NOT_FOUND, "no such API call");
    }
    let ours = id == fake.opts.game_id;
    if ours && fake.now().is_none() || !ours && !fake.sim.lock().unwrap().others.contains(&id) {
        return err(StatusCode::NOT_FOUND, &format!("unknown game {id}"));
    }
    let actions: Option<HashSet<String>> =
        q.actions.filter(|a| !a.is_empty()).map(|a| a.split(',').map(str::to_owned).collect());
    let follow = q.follow == Some(1);
    let from = q.from.unwrap_or(0);
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(produce(fake.clone(), ours, actions, from, follow, tx));
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) }));
    ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response()
}

/// Sends a body with the configured cuts and stalls; `false` once it must end.
struct Sender {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    sent: usize,
    stalled: bool,
    drop_after: Option<usize>,
    drop_clean: bool,
    stall_after: Option<usize>,
    stall_s: Option<f64>,
    epoch: watch::Receiver<u64>,
}

impl Sender {
    async fn send(&mut self, mut bytes: Vec<u8>) -> bool {
        if self.epoch.has_changed().unwrap_or(true) {
            let _ = self.tx.send(Err(std::io::Error::other("fake: service went away"))).await;
            return false;
        }
        if let Some(at) = self.stall_after.filter(|at| !self.stalled && self.sent + bytes.len() > *at) {
            let head: Vec<u8> = bytes.drain(..at - self.sent).collect();
            self.sent += head.len();
            if !head.is_empty() && self.tx.send(Ok(Bytes::from(head))).await.is_err() {
                return false;
            }
            self.stalled = true;
            match self.stall_s {
                Some(s) => {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs_f64(s)) => {}
                        _ = self.epoch.changed() => return false,
                    }
                }
                None => {
                    let _ = self.epoch.changed().await;
                    return false;
                }
            }
        }
        if let Some(at) = self.drop_after.filter(|at| self.sent + bytes.len() > *at) {
            bytes.truncate(at - self.sent);
            if !bytes.is_empty() {
                let _ = self.tx.send(Ok(Bytes::from(bytes))).await;
                // Let the server write it out: an error right behind it makes the
                // server drop the connection with the piece still in its buffer.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !self.drop_clean {
                let _ = self.tx.send(Err(std::io::Error::other("fake: connection cut"))).await;
            }
            return false;
        }
        self.sent += bytes.len();
        self.tx.send(Ok(Bytes::from(bytes))).await.is_ok()
    }
}

async fn produce(
    fake: Arc<Fake>,
    ours: bool,
    actions: Option<HashSet<String>>,
    from: u64,
    follow: bool,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let (drop_after, stall_after) = {
        let mut left = fake.drops_left.lock().unwrap();
        let take = |n: &mut Option<u32>, what: Option<usize>| match n {
            _ if what.is_none() => None,
            None => what,
            Some(0) => None,
            Some(k) => {
                *k -= 1;
                what
            }
        };
        let (d, s) = &mut *left;
        (take(d, fake.opts.drop_after_bytes), take(s, fake.opts.stall_after_bytes))
    };
    let mut out = Sender {
        tx,
        sent: 0,
        stalled: false,
        drop_after,
        drop_clean: fake.opts.drop_clean,
        stall_after,
        stall_s: fake.opts.stall_s,
        epoch: fake.epoch.subscribe(),
    };
    out.epoch.mark_unchanged();
    let total = fake.lines.len();
    let mut pos = (from.min(total as u64)) as usize;
    let mut sent_records = 0usize;
    let mut garbage_n = 0usize;
    let mut last_cursor: Option<Instant> = None;
    loop {
        let now = if ours { fake.now() } else { None };
        let (available, done) = now.as_ref().map_or((0, false), |n| (n.available, n.complete));
        let mut batch = Vec::new();
        let start = pos;
        while pos < available {
            let line = &fake.lines[pos];
            pos += 1;
            if actions.as_ref().is_some_and(|a| !a.contains(&line.action)) {
                continue;
            }
            batch.extend_from_slice(&line.bytes);
            batch.push(b'\n');
            sent_records += 1;
            if fake.opts.garbage_every.is_some_and(|m| m > 0 && sent_records.is_multiple_of(m)) {
                batch.extend_from_slice(&garbage(garbage_n));
                batch.push(b'\n');
                garbage_n += 1;
            }
            if batch.len() > 256 << 10 {
                break;
            }
        }
        let complete = done && pos >= total;
        let due = last_cursor.is_none_or(|t| t.elapsed() >= fake.opts.cursor_every);
        if pos > start || due || complete || !follow {
            let frame = if pos > 0 { fake.lines[pos - 1].written } else { 0 };
            let next = (pos as u64).max(from);
            batch.extend_from_slice(json!({"action": "_cursor", "next": next, "frame": frame, "complete": complete}).to_string().as_bytes());
            batch.push(b'\n');
            last_cursor = Some(Instant::now());
            if !out.send(batch).await {
                return;
            }
        }
        let more_now = pos < available;
        if complete || (!follow && !more_now) {
            return;
        }
        if !more_now {
            tokio::select! {
                _ = tokio::time::sleep(fake.opts.poll) => {}
                _ = out.epoch.changed() => {
                    let _ = out.tx.send(Err(std::io::Error::other("fake: service went away"))).await;
                    return;
                }
            }
        }
    }
}
