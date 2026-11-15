//! The worker behind the window: one thread with an async runtime that does all
//! network and disk work. The window sends [`Cmd`]s and drains [`Msg`]s; each
//! message wakes the window for one frame.

use crate::client::{self, Settings};
use crate::mapimg::{self, Picture};
use crate::replays::{self, LocalReplay};
use barwatch_core::maps::MapIndex;
use barwatch_host::Wakeup;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::mpsc as async_mpsc;
use url::Url;

pub enum Cmd {
    /// Join a party, leaving the current one.
    Join { server: Url, session: String, settings: Settings },
    /// For the party joined last.
    Session(client::Command),
    /// List and describe the replays in this folder.
    Scan(PathBuf),
    /// Fetch the picture of the map with this in-game name.
    MapImage(String),
    /// Upload a replay to a paired server as a new party.
    Create { server: Url, token: String, path: PathBuf },
}

pub enum Msg {
    Session(Box<client::Update>),
    /// The party joined last is over for this app (left, failed or gone).
    SessionEnded,
    /// The next batch of described replays, in list order.
    Replays(Vec<LocalReplay>),
    ScanDone,
    MapImage {
        map: String,
        picture: Option<Picture>,
    },
    Created {
        server: Url,
        id: String,
    },
    CreateFailed(String),
    /// Another launch of the app handed over the link it was started with (`None`: it
    /// had none).
    Opened(Option<String>),
}

/// The window's wake-up handle, once the window exists.
pub type WakeSlot = Arc<Mutex<Option<Wakeup>>>;

pub struct Backend {
    commands: async_mpsc::UnboundedSender<Cmd>,
    messages: mpsc::Receiver<Msg>,
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
        let outbox = Outbox { messages: message_tx, wake };
        let to_window = outbox.clone();
        std::thread::Builder::new()
            .name("barwatch-backend".into())
            .spawn(move || {
                let runtime =
                    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("async runtime");
                runtime.block_on(work(command_rx, outbox));
            })
            .expect("backend thread");
        (Backend { commands, messages }, to_window)
    }

    /// A backend that does nothing, for rendering fixed scenes.
    pub fn inert() -> Backend {
        Backend { commands: async_mpsc::unbounded_channel().0, messages: mpsc::channel().1 }
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
            outbox.post(Msg::CreateFailed(format!("network setup failed: {e:#}")));
            return;
        }
    };
    let cache = crate::config::cache_dir();
    // The map list is fetched once, by the first picture request.
    let maps: Arc<tokio::sync::OnceCell<MapIndex>> = Arc::new(tokio::sync::OnceCell::new());
    let mut session: Option<async_mpsc::UnboundedSender<client::Command>> = None;
    let generation = Arc::new(AtomicU64::new(0));

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Cmd::Join { server, session: id, settings } => {
                if let Some(old) = session.take() {
                    let _ = old.send(client::Command::Leave);
                }
                let (tx, rx) = async_mpsc::unbounded_channel();
                session = Some(tx);
                // Only the party joined last may speak to the window: an older one
                // that is still winding down must not overwrite or end it.
                let mine = generation.fetch_add(1, Ordering::SeqCst) + 1;
                let current = generation.clone();
                let (http, outbox) = (http.clone(), outbox.clone());
                tokio::spawn(async move {
                    let (notify, live) = (outbox.clone(), current.clone());
                    let on_update = move |update| {
                        if live.load(Ordering::SeqCst) == mine {
                            notify.post(Msg::Session(Box::new(update)));
                        }
                    };
                    client::run(http, server, id, settings, rx, on_update).await;
                    if current.load(Ordering::SeqCst) == mine {
                        outbox.post(Msg::SessionEnded);
                    }
                });
            }
            Cmd::Session(command) => {
                if let Some(tx) = &session {
                    let _ = tx.send(command);
                }
                if command == client::Command::Leave {
                    session = None;
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
            Cmd::Create { server, token, path } => {
                let (http, outbox) = (http.clone(), outbox.clone());
                tokio::spawn(async move {
                    match client::create_session(&http, &server, &token, &path).await {
                        Ok(id) => outbox.post(Msg::Created { server, id }),
                        Err(e) => outbox.post(Msg::CreateFailed(format!("{e:#}"))),
                    }
                });
            }
        }
    }
}
