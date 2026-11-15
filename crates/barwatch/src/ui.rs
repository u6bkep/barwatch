//! The window: join a party, browse local replays, settings.

use crate::backend::{Backend, Cmd, Msg};
use crate::client::{self, Stage};
use crate::config::Config;
use crate::datadir::{self, DataDir};
use crate::replays::LocalReplay;
use crate::uri;
use barwatch_core::api::{self, Link, Phase, ReplayInfo, SessionView, ViewerState};
use damascene_core::prelude::*;
use damascene_core::widgets::{tabs, text_input};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

const TAB_PARTY: &str = "party";
const TAB_REPLAYS: &str = "replays";
const TAB_SETTINGS: &str = "settings";
const REPLAY_ROW_HEIGHT: f32 = 52.0;
const CHAT_HINT: &str = "In game chat: !skip 14:00   !skip +2m   !pause   !resume   !speed 2";

/// The party this app is in (or was, until the user dismisses it).
struct Party {
    server: Url,
    session: String,
    update: Option<client::Update>,
    /// The client has stopped: left, failed, or the party is gone.
    over: bool,
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
        let visible: Vec<usize> = (0..self.replays.len())
            .filter(|&i| needle.is_empty() || self.haystacks[i].contains(&needle))
            .collect();
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
        self.party = Some(Party { server, session, update: None, over: false });
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
                    party.update = Some(update);
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
        self.party = Some(Party { server, session, update: Some(update), over: false });
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
    }

    fn build(&self, _cx: &BuildCx) -> El {
        let body = match self.tab.as_str() {
            TAB_REPLAYS => self.replays_view(),
            TAB_SETTINGS => self.settings_view(),
            _ => self.party_view(),
        };
        overlays(
            page([
                tabs_list("tab", &self.tab, [(TAB_PARTY, "Watch party"), (TAB_REPLAYS, "My replays"), (TAB_SETTINGS, "Settings")]),
                body,
            ])
            .gap(tokens::SPACE_4),
            [self.pending.as_ref().map(consent_dialog)],
        )
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    fn drain_toasts(&mut self) -> Vec<ToastSpec> {
        std::mem::take(&mut self.toasts)
    }

    fn drain_focus_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.focus_requests)
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
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
        if event.is_click_or_activate("consent:no") || event.is_route("consent:dismiss") {
            self.pending = None;
            return;
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
        } else if let Some(index) = event
            .route_index::<usize>("replay")
            .filter(|_| matches!(event.kind, UiEventKind::Click | UiEventKind::Activate))
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
        } else if let Some(index) = event
            .route_index::<usize>("unpair")
            .filter(|_| matches!(event.kind, UiEventKind::Click | UiEventKind::Activate))
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
            dialog_footer([
                button("Not now").key("consent:no").ghost(),
                button(action).key("consent:yes").primary(),
            ]),
        ],
    )
}

impl Ui {
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

    fn party_view(&self) -> El {
        let Some(party) = &self.party else {
            return column([
                card([
                    card_header([
                        card_title("Join a watch party"),
                        card_description("Paste the invite link you were sent, or press the join button on the invite page."),
                    ]),
                    card_content([row([
                        text_input_with("link", &self.link, &self.selection, TextInputOpts::default().placeholder("https://…/app/join/…"))
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
        if let Some(session) = &update.session {
            sections.push(card([card_content([self.replay_card(&session.replay, 200.0)]).pt(tokens::SPACE_5)]));
        }
        sections.push(status_block(update, party.over));
        sections.push(self.party_actions(update, party.over));
        if let Some(session) = &update.session {
            sections.push(viewers_card(session, &update.name));
        }
        scroll(sections).key("party-scroll").gap(tokens::SPACE_4).fill_height()
    }

    fn party_actions(&self, update: &client::Update, over: bool) -> El {
        let phase = update.session.as_ref().map(|s| s.phase);
        let installed = matches!(update.stage, Stage::Ready | Stage::Closed);
        let mut buttons: Vec<El> = Vec::new();
        if !over && installed && matches!(phase, Some(Phase::Idle | Phase::Failed)) {
            let again = update.session.as_ref().is_some_and(|s| s.run > 0);
            buttons.push(button(if again { "Start again for everyone" } else { "Start for everyone" }).key("party:start").primary());
        }
        if !over && update.stage == Stage::Closed && phase == Some(Phase::Running) {
            buttons.push(button("Rejoin").key("party:rejoin").primary());
        }
        buttons.push(button("Copy invite link").key("party:copy").secondary());
        buttons.push(spacer());
        buttons.push(button(if over { "Close" } else { "Leave" }).key("party:leave").ghost());
        row(buttons).gap(tokens::SPACE_2).align(Align::Center)
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
            text_input_with("filter", &self.filter, &self.selection, TextInputOpts::default().placeholder("Filter by map or player"))
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
            return card([card_content([text(replay.file_name.clone()).wrap_text(), text("This file is not a readable replay.").muted()])
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
            vec![text("None. Pairing is only needed to start watch parties from this app; joining never needs it.").muted().wrap_text()]
        } else {
            self.cfg
                .servers
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    row([text(s.url.clone()).ellipsis().fill_width(), button("Unpair").key(format!("unpair:{i}")).ghost().destructive()])
                        .gap(tokens::SPACE_2)
                        .align(Align::Center)
                })
                .collect()
        };
        let (link_text, link_badge) = match self.link_status {
            uri::Status::Registered => ("barwatch:// links open this app.", badge("working").success()),
            uri::Status::Missing => ("barwatch:// links do not open this app yet.", badge("not set up").warning()),
            uri::Status::Unsupported => ("Link handling is not supported on this system; paste invite links instead.", badge("unsupported").muted()),
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
            alert_description(if phase == Some(Phase::Running) { "The party is still going. Rejoin to get back in." } else { "The replay is not running right now." }),
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
    fn display_helpers() {
        assert_eq!(clock(834), "13:54");
        assert_eq!(clock(3723), "1:02:03");
        let info = ReplayInfo { teams: vec![vec!["a".into(), "b".into()], vec!["c".into()]], ..Default::default() };
        assert_eq!(versus(&info), "a, b  vs  c");
        assert_eq!(versus(&ReplayInfo::default()), "no players");
    }
}
