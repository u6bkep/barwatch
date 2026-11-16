//! The stats client and timeline follower against the fake stats service
//! (`examples/fake_stats`), through the daemon's own HTTP surface.

use crate::fake_service::{self as fake, Fake, Options};
use crate::state::{App, Config};
use crate::stats_client::StatsClient;
use crate::timeline::{self, Tuning};
use barwatch_core::api::{Created, SessionView, TimelineState, TimelineView};
use barwatch_core::timeline::{TimelineBuilder, TimelineDoc, ACTIONS};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token";
const GAME: [u8; 16] = [0x85, 0x0a, 0xd2, 0x6a, 0xd4, 0x15, 0xd6, 0x34, 0x30, 0xd3, 0x2c, 0x0e, 0x36, 0xeb, 0x21, 0xc9];

fn game_id() -> String {
    GAME.iter().map(|b| format!("{b:02x}")).collect()
}

// -- fixtures ------------------------------------------------------------------

const SETUP: &str = "[game]\n{\n[team0]\n{\nallyteam=0;\nteamleader=0;\nside=Armada;\nrgbcolor=0.1 0.2 0.9;\n}\n\
[team1]\n{\nallyteam=1;\nside=Cortex;\n}\n[allyteam0]\n{\n}\n[allyteam1]\n{\n}\n\
[player0]\n{\nteam=0;\nname=Alice;\nspectator=0;\n}\n[player1]\n{\nname=Watcher;\nspectator=1;\n}\n\
[ai0]\n{\nteam=1;\nname=Bot1;\nshortname=BARb;\n}\n\
hostip=1.2.3.4;\nhostport=8452;\nishost=1;\nmyplayername=Host;\ngametype=Some Game 1.0;\nmapname=Some Map v1;\n}\n";

/// A small, structurally valid replay of two teams carrying [`GAME`] as its id.
fn synth_replay(id: [u8; 16]) -> Vec<u8> {
    use std::io::Write;
    let mut head = vec![0u8; 352];
    head[..15].copy_from_slice(b"spring demofile");
    head[16..20].copy_from_slice(&5i32.to_le_bytes());
    head[20..24].copy_from_slice(&352i32.to_le_bytes());
    head[24..34].copy_from_slice(b"2026.07.04");
    head[280..296].copy_from_slice(&id);
    head[296..304].copy_from_slice(&1_700_000_000u64.to_le_bytes());
    head[304..308].copy_from_slice(&(SETUP.len() as i32).to_le_bytes());
    head[312..316].copy_from_slice(&900i32.to_le_bytes());
    head[316..320].copy_from_slice(&950i32.to_le_bytes());
    let mut z = flate2_free_zlib(SETUP.as_bytes());
    let mut pkt = vec![52u8, 0, 0];
    pkt.extend_from_slice(&(z.len() as u16).to_le_bytes());
    pkt.append(&mut z);
    pkt.extend_from_slice(&[0xab; 64]);
    pkt.extend_from_slice(&[0xcd; 64]);
    pkt.extend_from_slice(&7u32.to_le_bytes());
    let mut raw = head;
    raw.extend_from_slice(SETUP.as_bytes());
    raw.extend_from_slice(&0.0f32.to_le_bytes());
    raw.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
    raw.extend_from_slice(&pkt);
    let mut gz = GzipStored::default();
    gz.write_all(&raw).unwrap();
    gz.finish()
}

/// zlib with stored (uncompressed) blocks: enough for the setup text, and needs
/// no compression crate here.
fn flate2_free_zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(deflate_stored(data));
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(65535).collect() };
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(c);
    }
    out
}

/// A gzip writer with stored blocks.
#[derive(Default)]
struct GzipStored(Vec<u8>);

