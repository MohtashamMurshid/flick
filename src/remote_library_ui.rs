//! Remote libraries in the view: a Home section per paired server, remote
//! entries in Continue Watching, remote subtitles and the OpenSubtitles
//! hash in the player, and the "Share my library on Tailscale" switch.
//!
//! Listings, posters, and downloads come from `crate::remote_client`; the
//! in-app server is `crate::remote_serve`.

use super::{
    chrome_ui::{notice, row, section_title, thumbnail},
    library_ui::{GridStep, card_grid, columns},
    *,
};
use crate::remote_client::{Ready, ServerView, Status, SubtitleRef};
use flick::remote::protocol::{Library, Progress, Video};
use gpui::{AnyElement, FontWeight};

/// Cards per group on Home before "Show All", in rows.
const COLLAPSED_ROWS: usize = 1;

/// View state for remote libraries that is not a setting.
#[derive(Default)]
pub(super) struct RemoteUiState {
    /// Card ids per grid, for arrow keys; rebuilt as grids render.
    grids: RefCell<HashMap<String, Rc<[ElementId]>>>,
    /// Servers whose Home section shows every video.
    expanded: HashSet<String>,
    /// Why a remote video did not open, shown on Home.
    pub(super) notice: Option<String>,
    /// Subtitle downloads for the playing remote video.
    subtitles: HashMap<SubtitleRef, SubtitleLoad>,
    /// OpenSubtitles hashes by (server, video); None when the server could
    /// not compute one.
    hashes: HashMap<(String, String), Option<String>>,
    /// A subtitle search waiting for its hash: server, video, generation.
    awaiting_hash: Option<(String, String, u64)>,
    /// When the pairing string was last copied, for the button's label.
    pub(super) copied: Option<Instant>,
    /// A transcode tier waiting for the server's answer before it loads.
    pub(super) checking: Option<PendingPlayback>,
}

/// A remote load waiting for [`Ready::Playback`].
pub(super) struct PendingPlayback {
    /// From `RemoteState::check_playback`.
    pub check: u64,
    /// `open_generation` when it was asked; any later open cancels it.
    pub generation: u64,
    pub remote: RemoteSource,
    pub resume: f64,
}

impl RemoteUiState {
    /// Forgets per-video player state when another video (or tier) loads;
    /// libmpv drops added subtitle files with the old file.
    pub(super) fn reset_player(&mut self) {
        self.subtitles.clear();
        self.awaiting_hash = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubtitleLoad {
    Loading,
    Loaded,
    Failed,
}

/// Videos of one server shown together: films, or one show's episodes.
#[derive(Debug, PartialEq)]
pub(super) struct Group<'a> {
    /// None when it is the only group of plain videos.
    pub title: Option<String>,
    pub key: String,
    pub videos: Vec<&'a Video>,
}

/// Whether a remote video matches the search text (already lowercase).
pub(super) fn matches_query(video: &Video, query: &str) -> bool {
    query.is_empty()
        || video.title.to_lowercase().contains(query)
        || video.file_name.to_lowercase().contains(query)
        || video
            .episode
            .as_ref()
            .is_some_and(|episode| episode.show.to_lowercase().contains(query))
}

/// Groups a listing like the local Shows page: plain videos by title, then
/// each show's episodes by season and number. Only matching videos appear,
/// and empty groups are left out.
pub(super) fn group_videos<'a>(library: &'a Library, query: &str) -> Vec<Group<'a>> {
    let mut films = Vec::new();
    let mut shows: Vec<(String, String, Vec<&Video>)> = Vec::new();
    for video in library.videos.iter().filter(|v| matches_query(v, query)) {
        match &video.episode {
            Some(episode) if !episode.show.trim().is_empty() => {
                let key = episode.show.trim().to_lowercase();
                match shows.iter_mut().find(|(k, ..)| *k == key) {
                    Some((.., episodes)) => episodes.push(video),
                    None => shows.push((key, episode.show.trim().to_owned(), vec![video])),
                }
            }
            _ => films.push(video),
        }
    }
    films.sort_by_cached_key(|v| display_title(v).to_lowercase());
    shows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut groups = Vec::new();
    if !films.is_empty() {
        groups.push(Group {
            title: (!shows.is_empty()).then(|| "Videos".to_owned()),
            key: "videos".into(),
            videos: films,
        });
    }
    for (key, show, mut episodes) in shows {
        episodes.sort_by_key(|v| {
            let episode = v.episode.as_ref();
            (
                episode.map_or(0, |e| e.season),
                episode.map_or(0, |e| e.number),
                display_title(v).to_lowercase(),
            )
        });
        groups.push(Group {
            title: Some(show),
            key: format!("show:{key}"),
            videos: episodes,
        });
    }
    groups
}

