# barwatch

Watch a [Beyond All Reason](https://www.beyondallreason.info/) replay together. A
server hosts the replay; everyone who joins sees the same game, in sync, with shared
pause, speed and jump-ahead. Viewers join from a link.

Two parts:

- **`barwatchd`**, the server. It runs the engine's dedicated server on the replay
  and serves a small web page. Runs in Docker.
- **`barwatch`**, the desktop app for Linux and Windows. It opens from an invite
  link, installs whatever the replay needs, and starts the game as a spectator.

## Joining a watch party

1. Get the app.
   - **Windows:** download `barwatch-…-windows-x86_64.exe` from the
     [latest release](https://github.com/u6bkep/barwatch/releases/latest) and run it
     once. Windows shows a "protected your PC" notice because the file is not signed:
     choose *More info*, then *Run anyway*. The app copies itself to
     `%LOCALAPPDATA%\Programs\barwatch` and registers `barwatch://` links, so the
     download can be deleted afterwards.
   - **Arch Linux:**
     ```
     git clone https://github.com/u6bkep/barwatch
     cd barwatch/packaging/arch && makepkg -si
     ```
   - **Other Linux:** take `barwatch` from the release tarball and run it once; it
     registers the link handler for your user.
2. Open the invite link you were sent and press **Join in barwatch**. The app asks
   before it joins.
3. Wait for *Ready*. The replay starts for everyone when someone presses **Start**.
   If it is already running you are dropped straight in and caught up.

The app uses Beyond All Reason's own data folder, so an engine, game version or map
that BAR already downloaded is not downloaded again, and anything barwatch fetches is
there for BAR afterwards. Without a BAR installation it keeps a folder of its own and
downloads everything (several GB the first time).

### While watching

Pause and the speed keys work in game for every viewer, as in any replay. These work
from game chat, and the server's web page has buttons for the same things:

| chat | does |
|---|---|
| `!skip 14:00` | jump to game time 14:00 |
| `!skip +2m` | jump ahead two minutes (`+90`, `+90s`, `+1:30` also work) |
| `!pause`, `!resume` | pause and resume for everyone |
| `!speed 2` | set the speed for everyone (0.3 to 20) |
| `!help` | list these |

Jumps only go forward: the engine cannot rewind. To go back, stop the party and start
it again.

### The statistics timeline

If the server has a stats service (below), the app shows the game's statistics beside
the party: each side's economy, army value, damage, units built and lost, on one time
axis, with markers for moments such as a first tech 2 factory, a commander killed or a
nuke built. Your own game is the line across the charts; move the cursor to read any
moment, and **Jump everyone to …** to move the party forward to it. What has not
happened yet stays hidden unless you choose to show it. The numbers arrive while the
service simulates the replay, usually well ahead of the party; marker pictures come from
your installed game.

The app learns where your game is from a small widget it puts only into the games it
starts. Without a stats service on the server there is no timeline and nothing else
changes.

## Starting a watch party

On the server's web page: drop a `.sdfz` replay onto it, or paste a replay link or id
from the BAR website. Then send the invite link it shows.

From the desktop app: pair it with the server once (on the web page, press *Pair the
desktop app on this computer*). After that, **My replays** lists the replays in your
BAR folder with their map, players and length; pick one and press **Watch together**.
The replay is uploaded, the invite link is copied to the clipboard, and the app joins.

## Running the server

```
cp docker-compose.example.yml docker-compose.yml
docker compose up -d
```

| what | default | setting |
|---|---|---|
| web page and API, HTTP/1 and h2c on one port | `0.0.0.0:8780` | `BARWATCH_LISTEN` |
| game traffic, one UDP port per running party | `8480-8489` | `BARWATCH_UDP_PORTS` |
| address game clients connect to | host of the page's URL | `BARWATCH_GAME_HOST` |
| replays, engines, state | `/data` | `BARWATCH_DATA` |
| largest replay accepted | 256 MiB | `BARWATCH_MAX_UPLOAD_MB` |
| parties are deleted after | 14 days | `BARWATCH_SESSION_DAYS` |
| stats service for the statistics timeline (bar-replays), optional | none | `BARWATCH_STATS_URL` |
| its `client` token, or a file holding it (e.g. a Docker secret) | none | `BARWATCH_STATS_TOKEN`, `BARWATCH_STATS_TOKEN_FILE` |

**The daemon has no login of its own.** The intended setup:

- Put the web page behind a reverse proxy that authenticates. The proxy may speak
  HTTP/1 or h2c to the daemon.
- Let the path prefix **`/app/`** through without a login. The desktop app and the
  public join page live there. A party's id (22 random characters) is what lets
  someone see, join and control that party; creating parties through `/app/` needs
  the token handed out by pairing.
- Forward the UDP port range from the internet to the host unchanged. Game clients
  connect to those ports directly; they do not pass through the proxy.

Anyone who learns a running party's UDP port can connect to it as a spectator: the
engine has no access control when it hosts a replay.

**The stats service is optional.** It is
[bar-replays](https://github.com/u6bkep/bar-replays), which simulates each party's replay
and streams the statistics; run it in the same compose stack and point
`BARWATCH_STATS_URL` at it over the stack's network (`http://bar-replays:8770/`), with a
token of role `client` minted there (`bar-replays token add --name barwatch --role client`).
With it, replays given by link or id come through its archive and uploads are handed to
it. If it is slow or down, parties go on and the timeline says so. Without it: no
timeline, everything else as before.

The server needs no game content. For each replay it downloads the matching engine
release once (about 35 MB) and runs its dedicated server, which only streams the
recording: around 30 MB of memory and next to no CPU per running party.

## How it works

- A replay records the original server's setup text, random seed and content hashes
  in its first packet. `barwatchd` builds a start script from those, marks every
  recorded player as coming from the demo, and starts `spring-dedicated` on it. The
  dedicated server streams the recording to whoever connects and never simulates.
- The daemon follows the server over the engine's autohost interface (UDP on
  localhost): it learns who connected, reads chat for `!` commands, and sends
  `/skip`, `/pause` and speed changes.
- The recording begins with the original lobby wait; the daemon skips it.
- The app registers with the party over HTTP, reports its progress, and long-polls
  for changes. When the party runs, it writes a four-line script and starts the
  engine: `spring --write-dir <BAR data> --isolation <script>`.

`crates/barwatch-core` holds what both sides share (replay parsing, start scripts,
engine installs, API types), `crates/barwatchd` the server, `crates/barwatch` the app.
The app's window is built with [damascene](https://github.com/computer-whisperer/damascene).

## Known limits

- Only tested with engine 2026.07.04. Older 105.x replays are untried.
- The Windows build is cross-compiled and its link registration was checked under
  Wine, not yet on a Windows machine.
- The engine saves its own recording of what you watch into your `demos` folder, as
  it does for any game you spectate.
- Do not run barwatch while a BAR game is open on the same data folder; the two
  engines would share one log and settings file.
- A replay is a recording of engine network traffic, and the server plays it to every
  viewer's engine. Host replays from sources you would open locally. The setup text
  of an uploaded replay is parsed strictly and written out afresh before the engine
  sees it; a replay that does not fit the plain format every real one uses is refused.
- A link opened while the app is running shows up in the window that is already open,
  but that window is not brought to the front.

## Development

```
cargo test --workspace
cargo run -p barwatchd -- --data ./data --listen 127.0.0.1:8780
cargo run -p barwatch -- join http://127.0.0.1:8780/app/join/<id> --headless --data-dir <scratch>
```

`cargo run -p barwatchd --example fake_stats -- --output <actions.jsonl> --replay
<game.sdfz> --rate 30` is a stand-in stats service that plays a raw output file as a
simulation would (its flags, including ways to misbehave, are in
`crates/barwatchd/examples/fake_stats/main.rs`); point `BARWATCH_STATS_URL` at it
with `BARWATCH_STATS_TOKEN=fake-token`.

`join --headless` runs the session client without a window and the engine without
graphics, which is how the sync tests were done. Point `--data-dir` at a scratch
folder that links `pool`, `packages`, `maps` and `rapid` from a real BAR folder.

The window renders without a display: `BARWATCH_FIXTURE=<scene>` picks a canned state
(`empty`, `preparing`, `ready`, `playing`, `failed`, `replays`, `settings`, `consent`,
and the timeline's `ready-statistics` and `timeline-*`: see `fixtures.rs`; with
`BARWATCH_FIXTURE_DATA=<BAR data folder>` the marker pictures are read from there)
and `DAMASCENE_HEADLESS_SHOT=out.png` writes one frame and exits (`BARWATCH_FIXTURE_SIZE=760x700`
for another window size, `DAMASCENE_HEADLESS_SCALE=2` for a hiDPI one). The same scenes are
held to damascene's layout lints in the tests.

## License

MIT or Apache-2.0, at your option.
