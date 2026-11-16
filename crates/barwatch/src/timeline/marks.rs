//! Event markers: the milestone kinds BAR's statistics collector records
//! (`luarules/gadgets/api_teamstats.lua`), how they are labelled and drawn, and
//! how close ones are gathered into one badge.

use damascene_core::prelude::Color;

/// BAR's milestone kinds, by the keys its collector uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum MarkKind {
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
    NukeLaunched,
    Nuked,
    EcoStrike,
    ArmyLoss,
    Raid,
    TeamDied,
}

/// What a milestone means for the team it belongs to; BAR draws this as a
/// small round sign on the picture's corner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkClass {
    Built,
    Tech,
    Attack,
    Lost,
    /// The team is out of the game: drawn as a skull, with no sign.
    Out,
}

impl MarkClass {
    /// BAR's colours for the corner sign (`teamstats_graphs.lua`, `BADGES`).
    pub fn color(self) -> Color {
        match self {
            MarkClass::Built => Color::srgb_u8(92, 199, 107),
            MarkClass::Tech => Color::srgb_u8(89, 153, 255),
            MarkClass::Attack => Color::srgb_u8(250, 168, 51),
            MarkClass::Lost => Color::srgb_u8(230, 77, 66),
            MarkClass::Out => Color::srgb_u8(160, 160, 160),
        }
    }

    /// The sign drawn on the badge, as text for a legend.
    pub fn sign(self) -> &'static str {
        match self {
            MarkClass::Built => "+",
            MarkClass::Tech => "↑",
            MarkClass::Attack => "×",
            MarkClass::Lost => "−",
            MarkClass::Out => "",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MarkClass::Built => "First built",
            MarkClass::Tech => "Tech up",
            MarkClass::Attack => "Attack",
            MarkClass::Lost => "Lost",
            MarkClass::Out => "Out",
        }
    }
}

pub const ALL_KINDS: [MarkKind; 25] = [
    MarkKind::Tech2,
    MarkKind::Tech3,
    MarkKind::Nuke,
    MarkKind::Antinuke,
    MarkKind::Lrpc,
    MarkKind::CommanderLost,
    MarkKind::FirstKill,
    MarkKind::FirstLoss,
    MarkKind::Moho,
    MarkKind::Converter,
    MarkKind::AdvConverter,
    MarkKind::Fusion,
    MarkKind::Afus,
    MarkKind::Geo,
    MarkKind::Radar,
    MarkKind::Nano,
    MarkKind::Air,
    MarkKind::Naval,
    MarkKind::CommanderKill,
    MarkKind::NukeLaunched,
    MarkKind::Nuked,
    MarkKind::EcoStrike,
    MarkKind::ArmyLoss,
    MarkKind::Raid,
    MarkKind::TeamDied,
];

