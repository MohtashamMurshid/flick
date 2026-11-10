//! HLS transcode delivery over loopback.
//!
//! The stand-in tests drive a generated shell script in place of FFmpeg, so
//! they run anywhere with `/bin/sh`. The `#[ignore]` test uses real FFmpeg
//! on a generated clip:
//!
//! ```sh
//! bash scripts/make-test-video.sh /tmp/flick-hls-test.mp4 --with-subtitles
//! FLICK_TEST_VIDEO=/tmp/flick-hls-test.mp4 \
//!   cargo test --locked --no-default-features --test remote_hls -- --ignored --nocapture
//! ```

use anyhow::{Context, Result, ensure};
use flick::remote::{
    hls::{HlsState, ResolvedVideo, router, segment_count},
    protocol::{Quality, paths},
    transcode::{self, Encoder},
};
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::time::timeout;

async fn serve(state: Arc<HlsState>) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let app = router(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), server))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// Runs a test body with a fixed overall deadline.
async fn within<T>(seconds: u64, body: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(Duration::from_secs(seconds), body)
        .await
        .context("the test did not finish in time")?
}

#[cfg(unix)]
mod stand_in {
    use super::*;
    use flick::remote::{
        hls::{HlsLimits, Resolver, playlist},
        protocol::ErrorBody,
        transcode::MediaInfo,
    };
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};
    use tempfile::TempDir;

    /// A test server whose FFmpeg is a generated shell script.
    struct Fixture {
        root: TempDir,
        temp: PathBuf,
        base: String,
        state: Arc<HlsState>,
        http: reqwest::Client,
        server: tokio::task::JoinHandle<()>,
    }

    struct Script {
        /// The script stops after this many segments in total, like FFmpeg
        /// at the end of the source.
        total: u32,
        /// Exit with an error instead of writing this segment.
        fail_at: Option<u32>,
        /// Seconds per segment.
        delay: &'static str,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                total: 100,
                fail_at: None,
                delay: "0.05",
            }
        }
    }

    fn limits() -> HlsLimits {
        HlsLimits {
            idle: Duration::from_secs(30),
            segment_wait: Duration::from_secs(10),
            poll: Duration::from_millis(20),
            ..Default::default()
        }
    }

    /// Writes the stand-in. Like FFmpeg with `-f hls -hls_flags temp_file`,
    /// it reads `-ss`, `-start_number`, and `-hls_segment_filename`, writes
    /// `init.mp4`, then writes each `{n}.m4s` under a temporary name and
    /// renames it when complete. It logs one line per run.
    fn write_script(dir: &Path, script: &Script) -> PathBuf {
        let path = dir.join("ffmpeg");
        std::fs::write(dir.join("delay"), script.delay).unwrap();
        let text = format!(
            r#"#!/bin/sh
start=0; ss=''; pattern=''; prev=''
for arg in "$@"; do
  case "$prev" in
    -start_number) start="$arg" ;;
    -ss) ss="$arg" ;;
    -hls_segment_filename) pattern="$arg" ;;
  esac
  prev="$arg"
done
out="${{pattern%/*}}"
printf 'start=%s ss=%s pid=%s\n' "$start" "$ss" "$$" >> '{log}'
printf 'init from %s\n' "$start" > "$out/init.mp4"
n=$start
while [ "$n" -lt {total} ]; do
  sleep "$(cat '{delay}')"
  if [ "$n" = '{fail_at}' ]; then
    echo 'Error while filtering: stand-in failure' >&2
    exit 1
  fi
  printf 'segment %s from %s\n' "$n" "$start" > "$out/$n.m4s.tmp"
  mv "$out/$n.m4s.tmp" "$out/$n.m4s"
  n=$((n + 1))
