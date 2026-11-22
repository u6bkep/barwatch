//! A replay's statistics (DESIGN §4.1, §9.4): making sure the stats service knows
//! the game, following its record stream into a [`TimelineBuilder`], and what
//! `GET /app/v1/rooms/{id}/timeline` answers for a room's current replay.
//!
//! Each replay has a [`Slot`], shared by every room playing it. One task per replay
//! ([`ensure`] starts it) does the work:
//! - **Registering**: right after a room picks the replay, and again whenever the
//!   service says it does not know the game. An upload goes up as the file
//!   (`PUT /api/v1/replays`); a replay fetched by a BAR link or id is submitted by
//!   game id (`POST /api/v1/games`), so the service records it as coming from BAR's
//!   store, and is uploaded only if the service cannot find it that way. The game
//!   id the service answers with must be the one in the replay's own header. It is
//!   stored with the replay (`meta.json`). Retries back off up to five minutes and
//!   go on for an hour without anyone watching; after that they wait for demand.
//! - **Following**: while there is demand (a registered viewer or an engine in the
//!   game of a room whose current replay it is, or a timeline request in the last
//!   [`POLL_DEMAND`]), it long-polls the game's status and reads the record stream
//!   with `follow=1`, resuming from the last `_cursor`'s `next` after a drop. At
//!   most [`MAX_FOLLOWERS`] replays follow
//!   at once: each holds a turn for as long as it holds a builder, so that many
//!   builders exist at most, whatever their streams do; the others say they are
//!   waiting for one. A follower that can read nothing (the game failed, the
//!   service has no record stream or refuses the token) is parked: its builder and
//!   turn go, and it tries again later.
//! - **Stopping**: [`LINGER`] after demand ends. A complete document is kept (it is
//!   small, and the game will not change); an incomplete one is dropped with its
//!   builder and read again from the start when demand returns. Kept documents
//!   are bounded in total by [`KEPT_BUDGET`] ([`evict`]); the least recently used
//!   go first.
//!
//! The service is not trusted: answers are size- and time-limited
//! (`stats_client`), lines longer than the builder's limit are skipped, a stream
//! that sends nothing for [`STALL`] is dropped and resumed, and one game's stream may
//! not exceed [`MAX_STREAM_BYTES`] in total.

use crate::state::{App, Replay};
use crate::stats_client::{GameStatus, Priority, StatsClient, SvcError, SvcPhase};
use axum::body::Bytes;
use barwatch_core::api::TimelineState;
use barwatch_core::timeline::{self, TimelineBuilder, TimelineDoc};
use futures_util::StreamExt;
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

/// Replays following a stream at the same time; each holds a builder.
pub const MAX_FOLLOWERS: usize = 16;
/// A timeline request counts as demand for this long.
pub const POLL_DEMAND: Duration = Duration::from_secs(90);
/// Following goes on this long after the last demand.
pub const LINGER: Duration = Duration::from_secs(60);
/// Total estimated size of the complete documents kept for replays nobody follows.
pub const KEPT_BUDGET: usize = 64 << 20;
/// A stream that sends nothing for this long is dropped and resumed (the service
/// sends a `_cursor` line at least every few seconds).
pub const STALL: Duration = Duration::from_secs(30);
/// Most bytes read from one game's record stream, over all connections. The
/// filtered stream of an 18-team game is about 7 MB.
pub const MAX_STREAM_BYTES: u64 = 512 << 20;
/// Registration is retried this long after a room picks the replay, watched or not.
const REGISTER_WINDOW: Duration = Duration::from_secs(3600);
/// Status long-poll wait.
const STATUS_WAIT: Duration = Duration::from_secs(25);
/// The service may fail this long, once it has answered, before viewers are told.
const UNREACHABLE_GRACE: Duration = Duration::from_secs(10);
/// A service without the record stream is asked again after this long.
const RECORDS_RETRY: Duration = Duration::from_secs(300);
/// A new document is published at most this often while the stream runs.
const PUBLISH_EVERY: Duration = Duration::from_secs(1);
/// Least time between two status calls of one follower.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// Bytes after which a record stream connection that ended counted as working.
const WORKING_CONNECTION: u64 = 64 << 10;
/// A game the service failed is asked about again after this long.
const FAILED_RECHECK: Duration = Duration::from_secs(30);
/// A record stream request that has not answered with its headers by then is given
/// up and tried again.
const CONNECT_LIMIT: Duration = Duration::from_secs(30);

/// The daemon's side: the client, if a service is configured, and the follower
/// permits.
pub struct Service {
    pub client: Option<StatsClient>,
    permits: Arc<Semaphore>,
    pub tuning: Tuning,
}

