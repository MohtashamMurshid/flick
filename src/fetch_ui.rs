//! Downloads from pasted web links on the Downloads page: video pages through
//! yt-dlp, and HTTPS files, including Internet Archive items.

use super::{
    chrome_ui::{group, notice, row, section_title},
    library_ui::{RING_INSET, checkbox},
    *,
};
use flick::{
    fetch::{FetchProgress, FetchSource, FetchStage, FetchUpdate, WebLink, archive::ArchiveItem},
    library::format_bytes,
    remote::protocol::{FetchPhase, FetchRow},
};
use gpui::{AnyElement, ElementId, FontWeight};

/// Link downloads, and an Internet Archive item awaiting file selection.
#[derive(Default)]
pub(super) struct Fetches {
    next_id: u64,
    /// Newest first.
    jobs: Vec<Job>,
    /// The Internet Archive item whose files are being listed.
    listing: Option<String>,
    item: Option<ArchiveItem>,
    chosen: HashSet<usize>,
}

impl Fetches {
    /// Running link downloads, for the sidebar badge.
    pub(super) fn active(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| matches!(job.state, JobState::Running | JobState::Cancelling))
            .count()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.listing.is_none() && self.item.is_none()
    }
}

struct Job {
    id: u64,
    title: String,
    source: FetchSource,
    folder: PathBuf,
    progress: FetchProgress,
    saved: Vec<PathBuf>,
    state: JobState,
    removal_epoch: u64,
}

#[derive(PartialEq)]
enum JobState {
    Running,
    Cancelling,
    Done,
    Failed(String),
}

impl Flick {
    /// Starts downloading an http(s) link typed into search, then clears the
    /// field. Returns false for any other text.
    pub(super) fn search_link(&mut self, query: &str, cx: &mut Context<Self>) -> bool {
        let Some(link) = flick::fetch::web_link(query) else {
            return false;
        };
        match link {
            Ok(link) => {
                self.search_input
                    .update(cx, |input, cx| input.set_value("", cx));
                self.query.clear();
                self.download_link(link, cx);
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
            }
        }
        true
    }

    /// Paste Link: a magnet link or a web address from the clipboard.
    pub(super) fn paste_link(&mut self, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let text = text.trim();
        if text.starts_with("magnet:") {
            self.resolve(flick::torrent::TorrentInput::Magnet(text.to_owned()), cx);
            return;
        }
        match flick::fetch::web_link(text) {
            Some(Ok(link)) => self.download_link(link, cx),
            Some(Err(error)) => self.error = Some(format!("{error:#}")),
            None => self.error = Some(
                "Copy a magnet link or a video page address first, then choose Paste Link again."
                    .into(),
            ),
        }
        cx.notify();
    }

    /// Internet Archive results download over HTTPS after choosing files.
    /// Other results go through the torrent session.
    pub(super) fn download_result(
        &mut self,
        result: &flick::catalog::SearchResult,
        cx: &mut Context<Self>,
    ) {
        match &result.archive_id {
            Some(identifier) => self.list_archive(identifier.clone(), cx),
            None => self.resolve(result.input(), cx),
        }
    }

    /// A pasted link: the Save to sheet when another machine takes links,
    /// else straight to this Mac.
    fn download_link(&mut self, link: WebLink, cx: &mut Context<Self>) {
        if self.open_link_sheet(&link, cx) {
            return;
        }
        self.download_link_here(link, cx);
    }

    /// Downloads a link on this Mac.
    pub(super) fn download_link_here(&mut self, link: WebLink, cx: &mut Context<Self>) {
        let title = match &link {
            WebLink::Archive(identifier) => return self.list_archive(identifier.clone(), cx),
            WebLink::Page(url) => url.clone(),
        };
        if let Some(source) = link.source() {
            self.start_fetch(title, source, self.download_folder.clone(), cx);
        }
    }

    /// Opens Downloads, pausing playback like a torrent download does.
    fn show_downloads(&mut self, cx: &mut Context<Self>) {
        if self.view == View::Player {
            self.persist();
            self.property("pause", "yes", cx);
        }
        self.view = View::Library;
        self.page = Page::Downloads;
        self.error = None;
    }

