//! The timeline builder: a streaming reducer from the stats service's record
//! stream (bar-replays DESIGN §9.8) to barwatch's timeline document (DESIGN §4.1).
//!
//! Feed it the stream as it arrives, in any pieces: [`TimelineBuilder::feed`]
//! takes bytes and keeps an unfinished last line for the next call. The document
//! ([`TimelineBuilder::document`]) is a pure function of the lines fed so far, so
//! how they were split makes no difference, `rev` included: it counts the lines
//! that changed the document. Lines it does not use are skipped, `_cursor` lines
//! move `frames` on and end the stream, and nothing a line says can make it panic
//! or hold more than the game's size in memory (see the limits below).
//!
//! # Series
//!
//! Sample `i` of every series is the value at frame `i × 450`. Each record is put
//! in a slot by its frame:
//! - within [`SNAP_FRAMES`] of a multiple of 450, in that multiple's slot. Gex
//!   stamps its periodic records with the frame they describe, the engine's
//!   history its own frame, and both may stand one frame off the other (Gex's
//!   `GameFrame` runs a frame behind the engine's count at the end of a game);
//! - anywhere else, in the next slot after it. Only the end of a game makes such
//!   records (Gex's and the engine's final sample at game over), and after the
//!   end nothing changes any more, so the next slot's value is that one.
//!
//! A later record for a slot replaces an earlier one. A slot a series skipped is
//! filled with the value before it (0 at the start). A series is as long as the
//! data it has had, so lengths differ between metrics and teams while the stream
//! runs; a metric is listed in `metrics` once it has a value.
//!
//! The engine's own counters (`team_stats`) are written by Gex only at game over,
//! all at once: until then a stream has only Gex's tally and the companion's
//! resources.
//!
//! # Marks
//!
//! See [`rules`] for which unit makes which mark. A mark's frame is the frame
//! Gex stamped on the record that made it. Kinds marked once per team are kept
//! the first time; the "every time" kinds are told apart by frame and unit, so a
//! stretch of the stream fed twice (a client resuming from an older cursor)
//! changes nothing.

mod metrics;
mod records;
mod rules;
#[cfg(test)]
mod tests;

pub use metrics::Metric;
pub use records::MAX_FRAME;
pub use rules::Milestone;

use crate::demo::Demo;
use crate::stats::StreamFacts;
use records::Record;
use rules::DefFacts;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The document format's version.
pub const DOC_VERSION: u32 = 1;
/// Frames between two samples: the engine's statistics period, 15 s of game.
pub const PERIOD_FRAMES: u32 = 450;
/// How far from a multiple of the period a record may be stamped and still count
/// as that sample.
pub const SNAP_FRAMES: u32 = 2;
/// The record types the builder uses, for the stream's `actions=` parameter.
pub const ACTIONS: &[&str] = &[
    "team_stats",
    "extra_stat_update",
    "bs_team_resources",
    "unit_def",
    "bs_unit_def",
    "unit_created",
    "unit_finished",
    "unit_killed",
    "team_died",
];

/// Longest line read; a longer one is skipped whole. Real lines are under 2 KiB.
pub const MAX_LINE: usize = 1 << 20;
/// Most unit definitions kept (a game has about 850).
const MAX_DEFS: usize = 1 << 13;
/// Most units tracked at once. The engine's unit ids stay below its unit limit,
/// 32000 by default.
const MAX_UNITS: usize = 1 << 17;
/// Most marks of one "every time" kind per team.
const MAX_EVERY: usize = 256;
/// Slots a series can have: [`MAX_FRAME`] in samples, plus the one after.
const MAX_SLOTS: usize = (MAX_FRAME / PERIOD_FRAMES) as usize + 2;

/// barwatch's timeline document (DESIGN §4.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineDoc {
    pub v: u32,
    /// Grows whenever the document changes.
    pub rev: u64,
    /// The stream has ended: nothing more will come.
    pub complete: bool,
    pub period_frames: u32,
    /// How far the simulation has got: the latest frame any line named.
    pub frames: u32,
    pub teams: Vec<TeamEntry>,
    /// The metrics that have values, in a fixed order.
    pub metrics: Vec<MetricEntry>,
    /// Per metric key, one list of samples per entry of `teams`, in that order.
    pub series: BTreeMap<String, Vec<Vec<f64>>>,
    /// In the order they happened.
    pub marks: Vec<Mark>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamEntry {
    /// The engine's team number, which records and marks use.
    pub team: u32,
    pub ally: u32,
    pub name: String,
    pub is_ai: bool,
    /// `armada`, `cortex`, `legion`, or the lobby's word when it named none of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// The colour BAR gave the team, `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    /// A running total since the start; rates are derived from it.
    Cumulative,
    /// The value at that moment.
    Level,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricEntry {
    pub key: String,
    pub kind: MetricKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub frame: u32,
    /// The engine's team number.
    pub team: u32,
    /// A key of BAR's milestones ([`Milestone::key`]).
    pub kind: String,
    /// The unit definition that made it, by name (`armfus`); none for `teamDied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub def: Option<String>,
}

/// What the builder made of its input, for logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedStats {
    /// Non-empty lines read.
    pub lines: u64,
    /// Lines that were not JSON objects with an action, lacked what their type
    /// needs, or were too long.
    pub malformed: u64,
    /// Records about a team the replay does not name, or past a limit.
    pub dropped: u64,
}

