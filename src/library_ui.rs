//! Library pages, navigation, downloads, and sharing.

use super::{
    chrome_ui::{group, notice, row, section_title, thumbnail, traffic_light_space},
    *,
};
use crate::store::Sort;
use flick::library::format_bytes;
use gpui::{
    AnyElement, Asset, AssetLogger, FontWeight, ImageAssetLoader, ImageCache, Resource,
    ScrollHandle, ScrollStrategy, UniformListScrollHandle, anchored, deferred, linear_color_stop,
    linear_gradient, point, uniform_list,
};

/// Derived views and retained scroll/image state. No GPUI entity is required at construction.
#[derive(Default)]
pub(super) struct LibraryUiState {
    videos: RefCell<Option<VideoQuery>>,
    history: RefCell<Option<(u64, Rc<[PathBuf]>)>>,
    episodes: RefCell<Option<EpisodeQuery>>,
    shows: RefCell<Option<ShowQuery>>,
    show_labels: RefCell<Option<(u64, Vec<String>)>>,
    show_stats: RefCell<Option<(u64, HashMap<ElementId, usize>)>>,
    sections: RefCell<HashMap<&'static str, VirtualSection>>,
    scrolls: RefCell<HashMap<String, UniformListScrollHandle>>,
    page_scroll: ScrollHandle,
    search_scroll: ScrollHandle,
    pub(super) posters: Option<Entity<PosterCache>>,
    visible_posters: RefCell<HashSet<PathBuf>>,
}

struct VideoQuery {
    revision: u64,
    query: String,
    filter: VideoFilter,
    sort: Sort,
    paths: Rc<[PathBuf]>,
}

struct EpisodeQuery {
    revision: u64,
    key: String,
    episodes: Rc<[(flick::library::Episode, PathBuf)]>,
    paths: Rc<[PathBuf]>,
}

struct ShowQuery {
    revision: u64,
    query: String,
    indices: Rc<[usize]>,
    ids: Rc<[ElementId]>,
}

#[derive(Clone)]
struct VirtualSection {
    ids: Rc<[ElementId]>,
    paths: Option<Rc<[PathBuf]>>,
    columns: usize,
    scroll: UniformListScrollHandle,
}

impl LibraryUiState {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            posters: Some(cx.new(PosterCache::new)),
            ..Self::default()
        }
    }

    pub(super) fn begin_poster_frame(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(posters) = &self.posters {
            posters.update(cx, |cache, cx| cache.begin_frame(window, cx));
        }
    }

    /// Most recently requested viewport; useful for prioritizing native poster extraction.
    pub(super) fn visible_poster_paths(&self) -> Vec<PathBuf> {
        self.visible_posters.borrow().iter().cloned().collect()
    }

    pub(super) fn record_visible_poster(&self, path: &std::path::Path) {
        self.visible_posters.borrow_mut().insert(path.to_owned());
    }

    fn begin_visible_posters(&self) {
        self.visible_posters.borrow_mut().clear();
    }

    /// Drops derived data while retaining navigation, scroll positions and poster cache.
    pub(super) fn invalidate(&self) {
        self.videos.borrow_mut().take();
        self.history.borrow_mut().take();
        self.episodes.borrow_mut().take();
        self.shows.borrow_mut().take();
        self.show_labels.borrow_mut().take();
        self.show_stats.borrow_mut().take();
    }

    fn video_paths(
        &self,
        data: &crate::store::Settings,
        query: &str,
        filter: VideoFilter,
    ) -> Rc<[PathBuf]> {
        let revision = data.revision();
        let query = query.to_lowercase();
        let sort = data.library_view.sort;
        let mut cache = self.videos.borrow_mut();
        if let Some(cached) = cache.as_ref()
            && cached.revision == revision
            && cached.query == query
            && cached.sort == sort
            && cached.filter == filter
        {
            return cached.paths.clone();
        }
        let paths = data
            .sorted_library(sort)
            .into_iter()
            .filter(|video| {
                data.normalized_title(&video.path)
                    .is_some_and(|title| title.contains(&query))
            })
            .filter(|video| match filter {
                VideoFilter::All => true,
                VideoFilter::Unwatched => !data.watched(&video.path),
                VideoFilter::InProgress => data.resume_position(&video.path) > 0.,
            })
            .map(|video| video.path.clone())
            .collect::<Rc<[PathBuf]>>();
        *cache = Some(VideoQuery {
            revision,
            query,
            sort,
            filter,
            paths: paths.clone(),
        });
        paths
    }
    fn show_unwatched(&self, data: &crate::store::Settings, show: &flick::library::Show) -> usize {
        let revision = data.revision();
        let mut stats = self.show_stats.borrow_mut();
        if !stats
            .as_ref()
            .is_some_and(|(old_revision, _)| *old_revision == revision)
        {
            *stats = Some((
                revision,
                data.shows()
                    .iter()
                    .map(|show| {
                        (
                            show_id(show),
                            show.episodes
                                .iter()
                                .filter(|(_, path)| !data.watched(path))
                                .count(),
                        )
                    })
                    .collect(),
            ));
        }
        stats
            .as_ref()
            .unwrap()
            .1
            .get(&show_id(show))
            .copied()
            .unwrap_or(0)
    }

    fn scroll(&self, key: &str) -> UniformListScrollHandle {
        self.scrolls
            .borrow_mut()
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    fn register(
        &self,
        section: &'static str,
        ids: Rc<[ElementId]>,
        columns: usize,
        scroll: UniformListScrollHandle,
    ) {
        self.sections.borrow_mut().insert(
            section,
            VirtualSection {
                ids,
                paths: None,
                columns,
                scroll,
            },
        );
    }
}

const POSTER_CACHE_CAPACITY: usize = 96;

/// The decoder task belongs to its cache entry: eviction cancels pending work,
/// and loaded textures are explicitly removed from every GPUI window.
pub(super) struct PosterCache {
    entries: HashMap<Resource, PosterEntry>,
    lru: PosterLru,
    weak: gpui::WeakEntity<Self>,
    generation: u64,
    protected: HashSet<Resource>,
    requested: HashSet<Resource>,
}

enum PosterEntry {
    Loading {
        generation: u64,
        _task: gpui::Task<()>,
    },
    Loaded(Result<Arc<RenderImage>, gpui::ImageCacheError>),
}

#[derive(Default)]
struct PosterLru(std::collections::VecDeque<Resource>);

impl PosterLru {
    #[cfg(test)]
    fn touch(&mut self, resource: &Resource) -> Option<Resource> {
        self.touch_protected(resource, &HashSet::new())
    }

    fn touch_protected(
        &mut self,
        resource: &Resource,
        protected: &HashSet<Resource>,
    ) -> Option<Resource> {
        let evicted = if let Some(index) = self.0.iter().position(|entry| entry == resource) {
            self.0.remove(index);
            None
        } else if self.0.len() >= POSTER_CACHE_CAPACITY {
            // A viewport can exceed the warm-cache budget on a large display.
            // Never evict a poster still needed by this or the preceding frame.
            self.0
                .iter()
                .position(|entry| !protected.contains(entry))
                .and_then(|index| self.0.remove(index))
        } else {
            None
        };
        self.0.push_back(resource.clone());
        evicted
    }
}

