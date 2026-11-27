//! The macOS menu bar, as in IINA: File, Playback, Video, Audio,
//! Subtitles, and Help menus with the player's commands. Each item is an
//! app-level action that runs a [`Command`] on the main window's view, so
//! it works whatever has keyboard focus. Shortcuts stay with Flick's own
//! rebindable keys rather than menu key equivalents.

use gpui::{AnyWindowHandle, App, Global, Menu, MenuItem, WeakEntity, actions};

use crate::ui::Flick;

macro_rules! commands {
    ($($name:ident),* $(,)?) => {
        actions!(flick, [Quit, $($name),*]);

        /// A menu bar command for the main window's view.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Command {
            $($name),*
        }

        fn register(cx: &mut App) {
            $(cx.on_action(|_: &$name, cx| run(cx, Command::$name));)*
        }
    };
}

commands!(
    OpenVideo,
    OpenUrl,
    AddVideos,
    AddFolder,
    PasteLink,
    Settings,
    PlayPause,
    SeekBack,
    SeekForward,
    Slower,
    Faster,
    NormalSpeed,
    PreviousChapter,
    NextChapter,
    PreviousVideo,
    NextVideo,
    Repeat,
    AbLoop,
    GoToTime,
    ShowQueue,
    Fullscreen,
    PictureInPicture,
    FloatOnTop,
    HalfSize,
    ActualSize,
    DoubleSize,
    FitToScreen,
    VideoAdjustments,
    Screenshot,
    CopyFrame,
    CopyText,
    MediaInfo,
    PreviousFrame,
    NextFrame,
    Mute,
    VolumeUp,
    VolumeDown,
    AudioEarlier,
    AudioLater,
    LoadAudio,
    Equalizer,
    AudioTracks,
    LoadSubtitles,
    FindSubtitles,
    SubtitlesEarlier,
    SubtitlesLater,
    SubtitleTracks,
    KeyboardShortcuts,
);

/// The view and window menu commands go to.
struct Target {
    root: WeakEntity<Flick>,
    window: AnyWindowHandle,
}

impl Global for Target {}

/// Registers the commands and, on macOS, shows the menu bar.
pub fn init(cx: &mut App) {
    register(cx);
    if cfg!(target_os = "macos") {
        cx.set_menus(menus());
    }
}

pub fn set_target(root: WeakEntity<Flick>, window: AnyWindowHandle, cx: &mut App) {
    cx.set_global(Target { root, window });
}

fn run(cx: &mut App, command: Command) {
    let Some(target) = cx.try_global::<Target>() else {
        return;
    };
    let (root, window) = (target.root.clone(), target.window);
    let _ = window.update(cx, |_, window, cx| {
        if let Some(root) = root.upgrade() {
            root.update(cx, |flick, cx| flick.menu_command(command, window, cx));
        }
    });
}

fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Flick".into(),
            items: vec![
                MenuItem::action("Settings…", Settings),
                MenuItem::separator(),
                MenuItem::action("Quit Flick", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Video…", OpenVideo),
                MenuItem::action("Open URL…", OpenUrl),
                MenuItem::separator(),
                MenuItem::action("Add Videos to Library…", AddVideos),
                MenuItem::action("Add Folder to Library…", AddFolder),
                MenuItem::action("Paste Link to Download", PasteLink),
            ],
        },
        Menu {
            name: "Playback".into(),
            items: vec![
                MenuItem::action("Play or Pause", PlayPause),
                MenuItem::action("Seek Back", SeekBack),
                MenuItem::action("Seek Forward", SeekForward),
                MenuItem::action("Go to Time…", GoToTime),
                MenuItem::separator(),
                MenuItem::action("Slower", Slower),
                MenuItem::action("Faster", Faster),
                MenuItem::action("Normal Speed", NormalSpeed),
                MenuItem::separator(),
                MenuItem::action("Previous Chapter", PreviousChapter),
                MenuItem::action("Next Chapter", NextChapter),
                MenuItem::action("Previous Video", PreviousVideo),
                MenuItem::action("Next Video", NextVideo),
                MenuItem::action("Queue and Chapters", ShowQueue),
                MenuItem::separator(),
                MenuItem::action("Repeat This Video", Repeat),
                MenuItem::action("A–B Loop", AbLoop),
            ],
        },
        Menu {
            name: "Video".into(),
            items: vec![
                MenuItem::action("Enter or Exit Fullscreen", Fullscreen),
                MenuItem::action("Picture in Picture", PictureInPicture),
                MenuItem::action("Float on Top", FloatOnTop),
                MenuItem::separator(),
                MenuItem::action("Half Size", HalfSize),
                MenuItem::action("Actual Size", ActualSize),
                MenuItem::action("Double Size", DoubleSize),
                MenuItem::action("Fit to Screen", FitToScreen),
                MenuItem::separator(),
                MenuItem::action("Video Adjustments", VideoAdjustments),
                MenuItem::action("Previous Frame", PreviousFrame),
                MenuItem::action("Next Frame", NextFrame),
                MenuItem::separator(),
                MenuItem::action("Save Screenshot", Screenshot),
                MenuItem::action("Copy Frame", CopyFrame),
                MenuItem::action("Copy Text in Frame", CopyText),
                MenuItem::action("Media Information", MediaInfo),
            ],
        },
        Menu {
            name: "Audio".into(),
            items: vec![
                MenuItem::action("Mute", Mute),
                MenuItem::action("Volume Up", VolumeUp),
                MenuItem::action("Volume Down", VolumeDown),
                MenuItem::separator(),
                MenuItem::action("Audio Earlier", AudioEarlier),
                MenuItem::action("Audio Later", AudioLater),
                MenuItem::separator(),
                MenuItem::action("Audio Tracks…", AudioTracks),
                MenuItem::action("Load Audio File…", LoadAudio),
                MenuItem::action("Equalizer…", Equalizer),
            ],
        },
        Menu {
            name: "Subtitles".into(),
            items: vec![
                MenuItem::action("Subtitle Tracks…", SubtitleTracks),
                MenuItem::action("Load Subtitle File…", LoadSubtitles),
                MenuItem::action("Find Subtitles Online…", FindSubtitles),
                MenuItem::separator(),
                MenuItem::action("Subtitles Earlier", SubtitlesEarlier),
                MenuItem::action("Subtitles Later", SubtitlesLater),
            ],
        },
        Menu {
            name: "Help".into(),
            items: vec![MenuItem::action("Keyboard Shortcuts", KeyboardShortcuts)],
        },
    ]
}