#[derive(Clone, Debug)]
struct MarkRec {
    frame: u32,
    team: usize,
    kind: Milestone,
    def: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct LiveUnit {
    def: u32,
    finished: bool,
}

/// The streaming reducer. See the module documentation.
#[derive(Clone, Debug)]
pub struct TimelineBuilder {
    teams: Vec<TeamEntry>,
    /// Team number → index into `teams`.
    index: HashMap<u32, usize>,
    /// Whether a team's side was read off its commander yet.
    side_seen: Vec<bool>,
    rev: u64,
    complete: bool,
    frames: u32,
    /// `[metric][team][slot]`.
    series: Vec<Vec<Vec<f64>>>,
    marks: Vec<MarkRec>,
    reached: HashSet<(usize, Milestone)>,
    /// "Every time" marks made, by kind, team, frame and unit.
    every: HashSet<(Milestone, usize, u32, u32)>,
    every_count: HashMap<(Milestone, usize), usize>,
    defs: HashMap<u32, DefFacts>,
    units: HashMap<u32, LiveUnit>,
    /// The unfinished last line of the input so far.
    partial: Vec<u8>,
    /// The current line has outgrown [`MAX_LINE`]: skip to its end.
    overlong: bool,
    stats: FeedStats,
}

impl TimelineBuilder {
    /// A builder for a game with these teams (from [`teams_from_replay`]). Records
    /// about any other team are dropped.
    pub fn new(teams: Vec<TeamEntry>) -> TimelineBuilder {
        let mut index = HashMap::new();
        let mut kept = Vec::new();
        for t in teams {
            if let std::collections::hash_map::Entry::Vacant(e) = index.entry(t.team) {
                e.insert(kept.len());
                kept.push(t);
            }
        }
        let n = kept.len();
        TimelineBuilder {
            side_seen: vec![false; n],
            teams: kept,
            index,
            rev: 0,
            complete: false,
            frames: 0,
            series: vec![vec![Vec::new(); n]; Metric::ALL.len()],
            marks: Vec::new(),
            reached: HashSet::new(),
            every: HashSet::new(),
            every_count: HashMap::new(),
            defs: HashMap::new(),
            units: HashMap::new(),
            partial: Vec::new(),
            overlong: false,
            stats: FeedStats::default(),
        }
    }