impl PosterCache {
    fn new(cx: &mut Context<Self>) -> Self {
        cx.on_release(|cache, cx| {
            for (_, entry) in std::mem::take(&mut cache.entries) {
                if let PosterEntry::Loaded(Ok(image)) = entry {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        Self {
            entries: HashMap::new(),
            lru: PosterLru::default(),
            weak: cx.entity().downgrade(),
            generation: 0,
            protected: HashSet::new(),
            requested: HashSet::new(),
        }
    }

    fn begin_frame(&mut self, window: &mut Window, cx: &mut App) {
        self.protected = std::mem::take(&mut self.requested);
        let capacity = POSTER_CACHE_CAPACITY.max(self.protected.len());
        while self.lru.0.len() > capacity {
            let Some(index) = self
                .lru
                .0
                .iter()
                .position(|entry| !self.protected.contains(entry))
            else {
                break;
            };
            let resource = self.lru.0[index].clone();
            self.remove(&resource, window, cx);
        }
    }

    fn remove(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) {
        if let Some(PosterEntry::Loaded(Ok(image))) = self.entries.remove(resource) {
            cx.drop_image(image, Some(window));
        }
        self.lru.0.retain(|entry| entry != resource);
    }
}

impl ImageCache for PosterCache {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, gpui::ImageCacheError>> {
        self.requested.insert(resource.clone());
        self.protected.insert(resource.clone());
        if let Some(evicted) = self.lru.touch_protected(resource, &self.protected) {
            self.remove(&evicted, window, cx);
        }
        if let Some(entry) = self.entries.get(resource) {
            return match entry {
                PosterEntry::Loaded(result) => Some(result.clone()),
                PosterEntry::Loading { .. } => None,
            };
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let future = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
        let decode = cx.background_executor().spawn(future);
        let weak = self.weak.clone();
        let resource = resource.clone();
        let loaded_resource = resource.clone();
        let view = window.current_view();
        let task = window.spawn(cx, async move |cx| {
            let result = decode.await;
            let _ = weak.update(cx, |cache, _| {
                if matches!(cache.entries.get(&loaded_resource), Some(PosterEntry::Loading { generation: current, .. }) if *current == generation) {
                    cache.entries.insert(loaded_resource, PosterEntry::Loaded(result));
                }
            });
            cx.on_next_frame(move |_, cx| cx.notify(view));
        });
        self.entries.insert(
            resource,
            PosterEntry::Loading {
                generation,
                _task: task,
            },
        );
        None
    }
}

fn video_id(section: &'static str, path: &std::path::Path) -> ElementId {
    ElementId::from((section, gpui::hash(&path)))
}

fn show_id(show: &flick::library::Show) -> ElementId {
    ElementId::from((
        "show",
        gpui::hash(
            &show
                .episodes
                .first()
                .map(|(episode, _)| episode.show_key())
                .unwrap_or_else(|| show.title.clone()),
        ),
    ))
}

fn first_visible_row(scroll: &UniformListScrollHandle, row_height: Pixels, rows: usize) -> usize {
    let state = scroll.0.borrow();
    if let Some(target) = &state.deferred_scroll_to_item {
        return target.item_index.min(rows.saturating_sub(1));
    }
    let top = -f32::from(state.base_handle.offset().y);
    ((top / f32::from(row_height).max(1.)).floor().max(0.) as usize).min(rows.saturating_sub(1))
}

fn action_id(section: &'static str, path: &std::path::Path, slot: FocusSlot) -> ElementId {
    SharedString::from(format!("{section}:{slot:?}:{}", gpui::hash(&path))).into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusSlot {
    Primary,
    Remove,
    Delete,
}

fn tab_target(
    index: usize,
    count: usize,
    slot: FocusSlot,
    reverse: bool,
    actions: bool,
) -> Option<(usize, FocusSlot)> {
    use FocusSlot::*;
    match (slot, reverse, actions) {
        (Primary, false, true) => Some((index, Remove)),
        (Remove, false, true) => Some((index, Delete)),
        (Delete, true, true) => Some((index, Remove)),
        (Remove, true, true) => Some((index, Primary)),
        (_, true, _) => index
            .checked_sub(1)
            .map(|index| (index, if actions { Delete } else { Primary })),
        (_, false, _) => (index + 1 < count).then_some((index + 1, Primary)),
    }
}

/// Half-open card range covered by a virtual grid row, including a partial last row.
fn grid_row_range(row: usize, columns: usize, count: usize) -> std::ops::Range<usize> {
    let start = row.saturating_mul(columns).min(count);
    start..start.saturating_add(columns).min(count)
}

fn validate_library_files(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<String>) {
    let mut videos = Vec::with_capacity(paths.len());
    let mut errors = Vec::new();
    for path in paths {
        match flick::library::canonical_video(&path) {
            Ok(path) => videos.push(path),
            Err(error) => errors.push(format!("Could not add {}: {error:#}", path.display())),
        }
    }
    (videos, errors)
}

impl Flick {
    /// Called when a poster artifact is replaced/deleted, if its path is known.
    pub(super) fn evict_library_poster(
        &mut self,
        poster: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(cache) = &self.library_ui_state.posters {
            let resource = Resource::from(poster.to_owned());
            cache.update(cx, |cache, cx| cache.remove(&resource, window, cx));
        }
    }

    /// Validate dropped/open-with batches on the executor before applying one model change.
    pub(super) fn import_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.importing = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (videos, torrents, errors) = cx
                .background_executor()
                .spawn(async move {
                    let mut videos = Vec::new();
                    let mut torrents = Vec::new();
                    let mut errors = Vec::new();
                    for path in paths {
                        if path
                            .extension()
                            .is_some_and(|v| v.eq_ignore_ascii_case("torrent"))
                        {
                            torrents.push(path);
                        } else if path.is_dir() {
                            match flick::library::scan_folder(&path) {
                                Ok(found) => videos.extend(found),
                                Err(error) => errors
                                    .push(format!("Could not add {}: {error:#}", path.display())),
                            }
                        } else {
                            match flick::library::canonical_video(&path) {
                                Ok(path) => videos.push(path),
                                Err(error) => errors
                                    .push(format!("Could not add {}: {error:#}", path.display())),
                            }
                        }
                    }
                    (videos, torrents, errors)
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.importing = false;
                this.library_files_restored(videos.iter());
                this.store.data.restore_videos(videos);
                this.library_changed(cx);
                this.resolve_imported_torrents(torrents, errors, cx);
                this.persist();
                cx.notify();
            });
        })
        .detach();
    }

    /// Keep the single metadata request/chooser flow and report every file not admitted.
    fn resolve_imported_torrents(
        &mut self,
        torrents: Vec<PathBuf>,
        mut errors: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let mut not_started = torrents.len();
        if !self.resolving
            && !self.quitting
            && let Some(path) = torrents.into_iter().next()
        {
            self.resolve(flick::torrent::TorrentInput::File(path), cx);
            if self.resolving {
                not_started -= 1;
            }
            // Resolve clears the previous error; preserve any new admission failure.
            if let Some(error) = self.error.take() {
                errors.push(error);
            }
        }
        if not_started > 0 {
            let files = if not_started == 1 {
                "file was"
            } else {
                "files were"
            };
            errors.push(format!(
                "{not_started} torrent {files} not started. Open torrent files one at a time and retry when Flick is ready."
            ));
        }
        if !errors.is_empty() {
            self.error = Some(errors.join("\n"));
        }
    }

    pub(super) fn send(&mut self, command: ServiceCommand, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if let ServiceCommand::Remove { id, delete_files } = &command {
            if !self.transfers_known
                || self.downloads.iter().any(|download| {
                    &download.infohash == id
                        && matches!(download.state, flick::torrent::TransferState::Checking)
                })
            {
                self.error = Some(
                    "Wait for downloads to finish checking before removing this transfer.".into(),
                );
                cx.notify();
                return;
            }
            if !delete_files
                && self.downloads.iter().any(|download| {
                    &download.infohash == id
                        && matches!(download.state, flick::torrent::TransferState::Error)
                })
            {
                self.error = Some(
                    "Resume this transfer in Downloads so its files can be verified, then retry Remove · Keep Files. Your files and their transfer were kept.".into(),
                );
                cx.notify();
                return;
            }
            // Keep Files is another ownership-release path. Its incomplete
            // destinations need the same durable protection as library Delete.
            if let Some(download) = self
                .downloads
                .iter()
                .find(|download| &download.infohash == id)
            {
                for file in &download.files {
                    if !file.selected
                        || file.verified_bytes != file.length
                        || matches!(
                            download.state,
                            flick::torrent::TransferState::Checking
                                | flick::torrent::TransferState::Error
                        )
                    {
                        self.store
                            .data
                            .quarantine_torrent_file(&file.path, id, file.index);
                    }
                }
            }
            if let Err(error) = self.store.queue_save() {
                self.error = Some(format!(
                    "Could not save verification state. The transfer was kept: {error}"
                ));
                cx.notify();
                return;
            }
            let barrier = self.store.save_barrier();
            self.transfers_changed(cx);
            let save = cx
                .background_executor()
                .spawn(async move { barrier.wait() });
            cx.spawn(async move |this, cx| {
                let saved = save.await;
                let _ = this.update(cx, |this, cx| {
                    match saved {
                        Ok(()) => {
                            if let Err(error) = this.services.send(command) {
                                this.error = Some(error);
                            }
                        }
                        Err(error) => {
                            this.error = Some(format!(
                                "Could not save verification state. The transfer was kept: {error}"
                            ))
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            cx.notify();
            return;
        }
        if let Err(error) = self.services.send(command) {
            self.error = Some(error);
            self.resolving = false;
        }
        cx.notify();
    }

    pub(super) fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.query.trim().to_owned();
        if self.search_link(&query, cx) {
            return;
        }
        if is_torrent_link(&query) {
            self.resolve(flick::torrent::TorrentInput::Magnet(query), cx);
            return;
        }
        if query.chars().count() < 3 {
            self.error = Some("Type at least 3 characters to search online.".into());
            cx.notify();
            return;
        }
        if !matches!(self.page, Page::Home | Page::Videos) {
            self.page = Page::Home;
        }
        self.search_generation += 1;
        self.searching = true;
        self.search_results = None;
        self.error = None;
        self.send(
            ServiceCommand::Search {
                generation: self.search_generation,
                query,
            },
            cx,
        );
    }

    pub(super) fn resolve(&mut self, input: flick::torrent::TorrentInput, cx: &mut Context<Self>) {
        if self.resolving {
            return;
        }
        self.resolving = true;
        self.pending_torrent = None;
        self.error = None;
        if self.view == View::Player {
            self.persist();
            self.property("pause", "yes", cx);
        }
        self.view = View::Library;
        self.page = Page::Downloads;
        self.send(ServiceCommand::Resolve(input), cx);
    }

    pub(super) fn poll_services(&mut self, cx: &mut Context<Self>) {
        let events = self.services.drain().collect::<Vec<_>>();
        let changed = !events.is_empty();
        for event in events {
            match event {
                ServiceEvent::Search {
                    generation,
                    result,
                    done,
                } if generation == self.search_generation => {
                    self.searching = !done;
                    match result {
                        Ok(result) => self.search_results = Some(result),
                        Err(error) => self.error = Some(error),
                    }
                }
                ServiceEvent::Search { .. } => {}
                ServiceEvent::Subtitles { generation, result }
                    if generation == self.subtitle_search.generation =>
                {
                    self.subtitle_search.searching = false;
                    self.subtitle_search.results = Some(result);
                }
                ServiceEvent::Subtitles { .. } => {}
                ServiceEvent::SubtitleSaved(result) => {
                    self.subtitle_search.downloading = None;
                    match result {
                        Ok(path) => {
                            if let Some(path) = path.to_str() {
                                self.command(&["sub-add", path, "select"], cx);
                            }
                            self.menu = None;
                            self.show_status("Subtitle loaded");
                        }
                        Err(error) => self.subtitle_search.results = Some(Err(error)),
                    }
                }
                ServiceEvent::Fetch { id, update } => self.fetch_event(id, update, cx),
                ServiceEvent::ArchiveResolved { identifier, result } => {
                    self.archive_listed(identifier, result)
                }
                ServiceEvent::ResolveCancelled => {
                    self.resolving = false;
                    self.pending_torrent = None;
                }
                ServiceEvent::Resolved(result) => {
                    self.resolving = false;
                    match result {
                        Ok(torrent) => {
                            self.selected_files = torrent
                                .files
                                .iter()
                                .filter(|f| flick::library::is_video(&f.relative_path))
                                .map(|f| f.index)
                                .collect();
                            if self.selected_files.is_empty() {
                                self.selected_files
                                    .extend(torrent.files.iter().map(|f| f.index));
                            }
                            self.pending_torrent = Some(torrent);
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                ServiceEvent::ShareChecking(path) => {
                    self.checking_share = Some(path);
                    self.page = Page::Downloads;
                }
                ServiceEvent::Shared(result) => {
                    self.checking_share = None;
                    match result {
                        Ok(shared) => {
                            self.magnet_field
                                .update(cx, |field, cx| field.set_value(shared.magnet.clone(), cx));
                            self.shared_magnet = Some(shared.magnet);
                            self.page = Page::Downloads;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                ServiceEvent::TransferAvailability(availability) => {
                    match &availability {
                        crate::services::TransferAvailability::Checking => {
                            self.transfer_roots = None;
                            self.transfers_known = false;
                            self.posters_dirty = true;
                        }
                        crate::services::TransferAvailability::Ready => {
                            self.posters_dirty = true;
                        }
                        crate::services::TransferAvailability::Unavailable {
                            message,
                            roots,
                            roots_complete,
                        } => {
                            self.transfers_known = false;
                            self.transfer_roots = roots_complete.then(|| roots.clone());
                            self.error = Some(format!(
                                "Downloads unavailable: {message}. Restore folder access and choose Retry downloads."
                            ));
                            self.posters_dirty = true;
                        }
                    }
                    self.transfer_availability = availability;
                }
                ServiceEvent::TransferRoots(roots) => {
                    self.transfer_roots = Some(roots);
                    self.posters_dirty = true;
                }
                ServiceEvent::Progress(downloads) => {
                    let mut verified = false;
                    for download in &downloads {
                        if matches!(
                            download.state,
                            flick::torrent::TransferState::Checking
                                | flick::torrent::TransferState::Error
                        ) {
                            continue;
                        }
                        for file in &download.files {
                            if file.selected && file.verified_bytes == file.length {
                                verified |= self.store.data.clear_verified_torrent_file(
                                    &file.path,
                                    &download.infohash,
                                    file.index,
                                );
                            }
                        }
                    }
                    self.downloads = downloads;
                    if !self.transfers_known {
                        self.posters_dirty = true;
                    }
                    self.transfers_known = true;
                    self.transfers_changed(cx);
                    if verified && let Err(error) = self.store.queue_save() {
                        self.error = Some(format!("Could not save verification state: {error}"));
                    }
                }
                ServiceEvent::DownloadAccepted { infohash, selected } => {
                    // New admission invalidates any old verified snapshot. Never
                    // let a partial re-download inherit a previous owner's proof.
                    self.downloads
                        .retain(|download| download.infohash != infohash);
                    for (path, index) in &selected {
                        self.store
                            .data
                            .allow_torrent_restore(path, &infohash, *index);
                        self.store
                            .data
                            .quarantine_torrent_file(path, &infohash, *index);
                    }
                    self.library_files_restored(selected.iter().map(|(path, _)| path));
                    self.transfers_changed(cx);
                    self.library_changed(cx);
                    if let Err(error) = self.store.queue_save() {
                        self.error = Some(format!("Could not save download state: {error}"));
                    }
                }
                ServiceEvent::TorrentCompleted {
                    infohash,
                    file_index,
                    path,
                } => {
                    // Completion is admitted by the service only after selected,
                    // full verification of this exact engine destination.
                    self.store
                        .data
                        .clear_verified_torrent_file(&path, &infohash, file_index);
                    if !self.store.data.is_torrent_removed(&infohash, file_index) {
                        self.store.data.bind_torrent(&path, &infohash, file_index);
                    }
                    self.transfers_changed(cx);
                    self.library_changed(cx);
                    if let Err(error) = self.store.queue_save() {
                        self.error = Some(format!("Could not save the library: {error}"));
                    }
                }
                ServiceEvent::Completed(path) => {
                    // The identified Fetch event decides whether this explicit
                    // admission predates any subsequent library removal.
                    self.store.data.add_video(&path);
                    self.library_changed(cx);
                    if let Err(error) = self.store.queue_save() {
                        self.error = Some(format!("Could not save the library: {error}"));
                    }
                }
                ServiceEvent::Error(error) => {
                    self.resolving = false;
                    self.error = Some(error);
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn pick_library(
        &mut self,
        folder: bool,
        share: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: !folder || share,
            directories: folder,
            multiple: !folder && !share,
            prompt: Some(
                if share {
                    "Share files or a folder"
                } else if folder {
                    "Add a video folder"
                } else {
                    "Add videos to library"
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            match result {
                Ok(Ok(Some(paths))) if !share && !folder => {
                    let (torrents, videos): (Vec<_>, Vec<_>) = paths.into_iter().partition(|path| {
                        path.extension().is_some_and(|v| v.eq_ignore_ascii_case("torrent"))
                    });
                    let _ = this.update_in(cx, |this, _, cx| {
                        this.importing = true;
                        cx.notify();
                    });
                    let (videos, errors) = cx.background_executor()
                        .spawn(async move { validate_library_files(videos) }).await;
                    let _ = this.update_in(cx, |this, _, cx| {
                        this.importing = false;
                        this.library_files_restored(videos.iter());
                        this.store.data.restore_videos(videos);
                        this.library_changed(cx);
                        this.resolve_imported_torrents(torrents, errors, cx);
                        this.persist();
                        cx.notify();
                    });
                }
                result => {
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(Ok(Some(paths))) => {
                            if let Some(path) = paths.into_iter().next() {
                                if share { this.show_page(Page::Downloads, window, cx); this.send(ServiceCommand::Share(path), cx); }
                                else { this.import_folder(path, window, cx); }
                            }
                        }
                        Ok(Ok(None)) => {}
                        _ => { this.error = Some("The file picker could not open. You can also drop videos into the library.".into()); cx.notify(); }
                    });
                }
            }
        }).detach();
    }

    pub(super) fn import_folder(&mut self, path: PathBuf, window: &Window, cx: &mut Context<Self>) {
        self.importing = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { flick::library::scan_folder(&path) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.importing = false;
                match result {
                    Ok(paths) => {
                        this.library_files_restored(paths.iter());
                        this.store.data.restore_videos(paths);
                        this.library_changed(cx);
                        this.persist();
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not add this folder: {error:#}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn export_torrent(&mut self, id: String, window: &Window, cx: &mut Context<Self>) {
        let receiver =
            cx.prompt_for_new_path(&self.download_folder, Some(&format!("{id}.torrent")));
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, _, cx| match result {
                Ok(Ok(Some(path))) => this.send(ServiceCommand::Export { id, path }, cx),
                Ok(Ok(None)) => {}
                _ => {
                    this.error = Some("The save picker could not open.".into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn update_transfer_preferences(&mut self, cx: &mut Context<Self>) {
        let preferences = &self.store.data.transfers;
        let command = ServiceCommand::ConfigureTransfers {
            seed_completed: preferences.seed_completed.unwrap_or(false),
            download_bps: preferences.download_bps,
            upload_bps: preferences.upload_bps,
        };
        if let Err(error) = self.store.queue_save() {
            self.error = Some(format!("Could not save transfer preferences: {error}"));
        }
        self.send(command, cx);
    }

    pub(super) fn seeding_choice(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        if self.store.data.transfers.seed_completed.is_some() {
            return div();
        }
        v_flex().gap_2().my_3()
            .child(div().child("Keep sharing completed downloads?"))
            .child(div().text_size(t.font(12.)).text_color(t.muted)
                .child("Sharing uploads to other peers while Flick is open. Until you choose, completed downloads pause. Files you explicitly share keep sharing."))
            .child(h_flex().gap_2()
                .child(self.button("seed-choice-on", None, "Keep Sharing", false, cx, |this, _, cx| {
                    this.store.data.transfers.seed_completed = Some(true); this.update_transfer_preferences(cx);
                }))
                .child(self.button("seed-choice-off", None, "Pause When Complete", false, cx, |this, _, cx| {
                    this.store.data.transfers.seed_completed = Some(false); this.update_transfer_preferences(cx);
                })))
    }

    pub(super) fn choose_download_folder(&mut self, window: &Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Download to folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, _, cx| {
                    this.folder_field.update(cx, |field, cx| {
                        field.set_value(path.to_string_lossy().into_owned(), cx)
                    });
                    this.download_folder = path;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn page_actions(&self) -> &'static [(&'static str, &'static str, Control)] {
        match self.page {
            Page::Home | Page::Videos | Page::Shows => &[
                ("folder-plus", "Add Folder…", Control::AddFolder),
                ("plus", "Add Videos…", Control::AddVideos),
            ],
            Page::Downloads => &[
                ("magnet", "Paste Link", Control::PasteMagnet),
                ("share-2", "Share Files…", Control::Share),
            ],
            Page::History | Page::Appearance | Page::Shortcuts => &[],
        }
    }

    /// Icon buttons for the current page, used in macOS toolbars and GNOME
    /// header bars.
    /// With "Show button labels" on, they become text buttons.
    fn action_icons(&self, cx: &Context<Self>) -> Div {
        let labels = self.store.data.appearance.show_labels;
        let mut actions = h_flex().gap_1();
        for (index, (name, label, control)) in self.page_actions().iter().copied().enumerate() {
            let id = ("page-action", index);
            let on_click = move |this: &mut Self, w: &mut Window, cx: &mut Context<Self>| {
                this.handle(control, w, cx)
            };
            actions = actions.child(if labels {
                self.button(id, Some(name), label, false, cx, on_click)
            } else {
                self.icon_button(id, name, label, cx, on_click)
            });
        }
        actions
    }

    pub(super) fn library_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let title = self.page.title();
        let body = self.page_body(window, cx);
        let controls = self.window_controls(window, false, cx);
        match t.style {
            Style::Mac => {
                let toolbar = div()
                    .id("toolbar")
                    .flex()
                    .items_center()
                    .flex_none()
                    .h(px(t.titlebar_height))
                    .px_3()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(t.font(15.))
                            .font_weight(FontWeight::BOLD)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .child(self.action_icons(cx))
                    .child(self.search_field(window, cx).w(t.font(250.)));
                div()
                    .flex()
                    .size_full()
                    .bg(t.window)
                    .child(self.sidebar(controls, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.titlebar_area(toolbar, cx))
                            .child(body),
                    )
                    .into_any_element()
            }
            Style::Windows => {
                let titlebar = div()
                    .id("titlebar")
                    .flex()
                    .items_center()
                    .flex_none()
                    .h(px(t.titlebar_height))
                    .pl(px(14. + traffic_light_space()))
                    .gap_3()
                    .child(app_icon(16., 4.))
                    .child(div().flex_none().text_size(t.font(12.)).child("Flick"))
                    .child(div().flex_1())
                    .child(self.search_field(window, cx).w(t.font(380.)).flex_shrink())
                    .child(div().flex_1())
                    .when_some(controls, |d, c| d.child(c));
                v_flex()
                    .size_full()
                    .bg(t.chrome)
                    .child(self.titlebar_area(titlebar, cx))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h(px(0.))
                            .child(self.sidebar(None, cx))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .bg(t.window)
                                    .rounded_tl(px(8.))
                                    .border_t_1()
                                    .border_l_1()
                                    .border_color(t.border)
                                    .child(body),
                            ),
                    )
                    .into_any_element()
            }
            Style::Gnome => {
                let searchable = matches!(self.page, Page::Home | Page::Videos);
                let search_visible = searchable && (self.search_open || !self.query.is_empty());
                let header = div()
                    .id("headerbar")
                    .flex()
                    .items_center()
                    .flex_none()
                    .relative()
                    .h(px(t.titlebar_height))
                    .px_1p5()
                    .gap_1()
                    .child(self.action_icons(cx))
                    .child(
                        div()
                            .absolute()
                            .left(px(140.))
                            .right(px(140.))
                            .text_center()
                            .font_weight(FontWeight::BOLD)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .child(div().flex_1())
                    .when(searchable, |d| {
                        d.child(self.icon_toggle(
                            "toggle-search",
                            "search",
                            "Search",
                            search_visible,
                            cx,
                            |this, w, cx| {
                                if this.search_open {
                                    this.search_open = false;
                                    this.focus.focus(w);
                                    cx.notify();
                                } else {
                                    this.focus_search(w, cx);
                                }
                            },
                        ))
                    })
                    .when_some(controls, |d, c| d.child(c));
                div()
                    .flex()
                    .size_full()
                    .bg(t.window)
                    .child(self.sidebar(None, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.titlebar_area(header, cx))
                            .when(search_visible, |d| {
                                d.child(h_flex().flex_none().justify_center().px_4().pb_2().child(
                                    self.search_field(window, cx).w(t.font(420.)).flex_shrink(),
                                ))
                            })
                            .child(body),
                    )
                    .into_any_element()
            }
        }
    }

    fn sidebar(&self, controls: Option<AnyElement>, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let active = self
            .downloads
            .iter()
            .filter(|d| {
                matches!(
                    d.state,
                    flick::torrent::TransferState::Downloading
                        | flick::torrent::TransferState::Checking
                )
            })
            .count()
            + self.fetches.active();
        let badge = (active > 0).then(|| active.to_string());
        let item = |page: Page, name: &'static str, badge: Option<String>| {
            self.nav_item(page, name, badge, cx).into_any_element()
        };
        let library = vec![
            item(Page::Home, "house", None),
            item(Page::Videos, "film", None),
            item(Page::Shows, "library", None),
            item(Page::History, "history", None),
        ];
        let transfers = item(Page::Downloads, "download", badge);
        let settings = v_flex()
            .gap_0p5()
            .child(item(Page::Shortcuts, "keyboard", None))
            .child(item(Page::Appearance, "palette", None));
        let heading = |text: &'static str| {
            div()
                .px_2p5()
                .pt_4()
                .pb_1()
                .text_size(t.font(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.muted)
                .child(text)
        };
        match t.style {
            Style::Mac => {
                let header = div()
                    .id("sidebar-titlebar")
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .px_1p5()
                    .flex_none()
                    .when_some(controls, |d, c| d.child(c));
                div().flex_none().p_2().pr_0().h_full().child(
                    v_flex()
                        .w(px(t.sidebar_width))
                        .h_full()
                        .p_2()
                        .rounded(px(12.))
                        .bg(t.sidebar)
                        .border_1()
                        .border_color(t.border)
                        .shadow_sm()
                        .child(self.titlebar_area(header, cx))
                        .child(heading("Library"))
                        .children(library)
                        .child(heading("Transfers"))
                        .child(transfers)
                        .child(div().flex_1())
                        .child(settings),
                )
            }
            Style::Windows => v_flex()
                .flex_none()
                .w(px(t.sidebar_width))
                .h_full()
                .px_1()
                .pt_1()
                .pb_1()
                .gap_0p5()
                .children(library)
                .child(transfers)
                .child(div().flex_1())
                .child(settings),
            Style::Gnome => {
                let header = div()
                    .id("sidebar-headerbar")
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .h(px(t.titlebar_height))
                    .pl(px(traffic_light_space()))
                    .font_weight(FontWeight::BOLD)
                    .child("Flick");
                v_flex()
                    .flex_none()
                    .w(px(t.sidebar_width))
                    .h_full()
                    .bg(t.sidebar)
                    .border_r_1()
                    .border_color(t.border)
                    .child(self.titlebar_area(header, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .px_1p5()
                            .pb_1p5()
                            .gap_0p5()
                            .children(library)
                            .child(div().my_1p5().mx_2().h(px(1.)).bg(t.border))
                            .child(transfers)
                            .child(div().flex_1())
                            .child(settings),
                    )
            }
        }
    }

    fn nav_item(
        &self,
        page: Page,
        name: &'static str,
        badge: Option<String>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let on = self.view == View::Library && self.page == page;
        let (height, radius, gap, icon_color) = match t.style {
            Style::Mac => (28., 7., 8., t.accent),
            Style::Windows => (36., 4., 14., t.text),
            Style::Gnome => (36., 6., 12., t.text),
        };
        let id = ElementId::from(SharedString::from(format!("nav-{}", page.title())));
        let el =
            h_flex()
                .id(id.clone())
                .relative()
                .flex_none()
                .h(px(height))
                .px_2p5()
                .gap(px(gap))
                .rounded(px(radius))
                .cursor_pointer()
                .when(on, |d| d.bg(t.selection))
                .when(!on, |d| d.hover(|s| s.bg(t.selection)))
                .when(on && t.style == Style::Gnome, |d| {
                    d.font_weight(FontWeight::SEMIBOLD)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.control_pressed = true),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.show_page(page, window, cx)
                }))
                .when(on && t.style == Style::Windows, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(10.))
                            .w(px(3.))
                            .h(px(16.))
                            .rounded_full()
                            .bg(t.accent),
                    )
                })
                .child(icon(
                    name,
                    if t.style == Style::Mac { 16. } else { 18. },
                    icon_color,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(page.title()),
                )
                .when_some(badge, |d, badge| {
                    d.child(
                        div()
                            .px_1p5()
                            .rounded_full()
                            .text_size(t.font(11.))
                            .when(t.style == Style::Mac, |d| d.text_color(t.muted))
                            .when(t.style != Style::Mac, |d| {
                                d.bg(t.accent).text_color(t.on_accent)
                            })
                            .child(badge),
                    )
                });
        self.focusable(el, &id, None, radius, (0., 0.), cx)
    }

    fn first_run(&self) -> bool {
        self.store.data.library.is_empty()
            && self.store.data.recent.is_empty()
            && self.query.is_empty()
            && self.search_results.is_none()
            && !self.searching
            && !self.importing
            && !self.resolving
            && self.pending_torrent.is_none()
            && self.pending_open.is_none()
    }

    /// Width available to page content, used to size card grids.
    fn content_width(&self, window: &Window, cx: &Context<Self>) -> f32 {
        let t = cx.theme();
        let chrome = match t.style {
            Style::Mac => 8. + 44.,
            Style::Windows => 1. + 64.,
            Style::Gnome => 1. + 56.,
        };
        let frame = if matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        ) {
            20.
        } else {
            0.
        };
        (f32::from(window.viewport_size().width) - t.sidebar_width - chrome - frame - 12.).max(240.)
    }

    fn page_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.library_ui_state.begin_visible_posters();
        if self.library_ui_state.posters.is_none() {
            self.library_ui_state.posters = Some(cx.new(PosterCache::new));
        }
        let t = cx.theme().clone();
        let error = self.error.clone().map(|error| {
            notice(&t, true, error).mb_4().child(self.icon_button(
                "dismiss-error",
                "x",
                "Dismiss",
                cx,
                |this, w, cx| this.handle(Control::DismissError, w, cx),
            ))
        });
        if self.page == Page::Home && self.first_run() {
            return v_flex()
                .flex_1()
                .min_h(px(0.))
                .items_center()
                .justify_center()
                .px_8()
                .children(error)
                .child(self.welcome(cx))
                .into_any_element();
        }
        let (x, top) = match t.style {
            Style::Mac => (22., 4.),
            Style::Windows => (32., 22.),
            Style::Gnome => (28., 10.),
        };
        let width = self.content_width(window, cx);
        let virtual_page = matches!(self.page, Page::Videos | Page::History | Page::Shows);
        let mut body = v_flex()
            .id("page-scroll")
            .flex_1()
            .min_h(px(0.))
            .when(!virtual_page, |d| d.overflow_y_scroll())
            .when(virtual_page, |d| d.overflow_hidden())
            .px(px(x))
            .pt(px(top))
            .pb_8();
        if !virtual_page {
            body =
                self.with_scroll_keys(body, "page-scroll", &self.library_ui_state.page_scroll, cx);
        }
        if t.style == Style::Windows {
            let mut header = h_flex().flex_none().mb_5().gap_2().child(
                div()
                    .flex_1()
                    .text_size(t.font(28.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.page.title()),
            );
            let actions = self.page_actions();
            for (index, (name, label, control)) in actions.iter().copied().enumerate() {
                header = header.child(self.button(
                    ("page-button", index),
                    Some(name),
                    label,
                    index + 1 == actions.len(),
                    cx,
                    move |this, w, cx| this.handle(control, w, cx),
                ));
            }
            body = body.child(header);
        }
        let has_notices = self.error.is_some()
            || self.importing
            || self.resolving
            || self.pending_open.is_some()
            || matches!(
                self.transfer_availability,
                crate::services::TransferAvailability::Unavailable { .. }
            );
        let mut notices = v_flex().w_full().children(error);
        if self.importing {
            notices = notices.child(notice(&t, false, "Adding videos from your folder…").mb_4());
        }
        if self.resolving {
            notices = notices.child(
                notice(&t, false, "Finding peers…")
                    .mb_4()
                    .child(self.button(
                        "cancel-resolve",
                        None,
                        "Cancel",
                        false,
                        cx,
                        |this, _, cx| this.send(ServiceCommand::CancelResolve, cx),
                    )),
            );
        }
        if matches!(
            self.transfer_availability,
            crate::services::TransferAvailability::Unavailable { .. }
        ) {
            notices = notices.child(
                self.button(
                    "retry-downloads",
                    None,
                    "Retry downloads",
                    false,
                    cx,
                    |this, _, cx| {
                        this.error = None;
                        match this.services.send(ServiceCommand::RetryTransfers) {
                            Ok(()) => {
                                this.transfer_availability =
                                    crate::services::TransferAvailability::Checking
                            }
                            Err(error) => this.error = Some(error),
                        }
                        cx.notify();
                    },
                )
                .mb_4(),
            );
        }
        if self.pending_open.is_some() {
            notices = notices.child(
                notice(
                    &t,
                    false,
                    if matches!(
                        self.transfer_availability,
                        crate::services::TransferAvailability::Unavailable { .. }
                    ) {
                        "This video is waiting for download access. Retry downloads or cancel."
                    } else {
                        "Checking saved downloads before opening this video…"
                    },
                )
                .mb_4()
                .child(self.button(
                    "cancel-pending-open",
                    None,
                    "Cancel",
                    false,
                    cx,
                    |this, _, cx| {
                        this.pending_open = None;
                        this.open_generation += 1;
                        cx.notify();
                    },
                )),
            );
        }
        if self.page == Page::Videos {
            return body
                .child(self.videos_page(width, notices, has_notices, cx))
                .into_any_element();
        }
        if has_notices {
            body = body.child(if virtual_page {
                self.search_pane(notices, false, cx).into_any_element()
            } else {
                notices.flex_none().into_any_element()
            });
        }
        let page = match self.page {
            Page::Home => self.home_page(width, cx),
            Page::Videos => unreachable!("Videos owns its notices and search viewport"),
            Page::Shows => self.shows_page(width, cx),
            Page::History => self.history_page(cx),
            Page::Downloads => self.downloads_page(cx),
            Page::Appearance => self.appearance_page(cx),
            Page::Shortcuts => self.shortcuts_page(cx),
        };
        body = body.child(page.when(!virtual_page, |d| d.flex_none()));
        body.into_any_element()
    }

    /// Page keys work within results and file choosers, including their bottom actions.
    fn with_scroll_keys(
        &self,
        el: Stateful<Div>,
        id: &'static str,
        scroll: &ScrollHandle,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let el = el.track_scroll(scroll);
        let scroll = scroll.clone();
        el.track_focus(&self.control_handle(&id.into(), cx))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.platform
                    || modifiers.control
                    || modifiers.alt
                    || this.text_focused(window, cx)
                {
                    return;
                }
                if let Some(y) = scroll_key_offset(
                    &event.keystroke.key,
                    scroll.offset().y,
                    scroll.max_offset().height,
                    scroll.bounds().size.height,
                ) {
                    scroll.set_offset(point(scroll.offset().x, y));
                    window.refresh();
                    cx.stop_propagation();
                }
            }))
    }

    fn search_pane(&self, content: Div, link: bool, cx: &Context<Self>) -> Stateful<Div> {
        self.with_scroll_keys(
            search_pane_layout(content, link),
            "video-search-scroll",
            &self.library_ui_state.search_scroll,
            cx,
        )
    }

    fn welcome(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        let command = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let pill = t.style == Style::Gnome;
        let round = |b: Stateful<Div>| if pill { b.rounded_full().px_5() } else { b };
        v_flex()
            .items_center()
            .gap_3()
            .max_w(px(520.))
            .text_center()
            .child(app_icon(
                72.,
                match t.style {
                    Style::Mac => 18.,
                    Style::Windows => 14.,
                    Style::Gnome => 36.,
                },
            ))
            .child(
                div()
                    .mt_2()
                    .text_size(t.font(26.))
                    .font_weight(FontWeight::BOLD)
                    .child("Welcome to Flick"),
            )
            .child(
                div()
                    .text_color(t.muted)
                    .child("Play your videos, pick up where you left off, and download or share over BitTorrent."),
            )
            .child(
                v_flex()
                    .my_3()
                    .w(px(440.))
                    .h(px(108.))
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded(px(t.card_radius))
                    .border_2()
                    .border_dashed()
                    .border_color(t.control_hover)
                    .text_color(t.muted)
                    .child(icon("upload", 24., t.muted))
                    .child("Drop videos, folders, or .torrent files here"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(round(self.button(
                        "welcome-open",
                        Some("play"),
                        "Open Video…",
                        true,
                        cx,
                        |this, w, cx| this.handle(Control::Open, w, cx),
                    )))
                    .child(round(self.button(
                        "welcome-folder",
                        Some("folder-plus"),
                        "Add Folder…",
                        false,
                        cx,
                        |this, w, cx| this.handle(Control::AddFolder, w, cx),
                    )))
                    .child(round(self.button(
                        "welcome-magnet",
                        Some("magnet"),
                        "Paste Link",
                        false,
                        cx,
                        |this, w, cx| this.handle(Control::PasteMagnet, w, cx),
                    ))),
            )
            .child(self.seeding_choice(cx))
            .child(
                div()
                    .mt_2()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(format!("{command}K to search or paste a link  ·  {command}, for Appearance")),
            )
    }

    /// Whether the search text is a link to download rather than words.
    fn query_is_link(&self) -> bool {
        let query = self.query.trim();
        is_torrent_link(query) || flick::fetch::web_link(query).is_some()
    }

    /// A download offer for a pasted YouTube, X, web, or magnet link. Enter
    /// or the button starts it, the same way.
    fn link_suggestion(&self, cx: &Context<Self>) -> Option<Div> {
        let t = cx.theme().clone();
        let query = self.query.trim();
        let (title, detail): (String, String) = if is_torrent_link(query) {
            (
                "Download this torrent".into(),
                "Magnet link · Press Enter to see its files".into(),
            )
        } else {
            match flick::fetch::web_link(query)? {
                Ok(link @ flick::fetch::WebLink::Archive(_)) => (
                    format!("Download from {}", link.site()),
                    "Choose files first · Press Enter".into(),
                ),
                Ok(link @ flick::fetch::WebLink::Page(_)) => (
                    format!("Download video from {}", link.site()),
                    format!("{query} · Press Enter"),
                ),
                Err(error) => return Some(notice(&t, true, format!("{error:#}")).mb_6()),
            }
        };
        let button = self.button(
            "download-link",
            Some("download"),
            "Download",
            true,
            cx,
            |this, _, cx| this.search(cx),
        );
        Some(
            v_flex()
                .mb_8()
                .child(section_title(&t, "Link"))
                .child(group(
                    &t,
                    vec![
                        row(&t, Some("download"), title, Some(detail.into()), button)
                            .into_any_element(),
                    ],
                )),
        )
    }

    fn search_section(&self, cx: &Context<Self>) -> Option<Div> {
        if self.query_is_link() {
            return self.link_suggestion(cx);
        }
        let t = cx.theme().clone();
        if self.searching && self.search_results.is_none() {
            return Some(notice(&t, false, "Searching for videos online…").mb_6());
        }
        let results = self.search_results.as_ref()?;
        let mut rows = Vec::new();
        for (index, result) in results.items.iter().enumerate() {
            let source = result.clone();
            let detail = result_detail(result);
            rows.push(
                row(
                    &t,
                    Some("download"),
                    result.name.clone(),
                    Some(detail.into()),
                    self.button(
                        ("download-result", index),
                        None,
                        "Download",
                        false,
                        cx,
                        move |this, _, cx| this.download_result(&source, cx),
                    ),
                )
                .into_any_element(),
            );
        }
        let mut section = v_flex().mb_8().child(section_title(
            &t,
            format!("Online Results · {}", results.items.len()),
        ));
        if !results.unavailable.is_empty() {
            section = section.child(
                div()
                    .mb_2()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(format!(
                        "Unavailable sources: {}. Try again or paste a magnet link.",
                        results.unavailable.join(", ")
                    )),
            );
        }
        if rows.is_empty() {
            if results.unavailable.is_empty() {
                section = section.child(empty_state(
                    &t,
                    "search",
                    "No videos found",
                    "Try another title.",
                ));
            }
        } else {
            section = section.child(group(&t, rows));
        }
        if self.searching {
            section = section.child(
                div()
                    .mt_2()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child("Still searching…"),
            );
        }
        Some(section)
    }

    pub(super) fn video_detail(&self, path: &std::path::Path) -> (Option<f32>, String) {
        let data = &self.store.data;
        let duration = data
            .recent_file(path)
            .map(|file| file.duration)
            .filter(|duration| *duration > 0.)
            .or_else(|| data.video(path).map(|video| video.duration))
            .unwrap_or(0.);
        let resume = self.store.resume_position(path);
        let unavailable = self.library_file_unavailable(path);
        let unverified = data.unverified_torrent_file(path).is_some();
        if !unavailable && !unverified && resume > 0. && duration > 0. {
            return (
                Some((resume / duration).clamp(0., 1.) as f32),
                format!("{} left", format_time(duration - resume)),
            );
        }
        let state = if unavailable {
            "Unavailable"
        } else if unverified {
            "Needs verification"
        } else if data.watched(path) {
            "Watched"
        } else {
            "Not started"
        };
        if duration > 0. {
            (None, format!("{state} · {}", format_time(duration)))
        } else {
            (None, state.into())
        }
    }

    /// Opens the context menu for a video at a window position.
    fn open_card_menu(
        &mut self,
        path: PathBuf,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let continuing = self
            .store
            .data
            .continue_watching()
            .any(|file| file.path == path);
        self.card_menu = Some(CardMenu {
            path,
            position,
            continuing,
        });
        if self.keyboard_nav {
            self.control_handle(&ElementId::from(("card-menu", 0usize)), cx)
                .focus(window);
        }
        cx.notify();
    }

    /// Mouse and keyboard ways to open a video's context menu: right-click,
    /// Shift+F10, or the Menu key.
    fn with_card_menu(
        &self,
        el: Stateful<Div>,
        path: PathBuf,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let key_path = path.clone();
        el.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.keyboard_nav = false;
                this.open_card_menu(path.clone(), e.position, window, cx);
            }),
        )
        .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
            let key = crate::shortcuts::normalize(&e.keystroke);
            if key == "shift-f10" || key == "menu" {
                cx.stop_propagation();
                let bounds = this.focused_card.get();
                let position = point(bounds.left() + px(12.), bounds.top() + px(24.));
                this.open_card_menu(key_path.clone(), position, window, cx);
            }
        }))
    }

    pub(super) fn card_menu_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.card_menu.as_ref()?;
        let t = cx.theme().clone();
        let path = menu.path.clone();
        let watched = self.store.data.watched(&path);
        let mut items: Vec<(&'static str, &'static str, Rc<MenuAction>)> = Vec::new();
        let p = path.clone();
        items.push((
            "play",
            "Play",
            Rc::new(move |this, w, cx| this.open(p.clone(), w, cx)),
        ));
        let p = path.clone();
        items.push((
            "folder-open",
            "Show in Folder",
            Rc::new(move |_, _, cx| cx.reveal_path(&p)),
        ));
        let p = path.clone();
        items.push(if watched {
            (
                "eye-off",
                "Mark as Unwatched",
                Rc::new(move |this, _, _| this.store.data.mark_unwatched(&p)),
            )
        } else {
            (
                "check",
                "Mark as Watched",
                Rc::new(move |this, _, _| this.store.data.mark_watched(&p)),
            )
        });
        if menu.continuing {
            let p = path.clone();
            items.push((
                "x",
                "Remove from Continue Watching",
                Rc::new(move |this, _, _| this.store.data.dismiss_from_continue(&p)),
            ));
        }
        let p = path.clone();
        items.push((
            "x",
            "Remove from Library",
            Rc::new(move |this, w, cx| this.remove_library_video(p.clone(), w, cx)),
        ));
        items.push((
            "x",
            "Delete File…",
            Rc::new(move |this, w, cx| this.request_delete_video(path.clone(), w, cx)),
        ));
        let mut panel = v_flex()
            .id("card-menu-panel")
            .occlude()
            .p_1()
            .min_w(px(220.))
            .bg(t.popover)
            .border_1()
            .border_color(t.border)
            .rounded(px(t.card_radius))
            .shadow_lg()
            .text_color(t.text)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
        for (index, (name, label, action)) in items.into_iter().enumerate() {
            let id = ElementId::from(("card-menu", index));
            let item = h_flex()
                .id(id.clone())
                .relative()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(px(t.radius))
                .cursor_pointer()
                .hover(|s| s.bg(t.selection))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.card_menu = None;
                    this.focus.focus(window);
                    action(this, window, cx);
                    this.persist();
                    cx.notify();
                }))
                .child(icon(name, 16., t.muted))
                .child(label);
            panel = panel.child(self.focusable(item, &id, None, t.radius, (-2., -2.), cx));
        }
        Some(
            deferred(
                div()
                    .id("card-menu-backdrop")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.card_menu = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            this.card_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(
                        anchored()
                            .position(menu.position)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel),
                    ),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    fn video_has_focus(
        &self,
        section: &'static str,
        path: &std::path::Path,
        cx: &Context<Self>,
    ) -> bool {
        self.keyboard_focused(&video_id(section, path), cx)
            || self.keyboard_focused(&action_id(section, path, FocusSlot::Remove), cx)
            || self.keyboard_focused(&action_id(section, path, FocusSlot::Delete), cx)
    }

    fn video_actions(
        &self,
        section: &'static str,
        path: &std::path::Path,
        cx: &Context<Self>,
    ) -> Div {
        h_flex()
            .flex_none()
            .gap_1()
            .child(
                self.button(
                    action_id(section, path, FocusSlot::Remove),
                    None,
                    "Remove",
                    false,
                    cx,
                    {
                        let path = path.to_owned();
                        move |this, w, cx| this.remove_library_video(path.clone(), w, cx)
                    },
                )
                .text_size(cx.theme().font(11.)),
            )
            .child(
                self.button(
                    action_id(section, path, FocusSlot::Delete),
                    None,
                    "Delete…",
                    false,
                    cx,
                    {
                        let path = path.to_owned();
                        move |this, w, cx| this.request_delete_video(path.clone(), w, cx)
                    },
                )
                .text_size(cx.theme().font(11.)),
            )
    }

    /// A video card in a grid of `cols` columns holding `count` cards. Arrow
    /// keys move focus between cards; Enter or Space opens the video.
    #[allow(clippy::too_many_arguments)]
    fn video_card(
        &self,
        (section, index): (&'static str, usize),
        (cols, count): (usize, usize),
        path: &std::path::Path,
        title: &str,
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (progress, detail) = self.video_detail(path);
        let path = path.to_owned();
        let height = (width * 9. / 16.).round();
        let id = video_id(section, &path);
        let group_name = SharedString::from(format!("{section}-{index}"));
        let menu_path = path.clone();
        let focused_card = self
            .video_has_focus(section, &path, cx)
            .then(|| self.focused_card.clone());
        let el = v_flex()
            .id(id.clone())
            .group(group_name.clone())
            .relative()
            .w(px(width))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open(path.clone(), window, cx)
            }))
            .child(
                self.poster(&menu_path, title, width, height, t.card_radius, cx)
                    .relative()
                    .border_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x00000059))
                            .opacity(0.)
                            .group_hover(group_name, |s| s.opacity(1.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(44.))
                                    .rounded_full()
                                    .bg(rgba(0xffffff33))
                                    .child(icon("play", 22., rgb(0xffffff))),
                            ),
                    )
                    .when_some(progress, |d, progress| {
                        d.child(
                            div()
                                .absolute()
                                .left(px(8.))
                                .right(px(8.))
                                .bottom(px(8.))
                                .h(px(4.))
                                .rounded_full()
                                .bg(rgba(0xffffff4d))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(progress))
                                        .rounded_full()
                                        .bg(t.accent),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .mt_1()
                    .font_weight(FontWeight::MEDIUM)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(title.to_owned()),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(detail),
            )
            .child(self.video_actions(section, &menu_path, cx))
            .when_some(focused_card, |d, cell| {
                d.child(
                    canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
            });
        let el = self.with_card_menu(el, menu_path, cx);
        let el = self.with_grid_keys(el, (section, index), (cols, count), cx);
        self.focusable(el, &id, None, t.card_radius, (0., 0.), cx)
            .into_any_element()
    }

    /// Arrow keys move focus within a row-major grid of `count` items with
    /// `cols` columns, and leave it at the edges. Lists use one column.
    fn with_grid_keys(
        &self,
        el: Stateful<Div>,
        (section, index): (&'static str, usize),
        (cols, count): (usize, usize),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        el.on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            let modifiers = event.keystroke.modifiers;
            if modifiers.control || modifiers.platform || modifiers.alt {
                return;
            }
            let virtual_section = this
                .library_ui_state
                .sections
                .borrow()
                .get(section)
                .cloned();
            let mut target_slot = FocusSlot::Primary;
            let target = if event.keystroke.key == "tab" {
                let Some(section_state) = virtual_section.as_ref() else {
                    return;
                };
                let actions = section_state.paths.is_some();
                let focused = window.focused(cx);
                let slot =
                    section_state
                        .paths
                        .as_ref()
                        .and_then(|paths| paths.get(index))
                        .map_or(FocusSlot::Primary, |path| {
                            if focused.as_ref()
                                == Some(&this.control_handle(
                                    &action_id(section, path, FocusSlot::Delete),
                                    cx,
                                ))
                            {
                                FocusSlot::Delete
                            } else if focused.as_ref()
                                == Some(&this.control_handle(
                                    &action_id(section, path, FocusSlot::Remove),
                                    cx,
                                ))
                            {
                                FocusSlot::Remove
                            } else {
                                FocusSlot::Primary
                            }
                        });
                let Some((target, slot)) = tab_target(index, count, slot, modifiers.shift, actions)
                else {
                    return;
                };
                target_slot = slot;
                Some(target)
            } else {
                if modifiers.shift {
                    return;
                }
                let Some(step) = GridStep::from_key(&event.keystroke.key) else {
                    return;
                };
                let target = step.target(index, cols, count);
                if target.is_none() {
                    cx.stop_propagation();
                    if matches!(step, GridStep::Left | GridStep::Up) {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    this.keyboard_nav = true;
                    cx.notify();
                    return;
                }
                target
            };
            let Some(target) = target else {
                return;
            };
            cx.stop_propagation();
            this.keyboard_nav = true;
            if let Some(section_state) = virtual_section {
                section_state
                    .scroll
                    .scroll_to_item(target / section_state.columns, ScrollStrategy::Center);
                let id = if target_slot != FocusSlot::Primary {
                    section_state
                        .paths
                        .as_ref()
                        .and_then(|paths| paths.get(target))
                        .map(|path| action_id(section, path, target_slot))
                } else {
                    section_state.ids.get(target).cloned()
                };
                if let Some(id) = id {
                    this.control_handle(&id, cx).focus(window);
                }
            }
            cx.notify();
        }))
    }

    /// A video as a list row: thumbnail, title, progress, and play icon.
    fn video_row(
        &self,
        (section, index): (&'static str, usize),
        count: usize,
        path: &std::path::Path,
        title: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (progress, detail) = self.video_detail(path);
        let id = video_id(section, path);
        let open_path = path.to_owned();
        let el = h_flex()
            .id(id.clone())
            .w_full()
            .relative()
            .gap_3()
            .px_3()
            .py_2()
            .cursor_pointer()
            .hover(|s| s.bg(t.selection))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open(open_path.clone(), window, cx)
            }))
            .child(self.poster(path, title, 80., 45., t.radius, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(title.to_owned()),
                    )
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(detail),
                    )
                    .when_some(progress, |d, progress| {
                        d.child(
                            div()
                                .w(px(160.))
                                .h(px(3.))
                                .rounded_full()
                                .bg(t.control_hover)
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(progress))
                                        .rounded_full()
                                        .bg(t.accent),
                                ),
                        )
                    }),
            )
            .child(self.video_actions(section, path, cx))
            .child(icon("play", 18., t.muted))
            .when(self.video_has_focus(section, path, cx), |d| {
                let cell = self.focused_card.clone();
                d.child(
                    canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
            });
        let el = self.with_card_menu(el, path.to_owned(), cx);
        let el = self.with_grid_keys(el, (section, index), (1, count), cx);
        self.focusable(el, &id, None, t.radius, (-RING_INSET, -RING_INSET), cx)
            .into_any_element()
    }

    fn filtered_video_paths(&self) -> Rc<[PathBuf]> {
        self.library_ui_state
            .video_paths(&self.store.data, &self.query, self.video_filter)
    }

    fn filtered_shows(&self) -> (Rc<[usize]>, Rc<[ElementId]>) {
        let revision = self.store.data.revision();
        let query = self.query.to_lowercase();
        let mut cache = self.library_ui_state.shows.borrow_mut();
        if let Some(cached) = cache.as_ref()
            && cached.revision == revision
            && cached.query == query
        {
            return (cached.indices.clone(), cached.ids.clone());
        }
        let shows = self.store.data.shows();
        let mut labels = self.library_ui_state.show_labels.borrow_mut();
        if !labels
            .as_ref()
            .is_some_and(|(old_revision, _)| *old_revision == revision)
        {
            *labels = Some((
                revision,
                shows.iter().map(|show| show.title.to_lowercase()).collect(),
            ));
        }
        let indices: Rc<[usize]> = labels
            .as_ref()
            .unwrap()
            .1
            .iter()
            .enumerate()
            .filter(|(_, title)| title.contains(&query))
            .map(|(index, _)| index)
            .collect();
        let ids = indices
            .iter()
            .map(|index| show_id(&shows[*index]))
            .collect::<Rc<[_]>>();
        *cache = Some(ShowQuery {
            revision,
            query,
            indices: indices.clone(),
            ids: ids.clone(),
        });
        (indices, ids)
    }

    fn register_video_section(
        &self,
        section: &'static str,
        paths: Rc<[PathBuf]>,
        columns: usize,
        scroll: UniformListScrollHandle,
    ) {
        let mut sections = self.library_ui_state.sections.borrow_mut();
        if let Some(previous) = sections.get_mut(section)
            && previous
                .paths
                .as_ref()
                .is_some_and(|old| Rc::ptr_eq(old, &paths))
        {
            previous.columns = columns;
            previous.scroll = scroll;
            return;
        }
        let ids = paths.iter().map(|path| video_id(section, path)).collect();
        sections.insert(
            section,
            VirtualSection {
                ids,
                paths: Some(paths),
                columns,
                scroll,
            },
        );
    }

    /// One scroll owner per page; only GPUI's requested rows create cards or images.
    fn virtual_videos(
        &self,
        section: &'static str,
        paths: Rc<[PathBuf]>,
        columns: usize,
        width: f32,
        list: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let count = paths.len();
        let scroll = self.library_ui_state.scroll(section);
        self.register_video_section(section, paths.clone(), columns, scroll.clone());
        let entity = cx.entity().downgrade();
        let row_height = if list {
            cx.theme().font(88.)
        } else {
            px((width * 9. / 16.).round() + 16.) + cx.theme().font(104.)
        };
        let measure = first_visible_row(&scroll, row_height, count.div_ceil(columns));
        uniform_list(section, count.div_ceil(columns), move |range, _, cx| {
            let Some(entity) = entity.upgrade() else {
                return Vec::new();
            };
            entity.update(cx, |this, cx| {
                this.library_ui_state.begin_visible_posters();
                range
                    .map(|row_index| {
                        let mut row = h_flex()
                            .items_start()
                            .gap(px(16.))
                            .h(row_height)
                            .w_full()
                            .px(px(RING_INSET));
                        for index in grid_row_range(row_index, columns, count) {
                            let path = &paths[index];
                            let Some(video) = this.store.data.video(path) else {
                                continue;
                            };
                            row = row.child(if list {
                                this.video_row((section, index), count, path, &video.title, cx)
                            } else {
                                this.video_card(
                                    (section, index),
                                    (columns, count),
                                    path,
                                    &video.title,
                                    width,
                                    cx,
                                )
                            });
                        }
                        row
                    })
                    .collect()
            })
        })
        .with_width_from_item(Some(measure))
        .track_scroll(scroll)
        .flex_1()
        .min_h(px(0.))
        .w_full()
        .into_any_element()
    }

    fn home_page(&self, width: f32, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let query = self.query.to_lowercase();
        let gap = 16.;
        let mut page = v_flex().w_full().children(self.search_section(cx));
        if self.query_is_link() {
            return page;
        }
        let (continue_cols, _) = columns(width, 220., gap);
        let continuing = self
            .store
            .data
            .continue_watching()
            .filter(|v| {
                self.store
                    .data
                    .normalized_recent_title(&v.path)
                    .is_some_and(|title| title.contains(&query))
            })
            .take(continue_cols)
            .collect::<Vec<_>>();
        if !continuing.is_empty() {
            let (cols, card) = columns(width, 220., gap);
            let shown = continuing.len();
            self.register_video_section(
                "continue",
                continuing.iter().map(|v| v.path.clone()).collect(),
                cols,
                UniformListScrollHandle::new(),
            );
            let cards = continuing
                .into_iter()
                .take(cols)
                .enumerate()
                .map(|(i, v)| {
                    self.video_card(("continue", i), (cols, shown), &v.path, &v.title, card, cx)
                })
                .collect();
            page = page
                .child(section_title(&t, "Continue Watching"))
                .child(card_grid(cards, cols, gap))
                .child(div().h(px(28.)));
        }
        let (cols, card) = columns(width, 180., gap);
        let added = self
            .store
            .data
            .library
            .iter()
            .rev()
            .filter(|v| {
                self.store
                    .data
                    .normalized_title(&v.path)
                    .is_some_and(|title| title.contains(&query))
            })
            .take(cols * 2)
            .collect::<Vec<_>>();
        page = page.child(
            h_flex()
                .justify_between()
                .child(section_title(&t, "Recently Added"))
                .child(
                    self.focusable(
                        div()
                            .id("see-all")
                            .relative()
                            .mb_2()
                            .text_size(t.font(12.))
                            .text_color(t.accent_text)
                            .cursor_pointer()
                            .child("See All")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_page(Page::Videos, window, cx)
                            })),
                        &"see-all".into(),
                        None,
                        t.radius,
                        (2., 0.),
                        cx,
                    ),
                ),
        );
        if added.is_empty() {
            page = page.child(empty_state(
                &t,
                "library",
                "No matching videos in your library",
                "Press Enter to search online.",
            ));
        } else {
            let shown = added.len();
            self.register_video_section(
                "recent",
                added.iter().map(|v| v.path.clone()).collect(),
                cols,
                UniformListScrollHandle::new(),
            );
            let cards = added
                .into_iter()
                .take(cols * 2)
                .enumerate()
                .map(|(i, v)| {
                    self.video_card(("recent", i), (cols, shown), &v.path, &v.title, card, cx)
                })
                .collect();
            page = page.child(card_grid(cards, cols, gap));
        }
        page
    }

    fn videos_page(&self, width: f32, notices: Div, has_notices: bool, cx: &Context<Self>) -> Div {
        let search = self.search_section(cx);
        let has_remote = has_notices || search.is_some();
        let remote = notices.children(search);
        if self.query_is_link() {
            return v_flex()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .child(self.search_pane(remote, true, cx));
        }
        let t = cx.theme().clone();
        let gap = 16.;
        let filter = self.segmented(
            "video-filter",
            &[
                (VideoFilter::All, "All"),
                (VideoFilter::Unwatched, "Unwatched"),
                (VideoFilter::InProgress, "In Progress"),
            ],
            self.video_filter,
            cx,
            |this, filter, _, cx| {
                this.video_filter = filter;
                cx.notify();
            },
        );
        let view = self.store.data.library_view;
        let sort = self.segmented(
            "video-sort",
            &[
                (Sort::Added, "Date Added"),
                (Sort::Title, "Title"),
                (Sort::Watched, "Last Watched"),
            ],
            view.sort,
            cx,
            |this, sort, _, cx| {
                this.store.data.library_view.sort = sort;
                this.persist();
                cx.notify();
            },
        );
        let layout = self.segmented(
            "video-layout",
            &[(false, "Grid"), (true, "List")],
            view.list,
            cx,
            |this, list, _, cx| {
                this.store.data.library_view.list = list;
                this.persist();
                cx.notify();
            },
        );
        let paths = self.filtered_video_paths();
        let mut page = v_flex()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .when(has_remote, |d| d.child(self.search_pane(remote, false, cx)))
            .child(
                h_flex()
                    .flex_none()
                    .mb_4()
                    .gap_3()
                    .flex_wrap()
                    .child(filter)
                    .child(sort)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(format!("{} videos", paths.len())),
                    )
                    .child(layout),
            );
        if paths.is_empty() {
            return page.child(if self.store.data.library.is_empty() {
                empty_state(
                    &t,
                    "library",
                    "Add your first videos",
                    "Add videos or a folder, or drop them here.",
                )
            } else {
                empty_state(
                    &t,
                    "library",
                    "No matching videos",
                    "Try another filter or search online.",
                )
            });
        }
        if view.list {
            return page.child(self.virtual_videos("library-row", paths, 1, width, true, cx));
        }
        let (cols, card) = columns(width - RING_INSET * 2., 180., gap);
        page = page.child(self.virtual_videos("library", paths, cols, card, false, cx));
        page
    }

    /// Shows found in the library, or one show's episodes by season.
    fn shows_page(&self, width: f32, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let (shows, show_ids) = self.filtered_shows();
        let selected = self.selected_show.as_ref().and_then(|key| {
            shows
                .iter()
                .filter_map(|index| self.store.data.shows().get(*index))
                .find(|show| {
                    show.episodes
                        .first()
                        .is_some_and(|(episode, _)| episode.show_key() == *key)
                })
        });
        if let Some(show) = selected {
            let count = show.episodes.len();
            let watched = count - self.library_ui_state.show_unwatched(&self.store.data, show);
            let mut page = v_flex().flex_1().min_h(px(0.)).w_full().child(
                h_flex()
                    .flex_none()
                    .mb_4()
                    .gap_3()
                    .child(self.button(
                        "all-shows",
                        Some("arrow-left"),
                        "All Shows",
                        false,
                        cx,
                        |this, _, cx| {
                            this.selected_show = None;
                            cx.notify();
                        },
                    ))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(show.title.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(format!("{count} episodes · {watched} watched")),
                    ),
            );
            let key = self.selected_show.as_deref().unwrap_or_default();
            let scroll = self.library_ui_state.scroll(&format!("episodes:{key}"));
            let revision = self.store.data.revision();
            let (episodes, paths) = {
                let mut cache = self.library_ui_state.episodes.borrow_mut();
                if !cache
                    .as_ref()
                    .is_some_and(|cached| cached.revision == revision && cached.key == key)
                {
                    *cache = Some(EpisodeQuery {
                        revision,
                        key: key.to_owned(),
                        episodes: show.episodes.clone().into(),
                        paths: show.episodes.iter().map(|(_, path)| path.clone()).collect(),
                    });
                }
                let cached = cache.as_ref().unwrap();
                (cached.episodes.clone(), cached.paths.clone())
            };
            self.register_video_section("episode", paths, 1, scroll.clone());
            let entity = cx.entity().downgrade();
            let measure = first_visible_row(&scroll, t.font(88.), count);
            page = page.child(
                uniform_list("episode-list", count, move |range, _, cx| {
                    let Some(entity) = entity.upgrade() else {
                        return Vec::new();
                    };
                    entity.update(cx, |this, cx| {
                        this.library_ui_state.begin_visible_posters();
                        range
                            .map(|index| {
                                let (episode, path) = &episodes[index];
                                let title = format!(
                                    "Season {} · {} · {}",
                                    episode.season,
                                    episode.label(),
                                    crate::model::file_title(path)
                                );
                                div()
                                    .h(cx.theme().font(88.))
                                    .w_full()
                                    .px(px(RING_INSET))
                                    .child(this.video_row(
                                        ("episode", index),
                                        count,
                                        path,
                                        &title,
                                        cx,
                                    ))
                            })
                            .collect()
                    })
                })
                .with_width_from_item(Some(measure))
                .track_scroll(scroll)
                .flex_1()
                .min_h(px(0.))
                .w_full(),
            );
            return page;
        }
        if shows.is_empty() {
            return v_flex().child(empty_state(
                &t,
                "library",
                "No shows yet",
                "Episodes named like Show S01E02, Show 1x02, or Show/Season 1/02 appear here.",
            ));
        }
        let gap = 16.;
        let (cols, card) = columns(width - RING_INSET * 2., 180., gap);
        let count = shows.len();
        let scroll = self.library_ui_state.scroll("shows");
        self.library_ui_state
            .register("show", show_ids, cols, scroll.clone());
        let entity = cx.entity().downgrade();
        let measure = first_visible_row(
            &scroll,
            px((card * 9. / 16.).round() + gap) + t.font(68.),
            count.div_ceil(cols),
        );
        v_flex().flex_1().min_h(px(0.)).w_full().child(
            uniform_list("shows-grid", count.div_ceil(cols), move |range, _, cx| {
                let Some(entity) = entity.upgrade() else {
                    return Vec::new();
                };
                entity.update(cx, |this, cx| {
                    this.library_ui_state.begin_visible_posters();
                    range
                        .map(|row| {
                            h_flex()
                                .items_start()
                                .gap(px(gap))
                                .w_full()
                                .px(px(RING_INSET))
                                .h(px((card * 9. / 16.).round() + gap) + cx.theme().font(68.))
                                .children(grid_row_range(row, cols, count).filter_map(|index| {
                                    this.store.data.shows().get(shows[index]).map(|show| {
                                        this.show_card(index, (cols, count), show, card, cx)
                                    })
                                }))
                        })
                        .collect()
                })
            })
            .with_width_from_item(Some(measure))
            .track_scroll(scroll)
            .flex_1()
            .min_h(px(0.))
            .w_full(),
        )
    }

    fn show_card(
        &self,
        index: usize,
        (cols, count): (usize, usize),
        show: &flick::library::Show,
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let key = show
            .episodes
            .first()
            .map(|(e, _)| e.show_key())
            .unwrap_or_default();
        let unwatched = self.library_ui_state.show_unwatched(&self.store.data, show);
        let detail = match unwatched {
            0 => format!("{} episodes · all watched", show.episodes.len()),
            n => format!("{} episodes · {n} unwatched", show.episodes.len()),
        };
        let id = show_id(show);
        let el = v_flex()
            .id(id.clone())
            .relative()
            .w(px(width))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected_show = Some(key.clone());
                if this.keyboard_nav
                    && let Some(path) = this
                        .store
                        .data
                        .shows()
                        .iter()
                        .find(|show| {
                            show.episodes
                                .first()
                                .is_some_and(|(e, _)| e.show_key() == key)
                        })
                        .and_then(|show| show.episodes.first())
                        .map(|(_, path)| path)
                {
                    this.control_handle(&video_id("episode", path), cx)
                        .focus(window);
                }
                cx.notify();
            }))
            .child(
                match show.episodes.first() {
                    Some((_, path)) => self.poster(
                        path,
                        &show.title,
                        width,
                        (width * 9. / 16.).round(),
                        t.card_radius,
                        cx,
                    ),
                    None => thumbnail(
                        &show.title,
                        width,
                        (width * 9. / 16.).round(),
                        t.card_radius,
                    ),
                }
                .border_1()
                .border_color(t.border),
            )
            .child(
                div()
                    .mt_1()
                    .font_weight(FontWeight::MEDIUM)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(show.title.clone()),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(detail),
            );
        let el = self.with_grid_keys(el, ("show", index), (cols, count), cx);
        self.focusable(el, &id, None, t.card_radius, (0., 0.), cx)
            .into_any_element()
    }

    fn history_page(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let count = self.store.data.recent.len();
        if count == 0 {
            return v_flex().child(empty_state(
                &t,
                "history",
                "Nothing watched yet",
                "Videos you play appear here so you can pick up where you left off.",
            ));
        }
        let paths = {
            let revision = self.store.data.revision();
            let mut cache = self.library_ui_state.history.borrow_mut();
            if !cache
                .as_ref()
                .is_some_and(|(old_revision, _)| *old_revision == revision)
            {
                *cache = Some((
                    revision,
                    self.store
                        .data
                        .recent
                        .iter()
                        .map(|video| video.path.clone())
                        .collect(),
                ));
            }
            cache.as_ref().unwrap().1.clone()
        };
        let scroll = self.library_ui_state.scroll("history");
        self.register_video_section("history", paths.clone(), 1, scroll.clone());
        let entity = cx.entity().downgrade();
        let measure = first_visible_row(&scroll, t.font(88.), count);
        v_flex().flex_1().min_h(px(0.)).w_full().child(
            uniform_list("history-list", count, move |range, _, cx| {
                let Some(entity) = entity.upgrade() else {
                    return Vec::new();
                };
                entity.update(cx, |this, cx| {
                    this.library_ui_state.begin_visible_posters();
                    range
                        .map(|index| {
                            let path = &paths[index];
                            let row = div().h(cx.theme().font(88.)).w_full().px(px(RING_INSET));
                            let Some(video) = this.store.data.recent_file(path) else {
                                return row;
                            };
                            row.child(this.video_row(
                                ("history", index),
                                count,
                                path,
                                &video.title,
                                cx,
                            ))
                        })
                        .collect()
                })
            })
            .with_width_from_item(Some(measure))
            .track_scroll(scroll)
            .flex_1()
            .min_h(px(0.))
            .w_full(),
        )
    }

    fn downloads_page(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let mut page = v_flex().w_full().child(self.seeding_choice(cx));
        if let Some(path) = &self.checking_share {
            page = page.child(
                notice(
                    &t,
                    false,
                    format!("Checking files… {} · creating piece hashes", path.display()),
                )
                .mb_3(),
            );
        }
        if let Some(torrent) = &self.pending_torrent {
            page = page
                .child(self.torrent_sheet(torrent, cx))
                .child(div().h(px(28.)));
        }
        page = page.child(self.link_downloads(cx));
        if let Some(magnet) = &self.shared_magnet {
            let magnet = magnet.clone();
            page = page.child(
                v_flex()
                    .mb_6()
                    .gap_2()
                    .child(
                        notice(
                            &t,
                            false,
                            "Share link ready. Keep Flick open while others download.",
                        )
                        .child(self.button(
                            "copy-magnet",
                            Some("copy"),
                            "Copy Magnet Link",
                            false,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(magnet.clone()))
                            },
                        )),
                    )
                    .child(self.text_field(&self.magnet_field, cx)),
            );
        }
        if self.downloads.is_empty() && self.fetches.is_empty() {
            page = page.child(empty_state(
                &t,
                "download",
                "No transfers yet",
                "Search for a video, paste a link, or share your own files.",
            ));
        } else if !self.downloads.is_empty() {
            let rows = self
                .downloads
                .iter()
                .map(|download| self.transfer_row(download, cx))
                .collect();
            page = page
                .child(
                    h_flex()
                        .justify_between()
                        .mb_2()
                        .child(section_title(&t, "Transfers"))
                        .child(
                            self.button(
                                "pause-all-transfers",
                                None,
                                if self
                                    .downloads
                                    .iter()
                                    .all(|d| d.state == flick::torrent::TransferState::Paused)
                                {
                                    "Resume All"
                                } else {
                                    "Pause All"
                                },
                                false,
                                cx,
                                |this, _, cx| {
                                    let paused = this
                                        .downloads
                                        .iter()
                                        .all(|d| d.state == flick::torrent::TransferState::Paused);
                                    this.send(
                                        if paused {
                                            ServiceCommand::ResumeAll
                                        } else {
                                            ServiceCommand::PauseAll
                                        },
                                        cx,
                                    );
                                },
                            ),
                        ),
                )
                .child(group(&t, rows));
        }
        page.child(
            h_flex()
                .mt_4()
                .gap_2()
                .text_size(t.font(12.))
                .text_color(t.muted)
                .child(icon("info", 14., t.muted))
                .child("Transfers and sharing run only while Flick is open."),
        )
    }

    fn transfer_row(
        &self,
        download: &flick::torrent::TransferSnapshot,
        cx: &Context<Self>,
    ) -> AnyElement {
        use flick::torrent::TransferState;
        let t = cx.theme().clone();
        let id = download.infohash.clone();
        let paused = matches!(download.state, TransferState::Paused | TransferState::Error);
        let complete = download.verified_bytes == download.total_bytes && download.total_bytes > 0;
        let (state, color) = match download.state {
            TransferState::Checking => ("Checking files…", t.accent),
            TransferState::Downloading => ("Downloading", t.accent),
            TransferState::Paused if complete => ("Complete · sharing paused", t.muted),
            TransferState::Paused => ("Paused", t.muted),
            TransferState::Seeding => ("Sharing", rgb(0x30b158)),
            TransferState::Error => ("Transfer failed", t.danger),
        };
        let (done, total) = if download.state == TransferState::Checking {
            (download.checked_bytes, download.checking_total_bytes)
        } else {
            (download.verified_bytes, download.total_bytes)
        };
        let progress = if total > 0 {
            (done as f64 / total as f64).clamp(0., 1.) as f32
        } else {
            0.
        };
        let playable = download.files.iter().find(|file| {
            file.selected
                && file.verified_bytes == file.length
                && !matches!(
                    download.state,
                    TransferState::Checking | TransferState::Error
                )
                && flick::library::is_video(&file.path)
        });
        let mut actions = h_flex().flex_wrap().gap_2();
        if let Some(file) = playable {
            let path = file.path.clone();
            let infohash = id.clone();
            let index = file.index;
            actions = actions.child(self.button(
                SharedString::from(format!("play-{id}")),
                Some("play"),
                "Play",
                false,
                cx,
                move |this, w, cx| {
                    this.store
                        .data
                        .restore_torrent_video(&path, &infohash, index);
                    this.open(path.clone(), w, cx);
                },
            ));
        }
        // A video still downloading plays from verified pieces, fetching
        // those near the playhead first.
        let streamable = download.files.iter().find(|file| {
            playable.is_none()
                && file.selected
                && file.verified_bytes < file.length
                && download.state == TransferState::Downloading
                && flick::library::is_video(&file.path)
        });
        if let Some(file) = streamable {
            let path = file.path.clone();
            let infohash = id.clone();
            let index = file.index;
            actions = actions.child(self.button(
                SharedString::from(format!("watch-{id}")),
                Some("play"),
                "Watch Now",
                false,
                cx,
                move |this, w, cx| {
                    this.store
                        .data
                        .restore_torrent_video(&path, &infohash, index);
                    this.open(path.clone(), w, cx);
                },
            ));
        }
        let toggle_id = id.clone();
        let export_id = id.clone();
        let remove_id = id.clone();
        let delete_id = id.clone();
        actions = actions
            .child(self.button(
                SharedString::from(format!("pause-{id}")),
                Some(if paused { "play" } else { "pause" }),
                if paused { "Resume" } else { "Pause" },
                false,
                cx,
                move |this, _, cx| {
                    this.send(
                        if paused {
                            ServiceCommand::Resume(toggle_id.clone())
                        } else {
                            ServiceCommand::Pause(toggle_id.clone())
                        },
                        cx,
                    )
                },
            ))
            .child(self.button(
                SharedString::from(format!("export-{id}")),
                None,
                "Export .torrent…",
                false,
                cx,
                move |this, w, cx| this.export_torrent(export_id.clone(), w, cx),
            ))
            .child(self.button(
                SharedString::from(format!("remove-{id}")),
                None,
                "Remove · Keep Files",
                false,
                cx,
                move |this, _, cx| {
                    this.send(
                        ServiceCommand::Remove {
                            id: remove_id.clone(),
                            delete_files: false,
                        },
                        cx,
                    )
                },
            ))
            .child(self.button(
                SharedString::from(format!("delete-{id}")),
                None,
                "Remove and Delete Files…",
                false,
                cx,
                move |this, _, cx| {
                    this.delete_transfer = Some(delete_id.clone());
                    cx.notify();
                },
            ));
        let mut entry = v_flex()
            .gap_2()
            .px_3()
            .py_2p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(icon(
                        if complete { "share-2" } else { "download" },
                        18.,
                        color,
                    ))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(download.name.clone()),
                    ),
            )
            .child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(t.control_hover)
                    .child(
                        div()
                            .h_full()
                            .w(relative(progress))
                            .rounded_full()
                            .bg(color),
                    ),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(format!(
                        "{state} · {} of {} · {:.0}%",
                        format_bytes(done),
                        format_bytes(total),
                        progress * 100.
                    )),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(format!(
                        "Download {}/s · Upload {}/s · {} uploaded · {} peers",
                        format_bytes(download.download_bytes_per_second),
                        format_bytes(download.upload_bytes_per_second),
                        format_bytes(download.uploaded_bytes),
                        download.peers
                    )),
            )
            .when_some(download.error.clone(), |d, error| {
                d.child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.danger)
                        .child(error),
                )
            })
            .child(actions);
        if self.delete_transfer.as_ref() == Some(&id) {
            let mut confirmation = v_flex()
                .gap_2()
                .p_3()
                .rounded(px(t.radius))
                .bg(t.control)
                .child(
                    div()
                        .text_color(t.danger)
                        .child("Permanently delete these torrent files? This cannot be undone."),
                );
            for file in &download.files {
                confirmation = confirmation.child(
                    div()
                        .text_size(t.font(12.))
                        .child(file.path.display().to_string()),
                );
            }
            confirmation = confirmation
                .child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child("Only listed files are deleted. Other files and folders are kept."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(self.button(
                            SharedString::from(format!("cancel-delete-{id}")),
                            None,
                            "Cancel",
                            false,
                            cx,
                            |this, _, cx| {
                                this.delete_transfer = None;
                                cx.notify();
                            },
                        ))
                        .child(self.button(
                            SharedString::from(format!("confirm-delete-{id}")),
                            None,
                            "Delete Listed Files",
                            false,
                            cx,
                            move |this, _, cx| {
                                this.delete_transfer = None;
                                this.send(
                                    ServiceCommand::Remove {
                                        id: id.clone(),
                                        delete_files: true,
                                    },
                                    cx,
                                );
                            },
                        )),
                );
            entry = entry.child(confirmation);
        }
        entry.into_any_element()
    }

    fn torrent_sheet(&self, torrent: &flick::torrent::ResolvedTorrent, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let total = torrent.files.iter().map(|file| file.length).sum::<u64>();
        let mut rows = vec![
            row(
                &t,
                Some("magnet"),
                torrent.name.clone(),
                Some(
                    format!(
                        "{} files · {} · choose what to download",
                        torrent.files.len(),
                        format_bytes(total)
                    )
                    .into(),
                ),
                div(),
            )
            .into_any_element(),
        ];
        for file in &torrent.files {
            let index = file.index;
            let id = ElementId::from(("torrent-file", index));
            let el = h_flex()
                .id(id.clone())
                .relative()
                .gap_3()
                .px_3()
                .py_2()
                .cursor_pointer()
                .hover(|s| s.bg(t.selection))
                .child(checkbox(&t, self.selected_files.contains(&index)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(file.relative_path.to_string_lossy().into_owned()),
                )
                .child(div().text_color(t.muted).child(format_bytes(file.length)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.selected_files.remove(&index) {
                        this.selected_files.insert(index);
                    }
                    cx.notify();
                }));
            rows.push(
                self.focusable(el, &id, None, t.radius, (-RING_INSET, -RING_INSET), cx)
                    .into_any_element(),
            );
        }
        rows.push(
            v_flex()
                .pb_3()
                .child(row(
                    &t,
                    Some("folder-open"),
                    "Save to",
                    None,
                    self.button(
                        "choose-destination",
                        None,
                        "Choose…",
                        false,
                        cx,
                        |this, w, cx| this.choose_download_folder(w, cx),
                    ),
                ))
                .child(div().px_3().child(self.text_field(&self.folder_field, cx)))
                .into_any_element(),
        );
        v_flex()
            .child(section_title(&t, "New Download"))
            .child(group(&t, rows))
            .child(
                h_flex()
                    .mt_3()
                    .gap_2()
                    .justify_end()
                    .child(self.button(
                        "cancel-download",
                        None,
                        "Cancel",
                        false,
                        cx,
                        |this, _, cx| {
                            this.pending_torrent = None;
                            cx.notify();
                        },
                    ))
                    .child(self.button(
                        "confirm-download",
                        Some("download"),
                        "Download Selected",
                        true,
                        cx,
                        |this, _, cx| {
                            if this.selected_files.is_empty() {
                                this.error = Some("Select at least one file to download.".into());
                                cx.notify();
                                return;
                            }
                            if let Some(torrent) = this.pending_torrent.take() {
                                let mut files =
                                    this.selected_files.iter().copied().collect::<Vec<_>>();
                                files.sort_unstable();
                                this.send(
                                    ServiceCommand::Download {
                                        torrent,
                                        files,
                                        folder: this.download_folder.clone(),
                                    },
                                    cx,
                                );
                            }
                        },
                    )),
            )
    }
}

