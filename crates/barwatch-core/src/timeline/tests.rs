use super::*;
use serde_json::{json, Value};

fn team(team: u32, ally: u32) -> TeamEntry {
    TeamEntry {
        team,
        ally,
        name: format!("p{team}"),
        is_ai: false,
        side: Some("armada".into()),
        color: Some("#102030".into()),
    }
}

/// Teams 0 and 2 are allies; team 1 is the enemy.
fn builder() -> TimelineBuilder {
    TimelineBuilder::new(vec![team(0, 0), team(1, 1), team(2, 0)])
}

fn line(v: Value) -> String {
    v.to_string()
}

/// A Gex `unit_def` with harmless defaults, changed by `extra`.
fn udef(id: u32, name: &str, extra: Value) -> String {
    let mut d = json!({
        "action": "unit_def", "frame": 0, "defID": id, "defName": name, "metalCost": 100, "energyCost": 600,
        "speed": 0, "unitGroup": "", "radarDistance": 0, "isCommander": false, "isFactory": false,
        "isReclaimer": false, "energyProduction": 0, "extractsMetal": 0, "energyConversionCapacity": 0,
        "weaponCount": 0
    });
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    line(d)
}

/// A companion `bs_unit_def` (bar-replays DESIGN §9.7).
fn bsdef(id: u32, name: &str, cp: Value, extra: Value) -> String {
    let mut d = json!({
        "action": "bs_unit_def", "frame": 0, "defID": id, "defName": name, "customParams": cp,
        "isBuilder": false, "canFly": false, "minWaterDepth": -10000000
    });
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    line(d)
}

fn created(frame: u32, unit: u32, team: u32, def: u32) -> String {
    line(json!({"action": "unit_created", "frame": frame, "unitID": unit, "teamID": team, "defID": def, "defName": "x"}))
}

fn finished(frame: u32, unit: u32, team: u32) -> String {
    line(json!({"action": "unit_finished", "frame": frame, "unitID": unit, "teamID": team}))
}

fn killed(frame: u32, unit: u32, team: u32, def: u32, attacker: Option<u32>) -> String {
    line(json!({
        "action": "unit_killed", "frame": frame, "unitID": unit, "teamID": team, "defID": def, "defName": "x",
        "attackerID": attacker.map(|_| 1), "attackerDefID": attacker.map(|_| 1), "attackerTeam": attacker,
        "weaponDefID": 3
    }))
}

fn extra(frame: u32, team: u32, value: f64) -> String {
    line(json!({
        "action": "extra_stat_update", "frame": frame, "teamID": team, "totalValue": value, "armyValue": 1,
        "defValue": 2, "utilValue": 3, "metalCurrent": 4, "energyCurrent": 5, "ecoValue": 6, "otherValue": 7,
        "buildPowerAvailable": 300, "buildPowerUsed": 75, "actions": 9
    }))
}

fn team_stats(frame: u32, team: u32, produced: f64) -> String {
    // As Gex writes them: no frame of its own, the engine's in the history entry.
    line(json!({
        "action": "team_stats", "teamID": team, "frame": frame, "time": frame / 30, "metalProduced": produced,
        "energyProduced": 1, "metalUsed": 1, "energyUsed": 1, "metalExcess": 0, "energyExcess": 0,
        "metalSent": 0, "energySent": 0, "metalReceived": 0, "energyReceived": 0, "damageDealt": 0,
        "damageReceived": 0, "unitsProduced": 1, "unitsDied": 0, "unitsKilled": 0, "unitsReceived": 0,
        "unitsSent": 0, "unitsCaptured": 0, "unitsOutCaptured": 0
    }))
}

fn bs_resources(frame: u32, team: u32, income: f64) -> String {
    line(json!({
        "action": "bs_team_resources", "frame": frame, "teamID": team,
        "metalCurrent": 250, "metalStorage": 1000, "metalPull": 3, "metalIncome": income, "metalExpense": 2,
        "metalShare": 0.99, "metalSent": 0, "metalReceived": 0,
        "energyCurrent": 0, "energyStorage": 0, "energyPull": 1, "energyIncome": 20, "energyExpense": 10,
        "energyShare": 0.95, "energySent": 0, "energyReceived": 0
    }))
}

fn cursor(next: u64, frame: u32, complete: bool) -> String {
    line(json!({"action": "_cursor", "next": next, "frame": frame, "complete": complete}))
}