done
exit 0
"#,
            log = dir.join("runs.log").display(),
            delay = dir.join("delay").display(),
            total = script.total,
            fail_at = script
                .fail_at
                .map_or(String::from("none"), |n| n.to_string()),
        );
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn resolver(root: &Path) -> Resolver {
        let source = root.join("film.mkv");
        std::fs::write(&source, b"not really a video").unwrap();
        Arc::new(move |id: &str| {
            let duration = match id {
                "film" => 400.0,   // 100 segments
                "short" => 10.5,   // 3 segments
                "other" => 40.0,   // 10 segments
                "partial" => 40.0, // still downloading
                "unknown-length" => 0.0,
                _ => return None,
            };
            Some(ResolvedVideo {
                path: source.clone(),
                info: MediaInfo {
                    duration,
                    width: 1920,
                    height: 1080,
                    frame_rate: 24.0,
                    ..Default::default()
                },
                complete: id != "partial",
            })
        })
    }

    async fn fixture(script: Script, max_jobs: usize, limits: HlsLimits) -> Result<Fixture> {
        let root = tempfile::tempdir()?;
        let ffmpeg = write_script(root.path(), &script);
        let temp = root.path().join("hls");
        std::fs::create_dir_all(&temp)?;
        let encoder = Encoder {
            ffmpeg,
            ffprobe: "/nonexistent/ffprobe".into(),
            video_encoder: "libx264".into(),
        };
        let state = Arc::new(
            HlsState::new(Some(encoder), max_jobs, temp.clone(), resolver(root.path()))
                .with_limits(limits),
        );
        let (base, server) = serve(state.clone()).await?;
        Ok(Fixture {
            root,
            temp,
            base,
            state,
            http: client(),
            server,
        })
    }

    impl Fixture {
        async fn get(&self, path: &str) -> Result<reqwest::Response> {
            Ok(self.http.get(format!("{}{path}", self.base)).send().await?)
        }

        async fn text(&self, path: &str) -> Result<String> {
            let response = self.get(path).await?;
            ensure!(
                response.status() == 200,
                "{path}: {} {}",
                response.status(),
                response.text().await?
            );
            Ok(response.text().await?)
        }

        async fn segment(&self, id: &str, quality: Quality, n: u32) -> Result<String> {
            self.text(&paths::hls_segment(id, quality, n)).await
        }

        /// (start, ss, pid) of every stand-in run so far.
        fn runs(&self) -> Vec<(u32, String, i32)> {
            std::fs::read_to_string(self.root.path().join("runs.log"))
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    let field = |name: &str| {
                        line.split(' ')
                            .find_map(|f| f.strip_prefix(&format!("{name}=")))
                            .unwrap()
                            .to_owned()
                    };
                    (
                        field("start").parse().unwrap(),
                        field("ss"),
                        field("pid").parse().unwrap(),
                    )
                })
                .collect()
        }

        fn job_folders(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.temp)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect()
        }

        async fn shutdown(self) {
            self.state.shutdown().await;
            self.server.abort();
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: kill with signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn until(what: &str, mut check: impl FnMut() -> bool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !check() {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn error_message(response: reqwest::Response) -> Result<String> {
        Ok(response.json::<ErrorBody>().await?.message)
    }

    #[tokio::test]
    async fn playlist_route_needs_no_job() -> Result<()> {
        within(30, async {
            let f = fixture(Script::default(), 2, limits()).await?;
            let response = f.get(&paths::hls_playlist("film", Quality::Low720)).await?;
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.headers()["content-type"],
                "application/vnd.apple.mpegurl"
            );
            let body = response.text().await?;
            assert_eq!(body, playlist(400.0, Quality::Low720));
            assert_eq!(body.matches(".m4s").count(), 100);
            assert_eq!(f.state.job_count(), 0);
            assert!(f.runs().is_empty());
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn unknown_ids_tiers_and_files_are_404() -> Result<()> {
        within(30, async {
            let f = fixture(Script::default(), 2, limits()).await?;
            for path in [
                "/v1/hls/missing/720p/index.m3u8".to_owned(),
                "/v1/hls/missing/720p/0.m4s".to_owned(),
                "/v1/hls/film/original/index.m3u8".to_owned(),
                "/v1/hls/film/original/0.m4s".to_owned(),
                "/v1/hls/film/4k/index.m3u8".to_owned(),
                "/v1/hls/film/720p/index.m3u9".to_owned(),
                "/v1/hls/film/720p/abc.m4s".to_owned(),
                "/v1/hls/film/720p/01.m4s".to_owned(),
                "/v1/hls/film/720p/-1.m4s".to_owned(),
                paths::hls_segment("film", Quality::Low720, 100),
                paths::hls_segment("short", Quality::Low720, 3),
            ] {
                let response = f.get(&path).await?;
                assert_eq!(response.status(), 404, "{path}");
                error_message(response).await?;
            }
            let response = f
                .get(&paths::hls_playlist("unknown-length", Quality::Low720))
                .await?;
            assert_eq!(response.status(), 422);
            assert!(error_message(response).await?.contains("Original"));
            assert!(f.runs().is_empty(), "no request above starts FFmpeg");
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn without_ffmpeg_every_tier_is_503() -> Result<()> {
        within(30, async {
            let root = tempfile::tempdir()?;
            let state = Arc::new(HlsState::new(
                None,
                2,
                root.path().join("hls"),
                resolver(root.path()),
            ));
            assert_eq!(state.qualities(), [Quality::Original]);
            let (base, server) = serve(state).await?;
            let http = client();
            for path in [
                paths::hls_playlist("film", Quality::Low720),
                paths::hls_init("film", Quality::Data480),
                paths::hls_segment("film", Quality::High1080, 0),
            ] {
                let response = http.get(format!("{base}{path}")).send().await?;
                assert_eq!(response.status(), 503, "{path}");
                assert!(
                    error_message(response)
                        .await?
                        .contains("This server has no FFmpeg")
                );
            }
            let missing = http
                .get(format!(
                    "{base}{}",
                    paths::hls_playlist("missing", Quality::Low720)
                ))
                .send()
                .await?;
            assert_eq!(missing.status(), 404);
            server.abort();
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn segments_wait_for_the_running_job() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.2",
                    ..Default::default()
                },
                2,
                limits(),
            )
            .await?;
            let response = f
                .get(&paths::hls_segment("film", Quality::Low720, 0))
                .await?;
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "video/iso.segment");
            assert_eq!(response.text().await?, "segment 0 from 0\n");

            let init = f.get(&paths::hls_init("film", Quality::Low720)).await?;
            assert_eq!(init.status(), 200);
            assert_eq!(init.headers()["content-type"], "video/mp4");
            assert_eq!(init.text().await?, "init from 0\n");

            // Two ahead is within the look-ahead, so it waits for this run.
            assert_eq!(
                f.segment("film", Quality::Low720, 3).await?,
                "segment 3 from 0\n"
            );
            assert_eq!(
                f.segment("film", Quality::Low720, 1).await?,
                "segment 1 from 0\n"
            );
            let runs = f.runs();
            assert_eq!(runs.len(), 1, "{runs:?}");
            assert_eq!((runs[0].0, runs[0].1.as_str()), (0, "0"));
            assert_eq!(f.state.job_count(), 1);
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn seeking_restarts_the_job_at_the_segment() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.1",
                    ..Default::default()
                },
                2,
                limits(),
            )
            .await?;
            let q = Quality::Data480;
            assert_eq!(f.segment("film", q, 0).await?, "segment 0 from 0\n");
            // Far ahead: a new run starts there instead of waiting.
            assert_eq!(f.segment("film", q, 50).await?, "segment 50 from 50\n");
            // Back to a point no run made yet.
            assert_eq!(f.segment("film", q, 20).await?, "segment 20 from 20\n");
            // The first segment is still kept from the first run.
            assert_eq!(f.segment("film", q, 0).await?, "segment 0 from 0\n");
            let runs = f.runs();
            let starts: Vec<_> = runs.iter().map(|r| (r.0, r.1.as_str())).collect();
            assert_eq!(starts, [(0, "0"), (50, "200"), (20, "80")]);
            // Replaced runs are stopped.
            until("replaced runs to exit", || {
                !alive(runs[0].2) && !alive(runs[1].2)
            })
            .await?;
            assert!(alive(runs[2].2));
            f.shutdown().await;
            until("the last run to exit", || !alive(runs[2].2)).await?;
            Ok(())
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jobs_with_waiters_are_never_closed_for_room() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "1",
                    ..Default::default()
                },
                1,
                HlsLimits {
                    // Any job nobody waits on may be closed for room.
                    reclaim_after: Duration::ZERO,
                    ..limits()
                },
            )
            .await?;
            let first_path = paths::hls_segment("film", Quality::Low720, 0);
            let waiting = f.get(&first_path);
            let refused = async {
                until("the first run to start", || f.runs().len() == 1).await?;
                let mut messages = Vec::new();
                for (id, quality) in [("film", Quality::Data480), ("other", Quality::Low720)] {
                    let response = f.get(&paths::hls_segment(id, quality, 0)).await?;
                    assert_eq!(response.status(), 503, "{id} {quality:?}");
                    messages.push(error_message(response).await?);
                }
                Ok::<_, anyhow::Error>(messages)
            };
            let (waiting, refused) = tokio::join!(waiting, refused);
            assert_eq!(waiting?.text().await?, "segment 0 from 0\n");
            for message in refused? {
                assert!(
                    message.contains("already making 1 transcode at once"),
                    "{message}"
                );
            }
            assert_eq!(f.runs().len(), 1, "a refused request starts nothing");
            // Nobody waits on the job now, so it makes room.
            let first = f.runs()[0].2;
            assert_eq!(
                f.segment("other", Quality::Low720, 0).await?,
                "segment 0 from 0\n"
            );
            assert_eq!(f.state.job_count(), 1);
            until("the closed job's FFmpeg to exit", || !alive(first)).await?;
            until("the closed job's folder to go", || {
                f.job_folders().len() == 1
            })
            .await?;
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    /// Switching tiers twice used to leave two abandoned jobs holding both
    /// slots for a minute. The abandoned tier of the same video makes room;
    /// another video's job that was just used does not.
    #[tokio::test]
    async fn switching_tiers_closes_the_abandoned_tier() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script::default(),
                2,
                HlsLimits {
                    reclaim_after: Duration::from_secs(2),
                    ..limits()
                },
            )
            .await?;
            f.segment("film", Quality::Low720, 0).await?;
            let first = f.runs()[0].2;
            f.segment("film", Quality::Data480, 0).await?;
            assert_eq!(
                f.segment("film", Quality::High1080, 0).await?,
                "segment 0 from 0\n"
            );
            assert_eq!(f.state.job_count(), 2);
            until("the 720p FFmpeg to exit", || !alive(first)).await?;
            until("the 720p folder to go", || f.job_folders().len() == 2).await?;
            // Both remaining jobs were just used, and are another video's.
            let response = f
                .get(&paths::hls_segment("other", Quality::Low720, 0))
                .await?;
            assert_eq!(response.status(), 503);
            error_message(response).await?;
            // Once one has gone unused for a while, it makes room.
            tokio::time::sleep(Duration::from_millis(2500)).await;
            assert_eq!(
                f.segment("other", Quality::Low720, 0).await?,
                "segment 0 from 0\n"
            );
            assert_eq!(f.state.job_count(), 2);
            // The 1080p job was used last, so 480p was the one closed.
            assert_eq!(
                f.segment("film", Quality::High1080, 1).await?,
                "segment 1 from 0\n"
            );
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    /// Two requests far apart in one (video, tier) job used to replace each
    /// other's FFmpeg run thousands of times a second. Now the request whose
    /// run was replaced answers 409 at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn far_apart_requests_do_not_restart_each_other() -> Result<()> {
        within(30, async {
            let q = Quality::Low720;
            // FFmpeg that makes segments slowly: the later request wins.
            let f = fixture(
                Script {
                    delay: "1",
                    ..Default::default()
                },
                2,
                limits(),
            )
            .await?;
            let (near, far) = (
                paths::hls_segment("film", q, 0),
                paths::hls_segment("film", q, 50),
            );
            let started = std::time::Instant::now();
            let (a, b) = tokio::join!(f.get(&near), f.get(&far));
            let (a, b) = (a?, b?);
            let elapsed = started.elapsed();
            let runs = f.runs();
            assert!(runs.len() <= 3, "{} runs: {runs:?}", runs.len());
            let mut statuses = [a.status().as_u16(), b.status().as_u16()];
            statuses.sort_unstable();
            assert_eq!(statuses, [200, 409]);
            let (served, conflict) = if a.status() == 409 { (b, a) } else { (a, b) };
            assert!(served.text().await?.starts_with("segment "));
            let message = error_message(conflict).await?;
            assert!(message.contains("Another viewer"), "{message}");
            assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
            f.shutdown().await;

            // FFmpeg that never makes a segment in time, as in the report.
            let f = fixture(
                Script {
                    delay: "30",
                    ..Default::default()
                },
                2,
                HlsLimits {
                    segment_wait: Duration::from_secs(2),
                    ..limits()
                },
            )
            .await?;
            let started = std::time::Instant::now();
            let (a, b) = tokio::join!(
                async {
                    let response = f.get(&paths::hls_segment("film", q, 0)).await?;
                    Ok::<_, anyhow::Error>((response.status().as_u16(), started.elapsed()))
                },
                async {
                    let response = f.get(&paths::hls_segment("film", q, 50)).await?;
                    Ok::<_, anyhow::Error>((response.status().as_u16(), started.elapsed()))
                }
            );
            let mut answers = [a?, b?];
            answers.sort_unstable_by_key(|(status, _)| *status);
            let runs = f.runs();
            assert!(runs.len() <= 3, "{} runs: {runs:?}", runs.len());
            assert_eq!(answers[0].0, 409);
            assert!(answers[0].1 < Duration::from_secs(1), "{answers:?}");
            assert_eq!(answers[1].0, 504);
            assert!(answers[1].1 < Duration::from_secs(4), "{answers:?}");
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    /// `init.mp4` is the same from any run, so it waits for whichever run
    /// is going rather than starting its own again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn init_waits_for_a_run_started_elsewhere() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.3",
                    ..Default::default()
                },
                2,
                limits(),
            )
            .await?;
            let q = Quality::Data480;
            let (init_path, far) = (
                paths::hls_init("film", q),
                paths::hls_segment("film", q, 50),
            );
            let (init, segment) = tokio::join!(f.get(&init_path), f.get(&far));
            let (init, segment) = (init?, segment?);
            assert_eq!(init.status(), 200);
            assert!(init.text().await?.starts_with("init from "));
            assert_eq!(segment.text().await?, "segment 50 from 50\n");
            let runs = f.runs();
            assert!(runs.len() <= 2, "{runs:?}");
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn downloads_in_progress_answer_conflict() -> Result<()> {
        within(30, async {
            let f = fixture(Script::default(), 2, limits()).await?;
            for path in [
                paths::hls_playlist("partial", Quality::Low720),
                paths::hls_init("partial", Quality::Low720),
                paths::hls_segment("partial", Quality::Low720, 0),
            ] {
                let response = f.get(&path).await?;
                assert_eq!(response.status(), 409, "{path}");
                assert_eq!(
                    error_message(response).await?,
                    flick::remote::hls::INCOMPLETE_MESSAGE
                );
            }
            assert!(f.runs().is_empty());
            assert_eq!(f.state.job_count(), 0);
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn idle_jobs_are_stopped_and_removed() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.2",
                    ..Default::default()
                },
                2,
                HlsLimits {
                    idle: Duration::from_millis(400),
                    ..limits()
                },
            )
            .await?;
            f.segment("film", Quality::Low720, 0).await?;
            assert_eq!(f.job_folders().len(), 1);
            let pid = f.runs()[0].2;
            assert!(alive(pid));
            until("the idle job to stop", || f.state.job_count() == 0).await?;
            until("FFmpeg to exit", || !alive(pid)).await?;
            until("the job folder to go", || f.job_folders().is_empty()).await?;
            // A new request starts again from nothing.
            assert_eq!(
                f.segment("film", Quality::Low720, 0).await?,
                "segment 0 from 0\n"
            );
            assert_eq!(f.runs().len(), 2);
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn a_client_that_leaves_mid_wait_does_not_wedge_the_job() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        within(30, async {
            let f = fixture(
                Script {
                    delay: "1.5",
                    ..Default::default()
                },
                2,
                HlsLimits {
                    idle: Duration::from_millis(500),
                    ..limits()
                },
            )
            .await?;
            let host = f.base.trim_start_matches("http://").to_owned();
            let mut stream = tokio::net::TcpStream::connect(&host).await?;
            let request = format!(
                "GET {} HTTP/1.1\r\nHost: {host}\r\n\r\n",
                paths::hls_segment("film", Quality::Low720, 0)
            );
            stream.write_all(request.as_bytes()).await?;
            until("the job to start", || f.runs().len() == 1).await?;
            drop(stream);

            // Another client still gets the segment from the same run.
            assert_eq!(
                f.segment("film", Quality::Low720, 0).await?,
                "segment 0 from 0\n"
            );
            assert_eq!(f.runs().len(), 1);
            // And the job still goes idle and is cleaned up afterwards.
            let pid = f.runs()[0].2;
            until("the idle job to stop", || f.state.job_count() == 0).await?;
            until("FFmpeg to exit", || !alive(pid)).await?;
            until("the job folder to go", || f.job_folders().is_empty()).await?;
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn ffmpeg_failures_and_early_ends_answer_quickly() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    total: 2,
                    fail_at: None,
                    delay: "0.02",
                },
                2,
                limits(),
            )
            .await?;
            // The playlist promises 3 segments but the source ends after 2.
            f.segment("short", Quality::Low720, 1).await?;
            let started = std::time::Instant::now();
            let response = f
                .get(&paths::hls_segment("short", Quality::Low720, 2))
                .await?;
            assert_eq!(response.status(), 404);
            assert!(started.elapsed() < Duration::from_secs(5));
            f.shutdown().await;

            let f = fixture(
                Script {
                    fail_at: Some(2),
                    ..Default::default()
                },
                2,
                limits(),
            )
            .await?;
            f.segment("film", Quality::Low720, 0).await?;
            let response = f
                .get(&paths::hls_segment("film", Quality::Low720, 2))
                .await?;
            assert_eq!(response.status(), 502);
            let message = error_message(response).await?;
            assert!(message.contains("stand-in failure"), "{message}");
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn runs_pause_far_ahead_and_resume_near_the_end() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.02",
                    ..Default::default()
                },
                2,
                HlsLimits {
                    max_ahead: 6,
                    resume_ahead: 3,
                    ..limits()
                },
            )
            .await?;
            let q = Quality::Standard1080;
            f.segment("film", q, 0).await?;
            let first = f.runs()[0].2;
            // The run stops once it is 6 ahead of the playhead.
            until("the run to pause", || !alive(first)).await?;
            assert_eq!(f.runs().len(), 1);
            assert_eq!(f.segment("film", q, 2).await?, "segment 2 from 0\n");
            assert_eq!(f.runs().len(), 1, "still far from the end of what was made");
            // Near the end of what was made: the next run starts after it.
            assert_eq!(f.segment("film", q, 4).await?, "segment 4 from 0\n");
            until("the next run", || f.runs().len() == 2).await?;
            assert_eq!(f.runs()[1].0, 7);
            assert_eq!(f.segment("film", q, 9).await?, "segment 9 from 7\n");
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn segments_behind_the_playhead_are_evicted_over_the_cap() -> Result<()> {
        within(30, async {
            let f = fixture(
                Script {
                    delay: "0.02",
                    ..Default::default()
                },
                2,
                HlsLimits {
                    // Each stand-in segment is 18 to 20 bytes.
                    max_job_bytes: 100,
                    max_ahead: 3,
                    resume_ahead: 3,
                    ..limits()
                },
            )
            .await?;
            let q = Quality::Low720;
            for n in 0..8 {
                let body = f.segment("film", q, n).await?;
                assert!(body.starts_with(&format!("segment {n} from ")), "{body}");
            }
            let folder = f.job_folders().pop().unwrap();
            until("old segments to go", || !folder.join("0.m4s").exists()).await?;
            assert!(folder.join("7.m4s").exists());
            let kept = std::fs::read_dir(&folder)?
                .filter(|e| {
                    e.as_ref()
                        .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".m4s"))
                })
                .count();
            assert!(kept <= 6, "{kept} segments kept");
            // An evicted segment is made again by a new run.
            assert_eq!(f.segment("film", q, 0).await?, "segment 0 from 0\n");
            assert_eq!(f.runs().last().unwrap().0, 0);
            assert!(f.runs().len() >= 2);
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn shutdown_stops_ffmpeg_and_removes_folders() -> Result<()> {
        within(30, async {
            let f = fixture(Script::default(), 2, limits()).await?;
            f.segment("film", Quality::Low720, 0).await?;
            f.segment("other", Quality::Data480, 0).await?;
            let pids: Vec<i32> = f.runs().iter().map(|r| r.2).collect();
            assert_eq!(pids.len(), 2);
            f.state.shutdown().await;
            assert!(pids.iter().all(|&pid| !alive(pid)));
            assert!(f.job_folders().is_empty());
            f.shutdown().await;
            Ok(())
        })
        .await
    }

    #[tokio::test]
    async fn dropping_the_state_stops_ffmpeg() -> Result<()> {
        within(30, async {
            let f = fixture(Script::default(), 2, limits()).await?;
            f.segment("film", Quality::Low720, 0).await?;
            let pid = f.runs()[0].2;
            let Fixture {
                root: _root,
                temp,
                state,
                server,
                http,
                ..
            } = f;
            // Closing the client's connections ends the server's connection
            // tasks, which hold the router and so the state.
            drop(http);
            server.abort();
            let _ = server.await;
            drop(state);
            until("FFmpeg to exit", || !alive(pid)).await?;
            until("the folders to go", || {
                std::fs::read_dir(&temp).map(|d| d.count()).unwrap_or(0) == 0
            })
            .await?;
            Ok(())
        })
        .await
    }
}

/// Real FFmpeg on a generated clip: init and segments are produced, the
/// playlist parses, segments from a seek-started job carry absolute times,
/// and embedded text subtitles convert to WebVTT.
#[tokio::test]
#[ignore = "requires FFmpeg and FLICK_TEST_VIDEO (scripts/make-test-video.sh)"]
async fn real_ffmpeg_makes_playable_segments() -> Result<()> {
    within(180, async {
        let video = std::path::PathBuf::from(
            std::env::var_os("FLICK_TEST_VIDEO").context("set FLICK_TEST_VIDEO")?,
        );
        let encoder = transcode::probe(None)
            .await
            .context("FFmpeg was not found")?;
        println!(
            "encoder: {} ({})",
            encoder.video_encoder,
            encoder.ffmpeg.display()
        );
        let info = transcode::inspect(&encoder, &video).await?;
        println!("source: {info:?}");
        ensure!(
            info.duration > 8.0,
            "the clip should be longer than two segments"
        );
        let root = tempfile::tempdir()?;
        let resolved = ResolvedVideo {
            path: video.clone(),
            info: info.clone(),
            complete: true,
        };
        let state = Arc::new(HlsState::new(
            Some(encoder.clone()),
            2,
            root.path().join("hls"),
            Arc::new(move |id: &str| (id == "clip").then(|| resolved.clone())),
        ));
        let (base, server) = serve(state.clone()).await?;
        let http = client();
        let get = |path: String| {
            let http = http.clone();
            let base = base.clone();
            async move {
                let response = http.get(format!("{base}{path}")).send().await?;
                ensure!(response.status() == 200, "{path}: {}", response.status());
                Ok::<_, anyhow::Error>(response.bytes().await?.to_vec())
            }
        };

        let q = Quality::Data480;
        let text = String::from_utf8(get(paths::hls_playlist("clip", q)).await?)?;
        let durations: Vec<f64> = text
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .map(|l| l.trim_end_matches(',').parse().unwrap())
            .collect();
        assert_eq!(durations.len() as u32, segment_count(info.duration));
        assert!((durations.iter().sum::<f64>() - info.duration).abs() < 0.01);
        assert!(text.starts_with("#EXTM3U\n") && text.ends_with("#EXT-X-ENDLIST\n"));

        let started = std::time::Instant::now();
        let first = get(paths::hls_segment("clip", q, 0)).await?;
        println!(
            "first 480p segment: {} bytes after {:?}",
            first.len(),
            started.elapsed()
        );
        let init = get(paths::hls_init("clip", q)).await?;
        assert_eq!(&init[4..8], b"ftyp", "init.mp4 is an MP4 header");
        assert!(
            first.windows(4).any(|w| w == b"moof"),
            "segment has a fragment"
        );

        let probe = |segment: &[u8], name: &str| -> Result<String> {
            let path = root.path().join(name);
            std::fs::write(&path, [init.as_slice(), segment].concat())?;
            let output = std::process::Command::new(&encoder.ffprobe)
                .args(["-v", "error", "-show_entries"])
                .arg("stream=codec_name,width,height,start_time,color_transfer")
                .args(["-of", "compact"])
                .arg(&path)
                .output()?;
            ensure!(output.status.success(), "ffprobe failed on {name}");
            Ok(String::from_utf8(output.stdout)?)
        };
        let streams = probe(&first, "first.mp4")?;
        println!("init + 0.m4s: {}", streams.trim());
        assert!(
            streams.contains("codec_name=h264|width=854|height=480"),
            "{streams}"
        );
        assert!(streams.contains("codec_name=aac"), "{streams}");

        // A seek: the job is gone and segment 2 is asked for first, so a new
        // FFmpeg run starts there. The client still has the first run's
        // init, and the segment must decode at 8 s with it.
        state.shutdown().await;
        let seeked = get(paths::hls_segment("clip", q, 2)).await?;
        let streams = probe(&seeked, "seeked.mp4")?;
        println!("first init + seek-started 2.m4s: {}", streams.trim());
        assert!(
            streams.contains("codec_name=h264|width=854|height=480"),
            "{streams}"
        );
        let start: f64 = streams
            .lines()
            .find(|l| l.contains("codec_name=h264"))
            .and_then(|l| l.split('|').find_map(|f| f.strip_prefix("start_time=")))
            .context("no video start time")?
            .parse()?;
        assert!((start - 8.0).abs() < 0.1, "segment 2 starts at {start}");

        // FFmpeg's own HLS demuxer, which libmpv uses, reads the whole
        // playlist over loopback, then again from a seek point.
        let url = format!("{base}{}", paths::hls_playlist("clip", q));
        for seek in ["0", "9"] {
            let started = std::time::Instant::now();
            let output = tokio::process::Command::new(&encoder.ffmpeg)
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-ss",
                    seek,
                    "-i",
                ])
                .arg(&url)
                .args(["-f", "null", "-"])
                .output()
                .await?;
            ensure!(
                output.status.success(),
                "FFmpeg could not play the playlist from {seek} s: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            println!(
                "FFmpeg's HLS demuxer decoded the playlist from {seek} s in {:?}",
                started.elapsed()
            );
        }
        state.shutdown().await;
        server.abort();

        // Embedded text subtitles, muxed from the clip's sidecar if present.
        let srt = video.with_extension("srt");
        if srt.exists() {
            let mkv = root.path().join("with-subtitles.mkv");
            let status = std::process::Command::new(&encoder.ffmpeg)
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(&video)
                .arg("-i")
                .arg(&srt)
                .args(["-map", "0", "-map", "1", "-c", "copy", "-c:s", "srt"])
                .args(["-metadata:s:s:0", "language=eng"])
                .arg(&mkv)
                .status()?;
            ensure!(status.success(), "could not mux the subtitle fixture");
            let info = transcode::inspect(&encoder, &mkv).await?;
            assert_eq!(info.subtitles.len(), 1);
            assert!(info.subtitles[0].text);
            assert_eq!(info.subtitles[0].language.as_deref(), Some("eng"));
            let vtt = String::from_utf8(transcode::extract_webvtt(&encoder, &mkv, 0).await?)?;
            assert!(vtt.starts_with("WEBVTT"), "{vtt}");
            assert!(vtt.contains("external subtitles are working"), "{vtt}");
            println!("embedded subtitle: {} bytes of WebVTT", vtt.len());
        } else {
            println!("no sidecar .srt next to the clip; skipped the subtitle check");
        }

        hdr_tone_maps(&encoder, root.path()).await
    })
    .await
}

