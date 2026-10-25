//! Filesystem work for the desktop library. No filesystem operation runs on GPUI.
//!
//! A latest-model mailbox and bounded, coalesced event/result mailboxes keep a
//! busy directory from growing queues. Native watches are nonrecursive. Only
//! failed/unavailable watches get periodic polling; startup/focus are explicit.
use flick::library::FileLocation;
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::oneshot;

#[cfg(target_os = "windows")]
mod trash_windows;

const EVENT_LIMIT: usize = 2048;
const RESULT_LIMIT: usize = 256;
const BATCH: usize = 32;
const DRAIN_BUDGET: Duration = Duration::from_millis(2);
const SETTLE: Duration = Duration::from_millis(400);
const FALLBACK: Duration = Duration::from_secs(30);
const FOCUS_INTERVAL: Duration = Duration::from_secs(5);

/// Incarnations follow membership/torrent identity, not poster/history revisions.
/// Filesystem observations additionally carry a snapshot revision, so a late
/// absence is rechecked after any import without invalidating Delete dialogs
/// when unrelated cached metadata arrives.
#[derive(Default)]
pub(crate) struct FileGenerations {
    next: u64,
    paths: HashMap<PathBuf, (u64, Option<(String, usize)>)>,
}

impl FileGenerations {
    pub fn observe(&mut self, path: &Path, torrent: Option<(&str, usize)>) -> u64 {
        if let Some((generation, identity)) = self.paths.get(path)
            && identity.as_ref().map(|(id, index)| (id.as_str(), *index)) == torrent
        {
            return *generation;
        }
        self.next = self.next.wrapping_add(1);
        self.paths.insert(
            path.to_path_buf(),
            (self.next, torrent.map(|(id, index)| (id.to_owned(), index))),
        );
        self.next
    }
    pub fn get(&self, path: &Path) -> Option<u64> {
        self.paths.get(path).map(|(generation, _)| *generation)
    }
    pub fn remove(&mut self, path: &Path) {
        self.paths.remove(path);
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&Path) -> bool) {
        self.paths.retain(|path, _| keep(path));
    }
}

/// Observation epochs change only for this path's inputs. History saves and
/// unrelated poster completions must not invalidate every filesystem result.
#[derive(Default)]
pub(crate) struct ObservationGenerations {
    next: u64,
    paths: HashMap<PathBuf, (u64, u64, Option<PathBuf>, bool)>,
}
impl ObservationGenerations {
    pub fn observe(
        &mut self,
        path: &Path,
        incarnation: u64,
        poster: Option<&Path>,
        incomplete: bool,
    ) -> u64 {
        if let Some((epoch, old, cached, partial)) = self.paths.get(path)
            && *old == incarnation
            && cached.as_deref() == poster
            && *partial == incomplete
        {
            return *epoch;
        }
        self.next = self.next.wrapping_add(1);
        self.paths.insert(
            path.to_owned(),
            (
                self.next,
                incarnation,
                poster.map(Path::to_owned),
                incomplete,
            ),
        );
        self.next
    }
    pub fn get(&self, path: &Path) -> Option<u64> {
        self.paths.get(path).map(|value| value.0)
    }
    pub fn refresh(&mut self, path: &Path) {
        self.paths.remove(path);
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&Path) -> bool) {
        self.paths.retain(|path, _| keep(path));
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TrackedFile {
    pub path: PathBuf,
    pub snapshot: u64,
    pub generation: u64,
    pub poster: Option<PathBuf>,
    pub incomplete: bool,
    pub location: Option<FileLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Available,
    Unavailable(String),
    Missing,
    Metadata {
        media: bool,
        poster: bool,
        replaced: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub path: PathBuf,
    pub snapshot: u64,
    pub generation: u64,
    pub change: Change,
    pub location: Option<FileLocation>,
    sequence: u64,
    stamp: Option<Stamp>,
    poster_stamp: Option<Stamp>,
}

#[derive(Clone, Debug)]
pub(crate) struct TrashTicket {
    pub path: PathBuf,
    pub generation: u64,
    stamp: Stamp,
    parent: Identity,
}

pub(crate) struct Prepared {
    pub request: u64,
    pub generation: u64,
    pub result: Result<TrashTicket, String>,
}

#[derive(Clone)]
pub(crate) struct LibraryFiles {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Inbox {
    stopped: bool,
    model: Option<Vec<TrackedFile>>,
    updates: HashMap<PathBuf, Option<TrackedFile>>,
    // This map is published synchronously, before queued native work can act.
    current: HashMap<PathBuf, u64>,
    events: HashSet<PathBuf>,
    reconcile: bool,
    focus: bool,
    watch_error: bool,
    prepare: Option<(PathBuf, u64, u64)>,
    trash: Option<(TrashTicket, oneshot::Sender<Result<(), String>>)>,
    acknowledgements: HashMap<PathBuf, Observation>,
    // Independent bounded lanes: an offline-library flood must never occupy
    // the slots needed for Missing, recovery, or metadata transitions.
    results: HashMap<PathBuf, Observation>,
    unavailable: HashMap<PathBuf, Observation>,
    // Initial positive proofs never hold up destructive/visible transitions.
    // At most one proof per tracked path; the UI takes the map in O(1).
    proofs: HashMap<PathBuf, Observation>,
    prepared: VecDeque<Prepared>,
}

#[derive(Default)]
struct EventIndex {
    files: HashMap<PathBuf, Vec<PathBuf>>,
    parents: HashMap<PathBuf, usize>,
}

impl EventIndex {
    fn add(&mut self, file: &TrackedFile) {
        for path in std::iter::once(&file.path).chain(file.poster.iter()) {
            self.files
                .entry(path.clone())
                .or_default()
                .push(file.path.clone());
            if let Some(parent) = path.parent() {
                *self.parents.entry(parent.to_owned()).or_default() += 1;
            }
        }
    }

    fn remove(&mut self, file: &TrackedFile) {
        for path in std::iter::once(&file.path).chain(file.poster.iter()) {
            if let Some(owners) = self.files.get_mut(path) {
                owners.retain(|owner| owner != &file.path);
                if owners.is_empty() {
                    self.files.remove(path);
                }
            }
            if let Some(parent) = path.parent()
                && let Some(count) = self.parents.get_mut(parent)
            {
                *count -= 1;
                if *count == 0 {
                    self.parents.remove(parent);
                }
            }
        }
    }
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    index: Mutex<EventIndex>,
    wake: Condvar,
}

impl LibraryFiles {
    pub fn start() -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        thread::Builder::new()
            .name("flick-library-files".into())
            .spawn(move || run(worker))
            .map_err(|error| format!("Could not start library file monitoring. {error}"))?;
        Ok(Self { shared })
    }

    pub fn sync(&self, files: Vec<TrackedFile>) {
        let current = files
            .iter()
            .map(|f| (f.path.clone(), f.generation))
            .collect();
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.current = current;
        inbox.model = Some(files);
        inbox.updates.clear();
        self.shared.wake.notify_one();
    }

    /// Publish only changed observation inputs. Membership generations become
    /// current synchronously, before a queued Trash operation can use them.
    pub fn update(&self, files: Vec<TrackedFile>, removed: Vec<PathBuf>) {
        if files.is_empty() && removed.is_empty() {
            return;
        }
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        for path in removed {
            inbox.current.remove(&path);
            inbox.updates.insert(path.clone(), None);
            inbox.results.remove(&path);
            inbox.unavailable.remove(&path);
            inbox.proofs.remove(&path);
        }
        for file in files {
            inbox.current.insert(file.path.clone(), file.generation);
            inbox.updates.insert(file.path.clone(), Some(file));
        }
        self.shared.wake.notify_one();
    }

    pub fn reconcile(&self) {
        self.shared
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconcile = true;
        self.shared.wake.notify_one();
    }

    pub fn reconcile_focus(&self) {
        self.shared
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .focus = true;
        self.shared.wake.notify_one();
    }

    pub fn drain(&self) -> (Vec<Observation>, Vec<Prepared>) {
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let mut observations = Vec::new();
        // Missing/Metadata/recovery precede bulk Unavailable, and all bypass
        // initial proofs. Each mailbox lane has at most RESULT_LIMIT entries.
        // This only transfers ownership; UI application has its own budget.
        loop {
            let next = if let Some(path) = inbox.results.keys().next().cloned() {
                inbox.results.remove(&path)
            } else if let Some(path) = inbox.unavailable.keys().next().cloned() {
                inbox.unavailable.remove(&path)
            } else {
                None
            };
            let Some(observation) = next else { break };
            observations.push(observation);
            if start.elapsed() >= DRAIN_BUDGET {
                break;
            }
        }
        let prepared = inbox.prepared.drain(..).collect();
        self.shared.wake.notify_one();
        (observations, prepared)
    }

    pub fn drain_proofs(&self) -> HashMap<PathBuf, Observation> {
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut inbox.proofs)
    }

    pub fn acknowledge(&self, observation: Observation) {
        self.acknowledge_many(std::iter::once(observation));
    }

    pub fn acknowledge_many(&self, observations: impl IntoIterator<Item = Observation>) {
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        for observation in observations {
            if inbox.current.get(&observation.path) == Some(&observation.generation) {
                let slot = inbox.acknowledgements.entry(observation.path.clone());
                slot.and_modify(|old| {
                    if old.sequence < observation.sequence {
                        *old = observation.clone();
                    }
                })
                .or_insert(observation);
            }
        }
        self.shared.wake.notify_one();
    }

    pub fn prepare(&self, path: PathBuf, generation: u64, request: u64) {
        self.shared
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prepare = Some((path, generation, request));
        self.shared.wake.notify_one();
    }

    pub fn trash(&self, ticket: TrashTicket) -> oneshot::Receiver<Result<(), String>> {
        let (sender, receiver) = oneshot::channel();
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.stopped || inbox.trash.is_some() {
            let _ = sender.send(Err(
                "Another file operation is still finishing. Please retry.".into(),
            ));
        } else {
            inbox.trash = Some((ticket, sender));
            self.shared.wake.notify_one();
        }
        receiver
    }

    pub fn shutdown(&self) {
        self.shared
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped = true;
        self.shared.wake.notify_one();
        // No UI-thread join against a filesystem/native operation.
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl Identity {
    fn key(&self) -> Option<String> {
        #[cfg(unix)]
        {
            Some(format!("unix:{}:{}", self.device, self.inode))
        }
        #[cfg(not(unix))]
        {
            self.created
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|time| format!("created:{}:{}", time.as_secs(), time.subsec_nanos()))
        }
    }
    fn of(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                created: metadata.created().ok(),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            identity: Identity::of(metadata),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            changed: {
                use std::os::unix::fs::MetadataExt;
                (metadata.ctime(), metadata.ctime_nsec())
            },
        }
    }

    fn content_differs(&self, other: &Self) -> bool {
        self.identity != other.identity || self.len != other.len || self.modified != other.modified
    }
}

struct Entry {
    file: TrackedFile,
    baseline: Option<Stamp>,
    parent: Option<Identity>,
    mount: Option<String>,
    poster: Option<Stamp>,
    checked_poster: bool,
    missing_since: Option<Instant>,
    pending: Option<Observation>,
    last: Option<Change>,
}

impl Entry {
    fn current_location(&self) -> Option<FileLocation> {
        Some(FileLocation {
            parent: self.parent.as_ref()?.key()?,
            volume: self.mount.clone()?,
        })
    }
    fn new(file: TrackedFile) -> Self {
        Self {
            file,
            baseline: None,
            parent: None,
            mount: None,
            poster: None,
            checked_poster: false,
            missing_since: None,
            pending: None,
            last: None,
        }
    }
}

#[derive(Default)]
struct Work {
    due: HashMap<PathBuf, Instant>,
    order: Vec<PathBuf>,
    cursor: Option<usize>,
}

#[derive(Default)]
struct ReadyLane {
    order: VecDeque<PathBuf>,
    paths: HashSet<PathBuf>,
}

impl ReadyLane {
    fn insert(&mut self, path: PathBuf) {
        if self.paths.insert(path.clone()) {
            self.order.push_back(path);
        }
    }

    fn remove(&mut self, path: &Path) {
        if self.paths.remove(path) {
            self.order.retain(|queued| queued != path);
        }
    }

    fn invalidate(&mut self, paths: &HashSet<PathBuf>) {
        self.order.retain(|path| !paths.contains(path));
        self.paths.retain(|path| !paths.contains(path));
    }

    fn publish(&mut self, entries: &HashMap<PathBuf, Entry>, inbox: &mut Inbox, offline: bool) {
        while let Some(path) = self.order.front() {
            let result = entries.get(path).and_then(|entry| entry.pending.as_ref());
            if let Some(result) = result
                && matches!(result.change, Change::Unavailable(_)) == offline
                && inbox.current.get(path) == Some(&result.generation)
            {
                let output = if offline {
                    &mut inbox.unavailable
                } else {
                    &mut inbox.results
                };
                if output.len() >= RESULT_LIMIT && !output.contains_key(path) {
                    break;
                }
                output.insert(path.clone(), result.clone());
                if offline {
                    inbox.results.remove(path);
                } else {
                    inbox.unavailable.remove(path);
                }
                inbox.proofs.remove(path);
            }
            self.paths.remove(path);
            self.order.pop_front();
        }
    }
}

#[derive(Default)]
struct ReadyResults {
    urgent: ReadyLane,
    unavailable: ReadyLane,
}

impl ReadyResults {
    fn insert(&mut self, path: PathBuf, offline: bool) {
        let (lane, other) = if offline {
            (&mut self.unavailable, &mut self.urgent)
        } else {
            (&mut self.urgent, &mut self.unavailable)
        };
        other.remove(&path);
        lane.insert(path);
    }

    fn remove(&mut self, path: &Path) {
        self.urgent.remove(path);
        self.unavailable.remove(path);
    }

    fn invalidate(&mut self, paths: &HashSet<PathBuf>) {
        self.urgent.invalidate(paths);
        self.unavailable.invalidate(paths);
    }

    fn is_empty(&self) -> bool {
        self.urgent.order.is_empty() && self.unavailable.order.is_empty()
    }

    fn publish(&mut self, entries: &HashMap<PathBuf, Entry>, inbox: &mut Inbox) {
        self.urgent.publish(entries, inbox, false);
        self.unavailable.publish(entries, inbox, true);
    }
}

impl Work {
    fn full(&mut self) {
        // Repeated focus/errors must not restart a sweep before its tail runs.
        self.cursor.get_or_insert(0);
    }
    fn queue(&mut self, path: PathBuf, when: Instant) {
        if let Some(existing) = self.due.get_mut(&path) {
            *existing = (*existing).min(when);
        } else if self.due.len() < EVENT_LIMIT {
            self.due.insert(path, when);
        } else {
            self.full();
        }
    }
    fn batch(&mut self, now: Instant) -> Vec<PathBuf> {
        let mut batch: Vec<_> = self
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .take(BATCH)
            .map(|(p, _)| p.clone())
            .collect();
        for path in &batch {
            self.due.remove(path);
        }
        if let Some(cursor) = &mut self.cursor {
            while batch.len() < BATCH && *cursor < self.order.len() {
                let path = self.order[*cursor].clone();
                *cursor += 1;
                if !batch.contains(&path) {
                    batch.push(path);
                }
            }
            if *cursor == self.order.len() {
                self.cursor = None;
            }
        }
        batch
    }
}

fn collect_event(shared: &Shared, event: notify::Result<notify::Event>) {
    // Reading/statting the watched directory itself must not cause feedback.
    if event
        .as_ref()
        .is_ok_and(|e| matches!(e.kind, EventKind::Access(_)))
    {
        return;
    }
    let index = shared.index.lock().unwrap_or_else(|e| e.into_inner());
    let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        Err(_) => {
            inbox.watch_error = true;
            inbox.reconcile = true;
        }
        Ok(event) => {
            if event.need_rescan() {
                inbox.reconcile = true;
            }
            if event.paths.len() > EVENT_LIMIT {
                inbox.reconcile = true;
            }
            for path in event.paths.iter().take(EVENT_LIMIT) {
                if index.parents.contains_key(path) {
                    inbox.reconcile = true;
                }
                if let Some(owners) = index.files.get(path) {
                    for owner in owners {
                        if inbox.events.len() < EVENT_LIMIT {
                            inbox.events.insert(owner.clone());
                        } else {
                            inbox.reconcile = true;
                            break;
                        }
                    }
                }
            }
        }
    }
    shared.wake.notify_one();
}

