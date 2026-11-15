//! The per-team statistics history a replay carries after its game stream.
//!
//! A game client writes one record per team every `period_s` seconds of game time
//! (engine `TeamStatistics`, `CDemoRecorder::WriteTeamStats`). A dedicated server
//! does not simulate, so the replays it records hold at most one record per team:
//! [`StatsHistory::has_history`] tells the two apart.

use anyhow::{ensure, Context, Result};
use std::io::Read;
use std::path::Path;

const HEADER_V5_SIZE: usize = 352;
const RECORD_SIZE: usize = 80;
/// Simulation frames per second of game time.
pub const GAME_SPEED: f32 = 30.0;
/// Largest replay read into memory; the biggest real ones are a few tens of MiB.
const MAX_REPLAY: u64 = 1 << 30;

/// One team's counters at one moment. Everything but `frame` is cumulative since
/// the start of the game.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TeamSample {
    pub frame: i32,
    pub metal_used: f32,
    pub energy_used: f32,
    pub metal_produced: f32,
    pub energy_produced: f32,
    pub metal_excess: f32,
    pub energy_excess: f32,
    pub metal_received: f32,
    pub energy_received: f32,
    pub metal_sent: f32,
    pub energy_sent: f32,
    pub damage_dealt: f32,
    pub damage_received: f32,
    pub units_produced: i32,
    pub units_died: i32,
    pub units_received: i32,
    pub units_sent: i32,
    pub units_captured: i32,
    pub units_out_captured: i32,
    pub units_killed: i32,
}

impl TeamSample {
    /// Units the team owns at this moment.
    pub fn units_alive(&self) -> i32 {
        self.units_produced + self.units_received + self.units_captured
            - self.units_died
            - self.units_sent
            - self.units_out_captured
    }

    fn parse(b: &[u8]) -> TeamSample {
        let i = |n: usize| i32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap());
        let f = |n: usize| f32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap());
        TeamSample {
            frame: i(0),
            metal_used: f(1),
            energy_used: f(2),
            metal_produced: f(3),
            energy_produced: f(4),
            metal_excess: f(5),
            energy_excess: f(6),
            metal_received: f(7),
            energy_received: f(8),
            metal_sent: f(9),
            energy_sent: f(10),
            damage_dealt: f(11),
            damage_received: f(12),
            units_produced: i(13),
            units_died: i(14),
            units_received: i(15),
            units_sent: i(16),
            units_captured: i(17),
            units_out_captured: i(18),
            units_killed: i(19),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatsHistory {
    /// Game seconds between two records.
    pub period_s: u32,
    /// Length of the game in seconds, from the header.
    pub game_time_s: u32,
    /// Records per engine team, indexed by team number.
    pub teams: Vec<Vec<TeamSample>>,
    /// What the game stream itself says, for the same timeline.
    pub events: StreamFacts,
}

/// Facts read from the replay's packet stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamFacts {
    /// The colours BAR gave each team on screen (its `AutoColors` Lua message):
    /// engine team number → sRGB.
    pub team_colors: std::collections::BTreeMap<u32, [u8; 3]>,
    /// Teams the engine declared dead, with the game frame it happened at.
    pub team_deaths: Vec<(i32, u32)>,
}

const PKT_KEYFRAME: u8 = 1;
const PKT_NEW_FRAME: u8 = 2;
const PKT_LUA_MSG: u8 = 50;
const PKT_TEAM_MSG: u8 = 51;
/// `TEAM_MSG` action: a team died (its parameter is the team).
const TEAM_DIED: u8 = 4;

