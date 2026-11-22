//! The window: be in a room, browse local replays, settings.

use crate::backend::{self, Backend, Cmd, Msg};
use crate::client::{self, Pick, Stage};
use crate::config::Config;
use crate::datadir::{self, DataDir};
use crate::position::{self, Position};
use crate::replays::LocalReplay;
use crate::timeline::{self, icons, Icons, Timeline};
use crate::uri;
use barwatch_core::api::{self, Archived, HistoryEntry, JumpView, Link, Phase, ReplayInfo, RoomView, TimelineView, ViewerState};
use damascene_core::prelude::*;
use damascene_core::widgets::{tabs, text_input};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const TAB_ROOM: &str = "room";
const TAB_REPLAYS: &str = "replays";
const TAB_SETTINGS: &str = "settings";
const REPLAY_ROW_HEIGHT: f32 = 52.0;
const CHAT_HINT: &str = "In game chat: !skip 14:00   !skip +2m   !pause   !resume   !speed 2";
/// Rooms offered by *Watch together*: the server's most recently used.
const TOGETHER_ROOMS: usize = 6;

/// How often the window is drawn while the game line moves: the clock on it
/// counts seconds, and the line creeps a pixel in about as long.
const GAME_LINE_FRAME: Duration = Duration::from_millis(250);

/// The room this app is in (or was, until the user dismisses it).
struct Room {
    server: Url,
    /// The id in the room's link.
    id: String,
    update: Option<client::Update>,
    /// The client has stopped: left, failed, or the room is gone.
    over: bool,
    /// The statistics timeline, from the first time this viewer is in game.
    watch: Option<Watch>,
    art: Art,
}

/// The timeline of the game this viewer is watching (DESIGN.md §5.7). It is
/// kept while the room plays one replay, so that what the viewer chose to look
/// at stays when the replay is started again; it is on screen while this viewer
/// is in the game, and after the game has ended for those who were.
struct Watch {
    /// The replay it is of (`RoomReplay::key`).
    replay: String,
    timeline: Timeline,
    /// The run of the room this viewer was last in the game of.
    run: u32,
    /// This viewer's engine was in the running game at the last update.
    in_game: bool,
    /// The statistics the timeline was given last, as the client handed them over.
    applied: Option<Arc<TimelineView>>,
    /// The jump the server named last. One that differs from it is new and
    /// under way; the one already named when this viewer got into the game is
    /// not (the engine is catching up to the room, not to that jump).
    jump_seen: Option<JumpView>,
    /// The engine's report before the latest, to tell how fast it goes.
    last_report: Option<Position>,
}

/// Marker pictures and unit names from the installed game (DESIGN.md §5.6).
#[derive(Default)]
struct Art {
    loading: Option<icons::Loading>,
    /// Picture names asked for so far: each is read once.
    asked: HashSet<String>,
    /// The game was asked at all (for its unit names, when no marker has a picture).
    started: bool,
    /// The statistics whose markers were looked through last: only new ones can
    /// name new pictures.
    looked_at: Option<Arc<TimelineView>>,
    icons: Icons,
}

impl Room {
    fn new(server: Url, id: String) -> Room {
        Room { server, id, update: None, over: false, watch: None, art: Art::default() }
    }

    fn view(&self) -> Option<&RoomView> {
        self.update.as_ref()?.room.as_ref()
    }

    /// The statistics of the room's current replay, once the server has said any.
    fn timeline(&self) -> Option<&Arc<TimelineView>> {
        let update = self.update.as_ref()?;
        let key = update.room.as_ref()?.replay.as_ref().map(|r| &r.key);
        update.timeline.as_ref().filter(|t| t.replay.as_ref() == key)
    }

    /// Whether this viewer's engine is in the running game: open, and started
    /// for this run (one left open after the last run ended is not).
    fn in_game(&self) -> bool {
        self.update.as_ref().is_some_and(|u| {
            u.stage == Stage::Playing && u.room.as_ref().is_some_and(|r| r.phase == Phase::Running && u.engine_run == Some(r.run))
        })
    }

    /// The timeline, while it is on screen: this viewer is in the game, or
    /// was when it ended (the whole game is then there to look back over; a
    /// viewer who was not in it may still want to watch it unspoiled).
    fn watching(&self) -> Option<&Watch> {
        let room = self.view()?;
        let key = &room.replay.as_ref()?.key;
        self.watch.as_ref().filter(|w| &w.replay == key && w.run == room.run && (self.in_game() || room.ended))
    }

    /// Takes in what an update says for the timeline: another replay, a new
    /// run, new statistics, a jump.
    fn sync_watch(&mut self) {
        let in_game = self.in_game();
        let statistics = self.timeline().cloned();
        let Some(update) = &self.update else { return };
        let Some(room) = &update.room else { return };
        // Another replay: what was watched, and the pictures read for it, are of
        // the one before.
        if self.watch.as_ref().is_some_and(|w| room.replay.as_ref().is_none_or(|r| r.key != w.replay)) {
            self.watch = None;
            self.art = Art::default();
        }
        let Some(replay) = &room.replay else { return };
        if in_game && self.watch.is_none() {
            self.watch = Some(Watch {
                replay: replay.key.clone(),
                timeline: Timeline::waiting(replay_length(&replay.info) as f32),
                run: room.run,
                in_game: false,
                applied: None,
                jump_seen: None,
                last_report: None,
            });
        }
        let Some(watch) = &mut self.watch else { return };
        // This viewer's engine has just got into a game: a new run, or the
        // same one again. Either way it starts from the beginning.
        if in_game && (!watch.in_game || watch.run != room.run) {
            watch.timeline.restart();
            watch.timeline.await_pace();
            watch.run = room.run;
            watch.jump_seen = room.jump;
            watch.last_report = None;
        }
        watch.in_game = in_game;
        // A new report: an engine that went much further than its speed
        // allows since the one before is running ahead to the room.
        if let Some(position) = update.position.filter(|p| watch.last_report.is_none_or(|l| l.seen != p.seen)) {
            if let Some(last) = watch.last_report {
                watch.timeline.set_catching_up(catching_up(&last, &position));
            }
            watch.last_report = Some(position);
        } else if update.position.is_none() && watch.last_report.is_some() {
            // Silent (stuck, or closed): whatever it was doing, it is not
            // running ahead now.
            watch.timeline.set_catching_up(false);
        }
        if let Some(view) = &statistics {
            if !watch.applied.as_ref().is_some_and(|applied| Arc::ptr_eq(applied, view)) {
                let (feed, data) = timeline::statistics(view, replay_length(&replay.info));
                watch.timeline.set_statistics(feed, data);
                watch.applied = Some(view.clone());
            }
        }
        if watch.run == room.run && room.jump != watch.jump_seen {
            watch.jump_seen = room.jump;
            if let (Some(jump), true) = (room.jump, in_game) {
                watch.timeline.show_jump(jump.target_s as f32);
            }
        }
        if update.jump_refused {
            watch.timeline.cancel_jump();
        }
    }

    /// Before every frame: where the game is now.
    fn tick_watch(&mut self) {
        let in_game = self.in_game();
        let (Some(update), Some(watch)) = (&self.update, &mut self.watch) else { return };
        let Some(room) = update.room.as_ref().filter(|r| r.run == watch.run) else { return };
        if room.ended {
            // The replay has played to its end, wherever the last frame fell.
            watch.timeline.set_game(f32::MAX);
        } else if let Some(seconds) = update.position.and_then(|p| p.seconds_at(Instant::now())) {
            watch.timeline.set_game(seconds as f32);
        } else if in_game {
            watch.timeline.set_game_silent();
        }
    }

    /// Whether the game line is moving by itself right now, between reports.
    fn game_line_moves(&self) -> bool {
        let Some(update) = &self.update else { return false };
        let running = update.position.is_some_and(|p| !p.report.paused && p.report.speed > 0.0 && p.is_fresh(Instant::now()));
        running && !update.room.as_ref().is_some_and(|r| r.ended)
    }

    /// Before every frame: marker pictures and unit names, read from the
    /// installed game once it is there, and again when new markers name
    /// units not asked for yet.
    fn tick_art(&mut self, data: &DataDir, wake: impl FnOnce() + Send + 'static) {
        let (Some(update), Some(watch)) = (&self.update, &mut self.watch) else { return };
        if let Some(result) = self.art.loading.as_ref().and_then(icons::Loading::poll) {
            self.art.loading = None;
            match result {
                Ok(art) => {
                    eprintln!("barwatch: {}", art.summary());
                    self.art.icons.merge(art.icons);
                    watch.timeline.set_icons(self.art.icons.clone());
                    watch.timeline.set_unit_names(art.names);
                }
                Err(e) => eprintln!("barwatch: no marker pictures from the installed game ({e:#}); markers use glyphs"),
            }
        }
        let installed = matches!(update.stage, Stage::Ready | Stage::Playing | Stage::Closed);
        let replay = update.room.as_ref().and_then(|r| r.replay.as_ref()).filter(|r| r.key == watch.replay);
        let Some(replay) = replay.filter(|_| installed && self.art.loading.is_none()) else { return };
        let same = |a: &Option<Arc<TimelineView>>, b: &Option<Arc<TimelineView>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if self.art.started && same(&self.art.looked_at, &watch.applied) {
            return;
        }
        self.art.looked_at = watch.applied.clone();
        let wanted: Vec<String> =
            watch.timeline.data.marker_pictures().into_iter().filter(|name| !self.art.asked.contains(name)).collect();
        if wanted.is_empty() && self.art.started {
            return;
        }
        self.art.started = true;
        self.art.asked.extend(wanted.iter().cloned());
        self.art.loading = Some(icons::Loading::start(data.root.clone(), replay.info.game.clone(), wanted, wake));
    }
}

