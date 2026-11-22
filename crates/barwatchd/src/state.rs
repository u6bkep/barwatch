//! Everything the daemon knows: configuration, rooms, replays, pairing tokens, game
//! ports (DESIGN §9.4, §9.5).
//!
//! A [`Room`] is about people: a name, a link, who is there, what runs. A [`Replay`]
//! is about a game: its record (what it is, the BAR id it came by, whether the stats
//! service holds it), its statistics, and, while the cache keeps it, its file. Rooms
//! name replays (the current one and the history); a replay's record lives while some
//! room names it.
//!
//! Locks, outermost first: [`App::cache_lock`] (setting a room's current replay,
//! trimming the cache), a room's state, a replay's state. The maps of rooms and
//! replays are never locked while a room's state is. A replay's
//! [`file_lock`](Replay::file_lock) is held from getting its file to setting it as a
//! room's current one; trimming skips a replay whose lock is held.

use crate::runner::RunnerCmd;
use anyhow::{Context, Result};
use barwatch_core::api::{HistoryEntry, JumpView, Phase, ReplayInfo, RoomReplay, RoomView, ViewerState, ViewerView};
use barwatch_core::demo::Demo;
use barwatch_core::maps::MapIndex;
use barwatch_core::names;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch, OwnedMutexGuard};

/// A viewer that has not reported for this long is forgotten.
pub const VIEWER_TTL: Duration = Duration::from_secs(30);
/// Most desktop apps registered with one room at a time. Registering needs no
/// login, so it must not be a way to grow the daemon without bound.
pub const MAX_VIEWERS: usize = 64;
/// Most entries in a room's history.
pub const HISTORY_LEN: usize = 50;
/// A file that cannot be fetched again is kept while it is among this many newest
/// entries of some room's history (DESIGN §9.5).
pub const KEEP_UNFETCHABLE: usize = 10;
/// Longest room name, in characters.
pub const MAX_ROOM_NAME: usize = 60;

pub struct Config {
    /// Absolute path of the data directory.
    pub data: PathBuf,
    pub udp_ports: std::ops::RangeInclusive<u16>,
    /// Host name viewers' engines connect to; `None` lets each app use the host of
    /// the URL it reached the server by.
    pub game_host: Option<String>,
    pub max_upload: usize,
    /// Bytes of replay files that can be fetched again kept at most (DESIGN §9.5).
    pub replay_cache: u64,
    pub download_url: String,
    /// BAR's replay archive, for replays picked by a link or id.
    pub archive: crate::source::Archive,
    /// Public base address of the stats service, for `match_url`.
    pub stats_public_url: Option<url::Url>,
}