    fn start_fetch(
        &mut self,
        title: String,
        source: FetchSource,
        folder: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.show_downloads(cx);
        self.fetches.next_id += 1;
        let id = self.fetches.next_id;
        let state = match self.services.send(ServiceCommand::Fetch {
            id,
            source: source.clone(),
            folder: folder.clone(),
        }) {
            Ok(()) => JobState::Running,
            Err(error) => JobState::Failed(error),
        };
        self.fetches.jobs.insert(
            0,
            Job {
                id,
                title,
                source,
                folder,
                progress: FetchProgress::default(),
                saved: Vec::new(),
                state,
                removal_epoch: self.library_removal_epoch,
            },
        );
        cx.notify();
    }

    fn list_archive(&mut self, identifier: String, cx: &mut Context<Self>) {
        self.show_downloads(cx);
        self.fetches.item = None;
        self.fetches.listing = Some(identifier.clone());
        self.send(ServiceCommand::ResolveArchive(identifier), cx);
        if self.error.is_some() {
            self.fetches.listing = None;
        }
    }

    pub(super) fn fetch_event(&mut self, id: u64, update: FetchUpdate, cx: &mut Context<Self>) {
        let Some(job) = self.fetches.jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        match update {
            FetchUpdate::Title(title) => job.title = title,
            FetchUpdate::Progress(progress) => job.progress = progress,
            FetchUpdate::Saved(path) => {
                let restore =
                    self.removal_generations.get(&path).copied().unwrap_or(0) <= job.removal_epoch;
                job.saved.push(path.clone());
                if restore {
                    self.library_files_restored([&path]);
                    self.store.data.restore_video(&path);
                    self.library_changed(cx);
                    if let Err(error) = self.store.queue_save() {
                        self.error = Some(format!("Could not save downloaded video: {error}"));
                    }
                }
            }
            FetchUpdate::Done => job.state = JobState::Done,
            FetchUpdate::Failed(error) => job.state = JobState::Failed(error),
            FetchUpdate::Cancelled => self.fetches.jobs.retain(|job| job.id != id),
        }
    }

