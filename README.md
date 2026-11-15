# barwatch

Watch a Beyond All Reason replay together. One person hosts; everyone else joins and
sees the same replay, in sync, with shared pause / speed / skip. The Recoil engine
already supports this (a replay host accepts extra spectators); `barwatch` just does
the coordination: matching engine version, game version and map on every machine,
avoiding player-name clashes, and launching the engine on each side at the right moment.

## Usage

Host (has the replay file):

```
barwatch host "demos/2026-11-09_03-41-09-673_Comet Catcher Remake 1.8_2026.07.04.sdfz"
```

It prints a join line. Friends run:

```
barwatch join <host-ip> --name <their-name>
```

Each joiner installs the engine version / game version / map from the replay if missing
(engine from the Recoil GitHub releases, game and map through the engine's
`pr-downloader`), then reports **ready**. When everyone is ready the host presses Enter
(or passes `--auto-start N`). The host engine starts first; once it is listening, every
joiner's engine launches and connects as a spectator.

Joiners do **not** need the replay file: the host engine streams the demo contents.
`--send-replay` on the host plus `--save-replay` on a joiner copies the file anyway.

The engine's console output goes to `<data>/barwatch_engine.log`. Other options: `barwatch info` (detected data dir and installed engines), `--data-dir`
(override auto-detection; also `$BAR_DATA_DIR`), `--headless` (use `spring-headless`,
for testing).

## Networking

Direct connection only. The host needs two reachable ports, both default values can be
changed with flags:

| purpose                       | default   |
|-------------------------------|-----------|
| control channel (`barwatch`)  | TCP 8471  |
| engine                        | UDP 8470  |

Behind NAT, forward both to the host machine, or put everyone on a VPN such as Tailscale.

## Data directory

Auto-detected in this order: `--data-dir`, `$BAR_DATA_DIR`, flatpak
(`~/.var/app/info.beyondallreason.bar/data`), native Linux launcher
(`~/.local/state/Beyond All Reason`), Windows launcher
(`%LOCALAPPDATA%\Programs\Beyond-All-Reason\data`). Engines are looked up in
`<data>/engine/` using the launcher's directory naming (`recoil_2026.07.04`,
`105.1.1-2590-gb9462a0 bar`, `rel2501.2025.01.6`) and downloaded there if absent.

## How it works

1. Host parses the `.sdfz` header and embedded start script for engine version,
   game version, map and the player names in the demo.
2. Host provisions engine + content, then listens on the control port.
3. Each joiner sends `Hello{name}`, gets a `Manifest` (versions, UDP port, assigned
   name), provisions, and sends `Status{ready}`. Names that collide with a demo player
   or another joiner get a `_spec` suffix: the engine rejects a joiner whose name is
   already in the replay.
4. On start, the host writes a five-line start script (`DemoFile=…; HostIP; HostPort;
   MyPlayerName; IsHost=1`) and runs the engine; it waits for the engine's infolog to
   report the UDP socket bound, then broadcasts `Start`.
5. Joiners write a connect script (`HostIP; HostPort; MyPlayerName; IsHost=0`) and run
   the engine.

The engine replays the original game's pre-start wait in real time, so expect the same
lobby-to-start delay the original match had before frame 0.

## Building

```
cargo build --release                                  # Linux
cargo build --release --target x86_64-pc-windows-gnu   # Windows (needs mingw-w64)
```

## Known limits

- The engine drops Lua messages between live spectators while replaying, so widget
  features that talk between spectators will not work; chat and map drawing do.
- Any spectator can pause, change speed or skip; the engine has no per-player control.
- Live joiners occupy player slots above the demo's own count; very crowded replays
  (hundreds of spectator joins in the original) can run out.