impl MarkKind {
    /// The collector's key for this kind, as a server would send it.
    // Read by the server feed to come; the replay file has no milestones.
    #[allow(dead_code)]
    pub fn key(self) -> &'static str {
        match self {
            MarkKind::Tech2 => "tech2",
            MarkKind::Tech3 => "tech3",
            MarkKind::Nuke => "nuke",
            MarkKind::Antinuke => "antinuke",
            MarkKind::Lrpc => "lrpc",
            MarkKind::CommanderLost => "commanderLost",
            MarkKind::FirstKill => "firstKill",
            MarkKind::FirstLoss => "firstLoss",
            MarkKind::Moho => "moho",
            MarkKind::Converter => "converter",
            MarkKind::AdvConverter => "advConverter",
            MarkKind::Fusion => "fusion",
            MarkKind::Afus => "afus",
            MarkKind::Geo => "geo",
            MarkKind::Radar => "radar",
            MarkKind::Nano => "nano",
            MarkKind::Air => "air",
            MarkKind::Naval => "naval",
            MarkKind::CommanderKill => "commanderKill",
            MarkKind::NukeLaunched => "nukeLaunched",
            MarkKind::Nuked => "nuked",
            MarkKind::EcoStrike => "ecoStrike",
            MarkKind::ArmyLoss => "armyLoss",
            MarkKind::Raid => "raid",
            MarkKind::TeamDied => "teamDied",
        }
    }

    #[allow(dead_code)]
    pub fn from_key(key: &str) -> Option<MarkKind> {
        ALL_KINDS.into_iter().find(|k| k.key() == key)
    }

    /// BAR's English label (`language/en/interface.json`, `milestone`).
    pub fn label(self) -> &'static str {
        match self {
            MarkKind::Tech2 => "Tech 2",
            MarkKind::Tech3 => "Tech 3",
            MarkKind::Nuke => "First nuke",
            MarkKind::Antinuke => "First anti-nuke",
            MarkKind::Lrpc => "First long-range cannon",
            MarkKind::CommanderLost => "Commander lost",
            MarkKind::FirstKill => "First kill",
            MarkKind::FirstLoss => "First loss",
            MarkKind::Moho => "First T2 extractor",
            MarkKind::Converter => "First converter",
            MarkKind::AdvConverter => "First advanced converter",
            MarkKind::Fusion => "First fusion reactor",
            MarkKind::Afus => "First advanced fusion",
            MarkKind::Geo => "First geothermal plant",
            MarkKind::Radar => "First radar tower",
            MarkKind::Nano => "First construction turret",
            MarkKind::Air => "First aircraft",
            MarkKind::Naval => "First ship",
            MarkKind::CommanderKill => "Enemy commander killed",
            MarkKind::NukeLaunched => "Nuke launched",
            MarkKind::Nuked => "Hit by a nuke",
            MarkKind::EcoStrike => "Economy struck",
            MarkKind::ArmyLoss => "Military hit",
            MarkKind::Raid => "Strike dealt",
            MarkKind::TeamDied => "Out of the game",
        }
    }

    pub fn class(self) -> MarkClass {
        match self {
            MarkKind::Tech2 | MarkKind::Tech3 => MarkClass::Tech,
            MarkKind::FirstKill | MarkKind::CommanderKill | MarkKind::Raid | MarkKind::NukeLaunched => MarkClass::Attack,
            MarkKind::FirstLoss | MarkKind::CommanderLost | MarkKind::EcoStrike | MarkKind::ArmyLoss | MarkKind::Nuked => {
                MarkClass::Lost
            }
            MarkKind::TeamDied => MarkClass::Out,
            _ => MarkClass::Built,
        }
    }

    /// Which badge wins when several share a spot (BAR's `IMPORTANCE`, with the
    /// kinds it leaves out placed by their class).
    pub fn importance(self) -> f32 {
        match self {
            MarkKind::TeamDied => 1.0,
            MarkKind::CommanderLost => 0.9,
            MarkKind::CommanderKill => 0.85,
            MarkKind::Tech3 => 0.8,
            MarkKind::Tech2 | MarkKind::Nuke => 0.7,
            MarkKind::Nuked => 0.65,
            MarkKind::Lrpc | MarkKind::NukeLaunched | MarkKind::Afus => 0.6,
            MarkKind::EcoStrike | MarkKind::ArmyLoss | MarkKind::Raid => 0.55,
            MarkKind::Antinuke | MarkKind::Fusion | MarkKind::Moho => 0.5,
            MarkKind::Air | MarkKind::Naval => 0.4,
            MarkKind::Geo | MarkKind::AdvConverter => 0.35,
            MarkKind::Converter | MarkKind::FirstKill | MarkKind::FirstLoss => 0.3,
            MarkKind::Nano => 0.25,
            MarkKind::Radar => 0.2,
        }
    }

    /// A glyph from the built-in icon set, for when no picture is at hand.
    pub fn glyph(self) -> &'static str {
        match self {
            MarkKind::Tech2 | MarkKind::Tech3 => "chevrons-up",
            MarkKind::Nuke | MarkKind::NukeLaunched | MarkKind::Nuked => "radiation",
            MarkKind::Antinuke => "shield",
            MarkKind::Lrpc => "crosshair",
            MarkKind::CommanderLost | MarkKind::CommanderKill => "crown",
            MarkKind::FirstKill | MarkKind::Raid => "swords",
            MarkKind::FirstLoss | MarkKind::ArmyLoss => "heart-crack",
            MarkKind::Moho => "pickaxe",
            MarkKind::Converter | MarkKind::AdvConverter => "refresh-cw",
            MarkKind::Fusion | MarkKind::Afus | MarkKind::Geo => "zap",
            MarkKind::Radar => "radar",
            MarkKind::Nano => "wrench",
            MarkKind::Air => "plane",
            MarkKind::Naval => "ship",
            MarkKind::EcoStrike => "flame",
            MarkKind::TeamDied => "skull",
        }
    }

    /// A typical unit for this milestone in a faction, used only by the sample
    /// markers; real markers name the unit that set them off.
    pub fn typical_unit(self, faction: Faction) -> &'static str {
        let [arm, cor, leg] = match self {
            MarkKind::Tech2 => ["armalab", "coralab", "legalab"],
            MarkKind::Tech3 => ["armshltx", "corgant", "leggant"],
            MarkKind::Nuke | MarkKind::NukeLaunched => ["armsilo", "corsilo", "legsilo"],
            MarkKind::Antinuke => ["armamd", "corfmd", "legabm"],
            MarkKind::Lrpc => ["armbrtha", "corint", "leglrpc"],
            MarkKind::CommanderLost | MarkKind::CommanderKill => ["armcom", "corcom", "legcom"],
            MarkKind::FirstKill | MarkKind::Raid => ["armflash", "corgator", "leghelios"],
            MarkKind::FirstLoss | MarkKind::ArmyLoss => ["armpw", "corak", "leggob"],
            MarkKind::Moho => ["armmoho", "cormoho", "legmoho"],
            MarkKind::Converter => ["armmakr", "cormakr", "legeconv"],
            MarkKind::AdvConverter => ["armmmkr", "cormmkr", "legadveconv"],
            MarkKind::Fusion => ["armfus", "corfus", "legfus"],
            MarkKind::Afus => ["armafus", "corafus", "legafus"],
            MarkKind::Geo => ["armgeo", "corgeo", "leggeo"],
            MarkKind::Radar => ["armrad", "corrad", "legrad"],
            MarkKind::Nano => ["armnanotc", "cornanotc", "legnanotc"],
            MarkKind::Air => ["armpeep", "corfink", "legfig"],
            MarkKind::Naval => ["armpt", "corpt", "legnavyscout"],
            MarkKind::Nuked => ["armsilo", "corsilo", "legsilo"],
            MarkKind::EcoStrike => ["armfus", "corfus", "legfus"],
            MarkKind::TeamDied => [SKULL, SKULL, SKULL],
        };
        match faction {
            Faction::Armada => arm,
            Faction::Cortex => cor,
            Faction::Legion => leg,
        }
    }
}

