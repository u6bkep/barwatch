//! `.sdfz` reader: header, embedded start script and the first stream packet.
//!
//! Layout follows the engine's `rts/System/LoadSave/demofile.h` (version 5,
//! little-endian). Only the head of the gzip stream is decompressed, so listing a
//! folder of replays stays cheap.

use anyhow::{bail, ensure, Context, Result};
use serde_json::{Map, Value};
use std::io::Read;
use std::path::Path;

const HEADER_V5_SIZE: usize = 352;
const NETMSG_GAMEDATA: u8 = 52;
/// Upper bound for the start script and the first packet; real ones are a few KiB.
const MAX_BLOB: usize = 4 << 20;

/// What the first packet of the demo stream (`NETMSG_GAMEDATA`) carries besides the
/// setup text. A server that hosts the demo must hand these to its clients unchanged,
/// or their simulation diverges from the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameData {
    /// Setup text exactly as the original server sent it.
    pub setup_text: String,
    pub map_hash: [u8; 64],
    pub mod_hash: [u8; 64],
    pub random_seed: u32,
}

#[derive(Debug, Clone)]
pub struct Demo {
    pub engine_version: String,
    /// Unix time the game started.
    pub unix_time: u64,
    pub game_time_s: u32,
    pub wall_time_s: u32,
    /// Start script as nested objects, keys lowercased; leaves are strings.
    pub script: Value,
    pub game_data: GameData,
}

/// Who played, grouped the way a lobby shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    /// One entry per ally team that has members, in ally-team order.
    pub teams: Vec<Vec<String>>,
    pub spectators: Vec<String>,
}

impl Demo {
    pub fn read(path: &Path) -> Result<Demo> {
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Demo::from_reader(flate2::read::GzDecoder::new(std::io::BufReader::new(file)))
            .with_context(|| format!("reading {}", path.display()))
    }

    pub fn from_gz_bytes(data: &[u8]) -> Result<Demo> {
        Demo::from_reader(flate2::read::GzDecoder::new(data))
    }

    /// Reads from an already decompressed stream.
    pub fn from_reader(mut r: impl Read) -> Result<Demo> {
        let mut head = [0u8; HEADER_V5_SIZE];
        r.read_exact(&mut head).context("file too short for a demo header")?;
        let i32_at = |off: usize| i32::from_le_bytes(head[off..off + 4].try_into().unwrap());
        let magic = cstr(&head[0..16]);
        ensure!(magic == "spring demofile", "not a replay (magic {magic:?})");
        let version = i32_at(16);
        ensure!(version == 5, "unsupported demo header version {version}");
        let header_size = i32_at(20);
        ensure!(header_size as usize == HEADER_V5_SIZE, "unexpected header size {header_size}");
        let engine_version = cstr(&head[24..280]);
        let unix_time = u64::from_le_bytes(head[296..304].try_into().unwrap());
        let script_size = usize::try_from(i32_at(304)).context("negative script size")?;
        ensure!(script_size <= MAX_BLOB, "start script of {script_size} bytes is implausible");
        let game_time_s = i32_at(312).max(0) as u32;
        let wall_time_s = i32_at(316).max(0) as u32;

        let mut script_bytes = vec![0u8; script_size];
        r.read_exact(&mut script_bytes).context("truncated start script")?;
        let script = parse_script(&String::from_utf8_lossy(&script_bytes)).context("parsing start script")?;

        // First chunk of the stream: { f32 modGameTime, u32 length } + packet.
        let mut chunk = [0u8; 8];
        r.read_exact(&mut chunk).context("replay has no game stream")?;
        let len = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as usize;
        ensure!(len <= MAX_BLOB, "first packet of {len} bytes is implausible");
        let mut packet = vec![0u8; len];
        r.read_exact(&mut packet).context("truncated first packet")?;
        let game_data = parse_game_data(&packet).context("parsing the game-data packet")?;

        Ok(Demo { engine_version, unix_time, game_time_s, wall_time_s, script, game_data })
    }

    fn game(&self) -> &Value {
        &self.script["game"]
    }

    fn game_str(&self, key: &str) -> Option<&str> {
        self.game().get(key)?.as_str()
    }

    pub fn game_version(&self) -> Option<&str> {
        self.game_str("gametype")
    }

    pub fn map(&self) -> Option<&str> {
        self.game_str("mapname")
    }

