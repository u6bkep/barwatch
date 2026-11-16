//! Client of the stats service (bar-replays' HTTP API v1, its DESIGN.md §9.5 and
//! §9.8): registering games, their status, their replay file and their record
//! stream.
//!
//! Nothing the service sends is trusted: every body is read with a size limit,
//! every call has a time limit, status fields are checked and its words cleaned
//! before they reach a viewer. The token goes only into the `Authorization` header
//! (marked sensitive) and appears in no log line, error or `Debug` output.

use futures_util::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// Largest JSON answer read (a status is a few hundred bytes).
const MAX_JSON: usize = 64 << 10;
/// Most of an error answer read for its message.
const MAX_ERROR_BODY: usize = 4 << 10;
/// Longest piece of the service's own words passed on to viewers.
const MAX_WORDS: usize = 200;

/// Registering a game can include the service fetching the replay from BAR's store.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(120);
/// A replay through the mirror; after this the daemon downloads it itself.
const REPLAY_TIMEOUT: Duration = Duration::from_secs(30);
/// Added to a status long-poll's own wait.
const STATUS_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SvcError {
    /// No answer: refused, timed out, broken off, or a server error (5xx).
    Unreachable(String),
    /// 404: the service does not know the game (or the call).
    NotFound(String),
    /// 401 or 403: the service refuses this daemon's token.
    Unauthorized,
    /// Any other 4xx: the service will not do it. Carries its cleaned words.
    Refused(String),
    /// An answer that is not what the API documents.
    Malformed(String),
}

impl std::fmt::Display for SvcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SvcError::Unreachable(m) => write!(f, "stats service unreachable: {m}"),
            SvcError::NotFound(m) => write!(f, "stats service: not found: {m}"),
            SvcError::Unauthorized => write!(f, "stats service refuses this server's token"),
            SvcError::Refused(m) => write!(f, "stats service refused: {m}"),
            SvcError::Malformed(m) => write!(f, "stats service sent a malformed answer: {m}"),
        }
    }
}

/// A game's status at the service (`GET /api/v1/games/{id}`), checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameStatus {
    pub game_id: String,
    /// The job state of bar-replays §3.4 plus `simulating`, as sent (checked to be a
    /// short lowercase word).
    pub state: String,
    pub frame: u32,
    pub of_frames: Option<u32>,
    /// The service's error, cleaned.
    pub error: Option<String>,
    pub rev: u64,
}

/// What a status means for the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcPhase {
    /// Known, not started (`selected`, `fetched`, `provisioned`, or a state this
    /// daemon does not know).
    Queued,
    Simulating,
    /// The output is whole (`simulated`, `ingested`).
    Done,
    /// `failed` or `skipped`.
    Failed,
}

impl GameStatus {
    pub fn phase(&self) -> SvcPhase {
        match self.state.as_str() {
            "simulating" => SvcPhase::Simulating,
            "simulated" | "ingested" => SvcPhase::Done,
            "failed" | "skipped" => SvcPhase::Failed,
            _ => SvcPhase::Queued,
        }
    }
}

#[derive(Deserialize)]
struct RawStatus {
    game_id: Option<String>,
    state: Option<String>,
    #[serde(default)]
    progress: Option<RawProgress>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    rev: Option<u64>,
}

#[derive(Deserialize)]
struct RawProgress {
    frame: Option<serde_json::Value>,
    of_frames: Option<serde_json::Value>,
}

/// A game id as the service and BAR use them: 32 hex digits, lowercased.
pub fn parse_game_id(id: &str) -> Option<String> {
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

/// The service's words, made fit to show: no control characters or invisible
/// direction and width marks (which can make text read other than it is), bounded
/// length.
pub fn clean_words(text: &str) -> String {
    let odd = |c: char| {
        c.is_control()
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{feff}')
    };
    let mut s: String = text.chars().map(|c| if odd(c) { ' ' } else { c }).take(MAX_WORDS).collect();
    if text.chars().count() > MAX_WORDS {
        s.push('…');
    }
    s.trim().to_owned()
}

fn parse_status(body: &[u8]) -> Result<GameStatus, SvcError> {
    // serde's words can quote the service's text: cleaned and bounded like any of it.
    let raw: RawStatus = serde_json::from_slice(body).map_err(|e| SvcError::Malformed(clean_words(&format!("status: {e}"))))?;
    let game_id =
        raw.game_id.as_deref().and_then(parse_game_id).ok_or_else(|| SvcError::Malformed("status has no game id".into()))?;
    let state = raw
        .state
        .filter(|s| !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        .ok_or_else(|| SvcError::Malformed("status has no state".into()))?;
    let frame_of =
        |v: &Option<serde_json::Value>| v.as_ref().and_then(serde_json::Value::as_u64).map(|f| f.min(u32::MAX as u64) as u32);
    let (frame, of_frames) = match &raw.progress {
        Some(p) => (frame_of(&p.frame).unwrap_or(0), frame_of(&p.of_frames)),
        None => (0, None),
    };
    let error = match raw.error {
        Some(serde_json::Value::String(e)) if !e.trim().is_empty() => Some(clean_words(&e)),
        _ => None,
    };
    Ok(GameStatus { game_id, state, frame, of_frames, error, rev: raw.rev.unwrap_or(0) })
}

/// The configured stats service. Cheap to clone.
#[derive(Clone)]
pub struct StatsClient {
    base: Url,
    auth: HeaderValue,
    http: reqwest::Client,
}

impl std::fmt::Debug for StatsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsClient").field("base", &self.base.as_str()).field("token", &"<hidden>").finish()
    }
}

