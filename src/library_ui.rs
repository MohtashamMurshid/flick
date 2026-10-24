//! Library pages, navigation, downloads, and sharing.

use super::{
    chrome_ui::{group, notice, row, section_title, thumbnail, traffic_light_space},
    *,
};
use crate::store::Sort;
use flick::library::format_bytes;
use gpui::{AnyElement, FontWeight, anchored, deferred, linear_color_stop, linear_gradient, point};

impl Flick {
    pub(super) fn add_library_files(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut videos = Vec::new();
        for path in paths {
            match flick::library::canonical_video(&path) {
                Ok(path) => videos.push(path),
                Err(error) => {
                    self.error = Some(format!("Could not add {}: {error:#}", path.display()))
                }
            }
        }
        self.store.data.add_videos(videos);
    }

    pub(super) fn send(&mut self, command: ServiceCommand, cx: &mut Context<Self>) {
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
                ServiceEvent::Fetch { id, update } => self.fetch_event(id, update),
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
                ServiceEvent::Progress(downloads) => self.downloads = downloads,
                ServiceEvent::TorrentCompleted {
                    infohash,
                    file_index,
                    path,
                } => {
                    self.store.data.bind_torrent(&path, &infohash, file_index);
                    if let Err(error) = self.store.save() {
                        self.error = Some(format!("Could not save the library: {error}"));
                    }
                }
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
                        if let Some(path) = paths.into_iter().next() { this.show_page(Page::Downloads, window, cx); this.send(ServiceCommand::Share(path), cx); }
                    } else if folder {
                        if let Some(path) = paths.into_iter().next() { this.import_folder(path, window, cx); }
                    } else {
                        let mut videos = Vec::new();
                        for path in paths {
                            if path.extension().is_some_and(|v| v.eq_ignore_ascii_case("torrent")) { this.resolve(flick::torrent::TorrentInput::File(path), cx); }
                            else { videos.push(path); }
                        }
                        this.add_library_files(videos);
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
                        this.store.data.add_videos(paths);
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
        if let Err(error) = self.store.save() {
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
                    .child(self.search_field(window, cx).w(t.font(220.)));
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
                            "Search (Ctrl+K)",
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
        let mut body = v_flex()
            .id("page-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(px(x))
            .pt(px(top))
            .pb_8();
        if t.style == Style::Windows {
            let mut header = h_flex().mb_5().gap_2().child(
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
        body = body.children(error);
        if self.importing {
            body = body.child(notice(&t, false, "Adding videos from your folder…").mb_4());
        }
        if self.resolving {
            body = body.child(
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
        body = body.child(match self.page {
            Page::Home => self.home_page(width, cx),
            Page::Videos => self.videos_page(width, cx),
            Page::Shows => self.shows_page(width, cx),
            Page::History => self.history_page(cx),
            Page::Downloads => self.downloads_page(cx),
            Page::Appearance => self.appearance_page(cx),
            Page::Shortcuts => self.shortcuts_page(cx),
        });
        body.into_any_element()
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

    fn search_section(&self, cx: &Context<Self>) -> Option<Div> {
        let t = cx.theme().clone();
        if self.searching && self.search_results.is_none() {
            return Some(notice(&t, false, "Searching for videos online…").mb_6());
        }
        let results = self.search_results.as_ref()?;
        let mut rows = Vec::new();
        for (index, result) in results.items.iter().enumerate() {
            let source = result.clone();
            let detail = if result.archive_id.is_some() {
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
            };
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

    fn video_detail(&self, path: &std::path::Path) -> (Option<f32>, String) {
        let Some(history) = self.store.data.recent.iter().find(|v| v.path == path) else {
            let duration = self
                .store
                .data
                .library
                .iter()
                .find(|video| video.path == path)
                .map_or(0., |video| video.duration);
            return (
                None,
                if duration > 0. {
                    format!("Not started · {}", format_time(duration))
                } else {
                    "Not started".into()
                },
            );
        };
        let resume = self.store.resume_position(path);
        if resume > 0. && history.duration > 0. {
            (
                Some((resume / history.duration).clamp(0., 1.) as f32),
                format!("{} left", format_time(history.duration - resume)),
            )
        } else if history.duration > 0. {
            (None, format!("Watched · {}", format_time(history.duration)))
        } else {
            (None, "Watched".into())
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
        let id = ElementId::from((section, index));
        let group_name = SharedString::from(format!("{section}-{index}"));
        let menu_path = path.clone();
        let focused_card = self
            .keyboard_focused(&id, cx)
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
            if modifiers.control || modifiers.platform || modifiers.alt || modifiers.shift {
                return;
            }
            let Some(step) = GridStep::from_key(&event.keystroke.key) else {
                return;
            };
            cx.stop_propagation();
            this.keyboard_nav = true;
            match step.target(index, cols, count) {
                Some(target) => this
                    .control_handle(&ElementId::from((section, target)), cx)
                    .focus(window),
                None if matches!(step, GridStep::Left | GridStep::Up) => window.focus_prev(),
                None => window.focus_next(),
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
        let id = ElementId::from((section, index));
        let open_path = path.to_owned();
        let el = h_flex()
            .id(id.clone())
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
            .child(icon("play", 18., t.muted))
            .when(self.keyboard_focused(&id, cx), |d| {
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

    fn home_page(&self, width: f32, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let query = self.query.to_lowercase();
        let gap = 16.;
        let mut page = v_flex().w_full().children(self.search_section(cx));
        let continuing = self
            .store
            .data
            .continue_watching()
            .filter(|v| v.title.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        if !continuing.is_empty() {
            let (cols, card) = columns(width, 220., gap);
            let shown = continuing.len().min(cols);
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
        let added = self
            .store
            .data
            .library
            .iter()
            .rev()
            .filter(|v| v.title.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        let (cols, card) = columns(width, 180., gap);
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
            let shown = added.len().min(cols * 2);
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

    fn videos_page(&self, width: f32, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let query = self.query.to_lowercase();
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
        let videos = self
            .store
            .data
            .sorted_library(view.sort)
            .into_iter()
            .filter(|v| v.title.to_lowercase().contains(&query))
            .filter(|v| match self.video_filter {
                VideoFilter::All => true,
                VideoFilter::Unwatched => !self.store.data.watched(&v.path),
                VideoFilter::InProgress => self.store.resume_position(&v.path) > 0.,
            })
            .collect::<Vec<_>>();
        let mut page = v_flex().w_full().children(self.search_section(cx)).child(
            h_flex()
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
                        .child(format!("{} videos", videos.len())),
                )
                .child(layout),
        );
        if videos.is_empty() {
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
        let count = videos.len();
        if view.list {
            let rows = videos
                .into_iter()
                .enumerate()
                .map(|(i, v)| self.video_row(("library-row", i), count, &v.path, &v.title, cx))
                .collect();
            return page.child(group(&t, rows));
        }
        let (cols, card) = columns(width, 180., gap);
        let cards = videos
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                self.video_card(("library", i), (cols, count), &v.path, &v.title, card, cx)
            })
            .collect();
        page = page.child(card_grid(cards, cols, gap));
        page
    }

    /// Shows found in the library, or one show's episodes by season.
    fn shows_page(&self, width: f32, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let query = self.query.to_lowercase();
        let shows = flick::library::group_shows(
            self.store
                .data
                .library
                .iter()
                .map(|video| video.path.as_path()),
        )
        .into_iter()
        .filter(|show| show.title.to_lowercase().contains(&query))
        .collect::<Vec<_>>();
        let selected = self.selected_show.as_ref().and_then(|key| {
            shows.iter().find(|show| {
                show.episodes
                    .first()
                    .is_some_and(|(e, _)| e.show_key() == *key)
            })
        });
        if let Some(show) = selected {
            let watched = show
                .episodes
                .iter()
                .filter(|(_, path)| self.store.data.watched(path))
                .count();
            let mut page = v_flex().w_full().child(
                h_flex()
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
                            .text_size(t.font(t.text_size + 5.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(show.title.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(format!(
                                "{} episodes · {watched} watched",
                                show.episodes.len()
                            )),
                    ),
            );
            let count = show.episodes.len();
            let mut index = 0;
            let mut seasons = show
                .episodes
                .chunk_by(|(a, _), (b, _)| a.season == b.season);
            for season in seasons.by_ref() {
                let rows = season
                    .iter()
                    .map(|(episode, path)| {
                        let title =
                            format!("{} · {}", episode.label(), crate::model::file_title(path));
                        let row = self.video_row(("episode", index), count, path, &title, cx);
                        index += 1;
                        row
                    })
                    .collect();
                page = page
                    .child(section_title(&t, format!("Season {}", season[0].0.season)))
                    .child(group(&t, rows))
                    .child(div().h(px(20.)));
            }
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
        let (cols, card) = columns(width, 180., gap);
        let count = shows.len();
        let cards = shows
            .iter()
            .enumerate()
            .map(|(index, show)| self.show_card(index, (cols, count), show, card, cx))
            .collect();
        v_flex().w_full().child(card_grid(cards, cols, gap))
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
        let unwatched = show
            .episodes
            .iter()
            .filter(|(_, path)| !self.store.data.watched(path))
            .count();
        let detail = match unwatched {
            0 => format!("{} episodes · all watched", show.episodes.len()),
            n => format!("{} episodes · {n} unwatched", show.episodes.len()),
        };
        let id = ElementId::from(("show", index));
        let el = v_flex()
            .id(id.clone())
            .relative()
            .w(px(width))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected_show = Some(key.clone());
                if this.keyboard_nav {
                    this.control_handle(&ElementId::from(("episode", 0usize)), cx)
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
        let rows = self
            .store
            .data
            .recent
            .iter()
            .enumerate()
            .map(|(index, video)| {
                self.video_row(("history", index), count, &video.path, &video.title, cx)
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return v_flex().child(empty_state(
                &t,
                "history",
                "Nothing watched yet",
                "Videos you play appear here so you can pick up where you left off.",
            ));
        }
        v_flex().child(group(&t, rows))
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
                    .child(self.read_only_field(&self.magnet_field, cx)),
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
        let paused = download.state == TransferState::Paused;
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
                    this.store.data.bind_torrent(&path, &infohash, index);
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
                .child(
                    div()
                        .px_3()
                        .child(self.read_only_field(&self.folder_field, cx)),
                )
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
    use super::GridStep;

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
}