    /// Names of every `[playerN]` section: players and the spectators present at
    /// start. The engine refuses a live viewer whose name equals one of these.
    pub fn player_names(&self) -> Vec<String> {
        self.sections("player").filter_map(|(_, sec)| sec.get("name")?.as_str().map(str::to_owned)).collect()
    }

    pub fn roster(&self) -> Roster {
        let ally_of_team = |team: &str| -> Option<u32> {
            self.game().get(format!("team{team}"))?.get("allyteam")?.as_str()?.trim().parse().ok()
        };
        let mut by_ally: std::collections::BTreeMap<u32, Vec<String>> = Default::default();
        let mut spectators = Vec::new();
        for (_, sec) in self.sections("player") {
            let Some(name) = sec.get("name").and_then(Value::as_str) else { continue };
            let is_spec = sec.get("spectator").and_then(Value::as_str).is_some_and(|s| s.trim() != "0");
            let ally = sec.get("team").and_then(Value::as_str).and_then(|t| ally_of_team(t.trim()));
            match (is_spec, ally) {
                (false, Some(a)) => by_ally.entry(a).or_default().push(name.to_owned()),
                _ => spectators.push(name.to_owned()),
            }
        }
        for (_, sec) in self.sections("ai") {
            let label = sec.get("shortname").or_else(|| sec.get("name")).and_then(Value::as_str).unwrap_or("AI");
            if let Some(a) = sec.get("team").and_then(Value::as_str).and_then(|t| ally_of_team(t.trim())) {
                by_ally.entry(a).or_default().push(format!("{label} (AI)"));
            }
        }
        Roster { teams: by_ally.into_values().collect(), spectators }
    }

    fn sections<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (u32, &'a Map<String, Value>)> + 'a {
        let mut found: Vec<(u32, &Map<String, Value>)> = self
            .game()
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(move |(k, v)| {
                let n = k.strip_prefix(prefix)?.parse().ok()?;
                Some((n, v.as_object()?))
            })
            .collect();
        found.sort_by_key(|(n, _)| *n);
        found.into_iter()
    }
}

fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
}

/// `NETMSG_GAMEDATA`: id u8, total size u16, compressed size u16, zlib setup text,
/// map hash, mod hash, random seed (engine `GameData::Pack`).
fn parse_game_data(pkt: &[u8]) -> Result<GameData> {
    ensure!(pkt.len() >= 5, "packet too short");
    ensure!(pkt[0] == NETMSG_GAMEDATA, "first packet has id {}, expected game data", pkt[0]);
    let zlen = u16::from_le_bytes([pkt[3], pkt[4]]) as usize;
    let tail = 5 + zlen;
    ensure!(pkt.len() >= tail + 64 + 64 + 4, "packet truncated");
    let mut setup = Vec::new();
    flate2::read::ZlibDecoder::new(&pkt[5..tail])
        .take(MAX_BLOB as u64)
        .read_to_end(&mut setup)
        .context("inflating setup text")?;
    Ok(GameData {
        setup_text: String::from_utf8_lossy(&setup).into_owned(),
        map_hash: pkt[tail..tail + 64].try_into().unwrap(),
        mod_hash: pkt[tail + 64..tail + 128].try_into().unwrap(),
        random_seed: u32::from_le_bytes(pkt[tail + 128..tail + 132].try_into().unwrap()),
    })
}

/// Parses Spring's TDF-like script format: `[name] { key=value; [sub] { … } }`.
pub fn parse_script(text: &str) -> Result<Value> {
    let mut p = ScriptParser { s: text.as_bytes(), i: 0 };
    let root = p.block(true)?;
    Ok(Value::Object(root))
}

