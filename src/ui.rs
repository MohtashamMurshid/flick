#[path = "chrome_ui.rs"]
mod chrome_ui;
#[path = "library_ui.rs"]
mod library_ui;
#[path = "player_ui.rs"]
mod player_ui;
#[path = "settings_ui.rs"]
mod settings_ui;

use crate::{
    assets::icon,
    input::{InputEvent, TextInput},
    model::{file_title, format_time, seek_target},
    playback::{PlaybackSnapshot, Player, PlayerEvent},
    services::{Command as ServiceCommand, Event as ServiceEvent, Services},
    store::Store,
    theme::{ActiveTheme, Appearance, Style, Theme},
};
use chrome_ui::{h_flex, v_flex};
use gpui::{
    Bounds, ClickEvent, ClipboardItem, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, ObjectFit, PathPromptOptions, Pixels, RenderImage, SharedString,
    Stateful, Window, canvas, div, img, prelude::*, px, relative, rgb, rgba,
};
use std::{
    cell::Cell,
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// How long playback controls stay visible without pointer movement.
const CONTROLS_TIMEOUT: Duration = Duration::from_millis(2500);

/// Clicks that open a video can finish as a double-click on the new video
/// surface. Ignore video double-clicks this soon after entering the player.
const DOUBLE_CLICK_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq)]
enum View {
    Library,
    Player,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Videos,
    History,
    Downloads,
    Appearance,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Videos => "All Videos",
            Self::History => "History",
            Self::Downloads => "Downloads",
            Self::Appearance => "Appearance",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum VideoFilter {
    All,
    Unwatched,
    InProgress,
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
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
    Tracks,
    Subtitle,
    Keys,
    DismissError,
    Library,
    AddVideos,
    AddFolder,
    Share,
    PasteMagnet,
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
    page: Page,
    video_filter: VideoFilter,
    /// GNOME shows search in a bar below the header bar while open.
    search_open: bool,
    search_input: Entity<TextInput>,
    /// Selectable read-only copies of the share magnet and download folder.
    magnet_field: Entity<TextInput>,
    folder_field: Entity<TextInput>,
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
    /// Set when a control inside a title bar is pressed, so the title bar
    /// does not start a window move.
    control_pressed: bool,
    should_move: bool,
    pointer_moved_at: Instant,
    /// When the player view last opened, for [`DOUBLE_CLICK_GRACE`].
    entered_player: Instant,
    controls_shown: bool,
    /// mpv `sub-pos`, raised so subtitles stay above visible controls.
    subtitle_position: u32,
}

impl Flick {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::load();
        crate::theme::apply(cx, &store.data.appearance);
        let search_input = cx.new(TextInput::new);
        let services = Services::start();
        let download_folder = services.download_dir.clone();
        let magnet_field = cx.new(TextInput::read_only);
        let folder_field = cx.new(|cx| {
            let mut field = TextInput::read_only(cx);
            field.set_value(download_folder.to_string_lossy().into_owned(), cx);
            field
        });
        cx.subscribe(&search_input, |this, input, event, cx| {
            match event {
                InputEvent::Changed => {
                    let _ = this.services.send(ServiceCommand::CancelSearch);
                    this.query = input.read(cx).value().to_owned();
                    this.search_generation += 1;
                    this.searching = false;
                    this.search_results = None;
                    if !this.query.is_empty() && !matches!(this.page, Page::Home | Page::Videos) {
                        this.page = Page::Videos;
                    }
                }
                InputEvent::Submit => this.search(cx),
            }
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |this, window, cx| {
            crate::theme::apply(cx, &this.store.data.appearance);
            window.refresh();
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
            page: Page::Home,
            video_filter: VideoFilter::All,
            search_open: false,
            search_input,
            magnet_field,
            folder_field,
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
            control_pressed: false,
            should_move: false,
            pointer_moved_at: Instant::now(),
            entered_player: Instant::now(),
            controls_shown: true,
            subtitle_position: 100,
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

    /// Changes appearance preferences, applies them immediately, and saves.
    fn set_appearance(
        &mut self,
        change: impl FnOnce(&mut Appearance),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.store.data.appearance);
        crate::theme::apply(cx, &self.store.data.appearance);
        self.persist();
        window.refresh();
        cx.notify();
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
                if self.view != View::Player {
                    self.entered_player = Instant::now();
                }
                self.view = View::Player;
                self.focus.focus(window);
                self.snapshot = PlaybackSnapshot::default();
                self.error = None;
                self.menu = None;
                self.opened_at = Some(Instant::now());
                self.pointer_moved_at = Instant::now();
                self.controls_shown = true;
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

    /// Whether a text field has keyboard focus, so typing must reach it.
    fn text_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        [&self.search_input, &self.magnet_field, &self.folder_field]
            .into_iter()
            .any(|input| input.read(cx).focused(window))
    }

    /// Whether a video double-click should toggle fullscreen.
    fn accepts_video_double_click(&self) -> bool {
        self.entered_player.elapsed() >= DOUBLE_CLICK_GRACE
    }

    fn playing(&self) -> bool {
        self.path.is_some() && !self.snapshot.paused && !self.snapshot.eof
    }

    /// Player controls hide while video plays and the pointer is still.
    fn controls_wanted(&self) -> bool {
        self.view != View::Player
            || !self.file_loaded
            || !self.playing()
            || !self.store.data.appearance.autohide_controls
            || self.menu.is_some()
            || self.scrubbing
            || self.changing_volume
            || self.pointer_moved_at.elapsed() < CONTROLS_TIMEOUT
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Duration {
        self.poll_services(cx);
        let shown = self.controls_wanted();
        if shown != self.controls_shown {
            self.controls_shown = shown;
            cx.notify();
        }
        let height = f32::from(self.video_bounds.get().size.height);
        let subtitle_position = if self.view == View::Player && self.controls_shown && height > 0. {
            let covered = player_ui::controls_height(cx.theme().style) + 8.;
            ((1. - covered / height) * 100.).clamp(50., 100.).round() as u32
        } else {
            100
        };
        if subtitle_position != self.subtitle_position
            && let Some(player) = &self.player
            && player
                .set_property("sub-pos", &subtitle_position.to_string())
                .is_ok()
        {
            self.subtitle_position = subtitle_position;
        }
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
        if self.playing() {
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

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Player {
            self.handle(Control::Library, window, cx);
        }
        self.page = page;
        if page == Page::Downloads {
            self.send(ServiceCommand::Load, cx);
        }
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Player {
            self.handle(Control::Library, window, cx);
        }
        if !matches!(self.page, Page::Home | Page::Videos) {
            self.page = Page::Home;
        }
        self.search_open = true;
        self.search_input.focus_handle(cx).focus(window);
        cx.notify();
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
            Control::AddVideos => self.pick_library(false, false, window, cx),
            Control::AddFolder => self.pick_library(true, false, window, cx),
            Control::Share => self.pick_library(true, true, window, cx),
            Control::PasteMagnet => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                let text = text.trim();
                if text.starts_with("magnet:") {
                    self.resolve(flick::torrent::TorrentInput::Magnet(text.to_owned()), cx);
                } else {
                    self.error = Some(
                        "Copy a magnet link first, then choose Paste Magnet Link again.".into(),
                    );
                    cx.notify();
                }
            }
            Control::Open => self.pick(false, window, cx),
            Control::Subtitle => self.pick(true, window, cx),
            Control::Play if self.path.is_some() => {
                if self.snapshot.eof {
                    self.command(&["seek", "0", "absolute+exact"], cx);
                    self.property("pause", "no", cx);
                } else {
                    self.command(&["cycle", "pause"], cx);
                }
                self.pointer_moved_at = Instant::now();
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
            Control::Tracks | Control::Keys => {
                let menu = if matches!(control, Control::Tracks) {
                    Menu::Tracks
                } else {
                    Menu::Keys
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
        let command = modifiers.control || modifiers.platform;
        if self.view == View::Player {
            self.pointer_moved_at = Instant::now();
        }
        if command && key == "o" {
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
        if command && (key == "k" || (key == "f" && self.view == View::Library)) {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if command && key == "," {
            self.show_page(Page::Appearance, window, cx);
            cx.stop_propagation();
            return;
        }
        if key == "escape" {
            if window.is_fullscreen() {
                window.toggle_fullscreen();
            } else if self.menu.is_none() && self.view == View::Library && self.search_open {
                self.search_open = false;
                self.focus.focus(window);
            }
            self.menu = None;
            cx.notify();
            return;
        }
        if self.text_focused(window, cx) {
            return;
        }
        if self.view == View::Library && !command {
            return;
        }
        let control = if command {
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
}

struct Hint(SharedString);
impl Render for Hint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(px(t.radius))
            .border_1()
            .border_color(t.border)
            .bg(t.popover)
            .text_color(t.text)
            .font_family(t.font.clone())
            .text_size(px(12.))
            .shadow_md()
            .child(self.0.clone())
    }
}

impl Render for Flick {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let content = match self.view {
            View::Library => self.library_frame(window, cx),
            View::Player => self.player_frame(window, cx),
        };
        let root = div()
            .id("flick")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(if self.view == View::Player {
                rgb(0x000000)
            } else {
                t.window
            })
            .text_color(t.text)
            .font_family(t.font.clone())
            .text_size(px(t.text_size))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.control_pressed = false),
            )
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                this.pointer_moved_at = Instant::now();
                if !this.controls_shown {
                    this.controls_shown = true;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.scrubbing = false;
                    this.changing_volume = false;
                    this.should_move = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.scrubbing = false;
                    this.changing_volume = false;
                    this.should_move = false;
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if this.view == View::Library {
                    let mut videos = Vec::new();
                    for path in paths.paths() {
                        if path.is_dir() {
                            this.import_folder(path.clone(), window, cx);
                        } else if path
                            .extension()
                            .is_some_and(|v| v.eq_ignore_ascii_case("torrent"))
                        {
                            this.resolve(flick::torrent::TorrentInput::File(path.clone()), cx);
                        } else {
                            videos.push(path.clone());
                        }
                    }
                    this.add_library_files(videos);
                    this.persist();
                    cx.notify();
                } else if let Some(path) = paths.paths().first() {
                    this.open(path.clone(), window, cx);
                }
            }))
            .child(content);
        self.window_frame(root.into_any_element(), window, cx)
    }
}
