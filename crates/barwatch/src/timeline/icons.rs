//! Pictures and unit names for event markers, read at run time from the game
//! the viewer has installed.
//!
//! BAR's unit pictures and interface images are licensed CC BY-NC-ND or "all
//! rights reserved" (`license_unitpics.txt`, `license_icons.txt`,
//! `luaui/images/license.txt` in the game repository), and its licence forbids
//! use "as a dependency", so barwatch carries none: they come from the game
//! version a replay names, out of BAR's own data directory
//! ([`barwatch_core::rapid`]), or, while developing, from a local checkout of the
//! game. Without a picture a marker falls back to a glyph of its own.
//!
//! What is read from an installed version is kept for the life of the process:
//! a version's files never change (they are addressed by content).

use super::dds;
use super::marks::SKULL;
use anyhow::{Context, Result};
use barwatch_core::rapid::Archive;
use damascene_core::prelude::Image;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Pictures are kept this many pixels across; badges are drawn at about 20.
const KEEP_PX: u32 = 64;
/// Largest picture read from a game (BAR's unit pictures: median 350 KB, largest 4.1 MB).
const MAX_PICTURE_BYTES: u32 = 16 << 20;
/// Largest name table read from a game (BAR's: 80 KB).
const MAX_NAMES_BYTES: u32 = 4 << 20;
const SKULL_FILE: &str = "luaui/images/skull.dds";
const NAMES_FILE: &str = "language/en/units.json";

#[derive(Default, Clone)]
pub struct Icons {
    by_name: HashMap<String, Image>,
}

impl Icons {
    /// Adds a picture under its unit-def name (or [`SKULL`]).
    pub fn insert(&mut self, name: &str, image: Image) {
        self.by_name.insert(name.to_owned(), image);
    }

    pub fn get(&self, name: &str) -> Option<&Image> {
        self.by_name.get(name)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Decodes a JPEG picture, the form a server sends unit pictures in.
    // For the server feed to come.
    #[allow(dead_code)]
    pub fn insert_jpeg(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let rgba = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.to_rgba8();
        self.insert(name, keep(rgba));
        Ok(())
    }

    /// DEVELOPMENT FIXTURE: reads the named pictures from a checkout of the
    /// Beyond-All-Reason repository (`unitpics/<name>.dds`, and the skull from
    /// `luaui/images/skull.dds`). Missing ones are skipped.
    pub fn from_game_checkout<'a>(root: &Path, names: impl IntoIterator<Item = &'a str>) -> Icons {
        let mut icons = Icons::default();
        for name in names {
            if icons.by_name.contains_key(name) || !plausible_def(name) {
                continue;
            }
            let path = root.join(picture_file(name));
            let read = std::fs::read(&path).with_context(|| format!("reading {}", path.display()));
            match read.and_then(|bytes| decode_picture(&bytes, name != SKULL)) {
                Ok(image) => icons.insert(name, image),
                Err(e) => eprintln!("no picture for {name}: {e:#}"),
            }
        }
        icons
    }
}

/// Display names by unit-def name (`armfus` → "Fusion Reactor"), from a game's
/// `language/en/units.json`.
#[derive(Default, Clone)]
pub struct UnitNames {
    by_def: Arc<HashMap<String, String>>,
}

impl UnitNames {
    fn parse(bytes: &[u8]) -> Result<UnitNames> {
        #[derive(serde::Deserialize)]
        struct File {
            units: Units,
        }
        #[derive(serde::Deserialize)]
        struct Units {
            names: HashMap<String, String>,
        }
        let file: File = serde_json::from_slice(bytes).context("reading the unit names")?;
        Ok(UnitNames { by_def: Arc::new(file.units.names) })
    }

    /// The name BAR shows for a unit, if the game has one.
    pub fn get(&self, def: &str) -> Option<&str> {
        self.by_def.get(def).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_def.len()
    }
}

/// What an installed game version gave for one request.
pub struct GameArt {
    pub version: String,
    pub icons: Icons,
    pub names: UnitNames,
    /// Asked-for pictures that version does not have (they fall back to glyphs).
    pub absent: Vec<String>,
    pub took: Duration,
}