struct ScriptParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl ScriptParser<'_> {
    fn skip_ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"//") {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
            } else {
                return;
            }
        }
    }

    fn block(&mut self, top: bool) -> Result<Map<String, Value>> {
        let mut map = Map::new();
        loop {
            self.skip_ws();
            if self.i >= self.s.len() {
                if top {
                    return Ok(map);
                }
                bail!("unterminated block");
            }
            match self.s[self.i] {
                b'}' if !top => {
                    self.i += 1;
                    return Ok(map);
                }
                b'[' => {
                    let end = self.s[self.i..].iter().position(|&c| c == b']').context("unterminated section name")?;
                    let name = String::from_utf8_lossy(&self.s[self.i + 1..self.i + end]).trim().to_lowercase();
                    self.i += end + 1;
                    self.skip_ws();
                    ensure!(self.i < self.s.len() && self.s[self.i] == b'{', "expected '{{' after [{name}]");
                    self.i += 1;
                    let sub = self.block(false)?;
                    map.insert(name, Value::Object(sub));
                }
                _ => {
                    let rest = &self.s[self.i..];
                    let eq = rest.iter().position(|&c| c == b'=' || c == b'\n' || c == b'}');
                    match eq {
                        Some(eq) if rest[eq] == b'=' => {
                            let key = String::from_utf8_lossy(&rest[..eq]).trim().to_lowercase();
                            let semi = rest[eq..].iter().position(|&c| c == b';').context("value without ';'")?;
                            let val = String::from_utf8_lossy(&rest[eq + 1..eq + semi]).trim().to_owned();
                            map.insert(key, Value::String(val));
                            self.i += eq + semi + 1;
                        }
                        _ => bail!("unexpected text at byte {}", self.i),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) const SETUP: &str = "[game]\n{\n[team0]\n{\nallyteam=0;\nteamleader=0;\n}\n[team1]\n{\nallyteam=1;\n}\n\
[modoptions]\n{\nhostip=keep-me;\nmaxspeed=3;\n}\n[player0]\n{\nteam=0;\nname=Alice;\nspectator=0;\n}\n\
[player1]\n{\nname=Watcher;\nspectator=1;\n}\n[ai0]\n{\nteam=1;\nname=Bot(1);\nshortname=BARb;\n}\n\
hostip=1.2.3.4;\nhostport=8452;\nishost=1;\nmyplayername=Host;\ngametype=Some Game 1.0;\nmapname=Some Map v1;\n}\n";

    /// A minimal, structurally valid uncompressed demo.
    pub(crate) fn fake_demo_bytes() -> Vec<u8> {
        let mut head = vec![0u8; HEADER_V5_SIZE];
        head[..15].copy_from_slice(b"spring demofile");
        head[16..20].copy_from_slice(&5i32.to_le_bytes());
        head[20..24].copy_from_slice(&(HEADER_V5_SIZE as i32).to_le_bytes());
        head[24..34].copy_from_slice(b"2026.07.04");
        head[296..304].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        head[304..308].copy_from_slice(&(SETUP.len() as i32).to_le_bytes());
        head[312..316].copy_from_slice(&834i32.to_le_bytes());
        head[316..320].copy_from_slice(&938i32.to_le_bytes());

        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(SETUP.as_bytes()).unwrap();
        let z = z.finish().unwrap();
        let mut pkt = vec![NETMSG_GAMEDATA, 0, 0];
        pkt.extend_from_slice(&(z.len() as u16).to_le_bytes());
        pkt.extend_from_slice(&z);
        pkt.extend_from_slice(&[0xab; 64]);
        pkt.extend_from_slice(&[0xcd; 64]);
        pkt.extend_from_slice(&668_500_579u32.to_le_bytes());

        let mut out = head;
        out.extend_from_slice(SETUP.as_bytes());
        out.extend_from_slice(&0.0f32.to_le_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(&pkt);
        out
    }

    #[test]
    fn reads_header_script_and_game_data() {
        let demo = Demo::from_reader(&fake_demo_bytes()[..]).unwrap();
        assert_eq!(demo.engine_version, "2026.07.04");
        assert_eq!(demo.unix_time, 1_700_000_000);
        assert_eq!(demo.game_time_s, 834);
        assert_eq!(demo.map(), Some("Some Map v1"));
        assert_eq!(demo.game_version(), Some("Some Game 1.0"));
        assert_eq!(demo.player_names(), ["Alice", "Watcher"]);
        assert_eq!(demo.game_data.setup_text, SETUP);
        assert_eq!(demo.game_data.random_seed, 668_500_579);
        assert_eq!(demo.game_data.map_hash, [0xab; 64]);
    }

    #[test]
    fn roster_groups_by_ally_team() {
        let demo = Demo::from_reader(&fake_demo_bytes()[..]).unwrap();
        let roster = demo.roster();
        assert_eq!(roster.teams, vec![vec!["Alice".to_string()], vec!["BARb (AI)".to_string()]]);
        assert_eq!(roster.spectators, ["Watcher"]);
    }

    #[test]
    fn rejects_other_files() {
        assert!(Demo::from_reader(&[0u8; 400][..]).is_err());
        assert!(Demo::from_reader(&b"short"[..]).is_err());
    }
}