    /// Takes the next piece of the stream, cut anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
            let (piece, tail) = (&rest[..nl], &rest[nl + 1..]);
            rest = tail;
            if self.overlong || self.partial.len() + piece.len() > MAX_LINE {
                self.stats.malformed += 1;
            } else if self.partial.is_empty() {
                self.line(piece);
            } else {
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(piece);
                self.line(&line);
            }
            self.partial.clear();
            self.overlong = false;
        }
        if self.overlong || self.partial.len() + rest.len() > MAX_LINE {
            self.overlong = true;
            self.partial.clear();
        } else {
            self.partial.extend_from_slice(rest);
        }
    }

    /// Takes one whole line. Not to be mixed with a [`feed`](Self::feed) that left
    /// a line unfinished.
    pub fn feed_line(&mut self, line: &str) {
        if line.len() > MAX_LINE {
            self.stats.malformed += 1;
        } else {
            self.line(line.as_bytes());
        }
    }

    /// The input is over and was the whole game: reads an unfinished last line and
    /// marks the document complete. A stream that breaks off is not finished.
    pub fn finish(&mut self) {
        if self.overlong {
            self.stats.malformed += 1;
        } else if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
        }
        self.partial.clear();
        self.overlong = false;
        if !self.complete {
            self.complete = true;
            self.rev += 1;
        }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn frames(&self) -> u32 {
        self.frames
    }

    pub fn stats(&self) -> FeedStats {
        self.stats
    }

    /// The document as it stands.
    pub fn document(&self) -> TimelineDoc {
        let present: Vec<Metric> =
            Metric::ALL.iter().copied().filter(|m| self.series[*m as usize].iter().any(|s| !s.is_empty())).collect();
        TimelineDoc {
            v: DOC_VERSION,
            rev: self.rev,
            complete: self.complete,
            period_frames: PERIOD_FRAMES,
            frames: self.frames,
            teams: self.teams.clone(),
            metrics: present.iter().map(|m| MetricEntry { key: m.key().to_owned(), kind: m.kind() }).collect(),
            series: present.iter().map(|m| (m.key().to_owned(), self.series[*m as usize].clone())).collect(),
            marks: self
                .marks
                .iter()
                .map(|m| Mark { frame: m.frame, team: self.teams[m.team].team, kind: m.kind.key().to_owned(), def: m.def.clone() })
                .collect(),
        }
    }

    fn line(&mut self, raw: &[u8]) {
        let raw = raw.trim_ascii();
        if raw.is_empty() {
            return;
        }
        self.stats.lines += 1;
        let Some(record) = serde_json::from_slice::<serde_json::Value>(raw).ok().as_ref().and_then(records::read) else {
            self.stats.malformed += 1;
            return;
        };
        if self.apply(record) {
            self.rev += 1;
        }
    }

    /// Applies one record; true if the document changed.
    fn apply(&mut self, record: Record) -> bool {
        let frame = match &record {
            Record::Cursor { frame, .. } | Record::Other { frame } => *frame,
            Record::Sample { frame, .. }
            | Record::UnitCreated { frame, .. }
            | Record::UnitFinished { frame, .. }
            | Record::UnitKilled { frame, .. }
            | Record::TeamDied { frame, .. } => Some(*frame),
            Record::UnitDef { .. } | Record::BsUnitDef { .. } => None,
        };
        let mut changed = false;
        if let Some(f) = frame.filter(|f| *f > self.frames) {
            self.frames = f;
            changed = true;
        }
        match record {
            Record::Cursor { complete, .. } => {
                if complete && !self.complete {
                    self.complete = true;
                    changed = true;
                }
            }
            Record::Other { .. } => {}
            Record::Sample { frame, team, values } => {
                let Some(t) = self.team(team) else { return changed };
                let slot = slot_of(frame);
                for (m, v) in values {
                    changed |= self.set_sample(m, t, slot, v);
                }
            }
            Record::UnitDef { def, name, facts } => {
                if let Some(d) = self.def_mut(def) {
                    d.name = name.or(d.name.take());
                    d.gex = Some(facts);
                }
            }
            Record::BsUnitDef { def, name, facts } => {
                if let Some(d) = self.def_mut(def) {
                    d.name = d.name.take().or(name);
                    d.bs = Some(facts);
                }
            }
            Record::UnitCreated { unit, team, def, .. } => {
                if self.units.len() < MAX_UNITS || self.units.contains_key(&unit) {
                    self.units.insert(unit, LiveUnit { def, finished: false });
                } else {
                    self.stats.dropped += 1;
                }
                changed |= self.side_from_commander(team, def);
            }
            Record::UnitFinished { frame, unit, team } => {
                let Some(live) = self.units.get_mut(&unit) else { return changed };
                live.finished = true;
                let def = live.def;
                let Some(t) = self.team(team) else { return changed };
                let Some(facts) = self.defs.get(&def) else { return changed };
                let kinds: Vec<Milestone> = facts.built().collect();
                let name = facts.name.clone();
                for k in kinds {
                    changed |= self.mark(t, k, frame, name.clone(), unit);
                }
            }
            Record::UnitKilled { frame, unit, team, def, def_name, attacker_team } => {
                let live = self.units.remove(&unit);
                let def = def.or(live.map(|l| l.def));
                let facts = def.and_then(|d| self.defs.get(&d));
                let name = facts.and_then(|f| f.name.clone()).or(def_name);
                let commander = facts.and_then(DefFacts::is_commander).unwrap_or(false);
                // BAR counts no kill of a unit worth nothing. One that was never
                // finished is worth what was put into it, which the stream does not
                // say: it counts here unless its kind is worth nothing at all.
                let worth = facts.and_then(DefFacts::cost).is_none_or(|c| c != 0.0);
                let victim = self.team(team);
                if let Some(v) = victim {
                    changed |= self.mark(v, Milestone::FirstLoss, frame, name.clone(), unit);
                    if commander {
                        changed |= self.mark(v, Milestone::CommanderLost, frame, name.clone(), unit);
                    }
                }
                // The killer: another team, on another side. A unit of a team the
                // replay does not name (Gaia's) is on nobody's side.
                let Some(k) = attacker_team.filter(|a| *a != team).and_then(|a| self.team(a)) else { return changed };
                let enemy = victim.is_none_or(|v| self.teams[v].ally != self.teams[k].ally);
                if enemy && worth {
                    changed |= self.mark(k, Milestone::FirstKill, frame, name.clone(), unit);
                    if commander {
                        changed |= self.mark(k, Milestone::CommanderKill, frame, name, unit);
                    }
                }
            }
            Record::TeamDied { frame, team } => {
                if let Some(t) = self.team(team) {
                    changed |= self.mark(t, Milestone::TeamDied, frame, None, u32::MAX);
                }
            }
        }
        changed
    }

    fn team(&mut self, team: u32) -> Option<usize> {
        let t = self.index.get(&team).copied();
        if t.is_none() {
            self.stats.dropped += 1;
        }
        t
    }

    fn def_mut(&mut self, def: u32) -> Option<&mut DefFacts> {
        if self.defs.len() >= MAX_DEFS && !self.defs.contains_key(&def) {
            self.stats.dropped += 1;
            return None;
        }
        Some(self.defs.entry(def).or_default())
    }

    /// The lobby's side may say Armada for a team that plays Legion (or Random);
    /// the first commander a team gets says what it really plays.
    fn side_from_commander(&mut self, team: u32, def: u32) -> bool {
        let Some(&t) = self.index.get(&team) else { return false };
        if self.side_seen[t] {
            return false;
        }
        let Some(facts) = self.defs.get(&def) else { return false };
        if facts.is_commander() != Some(true) {
            return false;
        }
        self.side_seen[t] = true;
        let Some(side) = facts.name.as_deref().and_then(rules::faction_of) else { return false };
        if self.teams[t].side.as_deref() == Some(side) {
            return false;
        }
        self.teams[t].side = Some(side.to_owned());
        true
    }

    fn set_sample(&mut self, m: Metric, team: usize, slot: usize, v: f64) -> bool {
        if slot >= MAX_SLOTS {
            self.stats.dropped += 1;
            return false;
        }
        let s = &mut self.series[m as usize][team];
        if let Some(old) = s.get_mut(slot) {
            if *old == v {
                return false;
            }
            *old = v;
        } else {
            let fill = s.last().copied().unwrap_or(0.0);
            s.resize(slot, fill);
            s.push(v);
        }
        true
    }

    fn mark(&mut self, team: usize, kind: Milestone, frame: u32, def: Option<String>, unit: u32) -> bool {
        if kind.every() {
            let count = self.every_count.entry((kind, team)).or_default();
            if *count >= MAX_EVERY || !self.every.insert((kind, team, frame, unit)) {
                return false;
            }
            *count += 1;
        } else if !self.reached.insert((team, kind)) {
            return false;
        }
        self.marks.push(MarkRec { frame, team, kind, def });
        true
    }
}

