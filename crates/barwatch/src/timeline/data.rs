//! What the timeline shows: series per team over game time, the teams and sides,
//! event markers, and what is derived from them (the lead, fight intensity).
//!
//! Today everything comes from the replay file (`barwatch_core::stats`), which
//! holds the engine's cumulative counters. A server will later supply richer
//! metrics and BAR's real milestones; both go into the same [`Data`], so the
//! view needs no change for them: a metric is a [`MetricDef`] plus values, a
//! milestone is a [`Marker`].
//!
//! Everything derived for sample `i` uses samples `0..=i` only, so drawing up to
//! the game position never hints at what comes after it.

// Sample loops index several parallel series at once.
#![allow(clippy::needless_range_loop)]

use super::marks::{Faction, MarkKind, Marker, ALL_KINDS};
use anyhow::{ensure, Result};
use barwatch_core::demo::Demo;
use barwatch_core::stats::{StatsHistory, TeamSample, GAME_SPEED};
use damascene_core::prelude::*;
use std::path::Path;

/// How a metric's values read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    /// Something per second, from a cumulative counter.
    PerSecond,
    /// Whole things.
    Count,
    /// An amount (metal, damage).
    Amount,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MetricDef {
    /// Stable name, as a server would send it.
    pub id: &'static str,
    /// Short label for a row.
    pub label: &'static str,
    /// What it is, in a sentence, for the hero chart and hover text.
    pub about: &'static str,
    pub unit: Unit,
}

impl MetricDef {
    pub fn show(&self, v: f32) -> String {
        super::axis::number(if self.unit == Unit::Count { v.round() } else { v })
    }
}

pub const METAL_INCOME: MetricDef =
    MetricDef { id: "metalIncome", label: "Metal /s", about: "Metal income per second", unit: Unit::PerSecond };
pub const ENERGY_INCOME: MetricDef =
    MetricDef { id: "energyIncome", label: "Energy /s", about: "Energy income per second", unit: Unit::PerSecond };
pub const METAL_SPENT: MetricDef =
    MetricDef { id: "metalExpense", label: "Spent /s", about: "Metal spent per second", unit: Unit::PerSecond };
pub const UNITS_ALIVE: MetricDef =
    MetricDef { id: "unitsActive", label: "Units", about: "Units alive", unit: Unit::Count };
pub const KILLS: MetricDef = MetricDef { id: "unitsKilled", label: "Kills", about: "Enemy units killed", unit: Unit::Count };
pub const LOSSES: MetricDef = MetricDef { id: "unitsDied", label: "Losses", about: "Own units lost", unit: Unit::Count };
pub const DAMAGE_RATE: MetricDef =
    MetricDef { id: "damageRate", label: "Damage /s", about: "Damage dealt per second", unit: Unit::PerSecond };
pub const DAMAGE: MetricDef =
    MetricDef { id: "damageDealt", label: "Damage", about: "Damage dealt in total", unit: Unit::Amount };
pub const METAL_WASTED: MetricDef =
    MetricDef { id: "metalExcess", label: "Wasted", about: "Metal wasted with storage full", unit: Unit::Amount };

/// The metrics a replay file can give, in the order rows are listed.
pub const REPLAY_METRICS: [MetricDef; 9] =
    [METAL_INCOME, ENERGY_INCOME, METAL_SPENT, UNITS_ALIVE, KILLS, LOSSES, DAMAGE_RATE, DAMAGE, METAL_WASTED];

pub struct Team {
    pub name: String,
    /// The colour BAR showed for this team, lifted to read on a dark background.
    pub color: Color,
    pub faction: Faction,
    pub is_ai: bool,
    pub side: usize,
}

pub struct Side {
    pub name: String,
    pub color: Color,
    pub teams: Vec<usize>,
}

pub struct Series {
    pub def: MetricDef,
    /// `[team][sample]`.
    pub values: Vec<Vec<f32>>,
}

