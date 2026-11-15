//! Locating Beyond All Reason's data directory.
//!
//! barwatch installs engines, game versions and maps into the same directory BAR
//! itself uses, so whatever either has downloaded is there for the other. Known
//! layouts: flatpak (`~/.var/app/info.beyondallreason.bar/data`), the native Linux
//! launcher (`~/.local/state/Beyond All Reason`) and the Windows launcher
//! (`%LOCALAPPDATA%\Programs\Beyond-All-Reason\data`).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    pub root: PathBuf,
    /// No BAR installation was found; this is barwatch's own directory and
    /// everything will be downloaded into it.
    pub own: bool,
}

fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(home) = dirs::home_dir() {
        found.push(home.join(".var/app/info.beyondallreason.bar/data"));
        found.push(home.join(".local/state/Beyond All Reason"));
    }
    if let Some(local) = dirs::data_local_dir() {
        found.push(local.join("Programs").join("Beyond-All-Reason").join("data"));
    }
    found
}

fn looks_like_bar(dir: &Path) -> bool {
    dir.join("engine").is_dir() || dir.join("pool").is_dir()
}

/// The data directory to use: `explicit` if given, else `$BAR_DATA_DIR`, else the
/// first known layout that exists, else a directory of barwatch's own.
pub fn find(explicit: Option<&Path>) -> DataDir {
    if let Some(p) = explicit {
        return DataDir { root: p.to_path_buf(), own: false };
    }
    if let Some(p) = std::env::var_os("BAR_DATA_DIR").filter(|p| !p.is_empty()) {
        return DataDir { root: PathBuf::from(p), own: false };
    }
    if let Some(found) = candidates().into_iter().find(|c| looks_like_bar(c)) {
        return DataDir { root: found, own: false };
    }
    let own = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("barwatch").join("bar");
    DataDir { root: own, own: true }
}

impl DataDir {
    pub fn engines(&self) -> PathBuf {
        self.root.join("engine")
    }

    pub fn demos(&self) -> PathBuf {
        self.root.join("demos")
    }

    /// The name the viewer uses in BAR's lobby, if the lobby has saved one.
    pub fn lobby_name(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.root.join("LuaMenu/Config/IGL_data.lua")).ok()?;
        let rest = &text[text.find("userName")? + "userName".len()..];
        let rest = rest.trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
        let name = &rest[..rest.find('"')?];
        (!name.is_empty()).then(|| name.to_owned())
    }
}

/// The name to show other viewers when the user has not chosen one.
pub fn default_name(data: &DataDir) -> String {
    data.lobby_name()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Spectator".into())
}
