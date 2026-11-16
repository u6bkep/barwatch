//! The window: join a party, browse local replays, settings.

use crate::backend::{Backend, Cmd, Msg};
use crate::client::{self, Stage};
use crate::config::Config;
use crate::datadir::{self, DataDir};
use crate::position::{self, Position};
use crate::replays::LocalReplay;
use crate::timeline::{self, icons, Icons, Timeline};
use crate::uri;
use barwatch_core::api::{self, JumpView, Link, Phase, ReplayInfo, SessionView, TimelineView, ViewerState};
use damascene_core::prelude::*;
use damascene_core::widgets::{tabs, text_input};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const TAB_PARTY: &str = "party";
const TAB_REPLAYS: &str = "replays";
const TAB_SETTINGS: &str = "settings";
const REPLAY_ROW_HEIGHT: f32 = 52.0;
const CHAT_HINT: &str = "In game chat: !skip 14:00   !skip +2m   !pause   !resume   !speed 2";

/// How often the window is drawn while the game line moves: the clock on it
/// counts seconds, and the line creeps a pixel in about as long.
const GAME_LINE_FRAME: Duration = Duration::from_millis(250);

/// The party this app is in (or was, until the user dismisses it).
struct Party {
    server: Url,
    session: String,
    update: Option<client::Update>,
    /// The client has stopped: left, failed, or the party is gone.
    over: bool,
    /// The statistics timeline, from the first time this viewer is in game.
    watch: Option<Watch>,
    art: Art,
}

/// The timeline of the game this viewer is watching (DESIGN.md §5.7). It is
/// kept for the whole party, so that what the viewer chose to look at stays
/// when the replay is started again; it is on screen while this viewer is in
/// the game, and after the game has ended for those who were.
struct Watch {
    timeline: Timeline,
    /// The run of the party this viewer was last in the game of.
    run: u32,
    /// This viewer's engine was in the running game at the last update.
    in_game: bool,
    /// The statistics the timeline was given last, as the client handed them over.
    applied: Option<Arc<TimelineView>>,
    /// The jump the server named last. One that differs from it is new and
    /// under way; the one already named when this viewer got into the game is
    /// not (the engine is catching up to the party, not to that jump).
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

impl Party {
    fn new(server: Url, session: String) -> Party {
        Party { server, session, update: None, over: false, watch: None, art: Art::default() }
    }

    /// Whether this viewer's engine is in the running game: open, and started
    /// for this run (one left open after the last run ended is not).
    fn in_game(&self) -> bool {
        self.update.as_ref().is_some_and(|u| {
            u.stage == Stage::Playing
                && u.session.as_ref().is_some_and(|s| s.phase == Phase::Running && u.engine_run == Some(s.run))
        })
    }

    /// The timeline, while it is on screen: this viewer is in the game, or
    /// was when it ended (the whole game is then there to look back over; a
    /// viewer who was not in it may still want to watch it unspoiled).
    fn watching(&self) -> Option<&Watch> {
        let session = self.update.as_ref()?.session.as_ref()?;
        self.watch.as_ref().filter(|w| w.run == session.run && (self.in_game() || session.ended))
    }

