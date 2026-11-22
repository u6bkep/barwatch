//! What the app remembers between runs: the viewer's name, where BAR lives, the
//! servers it is paired with, and the rooms it has been in.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Name shown to other viewers. Empty: use the BAR lobby name or the login name.
    pub name: String,
    /// BAR's data directory. `None`: detect it.
    pub data_dir: Option<PathBuf>,
    pub servers: Vec<PairedServer>,
    /// Rooms this app has been in, the latest first, so that a friend gets back in
    /// without the link. A room id is its link: this file is the user's alone.
    pub rooms: Vec<RecentRoom>,
}

/// A server this app may make rooms on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairedServer {
    pub url: String,
    pub token: String,
}

/// A room this app has been in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecentRoom {
    pub server: String,
    pub room: String,
    /// The room's name when it was last seen.
    pub name: String,
}

/// How many rooms are remembered.
const RECENT_ROOMS: usize = 20;

fn config_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("barwatch").join("config.json"))
}

impl Config {
    pub fn load() -> Config {
        config_file().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file().context("this system has no configuration directory")?;
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            // The file holds pairing tokens.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Remembers a pairing, replacing an older one with the same server.
    pub fn pair(&mut self, url: &str, token: &str) {
        self.servers.retain(|s| s.url != url);
        self.servers.push(PairedServer { url: url.to_owned(), token: token.to_owned() });
    }

    /// Remembers being in a room, first in the list. `false` if that changes nothing.
    pub fn remember_room(&mut self, server: &str, room: &str, name: &str) -> bool {
        let entry = RecentRoom { server: server.to_owned(), room: room.to_owned(), name: name.to_owned() };
        if self.rooms.first() == Some(&entry) {
            return false;
        }
        self.rooms.retain(|r| !(r.server == server && r.room == room));
        self.rooms.insert(0, entry);
        self.rooms.truncate(RECENT_ROOMS);
        true
    }

    /// Forgets a room (it is gone, or the user asked). `false` if it was not remembered.
    pub fn forget_room(&mut self, server: &str, room: &str) -> bool {
        let before = self.rooms.len();
        self.rooms.retain(|r| !(r.server == server && r.room == room));
        self.rooms.len() != before
    }
}

/// Where downloaded pictures and lists are kept.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("barwatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms_are_remembered_latest_first_once_each() {
        let mut cfg = Config::default();
        assert!(cfg.remember_room("https://a/", "r1", "Friday"));
        assert!(cfg.remember_room("https://a/", "r2", "Duels"));
        assert!(!cfg.remember_room("https://a/", "r2", "Duels"), "nothing new");
        assert!(cfg.remember_room("https://a/", "r1", "Friday night"));
        let names: Vec<&str> = cfg.rooms.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Friday night", "Duels"]);
        assert!(cfg.forget_room("https://a/", "r2"));
        assert!(!cfg.forget_room("https://a/", "r2"));
        for i in 0..30 {
            cfg.remember_room("https://a/", &format!("x{i}"), "x");
        }
        assert_eq!(cfg.rooms.len(), RECENT_ROOMS);
        assert_eq!(cfg.rooms[0].room, "x29");
        // An older config file, without rooms, still reads.
        let old: Config = serde_json::from_str(r#"{"name":"a","servers":[]}"#).unwrap();
        assert!(old.rooms.is_empty());
    }
}
