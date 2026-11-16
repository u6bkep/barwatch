//! Everything the daemon knows: configuration, sessions, pairing tokens, game ports.

use crate::runner::RunnerCmd;
use anyhow::{Context, Result};
use barwatch_core::api::{JumpView, Phase, ReplayInfo, SessionView, ViewerState, ViewerView};
use barwatch_core::demo::Demo;
use barwatch_core::maps::MapIndex;
use barwatch_core::names;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

/// A viewer that has not reported for this long is forgotten.
pub const VIEWER_TTL: Duration = Duration::from_secs(30);
/// Most desktop apps registered with one party at a time. Registering needs no
/// login, so it must not be a way to grow the daemon without bound.
pub const MAX_VIEWERS: usize = 64;

pub struct Config {
    /// Absolute path of the data directory.
    pub data: PathBuf,
    pub udp_ports: std::ops::RangeInclusive<u16>,
    /// Host name viewers' engines connect to; `None` lets each app use the host of
    /// the URL it reached the server by.
    pub game_host: Option<String>,
    pub max_upload: usize,
    pub session_days: u64,
    pub download_url: String,
    /// BAR's replay archive, for parties made from a link or id.
    pub archive: crate::source::Archive,
}

pub struct App {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// The stats service, if one is configured, and what follows it.
    pub stats: crate::timeline::Service,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tokens: Mutex<Vec<Token>>,
    free_ports: Mutex<BTreeSet<u16>>,
    pub maps: RwLock<MapIndex>,
    /// Held while installing an engine, so two sessions never unpack the same one.
    pub engine_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Token {
    /// Public handle, shown in the token list and used to revoke.
    pub id: String,
    pub secret: String,
    pub label: String,
    pub created_unix: u64,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A random URL-safe identifier with about 5.95 bits per character.
pub fn random_id(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

impl App {
    pub fn new(cfg: Config, http: reqwest::Client, stats: crate::timeline::Service) -> Result<App> {
        for sub in ["engines", "sessions", "run", "cache"] {
            let dir = cfg.data.join(sub);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tokens = match std::fs::read(cfg.data.join("tokens.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("parsing tokens.json")?,
            Err(_) => Vec::new(),
        };
        let free_ports = cfg.udp_ports.clone().collect();
        let app = App {
            cfg,
            http,
            stats,
            sessions: Mutex::new(HashMap::new()),
            tokens: Mutex::new(tokens),
            free_ports: Mutex::new(free_ports),
            maps: RwLock::new(MapIndex::default()),
            engine_lock: tokio::sync::Mutex::new(()),
        };
        app.load_sessions();
        Ok(app)
    }

    pub fn engines_dir(&self) -> PathBuf {
        self.cfg.data.join("engines")
    }

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.cfg.data.join("run").join(id)
    }

    fn load_sessions(&self) {
        let root = self.cfg.data.join("sessions");
        let Ok(entries) = std::fs::read_dir(&root) else { return };
        let mut sessions = self.sessions.lock().unwrap();
        for entry in entries.flatten() {
            let dir = entry.path();
            match Session::load(&dir) {
                Ok(s) => {
                    sessions.insert(s.id.clone(), Arc::new(s));
                }
                Err(e) => crate::log(&format!("skipping {}: {e:#}", dir.display())),
            }
        }
        crate::log(&format!("loaded {} session(s)", sessions.len()));
    }

    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        let mut all: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        all.sort_by_key(|s| std::cmp::Reverse(s.read(|st| st.created_unix)));
        all
    }

    /// Stores a replay as a new session in phase `Preparing`. `bar_replay` is the id
    /// it was fetched by from BAR's archive; `None` for an upload.
    pub fn create_session(&self, file_name: &str, bytes: &[u8], bar_replay: Option<&str>) -> Result<Arc<Session>> {
        let demo = Demo::from_gz_bytes(bytes).context("this is not a readable replay")?;
        anyhow::ensure!(names::valid_engine_version(&demo.engine_version), "replay names an odd engine version");
        let map = demo.map().context("replay does not name a map")?;
        let map_file = self.maps.read().unwrap().file_name(map);
        let replay = ReplayInfo::from_demo(&demo, &clean_file_name(file_name), Some(map_file))
            .context("replay does not name a game version")?;
        let id = random_id(22);
        let dir = self.cfg.data.join("sessions").join(&id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("replay.sdfz"), bytes)?;
        let meta = Meta {
            id: id.clone(),
            created_unix: unix_now(),
            replay,
            game_id: None,
            bar_replay: bar_replay.map(str::to_owned),
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        let header_game_id = barwatch_core::demo::game_id_from_gz(bytes);
        let session = Arc::new(Session::new(meta, dir, demo, header_game_id, Phase::Preparing));
        self.sessions.lock().unwrap().insert(id, session.clone());
        Ok(session)
    }

    /// Forgets a session and deletes its files. The caller stops its server first.
    pub fn remove_session(&self, id: &str) {
        let removed = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = removed {
            // From here on nothing writes into the session's directory.
            s.touch(|st| st.closed = true);
            let _ = std::fs::remove_dir_all(&s.dir);
            let _ = std::fs::remove_dir_all(self.run_dir(id));
        }
    }

    pub fn take_port(self: &Arc<Self>) -> Option<PortLease> {
        let port = self.free_ports.lock().unwrap().pop_first()?;
        Some(PortLease { app: self.clone(), port })
    }

    pub fn mint_token(&self, label: &str) -> Result<Token> {
        let token = Token {
            id: random_id(8),
            secret: random_id(40),
            label: label.chars().filter(|c| !c.is_control()).take(60).collect(),
            created_unix: unix_now(),
        };
        let mut tokens = self.tokens.lock().unwrap();
        tokens.push(token.clone());
        self.save_tokens(&tokens)?;
        Ok(token)
    }

    pub fn tokens(&self) -> Vec<Token> {
        self.tokens.lock().unwrap().clone()
    }

    pub fn revoke_token(&self, id: &str) -> Result<bool> {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|t| t.id != id);
        self.save_tokens(&tokens)?;
        Ok(tokens.len() != before)
    }

    pub fn token_valid(&self, secret: &str) -> bool {
        // Compare every byte of every token so timing says nothing about a near miss.
        self.tokens.lock().unwrap().iter().fold(false, |hit, t| {
            let same = t.secret.len() == secret.len()
                && t.secret.bytes().zip(secret.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
            hit | same
        })
    }

    fn save_tokens(&self, tokens: &[Token]) -> Result<()> {
        let path = self.cfg.data.join("tokens.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(tokens)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// A game port taken from the pool; returned when dropped.
pub struct PortLease {
    app: Arc<App>,
    pub port: u16,
}

impl Drop for PortLease {
    fn drop(&mut self) {
        self.app.free_ports.lock().unwrap().insert(self.port);
    }
}

fn clean_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(160).collect();
    if cleaned.trim().is_empty() {
        "replay.sdfz".into()
    } else {
        cleaned
    }
}

/// What is kept on disk about a session, next to its replay file.
#[derive(Serialize, Deserialize)]
struct Meta {
    id: String,
    created_unix: u64,
    replay: ReplayInfo,
    /// The game's id at the stats service, once it has taken the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    game_id: Option<String>,
    /// The BAR replay id the party was made from; absent for an upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bar_replay: Option<String>,
}

pub struct Viewer {
    pub id: String,
    pub name: String,
    pub state: ViewerState,
    pub detail: String,
    pub last_seen: Instant,
}

pub struct State {
    pub rev: u64,
    pub created_unix: u64,
    pub replay: ReplayInfo,
    pub phase: Phase,
    pub note: Option<String>,
    pub game_port: Option<u16>,
    pub run: u32,
    pub paused: bool,
    pub speed: f32,
    pub ended: bool,
    /// Desktop apps that registered and keep reporting.
    pub viewers: Vec<Viewer>,
    /// Player number → name of everyone the hosting server has connected.
    pub in_game: BTreeMap<u8, String>,
    /// Command channel of the task that owns the hosting server, while one runs.
    pub runner: Option<mpsc::UnboundedSender<RunnerCmd>>,
    /// The game's id at the stats service.
    pub game_id: Option<String>,
    /// The last jump issued in this run.
    pub jump: Option<JumpView>,
    /// The session was removed; its directory is gone or going.
    pub closed: bool,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub demo: Demo,
    /// The game id in the replay's header.
    pub header_game_id: Option<String>,
    /// See [`Meta::bar_replay`].
    pub bar_replay: Option<String>,
    /// The party's statistics.
    pub stats: crate::timeline::Slot,
    state: Mutex<State>,
    rev: watch::Sender<u64>,
}

impl Session {
    fn new(meta: Meta, dir: PathBuf, demo: Demo, header_game_id: Option<String>, phase: Phase) -> Session {
        // A stored id that is not the header's (an older daemon, a hand edit) is not used.
        let game_id = meta.game_id.filter(|g| Some(g) == header_game_id.as_ref());
        Session {
            id: meta.id,
            dir,
            demo,
            header_game_id,
            bar_replay: meta.bar_replay,
            stats: crate::timeline::Slot::new(game_id.clone()),
            state: Mutex::new(State {
                rev: 1,
                created_unix: meta.created_unix,
                replay: meta.replay,
                phase,
                note: None,
                game_port: None,
                run: 0,
                paused: false,
                speed: 1.0,
                ended: false,
                viewers: Vec::new(),
                in_game: BTreeMap::new(),
                runner: None,
                game_id,
                jump: None,
                closed: false,
            }),
            rev: watch::channel(1).0,
        }
    }

    fn load(dir: &Path) -> Result<Session> {
        let meta: Meta = serde_json::from_slice(&std::fs::read(dir.join("meta.json"))?).context("parsing meta.json")?;
        anyhow::ensure!(names::valid_id(&meta.id), "malformed session id");
        let demo = Demo::read(&dir.join("replay.sdfz"))?;
        let header_game_id = barwatch_core::demo::read_game_id(&dir.join("replay.sdfz"));
        // The engine is checked again when the session is started.
        Ok(Session::new(meta, dir.to_path_buf(), demo, header_game_id, Phase::Idle))
    }

    pub fn is_closed(&self) -> bool {
        self.read(|st| st.closed)
    }

    /// Records the stats service's id for the game, in memory and on disk.
    pub fn set_game_id(&self, game_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if st.closed || st.game_id.as_deref() == Some(game_id) {
            return Ok(());
        }
        st.game_id = Some(game_id.to_owned());
        st.rev += 1;
        self.rev.send_replace(st.rev);
        let meta = Meta {
            id: self.id.clone(),
            created_unix: st.created_unix,
            replay: st.replay.clone(),
            game_id: st.game_id.clone(),
            bar_replay: self.bar_replay.clone(),
        };
        // Written under the lock, which `App::remove_session` takes before it
        // deletes the directory: a removed session's directory is never recreated.
        let path = self.dir.join("meta.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&meta)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn replay_path(&self) -> PathBuf {
        self.dir.join("replay.sdfz")
    }

    pub fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().unwrap())
    }

    /// Changes the state and wakes everyone waiting on this session.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut st = self.state.lock().unwrap();
        let r = f(&mut st);
        st.rev += 1;
        // Published under the lock, so waiters never see revisions out of order.
        self.rev.send_replace(st.rev);
        r
    }

    /// Like [`mutate`](Self::mutate) for changes nobody is waiting to see.
    pub fn touch<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Returns once the session has changed since `rev`, or after `wait`.
    pub async fn changed_since(&self, rev: u64, wait: Duration) {
        let mut rx = self.rev.subscribe();
        let _ = tokio::time::timeout(wait, rx.wait_for(|current| *current > rev)).await;
    }

    pub fn view(&self, game_host: Option<&str>) -> SessionView {
        let st = self.state.lock().unwrap();
        let in_game = |name: &str| st.in_game.values().any(|n| n == name);
        let mut viewers: Vec<ViewerView> = st
            .viewers
            .iter()
            .map(|v| ViewerView { name: v.name.clone(), state: v.state, detail: v.detail.clone(), in_game: in_game(&v.name) })
            .collect();
        // Engines that connected without going through the app.
        for name in st.in_game.values() {
            if !st.viewers.iter().any(|v| &v.name == name) {
                viewers.push(ViewerView {
                    name: name.clone(),
                    state: ViewerState::Playing,
                    detail: String::new(),
                    in_game: true,
                });
            }
        }
        SessionView {
            id: self.id.clone(),
            rev: st.rev,
            created_unix: st.created_unix,
            replay: st.replay.clone(),
            phase: st.phase,
            note: st.note.clone(),
            game_host: game_host.map(str::to_owned),
            game_port: st.game_port,
            run: st.run,
            paused: st.paused,
            speed: st.speed,
            ended: st.ended,
            viewers,
            game_id: st.game_id.clone(),
            jump: st.jump,
        }
    }

    /// Registers a desktop app; returns its secret id and the name its engine must use.
    ///
    /// `playing_as` is the name of an engine this app already has in the game (it is
    /// registering again after the server forgot it); that name is kept if no other
    /// registered viewer holds it. `None` when the party is full.
    pub fn join(&self, wanted_name: &str, playing_as: Option<&str>) -> Option<(String, String)> {
        self.mutate(|st| {
            if st.viewers.len() >= MAX_VIEWERS {
                return None;
            }
            let registered = |name: &str| st.viewers.iter().any(|v| v.name == name);
            let name = match playing_as {
                Some(current) if st.in_game.values().any(|n| n == current) && !registered(current) => current.to_owned(),
                _ => {
                    let mut taken = self.demo.player_names();
                    taken.extend(st.viewers.iter().map(|v| v.name.clone()));
                    taken.extend(st.in_game.values().cloned());
                    names::unique(wanted_name, &taken)
                }
            };
            let id = random_id(22);
            st.viewers.push(Viewer {
                id: id.clone(),
                name: name.clone(),
                state: ViewerState::Preparing,
                detail: String::new(),
                last_seen: Instant::now(),
            });
            Some((id, name))
        })
    }

    /// Records a viewer's report. `false` if the viewer is unknown (expired).
    pub fn report(&self, viewer_id: &str, state: ViewerState, detail: &str) -> bool {
        let detail: String = detail.chars().filter(|c| !c.is_control()).take(200).collect();
        let changed = self.touch(|st| {
            let v = st.viewers.iter_mut().find(|v| v.id == viewer_id)?;
            v.last_seen = Instant::now();
            let changed = v.state != state || v.detail != detail;
            v.state = state;
            v.detail = detail;
            Some(changed)
        });
        if changed == Some(true) {
            self.mutate(|_| ());
        }
        changed.is_some()
    }

    pub fn leave(&self, viewer_id: &str) {
        self.mutate(|st| st.viewers.retain(|v| v.id != viewer_id));
    }

    /// Drops viewers that stopped reporting.
    pub fn expire_viewers(&self) {
        let stale = self.read(|st| st.viewers.iter().any(|v| v.last_seen.elapsed() > VIEWER_TTL));
        if stale {
            self.mutate(|st| st.viewers.retain(|v| v.last_seen.elapsed() <= VIEWER_TTL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_long_and_url_safe() {
        let id = random_id(22);
        assert!(names::valid_id(&id), "{id}");
        assert_ne!(id, random_id(22));
    }

    #[test]
    fn file_names_lose_their_directories() {
        assert_eq!(clean_file_name("C:\\Users\\x\\demos\\game.sdfz"), "game.sdfz");
        assert_eq!(clean_file_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_file_name(""), "replay.sdfz");
    }
}