impl std::io::Write for GzipStored {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl GzipStored {
    fn finish(self) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        out.extend(deflate_stored(&self.0));
        out.extend_from_slice(&crc32(&self.0).to_le_bytes());
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        out
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A raw output of `periods` statistics periods for the replay above, in the
/// shape the real widgets write: unit definitions, periodic records per team,
/// noise the stream filters out, `bs_team_stats` one frame after the period
/// carrying its own frame, a kill, a team dying, and Gex's `team_stats` batch at
/// game over carrying the history's frames.
fn synth_output(periods: u32) -> Vec<Vec<u8>> {
    let mut out: Vec<Value> = vec![json!({"action": "init", "frame": 0, "version": 1})];
    out.push(json!({"action": "unit_def", "frame": 0, "defID": 1, "defName": "armcom", "isCommander": true, "metalCost": 1000, "energyCost": 1000, "unitGroup": ""}));
    out.push(json!({"action": "unit_def", "frame": 0, "defID": 2, "defName": "armpw", "metalCost": 50, "energyCost": 900, "speed": 80, "unitGroup": "weapon"}));
    out.push(json!({"action": "unit_def", "frame": 0, "defID": 3, "defName": "corcom", "isCommander": true, "metalCost": 1000, "energyCost": 1000, "unitGroup": ""}));
    for (unit, team, def) in [(100, 0, 1), (101, 1, 3)] {
        out.push(json!({"action": "unit_created", "frame": 0, "unitID": unit, "teamID": team, "defID": def}));
        out.push(json!({"action": "unit_finished", "frame": 0, "unitID": unit, "teamID": team}));
    }
    let stats = |frame: u32, team: u32, k: u32, action: &str| {
        json!({"action": action, "frame": frame, "teamID": team, "metalProduced": f64::from(k * (20 + team)),
               "energyProduced": f64::from(k * 300), "unitsProduced": k / 2, "damageDealt": f64::from(k * k)})
    };
    for k in 1..=periods {
        let f = 450 * k;
        out.push(json!({"action": "unit_position", "frame": f - 10, "unitID": 100, "x": 1, "z": 2}));
        for team in 0..2 {
            out.push(json!({"action": "extra_stat_update", "frame": f, "teamID": team, "totalValue": f64::from(k * 100 * (team + 1)),
                            "armyValue": f64::from(k * 40), "metalCurrent": 300.5, "actions": k * 7}));
            out.push(json!({"action": "bs_team_resources", "frame": f, "teamID": team, "metalCurrent": 200, "metalStorage": 1000,
                            "metalIncome": 2.5 + f64::from(k), "metalExpense": 2, "energyCurrent": 500, "energyStorage": 1000,
                            "energyIncome": 30, "energyExpense": 20}));
        }
        out.push(json!({"action": "unit_position", "frame": f + 1, "unitID": 101, "x": 1, "z": 2}));
        for team in 0..2 {
            out.push(stats(f, team, k, "bs_team_stats"));
        }
        if k == periods / 2 {
            out.push(json!({"action": "unit_created", "frame": f + 5, "unitID": 200, "teamID": 0, "defID": 2}));
            out.push(json!({"action": "unit_finished", "frame": f + 60, "unitID": 200, "teamID": 0}));
            out.push(json!({"action": "unit_killed", "frame": f + 90, "unitID": 200, "teamID": 0, "defID": 2, "attackerTeam": 1}));
        }
    }
    let end = 450 * periods + 3;
    out.push(json!({"action": "team_died", "frame": end, "teamID": 0}));
    for k in 0..=periods {
        for team in 0..2 {
            out.push(stats(450 * k, team, k, "team_stats"));
        }
    }
    out.push(json!({"action": "end", "frame": end}));
    out.push(json!({"action": "shutdown", "frame": end, "done": "done"}));
    out.into_iter().map(|v| v.to_string().into_bytes()).collect()
}

/// The document of the whole output, built directly.
fn reference(replay: &[u8], lines: &[Vec<u8>], dir: &Path) -> TimelineDoc {
    let mut b = builder_for(replay, dir);
    for l in lines {
        let v: Value = serde_json::from_slice(l).unwrap();
        if ACTIONS.contains(&v["action"].as_str().unwrap()) {
            b.feed_line(std::str::from_utf8(l).unwrap());
        }
    }
    b.feed_line(&json!({"action": "_cursor", "next": lines.len(), "frame": 1, "complete": true}).to_string());
    // Through JSON as the daemon serves it: serde_json's float parsing may differ
    // from the computed value in the last bit.
    norm(serde_json::from_slice(&serde_json::to_vec(&b.document()).unwrap()).unwrap())
}

fn builder_for(replay: &[u8], dir: &Path) -> TimelineBuilder {
    let path = dir.join("reference.sdfz");
    std::fs::write(&path, replay).unwrap();
    let demo = barwatch_core::demo::Demo::read(&path).unwrap();
    let facts = barwatch_core::stats::StatsHistory::read(&path).unwrap().events;
    TimelineBuilder::new(barwatch_core::timeline::teams_from_replay(&demo, &facts))
}

/// A document without its `rev`, which counts lines that changed it and so
/// depends on where cursors fell.
fn norm(mut d: TimelineDoc) -> TimelineDoc {
    d.rev = 0;
    d
}

/// Asserts two documents are equal, naming the first difference.
#[track_caller]
fn same_doc(got: &TimelineDoc, want: &TimelineDoc, what: &str) {
    let (got, want) = (norm(got.clone()), norm(want.clone()));
    if got == want {
        return;
    }
    let head = |d: &TimelineDoc| (d.complete, d.frames, d.teams.clone(), d.metrics.clone());
    assert_eq!(head(&got), head(&want), "{what}: header");
    for (k, w) in &want.series {
        let g = got.series.get(k);
        for (t, wt) in w.iter().enumerate() {
            let gt = g.and_then(|g| g.get(t));
            if gt != Some(wt) {
                let at = gt.map_or(0, |gt| gt.iter().zip(wt).take_while(|(a, b)| a == b).count());
                panic!("{what}: series {k} team {t} differs from sample {at}: got {:?} want {:?}", gt.map(|g| &g[at.min(g.len())..(at + 4).min(g.len())]), &wt[at..(at + 4).min(wt.len())]);
            }
        }
    }
    assert_eq!(got.series.len(), want.series.len(), "{what}: series keys");
    for (i, (g, w)) in got.marks.iter().zip(&want.marks).enumerate() {
        assert_eq!(g, w, "{what}: mark {i}");
    }
    assert_eq!(got.marks.len(), want.marks.len(), "{what}: number of marks");
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("barwatchd-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// -- harness -------------------------------------------------------------------

fn quick() -> Tuning {
    Tuning {
        poll_demand: Duration::from_secs(5),
        linger: Duration::from_secs(1),
        stall: Duration::from_secs(2),
        unreachable_grace: Duration::from_secs(1),
        records_retry: Duration::from_secs(2),
        stream_retry: Duration::from_millis(100),
        ..Tuning::default()
    }
}

struct Daemon {
    app: Arc<App>,
    base: String,
    http: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Daemon {
    /// A daemon whose BAR archive is a port nobody listens on: tests never reach
    /// the real one.
    async fn start(data: &Path, stats: Option<SocketAddr>, tuning: Tuning) -> Daemon {
        let nowhere = crate::source::Archive { api: "http://127.0.0.1:9/replays".into(), bucket: "http://127.0.0.1:9/BAR".into() };
        Daemon::start_with(data, stats, tuning, nowhere).await
    }

    async fn start_with(data: &Path, stats: Option<SocketAddr>, tuning: Tuning, archive: crate::source::Archive) -> Daemon {
        // The engine is "installed", so preparing a party downloads nothing.
        let engine = data.join("engines").join("2026.07.04");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(engine.join("spring-dedicated"), b"").unwrap();
        let cfg = Config {
            data: data.to_path_buf(),
            udp_ports: 18480..=18481,
            game_host: None,
            max_upload: 16 << 20,
            session_days: 14,
            download_url: "https://example.org".into(),
            archive,
        };
        let client = stats.map(|a| StatsClient::new(&format!("http://{a}/"), TOKEN).unwrap());
        let app = Arc::new(App::new(cfg, reqwest::Client::new(), timeline::Service::new(client, tuning)).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(app.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Daemon { app, base, http: reqwest::Client::new(), task }
    }

    async fn upload(&self, replay: &[u8]) -> String {
        let resp = self
            .http
            .post(format!("{}/api/sessions", self.base))
            .header("x-barwatch-filename", "game.sdfz")
            .body(replay.to_vec())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{:?}", resp.text().await);
        resp.json::<Created>().await.unwrap().id
    }

    async fn link_party(&self, link: &str) -> reqwest::Response {
        self.http.post(format!("{}/api/sessions", self.base)).json(&json!({ "bar_replay": link })).send().await.unwrap()
    }

    async fn session(&self, id: &str) -> SessionView {
        self.http.get(format!("{}/app/v1/sessions/{id}", self.base)).send().await.unwrap().json().await.unwrap()
    }

    async fn timeline(&self, id: &str, rev: Option<u64>) -> TimelineView {
        let mut url = format!("{}/app/v1/sessions/{id}/timeline", self.base);
        if let Some(rev) = rev {
            url.push_str(&format!("?rev={rev}&wait=2"));
        }
        let resp = self.http.get(url).send().await.unwrap();
        assert_eq!(resp.headers()["content-type"], "application/json");
        resp.json().await.unwrap()
    }

    /// Follows the timeline until `done` says so; returns every view seen.
    async fn watch(&self, id: &str, limit: Duration, done: impl Fn(&TimelineView) -> bool) -> Vec<TimelineView> {
        let until = Instant::now() + limit;
        let mut seen: Vec<TimelineView> = Vec::new();
        loop {
            let v = self.timeline(id, seen.last().map(|v| v.rev)).await;
            if let Some(last) = seen.last() {
                assert!(v.rev >= last.rev, "rev went back: {} after {}", v.rev, last.rev);
            }
            let finished = done(&v);
            if seen.last().is_none_or(|l| l.rev != v.rev) {
                seen.push(v);
            }
            if finished {
                return seen;
            }
            assert!(Instant::now() < until, "timed out; last view {:?}", seen.last().map(|v| (v.state, &v.note, v.doc.as_ref().map(|d| d.frames))));
        }
    }
}

async fn start_fake(opts: Options, at: Option<SocketAddr>) -> (Arc<Fake>, fake::Server, SocketAddr) {
    let fake = Fake::new(opts);
    let (server, addr) = serve_fake(&fake, at).await;
    (fake, server, addr)
}

async fn serve_fake(fake: &Arc<Fake>, at: Option<SocketAddr>) -> (fake::Server, SocketAddr) {
    let listener = tokio::net::TcpListener::bind(at.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap())).await.unwrap();
    let addr = listener.local_addr().unwrap();
    (fake.serve(listener), addr)
}

fn opts(lines: Vec<Vec<u8>>, replay: &[u8]) -> Options {
    let mut o = Options::new(TOKEN, lines, &game_id());
    o.replay = Some(replay.to_vec());
    o.replay_name = "2026-01-01_game.sdfz".into();
    o.poll = Duration::from_millis(50);
    o.cursor_every = Duration::from_millis(500);
    o
}

fn complete(v: &TimelineView) -> bool {
    v.state == TimelineState::Complete
}

fn states(seen: &[TimelineView]) -> Vec<TimelineState> {
    let mut s: Vec<TimelineState> = seen.iter().map(|v| v.state).collect();
    s.dedup();
    s
}

// -- tests -----------------------------------------------------------------------

/// An upload registers the game, and the timeline goes queued → building
/// (growing, with the companion's counters during the game) → complete, equal to
/// the document of the whole output.
#[tokio::test(flavor = "multi_thread")]
async fn upload_party_grows_to_complete() {
    let dir = scratch("upload");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines, &replay);
    o.rate = Some(250.0);
    o.queued_s = 1.0;
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;

    let started = Instant::now();
    let id = d.upload(&replay).await;
    assert!(started.elapsed() < Duration::from_secs(2), "creating a party waited for the service");
    let seen = d.watch(&id, Duration::from_secs(30), complete).await;

    let st = states(&seen);
    assert!(st.contains(&TimelineState::Queued) && st.contains(&TimelineState::Building), "{st:?}");
    assert_eq!(st.last(), Some(&TimelineState::Complete));
    let building: Vec<&TimelineDoc> =
        seen.iter().filter(|v| v.state == TimelineState::Building).filter_map(|v| v.doc.as_ref()).collect();
    assert!(building.len() >= 3, "the document grew in {} steps", building.len());
    assert!(building.windows(2).all(|w| w[0].frames <= w[1].frames));
    // The companion's `bs_team_stats` carry the engine's counters before game over.
    assert!(building.iter().any(|d| d.series.get("metalProduced").is_some_and(|s| s[1].len() > 3) && !d.complete));
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    assert_eq!((fake.count("upload"), fake.count("submit")), (1, 0));
    assert_eq!(fake.priorities(), ["interactive"]);

    let view = d.session(&id).await;
    assert_eq!(view.game_id.as_deref(), Some(game_id().as_str()));
    let meta: Value = serde_json::from_slice(&std::fs::read(dir.join("data/sessions").join(&id).join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["game_id"], game_id());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A link gets its replay through the service's mirror, registers by game id, and
/// grows a timeline.
#[tokio::test(flavor = "multi_thread")]
async fn link_party_through_the_service() {
    let dir = scratch("link");
    let (replay, lines) = (synth_replay(GAME), synth_output(20));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines, &replay);
    o.rate = Some(200.0);
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;

    let resp = d.link_party(&format!("https://www.beyondallreason.info/replays?gameId={}", game_id().to_uppercase())).await;
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let id = resp.json::<Created>().await.unwrap().id;
    assert_eq!(std::fs::read(dir.join("data/sessions").join(&id).join("replay.sdfz")).unwrap(), replay);
    let view = d.session(&id).await;
    assert_eq!(view.replay.file_name, "2026-01-01_game.sdfz");
    let seen = d.watch(&id, Duration::from_secs(30), complete).await;
    assert!(states(&seen).contains(&TimelineState::Building));
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    assert_eq!((fake.count("replay"), fake.count("submit"), fake.count("upload")), (1, 1, 0));
    assert_eq!(fake.priorities(), ["interactive"]);
    assert_eq!(d.session(&id).await.game_id.as_deref(), Some(game_id().as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stand-in for BAR's replay archive that knows one replay; counts its calls.
async fn start_archive(id: &str, replay: Vec<u8>) -> (crate::source::Archive, Arc<std::sync::atomic::AtomicU64>, tokio::task::JoinHandle<()>) {
    use axum::extract::Path as P;
    use std::sync::atomic::{AtomicU64, Ordering};
    let hits = Arc::new(AtomicU64::new(0));
    let (h1, h2, id) = (hits.clone(), hits.clone(), id.to_owned());
    let router = axum::Router::new()
        .route(
            "/replays/{id}",
            axum::routing::get(move |P(asked): P<String>| async move {
                h1.fetch_add(1, Ordering::SeqCst);
                if asked.eq_ignore_ascii_case(&id) {
                    Ok(axum::Json(json!({"fileName": "2026-01-01_direct.sdfz"})))
                } else {
                    Err(axum::http::StatusCode::NOT_FOUND)
                }
            }),
        )
        .route(
            "/BAR/demos/{name}",
            axum::routing::get(move |P(name): P<String>| async move {
                h2.fetch_add(1, Ordering::SeqCst);
                if name == "2026-01-01_direct.sdfz" { Ok(replay.clone()) } else { Err(axum::http::StatusCode::NOT_FOUND) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (crate::source::Archive { api: format!("http://{addr}/replays"), bucket: format!("http://{addr}/BAR") }, hits, task)
}

/// A link whose replay the service will not give properly (here it sends another
/// game's file) comes from BAR's archive directly, and a game the service cannot
/// find by id is uploaded to it instead.
#[tokio::test(flavor = "multi_thread")]
async fn link_party_falls_back() {
    let dir = scratch("fallback");
    let replay = synth_replay(GAME);
    let mut o = opts(synth_output(4), &replay);
    o.no_resolve = true;
    o.replay = Some(synth_replay([7; 16]));
    let (fake, _server, addr) = start_fake(o, None).await;
    let (archive, hits, archive_task) = start_archive(&game_id(), replay.clone()).await;
    let d = Daemon::start_with(&dir.join("data"), Some(addr), quick(), archive).await;

    let resp = d.link_party(&game_id()).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let id = resp.json::<Created>().await.unwrap().id;
    assert_eq!(std::fs::read(dir.join("data/sessions").join(&id).join("replay.sdfz")).unwrap(), replay);
    assert_eq!(d.session(&id).await.replay.file_name, "2026-01-01_direct.sdfz");
    assert_eq!((fake.count("replay"), hits.load(std::sync::atomic::Ordering::SeqCst)), (1, 2));

    let seen = d.watch(&id, Duration::from_secs(20), complete).await;
    assert!(seen.last().unwrap().doc.is_some());
    assert_eq!((fake.count("submit"), fake.count("upload")), (1, 1));
    assert_eq!(fake.priorities(), ["interactive", "interactive"]);
    assert_eq!(d.session(&id).await.game_id.as_deref(), Some(game_id().as_str()));

    // Neither knows the id: refused, with words.
    let resp = d.link_party(&"ab".repeat(16)).await;
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("does not know"));
    archive_task.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The follower survives a connection cut at every kind of place: inside a line,
/// at a line's end, inside a `_cursor`, right after one; broken or cleanly ended.
#[tokio::test(flavor = "multi_thread")]
async fn resumes_after_drops() {
    let dir = scratch("drops");
    let (replay, lines) = (synth_replay(GAME), synth_output(12));
    let want = reference(&replay, &lines, &dir);
    // Where things are in the first response of a finished game.
    let filtered: Vec<&Vec<u8>> = lines
        .iter()
        .filter(|l| ACTIONS.contains(&serde_json::from_slice::<Value>(l).unwrap()["action"].as_str().unwrap()))
        .collect();
    let first_line = filtered[0].len();
    let body: usize = filtered.iter().map(|l| l.len() + 1).sum();
    let cuts = [1, first_line / 2, first_line, first_line + 1, body - 1, body, body + 10, body + 40];
    for (i, &cut) in cuts.iter().enumerate() {
        for clean in [false, true] {
            let mut o = opts(lines.clone(), &replay);
            o.drop_after_bytes = Some(cut);
            o.drop_times = Some(2);
            o.drop_clean = clean;
            let (fake, _server, addr) = start_fake(o, None).await;
            let data = dir.join(format!("data{i}{clean}"));
            let d = Daemon::start(&data, Some(addr), quick()).await;
            let id = d.upload(&replay).await;
            let seen = d.watch(&id, Duration::from_secs(30), complete).await;
            same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, &format!("cut at {cut}, clean {clean}"));
            assert!(fake.count("records") >= 2, "cut at {cut}: {} record calls", fake.count("records"));
        }
    }
    // And while following a running simulation, with cuts in later batches.
    let mut o = opts(lines.clone(), &replay);
    o.rate = Some(100.0);
    o.drop_after_bytes = Some(1500);
    o.drop_times = Some(6);
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data-live"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(40), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    assert!(fake.count("records") >= 7);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Garbage in the stream changes nothing; a stream that stalls is dropped and
/// resumed.
#[tokio::test(flavor = "multi_thread")]
async fn garbage_and_stalls() {
    let dir = scratch("garbage");
    let (replay, lines) = (synth_replay(GAME), synth_output(16));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines.clone(), &replay);
    o.garbage_every = Some(3);
    o.rate = Some(300.0);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(30), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");

    let mut o = opts(lines, &replay);
    o.stall_after_bytes = Some(2000);
    o.stall_times = Some(1);
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data2"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(30), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    assert!(fake.count("records") >= 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The service goes away mid-game: the timeline says so and keeps its document;
/// when it comes back (having forgotten nothing), following resumes and completes.
/// Then it comes back having forgotten the game: the daemon registers it again.
#[tokio::test(flavor = "multi_thread")]
async fn service_goes_away_and_comes_back() {
    let dir = scratch("away");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines, &replay);
    o.rate = Some(150.0);
    let (fake, server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(20), |v| v.doc.as_ref().is_some_and(|d| d.frames > 3000)).await;
    let before = seen.last().unwrap().doc.clone().unwrap();

    server.stop().await;
    let seen = d.watch(&id, Duration::from_secs(20), |v| v.state == TimelineState::Unavailable).await;
    let down = seen.last().unwrap();
    assert!(down.note.as_deref().unwrap().contains("cannot be reached"), "{:?}", down.note);
    let kept = down.doc.as_ref().expect("the document is kept while the service is away");
    assert!(kept.frames >= before.frames);

    let (_server, _) = serve_fake(&fake, Some(addr)).await;
    let seen = d.watch(&id, Duration::from_secs(40), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    assert_eq!(fake.count("upload"), 1);

    // A service that forgets the game mid-simulation (its store was reset): the
    // daemon registers it again and follows the new simulation to the end.
    let mut o = opts(synth_output(40), &replay);
    o.rate = Some(100.0);
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data2"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(20), |v| v.doc.as_ref().is_some_and(|d| d.frames > 2000)).await;
    assert!(!seen.last().unwrap().doc.as_ref().unwrap().complete);
    fake.forget();
    let seen = d.watch(&id, Duration::from_secs(40), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "after forgetting");
    assert_eq!(fake.count("upload"), 2, "registered again");
    let _ = std::fs::remove_dir_all(&dir);
}

/// barwatchd restarts mid-game: the session keeps its game id (no new
/// registration) and the timeline is read again and completes.
#[test]
fn daemon_restart_mid_game() {
    let dir = scratch("restart");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let want = reference(&replay, &lines, &dir);
    let fake_rt = tokio::runtime::Runtime::new().unwrap();
    let mut o = opts(lines, &replay);
    o.rate = Some(100.0);
    let (fake, _server, addr) = fake_rt.block_on(start_fake(o, None));
    let data = dir.join("data");

    let first = tokio::runtime::Runtime::new().unwrap();
    let id = first.block_on(async {
        let d = Daemon::start(&data, Some(addr), quick()).await;
        let id = d.upload(&replay).await;
        let seen = d.watch(&id, Duration::from_secs(20), |v| v.doc.as_ref().is_some_and(|d| d.frames > 2000)).await;
        assert!(!seen.last().unwrap().doc.as_ref().unwrap().complete);
        id
    });
    // Everything of the first daemon stops here, followers included.
    first.shutdown_timeout(Duration::from_secs(1));

    let second = tokio::runtime::Runtime::new().unwrap();
    second.block_on(async {
        let d = Daemon::start(&data, Some(addr), quick()).await;
        assert_eq!(d.session(&id).await.game_id.as_deref(), Some(game_id().as_str()));
        let seen = d.watch(&id, Duration::from_secs(40), complete).await;
        same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    });
    second.shutdown_timeout(Duration::from_secs(1));
    assert_eq!(fake.count("upload"), 1, "the restarted daemon did not register again");
    drop(fake_rt);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A simulation that fails: the timeline says so, with the service's reason, and
/// keeps what it had.
#[tokio::test(flavor = "multi_thread")]
async fn failed_simulation() {
    let dir = scratch("failed");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let mut o = opts(lines, &replay);
    o.rate = Some(300.0);
    o.fail_at_frame = Some(450 * 20);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(30), |v| v.state == TimelineState::Failed).await;
    let last = seen.last().unwrap();
    assert!(last.note.as_deref().unwrap().contains("the engine crashed (fake)"), "{:?}", last.note);
    assert!(last.doc.as_ref().is_some_and(|d| d.frames > 0 && d.frames <= 450 * 20 + 3 && !d.complete));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service without the record stream (bar-replays before §9.8): the game is
/// registered, and the timeline says plainly that results cannot be had.
#[tokio::test(flavor = "multi_thread")]
async fn service_without_records() {
    let dir = scratch("norecords");
    let replay = synth_replay(GAME);
    let mut o = opts(synth_output(4), &replay);
    o.no_records = true;
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(20), |v| v.state == TimelineState::Unavailable && v.note.as_deref().is_some_and(|n| n.contains("newer version"))).await;
    assert!(seen.last().unwrap().doc.is_none());
    assert_eq!(d.session(&id).await.game_id.as_deref(), Some(game_id().as_str()));
    // It asks again now and then, not in a loop.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let calls = fake.count("records");
    assert!((1..=3).contains(&calls), "{calls} record calls");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a service everything works as before and the timeline says why there
/// is none; with a service that is down at creation, the party is made at once and
/// the game is registered once the service comes up.
#[tokio::test(flavor = "multi_thread")]
async fn no_service_and_service_down() {
    let dir = scratch("down");
    let replay = synth_replay(GAME);
    let d = Daemon::start(&dir.join("none"), None, quick()).await;
    let id = d.upload(&replay).await;
    let v = d.timeline(&id, None).await;
    assert_eq!((v.state, v.note.as_deref()), (TimelineState::Unavailable, Some("This server has no statistics service.")));
    assert_eq!(d.session(&id).await.game_id, None);

    // A port nobody listens on, for now.
    let addr = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap() };
    let d = Daemon::start(&dir.join("down"), Some(addr), quick()).await;
    let started = Instant::now();
    let id = d.upload(&replay).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let v = d.timeline(&id, None).await;
    assert_eq!(v.state, TimelineState::Unavailable);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut o = opts(synth_output(4), &replay);
    o.rate = None;
    let (fake, _server, _) = start_fake(o, Some(addr)).await;
    let seen = d.watch(&id, Duration::from_secs(30), complete).await;
    assert!(seen.last().unwrap().doc.is_some());
    assert_eq!(fake.count("upload"), 1);
    assert_eq!(d.session(&id).await.game_id.as_deref(), Some(game_id().as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service that refuses the token, errs often, or sends far too much.
#[tokio::test(flavor = "multi_thread")]
async fn hostile_or_broken_service() {
    let dir = scratch("hostile");
    let (replay, lines) = (synth_replay(GAME), synth_output(16));
    let want = reference(&replay, &lines, &dir);

    let mut o = opts(lines.clone(), &replay);
    o.token = "another".into();
    let (_fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("token"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(10), |v| v.note.as_deref().is_some_and(|n| n.contains("does not accept"))).await;
    assert_eq!(seen.last().unwrap().state, TimelineState::Unavailable);

    // Every third call fails: it all gets there in the end.
    let mut o = opts(lines.clone(), &replay);
    o.error_every = Some(3);
    o.rate = Some(300.0);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("errors"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(40), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");

    // More bytes than a game can make: given up, said plainly.
    let mut o = opts(lines, &replay);
    o.garbage_every = Some(1);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let tuning = Tuning { max_stream_bytes: 200_000, ..quick() };
    let d = Daemon::start(&dir.join("big"), Some(addr), tuning).await;
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(20), |v| v.state == TimelineState::Failed).await;
    assert!(seen.last().unwrap().note.as_deref().unwrap().contains("far more"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Following stops when nobody asks, an unfinished document is let go, and a
/// complete one is kept until the budget for kept documents is exceeded.
#[tokio::test(flavor = "multi_thread")]
async fn demand_and_memory() {
    let dir = scratch("demand");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines, &replay);
    o.rate = Some(40.0);
    let (fake, _server, addr) = start_fake(o, None).await;
    let tuning = Tuning { poll_demand: Duration::from_secs(1), kept_budget: 1, ..quick() };
    let d = Daemon::start(&dir.join("data"), Some(addr), tuning).await;
    let id = d.upload(&replay).await;
    let _ = d.watch(&id, Duration::from_secs(20), |v| v.doc.is_some()).await;
    let session = d.app.session(&id).unwrap();

    // Nobody asks: the follower stops and the unfinished document goes.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let records = fake.count("records");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(fake.count("records"), records, "still following");
    let v: TimelineView = serde_json::from_slice(&session.stats.view_json(true).await).unwrap();
    assert!(v.doc.is_none());

    // Asked again: read from the start, to the end.
    let seen = d.watch(&id, Duration::from_secs(60), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    // Kept while nobody asks; evicted by the (tiny) budget.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let v: TimelineView = serde_json::from_slice(&session.stats.view_json(true).await).unwrap();
    assert_eq!(v.state, TimelineState::Complete);
    timeline::evict(&d.app);
    let v: TimelineView = serde_json::from_slice(&session.stats.view_json(true).await).unwrap();
    assert!(v.doc.is_none());
    let seen = d.watch(&id, Duration::from_secs(20), complete).await;
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    let _ = std::fs::remove_dir_all(&dir);
}

/// At most `max_followers` parties follow at once: the next one says it waits for a
/// turn, and follows to the end once the first is done.
#[tokio::test(flavor = "multi_thread")]
async fn followers_take_turns() {
    let dir = scratch("turns");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let want = reference(&replay, &lines, &dir);
    let mut o = opts(lines, &replay);
    o.rate = Some(60.0);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let tuning = Tuning { max_followers: 1, poll_demand: Duration::from_secs(60), ..quick() };
    let d = Daemon::start(&dir.join("data"), Some(addr), tuning).await;
    let a = d.upload(&replay).await;
    let _ = d.watch(&a, Duration::from_secs(20), |v| v.doc.is_some()).await;
    let b = d.upload(&replay).await;
    let seen = d.watch(&b, Duration::from_secs(20), |v| v.note.as_deref().is_some_and(|n| n.contains("waiting for a turn"))).await;
    let waiting = seen.last().unwrap();
    assert_eq!((waiting.state, waiting.doc.is_none()), (TimelineState::Unavailable, true));
    let first = d.watch(&a, Duration::from_secs(40), complete).await;
    same_doc(first.last().unwrap().doc.as_ref().unwrap(), &want, "first");
    let second = d.watch(&b, Duration::from_secs(40), complete).await;
    same_doc(second.last().unwrap().doc.as_ref().unwrap(), &want, "second");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deleting a party while it is being followed stops the follower and leaves
/// nothing on disk.
#[tokio::test(flavor = "multi_thread")]
async fn delete_while_following() {
    let dir = scratch("delete");
    let (replay, lines) = (synth_replay(GAME), synth_output(40));
    let mut o = opts(lines, &replay);
    o.rate = Some(30.0);
    o.queued_s = 0.5;
    let (fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let _ = d.watch(&id, Duration::from_secs(20), |v| v.doc.is_some()).await;
    let session = d.app.session(&id).unwrap();
    let resp = d.http.delete(format!("{}/api/sessions/{id}", d.base)).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(session.is_closed());
    assert!(!dir.join("data/sessions").join(&id).exists());
    let records = fake.count("records") + fake.count("status");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(fake.count("records") + fake.count("status"), records, "still following a deleted party");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real duel fixture, if this machine has it: the live path with the
/// companion's records.
#[tokio::test(flavor = "multi_thread")]
async fn real_duel_fixture() {
    let output = Path::new("/data/projects/bar-replays-dev/outputs/duel-1v1.actions.jsonl");
    let replay_path = Path::new("/data/projects/bar-replays-dev/demos/duel-1v1.sdfz");
    if !output.exists() || !replay_path.exists() {
        eprintln!("skipped: no fixture at {}", output.display());
        return;
    }
    let dir = scratch("duel");
    let replay = std::fs::read(replay_path).unwrap();
    let lines = fake::split_lines(&std::fs::read(output).unwrap());
    let want = reference(&replay, &lines, &dir);
    let id_hex = fake::header_game_id(&replay).unwrap();
    let mut o = Options::new(TOKEN, lines, &id_hex);
    o.replay = Some(replay.clone());
    o.rate = Some(400.0);
    let (_fake, _server, addr) = start_fake(o, None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    // Uses the replay's own engine version, which this harness "installs" under
    // another name; preparing may fail, which does not matter here.
    let id = d.upload(&replay).await;
    let seen = d.watch(&id, Duration::from_secs(60), complete).await;
    let growing = seen
        .iter()
        .filter_map(|v| v.doc.as_ref())
        .filter(|d| !d.complete && d.series.get("metalProduced").is_some_and(|s| s[0].len() > 10))
        .count();
    assert!(growing > 0, "the engine's counters grew during the game");
    same_doc(seen.last().unwrap().doc.as_ref().unwrap(), &want, "final");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A skip fills `SessionView.jump` with its absolute target: a relative one
/// resolved from where the server stands, capped at the replay's end; a skip
/// behind an earlier one (which the engine ignores) leaves it; a new run clears it.
#[tokio::test(flavor = "multi_thread")]
async fn jumps_are_reported() {
    use crate::runner::{apply, GameClock, Link};
    use barwatch_core::api::Control;
    let dir = scratch("jump");
    let d = Daemon::start(&dir.join("data"), None, quick()).await;
    let id = d.upload(&synth_replay(GAME)).await;
    let session = d.app.session(&id).unwrap();
    let engine = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let link = Link::for_test(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(), engine.local_addr().unwrap());
    let heard = || async {
        let mut buf = [0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(2), engine.recv(&mut buf)).await.unwrap().unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    let length = session.read(|st| st.replay.duration_s);
    assert!(length > 700, "the fixture is {length} s long");
    let mut clock = GameClock::default();

    // Before the pre-game skip: refused, nothing sent, no jump.
    apply(&link, &session, Control::Skip { seconds: 60, relative: true }, false, &mut clock).await;
    assert_eq!(d.session(&id).await.jump, None);

    clock.skipped_to(1);
    clock.advance(Duration::from_secs(30), 2.0);
    let before = crate::state::unix_now();
    apply(&link, &session, Control::Skip { seconds: 120, relative: true }, true, &mut clock).await;
    assert_eq!(heard().await, "/skip +120");
    let jump = d.session(&id).await.jump.expect("a jump");
    assert_eq!(jump.target_s, 181);
    assert!(jump.at_unix >= before);

    apply(&link, &session, Control::Skip { seconds: 600, relative: false }, true, &mut clock).await;
    assert_eq!(heard().await, "/skip 600");
    assert_eq!(d.session(&id).await.jump.map(|j| j.target_s), Some(600));
    apply(&link, &session, Control::Skip { seconds: 300, relative: false }, true, &mut clock).await;
    assert_eq!(heard().await, "/skip 300");
    assert_eq!(d.session(&id).await.jump.map(|j| j.target_s), Some(600), "a skip behind the last one is ignored");
    apply(&link, &session, Control::Skip { seconds: 100_000, relative: true }, true, &mut clock).await;
    assert_eq!(d.session(&id).await.jump.map(|j| j.target_s), Some(length));

    // A new run (which fails at once here: the engine is a stand-in).
    let resp = d.http.post(format!("{}/api/sessions/{id}/start", d.base)).send().await.unwrap();
    assert!(resp.status().is_success());
    assert_eq!(resp.json::<SessionView>().await.unwrap().jump, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The view's JSON is `api::TimelineView`'s.
#[tokio::test]
async fn view_json_is_the_api_type() {
    let slot = timeline::Slot::new(None);
    let v: TimelineView = serde_json::from_slice(&slot.view_json(false).await).unwrap();
    assert_eq!(v, TimelineView { rev: 1, state: TimelineState::Unavailable, note: Some("This server has no statistics service.".into()), doc: None });
    let raw: Value = serde_json::from_slice(&slot.view_json(false).await).unwrap();
    assert_eq!(raw, serde_json::to_value(&v).unwrap());
}

/// Services that answer in ways the fake does not: each found by review, each was a
/// loop or a lost record before.
mod odd_services {
    use super::*;
    use axum::routing::{get, put};
    use axum::Router;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

    async fn serve(router: Router) -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, router).await;
        });
        a
    }

    fn counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// Takes every upload but denies knowing the game: uploads back off.
    #[tokio::test(flavor = "multi_thread")]
    async fn takes_games_but_denies_them() {
        let dir = scratch("odd-deny");
        let (uploads, g) = (counter(), game_id());
        let u = uploads.clone();
        let router = Router::new()
            .route(
                "/api/v1/replays",
                put(move || {
                    let (u, g) = (u.clone(), g.clone());
                    async move {
                        u.fetch_add(1, SeqCst);
                        axum::Json(json!({"game_id": g, "state": "fetched", "rev": 1}))
                    }
                }),
            )
            .route("/api/v1/games/{id}", get(|| async { (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": "unknown game"}))) }));
        let d = Daemon::start(&dir.join("data"), Some(serve(router).await), quick()).await;
        let id = d.upload(&synth_replay(GAME)).await;
        let rev = d.app.session(&id).unwrap().read(|st| st.rev);
        // Someone watches, so the follower runs and keeps hearing "unknown".
        for _ in 0..5 {
            let _ = d.timeline(&id, None).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let n = uploads.load(SeqCst);
        assert!((2..=5).contains(&n), "{n} uploads in 5 s");
        // Taking the same id again is no change of the session.
        assert!(d.app.session(&id).unwrap().read(|st| st.rev) <= rev + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The service forgets the game while a line is half sent; after registering
    /// again, the resumed stream's first line is read on its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn forgets_the_game_mid_line() {
        let dir = scratch("odd-midline");
        let (statuses, records, g) = (counter(), counter(), game_id());
        let st = move |g: &str| json!({"game_id": g, "state": "simulating", "progress": {"frame": 5, "of_frames": 10}, "rev": 1});
        let (s, r, g1, g2, st1, st2) = (statuses.clone(), records.clone(), g.clone(), g.clone(), st, st);
        let router = Router::new()
            .route("/api/v1/replays", put(move || { let g = g1.clone(); async move { axum::Json(st1(&g)) } }))
            .route(
                "/api/v1/games/{id}",
                get(move || {
                    let (s, g) = (s.clone(), g2.clone());
                    async move {
                        let n = s.fetch_add(1, SeqCst) + 1;
                        if n == 2 {
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": "unknown game"})));
                        }
                        if n > 2 {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        (axum::http::StatusCode::OK, axum::Json(st2(&g)))
                    }
                }),
            )
            .route(
                "/api/v1/games/{id}/records",
                get(move || {
                    let r = r.clone();
                    async move {
                        if r.fetch_add(1, SeqCst) == 0 {
                            let first = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                                b"{\"action\":\"_cursor\",\"next\":5,\"frame\":0,\"complete\":false}\n{\"action\":\"team_died\",\"frame\":5,\"teamID\":",
                            ))]);
                            axum::body::Body::from_stream(first.chain(futures_util::stream::pending()))
                        } else {
                            axum::body::Body::from(
                                "{\"action\":\"team_died\",\"frame\":5,\"teamID\":1}\n{\"action\":\"_cursor\",\"next\":7,\"frame\":5,\"complete\":true}\n",
                            )
                        }
                    }
                }),
            );
        let d = Daemon::start(&dir.join("data"), Some(serve(router).await), quick()).await;
        let id = d.upload(&synth_replay(GAME)).await;
        let seen = d.watch(&id, Duration::from_secs(20), complete).await;
        let marks = &seen.last().unwrap().doc.as_ref().unwrap().marks;
        assert!(marks.iter().any(|m| m.kind == "teamDied" && m.team == 1), "{marks:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A status long-poll answered at once every time: at most about one call a second.
    #[tokio::test(flavor = "multi_thread")]
    async fn status_answered_at_once() {
        let dir = scratch("odd-nowait");
        let (statuses, g) = (counter(), game_id());
        let (s, g1, g2) = (statuses.clone(), g.clone(), g);
        let router = Router::new()
            .route("/api/v1/replays", put(move || { let g = g1.clone(); async move { axum::Json(json!({"game_id": g, "state": "failed", "error": "x", "rev": 1})) } }))
            .route(
                "/api/v1/games/{id}",
                get(move || {
                    let (s, g) = (s.clone(), g2.clone());
                    async move {
                        s.fetch_add(1, SeqCst);
                        axum::Json(json!({"game_id": g, "state": "failed", "error": "x", "rev": 1}))
                    }
                }),
            );
        let d = Daemon::start(&dir.join("data"), Some(serve(router).await), quick()).await;
        let id = d.upload(&synth_replay(GAME)).await;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let v = d.timeline(&id, None).await;
            assert_eq!(v.state, TimelineState::Failed, "{:?}", v.note);
        }
        let n = statuses.load(SeqCst);
        assert!(n <= 8, "{n} status calls in 5 s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stream that only moves its cursor and closes brings nothing: the daemon
    /// backs off instead of asking again every moment.
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_only_streams_back_off() {
        let dir = scratch("odd-cursor");
        let calls = counter();
        let g = game_id();
        let status = json!({"game_id": g, "state": "simulating", "progress": {"frame": 900}, "rev": 1});
        let st = status.clone();
        let c = calls.clone();
        let router = Router::new()
            .route("/api/v1/replays", put(move || {
                let st = st.clone();
                async move { axum::Json(st) }
            }))
            .route("/api/v1/games/{id}", get(move || {
                let st = status.clone();
                async move { axum::Json(st) }
            }))
            .route("/api/v1/games/{id}/records", get(move || {
                let n = c.fetch_add(1, SeqCst) + 1;
                async move { format!("{{\"action\":\"_cursor\",\"next\":{n},\"frame\":0,\"complete\":false}}\n") }
            }));
        let d = Daemon::start(&dir.join("data"), Some(serve(router).await), quick()).await;
        let id = d.upload(&synth_replay(GAME)).await;
        for _ in 0..5 {
            let _ = d.timeline(&id, None).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // 0.1 s doubling: about six in five seconds; without backing off, dozens.
        let n = calls.load(SeqCst);
        assert!((2..=10).contains(&n), "{n} record calls in 5 s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The token is refused after the game was registered: the timeline says so
    /// (not "cannot be reached"), and once it is accepted again the note goes.
    #[tokio::test(flavor = "multi_thread")]
    async fn token_refused_then_accepted() {
        let dir = scratch("odd-token");
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let g = game_id();
        let status = json!({"game_id": g, "state": "fetched", "progress": {"frame": 0}, "rev": 1});
        let st = status.clone();
        let r = refuse.clone();
        let router = Router::new()
            .route("/api/v1/replays", put(move || {
                let st = st.clone();
                async move { axum::Json(st) }
            }))
            .route("/api/v1/games/{id}", get(move || {
                let (r, st) = (r.clone(), status.clone());
                async move {
                    if r.load(SeqCst) {
                        (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "missing or unknown bearer token"})))
                    } else {
                        (axum::http::StatusCode::OK, axum::Json(st))
                    }
                }
            }))
            .route("/api/v1/{*rest}", get(|| async { (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": "no such API call"}))) }));
        let d = Daemon::start(&dir.join("data"), Some(serve(router).await), quick()).await;
        let id = d.upload(&synth_replay(GAME)).await;
        let seen = d.watch(&id, Duration::from_secs(10), |v| v.note.as_deref().is_some_and(|n| n.contains("does not accept"))).await;
        assert_eq!(seen.last().unwrap().state, TimelineState::Unavailable);
        refuse.store(false, SeqCst);
        let seen = d.watch(&id, Duration::from_secs(10), |v| v.note.as_deref().is_some_and(|n| n.contains("newer version"))).await;
        assert_eq!(seen.last().unwrap().state, TimelineState::Unavailable);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Parties that cannot read anything (here: a service without the record stream)
/// do not hold the follower turns: with one turn, both say why, neither waits.
#[tokio::test(flavor = "multi_thread")]
async fn turns_are_not_held_without_a_stream() {
    let dir = scratch("turns-idle");
    let replay = synth_replay(GAME);
    let mut o = opts(synth_output(4), &replay);
    o.no_records = true;
    let (_fake, _server, addr) = start_fake(o, None).await;
    let tuning = Tuning { max_followers: 1, records_retry: Duration::from_secs(30), poll_demand: Duration::from_secs(60), ..quick() };
    let d = Daemon::start(&dir.join("data"), Some(addr), tuning).await;
    let a = d.upload(&replay).await;
    let _ = d.watch(&a, Duration::from_secs(10), |v| v.note.as_deref().is_some_and(|n| n.contains("newer version"))).await;
    let b = d.upload(&replay).await;
    let _ = d.watch(&b, Duration::from_secs(10), |v| v.note.as_deref().is_some_and(|n| n.contains("newer version"))).await;
    // And they stay so: the parked ones leave the turn free.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for id in [&a, &b] {
        let v = d.timeline(id, None).await;
        assert!(v.note.as_deref().is_some_and(|n| n.contains("newer version")), "{:?}", v.note);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
