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
    /// Something per second: a rate the game reports, or one derived from a
    /// cumulative counter.
    PerSecond,
    /// Something per minute of game time.
    PerMinute,
    /// Whole things.
    Count,
    /// An amount (metal, damage).
    Amount,
    /// A share of a whole, held as 0 to 1 and written as a percentage. A side's
    /// value is the mean of its players', where every other unit adds up.
    Percent,
}

/// The sections the list of metrics is read in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Economy,
    Value,
    Units,
    Combat,
    Activity,
    Totals,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Economy => "Economy",
            Group::Value => "Unit value",
            Group::Units => "Units",
            Group::Combat => "Combat",
            Group::Activity => "Activity",
            Group::Totals => "Running totals",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MetricDef {
    /// Stable name: BAR's TeamStats key where the meaning is BAR's.
    pub id: &'static str,
    /// Short label for a row: the words BAR's panel uses for the same number
    /// (its group and column captions, "Metal · Income"), run together.
    pub label: &'static str,
    /// What it is, in a few words of our own: hover text and readout titles.
    pub about: &'static str,
    /// What else a viewer should know about the numbers, or empty.
    pub note: &'static str,
    pub unit: Unit,
    pub group: Group,
    /// A running total since the start of the game, not a state of the moment.
    pub total: bool,
    /// Which rows show before the list is opened in full: 1 always, 2 when
    /// few of the first kind exist, 3 only in the full list.
    pub tier: u8,
}

impl MetricDef {
    const fn new(id: &'static str, label: &'static str, about: &'static str, unit: Unit, group: Group, tier: u8) -> MetricDef {
        MetricDef { id, label, about, note: "", unit, group, total: false, tier }
    }

    const fn note(mut self, note: &'static str) -> MetricDef {
        self.note = note;
        self
    }

    const fn total(mut self) -> MetricDef {
        self.total = true;
        self
    }

    /// How the values read, for the line under a chart's title.
    pub fn reading(&self) -> &'static str {
        match self.unit {
            Unit::PerSecond => "Per second",
            Unit::PerMinute => "Per minute",
            Unit::Count | Unit::Amount if self.total => "Total so far",
            Unit::Percent | Unit::Count | Unit::Amount => "",
        }
    }

    pub fn show(&self, v: f32) -> String {
        match self.unit {
            Unit::Percent => format!("{:.0}%", v * 100.0),
            Unit::Count | Unit::PerMinute => super::axis::number(v.round()),
            Unit::PerSecond | Unit::Amount => super::axis::number(v),
        }
    }

    /// The top of a chart whose highest value is `peak`: a round number above
    /// it, or the whole for a share.
    pub fn scale_top(&self, peak: f32) -> f32 {
        match self.unit {
            Unit::Percent => 1.0,
            _ => super::axis::nice_ceil(peak * 1.04),
        }
    }
}

/// The unit value series are the stats service's own count: they carry BAR's
/// names, and BAR's panel sorts units into more classes, so the numbers differ.
const VALUE_NOTE: &str = "Counted by the stats service, not by BAR: metal cost only, aircraft and ships count as army.";

use Group::*;
use Unit::*;

pub const METAL_INCOME: MetricDef =
    MetricDef::new("metalIncome", "Metal income", "Metal income per second", PerSecond, Economy, 1);
pub const ENERGY_INCOME: MetricDef =
    MetricDef::new("energyIncome", "Energy income", "Energy income per second", PerSecond, Economy, 1);
pub const METAL_EXPENSE: MetricDef =
    MetricDef::new("metalExpense", "Metal expense", "Metal spent per second", PerSecond, Economy, 3);
pub const ENERGY_EXPENSE: MetricDef =
    MetricDef::new("energyExpense", "Energy expense", "Energy spent per second", PerSecond, Economy, 3);
pub const METAL_LEVEL: MetricDef =
    MetricDef::new("metalLevel", "Metal level", "Metal held, as a share of the room in storage", Percent, Economy, 3);
pub const ENERGY_LEVEL: MetricDef =
    MetricDef::new("energyLevel", "Energy level", "Energy held, as a share of the room in storage", Percent, Economy, 3);
