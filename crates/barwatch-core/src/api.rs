//! What the daemon and the desktop app say to each other, and the `barwatch://`
//! links that start the app.
//!
//! The app talks to `<server>/app/v1/…`, the one path prefix a reverse proxy lets
//! through without a login. A room id is the capability to join and control that room;
//! creating, deleting and re-linking rooms needs a pairing token.

use crate::names;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

/// Bumped when the app and daemon stop understanding each other. The app refuses a
/// server with another number, so additions an older side can ignore (optional
/// fields, new calls) do not bump it.
pub const API_VERSION: u32 = 2;
pub const APP_PREFIX: &str = "/app/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    pub api: u32,
}

/// What a replay is, as shown before anyone starts an engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReplayInfo {
    pub file_name: String,
    pub engine: String,
    pub game: String,
    pub map: String,
    /// Base name of the map archive on BAR's map API, when known (for pictures).
    pub map_file: Option<String>,
    pub start_unix: u64,
    pub duration_s: u32,
    pub teams: Vec<Vec<String>>,
    pub spectators: Vec<String>,
}

impl ReplayInfo {
    /// Describes a parsed replay. `None` if it names no map or game version.
    pub fn from_demo(demo: &crate::demo::Demo, file_name: &str, map_file: Option<String>) -> Option<ReplayInfo> {
        let roster = demo.roster();
        Some(ReplayInfo {
            file_name: file_name.to_owned(),
            engine: demo.engine_version.clone(),
            game: demo.game_version()?.to_owned(),
            map: demo.map()?.to_owned(),
            map_file,
            start_unix: demo.unix_time,
            duration_s: demo.game_time_s,
            teams: roster.teams,
            spectators: roster.spectators,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The room has no replay yet.
    Empty,
    /// The server is still fetching the replay or the engine it needs.
    Preparing,
    /// Ready; nothing is running. Viewers gather here until someone starts.
    Idle,
    /// The hosting server has been launched and is not listening yet.
    Starting,
    /// The hosting server accepts viewers on `game_port`.
    Running,
    /// The replay or its engine could not be provided; `note` says why. Starting retries.
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ViewerState {
    /// Downloading the engine, game or map.
    Preparing,
    /// Everything is installed; waiting for the room to run.
    Ready,
    /// The engine is running on the viewer's machine.
    Playing,
    /// Something went wrong on the viewer's side; `detail` says what.
    Error,
}

/// A member of a room: a desktop app that registered and keeps reporting, or an
/// engine the hosting server reports that came without one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewerView {
    /// Unique among the room's members.
    pub name: String,
    /// The member's state for the room's current replay: what it reported, if the
    /// report was about that replay (`replay`), else `Preparing`.
    pub state: ViewerState,
    pub detail: String,
    /// The hosting server reports `engine_name` as connected.
    pub in_game: bool,
    /// The name this member's engine must use for the current replay: unique against
    /// the replay's players, the other members' engine names and the names in the
    /// game. It stays put while the replay is current and is given afresh when it
    /// changes.
    pub engine_name: String,
    /// The replay key the member's last report was about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
}

/// A room's current replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomReplay {
    /// The replay's key at this server: the game id in its header (32 lowercase hex
    /// digits), or for a replay without one `u` and 31 hex digits of its hash.
    pub key: String,
    pub info: ReplayInfo,
    /// The game's id at the stats service, once the service has confirmed it holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    /// The game's page at the stats service (behind its login), when the server knows
    /// where that is and the service has the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_url: Option<String>,
    /// When the room picked it.
    pub picked_unix: u64,
}

/// A replay the room played before.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub key: String,
    pub info: ReplayInfo,
    /// When the room picked it.
    pub picked_unix: u64,
    /// The server has the file or can fetch it again: picking it works.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_url: Option<String>,
}

/// A room: a place with a link where a group watches replays together.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomView {
    /// The id in the room's link; the capability to see, join and control it.
    pub id: String,
    /// Increases with every change; pass it back as `rev` to wait for the next one.
    pub rev: u64,
    pub name: String,
    pub phase: Phase,
    /// Error text, or why the last run ended.
    pub note: Option<String>,
    /// Where engines connect while `Running`. `None` means "the host of the server URL".
    pub game_host: Option<String>,
    pub game_port: Option<u16>,
    /// Counts server launches, so a viewer can tell a new run from the one it left.
    pub run: u32,
    pub paused: bool,
    pub speed: f32,
    /// The current run has played the replay to its end.
    pub ended: bool,
    /// The last jump the server issued in this run, so every viewer's app can show it
    /// while its own engine is still on the way. Cleared by a new run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<JumpView>,
    pub members: Vec<ViewerView>,
    /// `None` until someone picks a replay.
    pub replay: Option<RoomReplay>,
    /// What the room played before, newest first; the current replay is not in it.
    pub history: Vec<HistoryEntry>,
}