fn feed_lines(b: &mut TimelineBuilder, lines: &[String]) {
    for l in lines {
        b.feed_line(l);
    }
}

fn doc_of(lines: &[String]) -> TimelineDoc {
    let mut b = builder();
    feed_lines(&mut b, lines);
    b.document()
}

fn kinds(doc: &TimelineDoc) -> Vec<(u32, &str, Option<&str>)> {
    doc.marks.iter().map(|m| (m.team, m.kind.as_str(), m.def.as_deref())).collect()
}

/// A small game: defs, a commander each, building, fighting, samples, the end.
fn game() -> Vec<String> {
    vec![
        line(json!({"action": "init", "frame": 0, "version": 1})),
        udef(1, "armcom", json!({"isCommander": true, "isReclaimer": true, "speed": 37.5, "unitGroup": "builder"})),
        udef(2, "corcom", json!({"isCommander": true, "isReclaimer": true, "speed": 37.5, "unitGroup": "builder"})),
        udef(3, "armrad", json!({"radarDistance": 2100, "unitGroup": "util"})),
        udef(4, "armpw", json!({"speed": 80, "unitGroup": "weapon", "weaponCount": 1})),
        udef(5, "armalab", json!({"isFactory": true, "unitGroup": "buildert2"})),
        bsdef(5, "armalab", json!({"techlevel": "2", "unitgroup": "buildert2"}), json!({"isBuilder": true})),
        created(0, 10, 0, 1),
        finished(0, 10, 0),
        created(0, 11, 1, 2),
        finished(0, 11, 1),
        extra(0, 0, 2700.0),
        extra(0, 1, 2700.0),
        line(json!({"action": "unit_position", "frame": 300, "unitID": 10, "teamID": 0, "x": 1, "y": 2, "z": 3})),
        created(400, 12, 0, 3),
        extra(450, 0, 2760.0),
        extra(450, 1, 2700.0),
        cursor(18, 450, false),
        finished(500, 12, 0),
        created(600, 13, 1, 4),
        finished(700, 13, 1),
        "{not json".into(),
        killed(800, 13, 1, 4, Some(0)),
        created(850, 14, 0, 5),
        extra(900, 0, 3000.0),
        extra(900, 1, 2600.0),
        finished(1000, 14, 0),
        killed(1100, 11, 1, 2, Some(0)),
        line(json!({"action": "team_died", "frame": 1150, "teamID": 1})),
        line(json!({"action": "end", "frame": 1200, "realtime": 3.5, "ingame": 40})),
        team_stats(0, 0, 0.0),
        team_stats(450, 0, 900.0),
        team_stats(900, 0, 1900.0),
        team_stats(1201, 0, 2500.0),
        team_stats(0, 1, 0.0),
        team_stats(450, 1, 800.0),
        team_stats(900, 1, 1500.0),
        team_stats(1201, 1, 1600.0),
        extra(1200, 0, 3100.0),
        extra(1200, 1, 0.0),
        cursor(41, 1201, true),
    ]
}

#[test]
fn a_small_game_reads_as_expected() {
    let d = doc_of(&game());
    assert!(d.complete);
    assert_eq!(d.frames, 1201);
    assert_eq!(
        kinds(&d),
        vec![
            (0, "radar", Some("armrad")),
            (1, "firstLoss", Some("armpw")),
            (0, "firstKill", Some("armpw")),
            (0, "tech2", Some("armalab")),
            (1, "commanderLost", Some("corcom")),
            (0, "commanderKill", Some("corcom")),
            (1, "teamDied", None),
        ]
    );
    // Gex's tally: 0, 450, 900, and the final one at 1200 in the slot after.
    assert_eq!(d.series["unitValue"], vec![vec![2700.0, 2760.0, 3000.0, 3100.0], vec![2700.0, 2700.0, 2600.0, 0.0], vec![]]);
    assert_eq!(d.series["metalProduced"][0], vec![0.0, 900.0, 1900.0, 2500.0]);
    assert_eq!(d.series["buildPowerUse"][0], vec![0.25; 4]);
    let kind_of = |k: &str| d.metrics.iter().find(|m| m.key == k).map(|m| m.kind);
    assert_eq!(kind_of("metalProduced"), Some(MetricKind::Cumulative));
    assert_eq!(kind_of("unitValue"), Some(MetricKind::Level));
    assert_eq!(kind_of("actions"), Some(MetricKind::Cumulative));
    // No companion: none of its series.
    assert_eq!(kind_of("metalIncome"), None);
    assert_eq!(kind_of("metalLevel"), None);
    let b = {
        let mut b = builder();
        feed_lines(&mut b, &game());
        b.stats()
    };
    assert_eq!(b.malformed, 1);
}

