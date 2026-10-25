//! UI half of library file monitoring and confirmed Move to Trash.
use super::*;
use crate::library_files::{
    Change, FileGenerations, LibraryFiles, ObservationGenerations, TrackedFile, TrashTicket,
};
use std::path::Path;
use tokio::sync::oneshot;

type Release = oneshot::Receiver<Result<(), String>>;

#[derive(Default)]
pub(super) struct LibraryFilesUiState {
    worker: Option<LibraryFiles>,
    revision: Option<u64>,
    generations: FileGenerations,
    observations: ObservationGenerations,
    published: HashMap<PathBuf, TrackedFile>,
    pending: VecDeque<crate::library_files::Observation>,
    proofs:
        Option<std::collections::hash_map::IntoValues<PathBuf, crate::library_files::Observation>>,
    request: u64,
    active: bool,
    unavailable: HashSet<PathBuf>,
    dialog: Option<DeleteDialog>,
}

struct DeleteDialog {
    path: PathBuf,
    request: u64,
    generation: u64,
    phase: DeletePhase,
}

enum DeletePhase {
    Checking,
    Confirm(TrashTicket),
    Working,
    Failed(String),
}

impl Drop for LibraryFilesUiState {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.shutdown();
        }
    }
}

impl Flick {
    pub(super) fn init_library_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.library_files_state.worker.is_some() {
            return;
        }
        match LibraryFiles::start() {
            Ok(worker) => {
                self.library_files_state.worker = Some(worker);
                self.library_files_state.active = window.is_window_active();
                self.library_files_changed(cx);
                if let Some(worker) = &self.library_files_state.worker {
                    worker.reconcile();
                }
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    /// Called by library_changed, including explicit open/import. Publishing
    /// generations is synchronous; no stat, canonicalization or directory read.
    pub(super) fn library_files_changed(&mut self, _cx: &mut Context<Self>) {
        let state = &mut self.library_files_state;
        let Some(worker) = &state.worker else {
            return;
        };
        let revision = self.store.data.revision();
        state.revision = Some(revision);
        state
            .generations
            .retain(|path| self.store.data.video(path).is_some());
        state
            .observations
            .retain(|path| self.store.data.video(path).is_some());
        let files: Vec<_> = self
            .store
            .data
            .library
            .iter()
            .map(|v| {
                let generation = state.generations.observe(
                    &v.path,
                    v.torrent
                        .as_ref()
                        .map(|id| (id.infohash.as_str(), id.file_index)),
                );
                let incomplete = self.incomplete_files.contains(&v.path);
                let snapshot = state.observations.observe(
                    &v.path,
                    generation,
                    v.poster.as_deref(),
                    incomplete,
                );
                TrackedFile {
                    path: v.path.clone(),
                    snapshot,
                    generation,
                    poster: v.poster.clone(),
                    incomplete,
                    location: v.location.clone(),
                }
            })
            .collect();
        state
            .unavailable
            .retain(|path| self.store.data.video(path).is_some());
        let current: HashSet<_> = files.iter().map(|file| file.path.clone()).collect();
        let removed = state
            .published
            .keys()
            .filter(|path| !current.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        let mut changed = Vec::new();
        for file in files {
            if state.published.get(&file.path).is_none_or(|old| {
                old.snapshot != file.snapshot
                    || old.generation != file.generation
                    || old.location != file.location
            }) {
                state.published.insert(file.path.clone(), file.clone());
                changed.push(file);
            }
        }
        for path in &removed {
            state.published.remove(path);
        }
        if state.published.len() == changed.len() && removed.is_empty() {
            worker.sync(changed);
        } else if !changed.is_empty() || !removed.is_empty() {
            worker.update(changed, removed);
        }
    }

    /// Explicit open/import can reintroduce an existing path before its old
    /// Missing callback arrives. Recheck this path without invalidating others.
    pub(super) fn library_files_restored<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a PathBuf>,
    ) {
        for path in paths {
            self.library_files_state.observations.refresh(path);
        }
    }

    pub(super) fn reconcile_library_files(&self) {
        if let Some(worker) = &self.library_files_state.worker {
            worker.reconcile();
        }
    }

    fn library_file_generation(&self, path: &Path) -> Option<u64> {
        self.library_files_state.generations.get(path)
    }

    pub(super) fn library_file_unavailable(&self, path: &Path) -> bool {
        self.library_files_state.unavailable.contains(path)
    }

    pub(super) fn poll_library_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.library_files_state.revision != Some(self.store.data.revision()) {
            self.library_files_changed(cx);
        }
        let active = window.is_window_active();
        if active
            && !self.library_files_state.active
            && let Some(worker) = &self.library_files_state.worker
        {
            worker.reconcile_focus();
        }
        self.library_files_state.active = active;
        let Some(worker) = self.library_files_state.worker.clone() else {
            return;
        };
        let (observations, prepared) = worker.drain();
        // Initial positive proofs have their own queue. A deleted-file report
        // never waits behind thousands of ordinary startup observations.
        if self.library_files_state.proofs.is_none() {
            self.library_files_state.proofs = Some(worker.drain_proofs().into_values());
        }
        let proof_start = Instant::now();
        let mut acknowledged = Vec::new();
        let mut proof_locations = false;
        while proof_start.elapsed() < Duration::from_millis(3) {
            let Some(observation) = self
                .library_files_state
                .proofs
                .as_mut()
                .and_then(Iterator::next)
            else {
                self.library_files_state.proofs = None;
                break;
            };
            if self.library_file_generation(&observation.path) != Some(observation.generation)
                || self.library_files_state.observations.get(&observation.path)
                    != Some(observation.snapshot)
                || self.store.data.is_removed(&observation.path)
            {
                continue;
            }
            if let Some(location) = &observation.location {
                proof_locations |= self
                    .store
                    .data
                    .remember_location(&observation.path, location.clone());
                if let Some(published) = self
                    .library_files_state
                    .published
                    .get_mut(&observation.path)
                {
                    published.location = Some(location.clone());
                }
            }
            acknowledged.push(observation);
        }
        worker.acknowledge_many(acknowledged);
        for observation in observations {
            if matches!(observation.change, Change::Missing | Change::Available) {
                self.library_files_state.pending.push_front(observation);
            } else {
                self.library_files_state.pending.push_back(observation);
            }
        }
        let apply_started = Instant::now();
        let mut missing = Vec::new();
        let mut removed = false;
        let mut metadata = false;
        let mut availability = false;
        let mut locations_changed = proof_locations;
        // Apply one model generation as a batch. Publish its successor only
        // after all observations from that generation have been considered.
        while apply_started.elapsed() < Duration::from_millis(4) {
            let Some(observation) = self.library_files_state.pending.pop_front() else {
                break;
            };
            if self.library_file_generation(&observation.path) != Some(observation.generation)
                || Some(observation.snapshot)
                    != self.library_files_state.observations.get(&observation.path)
                || self.store.data.video(&observation.path).is_none()
                || self.store.data.is_removed(&observation.path)
            {
                continue;
            }
            if let Some(location) = &observation.location {
                locations_changed |= self
                    .store
                    .data
                    .remember_location(&observation.path, location.clone());
            }
            match &observation.change {
                Change::Available => {
                    if self
                        .library_files_state
                        .unavailable
                        .remove(&observation.path)
                    {
                        self.poster_requests.remove(&observation.path);
                        self.posters_dirty = true;
                        availability = true;
                    }
                }
                Change::Unavailable(_reason) => {
                    availability |= self
                        .library_files_state
                        .unavailable
                        .insert(observation.path.clone());
                    self.thumbnails.cancel(&observation.path);
                    self.poster_requests.remove(&observation.path);
                    self.poster_queue.retain(|p| p != &observation.path);
                }
                Change::Missing => {
                    if self.incomplete_files.contains(&observation.path) {
                        self.reconcile_library_files();
                        continue;
                    }
                    // The filesystem worker proves readable stable local-parent
                    // absence twice. Settings owns path/identity tombstones.
                    missing.push(observation.path.clone());
                    removed = true;
                }
                Change::Metadata {
                    media,
                    poster,
                    replaced,
                } => {
                    let cached = self
                        .store
                        .data
                        .video(&observation.path)
                        .and_then(|v| v.poster.clone());
                    self.thumbnails.cancel(&observation.path);
                    self.poster_requests.remove(&observation.path);
                    self.poster_queue.retain(|p| p != &observation.path);
                    if let Some(cached) = cached {
                        self.evict_library_poster(&cached, window, cx);
                    }
                    if let Some(video) = self
                        .store
                        .data
                        .library
                        .iter_mut()
                        .find(|v| v.path == observation.path)
                    {
                        if *media || *poster {
                            video.poster = None;
                        }
                        if *media {
                            video.duration = 0.;
                            video.width = 0;
                            video.height = 0;
                        }
                    }
                    if *replaced && self.path.as_deref() == Some(observation.path.as_path()) {
                        // Do not continue showing/remembering the old inode as
                        // the newly replaced video. Explicit Open reloads it.
                        let _ = self.release_library_playback(&observation.path, window, cx);
                    }
                    if *replaced {
                        self.library_files_state
                            .generations
                            .remove(&observation.path);
                    }
                    metadata = true;
                    self.posters_dirty = true;
                }
            }
            worker.acknowledge(observation);
        }
        if locations_changed {
            // Proofs do not change derived UI data or model incarnations. Persist
            // as a batch without invalidating pending observations from this sweep.
            self.posters_unsaved = true;
        }
        if removed {
            self.remove_library_records(&missing, window, cx);
            self.library_changed(cx);
            self.persist();
        } else if metadata {
            self.store.data.invalidate_metadata();
            self.library_ui_state.invalidate();
            self.library_files_changed(cx);
            self.posters_unsaved = true;
            cx.notify();
        } else if availability {
            cx.notify();
        }
        for result in prepared {
            let Some(dialog) = &mut self.library_files_state.dialog else {
                continue;
            };
            if result.request != dialog.request || !matches!(dialog.phase, DeletePhase::Checking) {
                continue;
            }
            if self.library_files_state.generations.get(&dialog.path) != Some(result.generation)
                || dialog.generation != result.generation
            {
                dialog.phase = DeletePhase::Failed("This library item changed while it was being checked. Close this dialog and open Delete again.".into());
            } else {
                dialog.phase = match result.result {
                    Ok(ticket) => DeletePhase::Confirm(ticket),
                    Err(error) => DeletePhase::Failed(error),
                };
            }
            cx.notify();
        }
    }

    pub(super) fn request_delete_video(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .library_files_state
            .dialog
            .as_ref()
            .is_some_and(|d| matches!(d.phase, DeletePhase::Working))
        {
            return;
        }
        self.card_menu = None;
        if self.store.data.video(&path).is_none() {
            return;
        }
        if self.library_files_state.revision != Some(self.store.data.revision()) {
            self.library_files_changed(cx);
        }
        self.library_files_state.request = self.library_files_state.request.wrapping_add(1);
        let request = self.library_files_state.request;
        let Some(generation) = self.library_file_generation(&path) else {
            return;
        };
        let phase = if let Some(worker) = &self.library_files_state.worker {
            worker.prepare(path.clone(), generation, request);
            DeletePhase::Checking
        } else {
            DeletePhase::Failed(
                "File monitoring could not start. Your file was kept; restart Flick and retry."
                    .into(),
            )
        };
        self.control_handle(&ElementId::from("library-delete-close"), cx)
            .focus(window);
        self.library_files_state.dialog = Some(DeleteDialog {
            path,
            request,
            generation,
            phase,
        });
        cx.notify();
    }

    pub(super) fn remove_library_video(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .library_files_state
            .dialog
            .as_ref()
            .is_some_and(|d| matches!(d.phase, DeletePhase::Working))
        {
            return;
        }
        self.card_menu = None;
        self.remove_library_record(&path, window, cx);
        self.library_changed(cx);
        self.persist();
        cx.notify();
    }

    fn remove_library_record(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_library_records(&[path.to_owned()], window, cx);
    }

    fn remove_library_records(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.library_removal_epoch = self.library_removal_epoch.wrapping_add(1);
        self.open_generation = self.open_generation.wrapping_add(1);
        let requested: HashSet<_> = paths.iter().collect();
        let identities: HashSet<_> = paths
            .iter()
            .filter_map(|path| {
                self.store
                    .data
                    .video(path)
                    .and_then(|video| video.torrent.clone())
            })
            .collect();
        let affected: Vec<_> = self
            .store
            .data
            .library
            .iter()
            .filter(|video| {
                requested.contains(&video.path)
                    || video
                        .torrent
                        .as_ref()
                        .is_some_and(|identity| identities.contains(identity))
            })
            .map(|video| (video.path.clone(), video.poster.clone()))
            .collect();
        for path in paths.iter().chain(affected.iter().map(|(path, _)| path)) {
            self.removal_generations
                .insert(path.clone(), self.library_removal_epoch);
        }
        // Clear an owning active path before any later persist/remember can run.
        for (path, poster) in &affected {
            let _ = self.release_library_playback(path, window, cx);
            self.thumbnails.cancel(path);
            self.poster_requests.remove(path);
            self.poster_queue.retain(|queued| queued != path);
            self.library_files_state.unavailable.remove(path);
            self.library_files_state.generations.remove(path);
            if let Some(poster) = poster {
                self.evict_library_poster(poster, window, cx);
            }
        }
        self.store
            .data
            .remove_videos(paths.iter().map(PathBuf::as_path));
        if self.selected_show.as_ref().is_some_and(|key| {
            !self.store.data.shows().iter().any(|show| {
                show.episodes
                    .first()
                    .is_some_and(|(episode, _)| episode.show_key() == *key)
            })
        }) {
            self.selected_show = None;
        }
        self.thumbnails
            .retain_sources(self.store.data.library.iter().map(|v| v.path.clone()));
    }

    /// Cancels only resources belonging to this path. PiP for another video
    /// remains open. The returned acknowledgement is awaited before Trash.
    fn release_library_playback(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Release> {
        if self.up_next.as_ref().is_some_and(|(next, _)| next == path) {
            self.up_next = None;
        }
        if self.pending_open.as_deref() == Some(path) {
            self.open_generation = self.open_generation.wrapping_add(1);
            self.pending_open = None;
        }
        if self.path.as_deref() != Some(path) {
            return None;
        }
        self.open_generation = self.open_generation.wrapping_add(1);
        let stopped = self.player.as_ref().map(|player| player.stop_for_delete());
        self.path = None;
        self.file_loaded = false;
        self.pending_resume = 0.;
        self.opened_at = None;
        self.up_next = None;
        self.was_eof = false;
        self.streaming = false;
        self.scrubbing = false;
        self.scrub_target = None;
        self.hover = None;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_shown = self.preview_generation;
        if let Some(previews) = &self.previews {
            previews.idle_close();
        }
        if let Some(image) = self.preview_image.take() {
            let _ = window.drop_image(image);
        }
        self.close_pip(false, cx);
        self.clear_frame(window, cx);
        self.snapshot = PlaybackSnapshot::default();
        self.view = View::Library;
        self.menu = None;
        window.set_window_title("Flick");
        stopped
    }

    fn confirm_library_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.library_files_state.dialog else {
            return;
        };
        let DeletePhase::Confirm(ticket) = &dialog.phase else {
            return;
        };
        let ticket = ticket.clone();
        let request = dialog.request;
        if self.library_file_generation(&ticket.path) != Some(ticket.generation)
            || self.store.data.video(&ticket.path).is_none()
        {
            if let Some(dialog) = &mut self.library_files_state.dialog {
                dialog.phase = DeletePhase::Failed("The library changed. Close this dialog and open Delete again before moving this file.".into());
            }
            cx.notify();
            return;
        }
        let Some(worker) = self.library_files_state.worker.clone() else {
            return;
        };
        // Owners are captured before Progress can replace the transfer list.
        // Removing an owner MUST keep files; only ticket.path goes to Trash.
        if !self.transfers_known && !self.can_open_before_transfers(&ticket.path) {
            if let Some(dialog) = &mut self.library_files_state.dialog {
                dialog.phase = DeletePhase::Failed("Downloads are unavailable or still being checked. Retry downloads before deleting this file.".into());
            }
            cx.notify();
            return;
        }
        let owners: HashSet<_> = self
            .downloads
            .iter()
            .filter(|download| download.files.iter().any(|file| file.path == ticket.path))
            .map(|download| download.infohash.clone())
            .collect();
        if self.downloads.iter().any(|download| {
            owners.contains(&download.infohash)
                && matches!(download.state, flick::torrent::TransferState::Checking)
        }) {
            if let Some(dialog) = &mut self.library_files_state.dialog {
                dialog.phase = DeletePhase::Failed("Wait until file checking finishes, then retry Delete. Your file and its transfer were kept.".into());
            }
            cx.notify();
            return;
        }
        if self.downloads.iter().any(|download| {
            owners.contains(&download.infohash)
                && matches!(download.state, flick::torrent::TransferState::Error)
        }) {
            if let Some(dialog) = &mut self.library_files_state.dialog {
                dialog.phase = DeletePhase::Failed("Resume this transfer in Downloads so its files can be verified, then retry Delete. Your file and its transfer were kept.".into());
            }
            cx.notify();
            return;
        }
        // Releasing a transfer keeps its files. Persist a separate byte-safety
        // gate for ALL unverified siblings before that release can survive quit.
        for download in self
            .downloads
            .iter()
            .filter(|download| owners.contains(&download.infohash))
        {
            for file in &download.files {
                if matches!(
                    download.state,
                    flick::torrent::TransferState::Checking | flick::torrent::TransferState::Error
                ) || !file.selected
                    || file.verified_bytes != file.length
                {
                    self.store.data.quarantine_torrent_file(
                        &file.path,
                        &download.infohash,
                        file.index,
                    );
                }
            }
        }
        if let Err(error) = self.store.queue_save() {
            if let Some(dialog) = &mut self.library_files_state.dialog {
                dialog.phase = DeletePhase::Failed(format!(
                    "Could not save file verification state. Your file was kept: {error}"
                ));
            }
            cx.notify();
            return;
        }
        let barrier = self.store.save_barrier();
        self.transfers_changed(cx);
        self.thumbnails.cancel(&ticket.path);
        self.poster_requests.remove(&ticket.path);
        self.poster_queue.retain(|p| p != &ticket.path);
        let playback = self.release_library_playback(&ticket.path, window, cx);
        if let Some(dialog) = &mut self.library_files_state.dialog {
            dialog.phase = DeletePhase::Working;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                cx.background_executor()
                    .spawn(async move { barrier.wait() })
                    .await
                    .map_err(|error| {
                        format!("Could not save verification state. Your file was kept: {error}")
                    })?;
                let releases = this
                    .update_in(cx, |this, _, _| {
                        if this.library_file_generation(&ticket.path) != Some(ticket.generation)
                            || !this
                                .library_files_state
                                .dialog
                                .as_ref()
                                .is_some_and(|dialog| dialog.request == request)
                        {
                            return Err("The library changed. Your file was kept.".to_owned());
                        }
                        Ok(owners
                            .into_iter()
                            .map(|id| this.services.release_for_deletion(id))
                            .collect::<Vec<_>>())
                    })
                    .map_err(|_| "The window closed. Your file was kept.".to_owned())??;
                if let Some(playback) = playback {
                    await_release(playback, "Playback").await?;
                }
                for release in releases {
                    await_release(release, "The transfer").await?;
                }
                // The worker checks the latest synchronously published model
                // epoch and filesystem identity immediately before OS Trash.
                worker
                    .trash(ticket.clone())
                    .await
                    .map_err(|_| "The filesystem worker stopped. Your file was kept.".to_owned())?
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let current = this
                    .library_files_state
                    .dialog
                    .as_ref()
                    .is_some_and(|d| d.request == request);
                if !current {
                    return;
                }
                match result {
                    Ok(()) => {
                        if this.library_file_generation(&ticket.path) == Some(ticket.generation)
                            && this.store.data.video(&ticket.path).is_some()
                        {
                            this.remove_library_record(&ticket.path, window, cx);
                            this.library_changed(cx);
                            this.persist();
                        } else {
                            // Never tombstone a reimported/new incarnation.
                            if let Some(worker) = &this.library_files_state.worker {
                                worker.reconcile();
                            }
                        }
                        this.library_files_state.dialog = None;
                        this.show_status("Moved to Trash");
                    }
                    Err(error) => {
                        if let Some(dialog) = &mut this.library_files_state.dialog {
                            dialog.phase = DeletePhase::Failed(error);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Route this at the start of Flick::key so the modal owns keyboard input.
    pub(super) fn handle_library_delete_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &self.library_files_state.dialog else {
            return false;
        };
        let working = matches!(dialog.phase, DeletePhase::Working);
        let confirm = matches!(dialog.phase, DeletePhase::Confirm(_));
        let key = event.keystroke.key.as_str();
        cx.stop_propagation();
        if key == "escape" && !working {
            self.library_files_state.dialog = None;
            self.focus.focus(window);
            cx.notify();
        } else if key == "tab" && !working {
            let close = self.control_handle(&ElementId::from("library-delete-close"), cx);
            let accept = self.control_handle(&ElementId::from("library-delete-confirm"), cx);
            if confirm && window.focused(cx).as_ref() == Some(&close) {
                accept.focus(window);
            } else {
                close.focus(window);
            }
            self.keyboard_nav = true;
            cx.notify();
        } else if matches!(key, "enter" | "space") && !working {
            let accept = self.control_handle(&ElementId::from("library-delete-confirm"), cx);
            if confirm && window.focused(cx).as_ref() == Some(&accept) {
                self.confirm_library_delete(window, cx);
            } else {
                self.library_files_state.dialog = None;
                self.focus.focus(window);
                cx.notify();
            }
        }
        true
    }

    pub(super) fn library_delete_panel(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.library_files_state.dialog.as_ref()?;
        let t = cx.theme();
        let checking = matches!(dialog.phase, DeletePhase::Checking);
        let working = matches!(dialog.phase, DeletePhase::Working);
        let confirm = matches!(dialog.phase, DeletePhase::Confirm(_));
        let message = match &dialog.phase {
            DeletePhase::Checking => "Checking this file…".to_owned(),
            DeletePhase::Confirm(_) => "Move this video file to your system Trash? You can restore it there. Its library entry and viewing history will be removed. A transfer using this file will stop; other files will be kept.".to_owned(),
            DeletePhase::Working => "Releasing playback and transfers, then moving this file to Trash…".to_owned(),
            DeletePhase::Failed(error) => error.clone(),
        };
        let mut actions = h_flex().gap_2().justify_end();
        if !working {
            actions = actions.child(self.button(
                "library-delete-close",
                None,
                if confirm || checking {
                    "Cancel"
                } else {
                    "Close"
                },
                false,
                cx,
                |this, window, cx| {
                    this.library_files_state.dialog = None;
                    this.focus.focus(window);
                    cx.notify();
                },
            ));
        }
        if confirm {
            actions = actions.child(self.button(
                "library-delete-confirm",
                None,
                "Move to Trash",
                true,
                cx,
                |this, window, cx| this.confirm_library_delete(window, cx),
            ));
        }
        Some(
            div()
                .id("library-delete-overlay")
                .absolute()
                .inset_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000080))
                .occlude()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(
                    v_flex()
                        .w(px(480.))
                        .max_w_full()
                        .p_5()
                        .gap_4()
                        .rounded(px(t.radius))
                        .border_1()
                        .border_color(t.border)
                        .bg(t.popover)
                        .shadow_lg()
                        .child(div().text_size(t.font(18.)).child(if working {
                            "Moving to Trash"
                        } else {
                            "Delete File"
                        }))
                        .child(
                            div()
                                .text_color(t.muted)
                                .child(dialog.path.display().to_string()),
                        )
                        .child(div().child(message))
                        .child(actions),
                )
                .into_any_element(),
        )
    }
}

async fn await_release(receiver: Release, owner: &str) -> Result<(), String> {
    receiver
        .await
        .map_err(|_| {
            format!("{owner} stopped before acknowledging file release. Your file was kept.")
        })?
        .map_err(|error| {
            format!("{owner} could not release this file. Your file was kept. {error}")
        })
}