/// The limits and waits above, as one value so that tests can shorten them.
#[derive(Clone, Debug)]
pub struct Tuning {
    pub max_followers: usize,
    pub poll_demand: Duration,
    pub linger: Duration,
    pub kept_budget: usize,
    pub stall: Duration,
    pub max_stream_bytes: u64,
    pub unreachable_grace: Duration,
    pub records_retry: Duration,
    /// First wait before a record stream is asked for again; it doubles while
    /// connections bring nothing new.
    pub stream_retry: Duration,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            max_followers: MAX_FOLLOWERS,
            poll_demand: POLL_DEMAND,
            linger: LINGER,
            kept_budget: KEPT_BUDGET,
            stall: STALL,
            max_stream_bytes: MAX_STREAM_BYTES,
            unreachable_grace: UNREACHABLE_GRACE,
            records_retry: RECORDS_RETRY,
            stream_retry: Duration::from_secs(1),
        }
    }
}

impl Service {
    pub fn new(client: Option<StatsClient>, tuning: Tuning) -> Service {
        Service { client, permits: Arc::new(Semaphore::new(tuning.max_followers)), tuning }
    }
}

/// Why registering stopped short of a game id.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reg {
    /// Not tried, in progress or retrying.
    Pending,
    /// The replay's header has no game id.
    NoId,
    /// The service will not take the replay; its words.
    Refused(String),
    /// The service refuses the token.
    BadToken,
}

#[derive(Debug)]
struct Inner {
    rev: u64,
    /// A task works for this replay.
    task: bool,
    /// Something wanted the task while it ran; it must not stop on stale facts.
    kick: bool,
    last_poll: Option<Instant>,
    /// For eviction: when the document was last asked for or followed.
    last_used: Instant,
    /// Registration goes on without demand until then.
    register_until: Option<Instant>,
    /// The service refused the game, the token or the replay: not asked again
    /// before then, by this task or a later one.
    register_after: Option<Instant>,
    game_id: Option<String>,
    reg: Reg,
    status: Option<GameStatus>,
    unreachable: bool,
    records_missing: bool,
    waiting_for_turn: bool,
    /// The follower gave up on this stream; why.
    trouble: Option<String>,
    doc: Option<Arc<TimelineDoc>>,
    cache: Option<(u64, Bytes)>,
}

/// One replay's statistics.
pub struct Slot {
    /// The replay's key, which the view names.
    key: String,
    inner: Mutex<Inner>,
    rev: watch::Sender<u64>,
    /// Held while the view is written: the pollers a change wakes all at once wait
    /// for one rendering and share it, instead of each writing its own copy.
    render: tokio::sync::Mutex<()>,
}

/// What the timeline call answers; the same JSON as `api::TimelineView`, written
/// from references.
#[derive(Serialize)]
struct ViewRef<'a> {
    rev: u64,
    state: TimelineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<&'a TimelineDoc>,
    replay: &'a str,
}

type Key = (TimelineState, Option<String>, Option<(u64, bool)>);

impl Inner {
    fn derive(&self, configured: bool) -> (TimelineState, Option<String>) {
        use TimelineState::*;
        let say = |s: &str| Some(s.to_owned());
        if !configured {
            return (Unavailable, say("This server has no statistics service."));
        }
        if self.doc.as_ref().is_some_and(|d| d.complete) {
            return (Complete, None);
        }
        if let Some(t) = &self.trouble {
            return (Failed, Some(t.clone()));
        }
        match &self.reg {
            Reg::NoId => {
                return (Unavailable, say("This replay carries no game id, so the statistics service cannot work on it."))
            }
            Reg::Refused(w) => return (Failed, Some(format!("The statistics service would not take this replay: {w}"))),
            Reg::BadToken => return (Unavailable, say("The statistics service does not accept this server.")),
            Reg::Pending => {}
        }
        if self.unreachable {
            return (Unavailable, say("The statistics service cannot be reached; trying again."));
        }
        if self.game_id.is_none() {
            return (Unavailable, say("Waiting for the statistics service to take the replay."));
        }
        if let Some(st) = self.status.as_ref().filter(|s| s.phase() == SvcPhase::Failed) {
            let why = st.error.clone().unwrap_or_else(|| "no reason given".into());
            return (Failed, Some(format!("The statistics service could not work out this game: {why}")));
        }
        if self.records_missing {
            return (Unavailable, say("The statistics service cannot send its results yet; it needs a newer version."));
        }
        if self.waiting_for_turn {
            return (Unavailable, say("Too many rooms are following statistics at once; waiting for a turn."));
        }
        match self.status.as_ref().map(GameStatus::phase) {
            Some(SvcPhase::Simulating | SvcPhase::Done) => (Building, None),
            Some(SvcPhase::Queued | SvcPhase::Failed) => (Queued, None),
            None if self.doc.is_some() => (Building, None),
            None => (Queued, None),
        }
    }

    fn key(&self, configured: bool) -> Key {
        let (state, note) = self.derive(configured);
        (state, note, self.doc.as_ref().map(|d| (d.rev, d.complete)))
    }
}