pub struct Data {
    pub duration_s: f32,
    pub period_s: f32,
    /// Samples per series; sample `i` is at `i * period_s`.
    pub n: usize,
    /// Statistics exist up to here; later ones are still being worked out (a
    /// server computing them as it goes). The whole game for a finished replay.
    pub computed_s: f32,
    pub teams: Vec<Team>,
    pub sides: Vec<Side>,
    pub metrics: Vec<Series>,
    pub markers: Vec<Marker>,
    /// `[side][sample]`: each side's share of the lead measure, summing to 1.
    pub shares: Vec<Vec<f32>>,
    /// `[sample]`: how hard the fighting is, from 0 to 1.
    pub heat: Vec<f32>,
    /// Samples at which the lead passed to another side, and to which.
    pub lead_changes: Vec<(usize, usize)>,
}

/// Fallback side colours when the replay names none (colour-blind-checked pair first).
const FALLBACK: [(u8, u8, u8); 6] =
    [(90, 150, 255), (255, 96, 80), (245, 197, 66), (95, 211, 141), (190, 132, 255), (64, 208, 220)];

/// Two sides' colours closer than this (OKLab distance) are too alike.
const MIN_SIDE_DISTANCE: f32 = 0.15;

fn oklab_distance(a: Color, b: Color) -> f32 {
    let (a, b) = (a.to_oklab(), b.to_oklab());
    ((a.l - b.l).powi(2) + (a.a - b.a).powi(2) + (a.b - b.b).powi(2)).sqrt()
}

/// Trailing window for the lead measure, in samples (75 s at BAR's 15 s).
const LEAD_WINDOW: usize = 5;
/// A side must pass the leader by this much share to take the lead, so a dead
/// heat does not flicker.
const LEAD_HYSTERESIS: f32 = 0.02;
/// Damage per second per metal income per second at which the heat is full.
const HEAT_FULL: f32 = 8.0;

/// A team colour as BAR draws it, made readable on the dark background: the hue
/// kept, too-dark colours (BAR's pure blue) lifted.
pub fn display_color(rgb: [u8; 3]) -> Color {
    let base = Color::srgb_u8(rgb[0], rgb[1], rgb[2]);
    let mut lab = base.to_oklab();
    let l = lab.l.clamp(0.68, 0.86);
    if l != lab.l {
        lab.l = l;
        return lab.to_color(damascene_core::color::ColorSpace::SRGB);
    }
    base
}

