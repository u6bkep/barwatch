# barwatch: replay timeline (design)

Status (2026-11-15): **a prototype exists; nothing below is wired to a server or an
engine.** Branch `timeline-prototype`, uncommitted. This document is the brief for the
implementation work. The stats service it talks to is designed in
`../bar-replays/DESIGN.md` §9 (this repository's sibling, not yet public).

What barwatch is and how it runs today: `README.md`.

## 1. The feature

While a watch party plays, the desktop app shows the game's statistics over time on a
shared time axis: who is ahead, each side's economy, army and losses, and event
markers. The viewer can scrub those statistics freely and can ask the party to jump
forward to a moment. The app window sits on a second monitor beside the game.

It is modelled on Beyond All Reason's in-game statistics panel (TeamStats by Floris,
`luaui/Widgets/gui_teamstats.lua` and `luaui/Include/teamstats_*.lua` in the game
repository). That panel only knows the game up to the present; ours knows the whole
replay because a server simulates it ahead of the viewers.

## 2. Decisions (user, 2026-11-15) and what was rejected

- **Two positions on one time axis**: the *game position* (where the engine is) and a
  *cursor* that drives every readout. The cursor follows the game until the viewer
  moves it. Both lines are drawn across every chart.
- **No backward seek** for now. **Forward seek only by an explicit button** ("Jump
  everyone to 18:30"), never as a side effect of scrubbing, because a jump moves every
  viewer. One jump at a time.
- **The future is hidden by default** ("Hide what has not happened yet"). Spoiler
  prevention is not a major feature: what the prototype does is enough, and it does not
  extend to the party card, the replay list or the web page.
- **The game position comes from each viewer's own engine**, not from the server and
  not synchronised over the network. The web page does not need it.
- **A reporter widget is acceptable** for that, on the condition that it can never load
  in a game the user is actually playing (§5.3).
- **The stats service is generic; barwatch is its consumer.** bar-replays offers
  general tools (fetch or upload a replay and extract it, stream the extraction output)
  and contains nothing shaped for barwatch. barwatch may depend on its specifics: the
  record formats and the API. So **the timeline is built here**, from that stream
  (§4.1). REJECTED: a ready-made timeline document served by bar-replays.
- **Full data comes from the stats service** (bar-replays), which simulates the replay.
  REJECTED: a mode that shows the counters stored in a client-recorded replay until the
  simulation finishes (marginal; the service delivers results progressively).
- **barwatch depends on the stats service, never the reverse.** Both deploy in one
  compose stack and barwatchd calls the service over the stack's private network.
  REJECTED: the service polling barwatchd for work.
- **The stats service's mirror is the single store for BAR replays.** barwatchd gets
  link and id replays through it and hands uploads to it (§4.2).
- **Marker pictures are read from the viewer's installed game files.** No BAR art is
  bundled: BAR's unit pictures and icons are CC BY-NC-ND or all rights reserved, and
  its licence text forbids use "as a dependency" (`license_general.txt`,
  `license_unitpics.txt`, `license_icons.txt` in the game repository).
- **damascene is used through custom widgets**; shaders are written wherever they make
  the picture clearer. The integration crate `damascene-winit-wgpu` is a quick-start
  example: when the app needs behaviour it does not wire up, **copy it into this
  workspace and change it** instead of asking upstream (§5.5).

REJECTED during design:
- A server-side check that refuses backward jumps. The engine's dedicated server
  ignores a skip to a time at or behind its own position (`CGameServer::SkipTo`), and it
  alone knows the true position. The client simply does not offer the button.
- The stock `plot()` widget for the charts: it consumes presses for zoom and pan and has
  no area or rule marks.

## 3. Facts established by measurement

- A widget in a folder passed to the engine as `SPRING_DATADIR` loads in isolation
  mode, is enabled by default on a fresh configuration, and leaves nothing in the BAR
  folder except its name in `LuaUI/Config/BYAR.lua`. The BAR folder stays the write
  directory; maps and packages are found as before. (Headless local playback; not yet
  tried as a spectator of a barwatch server in a real window, nor on Windows.)
- BAR allows user widgets whenever the client is spectating or playing a replay
  (`luaui/barwidgets.lua`), unless the user switched user widgets off in BAR's own
  settings.
- BAR's sandbox gives a user widget no LuaSocket (`socket == nil`) but does give `io`.
  Both `Spring.Echo` and `io.stdout` reach the engine output the app already captures
  (`<data>/barwatch_engine.log`).
- The engine has no client-side control socket. The dedicated server reports neither
  frame, pause nor speed to barwatchd and drops viewers' Lua messages in replay mode.
  The engine log alone is too sparse to follow (gaps of up to 295 s in a real session).
- A jump is the engine's `/skip <seconds>` on the server (already `Control::Skip`); a
  real client caught up 4 min 17 s of game in 4.7 s.
- damascene 0.9: only keyed elements are hit targets; `vector()` stretches its view box
  to its rect; `cx.rect_of_key` gives last frame's rect; a custom shader gets five vec4
  slots per quad; a custom `.layout()` needs a fixed or fill size on both axes; `Escape`
  arrives as its own event kind, not as a key press.
- Hover: the winit host receives every cursor move and keeps the position, and the core
  stores it, but an app is told only when the hovered element changes. That is wiring in
  the host crate, not a limit of the library.

## 4. barwatchd

### 4.1 Stats client and timeline builder

New settings: `BARWATCH_STATS_URL`, `BARWATCH_STATS_TOKEN`. Unset means no stats
service: everything works as today, without a timeline.

- **On party creation** barwatchd makes sure the service knows the game, with
  `interactive` priority: `PUT /api/v1/replays` for an upload, `POST /api/v1/games`
  with the game id for a link or id. It stores the returned `game_id` with the session.
- **Reading the extraction**: barwatchd follows the service's record stream
  (`GET /api/v1/games/{game_id}/records?actions=…&from=…&follow=1`, bar-replays DESIGN
  §9.8), asking only for the record types it uses, which the builder exports as
  `timeline::ACTIONS`: `team_stats`, `bs_team_stats`, `extra_stat_update`,
  `bs_team_resources`, `unit_def`, `bs_unit_def`, `unit_created`, `unit_finished`,
  `unit_killed`, `team_died`. (`unit_finished` carries no unit type; it is joined to
  `unit_created`.) It resumes from the last `_cursor` after a drop.
- **Building the timeline**: a streaming reducer in `barwatch-core` turns those records
  into the timeline document below. It lives in the core crate so it is tested without a
  server, against recorded streams. Team names, sides and colours come from the replay
  barwatchd already holds (`barwatch_core::stats` reads the `AutoColors` message).
- **Serving it**: `GET /app/v1/sessions/{id}/timeline?rev=<n>&wait=<s>` returns
  `api::TimelineView` (`rev`, `state`: unavailable, queued, building, complete or
  failed, a `note`, and the document once there is one), long-polling for change as
  the session call does. barwatchd keeps one per session in memory and
  follows the stream while any viewer is connected. A party's id remains the
  capability, as for the rest of `/app/`.
- The service being down or slow never blocks a party; the app shows "no statistics"
  or "working out the numbers" (§5.2).

The timeline document is barwatch's own format and may change with the screen:

```
{ "v": 1, "rev": 12, "complete": false,
  "period_frames": 450, "frames": 51000,              // simulated so far
  "teams": [ {"team": 0, "ally": 0, "name": "…", "is_ai": false, "side": "armada",
              "color": "#3f6ef0"} ],
  "metrics": [ {"key": "metalProduced", "kind": "cumulative"} ],
  "series":  { "metalProduced": [[…team 0 samples…], […team 1…]] },
  "marks":   [ {"frame": 12340, "team": 1, "kind": "tech2", "def": "armalab"} ] }
```

- Sample `i` of every series is at frame `i × period_frames`. `kind` is `cumulative`
  (counter since the start; the app derives rates) or `level` (value at that moment).
- Metric keys reuse the names of BAR's TeamStats panel where the meaning is the same
  (`luaui/Include/teamstats_model.lua`):
  - the engine's counters: `metalProduced`, `energyProduced`, `metalUsed`,
    `energyUsed`, `metalExcess`, `energyExcess`, `damageDealt`, `damageReceived`,
    `unitsProduced`, `unitsDied`, `unitsKilled`, plus the sent/received/captured
    counters. Gex writes every `team_stats` record in one batch at game over, so
    during a simulation these come from the companion's periodic `bs_team_stats`
    (same key names; bar-replays DESIGN §9.7). For games extracted before the
    companion existed they appear only when the stream completes.
  - from `extra_stat_update`: `unitValue` (totalValue), `valueArmy`, `valueDefense`,
    `valueUtility`, `valueEconomy`, `metalStored` (metalCurrent), `energyStored`,
    `buildPower` (available), `buildPowerUse` (used / available, 0 to 1), `actions`.
    The `value*` keys carry BAR's names but Gex's classification (metal only; air and
    sea count as army; no factory or builder class): not comparable with BAR's panel.
  - from `bs_team_resources` (absent for games extracted before the companion widget
    existed): `metalIncome`, `metalExpense`, `energyIncome`, `energyExpense`, and
    `metalLevel`, `energyLevel` (current / storage, 0 to 1, as in BAR's panel).
  - CORRECTED 2026-11-15 (found by the builder's implementation): the first draft
    mapped `metalLevel` and `buildPowerUse` to absolute amounts and took `metalStored`
    from the companion; BAR's panel uses those keys for ratios and the current amount.
- A record stamped up to 2 frames after a multiple of `period_frames` belongs to that
  sample; anything else to the next one. The last sample can therefore lie past
  `frames`: take the game's length from `frames`, not from a series' length.
- Marks use BAR's milestone keys, which `MarkKind::from_key` already knows: `tech2`,
  `tech3`, `nuke`, `antinuke`, `lrpc`, `commanderLost`, `commanderKill`, `firstKill`,
  `firstLoss`, `moho`, `converter`, `advConverter`, `fusion`, `afus`, `geo`, `radar`,
  `nano`, `air`, `naval`, `teamDied`. Each is "first unit of a class finished by a
  team" or a kill/death event. The rules are BAR's
  (`luarules/gadgets/api_teamstats.lua`, the milestone table near line 170): port the
  rules, do not copy the code (it is GPL; this repository is not). Rules that need
  facts only `bs_unit_def` carries produce nothing when it is absent. The aggregate
  ones (`nukeLaunched`, `nuked`, `ecoStrike`, `armyLoss`, `raid`) are out of scope for
  the first version.

### 4.2 Replays through the mirror

For a link or id, barwatchd asks the service for the file
(`GET /api/v1/games/{game_id}/replay`) instead of downloading from BAR's store itself.
`source.rs`'s direct download stays as the fallback when no service is configured or it
fails. barwatchd keeps its own working copy under `/data` as now (the hosting engine
reads the file from there) with the existing expiry.

### 4.3 Session state

`SessionView` gains, all optional so older apps ignore them:
- `game_id`;
- `jump: {target_s, at_unix}`: the last jump barwatchd issued in this run, so every
  viewer's app can show a jump in flight. Cleared on a new run.

`API_VERSION` stays 1 (CORRECTED 2026-11-15: the first draft bumped it, but the app
refuses a server whose number differs, which would lock released apps out for an
addition they can ignore). `Control::Skip` is unchanged. The fields and `JumpView`
exist in `barwatch-core::api`; barwatchd fills them in T4b.

### 4.4 Web page

`/?replay=<id>` opens the page with that replay preselected (the stats console links
here, "Watch together"). No timeline on the web page for now.

### 4.5 As built (T4b, 2026-11-15)

`crates/barwatchd/src/stats_client.rs` (the calls), `timeline.rs` (registering,
following, the timeline call), `examples/fake_stats` (a stand-in service, also used by
the tests in `src/stats_tests.rs`). Choices this section did not dictate:
- `BARWATCH_STATS_URL` without `BARWATCH_STATS_TOKEN` refuses to start.
- A link or id party registers by game id and falls back to uploading the file when
  the service cannot find it. The `game_id` the service answers with must be the one
  in the replay's header. Registration retries with backoff (up to 5 min) for an hour
  after creation, then only while someone wants the timeline.
- The replay through the mirror has 30 s before barwatchd downloads it itself, and must
  carry the requested game id in its header.
- Demand for following is a registered viewer, an engine in the game, or a timeline
  request in the last 90 s; following stops 60 s after the last. At most 16 parties
  follow at once (the rest are `unavailable`, "waiting for a turn"); a turn is held
  with the builder, and a party that can read nothing (its game failed, the service
  has no record stream or refuses the token) gives it up and tries again after 30 s,
  5 min and 5 min. While a party waits like that nobody asks the service about it, so
  a service that goes away is noticed only at the next try. An unfinished
  document is dropped when following stops and read again from the start; complete
  ones are kept, 64 MiB in all, least recently used dropped first.
- Untrusted stream: nothing for 30 s drops and resumes the connection; one game's
  stream may not exceed 512 MiB; lines over 1 MiB are skipped; JSON answers over
  64 KiB are refused; the service's words are cleaned before they reach a note.
- `state`: `unavailable` (no service; unreachable for 10 s; token refused; replay
  without a game id; the record call answers 404 although the game is known, as on
  bar-replays before §9.8, asked again every 5 min; no turn free), `queued`
  (`selected`, `fetched`, `provisioned`, or not yet heard), `building` (`simulating`,
  or `simulated`/`ingested` while the stream is still being read), `complete` (the
  stream's `_cursor` said so), `failed` (`failed`/`skipped` with the service's error,
  the service refused the replay, or the stream broke a limit). The document is kept
  through `unavailable` and `failed`, except after a stream that broke a limit.
- `jump.target_s` of a relative skip is estimated from unpaused time at the speed the
  daemon set, counted from the pre-game skip (the engine reports no position); it is
  capped at the replay's length. A skip behind the last target of the run (which the
  engine ignores) leaves `jump` as it was.
- `/?replay=<32 hex digits>` fills the link field and asks for a press of Fetch; it
  never creates a party by itself. Anything else in the parameter is ignored.

## 5. Desktop app

### 5.1 What exists (prototype)

`crates/barwatch/src/timeline/` (3,300 lines) and `crates/barwatch-core/src/stats.rs`:

- `Timeline`: state and behaviour. `view(&BuildCx) -> El`, `on_event(&UiEvent, &EventCx)
  -> Option<Action>` (`Action::Jump(seconds)`), `set_game(seconds)`, `cancel_jump()`,
  `set_icons(Icons)`, `take_focus_request()`, and since T1 `pointer_moved(position,
  &EventCx) -> bool` and `layers() -> Vec<El>` (§5.5, §5.8). A host app must also return
  `timeline::shaders()` from `App::shaders`.
- `Data`: teams, sides, metrics (`MetricDef`), series, markers. Built today from the
  statistics stored in a client-recorded replay (`Data::from_replay`);
  `Data::assemble` is the general constructor.
- `marks.rs`: the 25 marker kinds, keyed as BAR's milestones are (`MarkKind::from_key`).
- `icons.rs`, `dds.rs`: pictures by unit name; a DDS decoder.
- `proto.rs`: a stand-in window with its own clock. `barwatch timeline <replay.sdfz>
  [--at s] [--paused] [--sample-markers] [--game-art <BAR checkout>] [--computed-to s]`.

Layout, as built: labels and readouts in a left column; to the right, tracks on one
axis (time ruler, an event lane per side, a lead strip with lead-change marks and fight
heat, one large chart, a compact row per metric with a tug-of-war bar). Game line,
cursor line and a fog over the unplayed part are drawn once across all tracks. The jump
button rides the cursor. "By player" turns the large chart's side panel into a ranked
list; picking a player thickens their line. With the future hidden, the axis reaches
only a rounded step beyond the game position, and every smoothed value uses past
samples only. States exist for: before the start, game over, a jump in flight,
statistics still being worked out, a narrow window, and a short window (strip only).

Verified by driving it with a simulated pointer and keyboard: click and drag move the
cursor, arrows step it, Escape returns to following, the jump button and Enter start a
jump. Not verified: Home/End, window sizes other than the two tried, anything with a
real server or engine.

### 5.2 Data from the server

- An adapter turns the timeline document into `Data` through `Data::assemble`. The
  metric list becomes what the document offers, labelled as BAR's panel labels them.
  The replay-file path (`Data::from_replay`) stays only for the prototype command.
- The client long-polls the relay (§4.1) and replaces `Data` as it grows; the cursor,
  selection and scales must survive a replacement.
- "Working out the numbers" (already built as `--computed-to`) shows wherever the
  document's `frames` ends short of what the viewer looks at.
- The lead strip switches from share of metal income to share of army value
  (`valueArmy`) when the document has it: metal income pegs to one edge in lopsided
  games.
- Marker text uses display names, not unit definition names. They come from the
  installed game (`language/en/units.json`, through the reader of §5.6); barwatchd has
  no game content.

### 5.3 Game position

A reporter widget, shipped inside the app and written at launch to an app-private
folder (`<local data>/barwatch/engine-extra/LuaUI/Widgets/barwatch_position.lua`),
which is passed to the engines the app starts as `SPRING_DATADIR`. Nothing is placed in
the BAR folder, so a game started from BAR's launcher never sees it.

- The widget removes itself unless `Spring.IsReplay()` or `Spring.GetSpectatingState()`.
- It writes `[barwatch] f=<frame> speed=<x> paused=<0|1>` to the engine's standard
  output (`io.stdout`, flushed) once a second by wall clock, and at once when speed or
  pause changes. REJECTED after the real-window test (T2): `Spring.Echo`, which put
  the line in every viewer's in-game console each second and into the console history
  BAR saves in `BYAR.lua`.
- `paused=1` also when no frame has come for 0.5 s: a pause on the server reaches a
  spectator only as frames that stop; its own pause flag stays off (measured).
- The app follows `barwatch_engine.log` (`position.rs`), accepts only lines that begin
  with `[barwatch] ` (a chat line from a viewer named "barwatch" is logged with a
  time prefix and is not taken), keeps the latest, and extrapolates from it using speed
  and pause, at most 2 s past the report. `Timeline::set_game` gets the result every
  frame.
- During catch-up (joining late, after a jump) the engine's speed factor stays 1.0
  while it runs many times faster, so the position advances in one-second steps and
  never runs ahead of the engine.
- No report for 5 s (user widgets switched off in BAR, engine closed): the timeline
  shows statistics and cursor without a game line, hides nothing, and still offers the
  jump button for any cursor position.
- When the engine restarts or the session's `run` changes, the position and any jump
  state reset.

### 5.4 Jump

`Action::Jump(seconds)` → `POST …/control` with `Control::Skip {seconds, relative:
false}`. The button shows only when the cursor is at least 10 s ahead of the game and
no jump is in flight. Every app shows a jump in flight from `SessionView.jump` (§4.3)
until its own engine reaches the target. A refused request calls
`Timeline::cancel_jump`.

### 5.5 Hover (copied host crate)

Built (T1). `crates/barwatch-host` is `damascene-winit-wgpu` 0.9.0, copied (MIT OR
Apache-2.0, its licence files kept; `crates/barwatch-host/README.md` lists every
difference). The one change in behaviour: `WinitWgpuApp::pointer_moved(position, cx) ->
bool`, called on every cursor move with the position in logical px (`None` when the
pointer leaves the window), after the events that move caused; `true` asks for a frame,
and that frame is always built (upstream's loop can turn a pointer-triggered frame into
a paint-only one while a time-driven shader is on screen). The default does nothing. An app that wants it implements `WinitWgpuApp` and starts
through `run_host_app` / `run_host_app_with_config`: `Proto` passes it to
`Timeline::pointer_moved`, `Ui` keeps the default. `damascene-core` is untouched.

What the timeline does with it:
- **A hover line** across all tracks, thin and plain beside the white game line and the
  amber cursor, with its time on the ruler. It snaps exactly as a click does (onto the
  game line, onto a marker), so it shows where a click would put the cursor. It never
  moves the cursor, and it is not drawn while a press drags the cursor, nor under a
  menu or the jump button.
- **A readout** beside the line for the track under the pointer: the moment, what the
  track shows, and its values there (the six highest and the picked player when there
  are more). Hollow rings mark the values on the lines; the cursor's dots are solid.
  The gutter keeps showing the cursor's values. Where the numbers are hidden the readout
  gives the reason instead, so hovering the fog gives nothing away.
- **Hover feedback** on what a click acts on: a marker badge steps forward, a metric
  row and a player row light up, the jump button brightens and gains a rim.

The timeline learns which track is under the pointer from `BuildCx::hovered_key`, so
anything drawn over the tracks hides the hover line with no extra bookkeeping. That
key changes only when the pointer moves: content scrolled under a resting pointer
keeps the old track's readout until the next move.

### 5.6 Pictures from the installed game

A reader for the game's rapid archive in `barwatch-core`: the version's `.sdp` index
under `<data>/packages/` lists each file's name and md5; the content is
`<data>/pool/<first two hex digits>/<rest>.gz`. For a replay's game version the app
loads `unitpics/<def>.dds` for the units the timeline's marks name, plus the skull used
for a team that is out, decodes them (`timeline/dds.rs`), and hands them to
`Timeline::set_icons`. The game version is always installed by the time a viewer is
playing, because the app installs it. Without a picture a badge falls back to the
built-in glyph, as it does now.

`--game-art` (reads a BAR checkout) stays a development flag only.

### 5.7 Embedding

The timeline goes into the party view once the party runs and this viewer is in game,
below the existing status and above the viewer list; the window's default size grows
to fit it. `Ui` passes the host hook to `Timeline::pointer_moved` (§5.5; it already
implements `WinitWgpuApp` with the default), puts `Timeline::layers()` among its root
overlays, returns the timeline's shaders, and passes on its focus requests. `Proto`
remains for development and shows the wiring.

### 5.8 Changes the user asked for after seeing the prototype

"It is not immediately clear what the main timeline graph is showing, or how to change
it." Built (T1):
- The large chart's title is a select control that names the metric and opens the list
  of metrics (`Timeline::layers` returns the open list; a host puts it at the root of
  its view, where popovers must be).
- The large chart and its controls stand on one surface. Below it, under the words
  "Shown in the large chart:", every metric row starts with a radio mark: the chosen
  row's is filled and the row is lit with the large chart's surface; a row under the
  pointer lights up and previews the mark.
- The hover readout (§5.5) names the metric of whatever track the pointer is on.
- With "by player" and a player picked, the compact rows show that player alone: their
  line in their colour, their value, and their place among all players ("#3 of 18").
- The ruler's tick labels no longer run into the chips (the last label sits inside the
  axis, off its tick); the large chart's grid labels show the round values of the scale
  being glided to instead of the numbers in between.

REJECTED for the clarity problem: opening the chosen row in place, accordion-style, in
place of a separate large chart. It needs no explaining, but the row clicked moves away
from under the pointer when the row above it closes, which makes stepping through
metrics a chase; the large chart also stops being where the eye left it.

Still open: the fight-heat scale (damage per metal of income, full at 8) is a guess to
revisit with real games.

## 6. Testing

A window can be driven without appearing on the user's screen: a headless sway
(`WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway`), the app started with that
`WAYLAND_DISPLAY`, an isolated `HOME` and its own `XDG_RUNTIME_DIR`, `grim` for
screenshots, `wtype` for keys, and a virtual pointer through
`zwlr_virtual_pointer_manager_v1` (the headless seat has none of its own).
`DAMASCENE_HEADLESS_SHOT=<file>` renders one frame with no window. Engine tests use a
scratch data folder that links `pool`, `packages`, `maps` and `rapid` from a real BAR
folder; never run a second engine on the live one. Always test with the pointer as well
as the keyboard, and stop processes by pid.

## 7. Work packages

Each ends with `cargo clippy --workspace --all-targets -- -D warnings` clean and
`cargo test --workspace` passing. T1 and T2 need no server and can start first.

- **T1. Host crate and hover** (§5.5, §5.8). Done when the hover line and readout work
  under a simulated pointer and the existing app still runs on the copied host.
- **T2. Game position** (§5.3). Done when a real engine spectating a barwatch server
  drives the game line, a jump shows catch-up, and a game started without the app shows
  no trace of the widget. Includes Windows (compile and, where possible, run).
- **T3. Pictures** (§5.6). Done when badges show the installed game's pictures for two
  different game versions.
- **T4. Timeline builder and stats client** (§4). The reducer can be written and
  tested first against raw output files that already exist; the client needs
  bar-replays work packages A and D, and C for the companion's series. Done when a
  party created each way (upload, link) yields a growing timeline, the marks of one
  recent game agree with BAR's in-game panel on kind and time within one sample, and
  everything still works with the service stopped.
- **T5. App on server data and embedding** (§5.2, §5.4, §5.7). Done when two viewers in
  one party see the same timeline, one jumps, and both show the jump.

## 8. Open items

- The last statistics sample can be stamped a few seconds after the game length in the
  replay header; check against a real engine that "game over" is reached.
- Windows: `SPRING_DATADIR`, the log follower and the shaders are untried there.
- A viewer who has switched user widgets off in BAR gets no game line (§5.3 fallback).
- Loading BAR's pictures from the viewer's own installation distributes nothing; it is
  still BAR's art shown in another program. If the BAR team objects, the glyph fallback
  is the answer.