impl Slot {
    pub fn new(key: &str, game_id: Option<String>) -> Slot {
        Slot {
            key: key.to_owned(),
            inner: Mutex::new(Inner {
                rev: 1,
                task: false,
                kick: false,
                last_poll: None,
                last_used: Instant::now(),
                register_until: None,
                register_after: None,
                game_id,
                reg: Reg::Pending,
                status: None,
                unreachable: false,
                records_missing: false,
                waiting_for_turn: false,
                trouble: None,
                doc: None,
                cache: None,
            }),
            rev: watch::channel(1).0,
            render: tokio::sync::Mutex::new(()),
        }
    }

    /// Changes the slot; bumps `rev` and wakes waiters if the answer changed.
    /// `configured` is constant for a daemon; it only matters for the key.
    fn change<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut i = self.inner.lock().unwrap();
        let before = i.key(true);
        let r = f(&mut i);
        if i.key(true) != before {
            i.rev += 1;
            i.cache = None;
            self.rev.send_replace(i.rev);
        }
        r
    }

    fn read<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
        f(&self.inner.lock().unwrap())
    }

    /// Returns once the slot's `rev` differs from `rev`, or after `wait`. A `rev`
    /// ahead of the slot's is from before a restart of the daemon: answered at once.
    pub async fn changed_since(&self, rev: u64, wait: Duration) {
        let mut rx = self.rev.subscribe();
        let _ = tokio::time::timeout(wait, rx.wait_for(|current| *current != rev)).await;
    }

    /// The cached answer, if it is the current one.
    fn cached(&self) -> Option<Bytes> {
        let mut i = self.inner.lock().unwrap();
        i.last_used = Instant::now();
        i.cache.as_ref().filter(|(rev, _)| *rev == i.rev).map(|(_, b)| b.clone())
    }

    /// The timeline call's answer, as JSON. Written once per `rev`.
    pub async fn view_json(&self, configured: bool) -> Bytes {
        if let Some(b) = self.cached() {
            return b;
        }
        let _rendering = self.render.lock().await;
        if let Some(b) = self.cached() {
            return b;
        }
        let (rev, state, note, doc) = {
            let i = self.inner.lock().unwrap();
            let (state, note) = i.derive(configured);
            (i.rev, state, note, i.doc.clone())
        };
        // Written outside the lock: a whole game's document takes a moment.
        let json = serde_json::to_vec(&ViewRef { rev, state, note: note.as_deref(), doc: doc.as_deref(), replay: &self.key })
            .expect("a timeline view serializes");
        let bytes = Bytes::from(json);
        let mut i = self.inner.lock().unwrap();
        if i.rev == rev {
            i.cache = Some((rev, bytes.clone()));
        }
        bytes
    }

    /// Estimated memory of what the slot keeps, and whether it is a candidate for
    /// eviction (a complete document nobody follows).
    fn kept(&self, watched: Duration) -> Option<(usize, Instant)> {
        self.read(|i| {
            let doc = i.doc.as_ref().filter(|d| d.complete && !i.task)?;
            // Someone is looking at it: dropping it would only make it be read again.
            if i.last_poll.is_some_and(|at| at.elapsed() < watched) {
                return None;
            }
            let cache = i.cache.as_ref().map_or(0, |(_, b)| b.len());
            Some((estimate(doc) + cache, i.last_used))
        })
    }
}

/// Rough bytes a document holds.
fn estimate(doc: &TimelineDoc) -> usize {
    let samples: usize = doc.series.values().flat_map(|teams| teams.iter().map(Vec::len)).sum();
    samples * 8 + doc.marks.len() * 64 + doc.teams.len() * 128 + 1024
}

/// Drops the least recently used complete documents of replays nobody follows
/// until the rest fit in [`KEPT_BUDGET`]. A dropped one is read again on demand.
pub fn evict(app: &App) {
    let mut kept: Vec<(Instant, usize, Arc<Replay>)> =
        app.replays().into_iter().filter_map(|s| s.stats.kept(app.stats.tuning.poll_demand).map(|(n, at)| (at, n, s))).collect();
    let mut total: usize = kept.iter().map(|(_, n, _)| n).sum();
    kept.sort_by_key(|(at, _, _)| *at);
    for (_, n, s) in kept {
        if total <= app.stats.tuning.kept_budget {
            break;
        }
        let dropped = s.stats.change(|i| {
            if i.task {
                return false;
            }
            i.doc = None;
            i.cache = None;
            true
        });
        if dropped {
            total -= n;
        }
    }
}

/// Something wants this replay's statistics: `poll` for a timeline request,
/// `register` right after a room picks it. Starts the task if it is needed and not
/// running.
pub fn ensure(app: &Arc<App>, replay: &Arc<Replay>, poll: bool, register: bool) {
    if app.stats.client.is_none() {
        return;
    }
    let spawn = replay.stats.change(|i| {
        let now = Instant::now();
        if poll {
            i.last_poll = Some(now);
            i.last_used = now;
        }
        if register {
            i.register_until = Some(now + REGISTER_WINDOW);
        }
        if i.task {
            i.kick = true;
            return false;
        }
        let complete = i.doc.as_ref().is_some_and(|d| d.complete);
        if complete && i.game_id.is_some() {
            return false;
        }
        i.task = true;
        true
    });
    if spawn {
        tokio::spawn(run(app.clone(), replay.clone()));
    }
}

