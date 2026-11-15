//! Milestones: which unit makes which mark, ported from the rules BAR's own
//! statistics collector applies (`luarules/gadgets/api_teamstats.lua`, its
//! milestone table), restated here from the facts the record stream carries.
//!
//! A unit definition is known from up to two records: Gex's `unit_def` (always
//! there) and the companion's `bs_unit_def` (only in games extracted since it
//! exists). A rule that needs a fact only the second one has answers "cannot
//! tell" without it, and no mark is made.

/// What Gex's `unit_def` says about a definition, as far as milestones care.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct GexDef {
    /// `customParams.unitgroup`, empty when none.
    pub group: String,
    pub is_commander: bool,
    pub is_factory: bool,
    /// The engine's `isBuilder and not isFactory` (Gex calls it `isReclaimer`).
    pub builder_not_factory: bool,
    pub speed: f64,
    pub radar: f64,
    pub energy_make: f64,
    pub extracts_metal: f64,
    /// `customParams.energyconv_capacity` as a number, 0 when none.
    pub conversion_capacity: f64,
    pub metal_cost: f64,
    pub energy_cost: f64,
}

/// What the companion's `bs_unit_def` adds (bar-replays DESIGN §9.7).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct BsDef {
    /// `tonumber(customParams.techlevel)`, 1 when it names none.
    pub tech_level: f64,
    pub group: Option<String>,
    /// Whether these custom parameters are present at all.
    pub lrpc: bool,
    pub geothermal: bool,
    pub commander: bool,
    pub converter: bool,
    pub is_builder: Option<bool>,
    pub can_fly: Option<bool>,
    pub min_water_depth: Option<f64>,
}

/// Everything known about one unit definition.
#[derive(Clone, Debug, Default)]
pub(super) struct DefFacts {
    pub name: Option<String>,
    pub gex: Option<GexDef>,
    pub bs: Option<BsDef>,
}

/// BAR's milestone kinds this builder makes, by the keys BAR uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Milestone {
    Tech2,
    Tech3,
    Nuke,
    Antinuke,
    Lrpc,
    CommanderLost,
    FirstKill,
    FirstLoss,
    Moho,
    Converter,
    AdvConverter,
    Fusion,
    Afus,
    Geo,
    Radar,
    Nano,
    Air,
    Naval,
    CommanderKill,
    TeamDied,
}

impl Milestone {
    pub const ALL: [Milestone; 20] = [
        Milestone::Tech2,
        Milestone::Tech3,
        Milestone::Nuke,
        Milestone::Antinuke,
        Milestone::Lrpc,
        Milestone::CommanderLost,
        Milestone::FirstKill,
        Milestone::FirstLoss,
        Milestone::Moho,
        Milestone::Converter,
        Milestone::AdvConverter,
        Milestone::Fusion,
        Milestone::Afus,
        Milestone::Geo,
        Milestone::Radar,
        Milestone::Nano,
        Milestone::Air,
        Milestone::Naval,
        Milestone::CommanderKill,
        Milestone::TeamDied,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Milestone::Tech2 => "tech2",
            Milestone::Tech3 => "tech3",
            Milestone::Nuke => "nuke",
            Milestone::Antinuke => "antinuke",
            Milestone::Lrpc => "lrpc",
            Milestone::CommanderLost => "commanderLost",
            Milestone::FirstKill => "firstKill",
            Milestone::FirstLoss => "firstLoss",
            Milestone::Moho => "moho",
            Milestone::Converter => "converter",
            Milestone::AdvConverter => "advConverter",
            Milestone::Fusion => "fusion",
            Milestone::Afus => "afus",
            Milestone::Geo => "geo",
            Milestone::Radar => "radar",
            Milestone::Nano => "nano",
            Milestone::Air => "air",
            Milestone::Naval => "naval",
            Milestone::CommanderKill => "commanderKill",
            Milestone::TeamDied => "teamDied",
        }
    }

    pub fn from_key(key: &str) -> Option<Milestone> {
        Milestone::ALL.into_iter().find(|m| m.key() == key)
    }

    /// Marked each time it happens; every other kind once per team.
    pub fn every(self) -> bool {
        matches!(self, Milestone::CommanderLost | Milestone::CommanderKill)
    }
}

/// What a fusion reactor makes at the least, and an advanced one (energy per second).
const FUSION_ENERGY: f64 = 500.0;
const ADVANCED_FUSION_ENERGY: f64 = 2500.0;

/// The kinds of BAR's unit classification this port needs to tell apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bucket {
    Air,
    Sea,
    /// Anything else: factories, builders, strategic, economy, utility, defence, army.
    Other,
}