fn install_watches(
    shared: &Arc<Shared>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    watches: &mut HashSet<PathBuf>,
    fallback: &mut HashSet<PathBuf>,
) {
    if watcher.is_none() {
        let callback = shared.clone();
        *watcher = notify::recommended_watcher(move |event| collect_event(&callback, event)).ok();
    }
    let parents: HashSet<_> = shared
        .index
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .parents
        .keys()
        .cloned()
        .collect();
    if let Some(watcher) = watcher {
        for old in watches.difference(&parents).cloned().collect::<Vec<_>>() {
            let _ = watcher.unwatch(&old);
            watches.remove(&old);
        }
        fallback.retain(|p| parents.contains(p));
        for parent in parents.difference(watches).cloned().collect::<Vec<_>>() {
            if watcher.watch(&parent, RecursiveMode::NonRecursive).is_ok() {
                watches.insert(parent.clone());
                fallback.remove(&parent);
            } else {
                fallback.insert(parent);
            }
        }
    } else {
        *fallback = parents;
    }
}

#[derive(Default)]
struct FocusSweeps {
    last: Option<Instant>,
}

impl FocusSweeps {
    fn accept(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < FOCUS_INTERVAL)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

fn run(shared: Arc<Shared>) {
    let mut watcher = None;
    let mut watches = HashSet::new();
    let mut fallback = HashSet::new();
    let mut entries: HashMap<PathBuf, Entry> = HashMap::new();
    let mut work = Work::default();
    let mut next_fallback = Instant::now() + FALLBACK;
    let mut next_watch_retry = Instant::now();
    let mut focus_sweeps = FocusSweeps::default();
    let mut sequence = 0u64;
    let mut ready = ReadyResults::default();
    loop {
        let (model, updates, events, full, focus, watch_error, prepare, trash, acks) = {
            let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            if inbox.stopped {
                break;
            }
            (
                inbox.model.take(),
                std::mem::take(&mut inbox.updates),
                std::mem::take(&mut inbox.events),
                std::mem::take(&mut inbox.reconcile),
                std::mem::take(&mut inbox.focus),
                std::mem::take(&mut inbox.watch_error),
                inbox.prepare.take(),
                inbox.trash.take(),
                std::mem::take(&mut inbox.acknowledgements),
            )
        };
        let now = Instant::now();
        for ack in acks.into_values() {
            if let Some(entry) = entries.get_mut(&ack.path)
                && entry.file.generation == ack.generation
                && entry.file.snapshot == ack.snapshot
                && entry
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.sequence == ack.sequence)
            {
                if matches!(ack.change, Change::Metadata { media: true, .. }) {
                    entry.baseline = ack.stamp;
                }
                if ack.location.is_some() {
                    entry.file.location = ack.location;
                }
                entry.poster = ack.poster_stamp;
                entry.last = Some(ack.change);
                entry.pending = None;
            }
        }
        let mut updates = updates;
        if let Some(model) = model {
            let mut replacement: HashMap<_, _> = model
                .into_iter()
                .map(|file| (file.path.clone(), Some(file)))
                .collect();
            for path in entries.keys() {
                if !replacement.contains_key(path) {
                    replacement.insert(path.clone(), None);
                }
            }
            replacement.extend(updates);
            updates = replacement;
        }
        let mut membership_changed = false;
        let mut watches_changed = false;
        let mut invalidated = HashSet::new();
        if !updates.is_empty() {
            let mut index = shared.index.lock().unwrap_or_else(|e| e.into_inner());
            for (path, file) in updates {
                let Some(file) = file else {
                    if let Some(entry) = entries.remove(&path) {
                        index.remove(&entry.file);
                        membership_changed = true;
                        watches_changed = true;
                    }
                    invalidated.insert(path);
                    continue;
                };
                match entries.get_mut(&path) {
                    Some(entry) => {
                        if entry.file.generation != file.generation {
                            index.remove(&entry.file);
                            index.add(&file);
                            *entry = Entry::new(file);
                            work.queue(path.clone(), now);
                            invalidated.insert(path);
                            watches_changed = true;
                            continue;
                        }
                        let poster_changed = entry.file.poster != file.poster;
                        let changed = poster_changed || entry.file.incomplete != file.incomplete;
                        let epoch_changed = entry.file.snapshot != file.snapshot;
                        if poster_changed {
                            index.remove(&entry.file);
                            index.add(&file);
                            watches_changed = true;
                            entry.checked_poster = false;
                            entry.poster = None;
                        }
                        if entry.file.incomplete && !file.incomplete {
                            entry.baseline = None;
                        }
                        entry.file = file;
                        // Explicit refresh must be inspected even without a
                        // pending result. Unrelated paths keep their baselines.
                        if changed || epoch_changed {
                            work.queue(path.clone(), now);
                        }
                        if epoch_changed {
                            entry.pending = None;
                            invalidated.insert(path);
                        }
                    }
                    None => {
                        index.add(&file);
                        work.queue(path.clone(), now);
                        entries.insert(path, Entry::new(file));
                        membership_changed = true;
                        watches_changed = true;
                    }
                }
            }
        }
        if !invalidated.is_empty() {
            ready.invalidate(&invalidated);
            let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            for path in &invalidated {
                inbox.results.remove(path);
                inbox.unavailable.remove(path);
                inbox.proofs.remove(path);
            }
        }
        if membership_changed {
            work.order = entries.keys().cloned().collect();
            work.due.retain(|path, _| entries.contains_key(path));
            if work.cursor.is_some() {
                work.cursor = Some(0);
            }
        }
        let focused = focus && focus_sweeps.accept(now);
        if full || focused {
            work.full();
        }
        if watch_error {
            fallback.extend(watches.drain());
            watcher = None;
            next_watch_retry = now + FOCUS_INTERVAL;
        }
        if watches_changed || focused || now >= next_watch_retry {
            install_watches(&shared, &mut watcher, &mut watches, &mut fallback);
            next_watch_retry = now + FALLBACK;
        }
        for path in events {
            if entries.contains_key(&path) {
                work.queue(path, now + SETTLE);
            }
        }
        if now >= next_fallback {
            if !fallback.is_empty() {
                for entry in entries.values() {
                    if std::iter::once(&entry.file.path)
                        .chain(entry.file.poster.iter())
                        .filter_map(|p| p.parent())
                        .any(|p| fallback.contains(p))
                    {
                        work.queue(entry.file.path.clone(), now);
                    }
                }
            }
            next_fallback = now + FALLBACK;
        }
        if let Some((path, generation, request)) = prepare {
            let result = if is_current(&shared, &path, generation) {
                prepare_trash(&path, generation)
            } else {
                Err("This library item changed. Open Delete again.".into())
            };
            let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            if inbox.prepared.len() == 8 {
                inbox.prepared.pop_front();
            }
            inbox.prepared.push_back(Prepared {
                request,
                generation,
                result,
            });
        }
        if let Some((ticket, sender)) = trash {
            let result = if is_current(&shared, &ticket.path, ticket.generation) {
                trash_checked(&ticket)
            } else {
                Err("This library item was removed or imported again. Nothing was moved.".into())
            };
            let _ = sender.send(result);
        }
        let batch = work.batch(now);
        let mounts = if batch.is_empty() {
            Mounts::default()
        } else {
            Mounts::read()
        };
        let mut proofs = Vec::new();
        for path in batch {
            let Some(entry) = entries.get_mut(&path) else {
                continue;
            };
            let (change, stamp, poster_stamp, retry) = inspect(entry, &mounts, now);
            if let Some(at) = retry {
                work.queue(path.clone(), at);
            }
            if let Some(change) = change {
                let location = stamp.as_ref().and_then(|_| entry.current_location());
                if entry.pending.as_ref().is_some_and(|pending| {
                    pending.change == change
                        && pending.stamp == stamp
                        && pending.poster_stamp == poster_stamp
                        && pending.location == location
                }) {
                    continue;
                }
                let recovery = matches!(entry.last, Some(Change::Unavailable(_) | Change::Missing))
                    || entry.pending.as_ref().is_some_and(|pending| {
                        matches!(pending.change, Change::Unavailable(_) | Change::Missing)
                    });
                let proof = change == Change::Available && !recovery;
                if proof && entry.pending.is_none() && entry.file.location == location {
                    entry.last = Some(Change::Available);
                    continue;
                }
                if !matches!(change, Change::Metadata { .. })
                    && entry.last.as_ref() == Some(&change)
                    && entry.pending.is_none()
                    && (location.is_none() || entry.file.location == location)
                {
                    continue;
                }
                sequence = sequence.wrapping_add(1);
                let observation = Observation {
                    path: path.clone(),
                    snapshot: entry.file.snapshot,
                    generation: entry.file.generation,
                    change,
                    sequence,
                    location,
                    stamp,
                    poster_stamp,
                };
                entry.pending = Some(observation.clone());
                if proof {
                    ready.remove(&path);
                    proofs.push(observation);
                } else {
                    ready.insert(path, matches!(observation.change, Change::Unavailable(_)));
                }
            }
        }
        {
            let mut inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            for proof in proofs {
                if inbox.current.get(&proof.path) == Some(&proof.generation) {
                    inbox.results.remove(&proof.path);
                    inbox.unavailable.remove(&proof.path);
                    inbox.proofs.insert(proof.path.clone(), proof);
                }
            }
            ready.publish(&entries, &mut inbox);
            if inbox.stopped {
                break;
            }
            // A producer can notify while this iteration is doing filesystem
            // work. Check its mailbox before sleeping to avoid a lost wake.
            if inbox.model.is_some()
                || !inbox.updates.is_empty()
                || !inbox.events.is_empty()
                || inbox.reconcile
                || inbox.focus
                || inbox.watch_error
                || inbox.prepare.is_some()
                || inbox.trash.is_some()
                || !inbox.acknowledgements.is_empty()
            {
                continue;
            }
            // Keep scanning ready startup work without a 32-files/30ms cap.
            // The mailbox is checked every batch, including Trash and stop.
            if work.cursor.is_some() || work.due.values().any(|at| *at <= Instant::now()) {
                continue;
            }
            // A bounded maintenance wake drains results/retries, never scans the
            // library. No work means sleep until the next event/fallback.
            let delay = if work.cursor.is_some() || !work.due.is_empty() || !ready.is_empty() {
                Duration::from_millis(30)
            } else {
                next_fallback
                    .saturating_duration_since(Instant::now())
                    .min(next_watch_retry.saturating_duration_since(Instant::now()))
                    .min(FALLBACK)
            };
            let _ = shared.wake.wait_timeout(inbox, delay);
        }
    }
}

