use super::*;
use flick::library::format_bytes;

impl Flick {
    pub(super) fn send(&mut self, command: ServiceCommand, cx: &mut Context<Self>) {
        if let Err(error) = self.services.send(command) {
            self.error = Some(error);
            self.resolving = false;
        }
        cx.notify();
    }

    pub(super) fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.query.trim().to_owned();
        if query.starts_with("magnet:")
            || (query.len() == 40 && query.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            self.resolve(flick::torrent::TorrentInput::Magnet(query), cx);
            return;
        }
        if query.chars().count() < 3 {
            self.error = Some("Type at least 3 characters to search online.".into());
            cx.notify();
            return;
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
                ServiceEvent::Shared(result) => match result {
                    Ok(shared) => {
                        self.shared_magnet = Some(shared.magnet);
                        self.library_tab = LibraryTab::Downloads;
                    }
                    Err(error) => self.error = Some(error),
                },
                ServiceEvent::Progress(downloads) => self.downloads = downloads,
                ServiceEvent::Completed(path) => {
                    self.store.data.add_video(&path);
                    if let Err(error) = self.store.save() {
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
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if share {
                        if let Some(path) = paths.into_iter().next() { this.send(ServiceCommand::Share(path), cx); }
                    } else if folder {
                        if let Some(path) = paths.into_iter().next() { this.import_folder(path, window, cx); }
                    } else {
                        for path in paths {
                            if path.extension().is_some_and(|v| v.eq_ignore_ascii_case("torrent")) { this.resolve(flick::torrent::TorrentInput::File(path), cx); }
                            else { this.store.data.add_video(&path); }
                        }
                        this.persist(); cx.notify();
                    }
                }
                Ok(Ok(None)) => {}
                _ => { this.error = Some("The file picker could not open. You can also drop videos into the library.".into()); cx.notify(); }
            });
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
                        for path in paths {
                            this.store.data.add_video(&path);
                        }
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

