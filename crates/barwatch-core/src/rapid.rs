//! Reading files out of a game version installed through "rapid", the way BAR
//! distributes its game: content-addressed files plus an index per version.
//!
//! Layout of a data directory, as `pr-downloader` writes it and the engine reads it
//! (`rts/System/FileSystem/Archives/PoolArchive.cpp`); checked against a real BAR
//! installation (2026-11):
//!
//! - `rapid/<repo host>/<repo>/versions.gz`: the repository's whole catalogue, not
//!   only what is installed. Gzip'd text, one line per version:
//!   `tag,index md5,dependencies,name`. The name is what a replay's `gametype` holds
//!   (`Beyond All Reason test-31836-b250a16`); one version may appear under several
//!   tags.
//! - `packages/<index md5>.sdp`: present once the version is installed. A gzip
//!   stream of entries: a length byte, the file name (lower case, `/`-separated),
//!   the md5 of the file's content (16 bytes), its crc32 and its size (4 bytes each,
//!   big-endian).
//! - `pool/<first two hex digits of the md5>/<other thirty>.gz`: the file, gzip'd.
//!
//! Everything read here is bounded: a replay names the version, so a stranger picks
//! which index is opened, and a broken installation must not take the app down.

use anyhow::{bail, ensure, Context, Result};
use flate2::read::MultiGzDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest `versions.gz` read, decompressed (BAR's main repository: about 1.2 MB).
const MAX_CATALOGUE_BYTES: u64 = 64 << 20;
/// Largest index read, decompressed (a BAR version: about 1.3 MB, 19,000 files).
const MAX_INDEX_BYTES: u64 = 32 << 20;

/// One file of a version, as its index lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    md5: [u8; 16],
    crc32: u32,
    size: u32,
}

/// An installed game version's index, ready to hand out its files.
#[derive(Debug)]
pub struct Archive {
    /// The data directory's `pool` folder.
    pool: PathBuf,
    /// Keyed by lower-case name, as the engine looks them up.
    files: HashMap<String, Entry>,
}

/// The index of an installed version: `<data>/packages/<md5>.sdp`.
///
/// Fails, with a message that says which, when `data_root` holds no rapid content
/// at all, when no catalogue lists `version`, or when it is listed but not
/// downloaded.
pub fn find_index(data_root: &Path, version: &str) -> Result<PathBuf> {
    let rapid = data_root.join("rapid");
    ensure!(rapid.is_dir(), "no game is installed in {} (it has no rapid folder)", data_root.display());
    let mut catalogues = Vec::new();
    for host in read_dir_sorted(&rapid)? {
        if host.is_dir() {
            catalogues.extend(read_dir_sorted(&host)?.into_iter().map(|repo| repo.join("versions.gz")).filter(|p| p.is_file()));
        }
    }
    ensure!(!catalogues.is_empty(), "no game is installed in {} (no rapid repository lists anything)", data_root.display());
    let mut listed = false;
    for catalogue in &catalogues {
        let text = read_gz(catalogue, MAX_CATALOGUE_BYTES).with_context(|| format!("reading {}", catalogue.display()))?;
        let text = String::from_utf8_lossy(&text);
        for md5 in index_hashes(&text, version) {
            listed = true;
            let sdp = data_root.join("packages").join(format!("{md5}.sdp"));
            if sdp.is_file() {
                return Ok(sdp);
            }
        }
    }
    if listed {
        bail!("{version} is not installed in {} (listed, but its index is not downloaded)", data_root.display())
    }
    bail!("{version} is not installed in {} (no rapid repository lists it)", data_root.display())
}

/// The index md5s a catalogue gives for `version`, in catalogue order.
fn index_hashes<'a>(catalogue: &'a str, version: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    catalogue.lines().filter_map(move |line| {
        let mut fields = line.trim_end_matches('\r').splitn(4, ',');
        let (_tag, md5, _deps, name) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
        (name == version && is_md5_hex(md5)).then_some(md5)
    })
}

fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    out.sort();
    Ok(out)
}

/// A whole gzip file, refusing more than `max` bytes of output.
fn read_gz(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut out = Vec::new();
    MultiGzDecoder::new(std::io::BufReader::new(file)).take(max + 1).read_to_end(&mut out)?;
    ensure!(out.len() as u64 <= max, "more than {max} bytes once decompressed");
    Ok(out)
}

impl Archive {
    /// Opens the installed version whose name a replay gives (`gametype`).
    pub fn open_version(data_root: &Path, version: &str) -> Result<Archive> {
        Archive::open_index(&find_index(data_root, version)?)
    }

    /// Opens an index file; its pool is the `pool` folder beside `packages`.
    pub fn open_index(sdp: &Path) -> Result<Archive> {
        let bytes = read_gz(sdp, MAX_INDEX_BYTES).with_context(|| format!("reading {}", sdp.display()))?;
        let files = parse_index(&bytes).with_context(|| format!("reading {}", sdp.display()))?;
        let data_root = sdp.parent().and_then(Path::parent).context("an index must sit in <data>/packages")?;
        Ok(Archive { pool: data_root.join("pool"), files })
    }