fn is_current(shared: &Shared, path: &Path, generation: u64) -> bool {
    let inbox = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
    !inbox.stopped && inbox.current.get(path) == Some(&generation)
}

type Inspection = (
    Option<Change>,
    Option<Stamp>,
    Option<Stamp>,
    Option<Instant>,
);

fn inspect(entry: &mut Entry, mounts: &Mounts, now: Instant) -> Inspection {
    let path = &entry.file.path;
    let Some(parent) = path.parent() else {
        return (
            Some(Change::Unavailable(
                "The file has no local parent folder.".into(),
            )),
            None,
            None,
            None,
        );
    };
    let parent_metadata = fs::metadata(parent);
    let parent_id = parent_metadata.as_ref().ok().map(Identity::of);
    let mount = mounts.local_mount(path);
    let stable = entry
        .parent
        .as_ref()
        .is_none_or(|old| parent_id.as_ref() == Some(old))
        && entry
            .mount
            .as_ref()
            .is_none_or(|old| mount.as_ref() == Some(old));
    if entry.parent.is_none() {
        entry.parent = parent_id.clone();
    }
    if entry.mount.is_none() {
        entry.mount = mount.clone();
    }
    let metadata = fs::metadata(path);
    let stamp = match metadata {
        Ok(metadata) if metadata.is_file() => {
            // Positive presence can recover a previously offline volume. The
            // stable-parent requirement belongs to destructive absence proofs.
            entry.parent = parent_id;
            entry.mount = mount;
            entry.missing_since = None;
            Stamp::of(&metadata)
        }
        Ok(_) => {
            return (
                Some(Change::Unavailable(
                    "The file or its volume has changed or is not a regular file.".into(),
                )),
                None,
                None,
                None,
            );
        }
        Err(error) => {
            // First observation after launch cannot distinguish a deleted file
            // from an unmounted custom mountpoint without saved positive proof.
            let known_location = entry.baseline.is_some()
                || entry
                    .file
                    .location
                    .as_ref()
                    .is_some_and(|saved| entry.current_location().as_ref() == Some(saved));
            let absent = error.kind() == io::ErrorKind::NotFound
                && known_location
                && stable
                && mount.is_some()
                && parent_metadata.as_ref().is_ok_and(|m| m.is_dir())
                && readable_absence(path, parent, parent_id.as_ref());
            if absent && !entry.file.incomplete {
                let since = *entry.missing_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= SETTLE {
                    return (Some(Change::Missing), None, None, None);
                }
                return (None, None, None, Some(since + SETTLE));
            }
            entry.missing_since = None;
            return (
                Some(Change::Unavailable(format!(
                    "The file is unavailable; its library record is kept. {error}"
                ))),
                None,
                None,
                None,
            );
        }
    };
    let media_changed = entry
        .baseline
        .as_ref()
        .is_some_and(|old| old.content_differs(&stamp))
        && !entry.file.incomplete;
    let replaced = media_changed
        && entry
            .baseline
            .as_ref()
            .is_some_and(|old| old.identity != stamp.identity);
    if entry.baseline.is_none() || entry.file.incomplete {
        entry.baseline = Some(stamp.clone());
    }
    let mut poster_stamp = None;
    let mut poster_changed = false;
    if let Some(poster) = &entry.file.poster {
        match fs::symlink_metadata(poster) {
            Ok(metadata) if metadata.is_file() => {
                let current = Stamp::of(&metadata);
                poster_changed = entry.checked_poster
                    && entry.poster.as_ref().is_some_and(|old| old != &current);
                if !entry.checked_poster {
                    entry.poster = Some(current.clone());
                }
                poster_stamp = Some(current);
                entry.checked_poster = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A missing/unreadable cache volume does not prove the poster
                // is gone; use the same stable-local-parent evidence.
                if let Some(parent) = poster.parent() {
                    let parent_id = fs::metadata(parent).ok().map(|m| Identity::of(&m));
                    poster_changed = mounts.local_mount(poster).is_some()
                        && readable_absence(poster, parent, parent_id.as_ref());
                }
            }
            Ok(_) => {
                poster_changed = true;
            } // never decode cache symlinks/directories
            Err(_) => {} // preserve cache metadata on permission/offline errors
        }
    }
    let change = if media_changed || poster_changed {
        Change::Metadata {
            media: media_changed,
            poster: poster_changed,
            replaced,
        }
    } else {
        Change::Available
    };
    (Some(change), Some(stamp), poster_stamp, None)
}

