//! Canned window states, for screenshots (`BARWATCH_FIXTURE=<name>` with
//! `DAMASCENE_HEADLESS_SHOT=<file>`) and for the layout lints in the tests.

use crate::backend::Backend;
use crate::client::{Stage, Update};
use crate::config::{Config, PairedServer};
use crate::datadir::DataDir;
use crate::position::{Position, Report};
use crate::replays::LocalReplay;
use crate::ui::Ui;
use barwatch_core::api::{
    JumpView, Link, Phase, ReplayInfo, SessionView, TimelineState, TimelineView, ViewerState, ViewerView,
};
use barwatch_core::timeline::TimelineDoc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SCENES: [&str; 22] = [
    "empty",
    "preparing",
    "ready",
    "ready-statistics",
    "playing",
    "failed",
    "replays",
    "settings",
    "consent",
    "timeline-asking",
    "timeline-queued",
    "timeline-building",
    "timeline-older",
    "timeline-complete",
    "timeline-marks",
    "timeline-teams",
    "timeline-unavailable",
    "timeline-stopped",
    "timeline-outage",
    "timeline-jump",
    "timeline-silent",
    "timeline-over",
];

/// Timeline documents for the scenes and the tests: two real ones, and what
/// the scenes make of them.
///
/// The files under `fixtures/` are the output of
/// `cargo run -p barwatch-core --example timeline_doc -- <replay> <raw output>`
/// with every number rounded (totals to whole numbers, the rest to three or four
/// digits) to keep them small:
/// - `duel-1v1.json`: a 21-minute 1v1, simulated by a stats service with the
///   companion widget, so it has every series from the first sample;
/// - `ai-18teams.json`: two players against sixteen AIs, from a stats service
///   without the companion; cut to its first twenty minutes and to the series
///   the scenes use, and told as if it had ended there.
pub mod docs {
    use barwatch_core::timeline::{Mark, TimelineDoc};

    pub fn duel() -> TimelineDoc {
        serde_json::from_str(include_str!("../fixtures/duel-1v1.json")).expect("fixture document")
    }

    pub fn teams() -> TimelineDoc {
        serde_json::from_str(include_str!("../fixtures/ai-18teams.json")).expect("fixture document")
    }

    /// `doc` as it stood when the simulation had got to `frames`.
    pub fn prefix(doc: &TimelineDoc, frames: u32) -> TimelineDoc {
        let mut doc = doc.clone();
        let frames = frames.min(doc.frames);
        let slots = (frames / doc.period_frames.max(1)) as usize + 1;
        doc.complete = false;
        doc.frames = frames;
        doc.rev = u64::from(frames / 30);
        for per_team in doc.series.values_mut() {
            for series in per_team {
                series.truncate(slots);
            }
        }
        doc.marks.retain(|m| m.frame <= frames);
        doc
    }

    /// `doc` as a stats service without the companion widget gives it: never
    /// the companion's income and storage levels, and the engine's own
    /// counters only once the simulation is over.
    pub fn older_service(doc: &TimelineDoc) -> TimelineDoc {
        const COMPANION: [&str; 6] = ["metalIncome", "metalExpense", "metalLevel", "energyIncome", "energyExpense", "energyLevel"];
        const AT_THE_END: [&str; 19] = [
            "metalProduced",
            "metalUsed",
            "metalExcess",
            "metalSent",
            "metalReceived",
            "energyProduced",
            "energyUsed",
            "energyExcess",
            "energySent",
            "energyReceived",
            "damageDealt",
            "damageReceived",
            "unitsProduced",
            "unitsDied",
            "unitsKilled",
            "unitsReceived",
            "unitsSent",
            "unitsCaptured",
            "unitsOutCaptured",
        ];
        let mut doc = doc.clone();
        let gone = |key: &str| COMPANION.contains(&key) || (!doc.complete && AT_THE_END.contains(&key));
        let keys: Vec<String> = doc.metrics.iter().map(|m| m.key.clone()).filter(|k| gone(k)).collect();
        doc.metrics.retain(|m| !keys.contains(&m.key));
        doc.series.retain(|key, _| !keys.contains(key));
        // The marks that need the companion's unit facts.
        doc.marks.retain(|m| !matches!(m.kind.as_str(), "air" | "naval" | "geo" | "tech2" | "tech3"));
        doc
    }

