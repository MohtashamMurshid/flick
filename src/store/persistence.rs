//! Single ordered writer. UI callers only clone settings and replace a pending snapshot.
use super::Settings;
use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    thread::{self, JoinHandle},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct SaveStatus {
    pub queued_generation: u64,
    pub completed_generation: u64,
    pub writes: u64,
    pub unchanged: u64,
}

struct Request {
    generation: u64,
    path: PathBuf,
    data: Settings,
    reply: Option<mpsc::Sender<std::result::Result<(), String>>>,
}

#[derive(Default)]
struct State {
    pending: VecDeque<Request>,
    status: SaveStatus,
    stopping: bool,
    worker_exited: bool,
    failures: HashMap<PathBuf, String>,
    unread_error: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    #[cfg(test)]
    write_gate: Mutex<()>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A Send durability fence. Wait on a background executor before an operation
/// that can make ownership changes durable independently of settings.
pub struct SaveBarrier {
    shared: Arc<Shared>,
    target: u64,
    start_error: Option<String>,
}

impl SaveBarrier {
    pub fn wait(self) -> Result<()> {
        if let Some(error) = self.start_error {
            bail!("{error}");
        }
        let mut state = self.shared.lock();
        while state.status.completed_generation < self.target {
            if state.worker_exited {
                bail!("Settings writer stopped before the requested save completed");
            }
            state = self
                .shared
                .wake
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if !state.failures.is_empty() {
            let mut errors = state.failures.values().cloned().collect::<Vec<_>>();
            errors.sort();
            bail!("{}", errors.join("; "));
        }
        Ok(())
    }
}

// Also runs during unwinding, so a failed worker cannot leave a waiter asleep.
struct WorkerLifetime(Arc<Shared>);
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        self.0.lock().worker_exited = true;
        self.0.wake.notify_all();
    }
}

pub(super) struct Writer {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    start_error: Option<String>,
}

impl Writer {
    pub fn new(initial: Option<(PathBuf, Vec<u8>)>) -> Self {
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("flick-settings".into())
            .spawn(move || {
                let _lifetime = WorkerLifetime(worker_shared.clone());
                let mut previous = initial;
                loop {
                    let request = {
                        let mut state = worker_shared.lock();
                        while state.pending.is_empty() && !state.stopping {
                            state = worker_shared
                                .wake
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        match state.pending.pop_front() {
                            Some(request) => request,
                            None => break,
                        }
                    };
                    #[cfg(test)]
                    let _gate = worker_shared.write_gate.lock().unwrap();
                    // No state lock is held during sanitization, serialization, or disk I/O.
                    let result = (|| -> Result<bool> {
                        let bytes = super::serialized(request.data)?;
                        if previous
                            .as_ref()
                            .is_some_and(|(path, old)| path == &request.path && *old == bytes)
                        {
                            return Ok(false);
                        }
                        super::atomic_write(&request.path, &bytes)?;
                        previous = Some((request.path.clone(), bytes));
                        Ok(true)
                    })();
                    let mut state = worker_shared.lock();
                    state.status.completed_generation = request.generation;
                    let reply = match result {
                        Ok(written) => {
                            if written {
                                state.status.writes += 1;
                            } else {
                                state.status.unchanged += 1;
                            }
                            state.failures.remove(&request.path);
                            Ok(())
                        }
                        Err(error) => {
                            let error = format!(
                                "Settings save generation {} at {} failed: {error:#}",
                                request.generation,
                                request.path.display()
                            );
                            tracing::error!(%error);
                            state.failures.insert(request.path.clone(), error.clone());
                            state.unread_error = Some(error.clone());
                            // A failed replacement/directory sync must never seed deduplication.
                            previous = None;
                            Err(error)
                        }
                    };
                    if let Some(sender) = request.reply {
                        let _ = sender.send(reply);
                    }
                    worker_shared.wake.notify_all();
                }
            });
        match worker {
            Ok(worker) => Self {
                shared,
                worker: Mutex::new(Some(worker)),
                start_error: None,
            },
            Err(error) => Self {
                shared,
                worker: Mutex::new(None),
                start_error: Some(format!("Cannot start settings writer: {error}")),
            },
        }
    }

    fn submit(
        &self,
        path: PathBuf,
        data: Settings,
        reply: Option<mpsc::Sender<std::result::Result<(), String>>>,
    ) -> Result<u64> {
        if let Some(error) = &self.start_error {
            bail!("{error}");
        }
        let mut state = self.shared.lock();
        if state.stopping || state.worker_exited {
            bail!("Settings writer has shut down");
        }
        state.status.queued_generation = state
            .status
            .queued_generation
            .checked_add(1)
            .context("Settings generation overflow")?;
        let generation = state.status.queued_generation;
        // Synchronous requests are barriers; never discard one or coalesce across it.
        if reply.is_none()
            && state
                .pending
                .back()
                .is_some_and(|old| old.reply.is_none() && old.path == path)
        {
            state.pending.pop_back();
        }
        state.pending.push_back(Request {
            generation,
            path,
            data,
            reply,
        });
        self.shared.wake.notify_all();
        Ok(generation)
    }

    pub fn queue(&self, path: PathBuf, data: Settings) -> Result<u64> {
        self.submit(path, data, None)
    }

    #[cfg_attr(not(test), allow(dead_code))] // Used by synchronous Store diagnostics.
    pub fn save(&self, path: PathBuf, data: Settings) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.submit(path, data, Some(sender))?;
        receiver
            .recv()
            .context("Settings writer stopped before saving")?
            .map_err(anyhow::Error::msg)
    }

