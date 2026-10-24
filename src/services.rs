//! Background search and one shared native transfer session for the desktop.
use flick::{
    catalog::{self, SearchResults},
    torrent::{
        ResolvedTorrent, SharedTorrent, TorrentInput, TransferConfig, TransferSession,
        TransferSnapshot,
    },
};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};
use tokio::{sync::mpsc as channel, task::JoinSet};

pub enum Command {
    Search {
        generation: u64,
        query: String,
    },
    Resolve(TorrentInput),
    Download {
        torrent: ResolvedTorrent,
        files: Vec<usize>,
        folder: PathBuf,
    },
    Pause(String),
    Resume(String),
    Remove(String),
    Share(PathBuf),
    Load,
    CancelSearch,
    Shutdown,
}
pub enum Event {
    Search {
        generation: u64,
        result: Result<SearchResults, String>,
        done: bool,
    },
    Resolved(Result<ResolvedTorrent, String>),
    Shared(Result<SharedTorrent, String>),
    Progress(Vec<TransferSnapshot>),
    Completed(PathBuf),
    Error(String),
}

pub struct Services {
    sender: channel::UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
    worker: Option<thread::JoinHandle<()>>,
    pub download_dir: PathBuf,
}

impl Services {
    pub fn start() -> Self {
        let base = std::env::var_os("FLICK_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("app", "Flick", "Flick")
                    .map(|d| d.data_local_dir().to_owned())
            })
            .unwrap_or_else(|| std::env::temp_dir().join("flick"));
        let download_dir = std::env::var_os("FLICK_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                directories::UserDirs::new()
                    .and_then(|d| d.download_dir().map(PathBuf::from))
                    .unwrap_or_else(|| base.clone())
                    .join("Flick")
            });
        let config = TransferConfig {
            download_dir: download_dir.clone(),
            state_dir: base.join("transfers"),
            listen_addr: (
                if std::env::var_os("FLICK_LOCAL_TRANSFERS").is_some() {
                    [127, 0, 0, 1]
                } else {
                    [0, 0, 0, 0]
                },
                0,
            )
                .into(),
            public_discovery: std::env::var_os("FLICK_LOCAL_TRANSFERS").is_none(),
            download_limit_bytes_per_second: None,
        };
        let (sender, receiver) = channel::unbounded_channel();
        let peers = std::env::var("FLICK_INITIAL_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|v| v.parse().ok())
            .collect::<Vec<std::net::SocketAddr>>();
        let (events_tx, events) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("flick-transfers".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = events_tx
                            .send(Event::Error(format!("Downloads could not start: {error}")));
                        return;
                    }
                };
                runtime.block_on(run(config, peers, receiver, events_tx));
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .expect("create Flick background worker");
        Self {
            sender,
            events,
            worker: Some(worker),
            download_dir,
        }
    }
    pub fn send(&self, command: Command) -> Result<(), String> {
        self.sender
            .send(command)
            .map_err(|_| "Downloads are unavailable. Restart Flick to try again.".into())
    }
    pub fn drain(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }
}
impl Drop for Services {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn run(
    config: TransferConfig,
    initial_peers: Vec<std::net::SocketAddr>,
    mut commands: channel::UnboundedReceiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let mut session: Option<Arc<TransferSession>> = None;
    let mut tasks = JoinSet::new();
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut completed = HashSet::new();
    let mut timer = tokio::time::interval(Duration::from_millis(700));
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if matches!(command, Command::Shutdown) { break; }
                if matches!(command, Command::CancelSearch) { if let Some(task) = search_task.take() { task.abort(); } continue; }
                if let Command::Search { generation, query } = command {
                    if let Some(task) = search_task.take() { task.abort(); }
                    let events = events.clone();
                    search_task = Some(tasks.spawn(async move {
                        let result = catalog::search(&query, |partial| { let _ = events.send(Event::Search {generation, result:Ok(partial), done:false}); }).await.map_err(|e| e.to_string());
                        let _ = events.send(Event::Search { generation, result, done:true });
                    }));
                    continue;
                }
                if session.is_none() {
                    match TransferSession::start(config.clone()).await {
                        Ok(started) => session = Some(Arc::new(started)),
                        Err(error) => { let _ = events.send(Event::Error(format!("Downloads are unavailable: {error:#}"))); continue; }
                    }
                }
                let session = session.as_ref().unwrap().clone();
                let peers = initial_peers.clone();
                let events = events.clone();
                tasks.spawn(async move {
                    let result: anyhow::Result<()> = match command {
                        Command::Resolve(input) => {
                            let result = session.resolve(input, peers, Duration::from_secs(30)).await.map_err(|e| format!("Could not read torrent files: {e:#}"));
                            let _ = events.send(Event::Resolved(result)); Ok(())
                        }
                        Command::Download { torrent, files, folder } => {
                            match session.download_to(&torrent, Some(files), &folder).await {
                                Ok(id) => session.resume(&id, peers).await,
                                Err(error) => Err(error),
                            }
                        }
                        Command::Pause(id) => session.pause(&id).await,
                        Command::Resume(id) => session.resume(&id, peers).await,
                        Command::Remove(id) => session.remove(&id, false).await,
                        Command::Share(path) => {
                            let result = session.share(&path, vec![]).await.map_err(|e| format!("Could not share these files: {e:#}"));
                            let _ = events.send(Event::Shared(result)); Ok(())
                        }
                        _ => Ok(())
                    };
                    if let Err(error) = result { let _ = events.send(Event::Error(format!("Transfer failed: {error:#}"))); }
                });
            }
            _ = timer.tick() => {
                if let Some(session) = &session {
                    match session.snapshots() {
                        Ok(snapshots) => {
                            for snapshot in &snapshots {
                                for file in &snapshot.files {
                                    let key = (snapshot.infohash.clone(), file.index);
                                    if !completed.contains(&key) && flick::library::is_video(&file.path) && let Ok(path) = session.completed_file(&snapshot.infohash, file.index) {
                                        completed.insert(key); let _ = events.send(Event::Completed(path));
                                    }
                                }
                            }
                            if events.send(Event::Progress(snapshots)).is_err() { break; }
                        }
                        Err(error) => { let _ = events.send(Event::Error(format!("Could not update downloads: {error}"))); }
                    }
                }
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    if let Some(session) = session {
        session.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_transfers_publish_completed_videos_and_stop_cleanly() -> anyhow::Result<()>
    {
        let root = tempfile::tempdir()?;
        let video = root.path().join("video.mp4");
        tokio::fs::write(&video, vec![42u8; 65_537]).await?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events));
        commands.send(Command::Share(video.clone()))?;
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            let mut shared = false;
            let mut completed = false;
            while !shared || !completed {
                match output.try_recv() {
                    Ok(Event::Shared(Ok(_))) => shared = true,
                    Ok(Event::Completed(path)) => {
                        assert_eq!(path, video.canonicalize()?);
                        completed = true;
                    }
                    Ok(Event::Error(error)) | Ok(Event::Shared(Err(error))) => anyhow::bail!(error),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        Ok(())
    }
}