impl Data {
    pub fn from_replay(path: &Path) -> Result<Data> {
        let demo = Demo::read(path)?;
        let stats = StatsHistory::read(path)?;
        ensure!(
            stats.has_history(),
            "this replay has no statistics history: a server recorded it, and only replays saved by a game client carry one"
        );
        let period_s = stats.period_s as f32;
        let infos = demo.team_list();
        // Teams that took part: named in the setup and with something built.
        let picked: Vec<(&barwatch_core::demo::TeamInfo, &Vec<TeamSample>)> = infos
            .iter()
            .filter_map(|info| stats.teams.get(info.team as usize).map(|s| (info, s)))
            .filter(|(_, s)| s.last().is_some_and(|s| s.units_produced > 0))
            .collect();
        ensure!(!picked.is_empty(), "no team in this replay built anything");
        let n = picked.iter().map(|(_, s)| s.len()).max().unwrap();
        let last_frame = picked.iter().filter_map(|(_, s)| s.last()).map(|s| s.frame).max().unwrap_or(0);
        let duration_s = (last_frame as f32 / GAME_SPEED).max(stats.game_time_s as f32).max(period_s);

        let mut allies: Vec<u32> = picked.iter().map(|(i, _)| i.ally_team).collect();
        allies.sort_unstable();
        allies.dedup();
        let teams: Vec<Team> = picked
            .iter()
            .enumerate()
            .map(|(k, (info, _))| {
                let side = allies.iter().position(|a| *a == info.ally_team).unwrap();
                let color = stats
                    .events
                    .team_colors
                    .get(&info.team)
                    .map(|c| display_color(*c))
                    .unwrap_or_else(|| {
                        let (r, g, b) = FALLBACK[(side + k * 2) % FALLBACK.len()];
                        Color::srgb_u8(r, g, b)
                    });
                Team {
                    name: info.name.clone(),
                    color,
                    faction: Faction::from_side(info.faction.as_deref()),
                    is_ai: info.is_ai,
                    side,
                }
            })
            .collect();

        let at = |s: &Vec<TeamSample>, i: usize| s.get(i).or(s.last()).copied().unwrap_or_default();
        // Per second over the last `w` samples, from a cumulative counter.
        let rate = |s: &Vec<TeamSample>, i: usize, w: usize, f: fn(&TeamSample) -> f32| {
            let lo = i.saturating_sub(w);
            if i == lo {
                0.0
            } else {
                ((f(&at(s, i)) - f(&at(s, lo))) / (period_s * (i - lo) as f32)).max(0.0)
            }
        };
        let metrics = REPLAY_METRICS
            .iter()
            .map(|def| Series {
                def: *def,
                values: picked
                    .iter()
                    .map(|(_, s)| {
                        (0..n)
                            .map(|i| match def.id {
                                "metalIncome" => rate(s, i, 1, |x| x.metal_produced),
                                "energyIncome" => rate(s, i, 1, |x| x.energy_produced),
                                "metalExpense" => rate(s, i, 1, |x| x.metal_used),
                                "unitsActive" => at(s, i).units_alive().max(0) as f32,
                                "unitsKilled" => at(s, i).units_killed as f32,
                                "unitsDied" => at(s, i).units_died as f32,
                                // Over three samples: single volleys make it unreadable.
                                "damageRate" => rate(s, i, 3, |x| x.damage_dealt),
                                "damageDealt" => at(s, i).damage_dealt,
                                "metalExcess" => at(s, i).metal_excess,
                                _ => 0.0,
                            })
                            .collect()
                    })
                    .collect(),
            })
            .collect();

        let mut markers = Vec::new();
        for (frame, team) in &stats.events.team_deaths {
            if let Some(k) = picked.iter().position(|(i, _)| i.team == *team) {
                markers.push(Marker {
                    t: *frame as f32 / GAME_SPEED,
                    kind: MarkKind::TeamDied,
                    team: k,
                    unit: None,
                    note: None,
                    sample: false,
                });
            }
        }
        // Replays rarely record the engine's "team died" message; a team whose
        // last unit is gone is out of the game all the same.
        for (k, (_, samples)) in picked.iter().enumerate() {
            if markers.iter().any(|m| m.team == k) {
                continue;
            }
            if let Some(i) = (1..samples.len()).find(|&i| samples[i].units_alive() <= 0 && samples[i - 1].units_alive() > 0) {
                markers.push(Marker {
                    t: i as f32 * period_s,
                    kind: MarkKind::TeamDied,
                    team: k,
                    unit: None,
                    note: Some("Its last unit is gone".into()),
                    sample: false,
                });
            }
        }
        Ok(Data::assemble(duration_s, period_s, n, teams, metrics, markers))
    }

