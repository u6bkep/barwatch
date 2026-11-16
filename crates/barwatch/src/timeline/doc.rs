//! The timeline document a server sends ([`TimelineDoc`], DESIGN.md §4.1) made
//! into the [`Data`] the view draws.
//!
//! The document is whatever the server's bytes parsed into: nothing here trusts
//! it beyond its types. Counts are capped, numbers made finite, references to
//! teams and kinds checked, and anything this app does not know is left out.
//!
//! What a document offers changes while a game is being simulated. A stats
//! service with the companion widget sends everything from the first sample; an
//! older one sends only its own tally (unit values, storage, build power) until
//! the simulation ends, and the engine's counters all at once then. The metric
//! list here is what the document has at this moment, in the catalog's order.

use super::data::{self, Data, MetricDef, Series, Team, Unit, CATALOG};
use super::marks::{Faction, MarkKind, Marker};
use super::Feed;
use barwatch_core::api::{TimelineState, TimelineView};
use barwatch_core::stats::GAME_SPEED;
use barwatch_core::timeline::{MetricKind, TimelineDoc, DOC_VERSION, MAX_FRAME, PERIOD_FRAMES, SNAP_FRAMES};
use damascene_core::prelude::Color;

/// Most teams taken from a document: BAR's largest games have 32 players, a
/// free-for-all 16 sides.
const MAX_TEAMS: usize = 64;
/// Most samples a series is drawn with: twelve hours at the usual period come
/// to 2880. With the cap on teams this bounds what a small document that
/// claims a long game can make the app allocate.
const MAX_SAMPLES: usize = 4096;
/// Most markers taken from a document; a long 8v8 has a few hundred.
const MAX_MARKS: usize = 4096;
/// Longest team name shown, in characters.
const MAX_NAME: usize = 48;
/// No statistic of a real game comes near this; larger values are cut to it so
/// that sums over teams and samples stay finite.
const MAX_VALUE: f64 = 1e12;

/// How a metric's values come out of a document.
#[derive(Clone, Copy)]
enum Recipe {
    /// The series under the metric's own key, as it is.
    Direct(MetricKind),
    /// The level under the metric's own key when the document has it (the
    /// companion widget's reading), else the rate of this cumulative counter.
    LevelOrRate(&'static str),
    /// Per `per` seconds over the last `window` samples of a cumulative counter.
    Rate { of: &'static str, window: usize, per: f32 },
    /// Units produced, received and captured, less those lost, given away and
    /// captured by the enemy: only when the document has all six counters.
    UnitsAlive,
}

fn recipe(def: &MetricDef) -> Recipe {
    match def.id {
        "metalIncome" => Recipe::LevelOrRate("metalProduced"),
        "energyIncome" => Recipe::LevelOrRate("energyProduced"),
        "metalExpense" => Recipe::LevelOrRate("metalUsed"),
        "energyExpense" => Recipe::LevelOrRate("energyUsed"),
        // Over three samples: single volleys make it unreadable.
        "damageRate" => Recipe::Rate { of: "damageDealt", window: 3, per: 1.0 },
        "actionsPerMinute" => Recipe::Rate { of: "actions", window: 4, per: 60.0 },
        "unitsActive" => Recipe::UnitsAlive,
        "unitValue" | "valueArmy" | "valueDefense" | "valueUtility" | "valueEconomy" | "metalStored" | "energyStored"
        | "buildPower" | "buildPowerUse" | "metalLevel" | "energyLevel" => Recipe::Direct(MetricKind::Level),
        _ => Recipe::Direct(MetricKind::Cumulative),
    }
}

const UNITS_IN: [&str; 3] = ["unitsProduced", "unitsReceived", "unitsCaptured"];
const UNITS_OUT: [&str; 3] = ["unitsDied", "unitsSent", "unitsOutCaptured"];

/// Reads a document's series by key, padded to one length.
struct Reader<'a> {
    doc: &'a TimelineDoc,
    teams: usize,
    n: usize,
}

impl Reader<'_> {
    /// The series under `key`, if the document lists it as `kind` and has it:
    /// `[team][sample]`, every team `n` long. A team's series that stops short
    /// (it left the game) carries its last total on, or drops to nothing for
    /// a level.
    fn series(&self, key: &str, kind: MetricKind) -> Option<Vec<Vec<f32>>> {
        if !self.doc.metrics.iter().any(|m| m.key == key && m.kind == kind) {
            return None;
        }
        let stored = self.doc.series.get(key)?;
        let mut out = Vec::with_capacity(self.teams);
        for team in 0..self.teams {
            let mut values: Vec<f32> = stored
                .get(team)
                .map(|v| {
                    v.iter()
                        .take(self.n)
                        .map(|x| if x.is_finite() { x.clamp(-MAX_VALUE, MAX_VALUE) as f32 } else { 0.0 })
                        .collect()
                })
                .unwrap_or_default();
            let fill = match kind {
                MetricKind::Cumulative => values.last().copied().unwrap_or(0.0),
                MetricKind::Level => 0.0,
            };
            values.resize(self.n, fill);
            out.push(values);
        }
        Some(out)
    }

