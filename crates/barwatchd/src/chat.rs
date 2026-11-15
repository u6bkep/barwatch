//! Playback commands typed into game chat: `!skip 14:00`, `!skip +2m`, `!pause`,
//! `!resume`, `!speed 2`, `!help`.

use barwatch_core::api::Control;

pub const HELP: &str = "barwatch: !skip 14:00 (to a game time)  !skip +2m (ahead)  !pause  !resume  !speed 2";
pub const MIN_SPEED: f32 = 0.3;
pub const MAX_SPEED: f32 = 20.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Control(Control),
    Help,
}

/// `None` when the line is not addressed to barwatch; `Some(Err)` with a reply when
/// it is but cannot be understood.
pub fn parse(line: &str) -> Option<Result<Command, String>> {
    let rest = line.trim().strip_prefix('!')?;
    let mut words = rest.split_whitespace();
    let verb = words.next()?.to_ascii_lowercase();
    let arg = words.next();
    Some(match verb.as_str() {
        "skip" | "jump" | "seek" => match arg.map(parse_target) {
            Some(Some((seconds, relative))) => Ok(Command::Control(Control::Skip { seconds, relative })),
            _ => Err("usage: !skip 14:00  or  !skip +2m".into()),
        },
        "pause" => Ok(Command::Control(Control::Pause { paused: true })),
        "resume" | "unpause" | "play" => Ok(Command::Control(Control::Pause { paused: false })),
        "speed" => match arg.and_then(|a| a.trim_end_matches('x').parse::<f32>().ok()) {
            Some(s) if (MIN_SPEED..=MAX_SPEED).contains(&s) => Ok(Command::Control(Control::Speed { speed: s })),
            _ => Err(format!("usage: !speed 2  (between {MIN_SPEED} and {MAX_SPEED})")),
        },
        "help" | "barwatch" => Ok(Command::Help),
        _ => return None,
    })
}

/// `14:00`, `1:02:03`, `90`, `90s`, `5m`, each optionally prefixed with `+` for
/// "from here". A bare number is seconds, as in the engine's own `/skip`.
pub fn parse_target(text: &str) -> Option<(u32, bool)> {
    let (text, relative) = match text.strip_prefix('+') {
        Some(t) => (t, true),
        None => (text, false),
    };
    let seconds = if text.contains(':') {
        let mut total: u32 = 0;
        let parts: Vec<&str> = text.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        for p in parts {
            total = total.checked_mul(60)?.checked_add(p.parse().ok()?)?;
        }
        total
    } else if let Some(m) = text.strip_suffix('m') {
        m.parse::<u32>().ok()?.checked_mul(60)?
    } else {
        text.strip_suffix('s').unwrap_or(text).parse().ok()?
    };
    // A day of game time is far beyond any replay; refuse nonsense early.
    (seconds <= 24 * 3600).then_some((seconds, relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(seconds: u32, relative: bool) -> Option<Result<Command, String>> {
        Some(Ok(Command::Control(Control::Skip { seconds, relative })))
    }

    #[test]
    fn skip_targets() {
        assert_eq!(parse("!skip 14:00"), skip(840, false));
        assert_eq!(parse("!skip 1:02:03"), skip(3723, false));
        assert_eq!(parse("!skip +2m"), skip(120, true));
        assert_eq!(parse("!SKIP +90"), skip(90, true));
        assert_eq!(parse("  !jump 45s "), skip(45, false));
        assert!(matches!(parse("!skip"), Some(Err(_))));
        assert!(matches!(parse("!skip soon"), Some(Err(_))));
        assert!(matches!(parse("!skip 99999999"), Some(Err(_))));
    }

    #[test]
    fn other_commands() {
        assert_eq!(parse("!pause"), Some(Ok(Command::Control(Control::Pause { paused: true }))));
        assert_eq!(parse("!resume"), Some(Ok(Command::Control(Control::Pause { paused: false }))));
        assert_eq!(parse("!speed 2x"), Some(Ok(Command::Control(Control::Speed { speed: 2.0 }))));
        assert!(matches!(parse("!speed 500"), Some(Err(_))));
        assert_eq!(parse("!help"), Some(Ok(Command::Help)));
    }

    #[test]
    fn ordinary_chat_is_ignored() {
        assert_eq!(parse("gg wp"), None);
        assert_eq!(parse("!vote y"), None);
        assert_eq!(parse("!"), None);
    }
}