/// A context-menu item's action.
type MenuAction = dyn Fn(&mut Flick, &mut Window, &mut Context<Flick>);

/// Rows inside grouped lists draw their focus ring inside the group.
pub(super) const RING_INSET: f32 = 4.;

fn search_pane_layout(content: Div, link: bool) -> Stateful<Div> {
    v_flex()
        .id("video-search-scroll")
        .min_h(px(0.))
        .w_full()
        .overflow_y_scroll()
        .when(link, |d| d.flex_1())
        .when(!link, |d| d.flex_none().max_h(relative(0.45)))
        // Intrinsic content must overflow the pane, never shrink to its viewport.
        .child(content.flex_none())
}

fn scroll_key_offset(key: &str, offset: Pixels, max: Pixels, viewport: Pixels) -> Option<Pixels> {
    let page = viewport * 0.9;
    Some(match key {
        "pageup" => (offset + page).min(px(0.)),
        "pagedown" => (offset - page).max(-max),
        "home" => px(0.),
        "end" => -max,
        _ => return None,
    })
}

/// Arrow-key movement within a card grid.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GridStep {
    Left,
    Right,
    Up,
    Down,
}

impl GridStep {
    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            _ => return None,
        })
    }

    /// The card to focus from `index` in a row-major grid of `count` cards,
    /// or `None` to leave the grid. Down from a row above a shorter last row
    /// goes to the last card.
    fn target(self, index: usize, cols: usize, count: usize) -> Option<usize> {
        let cols = cols.max(1);
        match self {
            Self::Left => index.checked_sub(1),
            Self::Right => Some(index + 1).filter(|next| *next < count),
            Self::Up => index.checked_sub(cols),
            Self::Down if index + cols < count => Some(index + cols),
            Self::Down if index / cols < count.saturating_sub(1) / cols => Some(count - 1),
            Self::Down => None,
        }
    }
}

