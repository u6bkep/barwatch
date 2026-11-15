//! What the daemon and the desktop app say to each other, and the `barwatch://`
//! links that start the app.
//!
//! The app talks to `<server>/app/v1/…`, the one path prefix a reverse proxy lets
//! through without a login. A session id is the capability to join that session;
//! creating sessions needs a pairing token.

use crate::names;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

/// Bumped when the app and daemon stop understanding each other. The app refuses a
/// server with another number, so additions an older side can ignore (optional
/// fields, new calls) do not bump it.
pub const API_VERSION: u32 = 1;
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
    /// The server is still fetching the engine this replay needs.
    Preparing,
    /// Ready; nothing is running. Viewers gather here until someone starts.
    Idle,
    /// The hosting server has been launched and is not listening yet.
    Starting,
    /// The hosting server accepts viewers on `game_port`.
    Running,
    /// The engine could not be provided; `note` says why. Starting retries.
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ViewerState {
    /// Downloading the engine, game or map.
    Preparing,
    /// Everything is installed; waiting for the session to run.
    Ready,
    /// The engine is running on the viewer's machine.
    Playing,
    /// Something went wrong on the viewer's side; `detail` says what.
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewerView {
    pub name: String,
    pub state: ViewerState,
    pub detail: String,
    /// The hosting server reports this name as connected.
    pub in_game: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionView {
    pub id: String,
    /// Increases with every change; pass it back as `rev` to wait for the next one.
    pub rev: u64,
    pub created_unix: u64,
    pub replay: ReplayInfo,
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
    pub viewers: Vec<ViewerView>,
    /// The game's id at the stats service (the replay's own 16-byte id, in hex), once
    /// the server has one. Absent from servers without a stats service or older than
    /// the timeline; apps older than it ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    /// The last jump the server issued in this run, so every viewer's app can show it
    /// while its own engine is still on the way. Cleared by a new run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<JumpView>,
}

/// A jump the server issued: see [`SessionView::jump`].
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
    /// The name to give the engine: the requested one, changed if it was taken.
    pub name: String,
    pub session: SessionView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerUpdate {
    pub state: ViewerState,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Created {
    pub id: String,
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
    /// `barwatch://join?server=<url>&session=<id>`
    Join { server: Url, session: String },
    /// `barwatch://pair?server=<url>&token=<token>`: lets this app create sessions.
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
                let session = param("session")?;
                ensure!(names::valid_id(&session), "malformed session id");
                Ok(Link::Join { server, session })
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
        let (prefix, session) =
            url.path().trim_end_matches('/').rsplit_once("/app/join/").context("this is not a barwatch invite link")?;
        ensure!(names::valid_id(session), "malformed session id");
        let mut server = url.clone();
        server.set_path(prefix);
        Ok(Link::Join { server: parse_server(server.as_str())?, session: session.to_owned() })
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
            Link::Join { server, session } => ("join", "session", session, server),
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
pub fn invite_url(server: &Url, session: &str) -> Result<Url> {
    server.join(&format!("app/join/{session}")).context("building invite URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let join =
            Link::Join { server: parse_server("https://bar.example.org").unwrap(), session: "abcdefghijklmnopqrstuv".into() };
        let text = join.to_string();
        assert_eq!(text, "barwatch://join?server=https%3A%2F%2Fbar.example.org%2F&session=abcdefghijklmnopqrstuv");
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
            assert_eq!(link, Link::Join { server: Url::parse(server).unwrap(), session: id.into() });
            assert_eq!(invite_url(link.server(), id).unwrap().as_str(), text.trim().trim_end_matches('/'));
        }
        assert!(Link::parse_any("https://bar.example.org/").is_err());
        assert!(Link::parse_any("https://bar.example.org/app/join/x").is_err());
        assert!(Link::parse_any(&format!("barwatch://join?server=https://x.org&session={id}")).is_ok());
    }

    #[test]
    fn hostile_links_are_rejected() {
        for bad in [
            "https://bar.example.org",
            "barwatch://join?server=file:///etc/passwd&session=abcdefghijklmnopqrstuv",
            "barwatch://join?server=https://x.org&session=../../x",
            "barwatch://join?server=https://x.org",
            "barwatch://run?server=https://x.org&session=abcdefghijklmnopqrstuv",
            "barwatch://join?server=https://user:pw@x.org&session=abcdefghijklmnopqrstuv",
        ] {
            assert!(Link::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_keep_a_proxy_prefix() {
        let server = parse_server("https://example.org/barwatch").unwrap();
        assert_eq!(app_url(&server, "sessions/abc").unwrap().as_str(), "https://example.org/barwatch/app/v1/sessions/abc");
        assert_eq!(invite_url(&server, "abc").unwrap().as_str(), "https://example.org/barwatch/app/join/abc");
    }

    #[test]
    fn control_wire_format() {
        let c: Control = serde_json::from_str(r#"{"action":"skip","seconds":840,"relative":false}"#).unwrap();
        assert_eq!(c, Control::Skip { seconds: 840, relative: false });
        assert_eq!(serde_json::to_string(&Control::Pause { paused: true }).unwrap(), r#"{"action":"pause","paused":true}"#);
    }
}
