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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_limits_completion_policy_and_delete_owned_files() -> Result<()> {
    timeout(Duration::from_secs(45), async {
        let root = TempDir::new()?;
        let source = root.path().join("fixture");
        tokio::fs::create_dir_all(source.join("nested")).await?;
        tokio::fs::write(source.join("clip.bin"), content(262_145)).await?;
        tokio::fs::write(source.join("nested/notes.txt"), b"torrent-owned notes").await?;
        let seed = TransferSession::start(config(root.path(), "seed")).await?;
        let mut cfg = config(root.path(), "receiver");
        cfg.download_limit_bytes_per_second = NonZeroU32::new(2 * 1024 * 1024);
        cfg.upload_limit_bytes_per_second = NonZeroU32::new(1024 * 1024);
        cfg.seed_completed_downloads = false;
        let receiver = TransferSession::start(cfg.clone()).await?;
        let result: Result<()> = async {
            assert_eq!(
                receiver.limits().download_bps,
                cfg.download_limit_bytes_per_second
            );
            assert_eq!(
                receiver.limits().upload_bps,
                cfg.upload_limit_bytes_per_second
            );
            receiver.set_limits(NonZeroU32::new(4 * 1024 * 1024), None);
            assert_eq!(
                receiver.limits().download_bps.unwrap().get(),
                4 * 1024 * 1024
            );
            assert_eq!(receiver.limits().upload_bps, None);
            receiver.set_limits(None, NonZeroU32::new(524_288));
            assert_eq!(receiver.limits().download_bps, None);
            assert_eq!(receiver.limits().upload_bps.unwrap().get(), 524_288);
            let shared = seed.share(&source, vec![]).await?;
            let exported_seed = root.path().join("seed.torrent");
            seed.export_torrent(&shared.infohash, &exported_seed)
                .await?;
            assert_eq!(
                tokio::fs::read(&exported_seed).await?,
                shared.torrent_bytes()
            );
            let resolved = receiver
                .resolve(
                    TorrentInput::File(exported_seed),
                    vec![seed.listen_addr().unwrap()],
                    Duration::from_secs(3),
                )
                .await?;
            let id = receiver.download(&resolved, None).await?;
            receiver
                .wait_for_completion(&id, Duration::from_secs(15))
                .await?;
            timeout(Duration::from_secs(3), async {
                while receiver.snapshot(&id)?.state != TransferState::Paused {
                    sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            assert_eq!(
                seed.snapshot(&id)?.state,
                TransferState::Seeding,
                "explicit sharing ignores download preference"
            );
            assert!(
                seed.snapshot(&id)?.uploaded_bytes > 0,
                "local seeder reports uploaded payload"
            );
            let exported = root.path().join("downloaded.torrent");
            receiver.export_torrent(&id, &exported).await?;
            assert_eq!(tokio::fs::read(&exported).await?, shared.torrent_bytes());
            let again = receiver
                .resolve(TorrentInput::File(exported), vec![], Duration::from_secs(3))
                .await?;
            assert_eq!(again.infohash, resolved.infohash);
            assert_eq!(
                again
                    .files
                    .iter()
                    .map(|f| (&f.relative_path, f.length))
                    .collect::<Vec<_>>(),
                resolved
                    .files
                    .iter()
                    .map(|f| (&f.relative_path, f.length))
                    .collect::<Vec<_>>()
            );
            let snapshot = receiver.snapshot(&id)?;
            for file in &snapshot.files {
                assert_eq!(
                    tokio::fs::read(receiver.completed_file(&id, file.index)?).await?,
                    tokio::fs::read(source.join(&resolved.files[file.index].relative_path)).await?
                );
            }
            let own_file = snapshot.files[0].path.clone();
            assert!(receiver.export_torrent(&id, &own_file).await.is_err());
            let keep = own_file.parent().unwrap().join("unrelated.keep");
            tokio::fs::write(&keep, b"keep this").await?;
            let sibling = cfg.download_dir.join("outside.keep");
            tokio::fs::write(&sibling, b"outside torrent").await?;
            receiver.resume_all(vec![]).await?;
            assert_eq!(receiver.snapshot(&id)?.state, TransferState::Paused);
            receiver.set_seed_completed_downloads(true).await;
            receiver.resume_all(vec![]).await?;
            assert_eq!(receiver.snapshot(&id)?.state, TransferState::Seeding);
            receiver.set_seed_completed_downloads(false).await;
            assert_eq!(receiver.snapshot(&id)?.state, TransferState::Paused);
            receiver.remove(&id, true).await?;
            for file in snapshot.files {
                assert!(!file.path.exists());
            }
            assert_eq!(tokio::fs::read(keep).await?, b"keep this");
            assert_eq!(tokio::fs::read(sibling).await?, b"outside torrent");
            assert!(receiver.snapshot(&id).is_err());
            Ok(())
        }
        .await;
        receiver.stop().await;
        seed.stop().await;
        result
    })
    .await
    .context("Transfer features regression timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_pause_resume_and_explicit_sharing_survive_restart() -> Result<()> {
    timeout(Duration::from_secs(25), async {
        let root = TempDir::new()?;
        let cfg = config(root.path(), "seed");
        let seed = TransferSession::start(cfg.clone()).await?;
        for (name, byte) in [("a.bin", 41), ("b.bin", 42)] {
            let path = root.path().join(name);
            tokio::fs::write(&path, vec![byte; 65_537]).await?;
            let shared = seed.share(&path, vec![]).await?;
            seed.remove(&shared.infohash, false).await?;
            // Even a source in an infohash-named folder remains an explicit
            // share, including after reopening persisted session state.
            let folder = root.path().join(&shared.infohash);
            tokio::fs::create_dir(&folder).await?;
            let moved = folder.join(name);
            tokio::fs::rename(&path, &moved).await?;
            assert_eq!(seed.share(&moved, vec![]).await?.infohash, shared.infohash);
        }
        seed.set_seed_completed_downloads(false).await;
        seed.pause_all().await?;
        assert!(
            seed.snapshots()?
                .iter()
                .all(|s| s.state == TransferState::Paused)
        );
        seed.pause_all().await?; // Idempotent.
        seed.resume_all(vec![]).await?;
        assert!(
            seed.snapshots()?
                .iter()
                .all(|s| s.state == TransferState::Seeding)
        );
        seed.stop().await;
        let mut cfg = cfg;
        cfg.seed_completed_downloads = false;
        let reopened = TransferSession::start(cfg).await?;
        let result: Result<()> = async {
            timeout(Duration::from_secs(5), async {
                loop {
                    let snapshots = reopened.snapshots()?;
                    if snapshots.len() == 2
                        && snapshots.iter().all(|s| s.state == TransferState::Seeding)
                    {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            reopened.pause_all().await?;
            reopened.resume_all(vec![]).await?;
            assert_eq!(reopened.snapshots()?.len(), 2);
            Ok(())
        }
        .await;
        reopened.stop().await;
        result
    })
    .await
    .context("Global pause regression timed out")?
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deletion_refuses_symlinks_and_preserves_outside_files() -> Result<()> {
    timeout(Duration::from_secs(20), async {
        let root = TempDir::new()?;
        let source = root.path().join("shared");
        tokio::fs::create_dir_all(source.join("nested")).await?;
        let file = source.join("nested/fixture.bin");
        tokio::fs::write(&file, content(65_537)).await?;
        let session = TransferSession::start(config(root.path(), "seed")).await?;
        let result: Result<()> = async {
            let shared = session.share(&source, vec![]).await?;
            session.pause(&shared.infohash).await?;
            let outside = root.path().join("outside");
            tokio::fs::rename(source.join("nested"), &outside).await?;
            std::os::unix::fs::symlink(&outside, source.join("nested"))?;
            assert!(session.remove(&shared.infohash, true).await.is_err());
            assert_eq!(
                tokio::fs::read(outside.join("fixture.bin")).await?,
                content(65_537)
            );
            assert!(session.snapshot(&shared.infohash).is_ok());
            tokio::fs::remove_file(source.join("nested")).await?;
            tokio::fs::rename(&outside, source.join("nested")).await?;
            let keep = source.join("nested/unrelated.txt");
            tokio::fs::write(&keep, b"not listed in metainfo").await?;
            session.remove(&shared.infohash, true).await?;
            assert!(!file.exists());
            assert_eq!(tokio::fs::read(keep).await?, b"not listed in metainfo");
            Ok(())
        }
        .await;
        session.stop().await;
        result
    })
    .await
    .context("Delete safety regression timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_download_rechecks_and_stops_sharing_on_restart() -> Result<()> {
    timeout(Duration::from_secs(30), async {
        let root = TempDir::new()?;
        let source = root.path().join("restart.bin");
        let bytes = content(65_537);
        tokio::fs::write(&source, &bytes).await?;
        let seed = TransferSession::start(config(root.path(), "seed")).await?;
        let cfg = config(root.path(), "receiver");
        let receiver = TransferSession::start(cfg.clone()).await?;
        let result: Result<String> = async {
            let shared = seed.share(&source, vec![]).await?;
            let resolved = receiver
                .resolve(
                    TorrentInput::Bytes(shared.torrent_bytes().to_vec()),
                    vec![seed.listen_addr().unwrap()],
                    Duration::from_secs(3),
                )
                .await?;
            let id = receiver.download(&resolved, None).await?;
            receiver
                .wait_for_completion(&id, Duration::from_secs(10))
                .await?;
            assert_eq!(receiver.snapshot(&id)?.state, TransferState::Seeding);
            Ok(id)
        }
        .await;
        receiver.stop().await;
        seed.stop().await;
        let id = result?;
        let mut cfg = cfg;
        cfg.seed_completed_downloads = false;
        let reopened = TransferSession::start(cfg).await?;
        let result: Result<()> = async {
            timeout(Duration::from_secs(5), async {
                loop {
                    let snapshot = reopened.snapshot(&id)?;
                    if snapshot.state == TransferState::Paused {
                        assert_eq!(snapshot.verified_bytes, bytes.len() as u64);
                        assert_eq!(
                            tokio::fs::read(reopened.completed_file(&id, 0)?).await?,
                            bytes
                        );
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            Ok(())
        }
        .await;
        reopened.stop().await;
        result
    })
    .await
    .context("Completion restart regression timed out")?
}