    /// How many files the version has.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.files.contains_key(&name.to_ascii_lowercase())
    }

    /// The bytes of `name` (`unitpics/armcom.dds`; case does not matter), or `None`
    /// when this version has no such file. An error means the file is listed but
    /// cannot be had: larger than `max_bytes`, missing from the pool, or not what
    /// the index says (size or crc32).
    pub fn read(&self, name: &str, max_bytes: u32) -> Result<Option<Vec<u8>>> {
        let Some(entry) = self.files.get(&name.to_ascii_lowercase()) else { return Ok(None) };
        ensure!(entry.size <= max_bytes, "{name} is {} bytes, more than the {max_bytes} allowed", entry.size);
        let path = self.pool_path(&entry.md5);
        let file = std::fs::File::open(&path).with_context(|| format!("{name}: opening {}", path.display()))?;
        let mut out = Vec::with_capacity(entry.size as usize);
        MultiGzDecoder::new(std::io::BufReader::new(file))
            .take(u64::from(entry.size) + 1)
            .read_to_end(&mut out)
            .with_context(|| format!("{name}: decompressing {}", path.display()))?;
        ensure!(out.len() == entry.size as usize, "{name}: {} holds {} bytes, the index says {}", path.display(), out.len(), entry.size);
        ensure!(crc32fast::hash(&out) == entry.crc32, "{name}: {} does not match the index's checksum", path.display());
        Ok(Some(out))
    }

    fn pool_path(&self, md5: &[u8; 16]) -> PathBuf {
        let hex: String = md5.iter().map(|b| format!("{b:02x}")).collect();
        self.pool.join(&hex[..2]).join(format!("{}.gz", &hex[2..]))
    }
}