#[test]
fn how_the_stream_is_cut_makes_no_difference() {
    let lines = game();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let whole = {
        let mut b = builder();
        b.feed(text.as_bytes());
        b.finish();
        b.document()
    };
    let by_line = {
        let mut b = builder();
        feed_lines(&mut b, &lines);
        b.finish();
        b.document()
    };
    let by_byte = {
        let mut b = builder();
        for byte in text.as_bytes() {
            b.feed(std::slice::from_ref(byte));
        }
        b.finish();
        b.document()
    };
    // Pieces of awkward sizes, the last one without its newline.
    let ragged = {
        let mut b = builder();
        let bytes = text.trim_end().as_bytes();
        let (mut at, mut seed) = (0usize, 7u32);
        while at < bytes.len() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let n = (1 + (seed >> 16) % 97) as usize;
            let end = (at + n).min(bytes.len());
            b.feed(&bytes[at..end]);
            at = end;
        }
        b.finish();
        b.document()
    };
    assert_eq!(whole, by_line);
    assert_eq!(whole, by_byte);
    assert_eq!(whole, ragged);
    // CRLF line ends read the same.
    let crlf = {
        let mut b = builder();
        b.feed(text.replace('\n', "\r\n").as_bytes());
        b.finish();
        b.document()
    };
    assert_eq!(whole, crlf);
}

#[test]
fn rev_and_frames_only_grow_and_rev_moves_exactly_when_the_document_does() {
    let mut b = builder();
    let mut prev = b.document();
    for l in game() {
        b.feed_line(&l);
        let now = b.document();
        assert!(now.frames >= prev.frames, "frames went back at {l}");
        let same = TimelineDoc { rev: 0, ..now.clone() } == TimelineDoc { rev: 0, ..prev.clone() };
        if same {
            assert_eq!(now.rev, prev.rev, "rev moved without a change at {l}");
        } else {
            assert_eq!(now.rev, prev.rev + 1, "change without one rev at {l}");
        }
        prev = now;
    }
    // A line that changes nothing changes no rev; finishing does once.
    let r = b.rev();
    b.feed_line(&extra(900, 0, 3000.0));
    assert_eq!(b.rev(), r);
    b.finish();
    assert_eq!(b.rev(), r, "the stream already said it was complete");
}

#[test]
fn feeding_a_stretch_again_changes_nothing() {
    // A client that resumes from an older cursor sees lines a second time.
    let lines = game();
    let mut b = builder();
    feed_lines(&mut b, &lines);
    let once = b.document();
    for from in [0, 10, 20, 30] {
        feed_lines(&mut b, &lines[from..]);
        assert_eq!(b.document(), once, "after feeding again from line {from}");
    }
}

#[test]
fn a_stream_that_ends_early_is_not_complete() {
    let lines = game();
    let cut = lines.iter().position(|l| l.contains("\"end\"")).unwrap();
    let d = doc_of(&lines[..cut]);
    assert!(!d.complete);
    assert_eq!(d.frames, 1150);
    // Before game over Gex has written none of the engine's counters.
    assert!(!d.series.contains_key("metalProduced"));
    assert_eq!(d.series["unitValue"][0], vec![2700.0, 2760.0, 3000.0]);
    assert_eq!(d.marks.len(), 7);
    // A cursor that says more is coming does not end it either; one that says
    // the end has come does.
    let mut b = builder();
    feed_lines(&mut b, &lines[..cut]);
    b.feed_line(&cursor(30, 1150, false));
    assert!(!b.is_complete());
    b.feed_line(&cursor(30, 1150, true));
    assert!(b.is_complete());
    // An empty stream: a document with teams and nothing else.
    let d = builder().document();
    assert_eq!((d.rev, d.frames, d.complete, d.marks.len(), d.metrics.len()), (0, 0, false, 0, 0));
    assert_eq!(d.teams.len(), 3);
}