impl DefFacts {
    fn group(&self) -> Option<&str> {
        match (&self.gex, &self.bs) {
            (Some(g), _) => Some(g.group.as_str()),
            (None, Some(b)) => Some(b.group.as_deref().unwrap_or("")),
            (None, None) => None,
        }
    }

    fn tech(&self) -> Option<f64> {
        self.bs.as_ref().map(|b| b.tech_level)
    }

    pub fn is_commander(&self) -> Option<bool> {
        match (&self.gex, &self.bs) {
            (Some(g), _) => Some(g.is_commander),
            (None, Some(b)) => Some(b.commander),
            (None, None) => None,
        }
    }

    /// What BAR counts a unit as worth: metal plus energy at sixty to one.
    pub fn cost(&self) -> Option<f64> {
        self.gex.as_ref().map(|g| g.metal_cost + g.energy_cost / 60.0)
    }

    fn converter(&self) -> Option<bool> {
        match (&self.bs, &self.gex) {
            (Some(b), _) => Some(b.converter),
            // Gex writes the parameter as a number, 0 when absent: a converter
            // declared with capacity 0 would be missed, and no real one is.
            (None, Some(g)) => Some(g.conversion_capacity > 0.0),
            (None, None) => None,
        }
    }

    /// The energy a power plant of the second tech level or above makes; 0 for
    /// anything that moves, a geothermal plant, a commander or a lower tech.
    fn plant_makes(&self) -> Option<f64> {
        let (g, b) = (self.gex.as_ref()?, self.bs.as_ref()?);
        if g.speed != 0.0 || b.geothermal || g.is_commander || b.tech_level < 2.0 {
            return Some(0.0);
        }
        Some(g.energy_make)
    }

    /// BAR's classification, as far as air and sea go. Its order matters: a
    /// factory or builder is that first, whatever else it is.
    fn bucket(&self) -> Option<Bucket> {
        let (g, b) = (self.gex.as_ref()?, self.bs.as_ref()?);
        if g.is_factory || b.is_builder.unwrap_or(g.builder_not_factory) {
            return Some(Bucket::Other);
        }
        if matches!(g.group.as_str(), "nuke" | "antinuke" | "energy" | "metal" | "util") || b.lrpc {
            return Some(Bucket::Other);
        }
        if g.speed == 0.0 {
            return Some(Bucket::Other);
        }
        if b.can_fly? {
            return Some(Bucket::Air);
        }
        // BAR asks the engine whether the unit moves as a ship. The companion
        // records `minWaterDepth` for this: every ship and submarine definition
        // needs water under it, and nothing else that moves does.
        if b.min_water_depth? > 0.0 {
            return Some(Bucket::Sea);
        }
        Some(Bucket::Other)
    }

    /// Whether finishing a unit of this definition reaches `kind`; `None` when
    /// the facts to tell are missing.
    fn reaches(&self, kind: Milestone) -> Option<bool> {
        let g = || self.gex.as_ref();
        let b = || self.bs.as_ref();
        Some(match kind {
            Milestone::Tech2 => self.tech()? == 2.0,
            Milestone::Tech3 => self.tech()? >= 3.0,
            Milestone::Nuke => self.group()? == "nuke",
            Milestone::Antinuke => self.group()? == "antinuke",
            Milestone::Lrpc => b()?.lrpc,
            Milestone::Moho => g()?.extracts_metal > 0.0 && self.tech()? >= 2.0,
            Milestone::Converter => self.converter()?,
            Milestone::AdvConverter => self.converter()? && self.tech()? >= 2.0,
            Milestone::Fusion => self.plant_makes()? >= FUSION_ENERGY,
            Milestone::Afus => self.plant_makes()? >= ADVANCED_FUSION_ENERGY,
            Milestone::Geo => b()?.geothermal,
            Milestone::Radar => {
                let g = g()?;
                g.radar > 0.0 && g.speed == 0.0 && g.group == "util"
            }
            Milestone::Nano => {
                let g = g()?;
                g.builder_not_factory && g.speed == 0.0
            }
            Milestone::Air => self.bucket()? == Bucket::Air,
            Milestone::Naval => self.bucket()? == Bucket::Sea,
            Milestone::CommanderLost | Milestone::FirstKill | Milestone::FirstLoss | Milestone::CommanderKill | Milestone::TeamDied => {
                false
            }
        })
    }

    /// The milestones finishing a unit of this definition reaches.
    pub fn built(&self) -> impl Iterator<Item = Milestone> + '_ {
        Milestone::ALL.into_iter().filter(|k| self.reaches(*k) == Some(true))
    }
}

/// The faction a commander's definition belongs to, by BAR's name prefixes.
pub(super) fn faction_of(def_name: &str) -> Option<&'static str> {
    match def_name.get(..3)? {
        "arm" => Some("armada"),
        "cor" => Some("cortex"),
        "leg" => Some("legion"),
        _ => None,
    }
}
