#[path = "library_ui.rs"]
mod library_ui;

use crate::{
    assets::icon,
    input::{InputEvent, TextInput},
    model::{file_title, format_time, seek_target},
    playback::{PlaybackSnapshot, Player, PlayerEvent},
    services::{Command as ServiceCommand, Event as ServiceEvent, Services},
    store::Store,
};
use gpui::{
    Bounds, ClickEvent, ClipboardItem, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, ObjectFit, PathPromptOptions, Pixels, RenderImage, SharedString,
    Stateful, Window, canvas, div, img, prelude::*, px, relative, rgb,
};
use std::{
    cell::Cell,
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

const BG: u32 = 0x0a0a0a;
const PANEL: u32 = 0x141414;
const LINE: u32 = 0x262626;
const TEXT: u32 = 0xfafafa;
const MUTED: u32 = 0x858585;
const HOVER: u32 = 0x202020;
const HEADER_HEIGHT: f32 = 56.;
const FOOTER_HEIGHT: f32 = 108.;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Library,
    Player,
}
#[derive(Clone, Copy, PartialEq)]
enum LibraryTab {
    Videos,
    History,
    Downloads,
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Recent,
    Tracks,
    Keys,
}
#[derive(Clone, Copy)]
enum Control {
    Open,
    Play,
    Back,
    Forward,
    Mute,
    Quieter,
    Louder,
    Speed,
    Fullscreen,
    Recent,
    Tracks,
    Subtitle,
    Keys,
    DismissError,
    Library,
    Search,
    AddVideos,
    AddFolder,
    Share,
}

pub struct Flick {
    player: Option<Player>,
    snapshot: PlaybackSnapshot,
    store: Store,
    path: Option<PathBuf>,
    frame: Option<Arc<RenderImage>>,
    error: Option<String>,
    menu: Option<Menu>,
    focus: FocusHandle,
    timeline: Rc<Cell<Bounds<Pixels>>>,
    video_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    scrubbing: bool,
    changing_volume: bool,
    last_snapshot: Instant,
    last_saved: Instant,
    opened_at: Option<Instant>,
    pending_resume: f64,
    file_loaded: bool,
    view: View,
    library_tab: LibraryTab,
    search_input: Entity<TextInput>,
    query: String,
    search_generation: u64,
    searching: bool,
    search_results: Option<flick::catalog::SearchResults>,
    services: Services,
    downloads: Vec<flick::torrent::TransferSnapshot>,
    resolving: bool,
    pending_torrent: Option<flick::torrent::ResolvedTorrent>,
    selected_files: HashSet<usize>,
    download_folder: PathBuf,
    shared_magnet: Option<String>,
    importing: bool,
}