/// The sample slot a record stamped `frame` belongs in (module documentation):
/// the multiple of the period at or up to [`SNAP_FRAMES`] before it, else the
/// next one. A record a frame or two short of a multiple lands in that multiple
/// either way.
pub fn slot_of(frame: u32) -> usize {
    let (q, r) = (frame / PERIOD_FRAMES, frame % PERIOD_FRAMES);
    q as usize + usize::from(r > SNAP_FRAMES)
}

/// The teams of a replay as the document lists them: every team of the setup,
/// with the colour BAR showed for it (its `AutoColors` message, else the setup's
/// own colour) and the lobby's faction.
pub fn teams_from_replay(demo: &Demo, facts: &StreamFacts) -> Vec<TeamEntry> {
    demo.team_list()
        .into_iter()
        .map(|info| {
            let color = facts.team_colors.get(&info.team).copied().or_else(|| setup_color(demo, info.team));
            TeamEntry {
                team: info.team,
                ally: info.ally_team,
                name: info.name,
                is_ai: info.is_ai,
                side: info.faction.map(|f| f.to_ascii_lowercase()).filter(|f| !f.is_empty()),
                color: color.map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")),
            }
        })
        .collect()
}

/// `rgbcolor=0.1 0.2 0.9` of a team's setup section.
fn setup_color(demo: &Demo, team: u32) -> Option<[u8; 3]> {
    let text = demo.script.get("game")?.get(format!("team{team}"))?.get("rgbcolor")?.as_str()?;
    let parts: Vec<f32> = text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let [r, g, b] = parts[..] else { return None };
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some([c(r), c(g), c(b)])
}