    /// `doc` with made-up milestones of the kinds the duel never reached,
    /// so that their badges are seen: MADE UP, for the fixtures only.
    pub fn with_made_up_tech(doc: &TimelineDoc) -> TimelineDoc {
        let mut doc = doc.clone();
        let mut add = |minute: u32, second: u32, team: u32, kind: &str, def: &str| {
            let frame = (minute * 60 + second) * 30;
            if frame <= doc.frames {
                doc.marks.push(Mark { frame, team, kind: kind.into(), def: Some(def.into()) });
            }
        };
        add(8, 40, 0, "tech2", "coralab");
        add(9, 55, 1, "tech2", "coravp");
        add(10, 30, 0, "moho", "cormoho");
        add(13, 20, 0, "fusion", "corfus");
        add(14, 5, 1, "fusion", "corfus");
        add(15, 40, 1, "antinuke", "corfmd");
        add(16, 10, 0, "advConverter", "cormmkr");
        add(17, 0, 0, "tech3", "corgant");
        add(17, 45, 0, "afus", "corafus");
        add(18, 30, 0, "nuke", "corsilo");
        add(19, 10, 1, "lrpc", "corint");
        add(20, 20, 0, "commanderKill", "corcom");
        doc.marks.sort_by_key(|m| m.frame);
        doc
    }
}

fn replay(map: &str, start_unix: u64, duration_s: u32, teams: &[&[&str]]) -> ReplayInfo {
    ReplayInfo {
        file_name: format!("2026-10-08_02-51-30-455_{map}_2026.07.04.sdfz"),
        engine: "2026.07.04".into(),
        game: "Beyond All Reason test-31516-95f3e26".into(),
        map: map.into(),
        map_file: None,
        start_unix,
        duration_s,
        teams: teams.iter().map(|t| t.iter().map(|n| n.to_string()).collect()).collect(),
        spectators: vec!["Kestrel".into()],
    }
}

fn session(phase: Phase, viewers: Vec<ViewerView>) -> SessionView {
    SessionView {
        id: "abcdefghijklmnopqrstuv".into(),
        rev: 7,
        created_unix: 1_791_427_890,
        replay: replay("Comet Catcher Remake 1.8", 1_791_427_890, 834, &[&["Moth", "Quill"], &["Ember", "Tarn"]]),
        phase,
        note: None,
        game_host: None,
        game_port: (phase == Phase::Running).then_some(8480),
        run: u32::from(phase == Phase::Running),
        paused: false,
        speed: 1.0,
        ended: false,
        viewers,
        game_id: None,
        jump: None,
    }
}

fn viewer(name: &str, state: ViewerState, detail: &str, in_game: bool) -> ViewerView {
    ViewerView { name: name.into(), state, detail: detail.into(), in_game }
}

/// What the stand-in for the engine reports: the game at `seconds`, paused,
/// so that the scene stands still.
pub(crate) fn at(seconds: u32) -> Option<Position> {
    Some(Position { report: Report { frame: seconds * 30, speed: 1.0, paused: true }, seen: Instant::now() })
}

pub(crate) fn view(state: TimelineState, note: Option<&str>, doc: Option<TimelineDoc>) -> Option<Arc<TimelineView>> {
    Some(Arc::new(TimelineView { rev: doc.as_ref().map_or(1, |d| d.rev), state, note: note.map(str::to_owned), doc }))
}

/// A party whose game is running with this viewer in it, on the duel or on
/// the 18-team game.
pub(crate) fn watching(teams: bool, position: Option<Position>, timeline: Option<Arc<TimelineView>>) -> Update {
    let mut session = session(
        Phase::Running,
        vec![viewer("Sorrel", ViewerState::Playing, "", true), viewer("Wren", ViewerState::Playing, "", true)],
    );
    session.replay = if teams {
        let sides: [&[&str]; 2] = [&["[gecko]thebluegecko", "[gecko]u6bkep"], &["BARbarianAI (16)"]];
        ReplayInfo {
            game: "Beyond All Reason test-31241-7e01f7d".into(),
            ..replay("All That Glitters v2.2.3", 1_791_427_890, 1200, &sides)
        }
    } else {
        ReplayInfo {
            game: "Beyond All Reason test-31653-ebba846".into(),
            ..replay("Scylla and Charybdis 1.1", 1_791_427_890, 1277, &[&["[MADO]vault"], &["[MADO]f1sher"]])
        }
    };
    session.game_id = Some("ff3bed6af9c40836c48f467c8374047b".into());
    let engine_run = Some(session.run);
    Update {
        session: Some(session),
        stage: Stage::Playing,
        name: "Sorrel".into(),
        notice: None,
        position,
        timeline,
        jump_refused: false,
        engine_run,
    }
}