fn display_title(video: &Video) -> &str {
    if video.title.trim().is_empty() {
        &video.file_name
    } else {
        &video.title
    }
}

/// A card's progress bar fraction and detail line, as local cards show them.
pub(super) fn card_detail(
    video: &Video,
    resume: f64,
    progress: Option<&crate::store::remote::RemoteProgress>,
) -> (Option<f32>, String) {
    let duration = if video.duration > 0. {
        video.duration
    } else {
        progress.map_or(0., |p| p.duration)
    };
    let episode = video
        .episode
        .as_ref()
        .filter(|e| e.number > 0)
        .map(|e| format!("S{} E{} · ", e.season, e.number))
        .unwrap_or_default();
    if !video.complete {
        return (None, format!("{episode}Still downloading on the server"));
    }
    if resume > 0. && duration > 0. {
        return (
            Some((resume / duration).clamp(0., 1.) as f32),
            format!("{episode}{} left", format_time(duration - resume)),
        );
    }
    let state = if progress.is_some_and(|p| p.finished) {
        "Watched"
    } else {
        "Not started"
    };
    if duration > 0. {
        (
            None,
            format!("{episode}{state} · {}", format_time(duration)),
        )
    } else {
        (None, format!("{episode}{state}"))
    }
}

/// Whether a server is known not to answer, so opening a video would fail.
fn unreachable(status: &Status) -> bool {
    !status.is_online() && *status != Status::Connecting
}

/// A message that ends like a sentence, for joining after another one.
fn sentence(message: &str) -> String {
    let message = message.trim();
    if message.ends_with(['.', '!', '?', '…']) {
        message.to_owned()
    } else {
        format!("{message}.")
    }
}

fn card_id(kind: &str, server: &str, video: &str) -> ElementId {
    ElementId::from(("remote-card", gpui::hash(&(kind, server, video))))
}

/// One remote card's contents.
struct Card<'a> {
    kind: &'static str,
    server: &'a str,
    video: &'a str,
    title: &'a str,
    detail: String,
    progress: Option<f32>,
    badge: Option<&'static str>,
}

impl Flick {
    /// The paired server with this id, with its status and listing.
    pub(super) fn remote_server(&self, id: &str) -> Option<ServerView> {
        self.remote
            .servers(&self.store.data)
            .into_iter()
            .find(|server| server.id == id)
    }

    /// Opens a remote video from a card. A server known to be offline gets
    /// a refresh and a note on Home instead of a playback error.
    pub(super) fn open_remote_video(
        &mut self,
        server_id: &str,
        video_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.remote_server(server_id) else {
            self.remote_ui.notice = Some("That library is no longer paired.".into());
            cx.notify();
            return;
        };
        if unreachable(&server.status) {
            self.remote.refresh_server(&self.store.data, server_id);
            self.remote_ui.notice = Some(format!("{}: {}", server.name, server.status.label()));
            cx.notify();
            return;
        }
        let Some(playback) = self.remote.playback(&self.store.data, server_id, video_id) else {
            self.remote.refresh_server(&self.store.data, server_id);
            self.remote_ui.notice = Some(format!("{} no longer lists this video.", server.name));
            cx.notify();
            return;
        };
        self.remote_ui.notice = None;
        let (url, authorization) = (playback.url(), playback.authorization());
        let source = RemoteSource {
            server_id: playback.server_id,
            server_name: playback.server_name,
            video_id: playback.video_id,
            title: playback.title,
            duration: playback.duration,
            quality: playback.quality,
            base_url: playback.base_url,
            token: playback.token,
        };
        // Both sides build the URL and header from the shared protocol.
        debug_assert_eq!(source.url(), url);
        debug_assert_eq!(source.header(), format!("Authorization: {authorization}"));
        self.open_remote(source, window, cx);
    }

