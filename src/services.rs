//! Background search and one shared native transfer session for the desktop.
use flick::{
    catalog::{self, SearchResults},
    fetch::{self, FetchSource, FetchUpdate, archive::ArchiveItem},
    torrent::{
        ResolvedTorrent, SharedTorrent, TorrentInput, TransferConfig, TransferSession,
        TransferSnapshot,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};
use tokio::{sync::mpsc as channel, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub enum Command {
    /// Download a web page's video with yt-dlp, or files over HTTPS. These
    /// run apart from the torrent session.
    Fetch {
        id: u64,
        source: FetchSource,
        folder: PathBuf,
    },
    CancelFetch(u64),
    /// List an Internet Archive item's video files.
    ResolveArchive(String),
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
    Fetch {
        id: u64,
        update: FetchUpdate,
    },
    ArchiveResolved {
        identifier: String,
        result: Result<ArchiveItem, String>,
    },
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
    let mut fetches = Fetches::default();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if matches!(command, Command::Shutdown) { break; }
                let Some(command) = fetches.handle(command, &events) else { continue; };
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
    fetches.shutdown().await;
    tasks.abort_all();
    if let Some(session) = session {
        session.shutdown().await;
    }
}

/// Link downloads, keyed by the identifier the UI chose.
#[derive(Default)]
struct Fetches {
    jobs: HashMap<u64, (CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl Fetches {
    /// Starts or cancels link downloads. Returns any other command.
    fn handle(&mut self, command: Command, events: &mpsc::Sender<Event>) -> Option<Command> {
        self.jobs.retain(|_, (_, job)| !job.is_finished());
        match command {
            Command::Fetch { id, source, folder } => {
                let cancel = CancellationToken::new();
                let job = tokio::spawn(fetch_job(
                    id,
                    source,
                    folder,
                    cancel.clone(),
                    events.clone(),
                ));
                if let Some((previous, _)) = self.jobs.insert(id, (cancel, job)) {
                    previous.cancel();
                }
            }
            Command::CancelFetch(id) => {
                if let Some((cancel, _)) = self.jobs.get(&id) {
                    cancel.cancel();
                }
            }
            Command::ResolveArchive(identifier) => {
                let events = events.clone();
                tokio::spawn(async move {
                    let result = fetch::archive::resolve(&identifier)
                        .await
                        .map_err(|e| format!("Could not list this Internet Archive item: {e:#}"));
                    let _ = events.send(Event::ArchiveResolved { identifier, result });
                });
            }
            command => return Some(command),
        }
        None
    }

    /// Cancels running downloads and waits briefly for their cleanup.
    async fn shutdown(&mut self) {
        for (cancel, _) in self.jobs.values() {
            cancel.cancel();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for (_, (_, job)) in self.jobs.drain() {
            let _ = tokio::time::timeout_at(deadline, job).await;
        }
    }
}

async fn fetch_job(
    id: u64,
    source: FetchSource,
    folder: PathBuf,
    cancel: CancellationToken,
    events: mpsc::Sender<Event>,
) {
    let result = fetch::fetch(source, &folder, &cancel, |update| {
        if let FetchUpdate::Saved(path) = &update
            && flick::library::is_video(path)
        {
            let _ = events.send(Event::Completed(path.clone()));
        }
        let _ = events.send(Event::Fetch { id, update });
    })
    .await;
    let update = match result {
        Ok(_) => FetchUpdate::Done,
        Err(_) if cancel.is_cancelled() => FetchUpdate::Cancelled,
        Err(error) => FetchUpdate::Failed(format!("{error:#}")),
    };
    let _ = events.send(Event::Fetch { id, update });
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn link_downloads_work_and_cancel_when_the_torrent_session_cannot_start()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        // A file where the state directory belongs, so the session fails.
        let blocked = root.path().join("state");
        std::fs::write(&blocked, b"")?;
        let config = TransferConfig::local(root.path().join("downloads"), blocked);
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events));
        // Accepts connections but never answers, so the download stays open.
        let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let folder = root.path().join("links");
        commands.send(Command::Load)?;
        commands.send(Command::Fetch {
            id: 1,
            source: FetchSource::Page("ftp://example.com/video".into()),
            folder: folder.clone(),
        })?;
        commands.send(Command::Fetch {
            id: 2,
            source: FetchSource::Files {
                folder: None,
                files: vec![fetch::RemoteFile {
                    url: format!("https://{}/clip.mp4", stalled.local_addr()?),
                    name: "clip.mp4".into(),
                    size: None,
                    sha1: None,
                }],
            },
            folder: folder.clone(),
        })?;
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            let (mut unavailable, mut rejected, mut cancelled, mut cancelling) =
                (false, false, false, false);
            while !(unavailable && rejected && cancelled) {
                match output.try_recv() {
                    Ok(Event::Error(error)) => {
                        assert!(error.contains("Downloads are unavailable"), "{error}");
                        unavailable = true;
                    }
                    Ok(Event::Fetch {
                        id: 1,
                        update: FetchUpdate::Failed(error),
                    }) => {
                        assert!(error.contains("http and https"), "{error}");
                        rejected = true;
                    }
                    Ok(Event::Fetch {
                        id: 2,
                        update: FetchUpdate::Progress(_),
                    }) if !cancelling => {
                        commands.send(Command::CancelFetch(2))?;
                        cancelling = true;
                    }
                    Ok(Event::Fetch {
                        id: 2,
                        update: FetchUpdate::Cancelled,
                    }) => cancelled = true,
                    Ok(Event::Fetch {
                        update: FetchUpdate::Failed(error),
                        ..
                    }) => anyhow::bail!(error),
                    Ok(Event::Fetch {
                        update: FetchUpdate::Saved(path),
                        ..
                    }) => anyhow::bail!("Unexpected download: {}", path.display()),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        assert_eq!(std::fs::read_dir(&folder)?.count(), 0);
        Ok(())
    }
}
