//! Where the replay is in this viewer's own engine.
//!
//! A reporter widget, shipped inside the app, is written to a folder of the app's
//! own and handed to the engines the app starts as `SPRING_DATADIR`; nothing goes
//! into BAR's folder, so a game started from BAR's launcher never sees it. The
//! widget writes `[barwatch] f=<frame> speed=<x> paused=<0|1>` to the engine's
//! standard output (not `Spring.Echo`, which would also show in the in-game
//! console), which the app captures into `barwatch_engine.log`; [`follow`] reads
//! that file as it grows and keeps the latest report, and [`Position`] turns a
//! report into game seconds for any instant.

use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The widget, as the engine loads it.
pub const WIDGET: &str = include_str!("../assets/barwatch_position.lua");
/// Its file name under `LuaUI/Widgets/`.
pub const WIDGET_FILE: &str = "barwatch_position.lua";

/// Simulation frames per game second (BAR's `Game.gameSpeed`).
pub const FRAMES_PER_SECOND: f64 = 30.0;
/// Without a report for this long the position is unknown: user widgets are off in
/// BAR, or the engine is gone or stuck.
pub const STALE_AFTER: Duration = Duration::from_secs(5);
/// How far past its report a position is carried forward. The widget reports once
/// a second; this keeps a stalled engine from being shown running on.
const EXTRAPOLATE_AT_MOST: Duration = Duration::from_secs(2);
/// How often the log is read.
const POLL: Duration = Duration::from_millis(100);

/// One line of the widget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    pub frame: u32,
    /// The engine's speed factor (1 is normal speed).
    pub speed: f32,
    pub paused: bool,
}

/// The latest report and when the app read it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub report: Report,
    pub seen: Instant,
}

impl Position {
    /// Game seconds at `now`, or `None` when the report is too old to go by.
    pub fn seconds_at(&self, now: Instant) -> Option<f64> {
        let age = now.saturating_duration_since(self.seen);
        if age > STALE_AFTER {
            return None;
        }
        let at_report = f64::from(self.report.frame) / FRAMES_PER_SECOND;
        if self.report.paused {
            return Some(at_report);
        }
        let ran = age.min(EXTRAPOLATE_AT_MOST).as_secs_f64() * f64::from(self.report.speed);
        Some(at_report + ran)
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.seen) <= STALE_AFTER
    }
}

/// Reads a widget report out of one line of engine output.
///
/// The widget writes its lines raw, so a report starts the line. Everything the
/// engine logs (chat included) starts with its `[t=…]` time stamp, so a line that
/// merely contains the marker is not ours: a live spectator named `barwatch` saying
/// `f=99999 …` is logged as `[t=…][f=…] [barwatch] f=99999 …`.
pub fn parse_line(line: &str) -> Option<Report> {
    let rest = line.trim_end_matches(['\r', '\n']).strip_prefix("[barwatch] ")?;
    let mut fields = rest.split(' ');
    let frame = fields.next()?.strip_prefix("f=")?.parse().ok()?;
    let speed: f32 = fields.next()?.strip_prefix("speed=")?.parse().ok()?;
    let paused = match fields.next()?.strip_prefix("paused=")? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    if fields.next().is_some() || !speed.is_finite() || speed < 0.0 {
        return None;
    }
    Some(Report { frame, speed, paused })
}

/// Follows an engine log from its start: [`Follower::poll`] reads what was added
/// since the last call and keeps the newest report.
pub struct Follower {
    path: PathBuf,
    offset: u64,
    /// The unfinished last line.
    partial: Vec<u8>,
    latest: Option<Position>,
}

impl Follower {
    pub fn new(path: PathBuf) -> Follower {
        Follower { path, offset: 0, partial: Vec::new(), latest: None }
    }

    pub fn latest(&self) -> Option<Position> {
        self.latest
    }

    /// Reads new output. Returns whether a new report came in. A file that shrank
    /// was started afresh (a new engine), and is read again from the start.
    pub fn poll(&mut self, now: Instant) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else { return false };
        let Ok(len) = file.metadata().map(|m| m.len()) else { return false };
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
            self.latest = None;
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut added = Vec::new();
        let Ok(read) = file.take(len - self.offset).read_to_end(&mut added) else { return false };
        self.offset += read as u64;
        self.feed(&added, now)
    }

    /// Takes in a piece of output; returns whether it held a report.
    fn feed(&mut self, bytes: &[u8], now: Instant) -> bool {
        self.partial.extend_from_slice(bytes);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else { return false };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        // Only the newest report matters.
        let report = complete.split(|b| *b == b'\n').rev().filter_map(|line| std::str::from_utf8(line).ok()).find_map(parse_line);
        match report {
            Some(report) => {
                self.latest = Some(Position { report, seen: now });
                true
            }
            None => false,
        }
    }
}

