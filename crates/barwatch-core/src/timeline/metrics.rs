//! The series the timeline offers: their keys, how they read, and which field of
//! which record each one comes from.
//!
//! Keys are those of BAR's TeamStats panel (`luaui/Include/teamstats_model.lua`)
//! where the value means the same; where the panel has a key for a ratio
//! (`metalLevel`, `buildPowerUse`), the value here is that ratio too.

use super::MetricKind;

macro_rules! metrics {
    ($($variant:ident = $key:literal, $kind:ident;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
        pub enum Metric { $($variant),* }

        impl Metric {
            /// Every metric, in the order the document lists them.
            pub const ALL: &'static [Metric] = &[$(Metric::$variant),*];

            pub fn key(self) -> &'static str {
                match self { $(Metric::$variant => $key),* }
            }

            pub fn kind(self) -> MetricKind {
                match self { $(Metric::$variant => MetricKind::$kind),* }
            }
        }
    };
}

metrics! {
    // `team_stats`: the engine's own counters (`Spring.GetTeamStatsHistory`), all
    // running totals since the start.
    MetalProduced = "metalProduced", Cumulative;
    MetalUsed = "metalUsed", Cumulative;
    MetalExcess = "metalExcess", Cumulative;
    MetalSent = "metalSent", Cumulative;
    MetalReceived = "metalReceived", Cumulative;
    EnergyProduced = "energyProduced", Cumulative;
    EnergyUsed = "energyUsed", Cumulative;
    EnergyExcess = "energyExcess", Cumulative;
    EnergySent = "energySent", Cumulative;
    EnergyReceived = "energyReceived", Cumulative;
    DamageDealt = "damageDealt", Cumulative;
    DamageReceived = "damageReceived", Cumulative;
    UnitsProduced = "unitsProduced", Cumulative;
    UnitsDied = "unitsDied", Cumulative;
    UnitsKilled = "unitsKilled", Cumulative;
    UnitsReceived = "unitsReceived", Cumulative;
    UnitsSent = "unitsSent", Cumulative;
    UnitsCaptured = "unitsCaptured", Cumulative;
    UnitsOutCaptured = "unitsOutCaptured", Cumulative;
    // `extra_stat_update`: Gex's own tally every 450 frames.
    UnitValue = "unitValue", Level;
    ValueArmy = "valueArmy", Level;
    ValueDefense = "valueDefense", Level;
    ValueUtility = "valueUtility", Level;
    ValueEconomy = "valueEconomy", Level;
    MetalStored = "metalStored", Level;
    EnergyStored = "energyStored", Level;
    BuildPower = "buildPower", Level;
    BuildPowerUse = "buildPowerUse", Level;
    Actions = "actions", Cumulative;
    // `bs_team_resources`: `Spring.GetTeamResources` every 450 frames.
    MetalIncome = "metalIncome", Level;
    MetalExpense = "metalExpense", Level;
    MetalLevel = "metalLevel", Level;
    EnergyIncome = "energyIncome", Level;
    EnergyExpense = "energyExpense", Level;
    EnergyLevel = "energyLevel", Level;
}

/// Which record type a sample came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Source {
    TeamStats,
    ExtraStatUpdate,
    BsTeamResources,
}

/// The metric values one record carries. `get` reads a field as a finite number.
/// A field that is missing leaves its metric out; the rest still count.
pub(super) fn values(source: Source, get: impl Fn(&str) -> Option<f64>) -> Vec<(Metric, f64)> {
    use Metric::*;
    let direct: &[(&str, Metric)] = match source {
        Source::TeamStats => &[
            ("metalProduced", MetalProduced),
            ("metalUsed", MetalUsed),
            ("metalExcess", MetalExcess),
            ("metalSent", MetalSent),
            ("metalReceived", MetalReceived),
            ("energyProduced", EnergyProduced),
            ("energyUsed", EnergyUsed),
            ("energyExcess", EnergyExcess),
            ("energySent", EnergySent),
            ("energyReceived", EnergyReceived),
            ("damageDealt", DamageDealt),
            ("damageReceived", DamageReceived),
            ("unitsProduced", UnitsProduced),
            ("unitsDied", UnitsDied),
            ("unitsKilled", UnitsKilled),
            ("unitsReceived", UnitsReceived),
            ("unitsSent", UnitsSent),
            ("unitsCaptured", UnitsCaptured),
            ("unitsOutCaptured", UnitsOutCaptured),
        ],
        Source::ExtraStatUpdate => &[
            ("totalValue", UnitValue),
            ("armyValue", ValueArmy),
            ("defValue", ValueDefense),
            ("utilValue", ValueUtility),
            ("ecoValue", ValueEconomy),
            ("metalCurrent", MetalStored),
            ("energyCurrent", EnergyStored),
            ("buildPowerAvailable", BuildPower),
            ("actions", Actions),
        ],
        Source::BsTeamResources => &[
            ("metalIncome", MetalIncome),
            ("metalExpense", MetalExpense),
            ("energyIncome", EnergyIncome),
            ("energyExpense", EnergyExpense),
        ],
    };
    let mut out: Vec<(Metric, f64)> = direct.iter().filter_map(|(f, m)| Some((*m, get(f)?))).collect();
    // A share of a whole, as BAR's panel shows it; a share of nothing is none.
    let ratio = |part: &str, whole: &str| -> Option<f64> {
        let (p, w) = (get(part)?, get(whole)?);
        Some(if w > 0.0 { p / w } else { 0.0 })
    };
    let derived: &[(Metric, &str, &str)] = match source {
        Source::TeamStats => &[],
        Source::ExtraStatUpdate => &[(BuildPowerUse, "buildPowerUsed", "buildPowerAvailable")],
        Source::BsTeamResources => {
            &[(MetalLevel, "metalCurrent", "metalStorage"), (EnergyLevel, "energyCurrent", "energyStorage")]
        }
    };
    out.extend(derived.iter().filter_map(|(m, p, w)| Some((*m, ratio(p, w)?))));
    out
}