    /// Home's note about a remote video that did not open.
    pub(super) fn remote_notice(&self, cx: &Context<Self>) -> Option<Div> {
        let text = self.remote_ui.notice.clone()?;
        let t = cx.theme();
        Some(notice(t, false, text).mb_4().child(self.icon_button(
            "remote-notice-dismiss",
            "x",
            "Dismiss",
            cx,
            |this, _, cx| {
                this.remote_ui.notice = None;
                cx.notify();
            },
        )))
    }

    /// Continue Watching cards for remote videos, newest first.
    pub(super) fn remote_continue_cards(
        &self,
        query: &str,
        limit: usize,
        (cols, width): (usize, f32),
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let servers = self.remote.servers(&self.store.data);
        let entries = self
            .store
            .data
            .remote
            .progress
            .iter()
            .filter(|entry| !entry.finished)
            .filter(|entry| {
                self.store
                    .data
                    .remote_resume_position(&entry.server, &entry.video)
                    > 0.
            })
            .filter(|entry| query.is_empty() || entry.title.to_lowercase().contains(query))
            .filter_map(|entry| {
                let server = servers.iter().find(|s| s.id == entry.server)?;
                Some((entry, server))
            })
            .take(limit)
            .collect::<Vec<_>>();
        let grid = "continue-remote".to_owned();
        let ids = entries
            .iter()
            .map(|(entry, _)| card_id("continue", &entry.server, &entry.video))
            .collect::<Rc<[_]>>();
        self.remote_ui.grids.borrow_mut().insert(grid.clone(), ids);
        let count = entries.len();
        entries
            .into_iter()
            .enumerate()
            .map(|(index, (entry, server))| {
                let resume = self
                    .store
                    .data
                    .remote_resume_position(&entry.server, &entry.video);
                let video = server
                    .listing
                    .as_ref()
                    .and_then(|listing| listing.videos.iter().find(|v| v.id == entry.video));
                let duration = video
                    .map(|v| v.duration)
                    .filter(|d| *d > 0.)
                    .unwrap_or(entry.duration);
                let state = if unreachable(&server.status) {
                    server.status.label()
                } else {
                    format!("{} left", format_time(duration - resume))
                };
                self.remote_card(
                    Card {
                        kind: "continue",
                        server: &entry.server,
                        video: &entry.video,
                        title: &entry.title,
                        detail: format!("{} · {state}", server.name),
                        progress: (duration > 0.).then(|| (resume / duration).clamp(0., 1.) as f32),
                        badge: video
                            .and_then(|v| crate::store::resolution_label(v.width, v.height)),
                    },
                    (&grid, index),
                    (cols, count),
                    width,
                    cx,
                )
            })
            .collect()
    }

