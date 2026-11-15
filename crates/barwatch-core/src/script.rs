//! Start scripts: the one that makes `spring-dedicated` host a replay, and the one a
//! viewer's engine uses to connect to it.

use crate::demo::{plain_value, GameData};
use anyhow::{ensure, Context, Result};
use serde_json::{Map, Value};
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
/// `script` is the replay's setup text as parsed by [`crate::demo::parse_script`].
/// It is written out afresh rather than edited as text: the replay is a file a
/// stranger may have made, and a script assembled from the parse can only say what
/// the parse found. The `[game]`-level host keys are replaced with ours, every
/// `[playerN]` is marked `isfromdemo` so the server treats those slots as recorded,
/// and the recording's random seed and content hashes are passed on, without which
/// viewers load a different game than the one recorded. With the hashes given, the
/// server needs neither the game nor the map on disk.
pub fn dedicated_script(
    script: &Value,
    game_data: &GameData,
    demo_path: &Path,
    host_port: u16,
    autohost_port: u16,
) -> Result<String> {
    let demo = demo_path.to_str().context("the replay's path is not valid text")?;
    ensure!(plain_value(demo), "the replay's path cannot be written into a start script: {demo:?}");
    let game = script.get("game").and_then(Value::as_object).context("setup text has no [game] section")?;

    let mut out = String::with_capacity(game_data.setup_text.len() + 512);
    out.push_str("[game]\n{\n");
    for (key, value) in [
        ("demofile", demo.to_string()),
        ("hostip", String::new()),
        ("hostport", host_port.to_string()),
        ("ishost", "1".to_string()),
        ("autohostip", "127.0.0.1".to_string()),
        ("autohostport", autohost_port.to_string()),
        ("fixedrngseed", game_data.random_seed.to_string()),
        ("maphash", hex(&game_data.map_hash)),
        ("modhash", hex(&game_data.mod_hash)),
    ] {
        let _ = writeln!(out, "\t{key}={value};");
    }
    for (key, value) in game {
        match value {
            Value::Object(section) => {
                let marker = is_player_section(key).then_some("isfromdemo");
                emit_section(&mut out, key, section, 1, marker);
            }
            Value::String(text) if !HOST_KEYS.contains(&key.as_str()) => {
                let _ = writeln!(out, "\t{key}={text};");
            }
            _ => {}
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// Writes `[name] { … }`. `marker`, if given, is a key set to `1` in this section
/// whatever the original said.
fn emit_section(out: &mut String, name: &str, section: &Map<String, Value>, depth: usize, marker: Option<&str>) {
    let pad = "\t".repeat(depth);
    let _ = writeln!(out, "{pad}[{name}]\n{pad}{{");
    if let Some(key) = marker {
        let _ = writeln!(out, "{pad}\t{key}=1;");
    }
    for (key, value) in section {
        match value {
            Value::Object(sub) => emit_section(out, key, sub, depth + 1, None),
            Value::String(text) if marker != Some(key.as_str()) => {
                let _ = writeln!(out, "{pad}\t{key}={text};");
            }
            _ => {}
        }
    }
    let _ = writeln!(out, "{pad}}}");
}

fn is_player_section(name: &str) -> bool {
    name.strip_prefix("player").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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

    fn game_data(setup: &str) -> GameData {
        GameData { setup_text: setup.to_string(), map_hash: [0xab; 64], mod_hash: [0xcd; 64], random_seed: 42 }
    }

    fn host(setup: &str) -> Value {
        let script = parse_script(setup).unwrap();
        let text = dedicated_script(&script, &game_data(setup), Path::new("/data/replay.sdfz"), 8480, 40001).unwrap();
        parse_script(&text).unwrap()
    }

    #[test]
    fn hosting_script_replaces_host_keys_and_marks_players() {
        let v = host(SETUP);
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
        assert_eq!(game["player0"]["name"], "Alice");
        assert_eq!(game["modoptions"]["hostip"], "keep-me");
        assert_eq!(game["modoptions"]["maxspeed"], "3");
    }

    #[test]
    fn nothing_but_host_keys_and_the_marker_changes() {
        let mut original = parse_script(SETUP).unwrap();
        let mut hosted = host(SETUP);
        for tree in [&mut original, &mut hosted] {
            let game = tree["game"].as_object_mut().unwrap();
            game.retain(|k, _| !HOST_KEYS.contains(&k.as_str()));
            for (name, section) in game.iter_mut() {
                if is_player_section(name) {
                    section.as_object_mut().unwrap().remove("isfromdemo");
                }
            }
        }
        assert_eq!(original, hosted);
    }

    #[test]
    fn a_replay_cannot_set_host_keys() {
        // each element on one line, a key repeated, a recorded isfromdemo=0
        let setup = "[game]{mapname=M;hostport=9999;demofile=/etc/passwd;AutoHostPort=1;\n\
                     hostport=7;[player0]{name=A;isfromdemo=0;}}";
        let script = parse_script(setup).unwrap();
        let text = dedicated_script(&script, &game_data(setup), Path::new("/d.sdfz"), 8480, 40001).unwrap();
        let count = |key: &str| text.lines().filter(|l| l.trim().starts_with(key)).count();
        assert_eq!(count("hostport="), 1);
        assert_eq!(count("demofile="), 1);
        assert_eq!(count("autohostport="), 1);
        assert_eq!(count("isfromdemo="), 1);
        let v = parse_script(&text).unwrap();
        assert_eq!(v["game"]["hostport"], "8480");
        assert_eq!(v["game"]["demofile"], "/d.sdfz");
        assert_eq!(v["game"]["player0"]["isfromdemo"], "1");
    }

    #[test]
    fn odd_replay_paths_are_refused() {
        let script = parse_script(SETUP).unwrap();
        for path in ["/data/a;b/replay.sdfz", "/data/]]/replay.sdfz", "/data/a\nb.sdfz"] {
            assert!(dedicated_script(&script, &game_data(SETUP), Path::new(path), 1, 2).is_err(), "{path}");
        }
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
