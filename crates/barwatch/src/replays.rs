//! The replays in BAR's `demos` folder.

use barwatch_core::api::ReplayInfo;
use barwatch_core::demo::Demo;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LocalReplay {
    pub path: PathBuf,
    pub file_name: String,
    /// `None` when the file could not be read as a replay.
    pub info: Option<ReplayInfo>,
}

/// Replay files in `dir`, newest first. BAR names them by start time, so the name
/// orders them; empty files (a game that crashed before recording) are left out.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sdfz"))
        .filter(|e| e.metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
        .map(|e| e.path())
        .collect();
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files
}

/// Reads a replay's header. Cheap: only the head of the file is decompressed.
pub fn describe(path: &Path) -> LocalReplay {
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let info = Demo::read(path).ok().and_then(|demo| ReplayInfo::from_demo(&demo, &file_name, None));
    LocalReplay { path: path.to_path_buf(), file_name, info }
}

impl LocalReplay {
    /// Lowercased text the list filter searches: map, players, file name.
    pub fn haystack(&self) -> String {
        let mut text = self.file_name.to_lowercase();
        if let Some(info) = &self.info {
            text.push(' ');
            text.push_str(&info.map.to_lowercase());
            for name in info.teams.iter().flatten().chain(&info.spectators) {
                text.push(' ');
                text.push_str(&name.to_lowercase());
            }
        }
        text
    }
}