#[test]
fn samples_snap_to_their_slot() {
    assert_eq!(slot_of(0), 0);
    assert_eq!(slot_of(1), 0);
    assert_eq!(slot_of(2), 0);
    assert_eq!(slot_of(3), 1);
    assert_eq!(slot_of(448), 1);
    assert_eq!(slot_of(449), 1);
    assert_eq!(slot_of(450), 1);
    assert_eq!(slot_of(451), 1);
    assert_eq!(slot_of(45490), 102);
    assert_eq!(slot_of(45491), 102);
    // The two sources a frame apart land together; a skipped slot holds the value before.
    let d = doc_of(&[
        extra(0, 0, 10.0),
        extra(449, 0, 11.0),
        team_stats(451, 0, 5.0),
        extra(1350, 0, 13.0),
        team_stats(1351, 0, 7.0),
    ]);
    assert_eq!(d.series["unitValue"][0], vec![10.0, 11.0, 11.0, 13.0]);
    assert_eq!(d.series["metalProduced"][0], vec![0.0, 5.0, 5.0, 7.0]);
    // A later record for a slot wins.
    let d = doc_of(&[extra(0, 0, 10.0), extra(1, 0, 12.0)]);
    assert_eq!(d.series["unitValue"][0], vec![12.0]);
}

#[test]
fn companion_resources_give_their_series() {
    let d = doc_of(&[bs_resources(0, 0, 1.5), bs_resources(450, 0, 2.5), bs_resources(901, 1, 4.0)]);
    assert_eq!(d.series["metalIncome"][0], vec![1.5, 2.5]);
    assert_eq!(d.series["metalIncome"][1], vec![0.0, 0.0, 4.0]);
    assert_eq!(d.series["metalExpense"][0], vec![2.0, 2.0]);
    assert_eq!(d.series["metalLevel"][0], vec![0.25, 0.25]);
    assert_eq!(d.series["energyIncome"][0], vec![20.0, 20.0]);
    assert_eq!(d.series["energyExpense"][0], vec![10.0, 10.0]);
    // No storage at all: a share of nothing is nothing.
    assert_eq!(d.series["energyLevel"][0], vec![0.0, 0.0]);
    let keys: Vec<&str> = d.metrics.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys, ["metalIncome", "metalExpense", "metalLevel", "energyIncome", "energyExpense", "energyLevel"]);
    assert!(d.metrics.iter().all(|m| m.kind == MetricKind::Level));
}

/// The companion's periodic counters fill the series while the game runs, and
/// Gex's batch at game over, which repeats them, changes nothing.
#[test]
fn companion_counters_arrive_during_the_game_and_the_final_batch_agrees() {
    let periodic = |frame: u32, produced: f64| team_stats(frame, 0, produced).replace("\"team_stats\"", "\"bs_team_stats\"");
    assert!(periodic(0, 0.0).contains("bs_team_stats"));
    let mut b = builder();
    feed_lines(&mut b, &[periodic(0, 0.0), periodic(450, 900.0), periodic(900, 1900.0)]);
    assert_eq!(b.document().series["metalProduced"][0], vec![0.0, 900.0, 1900.0]);
    let rev = b.rev();
    feed_lines(&mut b, &[team_stats(0, 0, 0.0), team_stats(450, 0, 900.0), team_stats(900, 0, 1900.0)]);
    assert_eq!(b.rev(), rev);
    feed_lines(&mut b, &[team_stats(1201, 0, 2500.0)]);
    assert_eq!(b.document().series["metalProduced"][0], vec![0.0, 900.0, 1900.0, 2500.0]);
    assert!(ACTIONS.contains(&"bs_team_stats"));
}

/// The marks one finished unit of a definition makes for team 0.
fn built_marks(gex: Value, bs: Option<Value>) -> Vec<String> {
    let mut lines = vec![udef(7, "thing", gex)];
    if let Some(bs) = bs {
        lines.push(line(bs));
    }
    lines.extend([created(30, 50, 0, 7), finished(60, 50, 0)]);
    doc_of(&lines).marks.into_iter().map(|m| m.kind).collect()
}

fn bs(cp: Value, extra: Value) -> Option<Value> {
    Some(serde_json::from_str(&bsdef(7, "thing", cp, extra)).unwrap())
}

