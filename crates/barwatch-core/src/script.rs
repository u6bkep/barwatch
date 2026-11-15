//! Start scripts: the one that makes `spring-dedicated` host a replay, and the one a
//! viewer's engine uses to connect to it.

use crate::demo::GameData;
use std::fmt::Write;
use std::path::Path;

/// Keys of the original `[game]` section that describe the original host and must
/// not survive into the hosting script. The engine's own replay path removes the
/// same ones (`CPreGame::StartServerForDemo`).
const HOST_KEYS: [&str; 13] = [
    "hostip",
    "hostport",
    "autohostip",
    "autohostport",
    "sourceport",
    "onlylocal",
    "ishost",
    "myplayername",
    "mypasswd",
    "demofile",
    "fixedrngseed",
    "maphash",
    "modhash",
];

/// Builds the script that makes `spring-dedicated` stream `demo_path` to viewers.
///
/// The original setup text is kept byte for byte apart from the `[game]`-level host
/// keys: it is what viewers' engines build their game setup from. Every `[playerN]`
/// is marked `isfromdemo` so the server treats those slots as recorded, and the
/// recording's random seed and content hashes are passed on, without which viewers
/// load a different game than the one recorded. With the hashes given, the server
/// needs neither the game nor the map on disk.
pub fn dedicated_script(game_data: &GameData, demo_path: &Path, host_port: u16, autohost_port: u16) -> String {
    let added = format!(
        "\tdemofile={};\n\thostip=;\n\thostport={host_port};\n\tishost=1;\n\tautohostip=127.0.0.1;\n\
         \tautohostport={autohost_port};\n\tfixedrngseed={};\n\tmaphash={};\n\tmodhash={};\n",
        demo_path.display(),
        game_data.random_seed,
        hex(&game_data.map_hash),
        hex(&game_data.mod_hash),
    );

    let mut out = String::with_capacity(game_data.setup_text.len() + added.len() + 256);
    let mut depth = 0usize;
    // A section header was seen and its `{` has not been yet.
    let mut pending: Option<Section> = None;
    for line in game_data.setup_text.split_inclusive('\n') {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if depth == 1 && pending.is_none() && is_host_key(&lower) {
            continue;
        }
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
        if lower.starts_with('[') {
            pending = Some(match depth {
                0 if section_name(&lower) == Some("game") => Section::Game,
                1 if section_name(&lower).is_some_and(is_player_section) => Section::Player,
                _ => Section::Other,
            });
        }
        // A `key=value;` line never opens or closes a section, whatever its value holds.
        let structural = !lower.contains('=') || lower.starts_with(['[', '{', '}']);
        for c in trimmed.chars().filter(|_| structural) {
            match c {
                '{' => {
                    depth += 1;
                    match pending.take() {
                        Some(Section::Game) => out.push_str(&added),
                        Some(Section::Player) => out.push_str("\t\tisfromdemo=1;\n"),
                        _ => {}
                    }
                }
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    out
}

enum Section {
    Game,
    Player,
    Other,
}

fn section_name(lower_line: &str) -> Option<&str> {
    let rest = lower_line.strip_prefix('[')?;
    Some(rest[..rest.find(']')?].trim())
}

fn is_player_section(name: &str) -> bool {
    name.strip_prefix("player").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_host_key(lower_line: &str) -> bool {
    let Some((key, _)) = lower_line.split_once('=') else { return false };
    HOST_KEYS.contains(&key.trim())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Script that connects a viewer's engine to a hosting server as an extra spectator.
/// `name` must already be sanitized ([`crate::names::sanitize`]).
pub fn join_script(host: &str, port: u16, name: &str) -> String {
    format!("[GAME]\n{{\n\tHostIP={host};\n\tHostPort={port};\n\tMyPlayerName={name};\n\tIsHost=0;\n}}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::parse_script;
    use crate::demo::tests::SETUP;

    fn game_data() -> GameData {
        GameData { setup_text: SETUP.to_string(), map_hash: [0xab; 64], mod_hash: [0xcd; 64], random_seed: 42 }
    }

    #[test]
    fn hosting_script_replaces_host_keys_and_marks_players() {
        let s = dedicated_script(&game_data(), Path::new("/data/replay.sdfz"), 8480, 40001);
        let v = parse_script(&s).unwrap();
        let game = &v["game"];
        assert_eq!(game["demofile"], "/data/replay.sdfz");
        assert_eq!(game["hostport"], "8480");
        assert_eq!(game["hostip"], "");
        assert_eq!(game["ishost"], "1");
        assert_eq!(game["autohostport"], "40001");
        assert_eq!(game["fixedrngseed"], "42");
        assert_eq!(game["maphash"], "ab".repeat(64));
        assert_eq!(game["modhash"], "cd".repeat(64));
        assert!(game.get("myplayername").is_none());
        assert_eq!(game["player0"]["isfromdemo"], "1");
        assert_eq!(game["player1"]["isfromdemo"], "1");
        assert!(game["team0"].get("isfromdemo").is_none());
        assert!(game["ai0"].get("isfromdemo").is_none());
        // untouched: game content keys and anything below [game] level
        assert_eq!(game["mapname"], "Some Map v1");
        assert_eq!(game["modoptions"]["hostip"], "keep-me");
        assert_eq!(game["modoptions"]["maxspeed"], "3");
        // exactly one of each host key
        let count = |key: &str| s.lines().filter(|l| l.trim().starts_with(key)).count();
        assert_eq!(count("hostport="), 1);
        assert_eq!(count("ishost="), 1);
        // the [game]-level one we wrote, and the one inside [modoptions]
        assert_eq!(count("hostip="), 2);
    }

    #[test]
    fn hosting_script_handles_brace_on_header_line() {
        let gd = GameData { setup_text: "[GAME] {\n[PLAYER0] {\nName=A;\n}\nHostIP=x;\nMapName=M;\n}\n".into(), ..game_data() };
        let v = parse_script(&dedicated_script(&gd, Path::new("/d.sdfz"), 1, 2)).unwrap();
        assert_eq!(v["game"]["hostip"], "");
        assert_eq!(v["game"]["player0"]["isfromdemo"], "1");
        assert_eq!(v["game"]["player0"]["name"], "A");
        assert_eq!(v["game"]["mapname"], "M");
    }

    #[test]
    fn braces_inside_values_do_not_confuse_section_depth() {
        let gd = GameData {
            setup_text: "[game]\n{\n[modoptions]\n{\ntweak={a}{;\n}\nhostip=x;\n[player0]\n{\nname=A;\n}\n}\n".into(),
            ..game_data()
        };
        let s = dedicated_script(&gd, Path::new("/d.sdfz"), 1, 2);
        assert!(!s.contains("hostip=x"));
        assert!(s.contains("tweak={a}{;"));
        assert_eq!(s.matches("isfromdemo=1;").count(), 1);
    }

    #[test]
    fn join_script_is_parseable() {
        let v = parse_script(&join_script("example.org", 8480, "viewer_1")).unwrap();
        assert_eq!(v["game"]["hostip"], "example.org");
        assert_eq!(v["game"]["hostport"], "8480");
        assert_eq!(v["game"]["myplayername"], "viewer_1");
        assert_eq!(v["game"]["ishost"], "0");
    }
}
