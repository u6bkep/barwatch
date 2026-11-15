//! One window per user: a second launch hands its link to the window already open.
//!
//! The open window listens on a loopback port and writes the port and a random secret to
//! a file only this user can read. A later launch reads the file, sends the secret and
//! its link, and exits once the window acknowledges. A stale file (the window is gone)
//! gets no acknowledgement, and the later launch opens a window of its own.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

const ACK: &str = "barwatch-ok";
/// Longest link line accepted from another launch.
const MAX_LINE: u64 = 8192;

fn file() -> Option<PathBuf> {
    // The runtime directory dies with the login session (Linux); elsewhere the user's
    // local application data.
    Some(dirs::runtime_dir().or_else(dirs::data_local_dir)?.join("barwatch").join("instance"))
}

/// Gives `link` to a window that is already open, or just tells it about this launch
/// when there is no link. False when no window answered: the caller opens one.
pub fn hand_over(link: Option<&str>) -> bool {
    file().and_then(|path| try_hand_over(&path, link)).unwrap_or(false)
}

fn try_hand_over(path: &std::path::Path, link: Option<&str>) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let (port, secret) = text.trim().split_once(' ')?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port.parse::<u16>().ok()?));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let link = link.and_then(|l| l.lines().next()).unwrap_or("");
    stream.write_all(format!("{secret}\n{link}\n").as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    Some(reply.trim() == ACK)
}

/// Held by the open window; dropping it withdraws the invitation to later launches.
pub struct Listening {
    path: PathBuf,
    contents: String,
}

impl Drop for Listening {
    fn drop(&mut self) {
        // Only our own: a window opened since may have replaced it.
        if std::fs::read_to_string(&self.path).is_ok_and(|text| text == self.contents) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Starts taking links from later launches; `on_link` runs on a background thread for
/// each (`None`: a launch without a link). `None` when that cannot be set up, in which
/// case every launch simply opens its own window.
pub fn listen(on_link: impl Fn(Option<String>) + Send + 'static) -> Option<Listening> {
    listen_at(file()?, on_link)
}

fn listen_at(path: PathBuf, on_link: impl Fn(Option<String>) + Send + 'static) -> Option<Listening> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let secret: String = rand::random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect();
    let contents = format!("{port} {secret}\n");

    std::fs::create_dir_all(path.parent()?).ok()?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &contents).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    std::fs::rename(&tmp, &path).ok()?;

    std::thread::Builder::new()
        .name("barwatch-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Some(link) = receive(stream, &secret) {
                    on_link(link);
                }
            }
        })
        .ok()?;
    Some(Listening { path, contents })
}

/// Reads one hand-over; `Some` only when it carried our secret.
fn receive(stream: TcpStream, secret: &str) -> Option<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?.take(MAX_LINE));
    let (mut theirs, mut link) = (String::new(), String::new());
    reader.read_line(&mut theirs).ok()?;
    reader.read_line(&mut link).ok()?;
    if theirs.trim_end() != secret || !link.ends_with('\n') {
        return None;
    }
    (&stream).write_all(format!("{ACK}\n").as_bytes()).ok()?;
    let link = link.trim();
    Some((!link.is_empty()).then(|| link.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_later_launch_hands_its_link_over() {
        let dir = std::env::temp_dir().join(format!("barwatch-instance-test-{}", std::process::id()));
        let path = dir.join("instance");
        // Nothing is open yet.
        assert_eq!(try_hand_over(&path, Some("barwatch://x")), None);

        let (tx, rx) = mpsc::channel();
        let listening = listen_at(path.clone(), move |link| tx.send(link).unwrap()).unwrap();
        assert_eq!(try_hand_over(&path, Some("barwatch://join?x=1")), Some(true));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().as_deref(), Some("barwatch://join?x=1"));
        assert_eq!(try_hand_over(&path, None), Some(true));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), None);

        // The wrong secret is not acknowledged and delivers nothing.
        let port = listening.contents.split(' ').next().unwrap().to_owned();
        let forged = dir.join("forged");
        std::fs::write(&forged, format!("{port} 00000000000000000000000000000000\n")).unwrap();
        assert_eq!(try_hand_over(&forged, Some("barwatch://evil")), Some(false));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

        // Closing the window withdraws the file.
        drop(listening);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