#[test]
fn milestones_that_gex_alone_can_tell() {
    assert_eq!(built_marks(json!({"unitGroup": "nuke", "weaponCount": 2}), None), ["nuke"]);
    assert_eq!(built_marks(json!({"unitGroup": "antinuke", "radarDistance": 50}), None), ["antinuke"]);
    assert_eq!(built_marks(json!({"unitGroup": "util", "radarDistance": 2100}), None), ["radar"]);
    // A scout plane with a radar moves: no radar tower.
    assert!(built_marks(json!({"unitGroup": "util", "radarDistance": 1140, "speed": 375}), None).is_empty());
    assert_eq!(built_marks(json!({"unitGroup": "builder", "isReclaimer": true}), None), ["nano"]);
    // A constructor that moves, and a factory, are no construction turrets.
    assert!(built_marks(json!({"unitGroup": "builder", "isReclaimer": true, "speed": 40}), None).is_empty());
    assert!(built_marks(json!({"unitGroup": "builder", "isFactory": true}), None).is_empty());
    assert_eq!(built_marks(json!({"unitGroup": "metal", "energyConversionCapacity": 70}), None), ["converter"]);
    assert!(built_marks(json!({}), None).is_empty());
}

#[test]
fn milestones_that_need_the_companion_wait_for_it() {
    let fusion = json!({"unitGroup": "energy", "energyProduction": 1000});
    let afus = json!({"unitGroup": "energy", "energyProduction": 3000});
    let moho = json!({"unitGroup": "metal", "extractsMetal": 0.004});
    let conv = json!({"unitGroup": "metal", "energyConversionCapacity": 600});
    let plane = json!({"unitGroup": "weapon", "speed": 300, "weaponCount": 1});
    let ship = json!({"unitGroup": "weapon", "speed": 60, "weaponCount": 1});
    let geo = json!({"unitGroup": "energy", "energyProduction": 300});
    let lab = json!({"isFactory": true, "unitGroup": "buildert2"});
    let gant = json!({"isFactory": true, "unitGroup": "buildert3"});
    let lrpc = json!({"unitGroup": "weapon", "weaponCount": 1});
    // Without the companion: none of these, though Gex knows the converter.
    for gex in [&fusion, &afus, &moho, &plane, &ship, &geo, &lab, &gant, &lrpc] {
        assert!(built_marks(gex.clone(), None).is_empty(), "{gex}");
    }
    assert_eq!(built_marks(conv.clone(), None), ["converter"]);
    let t2 = json!({"techlevel": "2"});
    let none = json!({});
    let builder = json!({"isBuilder": true});
    assert_eq!(built_marks(lab, bs(json!({"techlevel": "2", "unitgroup": "buildert2"}), builder.clone())), ["tech2"]);
    assert_eq!(built_marks(gant, bs(json!({"techlevel": 3}), builder)), ["tech3"]);
    assert_eq!(built_marks(fusion.clone(), bs(t2.clone(), none.clone())), ["tech2", "fusion"]);
    assert_eq!(built_marks(afus, bs(t2.clone(), none.clone())), ["tech2", "fusion", "afus"]);
    // A fusion-sized plant of tech 1 is no fusion reactor; nor is a geothermal one.
    assert!(built_marks(fusion.clone(), bs(none.clone(), none.clone())).is_empty());
    assert_eq!(built_marks(fusion, bs(json!({"techlevel": "2", "geothermal": "1"}), none.clone())), ["tech2", "geo"]);
    assert_eq!(built_marks(geo, bs(json!({"geothermal": "1"}), none.clone())), ["geo"]);
    assert_eq!(built_marks(moho.clone(), bs(t2, none.clone())), ["tech2", "moho"]);
    assert!(built_marks(moho, bs(none.clone(), none.clone())).is_empty());
    let adv = json!({"techlevel": "2", "energyconv_capacity": "600"});
    assert_eq!(built_marks(conv.clone(), bs(adv, none.clone())), ["tech2", "converter", "advConverter"]);
    // With the companion its own word on converters counts, not Gex's number.
    assert!(built_marks(conv, bs(none.clone(), none.clone())).is_empty());
    assert_eq!(built_marks(lrpc, bs(json!({"islrpc": "1"}), none.clone())), ["lrpc"]);
    assert_eq!(built_marks(plane.clone(), bs(none.clone(), json!({"canFly": true}))), ["air"]);
    assert_eq!(built_marks(ship.clone(), bs(none.clone(), json!({"minWaterDepth": 12}))), ["naval"]);
    // A construction plane or ship is a builder first.
    assert!(built_marks(plane, bs(none.clone(), json!({"canFly": true, "isBuilder": true}))).is_empty());
    // A floating building needs water and does not move: no ship.
    let floating = json!({"unitGroup": "weapon", "weaponCount": 1});
    assert!(built_marks(floating, bs(none.clone(), json!({"minWaterDepth": 12}))).is_empty());
    // Without the companion's flight and water facts there is no telling.
    assert!(built_marks(ship, bs(none, json!({"canFly": null, "minWaterDepth": null}))).is_empty());
}