pub const METAL_STORED: MetricDef = MetricDef::new("metalStored", "Metal stored", "Metal held in storage", Amount, Economy, 2);
pub const ENERGY_STORED: MetricDef =
    MetricDef::new("energyStored", "Energy stored", "Energy held in storage", Amount, Economy, 2);
pub const VALUE_ARMY: MetricDef =
    MetricDef::new("valueArmy", "Army value", "Army value: what the armed mobile units cost", Amount, Value, 1).note(VALUE_NOTE);
pub const VALUE_DEFENSE: MetricDef =
    MetricDef::new("valueDefense", "Defense value", "Defense value: what the static defenses cost", Amount, Value, 2)
        .note(VALUE_NOTE);
pub const VALUE_ECONOMY: MetricDef =
    MetricDef::new("valueEconomy", "Economy value", "Economy value: what the energy and metal makers cost", Amount, Value, 2)
        .note(VALUE_NOTE);
pub const VALUE_UTILITY: MetricDef =
    MetricDef::new("valueUtility", "Utility value", "Utility value: what everything else cost", Amount, Value, 3)
        .note(VALUE_NOTE);
pub const UNIT_VALUE: MetricDef =
    MetricDef::new("unitValue", "Unit value", "Unit value: what every unit alive cost", Amount, Value, 1).note(VALUE_NOTE);
pub const UNITS_ALIVE: MetricDef = MetricDef::new("unitsActive", "Units alive", "Units alive", Count, Units, 1)
    .note("Worked out here: units built, received and captured, less those lost, given away and stolen.");
pub const UNITS_PRODUCED: MetricDef = MetricDef::new("unitsProduced", "Units produced", "Units built", Count, Units, 2).total();
pub const KILLS: MetricDef = MetricDef::new("unitsKilled", "Units killed", "Enemy units killed", Count, Units, 1).total();
pub const LOSSES: MetricDef = MetricDef::new("unitsDied", "Units lost", "Own units lost", Count, Units, 2).total();
pub const UNITS_CAPTURED: MetricDef =
    MetricDef::new("unitsCaptured", "Units captured", "Enemy units taken over", Count, Units, 3).total();
pub const UNITS_STOLEN: MetricDef =
    MetricDef::new("unitsOutCaptured", "Units stolen", "Own units the enemy took over", Count, Units, 3).total();
pub const UNITS_RECEIVED: MetricDef =
    MetricDef::new("unitsReceived", "Units received", "Units handed over by allies", Count, Units, 3).total();
pub const UNITS_SENT: MetricDef = MetricDef::new("unitsSent", "Units sent", "Units handed to allies", Count, Units, 3).total();
pub const DAMAGE_RATE: MetricDef = MetricDef::new("damageRate", "Damage rate", "Damage done per second", PerSecond, Combat, 1)
    .note("Worked out here from the damage total, averaged over 45 seconds: single volleys would make it unreadable.");
pub const DAMAGE: MetricDef = MetricDef::new("damageDealt", "Damage dealt", "Damage done", Amount, Combat, 3).total();
pub const DAMAGE_RECEIVED: MetricDef =
    MetricDef::new("damageReceived", "Damage received", "Damage suffered", Amount, Combat, 3).total();
pub const BUILD_POWER: MetricDef =
    MetricDef::new("buildPower", "Build power", "Build power of everything that can build", Amount, Activity, 2);
pub const BUILD_POWER_USE: MetricDef =
    MetricDef::new("buildPowerUse", "Build power in use", "Share of the build power that is busy", Percent, Activity, 1);
pub const ACTIONS_PER_MINUTE: MetricDef =
    MetricDef::new("actionsPerMinute", "Actions per minute", "Orders given per minute of game time", PerMinute, Activity, 3)
        .note("Worked out here from the count of orders, over the last minute.");
pub const METAL_PRODUCED: MetricDef =
    MetricDef::new("metalProduced", "Metal produced", "Metal gathered", Amount, Totals, 3).total();
pub const METAL_USED: MetricDef = MetricDef::new("metalUsed", "Metal used", "Metal spent", Amount, Totals, 3).total();
pub const METAL_WASTED: MetricDef =
    MetricDef::new("metalExcess", "Metal excess", "Metal lost to a full storage", Amount, Totals, 3).total();