/// Generates a short PQ clip with x265 (skipped when FFmpeg lacks it) and
/// checks the transcode comes out tagged SDR BT.709.
async fn hdr_tone_maps(encoder: &Encoder, dir: &Path) -> Result<()> {
    let hdr = dir.join("hdr.mp4");
    let status = std::process::Command::new(&encoder.ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30:duration=4",
        ])
        .args(["-pix_fmt", "yuv420p10le", "-c:v", "libx265"])
        .args([
            "-x265-params",
            "log-level=error:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc",
        ])
        .args([
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
        ])
        .arg(&hdr)
        .status()?;
    if !status.success() {
        println!("FFmpeg cannot encode x265 here; skipped the HDR check");
        return Ok(());
    }
    let info = transcode::inspect(encoder, &hdr).await?;
    assert!(info.hdr, "{info:?}");
    let out = dir.join("hdr-out");
    std::fs::create_dir_all(&out)?;
    let args = transcode::job_args(encoder, &hdr, &info, Quality::Low720, 0, &out);
    let status = std::process::Command::new(&encoder.ffmpeg)
        .args(&args)
        .status()?;
    ensure!(status.success(), "the HDR transcode failed");
    let joined = dir.join("hdr-joined.mp4");
    std::fs::write(
        &joined,
        [
            std::fs::read(out.join("init.mp4"))?,
            std::fs::read(out.join("0.m4s"))?,
        ]
        .concat(),
    )?;
    let output = std::process::Command::new(&encoder.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=pix_fmt,color_transfer,color_primaries")
        .args(["-of", "compact"])
        .arg(&joined)
        .output()?;
    let streams = String::from_utf8(output.stdout)?;
    println!("HDR source transcoded: {}", streams.trim());
    assert!(streams.contains("pix_fmt=yuv420p"), "{streams}");
    assert!(streams.contains("color_transfer=bt709"), "{streams}");
    Ok(())
}
