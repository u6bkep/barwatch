//! Fetching a game version and a map through the engine's own `pr-downloader`, into
//! the data directory BAR uses. Both are quick no-ops when the content is present.

use anyhow::{bail, ensure, Context, Result};
use barwatch_core::engine::Binary;
use barwatch_core::names;
use std::path::Path;
use std::time::Duration;

pub async fn ensure_game(engine_dir: &Path, data_root: &Path, game: &str) -> Result<()> {
    run(engine_dir, data_root, "--download-game", game, Duration::from_secs(1800)).await
}

pub async fn ensure_map(engine_dir: &Path, data_root: &Path, map: &str) -> Result<()> {
    run(engine_dir, data_root, "--download-map", map, Duration::from_secs(900)).await
}

async fn run(engine_dir: &Path, data_root: &Path, flag: &str, name: &str, timeout: Duration) -> Result<()> {
    // `name` comes from the server; it must not be taken for an option.
    ensure!(names::valid_content_name(name), "refusing to download {name:?}");
    let prd = Binary::PrDownloader
        .find(engine_dir)
        .with_context(|| format!("no pr-downloader in {}", engine_dir.display()))?;
    let mut cmd = tokio::process::Command::new(&prd);
    cmd.current_dir(engine_dir)
        .env("PRD_RAPID_USE_STREAMER", "false")
        .env("PRD_HTTP_SEARCH_URL", "https://files-cdn.beyondallreason.dev/find")
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
