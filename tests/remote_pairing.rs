//! The routes that need no token, `GET /v1/hello` and `POST /v1/pair`, and
//! the `flick-serve` commands for codes and status. Loopback only, with
//! generated fixtures in temporary folders.

use anyhow::{Context, Result, ensure};
use flick::remote::{
    auth,
    pairing::{CODE_DIGITS, MAX_WRONG_ATTEMPTS},
    protocol::{self, ErrorBody, Hello, Library, PairReply, PairRequest, Pairing, paths},
    server::{self, PairingCodes, Server, ServerConfig, Sources},
};
use reqwest::{StatusCode, header};
use std::{path::PathBuf, time::Duration};
use tempfile::TempDir;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(30);

struct Fixture {
    _root: TempDir,
    media: PathBuf,
    state: PathBuf,
    token: String,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let media = root.path().join("media");
        std::fs::create_dir_all(&media)?;
        std::fs::write(media.join("Film.mp4"), vec![7u8; 4096])?;
        std::fs::write(media.join("Other.mkv"), vec![9u8; 2048])?;
        let state = root.path().join("state");
        let token = auth::load_or_create_token(&state, false)?;
        Ok(Self {
            media,
            state,
            token,
            _root: root,
        })
    }

    async fn start(&self, pairing: Option<PairingCodes>) -> Result<Running> {
        let server = Server::start(ServerConfig {
            bind: "127.0.0.1:0".parse()?,
            token: self.token.clone(),
            name: "Pairing Mini".into(),
            sources: Sources::Folders(vec![self.media.clone()]),
            poster_cache: None,
            state_dir: self.state.clone(),
            ffmpeg: Some(self.state.join("no-ffmpeg")),
            max_transcodes: 1,
            transfers: None,
            pairing,
            advertise_hardware: true,
        })
        .await?;
        Ok(Running {
            base: format!("http://{}", server.addr()),
            client: client()?,
            server,
        })
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()?)
}

struct Running {
    server: Server,
    base: String,
    client: reqwest::Client,
}

impl Running {
    async fn hello(&self) -> Result<Hello> {
        let response = self
            .client
            .get(format!("{}{}", self.base, paths::hello()))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        Ok(response.json().await?)
    }

    async fn pair(&self, code: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .post(format!("{}{}", self.base, paths::pair()))
            .json(&PairRequest { code: code.into() })
            .send()
            .await?)
    }

    fn codes(&self) -> &PairingCodes {
        self.server.pairing().expect("started with pairing codes")
    }
}

async fn message(response: reqwest::Response) -> Result<String> {
    Ok(response.json::<ErrorBody>().await?.message)
}