impl Flick {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::load();
        let search_input = cx.new(TextInput::new);
        let services = Services::start();
        let download_folder = services.download_dir.clone();
        cx.subscribe(&search_input, |this, input, event, cx| {
            match event {
                InputEvent::Changed => {
                    let _ = this.services.send(ServiceCommand::CancelSearch);
                    this.query = input.read(cx).value().to_owned();
                    this.search_generation += 1;
                    this.searching = false;
                    this.search_results = None;
                }
                InputEvent::Submit => this.search(cx),
            }
            cx.notify();
        })
        .detach();
        let (player, error) = match Player::new() {
            Ok(player) => (Some(player), None),
            Err(error) => (None, Some(format!("Playback is unavailable. {error:#}"))),
        };
        let focus = cx.focus_handle();
        focus.focus(window);
        let mut this = Self {
            player,
            snapshot: PlaybackSnapshot::default(),
            store,
            path: None,
            frame: None,
            error,
            menu: None,
            focus,
            timeline: Rc::new(Cell::new(Bounds::default())),
            video_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            scrubbing: false,
            changing_volume: false,
            last_snapshot: Instant::now() - Duration::from_secs(1),
            last_saved: Instant::now(),
            opened_at: None,
            pending_resume: 0.,
            file_loaded: false,
            view: View::Library,
            library_tab: LibraryTab::Videos,
            search_input,
            query: String::new(),
            search_generation: 0,
            searching: false,
            search_results: None,
            services,
            downloads: Vec::new(),
            resolving: false,
            pending_torrent: None,
            selected_files: HashSet::new(),
            download_folder,
            shared_magnet: None,
            importing: false,
        };
        this.apply_preferences();
        cx.on_release(|this, _| this.persist()).detach();
        cx.spawn_in(window, async move |this, cx| {
            let mut delay = Duration::from_millis(100);
            loop {
                cx.background_executor().timer(delay).await;
                match this.update_in(cx, |this, window, cx| this.tick(window, cx)) {
                    Ok(next) => delay = next,
                    Err(_) => break,
                }
            }
        })
        .detach();
        if let Some(path) = path {
            this.open(path, window, cx);
        }
        this
    }

    fn apply_preferences(&mut self) {
        if let Some(player) = &self.player {
            let _ = player.set_property("volume", &self.store.data.volume.to_string());
            let _ = player.set_property("mute", if self.store.data.muted { "yes" } else { "no" });
            let _ = player.set_property("speed", &self.store.data.speed.to_string());
            self.snapshot = player.snapshot();
        }
    }

    fn persist(&mut self) {
        if let Some(player) = &self.player {
            let s = player.snapshot();
            self.store.data.volume = s.volume;
            self.store.data.muted = s.muted;
            self.store.data.speed = s.speed;
            if self.file_loaded
                && self.pending_resume == 0.
                && s.duration > 0.
                && let Some(path) = &self.path
            {
                self.store.remember(path, s.position, s.duration);
            }
        }
        if let Err(error) = self.store.save() {
            tracing::warn!(%error, "Could not save viewing preferences");
        }
        self.last_saved = Instant::now();
    }

    fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path
            .extension()
            .is_some_and(|v| v.eq_ignore_ascii_case("torrent"))
        {
            self.resolve(flick::torrent::TorrentInput::File(path), cx);
            return;
        }
        if let Some(value) = path.to_str()
            && value.starts_with("magnet:")
        {
            self.resolve(flick::torrent::TorrentInput::Magnet(value.to_owned()), cx);
            return;
        }
        if !path.is_file() {
            self.error = Some(format!("This file could not be opened: {}", path.display()));
            cx.notify();
            return;
        }
        let path = path.canonicalize().unwrap_or(path);
        let Some(path_str) = path.to_str() else {
            self.error = Some(
                "The file path contains unsupported characters. Try renaming the file.".into(),
            );
            cx.notify();
            return;
        };
        self.persist();
        let Some(player) = &self.player else {
            cx.notify();
            return;
        };
        match player.load(path_str) {
            Ok(()) => {
                self.pending_resume = self.store.resume_position(&path);
                self.file_loaded = false;
                self.path = Some(path.clone());
                self.store.data.add_video(&path);
                self.view = View::Player;
                self.focus.focus(window);
                self.snapshot = PlaybackSnapshot::default();
                self.error = None;
                self.menu = None;
                self.opened_at = Some(Instant::now());
                if let Some(old) = self.frame.take() {
                    let _ = window.drop_image(old);
                }
                window.set_window_title(&format!("{} — Flick", file_title(&path)));
                tracing::info!(path = %path.display(), "Opening video");
            }
            Err(error) => self.error = Some(format!("Could not play this file. {error:#}")),
        }
        cx.notify();
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Duration {
        self.poll_services(cx);
        let Some(player) = &self.player else {
            return Duration::from_millis(500);
        };
        let mut changed = false;
        for event in player.poll_events() {
            changed = true;
            match event {
                PlayerEvent::FileLoaded => {
                    self.file_loaded = true;
                    if self.pending_resume > 0. {
                        if let Err(error) = player.command(&[
                            "seek",
                            &self.pending_resume.to_string(),
                            "absolute+exact",
                        ]) {
                            tracing::warn!(%error, "Could not restore playback position");
                        }
                        self.pending_resume = 0.;
                    }
                }
                PlayerEvent::EndFile => {}
                PlayerEvent::Error(error) => {
                    self.file_loaded = false;
                    self.error = Some(error);
                }
            }
        }
        if self.last_snapshot.elapsed() >= Duration::from_millis(200) || changed {
            let next = player.snapshot();
            changed |= (self.snapshot.position - next.position).abs() > 0.01
                || self.snapshot.paused != next.paused
                || self.snapshot.duration != next.duration;
            self.snapshot = next;
            self.last_snapshot = Instant::now();
        }
        if self.file_loaded && self.view == View::Player {
            let bounds = self.video_bounds.get();
            let width = f32::from(bounds.size.width).max(1.);
            let height = f32::from(bounds.size.height).max(1.);
            let (w, h) = flick::presentation::target_size(width, height, window.scale_factor());
            match player.render(w.max(1), h.max(1)) {
                Ok(Some(frame)) => {
                    if let Some(buffer) =
                        image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
                    {
                        let next = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                        if let Some(old) = self.frame.replace(next) {
                            let _ = window.drop_image(old);
                        }
                        if let Some(started) = self.opened_at.take() {
                            tracing::info!(
                                elapsed_ms = started.elapsed().as_millis(),
                                "First video presentation submitted"
                            );
                        }
                        changed = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if self.error.is_none() {
                        self.error = Some(format!("Could not display this video. {error:#}"));
                        changed = true;
                    }
                }
            }
        }
        if self.path.is_some() && self.last_saved.elapsed() > Duration::from_secs(5) {
            self.persist();
        }
        if changed {
            cx.notify();
        }
        if self.path.is_some() && !self.snapshot.paused && !self.snapshot.eof {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(150)
        }
    }

    fn command(&mut self, args: &[&str], cx: &mut Context<Self>) {
        if let Some(player) = &self.player {
            if let Err(error) = player.command(args) {
                self.error = Some(error.to_string());
            }
            self.snapshot = player.snapshot();
        }
        cx.notify();
    }

    fn property(&mut self, name: &str, value: &str, cx: &mut Context<Self>) {
        if let Some(player) = &self.player {
            if let Err(error) = player.set_property(name, value) {
                self.error = Some(error.to_string());
            }
            self.snapshot = player.snapshot();
        }
        cx.notify();
    }

    fn pick(&mut self, subtitle: bool, window: &Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                if subtitle {
                    "Load subtitles"
                } else {
                    "Open video"
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => if let Some(path) = paths.into_iter().next() {
                        if subtitle {
                            if let Some(s) = path.to_str() { this.command(&["sub-add", s, "select"], cx); }
                        } else { this.open(path, window, cx); }
                    },
                    Ok(Ok(None)) => {},
                    _ => { this.error = Some("The file picker could not open. Drop a file into Flick or launch it with a video path.".into()); cx.notify(); }
                }
            });
        }).detach();
    }

    fn handle(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        match control {
            Control::Library => {
                self.persist();
                self.property("pause", "yes", cx);
                self.view = View::Library;
                self.menu = None;
                window.set_window_title("Flick");
                self.focus.focus(window);
                cx.notify();
            }
            Control::Search => self.search(cx),
            Control::AddVideos => self.pick_library(false, false, window, cx),
            Control::AddFolder => self.pick_library(true, false, window, cx),
            Control::Share => self.pick_library(true, true, window, cx),
            Control::Open => self.pick(false, window, cx),
            Control::Subtitle => self.pick(true, window, cx),
            Control::Play if self.path.is_some() => {
                if self.snapshot.eof {
                    self.command(&["seek", "0", "absolute+exact"], cx);
                    self.property("pause", "no", cx);
                } else {
                    self.command(&["cycle", "pause"], cx);
                }
            }
            Control::Back | Control::Forward if self.path.is_some() => {
                let delta = if matches!(control, Control::Back) {
                    -10.
                } else {
                    10.
                };
                let target = seek_target(self.snapshot.position, delta, self.snapshot.duration);
                self.command(&["seek", &target.to_string(), "absolute+exact"], cx);
            }
            Control::Mute => self.command(&["cycle", "mute"], cx),
            Control::Quieter | Control::Louder => {
                let delta = if matches!(control, Control::Quieter) {
                    -5.
                } else {
                    5.
                };
                self.property(
                    "volume",
                    &(self.snapshot.volume + delta).clamp(0., 100.).to_string(),
                    cx,
                );
            }
            Control::Speed => {
                let speeds = [0.5, 0.75, 1., 1.25, 1.5, 2.];
                let next = speeds
                    .iter()
                    .copied()
                    .find(|speed| *speed > self.snapshot.speed + 0.01)
                    .unwrap_or(0.5);
                self.property("speed", &next.to_string(), cx);
            }
            Control::Fullscreen => window.toggle_fullscreen(),
            Control::DismissError => {
                self.error = None;
                cx.notify();
            }
            Control::Recent | Control::Tracks | Control::Keys => {
                let menu = match control {
                    Control::Recent => Menu::Recent,
                    Control::Tracks => Menu::Tracks,
                    _ => Menu::Keys,
                };
                self.menu = if self.menu == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                cx.notify();
            }
            _ => {}
        }
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if (modifiers.control || modifiers.platform) && key == "o" {
            self.handle(
                if self.view == View::Library {
                    Control::AddVideos
                } else {
                    Control::Open
                },
                window,
                cx,
            );
            cx.stop_propagation();
            return;
        }
        if (modifiers.control || modifiers.platform) && key == "k" {
            self.handle(Control::Library, window, cx);
            self.search_input.focus_handle(cx).focus(window);
            cx.stop_propagation();
            return;
        }
        if key == "escape" {
            if window.is_fullscreen() {
                window.toggle_fullscreen();
            }
            self.menu = None;
            cx.notify();
            return;
        }
        if self.search_input.read(cx).focused(window) {
            return;
        }
        if self.view == View::Library && !(modifiers.control || modifiers.platform) {
            return;
        }
        let control = if modifiers.control || modifiers.platform {
            match key {
                "o" => Some(Control::Open),
                "l" => Some(Control::Subtitle),
                _ => None,
            }
        } else {
            match key {
                "space" => Some(Control::Play),
                "left" => Some(Control::Back),
                "right" => Some(Control::Forward),
                "up" => Some(Control::Louder),
                "down" => Some(Control::Quieter),
                "m" => Some(Control::Mute),
                "f" => Some(Control::Fullscreen),
                "?" => Some(Control::Keys),
                _ => None,
            }
        };
        if let Some(control) = control {
            cx.stop_propagation();
            self.handle(control, window, cx);
        }
    }

    fn control(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        action: Control,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let primary = id == "open-primary";
        let color = if primary { BG } else { TEXT };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(36.))
            .px_3()
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .bg(rgb(if primary { TEXT } else { BG }))
            .border_1()
            .border_color(rgb(if primary { TEXT } else { LINE }))
            .text_color(rgb(color))
            .hover(move |s| s.bg(rgb(if primary { 0xdedede } else { HOVER })))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.handle(action, window, cx);
            }))
            .child(icon(
                match action {
                    Control::Subtitle => "captions",
                    Control::Library => "arrow-left",
                    Control::Search => "search",
                    Control::AddFolder => "folder-plus",
                    Control::Share => "share-2",
                    _ => "folder-open",
                },
                15.,
                color,
            ))
            .child(label.into())
    }

    fn icon_control(
        &self,
        id: &'static str,
        name: &'static str,
        label: impl Into<SharedString>,
        action: Control,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let play = id == "play";
        let disabled = self.path.is_none()
            && matches!(
                action,
                Control::Play | Control::Back | Control::Forward | Control::Tracks
            );
        let label = label.into();
        let color = if disabled {
            0x404040
        } else if play {
            BG
        } else {
            TEXT
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(if play { 44. } else { 34. }))
            .rounded(px(if play { 22. } else { 6. }))
            .bg(rgb(if play && !disabled { TEXT } else { BG }))
            .border_1()
            .border_color(rgb(if play {
                if disabled { LINE } else { TEXT }
            } else {
                BG
            }))
            .when(!disabled, |d| {
                d.cursor_pointer()
                    .hover(move |s| s.bg(rgb(if play { 0xdedede } else { HOVER })))
            })
            .tooltip(move |_, cx| cx.new(|_| Hint(label.clone())).into())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if !disabled {
                    this.handle(action, window, cx);
                }
            }))
            .child(icon(name, if play { 19. } else { 18. }, color))
    }

    fn seek_at(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let bounds = self.timeline.get();
        if self.snapshot.duration > 0. && bounds.size.width > px(0.) {
            let ratio = f32::from(x - bounds.origin.x) / f32::from(bounds.size.width);
            let target = ratio.clamp(0., 1.) as f64 * self.snapshot.duration;
            self.command(&["seek", &target.to_string(), "absolute+exact"], cx);
        }
    }

    fn volume_at(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let bounds = self.volume_bounds.get();
        if bounds.size.width > px(0.) {
            let value =
                (f32::from(x - bounds.origin.x) / f32::from(bounds.size.width)).clamp(0., 1.);
            self.property("volume", &(value * 100.).round().to_string(), cx);
        }
    }

    fn menu_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.menu?;
        let title = match menu {
            Menu::Recent => "Recent files",
            Menu::Tracks => "Audio & subtitles",
            Menu::Keys => "Keyboard shortcuts",
        };
        let mut panel = div()
            .id("panel")
            .occlude()
            .flex()
            .flex_col()
            .p_2()
            .w(px(300.))
            .max_h(px(380.))
            .overflow_y_scroll()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(LINE))
            .rounded(px(8.))
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .px_2()
                    .py_2()
                    .mb_1()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            );
        match menu {
            Menu::Recent => {
                if self.store.data.recent.is_empty() {
                    panel = panel.child(
                        div()
                            .px_2()
                            .py_3()
                            .text_color(rgb(MUTED))
                            .child("No recent files yet."),
                    );
                }
                for (index, recent) in self.store.data.recent.iter().enumerate() {
                    let path = recent.path.clone();
                    panel = panel.child(
                        div()
                            .id(("recent", index))
                            .flex()
                            .items_center()
                            .gap_3()
                            .p_2()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child(icon("file-video", 16., MUTED))
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
                                            .child(recent.title.clone()),
                                    )
                                    .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                                        format!(
                                            "{} / {}",
                                            format_time(recent.position),
                                            format_time(recent.duration)
                                        ),
                                    )),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open(path.clone(), window, cx)
                            })),
                    );
                }
            }
            Menu::Tracks => {
                for (kind, title) in [("audio", "Audio"), ("sub", "Subtitles")] {
                    panel = panel.child(
                        div()
                            .px_2()
                            .pt_3()
                            .pb_1()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(title),
                    );
                    for track in self
                        .snapshot
                        .tracks
                        .iter()
                        .filter(|track| track.kind == kind)
                    {
                        let id = track.id.to_string();
                        let property = if kind == "audio" { "aid" } else { "sid" };
                        let label = if !track.title.is_empty() {
                            track.title.clone()
                        } else if !track.language.is_empty() {
                            track.language.clone()
                        } else {
                            format!("Track {}", track.id)
                        };
                        panel =
                            panel.child(
                                div()
                                    .id(SharedString::from(format!("{property}-{id}")))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .p_2()
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .child(div().size(px(16.)).when(track.selected, |d| {
                                        d.child(icon("check", 15., TEXT))
                                    }))
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.property(property, &id, cx)
                                    })),
                            );
                    }
                    if kind == "sub" {
                        let off = !self
                            .snapshot
                            .tracks
                            .iter()
                            .any(|t| t.kind == "sub" && t.selected);
                        panel = panel.child(
                            div()
                                .id("subs-off")
                                .flex()
                                .items_center()
                                .gap_2()
                                .p_2()
                                .rounded(px(5.))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(HOVER)))
                                .child(
                                    div()
                                        .size(px(16.))
                                        .when(off, |d| d.child(icon("check", 15., TEXT))),
                                )
                                .child("Off")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.property("sid", "no", cx)),
                                ),
                        );
                    }
                }
                panel = panel.child(
                    div()
                        .mt_2()
                        .pt_2()
                        .border_t_1()
                        .border_color(rgb(LINE))
                        .child(self.control(
                            "load-subtitle",
                            "Load subtitle file",
                            Control::Subtitle,
                            cx,
                        )),
                );
            }
            Menu::Keys => {
                for (key, action) in [
                    ("Space", "Play / pause"),
                    ("←  →", "Seek 10 seconds"),
                    ("↑  ↓", "Change volume"),
                    ("F / Esc", "Fullscreen / exit"),
                    ("M", "Mute"),
                    ("Ctrl / ⌘ O", "Open video"),
                    ("Ctrl / ⌘ L", "Load subtitles"),
                ] {
                    panel = panel.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .px_2()
                            .py_2()
                            .child(action)
                            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(key)),
                    );
                }
            }
        }
        Some(panel.into_any_element())
    }
}