/// Whether anyone wants the statistics now: someone in a room playing the replay, or
/// a recent timeline request.
fn demand(app: &App, replay: &Replay, t: &Tuning) -> bool {
    replay.stats.read(|i| i.last_poll.is_some_and(|at| at.elapsed() < t.poll_demand)) || app.watched(replay)
}

/// Doubling waits from a first one (one second unless said) up to a ceiling.
struct Backoff {
    first: Duration,
    next: Duration,
    max: Duration,
}

impl Backoff {
    fn new(max: Duration) -> Backoff {
        Backoff::starting(Duration::from_secs(1), max)
    }

    fn starting(first: Duration, max: Duration) -> Backoff {
        Backoff { first, next: first, max }
    }

    fn take(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }

    fn reset(&mut self) {
        self.next = self.first;
    }
}

/// How a stretch of following ended.
enum FollowEnd {
    Complete,
    NoDemand,
    Gone,
    /// The service does not know the game: register it again.
    Unknown,
    /// The stream broke a limit; `trouble` says which.
    GaveUp,
    /// Nothing can be read for a while (the game failed, the service has no record
    /// stream or refuses this daemon): the builder and its turn are let go, and
    /// following is tried again after this long.
    Parked(Duration),
}

/// The replay's task. Ends when nothing needs it (see the module documentation).
async fn run(app: Arc<App>, replay: Arc<Replay>) {
    let client = app.stats.client.as_ref().expect("only started with a client");
    let slot = &replay.stats;
    let t = &app.stats.tuning;
    let mut builder: Option<Follower> = None;
    // Read from the replay once per task, not on every new builder.
    let mut teams: Option<Vec<timeline::TeamEntry>> = None;
    let mut backoff = Backoff::new(Duration::from_secs(300));
    let mut last_demand = Instant::now();
    loop {
        if replay.is_closed() {
            break;
        }
        let wanted = still_wanted(&app, &replay, t, &mut last_demand);
        let (game_id, registering, settled, gave_up) = slot.read(|i| {
            let registering = i.register_until.is_some_and(|t| Instant::now() < t);
            (i.game_id.clone(), registering, i.doc.as_ref().is_some_and(|d| d.complete), i.trouble.is_some())
        });
        let work = match &game_id {
            None => wanted || registering,
            Some(_) => wanted && !settled && !gave_up,
        };
        if !work {
            if gave_up && wanted {
                // Nothing to do but stay until demand ends, so the note stands.
                nap(&app, &replay, t, &mut last_demand, false, Duration::from_secs(5)).await;
                continue;
            }
            // Stop, unless something wanted the task since the facts were read.
            let stop = slot.change(|i| {
                if std::mem::take(&mut i.kick) {
                    return false;
                }
                i.task = false;
                i.waiting_for_turn = false;
                i.unreachable = false;
                // `trouble` stays: what broke a limit or could not be read would
                // only do so again when demand returns.
                if !i.doc.as_ref().is_some_and(|d| d.complete) {
                    i.doc = None;
                    i.cache = None;
                }
                true
            });
            if stop {
                return;
            }
            continue;
        }
        match game_id {
            None if slot.read(|i| i.register_after.is_some_and(|t| Instant::now() < t)) => {
                // Refused not long ago: nothing changes by asking again yet.
                nap(&app, &replay, t, &mut last_demand, true, Duration::from_secs(5)).await;
            }
            None => match register(&app, &replay, client).await {
                // The backoff is reset once following gets somewhere, not here: a
                // service that takes games and then denies them is not to be
                // cycled quickly.
                Ok(()) => {}
                // `register_after` holds the next try back.
                Err(Some(_)) => {}
                Err(None) => nap(&app, &replay, t, &mut last_demand, true, backoff.take()).await,
            },
            Some(id) => {
                if builder.is_none() {
                    if teams.is_none() && !replay.has_file() {
                        // The cache let the file go (the room moved on): nothing to
                        // build with until a room picks the replay again.
                        nap(&app, &replay, t, &mut last_demand, false, Duration::from_secs(5)).await;
                        continue;
                    }
                    // A turn among the followers, held as long as the builder: at
                    // most that many builders exist, whatever their streams do.
                    let Ok(permit) = app.stats.permits.clone().try_acquire_owned() else {
                        slot.change(|i| i.waiting_for_turn = true);
                        nap(&app, &replay, t, &mut last_demand, false, Duration::from_secs(2)).await;
                        continue;
                    };
                    slot.change(|i| i.waiting_for_turn = false);
                    if teams.is_none() {
                        teams = read_teams(&replay).await;
                    }
                    let Some(teams) = teams.clone() else {
                        slot.change(|i| i.trouble = Some("The server could not read its own copy of the replay.".into()));
                        continue;
                    };
                    builder = Some(Follower::new(slot, teams, permit));
                }
                let f = builder.as_mut().unwrap();
                let end = follow(&app, &replay, client, &id, f, &mut last_demand).await;
                // Whatever connection was open is gone: a resumed one starts on a
                // line of its own.
                f.lost();
                if !matches!(end, FollowEnd::Unknown) {
                    backoff.reset();
                }
                match end {
                    FollowEnd::Complete => {
                        crate::log(&format!("replay {}: statistics complete ({:?})", replay.key, f.builder.stats()));
                        builder = None;
                    }
                    FollowEnd::Unknown => {
                        crate::log(&format!(
                            "replay {}: the stats service does not know game {id}; registering it again",
                            replay.key
                        ));
                        // What was read belongs to a simulation the service no longer
                        // has; the turn is not needed while registering.
                        builder = None;
                        slot.change(|i| {
                            i.doc = None;
                            i.cache = None;
                        });
                        match register(&app, &replay, client).await {
                            // Not at once even so: a service that takes games and
                            // then denies knowing them would otherwise be sent the
                            // replay in a loop.
                            Ok(()) => nap(&app, &replay, t, &mut last_demand, false, backoff.take()).await,
                            // Registering starts over, with its own waits and notes.
                            Err(again) => {
                                if again.is_none() {
                                    nap(&app, &replay, t, &mut last_demand, false, backoff.take()).await;
                                }
                                slot.change(|i| i.game_id = None);
                            }
                        }
                    }
                    FollowEnd::GaveUp => {
                        builder = None;
                        // The note says why; an unfinished document of a stream that
                        // broke a limit is not kept without its turn.
                        slot.change(|i| {
                            i.doc = None;
                            i.cache = None;
                        });
                    }
                    FollowEnd::Parked(wait) => {
                        builder = None;
                        nap(&app, &replay, t, &mut last_demand, false, wait).await;
                    }
                    FollowEnd::NoDemand | FollowEnd::Gone => {}
                }
            }
        }
    }
    slot.change(|i| i.task = false);
}