/// Flick's app icon: a play glyph on a warm-to-violet gradient.
fn app_icon(size: f32, radius: f32) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(radius))
        .bg(linear_gradient(
            145.,
            linear_color_stop(rgb(0xff8a4c), 0.),
            linear_color_stop(rgb(0x6a5cff), 1.),
        ))
        .when(size > 24., |d| d.shadow_md())
        .child(icon("play", (size * 0.5).round(), rgb(0xffffff)))
}

/// Column count and card width that fill `width` with cards at least `min` wide.
fn columns(width: f32, min: f32, gap: f32) -> (usize, f32) {
    let cols = ((width + gap) / (min + gap)).floor().max(1.) as usize;
    (
        cols,
        ((width - gap * (cols as f32 - 1.)) / cols as f32).floor(),
    )
}

fn card_grid(cards: Vec<AnyElement>, cols: usize, gap: f32) -> Div {
    let mut grid = v_flex().gap(px(gap + 6.));
    let mut cards = cards.into_iter().peekable();
    while cards.peek().is_some() {
        grid = grid.child(
            h_flex()
                .items_start()
                .gap(px(gap))
                .children(cards.by_ref().take(cols)),
        );
    }
    grid
}

/// Where an online result comes from and how it downloads.
pub(super) fn result_detail(result: &flick::catalog::SearchResult) -> String {
    if result.archive_id.is_some() {
        format!(
            "{} · Downloads over HTTPS · choose files first",
            result.source
        )
    } else if result.torrent_url.is_some() {
        format!(
            "{} · File sizes are shown before downloading",
            result.source
        )
    } else {
        format!(
            "{} · {} · {} seeders",
            result.source,
            format_bytes(result.size_bytes),
            result.seeders
        )
    }
}

