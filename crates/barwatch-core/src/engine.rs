//! Finding and installing Recoil engine builds.
//!
//! Engines live in `<root>/<dir>` using the BAR launcher's directory names
//! (`recoil_2026.07.04`, `rel2501.2025.01.6`, `105.1.1-2590-gb9462a0 bar`), while
//! replay headers say `2026.07.04`, `2025.01.6` or `105.1.1-2590-gb9462a0 BAR105`.
//! Using the launcher's names means an engine installed by either side is found by
//! the other.

use crate::names;
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const RELEASES: &str = "https://github.com/beyond-all-reason/RecoilEngine/releases/download";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }
}

/// The executables of an engine build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    /// The game client.
    Spring,
    Headless,
    Dedicated,
    PrDownloader,
}

impl Binary {
    fn stem(self) -> &'static str {
        match self {
            Binary::Spring => "spring",
            Binary::Headless => "spring-headless",
            Binary::Dedicated => "spring-dedicated",
            Binary::PrDownloader => "pr-downloader",
        }
    }

    /// Path of this executable in `engine_dir`, if it is there.
    pub fn find(self, engine_dir: &Path) -> Option<PathBuf> {
        [format!("{}.exe", self.stem()), self.stem().to_string()].into_iter().map(|n| engine_dir.join(n)).find(|p| p.is_file())
    }
}

fn candidate_dir_names(version: &str) -> Vec<String> {
    if let Some((v, branch)) = version.split_once(' ') {
        let b = branch.to_lowercase();
        let short = b.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
        return vec![format!("{v} {short}"), format!("{v} {b}"), v.to_string()];
    }
    vec![format!("recoil_{version}"), format!("rel2501.{version}"), version.to_string()]
}

/// Download URLs to try for an engine version as written in replay headers.
fn engine_urls(version: &str, os: Os) -> Vec<String> {
    let os = os.tag();
    if let Some((v, branch)) = version.split_once(' ') {
        // Spring 105 BAR branch builds, e.g. "105.1.1-2590-gb9462a0 BAR105".
        return vec![format!("{RELEASES}/spring_bar_%7B{branch}%7D{v}/spring_bar_.{branch}.{v}_{os}-64-minimal-portable.7z")];
    }
    vec![
        format!("{RELEASES}/{version}/recoil_{version}_amd64-{os}.7z"),
        format!("{RELEASES}/{version}/spring_bar_.rel2501.{version}_{os}-64-minimal-portable.7z"),
    ]
}

/// Installed engine directory under `root` for `version` that has `needs`.
pub fn find(root: &Path, version: &str, needs: Binary) -> Option<PathBuf> {
    for n in candidate_dir_names(version) {
        let d = root.join(n);
        if needs.find(&d).is_some() {
            return Some(d);
        }
    }
    // Anything whose name is the version's first token with a known decoration.
    let first = version.split(' ').next().unwrap_or(version).to_lowercase();
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        let hit = name == first
            || name.starts_with(&format!("{first} "))
            || name.ends_with(&format!("_{first}"))
            || name.ends_with(&format!(".{first}"));
        if hit && needs.find(&e.path()).is_some() {
            return Some(e.path());
        }
    }
    None
}

/// Names of the engine directories under `root`.
pub fn installed(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| Binary::Spring.find(&e.path()).is_some() || Binary::Dedicated.find(&e.path()).is_some())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Progress of a download: bytes so far and, when the server says, the total.
pub type Progress<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