/// Follows `log` until dropped, calling `on_change` with each new position, and with
/// `None` once no report has come for [`STALE_AFTER`].
pub async fn follow(log: PathBuf, on_change: impl Fn(Option<Position>)) {
    let mut follower = Follower::new(log);
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut known = false;
    loop {
        tick.tick().await;
        let now = Instant::now();
        if follower.poll(now) {
            known = true;
            on_change(follower.latest());
        } else if known && !follower.latest().is_some_and(|p| p.is_fresh(now)) {
            known = false;
            on_change(None);
        }
    }
}

/// Writes the widget into `dir` (as `LuaUI/Widgets/barwatch_position.lua`) and
/// returns the value for the engine's `SPRING_DATADIR`: `dir`, ahead of whatever
/// the user had set there. `None` when `dir` cannot be handed to the engine intact
/// (see [`engine_safe`]).
pub fn install(dir: &Path) -> std::io::Result<Option<OsString>> {
    if !engine_safe(dir) {
        return Ok(None);
    }
    let widgets = dir.join("LuaUI").join("Widgets");
    std::fs::create_dir_all(&widgets)?;
    let path = widgets.join(WIDGET_FILE);
    if std::fs::read(&path).ok().as_deref() != Some(WIDGET.as_bytes()) {
        // Another copy of the app may be starting an engine at the same moment.
        let tmp = widgets.join(format!(".{WIDGET_FILE}.{}", std::process::id()));
        std::fs::write(&tmp, WIDGET)?;
        std::fs::rename(&tmp, &path)?;
    }
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(theirs) = std::env::var_os("SPRING_DATADIR").filter(|v| !v.is_empty()) {
        dirs.extend(std::env::split_paths(&theirs));
    }
    Ok(std::env::join_paths(dirs).ok())
}

/// Whether the engine reads `dir` back as written. It splits `SPRING_DATADIR` at the
/// platform's path separator and expands variables in each entry (`wordexp` on
/// Linux, `ExpandEnvironmentStrings` on Windows).
fn engine_safe(dir: &Path) -> bool {
    let Some(text) = dir.to_str() else { return false };
    let special: &[char] = if cfg!(windows) { &[';', '%'] } else { &[':', '$', '`', '"', '\'', '\\', '*', '?', '[', '\n', '\t'] };
    dir.is_absolute() && !text.contains(special) && !text.contains("  ")
}