/// A magnet link or a bare 40-character infohash.
pub(super) fn is_torrent_link(text: &str) -> bool {
    text.starts_with("magnet:") || (text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn empty_state(t: &Theme, name: &'static str, title: &'static str, detail: &'static str) -> Div {
    v_flex()
        .items_center()
        .gap_2()
        .py_12()
        .text_center()
        .child(icon(name, 40., t.muted))
        .child(
            div()
                .mt_1()
                .text_size(t.font(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(div().max_w(px(380.)).text_color(t.muted).child(detail))
}

pub(super) fn checkbox(t: &Theme, checked: bool) -> Div {
    let (size, radius) = match t.style {
        Style::Mac => (14., 4.),
        Style::Windows => (20., 4.),
        Style::Gnome => (18., 5.),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(radius))
        .when(checked, |d| d.bg(t.accent))
        .when(!checked, |d| d.border_1().border_color(t.muted))
        .when(checked, |d| d.child(icon("check", size - 4., t.on_accent)))
}

#[cfg(test)]
mod tests {
    use super::{
        FocusSlot, GridStep, LibraryUiState, POSTER_CACHE_CAPACITY, PosterLru, VideoFilter,
        action_id, first_visible_row, grid_row_range, scroll_key_offset, tab_target,
        validate_library_files, video_id,
    };
    use crate::store::{Settings, Sort};
    use gpui::{Resource, UniformListScrollHandle, point, px};
    use std::{path::PathBuf, rc::Rc};

    #[test]
    fn keyboard_paging_reaches_last_result_and_chooser_actions_without_overscroll() {
        let viewport = px(240.);
        // 150 remote rows followed by destination and confirmation actions.
        let content = px(150. * 64. + 140.);
        let max = content - viewport;
        let mut offset = px(0.);
        for _ in 0..100 {
            offset = scroll_key_offset("pagedown", offset, max, viewport).unwrap();
            assert!(offset >= -max && offset <= px(0.));
        }
        assert_eq!(offset, -max);
        assert_eq!(-offset + viewport, content);
        assert_eq!(
            scroll_key_offset("home", offset, max, viewport),
            Some(px(0.))
        );
        assert_eq!(scroll_key_offset("end", px(0.), max, viewport), Some(-max));
        for _ in 0..100 {
            offset = scroll_key_offset("pageup", offset, max, viewport).unwrap();
        }
        assert_eq!(offset, px(0.));
        for key in ["pageup", "pagedown", "home", "end"] {
            assert_eq!(
                scroll_key_offset(key, px(0.), px(0.), viewport),
                Some(px(0.))
            );
        }
        // Tab and arrows must continue to their existing focus/navigation handlers.
        assert_eq!(scroll_key_offset("tab", offset, max, viewport), None);
        assert_eq!(scroll_key_offset("down", offset, max, viewport), None);
    }

    #[test]
    fn arrow_keys_move_through_a_card_grid() {
        // 7 cards in 3 columns: rows [0 1 2] [3 4 5] [6].
        assert_eq!(GridStep::Right.target(1, 3, 7), Some(2));
        assert_eq!(GridStep::Right.target(6, 3, 7), None);
        assert_eq!(GridStep::Left.target(3, 3, 7), Some(2));
        assert_eq!(GridStep::Left.target(0, 3, 7), None);
        assert_eq!(GridStep::Up.target(4, 3, 7), Some(1));
        assert_eq!(GridStep::Up.target(2, 3, 7), None);
        assert_eq!(GridStep::Down.target(1, 3, 7), Some(4));
        assert_eq!(GridStep::Down.target(5, 3, 7), Some(6));
        assert_eq!(GridStep::Down.target(6, 3, 7), None);
        assert_eq!(GridStep::Down.target(0, 0, 1), None);
    }

    #[test]
    fn virtual_grid_rows_cover_large_libraries_without_overrun() {
        for count in [0usize, 1, 100, 1_000, 5_000, 10_000] {
            for columns in [1, 3, 7] {
                let rows = count.div_ceil(columns);
                let items = (0..rows)
                    .flat_map(|row| grid_row_range(row, columns, count))
                    .collect::<Vec<_>>();
                assert_eq!(items, (0..count).collect::<Vec<_>>());
                assert_eq!(grid_row_range(rows + 1, columns, count), count..count);
            }
        }
        assert_eq!(grid_row_range(1, 3, 5), 3..5);
    }

    #[test]
    fn query_cache_reuses_results_and_invalidates_on_query_filter_sort_and_revision() {
        let mut data = Settings::default();
        let alpha = PathBuf::from("/fixture/Alpha.mkv");
        let beta = PathBuf::from("/fixture/Beta.mkv");
        data.restore_videos([alpha.clone(), beta.clone()]);
        let state = LibraryUiState::default();
        let added = state.video_paths(&data, "", VideoFilter::All);
        assert_eq!(&*added, &[beta.clone(), alpha.clone()]);
        assert!(Rc::ptr_eq(
            &added,
            &state.video_paths(&data, "", VideoFilter::All)
        ));
        let query = state.video_paths(&data, "ALPHA", VideoFilter::All);
        assert_eq!(&*query, std::slice::from_ref(&alpha));
        assert!(Rc::ptr_eq(
            &query,
            &state.video_paths(&data, "alpha", VideoFilter::All)
        ));
        data.library_view.sort = Sort::Title;
        assert_eq!(
            &*state.video_paths(&data, "", VideoFilter::All),
            &[alpha.clone(), beta.clone()]
        );
        data.remember(&alpha, 30., 100.);
        assert_eq!(
            &*state.video_paths(&data, "", VideoFilter::InProgress),
            std::slice::from_ref(&alpha)
        );
        data.mark_watched(&beta);
        assert_eq!(
            &*state.video_paths(&data, "", VideoFilter::Unwatched),
            std::slice::from_ref(&alpha)
        );
        let before = state.video_paths(&data, "", VideoFilter::All);
        data.remove_video(&alpha);
        let after = state.video_paths(&data, "", VideoFilter::All);
        assert!(!Rc::ptr_eq(&before, &after));
        assert_eq!(&*after, &[beta]);
    }

    #[test]
    fn poster_lru_evicts_cold_entries_and_never_exceeds_capacity() {
        let mut lru = PosterLru::default();
        let resource = |index| Resource::from(PathBuf::from(format!("/posters/{index}.png")));
        for index in 0..POSTER_CACHE_CAPACITY {
            assert_eq!(lru.touch(&resource(index)), None);
        }
        assert_eq!(lru.touch(&resource(0)), None);
        assert_eq!(
            lru.touch(&resource(POSTER_CACHE_CAPACITY)),
            Some(resource(1))
        );
        for index in POSTER_CACHE_CAPACITY + 1..5_000 {
            assert!(lru.touch(&resource(index)).is_some());
            assert_eq!(lru.0.len(), POSTER_CACHE_CAPACITY);
        }
    }

    #[test]
    fn large_viewports_pin_every_visible_poster_and_evict_old_viewports() {
        let mut lru = PosterLru::default();
        let resource = |index| Resource::from(PathBuf::from(format!("/posters/{index}.png")));
        let visible = (0..180)
            .map(resource)
            .collect::<std::collections::HashSet<_>>();
        for index in 0..180 {
            assert_eq!(lru.touch_protected(&resource(index), &visible), None);
        }
        for _ in 0..3 {
            for index in 0..180 {
                assert_eq!(lru.touch_protected(&resource(index), &visible), None);
            }
            assert_eq!(lru.0.len(), 180);
        }
        let next = (180..360)
            .map(resource)
            .collect::<std::collections::HashSet<_>>();
        for index in 180..360 {
            assert!(lru.touch_protected(&resource(index), &next).is_some());
        }
        assert_eq!(lru.0.len(), 180);
        assert!(lru.0.iter().all(|entry| next.contains(entry)));
    }

    #[test]
    fn tab_moves_through_actions_and_across_unrendered_row_boundaries() {
        use FocusSlot::*;
        assert_eq!(
            tab_target(99, 10_000, Primary, false, true),
            Some((99, Remove))
        );
        assert_eq!(
            tab_target(99, 10_000, Remove, false, true),
            Some((99, Delete))
        );
        assert_eq!(
            tab_target(99, 10_000, Delete, false, true),
            Some((100, Primary))
        );
        assert_eq!(
            tab_target(100, 10_000, Primary, true, true),
            Some((99, Delete))
        );
        assert_eq!(
            tab_target(100, 10_000, Delete, true, true),
            Some((100, Remove))
        );
        assert_eq!(
            tab_target(100, 10_000, Remove, true, true),
            Some((100, Primary))
        );
        assert_eq!(tab_target(0, 10_000, Primary, true, true), None);
        assert_eq!(tab_target(9_999, 10_000, Delete, false, true), None);
        assert_eq!(
            tab_target(99, 10_000, Primary, false, false),
            Some((100, Primary))
        );
    }

    #[test]
    fn virtual_measurement_follows_scroll_and_clamps_after_filtering() {
        let scroll = UniformListScrollHandle::new();
        scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-8_000.)));
        assert_eq!(first_visible_row(&scroll, px(80.), 10_000), 100);
        assert_eq!(first_visible_row(&scroll, px(80.), 5), 4);
        assert_eq!(first_visible_row(&scroll, px(80.), 0), 0);
    }

    #[test]
    fn stable_focus_ids_follow_paths_and_scope_actions_to_the_section() {
        let path = PathBuf::from("/fixture/show.mkv");
        assert_eq!(video_id("library", &path), video_id("library", &path));
        assert_ne!(video_id("library", &path), video_id("history", &path));
        assert_ne!(
            action_id("library", &path, FocusSlot::Delete),
            action_id("history", &path, FocusSlot::Delete)
        );
        assert_ne!(
            action_id("library", &path, FocusSlot::Delete),
            action_id("library", &path, FocusSlot::Remove)
        );
    }

    #[test]
    fn batched_import_keeps_valid_file_and_error_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("First.mkv");
        let second = dir.path().join("Second.mp4");
        let invalid = dir.path().join("Notes.txt");
        let missing = dir.path().join("Missing.mkv");
        for path in [&first, &second, &invalid] {
            std::fs::write(path, b"local import fixture").unwrap();
        }
        let (videos, errors) =
            validate_library_files(vec![second.clone(), invalid, first.clone(), missing]);
        assert_eq!(
            videos,
            vec![
                second.canonicalize().unwrap(),
                first.canonicalize().unwrap()
            ]
        );
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("Notes.txt"));
        assert!(errors[1].contains("Missing.mkv"));
    }

    #[test]
    fn ten_thousand_video_queries_reuse_the_result_set_until_data_changes() {
        let mut data = Settings::default();
        data.restore_videos(
            (0..10_000).map(|index| PathBuf::from(format!("/fixture/clip{index:05}.mkv"))),
        );
        let state = LibraryUiState::default();
        let paths = state.video_paths(&data, "clip", VideoFilter::All);
        assert_eq!(paths.len(), 10_000);
        for _ in 0..100 {
            assert!(Rc::ptr_eq(
                &paths,
                &state.video_paths(&data, "clip", VideoFilter::All)
            ));
        }
        assert_eq!(
            &*state.video_paths(&data, "CLIP09999", VideoFilter::All),
            &[PathBuf::from("/fixture/clip09999.mkv")]
        );
        data.library[9_999].title = "Renamed".to_owned();
        data.invalidate();
        assert!(
            state
                .video_paths(&data, "clip09999", VideoFilter::All)
                .is_empty()
        );
        assert_eq!(
            &*state.video_paths(&data, "renamed", VideoFilter::All),
            &[PathBuf::from("/fixture/clip09999.mkv")]
        );
    }

    #[test]
    fn scroll_positions_survive_query_invalidation_and_remain_separate_per_page_and_show() {
        let state = LibraryUiState::default();
        let history = state.scroll("history");
        history
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-880.)));
        let show = state.scroll("episodes:a");
        show.0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-176.)));
        state.invalidate();
        assert!(Rc::ptr_eq(&history.0, &state.scroll("history").0));
        assert_eq!(
            state.scroll("history").0.borrow().base_handle.offset().y,
            px(-880.)
        );
        assert_eq!(
            state.scroll("episodes:a").0.borrow().base_handle.offset().y,
            px(-176.)
        );
        assert_eq!(
            state.scroll("episodes:b").0.borrow().base_handle.offset().y,
            px(0.)
        );
        assert_eq!(
            state.scroll("library").0.borrow().base_handle.offset().y,
            px(0.)
        );
    }

    #[test]
    fn show_watched_counts_refresh_after_history_changes() {
        let mut data = Settings::default();
        let first = PathBuf::from("/fixture/Example.S01E01.mkv");
        let second = PathBuf::from("/fixture/Example.S01E02.mkv");
        data.restore_videos([first.clone(), second]);
        let show = data.shows()[0].clone();
        let state = LibraryUiState::default();
        assert_eq!(state.show_unwatched(&data, &show), 2);
        data.mark_watched(&first);
        assert_eq!(state.show_unwatched(&data, &show), 1);
        data.mark_unwatched(&first);
        assert_eq!(state.show_unwatched(&data, &show), 2);
    }
}
