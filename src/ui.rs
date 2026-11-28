#[path = "chrome_ui.rs"]
mod chrome_ui;
#[path = "fetch_ui.rs"]
mod fetch_ui;
#[path = "library_files_ui.rs"]
mod library_files_ui;
#[path = "library_ui.rs"]
mod library_ui;
#[path = "onboarding_ui.rs"]
mod onboarding_ui;
#[path = "palette_ui.rs"]
mod palette_ui;
#[path = "pip_ui.rs"]
mod pip_ui;
#[path = "player_ui.rs"]
mod player_ui;
#[path = "remote_library_ui.rs"]
mod remote_library_ui;
#[path = "settings_ui.rs"]
mod settings_ui;

use crate::{
    assets::icon,
    input::{InputEvent, TextInput},
    model::{file_title, format_time, seek_target},
    playback::{PlaybackSnapshot, PlayerEvent},
    playback_actor::PlayerProxy as Player,
    services::{Command as ServiceCommand, Event as ServiceEvent, Services},
    shortcuts::Shortcut,
    source::{RemoteSource, Source},
    store::Store,
    theme::{ActiveTheme, Appearance, Style, Theme},
    video_frame_ui::VideoFrameView,
};
use chrome_ui::{h_flex, v_flex};
use flick::remote::protocol::Quality;
use gpui::{
    Animation, AnimationExt, AnyWindowHandle, App, Bounds, ClickEvent, ClipboardItem, Context, Div,
    ElementId, Entity, ExternalPaths, FocusHandle, Focusable, KeyDownEvent, MouseButton, ObjectFit,
    PathPromptOptions, Pixels, RenderImage, SharedString, Stateful, Window, canvas, div, img,
    prelude::*, px, relative, rgb, rgba,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// Sets network properties before a load: the bearer header and cache
/// tuning for a remote video, or no header and libmpv's defaults for local
/// files, so nothing carries over from an earlier remote load.
fn set_network_profile(player: &Player, remote: Option<&RemoteSource>) {
    let header = remote.map(RemoteSource::header).unwrap_or_default();
    if let Err(error) = player.set_property(crate::source::HTTP_HEADERS, &header) {
        tracing::warn!(%error, "Could not set HTTP request headers");
    }
    for (name, tuned, default) in crate::source::NETWORK_TUNING {
        let value = if remote.is_some() { tuned } else { default };
        let _ = player.set_optional_property(name, value);
    }
}

/// The bearer header stays set for the whole remote session. libmpv opens a
/// new HTTP stream for every HLS segment and for reconnects after a seek,
/// and reads `http-header-fields` each time, so clearing it after the file
/// loads breaks playback at the next segment (checked on 2026-11-10 with
/// mpv 0.40: the second segment answered 401 and playback stopped at 8 s).
/// Every `player.load` sets the header for its own server or clears it for a
/// local file first, so it never reaches another server or a local load.
/// Removes the bearer header so no later request or load carries it.
fn clear_http_headers(player: &Player) {
    if let Err(error) = player.set_property(crate::source::HTTP_HEADERS, "") {
        tracing::warn!(%error, "Could not clear HTTP request headers");
    }
}

/// How long playback controls stay visible without pointer movement.
const CONTROLS_TIMEOUT: Duration = Duration::from_millis(2500);

/// Clicks that open a video can finish as a double-click on the new video
/// surface. Ignore video double-clicks this soon after entering the player.
const DOUBLE_CLICK_GRACE: Duration = Duration::from_millis(500);

/// How long player controls take to fade in or out, unless motion is reduced.
const FADE: Duration = Duration::from_millis(180);

/// How long a keyboard action's status message stays on the video.
const STATUS_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long an automatic quality change is explained on the video.
const AUTO_QUALITY_NOTICE: Duration = Duration::from_secs(4);

/// How long the next episode waits before starting.
const UP_NEXT_DELAY: Duration = Duration::from_secs(8);

/// Playback speeds stepped through by the shuttle-forward key.
const SHUTTLE_SPEEDS: [f64; 4] = [1., 1.5, 2., 4.];

/// Speeds in the speed menu, and the steps of the faster and slower keys.
const SPEEDS: [f64; 12] = [0.25, 0.5, 0.75, 1., 1.25, 1.5, 1.75, 2., 3., 4., 8., 16.];

#[derive(Clone, Copy, PartialEq)]
enum View {
    Library,
    Player,
    /// First-launch setup; see `onboarding_ui.rs`.
    Onboarding,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Videos,
    History,
    Shows,
    Downloads,
    Settings,
    Shortcuts,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Videos => "All Videos",
            Self::History => "History",
            Self::Shows => "Shows",
            Self::Downloads => "Downloads",
            Self::Settings => "Settings",
            Self::Shortcuts => "Keyboard Shortcuts",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum VideoFilter {
    All,
    Unwatched,
    InProgress,
}

/// The context menu for a video card or history row.
struct CardMenu {
    path: PathBuf,
    position: gpui::Point<Pixels>,
    /// Shown in Continue Watching, so it can be removed from there.
    continuing: bool,
}

/// What a Downloads row's More menu acts on.
#[derive(Clone)]
enum TransferTarget {
    Local(String),
    Remote { server: String, id: String },
}

/// A Downloads row's More menu, at a window position.
struct TransferMenu {
    position: gpui::Point<Pixels>,
    target: TransferTarget,
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Tracks,
    Speed,
    /// Codecs, sizes, bitrates, dropped frames, and the cache.
    Info,
    /// The audio equalizer, reached from Tracks.
    Equalizer,
    /// The queue and chapters.
    Queue,
    /// Picture adjustments: aspect ratio, rotation, color.
    Video,
    Keys,
    /// OpenSubtitles search, reached from Tracks.
    FindSubtitles,
}

/// The state of an OpenSubtitles search in the player.
#[derive(Default)]
struct SubtitleSearch {
    generation: u64,
    searching: bool,
    results: Option<Result<Vec<flick::subtitles::SubtitleResult>, String>>,
    /// The file being downloaded.
    downloading: Option<u64>,
}

/// Settings text fields that save as they change and apply on Return.
struct PreferenceFields {
    video_filters: Entity<TextInput>,
    audio_filters: Entity<TextInput>,
    mpv_options: Entity<TextInput>,
    subtitle_font: Entity<TextInput>,
    subtitle_encoding: Entity<TextInput>,
}

impl PreferenceFields {
    fn new(data: &crate::store::Settings, cx: &mut Context<Flick>) -> Self {
        let player = &data.player;
        let subtitles = &data.appearance.subtitles;
        Self {
            video_filters: preference_field(
                cx,
                "None, or FFmpeg filters such as gradfun",
                &player.video_filters,
                |data, value| data.player.video_filters = value,
                |this, cx| this.apply_player_preferences(cx),
            ),
            audio_filters: preference_field(
                cx,
                "None, or FFmpeg filters such as loudnorm",
                &player.audio_filters,
                |data, value| data.player.audio_filters = value,
                |this, cx| this.apply_player_preferences(cx),
            ),
            mpv_options: preference_field(
                cx,
                "name=value; name=value",
                &player.mpv_options,
                |data, value| data.player.mpv_options = value,
                |this, cx| this.apply_player_preferences(cx),
            ),
            subtitle_font: preference_field(
                cx,
                "Default",
                &subtitles.font,
                |data, value| data.appearance.subtitles.font = value,
                |this, _| this.apply_subtitle_style(),
            ),
            subtitle_encoding: preference_field(
                cx,
                "Detect",
                &subtitles.encoding,
                |data, value| data.appearance.subtitles.encoding = value,
                |this, _| this.apply_subtitle_style(),
            ),
        }
    }

    fn all(&self) -> [&Entity<TextInput>; 5] {
        [
            &self.video_filters,
            &self.audio_filters,
            &self.mpv_options,
            &self.subtitle_font,
            &self.subtitle_encoding,
        ]
    }
}

fn preference_field(
    cx: &mut Context<Flick>,
    placeholder: &'static str,
    value: &str,
    save: fn(&mut crate::store::Settings, String),
    apply: fn(&mut Flick, &mut Context<Flick>),
) -> Entity<TextInput> {
    let value = value.to_owned();
    let field = cx.new(|cx| {
        let mut field = TextInput::with_placeholder(cx, placeholder);
        field.set_value(value, cx);
        field
    });
    cx.subscribe(&field, move |this, input, event, cx| {
        save(
            &mut this.store.data,
            input.read(cx).value().trim().to_owned(),
        );
        this.persist();
        if matches!(event, InputEvent::Submit) {
            apply(this, cx);
        }
    })
    .detach();
    field
}

/// What a screenshot is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shot {
    Save,
    CopyImage,
    /// Copy the text in the frame, as IINA's Live Text.
    CopyText,
}

/// What a file picker in the player is for.
#[derive(Clone, Copy)]
enum Pick {
    Video,
    Subtitles,
    Audio,
}

#[derive(Clone, Copy)]
enum Control {
    /// A rebindable player action.
    Shortcut(Shortcut),
    /// Seek to this fraction of the video.
    SeekFraction(f64),
    Open,
    Play,
    Back,
    Forward,
    Mute,
    Quieter,
    Louder,
    /// Keep the window above others.
    FloatOnTop,
    /// Size the window to this multiple of the video; 0 fits the screen.
    WindowSize(f32),
    /// Open the speed menu.
    Speed,
    Info,
    Equalizer,
    Queue,
    SetSpeed(f64),
    Fullscreen,
    Pip,
    Tracks,
    Video,
    Subtitle,
    /// Load an audio file as another audio track.
    AudioFile,
    /// Go to this many seconds into the video.
    SeekTo(f64),
    Keys,
    DismissError,
    Library,
    AddVideos,
    AddFolder,
    Share,
    PasteMagnet,
}

/// The audio stream of a remote transcode, by `AudioTrack::track`. None
/// leaves it to the server, which maps the first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RemoteAudio {
    /// What the playing stream was loaded with.
    playing: Option<u32>,
    /// What the next remote load asks for; it becomes `playing` once
    /// libmpv accepts the load, so a refused switch leaves the menu true.
    next: Option<u32>,
}