    /// Puts the parts together and works out what is derived from them.
    pub fn assemble(
        duration_s: f32,
        period_s: f32,
        n: usize,
        teams: Vec<Team>,
        mut metrics: Vec<Series>,
        mut markers: Vec<Marker>,
    ) -> Data {
        // Whatever a source sends, the view may index every series to `n` and
        // every marker's team: make it so here rather than at every use.
        let n = n.max(1);
        let period_s = if period_s > 0.0 { period_s } else { 15.0 };
        for m in &mut metrics {
            m.values.resize(teams.len(), Vec::new());
            for v in &mut m.values {
                let last = v.last().copied().unwrap_or(0.0);
                v.resize(n, last);
            }
        }
        if metrics.is_empty() {
            metrics.push(Series { def: METAL_INCOME, values: vec![vec![0.0; n]; teams.len()] });
        }
        markers.retain(|m| m.team < teams.len() && m.t.is_finite());
        let side_count = teams.iter().map(|t| t.side + 1).max().unwrap_or(0);
        let mut sides: Vec<Side> = (0..side_count)
            .map(|s| {
                let members: Vec<usize> = (0..teams.len()).filter(|t| teams[*t].side == s).collect();
                // A side wears its first human's colour, as BAR's "group by team" does.
                let lead = members.iter().copied().find(|t| !teams[*t].is_ai).or(members.first().copied());
                Side {
                    name: String::new(),
                    color: lead.map(|t| teams[t].color).unwrap_or(tokens::MUTED_FOREGROUND),
                    teams: members,
                }
            })
            .collect();
        // Sides must stay apart even if the game's colours do not (or a viewer
        // sees fewer of them): a side too close to an earlier one takes the
        // next fallback colour that is far enough from all of them.
        for s in 1..sides.len() {
            let far = |c: Color, others: &[Side]| others.iter().all(|o| oklab_distance(o.color, c) >= MIN_SIDE_DISTANCE);
            if !far(sides[s].color, &sides[..s]) {
                if let Some(c) = FALLBACK.iter().map(|(r, g, b)| Color::srgb_u8(*r, *g, *b)).find(|c| far(*c, &sides[..s])) {
                    sides[s].color = c;
                }
            }
        }
        for side in &mut sides {
            let first = side.teams.first().map(|t| teams[*t].name.as_str()).unwrap_or("");
            side.name = match side.teams.len() {
                0 => String::new(),
                1 | 2 => side.teams.iter().map(|t| teams[*t].name.as_str()).collect::<Vec<_>>().join(" + "),
                k => format!("{first} + {}", k - 1),
            };
        }
        markers.sort_by(|a, b| a.t.total_cmp(&b.t));
        let mut data = Data {
            duration_s,
            period_s,
            n,
            computed_s: duration_s,
            teams,
            sides,
            metrics,
            markers,
            shares: Vec::new(),
            heat: Vec::new(),
            lead_changes: Vec::new(),
        };
        data.shares = data.lead_shares(METAL_INCOME.id);
        data.heat = data.fight_heat();
        data.lead_changes = lead_changes(&data.shares);
        data
    }

    pub fn metric_index(&self, id: &str) -> Option<usize> {
        self.metrics.iter().position(|m| m.def.id == id)
    }

    pub fn team_values(&self, metric: usize, team: usize) -> &[f32] {
        &self.metrics[metric].values[team]
    }

    pub fn side_values(&self, metric: usize, side: usize) -> Vec<f32> {
        let m = &self.metrics[metric].values;
        (0..self.n).map(|i| self.sides[side].teams.iter().map(|t| m[*t].get(i).copied().unwrap_or(0.0)).sum()).collect()
    }

    /// Each side's share of a metric, averaged over a trailing window.
    fn lead_shares(&self, id: &str) -> Vec<Vec<f32>> {
        let k = self.sides.len().max(1);
        let even = 1.0 / k as f32;
        let mut shares = vec![vec![even; self.n]; self.sides.len()];
        let Some(m) = self.metric_index(id) else { return shares };
        let per_side: Vec<Vec<f32>> = (0..self.sides.len()).map(|s| self.side_values(m, s)).collect();
        let window = |v: &Vec<f32>, i: usize| -> f32 { v[i.saturating_sub(LEAD_WINDOW - 1)..=i].iter().sum() };
        for i in 0..self.n {
            let total: f32 = per_side.iter().map(|v| window(v, i)).sum();
            if total > 0.0 {
                for (s, v) in per_side.iter().enumerate() {
                    shares[s][i] = window(v, i) / total;
                }
            }
        }
        shares
    }