/// *Watch together* from My replays, outside a room: which of the paired
/// server's rooms to play the replay in, or a new one.
struct Together {
    path: PathBuf,
    /// The replay's map, for the dialog's words.
    map: String,
    server: Url,
    token: String,
    /// The server's rooms, once it has answered.
    rooms: Option<Result<Vec<RoomView>, String>>,
    /// The new room's name, as typed.
    name: String,
    /// A new room has been asked for.
    creating: bool,
}

/// A replay sent to the archive from My replays.
enum Archiving {
    Sending,
    Done(Archived),
}

pub struct Ui {
    backend: Backend,
    cfg: Config,
    data: DataDir,
    tab: String,
    selection: Selection,
    toasts: Vec<ToastSpec>,
    /// Keys to move keyboard focus to on the next frame.
    focus_requests: Vec<String>,
    /// Pages to open in the browser (a game's page on the stats service).
    link_opens: Vec<String>,
    clipboard: Option<arboard::Clipboard>,

    /// The invite link being typed or pasted.
    link: String,
    room: Option<Room>,
    /// A BAR replay link or id being typed or pasted into the room.
    bar_link: String,
    /// A link that arrived from outside (a browser) and awaits the user's consent.
    pending: Option<Link>,
    together: Option<Together>,
    archives: HashMap<PathBuf, Archiving>,

    replays: Arc<Vec<LocalReplay>>,
    /// Lowercased search text per replay, parallel to `replays`.
    haystacks: Vec<String>,
    scanning: bool,
    filter: String,
    /// Indices into `replays` that pass the filter, in list order.
    visible: Arc<Vec<usize>>,
    selected: Option<usize>,

    /// Map name → picture; `None` once a picture turned out to be unavailable.
    pictures: HashMap<String, Option<Image>>,

    name: String,
    data_dir: String,
    link_status: uri::Status,
    /// Settings are written to disk when they change (not in canned scenes).
    persist: bool,
}

impl Ui {
    pub fn new(backend: Backend, cfg: Config, data: DataDir, opened: Option<Link>) -> Ui {
        let name = if cfg.name.is_empty() { datadir::default_name(&data) } else { cfg.name.clone() };
        let mut ui = Ui {
            backend,
            data_dir: data.root.display().to_string(),
            data,
            cfg,
            tab: TAB_ROOM.into(),
            selection: Selection::default(),
            toasts: Vec::new(),
            focus_requests: Vec::new(),
            link_opens: Vec::new(),
            clipboard: None,
            link: String::new(),
            room: None,
            bar_link: String::new(),
            pending: opened,
            together: None,
            archives: HashMap::new(),
            replays: Arc::new(Vec::new()),
            haystacks: Vec::new(),
            scanning: false,
            filter: String::new(),
            visible: Arc::new(Vec::new()),
            selected: None,
            pictures: HashMap::new(),
            name,
            link_status: uri::status(),
            persist: true,
        };
        ui.scan();
        ui
    }

    fn scan(&mut self) {
        self.replays = Arc::new(Vec::new());
        self.haystacks.clear();
        self.visible = Arc::new(Vec::new());
        self.selected = None;
        self.scanning = true;
        self.backend.send(Cmd::Scan(self.data.demos()));
    }

    fn refilter(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        let visible: Vec<usize> =
            (0..self.replays.len()).filter(|&i| needle.is_empty() || self.haystacks[i].contains(&needle)).collect();
        self.visible = Arc::new(visible);
    }

    fn want_picture(&mut self, map: &str) {
        if !self.pictures.contains_key(map) {
            // Present-but-`None` doubles as "asked for": one request per map.
            self.pictures.insert(map.to_owned(), None);
            self.backend.send(Cmd::MapImage(map.to_owned()));
        }
    }

    fn join(&mut self, server: Url, room: String) {
        let settings = client::Settings { name: self.name.clone(), data: self.data.clone(), headless: false };
        self.backend.send(Cmd::Join { server: server.clone(), room: room.clone(), settings });
        self.room = Some(Room::new(server, room));
        self.tab = TAB_ROOM.into();
        self.link.clear();
        self.bar_link.clear();
    }

    /// Joins `room` and makes the replay at `path` its replay.
    fn join_and_pick(&mut self, server: Url, room: String, path: PathBuf) {
        self.join(server, room);
        // Queued behind the join: the client takes it once it is in.
        self.backend.send(Cmd::Room(client::Command::Pick(Pick::File(path))));
    }

    fn open_link(&mut self, link: Link) {
        match link {
            Link::Join { server, room } => self.join(server, room),
            Link::Pair { server, token } => {
                self.cfg.pair(server.as_str(), &token);
                self.save_config();
                self.toasts.push(ToastSpec::success(format!(
                    "Paired with {}. You can now make rooms and archive replays from My replays.",
                    host_of(&server)
                )));
            }
        }
    }

    fn save_config(&mut self) {
        if !self.persist {
            return;
        }
        if let Err(e) = self.cfg.save() {
            self.toasts.push(ToastSpec::error(format!("Could not save settings: {e:#}")));
        }
    }

