# Changelog

## 0.4.0

### Rooms

- Watch parties are replaced by **rooms**. A room is a permanent place with a name and a
  link: join it once, and it watches one replay after another together. Rooms are made,
  deleted and given a new link on the server's web page or from a paired app.
- Everyone in a room can pick its replay (a link or id from the BAR website, a file from
  **My replays** with **Play in <room>**, or one from **Played here before**), start and
  stop it, and pause, change speed or jump as before. Picking never starts a replay: the
  room shows who has everything for it, and someone presses **Start**.
- **Played here before** lists a room's earlier replays to watch again.
- **View in Gex** opens a replay's page on the replay archive (bar-replays), for the
  current replay and every earlier one. The server needs `BARWATCH_STATS_PUBLIC_URL`.
- **Archive** in **My replays** (paired apps) sends a replay to the replay archive without
  watching it.
- The app's **Join** tab lists the rooms you have been in.

### Server

- With a stats service, the replay archive is bar-replays: the server keeps replay files
  only as a cache, `BARWATCH_REPLAY_CACHE_MB` (default 4096), and fetches one again when
  a room picks it. Without one, it keeps each room's current replay and the last 10 it
  played.
- `BARWATCH_SESSION_DAYS` is gone: rooms stay until deleted.

### Compatibility

- **The app and server must both be 0.4.** The protocol changed (version 2); an 0.3 app
  refuses an 0.4 server and the other way round. Watch parties of an 0.3 server are
  deleted when an 0.4 server first starts, and their invite links stop working.

## 0.3.0

### Statistics timeline

- While a watch party plays, the desktop app can show the game's statistics over time:
  each side's economy, army value, damage, units built and lost, on one time axis, with
  markers for moments such as a side's first tech 2 factory, a commander killed or a
  nuke built. It is modelled on Beyond All Reason's own in-game statistics panel, but it
  knows the whole game, not only the part already played.
- The game you are watching is drawn as a line across every chart; a second line, the
  cursor, drives every number shown. Move it to read any moment. What has not happened
  yet is hidden unless you choose to show it.
- **Jump everyone to …** moves the whole party forward to the cursor's moment. Jumps only
  go forward, as before.
- The numbers come from a separate stats service, [bar-replays](https://github.com/u6bkep/bar-replays),
  which simulates the replay faster than the party watches it. The server needs its
  address and a token (`BARWATCH_STATS_URL`, and `BARWATCH_STATS_TOKEN` or
  `BARWATCH_STATS_TOKEN_FILE`). **The service is optional**: without it everything works
  as in 0.2, without a timeline. If it is slow or down, parties are not affected; the
  app says the statistics are being worked out or were interrupted.
- The app knows where your own game is from a small reporter widget it places only in
  the games it starts. Someone who joins late sees their game as loading, then catching
  up, then in game.
- Marker pictures are read from your installed game; barwatch bundles none of BAR's art.
- With a stats service, replays given by link or id are fetched through its archive, and
  uploaded replays are handed to it, so a replay is downloaded once.
- The server's web page takes `/?replay=<id>` to fill in a replay (the stats service's
  "Watch together" link uses it).

### Fixes

- The app no longer downloads a map again on every join. It fetched the whole map each
  time (84 MB for Scylla and Charybdis) even when BAR already had it.
- **Leave** now makes your game leave the party properly. The game used to be killed,
  and the server went on listing you as a viewer for about two minutes (and kept a party
  running whose last viewer had left). The server now sees you go within a few seconds.

### Compatibility

- The app's protocol version is unchanged, so 0.2 and 0.3 apps and servers work with each
  other: an 0.2 app shows no timeline, and an 0.3 app on an 0.2 server says there are no
  statistics.

## 0.2.1 (2026-11-15) and earlier

See the [releases](https://github.com/u6bkep/barwatch/releases).