/// A code that is not `code`.
fn other_than(code: &str) -> String {
    if code == "000000" {
        "000001".into()
    } else {
        "000000".into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_needs_no_token_and_says_nothing_else() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start(Some(PairingCodes::new())).await?;
        let hello = running.hello().await?;
        ensure!(
            hello
                == Hello {
                    protocol: protocol::VERSION,
                    server_name: "Pairing Mini".into(),
                    pairing_open: false,
                },
            "{hello:?}"
        );
        running.codes().open();
        ensure!(running.hello().await?.pairing_open);
        running.codes().close();
        ensure!(!running.hello().await?.pairing_open);

        // Everything else still needs the token, including near misses.
        for path in [
            paths::library(),
            "/v1/hello/".to_owned(),
            "/v1/hellox".to_owned(),
            server::LOCAL_PAIR_CODE_PATH.to_owned(),
        ] {
            let response = running
                .client
                .get(format!("{}{path}", running.base))
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::UNAUTHORIZED,
                "{path}: {}",
                response.status()
            );
        }
        let response = running
            .client
            .post(format!("{}{}", running.base, server::LOCAL_PAIR_CODE_PATH))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        // The wrong method is refused with a JSON body.
        let response = running
            .client
            .post(format!("{}{}", running.base, paths::hello()))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::METHOD_NOT_ALLOWED);
        message(response).await?;

        // Without pairing codes, hello still answers, never open.
        running.server.shutdown().await;
        let closed = fixture.start(None).await?;
        ensure!(!closed.hello().await?.pairing_open);
        ensure!(closed.server.pairing().is_none());
        closed.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_right_code_pairs_once() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start(Some(PairingCodes::new())).await?;
        let code = running.codes().open();
        ensure!(
            code.len() == CODE_DIGITS && code.bytes().all(|b| b.is_ascii_digit()),
            "{code}"
        );
        // Spaces the owner may type are ignored.
        let typed = format!("{} {}", &code[..3], &code[3..]);
        let response = running.pair(&typed).await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        let reply: PairReply = response.json().await?;
        let pairing = Pairing::parse(&reply.pairing)?;
        let addr = running.server.addr();
        ensure!(pairing.host == addr.ip().to_string() && pairing.port == addr.port());
        ensure!(
            pairing.token == fixture.token,
            "the reply carries the server's token"
        );
        // The pairing string works.
        let library: Library = running
            .client
            .get(format!("{}{}", pairing.base_url(), paths::library()))
            .header(header::AUTHORIZATION, protocol::bearer(&pairing.token))
            .send()
            .await?
            .json()
            .await?;
        ensure!(library.videos.len() == 2);

        // The code is closed now.
        ensure!(!running.codes().is_open());
        ensure!(!running.hello().await?.pairing_open);
        let response = running.pair(&code).await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        ensure!(message(response).await? == server::NO_CODE_MESSAGE);

        // A newer code replaces an older one.
        let first = running.codes().open();
        let second = running.codes().open();
        if first != second {
            let response = running.pair(&first).await?;
            ensure!(response.status() == StatusCode::FORBIDDEN);
        }
        ensure!(running.pair(&second).await?.status() == StatusCode::OK);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_wrong_codes_close_it() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start(Some(PairingCodes::new())).await?;
        let code = running.codes().open();
        let wrong = other_than(&code);
        for left in (1..MAX_WRONG_ATTEMPTS).rev() {
            let response = running.pair(&wrong).await?;
            ensure!(
                response.status() == StatusCode::FORBIDDEN,
                "{}",
                response.status()
            );
            let text = message(response).await?;
            ensure!(text.contains(&format!("{left} attempt")), "{text}");
            ensure!(!text.contains(&code), "the message never reveals the code");
        }
        let response = running.pair(&wrong).await?;
        ensure!(response.status() == StatusCode::FORBIDDEN);
        ensure!(message(response).await?.contains("closed"));
        ensure!(!running.hello().await?.pairing_open);
        // Even the right code is refused now.
        let response = running.pair(&code).await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_without_an_open_code_is_not_found() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        // No pairing codes at all, and codes that were never opened.
        for pairing in [None, Some(PairingCodes::new())] {
            let running = fixture.start(pairing).await?;
            let response = running.pair("123456").await?;
            ensure!(
                response.status() == StatusCode::NOT_FOUND,
                "{}",
                response.status()
            );
            let text = message(response).await?;
            ensure!(text == server::NO_CODE_MESSAGE, "{text}");
            ensure!(text.contains("Settings → Remote Libraries") && text.contains("--pair-code"));
            running.server.shutdown().await;
        }
        // A code that expired is closed too.
        let running = fixture
            .start(Some(PairingCodes::with_lifetime(Duration::from_millis(
                200,
            ))))
            .await?;
        let code = running.codes().open();
        tokio::time::sleep(Duration::from_millis(400)).await;
        ensure!(running.pair(&code).await?.status() == StatusCode::NOT_FOUND);
        // An open code with a body that is not a pair request.
        running.codes().open();
        let response = running
            .client
            .post(format!("{}{}", running.base, paths::pair()))
            .body("123456")
            .send()
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        message(response).await?;
        ensure!(
            running.codes().is_open(),
            "a malformed request is not an attempt"
        );
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthenticated_routes_are_rate_limited() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start(Some(PairingCodes::new())).await?;
        let limit = server::UNAUTHENTICATED_PER_MINUTE;
        for _ in 0..limit {
            running.hello().await?;
        }
        let response = running
            .client
            .get(format!("{}{}", running.base, paths::hello()))
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::TOO_MANY_REQUESTS,
            "{}",
            response.status()
        );
        let retry: u64 = response
            .headers()
            .get(header::RETRY_AFTER)
            .context("Retry-After")?
            .to_str()?
            .parse()?;
        ensure!((1..=60).contains(&retry), "{retry}");
        message(response).await?;

        // Pairing has its own budget, which runs out the same way, and a
        // refused request is not counted as a wrong code.
        let code = running.codes().open();
        let wrong = other_than(&code);
        for attempt in 0..limit {
            let response = running.pair(&wrong).await?;
            let expected = if attempt < MAX_WRONG_ATTEMPTS {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            };
            ensure!(
                response.status() == expected,
                "{attempt}: {}",
                response.status()
            );
        }
        let response = running.pair(&wrong).await?;
        ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
        // Routes with the token are not limited.
        for _ in 0..5 {
            let response = running
                .client
                .get(format!("{}{}", running.base, paths::library()))
                .header(header::AUTHORIZATION, protocol::bearer(&fixture.token))
                .send()
                .await?;
            ensure!(response.status() == StatusCode::OK);
        }
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