/// The window in the named state, or `None` for an unknown name.
pub fn scene(name: &str) -> Option<Ui> {
    let cfg = Config {
        name: "Sorrel".into(),
        data_dir: None,
        servers: vec![PairedServer { url: "https://watch.example.org/".into(), token: "x".repeat(40) }],
    };
    // DEVELOPMENT: `BARWATCH_FIXTURE_DATA` names a real BAR data folder to read
    // marker pictures and unit names from (read only), for the scenes whose
    // game version is installed there.
    let art = std::env::var_os("BARWATCH_FIXTURE_DATA").filter(|v| !v.is_empty());
    let root = art.clone().map(Into::into).unwrap_or_else(|| "/home/sorrel/.local/state/Beyond All Reason".into());
    let data = DataDir { root, own: false };
    let server = url::Url::parse("https://watch.example.org/").unwrap();
    let mut ui = Ui::new(Backend::inert(), cfg, data, None);
    ui.fixture_done_scanning();
    let party = |stage: Stage, phase: Phase, viewers: Vec<ViewerView>| {
        let session = session(phase, viewers);
        Update {
            engine_run: (stage == Stage::Playing).then_some(session.run),
            session: Some(session),
            stage,
            name: "Sorrel".into(),
            notice: None,
            position: None,
            timeline: None,
            jump_refused: false,
        }
    };
    let duel = docs::duel();
    match name {
        "empty" => {}
        "preparing" => ui.fixture_party(
            server,
            party(
                Stage::Preparing { what: "Downloading engine 2026.07.04".into(), fraction: Some(0.43) },
                Phase::Idle,
                vec![
                    viewer("Sorrel", ViewerState::Preparing, "Downloading engine 2026.07.04 43%", false),
                    viewer("Wren", ViewerState::Ready, "", false),
                ],
            ),
        ),
        "ready" => ui.fixture_party(
            server,
            party(
                Stage::Ready,
                Phase::Idle,
                vec![viewer("Sorrel", ViewerState::Ready, "", false), viewer("Wren", ViewerState::Ready, "", false)],
            ),
        ),
        // Before the game: one line says where the statistics stand.
        "ready-statistics" => {
            let mut update = party(
                Stage::Ready,
                Phase::Idle,
                vec![viewer("Sorrel", ViewerState::Ready, "", false), viewer("Wren", ViewerState::Ready, "", false)],
            );
            update.timeline = view(TimelineState::Building, None, Some(docs::prefix(&duel, 9_000)));
            ui.fixture_party(server, update);
        }
        "playing" => ui.fixture_party(
            server,
            party(
                Stage::Playing,
                Phase::Running,
                vec![
                    viewer("Sorrel", ViewerState::Playing, "", true),
                    viewer("Wren", ViewerState::Playing, "", true),
                    viewer("Alder", ViewerState::Preparing, "Checking map Comet Catcher Remake 1.8", false),
                ],
            ),
        ),
        "failed" => ui.fixture_party(
            server,
            party(
                Stage::Failed("could not download engine 2026.07.04: the release has no build for this system".into()),
                Phase::Idle,
                vec![viewer("Sorrel", ViewerState::Error, "could not download engine 2026.07.04", false)],
            ),
        ),
        // In game, the server has not answered about statistics yet.
        "timeline-asking" => ui.fixture_party(server, watching(false, at(45), None)),
        "timeline-queued" => ui.fixture_party(server, watching(false, at(150), view(TimelineState::Queued, None, None))),
        // The game (9:40) is ahead of the numbers (8:00).
        "timeline-building" => ui.fixture_party(
            server,
            watching(false, at(580), view(TimelineState::Building, None, Some(docs::prefix(&duel, 14_400)))),
        ),
        // A stats service without the companion, part-way: only its own tally.
        "timeline-older" => {
            let doc = docs::older_service(&docs::prefix(&duel, 28_800));
            ui.fixture_party(server, watching(false, at(420), view(TimelineState::Building, None, Some(doc))));
        }
        "timeline-complete" => {
            ui.fixture_party(server, watching(false, at(750), view(TimelineState::Complete, None, Some(duel))))
        }
        "timeline-marks" => {
            let doc = docs::with_made_up_tech(&duel);
            ui.fixture_party(server, watching(false, at(1140), view(TimelineState::Complete, None, Some(doc))));
        }
        "timeline-teams" => {
            ui.fixture_party(server, watching(true, at(900), view(TimelineState::Complete, None, Some(docs::teams()))));
            if let Some(timeline) = ui.fixture_timeline() {
                timeline.fixture_by_player();
            }
        }
        // A server older than the timeline: the client makes this of its 404.
        "timeline-unavailable" => ui.fixture_party(
            server,
            watching(
                false,
                at(310),
                view(TimelineState::Unavailable, Some("This server does not offer statistics: it is older than they are."), None),
            ),
        ),
        // The stats service gave up on the game at 6:00; the game is at 9:00.
        "timeline-stopped" => ui.fixture_party(
            server,
            watching(
                false,
                at(540),
                view(
                    TimelineState::Failed,
                    Some("The statistics service could not work out this game: the simulation ended early."),
                    Some(docs::prefix(&duel, 10_800)),
                ),
            ),
        ),
        // The stats service cannot be reached since 6:00; the server keeps trying.
        "timeline-outage" => ui.fixture_party(
            server,
            watching(
                false,
                at(540),
                view(
                    TimelineState::Unavailable,
                    Some("The statistics service cannot be reached; trying again."),
                    Some(docs::prefix(&duel, 10_800)),
                ),
            ),
        ),
        // Another viewer jumped everyone from 6:00 to 14:00; this engine is on its way.
        "timeline-jump" => {
            let stats = view(TimelineState::Complete, None, Some(duel));
            ui.fixture_party(server, watching(false, at(360), stats.clone()));
            let mut update = watching(false, at(372), stats);
            if let Some(session) = &mut update.session {
                session.jump = Some(JumpView { target_s: 840, at_unix: 1_791_428_400 });
            }
            ui.fixture_update(update);
        }
        // The game has not said where it is for a while (user widgets off in BAR).
        "timeline-silent" => {
            ui.fixture_party(server, watching(false, None, view(TimelineState::Complete, None, Some(duel))));
            if let Some(timeline) = ui.fixture_timeline() {
                timeline.fixture_silent_for(Duration::from_secs(60));
            }
        }
        // The replay has played to its end; the engine is still open.
        "timeline-over" => {
            let stats = view(TimelineState::Complete, None, Some(duel));
            ui.fixture_party(server, watching(false, at(1200), stats.clone()));
            let mut update = watching(false, at(1271), stats);
            if let Some(session) = &mut update.session {
                session.ended = true;
            }
            ui.fixture_update(update);
        }
        "replays" => {
            let maps =
                ["Comet Catcher Remake 1.8", "Supreme Isthmus v2.1", "All That Glitters v2.2.3", "Quicksilver Remake 1.24"];
            let list: Vec<LocalReplay> = (0..40)
                .map(|i| {
                    let info = replay(
                        maps[i % maps.len()],
                        1_791_427_890 - i as u64 * 86_400,
                        600 + i as u32 * 37,
                        &[&["Moth", "Quill"], &["Ember", "Tarn"]],
                    );
                    LocalReplay {
                        path: format!("/demos/{}", info.file_name).into(),
                        file_name: info.file_name.clone(),
                        info: (i != 5).then_some(info),
                    }
                })
                .collect();
            ui.fixture_replays(list, Some(1));
        }
        "settings" => ui.fixture_tab("settings"),
        "consent" => ui.fixture_pending(Link::Join { server, session: "abcdefghijklmnopqrstuv".into() }),
        _ => return None,
    }
    if art.is_some() {
        ui.fixture_wait_for_art(Duration::from_secs(30));
    }
    Some(ui)
}

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::prelude::*;

    /// What the timeline's charts do on purpose: a team's colour is data, not
    /// a token, and the marks drawn on a track's time area (dots on a line, a
    /// badge's corner sign, the glow of a position line) stand out over the
    /// edges of what they are drawn in. Everything else of the timeline, its
    /// text, its gutter and its controls, is held to every lint like the rest.
    fn by_design(finding: &Finding) -> bool {
        let timeline = finding.source.file.contains("timeline/")
            || finding.node_id.contains("[tl:")
            || finding.node_id.split('.').any(|part| part.starts_with("timeline_") || part.starts_with("tl_"));
        // Drawn on a time area: inside one, not merely of a drawn kind.
        let on_track = finding.node_id.contains(".timeline_area");
        let spills = match finding.kind {
            FindingKind::Overflow => finding.message.contains("child overflows parent"),
            FindingKind::CornerStackup => true,
            _ => false,
        };
        timeline && (finding.kind == FindingKind::RawColor || (on_track && spills))
    }

    /// Every scene lays out without a lint finding: no overflowing text, no raw
    /// colours, no clipped focus rings, no duplicate keys, no unnamed
    /// controls; the timeline's charts as [`by_design`] allows.
    #[test]
    fn scenes_are_lint_clean() {
        let theme = Theme::default();
        let mut report = String::new();
        for name in SCENES {
            let app = scene(name).unwrap();
            // The default window, and a narrow one.
            for (w, h) in [crate::WINDOW, (760.0, 700.0)] {
                let cx = BuildCx::new(&theme).with_viewport(w, h);
                let mut root = app.build(&cx);
                let mut bundle = damascene_core::render_bundle_themed(&mut root, Rect::new(0.0, 0.0, w, h), &theme);
                bundle.lint.findings.retain(|f| !by_design(f));
                if !bundle.lint.findings.is_empty() {
                    report.push_str(&format!("scene {name} at {w}x{h}:\n{}\n", bundle.lint.text()));
                }
            }
        }
        assert!(report.is_empty(), "{report}");
    }

    #[test]
    fn unknown_scene() {
        assert!(scene("nope").is_none());
    }
}