    /// Fight intensity: damage dealt per second for every metal earned per
    /// second, so a skirmish at minute five and a battle at minute forty both
    /// read against the size of the game at that moment. The scale is fixed, so
    /// it says nothing about fights still to come.
    fn fight_heat(&self) -> Vec<f32> {
        let (Some(dmg), Some(inc)) = (self.metric_index(DAMAGE_RATE.id), self.metric_index(METAL_INCOME.id)) else {
            return vec![0.0; self.n];
        };
        let sum = |m: usize, i: usize| -> f32 { self.metrics[m].values.iter().map(|v| v.get(i).copied().unwrap_or(0.0)).sum() };
        (0..self.n)
            .map(|i| {
                let income = (i.saturating_sub(2)..=i).map(|j| sum(inc, j)).sum::<f32>() / 3.0;
                let ratio = sum(dmg, i) / income.max(5.0 * self.teams.len() as f32);
                (ratio / HEAT_FULL).clamp(0.0, 1.0).powf(0.7)
            })
            .collect()
    }

    /// The last sample at or before `t`: what the numbers say at that moment.
    /// Values between samples are not invented.
    pub fn sample_index(&self, t: f32) -> usize {
        ((t / self.period_s).floor().max(0.0) as usize).min(self.n.saturating_sub(1))
    }

    /// Made-up milestones for trying the design, every one flagged as a sample.
    /// They are placed where the replay's numbers make them plausible.
    pub fn add_sample_markers(&mut self) {
        let Some(inc) = self.metric_index(METAL_INCOME.id) else { return };
        let dmg = self.metric_index(DAMAGE_RATE.id);
        let per = self.period_s;
        // At most two teams per side carry samples, so a 16-AI side stays legible.
        let chosen: Vec<usize> = self.sides.iter().flat_map(|s| s.teams.iter().copied().take(2)).collect();
        let mut out = Vec::new();
        for (k, &team) in chosen.iter().enumerate() {
            let income = &self.metrics[inc].values[team];
            let first_above = |v: f32| income.iter().position(|x| *x >= v).map(|i| i as f32 * per);
            let jitter = (k as f32) * 20.0;
            let mut push = |t: Option<f32>, kind: MarkKind| {
                if let Some(t) = t.map(|t| t + jitter).filter(|t| *t < self.duration_s) {
                    let faction = self.teams[team].faction;
                    out.push(Marker {
                        t,
                        kind,
                        team,
                        unit: Some(kind.typical_unit(faction).to_owned()),
                        note: None,
                        sample: true,
                    });
                }
            };
            push(Some(150.0), MarkKind::Radar);
            push(first_above(12.0), MarkKind::Converter);
            push(first_above(18.0).map(|t| t + 60.0), MarkKind::Nano);
            push(first_above(25.0), MarkKind::Tech2);
            push(first_above(35.0).map(|t| t + 45.0), MarkKind::Moho);
            push(first_above(45.0), MarkKind::Air);
            push(first_above(60.0), MarkKind::Fusion);
            push(first_above(110.0), MarkKind::AdvConverter);
            push(first_above(160.0), MarkKind::Afus);
            push(first_above(200.0), MarkKind::Lrpc);
            push(first_above(260.0), MarkKind::Tech3);
            push(first_above(320.0), MarkKind::Nuke);
            if let Some(d) = dmg {
                let v = &self.metrics[d].values[team];
                push(v.iter().position(|x| *x > 0.0).map(|i| i as f32 * per), MarkKind::FirstKill);
            }
        }
        // Strikes where the fighting runs hot, against the side behind there;
        // decided in time order so a later fight never moves an earlier one.
        let mut kept: Vec<usize> = Vec::new();
        for i in 1..self.n {
            let spaced = kept.last().is_none_or(|k| (i - k) as f32 * per >= 300.0);
            if self.heat[i] > 0.7 && self.heat[i] >= self.heat[i - 1] && spaced {
                kept.push(i);
            }
        }
        for i in kept {
            let behind = (0..self.sides.len()).min_by(|a, b| self.shares[*a][i].total_cmp(&self.shares[*b][i])).unwrap_or(0);
            if let Some(&team) = self.sides[behind].teams.first() {
                let faction = self.teams[team].faction;
                out.push(Marker {
                    t: i as f32 * per,
                    kind: MarkKind::EcoStrike,
                    team,
                    unit: Some(MarkKind::EcoStrike.typical_unit(faction).to_owned()),
                    note: Some("12 units lost, worth 3.4k metal: 18% of its economy".into()),
                    sample: true,
                });
            }
        }
        self.markers.extend(out);
        self.markers.sort_by(|a, b| a.t.total_cmp(&b.t));
    }