pub const METAL_SENT: MetricDef = MetricDef::new("metalSent", "Metal sent", "Metal given to allies", Amount, Totals, 3).total();
pub const METAL_RECEIVED: MetricDef =
    MetricDef::new("metalReceived", "Metal received", "Metal given by allies", Amount, Totals, 3).total();
pub const ENERGY_PRODUCED: MetricDef =
    MetricDef::new("energyProduced", "Energy produced", "Energy gathered", Amount, Totals, 3).total();
pub const ENERGY_USED: MetricDef = MetricDef::new("energyUsed", "Energy used", "Energy spent", Amount, Totals, 3).total();
pub const ENERGY_WASTED: MetricDef =
    MetricDef::new("energyExcess", "Energy excess", "Energy lost to a full storage", Amount, Totals, 3).total();
pub const ENERGY_SENT: MetricDef =
    MetricDef::new("energySent", "Energy sent", "Energy given to allies", Amount, Totals, 3).total();
pub const ENERGY_RECEIVED: MetricDef =
    MetricDef::new("energyReceived", "Energy received", "Energy given by allies", Amount, Totals, 3).total();

/// Every metric the timeline can show, in the order rows and menus list them.
/// A source offers some of them; the rest are left out.
pub const CATALOG: [MetricDef; 37] = [
    METAL_INCOME,
    ENERGY_INCOME,
    METAL_EXPENSE,
    ENERGY_EXPENSE,
    METAL_LEVEL,
    ENERGY_LEVEL,
    METAL_STORED,
    ENERGY_STORED,
    VALUE_ARMY,
    UNIT_VALUE,
    VALUE_DEFENSE,
    VALUE_ECONOMY,
    VALUE_UTILITY,
    UNITS_ALIVE,
    KILLS,
    LOSSES,
    UNITS_PRODUCED,
    UNITS_CAPTURED,
    UNITS_STOLEN,
    UNITS_RECEIVED,
    UNITS_SENT,
    DAMAGE_RATE,
    DAMAGE,
    DAMAGE_RECEIVED,
    BUILD_POWER_USE,
    BUILD_POWER,
    ACTIONS_PER_MINUTE,
    METAL_PRODUCED,
    METAL_USED,
    METAL_WASTED,
    METAL_SENT,
    METAL_RECEIVED,
    ENERGY_PRODUCED,
    ENERGY_USED,
    ENERGY_WASTED,
    ENERGY_SENT,
    ENERGY_RECEIVED,
];

/// What the large chart shows until the viewer picks: the first of these a
/// source offers. Income is the number a game of BAR turns on; a source that
/// has no income yet (a simulation by an older stats service gives the
/// engine's counters only at its end) starts on army value, which every
/// source has from its first sample.
pub const DEFAULT_HERO: [&str; 3] = [METAL_INCOME.id, VALUE_ARMY.id, UNIT_VALUE.id];

/// The metrics a replay file can give, in the order rows are listed.
pub const REPLAY_METRICS: [MetricDef; 9] =
    [METAL_INCOME, ENERGY_INCOME, METAL_EXPENSE, UNITS_ALIVE, KILLS, LOSSES, DAMAGE_RATE, DAMAGE, METAL_WASTED];

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
    /// `metrics` is only a stand-in of zeroes: the source had no metric yet.
    pub placeholder: bool,
    /// What the lead strip measures: the best measure these metrics offer.
    pub lead: Lead,
    /// `[side][sample]`: each side's share of the lead measure, summing to 1.
    pub shares: Vec<Vec<f32>>,
    /// `[sample]`: how hard the fighting is, from 0 to 1.
    pub heat: Vec<f32>,
    /// Samples at which the lead passed to another side, and to which.
    pub lead_changes: Vec<(usize, usize)>,
}

/// What "who is ahead" is measured by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lead {
    /// Each side's share of the army value. Preferred: metal income pegs to
    /// one edge in lopsided games, and says little about who is winning a fight.
    ArmyValue,
    /// Each side's share of metal income over the last 75 s, for a source
    /// without army value.
    MetalIncome,
    /// Nothing to measure it by: the strip stays level.
    Nothing,
}

