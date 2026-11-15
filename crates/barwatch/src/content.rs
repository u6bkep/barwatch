//! Fetching a game version and a map through the engine's own `pr-downloader`, into
//! the data directory BAR uses.
//!
//! A game version comes through rapid, which fetches only the files the pool lacks. A
//! map is one file, and `pr-downloader --download-map` has no "already there" check
//! of its own (BAR's launcher asks the engine's archive list before calling it): run
//! on a map that is installed, it downloads the whole file again and replaces it
//! (measured 2026-11: 84 MB on every call). So the map is looked up first, the same
//! way `pr-downloader` does, and skipped when the file it names is already there.

use anyhow::{bail, ensure, Context, Result};
use barwatch_core::engine::Binary;
use barwatch_core::names;
use std::path::Path;
use std::time::Duration;

pub async fn ensure_game(engine_dir: &Path, data_root: &Path, game: &str) -> Result<()> {
    run(engine_dir, data_root, "--download-game", game, Duration::from_secs(1800)).await
}

pub async fn ensure_map(http: &reqwest::Client, engine_dir: &Path, data_root: &Path, map: &str) -> Result<()> {
    ensure!(names::valid_content_name(map), "refusing to download {map:?}");
    match map_file(http, map).await {
        Ok(file) if file.is_installed(data_root) => return Ok(()),
        Ok(_) => {}
        // The lookup is only a shortcut: `pr-downloader` asks again, and reports
        // properly if the map cannot be found.
        Err(_) => {}
    }
    run(engine_dir, data_root, "--download-map", map, Duration::from_secs(900)).await
}

/// What BAR's file index says a map's archive is.
#[derive(Debug, PartialEq, Eq)]
struct MapFile {
    name: String,
    size: u64,
}

impl MapFile {
    /// Reads the index's answer: a list whose entry for this map names the file.
    fn from_index(reply: &serde_json::Value, map: &str) -> Option<MapFile> {
        let entry = reply
            .as_array()?
            .iter()
            .find(|e| e["category"] == "map" && e["springname"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(map)))?;
        let name = entry["filename"].as_str()?;
        // A plain archive name, never a path: it is joined onto the maps folder.
        let plain = !name.is_empty()
            && name.len() <= 200
            && !name.starts_with('.')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '~'));
        let size = entry["size"].as_u64()?;
        (plain && size > 0).then(|| MapFile { name: name.to_owned(), size })
    }

    /// The archive is in the maps folder, whole. Size is the test: `pr-downloader`
    /// writes under another name and renames, so a part-written file is never seen
    /// under this one.
    fn is_installed(&self, data_root: &Path) -> bool {
        std::fs::metadata(data_root.join("maps").join(&self.name)).is_ok_and(|m| m.is_file() && m.len() == self.size)
    }
}

async fn map_file(http: &reqwest::Client, map: &str) -> Result<MapFile> {
    let mut url = url::Url::parse(SEARCH_URL).context("search address")?;
    url.query_pairs_mut().append_pair("category", "map").append_pair("springname", map);
    let resp = http.get(url).timeout(Duration::from_secs(10)).send().await?.error_for_status()?;
    let body = resp.bytes().await?;
    ensure!(body.len() <= 1 << 20, "the index answered with {} bytes", body.len());
    let reply: serde_json::Value = serde_json::from_slice(&body).context("not JSON")?;
    MapFile::from_index(&reply, map).context("the index does not list it")
}

/// BAR's file index, which `pr-downloader` is pointed at as well.
const SEARCH_URL: &str = "https://files-cdn.beyondallreason.dev/find";

async fn run(engine_dir: &Path, data_root: &Path, flag: &str, name: &str, timeout: Duration) -> Result<()> {
    // `name` comes from the server; it must not be taken for an option.
    ensure!(names::valid_content_name(name), "refusing to download {name:?}");
    let prd = Binary::PrDownloader.find(engine_dir).with_context(|| format!("no pr-downloader in {}", engine_dir.display()))?;
    let mut cmd = tokio::process::Command::new(&prd);
    cmd.current_dir(engine_dir)
        .env("PRD_RAPID_USE_STREAMER", "false")
        .env("PRD_HTTP_SEARCH_URL", SEARCH_URL)
        .env("PRD_RAPID_REPO_MASTER", "https://repos-cdn.beyondallreason.dev/repos.gz")
        .arg("--filesystem-writepath")
        .arg(data_root)
        .arg(flag)
        .arg(name)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .with_context(|| format!("downloading {name} took longer than {} minutes", timeout.as_secs() / 60))?
        .context("running pr-downloader")?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let tail: Vec<&str> = stdout.lines().rev().take(4).collect();
        bail!("could not download {name}: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn index() -> serde_json::Value {
        json!([{"filename": "scylla_and_charybdis_1.1.sd7", "springname": "Scylla and Charybdis 1.1",
                "md5": "392cb42bbfe16e9eeec28dd0fadb7ccb", "category": "map", "path": "maps", "size": 11,
                "mirrors": ["https://example.invalid/file"]}])
    }

    #[test]
    fn the_index_names_the_map_file() {
        let f = MapFile::from_index(&index(), "Scylla and Charybdis 1.1").unwrap();
        assert_eq!(f, MapFile { name: "scylla_and_charybdis_1.1.sd7".into(), size: 11 });
        assert_eq!(MapFile::from_index(&index(), "Another Map 2"), None);
        assert_eq!(MapFile::from_index(&json!({}), "Scylla and Charybdis 1.1"), None);
    }

    #[test]
    fn a_file_name_that_is_a_path_is_not_taken() {
        for bad in ["../x.sd7", "a/b.sd7", "a\\b.sd7", ".hidden", "", "x y.sd7"] {
            let mut reply = index();
            reply[0]["filename"] = bad.into();
            assert_eq!(MapFile::from_index(&reply, "Scylla and Charybdis 1.1"), None, "{bad:?}");
        }
    }

    #[test]
    fn a_map_is_installed_when_its_file_is_there_whole() {
        let root = std::env::temp_dir().join(format!("barwatch-content-test-{}", std::process::id()));
        let maps = root.join("maps");
        std::fs::create_dir_all(&maps).unwrap();
        let f = MapFile::from_index(&index(), "Scylla and Charybdis 1.1").unwrap();
        assert!(!f.is_installed(&root));
        std::fs::write(maps.join(&f.name), b"short").unwrap();
        assert!(!f.is_installed(&root));
        std::fs::write(maps.join(&f.name), b"eleven byte").unwrap();
        assert!(f.is_installed(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