/// The entries of a decompressed index. A truncated entry is an error: the
/// installation is broken, and what precedes it cannot be trusted either.
fn parse_index(mut b: &[u8]) -> Result<HashMap<String, Entry>> {
    let mut files = HashMap::new();
    let mut at = 0usize;
    while let Some((&len, rest)) = b.split_first() {
        let need = len as usize + 24;
        ensure!(rest.len() >= need, "index entry at byte {at} is cut short");
        let (name, rest) = rest.split_at(len as usize);
        let u32_at = |o: usize| u32::from_be_bytes(rest[o..o + 4].try_into().unwrap());
        let entry = Entry { md5: rest[..16].try_into().unwrap(), crc32: u32_at(16), size: u32_at(20) };
        // Later entries win, as in the engine.
        files.insert(String::from_utf8_lossy(name).to_ascii_lowercase(), entry);
        b = &rest[24..];
        at += 1 + need;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    /// A scratch data directory holding one version made of `files`.
    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn md5_of(name: &str) -> [u8; 16] {
        // Any distinct 16 bytes do: the reader trusts the index for the address.
        let mut m = [0u8; 16];
        for (i, b) in name.bytes().enumerate() {
            m[i % 16] = m[i % 16].wrapping_mul(31).wrapping_add(b);
        }
        m
    }

    fn entry_bytes(name: &str, md5: [u8; 16], crc: u32, size: u32) -> Vec<u8> {
        let mut e = vec![name.len() as u8];
        e.extend_from_slice(name.as_bytes());
        e.extend_from_slice(&md5);
        e.extend_from_slice(&crc.to_be_bytes());
        e.extend_from_slice(&size.to_be_bytes());
        e
    }

    const INDEX: &str = "0123456789abcdef0123456789abcdef";

    fn fixture(tag: &str, files: &[(&str, &[u8])]) -> Fixture {
        let root = std::env::temp_dir().join(format!("barwatch-rapid-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("rapid/repos.example/byar");
        std::fs::create_dir_all(&repo).unwrap();
        let catalogue = format!(
            "byar:test,ffffffffffffffffffffffffffffffff,,Game other-1\nbyar:git:abc,{INDEX},,Game test-2-abc\nbyar:test,{INDEX},,Game test-2-abc\n"
        );
        std::fs::write(repo.join("versions.gz"), gz(catalogue.as_bytes())).unwrap();
        std::fs::create_dir_all(root.join("packages")).unwrap();
        let mut index = Vec::new();
        for (name, content) in files {
            let md5 = md5_of(name);
            index.extend(entry_bytes(name, md5, crc32fast::hash(content), content.len() as u32));
            let hex: String = md5.iter().map(|b| format!("{b:02x}")).collect();
            let dir = root.join("pool").join(&hex[..2]);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{}.gz", &hex[2..])), gz(content)).unwrap();
        }
        std::fs::write(root.join(format!("packages/{INDEX}.sdp")), gz(&index)).unwrap();
        Fixture { root }
    }

    #[test]
    fn reads_a_file_by_name() {
        let f = fixture("read", &[("unitpics/armcom.dds", b"DDS picture"), ("language/en/units.json", b"{}")]);
        let a = Archive::open_version(&f.root, "Game test-2-abc").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.read("unitpics/armcom.dds", 1 << 20).unwrap().as_deref(), Some(&b"DDS picture"[..]));
        assert_eq!(a.read("UnitPics/ArmCom.dds", 1 << 20).unwrap().as_deref(), Some(&b"DDS picture"[..]));
        assert!(a.contains("language/en/units.json"));
    }

    #[test]
    fn a_missing_file_is_none() {
        let f = fixture("missing", &[("unitpics/armcom.dds", b"x")]);
        let a = Archive::open_version(&f.root, "Game test-2-abc").unwrap();
        assert!(a.read("unitpics/legcom.dds", 1 << 20).unwrap().is_none());
    }

    #[test]
    fn missing_versions_say_why() {
        let f = fixture("versions", &[]);
        let e = Archive::open_version(&f.root, "Game test-9-zzz").unwrap_err();
        assert!(format!("{e:#}").contains("no rapid repository lists it"), "{e:#}");
        let e = Archive::open_version(&f.root, "Game other-1").unwrap_err();
        assert!(format!("{e:#}").contains("not downloaded"), "{e:#}");
        let empty = std::env::temp_dir().join(format!("barwatch-rapid-test-{}-empty", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let e = Archive::open_version(&empty, "Game test-2-abc").unwrap_err();
        assert!(format!("{e:#}").contains("no rapid folder"), "{e:#}");
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn catalogue_lines() {
        let text = "a,0123456789abcdef0123456789abcdef,,Name, with comma\r\nb,nothex,,Name, with comma\nc,ffffffffffffffffffffffffffffffff,dep,Other\n";
        assert_eq!(index_hashes(text, "Name, with comma").collect::<Vec<_>>(), vec!["0123456789abcdef0123456789abcdef"]);
        assert_eq!(index_hashes("short,line", "line").count(), 0);
    }

    #[test]
    fn index_parsing() {
        let mut b = entry_bytes("A/B.txt", [1; 16], 7, 9);
        b.extend(entry_bytes("a/b.txt", [2; 16], 8, 10));
        b.extend(entry_bytes("", [3; 16], 0, 0));
        let files = parse_index(&b).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files["a/b.txt"], Entry { md5: [2; 16], crc32: 8, size: 10 });
        assert!(parse_index(&[]).unwrap().is_empty());
    }

    #[test]
    fn truncated_index_is_an_error() {
        let b = entry_bytes("unitpics/armcom.dds", [1; 16], 7, 9);
        for cut in 1..b.len() {
            assert!(parse_index(&b[..cut]).is_err(), "cut at {cut}");
        }
        let mut two = b.clone();
        two.extend_from_slice(&b[..5]);
        assert!(format!("{:#}", parse_index(&two).unwrap_err()).contains("byte 44"));
    }

    #[test]
    fn oversized_and_corrupt_entries_are_errors() {
        let f = fixture("corrupt", &[("big.bin", &[7u8; 4096]), ("ok.bin", b"fine")]);
        let a = Archive::open_version(&f.root, "Game test-2-abc").unwrap();
        let e = a.read("big.bin", 1024).unwrap_err();
        assert!(format!("{e:#}").contains("more than the 1024 allowed"), "{e:#}");
        assert_eq!(a.read("big.bin", 4096).unwrap().unwrap().len(), 4096);

        // Pool file swapped for other content of the same size: checksum.
        let path = a.pool_path(&md5_of("ok.bin"));
        std::fs::write(&path, gz(b"evil")).unwrap();
        assert!(format!("{:#}", a.read("ok.bin", 64).unwrap_err()).contains("checksum"));
        // Longer than the index says: refused without reading it all.
        std::fs::write(&path, gz(&[0u8; 1 << 16])).unwrap();
        assert!(format!("{:#}", a.read("ok.bin", 64).unwrap_err()).contains("index says 4"));
        // Not gzip at all.
        std::fs::write(&path, b"plain").unwrap();
        assert!(a.read("ok.bin", 64).is_err());
        // Gone from the pool.
        std::fs::remove_file(&path).unwrap();
        assert!(format!("{:#}", a.read("ok.bin", 64).unwrap_err()).contains("opening"));
    }

    #[test]
    fn a_broken_index_file_is_an_error() {
        let f = fixture("badindex", &[("ok.bin", b"fine")]);
        let sdp = f.root.join(format!("packages/{INDEX}.sdp"));
        std::fs::write(&sdp, b"not gzip").unwrap();
        assert!(Archive::open_version(&f.root, "Game test-2-abc").is_err());
        std::fs::write(&sdp, gz(&entry_bytes("ok.bin", [1; 16], 0, 4)[..10])).unwrap();
        assert!(format!("{:#}", Archive::open_version(&f.root, "Game test-2-abc").unwrap_err()).contains("cut short"));
    }
}