struct Hint(SharedString);
impl Render for Hint {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .font_family("Inter")
            .text_size(px(12.))
            .child(self.0.clone())
    }
}

impl Render for Flick {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = if self.snapshot.duration > 0. {
            (self.snapshot.position / self.snapshot.duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let volume = (self.snapshot.volume / 100.).clamp(0., 1.) as f32;
        let playing = self.path.is_some() && !self.snapshot.paused && !self.snapshot.eof;
        let timeline = self.timeline.clone();
        let volume_bounds = self.volume_bounds.clone();
        let video_bounds = self.video_bounds.clone();
        let header = div()
            .relative()
            .flex()
            .items_center()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_6()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .bg(rgb(TEXT))
                            .child(icon("play", 13., BG)),
                    )
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Flick"),
                    )
                    .when(self.view == View::Player, |d| {
                        d.child(self.control("library-home", "Library", Control::Library, cx))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    // GPUI 0.2.2 mutates truncation runs between layout passes,
                    // which can split UTF-8 and abort on macOS. Clip instead.
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        self.path
                            .as_ref()
                            .filter(|_| self.view == View::Player)
                            .map(|p| file_title(p))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(self.icon_control(
                        "keys",
                        "keyboard",
                        "Keyboard shortcuts (?)",
                        Control::Keys,
                        cx,
                    ))
                    .child(self.icon_control(
                        "recent",
                        "history",
                        "Recent files",
                        Control::Recent,
                        cx,
                    ))
                    .child(self.control(
                        "open",
                        if self.view == View::Library {
                            "Add videos"
                        } else {
                            "Open"
                        },
                        if self.view == View::Library {
                            Control::AddVideos
                        } else {
                            Control::Open
                        },
                        cx,
                    )),
            );
        let video = div()
            .id("video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000))
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _, _| video_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_click(cx.listener(|this, event: &ClickEvent, window, _cx| {
                this.focus.focus(window);
                if event.click_count() == 2 {
                    window.toggle_fullscreen();
                }
            }))
            .when_some(self.frame.clone(), |d, frame| {
                d.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            })
            .when(self.frame.is_none(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .text_center()
                        .child(
                            div()
                                .size(px(56.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(12.))
                                .border_1()
                                .border_color(rgb(LINE))
                                .bg(rgb(BG))
                                .child(icon("film", 24., TEXT)),
                        )
                        .child(
                            div()
                                .mt_5()
                                .text_size(px(23.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(if self.path.is_some() {
                                    if self.error.is_some() {
                                        "Unable to open video"
                                    } else {
                                        "Opening video…"
                                    }
                                } else {
                                    "Open a video"
                                }),
                        )
                        .child(
                            div()
                                .mt_2()
                                .text_size(px(13.))
                                .text_color(rgb(MUTED))
                                .child("Choose a local file or drag it into this window."),
                        )
                        .child(div().mt_6().child(self.control(
                            "open-primary",
                            "Open video",
                            Control::Open,
                            cx,
                        ))),
                )
            });
        let timeline_bar = div()
            .id("timeline")
            .relative()
            .h(px(20.))
            .flex_1()
            .cursor_pointer()
            .flex()
            .items_center()
            .child(
                canvas(move |bounds, _, _| timeline.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(3.))
                    .rounded_full()
                    .bg(rgb(LINE))
                    .child(
                        div()
                            .w(relative(progress))
                            .h_full()
                            .rounded_full()
                            .bg(rgb(TEXT)),
                    )
                    .when(self.path.is_some(), |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(-2.5))
                                .left(relative(progress))
                                .ml(px(-4.))
                                .size(px(8.))
                                .rounded_full()
                                .bg(rgb(TEXT)),
                        )
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.scrubbing = true;
                    this.seek_at(e.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.scrubbing {
                    this.seek_at(e.position.x, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.scrubbing = false),
            );
        let volume_slider = div()
            .id("volume-slider")
            .relative()
            .w(px(72.))
            .h(px(20.))
            .cursor_pointer()
            .flex()
            .items_center()
            .tooltip(|_, cx| cx.new(|_| Hint("Volume".into())).into())
            .child(
                canvas(
                    move |bounds, _, _| volume_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div().w_full().h(px(3.)).rounded_full().bg(rgb(LINE)).child(
                    div()
                        .w(relative(volume))
                        .h_full()
                        .rounded_full()
                        .bg(rgb(if self.snapshot.muted { MUTED } else { TEXT })),
                ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.changing_volume = true;
                    this.volume_at(e.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.changing_volume {
                    this.volume_at(e.position.x, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.changing_volume = false),
            );
        let controls = div()
            .flex()
            .items_center()
            .w_full()
            .h(px(44.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.icon_control(
                        "mute",
                        if self.snapshot.muted {
                            "volume-x"
                        } else {
                            "volume-2"
                        },
                        if self.snapshot.muted {
                            "Unmute (M)"
                        } else {
                            "Mute (M)"
                        },
                        Control::Mute,
                        cx,
                    ))
                    .child(volume_slider)
                    .child(
                        div()
                            .w(px(32.))
                            .ml_1()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!("{:.0}%", self.snapshot.volume)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(self.icon_control(
                        "back",
                        "rotate-ccw",
                        "Back 10 seconds (←)",
                        Control::Back,
                        cx,
                    ))
                    .child(self.icon_control(
                        "play",
                        if playing { "pause" } else { "play" },
                        if playing {
                            "Pause (Space)"
                        } else {
                            "Play (Space)"
                        },
                        Control::Play,
                        cx,
                    ))
                    .child(self.icon_control(
                        "forward",
                        "rotate-cw",
                        "Forward 10 seconds (→)",
                        Control::Forward,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .id("speed")
                            .h(px(34.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .hover(|s| s.bg(rgb(HOVER)))
                            .tooltip(|_, cx| {
                                cx.new(|_| Hint("Playback speed · click to cycle".into()))
                                    .into()
                            })
                            .child(format!("{}×", self.snapshot.speed))
                            .child(icon("chevron-down", 12., MUTED))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.handle(Control::Speed, window, cx)
                            })),
                    )
                    .child(self.icon_control(
                        "tracks",
                        "captions",
                        "Audio & subtitles",
                        Control::Tracks,
                        cx,
                    ))
                    .child(self.icon_control(
                        "fullscreen",
                        if window.is_fullscreen() {
                            "minimize"
                        } else {
                            "maximize"
                        },
                        if window.is_fullscreen() {
                            "Exit fullscreen (F)"
                        } else {
                            "Fullscreen (F)"
                        },
                        Control::Fullscreen,
                        cx,
                    )),
            );
        let footer = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .h(px(FOOTER_HEIGHT))
            .justify_center()
            .gap_3()
            .px_6()
            .py_4()
            .border_t_1()
            .border_color(rgb(LINE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(20.))
                    .w_full()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(div().w(px(42.)).child(format_time(self.snapshot.position)))
                    .child(timeline_bar)
                    .child(
                        div()
                            .w(px(42.))
                            .text_right()
                            .child(format_time(self.snapshot.duration)),
                    ),
            )
            .child(controls);
        let menu = self.menu_panel(cx);
        let library = (self.view == View::Library).then(|| self.library_view(cx));
        div()
            .id("flick")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Inter")
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.scrubbing = false;
                    this.changing_volume = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.scrubbing = false;
                    this.changing_volume = false;
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if this.view == View::Library {
                    for path in paths.paths() {
                        if path.is_dir() {
                            this.import_folder(path.clone(), window, cx);
                        } else if path
                            .extension()
                            .is_some_and(|v| v.eq_ignore_ascii_case("torrent"))
                        {
                            this.resolve(flick::torrent::TorrentInput::File(path.clone()), cx);
                        } else {
                            this.store.data.add_video(path);
                        }
                    }
                    this.persist();
                    cx.notify();
                } else if let Some(path) = paths.paths().first() {
                    this.open(path.clone(), window, cx);
                }
            }))
            .child(header)
            .when_some(library, |d, library| d.child(library))
            .when(self.view == View::Player, |d| d.child(video))
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_6()
                        .py_2()
                        .bg(rgb(PANEL))
                        .border_t_1()
                        .border_color(rgb(LINE))
                        .child(icon("circle-alert", 16., TEXT))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .text_color(rgb(0xd4d4d4))
                                .child(error),
                        )
                        .child(self.icon_control(
                            "dismiss-error",
                            "x",
                            "Dismiss",
                            Control::DismissError,
                            cx,
                        )),
                )
            })
            .when(self.file_loaded && self.view == View::Player, |d| {
                d.child(footer)
            })
            .when_some(menu, |d, panel| {
                d.child(
                    div()
                        .absolute()
                        .right(px(24.))
                        .top(px(HEADER_HEIGHT + 8.))
                        .child(panel),
                )
            })
    }
}