impl GameArt {
    /// One line for the log.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "read {} marker pictures and {} unit names from the installed {} in {} ms",
            self.icons.len(),
            self.names.len(),
            self.version,
            self.took.as_millis()
        );
        if !self.absent.is_empty() {
            line += &format!("; not in that version, drawn as glyphs: {}", self.absent.join(", "));
        }
        line
    }
}

/// One installed version, as far as it has been read.
struct Installed {
    archive: Archive,
    /// `None`: the version has no such picture, or it does not decode.
    pictures: HashMap<String, Option<Image>>,
    names: Option<UnitNames>,
}

type Key = (PathBuf, String);
static INSTALLED: LazyLock<Mutex<HashMap<Key, Arc<Mutex<Installed>>>>> = LazyLock::new(Default::default);

/// The cached reader for `version`, opening it the first time. A failure is not
/// kept: the app may be installing that version right now.
fn installed(data_root: &Path, version: &str) -> Result<Arc<Mutex<Installed>>> {
    let key = (data_root.to_path_buf(), version.to_owned());
    if let Some(found) = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Ok(found.clone());
    }
    // Opened outside the lock; two first requests at once both open it, harmlessly.
    let archive = Archive::open_version(data_root, version)?;
    let fresh = Arc::new(Mutex::new(Installed { archive, pictures: HashMap::new(), names: None }));
    Ok(INSTALLED.lock().unwrap_or_else(PoisonError::into_inner).entry(key).or_insert(fresh).clone())
}

/// Reads the pictures `names` ask for (unit-def names, and [`SKULL`]) and the unit
/// names from the installed game version `version` (a replay's `gametype`). Blocks
/// on the disk; see [`Loading`] for use from a window.
///
/// An error means the version cannot be read at all (not installed, no BAR in
/// `data_root`, a broken index); a picture that version lacks is only listed in
/// [`GameArt::absent`], and one that cannot be read is logged and left out.
pub fn load_installed(data_root: &Path, version: &str, names: &[String]) -> Result<GameArt> {
    let started = Instant::now();
    let shared = installed(data_root, version)?;
    let mut cached = shared.lock().unwrap_or_else(PoisonError::into_inner);
    let mut icons = Icons::default();
    let mut absent = Vec::new();
    for name in names {
        if icons.by_name.contains_key(name.as_str()) || absent.contains(name) || !plausible_def(name) {
            continue;
        }
        if !cached.pictures.contains_key(name) {
            match read_picture(&cached.archive, name) {
                Ok(picture) => {
                    cached.pictures.insert(name.clone(), picture);
                }
                // Not kept, so a later request tries again.
                Err(e) => eprintln!("no picture for {name} from {version}: {e:#}"),
            }
        }
        match cached.pictures.get(name) {
            Some(Some(image)) => icons.insert(name, image.clone()),
            Some(None) => absent.push(name.clone()),
            None => {}
        }
    }
    if cached.names.is_none() {
        let read = cached.archive.read(NAMES_FILE, MAX_NAMES_BYTES).and_then(|bytes| match bytes {
            Some(bytes) => UnitNames::parse(&bytes),
            None => anyhow::bail!("the game has no {NAMES_FILE}"),
        });
        match read {
            Ok(n) => cached.names = Some(n),
            Err(e) => eprintln!("no unit names from {version}: {e:#}"),
        }
    }
    let names = cached.names.clone().unwrap_or_default();
    Ok(GameArt { version: version.to_owned(), icons, names, absent, took: started.elapsed() })
}

/// The picture `name` stands for, or `None` if this version has none (or it does
/// not decode: a broken picture is as good as none and is not worth retrying).
fn read_picture(archive: &Archive, name: &str) -> Result<Option<Image>> {
    let Some(bytes) = archive.read(&picture_file(name), MAX_PICTURE_BYTES)? else { return Ok(None) };
    match decode_picture(&bytes, name != SKULL) {
        Ok(image) => Ok(Some(image)),
        Err(e) => {
            eprintln!("picture for {name} does not decode: {e:#}");
            Ok(None)
        }
    }
}