    fn copy(&mut self, text: &str, what: &str) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        match self.clipboard.as_mut().map(|c| c.set_text(text.to_owned())) {
            Some(Ok(())) => self.toasts.push(ToastSpec::success(format!("{what} copied"))),
            _ => self.toasts.push(ToastSpec::error("Could not reach the clipboard")),
        }
    }

    fn invite_link(&self) -> Option<String> {
        let room = self.room.as_ref()?;
        api::invite_url(&room.server, &room.id).ok().map(String::from)
    }

    /// The room, while the app is in it (not after it left or lost it).
    fn live_room(&self) -> Option<&Room> {
        self.room.as_ref().filter(|r| !r.over)
    }

    fn to_room(&self, command: client::Command) {
        self.backend.send(Cmd::Room(command));
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Room(update) => {
                if let Some(notice) = &update.notice {
                    self.toasts.push(ToastSpec::warning(notice.clone()));
                }
                if let Some(map) = update.room.as_ref().and_then(|r| r.replay.as_ref()).map(|r| r.info.map.clone()) {
                    self.want_picture(&map);
                }
                let Some(room) = &mut self.room else { return };
                let server = room.server.to_string();
                // Remembered for the Join tab; forgotten once the server says it is gone.
                let changed = match (&update.stage, &update.room) {
                    (Stage::Gone, _) => self.cfg.forget_room(&server, &room.id),
                    (_, Some(view)) => self.cfg.remember_room(&server, &room.id, &view.name),
                    _ => false,
                };
                room.update = Some(*update);
                room.sync_watch();
                if changed {
                    self.save_config();
                }
            }
            Msg::RoomEnded => {
                if let Some(room) = &mut self.room {
                    room.over = true;
                }
            }
            Msg::Replays(batch) => {
                let mut all = (*self.replays).clone();
                self.haystacks.extend(batch.iter().map(LocalReplay::haystack));
                all.extend(batch);
                self.replays = Arc::new(all);
                self.refilter();
            }
            Msg::ScanDone => self.scanning = false,
            Msg::MapImage { map, picture } => {
                let image = picture.map(|p| Image::from_rgba8(p.width, p.height, p.rgba));
                self.pictures.insert(map, image);
            }
            Msg::Rooms { server, rooms } => {
                if let Some(together) = self.together.as_mut().filter(|t| t.server == server) {
                    together.rooms = Some(rooms);
                }
            }
            Msg::RoomCreated { server, room } => {
                let Some(together) = self.together.take_if(|t| t.server == server && t.creating) else { return };
                match room {
                    Ok(room) => {
                        if let Ok(invite) = api::invite_url(&server, &room.id) {
                            self.copy(invite.as_str(), "The new room's link");
                        }
                        self.join_and_pick(server, room.id, together.path);
                    }
                    Err(e) => {
                        self.toasts.push(ToastSpec::error(e));
                        self.together = Some(Together { creating: false, ..together });
                    }
                }
            }
            Msg::Archived { path, result } => match result {
                Ok(archived) => {
                    self.toasts.push(ToastSpec::success("The replay is in the archive"));
                    self.archives.insert(path, Archiving::Done(archived));
                }
                Err(e) => {
                    self.archives.remove(&path);
                    self.toasts.push(ToastSpec::error(e));
                }
            },
            Msg::Failed(e) => self.toasts.push(ToastSpec::error(e)),
            // A link opened while this window was up: it asks here, like one that
            // started the app.
            Msg::Opened(Some(text)) => match uri::parse_link(&text) {
                Ok(link) => self.pending = Some(link),
                Err(e) => self.toasts.push(ToastSpec::error(format!("That link could not be opened: {e:#}"))),
            },
            Msg::Opened(None) => self.toasts.push(ToastSpec::info("barwatch is already open.")),
        }
    }

    /// The server rooms are made on and replays archived to: the one paired last.
    fn paired_server(&self) -> Option<(Url, String)> {
        let s = self.cfg.servers.last()?;
        Some((api::parse_server(&s.url).ok()?, s.token.clone()))
    }

    fn selected_replay(&self) -> Option<&LocalReplay> {
        self.selected.and_then(|i| self.replays.get(i))
    }

    fn apply_settings(&mut self) {
        let name = self.name.trim().to_owned();
        let wanted = PathBuf::from(self.data_dir.trim());
        let detected = datadir::find(None);
        // Remember a folder only when it differs from what would be detected anyway.
        let (data, stored) = if self.data_dir.trim().is_empty() || wanted == detected.root {
            (detected, None)
        } else {
            (DataDir { root: wanted.clone(), own: false }, Some(wanted))
        };
        self.cfg.name = name;
        self.cfg.data_dir = stored;
        self.save_config();
        self.name = if self.cfg.name.is_empty() { datadir::default_name(&data) } else { self.cfg.name.clone() };
        self.data_dir = data.root.display().to_string();
        if data != self.data {
            self.data = data;
            self.scan();
        }
        self.toasts.push(ToastSpec::success("Settings saved"));
    }

    /// The events of the room tab, inside a room and on the way into one.
    /// `true` if `event` was one of them.
    fn on_room_event(&mut self, event: &UiEvent) -> bool {
        let clicked = matches!(event.kind, UiEventKind::Click | UiEventKind::Activate);
        if event.is_click_or_activate("join") || text_input::submitted(event, "link") {
            match uri::parse_link(&self.link) {
                // Typed or pasted by the user: that is the consent.
                Ok(link) => self.open_link(link),
                Err(e) => self.toasts.push(ToastSpec::error(format!("That link does not work: {e:#}"))),
            }
        } else if let Some(i) = event.route_index::<usize>("recent").filter(|_| clicked) {
            let entry = self.cfg.rooms.get(i).cloned();
            if let Some((server, entry)) = entry.and_then(|e| Some((api::parse_server(&e.server).ok()?, e))) {
                self.join(server, entry.room);
            }
        } else if let Some(i) = event.route_index::<usize>("forget").filter(|_| clicked) {
            if i < self.cfg.rooms.len() {
                self.cfg.rooms.remove(i);
                self.save_config();
            }
        } else if event.is_click_or_activate("room:start") {
            self.to_room(client::Command::Start);
        } else if event.is_click_or_activate("room:stop") {
            self.to_room(client::Command::Stop);
        } else if event.is_click_or_activate("room:rejoin") {
            self.to_room(client::Command::Rejoin);
        } else if event.is_click_or_activate("room:leave") {
            self.to_room(client::Command::Leave);
            self.room = None;
        } else if event.is_click_or_activate("room:copy") {
            if let Some(link) = self.invite_link() {
                self.copy(&link, "The room's link");
            }
        } else if event.is_click_or_activate("room:gex") {
            let url = self.room.as_ref().and_then(Room::view).and_then(|r| r.replay.as_ref()?.match_url.clone());
            self.link_opens.extend(url);
        } else if event.is_click_or_activate("room:pick-bar") || text_input::submitted(event, "bar-link") {
            let text = self.bar_link.trim().to_owned();
            if text.is_empty() {
                self.toasts.push(ToastSpec::info("Paste a link to a replay on BAR's site, or its id."));
            } else {
                self.to_room(client::Command::Pick(Pick::Bar(text)));
                self.bar_link.clear();
            }
        } else if let Some(i) = event.route_index::<usize>("history-watch").filter(|_| clicked) {
            let key = self.room.as_ref().and_then(Room::view).and_then(|r| r.history.get(i)).map(|h| h.key.clone());
            if let Some(key) = key {
                self.to_room(client::Command::Pick(Pick::History(key)));
            }
        } else if let Some(i) = event.route_index::<usize>("history-gex").filter(|_| clicked) {
            let url = self.room.as_ref().and_then(Room::view).and_then(|r| r.history.get(i)?.match_url.clone());
            self.link_opens.extend(url);
        } else {
            return false;
        }
        true
    }

    /// The events of My replays and of the *Watch together* dialog.
    fn on_replays_event(&mut self, event: &UiEvent) -> bool {
        let clicked = matches!(event.kind, UiEventKind::Click | UiEventKind::Activate);
        if event.is_click_or_activate("replays:refresh") {
            self.scan();
        } else if event.is_click_or_activate("replays:play") {
            let path = self.selected_replay().map(|r| r.path.clone());
            if let (Some(path), Some(_)) = (path, self.live_room()) {
                self.to_room(client::Command::Pick(Pick::File(path)));
                self.tab = TAB_ROOM.into();
            }
        } else if event.is_click_or_activate("replays:together") {
            let chosen = self.selected_replay().and_then(|r| Some((r.path.clone(), r.info.as_ref()?.map.clone())));
            if let (Some((path, map)), Some((server, token))) = (chosen, self.paired_server()) {
                self.backend.send(Cmd::ListRooms(backend::Paired { server: server.clone(), token: token.clone() }));
                self.together = Some(Together { path, map, server, token, rooms: None, name: String::new(), creating: false });
            }
        } else if event.is_click_or_activate("replays:archive") {
            let path = self.selected_replay().map(|r| r.path.clone());
            if let (Some(path), Some((server, token))) = (path, self.paired_server()) {
                if !self.archives.contains_key(&path) {
                    self.archives.insert(path.clone(), Archiving::Sending);
                    self.backend.send(Cmd::Archive { server: backend::Paired { server, token }, path });
                }
            }
        } else if event.is_click_or_activate("replays:gex") {
            let url = self.selected_replay().and_then(|r| match self.archives.get(&r.path) {
                Some(Archiving::Done(a)) => a.match_url.clone(),
                _ => None,
            });
            self.link_opens.extend(url);
        } else if let Some(index) = event.route_index::<usize>("replay").filter(|_| clicked) {
            self.selected = Some(index);
            if let Some(map) = self.replays.get(index).and_then(|r| r.info.as_ref()).map(|i| i.map.clone()) {
                self.want_picture(&map);
            }
        } else if let Some(i) = event.route_index::<usize>("pick-room").filter(|_| clicked) {
            let chosen = self.together.as_ref().and_then(|t| match &t.rooms {
                Some(Ok(rooms)) => rooms.get(i).map(|r| (t.server.clone(), r.id.clone(), t.path.clone())),
                _ => None,
            });
            if let Some((server, room, path)) = chosen {
                self.together = None;
                self.join_and_pick(server, room, path);
            }
        } else if event.is_click_or_activate("together:create") || text_input::submitted(event, "room-name") {
            if let Some(t) = self.together.as_mut().filter(|t| !t.creating) {
                t.creating = true;
                let paired = backend::Paired { server: t.server.clone(), token: t.token.clone() };
                self.backend.send(Cmd::CreateRoom { server: paired, name: t.name.clone() });
            }
        } else if event.is_click_or_activate("together:cancel")
            || (event.kind == UiEventKind::Click && event.is_route("together:dismiss"))
        {
            self.together = None;
        } else {
            return false;
        }
        true
    }
}

/// Setters for canned scenes ([`crate::fixtures`]).
impl Ui {
    /// A canned scene: settings stay in memory, whatever happens in it.
    pub fn fixture_in_memory(&mut self) {
        self.persist = false;
    }

    pub fn fixture_done_scanning(&mut self) {
        self.scanning = false;
    }

    pub fn fixture_room(&mut self, server: Url, update: client::Update) {
        let id = update.room.as_ref().map(|r| r.id.clone()).unwrap_or_default();
        self.room = Some(Room::new(server, id));
        self.fixture_update(update);
        // One report is all a canned scene has: its game goes at the room's
        // pace unless the scene says otherwise.
        if let Some(timeline) = self.fixture_timeline() {
            timeline.set_catching_up(false);
        }
    }

    /// The next thing the room's client says, as the window takes it in.
    pub fn fixture_update(&mut self, update: client::Update) {
        self.apply(Msg::Room(Box::new(update)));
        self.toasts.clear();
        if let Some(room) = &mut self.room {
            room.tick_watch();
        }
    }

    /// The room's client has ended (left, failed, or the room is gone).
    pub fn fixture_room_ended(&mut self) {
        self.apply(Msg::RoomEnded);
    }

    /// The room's timeline, once this viewer has been in game.
    pub fn fixture_timeline(&mut self) -> Option<&mut Timeline> {
        self.room.as_mut()?.watch.as_mut().map(|w| &mut w.timeline)
    }

