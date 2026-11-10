//! "Share my library on Tailscale": the remote library server from
//! `flick::remote::server`, run inside the desktop app instead of as
//! `flick-serve`. It runs on its own Tokio runtime thread, like `Services`,
//! and makes the same choices as `flick-serve --library`: the live
//! `settings.json` as the source, `<data dir>/remote` as the state folder,
//! the thumbnail cache for posters, and this machine's Tailscale IPv4
//! address on `protocol::DEFAULT_PORT`.
//!
//! The server re-reads `settings.json` for every listing, so videos added
//! while it runs appear once the desktop's background save lands. Before it
//! starts, the worker waits for saves queued until then, so the first
//! listing already sees the library the switch was turned on with.

use flick::remote::{
    auth, bind,
    protocol::{self, Pairing},
    server::{Server, ServerConfig, Sources},
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::mpsc,
    thread,
};

/// Shown when `tailscale ip -4` gives no Tailscale address.
pub const TAILSCALE_MISSING: &str = if cfg!(target_os = "macos") {
    "Tailscale is not running on this Mac"
} else {
    "Tailscale is not running on this computer"
};

/// What the settings row shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ServeStatus {
    #[default]
    Off,
    Starting,
    Running {
        listen: SocketAddr,
        /// `flick-remote://host:port/#token`, for the other Flick to paste.
        pairing: String,
    },
    Failed(String),
}

/// Runs before the server reads its sources, on the server thread.
pub type BeforeStart = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;

pub struct ServeOptions {
    /// The desktop's live `settings.json`.
    pub settings_path: PathBuf,
    /// Token, id salt, and transcode temporaries.
    pub state_dir: PathBuf,
    pub poster_cache: Option<PathBuf>,
    /// Empty uses the computer's name.
    pub name: String,
    /// Replaces the saved token first, revoking every paired Flick.
    pub new_token: bool,
    /// None binds to this machine's Tailscale IPv4 address on
    /// [`protocol::DEFAULT_PORT`]. Tests pass loopback.
    pub bind: Option<SocketAddr>,
    /// For example, waiting for queued settings saves to reach the disk.
    pub before_start: Option<BeforeStart>,
}

/// The in-app server's state. One per window; `start` replaces a running
/// server, `stop` and `shutdown` never block the UI thread.
pub struct RemoteServe {
    status: ServeStatus,
    /// Events from older starts are ignored.
    generation: u64,
    running: Option<Running>,
    /// A stopped server's thread, joined by the next start's thread or at quit.
    stopping: Option<thread::JoinHandle<()>>,
    events_tx: mpsc::Sender<(u64, ServeStatus)>,
    events: mpsc::Receiver<(u64, ServeStatus)>,
}

struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl Default for RemoteServe {
    fn default() -> Self {
        let (events_tx, events) = mpsc::channel();
        Self {
            status: ServeStatus::Off,
            generation: 0,
            running: None,
            stopping: None,
            events_tx,
            events,
        }
    }
}

impl RemoteServe {
    pub fn status(&self) -> &ServeStatus {
        &self.status
    }

    /// Starts a server, replacing one already running.
    pub fn start(&mut self, options: ServeOptions) {
        self.stop();
        self.generation += 1;
        self.status = ServeStatus::Starting;
        let generation = self.generation;
        let events = self.events_tx.clone();
        let previous = self.stopping.take();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let spawned = thread::Builder::new()
            .name("flick-serve".into())
            .spawn(move || {
                // The previous server must release the port and state lock.
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("flick-serve-worker")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = events.send((
                            generation,
                            ServeStatus::Failed(format!("Could not start sharing: {error}")),
                        ));
                        return;
                    }
                };
                runtime.block_on(serve(options, generation, events, stopped));
                runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            });
        match spawned {
            Ok(thread) => self.running = Some(Running { stop, thread }),
            Err(error) => {
                self.status = ServeStatus::Failed(format!("Could not start sharing: {error}"))
            }
        }
    }

    /// Shows why sharing could not start, without starting.
    pub fn fail(&mut self, message: String) {
        self.stop();
        self.status = ServeStatus::Failed(message);
    }

    /// Stops the server without waiting for it.
    pub fn stop(&mut self) {
        self.generation += 1;
        self.status = ServeStatus::Off;
        if let Some(running) = self.running.take() {
            let _ = running.stop.send(());
            if let Some(older) = self.stopping.replace(running.thread) {
                // Two stops without a start in between: the older one is
                // already ending; join it at quit.
                crate::services::register_shutdown_worker(older);
            }
        }
    }

    /// Stops the server and leaves its thread for `services::finish_shutdown`.
    pub fn shutdown(&mut self) {
        self.stop();
        if let Some(thread) = self.stopping.take() {
            crate::services::register_shutdown_worker(thread);
        }
    }

    /// Applies status changes from the server thread. True when the status
    /// changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok((generation, status)) = self.events.try_recv() {
            if generation == self.generation && self.status != status {
                if matches!(status, ServeStatus::Failed(_))
                    && let Some(running) = self.running.take()
                {
                    // The thread is ending on its own.
                    self.stopping = Some(running.thread);
                }
                self.status = status;
                changed = true;
            }
        }
        changed
    }
}

