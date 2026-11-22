//! The worker behind the window: one thread with an async runtime that does all
//! network and disk work. The window sends [`Cmd`]s and drains [`Msg`]s; each
//! message wakes the window for one frame.

use crate::client::{self, Settings};
use crate::mapimg::{self, Picture};
use crate::replays::{self, LocalReplay};
use barwatch_core::api::{Archived, RoomView};
use barwatch_core::maps::MapIndex;
use barwatch_host::Wakeup;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::mpsc as async_mpsc;
use url::Url;

pub enum Cmd {
    /// Join a room, leaving the current one.
    Join { server: Url, room: String, settings: Settings },
    /// For the room joined last.
    Room(client::Command),
    /// List and describe the replays in this folder.
    Scan(PathBuf),
    /// Fetch the picture of the map with this in-game name.
    MapImage(String),
    /// The rooms of a paired server.
    ListRooms(Paired),
    /// Make a room on a paired server.
    CreateRoom { server: Paired, name: String },
    /// Send a replay to a paired server's archive.
    Archive { server: Paired, path: PathBuf },
}

/// A paired server, as the window knows it.
pub struct Paired {
    pub server: Url,
    pub token: String,
}

pub enum Msg {
    Room(Box<client::Update>),
    /// The room joined last is over for this app (left, failed or gone).
    RoomEnded,
    /// The next batch of described replays, in list order.
    Replays(Vec<LocalReplay>),
    ScanDone,
    MapImage {
        map: String,
        picture: Option<Picture>,
    },
    Rooms {
        server: Url,
        rooms: Result<Vec<RoomView>, String>,
    },
    RoomCreated {
        server: Url,
        room: Result<Box<RoomView>, String>,
    },
    Archived {
        path: PathBuf,
        result: Result<Archived, String>,
    },
    /// Something the window asked for could not be done.
    Failed(String),
    /// Another launch of the app handed over the link it was started with (`None`: it
    /// had none).
    Opened(Option<String>),
}

/// The window's wake-up handle, once the window exists.
pub type WakeSlot = Arc<Mutex<Option<Wakeup>>>;

pub struct Backend {
    commands: async_mpsc::UnboundedSender<Cmd>,
    messages: mpsc::Receiver<Msg>,
    wake: WakeSlot,
}

/// Posts messages to the window and wakes it.
#[derive(Clone)]
pub struct Outbox {
    messages: mpsc::Sender<Msg>,
    wake: WakeSlot,
}

impl Outbox {
    pub fn post(&self, msg: Msg) {
        let _ = self.messages.send(msg);
        if let Some(wakeup) = self.wake.lock().unwrap().as_ref() {
            wakeup.wake();
        }
    }
}

impl Backend {
    pub fn spawn(wake: WakeSlot) -> (Backend, Outbox) {
        let (commands, command_rx) = async_mpsc::unbounded_channel();
        let (message_tx, messages) = mpsc::channel();
        let outbox = Outbox { messages: message_tx, wake: wake.clone() };
        let to_window = outbox.clone();
        std::thread::Builder::new()
            .name("barwatch-backend".into())
            .spawn(move || {
                let runtime =
                    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("async runtime");
                runtime.block_on(work(command_rx, outbox));
            })
            .expect("backend thread");
        (Backend { commands, messages, wake }, to_window)
    }

    /// A backend that does nothing, for rendering fixed scenes.
    pub fn inert() -> Backend {
        Backend { commands: async_mpsc::unbounded_channel().0, messages: mpsc::channel().1, wake: WakeSlot::default() }
    }

    /// Asks the window for a frame, from any thread: for work done off the
    /// backend whose result the window picks up when it next draws.
    pub fn waker(&self) -> impl FnOnce() + Send + 'static {
        let wake = self.wake.clone();
        move || {
            if let Some(wakeup) = wake.lock().unwrap().as_ref() {
                wakeup.wake();
            }
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.commands.send(cmd);
    }