impl Lead {
    /// For the strip's label.
    pub fn label(self) -> &'static str {
        match self {
            Lead::ArmyValue => "army value",
            Lead::MetalIncome => "metal income, 75 s",
            Lead::Nothing => "no measure yet",
        }
    }

    /// For the hover readout's title.
    pub fn title(self) -> &'static str {
        match self {
            Lead::ArmyValue => "Share of army value",
            Lead::MetalIncome => "Share of metal income",
            Lead::Nothing => "Who is ahead",
        }
    }

    /// The lead, in share above an even split, at which the strip is full.
    /// Fixed, so the strip cannot hint at how lopsided the game gets.
    pub fn full_at(self) -> f32 {
        match self {
            Lead::ArmyValue => 0.35,
            Lead::MetalIncome | Lead::Nothing => 0.125,
        }
    }
}

/// Counted into each side's army value before shares are taken, so the first
/// scout of the game does not swing the strip to an edge.
const ARMY_PRIOR: f32 = 300.0;

/// Fallback side colours when the replay names none (colour-blind-checked pair first).
pub(super) const FALLBACK: [(u8, u8, u8); 6] =
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
                let color = stats.events.team_colors.get(&info.team).map(|c| display_color(*c)).unwrap_or_else(|| {
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
        // The view always has a metric to index; one that is all zeroes when a
        // source has none (the view then shows no charts at all).
        let placeholder = metrics.is_empty();
        if placeholder {
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
            placeholder,
            lead: Lead::Nothing,
            shares: Vec::new(),
            heat: Vec::new(),
            lead_changes: Vec::new(),
        };
        data.lead = if data.metric_index(VALUE_ARMY.id).is_some() {
            Lead::ArmyValue
        } else if data.metric_index(METAL_INCOME.id).is_some() {
            Lead::MetalIncome
        } else {
            Lead::Nothing
        };
        data.shares = match data.lead {
            Lead::ArmyValue => data.lead_shares(VALUE_ARMY.id, 1, ARMY_PRIOR),
            Lead::MetalIncome | Lead::Nothing => data.lead_shares(METAL_INCOME.id, LEAD_WINDOW, 0.0),
        };
        if placeholder {
            data.lead = Lead::Nothing;
        }
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

    /// A side's value of a metric: its players' added up, or their mean for
    /// a share of a whole.
    pub fn side_values(&self, metric: usize, side: usize) -> Vec<f32> {
        let m = &self.metrics[metric].values;
        let teams = &self.sides[side].teams;
        let scale = if self.metrics[metric].def.unit == Unit::Percent { 1.0 / teams.len().max(1) as f32 } else { 1.0 };
        (0..self.n).map(|i| teams.iter().map(|t| m[*t].get(i).copied().unwrap_or(0.0)).sum::<f32>() * scale).collect()
    }

    /// The moment of sample `i`. The last sample of a finished game is taken
    /// when the game ends, short of its place on the grid.
    pub fn sample_t(&self, i: usize) -> f32 {
        (i as f32 * self.period_s).min(self.duration_s)
    }

    /// How many samples lie at or before `t`.
    pub fn samples_until(&self, t: f32) -> usize {
        if t < 0.0 {
            0
        } else {
            self.sample_index(t) + 1
        }
    }

    /// Each side's share of a metric, summed over a trailing window of
    /// `window` samples, every side counted with `prior` more than it has.
    fn lead_shares(&self, id: &str, window: usize, prior: f32) -> Vec<Vec<f32>> {
        let k = self.sides.len().max(1);
        let even = 1.0 / k as f32;
        let mut shares = vec![vec![even; self.n]; self.sides.len()];
        let Some(m) = self.metric_index(id) else { return shares };
        let per_side: Vec<Vec<f32>> = (0..self.sides.len()).map(|s| self.side_values(m, s)).collect();
        let window = |v: &Vec<f32>, i: usize| -> f32 { v[i.saturating_sub(window - 1)..=i].iter().sum::<f32>() + prior };
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
        let last = self.n.saturating_sub(1);
        if t >= self.duration_s {
            return last;
        }
        ((t / self.period_s).floor().max(0.0) as usize).min(last)
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

    /// The pictures the markers ask for: the units that set them off, and the
    /// skull of a team that is out.
    pub fn marker_pictures(&self) -> Vec<String> {
        let mut names: Vec<String> = self.markers.iter().filter_map(|m| m.picture().map(str::to_owned)).collect();
        names.sort();
        names.dedup();
        names
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
