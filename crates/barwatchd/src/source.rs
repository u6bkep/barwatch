//! Fetching a replay from BAR's public replay archive by its id or its page link.

use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Duration;

const REPLAY_API: &str = "https://api.bar-rts.com/replays";
/// The object store BAR's replay site links its downloads to.
const BUCKET: &str = "https://storage.uk.cloud.ovh.net/v1/AUTH_10286efc0d334efd917d476d7183232e/BAR";

/// Where BAR's replay archive is: its API (`<api>/<id>` describes a replay) and the
/// store its files are in (`<bucket>/demos/<file name>`). Only tests change it.
#[derive(Clone, Debug)]
pub struct Archive {
    pub api: String,
    pub bucket: String,
}

impl Default for Archive {
    fn default() -> Archive {
        Archive { api: REPLAY_API.into(), bucket: BUCKET.into() }
    }
}

#[derive(Deserialize)]
struct ReplayMeta {
    #[serde(rename = "fileName")]
    file_name: String,
}

/// The replay id inside `input`: a bare id, or any link that contains one
/// (`…/replays?gameId=<id>`, `…/replays/<id>`).
pub fn replay_id(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let len = bytes[start..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
        if len == 32 {
            return Some(&input[start..start + 32]);
        }
        start += len.max(1);
    }
    None
}

/// Downloads the replay; returns its file name and contents.
pub async fn fetch(http: &reqwest::Client, archive: &Archive, input: &str, max_bytes: usize) -> Result<(String, Vec<u8>)> {
    let id = replay_id(input).context("that is not a BAR replay link or id")?;
    let meta: ReplayMeta = http
        .get(format!("{}/{id}", archive.api))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("asking BAR's replay archive")?
        .error_for_status()
        .context("BAR's replay archive does not know that replay")?
        .json()
        .await
        .context("reading the replay's description")?;
    ensure!(!meta.file_name.contains(['/', '\\']), "replay has an odd file name");

    let mut url = url::Url::parse(&archive.bucket)?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("bad bucket url"))?.push("demos").push(&meta.file_name);
    let resp = http.get(url).send().await?.error_for_status().context("downloading the replay file")?;
    if resp.content_length().is_some_and(|n| n > max_bytes as u64) {
        bail!("replay is larger than the {} MiB limit", max_bytes >> 20);
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        ensure!(body.len() <= max_bytes, "replay is larger than the {} MiB limit", max_bytes >> 20);
    }
    Ok((meta.file_name, body))
}

/// Gets the replay through the stats service's mirror (DESIGN §4.2): `id` is a
/// replay id from [`replay_id`]. The file must carry that game id in its header;
/// the caller validates the rest as it does any replay.
pub async fn through_service(client: &crate::stats_client::StatsClient, id: &str, max_bytes: usize) -> Result<(String, Vec<u8>)> {
    let id = id.to_ascii_lowercase();
    let (name, body) = client.replay(&id, max_bytes).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let found = barwatch_core::demo::game_id_from_gz(&body);
    ensure!(found.as_deref() == Some(id.as_str()), "the stats service sent a replay of another game ({found:?})");
    // Only a plain file name is kept; anything else becomes the id.
    let name = name
        .filter(|n| n.len() <= 200 && n.ends_with(".sdfz") && !n.contains(['/', '\\']) && !n.chars().any(char::is_control))
        .unwrap_or_else(|| format!("{id}.sdfz"));
    Ok((name, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_id_in_links() {
        let id = "cc40f96a2793765a13a582da36642840";
        assert_eq!(replay_id(id), Some(id));
        assert_eq!(replay_id(&format!("https://www.beyondallreason.info/replays?gameId={id}")), Some(id));
        assert_eq!(replay_id(&format!("https://bar-rts.com/replays/{id}/")), Some(id));
        assert_eq!(replay_id("https://example.org/replays"), None);
        assert_eq!(replay_id(&format!("{id}0")), None);
    }
}