#[test]
fn once_per_team_kinds_are_kept_once_and_every_time_kinds_each_time() {
    let d = doc_of(&[
        udef(3, "armrad", json!({"radarDistance": 2100, "unitGroup": "util"})),
        udef(1, "armcom", json!({"isCommander": true, "speed": 37.5})),
        created(1, 20, 0, 3),
        finished(2, 20, 0),
        created(3, 21, 0, 3),
        finished(4, 21, 0),
        created(3, 22, 2, 3),
        finished(4, 22, 2),
        created(0, 30, 0, 1),
        created(0, 31, 0, 1),
        killed(100, 30, 0, 1, None),
        killed(200, 31, 0, 1, Some(1)),
    ]);
    assert_eq!(
        kinds(&d),
        vec![
            (0, "radar", Some("armrad")),
            (2, "radar", Some("armrad")),
            (0, "firstLoss", Some("armcom")),
            (0, "commanderLost", Some("armcom")),
            (0, "commanderLost", Some("armcom")),
            (1, "firstKill", Some("armcom")),
            (1, "commanderKill", Some("armcom")),
        ]
    );
    assert_eq!(d.marks[3].frame, 100);
    assert_eq!(d.marks[4].frame, 200);
}

#[test]
fn only_an_enemy_kill_of_something_worth_it_is_a_kill() {
    let base = || {
        vec![
            udef(4, "armpw", json!({"speed": 80})),
            udef(9, "freebie", json!({"metalCost": 0, "energyCost": 0})),
            created(0, 40, 0, 4),
            created(0, 41, 0, 4),
            created(0, 42, 0, 4),
            created(0, 43, 0, 9),
        ]
    };
    let kills = |extra: Vec<String>| -> Vec<u32> {
        let mut l = base();
        l.extend(extra);
        doc_of(&l).marks.into_iter().filter(|m| m.kind == "firstKill").map(|m| m.team).collect()
    };
    // By an ally, by itself, by a team the replay does not name, of a free unit.
    assert!(kills(vec![killed(10, 40, 0, 4, Some(2))]).is_empty());
    assert!(kills(vec![killed(10, 40, 0, 4, Some(0))]).is_empty());
    assert!(kills(vec![killed(10, 40, 0, 4, Some(99))]).is_empty());
    assert!(kills(vec![killed(10, 43, 0, 9, Some(1))]).is_empty());
    assert_eq!(kills(vec![killed(10, 40, 0, 4, Some(1))]), vec![1]);
    // A unit of a team the replay does not name (Gaia's) is anybody's enemy.
    assert_eq!(kills(vec![created(0, 60, 99, 4), killed(10, 60, 99, 4, Some(2))]), vec![2]);
    // Every death is a loss, killer or not.
    let d = doc_of(&[base(), vec![killed(10, 41, 0, 4, None)]].concat());
    assert_eq!(kinds(&d), vec![(0, "firstLoss", Some("armpw"))]);
}

#[test]
fn a_team_dies_once() {
    let d = doc_of(&[
        line(json!({"action": "team_died", "frame": 10, "teamID": 1})),
        line(json!({"action": "team_died", "frame": 20, "teamID": 1})),
        line(json!({"action": "team_died", "frame": 20, "teamID": 7})),
    ]);
    assert_eq!(kinds(&d), vec![(1, "teamDied", None)]);
    assert_eq!(d.marks[0].frame, 10);
}

#[test]
fn a_team_plays_the_side_of_its_commander() {
    let d = doc_of(&[
        udef(1, "legcom", json!({"isCommander": true, "speed": 37.5})),
        udef(2, "corcom", json!({"isCommander": true, "speed": 37.5})),
        udef(3, "armpw", json!({"speed": 80})),
        created(0, 10, 0, 3),
        created(0, 11, 0, 1),
        // A second commander later (given, or a mode with several) changes nothing.
        created(500, 12, 0, 2),
    ]);
    assert_eq!(d.teams[0].side.as_deref(), Some("legion"));
    assert_eq!(d.teams[1].side.as_deref(), Some("armada"));
}