pub struct App {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// The stats service, if one is configured, and what follows it.
    pub stats: crate::timeline::Service,
    /// By link id. A deleted room stays here, closed, until its server has stopped:
    /// its replays stay named until then.
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    /// By key.
    replays: Mutex<HashMap<String, Arc<Replay>>>,
    /// Held while a room's current replay is set and while the cache is trimmed.
    cache_lock: Mutex<()>,
    tokens: Mutex<Vec<Token>>,
    free_ports: Mutex<BTreeSet<u16>>,
    pub maps: RwLock<MapIndex>,
    /// Held while installing an engine, so two rooms never unpack the same one.
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

/// The key of a replay file (DESIGN §9.4): the game id in its header, as 32
/// lowercase hex digits; without one, `u` and the first 31 hex digits of the
/// SHA-256 of its bytes, which no game id can be.
pub fn replay_key(bytes: &[u8]) -> String {
    if let Some(id) = barwatch_core::demo::game_id_from_gz(bytes) {
        return id;
    }
    use sha2::Digest;
    let hash = sha2::Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("u{}", &hex[..31])
}

/// Whether `key` is a replay key: it names a directory, so nothing else is.
pub fn valid_key(key: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    key.len() == 32 && (hex(key) || key.starts_with('u') && hex(&key[1..]))
}

/// A room's name as given: trimmed, without control characters, at most
/// [`MAX_ROOM_NAME`] characters; "Room" if nothing is left.
pub fn clean_room_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    let cut: String = cleaned.trim().chars().take(MAX_ROOM_NAME).collect();
    let cut = cut.trim();
    if cut.is_empty() {
        "Room".into()
    } else {
        cut.to_owned()
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl App {
    pub fn new(cfg: Config, http: reqwest::Client, stats: crate::timeline::Service) -> Result<App> {
        // Watch parties of barwatch 0.3 are not carried over (DESIGN §9.2, §9.5); their
        // run directories were named by party.
        let sessions = cfg.data.join("sessions");
        if sessions.exists() {
            std::fs::remove_dir_all(&sessions).with_context(|| format!("removing {}", sessions.display()))?;
            let _ = std::fs::remove_dir_all(cfg.data.join("run"));
            crate::log(&format!("removed {}: watch parties of barwatch 0.3 are not kept as rooms", sessions.display()));
        }
        for sub in ["engines", "replays", "rooms", "run", "cache"] {
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
            rooms: Mutex::new(HashMap::new()),
            replays: Mutex::new(HashMap::new()),
            cache_lock: Mutex::new(()),
            tokens: Mutex::new(tokens),
            free_ports: Mutex::new(free_ports),
            maps: RwLock::new(MapIndex::default()),
            engine_lock: tokio::sync::Mutex::new(()),
        };
        app.load();
        app.trim_cache();
        Ok(app)
    }

    pub fn engines_dir(&self) -> PathBuf {
        self.cfg.data.join("engines")
    }

    /// Where a room's server writes; by the room's own id, which a new link keeps.
    pub fn run_dir(&self, uid: &str) -> PathBuf {
        self.cfg.data.join("run").join(uid)
    }

    fn load(&self) {
        let mut replays = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(self.cfg.data.join("replays")) {
            for entry in entries.flatten() {
                let dir = entry.path();
                match Replay::load(&dir) {
                    Ok(r) => {
                        replays.insert(r.key.clone(), Arc::new(r));
                    }
                    Err(e) => crate::log(&format!("skipping {}: {e:#}", dir.display())),
                }
            }
        }
        let mut rooms = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(self.cfg.data.join("rooms")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                match Room::load(&path, &replays) {
                    Ok(room) => {
                        let link = room.read(|st| st.link.clone());
                        rooms.insert(link, Arc::new(room));
                    }
                    Err(e) => crate::log(&format!("skipping {}: {e:#}", path.display())),
                }
            }
        }
        crate::log(&format!("loaded {} room(s) and {} replay(s)", rooms.len(), replays.len()));
        *self.replays.lock().unwrap() = replays;
        *self.rooms.lock().unwrap() = rooms;
    }

    /// The room with this link id, unless it was deleted.
    pub fn room(&self, link: &str) -> Option<Arc<Room>> {
        self.rooms.lock().unwrap().get(link).filter(|r| !r.is_closed()).cloned()
    }

    /// Every room that is not deleted, most recently used first.
    pub fn rooms(&self) -> Vec<Arc<Room>> {
        let mut all: Vec<_> = self.all_rooms().into_iter().filter(|r| !r.is_closed()).collect();
        all.sort_by_key(|r| std::cmp::Reverse(r.read(|st| (st.last_used_unix, st.created_unix))));
        all
    }

    /// Every room, deleted ones whose server is still stopping included.
    pub fn all_rooms(&self) -> Vec<Arc<Room>> {
        self.rooms.lock().unwrap().values().cloned().collect()
    }

    pub fn replays(&self) -> Vec<Arc<Replay>> {
        self.replays.lock().unwrap().values().cloned().collect()
    }

    pub fn replay(&self, key: &str) -> Option<Arc<Replay>> {
        self.replays.lock().unwrap().get(key).cloned()
    }

    pub fn create_room(&self, name: &str) -> Result<Arc<Room>> {
        let now = unix_now();
        let uid = random_id(22);
        let link = random_id(22);
        let file = self.cfg.data.join("rooms").join(format!("{uid}.json"));
        let room =
            Arc::new(Room::new(uid, file, RoomState::new(link.clone(), clean_room_name(name), now, now, None, Vec::new(), now)));
        room.read(|st| room.save(st))?;
        self.rooms.lock().unwrap().insert(link, room.clone());
        Ok(room)
    }

    /// Gives a room a new link id; the old one stops working. Returns the new id.
    pub fn relink(&self, room: &Arc<Room>) -> Result<String> {
        let mut rooms = self.rooms.lock().unwrap();
        let old = room.read(|st| st.link.clone());
        anyhow::ensure!(rooms.get(&old).is_some_and(|r| Arc::ptr_eq(r, room)) && !room.is_closed(), "no such room");
        let link = random_id(22);
        // The new id is in place in memory and on disk before the old one goes.
        room.mutate(|st| {
            st.link = link.clone();
            room.save(st)
        })?;
        rooms.remove(&old);
        rooms.insert(link.clone(), room.clone());
        Ok(link)
    }

    /// Marks a room deleted: its link answers 404 from now on and its file is gone.
    /// It stays listed (closed), naming its replays, until [`forget_room`](Self::forget_room).
    pub fn close_room(&self, room: &Room) {
        room.mutate(|st| {
            st.closed = true;
            let _ = std::fs::remove_file(&room.file);
        });
        room.picks.send_modify(|n| *n += 1);
    }

    /// Drops a closed room whose server has stopped, and what it named.
    pub fn forget_room(&self, room: &Arc<Room>) {
        self.rooms.lock().unwrap().retain(|_, r| !Arc::ptr_eq(r, room));
        let _ = std::fs::remove_dir_all(self.run_dir(&room.uid));
        self.trim_cache();
    }

    /// The replay with the key of `bytes`, holding its file lock, with the file
    /// stored: an existing record if there is one (two rooms on one game share it),
    /// else a new one. `bar_replay` is the BAR id it was fetched by; `None` for an
    /// upload.
    pub async fn replay_for(
        &self,
        file_name: &str,
        bytes: &[u8],
        bar_replay: Option<&str>,
    ) -> Result<(Arc<Replay>, OwnedMutexGuard<()>)> {
        let demo = Demo::from_gz_bytes(bytes).context("this is not a readable replay")?;
        anyhow::ensure!(names::valid_engine_version(&demo.engine_version), "replay names an odd engine version");
        let map = demo.map().context("replay does not name a map")?;
        let map_file = self.maps.read().unwrap().file_name(map);
        let info = ReplayInfo::from_demo(&demo, &clean_file_name(file_name), Some(map_file))
            .context("replay does not name a game version")?;
        let key = replay_key(bytes);
        let mut demo = Some(demo);
        loop {
            let existing = {
                let mut replays = self.replays.lock().unwrap();
                match replays.get(&key) {
                    Some(r) => r.clone(),
                    None => {
                        let meta = ReplayMeta {
                            key: key.clone(),
                            info: info.clone(),
                            game_id: None,
                            bar_replay: bar_replay.map(str::to_ascii_lowercase),
                            last_used_unix: unix_now(),
                        };
                        let dir = self.cfg.data.join("replays").join(&key);
                        let replay = Arc::new(Replay::new(dir, meta, None));
                        // Locked before it is listed, so a trim cannot take it before
                        // the caller has made a room name it.
                        let guard = replay.file_lock.clone().try_lock_owned().expect("a new lock is free");
                        replays.insert(key.clone(), replay.clone());
                        drop(replays);
                        std::fs::create_dir_all(&replay.dir)?;
                        replay.read(|st| replay.save_meta(st))?;
                        replay.store_file(bytes, demo.take())?;
                        return Ok((replay, guard));
                    }
                }
            };
            let guard = existing.file_lock.clone().lock_owned().await;
            if existing.is_closed() {
                // Trimmed while we waited: make it anew.
                continue;
            }
            if !existing.has_file() {
                existing.store_file(bytes, demo.take())?;
            }
            return Ok((existing, guard));
        }
    }

    /// Makes `replay` the room's current one (the caller has stopped its run and
    /// holds the replay's file lock). The old one goes to the front of the history,
    /// every member is given a fresh engine name, and the phase is `Preparing`.
    pub fn set_current(&self, room: &Room, replay: Arc<Replay>) {
        let _cache = self.cache_lock.lock().unwrap();
        let now = unix_now();
        room.mutate(|st| {
            if let Some(old) = st.replay.take() {
                old.touch(now);
                st.history.insert(0, Picked { replay: old, picked_unix: st.picked_unix });
            }
            st.history.retain(|p| p.replay.key != replay.key);
            let mut seen = HashSet::new();
            st.history.retain(|p| seen.insert(p.replay.key.clone()));
            st.history.truncate(HISTORY_LEN);
            replay.touch(now);
            st.replay = Some(replay);
            st.picked_unix = now;
            st.last_used_unix = now;
            st.phase = Phase::Preparing;
            st.note = None;
            st.ended = false;
            st.paused = false;
            st.speed = 1.0;
            st.jump = None;
            st.game_port = None;
            st.reassign_engine_names();
            if let Err(e) = room.save(st) {
                crate::log(&format!("room {}: saving: {e:#}", room.uid));
            }
        });
        room.picks.send_modify(|n| *n += 1);
    }

    /// The stats service can send this replay's file again.
    pub fn refetchable(&self, replay: &Replay) -> bool {
        self.stats.client.is_some() && replay.game_id().is_some()
    }

    /// `<BARWATCH_STATS_PUBLIC_URL>match/<game id>`, when that is set.
    pub fn match_url(&self, game_id: &str) -> Option<String> {
        let base = self.cfg.stats_public_url.as_ref()?;
        Some(base.join(&format!("match/{game_id}")).ok()?.to_string())
    }

    /// Applies DESIGN §9.5: records no room names go with their files; files that
    /// cannot be fetched again are kept while some room has them as current or among
    /// the newest [`KEEP_UNFETCHABLE`] of its history; files that can are kept, least
    /// recently used dropped first, while they fit in the cache budget. A room's
    /// current replay is never touched, nor a replay whose file lock is held.
    pub fn trim_cache(&self) {
        let cache = self.cache_lock.lock().unwrap();
        let rooms = self.all_rooms();
        let (mut current, mut recent, mut named) = (HashSet::new(), HashSet::new(), HashSet::new());
        for room in &rooms {
            room.read(|st| {
                if let Some(r) = &st.replay {
                    current.insert(r.key.clone());
                    recent.insert(r.key.clone());
                    named.insert(r.key.clone());
                }
                for (i, p) in st.history.iter().enumerate() {
                    named.insert(p.replay.key.clone());
                    if i < KEEP_UNFETCHABLE {
                        recent.insert(p.replay.key.clone());
                    }
                }
            });
        }
        let mut changed = Vec::new();
        self.trim_locked(&current, &recent, &named, &mut changed);
        drop(cache);
        // Histories show whether a file can be had.
        for key in changed {
            self.replay_changed(&key);
        }
    }

    fn trim_locked(
        &self,
        current: &HashSet<String>,
        recent: &HashSet<String>,
        named: &HashSet<String>,
        changed: &mut Vec<String>,
    ) {
        let mut replays = self.replays.lock().unwrap();
        let unnamed: Vec<String> = replays.keys().filter(|k| !named.contains(*k)).cloned().collect();
        for key in unnamed {
            let replay = replays[&key].clone();
            let Ok(_held) = replay.file_lock.try_lock() else { continue };
            replays.remove(&key);
            replay.delete();
            crate::log(&format!("replay {key}: no room names it any more; deleted"));
        }
        for replay in replays.values() {
            if replay.has_file() && !self.refetchable(replay) && !recent.contains(&replay.key) {
                let Ok(_held) = replay.file_lock.try_lock() else { continue };
                replay.drop_file();
                changed.push(replay.key.clone());
                crate::log(&format!(
                    "replay {}: file deleted (it cannot be fetched again, and no room has it recently)",
                    replay.key
                ));
            }
        }
        let mut total: u64 = replays.values().map(|r| r.file_size()).sum();
        if total <= self.cfg.replay_cache {
            return;
        }
        let mut evictable: Vec<(u64, Arc<Replay>)> = replays
            .values()
            .filter(|r| r.has_file() && self.refetchable(r) && !current.contains(&r.key))
            .map(|r| (r.last_used(), r.clone()))
            .collect();
        evictable.sort_by_key(|(used, _)| *used);
        for (_, replay) in evictable {
            if total <= self.cfg.replay_cache {
                break;
            }
            let Ok(_held) = replay.file_lock.try_lock() else { continue };
            let size = replay.file_size();
            replay.drop_file();
            total -= size;
            crate::log(&format!("replay {}: file evicted from the cache ({} KiB)", replay.key, size >> 10));
        }
    }

    /// Wakes everyone waiting on a room that names `key` (its view shows the replay).
    pub fn replay_changed(&self, key: &str) {
        for room in self.all_rooms() {
            let names = room
                .read(|st| st.replay.as_ref().is_some_and(|r| r.key == key) || st.history.iter().any(|p| p.replay.key == key));
            if names {
                room.mutate(|_| ());
            }
        }
    }

    /// Whether some room playing this replay has someone in it: demand for its
    /// statistics (DESIGN §4.1).
    pub fn watched(&self, replay: &Replay) -> bool {
        self.all_rooms().iter().any(|room| {
            room.read(|st| {
                !st.closed
                    && st.replay.as_ref().is_some_and(|r| r.key == replay.key)
                    && (!st.members.is_empty() || !st.in_game.is_empty())
            })
        })
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
        write_json(&self.cfg.data.join("tokens.json"), &tokens)
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

// -- replays ---------------------------------------------------------------------

/// A replay's record, `replays/<key>/meta.json`, next to its file while the cache
/// keeps that.
#[derive(Serialize, Deserialize)]
struct ReplayMeta {
    key: String,
    info: ReplayInfo,
    /// The game's id at the stats service, once it has confirmed it holds the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    game_id: Option<String>,
    /// The BAR replay id it was fetched by; absent for an upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bar_replay: Option<String>,
    /// For the cache: when a room last picked it, left it or started it.
    #[serde(default)]
    last_used_unix: u64,
}

pub struct ReplayState {
    info: ReplayInfo,
    game_id: Option<String>,
    last_used_unix: u64,
    /// The parsed file, while the file is there.
    demo: Option<Arc<Demo>>,
    file_size: u64,
    /// The record was deleted; its directory is gone or going.
    closed: bool,
}

pub struct Replay {
    pub key: String,
    dir: PathBuf,
    /// The game id in the replay's header: the key, when it is one.
    pub header_game_id: Option<String>,
    /// See [`ReplayMeta::bar_replay`].
    pub bar_replay: Option<String>,
    /// The game's statistics, shared by every room playing it.
    pub stats: crate::timeline::Slot,
    state: Mutex<ReplayState>,
    /// Held from getting the file to a room naming the replay as its current one.
    pub file_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Replay {
    fn new(dir: PathBuf, meta: ReplayMeta, demo: Option<(Demo, u64)>) -> Replay {
        let header_game_id = (!meta.key.starts_with('u')).then(|| meta.key.clone());
        // A stored id that is not the header's (a hand edit) is not used.
        let game_id = meta.game_id.filter(|g| Some(g) == header_game_id.as_ref());
        let (demo, file_size) = match demo {
            Some((d, n)) => (Some(Arc::new(d)), n),
            None => (None, 0),
        };
        Replay {
            stats: crate::timeline::Slot::new(&meta.key, game_id.clone()),
            key: meta.key,
            dir,
            header_game_id,
            bar_replay: meta.bar_replay,
            state: Mutex::new(ReplayState {
                info: meta.info,
                game_id,
                last_used_unix: meta.last_used_unix,
                demo,
                file_size,
                closed: false,
            }),
            file_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn load(dir: &Path) -> Result<Replay> {
        let meta: ReplayMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json"))?).context("parsing meta.json")?;
        anyhow::ensure!(valid_key(&meta.key), "malformed replay key");
        anyhow::ensure!(dir.file_name().is_some_and(|n| n == meta.key.as_str()), "replay key is not the directory's name");
        let file = dir.join("replay.sdfz");
        let demo = match std::fs::metadata(&file) {
            Ok(m) => match Demo::read(&file) {
                Ok(d) => Some((d, m.len())),
                Err(e) => {
                    crate::log(&format!("replay {}: unreadable file dropped: {e:#}", meta.key));
                    let _ = std::fs::remove_file(&file);
                    None
                }
            },
            Err(_) => None,
        };
        Ok(Replay::new(dir.to_path_buf(), meta, demo))
    }

    fn read<R>(&self, f: impl FnOnce(&ReplayState) -> R) -> R {
        f(&self.state.lock().unwrap())
    }

    /// Writes `meta.json` from `st` (held by the caller): never after the record was
    /// deleted, so a deleted replay's directory is not made again.
    fn save_meta(&self, st: &ReplayState) -> Result<()> {
        if st.closed {
            return Ok(());
        }
        let meta = ReplayMeta {
            key: self.key.clone(),
            info: st.info.clone(),
            game_id: st.game_id.clone(),
            bar_replay: self.bar_replay.clone(),
            last_used_unix: st.last_used_unix,
        };
        write_json(&self.dir.join("meta.json"), &meta)
    }

    pub fn replay_path(&self) -> PathBuf {
        self.dir.join("replay.sdfz")
    }

    /// The parsed file; `None` while the cache does not hold it.
    pub fn demo(&self) -> Option<Arc<Demo>> {
        self.read(|st| st.demo.clone())
    }

    pub fn has_file(&self) -> bool {
        self.read(|st| st.demo.is_some())
    }

    fn file_size(&self) -> u64 {
        self.read(|st| if st.demo.is_some() { st.file_size } else { 0 })
    }

    pub fn info(&self) -> ReplayInfo {
        self.read(|st| st.info.clone())
    }

    /// The game's id at the stats service, once it has confirmed it holds the game.
    pub fn game_id(&self) -> Option<String> {
        self.read(|st| st.game_id.clone())
    }

    fn last_used(&self) -> u64 {
        self.read(|st| st.last_used_unix)
    }

    /// The record was deleted (no room names it any more).
    pub fn is_closed(&self) -> bool {
        self.read(|st| st.closed)
    }

    /// The names of the replay's players, which a viewer's engine must not use.
    pub fn player_names(&self) -> Vec<String> {
        self.read(|st| match &st.demo {
            Some(d) => d.player_names(),
            None => st.info.teams.iter().flatten().chain(&st.info.spectators).cloned().collect(),
        })
    }

    /// Records the stats service's id for the game, in memory and on disk. `true` if
    /// that is news (rooms showing the replay should be told: [`App::replay_changed`]).
    pub fn set_game_id(&self, game_id: &str) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        if st.closed || st.game_id.as_deref() == Some(game_id) {
            return Ok(false);
        }
        st.game_id = Some(game_id.to_owned());
        self.save_meta(&st)?;
        Ok(true)
    }

    pub fn set_map_file(&self, map_file: String) {
        let mut st = self.state.lock().unwrap();
        if st.info.map_file.as_deref() != Some(map_file.as_str()) {
            st.info.map_file = Some(map_file);
            let _ = self.save_meta(&st);
        }
    }

    /// Notes a use, for the cache's "least recently used".
    fn touch(&self, now: u64) {
        let mut st = self.state.lock().unwrap();
        st.last_used_unix = now;
        if let Err(e) = self.save_meta(&st) {
            crate::log(&format!("replay {}: saving: {e:#}", self.key));
        }
    }

    pub fn touch_now(&self) {
        self.touch(unix_now());
    }

    /// Stores the file (the caller holds the file lock). `demo` is its parsed form,
    /// if the caller has it.
    pub fn store_file(&self, bytes: &[u8], demo: Option<Demo>) -> Result<()> {
        let demo = match demo {
            Some(d) => d,
            None => Demo::from_gz_bytes(bytes).context("this is not a readable replay")?,
        };
        anyhow::ensure!(!self.is_closed(), "the replay was deleted meanwhile");
        let path = self.replay_path();
        let tmp = path.with_extension("sdfz.tmp");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)?;
        let mut st = self.state.lock().unwrap();
        st.demo = Some(Arc::new(demo));
        st.file_size = bytes.len() as u64;
        Ok(())
    }

    /// Deletes the file, keeping the record.
    fn drop_file(&self) {
        let mut st = self.state.lock().unwrap();
        st.demo = None;
        st.file_size = 0;
        let _ = std::fs::remove_file(self.replay_path());
    }

    /// Deletes the record and the file.
    fn delete(&self) {
        let mut st = self.state.lock().unwrap();
        st.closed = true;
        st.demo = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// -- rooms -----------------------------------------------------------------------

/// A replay a room had, and when it picked it.
#[derive(Clone)]
pub struct Picked {
    pub replay: Arc<Replay>,
    pub picked_unix: u64,
}

/// A room as stored, `rooms/<uid>.json`.
#[derive(Serialize, Deserialize)]
struct RoomFile {
    uid: String,
    link: String,
    name: String,
    created_unix: u64,
    last_used_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay: Option<PickedFile>,
    #[serde(default)]
    history: Vec<PickedFile>,
}

#[derive(Serialize, Deserialize)]
struct PickedFile {
    key: String,
    picked_unix: u64,
}

/// A registered desktop app.
pub struct Member {
    pub id: String,
    /// Unique among the room's members.
    pub name: String,
    /// The name its engine uses for the current replay.
    pub engine_name: String,
    pub state: ViewerState,
    pub detail: String,
    /// The replay key `state` is about.
    pub replay: Option<String>,
    pub last_seen: Instant,
}

pub struct RoomState {
    pub rev: u64,
    /// The id in the room's link.
    pub link: String,
    pub name: String,
    pub created_unix: u64,
    pub last_used_unix: u64,
    pub replay: Option<Arc<Replay>>,
    /// When the current replay was picked.
    pub picked_unix: u64,
    /// Newest first; never holds the current replay.
    pub history: Vec<Picked>,
    pub phase: Phase,
    pub note: Option<String>,
    pub game_port: Option<u16>,
    pub run: u32,
    pub paused: bool,
    pub speed: f32,
    pub ended: bool,
    pub members: Vec<Member>,
    /// Player number → name of everyone the hosting server has connected.
    pub in_game: BTreeMap<u8, String>,
    /// Command channel of the task that owns the hosting server, while one runs.
    pub runner: Option<mpsc::UnboundedSender<RunnerCmd>>,
    /// The last jump issued in this run.
    pub jump: Option<JumpView>,
    /// The room was deleted.
    pub closed: bool,
}

impl RoomState {
    fn new(
        link: String,
        name: String,
        created_unix: u64,
        last_used_unix: u64,
        replay: Option<Arc<Replay>>,
        history: Vec<Picked>,
        picked_unix: u64,
    ) -> RoomState {
        let phase = match &replay {
            None => Phase::Empty,
            // The engine is checked again when the room is started.
            Some(r) if r.has_file() => Phase::Idle,
            Some(_) => Phase::Failed,
        };
        let note = (phase == Phase::Failed).then(|| "The server no longer has this replay's file; pick it again.".to_owned());
        RoomState {
            rev: 1,
            link,
            name,
            created_unix,
            last_used_unix,
            replay,
            picked_unix,
            history,
            phase,
            note,
            game_port: None,
            run: 0,
            paused: false,
            speed: 1.0,
            ended: false,
            members: Vec::new(),
            in_game: BTreeMap::new(),
            runner: None,
            jump: None,
            closed: false,
        }
    }

    /// An engine name for `base` that clashes with nothing in the room.
    fn fresh_engine_name(&self, base: &str) -> String {
        let mut taken = self.replay.as_ref().map(|r| r.player_names()).unwrap_or_default();
        taken.extend(self.members.iter().map(|m| m.engine_name.clone()));
        taken.extend(self.in_game.values().cloned());
        names::unique(base, &taken)
    }

    /// Gives every member an engine name for the current replay, in joining order.
    fn reassign_engine_names(&mut self) {
        let mut taken = self.replay.as_ref().map(|r| r.player_names()).unwrap_or_default();
        taken.extend(self.in_game.values().cloned());
        for m in &mut self.members {
            m.engine_name = names::unique(&m.name, &taken);
            taken.push(m.engine_name.clone());
        }
    }
}

pub struct Room {
    /// The room's own id: names its file and run directory, and stays when the link
    /// changes.
    pub uid: String,
    file: PathBuf,
    state: Mutex<RoomState>,
    rev: watch::Sender<u64>,
    /// Counts changes of the current replay (and the room's deletion), for timeline
    /// calls waiting on one replay's statistics.
    picks: watch::Sender<u64>,
    /// Held while a replay is picked, a run is started or the room is deleted.
    pub pick_lock: tokio::sync::Mutex<()>,
}

impl Room {
    fn new(uid: String, file: PathBuf, state: RoomState) -> Room {
        Room {
            uid,
            file,
            state: Mutex::new(state),
            rev: watch::channel(1).0,
            picks: watch::channel(0).0,
            pick_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn load(path: &Path, replays: &HashMap<String, Arc<Replay>>) -> Result<Room> {
        let f: RoomFile = serde_json::from_slice(&std::fs::read(path)?).context("parsing the room")?;
        anyhow::ensure!(names::valid_id(&f.uid) && names::valid_id(&f.link), "malformed room id");
        anyhow::ensure!(path.file_stem().is_some_and(|s| s == f.uid.as_str()), "room id is not the file's name");
        let find = |p: &PickedFile| replays.get(&p.key).cloned();
        let (replay, picked_unix) = match &f.replay {
            Some(p) => (find(p), p.picked_unix),
            None => (None, 0),
        };
        let history = f.history.iter().filter_map(|p| Some(Picked { replay: find(p)?, picked_unix: p.picked_unix })).collect();
        let state =
            RoomState::new(f.link, clean_room_name(&f.name), f.created_unix, f.last_used_unix, replay, history, picked_unix);
        Ok(Room::new(f.uid, path.to_path_buf(), state))
    }

    /// Writes the room's file from `st` (held by the caller): never after the room
    /// was deleted.
    fn save(&self, st: &RoomState) -> Result<()> {
        if st.closed {
            return Ok(());
        }
        let picked = |p: &Picked| PickedFile { key: p.replay.key.clone(), picked_unix: p.picked_unix };
        let f = RoomFile {
            uid: self.uid.clone(),
            link: st.link.clone(),
            name: st.name.clone(),
            created_unix: st.created_unix,
            last_used_unix: st.last_used_unix,
            replay: st.replay.as_ref().map(|r| PickedFile { key: r.key.clone(), picked_unix: st.picked_unix }),
            history: st.history.iter().map(picked).collect(),
        };
        write_json(&self.file, &f)
    }

    pub fn is_closed(&self) -> bool {
        self.read(|st| st.closed)
    }

    pub fn current(&self) -> Option<Arc<Replay>> {
        self.read(|st| st.replay.clone())
    }

    /// Notes that the room was used now (a run started), on disk too.
    pub fn touch_used(&self) {
        self.touch(|st| {
            st.last_used_unix = unix_now();
            if let Err(e) = self.save(st) {
                crate::log(&format!("room {}: saving: {e:#}", self.uid));
            }
        });
    }

    pub fn read<R>(&self, f: impl FnOnce(&RoomState) -> R) -> R {
        f(&self.state.lock().unwrap())
    }

    /// Changes the state and wakes everyone waiting on this room.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut RoomState) -> R) -> R {
        let mut st = self.state.lock().unwrap();
        let r = f(&mut st);
        st.rev += 1;
        // Published under the lock, so waiters never see revisions out of order.
        self.rev.send_replace(st.rev);
        r
    }

    /// Like [`mutate`](Self::mutate) for changes nobody is waiting to see.
    pub fn touch<R>(&self, f: impl FnOnce(&mut RoomState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Returns once the room has changed since `rev`, or after `wait`.
    pub async fn changed_since(&self, rev: u64, wait: Duration) {
        let mut rx = self.rev.subscribe();
        let _ = tokio::time::timeout(wait, rx.wait_for(|current| *current > rev)).await;
    }

    /// How many times the current replay has changed; see [`picked_since`](Self::picked_since).
    pub fn picks(&self) -> u64 {
        *self.picks.borrow()
    }

    /// Returns once the current replay has changed since [`picks`](Self::picks) said
    /// `seen`, or after `wait`.
    pub async fn picked_since(&self, seen: u64, wait: Duration) {
        let mut rx = self.picks.subscribe();
        let _ = tokio::time::timeout(wait, rx.wait_for(|current| *current != seen)).await;
    }

    pub fn view(&self, app: &App) -> RoomView {
        let st = self.state.lock().unwrap();
        let current = st.replay.as_ref().map(|r| r.key.clone());
        let in_game = |name: &str| st.in_game.values().any(|n| n == name);
        let mut members: Vec<ViewerView> = st
            .members
            .iter()
            .map(|m| {
                // A report about another replay says nothing about this one.
                let about_current = m.replay == current;
                ViewerView {
                    name: m.name.clone(),
                    state: if about_current { m.state } else { ViewerState::Preparing },
                    detail: if about_current { m.detail.clone() } else { String::new() },
                    in_game: in_game(&m.engine_name),
                    engine_name: m.engine_name.clone(),
                    replay: m.replay.clone(),
                }
            })
            .collect();
        // Engines that connected without going through the app.
        for name in st.in_game.values() {
            if !st.members.iter().any(|m| &m.engine_name == name) {
                members.push(ViewerView {
                    name: name.clone(),
                    state: ViewerState::Playing,
                    detail: String::new(),
                    in_game: true,
                    engine_name: name.clone(),
                    replay: current.clone(),
                });
            }
        }
        let match_url = |r: &Replay| r.game_id().and_then(|g| app.match_url(&g));
        RoomView {
            id: st.link.clone(),
            rev: st.rev,
            name: st.name.clone(),
            phase: st.phase,
            note: st.note.clone(),
            game_host: app.cfg.game_host.clone(),
            game_port: st.game_port,
            run: st.run,
            paused: st.paused,
            speed: st.speed,
            ended: st.ended,
            jump: st.jump,
            members,
            replay: st.replay.as_ref().map(|r| RoomReplay {
                key: r.key.clone(),
                info: r.info(),
                game_id: r.game_id(),
                match_url: match_url(r),
                picked_unix: st.picked_unix,
            }),
            history: st
                .history
                .iter()
                .map(|p| HistoryEntry {
                    key: p.replay.key.clone(),
                    info: p.replay.info(),
                    picked_unix: p.picked_unix,
                    available: p.replay.has_file() || app.refetchable(&p.replay),
                    game_id: p.replay.game_id(),
                    match_url: match_url(&p.replay),
                })
                .collect(),
        }
    }

    /// Registers a desktop app; returns its secret id and its name in the room.
    ///
    /// `playing_as` is the name of an engine this app already has in the game (it is
    /// registering again after the server forgot it); that name is kept as its engine
    /// name if no other member's engine has it. `None` when the room is full.
    pub fn join(&self, wanted_name: &str, playing_as: Option<&str>) -> Option<(String, String)> {
        self.mutate(|st| {
            if st.members.len() >= MAX_VIEWERS {
                return None;
            }
            let others: Vec<String> = st.members.iter().map(|m| m.name.clone()).collect();
            let name = names::unique(wanted_name, &others);
            let engine_name = match playing_as {
                Some(current)
                    if st.in_game.values().any(|n| n == current) && !st.members.iter().any(|m| m.engine_name == current) =>
                {
                    current.to_owned()
                }
                _ => st.fresh_engine_name(&name),
            };
            let id = random_id(22);
            st.members.push(Member {
                id: id.clone(),
                name: name.clone(),
                engine_name,
                state: ViewerState::Preparing,
                detail: String::new(),
                replay: None,
                last_seen: Instant::now(),
            });
            Some((id, name))
        })
    }

    /// Records a viewer's report about the replay `replay`. `false` if the viewer is
    /// unknown (expired).
    pub fn report(&self, viewer_id: &str, state: ViewerState, detail: &str, replay: Option<&str>) -> bool {
        let detail: String = detail.chars().filter(|c| !c.is_control()).take(200).collect();
        let replay = replay.filter(|k| valid_key(k)).map(str::to_owned);
        let changed = self.touch(|st| {
            let m = st.members.iter_mut().find(|m| m.id == viewer_id)?;
            m.last_seen = Instant::now();
            let changed = m.state != state || m.detail != detail || m.replay != replay;
            m.state = state;
            m.detail = detail;
            m.replay = replay;
            Some(changed)
        });
        if changed == Some(true) {
            self.mutate(|_| ());
        }
        changed.is_some()
    }

    pub fn leave(&self, viewer_id: &str) {
        self.mutate(|st| st.members.retain(|m| m.id != viewer_id));
    }

    /// Drops members that stopped reporting.
    pub fn expire_viewers(&self) {
        let stale = self.read(|st| st.members.iter().any(|m| m.last_seen.elapsed() > VIEWER_TTL));
        if stale {
            self.mutate(|st| st.members.retain(|m| m.last_seen.elapsed() <= VIEWER_TTL));
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

    #[test]
    fn room_names_are_cleaned() {
        assert_eq!(clean_room_name("  Friday night  "), "Friday night");
        assert_eq!(clean_room_name("a\u{7}b\nc"), "abc");
        assert_eq!(clean_room_name(" \t "), "Room");
        assert_eq!(clean_room_name(""), "Room");
        assert_eq!(clean_room_name(&"é".repeat(100)).chars().count(), MAX_ROOM_NAME);
    }

    #[test]
    fn keys() {
        assert!(valid_key(&"ab".repeat(16)));
        assert!(valid_key(&format!("u{}", "0".repeat(31))));
        assert!(!valid_key(&"AB".repeat(16)));
        assert!(!valid_key("../../etc/passwd"));
        assert!(!valid_key(&format!("x{}", "0".repeat(31))));
        // Not a replay: no game id, so a hash.
        let k = replay_key(b"not a replay");
        assert!(k.starts_with('u') && valid_key(&k), "{k}");
        assert_eq!(k, replay_key(b"not a replay"));
        assert_ne!(k, replay_key(b"another"));
    }
}
