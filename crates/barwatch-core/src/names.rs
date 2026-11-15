//! Viewer names and the checks applied to strings that arrive from the network
//! before they reach a start script, a download URL or a command line.

/// Longest viewer name handed to the engine.
pub const MAX_NAME_LEN: usize = 24;

/// Reduces a display name to what is safe inside a start script and what the engine
/// accepts as a player name: ASCII letters, digits, `_`, `-`, `[` and `]`.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '[' | ']') { c } else { '_' })
        .take(MAX_NAME_LEN)
        .collect();
    // "[[" and "]]" are quote marks in the engine's script reader.
    while s.contains("[[") || s.contains("]]") {
        s = s.replace("[[", "[").replace("]]", "]");
    }
    if s.trim_matches('_').is_empty() {
        "Spectator".to_string()
    } else {
        s
    }
}

/// `wanted`, changed until it differs (ignoring case) from everything in `taken`.
/// The engine rejects a viewer whose name equals a recorded player's.
pub fn unique(wanted: &str, taken: &[String]) -> String {
    let clash = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    let base = sanitize(wanted);
    if !clash(&base) {
        return base;
    }
    for i in 2.. {
        let suffix = format!("_{i}");
        let keep = MAX_NAME_LEN.saturating_sub(suffix.len()).min(base.len());
        let candidate = format!("{}{suffix}", &base[..keep]);
        if !clash(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// An engine version as written in replay headers, e.g. `2026.07.04` or
/// `105.1.1-2590-gb9462a0 BAR105`. It becomes part of a download URL and a
/// directory name.
pub fn valid_engine_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 80
        && !v.starts_with(['.', '-', ' '])
        && !v.contains("..")
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ' '))
}

/// A game or map name. It is passed to `pr-downloader` as an argument, so it must
/// not look like an option, and it must be printable.
pub fn valid_content_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && !n.starts_with('-') && !n.chars().any(char::is_control)
}

/// A session id or token: URL-safe and long enough not to be guessable.
pub fn valid_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A host name or address a start script may name as the game server.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_lobby_names_and_strips_script_syntax() {
        assert_eq!(sanitize("[gecko]u6bkep"), "[gecko]u6bkep");
        assert_eq!(sanitize(" two words "), "two_words");
        assert_eq!(sanitize("a;b}c=d"), "a_b_c_d");
        assert_eq!(sanitize("[[x]]"), "[x]");
        assert_eq!(sanitize(""), "Spectator");
        assert_eq!(sanitize("???"), "Spectator");
        assert_eq!(sanitize(&"x".repeat(60)).len(), MAX_NAME_LEN);
    }

    #[test]
    fn unique_avoids_recorded_players_ignoring_case() {
        let taken = vec!["Alice".to_string(), "alice_2".to_string()];
        assert_eq!(unique("bob", &taken), "bob");
        assert_eq!(unique("ALICE", &taken), "ALICE_3");
        let long = "x".repeat(MAX_NAME_LEN);
        let got = unique(&long, std::slice::from_ref(&long));
        assert_eq!(got.len(), MAX_NAME_LEN);
        assert!(got.ends_with("_2"));
    }

    #[test]
    fn validators() {
        assert!(valid_engine_version("2026.07.04"));
        assert!(valid_engine_version("105.1.1-2590-gb9462a0 BAR105"));
        assert!(!valid_engine_version("../../etc"));
        assert!(!valid_engine_version("a/b"));
        assert!(valid_content_name("Comet Catcher Remake 1.8"));
        assert!(!valid_content_name("--filesystem-writepath"));
        assert!(!valid_content_name("a\nb"));
        assert!(valid_id("abcdefghijklmnop1234"));
        assert!(!valid_id("short"));
        assert!(!valid_id("has/slash/in/it/1234"));
        assert!(valid_host("bar.example.org") && valid_host("fd00::1") && !valid_host("a;b"));
    }
}