/// The folder the widget is written to: `<local data>/barwatch/engine-extra`.
pub fn widget_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("barwatch").join("engine-extra"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(frame: u32, speed: f32, paused: bool) -> Report {
        Report { frame, speed, paused }
    }

    #[test]
    fn reads_the_widgets_lines() {
        assert_eq!(parse_line("[barwatch] f=9000 speed=1.00 paused=0"), Some(report(9000, 1.0, false)));
        assert_eq!(parse_line("[barwatch] f=0 speed=0.50 paused=1\r\n"), Some(report(0, 0.5, true)));
        assert_eq!(parse_line("[barwatch] f=12 speed=20.00 paused=0\n"), Some(report(12, 20.0, false)));
    }

    #[test]
    fn ignores_everything_else() {
        for line in [
            "",
            "[t=00:00:01.000000][f=0000100] LuaUI: Allowing User Widgets",
            // A player of the replay saying it in chat.
            "[t=00:01:02.345678][f=0009000] <[MADO]vault> Allies: [barwatch] f=99999 speed=1.00 paused=0",
            // A live spectator named barwatch saying it, or anything else the engine logs.
            "[t=00:01:02.345678][f=0009000] [barwatch] f=99999 speed=1.00 paused=0",
            " [barwatch] f=10 speed=1.00 paused=0",
            "[barwatch] f=-5 speed=1.00 paused=0",
            "[barwatch] f=10 speed=fast paused=0",
            "[barwatch] f=10 speed=1.00 paused=2",
            "[barwatch] f=10 speed=-1.00 paused=0",
            "[barwatch] f=10 speed=NaN paused=0",
            "[barwatch] f=10 speed=1.00 paused=0 extra",
            "[barwatch] f=10 speed=1.00",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    fn at(frame: u32, speed: f32, paused: bool, seen: Instant) -> Position {
        Position { report: report(frame, speed, paused), seen }
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-6)
    }

    #[test]
    fn runs_on_at_the_reported_speed() {
        let t0 = Instant::now();
        let p = at(300, 1.0, false, t0);
        assert!(close(p.seconds_at(t0), 10.0));
        assert!(close(p.seconds_at(t0 + Duration::from_millis(500)), 10.5));
        let fast = at(300, 2.0, false, t0);
        assert!(close(fast.seconds_at(t0 + Duration::from_millis(500)), 11.0));
        // A clock that reads before the report does not move it back.
        assert!(close(p.seconds_at(t0.checked_sub(Duration::from_secs(1)).unwrap_or(t0)), 10.0));
    }

    #[test]
    fn a_new_speed_counts_from_its_own_report() {
        let t0 = Instant::now();
        let mut f = Follower::new(PathBuf::new());
        f.feed(b"[barwatch] f=300 speed=1.00 paused=0\n", t0);
        assert!(close(f.latest().unwrap().seconds_at(t0 + Duration::from_millis(900)), 10.9));
        // Speed raised to 3x, reported 0.3 s after the change was made.
        let t1 = t0 + Duration::from_secs(1);
        f.feed(b"[barwatch] f=339 speed=3.00 paused=0\n", t1);
        let p = f.latest().unwrap();
        assert!(close(p.seconds_at(t1), 11.3));
        assert!(close(p.seconds_at(t1 + Duration::from_millis(500)), 12.8));
        // And a pause stops it where the report says.
        let t2 = t1 + Duration::from_millis(600);
        f.feed(b"[barwatch] f=390 speed=3.00 paused=1\n", t2);
        assert!(close(f.latest().unwrap().seconds_at(t2 + Duration::from_secs(4)), 13.0));
    }

    #[test]
    fn stands_still_while_paused() {
        let t0 = Instant::now();
        let p = at(900, 1.0, true, t0);
        assert!(close(p.seconds_at(t0 + Duration::from_secs(3)), 30.0));
    }

    #[test]
    fn carries_forward_only_so_far_and_then_is_unknown() {
        let t0 = Instant::now();
        let p = at(0, 1.0, false, t0);
        assert!(close(p.seconds_at(t0 + Duration::from_secs(4)), 2.0));
        assert!(p.seconds_at(t0 + STALE_AFTER).is_some());
        assert_eq!(p.seconds_at(t0 + STALE_AFTER + Duration::from_millis(1)), None);
        assert!(!p.is_fresh(t0 + Duration::from_secs(6)));
    }

    #[test]
    fn follower_keeps_the_newest_report_and_waits_for_whole_lines() {
        let t0 = Instant::now();
        let mut f = Follower::new(PathBuf::new());
        assert!(f.feed(b"[t=00:00:01.000000] loading\n[barwatch] f=30 speed=1.00 paused=0\n[barwatch] f=6", t0));
        assert_eq!(f.latest().map(|p| p.report.frame), Some(30));
        // The second half of the line arrives later.
        let t1 = t0 + Duration::from_secs(1);
        assert!(f.feed(b"0 speed=2.00 paused=0\n", t1));
        assert_eq!(f.latest(), Some(at(60, 2.0, false, t1)));
        // Lines that are not reports change nothing.
        assert!(!f.feed(b"[t=00:00:03.000000][f=0000090] something else\n", t1 + Duration::from_secs(1)));
        assert_eq!(f.latest(), Some(at(60, 2.0, false, t1)));
    }

    #[test]
    fn a_restarted_engine_starts_from_nothing() {
        let dir = std::env::temp_dir().join(format!("barwatch-position-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("barwatch_engine.log");
        std::fs::write(&log, "[barwatch] f=9000 speed=1.00 paused=0\n").unwrap();
        let mut f = Follower::new(log.clone());
        let t0 = Instant::now();
        assert!(f.poll(t0));
        assert_eq!(f.latest().map(|p| p.report.frame), Some(9000));
        assert!(!f.poll(t0), "nothing new");
        // The app creates the log afresh for the next engine.
        std::fs::write(&log, "loading\n").unwrap();
        assert!(!f.poll(t0));
        assert_eq!(f.latest(), None);
        std::fs::write(&log, "loading\n[barwatch] f=30 speed=1.00 paused=1\n").unwrap();
        assert!(f.poll(t0));
        assert_eq!(f.latest().map(|p| p.report), Some(report(30, 1.0, true)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installs_the_widget_where_the_engine_will_find_it() {
        let dir = std::env::temp_dir().join(format!("barwatch-widget-test-{}", std::process::id())).join("engine-extra");
        let value = install(&dir).unwrap().expect("a plain path is safe");
        let written = std::fs::read_to_string(dir.join("LuaUI/Widgets").join(WIDGET_FILE)).unwrap();
        assert_eq!(written, WIDGET);
        assert_eq!(std::env::split_paths(&value).next(), Some(dir.clone()));
        // Again, unchanged: no error, same file.
        install(&dir).unwrap();
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
        assert!(!engine_safe(Path::new("relative/dir")));
        if cfg!(windows) {
            assert!(!engine_safe(Path::new(r"C:\Users\a;b\x")));
        } else {
            assert!(!engine_safe(Path::new("/home/a:b/x")));
            assert!(!engine_safe(Path::new("/home/$USER/x")));
        }
    }

    #[test]
    fn widget_speaks_the_format_we_read() {
        assert!(WIDGET.contains(r#""[barwatch] f=%d speed=%s paused=%d\n""#));
        assert!(WIDGET.contains("Spring.IsReplay() or Spring.GetSpectatingState()"));
    }
}