pub struct Flick {
    player: Option<Player>,
    snapshot: PlaybackSnapshot,
    store: Store,
    /// The local file of [`Self::source`], kept in sync by `set_source` for
    /// sub-modules that read it directly. `library_files_ui` clears it when
    /// it releases a deleted file; [`Self::source`] then reports nothing.
    path: Option<PathBuf>,
    /// What is playing. Read through [`Self::source`], set through
    /// [`Self::set_source`].
    source: Option<Source>,
    frame: Option<Arc<RenderImage>>,
    video_frame: Entity<VideoFrameView>,
    library_ui_state: library_ui::LibraryUiState,
    library_files_state: library_files_ui::LibraryFilesUiState,
    /// Remote libraries; everything for that feature lives inside it.
    remote: crate::remote_client::RemoteState,
    /// First-launch setup state; everything for that flow lives inside it.
    onboarding: onboarding_ui::OnboardingState,
    /// Finding Flicks on the tailnet, pairing by code, waking machines.
    discovery: crate::remote_discovery::DiscoveryState,
    /// Remote videos being copied to this computer.
    keep: crate::remote_keep::KeepState,
    /// Remote library state of this view: cards, notices, player subtitles.
    remote_ui: remote_library_ui::RemoteUiState,
    /// "Share my library on Tailscale".
    serve: crate::remote_serve::RemoteServe,
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
    /// The menu of results under the search field is open.
    search_menu: bool,
    /// Why the last online search failed, shown in the search menu.
    search_error: Option<String>,
    search_input: Entity<TextInput>,
    /// The command center, while open, and its search field.
    palette: Option<palette_ui::Palette>,
    palette_input: Entity<TextInput>,
    /// Selectable read-only copies of the share magnet and download folder.
    magnet_field: Entity<TextInput>,
    folder_field: Entity<TextInput>,
    subtitle_query: Entity<TextInput>,
    subtitle_key_field: Entity<TextInput>,
    subtitle_languages_field: Entity<TextInput>,
    /// Settings fields for filters, mpv options, and subtitle text.
    preference_fields: PreferenceFields,
    subtitle_search: SubtitleSearch,
    query: String,
    search_generation: u64,
    searching: bool,
    search_results: Option<flick::catalog::SearchResults>,
    services: Services,
    downloads: Vec<flick::torrent::TransferSnapshot>,
    delete_transfer: Option<String>,
    checking_share: Option<PathBuf>,
    resolving: bool,
    pending_torrent: Option<flick::torrent::ResolvedTorrent>,
    selected_files: HashSet<usize>,
    download_folder: PathBuf,
    shared_magnet: Option<String>,
    /// Downloads from pasted web links.
    fetches: fetch_ui::Fetches,
    importing: bool,
    /// Set when a control inside a title bar is pressed, so the title bar
    /// does not start a window move.
    control_pressed: bool,
    /// The last press inside a title bar landed on the bar, not a control.
    titlebar_pressed: bool,
    should_move: bool,
    pointer_moved_at: Instant,
    /// When the player view last opened, for [`DOUBLE_CLICK_GRACE`].
    entered_player: Instant,
    controls_shown: bool,
    /// mpv `sub-pos`, raised so subtitles stay above visible controls.
    subtitle_position: u32,
    /// Set by Tab and arrow navigation, cleared by the mouse. Focus rings
    /// and focus tooltips show only while navigating by keyboard.
    keyboard_nav: bool,
    /// Stable focus handles that put controls in the Tab order.
    control_focus: RefCell<HashMap<ElementId, FocusHandle>>,
    /// The focused element when this frame was rendered.
    focused: Option<FocusHandle>,
    /// A shortcut waiting for its new key on the shortcuts page.
    capturing: Option<Shortcut>,
    /// A short message about the last keyboard action, shown on the video.
    status: Option<(SharedString, Instant)>,
    /// How long [`Self::status`] stays.
    status_for: Duration,
    /// Automatic quality for the playing remote video; None for local
    /// files, servers that cannot transcode, or with the server's switch off.
    auto_quality: Option<crate::remote_quality::AutoQuality>,
    /// The audio stream asked of a remote transcode.
    remote_audio: RemoteAudio,
    /// The speed before shuttling faster, restored by shuttle-pause.
    shuttle_base: Option<f64>,
    /// Picture adjustments for the playing video.
    video_adjust: crate::video_adjust::VideoAdjust,
    /// Clockwise rotation in the playing video's metadata, applied as a
    /// filter because every player runs with `video-rotate=no`.
    file_rotation: u16,
    /// The picture and audio filter properties last sent to mpv, so only
    /// changes are sent again.
    picture_sent: Vec<(&'static str, String)>,
    /// Holds the display awake while a video plays.
    awake: crate::awake::DisplayAwake,
    /// The `ao` Flick set for Spatial Audio; None leaves mpv's choice.
    audio_output: Option<&'static str>,
    /// What plays next.
    queue: crate::queue::Queue,
    /// Float on Top is on.
    floating: bool,
    /// Whether the window floats now: Float on Top, or Settings' float
    /// while playing.
    floated: bool,
    /// This system cannot float the window; set once asking fails.
    float_unsupported: bool,
    /// The extra mpv options last applied, so they are sent only when the
    /// field changes.
    mpv_options_applied: Option<String>,
    /// Media keys and the system's Now Playing controls.
    now_playing: crate::now_playing::NowPlaying,
    /// Scrolling on the video not yet turned into volume or seeking: whether
    /// it was horizontal, and its pixels.
    scroll_pending: (bool, f32),
    /// Measured slider tracks, keyed by slider ID.
    slider_bounds: RefCell<HashMap<&'static str, Rc<Cell<Bounds<Pixels>>>>>,
    /// The slider being dragged.
    dragging_slider: Option<&'static str>,
    card_menu: Option<CardMenu>,
    transfer_menu: Option<TransferMenu>,
    /// The show whose episodes the Shows page lists, by show key.
    selected_show: Option<String>,
    /// The next episode, counting down after the current one ends.
    up_next: Option<(PathBuf, Instant)>,
    /// Whether the last snapshot was at the end, to notice new endings.
    was_eof: bool,
    thumbnails: crate::thumbnails::Thumbnails,
    /// Videos already sent to the thumbnail worker this session.
    poster_requests: HashSet<PathBuf>,
    poster_queue: VecDeque<PathBuf>,
    poster_viewport: HashSet<PathBuf>,
    posters_dirty: bool,
    incomplete_files: HashSet<PathBuf>,
    /// Poster results not yet saved to settings.
    posters_unsaved: bool,
    /// Started on the first timeline hover.
    previews: Option<crate::thumbnails::Previews>,
    preview_generation: u64,
    /// Results up to this generation are stale.
    preview_shown: u64,
    /// The hovered timeline position as a fraction, and its time.
    hover: Option<(f32, f64)>,
    /// The time of the last preview requested.
    preview_requested: f64,
    preview_image: Option<Arc<RenderImage>>,
    /// Bounds of the keyboard-focused card, where its menu opens.
    focused_card: Rc<Cell<Bounds<Pixels>>>,
    /// The picture-in-picture window while it is open.
    pip: Option<pip_ui::PipHandle>,
    main_window: Option<AnyWindowHandle>,
    /// Frames presented since the last debug report.
    presented: (u32, Instant),
    /// Counts control visibility changes, restarting their fade.
    controls_changes: usize,
    controls_changed_at: Instant,
    /// Counts status messages, restarting their fade.
    status_count: usize,
    /// Whether saved transfers have reported their files.
    transfers_known: bool,
    transfer_roots: Option<Vec<PathBuf>>,
    transfer_availability: crate::services::TransferAvailability,
    library_removal_epoch: u64,
    removal_generations: HashMap<PathBuf, u64>,
    quitting: bool,
    open_generation: u64,
    /// A video to open once transfers are known.
    pending_open: Option<PathBuf>,
    back_started: Option<Instant>,
    saved_position: Option<(PathBuf, f64, bool)>,
    scrub_target: Option<f64>,
    /// Playing a torrent file that is still downloading.
    streaming: bool,
    /// When the playback position last moved, to notice a stream waiting
    /// for pieces.
    position_moved: (f64, Instant),
}

impl Flick {
    pub(crate) fn prepare_quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.services.begin_shutdown();
        self.remote.shutdown();
        self.discovery.shutdown();
        self.keep.shutdown();
        self.serve.shutdown();
        self.up_next = None;
        self.pending_open = None;
        self.open_generation = self.open_generation.wrapping_add(1);
        if let Some(player) = &self.player {
            let _ = player.set_property("pause", "yes");
            player.set_render_target(0, 0);
        }
        self.snapshot.paused = true;
        self.awake.set(false);
    }

    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::load();
        crate::theme::apply(cx, &store.data.appearance);
        let search_input = cx.new(TextInput::new);
        let palette_input = cx.new(|cx| {
            TextInput::with_placeholder(cx, "Search videos and commands, or paste a link")
                .with_size(17.)
        });
        cx.subscribe_in(
            &palette_input,
            window,
            |this, _, event, window, cx| match event {
                InputEvent::Changed => this.palette_changed(cx),
                InputEvent::Submit => this.palette_submit(window, cx),
            },
        )
        .detach();
        let services = Services::start(&store.data.transfers);
        let download_folder = services.download_dir.clone();
        let magnet_field = cx.new(TextInput::read_only);
        let subtitle_query = cx.new(|cx| TextInput::with_placeholder(cx, "Title to search for"));
        cx.subscribe(&subtitle_query, |this, _, event, cx| {
            if matches!(event, InputEvent::Submit) {
                this.search_subtitles(cx);
            }
        })
        .detach();
        let subtitle_key_field = cx.new(|cx| {
            let mut field = TextInput::with_placeholder(cx, "Paste your API key");
            field.set_value(store.data.opensubtitles_key.clone(), cx);
            field
        });
        cx.subscribe(&subtitle_key_field, |this, input, event, cx| {
            if matches!(event, InputEvent::Changed) {
                this.store.data.opensubtitles_key = input.read(cx).value().trim().to_owned();
                this.persist();
            }
        })
        .detach();
        let subtitle_languages_field = cx.new(|cx| {
            let mut field = TextInput::with_placeholder(cx, "en");
            field.set_value(store.data.subtitle_languages.clone(), cx);
            field
        });
        cx.subscribe(&subtitle_languages_field, |this, input, event, cx| {
            if matches!(event, InputEvent::Changed) {
                this.store.data.subtitle_languages = input.read(cx).value().trim().to_owned();
                this.persist();
            }
        })
        .detach();
        let preference_fields = PreferenceFields::new(&store.data, cx);
        let folder_field = cx.new(|cx| {
            let mut field = TextInput::read_only(cx);
            field.set_value(download_folder.to_string_lossy().into_owned(), cx);
            field
        });
        cx.subscribe(&search_input, |this, input, event, cx| {
            match event {
                InputEvent::Changed => {
                    let value = input.read(cx).value();
                    if value == this.query {
                        return;
                    }
                    let _ = this.services.send(ServiceCommand::CancelSearch);
                    this.query = value.to_owned();
                    this.search_generation += 1;
                    this.searching = false;
                    this.search_results = None;
                    this.search_error = None;
                    // Results open under the field; the page stays put.
                    this.search_menu = !this.query.trim().is_empty();
                }
                InputEvent::Submit => {
                    this.search_menu = !this.query.trim().is_empty();
                    this.search(cx);
                }
            }
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |this, window, cx| {
            crate::theme::apply(cx, &this.store.data.appearance);
            window.refresh();
        })
        .detach();
        // Reading OS accessibility settings can start a helper process.
        cx.spawn_in(window, async move |this, cx| {
            let preferences = cx
                .background_executor()
                .spawn(async { crate::theme::SystemPreferences::detect() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                cx.set_global(preferences);
                crate::theme::apply(cx, &this.store.data.appearance);
                window.refresh();
            });
        })
        .detach();
        let streams = services.streams.clone();
        let opener: Arc<crate::playback::StreamOpener> = Arc::new(move |uri: &str| {
            streams
                .open(uri)
                .map(|stream| {
                    Box::new(crate::services::TorrentMediaStream(stream))
                        as Box<dyn crate::playback::MediaStream>
                })
                .map_err(|error| std::io::Error::other(format!("{error:#}")))
        });
        let (player, frames, error) = match Player::start(opener) {
            Ok((player, frames)) => (Some(player), Some(frames), None),
            Err(error) => (
                None,
                None,
                Some(format!("Playback is unavailable. {error:#}")),
            ),
        };
        let video_frame = cx.new(|_| VideoFrameView::default());
        let focus = cx.focus_handle();
        focus.focus(window);
        let needs_setup = onboarding_ui::needs_setup(&store.data);
        let mut this = Self {
            player,
            snapshot: PlaybackSnapshot::default(),
            store,
            path: None,
            source: None,
            frame: None,
            video_frame,
            library_ui_state: library_ui::LibraryUiState::new(cx),
            library_files_state: library_files_ui::LibraryFilesUiState::default(),
            remote: Default::default(),
            onboarding: Default::default(),
            discovery: Default::default(),
            keep: Default::default(),
            remote_ui: Default::default(),
            serve: Default::default(),
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
            view: if needs_setup {
                View::Onboarding
            } else {
                View::Library
            },
            page: Page::Home,
            video_filter: VideoFilter::All,
            search_open: false,
            search_menu: false,
            search_error: None,
            search_input,
            palette: None,
            palette_input,
            magnet_field,
            folder_field,
            subtitle_query,
            subtitle_key_field,
            subtitle_languages_field,
            preference_fields,
            subtitle_search: SubtitleSearch::default(),
            query: String::new(),
            search_generation: 0,
            searching: false,
            search_results: None,
            services,
            downloads: Vec::new(),
            delete_transfer: None,
            checking_share: None,
            resolving: false,
            pending_torrent: None,
            selected_files: HashSet::new(),
            download_folder,
            shared_magnet: None,
            fetches: Default::default(),
            importing: false,
            control_pressed: false,
            titlebar_pressed: false,
            should_move: false,
            pointer_moved_at: Instant::now(),
            entered_player: Instant::now(),
            controls_shown: true,
            subtitle_position: 100,
            keyboard_nav: false,
            control_focus: RefCell::default(),
            focused: None,
            capturing: None,
            status: None,
            status_for: STATUS_TIMEOUT,
            auto_quality: None,
            remote_audio: RemoteAudio::default(),
            shuttle_base: None,
            video_adjust: Default::default(),
            file_rotation: 0,
            picture_sent: crate::video_adjust::VideoAdjust::default()
                .properties(0)
                .into_iter()
                .chain([("af", String::new())])
                .collect(),
            awake: Default::default(),
            audio_output: None,
            scroll_pending: (false, 0.),
            queue: Default::default(),
            floating: false,
            floated: false,
            float_unsupported: false,
            mpv_options_applied: None,
            now_playing: crate::now_playing::NowPlaying::new(crate::window_ext::win32_handle(
                window,
            )),
            slider_bounds: RefCell::default(),
            dragging_slider: None,
            card_menu: None,
            transfer_menu: None,
            focused_card: Rc::default(),
            selected_show: None,
            up_next: None,
            was_eof: false,
            thumbnails: crate::thumbnails::Thumbnails::start(thumbnail_cache()),
            poster_requests: HashSet::new(),
            poster_queue: VecDeque::new(),
            poster_viewport: HashSet::new(),
            posters_dirty: true,
            incomplete_files: HashSet::new(),
            posters_unsaved: false,
            previews: None,
            preview_generation: 0,
            preview_shown: 0,
            hover: None,
            preview_requested: f64::NAN,
            preview_image: None,
            pip: None,
            main_window: Some(window.window_handle()),
            presented: (0, Instant::now()),
            controls_changes: 0,
            controls_changed_at: Instant::now(),
            status_count: 0,
            transfers_known: false,
            transfer_roots: None,
            transfer_availability: crate::services::TransferAvailability::Checking,
            library_removal_epoch: 0,
            removal_generations: HashMap::new(),
            quitting: false,
            open_generation: 0,
            pending_open: None,
            back_started: None,
            saved_position: None,
            scrub_target: None,
            streaming: false,
            position_moved: (0., Instant::now()),
        };
        this.transfers_changed(cx);
        this.init_library_files(window, cx);
        this.apply_preferences(cx);
        if this.store.data.remote.serve {
            this.start_serving(false);
        }
        cx.on_release(|this, cx| {
            this.persist();
            this.services.begin_shutdown();
            this.serve.shutdown();
            if let Err(error) = this.store.shutdown() {
                tracing::warn!(%error, "Could not flush viewing preferences on exit");
            }
            if let Some(pip) = this.pip.take() {
                let _ = pip.update(cx, |_, window, _| window.remove_window());
            }
        })
        .detach();
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
        if let Some(frames) = frames {
            cx.spawn_in(window, async move |this, cx| {
                let mut frames = match frames.await {
                    Ok(Ok(frames)) => frames,
                    result => {
                        let message = match result {
                            Ok(Err(error)) => error,
                            _ => "Playback worker stopped during startup".into(),
                        };
                        let _ = this.update_in(cx, |this, _, cx| {
                            this.error = Some(message);
                            cx.notify();
                        });
                        return;
                    }
                };
                while let Some(mut message) = frames.recv().await {
                    // Present only the newest of any frames that queued up.
                    while let Ok(newer) = frames.try_recv() {
                        message = newer;
                    }
                    if this
                        .update_in(cx, |this, window, cx| this.present(message, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        if let Some(path) = path {
            this.open(path, window, cx);
        }
        this
    }

    /// Shows a frame from the render thread, unless it belongs to a file
    /// that has since been replaced.
    fn present(
        &mut self,
        (generation, frame): (u64, Result<crate::playback::VideoFrame, String>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A final in-flight frame can arrive after Back idles the render target.
        if self.quitting || (self.view == View::Library && self.pip.is_none()) {
            return;
        }
        let Some(player) = &self.player else {
            return;
        };
        if generation != player.frame_generation() {
            return;
        }
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                if self.error.is_none() {
                    self.error = Some(format!("Could not display this video. {error}"));
                    cx.notify();
                }
                return;
            }
        };
        let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
        else {
            return;
        };
        let next = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        let first = self.frame.is_none();
        self.video_frame.update(cx, |view, cx| {
            view.frame = Some(next.clone());
            cx.notify();
        });
        if let Some(old) = self.frame.replace(next) {
            self.drop_pip_image(&old, cx);
            let _ = window.drop_image(old);
        }
        if let Some(started) = self.opened_at.take() {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                hardware_decoder = ?player.hardware_decoder(),
                "First video presentation submitted"
            );
        }
        self.presented.0 += 1;
        if self.presented.1.elapsed() >= Duration::from_secs(5) {
            tracing::debug!(
                frames = self.presented.0,
                seconds = self.presented.1.elapsed().as_secs_f32(),
                dropped_total = ?player.dropped_frames(),
                "Presented video frames"
            );
            self.presented = (0, Instant::now());
        }
        if first && self.view == View::Player && self.pip.is_none() {
            cx.notify();
        }
    }

    fn apply_preferences(&mut self, cx: &mut Context<Self>) {
        self.apply_player_preferences(cx);
        if let Some(player) = &self.player {
            let _ = player.set_property("volume", &self.store.data.volume.to_string());
            let _ = player.set_property("mute", if self.store.data.muted { "yes" } else { "no" });
            let _ = player.set_property("speed", &self.store.data.speed.to_string());
            self.snapshot = player.snapshot();
        }
        self.apply_subtitle_style();
    }

    /// Applies the subtitle appearance. Properties a libmpv build lacks are
    /// skipped; `sub-pos` follows the controls in [`Self::tick`].
    fn apply_subtitle_style(&mut self) {
        let Some(player) = &self.player else {
            return;
        };
        for (name, value) in self.store.data.appearance.subtitles.properties() {
            if let Err(error) = player.set_optional_property(name, &value) {
                tracing::debug!(%error, name, "Subtitle style not supported by this libmpv");
            }
        }
        // Force tick to reapply the base position.
        self.subtitle_position = 0;
    }

    /// Changes appearance preferences, applies them immediately, and saves.
    fn set_appearance(
        &mut self,
        change: impl FnOnce(&mut Appearance),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subtitles = self.store.data.appearance.subtitles.clone();
        change(&mut self.store.data.appearance);
        self.store.data.appearance.sanitize();
        crate::theme::apply(cx, &self.store.data.appearance);
        if self.store.data.appearance.subtitles != subtitles {
            self.apply_subtitle_style();
        }
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
            if self.file_loaded && self.pending_resume == 0. {
                if let Some(remote) = self.remote_source() {
                    // The listing's duration covers streams libmpv cannot size.
                    let duration = if s.duration > 0. {
                        s.duration
                    } else {
                        remote.duration
                    };
                    let (server, video, title) = (
                        remote.server_id.clone(),
                        remote.video_id.clone(),
                        remote.title.clone(),
                    );
                    if duration > 0. {
                        self.store
                            .data
                            .remember_remote(&server, &video, &title, s.position, duration);
                        self.upload_remote_progress(&server, &video);
                    }
                } else if s.duration > 0.
                    && let Some(path) = &self.path
                {
                    self.store.remember(path, s.position, s.duration);
                }
            }
        }
        if let Err(error) = self.store.queue_save() {
            tracing::warn!(%error, "Could not save viewing preferences");
        }
        self.last_saved = Instant::now();
        self.saved_position = self.source().map(|_| {
            (
                self.path.clone().unwrap_or_default(),
                self.snapshot.position,
                self.snapshot.paused,
            )
        });
    }

    /// What is playing. A local source counts only while `path` still names
    /// it, because `library_files_ui` clears `path` on its own.
    fn source(&self) -> Option<&Source> {
        match &self.source {
            Some(Source::Local(path)) if self.path.as_ref() != Some(path) => None,
            source => source.as_ref(),
        }
    }

    fn set_source(&mut self, source: Option<Source>) {
        self.path = source
            .as_ref()
            .and_then(Source::local_path)
            .map(PathBuf::from);
        self.source = source;
    }

    /// The playing remote video, if any.
    fn remote_source(&self) -> Option<&RemoteSource> {
        match self.source() {
            Some(Source::Remote(remote)) => Some(remote),
            _ => None,
        }
    }

    /// The tier of the playing remote video, for the player's quality picker.
    pub(crate) fn remote_quality(&self) -> Option<Quality> {
        self.remote_source().map(|remote| remote.quality)
    }

    /// Tiers the quality picker offers: those the playing video's server
    /// lists. A video still downloading on the server, or one not known to
    /// be finished because the server has not listed it yet (Watch Now from
    /// Downloads before the first listing), plays only at Original.
    pub(crate) fn quality_choices(&self) -> Vec<Quality> {
        let Some(remote) = self.remote_source() else {
            return Quality::ALL.to_vec();
        };
        crate::remote_client::picker_qualities(
            self.remote
                .video_complete(&remote.server_id, &remote.video_id),
            self.remote_server(&remote.server_id)
                .map(|server| server.qualities()),
        )
    }

    fn library_changed(&mut self, cx: &mut Context<Self>) {
        self.store.data.invalidate();
        self.library_ui_state.invalidate();
        self.posters_dirty = true;
        self.library_files_changed(cx);
        cx.notify();
    }

    fn clear_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.video_frame.update(cx, |view, cx| {
            view.frame = None;
            cx.notify();
        });
        if let Some(old) = self.frame.take() {
            self.drop_pip_image(&old, cx);
            let _ = window.drop_image(old);
        }
    }

    fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
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
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        let was_removed = self.store.data.is_removed(&path);
        cx.spawn_in(window, async move |this, cx| {
            let (path, is_file) = cx
                .background_executor()
                .spawn(async move {
                    let path = path.canonicalize().unwrap_or(path);
                    let is_file = path.is_file();
                    (path, is_file)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.open_generation
                    || (!was_removed && this.store.data.is_removed(&path))
                {
                    return;
                }
                this.open_prepared(path, is_file, window, cx);
            });
        })
        .detach();
    }

    /// Saved transfer ownership must be known before direct reads inside its roots.
    fn can_open_before_transfers(&self, path: &std::path::Path) -> bool {
        self.transfers_known
            || (path.is_absolute()
                && self
                    .store
                    .data
                    .video(path)
                    .is_none_or(|video| video.torrent.is_none())
                && self
                    .transfer_roots
                    .as_ref()
                    .is_some_and(|roots| !roots.iter().any(|root| path.starts_with(root))))
    }

    fn open_prepared(
        &mut self,
        path: PathBuf,
        is_file: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_open_before_transfers(&path) {
            if self.view == View::Player {
                self.handle(Control::Library, window, cx);
            }
            self.pending_open = Some(path);
            if let crate::services::TransferAvailability::Unavailable { message, .. } =
                &self.transfer_availability
            {
                self.error = Some(format!(
                    "This video's download state is unavailable: {message}. Restore access to the download folder, then choose Retry downloads."
                ));
            }
            cx.notify();
            return;
        }
        self.pending_open = None;
        let stream = self.incomplete_torrent_file(&path);
        if stream.is_none()
            && (self.incomplete_files.contains(&path)
                || self.store.data.unverified_torrent_file(&path).is_some())
        {
            self.error = Some(
                "This file needs torrent verification before it can be played. If its transfer is still in Downloads, resume it and wait for file checking to finish.".into(),
            );
            cx.notify();
            return;
        }
        if stream.is_none() && !is_file {
            self.error = Some(format!("This file could not be opened: {}", path.display()));
            cx.notify();
            return;
        }
        let Some(path_str) = path.to_str() else {
            self.error = Some(
                "The file path contains unsupported characters. Try renaming the file.".into(),
            );
            cx.notify();
            return;
        };
        let source = match &stream {
            Some((infohash, index)) => crate::services::stream_uri(infohash, *index),
            None => path_str.to_owned(),
        };
        self.persist();
        let Some(player) = &self.player else {
            cx.notify();
            return;
        };
        set_network_profile(player, None);
        match player.load(&source) {
            Ok(()) => {
                let resume = self.store.resume_position(&path);
                self.begin_playback(Source::Local(path), resume, stream.is_some(), window, cx);
            }
            Err(error) => self.error = Some(format!("Could not play this file. {error:#}")),
        }
        cx.notify();
    }

    /// Plays a web link without downloading it: libmpv opens direct video
    /// links itself and pages through yt-dlp, found where link downloads
    /// look for it.
    pub(crate) fn open_stream(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        // Opens still on their way, local or remote, must not replace this.
        self.open_generation = self.open_generation.wrapping_add(1);
        let title = match flick::fetch::web_link(&url) {
            Some(Ok(link)) => format!("{} video", link.site()),
            _ => url.clone(),
        };
        self.persist();
        let Some(player) = &self.player else {
            return;
        };
        set_network_profile(player, None);
        if let Ok(tools) = flick::fetch::ytdlp::Tools::discover()
            && let Some(path) = tools.ytdlp.to_str()
        {
            let _ =
                player.set_optional_property("script-opts", &format!("ytdl_hook-ytdl_path={path}"));
        }
        match player.load(&url) {
            Ok(()) => {
                self.begin_playback(Source::Stream { url, title }, 0., true, window, cx);
                self.show_status("Streaming; nothing is saved");
            }
            Err(error) => self.error = Some(format!("Could not play this link. {error:#}")),
        }
        cx.notify();
    }

    /// Plays a video from a paired remote library. Resumes where it was
    /// left, under the same rules as local files.
    pub(crate) fn open_remote(
        &mut self,
        remote: RemoteSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting {
            return;
        }
        let resume = self
            .store
            .data
            .remote_resume_position(&remote.server_id, &remote.video_id);
        self.remote_audio = RemoteAudio::default();
        let choices = crate::remote_client::picker_qualities(
            self.remote
                .video_complete(&remote.server_id, &remote.video_id),
            self.remote_server(&remote.server_id)
                .map(|server| server.qualities()),
        );
        self.auto_quality = crate::remote_quality::AutoQuality::for_open(
            self.auto_quality_enabled(&remote.server_id),
            remote.quality,
            &choices,
            Instant::now(),
        );
        self.prepare_remote(remote, resume, window, cx);
    }

    /// The per-server automatic quality switch; on for an unknown server.
    fn auto_quality_enabled(&self, server_id: &str) -> bool {
        self.store
            .data
            .remote
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .is_none_or(|server| server.auto_quality)
    }

    /// Reloads the playing remote video at a tier the person chose, at the
    /// current position, and stops automatic changes for this video. Does
    /// nothing for local files.
    pub(crate) fn switch_quality(
        &mut self,
        quality: Quality,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting || self.remote_source().is_none() {
            return;
        }
        if let Some(auto) = &mut self.auto_quality {
            auto.chosen_by_hand(quality);
        }
        self.reload_remote(Some(quality), None, window, cx);
    }

    /// Reloads the playing remote transcode with another of the server's
    /// audio streams, at the current position.
    pub(crate) fn switch_remote_audio(
        &mut self,
        track: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting {
            return;
        }
        self.reload_remote(None, Some(track), window, cx);
    }

    /// The server audio streams the player's menu offers for the playing
    /// remote video, and the one playing; empty at Original, where
    /// libmpv's own tracks are listed instead.
    pub(crate) fn remote_audio_choices(
        &self,
    ) -> (Vec<flick::remote::protocol::AudioTrack>, Option<u32>) {
        let Some(remote) = self.remote_source() else {
            return (Vec::new(), None);
        };
        let tracks = self
            .remote
            .audio_tracks(&remote.server_id, &remote.video_id);
        if !crate::source::server_audio_menu(remote.quality, &tracks) {
            return (Vec::new(), None);
        }
        let current = crate::source::current_audio(self.remote_audio.playing, &tracks);
        (tracks, current)
    }

    /// Reloads the playing remote video at the current position with a new
    /// tier and/or audio stream. Nothing happens when neither changes.
    fn reload_remote(
        &mut self,
        quality: Option<Quality>,
        audio: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(remote) = self.remote_source() else {
            return;
        };
        let mut remote = remote.clone();
        let mut next = self.remote_audio.playing;
        let mut changed = false;
        if let Some(quality) = quality
            && quality != remote.quality
        {
            remote.quality = quality;
            changed = true;
        }
        if let Some(track) = audio
            && Some(track) != next
        {
            next = Some(track);
            changed = true;
        }
        if !changed {
            return;
        }
        self.remote_audio.next = next;
        let position = if self.file_loaded {
            self.snapshot.position
        } else {
            self.pending_resume
        };
        self.prepare_remote(remote, position, window, cx);
    }

    /// Feeds automatic quality one observation and acts on its decision:
    /// a drop reloads at the lower tier, at the current position, and says
    /// why on the video for a few seconds.
    fn observe_auto_quality(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server_id, playing)) = self
            .remote_source()
            .map(|r| (r.server_id.clone(), r.quality))
        else {
            return;
        };
        // Only while the tier is actually playing: not during a reload,
        // a server check, or after the person turned the switch off.
        if !self.file_loaded
            || self.remote_ui.checking.is_some()
            || !self.auto_quality_enabled(&server_id)
        {
            return;
        }
        let Some(auto) = &mut self.auto_quality else {
            return;
        };
        // The tier loaded now confirms a drop, or shows it did not happen.
        let decision = auto.observe(
            playing,
            self.snapshot.buffering,
            self.snapshot.position,
            Instant::now(),
        );
        if let crate::remote_quality::Decision::Drop { to, reason } = decision {
            tracing::info!(quality = %to, "Dropping remote quality after buffering");
            self.reload_remote(Some(to), None, window, cx);
            self.show_status_for(reason, AUTO_QUALITY_NOTICE);
            cx.notify();
        }
    }

    /// Loads Original at once. A transcode tier waits for the server to
    /// confirm it will play it, because libmpv reports a refused HLS load
    /// only as a generic failure; the server's reason is shown instead, and
    /// whatever is playing keeps playing.
    fn prepare_remote(
        &mut self,
        remote: RemoteSource,
        resume: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remote_ui.checking = None;
        let complete = self
            .remote
            .video_complete(&remote.server_id, &remote.video_id);
        if !crate::remote_client::check_before_loading(remote.quality, complete) {
            self.load_remote(remote, resume, window, cx);
            return;
        }
        // Any open or Back before the answer cancels this one.
        self.open_generation = self.open_generation.wrapping_add(1);
        self.pending_open = None;
        let check = self
            .remote
            .check_playback(&remote.server_id, &remote.video_id, remote.quality);
        self.show_status(format!("Preparing {}…", remote.quality.label()));
        self.remote_ui.checking = Some(remote_library_ui::PendingPlayback {
            check,
            generation: self.open_generation,
            remote,
            resume,
        });
        cx.notify();
    }

    fn load_remote(
        &mut self,
        remote: RemoteSource,
        resume: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A local open still resolving in the background must not replace this.
        self.open_generation = self.open_generation.wrapping_add(1);
        self.pending_open = None;
        self.remote_ui.checking = None;
        self.persist();
        let Some(player) = &self.player else {
            cx.notify();
            return;
        };
        set_network_profile(player, Some(&remote));
        match player.load(&remote.url_with_audio(self.remote_audio.next)) {
            Ok(()) => {
                self.remote_audio.playing = self.remote_audio.next;
                tracing::info!(
                    server = %remote.server_name,
                    video = %remote.video_id,
                    quality = %remote.quality,
                    "Opening remote video"
                );
                self.begin_playback(Source::Remote(remote), resume, false, window, cx);
            }
            Err(error) => {
                clear_http_headers(player);
                self.error = Some(format!("Could not play this video. {error:#}"));
            }
        }
        cx.notify();
    }

    /// Player state for a source `player.load` accepted.
    fn begin_playback(
        &mut self,
        source: Source,
        resume: f64,
        streaming: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.thumbnails.idle_close();
        self.poster_requests.clear();
        self.poster_queue.clear();
        self.posters_dirty = true;
        if let Some(previews) = &self.previews {
            previews.idle_close();
        }
        self.pending_resume = resume;
        self.file_loaded = false;
        self.streaming = streaming;
        self.remote_ui.reset_player();
        if !matches!(source, Source::Remote(_)) {
            self.auto_quality = None;
            self.remote_audio = RemoteAudio::default();
        }
        // Repeat, subtitle timing, and picture adjustments belong to one
        // video. Reloading the same one, as a quality switch does, keeps them.
        if !self
            .source()
            .is_some_and(|playing| same_video(playing, &source))
        {
            self.reset_video_settings(cx);
        }
        let title = source.title();
        let local = source.local_path().map(PathBuf::from);
        self.set_source(Some(source));
        if let Some(path) = &local {
            self.library_files_restored([path]);
            self.store.data.restore_video(path);
        }
        match &local {
            Some(path) => self.follow_queue(path, cx),
            None => self.queue.clear(),
        }
        self.library_changed(cx);
        if self.view != View::Player {
            self.entered_player = Instant::now();
        }
        self.view = View::Player;
        self.focus.focus(window);
        self.snapshot = PlaybackSnapshot::default();
        self.error = None;
        self.menu = None;
        self.opened_at = Some(Instant::now());
        self.up_next = None;
        self.was_eof = false;
        self.hover = None;
        self.preview_requested = f64::NAN;
        self.preview_shown = self.preview_generation;
        if let Some(old) = self.preview_image.take() {
            let _ = window.drop_image(old);
        }
        self.pointer_moved_at = Instant::now();
        self.controls_shown = true;
        self.clear_frame(window, cx);
        window.set_window_title(&format!("{title} — Flick"));
        if let Some(path) = &local {
            tracing::info!(path = %path.display(), "Opening video");
        }
    }

    /// Turns off repeat, subtitle delay, and picture adjustments for the
    /// next video. mpv keeps them from one file to the next otherwise.
    fn reset_video_settings(&mut self, cx: &mut Context<Self>) {
        self.video_adjust = Default::default();
        self.file_rotation = 0;
        if let Some(player) = &self.player {
            for (name, value) in [
                ("loop-file", "no"),
                ("sub-delay", "0"),
                ("secondary-sid", "no"),
            ] {
                if let Err(error) = player.set_optional_property(name, value) {
                    tracing::debug!(%error, name, "Could not reset a playback setting");
                }
            }
        }
        self.apply_picture(cx);
    }

    /// Changes the picture adjustments and sends mpv what changed.
    fn adjust_video(
        &mut self,
        change: impl FnOnce(&mut crate::video_adjust::VideoAdjust),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.video_adjust);
        self.apply_picture(cx);
        cx.notify();
    }

    /// The media information panel's rows, read by the playback worker.
    fn player_media_rows(&self) -> Vec<(&'static str, String)> {
        self.player
            .as_ref()
            .map(|player| player.media_rows())
            .unwrap_or_default()
    }

    /// Audio outputs and the one in use, read by the playback worker.
    fn player_audio_devices(&self) -> (Vec<crate::playback::AudioDevice>, String) {
        self.player
            .as_ref()
            .map(|player| player.audio_devices())
            .unwrap_or_default()
    }

    /// Floats the window while Float on Top is on, or while a video plays
    /// with Settings › Playback › Float on top while playing.
    fn float_window(&mut self, window: &mut Window) {
        let wanted = self.floating
            || (self.store.data.player.float_while_playing
                && self.view == View::Player
                && self.playing());
        if wanted == self.floated || self.float_unsupported {
            return;
        }
        match crate::window_ext::set_floating(window, wanted) {
            Ok(()) => self.floated = wanted,
            Err(error) => {
                // Unsupported here: say so once and stop asking.
                self.float_unsupported = true;
                self.floating = false;
                self.error = Some(format!("{error:#}"));
            }
        }
    }

    /// Runs media key and Now Playing commands, and shows the playing video
    /// there.
    fn follow_media_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::now_playing::RemoteCommand as R;
        let watching = self.file_loaded && (self.view == View::Player || self.pip.is_some());
        for command in self.now_playing.take() {
            if !watching {
                break;
            }
            match command {
                R::Play if !self.playing() => self.handle(Control::Play, window, cx),
                R::Pause if self.playing() => self.handle(Control::Play, window, cx),
                R::Play | R::Pause => {}
                R::TogglePlayPause => self.handle(Control::Play, window, cx),
                R::Next => self.shortcut(Shortcut::NextEpisode, window, cx),
                R::Previous => self.shortcut(Shortcut::PreviousEpisode, window, cx),
                R::SeekTo(seconds) => self.handle(Control::SeekTo(seconds), window, cx),
                R::SeekBy(delta) => {
                    let target = seek_target(self.snapshot.position, delta, self.snapshot.duration);
                    self.handle(Control::SeekTo(target), window, cx);
                }
            }
        }
        let info = (watching && self.source().is_some()).then(|| crate::now_playing::Info {
            title: self.source().map(Source::title).unwrap_or_default(),
            duration: self.snapshot.duration,
            position: self.snapshot.position,
            speed: self.snapshot.speed,
            playing: self.playing(),
        });
        self.now_playing.update(info);
    }

    /// Sizes the window to `factor` of the video's own size, or to fit the
    /// screen with 0, as IINA's Half, Actual, and Double Size do.
    fn size_window(&mut self, factor: f32, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_fullscreen() {
            self.show_status("Leave fullscreen to resize the window");
            cx.notify();
            return;
        }
        let Some(video) = self
            .player
            .as_ref()
            .and_then(|player| player.display_size())
        else {
            self.show_status("Play a video to size the window to it");
            cx.notify();
            return;
        };
        let room = window
            .display(cx)
            .map(|display| {
                let size = display.bounds().size;
                (f32::from(size.width) * 0.9, f32::from(size.height) * 0.85)
            })
            .unwrap_or((1280., 720.));
        let (width, height) = crate::window_ext::window_size(
            video,
            factor,
            window.scale_factor(),
            room,
            (760., 500.),
        );
        window.resize(gpui::size(px(width), px(height)));
        cx.notify();
    }

    /// A menu bar command. Panels open in the player, returning to it from
    /// the library while a video is loaded.
    pub(crate) fn menu_command(
        &mut self,
        command: crate::menus::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::menus::Command as M;
        if self.quitting {
            return;
        }
        let panel = matches!(
            command,
            M::ShowQueue
                | M::VideoAdjustments
                | M::MediaInfo
                | M::Equalizer
                | M::AudioTracks
                | M::SubtitleTracks
        );
        if panel && self.view != View::Player && self.source().is_some() {
            self.entered_player = Instant::now();
            self.view = View::Player;
        }
        let control = match command {
            M::OpenVideo => Control::Open,
            M::AddVideos => Control::AddVideos,
            M::AddFolder => Control::AddFolder,
            M::PasteLink => Control::PasteMagnet,
            M::PlayPause => Control::Play,
            M::SeekBack => Control::Back,
            M::SeekForward => Control::Forward,
            M::Slower => Control::Shortcut(Shortcut::SpeedDown),
            M::Faster => Control::Shortcut(Shortcut::SpeedUp),
            M::NormalSpeed => Control::Shortcut(Shortcut::SpeedReset),
            M::PreviousChapter => Control::Shortcut(Shortcut::PreviousChapter),
            M::NextChapter => Control::Shortcut(Shortcut::NextChapter),
            M::PreviousVideo => Control::Shortcut(Shortcut::PreviousEpisode),
            M::NextVideo => Control::Shortcut(Shortcut::NextEpisode),
            M::Repeat => Control::Shortcut(Shortcut::LoopFile),
            M::AbLoop => Control::Shortcut(Shortcut::LoopAb),
            M::ShowQueue => Control::Queue,
            M::Fullscreen => Control::Fullscreen,
            M::PictureInPicture => Control::Pip,
            M::FloatOnTop => Control::FloatOnTop,
            M::HalfSize => Control::WindowSize(0.5),
            M::ActualSize => Control::WindowSize(1.),
            M::DoubleSize => Control::WindowSize(2.),
            M::FitToScreen => Control::WindowSize(0.),
            M::VideoAdjustments => Control::Video,
            M::Screenshot => Control::Shortcut(Shortcut::Screenshot),
            M::CopyFrame => Control::Shortcut(Shortcut::CopyScreenshot),
            M::CopyText => Control::Shortcut(Shortcut::CopyText),
            M::MediaInfo => Control::Info,
            M::PreviousFrame => Control::Shortcut(Shortcut::FrameBack),
            M::NextFrame => Control::Shortcut(Shortcut::FrameForward),
            M::Mute => Control::Mute,
            M::VolumeUp => Control::Louder,
            M::VolumeDown => Control::Quieter,
            M::AudioEarlier => Control::Shortcut(Shortcut::AudioDelayDown),
            M::AudioLater => Control::Shortcut(Shortcut::AudioDelayUp),
            M::LoadAudio => Control::AudioFile,
            M::Equalizer => Control::Equalizer,
            M::AudioTracks | M::SubtitleTracks => Control::Tracks,
            M::LoadSubtitles => Control::Subtitle,
            M::SubtitlesEarlier => Control::Shortcut(Shortcut::SubtitleDelayDown),
            M::SubtitlesLater => Control::Shortcut(Shortcut::SubtitleDelayUp),
            M::Settings => return self.show_page(Page::Settings, window, cx),
            M::FindSubtitles => {
                if self.view != View::Player && self.source().is_some() {
                    self.view = View::Player;
                }
                return self.find_subtitles(window, cx);
            }
            M::OpenUrl | M::GoToTime => {
                if self.palette.is_none() {
                    self.toggle_palette(window, cx);
                }
                self.show_status(if command == M::GoToTime {
                    "Type a time, such as 12:34"
                } else {
                    "Paste a video link to play or download it"
                });
                return;
            }
            M::KeyboardShortcuts if self.view != View::Player => {
                return self.show_page(Page::Shortcuts, window, cx);
            }
            M::KeyboardShortcuts => Control::Keys,
        };
        self.handle(control, window, cx);
    }

    /// Shows Flick's log file in the file manager.
    fn show_log(&mut self, cx: &mut Context<Self>) {
        match crate::logs::folder() {
            Some(folder) => cx.reveal_path(&folder.join("flick.log")),
            None => {
                self.error = Some(
                    "Flick could not create its log folder, so messages go only to the terminal."
                        .into(),
                );
                cx.notify();
            }
        }
    }

    /// Changes Settings › Player preferences, saves, and applies them.
    fn set_player(
        &mut self,
        change: impl FnOnce(&mut crate::player_prefs::PlayerPreferences),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.store.data.player);
        self.store.data.player.sanitize();
        self.persist();
        self.apply_player_preferences(cx);
    }

    /// The aspect ratio, deinterlacing, and filter chains for the playing
    /// video: Flick's picture adjustments and the file's own rotation, then
    /// the equalizer and the filters from Settings.
    fn picture_properties(&self) -> Vec<(&'static str, String)> {
        let preferences = &self.store.data.player;
        self.video_adjust
            .properties(self.file_rotation)
            .into_iter()
            .map(|(name, value)| {
                if name == "vf" {
                    (name, preferences.video_filter_chain(&value))
                } else {
                    (name, value)
                }
            })
            .chain([("af", preferences.audio_filter_chain())])
            .collect()
    }

    /// Sends mpv the picture and filter properties that changed.
    fn apply_picture(&mut self, cx: &mut Context<Self>) {
        let wanted = self.picture_properties();
        for (name, value) in &wanted {
            if !self.picture_sent.contains(&(*name, value.clone())) {
                self.property(name, value, cx);
            }
        }
        self.picture_sent = wanted;
    }

    /// Applies Settings › Player and Advanced: the volume ceiling,
    /// ReplayGain, the audio output, filters, and extra mpv options.
    fn apply_player_preferences(&mut self, cx: &mut Context<Self>) {
        let preferences = self.store.data.player.clone();
        if let Some(player) = &self.player {
            for (name, value) in preferences.properties() {
                if let Err(error) = player.set_optional_property(name, &value) {
                    tracing::debug!(%error, name, "Player preference not supported by this libmpv");
                }
            }
        }
        self.apply_audio_output(cx);
        self.apply_picture(cx);
        // Extra options go to mpv only when the field changed, so other
        // settings never repeat their errors. One taken out keeps its value
        // until Flick starts again.
        if self.mpv_options_applied.as_deref() == Some(preferences.mpv_options.as_str()) {
            cx.notify();
            return;
        }
        self.mpv_options_applied = Some(preferences.mpv_options.clone());
        let (options, errors) = preferences.extra_options();
        if let Some(player) = &self.player {
            for (name, value) in &options {
                if let Err(error) = player.set_property(name, value) {
                    self.error = Some(format!("Could not set the mpv option {name}: {error:#}"));
                }
            }
        }
        if let Some(line) = errors.first() {
            self.error = Some(format!(
                "“{line}” is not an mpv option. Write one name=value a line."
            ));
        }
        cx.notify();
    }

    /// Switches mpv's audio output for Spatial Audio, reloading it so the
    /// change takes effect now.
    fn apply_audio_output(&mut self, cx: &mut Context<Self>) {
        let wanted = self.store.data.player.audio_output();
        if wanted == self.audio_output {
            return;
        }
        let previous = std::mem::replace(&mut self.audio_output, wanted);
        let Some(player) = &self.player else {
            return;
        };
        // Back to mpv's own choice, or the null output tests ask for.
        let value = wanted.unwrap_or_else(|| {
            if std::env::var("FLICK_AUDIO").as_deref() == Ok("null") {
                "null"
            } else {
                ""
            }
        });
        if let Err(error) = player.set_optional_property("ao", value) {
            tracing::debug!(%error, "Audio output not supported by this libmpv");
            self.audio_output = previous;
            return;
        }
        let _ = player.command(&["ao-reload"]);
        cx.notify();
    }

    /// Saves the frame on screen at the video's own size, with subtitles
    /// unless Settings say otherwise, to the screenshot folder; copies it as
    /// a PNG; or copies the text Vision finds in it, as IINA's Live Text.
    /// Folders are made, files read, and text recognized off the UI thread.
    fn screenshot(&mut self, shot: Shot, cx: &mut Context<Self>) {
        let clipboard = shot != Shot::Save;
        let preferences = &self.store.data.player;
        let folder = if clipboard {
            Some(std::env::temp_dir().join("flick-screenshots"))
        } else {
            preferences
                .screenshot_folder
                .clone()
                .or_else(crate::screenshot::folder)
        };
        let Some(folder) = folder else {
            self.error = Some("There is no Pictures or home folder to save screenshots in.".into());
            return;
        };
        let extension = if clipboard {
            "png"
        } else {
            preferences.screenshot_format.extension()
        };
        let mode = if preferences.screenshot_subtitles || shot == Shot::CopyText {
            "subtitles"
        } else {
            "video"
        };
        // A file name's extension is not part of the title.
        let title = match self.source() {
            Some(Source::Local(path)) => path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Some(source) => source.title(),
            None => return,
        };
        let position = self.snapshot.position;
        let generation = self.open_generation;
        cx.spawn(async move |this, cx| {
            let background = cx.background_executor().clone();
            let saved = background
                .spawn(async move {
                    std::fs::create_dir_all(&folder).map_err(|error| {
                        format!("Could not create {}: {error}", folder.display())
                    })?;
                    let path = crate::screenshot::path(&folder, &title, position, extension, |p| {
                        p.exists()
                    });
                    path.to_str().map(str::to_owned).ok_or_else(|| {
                        "The screenshot folder's path contains unsupported characters.".to_owned()
                    })
                })
                .await;
            let path = match saved {
                Ok(path) => path,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(error);
                        cx.notify();
                    });
                    return;
                }
            };
            let sent = this
                .update(cx, |this, cx| {
                    if generation != this.open_generation {
                        return false;
                    }
                    this.command(&["screenshot-to-file", &path, mode], cx);
                    if !clipboard {
                        // "Pictures/Flick/Sintel at 12.34.png"
                        let shown = std::path::Path::new(&path)
                            .iter()
                            .rev()
                            .take(3)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<PathBuf>();
                        this.show_status_for(
                            format!("Saved {}", shown.display()),
                            Duration::from_secs(3),
                        );
                    }
                    true
                })
                .unwrap_or(false);
            if !clipboard || !sent {
                return;
            }
            // libmpv writes the file after the command returns; wait for a
            // whole PNG, then copy it, or its text, and remove the file.
            let mut bytes = None;
            for _ in 0..100 {
                background.timer(Duration::from_millis(50)).await;
                let file = path.clone();
                let read = background
                    .spawn(async move { std::fs::read(&file).ok() })
                    .await;
                if let Some(read) = read.filter(|read| crate::screenshot::complete_png(read)) {
                    bytes = Some(read);
                    break;
                }
            }
            let file = std::path::PathBuf::from(&path);
            let text = if shot == Shot::CopyText && bytes.is_some() {
                let image = file.clone();
                Some(
                    background
                        .spawn(async move { crate::live_text::recognize(&image) })
                        .await,
                )
            } else {
                None
            };
            background
                .spawn(async move {
                    let _ = std::fs::remove_file(file);
                })
                .detach();
            let _ = this.update(cx, |this, cx| {
                match (bytes, text) {
                    (None, _) => this.error = Some("The frame could not be copied.".into()),
                    (Some(_), Some(Ok(lines))) if lines.is_empty() => {
                        this.show_status("No text in this frame")
                    }
                    (Some(_), Some(Ok(lines))) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(lines.join("\n")));
                        this.show_status(if lines.len() == 1 {
                            "Copied 1 line of text".to_owned()
                        } else {
                            format!("Copied {} lines of text", lines.len())
                        });
                    }
                    (Some(_), Some(Err(error))) => this.error = Some(format!("{error:#}")),
                    (Some(bytes), None) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_image(
                            &gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes),
                        ));
                        this.show_status("Copied the frame");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Scrolling on the video: up and down change the volume, left and right
    /// seek, one percent or second for every few pixels, as in IINA.
    fn scroll_video(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        if self.source().is_none() || !self.file_loaded || self.pip.is_some() {
            return;
        }
        let delta = event.delta.pixel_delta(px(crate::model::SCROLL_LINE));
        let (x, y) = (f32::from(delta.x), f32::from(delta.y));
        let horizontal = x.abs() > y.abs();
        if horizontal != self.scroll_pending.0
            || matches!(event.touch_phase, gpui::TouchPhase::Started)
        {
            self.scroll_pending = (horizontal, 0.);
        }
        let (units, rest) =
            crate::model::scroll_units(self.scroll_pending.1, if horizontal { x } else { y });
        self.scroll_pending.1 = rest;
        if units == 0 {
            return;
        }
        if horizontal {
            // Content moving left reveals what comes later.
            let target = seek_target(
                self.snapshot.position,
                -f64::from(units),
                self.snapshot.duration,
            );
            let flags = self.store.data.player.seek_flags();
            self.command(&["seek", &target.to_string(), flags], cx);
            self.snapshot.position = target;
            self.show_status(format!(
                "{} / {}",
                format_time(target),
                format_time(self.snapshot.duration)
            ));
        } else {
            let max = f64::from(self.store.data.player.max_volume);
            let volume = (self.snapshot.volume + f64::from(units)).clamp(0., max);
            self.property("volume", &volume.to_string(), cx);
            self.show_status(format!("Volume {volume:.0}%"));
        }
    }

    /// The torrent and file index of a selected file that is still
    /// downloading at `path`.
    fn incomplete_torrent_file(&self, path: &std::path::Path) -> Option<(String, usize)> {
        self.downloads.iter().find_map(|download| {
            download
                .files
                .iter()
                .find(|file| {
                    file.selected
                        && file.path == path
                        && (file.verified_bytes < file.length
                            || matches!(download.state, flick::torrent::TransferState::Checking))
                })
                .map(|file| (download.infohash.clone(), file.index))
        })
    }

    /// Paths of selected torrent files that are not fully verified.
    fn incomplete_paths(&self) -> HashSet<PathBuf> {
        self.downloads
            .iter()
            .flat_map(|download| {
                download.files.iter().filter(move |file| {
                    !file.selected
                        || file.verified_bytes < file.length
                        || matches!(
                            download.state,
                            flick::torrent::TransferState::Checking
                                | flick::torrent::TransferState::Error
                        )
                })
            })
            .map(|file| file.path.clone())
            .chain(
                self.store
                    .data
                    .unverified_torrent_files
                    .iter()
                    .map(|file| file.path.clone()),
            )
            .collect()
    }

    /// Whether a text field has keyboard focus, so typing must reach it.
    fn text_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        [
            &self.search_input,
            &self.palette_input,
            &self.magnet_field,
            &self.folder_field,
            &self.subtitle_query,
            &self.subtitle_key_field,
            &self.subtitle_languages_field,
        ]
        .into_iter()
        .chain(self.preference_fields.all())
        .any(|input| input.read(cx).focused(window))
    }

    /// Whether a video double-click should toggle fullscreen.
    fn accepts_video_double_click(&self) -> bool {
        self.entered_player.elapsed() >= DOUBLE_CLICK_GRACE
    }

    /// Waiting for data: mpv's cache says so, or a torrent stream that
    /// should be playing has not advanced for a second.
    fn buffering(&self) -> bool {
        self.snapshot.buffering
            || (self.streaming
                && self.file_loaded
                && self.playing()
                && self.position_moved.1.elapsed() > Duration::from_secs(1))
    }

    fn playing(&self) -> bool {
        self.source().is_some() && !self.snapshot.paused && !self.snapshot.eof
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
            || (self.keyboard_nav && self.control_has_focus())
            || self.pointer_moved_at.elapsed() < CONTROLS_TIMEOUT
    }

    fn transfers_changed(&mut self, cx: &mut Context<Self>) {
        let incomplete = self.incomplete_paths();
        if incomplete != self.incomplete_files {
            self.incomplete_files = incomplete;
            self.posters_dirty = true;
            self.library_files_changed(cx);
        }
    }

    /// Admit a bounded backlog when idle; never rescan the library per frame.
    fn update_posters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let invalidations = self.thumbnails.take_cache_invalidations();
        if invalidations.rescan_required {
            self.reconcile_library_files();
        }
        if !invalidations.paths.is_empty() {
            let evicted: HashSet<_> = invalidations.paths.into_iter().collect();
            for poster in &evicted {
                self.evict_library_poster(poster, window, cx);
            }
            for video in &mut self.store.data.library {
                if video
                    .poster
                    .as_ref()
                    .is_some_and(|poster| evicted.contains(poster))
                {
                    video.poster = None;
                    self.poster_requests.remove(&video.path);
                    self.posters_dirty = true;
                }
            }
            self.store.data.invalidate_metadata();
            self.library_ui_state.invalidate();
            cx.notify();
        }
        let mut changed = false;
        for poster in self.thumbnails.drain().collect::<Vec<_>>() {
            let Some(video) = self
                .store
                .data
                .library
                .iter_mut()
                .find(|v| v.path == poster.video)
            else {
                continue;
            };
            match poster.result {
                Ok((image, details)) => {
                    video.poster = Some(image);
                    video.duration = details.duration;
                    video.width = details.width;
                    video.height = details.height;
                    changed = true;
                }
                Err(error) => tracing::debug!(path = %poster.video.display(), %error, "No poster"),
            }
        }
        if changed {
            self.store.data.invalidate_metadata();
            self.library_ui_state.invalidate();
            self.posters_unsaved = true;
            cx.notify();
        }
        if self.posters_unsaved && self.last_saved.elapsed() > Duration::from_secs(2) {
            self.posters_unsaved = false;
            self.persist();
        }
        if self.view == View::Player || (self.playing() && self.pip.is_some()) {
            return;
        }
        let visible = self
            .library_ui_state
            .visible_poster_paths()
            .into_iter()
            .collect::<HashSet<_>>();
        if visible != self.poster_viewport {
            for path in self.poster_viewport.difference(&visible) {
                self.thumbnails.cancel(path);
                self.poster_requests.remove(path);
            }
            self.poster_viewport = visible;
            self.posters_dirty = true;
        }
        if self.posters_dirty {
            // Offscreen work must not feed an eviction/regeneration loop once
            // the library is larger than the disk poster quota.
            self.poster_queue =
                visible_poster_candidates(&self.store.data, &self.poster_viewport, |path| {
                    self.poster_requests.contains(path)
                        || self.incomplete_files.contains(path)
                        || self.library_file_unavailable(path)
                        || !self.can_open_before_transfers(path)
                });
            self.posters_dirty = false;
        }
        while let Some(path) = self.poster_queue.front() {
            if !self.thumbnails.try_request(path.clone()) {
                break;
            }
            self.poster_requests.insert(path.clone());
            self.poster_queue.pop_front();
        }
    }

    /// Shows the frame under the pointer while hovering the timeline.
    fn hover_timeline(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let bounds = self.timeline.get();
        let duration = self.snapshot.duration;
        // Previews decode the local file; remote videos have none.
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.streaming || self.incomplete_files.contains(&path) {
            return;
        }
        if duration <= 0. || bounds.size.width <= px(0.) {
            return;
        }
        let fraction =
            (f32::from(x - bounds.origin.x) / f32::from(bounds.size.width)).clamp(0., 1.);
        let time = f64::from(fraction) * duration;
        self.hover = Some((fraction, time));
        // Ask again once the pointer moves by about a pixel's worth of time.
        let step = (duration / f64::from(f32::from(bounds.size.width))).max(0.25);
        if (time - self.preview_requested).abs() >= step || self.preview_requested.is_nan() {
            self.preview_requested = time;
            self.preview_generation += 1;
            self.previews
                .get_or_insert_with(crate::thumbnails::Previews::start)
                .request(crate::thumbnails::PreviewRequest {
                    video: path,
                    time,
                    generation: self.preview_generation,
                });
        }
        cx.notify();
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Duration {
        self.poll_services(cx);
        if self.quitting {
            return Duration::from_millis(500);
        }
        self.poll_library_files(window, cx);
        self.remote.set_local_folder(&self.download_folder);
        self.remote.poll_transfers(
            &self.store.data,
            self.view == View::Library && self.page == Page::Downloads,
        );
        if self.remote.drain() {
            cx.notify();
        }
        self.sync_remote_settings(cx);
        let discovered = self.discovery.drain();
        self.handle_discovery_events(discovered, cx);
        let kept = self.keep.drain();
        self.notify_kept(&kept);
        self.handle_keep_events(kept, cx);
        self.apply_remote_ready(window, cx);
        for image in self.remote.take_dropped_images() {
            let _ = window.drop_image(image);
        }
        if self.serve.drain() {
            if matches!(
                self.serve.status(),
                crate::remote_serve::ServeStatus::Running { .. }
            ) {
                // A Flick paired with its own library asked before the
                // server was listening; ask again now that it is.
                self.remote.refresh(&self.store.data);
            }
            cx.notify();
        }
        if self
            .remote_ui
            .copied
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.remote_ui.copied = None;
            cx.notify();
        }
        let streams =
            std::mem::take(&mut *STREAM_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(url) = streams.into_iter().last() {
            self.open_stream(url, window, cx);
        }
        let requested =
            std::mem::take(&mut *OPEN_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()));
        let mut requested = requested.into_iter();
        if let Some(first) = requested.next() {
            // Open the first; add the rest to the library.
            self.import_paths(requested.collect(), window, cx);
            self.open(first, window, cx);
        }
        if self
            .pending_open
            .as_ref()
            .is_some_and(|path| self.can_open_before_transfers(path))
            && let Some(path) = self.pending_open.take()
        {
            self.open(path, window, cx);
        }
        self.update_posters(window, cx);
        // A preview for another video, from before it opened, is dropped.
        if let Some(preview) = self.previews.as_ref().and_then(|p| p.latest())
            && preview.generation > self.preview_shown
            && let Some(buffer) = image::RgbaImage::from_raw(
                preview.frame.width,
                preview.frame.height,
                preview.frame.pixels,
            )
        {
            self.preview_shown = preview.generation;
            let next = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            if let Some(old) = self.preview_image.replace(next) {
                let _ = window.drop_image(old);
            }
            cx.notify();
        }
        if self
            .status
            .as_ref()
            .is_some_and(|(_, shown)| shown.elapsed() >= self.status_for)
        {
            self.status = None;
            cx.notify();
        }
        if !self.controls_shown
            && self.controls_changed_at.elapsed() >= FADE
            && self.controls_changed_at.elapsed() < FADE * 3
        {
            // Unmount controls once their fade-out ends.
            cx.notify();
        }
        let shown = self.controls_wanted();
        if shown != self.controls_shown {
            self.set_controls_shown(shown);
            cx.notify();
        }
        let height = f32::from(self.video_bounds.get().size.height);
        let base = self.store.data.appearance.subtitles.position;
        let subtitle_position = if self.view == View::Player && self.controls_shown && height > 0. {
            let covered = player_ui::controls_height(self.controls_style(cx)) + 8.;
            (((1. - covered / height) * 100.).clamp(50., 100.).round() as u32).min(base)
        } else {
            base
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
        // Once a remote file has loaded or failed, libmpv no longer needs
        // the token for new requests.
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
                PlayerEvent::OperationCompleted { .. } => {}
                PlayerEvent::Error(error) => {
                    self.error = Some(error);
                }
            }
        }
        if self.last_snapshot.elapsed() >= Duration::from_millis(200) || changed {
            let mut next = player.snapshot();
            // The decoder may land on an earlier keyframe during a drag. Keep
            // the thumb at the pointer until release sends the exact seek.
            if self.scrubbing
                && let Some(target) = self.scrub_target
            {
                next.position = target;
            }
            changed |= (self.view == View::Player
                && (self.snapshot.position - next.position).abs() > 0.01)
                || self.snapshot.paused != next.paused
                || self.snapshot.duration != next.duration;
            if (next.position - self.position_moved.0).abs() > 0.01 || next.paused {
                self.position_moved = (next.position, Instant::now());
            }
            self.snapshot = next;
            self.last_snapshot = Instant::now();
        }
        // The display stays awake while a video plays, here or in picture
        // in picture, and may sleep once it pauses or ends.
        let watching = self.file_loaded && (self.view == View::Player || self.pip.is_some());
        self.awake.set(watching && self.playing());
        let pip_target = self.pip_target(cx);
        if self.pip.is_some() && pip_target.is_none() {
            // The window was closed some other way.
            self.pip = None;
            changed = true;
        }
        // Frames arrive from the render thread; see `present`.
        let target = if self.file_loaded && (self.view == View::Player || pip_target.is_some()) {
            let bounds = self.video_bounds.get();
            let width = f32::from(bounds.size.width).max(1.);
            let height = f32::from(bounds.size.height).max(1.);
            pip_target.unwrap_or_else(|| {
                flick::presentation::target_size(width, height, window.scale_factor())
            })
        } else {
            (0, 0)
        };
        player.set_render_target(target.0, target.1);
        // A phone video's rotation metadata, turned by a filter once its
        // tracks are known.
        let rotation = self
            .snapshot
            .tracks
            .iter()
            .find(|track| track.kind == "video" && track.selected)
            .map_or(0, |track| track.rotation);
        if self.file_loaded && rotation != self.file_rotation {
            self.file_rotation = rotation;
            self.apply_picture(cx);
        }
        self.follow_media_keys(window, cx);
        self.float_window(window);
        let ended = self.snapshot.eof && self.file_loaded;
        if ended && !self.was_eof {
            self.up_next = self
                .store
                .data
                .appearance
                .autoplay_next
                .then(|| self.adjacent_episode(1))
                .flatten()
                .map(|path| (path, Instant::now()));
            changed |= self.up_next.is_some();
        } else if !ended && self.up_next.is_some() {
            // Seeking back or replaying cancels the countdown.
            self.up_next = None;
            changed = true;
        }
        self.was_eof = ended;
        if let Some((path, started)) = &self.up_next
            && started.elapsed() >= UP_NEXT_DELAY
            && self.view == View::Player
        {
            let path = path.clone();
            self.persist();
            self.open(path, window, cx);
            return Duration::from_millis(16);
        }
        if self.up_next.is_some() {
            // Refresh the countdown text.
            changed = true;
        }
        if self.playing()
            && (self.view == View::Player || self.pip.is_some())
            && self.last_saved.elapsed() > Duration::from_secs(5)
            && self
                .saved_position
                .as_ref()
                .is_none_or(|(_, position, _)| (self.snapshot.position - *position).abs() >= 1.)
        {
            self.persist();
        }
        if let Some(error) = self.store.take_save_error() {
            self.error = Some(format!("Could not save viewing preferences: {error}"));
            changed = true;
        }
        if self.view == View::Player || self.pip.is_some() {
            self.observe_auto_quality(window, cx);
        }
        if changed {
            cx.notify();
        }
        // Frames no longer depend on this poll; it only follows playback
        // state, controls, and the timeline.
        if self.playing() {
            Duration::from_millis(50)
        } else if self.view == View::Player {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(500)
        }
    }

    /// Positions, wake targets, and queued commands the remote client
    /// learned, saved like any settings change, and its notifications.
    fn sync_remote_settings(&mut self, cx: &mut Context<Self>) {
        let synced = self.remote.sync_settings(&mut self.store.data);
        if synced.changed {
            if let Err(error) = self.store.queue_save() {
                tracing::warn!(%error, "Could not save remote library settings");
            }
            cx.notify();
        }
        for (title, body) in synced.notifications {
            crate::notify::notify(&title, &body);
        }
    }

    /// One notification per finished kept copy, before the views see it.
    fn notify_kept(&self, events: &[crate::remote_keep::KeepEvent]) {
        for event in events {
            let crate::remote_keep::KeepEvent::Done {
                server_id,
                video_id,
                ..
            } = event
            else {
                continue;
            };
            let server = self
                .store
                .data
                .remote
                .servers
                .iter()
                .find(|server| server.id == *server_id)
                .map_or("", |server| server.name.as_str());
            let title = self
                .remote
                .playback(&self.store.data, server_id, video_id)
                .map(|playback| playback.title)
                .unwrap_or_default();
            let (title, body) = crate::remote_client::kept_notification(&title, server);
            crate::notify::notify(&title, &body);
        }
    }

    fn command(&mut self, args: &[&str], cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if let Some(player) = &self.player {
            if let Err(error) = player.command(args) {
                self.error = Some(error.to_string());
            }
            self.snapshot = player.snapshot();
        }
        cx.notify();
    }

    fn property(&mut self, name: &str, value: &str, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if let Some(player) = &self.player {
            if let Err(error) = player.set_property(name, value) {
                self.error = Some(error.to_string());
            }
            self.snapshot = player.snapshot();
        }
        cx.notify();
    }

    fn pick(&mut self, kind: Pick, window: &Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                match kind {
                    Pick::Video => "Open video",
                    Pick::Subtitles => "Load subtitles",
                    Pick::Audio => "Load audio track",
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => if let Some(path) = paths.into_iter().next() {
                        match kind {
                            Pick::Video => this.open(path, window, cx),
                            Pick::Subtitles => if let Some(s) = path.to_str() { this.command(&["sub-add", s, "select"], cx); },
                            Pick::Audio => if let Some(s) = path.to_str() { this.command(&["audio-add", s, "select"], cx); },
                        }
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
        if page == Page::Shows {
            self.selected_show = None;
        }
        if page == Page::Home {
            // Opening the library asks every paired server for its videos.
            self.remote.refresh(&self.store.data);
        }
        self.page = page;
        self.search_menu = false;
        self.close_menus();
        if page == Page::Downloads {
            self.send(ServiceCommand::Load, cx);
        }
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Player {
            self.handle(Control::Library, window, cx);
        }
        // macOS and Windows show search on every page; GNOME only on these.
        if cx.theme().style == crate::theme::Style::Gnome
            && !matches!(self.page, Page::Home | Page::Videos)
        {
            self.page = Page::Home;
        }
        self.search_menu = !self.query.trim().is_empty();
        self.search_open = true;
        self.search_input.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn handle(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        match control {
            Control::Library => {
                self.open_generation = self.open_generation.wrapping_add(1);
                self.pending_open = None;
                self.back_started = Some(Instant::now());
                self.view = View::Library;
                self.up_next = None;
                self.hover = None;
                self.scrubbing = false;
                self.scrub_target = None;
                self.preview_generation += 1;
                self.preview_shown = self.preview_generation;
                if let Some(previews) = &self.previews {
                    previews.idle_close();
                }
                if let Some(image) = self.preview_image.take() {
                    let _ = window.drop_image(image);
                }
                self.persist();
                // Picture in picture keeps playing while the library is used.
                if self.pip.is_none() {
                    self.property("pause", "yes", cx);
                    if let Some(player) = &self.player {
                        player.set_render_target(0, 0);
                    }
                    self.clear_frame(window, cx);
                }
                self.view = View::Library;
                self.menu = None;
                self.posters_dirty = true;
                window.set_window_title("Flick");
                self.focus.focus(window);
                cx.notify();
            }
            Control::AddVideos => self.pick_library(false, false, window, cx),
            Control::AddFolder => self.pick_library(true, false, window, cx),
            Control::Share => self.open_share(window, cx),
            Control::PasteMagnet => self.paste_link(cx),
            Control::Open => self.pick(Pick::Video, window, cx),
            Control::Subtitle => self.pick(Pick::Subtitles, window, cx),
            Control::AudioFile => self.pick(Pick::Audio, window, cx),
            Control::Play if self.source().is_some() => {
                if self.snapshot.eof {
                    self.command(&["seek", "0", "absolute+exact"], cx);
                    self.property("pause", "no", cx);
                } else {
                    self.command(&["cycle", "pause"], cx);
                }
                self.pointer_moved_at = Instant::now();
            }
            Control::Back | Control::Forward if self.source().is_some() => {
                let step = f64::from(self.store.data.player.seek_step);
                let delta = if matches!(control, Control::Back) {
                    -step
                } else {
                    step
                };
                let target = seek_target(self.snapshot.position, delta, self.snapshot.duration);
                let flags = self.store.data.player.seek_flags();
                self.command(&["seek", &target.to_string(), flags], cx);
            }
            Control::Mute => self.command(&["cycle", "mute"], cx),
            Control::Quieter | Control::Louder => {
                let delta = if matches!(control, Control::Quieter) {
                    -5.
                } else {
                    5.
                };
                let max = f64::from(self.store.data.player.max_volume);
                let volume = (self.snapshot.volume + delta).clamp(0., max);
                self.property("volume", &volume.to_string(), cx);
                if volume > 100. {
                    self.show_status(format!("Volume {volume:.0}%"));
                }
            }
            Control::SetSpeed(speed) => {
                let speed = speed.clamp(crate::store::MIN_SPEED, crate::store::MAX_SPEED);
                self.shuttle_base = None;
                self.property("speed", &speed.to_string(), cx);
                self.show_status(format!("{speed}×"));
            }
            Control::Fullscreen => window.toggle_fullscreen(),
            Control::FloatOnTop => {
                if self.float_unsupported {
                    self.error =
                        Some("This system does not let Flick keep its window on top.".into());
                    cx.notify();
                    return;
                }
                self.floating = !self.floating;
                self.float_window(window);
                if self.error.is_none() {
                    self.show_status(if self.floating {
                        "Floating on top"
                    } else {
                        "Not floating"
                    });
                }
                cx.notify();
            }
            Control::WindowSize(factor) => self.size_window(factor, window, cx),
            Control::Pip => self.toggle_pip(window, cx),
            Control::DismissError => {
                self.error = None;
                cx.notify();
            }
            Control::Tracks
            | Control::Video
            | Control::Speed
            | Control::Info
            | Control::Equalizer
            | Control::Queue
            | Control::Keys => {
                let menu = match control {
                    Control::Tracks => Menu::Tracks,
                    Control::Video => Menu::Video,
                    Control::Speed => Menu::Speed,
                    Control::Info => Menu::Info,
                    Control::Equalizer => Menu::Equalizer,
                    Control::Queue => Menu::Queue,
                    _ => Menu::Keys,
                };
                self.menu = if self.menu == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                cx.notify();
            }
            Control::SeekFraction(fraction) if self.snapshot.duration > 0. => {
                let target = fraction * self.snapshot.duration;
                self.command(&["seek", &target.to_string(), "absolute+exact"], cx);
                self.show_status(format!("{}%", (fraction * 100.).round()));
            }
            Control::SeekTo(seconds) if self.source().is_some() && self.file_loaded => {
                let target = seek_target(seconds, 0., self.snapshot.duration);
                self.command(&["seek", &target.to_string(), "absolute+exact"], cx);
                self.show_status(format_time(target));
            }
            Control::Shortcut(shortcut) => self.shortcut(shortcut, window, cx),
            _ => {}
        }
    }

    /// The next (`step` 1) or previous (-1) video in the queue, or the
    /// show's adjacent episode. Local files only.
    fn adjacent_episode(&self, step: isize) -> Option<PathBuf> {
        let queued = if step > 0 {
            self.queue.next()
        } else {
            self.queue.previous()
        };
        if let Some(path) = queued {
            return Some(path.clone());
        }
        let current = self.path.as_deref()?;
        self.store.data.adjacent_episode(current, step)
    }

    /// Plays `path` from the queue, or starts a queue for it: its show's
    /// episodes in order, the rest of its folder (read off the UI thread),
    /// or just this video.
    fn follow_queue(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if self.queue.select(path) {
            return;
        }
        let seed = getrandom::u64().unwrap_or(0x9e37_79b9_7f4a_7c15);
        let episodes = self.show_episodes(path);
        if episodes.len() > 1 || !self.store.data.player.queue_folder {
            self.queue.replace(episodes, path, seed);
            return;
        }
        self.queue.replace(Vec::new(), path, seed);
        let Some(folder) = path.parent().map(PathBuf::from) else {
            return;
        };
        let playing = path.to_path_buf();
        let generation = self.open_generation;
        cx.spawn(async move |this, cx| {
            let videos = cx
                .background_executor()
                .spawn(async move {
                    let mut videos: Vec<PathBuf> = std::fs::read_dir(&folder)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| flick::library::is_video(path) && path.is_file())
                        .collect();
                    videos.sort_by(|a, b| crate::queue::natural_order(a, b));
                    videos
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Only for the same video, and a queue nobody changed.
                if generation == this.open_generation
                    && this.path.as_deref() == Some(playing.as_path())
                    && this.queue.items().len() == 1
                {
                    this.queue.replace(videos, &playing, seed);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The episodes of `path`'s show in order, one copy each, or just it.
    fn show_episodes(&self, path: &std::path::Path) -> Vec<PathBuf> {
        let data = &self.store.data;
        let mut before = Vec::new();
        let mut at = path.to_path_buf();
        while let Some(previous) = data.adjacent_episode(&at, -1) {
            if previous == path || before.contains(&previous) {
                break;
            }
            before.push(previous.clone());
            at = previous;
        }
        before.reverse();
        let mut after = Vec::new();
        let mut at = path.to_path_buf();
        while let Some(next) = data.adjacent_episode(&at, 1) {
            if next == path || after.contains(&next) || before.contains(&next) {
                break;
            }
            after.push(next.clone());
            at = next;
        }
        before
            .into_iter()
            .chain([path.to_path_buf()])
            .chain(after)
            .collect()
    }

    /// Opens the OpenSubtitles search with the video's title filled in.
    fn find_subtitles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.source().map(Source::title).unwrap_or_default();
        let query = flick::subtitles::query_from_title(&title);
        self.subtitle_query
            .update(cx, |field, cx| field.set_value(query, cx));
        self.menu = Some(Menu::FindSubtitles);
        self.subtitle_query.focus_handle(cx).focus(window);
        if self.subtitle_search.results.is_none() && !self.store.data.subtitle_key().is_empty() {
            self.search_subtitles(cx);
        }
        cx.notify();
    }

    fn search_subtitles(&mut self, cx: &mut Context<Self>) {
        let key = self.store.data.subtitle_key();
        if key.is_empty() {
            self.subtitle_search.results = Some(Err(
                "Add an OpenSubtitles API key in Settings › Subtitles to search.".into(),
            ));
            cx.notify();
            return;
        }
        self.subtitle_search.generation += 1;
        self.subtitle_search.searching = true;
        self.subtitle_search.results = None;
        let generation = self.subtitle_search.generation;
        // A remote video's hash comes from its server, which can read the
        // file; the search waits for it in `apply_remote_ready`.
        let hash = if self.remote_source().is_some() {
            match self.remote_search_hash(generation) {
                Some(hash) => hash,
                None => {
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.send_subtitle_search(self.path.clone(), hash, cx);
    }

    /// Searches OpenSubtitles for the query in the field, matching by the
    /// local file's hash or a hash the server computed.
    fn send_subtitle_search(
        &mut self,
        video: Option<PathBuf>,
        hash: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let command = ServiceCommand::SubtitleSearch {
            generation: self.subtitle_search.generation,
            key: self.store.data.subtitle_key(),
            query: self.subtitle_query.read(cx).value().to_owned(),
            video,
            hash,
            languages: self.store.data.subtitle_languages.clone(),
        };
        self.send(command, cx);
    }

    fn download_subtitle(
        &mut self,
        result: flick::subtitles::SubtitleResult,
        cx: &mut Context<Self>,
    ) {
        let fallback = thumbnail_cache()
            .and_then(|cache| cache.parent().map(|base| base.join("subtitles")))
            .unwrap_or_else(|| std::env::temp_dir().join("flick-subtitles"));
        // Beside a local file; a remote video's folder is on its server.
        let (folder, stem) = match self.source() {
            Some(Source::Local(path)) => (
                path.parent().map(PathBuf::from).unwrap_or_default(),
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "subtitles".into()),
            ),
            Some(Source::Remote(remote)) => (fallback.clone(), remote.video_id.clone()),
            Some(Source::Stream { .. }) => (fallback.clone(), "stream".to_owned()),
            None => return,
        };
        self.subtitle_search.downloading = Some(result.file_id);
        let command = ServiceCommand::SubtitleDownload {
            key: self.store.data.subtitle_key(),
            result,
            folder,
            fallback,
            stem,
        };
        self.send(command, cx);
    }

    fn show_status(&mut self, text: impl Into<SharedString>) {
        self.show_status_for(text, STATUS_TIMEOUT);
    }

    /// A status message that stays longer than a keyboard action's.
    fn show_status_for(&mut self, text: impl Into<SharedString>, duration: Duration) {
        self.status = Some((text.into(), Instant::now()));
        self.status_for = duration;
        self.status_count += 1;
    }

    fn set_controls_shown(&mut self, shown: bool) {
        self.controls_shown = shown;
        self.controls_changes += 1;
        self.controls_changed_at = Instant::now();
    }

    /// Whether hidden controls are still fading out.
    fn controls_fading(&self, cx: &App) -> bool {
        !self.controls_shown
            && !cx.theme().reduce_motion
            && self.controls_changed_at.elapsed() < FADE
    }

    fn shortcut(&mut self, shortcut: Shortcut, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = self.source().is_some() && self.file_loaded;
        match shortcut {
            Shortcut::PlayPause => self.handle(Control::Play, window, cx),
            Shortcut::SeekBack => self.handle(Control::Back, window, cx),
            Shortcut::SeekForward => self.handle(Control::Forward, window, cx),
            Shortcut::VolumeUp => self.handle(Control::Louder, window, cx),
            Shortcut::VolumeDown => self.handle(Control::Quieter, window, cx),
            Shortcut::Mute => self.handle(Control::Mute, window, cx),
            Shortcut::Fullscreen => self.handle(Control::Fullscreen, window, cx),
            Shortcut::PictureInPicture => self.handle(Control::Pip, window, cx),
            Shortcut::ShowShortcuts => self.handle(Control::Keys, window, cx),
            _ if !loaded => {}
            Shortcut::FrameBack => self.command(&["frame-back-step"], cx),
            Shortcut::FrameForward => self.command(&["frame-step"], cx),
            Shortcut::ShuttleForward => {
                if self.snapshot.paused || self.snapshot.eof {
                    self.handle(Control::Play, window, cx);
                } else {
                    let base = *self.shuttle_base.get_or_insert(self.snapshot.speed);
                    let next = SHUTTLE_SPEEDS
                        .into_iter()
                        .find(|speed| *speed > self.snapshot.speed.max(base) + 0.01)
                        .unwrap_or(SHUTTLE_SPEEDS[SHUTTLE_SPEEDS.len() - 1]);
                    self.property("speed", &next.to_string(), cx);
                    self.show_status(format!("{next}×"));
                }
            }
            Shortcut::ShuttleBack => match self.shuttle_base {
                Some(base) if self.snapshot.speed > base + 0.01 => {
                    let next = SHUTTLE_SPEEDS
                        .into_iter()
                        .rev()
                        .find(|speed| *speed < self.snapshot.speed - 0.01)
                        .unwrap_or(base)
                        .max(base);
                    if (next - base).abs() < 0.01 {
                        self.shuttle_base = None;
                    }
                    self.property("speed", &next.to_string(), cx);
                    self.show_status(format!("{next}×"));
                }
                _ => self.handle(Control::Back, window, cx),
            },
            Shortcut::ShuttlePause => {
                if let Some(base) = self.shuttle_base.take() {
                    self.property("speed", &base.to_string(), cx);
                }
                self.handle(Control::Play, window, cx);
            }
            Shortcut::PreviousChapter | Shortcut::NextChapter => {
                if self.snapshot.chapters.is_empty() {
                    self.show_status("No chapters");
                } else {
                    let step = if shortcut == Shortcut::NextChapter {
                        "1"
                    } else {
                        "-1"
                    };
                    self.command(&["add", "chapter", step], cx);
                }
            }
            Shortcut::PreviousEpisode | Shortcut::NextEpisode => {
                let step = if shortcut == Shortcut::NextEpisode {
                    1
                } else {
                    -1
                };
                match self.adjacent_episode(step) {
                    Some(path) => {
                        self.persist();
                        self.open(path, window, cx);
                    }
                    None => self.show_status(if step > 0 {
                        "Nothing up next"
                    } else {
                        "Nothing before this"
                    }),
                }
            }
            Shortcut::LoopAb => {
                let s = &self.snapshot;
                let text = match loop_step(s.loop_start, s.loop_end, s.position) {
                    LoopStep::Start(a) => {
                        self.property("ab-loop-a", &a.to_string(), cx);
                        format!("Loop from {}", format_time(a))
                    }
                    LoopStep::Span(a, b) => {
                        self.property("ab-loop-a", &a.to_string(), cx);
                        self.property("ab-loop-b", &b.to_string(), cx);
                        format!("Loop {}–{}", format_time(a), format_time(b))
                    }
                    LoopStep::Clear => {
                        self.property("ab-loop-a", "no", cx);
                        self.property("ab-loop-b", "no", cx);
                        "Loop off".into()
                    }
                };
                self.show_status(text);
            }
            Shortcut::AudioDelayDown | Shortcut::AudioDelayUp => {
                let step = if shortcut == Shortcut::AudioDelayUp {
                    "0.05"
                } else {
                    "-0.05"
                };
                self.command(&["add", "audio-delay", step], cx);
                self.show_status(format!(
                    "Audio delay {:+.0} ms",
                    self.snapshot.audio_delay * 1000.
                ));
            }
            Shortcut::SubtitleDelayDown | Shortcut::SubtitleDelayUp => {
                let step = if shortcut == Shortcut::SubtitleDelayUp {
                    "0.1"
                } else {
                    "-0.1"
                };
                self.command(&["add", "sub-delay", step], cx);
                self.show_status(format!(
                    "Subtitle delay {:+.0} ms",
                    self.snapshot.sub_delay * 1000.
                ));
            }
            Shortcut::LoopFile => {
                let repeat = !self.snapshot.loop_file;
                self.property("loop-file", if repeat { "inf" } else { "no" }, cx);
                // A repeating video never ends, so nothing is up next.
                if repeat {
                    self.up_next = None;
                }
                self.show_status(if repeat {
                    "Repeating this video"
                } else {
                    "Repeat off"
                });
            }
            Shortcut::Screenshot => self.screenshot(Shot::Save, cx),
            Shortcut::CopyScreenshot => self.screenshot(Shot::CopyImage, cx),
            Shortcut::CopyText => self.screenshot(Shot::CopyText, cx),
            Shortcut::SpeedDown | Shortcut::SpeedUp => {
                let speed = self.snapshot.speed;
                let next = if shortcut == Shortcut::SpeedUp {
                    SPEEDS.into_iter().find(|s| *s > speed + 0.01)
                } else {
                    SPEEDS.into_iter().rev().find(|s| *s < speed - 0.01)
                };
                match next {
                    Some(next) => self.handle(Control::SetSpeed(next), window, cx),
                    None => self.show_status(format!("{speed}×")),
                }
            }
            Shortcut::SpeedReset => self.handle(Control::SetSpeed(1.), window, cx),
            Shortcut::MediaInfo => self.handle(Control::Info, window, cx),
        }
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if self.handle_library_delete_key(event, window, cx)
            || self.handle_transfer_delete_key(event, window, cx)
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.control || modifiers.platform;
        let binding = crate::shortcuts::normalize(&event.keystroke);
        if let Some(shortcut) = self.capturing.take() {
            cx.stop_propagation();
            if key != "escape" {
                match self.store.data.shortcuts.assign(shortcut, &binding) {
                    Ok(Some(displaced)) => {
                        self.error = Some(format!(
                            "{} is now unassigned; {} uses {}.",
                            displaced.label(),
                            shortcut.label(),
                            crate::shortcuts::display(&binding)
                        ))
                    }
                    Ok(None) => self.error = None,
                    Err(error) => self.error = Some(error),
                }
                self.persist();
            }
            cx.notify();
            return;
        }
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
        if command && key == "k" {
            self.toggle_palette(window, cx);
            cx.stop_propagation();
            return;
        }
        if command && key == "f" && self.view == View::Library {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if command && key == "," {
            self.show_page(Page::Settings, window, cx);
            cx.stop_propagation();
            return;
        }
        if key == "tab" && !command {
            if modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
            self.keyboard_nav = true;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // A focused control activates with Enter or Space on key release.
        if matches!(key, "space" | "enter")
            && !command
            && window
                .focused(cx)
                .is_some_and(|focused| self.is_control_handle(&focused))
        {
            return;
        }
        if key == "escape" && self.close_menus() {
            self.focus.focus(window);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "escape" && self.search_menu {
            self.search_menu = false;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // Down from the search field moves into its menu.
        if key == "down" && self.search_menu && self.search_input.read(cx).focused(window) {
            let count = self.library_ui_state.search_menu_rows.borrow().len();
            self.step_search_menu(1, count, window, cx);
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
        let shortcut = self.store.data.shortcuts.lookup(&binding);
        // Onboarding has text fields too; player shortcuts must not swallow their keys.
        if matches!(self.view, View::Library | View::Onboarding) {
            if shortcut == Some(Shortcut::ShowShortcuts) {
                cx.stop_propagation();
                self.show_page(Page::Shortcuts, window, cx);
            }
            return;
        }
        let control = if command && key == "l" {
            Some(Control::Subtitle)
        } else if let Some(fraction) = crate::shortcuts::percent_key(&binding) {
            Some(Control::SeekFraction(fraction))
        } else {
            shortcut.map(Control::Shortcut)
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
            self.scrub_target = Some(target);
            if let Some(player) = &self.player {
                if let Err(error) = player.seek(target, false) {
                    self.error = Some(error.to_string());
                }
                self.snapshot.position = target;
            }
            cx.notify();
        }
    }

    fn finish_scrub(&mut self, cx: &mut Context<Self>) {
        self.scrubbing = false;
        if let Some(target) = self.scrub_target.take()
            && let Some(player) = &self.player
            && let Err(error) = player.seek(target, true)
        {
            self.error = Some(error.to_string());
        }
        if let Some(previews) = &self.previews {
            previews.cancel();
        }
        cx.notify();
    }

    fn volume_at(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let bounds = self.volume_bounds.get();
        if bounds.size.width > px(0.) {
            let value =
                (f32::from(x - bounds.origin.x) / f32::from(bounds.size.width)).clamp(0., 1.);
            let max = self.store.data.player.max_volume as f32;
            self.property("volume", &(value * max).round().to_string(), cx);
        }
    }
}

/// Admission follows the viewport, so quota eviction of offscreen posters cannot
/// generate another whole-library extraction pass.
fn visible_poster_candidates(
    data: &crate::store::Settings,
    visible: &HashSet<PathBuf>,
    blocked: impl Fn(&std::path::Path) -> bool,
) -> VecDeque<PathBuf> {
    visible
        .iter()
        .filter(|path| {
            data.video(path).is_some_and(|video| video.poster.is_none()) && !blocked(path)
        })
        .cloned()
        .collect()
}

/// Files the operating system asked Flick to open, such as from Finder's
/// Open With, waiting for the window.
static OPEN_REQUESTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

static STREAM_REQUESTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Queues `file://` URLs (and magnet links) from the platform, and web
/// links to play from `flick://open?url=` links.
pub fn queue_open_urls(urls: Vec<String>) {
    let (streams, files): (Vec<_>, Vec<_>) = urls
        .into_iter()
        .partition(|url| url.to_ascii_lowercase().starts_with("flick:"));
    STREAM_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(streams.iter().filter_map(|url| stream_url(url)));
    OPEN_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(files.iter().filter_map(|url| url_path(url)));
}

/// The web link in `flick://open?url=<link>`, from a browser bookmarklet or
/// another app, as IINA's `iina://` links work. Only `http` and `https`
/// links are played, and only by loading them; nothing else is accepted.
fn stream_url(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    if url.scheme() != "flick" || url.host_str() != Some("open") {
        return None;
    }
    let (_, link) = url.query_pairs().find(|(key, _)| key == "url")?;
    let link = reqwest::Url::parse(&link).ok()?;
    matches!(link.scheme(), "http" | "https").then(|| link.to_string())
}

fn url_path(url: &str) -> Option<PathBuf> {
    if url.starts_with("magnet:") {
        return Some(PathBuf::from(url));
    }
    let path = url.strip_prefix("file://")?;
    // Drop an optional host, keeping the absolute path.
    let path = &path[path.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let mut chars = path.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hex = [chars.next()?, chars.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// Where posters are cached: beside other data with `FLICK_DATA_DIR`, else
/// in the platform cache directory.
fn thumbnail_cache() -> Option<PathBuf> {
    std::env::var_os("FLICK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("app", "Flick", "Flick")
                .map(|dirs| dirs.cache_dir().to_owned())
        })
        .map(|base| base.join("thumbnails"))
}

/// What the A–B loop key does next.
#[derive(Debug, PartialEq)]
enum LoopStep {
    Start(f64),
    /// Start and end, in order.
    Span(f64, f64),
    Clear,
}

/// Whether two sources are one video, such as a remote video at two
/// quality tiers.
fn same_video(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Local(a), Source::Local(b)) => a == b,
        (Source::Remote(a), Source::Remote(b)) => {
            a.server_id == b.server_id && a.video_id == b.video_id
        }
        (Source::Stream { url: a, .. }, Source::Stream { url: b, .. }) => a == b,
        _ => false,
    }
}

/// Sets the loop start, then the end (swapping so the loop runs forward),
/// then clears the loop. An end too close to the start is ignored.
fn loop_step(start: Option<f64>, end: Option<f64>, position: f64) -> LoopStep {
    match (start, end) {
        (None, _) => LoopStep::Start(position),
        (Some(a), None) if (position - a).abs() < 0.1 => LoopStep::Start(a),
        (Some(a), None) => LoopStep::Span(a.min(position), a.max(position)),
        (Some(_), Some(_)) => LoopStep::Clear,
    }
}

struct Hint(SharedString);
impl Render for Hint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        hint_box(cx.theme(), self.0.clone())
    }
}

/// A tooltip's appearance, shared by hover and keyboard-focus tooltips.
fn hint_box(t: &Theme, text: SharedString) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded(px(t.radius))
        .border_1()
        .border_color(t.border)
        .bg(t.popover)
        .text_color(t.text)
        .font_family(t.font.clone())
        .text_size(t.font(12.))
        .whitespace_nowrap()
        .shadow_md()
        .child(text)
}

impl Render for Flick {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.quitting {
            let t = cx.theme();
            return self.window_frame(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(t.window)
                    .text_color(t.text)
                    .child("Closing…")
                    .into_any_element(),
                window,
                cx,
            );
        }
        self.library_ui_state.begin_poster_frame(window, cx);
        tracing::trace!(
            view = if self.view == View::Library {
                "library"
            } else {
                "player"
            },
            "Root view rendered"
        );
        let t = cx.theme().clone();
        self.focused = window.focused(cx);
        let content = match self.view {
            View::Library => {
                let started = Instant::now();
                let content = self.library_frame(window, cx);
                if let Some(back) = self.back_started.take() {
                    tracing::info!(
                        elapsed_ms = back.elapsed().as_secs_f64() * 1000.,
                        library_build_ms = started.elapsed().as_secs_f64() * 1000.,
                        "Back library tree submitted"
                    );
                    window.on_next_frame(move |_, _| {
                        tracing::info!(
                            elapsed_ms = back.elapsed().as_secs_f64() * 1000.,
                            "Back library frame rendered"
                        );
                    });
                }
                content
            }
            View::Player => self.player_frame(window, cx),
            View::Onboarding => self.render_onboarding(window, cx),
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
            .text_size(t.font(t.text_size))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.control_pressed = false;
                    if std::mem::take(&mut this.keyboard_nav) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, _, _, cx| {
                this.pointer_moved_at = Instant::now();
                if !this.controls_shown {
                    this.set_controls_shown(true);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.scrubbing {
                        this.finish_scrub(cx);
                    }
                    this.changing_volume = false;
                    this.should_move = false;
                    this.dragging_slider = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.scrubbing {
                        this.finish_scrub(cx);
                    }
                    this.changing_volume = false;
                    this.should_move = false;
                    this.dragging_slider = None;
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if this.view == View::Library {
                    this.import_paths(paths.paths().to_vec(), window, cx);
                } else if let Some(path) = paths.paths().first() {
                    this.open(path.clone(), window, cx);
                }
            }))
            .child(content)
            .children(self.card_menu_panel(cx))
            .children(self.palette_view(window, cx))
            .children(self.library_delete_panel(cx))
            .children(self.transfer_delete_panel(cx));
        self.window_frame(root.into_any_element(), window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offscreen_quota_eviction_never_requeues_the_library() {
        let mut data = crate::store::Settings::default();
        data.restore_videos((0..5000).map(|i| PathBuf::from(format!("generated-{i}.mp4"))));
        let visible = data
            .library
            .iter()
            .take(12)
            .map(|video| video.path.clone())
            .collect::<HashSet<_>>();
        let jobs = visible_poster_candidates(&data, &visible, |_| false);
        assert_eq!(jobs.len(), 12);
        assert!(jobs.iter().all(|path| visible.contains(path)));
        for video in data.library.iter_mut().take(12) {
            video.poster = Some("generated-poster.png".into());
        }
        data.invalidate_metadata();
        // 4,988 missing/evicted offscreen artifacts do not start work while idle.
        assert!(visible_poster_candidates(&data, &visible, |_| false).is_empty());
        let next = data
            .library
            .iter()
            .skip(12)
            .take(12)
            .map(|video| video.path.clone())
            .collect::<HashSet<_>>();
        let unavailable = next.iter().next().unwrap().clone();
        let jobs = visible_poster_candidates(&data, &next, |path| path == unavailable);
        assert_eq!(jobs.len(), 11);
        assert!(
            jobs.iter()
                .all(|path| next.contains(path) && path != &unavailable)
        );
    }

    #[test]
    fn opened_urls_become_paths() {
        assert_eq!(
            url_path("file:///Users/me/My%20Movies/Caf%C3%A9.mp4"),
            Some(PathBuf::from("/Users/me/My Movies/Café.mp4"))
        );
        assert_eq!(
            url_path("file://localhost/tmp/a.mkv"),
            Some(PathBuf::from("/tmp/a.mkv"))
        );
        assert_eq!(
            url_path("magnet:?xt=urn:btih:abc"),
            Some(PathBuf::from("magnet:?xt=urn:btih:abc"))
        );
        assert_eq!(url_path("https://example.com/a.mp4"), None);
        assert_eq!(url_path("file:///bad%2"), None);
    }

    #[test]
    fn flick_links_play_only_web_links() {
        assert_eq!(
            stream_url("flick://open?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc"),
            Some("https://www.youtube.com/watch?v=abc".to_owned())
        );
        assert_eq!(
            stream_url("flick://open?url=http://example.com/a.mp4"),
            Some("http://example.com/a.mp4".to_owned())
        );
        for refused in [
            "flick://open?url=file:///etc/passwd",
            "flick://open?url=av://lavfi:anullsrc",
            "flick://open?url=",
            "flick://open",
            "flick://delete?url=https://example.com",
            "https://example.com",
            "flick://open?url=not a url",
        ] {
            assert_eq!(stream_url(refused), None, "{refused}");
        }
    }

    #[test]
    fn network_profile_header_is_set_for_remote_and_empty_for_local() {
        let remote = crate::source::RemoteSource {
            server_id: "s".into(),
            server_name: "Mac mini".into(),
            video_id: "v".into(),
            title: "Clip".into(),
            duration: 90.,
            quality: flick::remote::protocol::Quality::Low720,
            base_url: "http://100.98.200.69:47117".into(),
            token: "t0ken".into(),
        };
        // The header libmpv sends on every stream it opens for this source.
        assert_eq!(remote.header(), "Authorization: Bearer t0ken");
        // A local load passes None, which maps to an empty header.
        let header = None::<&crate::source::RemoteSource>
            .map(crate::source::RemoteSource::header)
            .unwrap_or_default();
        assert_eq!(header, "");
    }

    #[test]
    fn loop_key_sets_start_then_ordered_end_then_clears() {
        assert_eq!(loop_step(None, None, 4.), LoopStep::Start(4.));
        assert_eq!(loop_step(Some(4.), None, 9.), LoopStep::Span(4., 9.));
        assert_eq!(loop_step(Some(4.), None, 1.), LoopStep::Span(1., 4.));
        assert_eq!(loop_step(Some(4.), None, 4.05), LoopStep::Start(4.));
        assert_eq!(loop_step(Some(1.), Some(4.), 2.), LoopStep::Clear);
        // A stray end without a start starts over.
        assert_eq!(loop_step(None, Some(4.), 2.), LoopStep::Start(2.));
    }
}