impl Drop for RemoteServe {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn serve(
    options: ServeOptions,
    generation: u64,
    events: mpsc::Sender<(u64, ServeStatus)>,
    stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let send = |status| {
        let _ = events.send((generation, status));
    };
    if let Some(before) = options.before_start {
        // Not fatal: the server re-reads the file for every listing.
        match tokio::task::spawn_blocking(before).await {
            Ok(Err(error)) => tracing::warn!(%error, "Settings were not saved before sharing"),
            Err(error) => tracing::warn!(%error, "Settings were not saved before sharing"),
            Ok(Ok(())) => {}
        }
    }
    let token = match auth::load_or_create_token(&options.state_dir, options.new_token) {
        Ok(token) => token,
        Err(error) => return send(ServeStatus::Failed(format!("{error:#}"))),
    };
    let bind = match options.bind {
        Some(bind) => bind,
        None => match bind::tailscale_ipv4().await {
            Ok(ip) => SocketAddr::new(IpAddr::V4(ip), protocol::DEFAULT_PORT),
            Err(error) => {
                tracing::info!(%error, "No Tailscale address for the in-app server");
                return send(ServeStatus::Failed(TAILSCALE_MISSING.into()));
            }
        },
    };
    let name = match options.name.trim() {
        "" => tokio::task::spawn_blocking(computer_name)
            .await
            .unwrap_or_else(|_| "Flick".into()),
        name => name.to_owned(),
    };
    let config = ServerConfig {
        bind,
        token: token.clone(),
        name,
        sources: Sources::SettingsFile(options.settings_path),
        poster_cache: options.poster_cache.filter(|dir| dir.is_dir()),
        state_dir: options.state_dir,
        ffmpeg: None,
        max_transcodes: 2,
    };
    let server = match Server::start(config).await {
        Ok(server) => server,
        Err(error) => return send(ServeStatus::Failed(format!("{error:#}"))),
    };
    let listen = server.addr();
    send(ServeStatus::Running {
        listen,
        pairing: Pairing {
            host: listen.ip().to_string(),
            port: listen.port(),
            token,
        }
        .to_string(),
    });
    // Resolves on stop, or when the view is gone and the sender dropped.
    let _ = stopped.await;
    tracing::info!("Stopping the in-app remote library server");
    server.shutdown().await;
}

/// The computer's friendly name on macOS, else the host name, as
/// `flick-serve` chooses it.
fn computer_name() -> String {
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|name| !name.is_empty())
    };
    let name = if cfg!(target_os = "macos") {
        run("scutil", &["--get", "ComputerName"])
    } else {
        None
    };
    name.or_else(|| run("hostname", &[]))
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "Flick".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        time::{Duration, Instant},
    };

    fn wait_for(serve: &mut RemoteServe, done: impl Fn(&ServeStatus) -> bool) -> ServeStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            serve.drain();
            if done(serve.status()) {
                return serve.status().clone();
            }
            assert!(Instant::now() < deadline, "status: {:?}", serve.status());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn get(listen: SocketAddr, path: &str, token: &str) -> String {
        let mut stream = std::net::TcpStream::connect(listen).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn options(dir: &std::path::Path, new_token: bool) -> ServeOptions {
        ServeOptions {
            settings_path: dir.join("settings.json"),
            state_dir: dir.join("data").join("remote"),
            poster_cache: None,
            name: "Test Library".into(),
            new_token,
            bind: Some("127.0.0.1:0".parse().unwrap()),
            before_start: None,
        }
    }

    #[test]
    fn serves_the_settings_file_until_stopped_and_new_token_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Clip.mp4");
        std::fs::write(&video, b"not really a video").unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            serde_json::json!({"library": [{"path": video, "title": "Clip"}]}).to_string(),
        )
        .unwrap();

        let mut serve = RemoteServe::default();
        assert_eq!(serve.status(), &ServeStatus::Off);
        let mut before = options(dir.path(), false);
        let (ran_tx, ran) = mpsc::channel();
        before.before_start = Some(Box::new(move || {
            ran_tx.send(()).unwrap();
            Ok(())
        }));
        serve.start(before);
        assert!(serve.running.is_some());
        let ServeStatus::Running { listen, pairing } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not start: {:?}", serve.status());
        };
        ran.try_recv().expect("before_start runs before serving");
        let parsed = Pairing::parse(&pairing).unwrap();
        assert_eq!(
            (parsed.host.as_str(), parsed.port),
            ("127.0.0.1", listen.port())
        );
        let listing = get(listen, "/v1/library", &parsed.token);
        assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
        assert!(listing.contains("\"Test Library\""), "{listing}");
        assert!(listing.contains("\"Clip\""), "{listing}");
        assert!(get(listen, "/v1/library", &"0".repeat(64)).starts_with("HTTP/1.1 401"));

        // A new token restarts the server and revokes the old one.
        serve.start(options(dir.path(), true));
        let ServeStatus::Running {
            listen: second,
            pairing: renewed,
        } = wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not restart: {:?}", serve.status());
        };
        let renewed = Pairing::parse(&renewed).unwrap();
        assert_ne!(renewed.token, parsed.token);
        assert!(get(second, "/v1/library", &parsed.token).starts_with("HTTP/1.1 401"));
        assert!(get(second, "/v1/library", &renewed.token).starts_with("HTTP/1.1 200"));

        serve.stop();
        assert_eq!(serve.status(), &ServeStatus::Off);
        assert!(serve.running.is_none());
        // Late events from the stopped server are ignored.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!serve.drain());
        let thread = serve.stopping.take().unwrap();
        thread.join().unwrap();
        assert!(std::net::TcpStream::connect(second).is_err());
    }

    #[test]
    fn a_refused_address_is_reported_inline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let mut serve = RemoteServe::default();
        let mut refused = options(dir.path(), false);
        refused.bind = Some("192.168.1.10:0".parse().unwrap());
        serve.start(refused);
        let ServeStatus::Failed(message) =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("a LAN address must be refused");
        };
        assert!(message.contains("not a Tailscale address"), "{message}");
        assert!(serve.running.is_none());
    }
}
