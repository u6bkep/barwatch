//! Canned window states, for screenshots (`BARWATCH_FIXTURE=<name>` with
//! `DAMASCENE_HEADLESS_SHOT=<file>`) and for the layout lints in the tests.

use crate::backend::Backend;
use crate::client::{Stage, Update};
use crate::config::{Config, PairedServer};
use crate::datadir::DataDir;
use crate::replays::LocalReplay;
use crate::ui::Ui;
use barwatch_core::api::{Link, Phase, ReplayInfo, SessionView, ViewerState, ViewerView};

pub const SCENES: [&str; 8] = ["empty", "preparing", "ready", "playing", "failed", "replays", "settings", "consent"];

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

/// The window in the named state, or `None` for an unknown name.
pub fn scene(name: &str) -> Option<Ui> {
    let cfg = Config {
        name: "Sorrel".into(),
        data_dir: None,
        servers: vec![PairedServer { url: "https://watch.example.org/".into(), token: "x".repeat(40) }],
    };
    let data = DataDir { root: "/home/sorrel/.local/state/Beyond All Reason".into(), own: false };
    let server = url::Url::parse("https://watch.example.org/").unwrap();
    let mut ui = Ui::new(Backend::inert(), cfg, data, None);
    ui.fixture_done_scanning();
    let party = |stage: Stage, phase: Phase, viewers: Vec<ViewerView>| Update {
        session: Some(session(phase, viewers)),
        stage,
        name: "Sorrel".into(),
        notice: None,
        position: None,
    };
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
    Some(ui)
}

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::prelude::*;

    /// Every scene lays out without a lint finding: no overflowing text, no raw
    /// colours, no clipped focus rings, no duplicate keys.
    #[test]
    fn scenes_are_lint_clean() {
        let theme = Theme::default();
        let mut report = String::new();
        for name in SCENES {
            let app = scene(name).unwrap();
            let cx = BuildCx::new(&theme).with_viewport(980.0, 700.0);
            let mut root = app.build(&cx);
            let bundle = damascene_core::render_bundle_themed(&mut root, Rect::new(0.0, 0.0, 980.0, 700.0), &theme);
            if !bundle.lint.findings.is_empty() {
                report.push_str(&format!("scene {name}:\n{}\n", bundle.lint.text()));
            }
        }
        assert!(report.is_empty(), "{report}");
    }

    #[test]
    fn unknown_scene() {
        assert!(scene("nope").is_none());
    }
}
