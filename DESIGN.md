# barwatch: design

Two designs live here: the replay timeline (§1–§8, built and released in 0.3.0) and rooms
(§9, designed 2026-11-22; the daemon side, R1, built on branch `rooms`, §9.12).

Status (2026-11-16): T1–T5 are built (each section's "as built" notes), on branch
`timeline-prototype`; release preparation for 0.3.0 on `release-prep` (`CHANGELOG.md`).
Status (2026-11-15, kept for the record): a prototype existed; nothing below was wired to
a server or an engine. This document is the brief for the
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
  `BARWATCH_STATS_TOKEN_FILE` (added for 0.3.0, release preparation) reads the token from a
  file instead, e.g. a compose secret; giving both is refused.
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
button rides the cursor (below the ruler since §5.9). "By player" turns the large chart's side panel into a ranked
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

Built (T5, `timeline/doc.rs`, `client.rs` `follow_timeline`):
- Metrics are the catalog's (`timeline/data.rs`) that the document can fill, in the
  catalog's order: rows read as BAR's group and column captions run together ("Metal
  income", "Army value", "Units lost"); rates come from cumulative counters, except
  income and expense, which use the companion's own reading when there is one; ratios
  show as percentages and a side's ratio is its players' mean; "Units alive", "Damage
  rate" and "Actions per minute" are derived and say so, and appear only when every
  input exists. The `value*` rows carry a note that the stats service counts them.
- The large chart settles on its metric with the first document that has one and then
  changes only by the viewer's hand: metal income when the document has it (the number
  a game turns on), else army value (an older stats service sends the engine's counters
  only at the end, but its own tally from the first sample), else unit value. Rows: the
  first tier (income, army and unit value, units alive and killed, damage rate, build
  power in use), with the second added when fewer than five of the first exist. So the
  short list changes once, when an older service's game completes: its second-tier
  rows (storage, defence and economy value, build power) give way to the engine's.
- The axis is the replay header's length while the document grows and `frames` once it
  is complete. Samples count only once a frame past their snap window is named.
- Replacing the data keeps the cursor, the chosen metric (by key), the open list, the
  picked player (while the same name stands there), "by player" (shown only while a
  side has several players; the viewer's choice is kept when it does not) and the
  scales (by metric key, eased; they move only when the drawn peak crosses a round
  step, which statistics past the game position never do while the future is hidden).
- Long-poll beside the session's with its 3 s retry, at most one answer per 3 s while
  building (the server's count moves with nearly every record and each answer is the
  whole document). A 404 before any answer means a server older than the timeline:
  shown once as "no statistics", asked again every ten minutes; after one it is
  passing (or the party is gone) and what was shown stays. An answer over 16 MiB, or
  one this app cannot parse (a later server's new state or series kind), is shown once
  as "no statistics" with the reason, and asked again from its `rev` ten minutes on.

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
  CORRECTED (T5): not on its own. A joining engine is silent while it loads, so this
  would show the whole future to everyone at every join. The game line stays where it
  was last seen and the future stays hidden; after 5 s of silence once it has reported
  (30 s before its first report) the timeline says why and offers "Use the timeline
  without it", which is the mode described above until the next report.
- When the engine restarts or the session's `run` changes, the position and any jump
  state reset.
- The widget also leaves the game when the app asks (§5.9): once a second it looks for
  `LuaUI/barwatch_quit` (a widget may look for raw files only under `LuaUI/`, in any
  data folder; the app writes it in its own widget folder, never BAR's) and sends
  `quitforce`, as BAR's own menu does.

### 5.4 Jump

`Action::Jump(seconds)` → `POST …/control` with `Control::Skip {seconds, relative:
false}`. The button shows only when the cursor is at least 10 s ahead of the game and
no jump is in flight. Every app shows a jump in flight from `SessionView.jump` (§4.3)
until its own engine reaches the target. A refused request calls
`Timeline::cancel_jump`.

Built (T5): a jump is "new" when `SessionView.jump` differs from the one named when this
viewer's engine got into the run, so a jump made before it joined (its engine is
catching up to the party, not to that jump) or while its engine was closed is not
shown. Arrival is within 2 s of the target; the server's whole-second name for this
app's own jump is the same jump. Without a game position nothing can say it arrived:
shown for at most 90 s while the game may still be loading, 8 s once its silence shows.
A refusal also shows the server's words as a notice.

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

Built (T5, `ui.rs`): the default window is 1180×900. While watching, the replay card
shrinks to one line (map picture, players, where this viewer stands, the party's
buttons) so the timeline fits above the viewer list. Before this viewer is in the
game the party view is as before, plus one line on where the statistics stand
("worked out up to 8:15 of 47:32 so far", queued, none and why). After the replay
ends the timeline stays, at game over, for those who were in it, until a new run
starts; a viewer who joins a new run gets it again from the start, with the metric
and view choices kept. "In game" needs the engine to have been started for the
party's current run (`Update.engine_run`): one left open after a replay ended is not,
and the client closes it when a new run starts so that the new one is joined. The
window redraws every 250 ms while the game line runs between reports, every 50 ms
while a jump is under way (its spinner), and not at all while nothing moves (measured
in the hidden rig: 0 % CPU idle, about 5 % for a debug build while the line runs). Pictures and names load once the
game version is installed and again for each picture name new marks bring.

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

### 5.9 After the integration run (2026-11-16)

- **A stats outage is said, and is not the end.** With a document there, `unavailable`
  is an outage: the server keeps the document and keeps trying (service out of reach,
  a turn to wait for, a token refused, records it cannot read yet; all retried). Only
  `failed` says the numbers stop, and no more than that they stopped: a game the
  service failed is asked about again every 30 s (`FAILED_RECHECK`) and would grow
  again if the service ran it once more. While either holds and the statistics stop short of the game's
  end, a notice above the tracks gives the server's note: "Statistics interrupted"
  (outage; what came stays, the rest fills in once they come again) or "Statistics
  stopped" (failed). The lag badge stays "Statistics N behind"
  through an outage, with the note as its tooltip; "No statistics after X" only once
  failed. The band past the statistics stands still during an outage ("Waiting for the
  stats service") instead of shimmering as if they were being worked out. Before the
  viewer is in game the one-line summary says the same ("…so far, then interrupted").
  REJECTED: putting the note in the header's status caption, which the cursor and the
  jump already use and which is cut short in a narrow window.
- **A late joiner's loading reads as loading.** Until this viewer's engine has said
  where it is, the timeline does not know where its game is: the app knows nothing of
  the party's own position (the server's `GameClock` is an estimate that runs off when
  viewers change speed with their keys, and is not sent), so the hide rule stays as it
  was: nothing past the start shows while the future is hidden. What changes is what
  is said. Header: the clock reads "unknown" and the game's chip "–:––"; the status is
  "Your game is loading" (then, past the 30 s grace, "Waiting for your game" beside
  the existing silence notice), "Your game is catching up" while it runs ahead, then
  "Following the game"; between the first report and the second, whose distance tells
  the pace, it is "Your game is starting" and the badge stays "Game loading". The gutters say "Hidden until your game has loaded" where
  numbers exist (not "Waiting for the first numbers", which stays for statistics that
  do not exist yet), and the fog says "Shown once your game has loaded" (while
  catching up, "Not reached yet"). The badge in the replay line is "Game loading",
  "Game open" (silent past the grace: open, but not saying), "Catching up", and "In
  game" only once the engine reports at the party's pace. Catching up is told by the
  app from consecutive reports: more than three times the reported speed in game
  seconds per wall second, plus 2 s of slack. No jump is offered before the first
  report (the game may already be past the cursor). With the future shown, the
  readouts are no longer held back before the first sample (`started()` only gates
  while hiding).
  REJECTED: revealing up to the last jump's target (`SessionView.jump`), a real lower
  bound on the party's position; it is rare (a jump before someone joins) and lines
  that run past a game line still at the start read as a fault. REJECTED: sending the
  server's `GameClock` estimate; it can run ahead of the party and give the future away.
- **Layout.** Marker badges keep 6 px from both ends of the axis (where the game line
  stands at game over and covered the last badge's count) and are gathered into
  clusters where they are drawn, so one pulled in from an end shares its neighbour's
  badge instead of landing on it; the count and the corner sign stand out of the
  frame only as far as the badge's box. The party view's content keeps 8 px from the
  scrollbar's lane. The chips on the ruler are as wide as their words measure
  (`layout_text_with_family`, as the renderer sets them), not estimated: tick labels
  give way to exactly what covers them. The jump button no longer rides the ruler: the
  cursor keeps its amber time chip there like any cursor, and the button hangs from
  the cursor just below the ruler, where it hides no tick label (it took the labels
  of one or two ticks with it before). A jump only goes past the game, and with the
  future hidden the lanes past the game are empty; with it shown, the button can
  cover a badge of the first lane near the cursor. REJECTED: shortening the button's
  words ("Jump to 4:00"), which hides fewer labels but drops "everyone", the point of
  the button; moving it into the header beside "Follow the game", which has no room
  for it in a narrow window and parts it from the cursor.
- **Grid labels** of the large chart sit on a backing of the chart's surface at 72 %
  opacity, so a line passing under "150" or "50%" still shows, dimmed, while the
  number reads. REJECTED: the gutter, whose right edge holds the values; a full-width
  left margin on the chart, which would part the large chart's axis from every other
  track's.
- **Leaving leaves the game.** Leave used to kill the engine, and the dedicated server
  listed the viewer as in the game until its connection timed out (measured: 122 s, and
  a party whose last viewer left that way ran on for as long). Now the app asks in
  turn, each step only if the one before did not close the engine: the widget's quit
  file (2.5 s; every platform, once the game has loaded with user widgets on), then on
  Unix SIGTERM (2.5 s), then a kill; the quit file is removed afterwards and before
  every launch unless it is under 10 s old (then it belongs to the engine of a party
  just left, still closing, which would otherwise be killed; it is gone long before the
  new engine has loaded). The engine runs in a process group of its own, so a Ctrl+C
  in the terminal reaches the app, which closes it this way, and not the headless
  engine, which would die without leaving. Closing the app's window does not close the
  engine (it never did): it watches on, in the game. Measured with real engines (2026.07.04) against a local barwatchd, two
  viewers in one party: through the widget the engine logs `[QuitAction] user exited
  to system`, shuts down in order, and the server drops it within 2 s, headless and
  windowed alike (and closes a party whose last viewer left within 5 s). SIGTERM: the
  windowed engine takes it as its window closing (SDL) and leaves the same way within
  1 s; the headless engine has no handler and dies as if killed (a 2 min ghost), which
  is why the widget goes first. An engine still loading is not yet listed by the
  server; it takes SIGTERM but finishes loading before acting on it, so it is killed
  after the 5 s. Windows: the widget, then the kill (untested there; no signal to send,
  and closing the window through the window system was not tried). REJECTED: SIGTERM
  first (kills the headless engine outright); `/quit` from the app, which the engine
  accepts only from its own console; the quit file in the BAR folder's `LuaUI/`, which
  works but puts a file of ours into BAR's folder.

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

## 9. Rooms (designed 2026-11-22; R1 built, §9.12; R2 built, §9.13)

### 9.1 Why

A watch party today is one replay: barwatchd stores the file, prepares the engine,
holds the viewers and the statistics, and the party's id is the invite link. People
expect the other way round, from BAR's battle rooms, Starcraft 2's parties and
Syncplay's rooms: join a place once, then that place watches things together. And the
only way to get rid of a party is *Delete*, whose wording ("and its replay") says the
replay is lost, while the replay archive is meant to keep it.

### 9.2 Decisions (user, 2026-11-22) and what was rejected

- **Rooms, not parties.** A room is a place with a link; a party in the Starcraft 2 sense
  is a group of accounts with a leader that moves between places. barwatch has no
  accounts (a viewer is an anonymous holder of a link with a name it chose), so on one
  server a party would be a room anyway. REJECTED: user parties, because they need
  identity, presence and invitations by name, which nothing here has.
- **Rooms are permanent.** One link per group of friends, kept until someone deletes the
  room. REJECTED: dissolving when the last member leaves (BAR's battle rooms do, because
  the host's session is the room; here "empty" means no app has reported for 30 s, so a
  link posted in a chat would die if the sender closed barwatch before the others
  opened it). REJECTED: deleting after an idle period, which was only carried over from
  the 14-day expiry of parties; nothing technical favours it, since a room is a few KB
  and replay files are cached separately (§9.5).
- **A room has a display name and a random id.** The id is in the link and is the only
  check on the paths reachable without a login (`/app/`), so the name cannot be the
  address as it is in Syncplay or Mumble: a name in the link could be guessed.
- **Full trust inside a room.** Anyone with the link can pick the room's replay (a BAR
  link or id, a file from their own replays, or one from the room's history), start,
  stop, pause, change the speed and jump. A replay picked this way goes to bar-replays
  for processing as an upload or submission does today, so the room link, not only the
  login or a pairing token, now lets someone spend simulation time. Accepted: barwatch
  is used by friends, and whoever has a link was invited by someone signed in. If it
  grows, the next step is BAR-lobby-like: a room boss and votes.
- **Creating, deleting and re-linking rooms needs a login or a pairing token**: the web
  page, or a paired app. *New link* replaces a room's id, for a link that went further
  than meant.
- **Start on command.** Picking a replay never starts it. The room shows who has
  everything for it; pressing Start early is fine, since a viewer still downloading
  joins when ready and is caught up, as now.
- **bar-replays is the replay archive; barwatchd only caches.** There is no deleting a
  replay, only a room. bar-replays stays optional (§9.5 for what that costs).
- **"View in Gex"** on the current replay and on every history entry opens
  `<bar-replays>/match/<game id>`. Those pages stay behind authentik: a viewer without an
  account sees the login page. Accepted, so `../bar-replays/THIRD_PARTY.md`'s decisions
  2 and 3 (no public match pages) stand.
- **"Archive"** beside "Watch together" in *My replays* sends a replay to bar-replays
  without watching it. Needs pairing: outside a room there is no room link to go by.
- **History, no queue.** A room shows what it is playing and what it played before. A
  Syncplay-style list of what comes next can follow later.
- **Membership is presence**: whoever has the room open in the app, as viewers are now.
  No member list is stored.
- **API version 2**, app and daemon released together. 0.3.0 apps refuse the new server
  (they refuse any other `api`), so existing parties are dropped, not converted.

### 9.3 What stays as it is

The engine side does not change: one `spring-dedicated` per running room, streaming one
replay file, forward seek only, a port from the same pool, `runner.rs` as it is. Chat
commands, the jump, the game position, the timeline document and its builder, pairing and
the authentik and `/app/` split in front of the daemon are unchanged.

### 9.4 Daemon state

`state.rs`'s `Session` splits in two.

**Replay** (`/data/replays/<key>/`: `replay.sdfz`, `meta.json`). What is about the game:
the file, the parsed `Demo`, `ReplayInfo`, the BAR id it was fetched by, engine
preparation, and the statistics (`timeline::Slot` moves here from the session, so two
rooms on one game share one follower). Keyed by the game id in the replay's header; a
replay without one is keyed by a hash of its bytes, has no statistics and is never sent
to the service (which refuses it). A replay records whether the service has confirmed it
holds the game (`game_id` set, as `Meta::game_id` is today).

A replay's record (`meta.json`: `ReplayInfo`, the BAR id, the confirmed game id) is kept
while some room's current replay or history names it, and deleted with its file when
none does; the file alone is what the cache rules (§9.5) keep or drop. Keys: the header's
game id as 32 lowercase hex digits; for a replay without one, `u` and the first 31 hex
digits of its bytes' SHA-256, which no game id can be.

**Room** (`/data/rooms/<id>.json`). What is about people: id, name, `created_unix`,
`last_used_unix`, the current replay's key (none in a new room), and the history: up to
50 entries of key and when the room picked it, newest first. In memory only, as now:
phase, run, paused, speed, ended, jump, game port, viewers, who is in the game, the
runner's channel.

`Phase` gains `Empty` (no replay picked). `Preparing` still means the server is fetching
the engine for the current replay. Picking a replay while the room runs stops the run
first (as *Stop* does), then sets the replay; the phase is `Preparing` or `Idle` once
the engine is there.

Viewers: a viewer's report names the replay key it is about, so a room never shows
someone as ready for the replay it had before. Names: a member's name is unique among
the room's members; the name its engine uses is unique against the current replay's
players, the other members' engine names and the names in the game, as `Session::join`
does now. It is stored with the member, given when the member joins and given afresh
to every member when the replay changes, so it stays put while a replay is current
(members coming and going do not move anyone else's). It is in the room's view next to
the member. `playing_as` keeps working as now.

Housekeeping: viewers expire as now; rooms do not. `BARWATCH_SESSION_DAYS` goes.

### 9.5 Replay cache

With a stats service, a replay file can be fetched again
(`GET /api/v1/games/{id}/replay`, what `source::through_service` already does), so
barwatchd keeps files only as a cache, bounded by `BARWATCH_REPLAY_CACHE_MB` (default
4096), least recently used going first. Never evicted: a replay some room has as its
current one, and a replay the service has not confirmed. A history entry whose file was
evicted is fetched again when someone picks it.

Without a service (or for a replay it never confirmed), nothing can be fetched again, so
a file is kept while it is some room's current replay or among the last 10 entries of
some room's history, and deleted when it is neither. Older history entries without a
file show as such and cannot be picked. This one rule (unconfirmed files are kept by
reference, confirmed ones by the cache budget) covers both setups.

Eviction and picking a replay are serialized: the cache is trimmed under the lock that
picking takes to set a room's current replay, and fetching a file again holds that
replay's own lock, which eviction skips a replay for. So a file is never deleted
between a room picking it and its server reading it.

On the first start of a version-2 daemon, `/data/sessions` is deleted (logged).

### 9.6 API (version 2)

Under `/app/v1/` (the prefix stays: it is the proxy's open path, not the API number).
Room id = capability, as the session id is now.

| call | needs | does |
|---|---|---|
| `GET rooms/{id}[?rev&wait]` | link | `RoomView`, long-polling as now |
| `GET rooms/{id}/timeline[?rev&wait]` | link | the current replay's `TimelineView` |
| `PUT rooms/{id}/replay` | link | pick a replay: the file (`x-barwatch-filename`), or JSON `{"bar_replay": …}` or `{"key": …}` (history) |
| `POST rooms/{id}/start`, `…/stop`, `…/control` | link | as the session calls (stop is new here) |
| `POST rooms/{id}/viewers`, `PUT`/`DELETE …/viewers/{viewer}` | link | as now; reports carry the replay key |
| `GET rooms`, `POST rooms {name}`, `DELETE rooms/{id}`, `POST rooms/{id}/relink` | pairing token | the paired app's room list, create, delete, new link |
| `POST archive` | pairing token | the file to bar-replays (`PUT /api/v1/replays`, priority `batch`); answers the game id |

The web page's `/api/` gets the same room calls without a token (authentik in front).
`/app/join/{id}` shows the room: name, what it plays, who is there.

`RoomView`: id, rev, name, phase, note, game host and port, run, paused, speed, ended,
jump, members (`ViewerView` plus the engine name and the replay key their state is
about), the current replay (`ReplayInfo`, key, game id, `match_url`) or none, and the
history (key, `ReplayInfo`, when, whether the file can be had, `match_url`).
`match_url` is `<BARWATCH_STATS_PUBLIC_URL>match/<game id>` when that setting is given
and the service has confirmed the game, else absent. `ServerInfo.api` becomes 2.

A relinked or deleted room answers 404; the app says the room is gone or has a new link.

Links: `barwatch://join?server=…&room=<id>`; the invite page's address stays
`<server>/app/join/<id>`.

### 9.7 Desktop app

- **Client** (`client.rs`): joins a room and stays in it until the user leaves. Getting
  the engine, game and map moves into the loop: when the room's replay changes, the
  download for the old one is abandoned and the new one's starts, and an engine still
  open on the old replay is closed. Everything about runs (launch on a new run, rejoin,
  closing a stale engine, the position follower, jumps) is as now.
- **Room view** replaces the party view: the room's name and *Copy link*; members with
  their readiness; the current replay's card with *View in Gex*, or "No replay yet";
  *Start*, *Stop*, *Rejoin*; the timeline as now; the history with *Watch* and
  *View in Gex*; a field for a BAR link or id.
- **My replays**: inside a room, *Play in <room>*; outside one, *Watch together* (paired:
  pick one of the server's rooms or make a new one). *Archive* when paired, then
  *View in Gex* once the game id is back.
- **Join tab**: besides pasting a link, the rooms this app has been in (kept in its
  config), so a friend gets back in without the link.

### 9.8 Web page

The party list becomes the room list: name, what it plays, who is there, *Copy link*,
*New link*, *Delete* ("Delete this room? Its replays stay in the archive."). *New room*.
Picking a replay works as on the app (drop a file, paste a link). `/?replay=<id>` (from
the bar-replays console) asks which room to put it in, or makes a new one. Pairing stays
as it is.

### 9.9 Infra

`stacks/barwatch/compose.yml` in the infra repo: `BARWATCH_STATS_PUBLIC_URL:
https://bar-replays.gecko.network/` and the new image. Nothing else changes in front of
the daemon: `/app/` stays the open path.

### 9.10 Work packages

Each ends clippy-clean with the workspace tests passing.

- **R1. Daemon** (§9.4–§9.6): rooms, replays, the cache, API 2, the web page (§9.8).
  Done when a room plays one replay, switches to another and back from history (with the
  file evicted in between, against `examples/fake_stats`), relinks, and the same works
  with no service configured.
- **R2. App** (§9.7). Done when two headless viewers stay in one room across a replay
  change, both engines restart for the new run, and a viewer who joined during the
  change is ready for the new replay, not the old one.
- **R3. Release and deploy**: app and daemon together, infra change, `README.md`.

### 9.11 Open items

- A room's link is a permanent capability; *New link* is the only remedy. Fine for
  friends (§9.2).
- Two rooms running the same replay each run a server; nothing is shared but the file and
  the statistics.

### 9.12 As built (R1, 2026-11-22)

`state.rs` (`Room`, `Replay`, the cache), `http.rs` (the calls), `rooms_tests.rs` (the
flows of §9.10), `stats_tests.rs` (the timeline, now through rooms). Choices the sections
above did not dictate:
- A room is stored by its own id (`rooms/<uid>.json`; run directory `run/<uid>`); the link
  id is a field, so *New link* changes nothing else and a running server is untouched.
  Log lines name rooms by that id, never by the link id (the capability).
- History: newest first, without duplicates, at most 50; each entry is when the room
  picked it. Picking the current replay again changes nothing (except that a room
  `failed` because its file was missing prepares again once the file is back). A `key` that is not in the
  history is 400 (not 404, which means the room is gone); one whose file is gone and
  cannot be fetched again is 409.
- Picking while a run is on stops it and waits up to 15 s for the server to exit (409 if
  it does not). Start waits while a pick switches the room's replay (not while the pick
  still downloads its file; a run started then is stopped by the switch). A BAR link
  whose record already has its file downloads nothing. A deleted room releases its
  replays only once its server has exited, however long that takes.
- "Least recently used" is `last_used_unix` in the replay's `meta.json`, set when a room
  picks it, leaves it for another and starts it. The cache is trimmed after every pick,
  after a deleted room's server has stopped, at start, and every 5 s (a replay the
  service confirmed becomes one it can let go).
- A deleted room answers 404 at once; its replays are released (and records no other room
  names deleted) once its server has stopped.
- Engine names are given in joining order from the member's name (`names::unique`
  against the replay's players and the names given so far). A member of a room without a
  replay gets one unique among the members. A report's state shows only if its `replay`
  equals the current replay's key (both absent counts as equal); else `preparing`.
- The game id becoming confirmed wakes the views of every room naming the replay (they
  show it, `match_url` and `available`).
- `TimelineView.replay` names the replay; a room without one answers `rev` 0,
  `unavailable`, "This room has no replay yet." A timeline long-poll also ends when the
  room picks another replay.
- `POST archive` refuses a replay without a game id (400) before asking the service;
  errors of the service are 502, a refusal by it 400, no service 503.
- The first start also deletes `/data/run` (named by party) along with `/data/sessions`.
- A room whose current replay's file is missing at start (removed by hand) is `failed`
  with a note; the cache never removes it.
- The web page keeps *Copy link*, *New link* and *Delete* both on the list and in the
  open room; picking works on the open room.

### 9.13 As built (R2, 2026-11-22)

`crates/barwatch`: `client.rs` (the room client), `ui.rs` (the room view, My replays, the
Join tab), `backend.rs`, `config.rs` (remembered rooms), `uri.rs` (`parse_link`),
`fixtures.rs` (room scenes), `main.rs` (`join`). Choices the sections above did not dictate:
- The client's `Stage` gains `NoReplay` (the room has none; reported as `ready` with no
  `replay`, which the server shows as ready; the window shows such members as "here") and
  `Gone` (any 404 from a room's calls, at joining or later: the client ends with "This room
  no longer exists: it was deleted or has a new link"). A 404 of the timeline call no
  longer means "a server older than statistics": every API 2 server has the call.
- On a replay change the client aborts the download, closes an engine still open on the
  old replay the proper way (the reporter widget's quit file, then SIGTERM, then a kill;
  leaving waits for these), restarts the timeline follower (asked without `rev`, the
  server answers at once for the new replay) and drops statistics of any other replay;
  the window also filters by `TimelineView.replay` and drops the timeline and marker
  pictures of the replay before. Downloads are told apart by a counter, so a stale result
  of an earlier download of the same replay (A, B, A) is not taken. The follower treats a
  view as new when its `(rev, replay)` differs.
- The engine name is read from the room view after a replay change has closed the old
  engine, and kept while an engine is in the game under it.
- Each engine install stages in its own folder (`.barwatch-<pid>-<n>`, `engine.rs`): a
  cancelled install may still be clearing or unpacking into its folder when the next starts.
- Start, Stop and picks are sent on their own tasks; a refusal comes back as a notice.
  `Update::picking` is true while a pick is on its way (buttons say "Uploading…" /
  "Fetching…").
- *Start for everyone* shows whenever the room has a replay and is `idle` or `failed`,
  whether or not this viewer is ready (§9.2: starting early is fine); no "Start again"
  wording, since run numbers carry across replays. *Stop for everyone* shows while
  `starting` or `running`.
- *Watch together* (outside a room, paired with the server paired last): a dialog with the
  server's six most recently used rooms and a field for a new room's name. The app joins
  the room first and queues the pick behind the join, so a refused file is reported inside
  the room. A new room's link is copied to the clipboard, as the old *Watch together* did.
- *Play in <room>* (the name cut to 24 characters) switches to the Room tab. *Archive*
  results are kept in memory for the session only.
- Remembered rooms: `rooms` in the app's config (server, room id, name), at most 20,
  latest first, updated when a room's name changes, dropped on its 404 or by *Forget*.
  Canned scenes never write the config (`Ui::fixture_in_memory`).
- A 0.3 party link (`session=`) is refused with "watch parties have become rooms, so ask
  for the room's link" (`uri::parse_link`; core's own error says only that `room` is
  missing).
- A gone room shows no *Copy link*. The first tab is called "Room"; the history card
  "Played here before", with "file gone" in place of *Watch* for an unavailable entry.
- `barwatch join` prints one line per change: `[member as engine name] <stage> for <replay
  key> (room "<name>": <phase>, replay <key>, run <n>)`.
- New scenes: `join-recent`, `room-empty`, `room`, `room-preparing`, `room-gone`,
  `replays-in-room`, `watch-together`, `archived`, `consent-pair`; the party scenes are room
  scenes now.