    /// Takes in what an update says for the timeline: a new run, new
    /// statistics, a jump.
    fn sync_watch(&mut self) {
        let in_game = self.in_game();
        let Some(update) = &self.update else { return };
        let Some(session) = &update.session else { return };
        if in_game && self.watch.is_none() {
            self.watch = Some(Watch {
                timeline: Timeline::waiting(replay_length(session) as f32),
                run: session.run,
                in_game: false,
                applied: None,
                jump_seen: None,
                last_report: None,
            });
        }
        let Some(watch) = &mut self.watch else { return };
        // This viewer's engine has just got into a game: a new run, or the
        // same one again. Either way it starts from the beginning.
        if in_game && (!watch.in_game || watch.run != session.run) {
            watch.timeline.restart();
            watch.timeline.await_pace();
            watch.run = session.run;
            watch.jump_seen = session.jump;
            watch.last_report = None;
        }
        watch.in_game = in_game;
        // A new report: an engine that went much further than its speed
        // allows since the one before is running ahead to the party.
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
        if let Some(view) = &update.timeline {
            if !watch.applied.as_ref().is_some_and(|applied| Arc::ptr_eq(applied, view)) {
                let (feed, data) = timeline::statistics(view, replay_length(session));
                watch.timeline.set_statistics(feed, data);
                watch.applied = Some(view.clone());
            }
        }
        if watch.run == session.run && session.jump != watch.jump_seen {
            watch.jump_seen = session.jump;
            if let (Some(jump), true) = (session.jump, in_game) {
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
        let Some(session) = update.session.as_ref().filter(|s| s.run == watch.run) else { return };
        if session.ended {
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
        running && !update.session.as_ref().is_some_and(|s| s.ended)
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
        let Some(session) = update.session.as_ref().filter(|_| installed && self.art.loading.is_none()) else { return };
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
        self.art.loading = Some(icons::Loading::start(data.root.clone(), session.replay.game.clone(), wanted, wake));
    }
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
    clipboard: Option<arboard::Clipboard>,

    /// The invite link being typed or pasted.
    link: String,
    party: Option<Party>,
    /// A link that arrived from outside (a browser) and awaits the user's consent.
    pending: Option<Link>,

    replays: Arc<Vec<LocalReplay>>,
    /// Lowercased search text per replay, parallel to `replays`.
    haystacks: Vec<String>,
    scanning: bool,
    filter: String,
    /// Indices into `replays` that pass the filter, in list order.
    visible: Arc<Vec<usize>>,
    selected: Option<usize>,
    /// An upload to a server is in flight.
    creating: bool,

    /// Map name → picture; `None` once a picture turned out to be unavailable.
    pictures: HashMap<String, Option<Image>>,

    name: String,
    data_dir: String,
    link_status: uri::Status,
}

impl Ui {
    pub fn new(backend: Backend, cfg: Config, data: DataDir, opened: Option<Link>) -> Ui {
        let name = if cfg.name.is_empty() { datadir::default_name(&data) } else { cfg.name.clone() };
        let mut ui = Ui {
            backend,
            data_dir: data.root.display().to_string(),
            data,
            cfg,
            tab: TAB_PARTY.into(),
            selection: Selection::default(),
            toasts: Vec::new(),
            focus_requests: Vec::new(),
            clipboard: None,
            link: String::new(),
            party: None,
            pending: opened,
            replays: Arc::new(Vec::new()),
            haystacks: Vec::new(),
            scanning: false,
            filter: String::new(),
            visible: Arc::new(Vec::new()),
            selected: None,
            creating: false,
            pictures: HashMap::new(),
            name,
            link_status: uri::status(),
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

    fn join(&mut self, server: Url, session: String) {
        let settings = client::Settings { name: self.name.clone(), data: self.data.clone(), headless: false };
        self.backend.send(Cmd::Join { server: server.clone(), session: session.clone(), settings });
        self.party = Some(Party::new(server, session));
        self.tab = TAB_PARTY.into();
        self.link.clear();
    }

    fn open_link(&mut self, link: Link) {
        match link {
            Link::Join { server, session } => self.join(server, session),
            Link::Pair { server, token } => {
                self.cfg.pair(server.as_str(), &token);
                self.save_config();
                self.toasts.push(ToastSpec::success(format!(
                    "Paired with {}. You can now start watch parties from My replays.",
                    host_of(&server)
                )));
            }
        }
    }

    fn save_config(&mut self) {
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
        let party = self.party.as_ref()?;
        api::invite_url(&party.server, &party.session).ok().map(String::from)
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Session(update) => {
                if let Some(notice) = &update.notice {
                    self.toasts.push(ToastSpec::warning(notice.clone()));
                }
                if let Some(map) = update.session.as_ref().map(|s| s.replay.map.clone()) {
                    self.want_picture(&map);
                }
                if let Some(party) = &mut self.party {
                    party.update = Some(*update);
                    party.sync_watch();
                }
            }
            Msg::SessionEnded => {
                if let Some(party) = &mut self.party {
                    party.over = true;
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
            Msg::Created { server, id } => {
                self.creating = false;
                if let Ok(invite) = api::invite_url(&server, &id) {
                    self.copy(invite.as_str(), "Invite link");
                }
                self.join(server, id);
            }
            Msg::CreateFailed(e) => {
                self.creating = false;
                self.toasts.push(ToastSpec::error(e));
            }
            // A link opened while this window was up: it asks here, like one that
            // started the app.
            Msg::Opened(Some(text)) => match Link::parse_any(&text) {
                Ok(link) => self.pending = Some(link),
                Err(e) => self.toasts.push(ToastSpec::error(format!("That link could not be opened: {e:#}"))),
            },
            Msg::Opened(None) => self.toasts.push(ToastSpec::info("barwatch is already open.")),
        }
    }

    /// The server new parties are created on: the one paired last.
    fn create_server(&self) -> Option<(Url, String)> {
        let s = self.cfg.servers.last()?;
        Some((api::parse_server(&s.url).ok()?, s.token.clone()))
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
}

/// Setters for canned scenes ([`crate::fixtures`]).
impl Ui {
    pub fn fixture_done_scanning(&mut self) {
        self.scanning = false;
    }

    pub fn fixture_party(&mut self, server: Url, update: client::Update) {
        let session = update.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
        self.party = Some(Party::new(server, session));
        self.fixture_update(update);
        // One report is all a canned scene has: its game goes at the party's
        // pace unless the scene says otherwise.
        if let Some(timeline) = self.fixture_timeline() {
            timeline.set_catching_up(false);
        }
    }

    /// The next thing the party's client says, as the window takes it in.
    pub fn fixture_update(&mut self, update: client::Update) {
        self.apply(Msg::Session(Box::new(update)));
        self.toasts.clear();
        if let Some(party) = &mut self.party {
            party.tick_watch();
        }
    }

    /// The party's timeline, once this viewer has been in game.
    pub fn fixture_timeline(&mut self) -> Option<&mut Timeline> {
        self.party.as_mut()?.watch.as_mut().map(|w| &mut w.timeline)
    }

    /// DEVELOPMENT: waits for the marker pictures of the installed game, for a
    /// one-frame render that has no later frame to show them in.
    pub fn fixture_wait_for_art(&mut self, timeout: Duration) {
        let Some(party) = &mut self.party else { return };
        party.tick_art(&self.data, || {});
        if let Some(loading) = party.art.loading.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(loading.wait(timeout));
            // Handed back as a finished load, for `tick_art` to take in.
            if let Ok(Some(result)) = rx.recv() {
                party.art.loading = Some(icons::Loading::ready(result));
            }
        }
        party.tick_art(&self.data, || {});
    }

    pub fn fixture_replays(&mut self, replays: Vec<LocalReplay>, selected: Option<usize>) {
        self.haystacks = replays.iter().map(LocalReplay::haystack).collect();
        self.replays = Arc::new(replays);
        self.refilter();
        self.selected = selected;
        self.tab = TAB_REPLAYS.into();
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
        if let Some(party) = &mut self.party {
            party.tick_watch();
            party.tick_art(&self.data, self.backend.waker());
        }
    }

    fn build(&self, cx: &BuildCx) -> El {
        let body = match self.tab.as_str() {
            TAB_REPLAYS => self.replays_view(),
            TAB_SETTINGS => self.settings_view(),
            _ => self.party_view(cx),
        };
        // The timeline's open list hangs from the timeline: only while that is drawn.
        let layers = self.watching().map(|w| w.timeline.layers()).unwrap_or_default();
        overlays(
            page([
                tabs_list(
                    "tab",
                    &self.tab,
                    [(TAB_PARTY, "Watch party"), (TAB_REPLAYS, "My replays"), (TAB_SETTINGS, "Settings")],
                ),
                body,
            ])
            .gap(tokens::SPACE_4),
            layers.into_iter().map(Some).chain([self.pending.as_ref().map(consent_dialog)]),
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
                self.backend.send(Cmd::Session(client::Command::Jump(seconds.round().max(0.0) as u32)));
            }
        }

        // Watch party.
        if event.is_click_or_activate("join") || text_input::submitted(&event, "link") {
            match Link::parse_any(&self.link) {
                // Typed or pasted by the user: that is the consent.
                Ok(link) => self.open_link(link),
                Err(e) => self.toasts.push(ToastSpec::error(format!("That link does not work: {e:#}"))),
            }
            return;
        }
        if event.is_click_or_activate("party:start") {
            self.backend.send(Cmd::Session(client::Command::Start));
        } else if event.is_click_or_activate("party:rejoin") {
            self.backend.send(Cmd::Session(client::Command::Rejoin));
        } else if event.is_click_or_activate("party:leave") {
            self.backend.send(Cmd::Session(client::Command::Leave));
            self.party = None;
        } else if event.is_click_or_activate("party:copy") {
            if let Some(link) = self.invite_link() {
                self.copy(&link, "Invite link");
            }
        }
        // My replays.
        else if event.is_click_or_activate("replays:refresh") {
            self.scan();
        } else if event.is_click_or_activate("replays:watch") {
            let chosen = self.selected.and_then(|i| self.replays.get(i)).map(|r| r.path.clone());
            if let (Some(path), Some((server, token)), false) = (chosen, self.create_server(), self.creating) {
                self.creating = true;
                self.backend.send(Cmd::Create { server, token, path });
            }
        } else if let Some(index) =
            event.route_index::<usize>("replay").filter(|_| matches!(event.kind, UiEventKind::Click | UiEventKind::Activate))
        {
            self.selected = Some(index);
            if let Some(map) = self.replays.get(index).and_then(|r| r.info.as_ref()).map(|i| i.map.clone()) {
                self.want_picture(&map);
            }
        }
        // Settings.
        else if event.is_click_or_activate("settings:save") {
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
        text_input::apply_event(&mut self.name, &mut self.selection, &event, "name");
        text_input::apply_event(&mut self.data_dir, &mut self.selection, &event, "data-dir");
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
const FIELD_NEIGHBOURS: [(&str, &str, &str); 4] = [
    ("link", "tab:tab:settings", "join"),
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
            "Join this watch party?",
            format!(
                "A link asks barwatch to join a watch party on {}. It will install what the replay needs and start Beyond All Reason as a spectator.",
                host_of(server)
            ),
            "Join",
        ),
        Link::Pair { server, .. } => (
            "Pair with this server?",
            format!(
                "A link asks to pair barwatch with {}. Pairing lets this app start watch parties there from your own replays.",
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
    /// The timeline, while it is on screen: in the party view, with no dialog over it.
    fn watching(&self) -> Option<&Watch> {
        let shown = self.tab == TAB_PARTY && self.pending.is_none();
        self.party.as_ref().filter(|_| shown)?.watching()
    }

    fn watching_mut(&mut self) -> Option<&mut Watch> {
        self.watching()?;
        self.party.as_mut()?.watch.as_mut()
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

    /// Map picture beside the replay's description.
    fn replay_card(&self, info: &ReplayInfo, side: f32) -> El {
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
        row([self.picture(&info.map, side), column(lines).gap(tokens::SPACE_2).fill_width()])
            .gap(tokens::SPACE_4)
            .align(Align::Start)
    }

    fn party_view(&self, cx: &BuildCx) -> El {
        let Some(party) = &self.party else {
            return column([
                card([
                    card_header([
                        card_title("Join a watch party"),
                        card_description("Paste the invite link you were sent, or press the join button on the invite page."),
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
                ]),
                text("To start one yourself, pick a replay under My replays.").muted(),
            ])
            .gap(tokens::SPACE_4);
        };
        let Some(update) = &party.update else {
            return row([spinner(), text(format!("Connecting to {}…", host_of(&party.server)))])
                .gap(tokens::SPACE_3)
                .align(Align::Center);
        };
        let mut sections: Vec<El> = Vec::new();
        match (party.watching(), &update.session) {
            // Watching: the timeline takes the window. What the replay is
            // shrinks to one line, with the party's buttons on it.
            (Some(watch), Some(session)) => {
                sections.push(self.watching_bar(update, session, party.over, watch.timeline.game_state()));
                if update.stage != Stage::Playing || party.over {
                    sections.push(status_block(update, party.over));
                }
                let view = column([watch.timeline.view(cx)]).width(Size::Fill(1.0));
                sections.push(if party.game_line_moves() { view.redraw_within(GAME_LINE_FRAME) } else { view });
                if update.stage == Stage::Playing {
                    sections.push(text(CHAT_HINT).caption().muted().ellipsis());
                }
            }
            _ => {
                if let Some(session) = &update.session {
                    sections.push(card([card_content([self.replay_card(&session.replay, 200.0)]).pt(tokens::SPACE_5)]));
                }
                sections.push(status_block(update, party.over));
                let (start, leave) = party_buttons(update, party.over);
                sections.push(row(start.into_iter().chain([spacer(), leave])).gap(tokens::SPACE_2).align(Align::Center));
                if let (Some(view), Some(session), false) = (&update.timeline, &update.session, party.over) {
                    sections.push(statistics_line(view, session));
                }
            }
        }
        if let Some(session) = &update.session {
            sections.push(viewers_card(session, &update.name));
        }
        // Air between the content's right edge and the scrollbar's lane.
        scroll(sections)
            .key("party-scroll")
            .gap(tokens::SPACE_4)
            .padding(Sides { right: tokens::SPACE_2, ..Sides::zero() })
            .fill_height()
    }

    /// What is being watched, in one line: the map's picture, the map and
    /// who plays, where this viewer stands, and the party's buttons. The
    /// game's length is left out: the timeline below may be hiding it.
    /// "In game" only once the game says where it is: an engine that is open
    /// may still be loading, or running ahead to where the party is.
    fn watching_bar(&self, update: &client::Update, session: &SessionView, over: bool, game: timeline::GameState) -> El {
        let state = if session.ended {
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
        let (start, leave) = party_buttons(update, over);
        let mut items = vec![
            self.picture(&session.replay.map, 40.0),
            column([
                text(session.replay.map.clone()).label().ellipsis(),
                text(versus(&session.replay)).caption().muted().ellipsis(),
            ])
            .gap(2.0)
            .width(Size::Fill(1.0)),
            state,
        ];
        items.extend(start);
        items.push(leave);
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
        let Some(replay) = self.selected.and_then(|i| self.replays.get(i)) else {
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
        match self.create_server() {
            Some((server, _)) => {
                let label = if self.creating { "Uploading…" } else { "Watch together" };
                let mut start = button(label).key("replays:watch").primary();
                if self.creating {
                    start = start.disabled();
                }
                body.push(start);
                body.push(text(format!("Uploads the replay to {} and copies an invite link.", host_of(&server))).caption().muted().wrap_text());
            }
            None => body.push(alert([
                alert_title("Not paired with a server"),
                alert_description("Open your barwatch server's page in a browser and press “Pair the desktop app”. Then you can start watch parties from here."),
            ])),
        }
        card([card_content([scroll(body).key("preview-scroll").gap(tokens::SPACE_2)]).pt(tokens::SPACE_5)])
    }

    fn settings_view(&self) -> El {
        let engines = barwatch_core::engine::installed(&self.data.engines()).len();
        let data_note = if self.data.own {
            "No Beyond All Reason installation was found, so this is barwatch's own folder: everything a replay needs will be downloaded into it.".to_string()
        } else {
            format!("{engines} engine version(s) installed here. barwatch shares this folder with BAR, so neither downloads what the other already has.")
        };
        let servers: Vec<El> = if self.cfg.servers.is_empty() {
            vec![text("None. Pairing is only needed to start watch parties from this app; joining never needs it.")
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
                ("Link handling is not supported on this system; paste invite links instead.", badge("unsupported").muted())
            }
        };
        scroll([
            titled_card(
                "You",
                [field_with(
                    "Name",
                    text_input("name", &self.name, &self.selection),
                    FieldOpts::default().description("Shown to the others in the party and in game. Changed slightly if a player in the replay has the same name."),
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
                "Invite links",
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

/// The party's buttons: those that start something and the invite link, and
/// the one that leaves, which stands apart at the far end.
fn party_buttons(update: &client::Update, over: bool) -> (Vec<El>, El) {
    let phase = update.session.as_ref().map(|s| s.phase);
    // After a run the engine may still be open: the next one can start regardless.
    let installed = matches!(update.stage, Stage::Ready | Stage::Closed | Stage::Playing);
    let mut buttons: Vec<El> = Vec::new();
    if !over && installed && matches!(phase, Some(Phase::Idle | Phase::Failed)) {
        let again = update.session.as_ref().is_some_and(|s| s.run > 0);
        buttons.push(button(if again { "Start again for everyone" } else { "Start for everyone" }).key("party:start").primary());
    }
    if !over && update.stage == Stage::Closed && phase == Some(Phase::Running) {
        buttons.push(button("Rejoin").key("party:rejoin").primary());
    }
    buttons.push(button("Copy invite link").key("party:copy").secondary());
    (buttons, button(if over { "Close" } else { "Leave" }).key("party:leave").ghost())
}

/// Game seconds an engine may go between two reports beyond what its speed
/// accounts for (the reports come about once a second, read every 0.1 s).
const CATCH_UP_SLACK_S: f64 = 2.0;

/// Whether an engine that reported `last` and then `now` is running ahead
/// of its own speed: catching up to the party after joining late or a jump.
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
fn replay_length(session: &SessionView) -> u32 {
    match session.replay.duration_s {
        0 => barwatch_core::timeline::MAX_FRAME / 30,
        s => s,
    }
}

/// Where the statistics stand, in one quiet line, for a viewer who is not
/// in the game yet: the stats service starts on a replay when its party is
/// made, so this says whether the timeline will have numbers from the start.
fn statistics_line(view: &TimelineView, session: &SessionView) -> El {
    let so_far = |doc: &barwatch_core::timeline::TimelineDoc| {
        let done = doc.frames / 30;
        format!("worked out up to {} of {}", clock(done), clock(session.replay.duration_s.max(done)))
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
    let phase = update.session.as_ref().map(|s| s.phase);
    let note = update.session.as_ref().and_then(|s| s.note.clone());
    let busy = |label: String| row([spinner(), text(label)]).gap(tokens::SPACE_3).align(Align::Center);
    match &update.stage {
        Stage::Failed(error) => alert([alert_title("This did not work"), alert_description(error.clone())]).destructive(),
        _ if over => alert([alert_title("You left this watch party")]),
        Stage::Connecting => busy("Connecting…".into()),
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
                "The party is still going. Rejoin to get back in."
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

fn viewers_card(session: &SessionView, me: &str) -> El {
    let rows: Vec<El> = session
        .viewers
        .iter()
        .map(|v| {
            let state = if v.in_game {
                badge("in game").success()
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
            if !v.detail.is_empty() {
                content.push(item_description(v.detail.clone()));
            }
            item([item_content(content), item_actions([state])]).align(Align::Center)
        })
        .collect();
    titled_card(format!("Viewers ({})", session.viewers.len()), [item_group(rows)])
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

    /// The party's timeline as the client's updates drive it (DESIGN.md §5.4, §5.7).
    mod watch {
        use super::*;
        use crate::fixtures::{at, docs, view, watching};
        use api::TimelineState;

        fn stats() -> Option<Arc<TimelineView>> {
            view(TimelineState::Complete, None, Some(docs::duel()))
        }

        fn party(update: client::Update) -> Party {
            let mut party = Party::new(Url::parse("https://watch.example.org/").unwrap(), "abcdefghijklmnopqrstuv".into());
            party.update = Some(update);
            party.sync_watch();
            party.tick_watch();
            party
        }

        /// The next update from the client, as the window takes it in.
        fn next(party: &mut Party, update: impl FnOnce(&mut client::Update)) {
            let mut u = party.update.clone().unwrap();
            u.jump_refused = false;
            update(&mut u);
            party.update = Some(u);
            party.sync_watch();
            party.tick_watch();
        }

        fn jump(target_s: u32, at_unix: u64) -> impl FnOnce(&mut client::Update) {
            move |u| u.session.as_mut().unwrap().jump = Some(JumpView { target_s, at_unix })
        }

        fn jumping(party: &Party) -> Option<f32> {
            party.watching().and_then(|w| w.timeline.jumping_to())
        }

        #[test]
        fn shown_once_in_game_with_its_statistics() {
            let mut out = watching(false, None, stats());
            out.stage = Stage::Ready;
            out.session.as_mut().unwrap().phase = Phase::Idle;
            let mut p = party(out);
            assert!(p.watching().is_none(), "not before the game runs");
            next(&mut p, |u| {
                u.stage = Stage::Playing;
                u.session.as_mut().unwrap().phase = Phase::Running;
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

        /// A late joiner's engine: loading, then running ahead to the party,
        /// then going at its pace; the timeline says which.
        #[test]
        fn a_late_joiner_is_loading_then_catching_up_then_in_game() {
            use timeline::GameState;
            let mut p = party(watching(false, None, stats()));
            let state = |p: &Party| p.watching().unwrap().timeline.game_state();
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
            assert_eq!(jumping(&p), None, "the engine is catching up to the party, not to that jump");
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
                let s = u.session.as_mut().unwrap();
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
                let s = u.session.as_mut().unwrap();
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

    #[test]
    fn display_helpers() {
        assert_eq!(clock(834), "13:54");
        assert_eq!(clock(3723), "1:02:03");
        let info = ReplayInfo { teams: vec![vec!["a".into(), "b".into()], vec!["c".into()]], ..Default::default() };
        assert_eq!(versus(&info), "a, b  vs  c");
        assert_eq!(versus(&ReplayInfo::default()), "no players");
    }
}