    /// DEVELOPMENT: waits for the marker pictures of the installed game, for a
    /// one-frame render that has no later frame to show them in.
    pub fn fixture_wait_for_art(&mut self, timeout: Duration) {
        let Some(room) = &mut self.room else { return };
        room.tick_art(&self.data, || {});
        if let Some(loading) = room.art.loading.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(loading.wait(timeout));
            // Handed back as a finished load, for `tick_art` to take in.
            if let Ok(Some(result)) = rx.recv() {
                room.art.loading = Some(icons::Loading::ready(result));
            }
        }
        room.tick_art(&self.data, || {});
    }

    pub fn fixture_replays(&mut self, replays: Vec<LocalReplay>, selected: Option<usize>) {
        self.haystacks = replays.iter().map(LocalReplay::haystack).collect();
        self.replays = Arc::new(replays);
        self.refilter();
        self.selected = selected;
        self.tab = TAB_REPLAYS.into();
    }

    /// The selected replay has been sent to the archive.
    pub fn fixture_archived(&mut self, archived: Archived) {
        if let Some(path) = self.selected_replay().map(|r| r.path.clone()) {
            self.archives.insert(path, Archiving::Done(archived));
        }
    }

    /// *Watch together* open on the selected replay, the server having answered with `rooms`.
    pub fn fixture_together(&mut self, rooms: Vec<RoomView>) {
        let Some(replay) = self.selected_replay() else { return };
        let (path, map) = (replay.path.clone(), replay.info.as_ref().map(|i| i.map.clone()).unwrap_or_default());
        let Some((server, token)) = self.paired_server() else { return };
        self.together = Some(Together { path, map, server, token, rooms: Some(Ok(rooms)), name: String::new(), creating: false });
    }

    pub fn fixture_tab(&mut self, tab: &str) {
        self.tab = tab.into();
    }

    pub fn fixture_pending(&mut self, link: Link) {
        self.pending = Some(link);
    }
}

fn host_of(server: &Url) -> String {
    server.host_str().unwrap_or("the server").to_owned()
}

fn clock(seconds: u32) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn when(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).format("%-d %b %Y, %H:%M").to_string())
        .unwrap_or_default()
}

/// `text` cut to `chars` characters, with an ellipsis when it was longer.
fn short(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(chars.saturating_sub(1)).collect::<String>())
    }
}

fn versus(info: &ReplayInfo) -> String {
    let sides: Vec<String> = info.teams.iter().map(|t| t.join(", ")).collect();
    if sides.is_empty() {
        "no players".into()
    } else {
        sides.join("  vs  ")
    }
}

impl App for Ui {
    fn before_build(&mut self) {
        for msg in self.backend.drain() {
            self.apply(msg);
        }
        if let Some(room) = &mut self.room {
            room.tick_watch();
            room.tick_art(&self.data, self.backend.waker());
        }
    }

    fn build(&self, cx: &BuildCx) -> El {
        let body = match self.tab.as_str() {
            TAB_REPLAYS => self.replays_view(),
            TAB_SETTINGS => self.settings_view(),
            _ => self.room_tab(cx),
        };
        // The timeline's open list hangs from the timeline: only while that is drawn.
        let layers = self.watching().map(|w| w.timeline.layers()).unwrap_or_default();
        let dialogs = [self.pending.as_ref().map(consent_dialog), self.together.as_ref().map(|t| self.together_dialog(t))];
        overlays(
            page([
                tabs_list("tab", &self.tab, [(TAB_ROOM, "Room"), (TAB_REPLAYS, "My replays"), (TAB_SETTINGS, "Settings")]),
                body,
            ])
            .gap(tokens::SPACE_4),
            layers.into_iter().map(Some).chain(dialogs),
        )
    }

    fn shaders(&self) -> Vec<AppShader> {
        timeline::shaders()
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    fn drain_toasts(&mut self) -> Vec<ToastSpec> {
        std::mem::take(&mut self.toasts)
    }

    fn drain_focus_requests(&mut self) -> Vec<String> {
        let mut requests = std::mem::take(&mut self.focus_requests);
        requests.extend(self.watching_mut().and_then(|w| w.timeline.take_focus_request()));
        requests
    }

    fn drain_link_opens(&mut self) -> Vec<String> {
        std::mem::take(&mut self.link_opens)
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        if event.kind == UiEventKind::SelectionChanged {
            if let Some(sel) = event.selection.as_ref() {
                self.selection = sel.clone();
            }
            return;
        }
        if tabs::apply_event(&mut self.tab, &event, "tab", |s| Some(s.to_owned())) {
            return;
        }
        if let Some(target) = tab_out_of_field(&event) {
            self.focus_requests.push(target.to_owned());
            return;
        }

        // The consent dialog for a link that came from outside.
        if event.is_click_or_activate("consent:yes") {
            if let Some(link) = self.pending.take() {
                self.open_link(link);
            }
            return;
        }
        if declines_consent(&event) {
            self.pending = None;
            return;
        }

        // The timeline: everything under its own keys, and Escape at the window.
        if let Some(watch) = self.watching_mut() {
            if let Some(timeline::Action::Jump(seconds)) = watch.timeline.on_event(&event, cx) {
                self.backend.send(Cmd::Room(client::Command::Jump(seconds.round().max(0.0) as u32)));
            }
        }

        if self.on_room_event(&event) || self.on_replays_event(&event) {
            return;
        }
        // Settings.
        if event.is_click_or_activate("settings:save") {
            self.apply_settings();
        } else if event.is_click_or_activate("settings:detect") {
            self.data_dir = datadir::find(None).root.display().to_string();
        } else if event.is_click_or_activate("settings:register") {
            match uri::register() {
                Ok(()) => self.toasts.push(ToastSpec::success("barwatch:// links now open this app")),
                Err(e) => self.toasts.push(ToastSpec::error(format!("{e:#}"))),
            }
            self.link_status = uri::status();
        } else if let Some(index) =
            event.route_index::<usize>("unpair").filter(|_| matches!(event.kind, UiEventKind::Click | UiEventKind::Activate))
        {
            if index < self.cfg.servers.len() {
                self.cfg.servers.remove(index);
                self.save_config();
            }
        }

        // Text fields: each folds only what was routed to it.
        text_input::apply_event(&mut self.link, &mut self.selection, &event, "link");
        text_input::apply_event(&mut self.bar_link, &mut self.selection, &event, "bar-link");
        text_input::apply_event(&mut self.name, &mut self.selection, &event, "name");
        text_input::apply_event(&mut self.data_dir, &mut self.selection, &event, "data-dir");
        if let Some(t) = &mut self.together {
            text_input::apply_event(&mut t.name, &mut self.selection, &event, "room-name");
        }
        if text_input::apply_event(&mut self.filter, &mut self.selection, &event, "filter") {
            self.refilter();
        }
    }
}

/// The window host's own hooks: the timeline follows the pointer (its hover
/// line and readout), so it gets every pointer move while it is on screen.
impl barwatch_host::WinitWgpuApp for Ui {
    fn pointer_moved(&mut self, position: Option<(f32, f32)>, cx: &EventCx) -> bool {
        match self.watching_mut() {
            Some(watch) => watch.timeline.pointer_moved(position, cx),
            None => false,
        }
    }
}

/// A focused text field receives Tab as a raw key instead of giving up focus, so
/// the fields hand focus to their neighbours here: (field, previous, next).
const FIELD_NEIGHBOURS: [(&str, &str, &str); 6] = [
    ("link", "tab:tab:settings", "join"),
    ("bar-link", "tab:tab:settings", "room:pick-bar"),
    ("room-name", "together:cancel", "together:create"),
    ("filter", "tab:tab:settings", "replays:refresh"),
    ("name", "tab:tab:settings", "data-dir"),
    ("data-dir", "name", "settings:detect"),
];

/// Where focus goes when `event` is Tab or Shift+Tab pressed in a text field.
fn tab_out_of_field(event: &UiEvent) -> Option<&'static str> {
    if event.kind != UiEventKind::KeyDown {
        return None;
    }
    let press = event.key_press.as_ref()?;
    if press.logical.named() != Some(NamedKey::Tab) {
        return None;
    }
    let (_, previous, next) = FIELD_NEIGHBOURS.iter().find(|(field, _, _)| event.is_route(field))?;
    Some(if press.modifiers.shift { previous } else { next })
}

/// Whether `event` turns the consent dialog down: "Not now", or a click on the backdrop.
/// The backdrop is a hover target too, so its route alone is not enough: the pointer
/// merely crossing the window would close the dialog before it could be answered.
fn declines_consent(event: &UiEvent) -> bool {
    event.is_click_or_activate("consent:no") || (event.kind == UiEventKind::Click && event.is_route("consent:dismiss"))
}

fn consent_dialog(link: &Link) -> El {
    let (title, description, action) = match link {
        Link::Join { server, .. } => (
            "Join this room?",
            format!(
                "A link asks barwatch to join a room on {}. It will install what the room's replays need and start Beyond All Reason as a spectator when the room plays.",
                host_of(server)
            ),
            "Join",
        ),
        Link::Pair { server, .. } => (
            "Pair with this server?",
            format!(
                "A link asks to pair barwatch with {}. Pairing lets this app make rooms there and send your replays to its archive.",
                host_of(server)
            ),
            "Pair",
        ),
    };
    dialog(
        "consent",
        [
            dialog_header([dialog_title(title), dialog_description(description)]),
            dialog_footer([button("Not now").key("consent:no").ghost(), button(action).key("consent:yes").primary()]),
        ],
    )
}

impl Ui {
    /// The timeline, while it is on screen: in the room view, with no dialog over it.
    fn watching(&self) -> Option<&Watch> {
        let shown = self.tab == TAB_ROOM && self.pending.is_none() && self.together.is_none();
        self.room.as_ref().filter(|_| shown)?.watching()
    }

    fn watching_mut(&mut self) -> Option<&mut Watch> {
        self.watching()?;
        self.room.as_mut()?.watch.as_mut()
    }

