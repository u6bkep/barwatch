//! What the app remembers between runs: the viewer's name, where BAR lives, and the
//! servers it is paired with.

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
}

/// A server this app may create watch parties on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairedServer {
    pub url: String,
    pub token: String,
}

fn config_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("barwatch").join("config.json"))
}

impl Config {
    pub fn load() -> Config {
        config_file()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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
}

/// Where downloaded pictures and lists are kept.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("barwatch")
}