    pub(super) fn archive_listed(
        &mut self,
        identifier: String,
        result: Result<ArchiveItem, String>,
    ) {
        if self.fetches.listing.as_deref() != Some(identifier.as_str()) {
            return;
        }
        self.fetches.listing = None;
        match result {
            Ok(item) => {
                self.fetches.chosen = item.preferred().collect();
                self.fetches.item = Some(item);
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn download_archive(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.fetches.item.take() else {
            return;
        };
        let mut chosen = self.fetches.chosen.iter().copied().collect::<Vec<_>>();
        chosen.sort_unstable();
        match item.source(chosen) {
            Ok(source) => self.start_fetch(item.title, source, self.download_folder.clone(), cx),
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                self.fetches.item = Some(item);
                cx.notify();
            }
        }
    }

    /// The Internet Archive file chooser and link downloads.
    pub(super) fn link_downloads(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let mut section = v_flex().w_full();
        if self.fetches.listing.is_some() {
            section = section.child(notice(&t, false, "Reading the Internet Archive item…").mb_4());
        }
        if let Some(item) = &self.fetches.item {
            section = section
                .child(self.archive_sheet(item, cx))
                .child(div().h(px(28.)));
        }
        if !self.fetches.jobs.is_empty() {
            let rows = self
                .fetches
                .jobs
                .iter()
                .map(|job| self.fetch_row(job, cx))
                .collect();
            section = section
                .child(section_title(&t, "Link Downloads"))
                .child(group(&t, rows))
                .child(div().h(px(28.)));
        }
        section
    }

    fn fetch_row(&self, job: &Job, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let id = job.id;
        let key = id as usize;
        let p = job.progress;
        let (color, fraction, detail) = match &job.state {
            JobState::Running | JobState::Cancelling => {
                let fraction = p
                    .total_bytes
                    .filter(|total| *total > 0)
                    .map(|total| (p.downloaded_bytes as f64 / total as f64).clamp(0., 1.) as f32);
                let detail = if job.state == JobState::Cancelling {
                    "Cancelling…".to_owned()
                } else {
                    progress_detail(&p, matches!(job.source, FetchSource::Page(_)))
                };
                (t.accent, fraction.unwrap_or(0.), detail)
            }
            JobState::Done => (
                rgb(0x30b158),
                1.,
                match job.saved.as_slice() {
                    [path] => format!("Saved to {}", path.display()),
                    saved => format!(
                        "Saved {} files to {}",
                        saved.len(),
                        saved
                            .first()
                            .and_then(|path| path.parent())
                            .unwrap_or(&job.folder)
                            .display()
                    ),
                },
            ),
            JobState::Failed(_) => (t.danger, 0., "Download failed".to_owned()),
        };
        let playable = job
            .saved
            .iter()
            .find(|path| flick::library::is_video(path))
            .cloned();
        let running = matches!(job.state, JobState::Running);
        let failed = matches!(job.state, JobState::Failed(_));
        let retry = (job.title.clone(), job.source.clone(), job.folder.clone());
        h_flex()
            .gap_3()
            .px_3()
            .py_2p5()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(40.))
                    .rounded(px(t.radius + 2.))
                    .bg(t.control)
                    .child(icon("download", 18., color)),
            )
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
                            .child(job.title.clone()),
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
                                    .w(relative(fraction))
                                    .rounded_full()
                                    .bg(color),
                            ),
                    )
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(detail),
                    )
                    .when_some(
                        match &job.state {
                            JobState::Failed(error) => Some(error.clone()),
                            _ => None,
                        },
                        |d, error| {
                            d.child(
                                div()
                                    .text_size(t.font(12.))
                                    .text_color(t.danger)
                                    .child(error),
                            )
                        },
                    ),
            )
            .when_some(playable, |d, path| {
                d.child(self.button(
                    ("fetch-play", key),
                    Some("play"),
                    "Play",
                    false,
                    cx,
                    move |this, w, cx| this.open(path.clone(), w, cx),
                ))
            })
            .when(failed, |d| {
                d.child(self.button(
                    ("fetch-retry", key),
                    None,
                    "Try Again",
                    false,
                    cx,
                    move |this, _, cx| {
                        let (title, source, folder) = retry.clone();
                        this.fetches.jobs.retain(|job| job.id != id);
                        this.start_fetch(title, source, folder, cx);
                    },
                ))
            })
            .child(self.icon_button(
                ("fetch-remove", key),
                "x",
                if running {
                    "Cancel download"
                } else {
                    "Remove from list · keep files"
                },
                cx,
                move |this, _, cx| {
                    let Some(job) = this.fetches.jobs.iter_mut().find(|job| job.id == id) else {
                        return;
                    };
                    match job.state {
                        JobState::Running => {
                            job.state = JobState::Cancelling;
                            this.send(ServiceCommand::CancelFetch(id), cx);
                        }
                        JobState::Cancelling => {}
                        JobState::Done | JobState::Failed(_) => {
                            this.fetches.jobs.retain(|job| job.id != id);
                            cx.notify();
                        }
                    }
                },
            ))
            .into_any_element()
    }

    /// A link download on another machine, in that machine's section.
    pub(super) fn remote_fetch_row(
        &self,
        machine: &crate::remote_client::MachineTransfers,
        fetch: &FetchRow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let key = ElementId::from(SharedString::from(format!(
            "remote-fetch-{}-{}",
            gpui::hash(&machine.server_id),
            fetch.id
        )));
        let running = matches!(
            fetch.phase,
            FetchPhase::Resolving | FetchPhase::Downloading | FetchPhase::Merging
        );
        let (color, fraction, detail) = remote_fetch_detail(fetch, &machine.name);
        let color = match color {
            FetchTone::Active => t.accent,
            FetchTone::Done => rgb(0x30b158),
            FetchTone::Failed => t.danger,
            FetchTone::Quiet => t.muted,
        };
        let title = if fetch.title.trim().is_empty() {
            fetch.url.clone()
        } else {
            fetch.title.clone()
        };
        let playable = (fetch.phase == FetchPhase::Done)
            .then(|| fetch.saved.first().cloned())
            .flatten();
        let server = machine.server_id.clone();
        let id = fetch.id;
        h_flex()
            .gap_3()
            .px_3()
            .py_2p5()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(40.))
                    .rounded(px(t.radius + 2.))
                    .bg(t.control)
                    .child(icon("download", 18., color)),
            )
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
                            .child(title.clone()),
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
                                    .w(relative(fraction))
                                    .rounded_full()
                                    .bg(color),
                            ),
                    )
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(detail),
                    )
                    .when_some(fetch.error.clone(), |d, error| {
                        d.child(
                            div()
                                .text_size(t.font(12.))
                                .text_color(t.danger)
                                .child(error),
                        )
                    }),
            )
            .when_some(playable, |d, video| {
                let server = server.clone();
                d.child(self.button(
                    ElementId::NamedChild(Box::new(key.clone()), "play".into()),
                    Some("play"),
                    "Play",
                    false,
                    cx,
                    move |this, w, cx| this.watch_remote(&server, &video, &title, false, w, cx),
                ))
            })
            .when(running, |d| {
                d.child(self.icon_button(
                    ElementId::NamedChild(Box::new(key.clone()), "cancel".into()),
                    "x",
                    "Cancel download",
                    cx,
                    move |this, _, cx| {
                        this.remote.fetch_cancel(&this.store.data, &server, id);
                        cx.notify();
                    },
                ))
            })
            .into_any_element()
    }

    fn archive_sheet(&self, item: &ArchiveItem, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let chosen_size = item
            .files
            .iter()
            .enumerate()
            .filter(|(index, _)| self.fetches.chosen.contains(index))
            .filter_map(|(_, file)| file.size)
            .sum::<u64>();
        let mut rows = vec![
            row(
                &t,
                Some("download"),
                item.title.clone(),
                Some(
                    format!(
                        "{} videos · {} selected · downloads over HTTPS from archive.org",
                        item.files.len(),
                        format_bytes(chosen_size)
                    )
                    .into(),
                ),
                div(),
            )
            .into_any_element(),
        ];
        for (index, file) in item.files.iter().enumerate() {
            let id = ElementId::from(("archive-file", index));
            let el = h_flex()
                .id(id.clone())
                .relative()
                .gap_3()
                .px_3()
                .py_2()
                .cursor_pointer()
                .hover(|s| s.bg(t.selection))
                .child(checkbox(&t, self.fetches.chosen.contains(&index)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(file.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(t.font(12.))
                                .text_color(t.muted)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(format!(
                                    "{} · {}",
                                    if file.original {
                                        "Original"
                                    } else {
                                        "Converted by archive.org"
                                    },
                                    file.format
                                )),
                        ),
                )
                .child(
                    div()
                        .text_color(t.muted)
                        .child(file.size.map(format_bytes).unwrap_or_default()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.fetches.chosen.remove(&index) {
                        this.fetches.chosen.insert(index);
                    }
                    cx.notify();
                }));
            rows.push(
                self.focusable(el, &id, None, t.radius, (-RING_INSET, -RING_INSET), cx)
                    .into_any_element(),
            );
        }
        // The torrent sheet may already show the shared folder field.
        let folder = if self.pending_torrent.is_none() {
            self.text_field(&self.folder_field, cx)
        } else {
            div()
                .text_size(t.font(12.))
                .text_color(t.muted)
                .child(self.download_folder.display().to_string())
        };
        rows.push(
            v_flex()
                .pb_3()
                .child(row(
                    &t,
                    Some("folder-open"),
                    "Save to",
                    None,
                    self.button(
                        "archive-destination",
                        None,
                        "Choose…",
                        false,
                        cx,
                        |this, w, cx| this.choose_download_folder(w, cx),
                    ),
                ))
                .child(div().px_3().child(folder))
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
                        "archive-cancel",
                        None,
                        "Cancel",
                        false,
                        cx,
                        |this, _, cx| {
                            this.fetches.item = None;
                            cx.notify();
                        },
                    ))
                    .child(self.button(
                        "archive-confirm",
                        Some("download"),
                        "Download Selected",
                        true,
                        cx,
                        |this, _, cx| this.download_archive(cx),
                    )),
            )
    }
}