/// Whether anyone wanted the statistics within [`LINGER`]; `last_demand` is when
/// someone last did.
fn still_wanted(app: &App, replay: &Replay, t: &Tuning, last_demand: &mut Instant) -> bool {
    if demand(app, replay, t) {
        *last_demand = Instant::now();
    }
    last_demand.elapsed() < t.linger
}

/// Sleeps `wait`, or less if the replay goes away or nobody wants its statistics
/// any more (nor, with `registering`, is it in its registration window).
async fn nap(app: &App, replay: &Replay, t: &Tuning, last_demand: &mut Instant, registering: bool, wait: Duration) {
    let until = Instant::now() + wait;
    while Instant::now() < until && !replay.is_closed() {
        let window = registering && replay.stats.read(|i| i.register_until.is_some_and(|r| Instant::now() < r));
        if !still_wanted(app, replay, t, last_demand) && !window {
            return;
        }
        tokio::time::sleep((until - Instant::now()).min(Duration::from_secs(1))).await;
    }
}

/// The teams of the replay the daemon holds, with names, sides and colours.
async fn read_teams(replay: &Replay) -> Option<Vec<timeline::TeamEntry>> {
    let path = replay.replay_path();
    let demo = replay.demo()?;
    tokio::task::spawn_blocking(move || {
        let facts = barwatch_core::stats::StatsHistory::read(&path).ok()?.events;
        Some(timeline::teams_from_replay(&demo, &facts))
    })
    .await
    .ok()?
}

/// Makes sure the service knows the game. `Err(Some(wait))` asks for a fixed
/// wait before the next try, `Err(None)` for the usual backoff.
async fn register(app: &App, replay: &Replay, client: &StatsClient) -> Result<(), Option<Duration>> {
    let slot = &replay.stats;
    let Some(id) = replay.header_game_id.clone() else {
        slot.change(|i| {
            i.reg = Reg::NoId;
            i.register_after = Some(Instant::now() + Duration::from_secs(3600));
        });
        return Err(Some(Duration::from_secs(3600)));
    };
    let upload = || async {
        let bytes =
            tokio::fs::read(replay.replay_path()).await.map_err(|e| SvcError::Unreachable(format!("reading the replay: {e}")))?;
        client.upload(bytes, Priority::Interactive).await
    };
    let answer = if replay.bar_replay.is_some() {
        match client.submit_game(&id).await {
            // Not found that way (too new for the service's lists, say): hand it the file.
            Err(SvcError::NotFound(_) | SvcError::Refused(_)) => upload().await,
            other => other,
        }
    } else {
        upload().await
    };
    let (reg, wait) = match answer {
        Ok(st) if st.game_id == id => {
            crate::log(&format!("replay {}: stats service has game {id} ({})", replay.key, st.state));
            match replay.set_game_id(&id) {
                // Rooms show it (and `match_url`, and whether the file can be had).
                Ok(true) => app.replay_changed(&replay.key),
                Ok(false) => {}
                Err(e) => crate::log(&format!("replay {}: storing the game id: {e:#}", replay.key)),
            }
            slot.change(|i| {
                i.game_id = Some(id.clone());
                i.reg = Reg::Pending;
                i.unreachable = false;
                i.status = Some(st);
            });
            return Ok(());
        }
        Ok(st) => (Reg::Refused(format!("it answered for another game ({})", st.game_id)), Duration::from_secs(3600)),
        Err(SvcError::Unauthorized) => {
            crate::log("the stats service refuses BARWATCH_STATS_TOKEN");
            (Reg::BadToken, Duration::from_secs(300))
        }
        Err(SvcError::Refused(w) | SvcError::NotFound(w)) => (Reg::Refused(w), Duration::from_secs(3600)),
        Err(e @ (SvcError::Unreachable(_) | SvcError::Malformed(_))) => {
            crate::log(&format!("replay {}: registering game {id}: {e}", replay.key));
            slot.change(|i| {
                i.reg = Reg::Pending;
                i.unreachable = true;
            });
            return Err(None);
        }
    };
    crate::log(&format!("replay {}: registering game {id}: {reg:?}", replay.key));
    slot.change(|i| {
        i.reg = reg;
        i.register_after = Some(Instant::now() + wait);
    });
    Err(Some(wait))
}