/// [`load_installed`] on a thread of its own, so a window keeps drawing.
pub struct Loading {
    rx: mpsc::Receiver<Result<GameArt>>,
}

impl Loading {
    /// Starts the load; `wake` is called from the loading thread once the result
    /// is ready (a window that does not redraw on its own uses it to ask for a frame).
    pub fn start(data_root: PathBuf, version: String, names: Vec<String>, wake: impl FnOnce() + Send + 'static) -> Loading {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("game-art".into()).spawn(move || {
            let _ = tx.send(load_installed(&data_root, &version, &names));
            wake();
        });
        if let Err(e) = spawned {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(Err(anyhow::Error::new(e).context("starting to read the game's pictures")));
            return Loading { rx };
        }
        Loading { rx }
    }

    /// The result, when it is ready. Once it has been given, the next call says
    /// the loader is gone.
    pub fn poll(&self) -> Option<Result<GameArt>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("reading the game's pictures stopped"))),
        }
    }

    /// The result, waiting up to `timeout` for it.
    pub fn wait(&self, timeout: Duration) -> Option<Result<GameArt>> {
        match self.rx.recv_timeout(timeout) {
            Ok(result) => Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(anyhow::anyhow!("reading the game's pictures stopped"))),
        }
    }
}

/// Names a unit def could have; anything else is not looked up.
fn plausible_def(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Where a picture sits in the game, relative to its root.
fn picture_file(name: &str) -> String {
    if name == SKULL {
        SKULL_FILE.to_owned()
    } else {
        format!("unitpics/{name}.dds")
    }
}

fn decode_picture(bytes: &[u8], crop: bool) -> Result<Image> {
    let mut rgba = dds::decode(bytes)?;
    if crop {
        // BAR zooms unit pictures in by 6% to cut the empty margin.
        let (w, h) = rgba.dimensions();
        let (mx, my) = (w * 6 / 100, h * 6 / 100);
        rgba = image::imageops::crop_imm(&rgba, mx, my, w - 2 * mx, h - 2 * my).to_image();
    }
    Ok(keep(rgba))
}

fn keep(rgba: image::RgbaImage) -> Image {
    let rgba = if rgba.width() > KEEP_PX || rgba.height() > KEEP_PX {
        image::imageops::resize(&rgba, KEEP_PX, KEEP_PX, image::imageops::FilterType::Triangle)
    } else {
        rgba
    };
    let (w, h) = rgba.dimensions();
    Image::from_rgba8(w, h, rgba.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_from_the_game_layout() {
        let json = br#"{"units": {"factions": {"arm": "Armada"}, "dead": "%{name} Wreckage",
            "names": {"armfus": "Fusion Reactor"}, "descriptions": {"armfus": "Produces Energy"}}}"#;
        let names = UnitNames::parse(json).unwrap();
        assert_eq!(names.get("armfus"), Some("Fusion Reactor"));
        assert_eq!(names.get("corfus"), None);
        assert!(UnitNames::parse(b"{\"units\": {}}").is_err());
        assert!(UnitNames::parse(b"\xff").is_err());
    }

    #[test]
    fn only_def_names_are_looked_up() {
        assert!(plausible_def("armfus") && plausible_def("leg_x2"));
        assert!(!plausible_def("") && !plausible_def("../x") && !plausible_def("a/b") && !plausible_def(&"a".repeat(65)));
        assert_eq!(picture_file(SKULL), "luaui/images/skull.dds");
        assert_eq!(picture_file("armcom"), "unitpics/armcom.dds");
    }

    #[test]
    fn a_data_folder_without_bar_is_an_error() {
        let empty = std::env::temp_dir().join(format!("barwatch-icons-test-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let e = load_installed(&empty, "Beyond All Reason test-1-abc", &["armcom".into()]).err().unwrap();
        assert!(format!("{e:#}").contains("no rapid folder"), "{e:#}");
        std::fs::remove_dir_all(&empty).unwrap();
    }
}
