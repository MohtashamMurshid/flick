use anyhow::{Context, Result, ensure};
use flick::torrent::{TorrentInput, TransferConfig, TransferSession, TransferState};
use sha1::{Digest, Sha1};
use std::{num::NonZeroU32, path::Path, time::Duration};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

fn config(root: &Path, name: &str) -> TransferConfig {
    TransferConfig::local(
        root.join(format!("{name}-downloads")),
        root.join(format!("{name}-state")),
    )
}

fn content(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 9)) % 251) as u8)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sharing_exact_piece_boundary_has_correct_hashes_and_magnet() -> Result<()> {
    timeout(Duration::from_secs(30), async {
        let root = TempDir::new()?;
        let source = root.path().join("boundary file.bin");
        let bytes = content(2 * 1024 * 1024);
        tokio::fs::write(&source, &bytes).await?;
        let session = TransferSession::start(config(root.path(), "seed")).await?;
        let result: Result<()> = async {
            let shared = session.share(&source, vec![]).await?;
            let parsed = librqbit::torrent_from_bytes(shared.torrent_bytes())?;
            assert_eq!(parsed.info.data.piece_length, 2 * 1024 * 1024);
            assert_eq!(parsed.info.data.pieces.as_ref().len(), 20);
            assert_eq!(
                parsed.info.data.pieces.as_ref(),
                Sha1::digest(&bytes).as_slice()
            );
            let expected_id = Sha1::digest(parsed.info.raw_bytes.as_ref())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            assert_eq!(shared.infohash, expected_id);
            assert_eq!(
                librqbit::Magnet::parse(&shared.magnet)?
                    .as_id20()
                    .unwrap()
                    .as_string(),
                expected_id
            );
            parsed.info.data.validate()?;
            assert_eq!(
                session.snapshot(&shared.infohash)?.state,
                TransferState::Seeding
            );
            assert_eq!(tokio::fs::read(&source).await?, bytes);
            assert!(!source.with_extension("bin.torrent").exists());
            Ok(())
        }
        .await;
        session.stop().await;
        result
    })
    .await
    .context("Boundary test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_clients_share_select_pause_restart_resume_and_verify() -> Result<()> {
    timeout(Duration::from_secs(60), async {
        let root = TempDir::new()?;
        let source = root.path().join("Shared videos ü");
        tokio::fs::create_dir(&source).await?;
        let bytes = content(8 * 1024 * 1024 + 137);
        tokio::fs::write(source.join("clip.bin"), &bytes).await?;
        tokio::fs::write(
            source.join("notes.txt"),
            b"Another file that will not be selected.",
        )
        .await?;
        let seed = TransferSession::start(config(root.path(), "seed")).await?;
        let result: Result<()> = async {
            let shared = seed.share(&source, vec![]).await?;
            let peer = seed.listen_addr().context("Missing loopback listener")?;
            assert!(peer.ip().is_loopback());
            let mut receiver_config = config(root.path(), "receiver");
            receiver_config.download_limit_bytes_per_second = NonZeroU32::new(4 * 1024 * 1024);
            let receiver = TransferSession::start(receiver_config.clone()).await?;
            let first: Result<(String, usize, u64)> = async {
                // Resolve the magnet over BitTorrent's real metadata exchange.
                let resolved = receiver
                    .resolve(
                        TorrentInput::Magnet(shared.magnet.clone()),
                        vec![peer],
                        Duration::from_secs(10),
                    )
                    .await?;
                assert_eq!(resolved.files.len(), 2);
                let index = resolved
                    .files
                    .iter()
                    .find(|f| f.relative_path == Path::new("clip.bin"))
                    .context("Missing clip metadata")?
                    .index;
                assert!(receiver.download(&resolved, Some(vec![])).await.is_err());
                assert!(receiver.download(&resolved, Some(vec![99])).await.is_err());
                let id = receiver
                    .download_to(
                        &resolved,
                        Some(vec![index]),
                        &root.path().join("chosen-folder"),
                    )
                    .await?;
                assert!(receiver.completed_file(&id, index).is_err());
                // Wait for actual verified pieces, not just an allocated file.
                let progress = timeout(Duration::from_secs(12), async {
                    loop {
                        let snapshot = receiver.snapshot(&id)?;
                        if snapshot.verified_bytes > 0 {
                            ensure!(
                                snapshot.verified_bytes < bytes.len() as u64,
                                "Fixture finished before restart checkpoint"
                            );
                            break Ok::<_, anyhow::Error>(snapshot.verified_bytes);
                        }
                        if let Some(error) = snapshot.error {
                            anyhow::bail!(error);
                        }
                        sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("No partial verified progress")??;
                receiver.pause(&id).await?;
                receiver.pause(&id).await?; // Setting paused is idempotent.
                assert_eq!(receiver.snapshot(&id)?.state, TransferState::Paused);
                assert!(
                    receiver
                        .wait_for_completion(&id, Duration::from_millis(50))
                        .await
                        .is_err()
                );
                Ok((id, index, progress))
            }
            .await;
            receiver.stop().await;
            let (id, index, prior_progress) = first?;
            receiver_config.download_limit_bytes_per_second = None;
            let receiver = TransferSession::start(receiver_config).await?;
            let second: Result<()> = async {
                let checked = timeout(Duration::from_secs(10), async {
                    loop {
                        let snapshot = receiver.snapshot(&id)?;
                        if snapshot.state != TransferState::Checking {
                            break Ok::<_, anyhow::Error>(snapshot);
                        }
                        sleep(Duration::from_millis(10)).await;
                    }
                })
                .await??;
                assert_eq!(checked.state, TransferState::Paused);
                assert!(checked.verified_bytes >= prior_progress);
                assert!(receiver.completed_file(&id, index).is_err());
                receiver.resume(&id, vec![peer]).await?;
                receiver
                    .wait_for_completion(&id, Duration::from_secs(15))
                    .await?;
                let completed = receiver.completed_file(&id, index)?;
                assert!(completed.is_absolute());
                assert!(completed.starts_with(root.path().join("chosen-folder").canonicalize()?));
                assert_eq!(tokio::fs::read(&completed).await?, bytes);
                let snapshot = receiver.snapshot(&id)?;
                assert_eq!(snapshot.state, TransferState::Seeding);
                assert_eq!(snapshot.files[index].verified_bytes, bytes.len() as u64);
                let other = snapshot.files.iter().find(|f| f.index != index).unwrap();
                assert!(!other.selected);
                assert!(receiver.completed_file(&id, other.index).is_err());
                receiver.remove(&id, false).await?;
                assert!(completed.is_file());
                assert!(receiver.snapshot(&id).is_err());
                Ok(())
            }
            .await;
            receiver.stop().await;
            second?;
            assert_eq!(tokio::fs::read(source.join("clip.bin")).await?, bytes);
            Ok(())
        }
        .await;
        seed.stop().await;
        result
    })
    .await
    .context("Local transfer regression timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_lock_invalid_inputs_and_existing_destinations_are_safe() -> Result<()> {
    timeout(Duration::from_secs(30), async {
        let root = TempDir::new()?;
        let cfg = config(root.path(), "client");
        let session = TransferSession::start(cfg.clone()).await?;
        // Local startup must never create public discovery/port mapping state.
        assert!(!cfg.state_dir.join("dht.json").exists());
        assert!(!cfg.state_dir.join("listen-port").exists());
        let result: Result<()> = async {
            assert!(TransferSession::start(cfg.clone()).await.is_err());
            assert!(
                session
                    .resolve(
                        TorrentInput::Bytes(b"invalid torrent".to_vec()),
                        vec![],
                        Duration::from_secs(1)
                    )
                    .await
                    .is_err()
            );
            // The engine and adapter must reject traversal before allocating
            // download files, including names the OS normalizes to a directory.
            for name in ["../escape.bin", "/escape.bin", ".", "folder\\escape.bin"] {
                #[derive(serde::Serialize)]
                struct Envelope {
                    info: librqbit::TorrentMetaV1Info<librqbit::ByteBufOwned>,
                }
                let info = librqbit::TorrentMetaV1Info {
                    name: Some(name.as_bytes().into()),
                    length: Some(1),
                    piece_length: 16384,
                    pieces: Sha1::digest([42]).to_vec().into(),
                    ..Default::default()
                };
                let mut bytes = Vec::new();
                librqbit_bencode::bencode_serialize_to_writer(&Envelope { info }, &mut bytes)?;
                assert!(
                    session
                        .resolve(TorrentInput::Bytes(bytes), vec![], Duration::from_secs(1))
                        .await
                        .is_err(),
                    "Accepted unsafe name: {name}"
                );
            }
            assert!(
                session
                    .resolve(
                        TorrentInput::File(root.path().join("missing.torrent")),
                        vec![],
                        Duration::from_secs(1)
                    )
                    .await
                    .is_err()
            );
            assert!(
                session
                    .resolve(
                        TorrentInput::Magnet("not a magnet".into()),
                        vec![],
                        Duration::from_secs(1)
                    )
                    .await
                    .is_err()
            );
            let empty = root.path().join("empty.bin");
            tokio::fs::write(&empty, []).await?;
            assert!(session.share(&empty, vec![]).await.is_err());
            let seed = TransferSession::start(config(root.path(), "seed")).await?;
            let transfer: Result<()> = async {
                let source = root.path().join("sample.bin");
                tokio::fs::write(&source, content(65_537)).await?;
                let shared = seed.share(&source, vec![]).await?;
                let resolved = session
                    .resolve(
                        TorrentInput::Bytes(shared.torrent_bytes().to_vec()),
                        vec![],
                        Duration::from_secs(2),
                    )
                    .await?;
                let destination = cfg.download_dir.join(&resolved.infohash);
                tokio::fs::create_dir(&destination).await?;
                let existing = destination.join("sample.bin");
                tokio::fs::write(&existing, b"Unrelated existing data").await?;
                assert!(session.download(&resolved, None).await.is_err());
                assert_eq!(tokio::fs::read(existing).await?, b"Unrelated existing data");
                Ok(())
            }
            .await;
            seed.stop().await;
            transfer
        }
        .await;
        session.stop().await;
        result?;
        let reopened = TransferSession::start(cfg).await?;
        reopened.stop().await;
        Ok(())
    })
    .await
    .context("Invalid-input test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_timeout_explains_recovery_without_starting_a_download() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let root = TempDir::new()?;
        let source = root.path().join("fixture.bin");
        tokio::fs::write(&source, content(65_537)).await?;
        let seed = TransferSession::start(config(root.path(), "seed")).await?;
        let shared = seed.share(&source, vec![]).await?;
        seed.stop().await;
        let cfg = config(root.path(), "receiver");
        let session = TransferSession::start(cfg.clone()).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let peer = listener.local_addr()?;
        let stalled = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await?;
            std::future::pending::<()>().await;
            Ok::<_, std::io::Error>(())
        });
        let result: Result<()> = async {
            let error = session
                .resolve(
                    TorrentInput::Magnet(shared.magnet),
                    vec![peer],
                    Duration::from_millis(200),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("active seeders"), "{error}");
            assert!(error.contains(".torrent file"), "{error}");
            assert!(session.snapshots()?.is_empty());
            assert!(std::fs::read_dir(&cfg.download_dir)?.next().is_none());
            Ok(())
        }
        .await;
        stalled.abort();
        session.stop().await;
        result
    })
    .await
    .context("Metadata timeout regression timed out")?
}

/// A file can be played while it downloads: reads return only verified
/// bytes in order, a seek near the end fetches those pieces early, and
/// cancelling ends a read that is waiting for pieces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_read_verified_bytes_while_downloading() -> Result<()> {
    timeout(Duration::from_secs(120), async {
        let root = TempDir::new()?;
        let source = root.path().join("movie.mp4");
        let bytes = std::sync::Arc::new(content(12 * 1024 * 1024 + 4099));
        tokio::fs::write(&source, bytes.as_slice()).await?;
        let seed = TransferSession::start(config(root.path(), "seed")).await?;
        let shared = seed.share(&source, vec![]).await?;
        let peer = seed.listen_addr().context("Missing loopback listener")?;
        let mut receiver_config = config(root.path(), "receiver");
        // Slow enough that every check below happens before completion.
        receiver_config.download_limit_bytes_per_second = NonZeroU32::new(512 * 1024);
        let receiver = TransferSession::start(receiver_config).await?;
        let resolved = receiver
            .resolve(
                TorrentInput::Magnet(shared.magnet),
                vec![peer],
                Duration::from_secs(10),
            )
            .await?;
        let id = receiver.download(&resolved, Some(vec![0])).await?;
        receiver.resume(&id, vec![peer]).await?;
        let stream = receiver.open_stream(&id, 0).await?;
        assert_eq!(stream.len(), bytes.len() as u64);
        let mut stream = stream.into_blocking(&tokio::runtime::Handle::current());
        let total = bytes.len() as u64;
        let incomplete = |stage: &str| -> Result<()> {
            let verified = receiver.snapshot(&id)?.verified_bytes;
            ensure!(verified < total, "The download finished before {stage}");
            Ok(())
        };

        let expected = bytes.clone();
        stream = tokio::task::spawn_blocking(move || {
            let mut start = vec![0; 200_000];
            let mut filled = 0;
            while filled < start.len() {
                let read = stream.read(&mut start[filled..])?;
                ensure!(read > 0, "Early end of stream");
                filled += read;
            }
            ensure!(
                start == expected[..start.len()],
                "Stream start differs from source"
            );
            Ok::<_, anyhow::Error>(stream)
        })
        .await??;
        incomplete("the start was read")?;

        let expected = bytes.clone();
        let tail = total - 50_000;
        stream = tokio::task::spawn_blocking(move || {
            ensure!(stream.seek(tail)? == tail, "Seek landed elsewhere");
            let mut end = Vec::new();
            let mut buffer = vec![0; 16 * 1024];
            loop {
                let read = stream.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                end.extend_from_slice(&buffer[..read]);
            }
            ensure!(
                end == expected[tail as usize..],
                "Stream end differs from source"
            );
            Ok::<_, anyhow::Error>(stream)
        })
        .await??;
        // The end arrived ahead of the middle, so the seek was prioritized.
        incomplete("the end was read")?;

        // A read waiting on a paused download ends when cancelled.
        receiver.pause(&id).await?;
        let cancel = stream.canceller();
        let middle = total / 2 + 777;
        let waiting = tokio::task::spawn_blocking(move || {
            stream.seek(middle)?;
            let mut buffer = vec![0; 64 * 1024];
            // Read through verified data until a missing piece blocks.
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => return Ok::<_, anyhow::Error>(false),
                    Ok(_) => continue,
                    Err(_) => return Ok(stream.read(&mut buffer).is_err()),
                }
            }
        });
        sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        let failed_after_cancel = timeout(Duration::from_secs(5), waiting)
            .await
            .context("Cancelling did not end the waiting read")???;
        ensure!(failed_after_cancel, "Reads continued after cancelling");
        receiver.stop().await;
        seed.stop().await;
        Ok(())
    })
    .await
    .context("Streaming test timed out")?
}