    fn picture(&self, map: &str, side: f32) -> El {
        let frame = |el: El| el.width(Size::Fixed(side)).height(Size::Fixed(side)).radius(tokens::RADIUS_MD);
        match self.pictures.get(map) {
            Some(Some(img)) => frame(image(img.clone()).image_fit(ImageFit::Cover)).clip(),
            _ => frame(
                column([icon("map").icon_size(tokens::ICON_LG).color(tokens::MUTED_FOREGROUND)])
                    .align(Align::Center)
                    .justify(Justify::Center)
                    .fill(tokens::MUTED),
            ),
        }
    }

    /// Map picture beside the replay's description, and what goes below it.
    fn replay_card(&self, info: &ReplayInfo, side: f32, extra: Vec<El>) -> El {
        let mut lines = vec![h3(info.map.clone())];
        for (i, team) in info.teams.iter().enumerate() {
            lines.push(
                row([text(format!("Team {}", i + 1)).label().muted().width(Size::Fixed(64.0)), paragraph(team.join(", "))])
                    .gap(tokens::SPACE_2),
            );
        }
        if !info.spectators.is_empty() {
            lines.push(
                row([text("Watching").label().muted().width(Size::Fixed(64.0)), paragraph(info.spectators.join(", ")).muted()])
                    .gap(tokens::SPACE_2),
            );
        }
        lines.push(text(format!("Played {}  ·  {} long", when(info.start_unix), clock(info.duration_s))).caption().muted());
        lines.push(
            text(format!("{}  ·  engine {}", info.game.strip_prefix("Beyond All Reason ").unwrap_or(&info.game), info.engine))
                .caption()
                .muted(),
        );
        lines.extend(extra);
        row([self.picture(&info.map, side), column(lines).gap(tokens::SPACE_2).fill_width()])
            .gap(tokens::SPACE_4)
            .align(Align::Start)
    }

    /// The first tab: the room this app is in, or the ways into one.
    fn room_tab(&self, cx: &BuildCx) -> El {
        let Some(room) = &self.room else { return self.join_view() };
        let Some(update) = &room.update else {
            return row([spinner(), text(format!("Connecting to {}…", host_of(&room.server)))])
                .gap(tokens::SPACE_3)
                .align(Align::Center);
        };
        let Some(view) = &update.room else {
            // The server never let us in: what it said, and the way out.
            return column([
                status_block(update, room.over),
                row([spacer(), button(if room.over { "Close" } else { "Leave" }).key("room:leave").ghost()]),
            ])
            .gap(tokens::SPACE_4);
        };
        let mut sections: Vec<El> = Vec::new();
        match room.watching() {
            // Watching: the timeline takes the window. What the replay is
            // shrinks to one line, with the room's buttons on it.
            Some(watch) => {
                sections.push(self.watching_bar(update, view, room.over, watch.timeline.game_state()));
                if update.stage != Stage::Playing || room.over {
                    sections.push(status_block(update, room.over));
                }
                let timeline = column([watch.timeline.view(cx)]).width(Size::Fill(1.0));
                sections.push(if room.game_line_moves() { timeline.redraw_within(GAME_LINE_FRAME) } else { timeline });
                if update.stage == Stage::Playing {
                    sections.push(text(CHAT_HINT).caption().muted().ellipsis());
                }
            }
            None => {
                sections.push(room_header(view, &room.server, room.over));
                sections.push(self.current_replay(view, update));
                // An empty room's card says it all.
                if update.stage != Stage::NoReplay || room.over {
                    sections.push(status_block(update, room.over));
                }
                let actions = room_actions(update, room.over);
                if !actions.is_empty() {
                    sections.push(row(actions).gap(tokens::SPACE_2).align(Align::Center));
                }
                if let (Some(statistics), Some(replay), false) = (room.timeline(), &view.replay, room.over) {
                    sections.push(statistics_line(statistics, &replay.info));
                }
            }
        }
        sections.push(members_card(view, &update.member));
        if !room.over {
            sections.push(self.pick_card(update));
        }
        if !view.history.is_empty() {
            sections.push(history_card(&view.history, room.over));
        }
        // Air between the content's right edge and the scrollbar's lane.
        scroll(sections)
            .key("room-scroll")
            .gap(tokens::SPACE_4)
            .padding(Sides { right: tokens::SPACE_2, ..Sides::zero() })
            .fill_height()
    }