    pub fn barrier(&self) -> SaveBarrier {
        SaveBarrier {
            shared: self.shared.clone(),
            target: self.shared.lock().status.queued_generation,
            start_error: self.start_error.clone(),
        }
    }

    pub fn flush(&self) -> Result<()> {
        self.barrier().wait()
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared
            .lock()
            .unread_error
            .take()
            .or_else(|| self.start_error.clone())
    }
    #[cfg_attr(not(test), allow(dead_code))] // Used by synchronous Store diagnostics.
    pub fn status(&self) -> SaveStatus {
        self.shared.lock().status
    }

    pub fn shutdown(&self) -> Result<()> {
        {
            let mut state = self.shared.lock();
            state.stopping = true;
            self.shared.wake.notify_all();
        }
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(worker) = worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Settings writer panicked"))?;
        }
        self.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::error!(%error, "Settings shutdown flush failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_writer_coalesces_latest_without_blocking_queue_and_flushes_on_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let writer = Writer::new(None);
        // Simulate stalled serialization/disk while the UI queues newer snapshots.
        let gate = writer.shared.write_gate.lock().unwrap();
        for generation in 1..=100 {
            let data = Settings {
                volume: generation as f64,
                ..Settings::default()
            };
            assert_eq!(writer.queue(path.clone(), data).unwrap(), generation);
        }
        assert_eq!(writer.status().completed_generation, 0);
        assert!(writer.shared.lock().pending.len() <= 1);
        drop(gate);
        writer.shutdown().unwrap();
        let saved: Settings = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.volume, 100.0);
        assert_eq!(writer.status().completed_generation, 100);
        assert!(writer.status().writes <= 2);
    }

    #[test]
    fn owned_barrier_waits_for_durable_snapshot_off_thread() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let writer = Writer::new(None);
        let gate = writer.shared.write_gate.lock().unwrap();
        writer.queue(path.clone(), Settings::default()).unwrap();
        let barrier = writer.barrier();
        let (done, completion) = mpsc::channel();
        let waiter = thread::spawn(move || done.send(barrier.wait()).unwrap());
        assert!(
            completion
                .recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        drop(gate);
        completion
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(path.is_file());
        waiter.join().unwrap();
    }

    #[test]
    fn worker_exit_wakes_pending_barrier_instead_of_waiting_forever() {
        let shared = Arc::new(Shared::default());
        shared.lock().status.queued_generation = 1;
        let barrier = SaveBarrier {
            shared: shared.clone(),
            target: 1,
            start_error: None,
        };
        let waiter = thread::spawn(move || barrier.wait());
        // This is the same guard that runs if serialization or I/O panics.
        drop(WorkerLifetime(shared));
        assert!(
            waiter
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("stopped")
        );
    }

    #[test]
    fn synchronous_barriers_keep_queued_write_order_and_distinct_destinations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let export = directory.path().join("export.json");
        let writer = Writer::new(None);
        let first = Settings {
            volume: 10.0,
            ..Settings::default()
        };
        writer.queue(path.clone(), first).unwrap();
        let middle = Settings {
            volume: 20.0,
            ..Settings::default()
        };
        writer.save(export.clone(), middle).unwrap();
        let last = Settings {
            volume: 30.0,
            ..Settings::default()
        };
        writer.queue(path.clone(), last).unwrap();
        writer.flush().unwrap();
        for (file, expected) in [(path, 30.0), (export, 20.0)] {
            let saved: Settings = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            assert_eq!(saved.volume, expected);
        }
        assert_eq!(writer.status().completed_generation, 3);
    }
}