fn readable_absence(path: &Path, parent: &Path, before: Option<&Identity>) -> bool {
    let Some(before) = before else {
        return false;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return false;
    };
    for entry in entries {
        match entry {
            Ok(entry) if Some(entry.file_name().as_os_str()) == path.file_name() => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    matches!(fs::metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
        && fs::metadata(parent).is_ok_and(|m| m.is_dir() && Identity::of(&m) == *before)
}

fn prepare_trash(path: &Path, generation: u64) -> Result<TrashTicket, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("This file could not be checked. {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Only the selected regular video file can be moved to Trash. Folders and symbolic links are kept.".into());
    }
    let parent = path.parent().ok_or("The file has no parent folder.")?;
    let parent =
        fs::metadata(parent).map_err(|e| format!("The parent folder is unavailable. {e}"))?;
    Ok(TrashTicket {
        path: path.to_path_buf(),
        generation,
        stamp: Stamp::of(&metadata),
        parent: Identity::of(&parent),
    })
}

fn trash_checked(ticket: &TrashTicket) -> Result<(), String> {
    let fresh = prepare_trash(&ticket.path, ticket.generation)?;
    if fresh.stamp != ticket.stamp || fresh.parent != ticket.parent {
        return Err("The file changed after Delete was opened. Nothing was moved; open Delete again to check the new file.".into());
    }
    // There is intentionally no permanent-delete or cross-volume copy fallback.
    native_trash(&ticket.path)
        .map_err(|e| format!("Could not confirm that this file was moved to Trash. {e}"))
}

fn native_trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut context = trash::TrashContext::new();
        context.set_delete_method(DeleteMethod::NsFileManager);
        context.delete(path).map_err(|error| error.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        linux_trash(path).map_err(|error| format!(
            "A writable Trash on the same volume is required; no file was copied or permanently deleted. {error}"
        ))
    }
    #[cfg(target_os = "windows")]
    {
        trash_windows::recycle(path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        trash::delete(path).map_err(|error| error.to_string())
    }
}

#[cfg(target_os = "linux")]
fn linux_trash(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let device = fs::symlink_metadata(path)?.dev();
    let uid = unsafe { libc::geteuid() };
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(data_home) = data_home.filter(|path| path.is_absolute()) {
        // Do not even create the home Trash when it belongs to another device.
        let mut ancestor = data_home.as_path();
        loop {
            match fs::metadata(ancestor) {
                Ok(metadata) => {
                    if metadata.dev() == device {
                        match fs::create_dir_all(&data_home) {
                            Ok(()) => candidates.push(data_home.join("Trash")),
                            Err(error) => errors.push(io::Error::new(
                                error.kind(),
                                format!("Could not create {}: {error}", data_home.display()),
                            )),
                        }
                    }
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let Some(parent) = ancestor.parent() else {
                        break;
                    };
                    ancestor = parent;
                }
                Err(error) => {
                    errors.push(io::Error::new(
                        error.kind(),
                        format!("Could not check {}: {error}", ancestor.display()),
                    ));
                    break;
                }
            }
        }
    }
    if let Some((root, _, _)) = Mounts::read()
        .entries
        .iter()
        .find(|(root, _, _)| path.starts_with(root))
    {
        let shared = root.join(".Trash");
        match linux_shared_trash(&shared, device, uid) {
            Ok(()) => candidates.push(shared.join(uid.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => errors.push(error),
        }
        candidates.push(root.join(format!(".Trash-{uid}")));
    }
    for root in candidates {
        // Directory setup may fail and try another native location. Once a
        // move is attempted, return its result without trying a different move.
        match linux_trash_directories(&root, device, uid) {
            Ok(()) => return linux_trash_move(path, &root, device),
            Err(error) => errors.push(error),
        }
    }
    // Keep every candidate's actual failure, especially the home Trash reason;
    // a later permission error at the volume root must not hide it.
    Err(io::Error::new(
        errors
            .first()
            .map_or(io::ErrorKind::PermissionDenied, io::Error::kind),
        if errors.is_empty() {
            "No same-volume Trash is available.".into()
        } else {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        },
    ))
}

#[cfg(target_os = "linux")]
fn linux_shared_trash(path: &Path, device: u64, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.dev() != device
        || metadata.mode() & 0o1000 == 0
        || (metadata.uid() != 0 && metadata.uid() != uid)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Shared Trash {} must be a non-symlink directory on the video's volume, owned by you or root, with the sticky bit set.",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_trash_directories(root: &Path, device: u64, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    for directory in [root.to_path_buf(), root.join("files"), root.join("info")] {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Could not create Trash directory {}: {error}",
                        directory.display()
                    ),
                ));
            }
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Could not check Trash directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
        let reason = if !metadata.is_dir() || metadata.file_type().is_symlink() {
            Some("must be a directory and cannot be a symbolic link")
        } else if metadata.dev() != device {
            Some("must be on the video's volume")
        } else if metadata.uid() != uid {
            Some("must be owned by you")
        } else {
            None
        };
        // FAT/exFAT/NTFS may synthesize 0755/0775 regardless of requested 0700.
        // Ownership, type and device checks apply to home and per-user Trash;
        // only the shared .Trash ancestor has an additional sticky-bit rule.
        if let Some(reason) = reason {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Trash directory {} {reason}.", directory.display()),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_trash_move(path: &Path, root: &Path, device: u64) -> io::Result<()> {
    use std::{
        ffi::{CString, OsString},
        io::Write,
        os::unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    };
    if !path.is_absolute()
        || fs::metadata(root.join("files"))?.dev() != device
        || fs::symlink_metadata(path)?.dev() != device
    {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "Trash is on another volume.",
        ));
    }
    let source = CString::new(path.as_os_str().as_bytes())?;
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No file name."))?
        .as_bytes();
    // Leave space for a collision suffix and the .trashinfo extension.
    let filename = &filename[..filename.len().min(200)];
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs() as libc::time_t;
    let mut date = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::localtime_r(&seconds, date.as_mut_ptr()) }.is_null() {
        return Err(io::Error::other("Could not determine Trash date."));
    }
    let date = unsafe { date.assume_init() };
    let info = format!(
        "[Trash Info]\nPath={encoded}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        date.tm_year + 1900,
        date.tm_mon + 1,
        date.tm_mday,
        date.tm_hour,
        date.tm_min,
        date.tm_sec
    );
    for suffix in 0..1024 {
        let mut name = OsString::from_vec(filename.to_vec());
        if suffix != 0 {
            name.push(format!(".{suffix}"));
        }
        let destination = root.join("files").join(&name);
        name.push(".trashinfo");
        let info_path = root.join("info").join(name);
        let destination = CString::new(destination.as_os_str().as_bytes())?;
        let mut record = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path)
        {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = record
            .write_all(info.as_bytes())
            .and_then(|()| record.sync_all())
            .and_then(|()| {
                // renameat2 is atomic, refuses collisions and never copies, even
                // if a mount changes after the same-device checks above.
                if unsafe {
                    libc::renameat2(
                        libc::AT_FDCWD,
                        source.as_ptr(),
                        libc::AT_FDCWD,
                        destination.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                } == 0
                {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                drop(record);
                let _ = fs::remove_file(&info_path); // Only our unused Trash metadata.
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Trash has too many name collisions.",
    ))
}

#[derive(Default)]
struct Mounts {
    #[cfg(target_os = "linux")]
    entries: Vec<(PathBuf, String, bool)>,
}

impl Mounts {
    fn read() -> Self {
        #[cfg(target_os = "linux")]
        {
            let Ok(contents) = fs::read_to_string("/proc/self/mountinfo") else {
                return Self::default();
            };
            let mut entries = Vec::new();
            for line in contents.lines() {
                let Some((left, right)) = line.split_once(" - ") else {
                    continue;
                };
                let fields: Vec<_> = left.split_whitespace().collect();
                let Some(mountpoint) = fields.get(4) else {
                    continue;
                };
                let fs_type = right.split_whitespace().next().unwrap_or("");
                let local = matches!(
                    fs_type,
                    "ext2"
                        | "ext3"
                        | "ext4"
                        | "xfs"
                        | "btrfs"
                        | "zfs"
                        | "overlay"
                        | "tmpfs"
                        | "ramfs"
                        | "f2fs"
                        | "ntfs"
                        | "ntfs3"
                        | "exfat"
                        | "vfat"
                        | "hfs"
                        | "hfsplus"
                        | "apfs"
                );
                entries.push((
                    PathBuf::from(unescape_mount(mountpoint)),
                    // Mount IDs, mountpoint paths and flags are per-boot/runtime
                    // details, not persistent volume identity.
                    format!(
                        "linux:{}:{}:{}:{}",
                        fields[2],
                        unescape_mount(fields[3]),
                        fs_type,
                        right.split_whitespace().nth(1).unwrap_or("")
                    ),
                    local,
                ));
            }
            entries.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.as_os_str().len()));
            Self { entries }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }

    fn local_mount(&self, path: &Path) -> Option<String> {
        if !path.is_absolute() {
            return None;
        }
        // A removable root's stub can remain readable after unmount. On
        // Linux require an actual local mount beneath that root, plus the
        // persisted positive parent/volume proof used by inspect().
        let removable_roots = [
            "/media",
            "/mnt",
            "/run/media",
            "/Volumes",
            "/net",
            "/Network",
        ];
        let removable = removable_roots.iter().any(|root| path.starts_with(root));
        #[cfg(not(target_os = "linux"))]
        if removable {
            return None;
        }
        #[cfg(target_os = "linux")]
        {
            self.entries
                .iter()
                .find(|(root, _, _)| path.starts_with(root))
                .and_then(|(root, id, local)| {
                    (*local
                        && (!removable
                            || removable_roots.iter().any(|base| root.starts_with(base))))
                    .then(|| id.clone())
                })
        }
        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let parent = CString::new(path.parent()?.as_os_str().as_bytes()).ok()?;
            let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::statfs(parent.as_ptr(), stat.as_mut_ptr()) } != 0 {
                return None;
            }
            let stat = unsafe { stat.assume_init() };
            (stat.f_flags & libc::MNT_LOCAL as u32 != 0).then(|| format!("{:?}", stat.f_fsid))
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::ffi::OsStrExt;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetDriveTypeW(root: *const u16) -> u32;
            }
            let root: PathBuf = path.components().take(2).collect();
            let wide: Vec<_> = root.as_os_str().encode_wide().chain(Some(0)).collect();
            (unsafe { GetDriveTypeW(wide.as_ptr()) } == 3).then(|| root.display().to_string())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn unescape_mount(value: &str) -> String {
    let mut bytes = Vec::new();
    let source = value.as_bytes();
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'\\'
            && i + 3 < source.len()
            && source[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            bytes.push(
                (source[i + 1] - b'0') * 64 + (source[i + 2] - b'0') * 8 + source[i + 3] - b'0',
            );
            i += 4;
        } else {
            bytes.push(source[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf, Entry) {
        // Match canonical picker/folder imports and native watcher event paths
        // (macOS temp directories can be reached via /var and /private/var).
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let path = temp.path().join("generated.mp4");
        fs::write(&path, b"generated local media fixture").unwrap();
        let entry = Entry::new(TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        });
        (temp, path, entry)
    }

    fn check(entry: &mut Entry, now: Instant) -> Option<Change> {
        inspect(entry, &Mounts::read(), now).0
    }

    fn drain_all(worker: &LibraryFiles) -> Vec<Observation> {
        let mut observations = worker.drain().0;
        observations.extend(worker.drain_proofs().into_values());
        observations
    }

    fn wait_for(worker: &LibraryFiles, predicate: impl Fn(&Observation) -> bool) -> Observation {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            for observation in drain_all(worker) {
                if predicate(&observation) {
                    return observation;
                }
                worker.acknowledge(observation);
            }
            assert!(
                Instant::now() < deadline,
                "filesystem worker did not deliver expected observation"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn observation_epochs_ignore_unrelated_changes_but_refresh_reimports() {
        let first = Path::new("first.mp4");
        let other = Path::new("other.mp4");
        let mut epochs = ObservationGenerations::default();
        let original = epochs.observe(first, 1, None, false);
        for generation in 2..100 {
            epochs.observe(other, generation, None, false);
            assert_eq!(epochs.observe(first, 1, None, false), original);
        }
        epochs.refresh(first);
        let imported = epochs.observe(first, 1, None, false);
        assert_ne!(imported, original);
        assert_ne!(
            epochs.observe(first, 1, Some(Path::new("new-poster.png")), false),
            imported
        );
        let poster = epochs.get(first).unwrap();
        assert_ne!(
            epochs.observe(first, 1, Some(Path::new("new-poster.png")), true),
            poster
        );
    }

    #[test]
    fn missing_result_survives_unrelated_ui_revision_churn() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        let mut epochs = ObservationGenerations::default();
        let mut tracked = TrackedFile {
            path: path.clone(),
            generation: 1,
            snapshot: epochs.observe(&path, 1, None, false),
            poster: None,
            incomplete: false,
            location: None,
        };
        worker.sync(vec![tracked.clone()]);
        let present = wait_for(&worker, |o| o.change == Change::Available);
        tracked.location = present.location.clone();
        worker.acknowledge(present);
        fs::remove_file(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut accepted = false;
        while Instant::now() < deadline {
            // This is the UI adapter's per-path epoch; arbitrary history/store
            // revisions no longer replace this path's observation identity.
            tracked.snapshot = epochs.observe(&path, 1, None, false);
            worker.sync(vec![tracked.clone()]);
            for observation in drain_all(&worker) {
                if observation.change == Change::Missing
                    && Some(observation.snapshot) == epochs.get(&path)
                {
                    accepted = true;
                }
                worker.acknowledge(observation);
            }
            if accepted {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        worker.shutdown();
        assert!(
            accepted,
            "unrelated model updates starved missing-file delivery"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mounted_local_removable_roots_are_distinct_from_unmounted_stubs() {
        let mounted = Mounts {
            entries: vec![
                (
                    "/media/generated-drive".into(),
                    "generated local volume".into(),
                    true,
                ),
                ("/".into(), "root filesystem".into(), true),
            ],
        };
        assert_eq!(
            mounted
                .local_mount(Path::new("/media/generated-drive/video.mp4"))
                .as_deref(),
            Some("generated local volume")
        );
        let unmounted = Mounts {
            entries: vec![("/".into(), "root filesystem".into(), true)],
        };
        assert!(
            unmounted
                .local_mount(Path::new("/media/generated-drive/video.mp4"))
                .is_none()
        );
        let network = Mounts {
            entries: vec![(
                "/media/generated-drive".into(),
                "network volume".into(),
                false,
            )],
        };
        assert!(
            network
                .local_mount(Path::new("/media/generated-drive/video.mp4"))
                .is_none()
        );
    }

    #[test]
    fn startup_absence_requires_saved_positive_location() {
        let (_temp, path, mut entry) = fixture();
        let now = Instant::now();
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            check(&mut entry, now),
            Some(Change::Unavailable(_))
        ));
        assert!(matches!(
            check(&mut entry, now + SETTLE),
            Some(Change::Unavailable(_))
        ));
        fs::write(&path, b"returned generated fixture").unwrap();
        assert_eq!(check(&mut entry, now + SETTLE), Some(Change::Available));
        let proof = entry.current_location().expect("local positive location");
        fs::remove_file(&path).unwrap();
        let mut restored = Entry::new(TrackedFile {
            location: Some(proof.clone()),
            ..entry.file.clone()
        });
        assert_eq!(check(&mut restored, now), None);
        assert_eq!(check(&mut restored, now + SETTLE), Some(Change::Missing));
        let mut offline = Entry::new(TrackedFile {
            location: Some(FileLocation {
                volume: "previous mounted volume".into(),
                ..proof
            }),
            ..entry.file
        });
        assert!(matches!(
            check(&mut offline, now),
            Some(Change::Unavailable(_))
        ));
        assert!(matches!(
            check(&mut offline, now + SETTLE),
            Some(Change::Unavailable(_))
        ));
    }

    #[test]
    fn external_unlink_requires_stable_readable_parent_and_two_probes() {
        let (_temp, path, mut entry) = fixture();
        let now = Instant::now();
        assert_eq!(check(&mut entry, now), Some(Change::Available));
        fs::remove_file(path).unwrap();
        assert_eq!(check(&mut entry, now), None);
        assert_eq!(check(&mut entry, now + SETTLE), Some(Change::Missing));
    }

    #[test]
    fn absent_parent_is_unavailable_and_never_pruned() {
        let (temp, path, mut entry) = fixture();
        let now = Instant::now();
        check(&mut entry, now);
        fs::remove_file(path).unwrap();
        fs::remove_dir(temp.path()).unwrap();
        assert!(matches!(
            check(&mut entry, now + FALLBACK),
            Some(Change::Unavailable(_))
        ));
    }

    #[test]
    fn replaced_parent_or_mount_keeps_missing_file_unavailable() {
        let (temp, path, mut entry) = fixture();
        let now = Instant::now();
        check(&mut entry, now);
        let old = temp.path().with_extension("old-parent");
        fs::rename(temp.path(), &old).unwrap();
        fs::create_dir(temp.path()).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
            // Directory identity uses creation time on Windows; distinguish
            // this replacement after recreation despite tunneling/granularity.
            let created = entry.parent.as_ref().unwrap().created.unwrap() + Duration::from_secs(60);
            fs::OpenOptions::new()
                .access_mode(0x100) // FILE_WRITE_ATTRIBUTES
                .custom_flags(0x02000000) // FILE_FLAG_BACKUP_SEMANTICS: open a directory
                .open(temp.path())
                .unwrap()
                .set_times(fs::FileTimes::new().set_created(created))
                .unwrap();
        }
        assert_ne!(
            entry.parent.as_ref().unwrap(),
            &Identity::of(&fs::metadata(temp.path()).unwrap()),
            "replacement fixture must have a distinct parent identity"
        );
        assert!(matches!(
            check(&mut entry, now + FALLBACK),
            Some(Change::Unavailable(_))
        ));
        fs::remove_dir_all(old).unwrap();
        assert!(!path.exists());
        entry.parent = fs::metadata(temp.path()).ok().map(|m| Identity::of(&m));
        entry.mount = Some("previous-mounted-device".into());
        assert!(matches!(
            check(&mut entry, now + FALLBACK),
            Some(Change::Unavailable(_))
        ));
    }

    #[test]
    fn incomplete_transfer_absence_does_not_prune() {
        let (_temp, path, mut entry) = fixture();
        entry.file.incomplete = true;
        fs::remove_file(path).unwrap();
        assert!(matches!(
            check(&mut entry, Instant::now()),
            Some(Change::Unavailable(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_keeps_entry() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        if let Some(path) = std::env::var_os("FLICK_FILES_PERMISSION_FIXTURE") {
            let mut entry = Entry::new(TrackedFile {
                path: path.into(),
                snapshot: 1,
                generation: 1,
                poster: None,
                incomplete: false,
                location: None,
            });
            assert!(matches!(
                check(&mut entry, Instant::now()),
                Some(Change::Unavailable(_))
            ));
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let parent = temp.path().join("private");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("generated.mp4");
        fs::write(&path, b"fixture").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "library_files::tests::permission_denied_keeps_entry",
                "--nocapture",
            ])
            .env("FLICK_FILES_PERMISSION_FIXTURE", &path);
        if unsafe { libc::geteuid() } == 0 {
            command.uid(65534).gid(65534);
        }
        let output = command.output().unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            output.status.success(),
            "permission child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn media_replacement_invalidates_but_metadata_only_change_preserves_cache() {
        let (_temp, path, mut entry) = fixture();
        let now = Instant::now();
        check(&mut entry, now);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(check(&mut entry, now), Some(Change::Available));
        }
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, b"different bytes").unwrap();
        fs::rename(replacement, &path).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTimesExt;
            // Windows identity uses creation time. Set it after the rename so
            // timestamp granularity/file tunneling cannot alias the old file.
            let created = entry.baseline.as_ref().unwrap().identity.created.unwrap()
                + Duration::from_secs(60);
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_created(created))
                .unwrap();
        }
        assert_ne!(
            entry.baseline.as_ref().unwrap().identity,
            Identity::of(&fs::metadata(&path).unwrap()),
            "replacement fixture must have a distinct filesystem identity"
        );
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: true,
                poster: false,
                replaced: true
            })
        );
        // The old baseline stays until the UI acknowledges that generation.
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: true,
                poster: false,
                replaced: true
            })
        );
    }

    #[test]
    fn timestamp_or_length_refresh_does_not_request_playback_release() {
        let (_temp, path, mut entry) = fixture();
        let now = Instant::now();
        check(&mut entry, now);
        let modified = entry.baseline.as_ref().unwrap().modified.unwrap() + Duration::from_secs(2);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: true,
                poster: false,
                replaced: false,
            })
        );
        fs::write(&path, b"tagged generated fixture with a new length").unwrap();
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: true,
                poster: false,
                replaced: false,
            })
        );
    }

    #[test]
    fn startup_missing_bypasses_ten_thousand_initial_proofs() {
        let temp = tempfile::tempdir().unwrap();
        let count = 10_000;
        let mut files = Vec::new();
        for i in 0..count {
            let path = temp.path().join(format!("generated-{i}.mp4"));
            fs::write(&path, b"generated local fixture").unwrap();
            files.push(TrackedFile {
                path,
                snapshot: 1,
                generation: 1,
                poster: None,
                incomplete: false,
                location: None,
            });
        }
        let missing = files[0].path.clone();
        let mut proven = Entry::new(files[0].clone());
        check(&mut proven, Instant::now());
        files[0].location = proven.current_location();
        assert!(files[0].location.is_some());
        fs::remove_file(&missing).unwrap();
        let worker = LibraryFiles::start().unwrap();
        let started = Instant::now();
        worker.sync(files);
        let mut delivered = false;
        let deadline = started + Duration::from_secs(6);
        while Instant::now() < deadline {
            // Deliberately never drain initial positive proofs in this loop.
            for observation in worker.drain().0 {
                assert_ne!(observation.change, Change::Available);
                delivered |= observation.path == missing && observation.change == Change::Missing;
                worker.acknowledge(observation);
            }
            if delivered {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            delivered,
            "missing observation was blocked by initial proofs"
        );
        eprintln!(
            "10k startup: Missing delivered in {:?} without draining initial proofs",
            started.elapsed()
        );
        let deadline = Instant::now() + Duration::from_secs(6);
        while worker.shared.inbox.lock().unwrap().proofs.len() < count - 1 {
            assert!(Instant::now() < deadline, "startup proof sweep stalled");
            thread::sleep(Duration::from_millis(10));
        }
        let proofs = worker.drain_proofs();
        assert_eq!(proofs.len(), count - 1);
        assert!(
            proofs
                .values()
                .all(|proof| proof.change == Change::Available && proof.location.is_some())
        );
        worker.acknowledge_many(proofs.into_values());
        worker.shutdown();
    }

    #[test]
    fn startup_missing_and_recovery_bypass_ten_thousand_offline_entries_at_500ms_tick() {
        let (_temp, missing, mut proven) = fixture();
        check(&mut proven, Instant::now());
        let location = proven
            .current_location()
            .expect("saved local positive proof");
        fs::remove_file(&missing).unwrap();
        let offline = missing.parent().unwrap().join("offline");
        let count = 10_000;
        let mut files: Vec<_> = (0..count)
            .map(|i| TrackedFile {
                path: offline.join(format!("generated-{i}.mp4")),
                snapshot: 1,
                generation: 1,
                poster: None,
                incomplete: false,
                location: None,
            })
            .collect();
        let recovering = files[0].path.clone();
        files.push(TrackedFile {
            location: Some(location),
            ..proven.file
        });
        let worker = LibraryFiles::start().unwrap();
        let started = Instant::now();
        worker.sync(files);
        let mut seen = HashSet::new();
        loop {
            // Match the real library tick, including the first drain.
            thread::sleep(Duration::from_millis(500));
            {
                let inbox = worker.shared.inbox.lock().unwrap();
                assert!(inbox.results.len() <= RESULT_LIMIT);
                assert!(inbox.unavailable.len() <= RESULT_LIMIT);
            }
            let mut got_missing = false;
            let mut offline_started = false;
            for result in worker.drain().0 {
                if matches!(result.change, Change::Unavailable(_)) {
                    offline_started = true;
                } else {
                    assert!(!offline_started, "urgent results must drain first");
                }
                got_missing |= result.path == missing && result.change == Change::Missing;
                seen.insert(result.path.clone());
                worker.acknowledge(result);
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "Missing starved behind offline startup results"
            );
            if got_missing {
                break;
            }
        }
        eprintln!(
            "10k offline startup at 500ms tick: Missing delivered in {:?}",
            started.elapsed()
        );
        fs::create_dir(&offline).unwrap();
        fs::write(&recovering, b"generated returned fixture").unwrap();
        worker.reconcile();
        let started = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(500));
            let mut recovered = false;
            for result in worker.drain().0 {
                recovered |= result.path == recovering && result.change == Change::Available;
                seen.insert(result.path.clone());
                worker.acknowledge(result);
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "recovery starved behind offline startup results"
            );
            if recovered {
                break;
            }
        }
        eprintln!(
            "10k offline recovery at 500ms tick: Available delivered in {:?}",
            started.elapsed()
        );
        // Finish draining faster to prove the segregated background lane has
        // retained every path, rather than dropping displaced offline state.
        let deadline = Instant::now() + Duration::from_secs(8);
        while seen.len() < count + 1 {
            for result in drain_all(&worker) {
                seen.insert(result.path.clone());
                worker.acknowledge(result);
            }
            assert!(
                Instant::now() < deadline,
                "lost offline state: {} of {} paths",
                seen.len(),
                count + 1
            );
            thread::sleep(Duration::from_millis(10));
        }
        worker.shutdown();
    }

    #[test]
    fn pending_proofs_are_coalesced_and_acknowledged_location_is_cached() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let proof = wait_for(&worker, |observation| {
            observation.change == Change::Available
        });
        for _ in 0..10 {
            worker.reconcile();
        }
        thread::sleep(Duration::from_millis(100));
        assert!(worker.drain().0.is_empty());
        assert!(
            worker.drain_proofs().is_empty(),
            "unacknowledged identical proof was resent"
        );
        worker.acknowledge(proof);
        worker.reconcile();
        thread::sleep(Duration::from_millis(100));
        assert!(
            drain_all(&worker).is_empty(),
            "cached positive proof was resent"
        );
        worker.shutdown();
    }

    #[test]
    fn deltas_reindex_only_changed_paths_and_preserve_unrelated_missing_proof() {
        let (temp, path, _entry) = fixture();
        let sibling = temp.path().join("sibling.mp4");
        fs::write(&sibling, b"generated sibling fixture").unwrap();
        let worker = LibraryFiles::start().unwrap();
        let file = |path: PathBuf, snapshot| TrackedFile {
            path,
            snapshot,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        };
        worker.sync(vec![file(path.clone(), 1), file(sibling.clone(), 1)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = HashSet::new();
        while seen.len() < 2 {
            for observation in drain_all(&worker) {
                seen.insert(observation.path.clone());
                worker.acknowledge(observation);
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let poster = temp.path().join("sibling.png");
        fs::write(&poster, b"generated poster fixture").unwrap();
        worker.update(
            vec![TrackedFile {
                poster: Some(poster.clone()),
                ..file(sibling.clone(), 2)
            }],
            vec![],
        );
        fs::remove_file(&path).unwrap();
        let missing = wait_for(&worker, |observation| observation.change == Change::Missing);
        assert_eq!(missing.path, path);
        assert_eq!(missing.snapshot, 1);
        {
            let index = worker.shared.index.lock().unwrap();
            assert_eq!(index.files.get(&poster).unwrap(), &vec![sibling.clone()]);
            assert_eq!(index.files.len(), 3);
        }
        worker.update(vec![], vec![path.clone()]);
        assert!(!is_current(&worker.shared, &path, 1));
        assert!(is_current(&worker.shared, &sibling, 1));
        worker.shutdown();
    }

    #[test]
    fn focus_sweeps_are_rate_limited_and_do_not_restart_the_active_sweep() {
        let mut focus = FocusSweeps::default();
        let now = Instant::now();
        assert!(focus.accept(now));
        for elapsed in 0..500 {
            assert!(!focus.accept(now + Duration::from_millis(elapsed)));
        }
        assert!(focus.accept(now + FOCUS_INTERVAL));
        let mut work = Work {
            cursor: Some(4321),
            ..Work::default()
        };
        work.full();
        assert_eq!(work.cursor, Some(4321));
    }

    #[test]
    fn failed_native_watcher_is_reinstalled_on_focus() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let proof = wait_for(&worker, |observation| {
            observation.change == Change::Available
        });
        worker.acknowledge(proof);
        collect_event(
            &worker.shared,
            Err(notify::Error::generic("generated watcher failure")),
        );
        worker.reconcile_focus();
        // Let the forced error/focus sweep finish while the file still exists.
        thread::sleep(SETTLE * 2);
        fs::remove_file(&path).unwrap();
        let missing = wait_for(&worker, |observation| observation.change == Change::Missing);
        assert_eq!(missing.path, path);
        worker.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_trash_never_overwrites_colliding_data_and_encodes_non_utf8_paths() {
        use std::os::unix::{ffi::OsStringExt, fs::MetadataExt};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Trash");
        let path = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"generated \xff.mp4".to_vec()));
        fs::write(&path, b"selected generated fixture").unwrap();
        let device = fs::metadata(&path).unwrap().dev();
        linux_trash_directories(&root, device, unsafe { libc::geteuid() }).unwrap();
        let collision = root.join("files").join(path.file_name().unwrap());
        fs::write(&collision, b"previously trashed generated fixture").unwrap();
        linux_trash_move(&path, &root, device).unwrap();
        assert!(!path.exists());
        assert_eq!(
            fs::read(collision).unwrap(),
            b"previously trashed generated fixture"
        );
        assert_eq!(fs::read_dir(root.join("files")).unwrap().count(), 2);
        let info: Vec<_> = fs::read_dir(root.join("info"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(info.len(), 1);
        let info = fs::read_to_string(&info[0]).unwrap();
        assert!(info.contains("generated%20%FF.mp4"));
        assert!(info.contains("DeletionDate="));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_trash_refuses_another_volume_and_preserves_source() {
        use std::os::unix::fs::MetadataExt;
        let (_temp, path, _entry) = fixture();
        let other = tempfile::tempdir_in("/dev/shm").expect("generated tmpfs Trash fixture");
        let root = other.path().join("Trash");
        let device = fs::metadata(&path).unwrap().dev();
        let other_device = fs::metadata(other.path()).unwrap().dev();
        assert_ne!(
            device, other_device,
            "this Linux fixture needs /dev/shm on another device"
        );
        linux_trash_directories(&root, other_device, unsafe { libc::geteuid() }).unwrap();
        let error = linux_trash_move(&path, &root, device).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::CrossesDevices);
        assert_eq!(fs::read(&path).unwrap(), b"generated local media fixture");
        assert_eq!(fs::read_dir(root.join("files")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("info")).unwrap().count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_per_user_trash_accepts_synthetic_modes_but_rejects_symlinks_and_wrong_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::geteuid() };
        let device = fs::metadata(temp.path()).unwrap().dev();
        let root = temp.path().join(format!(".Trash-{uid}"));
        linux_trash_directories(&root, device, uid).unwrap();
        for mode in [0o755, 0o775] {
            for path in [root.clone(), root.join("files"), root.join("info")] {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            }
            linux_trash_directories(&root, device, uid).unwrap();
        }
        let error = linux_trash_directories(&root, device, uid.wrapping_add(1)).unwrap_err();
        assert!(error.to_string().contains("must be owned by you"));
        assert!(error.to_string().contains(&root.display().to_string()));
        fs::remove_dir(root.join("files")).unwrap();
        symlink(temp.path(), root.join("files")).unwrap();
        assert!(linux_trash_directories(&root, device, uid).is_err());
        let shared = temp.path().join(".Trash");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(linux_shared_trash(&shared, device, uid).is_err());
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        linux_shared_trash(&shared, device, uid).unwrap();
        assert!(linux_shared_trash(&shared, device.wrapping_add(1), uid).is_err());
        fs::remove_dir(&shared).unwrap();
        symlink(temp.path(), &shared).unwrap();
        assert!(linux_shared_trash(&shared, device, uid).is_err());
    }

    #[test]
    fn poster_deletion_and_replacement_are_independent_of_media_dimensions() {
        let (temp, _path, mut entry) = fixture();
        let poster = temp.path().join("generated-poster.png");
        fs::write(&poster, b"generated poster fixture").unwrap();
        entry.file.poster = Some(poster.clone());
        let now = Instant::now();
        assert_eq!(check(&mut entry, now), Some(Change::Available));
        let replacement = temp.path().join("replacement.png");
        fs::write(&replacement, b"different fixture").unwrap();
        fs::rename(replacement, &poster).unwrap();
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: false,
                poster: true,
                replaced: false
            })
        );
        fs::remove_file(&poster).unwrap();
        assert_eq!(
            check(&mut entry, now),
            Some(Change::Metadata {
                media: false,
                poster: true,
                replaced: false
            })
        );
    }

    #[test]
    fn missing_cache_parent_preserves_cached_metadata() {
        let (temp, _path, mut entry) = fixture();
        entry.file.poster = Some(temp.path().join("offline-cache/poster.png"));
        assert_eq!(check(&mut entry, Instant::now()), Some(Change::Available));
    }

    #[test]
    fn event_mailbox_filters_coalesces_and_bounds_known_paths() {
        let shared = Shared::default();
        let paths: Vec<_> = (0..EVENT_LIMIT + 100)
            .map(|i| PathBuf::from(format!("/generated/{i}.mp4")))
            .collect();
        for path in &paths {
            shared
                .index
                .lock()
                .unwrap()
                .files
                .insert(path.clone(), vec![path.clone()]);
        }
        let kind = EventKind::Modify(notify::event::ModifyKind::Any);
        for _ in 0..100 {
            collect_event(
                &shared,
                Ok(notify::Event::new(kind).add_path(paths[0].clone())),
            );
        }
        assert_eq!(shared.inbox.lock().unwrap().events.len(), 1);
        collect_event(
            &shared,
            Ok(notify::Event::new(kind).add_path("/unrelated/file".into())),
        );
        assert_eq!(shared.inbox.lock().unwrap().events.len(), 1);
        for path in paths {
            collect_event(&shared, Ok(notify::Event::new(kind).add_path(path)));
        }
        let inbox = shared.inbox.lock().unwrap();
        assert_eq!(inbox.events.len(), EVENT_LIMIT);
        assert!(inbox.reconcile);
    }

    #[test]
    fn worker_batches_and_dirty_queue_are_bounded() {
        let mut work = Work::default();
        let now = Instant::now();
        work.order = (0..10_000)
            .map(|i| PathBuf::from(format!("/generated/{i}")))
            .collect();
        for path in work.order.clone() {
            work.queue(path, now);
        }
        assert_eq!(work.due.len(), EVENT_LIMIT);
        assert!(work.cursor.is_some());
        assert_eq!(work.batch(now).len(), BATCH);
    }

    #[test]
    fn removable_network_and_unknown_mounts_are_conservative() {
        let mounts = Mounts::read();
        for path in [
            "/media/device/file.mp4",
            "/mnt/device/file.mp4",
            "/Volumes/Drive/file.mp4",
            "/net/server/file.mp4",
        ] {
            assert!(mounts.local_mount(Path::new(path)).is_none());
        }
        assert!(mounts.local_mount(Path::new("relative/file.mp4")).is_none());
        #[cfg(target_os = "linux")]
        {
            let mounts = Mounts {
                entries: vec![(PathBuf::from("/"), "remote".into(), false)],
            };
            assert!(mounts.local_mount(Path::new("/home/file.mp4")).is_none());
            assert_eq!(
                unescape_mount("/space\\040folder\\134name"),
                "/space folder\\name"
            );
        }
    }

    #[test]
    fn native_watcher_detects_external_unlink_without_requested_rescan() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let present = wait_for(&worker, |o| o.change == Change::Available);
        worker.acknowledge(present);
        fs::remove_file(&path).unwrap();
        let missing = wait_for(&worker, |o| o.change == Change::Missing);
        assert_eq!(missing.generation, 1);
        worker.shutdown();
    }

    #[test]
    fn stale_trash_ticket_cannot_delete_reimported_generation_or_replaced_file() {
        let (_temp, path, _entry) = fixture();
        let ticket = prepare_trash(&path, 1).unwrap();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 2,
            generation: 2,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let result = worker.trash(ticket.clone()).blocking_recv().unwrap();
        assert!(result.is_err());
        assert!(path.exists());
        worker.shutdown();
        let replacement = path.with_extension("new");
        fs::write(&replacement, b"replacement fixture").unwrap();
        fs::rename(replacement, &path).unwrap();
        assert!(trash_checked(&ticket).unwrap_err().contains("changed"));
        assert_eq!(fs::read(path).unwrap(), b"replacement fixture");
    }

    #[test]
    fn trash_refuses_directories_and_symlinks_without_touching_targets() {
        let (temp, path, _entry) = fixture();
        assert!(prepare_trash(temp.path(), 1).is_err());
        #[cfg(unix)]
        {
            let link = temp.path().join("link.mp4");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(prepare_trash(&link, 1).is_err());
        }
        assert!(path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn isolated_xdg_trash_moves_only_selected_generated_fixture() {
        if let Some(root) = std::env::var_os("FLICK_FILES_TRASH_FIXTURE") {
            let root = PathBuf::from(root);
            let path = root.join("selected.mp4");
            let sibling = root.join("sibling.mp4");
            fs::write(&path, b"generated selected fixture").unwrap();
            fs::write(&sibling, b"generated sibling fixture").unwrap();
            let ticket = prepare_trash(&path, 1).unwrap();
            trash_checked(&ticket).unwrap();
            assert!(!path.exists());
            assert_eq!(fs::read(&sibling).unwrap(), b"generated sibling fixture");
            let trash_root = root.join("xdg/Trash");
            let files: Vec<_> = fs::read_dir(trash_root.join("files"))
                .unwrap()
                .map(|f| f.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1);
            assert_eq!(fs::read(&files[0]).unwrap(), b"generated selected fixture");
            assert_eq!(fs::read_dir(trash_root.join("info")).unwrap().count(), 1);
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "library_files::tests::isolated_xdg_trash_moves_only_selected_generated_fixture",
                "--nocapture",
            ])
            .env("FLICK_FILES_TRASH_FIXTURE", temp.path())
            .env("XDG_DATA_HOME", temp.path().join("xdg"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated Trash child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn isolated_native_trash_accepts_owned_775_home_without_chmod() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(root) = std::env::var_os("FLICK_FILES_TRASH_775_FIXTURE") {
            let root = PathBuf::from(root);
            let trash = root.join("xdg/Trash");
            let path = root.join("selected.mp4");
            fs::write(&path, b"generated selected fixture").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            for mode in [0o755, 0o775] {
                for directory in [trash.clone(), trash.join("files"), trash.join("info")] {
                    fs::create_dir_all(&directory).unwrap();
                    fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
                }
                trash_checked(&prepare_trash(&path, 1).unwrap()).unwrap();
                assert!(!path.exists());
                let moved = trash.join("files/selected.mp4");
                assert_eq!(fs::read(&moved).unwrap(), b"generated selected fixture");
                assert_eq!(fs::metadata(&moved).unwrap().mode() & 0o777, 0o640);
                assert!(
                    fs::read_to_string(trash.join("info/selected.mp4.trashinfo"))
                        .unwrap()
                        .contains(&format!("Path={}", path.display()))
                );
                for directory in [trash.clone(), trash.join("files"), trash.join("info")] {
                    assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, mode);
                }
                // Restore only this generated fixture for the next mode.
                fs::rename(&moved, &path).unwrap();
                fs::remove_file(trash.join("info/selected.mp4.trashinfo")).unwrap();
            }
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "library_files::tests::isolated_native_trash_accepts_owned_775_home_without_chmod",
                "--nocapture",
            ])
            .env("FLICK_FILES_TRASH_775_FIXTURE", temp.path())
            .env("XDG_DATA_HOME", temp.path().join("xdg"))
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated 0755/0775 Trash child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn reimported_generation_rechecks_stale_missing_before_any_new_result() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 1,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let present = wait_for(&worker, |o| o.change == Change::Available);
        worker.acknowledge(present);
        fs::remove_file(&path).unwrap();
        let stale = wait_for(&worker, |o| o.change == Change::Missing);
        assert_eq!(stale.generation, 1);
        fs::write(&path, b"explicitly reimported fixture").unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 2,
            generation: 2,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        // Acknowledging late generation 1 must not adopt its absent baseline.
        worker.acknowledge(stale);
        let fresh = wait_for(&worker, |o| o.generation == 2);
        assert!(matches!(
            fresh.change,
            Change::Available | Change::Metadata { media: true, .. }
        ));
        assert!(path.exists());
        worker.acknowledge(fresh);
        worker.shutdown();
    }

    #[test]
    fn result_backpressure_is_bounded_and_every_known_path_eventually_delivers() {
        let temp = tempfile::tempdir().unwrap();
        let count = RESULT_LIMIT + BATCH * 4;
        let files: Vec<_> = (0..count)
            .map(|i| {
                let path = temp.path().join(format!("generated-{i}.mp4"));
                fs::write(&path, b"fixture").unwrap();
                TrackedFile {
                    path,
                    snapshot: 1,
                    generation: 1,
                    poster: None,
                    incomplete: false,
                    location: None,
                }
            })
            .collect();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(files);
        thread::sleep(Duration::from_millis(550));
        assert!(worker.shared.inbox.lock().unwrap().results.len() <= RESULT_LIMIT);
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut seen = HashSet::new();
        while seen.len() < count {
            for result in drain_all(&worker) {
                assert_eq!(result.change, Change::Available);
                seen.insert(result.path.clone());
                worker.acknowledge(result);
            }
            assert!(
                Instant::now() < deadline,
                "lost queued path under result backpressure: {} of {count}",
                seen.len()
            );
            thread::sleep(Duration::from_millis(10));
        }
        worker.shutdown();
    }

    #[test]
    fn prepare_and_focus_reconcile_are_asynchronous_and_generation_checked() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 8,
            generation: 8,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        worker.prepare(path.clone(), 8, 42);
        worker.reconcile();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let (observations, prepared) = worker.drain();
            for observation in observations {
                worker.acknowledge(observation);
            }
            if let Some(prepared) = prepared.into_iter().next() {
                assert_eq!(prepared.request, 42);
                assert_eq!(prepared.generation, 8);
                assert_eq!(prepared.result.unwrap().path, path);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        worker.shutdown();
    }

    #[test]
    fn unrelated_posters_and_history_do_not_invalidate_a_paths_delete_generation() {
        let mut generations = FileGenerations::default();
        let selected = Path::new("/generated/selected.mp4");
        let other = Path::new("/generated/other.mp4");
        let selected_generation = generations.observe(selected, None);
        generations.observe(other, None);
        // Metadata/history snapshots repeatedly observe the same identities.
        for _revision in 0..100 {
            assert_eq!(generations.observe(selected, None), selected_generation);
            generations.observe(other, None);
        }
        generations.remove(other);
        assert_eq!(generations.get(selected), Some(selected_generation));
    }

    #[test]
    fn removal_reimport_and_identity_rebind_change_only_the_affected_generation() {
        let mut generations = FileGenerations::default();
        let a = Path::new("/generated/a.mp4");
        let b = Path::new("/generated/b.mp4");
        let old = generations.observe(a, Some(("torrent-a", 1)));
        let unaffected = generations.observe(b, None);
        generations.remove(a);
        let restored = generations.observe(a, Some(("torrent-a", 1)));
        assert_ne!(restored, old);
        let rebound = generations.observe(a, Some(("torrent-b", 1)));
        assert_ne!(rebound, restored);
        assert_eq!(generations.get(b), Some(unaffected));
        generations.retain(|path| path == b);
        assert_eq!(generations.get(a), None);
        assert_eq!(generations.get(b), Some(unaffected));
    }

    #[test]
    fn new_metadata_snapshot_rechecks_pending_absence_without_changing_delete_identity() {
        let (_temp, path, _entry) = fixture();
        let worker = LibraryFiles::start().unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 1,
            generation: 7,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let present = wait_for(&worker, |o| o.change == Change::Available);
        worker.acknowledge(present);
        fs::remove_file(&path).unwrap();
        let old = wait_for(&worker, |o| o.change == Change::Missing);
        assert_eq!(old.snapshot, 1);
        fs::write(&path, b"restored before old callback").unwrap();
        worker.sync(vec![TrackedFile {
            path: path.clone(),
            snapshot: 2,
            generation: 7,
            poster: None,
            incomplete: false,
            location: None,
        }]);
        let fresh = wait_for(&worker, |o| o.snapshot == 2);
        assert_eq!(fresh.generation, 7);
        assert_ne!(fresh.change, Change::Missing);
        worker.acknowledge(fresh);
        worker.shutdown();
    }

    #[test]
    fn readable_returning_volume_recovers_but_absence_never_adopts_new_parent() {
        let (_temp, _path, mut entry) = fixture();
        let now = Instant::now();
        check(&mut entry, now);
        entry.mount = Some("disconnected-old-volume".into());
        // An actually present regular file recovers instead of being kept
        // unavailable forever merely because the previous mount changed.
        assert_eq!(check(&mut entry, now), Some(Change::Available));
        fs::remove_file(&entry.file.path).unwrap();
        entry.mount = Some("disconnected-old-volume".into());
        assert!(matches!(
            check(&mut entry, now + FALLBACK),
            Some(Change::Unavailable(_))
        ));
    }
}