/// The builder of one following, and where the stream stands.
struct Follower {
    builder: TimelineBuilder,
    /// This follower's turn ([`MAX_FOLLOWERS`]).
    _permit: OwnedSemaphorePermit,
    /// `rev` of the document the slot showed when this builder started. The
    /// document is a pure function of the lines read, so the builder passes it once
    /// it has read the same lines again.
    floor: u64,
    /// Where to resume: the last accepted `_cursor`'s `next`.
    from: u64,
    /// The unfinished last line of the current connection.
    line: Vec<u8>,
    /// The current line outgrew the limit; skip to its end.
    overlong: bool,
    bytes: u64,
    published: u64,
    published_at: Option<Instant>,
}

impl Follower {
    /// A builder with the teams of the replay the daemon holds.
    /// A builder with these teams. Until it has read as far as the document the slot
    /// already shows (read before a pause), it publishes nothing, so the document
    /// does not shrink while the stream is read again.
    fn new(slot: &Slot, teams: Vec<timeline::TeamEntry>, permit: OwnedSemaphorePermit) -> Follower {
        let builder = TimelineBuilder::new(teams);
        let builder_rev = builder.rev();
        let floor = slot.read(|i| i.doc.as_ref().filter(|d| !d.complete).map_or(0, |d| d.rev));
        Follower {
            builder,
            _permit: permit,
            floor,
            from: 0,
            line: Vec::new(),
            overlong: false,
            bytes: 0,
            // Nothing to show until a line has changed the document.
            published: builder_rev,
            published_at: None,
        }
    }

    /// Takes the next piece of the stream. `true` if a `_cursor` moved `from` on.
    fn take(&mut self, mut bytes: &[u8]) -> bool {
        self.bytes += bytes.len() as u64;
        let mut advanced = false;
        while let Some(nl) = bytes.iter().position(|&b| b == b'\n') {
            let piece = &bytes[..nl];
            bytes = &bytes[nl + 1..];
            if !self.overlong && self.line.len() + piece.len() <= timeline::MAX_LINE {
                if self.line.is_empty() {
                    advanced |= self.line_done(piece);
                } else {
                    let mut line = std::mem::take(&mut self.line);
                    line.extend_from_slice(piece);
                    advanced |= self.line_done(&line);
                }
            }
            self.line.clear();
            self.overlong = false;
        }
        if self.overlong || self.line.len() + bytes.len() > timeline::MAX_LINE {
            self.overlong = true;
            self.line.clear();
        } else {
            self.line.extend_from_slice(bytes);
        }
        advanced
    }

    fn line_done(&mut self, line: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(line) else { return false };
        let mut advanced = false;
        if text.contains("\"_cursor\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                if v.get("action").and_then(|a| a.as_str()) == Some("_cursor") {
                    // A cursor that goes back would only repeat lines; one that is
                    // not a number is ignored.
                    if let Some(next) = v.get("next").and_then(serde_json::Value::as_u64).filter(|n| *n > self.from) {
                        self.from = next;
                        advanced = true;
                    }
                }
            }
        }
        self.builder.feed_line(text);
        advanced
    }

    /// The connection broke off.
    fn lost(&mut self) {
        self.line.clear();
        self.overlong = false;
        self.builder.connection_lost();
    }

    /// Hands the document to the slot if it changed (at most every
    /// [`PUBLISH_EVERY`] unless `now`).
    fn publish(&mut self, slot: &Slot, now: bool) {
        if self.builder.rev() == self.published || (self.builder.rev() < self.floor && !self.builder.is_complete()) {
            return;
        }
        if !now && self.published_at.is_some_and(|t| t.elapsed() < PUBLISH_EVERY) {
            return;
        }
        let doc = Arc::new(self.builder.document());
        self.published = self.builder.rev();
        self.published_at = Some(Instant::now());
        slot.change(|i| {
            i.doc = Some(doc);
            i.last_used = Instant::now();
        });
    }
}