/// The picture name of "out of the game": BAR draws `luaui/images/skull.dds`.
pub const SKULL: &str = "skull";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Faction {
    Armada,
    Cortex,
    Legion,
}

impl Faction {
    /// From the lobby's `side` value; random or unknown reads as Armada.
    pub fn from_side(side: Option<&str>) -> Faction {
        match side.map(str::to_ascii_lowercase).as_deref() {
            Some("cortex") => Faction::Cortex,
            Some("legion") => Faction::Legion,
            _ => Faction::Armada,
        }
    }
}

/// One event on the timeline.
#[derive(Clone, Debug)]
pub struct Marker {
    pub t: f32,
    pub kind: MarkKind,
    /// The team it happened to, as an index into `Data::teams`.
    pub team: usize,
    /// The unit that set it off, by unit-def name (`armfus`): the picture key.
    pub unit: Option<String>,
    /// More to say in the hover text (a strike's cost, the unit's name).
    pub note: Option<String>,
    /// Made up to try the design: never from the replay.
    pub sample: bool,
}

impl Marker {
    /// The unit that set it off, by the name BAR shows for it; by its
    /// definition's name where the game gave none.
    pub fn unit_name<'a>(&'a self, names: &'a super::icons::UnitNames) -> Option<&'a str> {
        let def = self.unit.as_deref()?;
        Some(names.get(def).unwrap_or(def))
    }

