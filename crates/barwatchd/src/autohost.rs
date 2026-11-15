//! The engine's autohost interface: the dedicated server sends one event per UDP
//! datagram to the port named in its start script, and accepts commands (`/skip 60`)
//! and chat lines on the same socket (engine `rts/Net/AutohostInterface.cpp`).

/// An event from the hosting server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server is listening for viewers.
    Started,
    Quit,
    /// A line of the server's log, e.g. `Beginning demo playback`.
    Message(String),
    PlayerJoined {
        number: u8,
        name: String,
    },
    PlayerLeft {
        number: u8,
    },
    Chat {
        number: u8,
        text: String,
    },
    GameOver,
    /// Anything barwatch has no use for.
    Other(u8),
}

impl Event {
    pub fn parse(datagram: &[u8]) -> Option<Event> {
        let (&kind, rest) = datagram.split_first()?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        Some(match kind {
            0 => Event::Started,
            1 => Event::Quit,
            3 => Event::GameOver,
            4 | 5 => Event::Message(text(rest)),
            10 => Event::PlayerJoined { number: *rest.first()?, name: text(rest.get(1..)?) },
            11 => Event::PlayerLeft { number: *rest.first()? },
            13 => Event::Chat { number: *rest.first()?, text: text(rest.get(2..)?) },
            other => Event::Other(other),
        })
    }
}

/// What the server tells us through its log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The replay stream has begun (its recorded pre-game wait comes first).
    PlaybackBegan,
    Paused(bool),
    ReplayEnded,
}

impl Notice {
    pub fn parse(message: &str) -> Option<Notice> {
        let m = message.trim();
        if m == "Beginning demo playback" {
            Some(Notice::PlaybackBegan)
        } else if m == "End of demo reached" {
            Some(Notice::ReplayEnded)
        } else if m.ends_with(" unpaused the demo") {
            Some(Notice::Paused(false))
        } else if m.ends_with(" paused the demo") {
            Some(Notice::Paused(true))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        assert_eq!(Event::parse(&[0]), Some(Event::Started));
        assert_eq!(Event::parse(b"\x04Server started on port 8480"), Some(Event::Message("Server started on port 8480".into())));
        assert_eq!(Event::parse(b"\x0a\x02viewer1"), Some(Event::PlayerJoined { number: 2, name: "viewer1".into() }));
        assert_eq!(Event::parse(&[11, 2, 1]), Some(Event::PlayerLeft { number: 2 }));
        assert_eq!(Event::parse(b"\x0d\x02\xfe!skip 10:00"), Some(Event::Chat { number: 2, text: "!skip 10:00".into() }));
        assert_eq!(Event::parse(&[20, 1, 2, 3]), Some(Event::Other(20)));
        assert_eq!(Event::parse(&[]), None);
        assert_eq!(Event::parse(&[10]), None);
    }

    #[test]
    fn parses_notices() {
        assert_eq!(Notice::parse("Beginning demo playback"), Some(Notice::PlaybackBegan));
        assert_eq!(Notice::parse("viewer1 paused the demo"), Some(Notice::Paused(true)));
        assert_eq!(Notice::parse("viewer1 unpaused the demo"), Some(Notice::Paused(false)));
        assert_eq!(Notice::parse("End of demo reached"), Some(Notice::ReplayEnded));
        assert_eq!(Notice::parse("Connection attempt from viewer1"), None);
    }
}