    fn rate(&self, of: &str, window: usize, per: f32, period_s: f32) -> Option<Vec<Vec<f32>>> {
        let totals = self.series(of, MetricKind::Cumulative)?;
        Some(
            totals
                .iter()
                .map(|c| {
                    (0..self.n)
                        .map(|i| {
                            let lo = i.saturating_sub(window);
                            if i == lo {
                                0.0
                            } else {
                                ((c[i] - c[lo]) / (period_s * (i - lo) as f32) * per).max(0.0)
                            }
                        })
                        .collect()
                })
                .collect(),
        )
    }

    fn values(&self, def: &MetricDef, period_s: f32) -> Option<Vec<Vec<f32>>> {
        let mut values = match recipe(def) {
            Recipe::Direct(kind) => self.series(def.id, kind)?,
            Recipe::LevelOrRate(of) => self.series(def.id, MetricKind::Level).or_else(|| self.rate(of, 1, 1.0, period_s))?,
            Recipe::Rate { of, window, per } => self.rate(of, window, per, period_s)?,
            Recipe::UnitsAlive => {
                let read = |keys: [&str; 3]| -> Option<Vec<Vec<Vec<f32>>>> {
                    keys.iter().map(|k| self.series(k, MetricKind::Cumulative)).collect()
                };
                let (plus, minus) = (read(UNITS_IN)?, read(UNITS_OUT)?);
                (0..self.teams)
                    .map(|t| {
                        (0..self.n)
                            .map(|i| {
                                let sum = |parts: &[Vec<Vec<f32>>]| parts.iter().map(|p| p[t][i]).sum::<f32>();
                                (sum(&plus) - sum(&minus)).max(0.0)
                            })
                            .collect()
                    })
                    .collect()
            }
        };
        for team in &mut values {
            for v in team.iter_mut() {
                *v = match def.unit {
                    Unit::Percent => v.clamp(0.0, 1.0),
                    _ => v.max(0.0),
                };
            }
        }
        Some(values)
    }
}

/// `#rrggbb`.
fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// A name a server sent, fit to show: no control characters, not endless.
fn display_name(name: &str, team: u32) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    if clean.trim().is_empty() {
        format!("Team {}", team + 1)
    } else {
        clean
    }
}

/// A unit definition's name, as the game writes them; anything else is dropped.
fn def_name(def: &str) -> Option<String> {
    let plain = !def.is_empty() && def.len() <= 64 && def.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    plain.then(|| def.to_owned())
}

/// Longest note of a server shown, in characters.
const MAX_NOTE: usize = 200;

/// What the timeline call answered, as the timeline takes it
/// ([`super::Timeline::set_statistics`]): where the statistics stand, and the
/// statistics when the answer has some this app can read.
pub fn statistics(view: &TimelineView, replay_s: u32) -> (Feed, Option<Data>) {
    match &view.doc {
        // A format from a later barwatch: its numbers may mean something else.
        Some(doc) if doc.v > DOC_VERSION => {
            (Feed::Unavailable("This server's statistics are in a newer format: update barwatch to see them.".into()), None)
        }
        Some(doc) if doc.v != DOC_VERSION => {
            (Feed::Unavailable("This server's statistics are in an older format this barwatch no longer reads.".into()), None)
        }
        Some(doc) => (feed(view), Some(data_from_doc(doc, replay_s))),
        None if view.state == TimelineState::Complete => (Feed::Unavailable("The server sent no statistics.".into()), None),
        None => (feed(view), None),
    }
}

/// Where the statistics stand, as the answer's state and note say.
pub fn feed(view: &TimelineView) -> Feed {
    let note = |otherwise: &str| {
        let said: String = view.note.as_deref().unwrap_or("").chars().filter(|c| !c.is_control()).take(MAX_NOTE).collect();
        if said.trim().is_empty() {
            otherwise.to_owned()
        } else {
            said.trim().to_owned()
        }
    };
    match view.state {
        TimelineState::Unavailable => Feed::Unavailable(note("This server has no stats service to ask.")),
        TimelineState::Queued => Feed::Queued,
        TimelineState::Building => Feed::Building,
        TimelineState::Complete => Feed::Complete,
        TimelineState::Failed => Feed::Failed(note("The stats service could not process this replay.")),
    }
}