type Pending<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Where the record stream stands.
enum Stream {
    /// Not connected; connect at this time (if the status allows).
    Idle(Instant),
    /// Asked; given up at the deadline.
    Connecting(Pending<Result<reqwest::Response, SvcError>>, Instant),
    /// Reading; when the last bytes came.
    Open(futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>, Instant),
}

enum StreamEvent {
    Connect,
    Opened(Result<reqwest::Response, SvcError>),
    Chunk(Bytes),
    /// Ended, broke off or stalled; the words for the log.
    Lost(String),
}

impl Stream {
    /// Waits for the next thing to happen. Cancel-safe: what is in flight stays in
    /// `self`, and the time limits are deadlines kept there, so being cancelled by
    /// another branch of a `select!` does not restart them.
    async fn next(&mut self, stall: Duration) -> StreamEvent {
        match self {
            Stream::Idle(at) => {
                tokio::time::sleep_until((*at).into()).await;
                StreamEvent::Connect
            }
            Stream::Connecting(fut, deadline) => match tokio::time::timeout_at((*deadline).into(), fut.as_mut()).await {
                Ok(r) => StreamEvent::Opened(r),
                Err(_) => StreamEvent::Opened(Err(SvcError::Unreachable("no answer".into()))),
            },
            Stream::Open(body, last) => match tokio::time::timeout_at((*last + stall).into(), body.next()).await {
                Ok(Some(Ok(chunk))) => {
                    *last = Instant::now();
                    StreamEvent::Chunk(chunk)
                }
                Ok(Some(Err(e))) => StreamEvent::Lost(format!("broke off: {}", e.without_url())),
                Ok(None) => StreamEvent::Lost("ended".into()),
                Err(_) => StreamEvent::Lost(format!("sent nothing for {} s", stall.as_secs_f32())),
            },
        }
    }
}