/// Returns the engine directory for `version`, downloading and unpacking the
/// official release into `root` when it is not there. `needs` is the executable
/// the caller is going to run.
pub async fn ensure(
    http: &reqwest::Client,
    root: &Path,
    version: &str,
    os: Os,
    needs: Binary,
    progress: Progress<'_>,
) -> Result<PathBuf> {
    ensure!(names::valid_engine_version(version), "refusing engine version {version:?}");
    if let Some(d) = find(root, version, needs) {
        return Ok(d);
    }
    let dest = root.join(&candidate_dir_names(version)[0]);
    tokio::fs::create_dir_all(root).await.with_context(|| format!("creating {}", root.display()))?;
    // Unpack beside the destination and rename, so a half-extracted engine is never
    // mistaken for an installed one.
    let stage = root.join(format!(".barwatch-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&stage).await;
    tokio::fs::create_dir_all(&stage).await?;
    let result = install(http, &stage, &dest, version, os, needs, progress).await;
    let _ = tokio::fs::remove_dir_all(&stage).await;
    result?;
    Ok(dest)
}

async fn install(
    http: &reqwest::Client,
    stage: &Path,
    dest: &Path,
    version: &str,
    os: Os,
    needs: Binary,
    progress: Progress<'_>,
) -> Result<()> {
    let archive = stage.join("engine.7z");
    let mut errors = Vec::new();
    let mut got = false;
    for url in engine_urls(version, os) {
        match download(http, &url, &archive, progress).await {
            Ok(()) => {
                got = true;
                break;
            }
            Err(e) => errors.push(format!("{url}: {e:#}")),
        }
    }
    if !got {
        bail!("could not download engine {version}:\n  {}", errors.join("\n  "));
    }
    let unpacked = stage.join("engine");
    let (a, u) = (archive.clone(), unpacked.clone());
    tokio::task::spawn_blocking(move || extract_7z(&a, &u))
        .await?
        .with_context(|| format!("extracting {}", archive.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for bin in [Binary::Spring, Binary::Headless, Binary::Dedicated, Binary::PrDownloader] {
            if let Some(p) = bin.find(&unpacked) {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    ensure!(needs.find(&unpacked).is_some(), "the engine {version} archive has no {}", needs.stem());
    if dest.exists() {
        // Present but without the executable we need: an incomplete directory.
        tokio::fs::remove_dir_all(dest).await.with_context(|| format!("replacing {}", dest.display()))?;
    }
    tokio::fs::rename(&unpacked, dest).await.with_context(|| format!("installing to {}", dest.display()))?;
    Ok(())
}

/// Downloads `url` to `dest`, reporting progress.
pub async fn download(http: &reqwest::Client, url: &str, dest: &Path, progress: Progress<'_>) -> Result<()> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("creating {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    progress(0, total);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.flush().await?;
    Ok(())
}

/// Pure-Rust extraction first; falls back to a system `7z` if that fails.
fn extract_7z(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let builtin = match sevenz_rust2::decompress_file(archive, dest) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest)?;
    let out = std::process::Command::new("7z")
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", dest.display()))
        .arg(archive)
        .output()
        .with_context(|| format!("built-in extraction failed ({builtin}) and no 7z command is available"))?;
    if !out.status.success() {
        bail!("built-in extraction failed ({builtin}); 7z failed too: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_directory_names() {
        assert_eq!(candidate_dir_names("2026.07.04")[0], "recoil_2026.07.04");
        assert_eq!(candidate_dir_names("105.1.1-2590-gb9462a0 BAR105")[0], "105.1.1-2590-gb9462a0 bar");
    }

    #[test]
    fn release_urls() {
        assert_eq!(
            engine_urls("2026.07.04", Os::Windows)[0],
            "https://github.com/beyond-all-reason/RecoilEngine/releases/download/2026.07.04/recoil_2026.07.04_amd64-windows.7z"
        );
        assert!(engine_urls("105.1.1-2590-gb9462a0 BAR105", Os::Linux)[0].ends_with(
            "spring_bar_%7BBAR105%7D105.1.1-2590-gb9462a0/spring_bar_.BAR105.105.1.1-2590-gb9462a0_linux-64-minimal-portable.7z"
        ));
    }

    #[test]
    fn finds_engines_under_any_known_name() {
        let root = std::env::temp_dir().join(format!("barwatch-engine-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, bin) in [("rel2501.2025.01.6", "spring-dedicated"), ("recoil_2026.07.04", "spring.exe")] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(bin), b"").unwrap();
        }
        assert_eq!(find(&root, "2025.01.6", Binary::Dedicated), Some(root.join("rel2501.2025.01.6")));
        assert_eq!(find(&root, "2025.01.6", Binary::Spring), None);
        assert_eq!(find(&root, "2026.07.04", Binary::Spring), Some(root.join("recoil_2026.07.04")));
        assert_eq!(find(&root, "2099.01.01", Binary::Spring), None);
        assert_eq!(installed(&root), ["recoil_2026.07.04", "rel2501.2025.01.6"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