    /// One Home section per paired server: its name and status, then its
    /// videos grouped into plain videos and shows. A server with nothing to
    /// show is one offline row with Retry.
    pub(super) fn remote_sections(&self, width: f32, query: &str, cx: &Context<Self>) -> Vec<Div> {
        let t = cx.theme().clone();
        let gap = 16.;
        let (cols, card) = columns(width, 180., gap);
        let mut sections = Vec::new();
        for server in self.remote.servers(&self.store.data) {
            let groups = server
                .listing
                .as_deref()
                .map(|listing| group_videos(listing, query))
                .unwrap_or_default();
            if groups.is_empty() && !query.is_empty() {
                continue;
            }
            let expanded = self.remote_ui.expanded.contains(&server.id);
            let truncated = groups
                .iter()
                .any(|group| group.videos.len() > cols * COLLAPSED_ROWS);
            let mut header = h_flex()
                .gap_3()
                .items_center()
                .child(section_title(&t, server.name.clone()))
                .child(
                    div()
                        .mb_2()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child(server.summary()),
                )
                .child(div().flex_1());
            let id = server.id.clone();
            if unreachable(&server.status) && server.listing.is_some() {
                header = header.child(
                    self.button(
                        SharedString::from(format!("remote-retry:{id}")),
                        Some("rotate-cw"),
                        "Retry",
                        false,
                        cx,
                        move |this, _, cx| {
                            this.remote.refresh_server(&this.store.data, &id);
                            cx.notify();
                        },
                    )
                    .mb_2(),
                );
            }
            if truncated {
                let id = server.id.clone();
                header = header.child(
                    self.focusable(
                        div()
                            .id(SharedString::from(format!("remote-all:{id}")))
                            .relative()
                            .mb_2()
                            .text_size(t.font(12.))
                            .text_color(t.accent_text)
                            .cursor_pointer()
                            .child(if expanded { "Show Less" } else { "Show All" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.remote_ui.expanded.remove(&id) {
                                    this.remote_ui.expanded.insert(id.clone());
                                }
                                cx.notify();
                            })),
                        &SharedString::from(format!("remote-all:{}", server.id)).into(),
                        None,
                        t.radius,
                        (2., 0.),
                        cx,
                    ),
                );
            }
            let mut section = v_flex().w_full().mt(px(28.)).child(header);
            if server.listing.is_none() {
                let id = server.id.clone();
                section =
                    section.child(notice(&t, false, server.status.label()).child(self.button(
                        SharedString::from(format!("remote-retry:{id}")),
                        Some("rotate-cw"),
                        "Retry",
                        false,
                        cx,
                        move |this, _, cx| {
                            this.remote.refresh_server(&this.store.data, &id);
                            cx.notify();
                        },
                    )));
                sections.push(section);
                continue;
            }
            if groups.is_empty() {
                section = section.child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child("No videos in this library yet."),
                );
            }
            for group in groups {
                let shown = if expanded {
                    group.videos.len()
                } else {
                    group.videos.len().min(cols * COLLAPSED_ROWS)
                };
                let grid = format!("{}/{}", server.id, group.key);
                let videos = &group.videos[..shown];
                let ids = videos
                    .iter()
                    .map(|video| card_id("library", &server.id, &video.id))
                    .collect::<Rc<[_]>>();
                self.remote_ui.grids.borrow_mut().insert(grid.clone(), ids);
                let cards = videos
                    .iter()
                    .enumerate()
                    .map(|(index, video)| {
                        let progress = self.store.data.remote_progress(&server.id, &video.id);
                        let resume = self
                            .store
                            .data
                            .remote_resume_position(&server.id, &video.id);
                        let (progress, detail) = card_detail(video, resume, progress);
                        self.remote_card(
                            Card {
                                kind: "library",
                                server: &server.id,
                                video: &video.id,
                                title: display_title(video),
                                detail,
                                progress,
                                badge: crate::store::resolution_label(video.width, video.height),
                            },
                            (&grid, index),
                            (cols, shown),
                            card,
                            cx,
                        )
                    })
                    .collect();
                if let Some(title) = &group.title {
                    section = section.child(
                        div()
                            .px_1()
                            .mb_2()
                            .text_size(t.font(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.muted)
                            .child(format!("{title} · {}", group.videos.len())),
                    );
                }
                section = section.child(card_grid(cards, cols, gap).mb_4());
            }
            sections.push(section);
        }
        sections
    }

    fn remote_card(
        &self,
        card: Card,
        (grid, index): (&str, usize),
        (cols, count): (usize, usize),
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let height = (width * 9. / 16.).round();
        let id = card_id(card.kind, card.server, card.video);
        let group_name = SharedString::from(format!(
            "remote-{}",
            gpui::hash(&(card.kind, card.server, card.video))
        ));
        let (server, video) = (card.server.to_owned(), card.video.to_owned());
        let poster = match self.remote.poster(card.server, card.video) {
            Some(image) => div()
                .flex_none()
                .w(px(width))
                .h(px(height))
                .rounded(px(t.card_radius))
                .overflow_hidden()
                .bg(rgb(0x000000))
                .child(img(image).size_full().object_fit(ObjectFit::Cover)),
            None => thumbnail(card.title, width, height, t.card_radius),
        };
        let el = v_flex()
            .id(id.clone())
            .group(group_name.clone())
            .relative()
            .w(px(width))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_remote_video(&server, &video, window, cx)
            }))
            .child(
                poster
                    .relative()
                    .border_1()
                    .border_color(t.border)
                    .when_some(card.badge.filter(|_| width >= 120.), |d, badge| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(6.))
                                .right(px(6.))
                                .px_1p5()
                                .rounded(px(4.))
                                .bg(rgba(0x000000b3))
                                .text_color(rgb(0xffffff))
                                .text_size(t.font(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(badge),
                        )
                    })
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
                    .when_some(card.progress, |d, progress| {
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
                    .child(card.title.to_owned()),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(card.detail),
            );
        let el = self.with_remote_grid_keys(el, (grid.to_owned(), index), (cols, count), cx);
        self.focusable(el, &id, None, t.card_radius, (0., 0.), cx)
            .into_any_element()
    }

    /// Arrow keys move between cards of one remote grid, as in local grids.
    /// Every remote card is rendered, so Tab follows the normal order.
    fn with_remote_grid_keys(
        &self,
        el: Stateful<Div>,
        (grid, index): (String, usize),
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
                Some(target) => {
                    let id = this
                        .remote_ui
                        .grids
                        .borrow()
                        .get(&grid)
                        .and_then(|ids| ids.get(target).cloned());
                    if let Some(id) = id {
                        this.control_handle(&id, cx).focus(window);
                    }
                }
                None if matches!(step, GridStep::Left | GridStep::Up) => window.focus_prev(),
                None => window.focus_next(),
            }
            cx.notify();
        }))
    }

    /// Subtitles the server has for the playing remote video: files next to
    /// it, and, in a transcode tier (which carries no subtitle streams),
    /// text streams from inside it. Original quality already has those.
    pub(super) fn remote_subtitle_choices(
        &self,
    ) -> Vec<(SubtitleRef, String, Option<SubtitleLoad>)> {
        let Some(remote) = self.remote_source() else {
            return Vec::new();
        };
        let Some(server) = self.remote_server(&remote.server_id) else {
            return Vec::new();
        };
        let Some(video) = server
            .listing
            .as_ref()
            .and_then(|listing| listing.videos.iter().find(|v| v.id == remote.video_id))
        else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for file in &video.subtitles {
            let label = match &file.language {
                Some(language) if !language.is_empty() => {
                    format!("{} ({language})", file.file_name)
                }
                _ => file.file_name.clone(),
            };
            choices.push((SubtitleRef::Sidecar(file.index), label));
        }
        if remote.quality.is_transcode() {
            for stream in video.embedded_subtitles.iter().filter(|s| s.text) {
                let label = stream
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .or_else(|| stream.language.clone().filter(|l| !l.is_empty()))
                    .unwrap_or_else(|| format!("Track {}", stream.track));
                choices.push((
                    SubtitleRef::Embedded(stream.track),
                    format!("{label} (from the file)"),
                ));
            }
        }
        choices
            .into_iter()
            .map(|(which, label)| {
                let state = self.remote_ui.subtitles.get(&which).copied();
                (which, label, state)
            })
            .collect()
    }

    /// Downloads a server subtitle; it is added to the player when it arrives.
    pub(super) fn load_remote_subtitle(&mut self, which: SubtitleRef, cx: &mut Context<Self>) {
        let Some(remote) = self.remote_source() else {
            return;
        };
        if matches!(
            self.remote_ui.subtitles.get(&which),
            Some(SubtitleLoad::Loading | SubtitleLoad::Loaded)
        ) {
            return;
        }
        let (server, video) = (remote.server_id.clone(), remote.video_id.clone());
        self.remote_ui
            .subtitles
            .insert(which, SubtitleLoad::Loading);
        self.remote.fetch_subtitle(&server, &video, which);
        cx.notify();
    }

    /// The OpenSubtitles hash of the playing remote video, if the server
    /// already sent it. None asks the server and the search runs once the
    /// answer arrives in [`Self::apply_remote_ready`].
    pub(super) fn remote_search_hash(&mut self, generation: u64) -> Option<Option<String>> {
        let remote = self.remote_source()?;
        let key = (remote.server_id.clone(), remote.video_id.clone());
        if let Some(hash) = self.remote_ui.hashes.get(&key) {
            return Some(hash.clone());
        }
        self.remote.fetch_hash(&key.0, &key.1);
        self.remote_ui.awaiting_hash = Some((key.0, key.1, generation));
        None
    }

    /// Finished subtitle downloads and hash requests from the remote client.
    pub(super) fn apply_remote_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for ready in self.remote.take_ready() {
            let current = self
                .remote_source()
                .map(|r| (r.server_id.clone(), r.video_id.clone()));
            match ready {
                Ready::Subtitle {
                    server_id,
                    video_id,
                    which,
                    result,
                } => {
                    if current.as_ref() != Some(&(server_id, video_id)) {
                        continue;
                    }
                    let added = result.and_then(|path| {
                        let path = path.to_str().ok_or("unsupported file name")?.to_owned();
                        let player = self.player.as_ref().ok_or("playback is unavailable")?;
                        player
                            .command(&["sub-add", &path, "select"])
                            .map_err(|error| error.to_string())
                    });
                    match added {
                        Ok(()) => {
                            self.remote_ui.subtitles.insert(which, SubtitleLoad::Loaded);
                            self.show_status("Subtitles added");
                        }
                        Err(error) => {
                            self.remote_ui.subtitles.insert(which, SubtitleLoad::Failed);
                            self.show_status(format!("Could not load subtitles: {error}"));
                        }
                    }
                    cx.notify();
                }
                Ready::Hash {
                    server_id,
                    video_id,
                    result,
                } => {
                    let hash = result
                        .inspect_err(|error| tracing::debug!(%error, "No remote video hash"))
                        .ok();
                    let key = (server_id, video_id);
                    self.remote_ui.hashes.insert(key.clone(), hash.clone());
                    let Some((server, video, generation)) = self.remote_ui.awaiting_hash.take()
                    else {
                        continue;
                    };
                    if (server, video) == key && generation == self.subtitle_search.generation {
                        self.send_subtitle_search(None, hash, cx);
                    }
                }
                Ready::Playback {
                    check,
                    server_id,
                    video_id,
                    quality,
                    result,
                } => {
                    let Some(pending) = self.remote_ui.checking.take_if(|p| {
                        p.check == check
                            && p.remote.server_id == server_id
                            && p.remote.video_id == video_id
                            && p.remote.quality == quality
                    }) else {
                        continue;
                    };
                    if self.quitting || pending.generation != self.open_generation {
                        continue;
                    }
                    match result {
                        Ok(()) => self.load_remote(pending.remote, pending.resume, window, cx),
                        Err(message) => {
                            self.status = None;
                            self.error = Some(format!(
                                "Could not play this video at {}. {}",
                                pending.remote.quality.label(),
                                sentence(&message)
                            ));
                            cx.notify();
                        }
                    }
                }
            }
        }
    }

    /// Sends this video's progress to its server, matching what was just
    /// saved locally.
    pub(super) fn upload_remote_progress(&mut self, server: &str, video: &str) {
        // The client's worker is already stopped; sending would restart it.
        if self.quitting {
            return;
        }
        let Some(entry) = self.store.data.remote_progress(server, video) else {
            return;
        };
        let progress = Progress {
            position: entry.position,
            duration: entry.duration,
            finished: entry.finished,
            updated: entry.updated,
        };
        self.remote
            .put_progress(&self.store.data, server, video, progress);
    }

    /// Turns the in-app server on, or restarts it with a new token.
    pub(super) fn start_serving(&mut self, new_token: bool) {
        // Queue the current library first; the server thread waits for it.
        self.persist();
        let Some(settings_path) = self.store.path().map(PathBuf::from) else {
            self.serve
                .fail("Flick has no settings file to share on this system".into());
            return;
        };
        let barrier = self.store.save_barrier();
        self.serve.start(crate::remote_serve::ServeOptions {
            settings_path,
            state_dir: crate::remote_client::default_data_dir().join("remote"),
            poster_cache: thumbnail_cache(),
            name: self.store.data.remote.serve_name.clone(),
            new_token,
            bind: None,
            ffmpeg: None,
            before_start: Some(Box::new(move || barrier.wait())),
        });
    }

    pub(super) fn set_serving(&mut self, on: bool, cx: &mut Context<Self>) {
        self.store.data.remote.serve = on;
        if on {
            self.start_serving(false);
        } else {
            self.serve.stop();
            self.persist();
        }
        cx.notify();
    }

    /// Settings rows for "Share my library on Tailscale".
    pub(super) fn serve_rows(&self, cx: &Context<Self>) -> (Vec<AnyElement>, Option<String>) {
        use crate::remote_serve::ServeStatus;
        let t = cx.theme().clone();
        let on = self.store.data.remote.serve;
        let status = self.serve.status().clone();
        let detail = match (&status, on) {
            (ServeStatus::Running { listen, .. }, _) => format!("Sharing on {listen}"),
            (ServeStatus::Starting, _) => "Starting…".to_owned(),
            (ServeStatus::Failed(_), _) => "Not sharing".to_owned(),
            (ServeStatus::Off, true) => "Starting…".to_owned(),
            (ServeStatus::Off, false) => {
                "Other Flicks on your tailnet can play this library at full quality".to_owned()
            }
        };
        let mut rows = vec![
            row(
                &t,
                Some("share-2"),
                "Share my library on Tailscale",
                Some(detail.into()),
                self.switch("remote-serve", on, cx, move |this, _, cx| {
                    this.set_serving(!on, cx)
                }),
            )
            .into_any_element(),
        ];
        let error = match &status {
            ServeStatus::Failed(message) => Some(message.clone()),
            _ => None,
        };
        if let ServeStatus::Running { pairing, listen } = status {
            let shown = match pairing.split_once("/#") {
                Some((address, token)) => {
                    format!("{address}/#{}…", &token[..token.len().min(8)])
                }
                None => pairing.clone(),
            };
            let copied = self
                .remote_ui
                .copied
                .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
            let actions = h_flex()
                .gap_2()
                .child(self.button(
                    "remote-serve-copy",
                    Some("copy"),
                    if copied { "Copied" } else { "Copy" },
                    true,
                    cx,
                    move |this, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(pairing.clone()));
                        this.remote_ui.copied = Some(Instant::now());
                        cx.notify();
                    },
                ))
                .child(self.button(
                    "remote-serve-new-token",
                    Some("rotate-ccw"),
                    "New Token",
                    false,
                    cx,
                    |this, _, cx| {
                        this.start_serving(true);
                        cx.notify();
                    },
                ));
            rows.push(
                row(
                    &t,
                    None,
                    "Pairing string",
                    Some(
                        format!(
                            "{shown} · Listening on {listen}. Paste it into Remote Libraries on the other Flick. New Token unpairs every Flick using this one."
                        )
                        .into(),
                    ),
                    actions,
                )
                .into_any_element(),
            );
        }
        (rows, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flick::remote::protocol::Episode;

    #[test]
    fn server_messages_end_as_sentences() {
        assert_eq!(
            sentence("Still downloading on the server"),
            "Still downloading on the server."
        );
        assert_eq!(sentence(" Try again. "), "Try again.");
        assert_eq!(sentence("Busy…"), "Busy…");
    }

    fn video(id: &str, title: &str, episode: Option<(&str, u32, u32)>) -> Video {
        Video {
            id: id.into(),
            title: title.into(),
            file_name: format!("{title}.mkv"),
            complete: true,
            episode: episode.map(|(show, season, number)| Episode {
                show: show.into(),
                season,
                number,
            }),
            ..Default::default()
        }
    }

    fn library(videos: Vec<Video>) -> Library {
        Library {
            videos,
            ..Default::default()
        }
    }

    #[test]
    fn plain_videos_come_first_then_shows_with_episodes_in_order() {
        let listing = library(vec![
            video("1", "Zeta", None),
            video("2", "Pilot", Some(("Night Shift", 1, 1))),
            video("3", "Finale", Some(("Night Shift", 2, 3))),
            video("4", "Alpha", None),
            video("5", "Two", Some(("Night Shift", 1, 2))),
            video("6", "Start", Some(("Bakers", 1, 1))),
        ]);
        let groups = group_videos(&listing, "");
        let summary: Vec<_> = groups
            .iter()
            .map(|g| {
                (
                    g.title.clone(),
                    g.videos.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (Some("Videos".into()), vec!["4", "1"]),
                (Some("Bakers".into()), vec!["6"]),
                (Some("Night Shift".into()), vec!["2", "5", "3"]),
            ]
        );
    }

    #[test]
    fn a_library_of_plain_videos_has_one_untitled_group() {
        let listing = library(vec![video("1", "B", None), video("2", "A", None)]);
        let groups = group_videos(&listing, "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, None);
        assert!(group_videos(&library(Vec::new()), "").is_empty());
    }

    #[test]
    fn search_matches_titles_file_names_and_show_names() {
        let listing = library(vec![
            video("1", "Arrival", None),
            video("2", "Pilot", Some(("Night Shift", 1, 1))),
            video("3", "Heat", None),
        ]);
        let ids = |query: &str| {
            group_videos(&listing, query)
                .into_iter()
                .flat_map(|g| g.videos)
                .map(|v| v.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("arr"), ["1"]);
        assert_eq!(ids("night"), ["2"]);
        assert_eq!(ids("heat.mkv"), ["3"]);
        assert!(ids("nothing").is_empty());
    }

    #[test]
    fn card_details_follow_progress_and_completion() {
        let mut film = video("1", "Film", None);
        film.duration = 3600.;
        assert_eq!(
            card_detail(&film, 0., None),
            (None, "Not started · 1:00:00".into())
        );
        assert_eq!(
            card_detail(&film, 900., None),
            (Some(0.25), "45:00 left".into())
        );
        let watched = crate::store::remote::RemoteProgress {
            finished: true,
            ..Default::default()
        };
        assert_eq!(
            card_detail(&film, 0., Some(&watched)),
            (None, "Watched · 1:00:00".into())
        );
        let mut episode = video("2", "Pilot", Some(("Show", 1, 2)));
        assert_eq!(card_detail(&episode, 0., None).1, "S1 E2 · Not started");
        episode.complete = false;
        assert_eq!(
            card_detail(&episode, 0., None).1,
            "S1 E2 · Still downloading on the server"
        );
    }

    #[test]
    fn only_known_failures_block_opening() {
        assert!(!unreachable(&Status::Online));
        assert!(!unreachable(&Status::Connecting));
        assert!(unreachable(&Status::Offline));
        assert!(unreachable(&Status::WrongToken));
        assert!(unreachable(&Status::Failed("x".into())));
    }
}