/// Follows one game while there is demand.
async fn follow(
    app: &App,
    replay: &Replay,
    client: &StatsClient,
    game_id: &str,
    f: &mut Follower,
    last_demand: &mut Instant,
) -> FollowEnd {
    let slot = &replay.stats;
    let t = &app.stats.tuning;

    let status_call = |rev: Option<u64>, delay: Duration| -> Pending<Result<GameStatus, SvcError>> {
        let (client, id) = (client.clone(), game_id.to_owned());
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            client.status(&id, rev, if rev.is_some() { STATUS_WAIT } else { Duration::ZERO }).await
        })
    };
    let mut status_fut = status_call(None, Duration::ZERO);
    // When the status call in flight was sent (or will be): the next one waits for
    // at least STATUS_EVERY after it, however fast the service answers.
    let mut status_sent = Instant::now();
    let mut status_rev: Option<u64> = None;
    let mut status_backoff = Backoff::new(Duration::from_secs(10));
    let mut stream_backoff = Backoff::starting(t.stream_retry, Duration::from_secs(30));
    let mut stream = Stream::Idle(Instant::now());
    let mut rev_at_connect = f.builder.rev();
    let mut bytes_at_connect = f.bytes;
    let mut heard = false;
    let mut first_failure: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    // Called on every failed request: viewers hear of it once the service has been
    // failing for a while (at once if it never answered).
    let failed = |first_failure: &mut Option<Instant>, heard: bool| {
        let since = *first_failure.get_or_insert_with(Instant::now);
        if !heard || since.elapsed() >= t.unreachable_grace {
            slot.change(|i| i.unreachable = true);
        }
    };
    let succeeded = |first_failure: &mut Option<Instant>| {
        *first_failure = None;
        slot.change(|i| i.unreachable = false);
    };

    loop {
        let phase = slot.read(|i| i.status.as_ref().map(GameStatus::phase));
        // Records are read once the service has answered for the game and while it
        // has not given up on it.
        let streaming = matches!(phase, Some(SvcPhase::Queued | SvcPhase::Simulating | SvcPhase::Done));
        if !streaming && !matches!(stream, Stream::Idle(_)) {
            // The service gave up on the game: stop reading, resume from the cursor
            // if it takes the game up again.
            f.lost();
            stream = Stream::Idle(Instant::now());
        }
        tokio::select! {
            r = &mut status_fut => {
                let delay = match r {
                    Ok(st) if st.game_id == game_id => {
                        heard = true;
                        succeeded(&mut first_failure);
                        status_backoff.reset();
                        status_rev = Some(st.rev);
                        let failed_game = st.phase() == SvcPhase::Failed;
                        slot.change(|i| {
                            i.status = Some(st);
                            // It answers for the game, so whatever it refused before is past.
                            i.reg = Reg::Pending;
                        });
                        if failed_game {
                            // Asked again now and then, in case the service tries again.
                            f.publish(slot, true);
                            return FollowEnd::Parked(FAILED_RECHECK);
                        }
                        STATUS_EVERY.saturating_sub(status_sent.elapsed())
                    }
                    Ok(st) => {
                        crate::log(&format!("replay {}: stats service answered for game {} instead of {game_id}", replay.key, st.game_id));
                        failed(&mut first_failure, heard);
                        status_backoff.take()
                    }
                    Err(SvcError::NotFound(_)) => return FollowEnd::Unknown,
                    Err(SvcError::Unauthorized) => {
                        crate::log("the stats service refuses BARWATCH_STATS_TOKEN");
                        f.publish(slot, true);
                        slot.change(|i| i.reg = Reg::BadToken);
                        return FollowEnd::Parked(t.records_retry);
                    }
                    Err(e) => {
                        if !matches!(e, SvcError::Unreachable(_)) {
                            crate::log(&format!("replay {}: status of game {game_id}: {e}", replay.key));
                        }
                        failed(&mut first_failure, heard);
                        status_backoff.take()
                    }
                };
                status_sent = Instant::now() + delay;
                status_fut = status_call(status_rev, delay);
            }
            ev = stream.next(t.stall), if streaming => match ev {
                StreamEvent::Connect => {
                    let (client, id, from) = (client.clone(), game_id.to_owned(), f.from);
                    stream = Stream::Connecting(
                        Box::pin(async move { client.records(&id, timeline::ACTIONS, from).await }),
                        Instant::now() + CONNECT_LIMIT,
                    );
                    rev_at_connect = f.builder.rev();
                    bytes_at_connect = f.bytes;
                }
                StreamEvent::Opened(Ok(resp)) => {
                    succeeded(&mut first_failure);
                    slot.change(|i| {
                        i.records_missing = false;
                        i.reg = Reg::Pending;
                    });
                    stream = Stream::Open(resp.bytes_stream().boxed(), Instant::now());
                }
                StreamEvent::Opened(Err(SvcError::NotFound(w))) => {
                    // The game is known (its status answered), so it is the call
                    // that is missing: a service older than the record stream.
                    // Nothing to read: the turn goes to others.
                    crate::log(&format!("replay {}: stats service has no record stream ({w}); asking again in {} s", replay.key, t.records_retry.as_secs()));
                    succeeded(&mut first_failure);
                    f.publish(slot, true);
                    slot.change(|i| i.records_missing = true);
                    return FollowEnd::Parked(t.records_retry);
                }
                StreamEvent::Opened(Err(SvcError::Unauthorized)) => {
                    crate::log("the stats service refuses BARWATCH_STATS_TOKEN");
                    succeeded(&mut first_failure);
                    f.publish(slot, true);
                    slot.change(|i| i.reg = Reg::BadToken);
                    return FollowEnd::Parked(t.records_retry);
                }
                StreamEvent::Opened(Err(e)) => {
                    if !matches!(e, SvcError::Unreachable(_)) {
                        crate::log(&format!("replay {}: record stream of game {game_id}: {e}", replay.key));
                    }
                    failed(&mut first_failure, heard);
                    stream = Stream::Idle(Instant::now() + stream_backoff.take());
                }
                StreamEvent::Chunk(bytes) => {
                    // Whether this connection got anywhere is judged when it ends,
                    // by the document (a cursor alone can be moved for nothing).
                    f.take(&bytes);
                    if f.bytes > t.max_stream_bytes {
                        crate::log(&format!("replay {}: record stream of game {game_id} passed {} bytes; giving up", replay.key, t.max_stream_bytes));
                        slot.change(|i| i.trouble = Some("The statistics service sent far more than any game makes; giving up on it.".into()));
                        return FollowEnd::GaveUp;
                    }
                    if f.builder.is_complete() {
                        f.publish(slot, true);
                        return FollowEnd::Complete;
                    }
                    f.publish(slot, false);
                }
                StreamEvent::Lost(why) => {
                    f.lost();
                    f.publish(slot, true);
                    if f.builder.is_complete() {
                        return FollowEnd::Complete;
                    }
                    // A connection that changed the document, or carried a good
                    // amount (unit definitions change nothing yet), was a working
                    // one: come back soon. Backing off is for one that brings next
                    // to nothing (cursors alone, a line or two); one that brings a
                    // lot of nothing runs into MAX_STREAM_BYTES instead.
                    if f.builder.rev() != rev_at_connect || f.bytes - bytes_at_connect >= WORKING_CONNECTION {
                        stream_backoff.reset();
                    }
                    let wait = stream_backoff.take();
                    crate::log(&format!("replay {}: record stream of game {game_id} {why}; resuming from line {} in {:.1} s", replay.key, f.from, wait.as_secs_f32()));
                    stream = Stream::Idle(Instant::now() + wait);
                }
            },
            _ = tick.tick() => {
                f.publish(slot, false);
                // Retries back off; the grace is kept by the clock, not by them.
                if heard && first_failure.is_some_and(|since| since.elapsed() >= t.unreachable_grace) {
                    slot.change(|i| i.unreachable = true);
                }
                if replay.is_closed() {
                    return FollowEnd::Gone;
                }
                if !still_wanted(app, replay, t, last_demand) {
                    return FollowEnd::NoDemand;
                }
            }
        }
    }
}