    /// Outside a room: paste a link, or go back into a room this app has been in.
    fn join_view(&self) -> El {
        let mut sections = vec![card([
            card_header([
                card_title("Join a room"),
                card_description("Paste the room's link you were sent, or press the join button on its invite page."),
            ]),
            card_content([row([
                text_input_with(
                    "link",
                    &self.link,
                    &self.selection,
                    TextInputOpts::default().placeholder("https://…/app/join/…"),
                )
                .fill_width(),
                button("Join").key("join").primary(),
            ])
            .gap(tokens::SPACE_2)]),
        ])];
        if !self.cfg.rooms.is_empty() {
            let rows: Vec<El> = self
                .cfg
                .rooms
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let host = api::parse_server(&r.server).map(|s| host_of(&s)).unwrap_or_else(|_| r.server.clone());
                    item([
                        item_content([item_title(r.name.clone()), item_description(host)]),
                        item_actions([
                            button("Forget").key(format!("forget:{i}")).ghost(),
                            button("Join").key(format!("recent:{i}")).secondary(),
                        ]),
                    ])
                    .align(Align::Center)
                })
                .collect();
            sections.push(titled_card("Rooms you have been in", [item_group(rows)]));
        }
        sections.push(text("To watch one of your own replays with friends, pick it under My replays.").muted().wrap_text());
        scroll(sections)
            .key("join-scroll")
            .gap(tokens::SPACE_4)
            .padding(Sides { right: tokens::SPACE_2, ..Sides::zero() })
            .fill_height()
    }

    /// The room's current replay, or word that it has none yet.
    fn current_replay(&self, view: &RoomView, update: &client::Update) -> El {
        match &view.replay {
            Some(replay) => {
                let mut extra = Vec::new();
                if replay.match_url.is_some() {
                    extra.push(row([button("View in Gex").key("room:gex").secondary()]));
                }
                card([card_content([self.replay_card(&replay.info, 200.0, extra)]).pt(tokens::SPACE_5)])
            }
            None => {
                let words = if update.picking {
                    "A replay is on its way to the server…"
                } else {
                    "Paste a link to a replay on BAR's site below, pick one of your own under My replays, or watch one again from the history."
                };
                card([card_header([card_title("No replay yet"), card_description(words)])])
            }
        }
    }

    /// A field for a BAR replay link or id, to play in the room.
    fn pick_card(&self, update: &client::Update) -> El {
        let mut play = button(if update.picking { "Fetching…" } else { "Play" }).key("room:pick-bar").secondary();
        if update.picking {
            play = play.disabled();
        }
        titled_card(
            "Play another replay",
            [
                row([
                    text_input_with(
                        "bar-link",
                        &self.bar_link,
                        &self.selection,
                        TextInputOpts::default().placeholder("A BAR replay link or id"),
                    )
                    .fill_width(),
                    play,
                ])
                .gap(tokens::SPACE_2),
                text("It becomes the room's replay for everyone, and starts when someone presses Start.")
                    .caption()
                    .muted()
                    .wrap_text(),
            ],
        )
    }

    /// What is being watched, in one line: the map's picture, the map and
    /// who plays, where this viewer stands, and the room's buttons. The
    /// game's length is left out: the timeline below may be hiding it.
    /// "In game" only once the game says where it is: an engine that is open
    /// may still be loading, or running ahead to where the room is.
    fn watching_bar(&self, update: &client::Update, room: &RoomView, over: bool, game: timeline::GameState) -> El {
        let state = if room.ended {
            badge("Replay finished").muted()
        } else if update.stage == Stage::Playing {
            match game {
                timeline::GameState::Loading => badge("Game loading").muted(),
                timeline::GameState::NotSaying => badge("Game open").muted(),
                timeline::GameState::CatchingUp => badge("Catching up").warning(),
                timeline::GameState::Running => badge("In game").success(),
            }
        } else {
            badge("Not in game").muted()
        };
        let info = room.replay.as_ref().map(|r| r.info.clone()).unwrap_or_default();
        let mut items = vec![
            self.picture(&info.map, 40.0),
            column([
                text(format!("{}  ·  {}", room.name, info.map)).label().ellipsis(),
                text(versus(&info)).caption().muted().ellipsis(),
            ])
            .gap(2.0)
            .width(Size::Fill(1.0)),
            state,
        ];
        items.extend(room_actions(update, over));
        items.push(button("Copy link").key("room:copy").secondary());
        items.push(button(if over { "Close" } else { "Leave" }).key("room:leave").ghost());
        row(items).gap(tokens::SPACE_3).align(Align::Center)
    }

    fn replays_view(&self) -> El {
        let (replays, visible, selected) = (self.replays.clone(), self.visible.clone(), self.selected);
        let count = visible.len();
        let list: El = if count == 0 {
            let message = if self.scanning {
                "Reading your replays…".to_string()
            } else if self.replays.is_empty() {
                format!("No replays in {}", self.data.demos().display())
            } else {
                "No replay matches.".to_string()
            };
            column([text(message).muted()]).padding(tokens::SPACE_4).fill_size()
        } else {
            virtual_list(count, REPLAY_ROW_HEIGHT, move |row| {
                let index = visible[row];
                let replay = &replays[index];
                let (title, description) = match &replay.info {
                    Some(info) => (
                        info.map.clone(),
                        format!("{}  ·  {}  ·  {}", when(info.start_unix), clock(info.duration_s), versus(info)),
                    ),
                    None => (replay.file_name.clone(), "Not a readable replay".to_string()),
                };
                let entry = item([item_content([item_title(title), item_description(description)])])
                    .key(format!("replay:{index}"))
                    .height(Size::Fixed(REPLAY_ROW_HEIGHT));
                if selected == Some(index) {
                    entry.selected()
                } else {
                    entry
                }
            })
            .key("replay-list")
            .gap(tokens::SPACE_1)
            .scrollbar_gutter()
            .fill_size()
        };

        let toolbar_row = row([
            text_input_with(
                "filter",
                &self.filter,
                &self.selection,
                TextInputOpts::default().placeholder("Filter by map or player"),
            )
            .fill_width(),
            text(format!("{count} of {}", self.replays.len())).caption().muted(),
            button("Refresh").key("replays:refresh").ghost(),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center);

        row([
            column([toolbar_row, list]).gap(tokens::SPACE_3).fill_size(),
            self.replay_preview().width(Size::Fixed(340.0)).fill_height(),
        ])
        .gap(tokens::SPACE_4)
        .fill_height()
    }

    fn replay_preview(&self) -> El {
        let Some(replay) = self.selected_replay() else {
            return card([card_content([text("Pick a replay to see what it is.").muted()]).pt(tokens::SPACE_5)]);
        };
        let Some(info) = &replay.info else {
            return card([card_content([
                text(replay.file_name.clone()).wrap_text(),
                text("This file is not a readable replay.").muted(),
            ])
            .pt(tokens::SPACE_5)
            .gap(tokens::SPACE_2)]);
        };
        let mut body: Vec<El> = vec![
            self.picture(&info.map, 292.0),
            h3(info.map.clone()),
            paragraph(versus(info)),
            text(format!("Played {}  ·  {} long", when(info.start_unix), clock(info.duration_s))).caption().muted(),
            text(format!("{}  ·  engine {}", info.game.strip_prefix("Beyond All Reason ").unwrap_or(&info.game), info.engine))
                .caption()
                .muted(),
            text(replay.file_name.clone()).caption().muted().ellipsis(),
        ];
        let paired = self.paired_server();
        match (self.live_room(), &paired) {
            // In a room: this replay becomes its replay.
            (Some(room), _) => {
                let picking = room.update.as_ref().is_some_and(|u| u.picking);
                let name = room.view().map_or("the room".to_owned(), |v| v.name.clone());
                let label = if picking { "Uploading…".to_owned() } else { format!("Play in {}", short(&name, 24)) };
                let mut play = button(label).key("replays:play").primary();
                if picking {
                    play = play.disabled();
                }
                body.push(play);
                body.push(
                    text(format!("Uploads the replay into {name}. It starts when someone there presses Start."))
                        .caption()
                        .muted()
                        .wrap_text(),
                );
            }
            (None, Some((server, _))) => {
                body.push(button("Watch together").key("replays:together").primary());
                body.push(
                    text(format!("Plays the replay in one of {}'s rooms, or in a new one.", host_of(server)))
                        .caption()
                        .muted()
                        .wrap_text(),
                );
            }
            (None, None) => body.push(alert([
                alert_title("Not in a room"),
                alert_description("Join a room to play this replay in it. To make rooms yourself, open your barwatch server's page in a browser and press “Pair the desktop app”."),
            ])),
        }
        if paired.is_some() {
            match self.archives.get(&replay.path) {
                None => body.push(row([button("Archive").key("replays:archive").secondary()])),
                Some(Archiving::Sending) => body.push(row([button("Archiving…").key("replays:archive").secondary().disabled()])),
                Some(Archiving::Done(archived)) => {
                    let mut line = vec![badge("In the archive").success()];
                    if archived.match_url.is_some() {
                        line.push(button("View in Gex").key("replays:gex").secondary());
                    }
                    body.push(row(line).gap(tokens::SPACE_2).align(Align::Center));
                }
            }
            body.push(
                text("Archive sends the replay to the server's replay archive without watching it.")
                    .caption()
                    .muted()
                    .wrap_text(),
            );
        }
        card([card_content([scroll(body).key("preview-scroll").gap(tokens::SPACE_2).fill_height()])
            .pt(tokens::SPACE_5)
            .fill_height()])
    }

    /// Which of the paired server's rooms to play a replay in, or a new one.
    fn together_dialog(&self, t: &Together) -> El {
        let rooms: El = match &t.rooms {
            None => row([spinner(), text("Asking for the server's rooms…")]).gap(tokens::SPACE_3).align(Align::Center),
            Some(Err(e)) => {
                alert([alert_title("The server's rooms could not be listed"), alert_description(e.clone())]).destructive()
            }
            Some(Ok(rooms)) if rooms.is_empty() => text("The server has no rooms yet.").muted(),
            Some(Ok(rooms)) => item_group(rooms.iter().take(TOGETHER_ROOMS).enumerate().map(|(i, r)| {
                let playing = r.replay.as_ref().map_or("no replay yet".to_owned(), |p| p.info.map.clone());
                let here = match r.members.len() {
                    0 => "nobody there".to_owned(),
                    1 => "1 there".to_owned(),
                    n => format!("{n} there"),
                };
                item([
                    item_content([item_title(r.name.clone()), item_description(format!("{playing}  ·  {here}"))]),
                    item_actions([button("Play here").key(format!("pick-room:{i}")).secondary()]),
                ])
                .align(Align::Center)
            })),
        };
        let mut create = button(if t.creating { "Making…" } else { "New room" }).key("together:create").secondary();
        if t.creating {
            create = create.disabled();
        }
        dialog(
            "together",
            [
                dialog_header([
                    dialog_title("Watch together"),
                    dialog_description(format!(
                        "Play {} in one of {}'s rooms, or make a new one. It starts when someone in the room presses Start.",
                        t.map,
                        host_of(&t.server)
                    )),
                ]),
                rooms,
                row([
                    text_input_with(
                        "room-name",
                        &t.name,
                        &self.selection,
                        TextInputOpts::default().placeholder("New room's name"),
                    )
                    .fill_width(),
                    create,
                ])
                .gap(tokens::SPACE_2),
                dialog_footer([button("Cancel").key("together:cancel").ghost()]),
            ],
        )
    }

    fn settings_view(&self) -> El {
        let engines = barwatch_core::engine::installed(&self.data.engines()).len();
        let data_note = if self.data.own {
            "No Beyond All Reason installation was found, so this is barwatch's own folder: everything a replay needs will be downloaded into it.".to_string()
        } else {
            format!("{engines} engine version(s) installed here. barwatch shares this folder with BAR, so neither downloads what the other already has.")
        };
        let servers: Vec<El> = if self.cfg.servers.is_empty() {
            vec![text(
                "None. Pairing is only needed to make rooms and to archive replays from this app; joining a room never needs it.",
            )
            .muted()
            .wrap_text()]
        } else {
            self.cfg
                .servers
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    row([
                        text(s.url.clone()).ellipsis().fill_width(),
                        button("Unpair").key(format!("unpair:{i}")).ghost().destructive(),
                    ])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
                })
                .collect()
        };
        let (link_text, link_badge) = match self.link_status {
            uri::Status::Registered => ("barwatch:// links open this app.", badge("working").success()),
            uri::Status::Missing => ("barwatch:// links do not open this app yet.", badge("not set up").warning()),
            uri::Status::Unsupported => {
                ("Link handling is not supported on this system; paste room links instead.", badge("unsupported").muted())
            }
        };
        scroll([
            titled_card(
                "You",
                [field_with(
                    "Name",
                    text_input("name", &self.name, &self.selection),
                    FieldOpts::default().description("Shown to the others in the room and in game. Changed slightly if a player in the replay has the same name."),
                )],
            ),
            titled_card(
                "Beyond All Reason",
                [
                    field_with("Data folder", text_input("data-dir", &self.data_dir, &self.selection), FieldOpts::default().description(&data_note)),
                    row([button("Detect").key("settings:detect").secondary()]),
                ],
            ),
            row([spacer(), button("Save").key("settings:save").primary()]),
            titled_card("Paired servers", servers),
            titled_card(
                "Room links",
                [
                    row([text(link_text).fill_width().wrap_text(), link_badge]).gap(tokens::SPACE_2).align(Align::Center),
                    row([button("Set up link handling").key("settings:register").secondary()]),
                ],
            ),
            text(format!("barwatch {}", env!("CARGO_PKG_VERSION"))).caption().muted(),
        ])
        .key("settings-scroll")
        .gap(tokens::SPACE_4)
        .fill_height()
    }
}

