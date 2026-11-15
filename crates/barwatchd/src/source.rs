//! Fetching a replay from BAR's public replay archive by its id or its page link.

use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Duration;

const REPLAY_API: &str = "https://api.bar-rts.com/replays";
/// The object store BAR's replay site links its downloads to.
const BUCKET: &str = "https://storage.uk.cloud.ovh.net/v1/AUTH_10286efc0d334efd917d476d7183232e/BAR";

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
pub async fn fetch(http: &reqwest::Client, input: &str, max_bytes: usize) -> Result<(String, Vec<u8>)> {
    let id = replay_id(input).context("that is not a BAR replay link or id")?;
    let meta: ReplayMeta = http
        .get(format!("{REPLAY_API}/{id}"))
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

    let mut url = url::Url::parse(BUCKET)?;
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
