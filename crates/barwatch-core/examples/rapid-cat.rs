//! Writes a file of an installed game version to stdout, for checking the rapid
//! reader against a game checkout:
//! `cargo run -p barwatch-core --example rapid-cat -- <BAR data dir> "<version>" unitpics/armcom.dds`

use barwatch_core::rapid::Archive;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [data, version, file] = args.as_slice() else { anyhow::bail!("usage: rapid-cat <data dir> <game version> <file>") };
    let archive = Archive::open_version(data.as_ref(), version)?;
    let bytes = archive.read(file, u32::MAX)?.ok_or_else(|| anyhow::anyhow!("{version} has no {file}"))?;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}