    /// The picture this marker wants, if any.
    pub fn picture(&self) -> Option<&str> {
        if self.kind == MarkKind::TeamDied {
            Some(SKULL)
        } else {
            self.unit.as_deref()
        }
    }
}

/// Markers that sit too close to draw apart, as one badge: the most important
/// first, then the rest in time order.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    pub members: Vec<usize>,
    /// Where the badge is drawn: the lead member's time.
    pub t: f32,
}

/// Gathers markers (by index, already filtered to one lane) into clusters whose
/// badges would overlap: two markers closer than `min_gap_s` share one badge.
pub fn cluster(markers: &[Marker], indices: &[usize], min_gap_s: f32) -> Vec<Cluster> {
    let mut order: Vec<usize> = indices.to_vec();
    order.sort_by(|a, b| markers[*a].t.total_cmp(&markers[*b].t));
    let mut out: Vec<Cluster> = Vec::new();
    for i in order {
        match out.last_mut() {
            Some(c) if markers[i].t - c.t < min_gap_s => c.members.push(i),
            _ => out.push(Cluster { members: vec![i], t: markers[i].t }),
        }
    }
    // The badge shows the most important; ties go to the earliest. It stands
    // where its lead happened, so a click lands there too.
    let lead = |c: &mut Cluster| {
        let lead = *c
            .members
            .iter()
            .max_by(|a, b| {
                markers[**a]
                    .kind
                    .importance()
                    .total_cmp(&markers[**b].kind.importance())
                    .then(markers[**b].t.total_cmp(&markers[**a].t))
            })
            .unwrap();
        c.members.retain(|m| *m != lead);
        c.members.insert(0, lead);
        c.t = markers[lead].t;
    };
    // A lead late in its cluster can bring its badge onto the next one's: those
    // two share a badge too, and the merged one may then reach back further.
    let mut placed: Vec<Cluster> = Vec::with_capacity(out.len());
    for mut c in out {
        lead(&mut c);
        placed.push(c);
        while placed.len() >= 2 && placed[placed.len() - 1].t - placed[placed.len() - 2].t < min_gap_s {
            let next = placed.pop().unwrap();
            let into = placed.last_mut().unwrap();
            into.members.extend(next.members);
            into.members.sort_by(|a, b| markers[*a].t.total_cmp(&markers[*b].t));
            lead(into);
        }
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(t: f32, kind: MarkKind) -> Marker {
        Marker { t, kind, team: 0, unit: None, note: None, sample: false }
    }

    #[test]
    fn badges_never_overlap_however_their_leads_fall() {
        // The second marker leads its cluster and stands 10 s before the third.
        let markers = vec![mark(0.0, MarkKind::Radar), mark(25.0, MarkKind::Nuke), mark(35.0, MarkKind::Radar)];
        let clusters = cluster(&markers, &[0, 1, 2], 30.0);
        for pair in clusters.windows(2) {
            assert!(pair[1].t - pair[0].t >= 30.0, "{:?}", clusters.iter().map(|c| c.t).collect::<Vec<_>>());
        }
        assert_eq!(clusters.iter().map(|c| c.members.len()).sum::<usize>(), 3);
    }

    #[test]
    fn keys_round_trip() {
        for k in ALL_KINDS {
            assert_eq!(MarkKind::from_key(k.key()), Some(k));
        }
        assert_eq!(MarkKind::from_key("nonsense"), None);
    }

    #[test]
    fn close_markers_share_a_badge_led_by_the_most_important() {
        let m = vec![mark(100.0, MarkKind::Radar), mark(110.0, MarkKind::Tech2), mark(400.0, MarkKind::Fusion)];
        let c = cluster(&m, &[0, 1, 2], 30.0);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].members, vec![1, 0]);
        assert_eq!(c[0].t, 110.0);
        assert_eq!(c[1].members, vec![2]);
    }

    #[test]
    fn the_skull_is_the_picture_of_a_dead_team() {
        assert_eq!(mark(1.0, MarkKind::TeamDied).picture(), Some(SKULL));
        assert_eq!(mark(1.0, MarkKind::Fusion).picture(), None);
    }
}