/// The room's name, where it is, and its link.
fn room_header(view: &RoomView, server: &Url, over: bool) -> El {
    row([
        column([h2(view.name.clone()).ellipsis(), text(format!("A room on {}", host_of(server))).caption().muted()])
            .gap(2.0)
            .width(Size::Fill(1.0)),
        button("Copy link").key("room:copy").secondary(),
        button(if over { "Close" } else { "Leave" }).key("room:leave").ghost(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
}

/// What can be done with the room's replay from here: start or stop it for
/// everyone, or get back into it.
fn room_actions(update: &client::Update, over: bool) -> Vec<El> {
    let Some(room) = update.room.as_ref().filter(|_| !over) else { return Vec::new() };
    let mut buttons = Vec::new();
    // Starting early is fine: a viewer still getting ready joins when it is (§9.2).
    if room.replay.is_some() && matches!(room.phase, Phase::Idle | Phase::Failed) {
        buttons.push(button("Start for everyone").key("room:start").primary());
    }
    if update.stage == Stage::Closed && room.phase == Phase::Running {
        buttons.push(button("Rejoin").key("room:rejoin").primary());
    }
    if matches!(room.phase, Phase::Starting | Phase::Running) {
        buttons.push(button("Stop for everyone").key("room:stop").secondary());
    }
    buttons
}

/// What the room played before: each can be watched again (if the server can
/// still have its file) or looked at on the stats service.
fn history_card(history: &[HistoryEntry], over: bool) -> El {
    let rows: Vec<El> = history
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut actions = Vec::new();
            if h.match_url.is_some() {
                actions.push(button("View in Gex").key(format!("history-gex:{i}")).ghost());
            }
            if !h.available {
                actions.push(badge("file gone").muted());
            } else if !over {
                actions.push(button("Watch").key(format!("history-watch:{i}")).secondary());
            }
            let description =
                format!("Picked {}  ·  {} long  ·  {}", when(h.picked_unix), clock(h.info.duration_s), versus(&h.info));
            item([item_content([item_title(h.info.map.clone()), item_description(description)]), item_actions(actions)])
                .align(Align::Center)
        })
        .collect();
    titled_card("Played here before", [item_group(rows)])
}

/// Game seconds an engine may go between two reports beyond what its speed
/// accounts for (the reports come about once a second, read every 0.1 s).
const CATCH_UP_SLACK_S: f64 = 2.0;

/// Whether an engine that reported `last` and then `now` is running ahead
/// of its own speed: catching up to the room after joining late or a jump.
/// Its speed factor says 1 meanwhile, and it reports once a second by the
/// wall clock with tens of game seconds between reports (DESIGN.md §5.3).
fn catching_up(last: &Position, now: &Position) -> bool {
    let wall = now.seen.saturating_duration_since(last.seen).as_secs_f64();
    let went = (f64::from(now.report.frame) - f64::from(last.report.frame)) / position::FRAMES_PER_SECOND;
    let pace = if last.report.paused { 0.0 } else { f64::from(last.report.speed.max(now.report.speed)) };
    went > 3.0 * pace * wall + CATCH_UP_SLACK_S
}

/// The game's length as the replay's header gives it; a header that gives none
/// counts as the longest game there is, so that a game position is never cut
/// short by it (the statistics, once complete, say how long it really was).
fn replay_length(info: &ReplayInfo) -> u32 {
    match info.duration_s {
        0 => barwatch_core::timeline::MAX_FRAME / 30,
        s => s,
    }
}

/// Where the statistics stand, in one quiet line, for a viewer who is not
/// in the game yet: the stats service starts on a replay when a room picks
/// it, so this says whether the timeline will have numbers from the start.
fn statistics_line(view: &TimelineView, info: &ReplayInfo) -> El {
    let so_far = |doc: &barwatch_core::timeline::TimelineDoc| {
        let done = doc.frames / 30;
        format!("worked out up to {} of {}", clock(done), clock(info.duration_s.max(done)))
    };
    let words = match (timeline::feed(view), &view.doc) {
        // An outage: the server keeps what it has and keeps trying.
        (timeline::Feed::Unavailable(why), Some(doc)) if !doc.complete => {
            format!("Statistics: {} so far, then interrupted. {why}", so_far(doc))
        }
        (timeline::Feed::Failed(why), Some(doc)) if !doc.complete => {
            format!("Statistics: {}, then stopped. {why}", so_far(doc))
        }
        (timeline::Feed::Unavailable(why) | timeline::Feed::Failed(why), _) => format!("Statistics: none. {why}"),
        (timeline::Feed::Complete, None) => "Statistics: none. The server sent none.".to_owned(),
        (timeline::Feed::Complete, Some(_)) => {
            "Statistics: ready. A timeline of the game shows here once you are in it.".to_owned()
        }
        (timeline::Feed::Building, Some(doc)) => format!("Statistics: {} so far.", so_far(doc)),
        (timeline::Feed::Building, None) => "Statistics: being worked out.".to_owned(),
        (timeline::Feed::Queued | timeline::Feed::Asking, _) => {
            "Statistics: the stats service has this replay in its queue.".to_owned()
        }
    };
    row([
        icon("chart-line").icon_size(tokens::ICON_SM).text_color(tokens::MUTED_FOREGROUND),
        text(words).caption().muted().wrap_text().width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
}
/// Where this viewer stands, in words.
fn status_block(update: &client::Update, over: bool) -> El {
    let room = update.room.as_ref();
    let phase = room.map(|r| r.phase);
    let note = room.and_then(|r| r.note.clone());
    let busy = |label: String| row([spinner(), text(label)]).gap(tokens::SPACE_3).align(Align::Center);
    match &update.stage {
        Stage::Gone => alert([alert_title("This room is gone"), alert_description(client::ROOM_GONE)]).destructive(),
        Stage::Failed(error) => alert([alert_title("This did not work"), alert_description(error.clone())]).destructive(),
        _ if over => alert([alert_title("You left this room")]),
        Stage::Connecting => busy("Connecting…".into()),
        Stage::NoReplay => column(Vec::<El>::new()),
        Stage::Preparing { what, fraction } => {
            let mut parts = vec![busy(format!("{what}…"))];
            if let Some(f) = fraction {
                parts.push(progress(f.clamp(0.0, 1.0)));
            }
            column(parts).gap(tokens::SPACE_2)
        }
        Stage::Playing => alert([alert_title("The game is running"), alert_description(CHAT_HINT)]).success(),
        Stage::Closed => alert([
            alert_title("You closed the game"),
            alert_description(if phase == Some(Phase::Running) {
                "The room is still playing. Rejoin to get back in."
            } else {
                "The replay is not running right now."
            }),
        ]),
        Stage::Ready => match phase {
            Some(Phase::Running | Phase::Starting) => busy("Starting the game…".into()),
            Some(Phase::Preparing) => busy("Ready here. The server is still fetching the engine for this replay…".into()),
            Some(Phase::Failed) => alert([
                alert_title("The server could not start the replay"),
                alert_description(note.unwrap_or_else(|| "Starting it again retries.".into())),
            ])
            .destructive(),
            _ => alert([
                alert_title("Ready"),
                alert_description(match note {
                    Some(n) => format!("Last time: {n}. The replay starts for everyone when someone presses Start."),
                    None => "The replay starts for everyone when someone presses Start.".into(),
                }),
            ])
            .info(),
        },
    }
}

/// Who is in the room, and how far each is with its replay.
fn members_card(room: &RoomView, me: &str) -> El {
    let has_replay = room.replay.is_some();
    let rows: Vec<El> = room
        .members
        .iter()
        .map(|v| {
            let state = if v.in_game {
                badge("in game").success()
            } else if !has_replay {
                badge("here").muted()
            } else {
                match v.state {
                    ViewerState::Preparing => badge("getting ready").warning(),
                    ViewerState::Ready => badge("ready").info(),
                    ViewerState::Playing => badge("starting").info(),
                    ViewerState::Error => badge("problem").destructive(),
                }
            };
            let title = if v.name == me { format!("{} (you)", v.name) } else { v.name.clone() };
            let mut content = vec![item_title(title)];
            let mut words: Vec<String> = Vec::new();
            if has_replay && !v.engine_name.is_empty() && v.engine_name != v.name {
                words.push(format!("in game as {}", v.engine_name));
            }
            if !v.detail.is_empty() {
                words.push(v.detail.clone());
            }
            if !words.is_empty() {
                content.push(item_description(words.join("  ·  ")));
            }
            item([item_content(content), item_actions([state])]).align(Align::Center)
        })
        .collect();
    let title = format!("In the room ({})", room.members.len());
    if rows.is_empty() {
        return titled_card(title, [text("Nobody is here right now.").muted()]);
    }
    titled_card(title, [item_group(rows)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_leaves_text_fields() {
        let tab = |key: &str| UiEvent::synthetic_key_down(key, LogicalKey::Named(NamedKey::Tab));
        assert_eq!(tab_out_of_field(&tab("filter")), Some("replays:refresh"));
        assert_eq!(tab_out_of_field(&tab("data-dir")), Some("settings:detect"));
        assert_eq!(tab_out_of_field(&tab("join")), None);
        let enter = UiEvent::synthetic_key_down("filter", LogicalKey::Named(NamedKey::Enter));
        assert_eq!(tab_out_of_field(&enter), None);
    }

    #[test]
    fn only_a_click_on_the_backdrop_declines() {
        assert!(declines_consent(&UiEvent::synthetic_click("consent:dismiss")));
        assert!(declines_consent(&UiEvent::synthetic_click("consent:no")));
        assert!(!declines_consent(&UiEvent::synthetic_click("consent:yes")));
        // What the pointer crossing the backdrop sends.
        for kind in [UiEventKind::PointerEnter, UiEventKind::PointerLeave, UiEventKind::PointerDown] {
            let mut hover = UiEvent::synthetic_click("consent:dismiss");
            hover.kind = kind;
            assert!(!declines_consent(&hover), "{kind:?}");
        }
    }

    /// The room's timeline as the client's updates drive it (DESIGN.md §5.4, §5.7).
    mod watch {
        use super::*;
        use crate::fixtures::{at, docs, view, watching};
        use api::TimelineState;

        fn stats() -> Option<Arc<TimelineView>> {
            view(TimelineState::Complete, None, Some(docs::duel()))
        }

        fn party(update: client::Update) -> Room {
            let mut party = Room::new(Url::parse("https://watch.example.org/").unwrap(), "abcdefghijklmnopqrstuv".into());
            party.update = Some(update);
            party.sync_watch();
            party.tick_watch();
            party
        }

        /// The next update from the client, as the window takes it in.
        fn next(party: &mut Room, update: impl FnOnce(&mut client::Update)) {
            let mut u = party.update.clone().unwrap();
            u.jump_refused = false;
            update(&mut u);
            party.update = Some(u);
            party.sync_watch();
            party.tick_watch();
        }

        fn jump(target_s: u32, at_unix: u64) -> impl FnOnce(&mut client::Update) {
            move |u| u.room.as_mut().unwrap().jump = Some(JumpView { target_s, at_unix })
        }

        fn jumping(party: &Room) -> Option<f32> {
            party.watching().and_then(|w| w.timeline.jumping_to())
        }

        #[test]
        fn shown_once_in_game_with_its_statistics() {
            let mut out = watching(false, None, stats());
            out.stage = Stage::Ready;
            out.room.as_mut().unwrap().phase = Phase::Idle;
            let mut p = party(out);
            assert!(p.watching().is_none(), "not before the game runs");
            next(&mut p, |u| {
                u.stage = Stage::Playing;
                u.room.as_mut().unwrap().phase = Phase::Running;
                u.position = at(300);
            });
            let w = p.watching().expect("in game");
            assert_eq!(w.timeline.game_s(), 300.0);
            assert!(w.timeline.data.metric_index("valueArmy").is_some());
        }

        /// A position as the engine reports it, read `after` past `t0`.
        fn report(t0: Instant, after_ms: u64, seconds: u32, paused: bool) -> Option<Position> {
            let report = position::Report { frame: seconds * 30, speed: 1.0, paused };
            Some(Position { report, seen: t0 + Duration::from_millis(after_ms) })
        }

        /// A late joiner's engine: loading, then running ahead to the room,
        /// then going at its pace; the timeline says which.
        #[test]
        fn a_late_joiner_is_loading_then_catching_up_then_in_game() {
            use timeline::GameState;
            let mut p = party(watching(false, None, stats()));
            let state = |p: &Room| p.watching().unwrap().timeline.game_state();
            assert_eq!(state(&p), GameState::Loading);
            let t0 = Instant::now();
            // The first report says nothing of pace yet: not "in game" so soon.
            next(&mut p, |u| u.position = report(t0, 0, 4, false));
            assert_eq!(state(&p), GameState::Loading);
            // A second on: 40 game seconds further at speed 1.
            next(&mut p, |u| u.position = report(t0, 1000, 44, false));
            assert_eq!(state(&p), GameState::CatchingUp);
            // The same report again (an update about something else) changes nothing.
            next(&mut p, |u| u.position = report(t0, 1000, 44, false));
            assert_eq!(state(&p), GameState::CatchingUp);
            next(&mut p, |u| u.position = report(t0, 2000, 270, false));
            assert_eq!(state(&p), GameState::CatchingUp);
            // Gone quiet: not running ahead any more, whatever it was doing.
            next(&mut p, |u| u.position = None);
            assert_eq!(state(&p), GameState::Running);
            next(&mut p, |u| u.position = report(t0, 2500, 270, false));
            // There: one game second a second.
            next(&mut p, |u| u.position = report(t0, 3000, 271, false));
            assert_eq!(state(&p), GameState::Running);
            // A pause and a resume are not a catch-up.
            next(&mut p, |u| u.position = report(t0, 4000, 272, true));
            next(&mut p, |u| u.position = report(t0, 9000, 272, false));
            next(&mut p, |u| u.position = report(t0, 10_000, 273, false));
            assert_eq!(state(&p), GameState::Running);
        }

        #[test]
        fn a_jump_by_another_viewer_shows_until_this_engine_gets_there() {
            let mut p = party(watching(false, at(360), stats()));
            next(&mut p, jump(840, 1000));
            assert_eq!(jumping(&p), Some(840.0));
            next(&mut p, |u| u.position = at(600));
            assert_eq!(jumping(&p), Some(840.0), "catching up");
            next(&mut p, |u| u.position = at(839));
            assert_eq!(jumping(&p), None, "arrived, within a keyframe");
            // The same jump named again is not a new one.
            next(&mut p, |u| u.position = at(845));
            assert_eq!(jumping(&p), None);
        }

        #[test]
        fn a_jump_made_before_this_viewer_was_in_game_is_not_under_way() {
            let mut update = watching(false, at(5), stats());
            jump(840, 1000)(&mut update);
            let mut p = party(update);
            assert_eq!(jumping(&p), None, "the engine is catching up to the room, not to that jump");
            // A later jump is.
            next(&mut p, jump(900, 1100));
            assert_eq!(jumping(&p), Some(900.0));
        }

        #[test]
        fn a_jump_while_out_of_the_game_is_not_shown_on_rejoining() {
            let mut p = party(watching(false, at(200), stats()));
            next(&mut p, |u| {
                u.stage = Stage::Closed;
                u.position = None;
            });
            assert!(p.watching().is_none());
            next(&mut p, jump(700, 1000));
            next(&mut p, |u| {
                u.stage = Stage::Playing;
                u.position = at(20);
            });
            assert_eq!(jumping(&p), None);
            assert_eq!(p.watching().unwrap().timeline.game_s(), 20.0, "the engine started over");
        }

        #[test]
        fn a_refused_jump_is_called_off() {
            let mut p = party(watching(false, at(360), stats()));
            p.watch.as_mut().unwrap().timeline.show_jump(900.0);
            assert_eq!(jumping(&p), Some(900.0));
            next(&mut p, |u| u.jump_refused = true);
            assert_eq!(jumping(&p), None);
        }

        #[test]
        fn a_new_run_starts_over_and_an_ended_one_stays_to_look_back_on() {
            let mut p = party(watching(false, at(700), stats()));
            next(&mut p, |u| {
                let s = u.room.as_mut().unwrap();
                s.ended = true;
                u.position = at(1271);
            });
            let w = p.watching().expect("after the end, for one who was in it");
            assert_eq!(w.timeline.game_s(), w.timeline.data.duration_s, "game over");
            // The engine closed after the end: still there.
            next(&mut p, |u| {
                u.stage = Stage::Closed;
                u.position = None;
            });
            assert!(p.watching().is_some());
            // Started again: gone until this viewer is in the new run's game.
            next(&mut p, |u| {
                let s = u.room.as_mut().unwrap();
                s.ended = false;
                s.run = 2;
            });
            assert!(p.watching().is_none());
            // An engine still open from the last run is not in this one.
            next(&mut p, |u| {
                u.stage = Stage::Playing;
                u.engine_run = Some(1);
            });
            assert!(p.watching().is_none(), "the engine left open in the last run");
            next(&mut p, |u| {
                u.engine_run = Some(2);
                u.position = at(3);
            });
            let w = p.watching().expect("in the new run");
            assert_eq!(w.timeline.game_s(), 3.0);
        }
    }

    /// The room picks another replay: the timeline of the one before goes, and
    /// statistics of another replay than the room's are never applied.
    mod another_replay {
        use super::*;
        use crate::fixtures::{at, docs, view, watching};
        use api::TimelineState;

        #[test]
        fn the_timeline_of_the_replay_before_goes() {
            let mut room = Room::new(Url::parse("https://watch.example.org/").unwrap(), "abcdefghijklmnopqrstuv".into());
            room.update = Some(watching(false, at(300), view(TimelineState::Complete, None, Some(docs::duel()))));
            room.sync_watch();
            assert!(room.watching().is_some_and(|w| w.applied.is_some()));
            // Picked another: the run is stopped, the client starts over.
            let mut u = room.update.clone().unwrap();
            let r = u.room.as_mut().unwrap();
            r.phase = Phase::Idle;
            r.replay.as_mut().unwrap().key = "0123456789abcdef0123456789abcdef".into();
            u.stage = Stage::Ready;
            u.engine_run = None;
            u.position = None;
            room.update = Some(u);
            room.sync_watch();
            assert!(room.watch.is_none(), "dropped with its replay");
            assert!(room.timeline().is_none(), "the statistics in hand are of the replay before");
            // In the new replay's game, before its statistics come.
            let mut u = room.update.clone().unwrap();
            let r = u.room.as_mut().unwrap();
            r.phase = Phase::Running;
            r.run += 1;
            u.engine_run = Some(r.run);
            u.stage = Stage::Playing;
            room.update = Some(u);
            room.sync_watch();
            let w = room.watching().expect("in the new game");
            assert!(w.applied.is_none(), "nothing of the old replay's document");
        }
    }

    #[test]
    fn display_helpers() {
        assert_eq!(clock(834), "13:54");
        assert_eq!(clock(3723), "1:02:03");
        let info = ReplayInfo { teams: vec![vec!["a".into(), "b".into()], vec!["c".into()]], ..Default::default() };
        assert_eq!(versus(&info), "a, b  vs  c");
        assert_eq!(versus(&ReplayInfo::default()), "no players");
    }
}
