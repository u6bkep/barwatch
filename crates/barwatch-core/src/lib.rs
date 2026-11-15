//! Shared pieces of barwatch: replay parsing, start scripts, engine installs and the
//! types the daemon and the desktop app exchange.

pub mod api;
pub mod demo;
pub mod engine;
pub mod maps;
pub mod names;
pub mod script;

/// `User-Agent` for every outgoing request: names the tool, nothing about the user.
pub const USER_AGENT: &str = concat!("barwatch/", env!("CARGO_PKG_VERSION"));

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?)
}