    fn choose_download_folder(&mut self, window: &Window, cx: &mut Context<Self>) {
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
                    this.download_folder = path;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn library_view(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut body = div()
            .id("library-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px_8()
            .py_6();
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .mb_5()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(26.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Library"),
                        )
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child(format!("{} videos", self.store.data.library.len())),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(self.control("add-folder", "Add folder", Control::AddFolder, cx))
                        .child(self.control("share", "Share", Control::Share, cx)),
                ),
        );
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .mb_5()
                .h(px(48.))
                .rounded(px(8.))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(LINE))
                .child(icon("search", 18., MUTED))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(self.search_input.clone()),
                )
                .child(self.control("search-online", "Search online", Control::Search, cx)),
        );
        let mut tabs = div()
            .flex()
            .gap_6()
            .border_b_1()
            .border_color(rgb(LINE))
            .mb_6();
        for (tab, label) in [
            (LibraryTab::Videos, "All videos"),
            (LibraryTab::History, "History"),
            (LibraryTab::Downloads, "Downloads"),
        ] {
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("tab-{label}")))
                    .pb_3()
                    .cursor_pointer()
                    .text_color(rgb(if self.library_tab == tab { TEXT } else { MUTED }))
                    .border_b_2()
                    .border_color(rgb(if self.library_tab == tab { TEXT } else { BG }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.library_tab = tab;
                        if tab == LibraryTab::Downloads {
                            this.send(ServiceCommand::Load, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        body = body.child(tabs);
        if self.importing {
            body = body.child(
                div()
                    .mb_4()
                    .text_color(rgb(MUTED))
                    .child("Adding videos from your folder…"),
            );
        }
        if self.resolving {
            body = body.child(
                div()
                    .mb_4()
                    .text_color(rgb(MUTED))
                    .child("Reading torrent files…"),
            );
        }
        if let Some(torrent) = &self.pending_torrent {
            let mut panel = div()
                .p_4()
                .mb_6()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(LINE))
                .rounded(px(8.))
                .child(div().text_size(px(16.)).mb_3().child(torrent.name.clone()));
            for file in &torrent.files {
                let index = file.index;
                panel = panel.child(
                    div()
                        .id(("torrent-file", index))
                        .flex()
                        .items_center()
                        .gap_3()
                        .py_2()
                        .cursor_pointer()
                        .child(
                            div()
                                .size(px(18.))
                                .border_1()
                                .border_color(rgb(MUTED))
                                .rounded(px(3.))
                                .when(self.selected_files.contains(&index), |d| {
                                    d.child(icon("check", 16., TEXT))
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(file.relative_path.to_string_lossy().into_owned()),
                        )
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child(format_bytes(file.length)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.selected_files.remove(&index) {
                                this.selected_files.insert(index);
                            }
                            cx.notify();
                        })),
                );
            }
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .mt_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(rgb(MUTED))
                            .child(self.download_folder.to_string_lossy().into_owned()),
                    )
                    .child(
                        div()
                            .id("choose-destination")
                            .cursor_pointer()
                            .child("Choose folder")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_download_folder(window, cx)
                            })),
                    ),
            );
            panel = panel.child(
                div()
                    .flex()
                    .gap_4()
                    .mt_4()
                    .child(
                        div()
                            .id("confirm-download")
                            .px_3()
                            .py_2()
                            .rounded(px(6.))
                            .bg(rgb(TEXT))
                            .text_color(rgb(BG))
                            .cursor_pointer()
                            .child("Download selected")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.selected_files.is_empty() {
                                    this.error =
                                        Some("Select at least one file to download.".into());
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
                                    this.library_tab = LibraryTab::Downloads;
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("cancel-download")
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending_torrent = None;
                                cx.notify();
                            })),
                    ),
            );
            body = body.child(panel);
        }
        if self.searching {
            body = body.child(div().py_4().child("Searching for videos…"));
        }
        if let Some(results) = &self.search_results {
            body = body.child(
                div()
                    .text_size(px(18.))
                    .mb_3()
                    .child(format!("Online results · {}", results.items.len())),
            );
            if !results.unavailable.is_empty() {
                body = body.child(div().mb_3().text_color(rgb(MUTED)).child(format!(
                    "Unavailable sources: {}. You can try again or paste a magnet link.",
                    results.unavailable.join(", ")
                )));
            }
            if results.items.is_empty() && results.unavailable.is_empty() {
                body = body.child(
                    div()
                        .py_4()
                        .text_color(rgb(MUTED))
                        .child("No videos found. Try another title."),
                );
            }
            for (index, result) in results.items.iter().enumerate() {
                let source = result.clone();
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_4()
                        .py_3()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .child(icon("download", 18., MUTED))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(result.name.clone()),
                                )
                                .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(
                                    if result.torrent_url.is_some() {
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
                                    },
                                )),
                        )
                        .child(
                            div()
                                .id(("download-result", index))
                                .px_3()
                                .py_2()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(rgb(LINE))
                                .cursor_pointer()
                                .child("Download")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.resolve(source.input(), cx)
                                })),
                        ),
                );
            }
            body = body.child(div().h(px(24.)));
        }
        let query = self.query.to_lowercase();
        match self.library_tab {
            LibraryTab::Downloads => {
                if let Some(magnet) = &self.shared_magnet {
                    let magnet = magnet.clone();
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .p_4()
                            .mb_4()
                            .bg(rgb(PANEL))
                            .rounded(px(8.))
                            .child("Share link ready. Keep the transfer running while others download.")
                            .child(
                                div()
                                    .id("copy-magnet")
                                    .px_3()
                                    .py_2()
                                    .cursor_pointer()
                                    .child("Copy magnet")
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            magnet.clone(),
                                        ))
                                    })),
                            ),
                    );
                }
                if self.downloads.is_empty() {
                    body = body.child(
                        div()
                            .py_6()
                            .text_color(rgb(MUTED))
                            .child("Your downloads and shared files will appear here."),
                    );
                }
                for download in &self.downloads {
                    let id = download.infohash.clone();
                    let pause_id = id.clone();
                    let resume_id = id.clone();
                    let paused = download.state == flick::torrent::TransferState::Paused;
                    let state = match download.state {
                        flick::torrent::TransferState::Checking => "Checking files",
                        flick::torrent::TransferState::Downloading => "Downloading",
                        flick::torrent::TransferState::Paused => "Paused",
                        flick::torrent::TransferState::Seeding => "Sharing",
                        flick::torrent::TransferState::Error => "Transfer failed",
                    };
                    let progress = if download.total_bytes > 0 {
                        (download.verified_bytes as f64 / download.total_bytes as f64).clamp(0., 1.)
                            as f32
                    } else {
                        0.
                    };
                    let mut row = div()
                        .p_4()
                        .mb_3()
                        .bg(rgb(PANEL))
                        .rounded(px(8.))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .mb_2()
                                .child(download.name.clone()),
                        )
                        .child(
                            div()
                                .h(px(3.))
                                .w_full()
                                .bg(rgb(LINE))
                                .child(div().h_full().w(relative(progress)).bg(rgb(TEXT))),
                        )
                        .child(
                            div()
                                .mt_3()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{state} · {} / {} · {}/s · {} peers",
                                    format_bytes(download.verified_bytes),
                                    format_bytes(download.total_bytes),
                                    format_bytes(download.download_bytes_per_second),
                                    download.peers
                                )),
                        );
                    if let Some(error) = &download.error {
                        row = row.child(div().mt_2().child(error.clone()));
                    }
                    let mut actions = div()
                        .flex()
                        .gap_5()
                        .mt_3()
                        .child(
                            div()
                                .id(SharedString::from(format!("pause-{id}")))
                                .cursor_pointer()
                                .child(if paused { "Resume" } else { "Pause" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send(
                                        if paused {
                                            ServiceCommand::Resume(resume_id.clone())
                                        } else {
                                            ServiceCommand::Pause(pause_id.clone())
                                        },
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("remove-{id}")))
                                .cursor_pointer()
                                .text_color(rgb(MUTED))
                                .child("Remove · keep files")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send(ServiceCommand::Remove(id.clone()), cx)
                                })),
                        );
                    for file in &download.files {
                        if file.selected
                            && file.verified_bytes == file.length
                            && !matches!(
                                download.state,
                                flick::torrent::TransferState::Checking
                                    | flick::torrent::TransferState::Error
                            )
                            && flick::library::is_video(&file.path)
                        {
                            let path = file.path.clone();
                            actions = actions.child(
                                div()
                                    .id(SharedString::from(format!(
                                        "play-download-{}-{}",
                                        download.infohash, file.index
                                    )))
                                    .cursor_pointer()
                                    .child("Play")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open(path.clone(), window, cx)
                                    })),
                            );
                        }
                    }
                    body = body.child(row.child(actions));
                }
                body = body.child(
                    div()
                        .mt_4()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child("Transfers and sharing run while Flick is open."),
                );
            }
            LibraryTab::History => {
                let mut count = 0;
                for (index, video) in self
                    .store
                    .data
                    .recent
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.title.to_lowercase().contains(&query))
                {
                    count += 1;
                    body = body.child(self.library_row(
                        index,
                        &video.path,
                        &video.title,
                        Some(video),
                        "history-video",
                        cx,
                    ));
                }
                if count == 0 {
                    body = body.child(
                        div()
                            .py_6()
                            .text_color(rgb(MUTED))
                            .child("Your viewing history will appear here."),
                    );
                }
            }
            LibraryTab::Videos => {
                let continuing = self
                    .store
                    .data
                    .recent
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| {
                        self.store.resume_position(&v.path) > 0.
                            && v.title.to_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>();
                if !continuing.is_empty() {
                    body = body.child(div().text_size(px(18.)).mb_3().child("Continue watching"));
                    for (index, video) in continuing {
                        body = body.child(self.library_row(
                            index,
                            &video.path,
                            &video.title,
                            Some(video),
                            "continue-video",
                            cx,
                        ));
                    }
                    body = body.child(div().h(px(24.)));
                }
                body = body.child(div().text_size(px(18.)).mb_3().child(if query.is_empty() {
                    "All videos"
                } else {
                    "In your library"
                }));
                let mut count = 0;
                for (index, video) in self
                    .store
                    .data
                    .library
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.title.to_lowercase().contains(&query))
                {
                    count += 1;
                    let history = self.store.data.recent.iter().find(|v| v.path == video.path);
                    body = body.child(self.library_row(
                        index,
                        &video.path,
                        &video.title,
                        history,
                        "library-video",
                        cx,
                    ));
                }
                if count == 0 {
                    body = body.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .py_8()
                            .gap_3()
                            .child(icon("library", 32., MUTED))
                            .child(if query.is_empty() {
                                "Add your first videos"
                            } else {
                                "No matching videos in your library"
                            })
                            .child(div().text_color(rgb(MUTED)).child(if query.is_empty() {
                                "Add videos, choose a folder, or search for something to download."
                            } else {
                                "Press Enter or Search online to find something to download."
                            }))
                            .child(self.control(
                                "library-add-first",
                                "Add videos",
                                Control::AddVideos,
                                cx,
                            )),
                    );
                }
            }
        }
        body.into_any_element()
    }

    fn library_row(
        &self,
        index: usize,
        path: &std::path::Path,
        title: &str,
        history: Option<&crate::store::RecentFile>,
        kind: &'static str,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let path = path.to_owned();
        let resume = history.map_or(0., |v| self.store.resume_position(&v.path));
        let detail = history.map_or_else(
            || "Not started".to_owned(),
            |v| {
                if resume > 0. {
                    format!(
                        "Continue at {} · {} total",
                        format_time(resume),
                        format_time(v.duration)
                    )
                } else {
                    format!("Watched · {}", format_time(v.duration))
                }
            },
        );
        let progress = history
            .filter(|v| v.duration > 0.)
            .map_or(0., |v| (v.position / v.duration).clamp(0., 1.) as f32);
        div()
            .id((kind, index))
            .flex()
            .items_center()
            .gap_4()
            .p_4()
            .mb_2()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(LINE))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(PANEL)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(84.))
                    .h(px(48.))
                    .rounded(px(5.))
                    .bg(rgb(PANEL))
                    .child(icon("film", 22., MUTED)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(title.to_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(detail),
                    )
                    .when(progress > 0., |d| {
                        d.child(
                            div()
                                .w(px(170.))
                                .h(px(2.))
                                .bg(rgb(LINE))
                                .child(div().h_full().w(relative(progress)).bg(rgb(MUTED))),
                        )
                    }),
            )
            .child(icon("play", 18., TEXT))
            .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
            .into_any_element()
    }
}