/// Makes `doc` into what the view draws. `replay_s` is the game's length as the
/// replay's header gives it (0 when unknown): the axis is that long while the
/// document is still growing, and as long as the document says once it is
/// complete.
pub fn data_from_doc(doc: &TimelineDoc, replay_s: u32) -> Data {
    let period = if (1..=MAX_FRAME).contains(&doc.period_frames) { doc.period_frames } else { PERIOD_FRAMES };
    let period_s = period as f32 / GAME_SPEED;
    let frames = doc.frames.min(MAX_FRAME);
    let frames_s = frames as f32 / GAME_SPEED;
    let duration_s = if doc.complete && frames > 0 { frames_s } else { (replay_s as f32).max(frames_s) }.max(period_s);

    let entries = &doc.teams[..doc.teams.len().min(MAX_TEAMS)];
    let mut allies: Vec<u32> = entries.iter().map(|t| t.ally).collect();
    allies.sort_unstable();
    allies.dedup();
    let teams: Vec<Team> = entries
        .iter()
        .enumerate()
        .map(|(k, entry)| {
            let side = allies.iter().position(|a| *a == entry.ally).unwrap_or(0);
            let color = entry.color.as_deref().and_then(parse_color).map(data::display_color).unwrap_or_else(|| {
                let (r, g, b) = data::FALLBACK[(side + k * 2) % data::FALLBACK.len()];
                Color::srgb_u8(r, g, b)
            });
            // The side as the builder corrected it from the team's first
            // commander: the lobby's word is "random" for most AIs.
            let faction = Faction::from_side(entry.side.as_deref());
            Team { name: display_name(&entry.name, entry.team), color, faction, is_ai: entry.is_ai, side }
        })
        .collect();

    // Samples every series has. While the stream runs, a slot is whole once a
    // frame past its snap window has been named: records come in frame order.
    // A finished game also has the sample taken at its end, in the slot after.
    let limit = ((MAX_FRAME / period) as usize + 2).min(MAX_SAMPLES);
    let n = if doc.complete {
        let longest = doc.series.values().flat_map(|per_team| per_team.iter().map(Vec::len)).max().unwrap_or(0);
        longest.max((frames / period) as usize + 1)
    } else {
        (frames.saturating_sub(SNAP_FRAMES + 1) / period) as usize + 1
    }
    .clamp(1, limit);

    let reader = Reader { doc, teams: teams.len(), n };
    let metrics: Vec<Series> =
        CATALOG.iter().filter_map(|def| Some(Series { def: *def, values: reader.values(def, period_s)? })).collect();

    let markers: Vec<Marker> = doc
        .marks
        .iter()
        .take(MAX_MARKS)
        .filter_map(|mark| {
            let team = entries.iter().position(|t| t.team == mark.team)?;
            let kind = MarkKind::from_key(&mark.kind)?;
            let unit = mark.def.as_deref().and_then(def_name);
            Some(Marker { t: mark.frame.min(MAX_FRAME) as f32 / GAME_SPEED, kind, team, unit, note: None, sample: false })
        })
        .collect();

    let has_metrics = !metrics.is_empty();
    let mut data = Data::assemble(duration_s, period_s, n, teams, metrics, markers);
    data.computed_s = if doc.complete { duration_s } else { frames_s.min(duration_s) };
    if !has_metrics {
        // Nothing to read yet: the view shows no numbers anywhere.
        data.computed_s = 0.0;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::docs;
    use barwatch_core::timeline::{Mark, MetricEntry, TeamEntry};

    fn ids(data: &Data) -> Vec<&'static str> {
        data.metrics.iter().map(|m| m.def.id).collect()
    }

    fn values<'a>(data: &'a Data, id: &str, team: usize) -> &'a [f32] {
        data.team_values(data.metric_index(id).unwrap(), team)
    }

    #[test]
    fn a_finished_game_with_the_companion_offers_everything() {
        let doc = docs::duel();
        let data = data_from_doc(&doc, 1277);
        // The game is as long as the simulation got, not as the header says.
        assert_eq!(data.duration_s, 38155.0 / 30.0);
        assert_eq!(data.computed_s, data.duration_s);
        assert_eq!(data.n, 86);
        // The last sample was taken at the end, not a full period after the one before.
        assert_eq!(data.sample_t(84), 1260.0);
        assert_eq!(data.sample_t(85), data.duration_s);
        assert_eq!(data.sample_index(data.duration_s), 85);
        assert_eq!(data.sample_index(1265.0), 84);
        // In the catalog's order, the derived ones among them.
        let ids = ids(&data);
        assert_eq!(&ids[..4], ["metalIncome", "energyIncome", "metalExpense", "energyExpense"]);
        for id in ["unitsActive", "damageRate", "actionsPerMinute", "metalLevel", "valueArmy", "metalProduced"] {
            assert!(ids.contains(&id), "{id}");
        }
        assert_eq!(ids.len(), CATALOG.len(), "this document has every series the app knows");
        assert_eq!(data.lead, data::Lead::ArmyValue);
        assert_eq!(data.teams.len(), 2);
        assert_eq!((data.teams[0].faction, data.teams[1].faction), (Faction::Cortex, Faction::Cortex));
        assert_eq!(data.markers.len(), doc.marks.len());
        assert_eq!(data.markers[0].unit.as_deref(), Some("corfav"));
    }

    #[test]
    fn income_is_the_games_own_reading_when_there_is_one_and_a_rate_otherwise() {
        let doc = docs::duel();
        let with = data_from_doc(&doc, 1277);
        let direct = doc.series["metalIncome"][0][40] as f32;
        assert_eq!(values(&with, "metalIncome", 0)[40], direct);
        // An older stats service: no companion, the engine's counters at the end.
        let without = data_from_doc(&docs::older_service(&doc), 1277);
        let produced = &doc.series["metalProduced"][0];
        let rate = ((produced[40] - produced[39]) / 15.0) as f32;
        assert!((values(&without, "metalIncome", 0)[40] - rate).abs() < 1e-3);
        assert!(without.metric_index("metalLevel").is_none());
        assert_eq!(values(&without, "metalIncome", 0)[0], 0.0, "no rate before the second sample");
    }

    #[test]
    fn units_alive_adds_up_and_ratios_are_shares() {
        let doc = docs::duel();
        let data = data_from_doc(&doc, 1277);
        let at = |key: &str| doc.series[key][0][50];
        let alive = at("unitsProduced") + at("unitsReceived") + at("unitsCaptured")
            - at("unitsDied")
            - at("unitsSent")
            - at("unitsOutCaptured");
        assert_eq!(values(&data, "unitsActive", 0)[50], alive as f32);
        let use_ = data.metric_index("buildPowerUse").unwrap();
        assert_eq!(data.metrics[use_].def.unit, Unit::Percent);
        assert!(values(&data, "buildPowerUse", 0).iter().all(|v| (0.0..=1.0).contains(v)));
        assert_eq!(data.metrics[use_].def.show(0.62), "62%");
        // A side's share is its players' mean, not their sum.
        let teams = data_from_doc(&docs::teams(), 2852);
        let side = teams.sides.iter().position(|s| s.teams.len() > 1).unwrap();
        let m = teams.metric_index("buildPowerUse").unwrap();
        assert!(teams.side_values(m, side).iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn during_a_simulation_by_an_older_service_only_its_own_tally_shows() {
        let doc = docs::older_service(&docs::prefix(&docs::duel(), 18_000));
        assert!(!doc.complete);
        let data = data_from_doc(&doc, 1277);
        assert_eq!(data.duration_s, 1277.0, "the header's length while the document grows");
        assert_eq!(data.computed_s, 600.0);
        assert_eq!(data.n, 40, "the slot at 18000 is not whole until a later frame is named");
        assert_eq!(
            ids(&data),
            [
                "metalStored",
                "energyStored",
                "valueArmy",
                "unitValue",
                "valueDefense",
                "valueEconomy",
                "valueUtility",
                "buildPowerUse",
                "buildPower",
                "actionsPerMinute"
            ]
        );
        assert_eq!(data.lead, data::Lead::ArmyValue);
        // Marks past what is simulated are not there yet.
        assert!(data.markers.iter().all(|m| m.t <= 600.0));
        // Three frames on, the slot at 18000 counts.
        let mut later = doc.clone();
        later.frames = 18_003;
        assert_eq!(data_from_doc(&later, 1277).n, 41);
    }

    #[test]
    fn a_team_that_stops_reporting_keeps_its_totals_and_loses_its_levels() {
        let mut doc = docs::duel();
        for key in ["metalProduced", "valueArmy", "metalIncome"] {
            doc.series.get_mut(key).unwrap()[1].truncate(30);
        }
        let data = data_from_doc(&doc, 1277);
        let last_total = doc.series["metalProduced"][1][29] as f32;
        assert_eq!(values(&data, "metalProduced", 1)[60], last_total);
        assert_eq!(values(&data, "valueArmy", 1)[60], 0.0);
        assert_eq!(values(&data, "metalIncome", 1)[60], 0.0);
    }

    fn team(team: u32, ally: u32) -> TeamEntry {
        TeamEntry { team, ally, name: format!("p{team}"), is_ai: false, side: None, color: None }
    }

    #[test]
    fn nothing_a_document_says_breaks_the_view() {
        let entry = |key: &str, kind| MetricEntry { key: key.into(), kind };
        let mut doc = TimelineDoc {
            v: 9,
            rev: 1,
            complete: false,
            period_frames: 0,
            frames: u32::MAX,
            teams: vec![team(7, 3), team(2, 3), team(9, 1)],
            metrics: vec![
                entry("metalProduced", MetricKind::Cumulative),
                // Listed as the wrong kind: not taken.
                entry("valueArmy", MetricKind::Cumulative),
                // A key this app does not know.
                entry("moonPhase", MetricKind::Level),
                // Listed without a series.
                entry("energyProduced", MetricKind::Cumulative),
                entry("buildPowerUse", MetricKind::Level),
            ],
            series: Default::default(),
            marks: vec![
                Mark { frame: 900, team: 2, kind: "fusion".into(), def: Some("armfus".into()) },
                Mark { frame: 900, team: 5, kind: "fusion".into(), def: None },
                Mark { frame: 900, team: 2, kind: "nonsense".into(), def: None },
                Mark { frame: 900, team: 9, kind: "tech2".into(), def: Some("../../etc/passwd".into()) },
            ],
        };
        // One team's series only, with numbers no game has; series for more teams than exist.
        doc.series.insert("metalProduced".into(), vec![vec![0.0, 1e300, -5.0, f64::MAX]]);
        doc.series.insert("valueArmy".into(), vec![vec![1.0; 4]; 3]);
        doc.series.insert("moonPhase".into(), vec![vec![1.0; 4]; 3]);
        doc.series.insert("buildPowerUse".into(), vec![vec![7.0, -1.0]; 9]);
        let data = data_from_doc(&doc, 0);
        assert_eq!(data.period_s, 15.0);
        assert!(data.duration_s.is_finite() && data.n <= (MAX_FRAME / PERIOD_FRAMES) as usize + 2);
        // A tiny period would ask for a sample per frame of a twelve-hour game.
        let mut fine = doc.clone();
        fine.period_frames = 1;
        assert_eq!(data_from_doc(&fine, 0).n, MAX_SAMPLES);
        assert_eq!(ids(&data), ["metalIncome", "buildPowerUse", "metalProduced"]);
        assert_eq!(data.teams.iter().map(|t| t.side).collect::<Vec<_>>(), [1, 1, 0]);
        for m in &data.metrics {
            assert_eq!(m.values.len(), 3);
            assert!(m.values.iter().all(|v| v.len() == data.n && v.iter().all(|x| x.is_finite() && *x >= 0.0)));
        }
        assert_eq!(values(&data, "buildPowerUse", 0)[..2], [1.0, 0.0]);
        let kinds: Vec<_> = data.markers.iter().map(|m| (m.kind, m.team, m.unit.as_deref())).collect();
        assert_eq!(kinds, [(MarkKind::Fusion, 1, Some("armfus")), (MarkKind::Tech2, 2, None)]);
        // An empty document is one too.
        doc.teams.clear();
        doc.metrics.clear();
        doc.frames = 0;
        let empty = data_from_doc(&doc, 0);
        assert_eq!((empty.teams.len(), empty.sides.len(), empty.computed_s), (0, 0, 0.0));
    }

    #[test]
    fn names_and_colours_from_a_server_are_made_presentable() {
        assert_eq!(parse_color("#0b3ef3"), Some([11, 62, 243]));
        assert_eq!(parse_color("0b3ef3"), None);
        assert_eq!(parse_color("#0b3e"), None);
        assert_eq!(parse_color("#ÿÿÿ"), None);
        assert_eq!(display_name("  ", 2), "Team 3");
        assert_eq!(display_name("a\u{7}b\nc", 0), "abc");
        assert_eq!(display_name(&"x".repeat(500), 0).chars().count(), MAX_NAME);
    }
}