    /// Every picture name the markers could ask for, for loading icons up front.
    pub fn picture_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.markers.iter().filter_map(|m| m.picture().map(str::to_owned)).collect();
        for kind in ALL_KINDS {
            for f in [Faction::Armada, Faction::Cortex, Faction::Legion] {
                names.push(kind.typical_unit(f).to_owned());
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

/// Where the lead changed hands, with a margin so a dead heat does not flicker.
pub fn lead_changes(shares: &[Vec<f32>]) -> Vec<(usize, usize)> {
    let n = shares.first().map(Vec::len).unwrap_or(0);
    let mut out = Vec::new();
    let mut leader: Option<usize> = None;
    for i in 0..n {
        let best = (0..shares.len()).max_by(|a, b| shares[*a][i].total_cmp(&shares[*b][i]));
        let Some(best) = best else { break };
        match leader {
            None if shares[best][i] > 1.0 / shares.len() as f32 + LEAD_HYSTERESIS => leader = Some(best),
            Some(l) if best != l && shares[best][i] > shares[l][i] + LEAD_HYSTERESIS => {
                out.push((i, best));
                leader = Some(best);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Two sides of one team each, with given metal income per sample.
    pub fn two_sided(income: [&[f32]; 2]) -> Data {
        let n = income[0].len();
        let teams = (0..2)
            .map(|s| Team { name: format!("p{s}"), color: tokens::FOREGROUND, faction: Faction::Armada, is_ai: false, side: s })
            .collect();
        let metrics = REPLAY_METRICS
            .iter()
            .map(|def| Series {
                def: *def,
                values: (0..2).map(|s| if def.id == METAL_INCOME.id { income[s].to_vec() } else { vec![1.0; n] }).collect(),
            })
            .collect();
        Data::assemble(n as f32 * 15.0, 15.0, n, teams, metrics, Vec::new())
    }

    #[test]
    fn derived_values_never_look_ahead() {
        let a = two_sided([&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0]]);
        let b = two_sided([&[1.0, 2.0, 3.0, 40.0, 0.0, 0.0], &[1.0, 1.0, 1.0, 9.0, 9.0, 9.0]]);
        for i in 0..3 {
            assert_eq!(a.shares[0][i], b.shares[0][i], "share at {i}");
            assert_eq!(a.heat[i], b.heat[i], "heat at {i}");
        }
    }

    #[test]
    fn shares_sum_to_one() {
        let d = two_sided([&[3.0, 5.0, 0.0, 2.0], &[1.0, 5.0, 0.0, 6.0]]);
        for i in 0..d.n {
            assert!((d.shares[0][i] + d.shares[1][i] - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn the_lead_changes_hands_only_past_the_margin() {
        let shares = vec![vec![0.6, 0.6, 0.51, 0.49, 0.45, 0.45], vec![0.4, 0.4, 0.49, 0.51, 0.55, 0.55]];
        assert_eq!(lead_changes(&shares), vec![(4, 1)]);
    }

    #[test]
    fn readouts_use_the_last_sample_at_or_before() {
        let d = two_sided([&[0.0; 4], &[0.0; 4]]);
        assert_eq!(d.sample_index(0.0), 0);
        assert_eq!(d.sample_index(29.9), 1);
        assert_eq!(d.sample_index(30.0), 2);
        assert_eq!(d.sample_index(1e6), 3);
    }

    #[test]
    fn dark_team_colours_are_lifted_and_keep_their_hue() {
        let blue = display_color([11, 62, 243]).to_oklab();
        assert!(blue.l >= 0.67);
        assert!(blue.b < -0.1, "still blue");
    }
}
