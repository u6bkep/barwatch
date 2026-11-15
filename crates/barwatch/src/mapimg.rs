//! Map pictures for the window: fetched once from BAR's map API, kept on disk, and
//! handed over as plain RGBA.

use anyhow::{ensure, Context, Result};
use barwatch_core::maps::{self, MapIndex, Texture};
use std::path::Path;
use std::time::Duration;

/// Decoded picture, RGBA8.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Longest side kept in memory; the window never shows a map larger than this.
const MAX_SIDE: u32 = 512;

/// Loads the picture of the map with in-game name `map`.
pub async fn load(http: &reqwest::Client, index: &MapIndex, cache: &Path, map: &str) -> Result<Picture> {
    let file = index.file_name(map);
    let safe: String =
        file.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let path = cache.join("maps").join(format!("{safe}.jpg"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            let url = maps::texture_url(&file, Texture::Medium)?;
            let resp = http.get(url).timeout(Duration::from_secs(30)).send().await?.error_for_status()?;
            let bytes = resp.bytes().await?.to_vec();
            ensure!(bytes.starts_with(&[0xff, 0xd8, 0xff]), "the map API did not send a JPEG");
            if let Some(dir) = path.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(&path, &bytes).await;
            bytes
        }
    };
    tokio::task::spawn_blocking(move || decode(&bytes)).await?
}

fn decode(bytes: &[u8]) -> Result<Picture> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).context("decoding the map picture")?;
    let img = if img.width().max(img.height()) > MAX_SIDE {
        img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Ok(Picture { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}