/// What `GET rooms/{id}/timeline[?rev=<n>&wait=<s>]` answers: the statistics of the
/// room's current replay as far as they have been worked out. Long-polls like the
/// room call: with `rev`, the answer waits until the view's `rev` differs from it,
/// the room's replay changes, or `wait` runs out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimelineView {
    /// Increases with every change of anything below, for one replay. It is the
    /// server's own count and restarts with the server; a view of another replay
    /// (`replay` differs) has a count of its own.
    pub rev: u64,
    pub state: TimelineState,
    /// For `Unavailable` and `Failed`: why, in words for the viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Present from `Building` on, and kept when the service is lost after that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<crate::timeline::TimelineDoc>,
    /// The key of the replay this view is about; absent when the room has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineState {
    /// No stats service is configured, or it cannot be reached.
    Unavailable,
    /// The service has the game and has not started on it.
    Queued,
    /// Being simulated: `doc` grows.
    Building,
    /// `doc` is the whole game.
    Complete,
    /// The service could not process the game.
    Failed,
}

/// A jump the server issued: see [`RoomView::jump`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct JumpView {
    /// Game time jumped to, in seconds.
    pub target_s: u32,
    /// When the server issued it.
    pub at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub name: String,
    pub app_version: String,
    /// Set when registering again while this viewer's engine is still in the game:
    /// the name that engine uses, to be kept instead of handing out a fresh one.
    #[serde(default)]
    pub playing_as: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    /// Secret of this viewer; needed to report status or leave.
    pub viewer_id: String,
    /// The member's name in the room: the requested one, changed if it was taken.
    /// The name its engine must use is this member's `engine_name` in `room.members`.
    pub name: String,
    pub room: RoomView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerUpdate {
    pub state: ViewerState,
    #[serde(default)]
    pub detail: String,
    /// The key of the replay this state is about (`RoomReplay::key`). A report about
    /// another replay than the room's current one shows as `Preparing`.
    #[serde(default)]
    pub replay: Option<String>,
}

/// `POST rooms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRoom {
    #[serde(default)]
    pub name: String,
}

/// `PUT rooms/{id}/replay` as JSON: exactly one of the two. (The other form of the
/// call sends the replay file itself, its name in `x-barwatch-filename`.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PickReplay {
    /// A replay id or a link to the replay on BAR's site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar_replay: Option<String>,
    /// A key from the room's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// What `POST archive` answers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Archived {
    pub game_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_url: Option<String>,
}

/// A playback command; the same ones game chat accepts as `!skip`, `!pause`, `!speed`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Control {
    /// Jump forward: to an absolute game time, or by `seconds` from here.
    Skip {
        seconds: u32,
        relative: bool,
    },
    Pause {
        paused: bool,
    },
    Speed {
        speed: f32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// A `barwatch://` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `barwatch://join?server=<url>&room=<id>`
    Join { server: Url, room: String },
    /// `barwatch://pair?server=<url>&token=<token>`: lets this app create rooms.
    Pair { server: Url, token: String },
}

impl Link {
    pub fn parse(text: &str) -> Result<Link> {
        let url = Url::parse(text.trim()).context("not a link")?;
        ensure!(url.scheme() == "barwatch", "not a barwatch:// link");
        let param = |key: &str| -> Result<String> {
            url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned()).with_context(|| format!("link has no '{key}'"))
        };
        let server = parse_server(&param("server")?)?;
        match url.host_str() {
            Some("join") => {
                let room = param("room")?;
                ensure!(names::valid_id(&room), "malformed room id");
                Ok(Link::Join { server, room })
            }
            Some("pair") => {
                let token = param("token")?;
                ensure!(names::valid_id(&token), "malformed token");
                Ok(Link::Pair { server, token })
            }
            other => bail!("unknown link kind {:?}", other.unwrap_or("")),
        }
    }

    /// Like [`parse`](Self::parse), and also accepts the invite page's address
    /// (`https://server/app/join/<id>`), which is what people paste.
    pub fn parse_any(text: &str) -> Result<Link> {
        let text = text.trim();
        if !(text.starts_with("http://") || text.starts_with("https://")) {
            return Link::parse(text);
        }
        let url = Url::parse(text).context("not a link")?;
        let (prefix, room) =
            url.path().trim_end_matches('/').rsplit_once("/app/join/").context("this is not a barwatch invite link")?;
        ensure!(names::valid_id(room), "malformed room id");
        let mut server = url.clone();
        server.set_path(prefix);
        Ok(Link::Join { server: parse_server(server.as_str())?, room: room.to_owned() })
    }

    pub fn server(&self) -> &Url {
        match self {
            Link::Join { server, .. } | Link::Pair { server, .. } => server,
        }
    }
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, key, value, server) = match self {
            Link::Join { server, room } => ("join", "room", room, server),
            Link::Pair { server, token } => ("pair", "token", token, server),
        };
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("server", server.as_str())
            .append_pair(key, value)
            .finish();
        write!(f, "barwatch://{kind}?{query}")
    }
}

