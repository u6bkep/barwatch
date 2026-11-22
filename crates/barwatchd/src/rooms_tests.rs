//! Rooms through the daemon's HTTP surface (DESIGN §9): picking, switching and
//! coming back from history with the replay cache in between, with and without a
//! stats service; members and their engine names; re-linking and deleting; the
//! archive call; the first start after barwatch 0.3.

use crate::stats_tests::{
    game_id, hex_id, opts, quick, scratch, start_fake, synth_output, synth_replay, synth_replay_with, Daemon, GAME, GAME2, SETUP,
};
use barwatch_core::api::{Archived, JoinResponse, Phase, RoomView, ViewerState, ViewerView};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// `<base>/app/v1/<path>` with the pairing token, if one is given.
fn app_call(d: &Daemon, method: reqwest::Method, path: &str, token: Option<&str>) -> reqwest::RequestBuilder {
    let req = d.http.request(method, format!("{}/app/v1/{path}", d.base));
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// Polls the room (long-polling) until `done` says so.
async fn wait_room(d: &Daemon, id: &str, limit: Duration, done: impl Fn(&RoomView) -> bool) -> RoomView {
    let until = Instant::now() + limit;
    let mut v = d.room(id).await;
    while !done(&v) {
        assert!(Instant::now() < until, "timed out; last view {v:?}");
        let resp = d.http.get(format!("{}/app/v1/rooms/{id}?rev={}&wait=1", d.base, v.rev)).send().await.unwrap();
        v = resp.json().await.unwrap();
    }
    v
}

async fn join(d: &Daemon, id: &str, name: &str) -> JoinResponse {
    let resp = app_call(d, reqwest::Method::POST, &format!("rooms/{id}/viewers"), None)
        .json(&json!({ "name": name, "app_version": "test" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    resp.json().await.unwrap()
}

async fn report(d: &Daemon, id: &str, viewer: &str, state: &str, replay: Option<&str>) {
    let resp = app_call(d, reqwest::Method::PUT, &format!("rooms/{id}/viewers/{viewer}"), None)
        .json(&json!({ "state": state, "detail": "", "replay": replay }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[track_caller]
fn member<'a>(v: &'a RoomView, name: &str) -> &'a ViewerView {
    v.members.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("no member {name} in {:?}", v.members))
}

fn engine_names(v: &RoomView) -> Vec<(String, String)> {
    v.members.iter().map(|m| (m.name.clone(), m.engine_name.clone())).collect()
}

fn history_keys(v: &RoomView) -> Vec<String> {
    v.history.iter().map(|h| h.key.clone()).collect()
}

fn file_of(data: &Path, key: &str) -> std::path::PathBuf {
    data.join("replays").join(key).join("replay.sdfz")
}

/// A replay whose players are `players` (the first one plays, the rest spectate).
fn replay_with_players(id: [u8; 16], players: &[&str]) -> Vec<u8> {
    let mut setup = SETUP.replace("name=Alice;", &format!("name={};", players[0]));
    setup = setup.replace("name=Watcher;", &format!("name={};", players.get(1).copied().unwrap_or("Nobody")));
    synth_replay_with(id, &setup)
}

/// Installs a stand-in `spring-dedicated` that tells the daemon it listens and then
/// waits (until it is killed): enough for a run to be `Running` and to be stopped.
#[cfg(unix)]
fn fake_engine(data: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let dir = data.join("engines").join("2026.07.04");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("spring-dedicated");
    std::fs::write(
        &bin,
        "#!/bin/bash\n\
         script=\"${@: -1}\"\n\
         port=$(grep -o 'autohostport=[0-9]*' \"$script\" | head -1 | cut -d= -f2)\n\
         printf '\\x00' > /dev/udp/127.0.0.1/$port\n\
         exec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A room plays one replay (started and stopped), switches to another while
/// running, and comes back to the first from its history after the cache let the
/// file go, fetched again from the service. Then it gets a new link, and is deleted.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_room_plays_switches_and_comes_back() {
    let dir = scratch("rooms-flow");
    let data = dir.join("data");
    let a = synth_replay(GAME);
    let b = replay_with_players(GAME2, &["Zed", "Yan"]);
    let (key_a, key_b) = (game_id(), hex_id(GAME2));
    let (fake, _server, addr) = start_fake(opts(synth_output(4), &a), None).await;
    fake_engine(&data);
    let d = Daemon::start_cfg(&data, Some(addr), quick(), |c| {
        // Nothing the service can send again is kept unless a room plays it.
        c.replay_cache = 1;
        c.stats_public_url = Some(barwatch_core::api::parse_server("https://stats.example.org/x").unwrap());
    })
    .await;
    let token = d.app.mint_token("test").unwrap().secret;

    // Making a room under /app/ needs the token.
    let resp = app_call(&d, reqwest::Method::POST, "rooms", None).json(&json!({"name": "x"})).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    let resp = app_call(&d, reqwest::Method::POST, "rooms", Some(&token))
        .json(&json!({"name": " Friday\n night "}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let made: RoomView = resp.json().await.unwrap();
    assert_eq!(
        (made.name.as_str(), made.phase, made.replay.is_none(), made.history.len()),
        ("Friday night", Phase::Empty, true, 0)
    );
    let id = made.id;
    assert_eq!(app_call(&d, reqwest::Method::GET, "rooms", None).send().await.unwrap().status(), 401);
    let listed: Vec<RoomView> =
        app_call(&d, reqwest::Method::GET, "rooms", Some(&token)).send().await.unwrap().json().await.unwrap();
    assert!(listed.iter().any(|r| r.id == id));
    let resp = d.http.post(format!("{}/api/rooms/{id}/start", d.base)).send().await.unwrap();
    assert_eq!(resp.status(), 409, "nothing to start without a replay");

    // The first replay: confirmed by the service, with its page.
    let resp = d.pick_file(&id, &a).await;
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let v = wait_room(&d, &id, Duration::from_secs(10), |v| {
        v.phase == Phase::Idle && v.replay.as_ref().is_some_and(|r| r.game_id.is_some())
    })
    .await;
    let current = v.replay.unwrap();
    assert_eq!(current.key, key_a);
    assert_eq!(current.match_url.as_deref(), Some(format!("https://stats.example.org/x/match/{key_a}").as_str()));

    // Members: Alice is one of the replay's players, so her engine is someone else.
    let alice = join(&d, &id, "Alice").await;
    let zed = join(&d, &id, "Zed").await;
    let v = zed.room;
    assert_eq!((alice.name.as_str(), zed.name.as_str()), ("Alice", "Zed"));
    assert_eq!(engine_names(&v), [("Alice".into(), "Alice_2".into()), ("Zed".into(), "Zed".into())]);
    report(&d, &id, &zed.viewer_id, "ready", Some(&key_a)).await;
    assert_eq!(member(&d.room(&id).await, "Zed").state, ViewerState::Ready);

    // Start: running.
    let resp = d.http.post(format!("{}/app/v1/rooms/{id}/start", d.base)).send().await.unwrap();
    let started: RoomView = resp.json().await.unwrap();
    assert!(matches!(started.phase, Phase::Starting | Phase::Running) && started.run > 0, "{started:?}");
    let running = wait_room(&d, &id, Duration::from_secs(10), |v| v.phase == Phase::Running).await;
    assert!(running.game_port.is_some());

    // The second replay, while running: the run stops first.
    let resp = d.pick_file(&id, &b).await;
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let v: RoomView = resp.json().await.unwrap();
    assert_eq!(v.replay.as_ref().unwrap().key, key_b);
    assert!(matches!(v.phase, Phase::Preparing | Phase::Idle), "{:?}", v.phase);
    assert_eq!((v.game_port, history_keys(&v)), (None, vec![key_a.clone()]));
    // Engine names afresh for the new players; Zed's report was about the old replay.
    assert_eq!(engine_names(&v), [("Alice".into(), "Alice".into()), ("Zed".into(), "Zed_2".into())]);
    let z = member(&v, "Zed");
    assert_eq!((z.state, z.replay.as_deref()), (ViewerState::Preparing, Some(key_a.as_str())));
    report(&d, &id, &zed.viewer_id, "ready", Some(&key_b)).await;
    assert_eq!(member(&d.room(&id).await, "Zed").state, ViewerState::Ready);
    // The timeline is the new replay's now.
    assert_eq!(d.timeline(&id, None).await.replay.as_deref(), Some(key_b.as_str()));

    // The first replay's file went (the service can send it again); its record stayed.
    assert!(!file_of(&data, &key_a).exists());
    assert!(data.join("replays").join(&key_a).join("meta.json").exists());
    let v = d.room(&id).await;
    assert!(v.history[0].available);
    assert!(v.history[0].match_url.is_some());

    // Back to it from the history: fetched again.
    let replays_before = fake.count("replay");
    let resp = d.pick_json(&id, json!({ "key": key_a })).await;
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let v: RoomView = resp.json().await.unwrap();
    assert_eq!((v.replay.as_ref().unwrap().key.as_str(), history_keys(&v)), (key_a.as_str(), vec![key_b.clone()]));
    assert_eq!(fake.count("replay"), replays_before + 1);
    assert_eq!(std::fs::read(file_of(&data, &key_a)).unwrap(), a);
    assert_eq!(engine_names(&v), [("Alice".into(), "Alice_2".into()), ("Zed".into(), "Zed".into())]);
    // Picking what it already plays changes nothing.
    let resp = d.pick_json(&id, json!({ "key": key_a })).await;
    assert_eq!(resp.json::<RoomView>().await.unwrap().history.len(), 1);
    // Not in the history: refused.
    assert_eq!(d.pick_json(&id, json!({ "key": "cd".repeat(16) })).await.status(), 400);

    // Start and stop, as the app does.
    let idle = wait_room(&d, &id, Duration::from_secs(10), |v| v.phase == Phase::Idle).await;
    let resp = d.http.post(format!("{}/app/v1/rooms/{id}/start", d.base)).send().await.unwrap();
    assert!(resp.status().is_success());
    let running = wait_room(&d, &id, Duration::from_secs(10), |v| v.phase == Phase::Running).await;
    assert!(running.run > idle.run);
    let resp = d.http.post(format!("{}/app/v1/rooms/{id}/stop", d.base)).send().await.unwrap();
    assert!(resp.status().is_success());
    let stopped = wait_room(&d, &id, Duration::from_secs(15), |v| v.phase == Phase::Idle).await;
    assert_eq!(stopped.note.as_deref(), Some("stopped"));

    // A new link: the old one is gone, the room is the same.
    assert_eq!(app_call(&d, reqwest::Method::POST, &format!("rooms/{id}/relink"), None).send().await.unwrap().status(), 401);
    let resp = app_call(&d, reqwest::Method::POST, &format!("rooms/{id}/relink"), Some(&token)).send().await.unwrap();
    let relinked: RoomView = resp.json().await.unwrap();
    let new_id = relinked.id.clone();
    assert_ne!(new_id, id);
    let resp = d.http.get(format!("{}/app/v1/rooms/{id}", d.base)).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.text().await.unwrap().contains("new link"));
    assert_eq!(
        app_call(&d, reqwest::Method::POST, &format!("rooms/{id}/viewers"), None)
            .json(&json!({"name": "x", "app_version": "t"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let v = d.room(&new_id).await;
    assert_eq!(
        (v.name.as_str(), v.replay.unwrap().key, history_keys(&relinked)),
        ("Friday night", key_a.clone(), vec![key_b.clone()])
    );
    // Members keep reporting under the new id.
    report(&d, &new_id, &zed.viewer_id, "ready", Some(&key_a)).await;

    // Deleted: gone, and so is everything only it named.
    assert_eq!(app_call(&d, reqwest::Method::DELETE, &format!("rooms/{new_id}"), None).send().await.unwrap().status(), 401);
    let resp = app_call(&d, reqwest::Method::DELETE, &format!("rooms/{new_id}"), Some(&token)).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(d.http.get(format!("{}/app/v1/rooms/{new_id}", d.base)).send().await.unwrap().status(), 404);
    let until = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(data.join("replays")).unwrap().count() > 0 {
        assert!(Instant::now() < until, "replays left after deleting the room");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_dir(data.join("rooms")).unwrap().count(), 0);
    assert!(d.app.replays().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a stats service nothing can be fetched again: a room keeps the files of
/// its current replay and the 10 newest history entries, however small the cache;
/// older entries show as not kept and cannot be picked. All of it survives a
/// restart.
#[tokio::test(flavor = "multi_thread")]
async fn rooms_without_a_service() {
    let dir = scratch("rooms-noservice");
    let data = dir.join("data");
    let tweak = |c: &mut crate::state::Config| {
        c.replay_cache = 1;
        c.stats_public_url = Some(barwatch_core::api::parse_server("https://stats.example.org/").unwrap());
    };
    let d = Daemon::start_cfg(&data, None, quick(), tweak).await;
    let id = d.new_room("solo").await;
    let key = |i: u8| hex_id([i; 16]);
    for i in 1..=12u8 {
        let resp = d.pick_file(&id, &synth_replay([i; 16])).await;
        assert!(resp.status().is_success(), "{:?}", resp.text().await);
    }
    let v = d.room(&id).await;
    assert_eq!(v.replay.as_ref().unwrap().key, key(12));
    assert_eq!(history_keys(&v), (1..=11).rev().map(key).collect::<Vec<_>>());
    let available: Vec<bool> = v.history.iter().map(|h| h.available).collect();
    assert_eq!(available, [vec![true; 10], vec![false]].concat());
    // No service: no game ids, so no pages either.
    assert!(v.replay.as_ref().unwrap().match_url.is_none() && v.history.iter().all(|h| h.match_url.is_none()));
    assert!(!file_of(&data, &key(1)).exists());
    assert!(data.join("replays").join(key(1)).join("meta.json").exists());
    for i in 2..=12 {
        assert!(file_of(&data, &key(i)).exists(), "replay {i} was deleted");
    }

    // The one no longer kept cannot be picked; a kept one can, and moves to the front.
    let resp = d.pick_json(&id, json!({ "key": key(1) })).await;
    assert_eq!(resp.status(), 409);
    assert!(resp.text().await.unwrap().contains("no longer keeps"));
    let resp = d.pick_json(&id, json!({ "key": key(5) })).await;
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let v: RoomView = resp.json().await.unwrap();
    assert_eq!(v.replay.as_ref().unwrap().key, key(5));
    let want: Vec<String> = [12, 11, 10, 9, 8, 7, 6, 4, 3, 2, 1].into_iter().map(key).collect();
    assert_eq!(history_keys(&v), want);
    // Picking one already in the history by its file again: no duplicate.
    let resp = d.pick_file(&id, &synth_replay([12; 16])).await;
    let v: RoomView = resp.json().await.unwrap();
    assert_eq!(v.replay.as_ref().unwrap().key, key(12));
    let want: Vec<String> = [5, 11, 10, 9, 8, 7, 6, 4, 3, 2, 1].into_iter().map(key).collect();
    assert_eq!(history_keys(&v), want);
    let before = d.room(&id).await;

    // A restarted daemon has the same room, replays and files.
    drop(d);
    let d = Daemon::start_cfg(&data, None, quick(), tweak).await;
    let after = d.room(&id).await;
    assert_eq!((after.name.as_str(), after.phase), ("solo", Phase::Idle));
    assert_eq!(after.replay.as_ref().map(|r| (&r.key, r.picked_unix)), before.replay.as_ref().map(|r| (&r.key, r.picked_unix)));
    assert_eq!(history_keys(&after), history_keys(&before));
    assert_eq!(
        after.history.iter().map(|h| (h.available, h.picked_unix)).collect::<Vec<_>>(),
        before.history.iter().map(|h| (h.available, h.picked_unix)).collect::<Vec<_>>()
    );

    // A replay without a game id is keyed by its bytes.
    let resp = d.pick_file(&id, &synth_replay([0; 16])).await;
    let v: RoomView = resp.json().await.unwrap();
    let k = v.replay.unwrap().key;
    assert!(k.starts_with('u') && crate::state::valid_key(&k), "{k}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Engine names stay put while members come and go, and are given afresh when the
/// replay changes; a report about another replay than the current one does not show
/// as ready. A room has room for `MAX_VIEWERS`.
#[tokio::test(flavor = "multi_thread")]
async fn members_and_engine_names() {
    let dir = scratch("rooms-names");
    let d = Daemon::start(&dir.join("data"), None, quick()).await;
    let id = d.new_room("names").await;
    let a = replay_with_players([0x21; 16], &["Alice", "Bob"]);
    let b = replay_with_players([0x22; 16], &["Carol"]);
    let (key_a, key_b) = (hex_id([0x21; 16]), hex_id([0x22; 16]));
    // Members of a room without a replay get names too.
    let early = join(&d, &id, "Alice").await;
    assert_eq!(engine_names(&early.room), [("Alice".into(), "Alice".into())]);
    assert!(d.pick_file(&id, &a).await.status().is_success());

    let bob = join(&d, &id, "Bob").await;
    let carol = join(&d, &id, "Carol").await;
    let bob2 = join(&d, &id, "bob").await;
    assert_eq!(bob2.name, "bob_2", "member names are unique in the room");
    let v = d.room(&id).await;
    let names = engine_names(&v);
    assert_eq!(
        names[..3],
        [("Alice".into(), "Alice_2".into()), ("Bob".into(), "Bob_2".into()), ("Carol".into(), "Carol".into())]
    );
    let all: std::collections::HashSet<String> = names.iter().map(|(_, e)| e.to_lowercase()).collect();
    assert_eq!(all.len(), 4, "engine names clash: {names:?}");
    assert!(!all.contains("alice") && !all.contains("bob"));

    // Coming and going moves nobody else.
    let resp =
        app_call(&d, reqwest::Method::DELETE, &format!("rooms/{id}/viewers/{}", early.viewer_id), None).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    let dave = join(&d, &id, "Dave").await;
    let v = dave.room;
    let kept: Vec<(String, String)> = names[1..].to_vec();
    assert_eq!(engine_names(&v)[..3], kept[..]);
    assert_eq!(member(&v, "Dave").engine_name, "Dave");

    // Reports: about the current replay, shown; about none or another, not.
    report(&d, &id, &bob.viewer_id, "ready", Some(&key_a)).await;
    report(&d, &id, &carol.viewer_id, "ready", None).await;
    let v = d.room(&id).await;
    assert_eq!((member(&v, "Bob").state, member(&v, "Carol").state), (ViewerState::Ready, ViewerState::Preparing));

    // A new replay: names afresh, in joining order; Bob's readiness was for the old one.
    let resp = d.pick_file(&id, &b).await;
    let v: RoomView = resp.json().await.unwrap();
    assert_eq!(
        engine_names(&v),
        [
            ("Bob".into(), "Bob".into()),
            ("Carol".into(), "Carol_2".into()),
            ("bob_2".into(), "bob_2".into()),
            ("Dave".into(), "Dave".into())
        ]
    );
    assert_eq!(member(&v, "Bob").state, ViewerState::Preparing);
    report(&d, &id, &bob.viewer_id, "ready", Some(&key_b)).await;
    report(&d, &id, &carol.viewer_id, "error", Some(&key_a)).await;
    let v = d.room(&id).await;
    assert_eq!((member(&v, "Bob").state, member(&v, "Carol").state), (ViewerState::Ready, ViewerState::Preparing));
    assert_eq!(member(&v, "Carol").replay.as_deref(), Some(key_a.as_str()));
    // An unknown viewer is told to register again.
    let resp = app_call(&d, reqwest::Method::PUT, &format!("rooms/{id}/viewers/nobody"), None)
        .json(&json!({"state": "ready"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);

    // A full room.
    let full = d.new_room("full").await;
    for i in 0..crate::state::MAX_VIEWERS {
        join(&d, &full, &format!("v{i}")).await;
    }
    let resp = app_call(&d, reqwest::Method::POST, &format!("rooms/{full}/viewers"), None)
        .json(&json!({ "name": "late", "app_version": "test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `POST archive`: needs the token, refuses what is not a replay (or has no game
/// id), sends the rest at `batch` priority, answers the game id and, with a public
/// address of the service, its page. Nothing is kept here. No service: 503.
#[tokio::test(flavor = "multi_thread")]
async fn archive_sends_replays_to_the_service() {
    let dir = scratch("rooms-archive");
    let replay = synth_replay(GAME);
    let (fake, _server, addr) = start_fake(opts(synth_output(4), &replay), None).await;
    let d = Daemon::start_cfg(&dir.join("data"), Some(addr), quick(), |c| {
        c.stats_public_url = Some(barwatch_core::api::parse_server("https://stats.example.org").unwrap());
    })
    .await;
    let token = d.app.mint_token("test").unwrap().secret;
    let send = |token: Option<&str>, body: Vec<u8>| app_call(&d, reqwest::Method::POST, "archive", token).body(body).send();

    assert_eq!(send(None, replay.clone()).await.unwrap().status(), 401);
    let resp = send(Some(&token), b"junk".to_vec()).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("not a readable replay"));
    let resp = send(Some(&token), synth_replay([0; 16])).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("no game id"));
    assert_eq!(fake.count("upload"), 0, "refused before the service was asked");

    let resp = send(Some(&token), replay.clone()).await.unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    let got: Archived = resp.json().await.unwrap();
    let want = format!("https://stats.example.org/match/{}", game_id());
    assert_eq!(got, Archived { game_id: game_id(), match_url: Some(want) });
    assert_eq!(fake.priorities(), ["batch"]);
    assert!(d.app.replays().is_empty());
    assert_eq!(std::fs::read_dir(dir.join("data/replays")).unwrap().count(), 0);

    // Without the public address: no page.
    let d2 = Daemon::start(&dir.join("data2"), Some(addr), quick()).await;
    let token2 = d2.app.mint_token("test").unwrap().secret;
    let got: Archived = app_call(&d2, reqwest::Method::POST, "archive", Some(&token2))
        .body(replay.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got.match_url, None);

    // Without a service.
    let d3 = Daemon::start(&dir.join("data3"), None, quick()).await;
    let token3 = d3.app.mint_token("test").unwrap().secret;
    let resp = app_call(&d3, reqwest::Method::POST, "archive", Some(&token3)).body(replay).send().await.unwrap();
    assert_eq!(resp.status(), 503);
    assert!(resp.text().await.unwrap().contains("no statistics service"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A confirmed game has a page only when the service's public address is set.
#[tokio::test(flavor = "multi_thread")]
async fn match_url_needs_the_public_address() {
    let dir = scratch("rooms-matchurl");
    let replay = synth_replay(GAME);
    let (_fake, _server, addr) = start_fake(opts(synth_output(4), &replay), None).await;
    let d = Daemon::start(&dir.join("data"), Some(addr), quick()).await;
    let id = d.upload(&replay).await;
    let v = wait_room(&d, &id, Duration::from_secs(10), |v| v.replay.as_ref().is_some_and(|r| r.game_id.is_some())).await;
    assert_eq!(v.replay.unwrap().match_url, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first start after barwatch 0.3 removes its watch parties.
#[tokio::test(flavor = "multi_thread")]
async fn old_sessions_are_removed() {
    let dir = scratch("rooms-sessions");
    let data = dir.join("data");
    let old = data.join("sessions").join("abcdefghijklmnopqrstuv");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("meta.json"), b"{}").unwrap();
    std::fs::create_dir_all(data.join("run").join("abcdefghijklmnopqrstuv")).unwrap();
    let d = Daemon::start(&data, None, quick()).await;
    assert!(!data.join("sessions").exists());
    assert!(!data.join("run").join("abcdefghijklmnopqrstuv").exists());
    assert!(data.join("rooms").is_dir() && data.join("replays").is_dir());
    let rooms: Vec<Value> = d.http.get(format!("{}/api/rooms", d.base)).send().await.unwrap().json().await.unwrap();
    assert!(rooms.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