impl StatsClient {
    /// `url` is the service's base address (`http://bar-replays:8770/`); `token` a
    /// `client` token of the service.
    pub fn new(url: &str, token: &str) -> anyhow::Result<StatsClient> {
        let base = barwatch_core::api::parse_server(url)?;
        anyhow::ensure!(!token.trim().is_empty(), "the stats service token is empty");
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| anyhow::anyhow!("the stats service token holds characters a header cannot"))?;
        auth.set_sensitive(true);
        let http = reqwest::Client::builder()
            .user_agent(barwatch_core::USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(60))
            // The token is for this address only: not followed elsewhere, not
            // handed to a proxy from the environment.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(StatsClient { base, auth, http })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Url {
        let mut url = self.base.join(path).expect("joining a relative API path");
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        url
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        self.http.request(method, url).header(AUTHORIZATION, self.auth.clone())
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, SvcError> {
        let resp = req.send().await.map_err(|e| SvcError::Unreachable(error_words(e)))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let words = error_message(resp).await;
        Err(match status {
            StatusCode::NOT_FOUND => SvcError::NotFound(words),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => SvcError::Unauthorized,
            s if s.is_client_error() => SvcError::Refused(words),
            s => SvcError::Unreachable(format!("{s}: {words}")),
        })
    }

    /// `POST /api/v1/games {game_id, priority: interactive}`.
    pub async fn submit_game(&self, game_id: &str) -> Result<GameStatus, SvcError> {
        let body = serde_json::json!({ "game_id": game_id, "priority": "interactive" });
        let req = self
            .request(Method::POST, self.url("api/v1/games", &[]))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .timeout(SUBMIT_TIMEOUT);
        let resp = self.send(req).await?;
        parse_status(&read_limited(resp, MAX_JSON).await?)
    }

    /// `PUT /api/v1/replays?priority=interactive` with the replay file.
    pub async fn upload(&self, replay: Vec<u8>) -> Result<GameStatus, SvcError> {
        // A large replay over a slow link: at least 1 MiB/s on top of the usual wait.
        let timeout = SUBMIT_TIMEOUT + Duration::from_secs((replay.len() >> 20) as u64);
        let req = self
            .request(Method::PUT, self.url("api/v1/replays", &[("priority", "interactive".into())]))
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(replay)
            .timeout(timeout);
        let resp = self.send(req).await?;
        parse_status(&read_limited(resp, MAX_JSON).await?)
    }

    /// `GET /api/v1/games/{id}`; with `rev`, waits up to `wait` for it to differ.
    pub async fn status(&self, game_id: &str, rev: Option<u64>, wait: Duration) -> Result<GameStatus, SvcError> {
        let mut query = Vec::new();
        if let Some(rev) = rev {
            query.push(("rev", rev.to_string()));
            query.push(("wait", wait.as_secs().to_string()));
        }
        let req = self.request(Method::GET, self.url(&format!("api/v1/games/{game_id}"), &query)).timeout(wait + STATUS_GRACE);
        let resp = self.send(req).await?;
        parse_status(&read_limited(resp, MAX_JSON).await?)
    }