/// A server base URL: http(s), with a host, without query or fragment. The path is
/// kept (a proxy may mount barwatch below a prefix) and always ends in `/`.
pub fn parse_server(text: &str) -> Result<Url> {
    let mut url = Url::parse(text.trim()).context("malformed server address")?;
    ensure!(matches!(url.scheme(), "http" | "https"), "server address must be http or https");
    ensure!(url.host_str().is_some(), "server address has no host");
    ensure!(url.username().is_empty() && url.password().is_none(), "server address must not carry credentials");
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// `<server>/app/v1/<path>`.
pub fn app_url(server: &Url, path: &str) -> Result<Url> {
    let rel = format!("{}/{}", APP_PREFIX.trim_start_matches('/'), path.trim_start_matches('/'));
    server.join(&rel).context("building request URL")
}

/// The page a browser opens to join: `<server>/app/join/<id>`. Reachable without a
/// login, so it is the link to hand to friends.
pub fn invite_url(server: &Url, room: &str) -> Result<Url> {
    server.join(&format!("app/join/{room}")).context("building invite URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let join = Link::Join { server: parse_server("https://bar.example.org").unwrap(), room: "abcdefghijklmnopqrstuv".into() };
        let text = join.to_string();
        assert_eq!(text, "barwatch://join?server=https%3A%2F%2Fbar.example.org%2F&room=abcdefghijklmnopqrstuv");
        assert_eq!(Link::parse(&text).unwrap(), join);

        let pair = Link::Pair { server: parse_server("http://10.0.0.5:8780/watch").unwrap(), token: "T".repeat(32) };
        assert_eq!(Link::parse(&pair.to_string()).unwrap(), pair);
        assert_eq!(pair.server().as_str(), "http://10.0.0.5:8780/watch/");
    }

    #[test]
    fn invite_page_addresses_are_links_too() {
        let id = "abcdefghijklmnopqrstuv";
        for (text, server) in [
            (format!("https://bar.example.org/app/join/{id}"), "https://bar.example.org/"),
            (format!(" http://10.0.0.5:8780/watch/app/join/{id}/ "), "http://10.0.0.5:8780/watch/"),
        ] {
            let link = Link::parse_any(&text).unwrap();
            assert_eq!(link, Link::Join { server: Url::parse(server).unwrap(), room: id.into() });
            assert_eq!(invite_url(link.server(), id).unwrap().as_str(), text.trim().trim_end_matches('/'));
        }
        assert!(Link::parse_any("https://bar.example.org/").is_err());
        assert!(Link::parse_any("https://bar.example.org/app/join/x").is_err());
        assert!(Link::parse_any(&format!("barwatch://join?server=https://x.org&room={id}")).is_ok());
    }

    #[test]
    fn hostile_links_are_rejected() {
        for bad in [
            "https://bar.example.org",
            "barwatch://join?server=file:///etc/passwd&room=abcdefghijklmnopqrstuv",
            "barwatch://join?server=https://x.org&room=../../x",
            "barwatch://join?server=https://x.org",
            "barwatch://run?server=https://x.org&room=abcdefghijklmnopqrstuv",
            "barwatch://join?server=https://user:pw@x.org&room=abcdefghijklmnopqrstuv",
            // A 0.3 invite: parties are gone.
            "barwatch://join?server=https://x.org&session=abcdefghijklmnopqrstuv",
        ] {
            assert!(Link::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_keep_a_proxy_prefix() {
        let server = parse_server("https://example.org/barwatch").unwrap();
        assert_eq!(app_url(&server, "rooms/abc").unwrap().as_str(), "https://example.org/barwatch/app/v1/rooms/abc");
        assert_eq!(invite_url(&server, "abc").unwrap().as_str(), "https://example.org/barwatch/app/join/abc");
    }

    #[test]
    fn control_wire_format() {
        let c: Control = serde_json::from_str(r#"{"action":"skip","seconds":840,"relative":false}"#).unwrap();
        assert_eq!(c, Control::Skip { seconds: 840, relative: false });
        assert_eq!(serde_json::to_string(&Control::Pause { paused: true }).unwrap(), r#"{"action":"pause","paused":true}"#);
    }
}