#[test]
fn bad_lines_are_skipped_and_good_ones_still_count() {
    let bad: Vec<String> = vec![
        "garbage".into(),
        "{".into(),
        "[]".into(),
        "{}".into(),
        "null".into(),
        r#"{"action":5}"#.into(),
        r#"{"action":"extra_stat_update","frame":450,"totalValue":1}"#.into(),
        r#"{"action":"extra_stat_update","frame":"450","teamID":0,"totalValue":1}"#.into(),
        r#"{"action":"extra_stat_update","frame":-450,"teamID":0,"totalValue":1}"#.into(),
        r#"{"action":"extra_stat_update","frame":4.5e15,"teamID":0,"totalValue":1}"#.into(),
        r#"{"action":"extra_stat_update","frame":450.5,"teamID":0,"totalValue":1}"#.into(),
        r#"{"action":"unit_position","frame":1e300,"unitID":1}"#.into(),
        r#"{"action":"unit_finished","frame":10,"unitID":"x","teamID":0}"#.into(),
        r#"{"action":"unit_killed","frame":10,"teamID":0}"#.into(),
        r#"{"action":"unit_def","defName":"nothing"}"#.into(),
        r#"{"action":"team_died","frame":10,"teamID":-1}"#.into(),
        // Gex writes -inf bare, which is no JSON.
        r#"{"action":"extra_stat_update","frame":450,"teamID":0,"totalValue":-inf}"#.into(),
        format!("{}{}", "[".repeat(5000), "]".repeat(5000)),
    ];
    let mut lines = bad.clone();
    lines.extend([
        "".into(),
        "   ".into(),
        // A cursor with nothing readable in it still reads, and says nothing.
        r#"{"action":"_cursor","next":"soon","frame":"later","complete":"yes"}"#.into(),
        // A name too long is left out; the definition still counts.
        format!(r#"{{"action":"unit_def","frame":0,"defID":1,"defName":"{}"}}"#, "x".repeat(1000)),
        // NaN as Gex writes it: the field is left out, the rest kept.
        r#"{"action":"extra_stat_update","frame":450,"teamID":0,"totalValue":"NaN","armyValue":"Infinity","defValue":3}"#
            .into(),
        // A team the replay does not name.
        line(json!({"action": "extra_stat_update", "frame": 900, "teamID": 42, "totalValue": 1})),
    ]);
    let mut b = builder();
    feed_lines(&mut b, &lines);
    b.feed(&[0xff, 0xfe, b'{', b'\n']);
    let d = b.document();
    assert_eq!(d.series.keys().collect::<Vec<_>>(), ["valueDefense"]);
    assert_eq!(d.series["valueDefense"][0], vec![0.0, 3.0]);
    assert_eq!(d.frames, 900);
    assert!(d.marks.is_empty());
    assert!(!d.complete);
    let s = b.stats();
    assert_eq!(s.malformed, bad.len() as u64 + 1, "{s:?}");
    assert_eq!(s.lines, lines.len() as u64 - 2 + 1);
    assert_eq!(s.dropped, 1);
    // A line longer than the limit is dropped whole, however it arrives.
    let mut b = builder();
    let long = format!(r#"{{"action":"team_died","frame":5,"teamID":1,"pad":"{}"}}"#, "x".repeat(MAX_LINE));
    for piece in long.as_bytes().chunks(4096) {
        b.feed(piece);
    }
    b.feed(b"\n");
    b.feed(line(json!({"action": "team_died", "frame": 6, "teamID": 0})).as_bytes());
    b.feed(b"\n");
    assert_eq!(kinds(&b.document()), vec![(0, "teamDied", None)]);
    assert!(b.partial.capacity() <= 2 * MAX_LINE);
    assert_eq!(b.stats().malformed, 1);
}

#[test]
fn a_hostile_stream_cannot_grow_memory_past_the_limits() {
    let mut b = builder();
    b.feed_line(&udef(1, "armcom", json!({"isCommander": true})));
    for u in 0..(MAX_UNITS as u32 + 100) {
        b.feed_line(&created(0, u, 0, 1));
    }
    assert_eq!(b.units.len(), MAX_UNITS);
    for u in 0..(MAX_EVERY as u32 + 100) {
        b.feed_line(&killed(u, u, 0, 1, Some(1)));
    }
    let d = b.document();
    let count = |k: &str| d.marks.iter().filter(|m| m.kind == k).count();
    assert_eq!(count("commanderLost"), MAX_EVERY);
    assert_eq!(count("commanderKill"), MAX_EVERY);
    for id in 0..(MAX_DEFS as u32 + 100) {
        b.feed_line(&udef(id + 10, "d", json!({})));
    }
    assert_eq!(b.defs.len(), MAX_DEFS);
    // The latest frame there can be: the series reach no further than its slot.
    b.feed_line(&extra(MAX_FRAME, 0, 1.0));
    b.feed_line(&extra(MAX_FRAME + 1, 0, 2.0));
    assert_eq!(b.document().series["unitValue"][0].len(), slot_of(MAX_FRAME) + 1);
    assert_eq!(b.frames(), MAX_FRAME);
}

#[test]
fn random_bytes_and_mangled_lines_never_panic() {
    let lines = game();
    let mut seed = 0x2545_f491u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _ in 0..300 {
        let mut b = builder();
        for l in &lines {
            let mut bytes = l.clone().into_bytes();
            // Flip, drop or add a few bytes of some lines.
            if next() % 3 == 0 {
                for _ in 0..(next() % 4) {
                    if bytes.is_empty() {
                        break;
                    }
                    let i = next() as usize % bytes.len();
                    match next() % 3 {
                        0 => bytes[i] = (next() % 256) as u8,
                        1 => {
                            bytes.remove(i);
                        }
                        _ => bytes.insert(i, b"{}[]\":,0-9e"[next() as usize % 11]),
                    }
                }
            }
            bytes.push(b'\n');
            b.feed(&bytes);
        }
        b.finish();
        let d = b.document();
        assert!(d.complete);
        serde_json::to_string(&d).unwrap();
    }
}

#[test]
fn the_document_round_trips_through_json() {
    let d = doc_of(&game());
    let text = serde_json::to_string(&d).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    for key in ["v", "rev", "complete", "period_frames", "frames", "teams", "metrics", "series", "marks"] {
        assert!(v.get(key).is_some(), "{key}");
    }
    assert_eq!(v["metrics"][0], json!({"key": "metalProduced", "kind": "cumulative"}));
    assert_eq!(v["marks"].as_array().unwrap().last().unwrap(), &json!({"frame": 1150, "team": 1, "kind": "teamDied"}));
    assert_eq!(serde_json::from_str::<TimelineDoc>(&text).unwrap(), d);
}

#[test]
fn every_milestone_key_is_one_the_app_knows() {
    // The app's `MarkKind::key` strings (crates/barwatch/src/timeline/marks.rs).
    const APP: [&str; 25] = [
        "tech2",
        "tech3",
        "nuke",
        "antinuke",
        "lrpc",
        "commanderLost",
        "firstKill",
        "firstLoss",
        "moho",
        "converter",
        "advConverter",
        "fusion",
        "afus",
        "geo",
        "radar",
        "nano",
        "air",
        "naval",
        "commanderKill",
        "nukeLaunched",
        "nuked",
        "ecoStrike",
        "armyLoss",
        "raid",
        "teamDied",
    ];
    for m in Milestone::ALL {
        assert!(APP.contains(&m.key()), "{}", m.key());
        assert_eq!(Milestone::from_key(m.key()), Some(m));
    }
}

#[test]
fn a_resumed_stream_reads_its_first_line_after_a_broken_one() {
    let died = line(json!({"action": "team_died", "frame": 5, "teamID": 1}));
    let mut b = builder();
    b.feed(format!("{}\n", cursor(10, 0, false)).as_bytes());
    // The connection breaks halfway through a line ...
    b.feed(&died.as_bytes()[..20]);
    b.connection_lost();
    // ... and the resumed stream sends it again from the cursor.
    b.feed(format!("{died}\n").as_bytes());
    assert_eq!(kinds(&b.document()), vec![(1, "teamDied", None)]);
    assert_eq!(b.stats().malformed, 0);
}

#[test]
fn a_ratio_that_overflows_is_left_out() {
    let d = doc_of(&[line(json!({
        "action": "bs_team_resources", "frame": 0, "teamID": 0, "metalCurrent": 1e300, "metalStorage": 1e-300,
        "metalIncome": 1
    }))]);
    assert!(!d.series.contains_key("metalLevel"));
    assert_eq!(d.series["metalIncome"][0], vec![1.0]);
    let text = serde_json::to_string(&d).unwrap();
    assert_eq!(serde_json::from_str::<TimelineDoc>(&text).unwrap(), d);
}