    /// `GET /api/v1/games/{id}/replay`: the file and the name the service gives it.
    pub async fn replay(&self, game_id: &str, max_bytes: usize) -> Result<(Option<String>, Vec<u8>), SvcError> {
        let req = self.request(Method::GET, self.url(&format!("api/v1/games/{game_id}/replay"), &[])).timeout(REPLAY_TIMEOUT);
        let resp = self.send(req).await?;
        let name = resp.headers().get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()).and_then(disposition_file_name);
        let body = read_limited(resp, max_bytes).await?;
        Ok((name, body))
    }

    /// `GET /api/v1/games/{id}/records?actions=…&from=<from>&follow=1`: the open
    /// response, whose body is read by the caller (with its own limits).
    pub async fn records(&self, game_id: &str, actions: &[&str], from: u64) -> Result<reqwest::Response, SvcError> {
        let query = [("actions", actions.join(",")), ("from", from.to_string()), ("follow", "1".into())];
        let req = self.request(Method::GET, self.url(&format!("api/v1/games/{game_id}/records"), &query));
        self.send(req).await
    }
}

/// An error in words, without the URL (which carries nothing secret, but makes long
/// notes).
fn error_words(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "cannot connect".into()
    } else {
        clean_words(&e.without_url().to_string())
    }
}

/// The `error` of an error answer, cleaned; the status line if there is none.
async fn error_message(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = read_limited(resp, MAX_ERROR_BODY).await.unwrap_or_default();
    #[derive(Deserialize)]
    struct E {
        error: String,
    }
    match serde_json::from_slice::<E>(&body) {
        Ok(e) => clean_words(&e.error),
        Err(_) => status.to_string(),
    }
}

/// The whole body, or an error once it passes `max` bytes.
async fn read_limited(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, SvcError> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(SvcError::Malformed(format!("answer larger than {max} bytes")));
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| SvcError::Unreachable(error_words(e)))?;
        if body.len() + chunk.len() > max {
            return Err(SvcError::Malformed(format!("answer larger than {max} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `attachment; filename="x.sdfz"` → `x.sdfz`. Only a plain quoted or bare name.
fn disposition_file_name(v: &str) -> Option<String> {
    let rest = v.split(';').map(str::trim).find_map(|p| p.strip_prefix("filename="))?;
    let name = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(rest);
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_checked() {
        let ok = parse_status(
            br#"{"game_id":"AE1B9E6A7C9F738BAABDEC84229B888F","state":"simulating","source":"bucket","replay_file_name":"x",
                "progress":{"frame":1200,"of_frames":36000},"error":null,"rev":77}"#,
        )
        .unwrap();
        assert_eq!(ok.game_id, "ae1b9e6a7c9f738baabdec84229b888f");
        assert_eq!((ok.phase(), ok.frame, ok.of_frames, ok.rev), (SvcPhase::Simulating, 1200, Some(36000), 77));
        let failed = parse_status(
            br#"{"game_id":"ae1b9e6a7c9f738baabdec84229b888f","state":"failed","error":"engine\u0007 crashed\nhere"}"#,
        )
        .unwrap();
        assert_eq!(failed.phase(), SvcPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some("engine  crashed here"));
        let odd = parse_status(br#"{"game_id":"ae1b9e6a7c9f738baabdec84229b888f","state":"thinking","progress":{"frame":-5}}"#)
            .unwrap();
        assert_eq!((odd.phase(), odd.frame), (SvcPhase::Queued, 0));
        for bad in [
            &br#"{"state":"simulating"}"#[..],
            br#"{"game_id":"../../etc","state":"simulating"}"#,
            br#"{"game_id":"ae1b9e6a7c9f738baabdec84229b888f","state":"<script>"}"#,
            br#"[1,2]"#,
            b"\xff\xfe",
        ] {
            assert!(parse_status(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn words_are_cleaned_and_bounded() {
        assert_eq!(clean_words("a\x1b[31mb"), "a [31mb");
        assert_eq!(clean_words("crashed \u{202e}gnp.exe"), "crashed  gnp.exe");
        assert_eq!(clean_words(&"x".repeat(500)).chars().count(), MAX_WORDS + 1);
    }

    #[test]
    fn file_names_from_disposition() {
        assert_eq!(disposition_file_name(r#"attachment; filename="2026-11-14_x.sdfz""#).as_deref(), Some("2026-11-14_x.sdfz"));
        assert_eq!(disposition_file_name("attachment; filename=a.sdfz").as_deref(), Some("a.sdfz"));
        assert_eq!(disposition_file_name("attachment"), None);
    }

    #[test]
    fn the_token_is_not_in_debug_output() {
        let c = StatsClient::new("http://127.0.0.1:1/", "s3cret-token").unwrap();
        assert!(!format!("{c:?}").contains("s3cret"));
        assert!(StatsClient::new("http://user:pw@host/", "t").is_err());
        assert!(StatsClient::new("http://host/", " ").is_err());
    }
}