impl StreamFacts {
    /// Walks the packet stream: each packet is `{ f32 time, u32 length }` then
    /// `length` bytes starting with the packet type. The frame number comes from
    /// counting frames, so it is game time from the start of play.
    pub fn scan(stream: &[u8]) -> StreamFacts {
        let mut facts = StreamFacts::default();
        let (mut i, mut frame) = (0usize, 0i32);
        while i + 8 <= stream.len() {
            let len = u32::from_le_bytes(stream[i + 4..i + 8].try_into().unwrap()) as usize;
            i += 8;
            if len == 0 || i + len > stream.len() {
                break;
            }
            let (kind, d) = (stream[i], &stream[i + 1..i + len]);
            i += len;
            match kind {
                PKT_KEYFRAME if d.len() >= 4 => frame = i32::from_le_bytes(d[0..4].try_into().unwrap()),
                PKT_NEW_FRAME => frame += 1,
                // i16 size, u8 player, i16 script, u8 mode, then the message.
                PKT_LUA_MSG if d.len() > 6 => {
                    if let Some(json) = d[6..].strip_prefix(b"AutoColors") {
                        facts.read_colors(json);
                    }
                }
                PKT_TEAM_MSG if d.len() >= 3 && d[1] == TEAM_DIED => {
                    let team = u32::from(d[2]);
                    if !facts.team_deaths.iter().any(|(_, t)| *t == team) {
                        facts.team_deaths.push((frame, team));
                    }
                }
                _ => {}
            }
        }
        facts
    }

    fn read_colors(&mut self, json: &[u8]) {
        let Ok(serde_json::Value::Array(teams)) = serde_json::from_slice::<serde_json::Value>(json) else { return };
        for t in teams {
            let c = |k: &str| t.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0).min(255) as u8;
            if let Some(team) = t.get("teamID").and_then(serde_json::Value::as_u64) {
                self.team_colors.insert(team as u32, [c("r"), c("g"), c("b")]);
            }
        }
    }
}

impl StatsHistory {
    pub fn read(path: &Path) -> Result<StatsHistory> {
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(std::io::BufReader::new(file))
            .take(MAX_REPLAY)
            .read_to_end(&mut data)
            .with_context(|| format!("reading {}", path.display()))?;
        StatsHistory::from_bytes(&data)
    }

    /// Parses a decompressed replay.
    pub fn from_bytes(d: &[u8]) -> Result<StatsHistory> {
        ensure!(d.len() >= HEADER_V5_SIZE, "file too short for a demo header");
        let field = |off: usize| -> Result<usize> {
            usize::try_from(i32::from_le_bytes(d[off..off + 4].try_into().unwrap())).context("negative size in header")
        };
        ensure!(field(20)? == HEADER_V5_SIZE, "unexpected header size");
        let (script, stream) = (field(304)?, field(308)?);
        let game_time_s = field(312)? as u32;
        let player_stats = field(324)?;
        let (num_teams, team_stats, elem, period) = (field(332)?, field(336)?, field(340)?, field(344)?);
        let winners = field(348)?;
        let events = d
            .get(HEADER_V5_SIZE + script..)
            .and_then(|rest| rest.get(..stream.min(rest.len())))
            .map(StreamFacts::scan)
            .unwrap_or_default();
        if team_stats == 0 {
            return Ok(StatsHistory { period_s: period as u32, game_time_s, teams: vec![Vec::new(); num_teams], events });
        }
        ensure!(elem == RECORD_SIZE, "team statistics records of {elem} bytes are not understood");
        let start = HEADER_V5_SIZE + script + stream + winners + player_stats;
        let end = start.checked_add(team_stats).filter(|&e| e <= d.len()).context("replay ends before its statistics")?;
        let block = &d[start..end];
        ensure!(block.len() >= num_teams * 4, "statistics block too short");
        let mut at = num_teams * 4;
        let mut teams = Vec::with_capacity(num_teams);
        for t in 0..num_teams {
            let count = usize::try_from(i32::from_le_bytes(block[t * 4..t * 4 + 4].try_into().unwrap()))
                .context("negative record count")?;
            let bytes = count.checked_mul(RECORD_SIZE).filter(|&n| at + n <= block.len()).context("statistics block truncated")?;
            teams.push(block[at..at + bytes].chunks_exact(RECORD_SIZE).map(TeamSample::parse).collect());
            at += bytes;
        }
        Ok(StatsHistory { period_s: period as u32, game_time_s, teams, events })
    }

