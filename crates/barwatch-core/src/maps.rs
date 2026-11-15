//! Map pictures. BAR's map API serves a rendered texture per map archive, keyed by
//! the archive's base name (`comet_catcher_remake_1.8`), while replays name maps by
//! their in-game name (`Comet Catcher Remake 1.8`). The API's map list links the two.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

const MAPS_URL: &str = "https://api.bar-rts.com/maps";
const LIST_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Deserialize)]
struct MapList {
    data: Vec<MapEntry>,
}

#[derive(Deserialize)]
struct MapEntry {
    #[serde(rename = "scriptName")]
    script_name: String,
    #[serde(rename = "fileName")]
    file_name: String,
}

/// In-game map name → archive base name.
#[derive(Debug, Default, Clone)]
pub struct MapIndex {
    by_name: HashMap<String, String>,
}

impl MapIndex {
    /// Loads the list from `cache_file` if it is recent, otherwise from the API
    /// (refreshing the file). A stale file is better than nothing when offline.
    pub async fn load(http: &reqwest::Client, cache_file: &Path) -> MapIndex {
        let fresh = std::fs::metadata(cache_file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age < LIST_TTL);
        if !fresh {
            if let Ok(text) = Self::fetch(http).await {
                if let Ok(index) = Self::parse(&text) {
                    if let Some(dir) = cache_file.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(cache_file, &text);
                    return index;
                }
            }
        }
        std::fs::read_to_string(cache_file)
            .ok()
            .and_then(|t| Self::parse(&t).ok())
            .unwrap_or_default()
    }

    async fn fetch(http: &reqwest::Client) -> Result<String> {
        let resp = http
            .get(format!("{MAPS_URL}?limit=5000"))
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.text().await?)
    }

    pub fn parse(json: &str) -> Result<MapIndex> {
        let list: MapList = serde_json::from_str(json).context("parsing the map list")?;
        Ok(MapIndex {
            by_name: list.data.into_iter().map(|m| (m.script_name.to_lowercase(), m.file_name)).collect(),
        })
    }

    /// Archive base name for an in-game map name. Unknown maps get the usual naming
    /// convention as a guess; the picture request simply fails if it is wrong.
    pub fn file_name(&self, map: &str) -> String {
        self.by_name
            .get(&map.to_lowercase())
            .cloned()
            .unwrap_or_else(|| map.to_lowercase().replace(' ', "_"))
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Picture sizes the API offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Texture {
    Thumb,
    Medium,
}

/// URL of a map's rendered texture (JPEG).
pub fn texture_url(map_file: &str, size: Texture) -> Result<url::Url> {
    let mut url = url::Url::parse(MAPS_URL)?;
    let leaf = match size {
        Texture::Thumb => "texture-thumb.jpg",
        Texture::Medium => "texture-mq.jpg",
    };
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("bad base url"))?.push(map_file).push(leaf);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_listed_maps_and_guesses_the_rest() {
        let index = MapIndex::parse(
            r#"{"totalResults":1,"data":[{"id":1,"scriptName":"Supreme Isthmus v2.1","fileName":"supreme_isthmus_v2.1","width":24}]}"#,
        )
        .unwrap();
        assert_eq!(index.file_name("supreme isthmus V2.1"), "supreme_isthmus_v2.1");
        assert_eq!(index.file_name("Comet Catcher Remake 1.8"), "comet_catcher_remake_1.8");
    }

    #[test]
    fn texture_urls_escape_the_name() {
        assert_eq!(
            texture_url("a b/c", Texture::Medium).unwrap().as_str(),
            "https://api.bar-rts.com/maps/a%20b%2Fc/texture-mq.jpg"
        );
    }
}
