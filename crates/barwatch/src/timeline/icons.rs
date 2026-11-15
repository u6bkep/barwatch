//! Pictures for event markers, supplied at run time.
//!
//! BAR's unit pictures and interface images are licensed CC BY-NC-ND or "all
//! rights reserved" (`license_unitpics.txt`, `license_icons.txt`,
//! `luaui/images/license.txt` in the game repository), so barwatch carries none:
//! a server hands them over (it already converts unit pictures), or, while
//! developing, they are read from a local checkout of the game. Without a
//! picture a marker falls back to a glyph of its own.

use super::dds;
use super::marks::SKULL;
use anyhow::{Context, Result};
use damascene_core::prelude::Image;
use std::collections::HashMap;
use std::path::Path;

/// Pictures are kept this many pixels across; badges are drawn at about 20.
const KEEP_PX: u32 = 64;

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
            if icons.by_name.contains_key(name) || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                continue;
            }
            let path =
                if name == SKULL { root.join("luaui/images/skull.dds") } else { root.join("unitpics").join(format!("{name}.dds")) };
            match read_dds(&path, name != SKULL) {
                Ok(image) => icons.insert(name, image),
                Err(e) => eprintln!("no picture for {name}: {e:#}"),
            }
        }
        icons
    }
}

fn read_dds(path: &Path, crop: bool) -> Result<Image> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut rgba = dds::decode(&bytes)?;
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