/// The local route that `flick-serve pair-code` uses refuses without the
/// token, and opens a code with it from this machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_route_opens_a_code() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start(Some(PairingCodes::new())).await?;
        let url = format!("{}{}", running.base, server::LOCAL_PAIR_CODE_PATH);
        let response = running
            .client
            .post(&url)
            .header(header::AUTHORIZATION, protocol::bearer(&fixture.token))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        let opened: serde_json::Value = response.json().await?;
        let code = opened["pair_code"]
            .as_str()
            .context("pair_code")?
            .to_owned();
        let expires = opened["expires_in"].as_u64().context("expires_in")?;
        ensure!(
            code.len() == CODE_DIGITS && (590..=600).contains(&expires),
            "{opened}"
        );
        ensure!(running.pair(&code).await?.status() == StatusCode::OK);
        running.server.shutdown().await;

        let closed = fixture.start(None).await?;
        let response = closed
            .client
            .post(format!("{}{}", closed.base, server::LOCAL_PAIR_CODE_PATH))
            .header(header::AUTHORIZATION, protocol::bearer(&fixture.token))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        closed.server.shutdown().await;
        Ok(())
    })
    .await?
}

fn flick_serve(state: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_flick-serve"));
    command
        .arg("--state-dir")
        .arg(state)
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

async fn json_output(mut command: tokio::process::Command) -> Result<(bool, serde_json::Value)> {
    let output = command.output().await?;
    let line: serde_json::Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok((output.status.success(), line))
}

/// `--pair-code` prints a code with the startup line; `status` and
/// `pair-code` talk to the running server through its state folder; after
/// it stops, `status` says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flick_serve_codes_and_status() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let (running, line) = json_output({
            let mut command = flick_serve(&fixture.state);
            command.arg("status");
            command
        })
        .await?;
        ensure!(!running && line["running"] == false, "{line}");

        let mut child = flick_serve(&fixture.state)
            .args(["--bind", "127.0.0.1:0", "--pair-code", "--no-wake-info"])
            .arg("--ffmpeg")
            .arg(fixture.state.join("no-ffmpeg"))
            .arg("--folder")
            .arg(&fixture.media)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("stdout")?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
        let line: serde_json::Value =
            serde_json::from_str(&lines.next_line().await?.context("startup line")?)?;
        let pairing = Pairing::parse(line["pairing"].as_str().context("pairing")?)?;
        let code = line["pair_code"].as_str().context("pair_code")?.to_owned();
        ensure!(code.len() == CODE_DIGITS, "{line}");
        ensure!(
            (590..=600).contains(&line["expires_in"].as_u64().unwrap_or(0)),
            "{line}"
        );
        let base = pairing.base_url();
        let http = client()?;
        let pair = |code: String| {
            let http = http.clone();
            let url = format!("{base}{}", paths::pair());
            async move {
                Ok::<_, anyhow::Error>(http.post(url).json(&PairRequest { code }).send().await?)
            }
        };
        let reply: PairReply = pair(code.clone()).await?.json().await?;
        ensure!(Pairing::parse(&reply.pairing)? == pairing);
        let library: Library = http
            .get(format!("{base}{}", paths::library()))
            .header(header::AUTHORIZATION, protocol::bearer(&pairing.token))
            .send()
            .await?
            .json()
            .await?;
        ensure!(library.hardware.is_none(), "--no-wake-info leaves it out");

        let (running, status) = json_output({
            let mut command = flick_serve(&fixture.state);
            command.arg("status");
            command
        })
        .await?;
        ensure!(running && status["running"] == true, "{status}");
        ensure!(status["listen"] == line["listen"], "{status}");
        ensure!(
            status["videos"] == 2 && status["incomplete_videos"] == 0,
            "{status}"
        );
        ensure!(
            status["pairing_open"] == false,
            "the code was used: {status}"
        );
        ensure!(status["transfers"].is_null(), "no downloads: {status}");
        ensure!(
            status["free_bytes"].as_u64().is_some_and(|free| free > 0),
            "{status}"
        );
        ensure!(
            status["qualities"] == serde_json::json!(["original"]),
            "{status}"
        );

        // A code for later, from another process.
        let (ok, opened) = json_output({
            let mut command = flick_serve(&fixture.state);
            command.arg("pair-code");
            command
        })
        .await?;
        ensure!(ok, "{opened}");
        let later = opened["pair_code"]
            .as_str()
            .context("pair_code")?
            .to_owned();
        ensure!(opened["listen"] == line["listen"], "{opened}");
        ensure!(pair(later).await?.status() == StatusCode::OK);

        #[cfg(unix)]
        {
            let pid = child.id().context("pid")? as libc::pid_t;
            ensure!(unsafe { libc::kill(pid, libc::SIGTERM) } == 0);
            ensure!(child.wait().await?.success());
            ensure!(
                !fixture.state.join(server::LISTEN_FILE).exists(),
                "the listen file is removed"
            );
            let (running, line) = json_output({
                let mut command = flick_serve(&fixture.state);
                command.arg("status");
                command
            })
            .await?;
            ensure!(!running && line["running"] == false, "{line}");
            let output = {
                let mut command = flick_serve(&fixture.state);
                command.arg("pair-code");
                command.output().await?
            };
            ensure!(!output.status.success() && output.stdout.is_empty());
        }
        #[cfg(not(unix))]
        child.kill().await?;
        Ok(())
    })
    .await?
}
