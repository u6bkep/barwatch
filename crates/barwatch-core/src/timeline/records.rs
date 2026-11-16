//! One line of the record stream, read into what the timeline needs of it.
//!
//! The lines are written by Lua widgets (bar-replays' `gex.lua` and the `bs_*`
//! companion) and are not trusted: any field may be missing, of another type, a
//! string where a number belongs (Gex writes NaN and infinity as `"NaN"` and
//! `"Infinity"`), or out of range. Whatever does not read cleanly is left out; a
//! record that lacks what identifies it is dropped as a whole.

use super::metrics::{self, Metric, Source};
use super::rules::{BsDef, GexDef};
use serde_json::{Map, Value};

/// Largest frame accepted: twelve hours of game. Anything later is treated as
/// malformed, which bounds every per-sample allocation.
pub const MAX_FRAME: u32 = 30 * 3600 * 12;
/// Longest unit-definition name or unit group kept; longer ones are refused.
const MAX_NAME: usize = 128;

pub(super) enum Record {
    /// The stream's control line: where to resume and whether it is over.
    Cursor {
        frame: Option<u32>,
        complete: bool,
    },
    /// Per-team values at one moment, from one of the three sampled record types.
    Sample {
        frame: u32,
        team: u32,
        values: Vec<(Metric, f64)>,
    },
    UnitDef {
        def: u32,
        name: Option<String>,
        facts: GexDef,
    },
    BsUnitDef {
        def: u32,
        name: Option<String>,
        facts: BsDef,
    },
    UnitCreated {
        frame: u32,
        unit: u32,
        team: u32,
        def: u32,
    },
    UnitFinished {
        frame: u32,
        unit: u32,
        team: u32,
    },
    UnitKilled {
        frame: u32,
        unit: u32,
        team: u32,
        def: Option<u32>,
        def_name: Option<String>,
        attacker_team: Option<u32>,
    },
    TeamDied {
        frame: u32,
        team: u32,
    },
    /// A record type the timeline does not use; its frame still says how far the
    /// simulation has got.
    Other {
        frame: Option<u32>,
    },
}

/// Reads one parsed line. `None`: not a record, or a record the timeline uses
/// that lacks a field it cannot do without.
pub(super) fn read(v: &Value) -> Option<Record> {
    let o = v.as_object()?;
    let action = o.get("action")?.as_str()?;
    let frame = int(o, "frame").filter(|f| *f <= MAX_FRAME);
    // A frame present but unreadable or too large spoils the whole record.
    if o.contains_key("frame") && frame.is_none() && action != "_cursor" {
        return None;
    }
    Some(match action {
        "_cursor" => Record::Cursor { frame, complete: o.get("complete").and_then(Value::as_bool).unwrap_or(false) },
        // The companion's periodic copy of the same history entries, with the same
        // keys and the entry's own frame: it gives the counters during a simulation.
        "team_stats" | "bs_team_stats" => sample(o, frame?, Source::TeamStats)?,
        "extra_stat_update" => sample(o, frame?, Source::ExtraStatUpdate)?,
        "bs_team_resources" => sample(o, frame?, Source::BsTeamResources)?,
        "unit_def" => Record::UnitDef { def: int(o, "defID")?, name: name(o, "defName"), facts: gex_def(o) },
        "bs_unit_def" => Record::BsUnitDef { def: int(o, "defID")?, name: name(o, "defName"), facts: bs_def(o) },
        "unit_created" => {
            Record::UnitCreated { frame: frame?, unit: int(o, "unitID")?, team: int(o, "teamID")?, def: int(o, "defID")? }
        }
        "unit_finished" => Record::UnitFinished { frame: frame?, unit: int(o, "unitID")?, team: int(o, "teamID")? },
        "unit_killed" => Record::UnitKilled {
            frame: frame?,
            unit: int(o, "unitID")?,
            team: int(o, "teamID")?,
            def: int(o, "defID"),
            def_name: name(o, "defName"),
            attacker_team: int(o, "attackerTeam"),
        },
        "team_died" => Record::TeamDied { frame: frame?, team: int(o, "teamID")? },
        _ => Record::Other { frame },
    })
}

/// A sampled record: the team, and the metrics its fields give.
fn sample(o: &Map<String, Value>, frame: u32, source: Source) -> Option<Record> {
    let team = int(o, "teamID")?;
    Some(Record::Sample { frame, team, values: metrics::values(source, |k| num(o, k)) })
}

fn gex_def(o: &Map<String, Value>) -> GexDef {
    let n = |k: &str| num(o, k).unwrap_or(0.0);
    let b = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
    GexDef {
        group: name(o, "unitGroup").unwrap_or_default(),
        is_commander: b("isCommander"),
        is_factory: b("isFactory"),
        // Gex's `isReclaimer` is `isBuilder and not isFactory`.
        builder_not_factory: b("isReclaimer"),
        speed: n("speed"),
        radar: n("radarDistance"),
        // Gex's `energyProduction` is the engine's `totalEnergyOut`, which is the
        // definition's `energyMake` (`LuaUnitDefs.cpp`).
        energy_make: n("energyProduction"),
        extracts_metal: n("extractsMetal"),
        conversion_capacity: n("energyConversionCapacity"),
        metal_cost: n("metalCost"),
        energy_cost: n("energyCost"),
    }
}

fn bs_def(o: &Map<String, Value>) -> BsDef {
    let cp = o.get("customParams").and_then(Value::as_object);
    // A custom parameter is there when it has any value but null, as in Lua.
    let has = |k: &str| cp.and_then(|c| c.get(k)).is_some_and(|v| !v.is_null());
    let text = |k: &str| -> Option<String> {
        match cp?.get(k)? {
            Value::String(s) if s.len() <= MAX_NAME => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    BsDef {
        tech_level: cp.and_then(|c| c.get("techlevel")).and_then(lua_tonumber).unwrap_or(1.0),
        group: text("unitgroup"),
        lrpc: has("islrpc"),
        geothermal: has("geothermal"),
        commander: has("iscommander"),
        converter: has("energyconv_capacity"),
        is_builder: o.get("isBuilder").and_then(Value::as_bool),
        can_fly: o.get("canFly").and_then(Value::as_bool),
        min_water_depth: num(o, "minWaterDepth"),
    }
}

/// Lua's `tonumber` on a custom parameter: a number, or a string that reads as
/// one with blanks around it. Anything else is nil.
fn lua_tonumber(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// A finite number. Gex's `"NaN"` and `"Infinity"` strings are not numbers.
fn num(o: &Map<String, Value>, k: &str) -> Option<f64> {
    o.get(k)?.as_f64().filter(|n| n.is_finite())
}

/// A whole number from 0 to `u32::MAX`; Lua may write it as `450` or `450.0`.
fn int(o: &Map<String, Value>, k: &str) -> Option<u32> {
    let v = o.get(k)?;
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    let f = v.as_f64()?;
    (f.is_finite() && f >= 0.0 && f <= u32::MAX as f64 && f.fract() == 0.0).then_some(f as u32)
}

fn name(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k)?.as_str().filter(|s| s.len() <= MAX_NAME).map(str::to_owned)
}