    pub fn drain(&self) -> Vec<Msg> {
        self.messages.try_iter().collect()
    }
}

async fn work(mut commands: async_mpsc::UnboundedReceiver<Cmd>, outbox: Outbox) {
    let http = match barwatch_core::http_client() {
        Ok(h) => h,
        Err(e) => {
            outbox.post(Msg::Failed(format!("network setup failed: {e:#}")));
            return;
        }
    };
    let cache = crate::config::cache_dir();
    // The map list is fetched once, by the first picture request.
    let maps: Arc<tokio::sync::OnceCell<MapIndex>> = Arc::new(tokio::sync::OnceCell::new());
    let mut room: Option<async_mpsc::UnboundedSender<client::Command>> = None;
    let generation = Arc::new(AtomicU64::new(0));

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Cmd::Join { server, room: id, settings } => {
                if let Some(old) = room.take() {
                    let _ = old.send(client::Command::Leave);
                }
                let (tx, rx) = async_mpsc::unbounded_channel();
                room = Some(tx);
                // Only the room joined last may speak to the window: an older one
                // that is still winding down must not overwrite or end it.
                let mine = generation.fetch_add(1, Ordering::SeqCst) + 1;
                let current = generation.clone();
                let (http, outbox) = (http.clone(), outbox.clone());
                tokio::spawn(async move {
                    let (notify, live) = (outbox.clone(), current.clone());
                    let on_update = move |update| {
                        if live.load(Ordering::SeqCst) == mine {
                            notify.post(Msg::Room(Box::new(update)));
                        }
                    };
                    client::run(http, server, id, settings, rx, on_update).await;
                    if current.load(Ordering::SeqCst) == mine {
                        outbox.post(Msg::RoomEnded);
                    }
                });
            }
            Cmd::Room(command) => {
                let leaving = command == client::Command::Leave;
                if let Some(tx) = &room {
                    let _ = tx.send(command);
                }
                if leaving {
                    room = None;
                }
            }
            Cmd::Scan(dir) => {
                let outbox = outbox.clone();
                tokio::task::spawn_blocking(move || {
                    for batch in replays::list(&dir).chunks(64) {
                        outbox.post(Msg::Replays(batch.iter().map(|p| replays::describe(p)).collect()));
                    }
                    outbox.post(Msg::ScanDone);
                });
            }
            Cmd::MapImage(map) => {
                let (http, outbox, maps, cache) = (http.clone(), outbox.clone(), maps.clone(), cache.clone());
                tokio::spawn(async move {
                    let list = cache.join("maps.json");
                    let index = maps.get_or_init(|| MapIndex::load(&http, &list)).await;
                    let picture = mapimg::load(&http, index, &cache, &map).await.ok();
                    outbox.post(Msg::MapImage { map, picture });
                });
            }
            Cmd::ListRooms(paired) => {
                let (paired, outbox) =
                    (client::Paired { http: http.clone(), server: paired.server, token: paired.token }, outbox.clone());
                tokio::spawn(async move {
                    let rooms = paired.rooms().await.map_err(|e| format!("{e:#}"));
                    outbox.post(Msg::Rooms { server: paired.server, rooms });
                });
            }
            Cmd::CreateRoom { server, name } => {
                let (paired, outbox) =
                    (client::Paired { http: http.clone(), server: server.server, token: server.token }, outbox.clone());
                tokio::spawn(async move {
                    let room = paired.create_room(&name).await.map(Box::new).map_err(|e| format!("{e:#}"));
                    outbox.post(Msg::RoomCreated { server: paired.server, room });
                });
            }
            Cmd::Archive { server, path } => {
                let (paired, outbox) =
                    (client::Paired { http: http.clone(), server: server.server, token: server.token }, outbox.clone());
                tokio::spawn(async move {
                    let result = paired.archive(&path).await.map_err(|e| format!("{e:#}"));
                    outbox.post(Msg::Archived { path, result });
                });
            }
        }
    }
}