    /// Whether this replay holds a time series (a client recorded it) and not just
    /// final values or nothing (a dedicated server did).
    pub fn has_history(&self) -> bool {
        self.period_s > 0 && self.teams.iter().any(|t| t.len() > 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(script: i32, stream: i32, teams: i32, team_stats: i32) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_V5_SIZE];
        let mut put = |off: usize, v: i32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(20, HEADER_V5_SIZE as i32);
        put(304, script);
        put(308, stream);
        put(312, 30);
        put(332, teams);
        put(336, team_stats);
        put(340, RECORD_SIZE as i32);
        put(344, 15);
        put(348, 1);
        h
    }

    #[test]
    fn reads_records_per_team() {
        let mut d = header(3, 5, 2, 2 * 4 + 3 * 80);
        d.extend([0u8; 3 + 5 + 1]); // script, stream, one winning ally team
        d.extend(2i32.to_le_bytes());
        d.extend(1i32.to_le_bytes());
        for (frame, produced, died) in [(0, 1, 0), (450, 9, 2), (450, 4, 4)] {
            let mut rec = [0u8; 80];
            rec[0..4].copy_from_slice(&i32::to_le_bytes(frame));
            rec[12..16].copy_from_slice(&f32::to_le_bytes(123.5));
            rec[52..56].copy_from_slice(&i32::to_le_bytes(produced));
            rec[56..60].copy_from_slice(&i32::to_le_bytes(died));
            d.extend(rec);
        }
        let s = StatsHistory::from_bytes(&d).unwrap();
        assert_eq!((s.period_s, s.game_time_s, s.teams.len()), (15, 30, 2));
        assert_eq!(s.teams[0].len(), 2);
        assert_eq!(s.teams[0][1].frame, 450);
        assert_eq!(s.teams[0][1].metal_produced, 123.5);
        assert_eq!(s.teams[0][1].units_alive(), 7);
        assert_eq!(s.teams[1][0].units_alive(), 0);
        assert!(!s.has_history());
    }

    #[test]
    fn refuses_a_truncated_block() {
        let mut d = header(0, 0, 1, 4 + 80);
        d.push(0);
        d.extend(1i32.to_le_bytes());
        d.extend([0u8; 40]);
        assert!(StatsHistory::from_bytes(&d).is_err());
    }

    fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut p = 0f32.to_le_bytes().to_vec();
        p.extend((body.len() as u32 + 1).to_le_bytes());
        p.push(kind);
        p.extend(body);
        p
    }

    #[test]
    fn stream_gives_team_colours_and_deaths_with_their_frame() {
        let mut lua = vec![0u8; 6];
        lua.extend(br#"AutoColors[{"teamID":0,"b":243,"g":62,"r":11},{"teamID":1,"b":5,"g":16,"r":255}]"#);
        let mut s = packet(PKT_LUA_MSG, &lua);
        s.extend(packet(PKT_KEYFRAME, &100i32.to_le_bytes()));
        s.extend(packet(PKT_NEW_FRAME, &[]));
        s.extend(packet(PKT_NEW_FRAME, &[]));
        s.extend(packet(PKT_TEAM_MSG, &[0, TEAM_DIED, 1]));
        s.extend(packet(PKT_TEAM_MSG, &[0, TEAM_DIED, 1]));
        s.extend(packet(PKT_TEAM_MSG, &[3, 2, 0])); // a resignation: not a death
        let f = StreamFacts::scan(&s);
        assert_eq!(f.team_colors.get(&0), Some(&[11, 62, 243]));
        assert_eq!(f.team_colors.get(&1), Some(&[255, 16, 5]));
        assert_eq!(f.team_deaths, vec![(102, 1)]);
    }

    #[test]
    fn server_recording_has_no_records() {
        let mut d = header(0, 0, 2, 0);
        d.push(0);
        let s = StatsHistory::from_bytes(&d).unwrap();
        assert_eq!(s.teams, vec![Vec::new(), Vec::new()]);
        assert!(!s.has_history());
    }
}