fn progress_detail(p: &FetchProgress, page: bool) -> String {
    let mut parts = Vec::new();
    if p.file_count > 1 {
        parts.push(format!("File {} of {}", p.file_index, p.file_count));
    }
    match p.stage {
        FetchStage::Starting if page => parts.push("Starting yt-dlp…".into()),
        FetchStage::Starting => parts.push("Connecting…".into()),
        FetchStage::Processing => parts.push("Processing…".into()),
        FetchStage::Verifying => parts.push("Verifying…".into()),
        FetchStage::Downloading => {
            parts.push(match p.total_bytes {
                Some(total) => format!(
                    "{} of {}",
                    format_bytes(p.downloaded_bytes),
                    format_bytes(total)
                ),
                None => format_bytes(p.downloaded_bytes),
            });
            if let Some(speed) = p.bytes_per_second {
                parts.push(format!("{}/s", format_bytes(speed)));
            }
            if let Some(eta) = p.eta_seconds {
                parts.push(format!("{} left", format_time(eta as f64)));
            }
        }
    }
    parts.join(" · ")
}

/// How a remote link download's row is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchTone {
    Active,
    Done,
    Failed,
    Quiet,
}

/// A remote link download's tone, bar fraction, and detail line.
fn remote_fetch_detail(fetch: &FetchRow, machine: &str) -> (FetchTone, f32, String) {
    match fetch.phase {
        FetchPhase::Resolving => (FetchTone::Active, 0., "Starting…".into()),
        FetchPhase::Merging => (FetchTone::Active, 1., "Processing…".into()),
        FetchPhase::Downloading => {
            let fraction = fetch
                .total_bytes
                .filter(|total| *total > 0)
                .map_or(0., |total| {
                    (fetch.downloaded_bytes as f64 / total as f64).clamp(0., 1.) as f32
                });
            let mut parts = vec![match fetch.total_bytes {
                Some(total) => format!(
                    "{} of {}",
                    format_bytes(fetch.downloaded_bytes),
                    format_bytes(total)
                ),
                None => format_bytes(fetch.downloaded_bytes),
            }];
            if let Some(speed) = fetch.bytes_per_second {
                parts.push(format!("{}/s", format_bytes(speed)));
            }
            (FetchTone::Active, fraction, parts.join(" · "))
        }
        FetchPhase::Done => (
            FetchTone::Done,
            1.,
            match fetch.saved.len() {
                0 | 1 => format!("Saved on {machine}"),
                count => format!("Saved {count} files on {machine}"),
            },
        ),
        FetchPhase::Failed => (FetchTone::Failed, 0., "Download failed".into()),
        FetchPhase::Cancelled => (FetchTone::Quiet, 0., "Cancelled".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_fetch_rows_describe_each_phase() {
        let mut fetch = FetchRow {
            phase: FetchPhase::Downloading,
            downloaded_bytes: 50,
            total_bytes: Some(200),
            bytes_per_second: Some(10),
            ..Default::default()
        };
        let (tone, fraction, detail) = remote_fetch_detail(&fetch, "Mac mini");
        assert_eq!(tone, FetchTone::Active);
        assert!((fraction - 0.25).abs() < 1e-6);
        assert_eq!(
            detail,
            format!(
                "{} of {} · {}/s",
                format_bytes(50),
                format_bytes(200),
                format_bytes(10)
            )
        );
        fetch.phase = FetchPhase::Done;
        fetch.saved = vec!["a".into(), "b".into()];
        assert_eq!(
            remote_fetch_detail(&fetch, "Mac mini"),
            (FetchTone::Done, 1., "Saved 2 files on Mac mini".into())
        );
        fetch.phase = FetchPhase::Failed;
        assert_eq!(remote_fetch_detail(&fetch, "Mac mini").0, FetchTone::Failed);
        fetch.phase = FetchPhase::Resolving;
        assert_eq!(remote_fetch_detail(&fetch, "Mac mini").2, "Starting…");
    }
}
