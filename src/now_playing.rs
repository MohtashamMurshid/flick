//! Media keys and the system's Now Playing controls, as IINA has them: the
//! keyboard's play/pause and track keys, headphone buttons, and the title
//! and timeline in Control Center, the Windows media overlay, or a Linux
//! desktop's MPRIS widget.
//!
//! Each platform reports button presses from its own thread or callback
//! into a shared queue; the UI takes them on its next tick and updates what
//! the system shows when the video, its position, or its state changes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RemoteCommand {
    Play,
    Pause,
    TogglePlayPause,
    Next,
    Previous,
    /// Go to this many seconds.
    SeekTo(f64),
    /// Move by this many seconds, back when negative.
    SeekBy(f64),
}

/// What the system shows for the playing video.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub title: String,
    pub duration: f64,
    pub position: f64,
    pub speed: f64,
    pub playing: bool,
}

type Commands = Arc<Mutex<VecDeque<RemoteCommand>>>;

fn push(commands: &Commands, command: RemoteCommand) {
    if let Ok(mut queue) = commands.lock() {
        // A stuck UI must not grow this without bound.
        if queue.len() < 64 {
            queue.push_back(command);
        }
    }
}

pub struct NowPlaying {
    commands: Commands,
    session: Option<platform::Session>,
    /// What was last sent, and where its position was expected to be by now.
    sent: Option<(Info, std::time::Instant)>,
}

impl NowPlaying {
    /// Registers for media keys. Windows ties its controls to the window,
    /// given as its `HWND`. Without a platform session, commands never
    /// arrive and updates do nothing.
    pub fn new(window: Option<isize>) -> Self {
        let commands = Commands::default();
        let session = match platform::Session::new(commands.clone(), window) {
            Ok(session) => Some(session),
            Err(error) => {
                tracing::info!(%error, "Media keys and Now Playing are unavailable");
                None
            }
        };
        Self {
            commands,
            session,
            sent: None,
        }
    }

    /// Commands pressed since the last call, oldest first.
    pub fn take(&self) -> Vec<RemoteCommand> {
        self.commands
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Shows `info`, or clears Now Playing with None. Sends only changes:
    /// the system advances the position itself at the given speed, so a
    /// position is sent again only after a seek.
    pub fn update(&mut self, info: Option<Info>) {
        let Some(session) = &mut self.session else {
            return;
        };
        if !needs_update(self.sent.as_ref(), info.as_ref()) {
            return;
        }
        let result = match &info {
            Some(info) => session.show(info),
            None => session.clear(),
        };
        if let Err(error) = result {
            tracing::debug!(%error, "Could not update Now Playing");
        }
        self.sent = info.map(|info| (info, std::time::Instant::now()));
    }
}

/// Whether `next` differs from what was sent by more than the time since.
fn needs_update(sent: Option<&(Info, std::time::Instant)>, next: Option<&Info>) -> bool {
    match (sent, next) {
        (None, None) => false,
        (Some((sent, at)), Some(next)) => {
            let expected = if sent.playing {
                sent.position + at.elapsed().as_secs_f64() * sent.speed
            } else {
                sent.position
            };
            sent.title != next.title
                || sent.playing != next.playing
                || (sent.speed - next.speed).abs() > 0.001
                || (sent.duration - next.duration).abs() > 0.5
                || (expected - next.position).abs() > 1.5
        }
        _ => true,
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Commands, Info, RemoteCommand, push};
    use anyhow::{Context as _, Result};
    use block2::RcBlock;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel};
    use std::ffi::CString;

    #[link(name = "MediaPlayer", kind = "framework")]
    unsafe extern "C" {
        static MPMediaItemPropertyTitle: *mut AnyObject;
        static MPMediaItemPropertyPlaybackDuration: *mut AnyObject;
        static MPNowPlayingInfoPropertyElapsedPlaybackTime: *mut AnyObject;
        static MPNowPlayingInfoPropertyPlaybackRate: *mut AnyObject;
    }

    /// `MPNowPlayingPlaybackState`.
    const PLAYING: usize = 1;
    const PAUSED: usize = 2;
    const STOPPED: usize = 3;
    /// `MPRemoteCommandHandlerStatusSuccess`.
    const SUCCESS: isize = 0;

    type Handler = RcBlock<dyn Fn(*mut AnyObject) -> isize>;

    pub struct Session {
        info_center: Retained<AnyObject>,
        /// Kept for the app's lifetime with the targets they serve.
        _handlers: Vec<Handler>,
    }

    fn class(name: &std::ffi::CStr) -> Result<&'static AnyClass> {
        AnyClass::get(name).with_context(|| format!("{name:?} is missing"))
    }

    impl Session {
        pub fn new(commands: Commands, _window: Option<isize>) -> Result<Self> {
            let center_class = class(c"MPRemoteCommandCenter")?;
            let info_class = class(c"MPNowPlayingInfoCenter")?;
            let mut handlers = Vec::new();
            // SAFETY: documented MediaPlayer API on the main thread. Each
            // handler block is copied by addTargetWithHandler: and kept here.
            unsafe {
                let center: Retained<AnyObject> = msg_send![center_class, sharedCommandCenter];
                let info_center: Retained<AnyObject> = msg_send![info_class, defaultCenter];
                let mut add = |command: Retained<AnyObject>, handler: Handler| {
                    let _: Option<Retained<AnyObject>> =
                        msg_send![&*command, addTargetWithHandler: &*handler];
                    let _: () = msg_send![&*command, setEnabled: true];
                    handlers.push(handler);
                };
                let simple = |command: RemoteCommand| {
                    let commands = commands.clone();
                    RcBlock::new(move |_event: *mut AnyObject| {
                        push(&commands, command);
                        SUCCESS
                    })
                };
                add(
                    msg_send![&*center, playCommand],
                    simple(RemoteCommand::Play),
                );
                add(
                    msg_send![&*center, pauseCommand],
                    simple(RemoteCommand::Pause),
                );
                add(
                    msg_send![&*center, togglePlayPauseCommand],
                    simple(RemoteCommand::TogglePlayPause),
                );
                add(
                    msg_send![&*center, nextTrackCommand],
                    simple(RemoteCommand::Next),
                );
                add(
                    msg_send![&*center, previousTrackCommand],
                    simple(RemoteCommand::Previous),
                );
                let seek = commands.clone();
                add(
                    msg_send![&*center, changePlaybackPositionCommand],
                    RcBlock::new(move |event: *mut AnyObject| {
                        if !event.is_null() {
                            let time: f64 = msg_send![event, positionTime];
                            push(&seek, RemoteCommand::SeekTo(time));
                        }
                        SUCCESS
                    }),
                );
                for (selector, sign) in [
                    (sel!(skipForwardCommand), 1.),
                    (sel!(skipBackwardCommand), -1.),
                ] {
                    let skip = commands.clone();
                    let command: Retained<AnyObject> =
                        msg_send![&*center, performSelector: selector];
                    add(
                        command,
                        RcBlock::new(move |event: *mut AnyObject| {
                            let interval: f64 = if event.is_null() {
                                10.
                            } else {
                                msg_send![event, interval]
                            };
                            push(&skip, RemoteCommand::SeekBy(sign * interval));
                            SUCCESS
                        }),
                    );
                }
                Ok(Self {
                    info_center,
                    _handlers: handlers,
                })
            }
        }

        pub fn show(&mut self, info: &Info) -> Result<()> {
            let title = CString::new(info.title.replace('\0', ""))?;
            autoreleasepool(|_| {
                // SAFETY: Foundation objects built and handed to the info
                // center on the main thread; the keys are MediaPlayer's.
                unsafe {
                    let dictionary: Retained<AnyObject> =
                        msg_send![class(c"NSMutableDictionary")?, new];
                    let string: Option<Retained<AnyObject>> = msg_send![
                        class(c"NSString")?,
                        stringWithUTF8String: title.as_ptr()
                    ];
                    let number = |value: f64| -> Result<Retained<AnyObject>> {
                        Ok(msg_send![class(c"NSNumber")?, numberWithDouble: value])
                    };
                    if let Some(string) = string {
                        let _: () = msg_send![&*dictionary, setObject: &*string, forKey: MPMediaItemPropertyTitle];
                    }
                    for (key, value) in [
                        (MPMediaItemPropertyPlaybackDuration, info.duration.max(0.)),
                        (
                            MPNowPlayingInfoPropertyElapsedPlaybackTime,
                            info.position.max(0.),
                        ),
                        (
                            MPNowPlayingInfoPropertyPlaybackRate,
                            if info.playing { info.speed } else { 0. },
                        ),
                    ] {
                        let value = number(value)?;
                        let _: () = msg_send![&*dictionary, setObject: &*value, forKey: key];
                    }
                    let _: () = msg_send![&*self.info_center, setNowPlayingInfo: &*dictionary];
                    let state = if info.playing { PLAYING } else { PAUSED };
                    let _: () = msg_send![&*self.info_center, setPlaybackState: state];
                }
                Ok(())
            })
        }

        pub fn clear(&mut self) -> Result<()> {
            // SAFETY: clearing the info center's dictionary and state.
            unsafe {
                let none: *mut AnyObject = std::ptr::null_mut();
                let _: () = msg_send![&*self.info_center, setNowPlayingInfo: none];
                let _: () = msg_send![&*self.info_center, setPlaybackState: STOPPED];
            }
            Ok(())
        }

        /// What the info center holds, for tests.
        #[cfg(test)]
        pub fn shown(&self) -> (usize, usize) {
            // SAFETY: reads of the info center's own properties.
            unsafe {
                let info: Option<Retained<AnyObject>> =
                    msg_send![&*self.info_center, nowPlayingInfo];
                let count: usize = match &info {
                    Some(info) => msg_send![&**info, count],
                    None => 0,
                };
                let state: usize = msg_send![&*self.info_center, playbackState];
                (count, state)
            }
        }
    }
}

/// MPRIS on the session bus, which GNOME, KDE, and other desktops show in
/// their media controls and drive from media keys.
#[cfg(target_os = "linux")]
mod platform {
    use super::{Commands, Info, RemoteCommand, push};
    use anyhow::Result;
    use std::collections::HashMap;
    use zbus::blocking::connection;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    const PATH: &str = "/org/mpris/MediaPlayer2";
    const TRACK: &str = "/app/flick/Flick/track";

    struct Root;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        fn raise(&self) {}
        fn quit(&self) {}
        #[zbus(property)]
        fn can_quit(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn can_raise(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn has_track_list(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn identity(&self) -> &str {
            "Flick"
        }
        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "flick"
        }
        #[zbus(property)]
        fn supported_uri_schemes(&self) -> Vec<String> {
            Vec::new()
        }
        #[zbus(property)]
        fn supported_mime_types(&self) -> Vec<String> {
            Vec::new()
        }
    }

    struct Player {
        commands: Commands,
        info: Option<Info>,
    }

    /// Microseconds, as MPRIS counts time.
    fn micros(seconds: f64) -> i64 {
        (seconds.max(0.) * 1e6) as i64
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        fn next(&self) {
            push(&self.commands, RemoteCommand::Next);
        }
        fn previous(&self) {
            push(&self.commands, RemoteCommand::Previous);
        }
        fn pause(&self) {
            push(&self.commands, RemoteCommand::Pause);
        }
        fn play_pause(&self) {
            push(&self.commands, RemoteCommand::TogglePlayPause);
        }
        fn stop(&self) {
            push(&self.commands, RemoteCommand::Pause);
        }
        fn play(&self) {
            push(&self.commands, RemoteCommand::Play);
        }
        fn seek(&self, offset: i64) {
            push(&self.commands, RemoteCommand::SeekBy(offset as f64 / 1e6));
        }
        fn set_position(&self, _track: ObjectPath<'_>, position: i64) {
            push(&self.commands, RemoteCommand::SeekTo(position as f64 / 1e6));
        }
        fn open_uri(&self, _uri: &str) {}
        #[zbus(property)]
        fn playback_status(&self) -> &str {
            match &self.info {
                Some(info) if info.playing => "Playing",
                Some(_) => "Paused",
                None => "Stopped",
            }
        }
        #[zbus(property)]
        fn rate(&self) -> f64 {
            self.info.as_ref().map_or(1., |info| info.speed)
        }
        #[zbus(property)]
        fn set_rate(&mut self, _rate: f64) {}
        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            let mut metadata = HashMap::new();
            if let Some(info) = &self.info {
                let entries = [
                    (
                        "mpris:trackid",
                        Value::from(ObjectPath::from_static_str_unchecked(TRACK)),
                    ),
                    ("mpris:length", Value::from(micros(info.duration))),
                    ("xesam:title", Value::from(info.title.clone())),
                ];
                for (key, value) in entries {
                    if let Ok(value) = value.try_into() {
                        metadata.insert(key.to_owned(), value);
                    }
                }
            }
            metadata
        }
        #[zbus(property)]
        fn volume(&self) -> f64 {
            1.
        }
        #[zbus(property)]
        fn set_volume(&mut self, _volume: f64) {}
        #[zbus(property(emits_changed_signal = "false"))]
        fn position(&self) -> i64 {
            self.info.as_ref().map_or(0, |info| micros(info.position))
        }
        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            0.25
        }
        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            16.
        }
        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }
        #[zbus(signal)]
        async fn seeked(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            position: i64,
        ) -> zbus::Result<()>;
    }

    pub struct Session {
        connection: zbus::blocking::Connection,
    }

    impl Session {
        pub fn new(commands: Commands, _window: Option<isize>) -> Result<Self> {
            let build = |name: String| -> zbus::Result<zbus::blocking::Connection> {
                connection::Builder::session()?
                    .name(name)?
                    .serve_at(PATH, Root)?
                    .serve_at(
                        PATH,
                        Player {
                            commands: commands.clone(),
                            info: None,
                        },
                    )?
                    .build()
            };
            // A second Flick takes an instance name, as MPRIS allows.
            let connection = build("org.mpris.MediaPlayer2.flick".into()).or_else(|_| {
                build(format!(
                    "org.mpris.MediaPlayer2.flick.instance{}",
                    std::process::id()
                ))
            })?;
            Ok(Self { connection })
        }

        fn set(&mut self, info: Option<Info>) -> Result<()> {
            let player = self
                .connection
                .object_server()
                .interface::<_, Player>(PATH)?;
            let seeked = {
                let mut state = player.get_mut();
                let jumped = match (&state.info, &info) {
                    (Some(old), Some(new)) => (old.position - new.position).abs() > 1.5,
                    _ => false,
                };
                state.info = info.clone();
                jumped
            };
            let emitter = player.signal_emitter();
            let state = player.get();
            zbus::block_on(async {
                state.playback_status_changed(emitter).await?;
                state.metadata_changed(emitter).await?;
                state.rate_changed(emitter).await?;
                if seeked && let Some(info) = &info {
                    Player::seeked(emitter, micros(info.position)).await?;
                }
                zbus::Result::Ok(())
            })?;
            Ok(())
        }

        pub fn show(&mut self, info: &Info) -> Result<()> {
            self.set(Some(info.clone()))
        }

        pub fn clear(&mut self) -> Result<()> {
            self.set(None)
        }
    }
}

/// The System Media Transport Controls: the media keys and the overlay
/// Windows shows by the volume flyout and on the lock screen.
#[cfg(target_os = "windows")]
mod platform {
    use super::{Commands, Info, RemoteCommand, push};
    use anyhow::{Context as _, Result};
    use windows::Foundation::{TimeSpan, TypedEventHandler};
    use windows::Media::{
        MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
        SystemMediaTransportControls, SystemMediaTransportControlsButton,
        SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
    use windows::core::{HSTRING, Ref, factory};

    /// TimeSpan counts 100-nanosecond ticks.
    fn ticks(seconds: f64) -> TimeSpan {
        TimeSpan {
            Duration: (seconds.max(0.) * 1e7) as i64,
        }
    }

    pub struct Session {
        controls: SystemMediaTransportControls,
    }

    impl Session {
        pub fn new(commands: Commands, window: Option<isize>) -> Result<Self> {
            let window = window.context("No window for the media controls")?;
            let interop =
                factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
            // SAFETY: GPUI's live top-level window handle.
            let controls: SystemMediaTransportControls =
                unsafe { interop.GetForWindow(HWND(window as *mut _))? };
            controls.SetIsEnabled(true)?;
            controls.SetIsPlayEnabled(true)?;
            controls.SetIsPauseEnabled(true)?;
            controls.SetIsNextEnabled(true)?;
            controls.SetIsPreviousEnabled(true)?;
            controls.SetIsFastForwardEnabled(true)?;
            controls.SetIsRewindEnabled(true)?;
            let buttons = commands.clone();
            controls.ButtonPressed(&TypedEventHandler::new(
                move |_, args: Ref<SystemMediaTransportControlsButtonPressedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        let command = match args.Button()? {
                            SystemMediaTransportControlsButton::Play => RemoteCommand::Play,
                            SystemMediaTransportControlsButton::Pause
                            | SystemMediaTransportControlsButton::Stop => RemoteCommand::Pause,
                            SystemMediaTransportControlsButton::Next => RemoteCommand::Next,
                            SystemMediaTransportControlsButton::Previous => RemoteCommand::Previous,
                            SystemMediaTransportControlsButton::FastForward => {
                                RemoteCommand::SeekBy(10.)
                            }
                            SystemMediaTransportControlsButton::Rewind => {
                                RemoteCommand::SeekBy(-10.)
                            }
                            _ => return Ok(()),
                        };
                        push(&buttons, command);
                    }
                    Ok(())
                },
            ))?;
            controls.PlaybackPositionChangeRequested(&TypedEventHandler::new(
                move |_, args: Ref<PlaybackPositionChangeRequestedEventArgs>| {
                    if let Some(args) = args.as_ref() {
                        let position = args.RequestedPlaybackPosition()?.Duration as f64 / 1e7;
                        push(&commands, RemoteCommand::SeekTo(position));
                    }
                    Ok(())
                },
            ))?;
            Ok(Self { controls })
        }

        pub fn show(&mut self, info: &Info) -> Result<()> {
            let updater = self.controls.DisplayUpdater()?;
            updater.SetType(MediaPlaybackType::Video)?;
            updater
                .VideoProperties()?
                .SetTitle(&HSTRING::from(info.title.as_str()))?;
            updater.Update()?;
            self.controls.SetPlaybackStatus(if info.playing {
                MediaPlaybackStatus::Playing
            } else {
                MediaPlaybackStatus::Paused
            })?;
            let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
            timeline.SetStartTime(ticks(0.))?;
            timeline.SetMinSeekTime(ticks(0.))?;
            timeline.SetEndTime(ticks(info.duration))?;
            timeline.SetMaxSeekTime(ticks(info.duration))?;
            timeline.SetPosition(ticks(info.position))?;
            self.controls.UpdateTimelineProperties(&timeline)?;
            Ok(())
        }

        pub fn clear(&mut self) -> Result<()> {
            self.controls.DisplayUpdater()?.ClearAll()?;
            self.controls
                .SetPlaybackStatus(MediaPlaybackStatus::Stopped)?;
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::{Commands, Info};
    use anyhow::Result;

    pub struct Session;

    impl Session {
        pub fn new(_commands: Commands, _window: Option<isize>) -> Result<Self> {
            anyhow::bail!("not supported on this platform")
        }

        pub fn show(&mut self, _info: &Info) -> Result<()> {
            Ok(())
        }

        pub fn clear(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn info(position: f64, playing: bool) -> Info {
        Info {
            title: "Sintel".into(),
            duration: 888.,
            position,
            speed: 1.,
            playing,
        }
    }

    #[test]
    fn only_real_changes_are_sent() {
        let at = Instant::now() - Duration::from_secs(10);
        let sent = (info(100., true), at);
        // Ten seconds of playing later, 110 is where the system shows it.
        assert!(!needs_update(Some(&sent), Some(&info(110., true))));
        assert!(needs_update(Some(&sent), Some(&info(300., true))));
        assert!(needs_update(Some(&sent), Some(&info(110., false))));
        let paused = (info(100., false), at);
        assert!(!needs_update(Some(&paused), Some(&info(100.5, false))));
        assert!(needs_update(Some(&paused), None));
        assert!(needs_update(None, Some(&info(0., true))));
        assert!(!needs_update(None, None));
    }

    #[test]
    fn commands_queue_in_order_and_stay_bounded() {
        let commands = Commands::default();
        for _ in 0..100 {
            push(&commands, RemoteCommand::Next);
        }
        push(&commands, RemoteCommand::SeekTo(5.));
        let queue = commands.lock().unwrap();
        assert_eq!(queue.len(), 64);
        assert_eq!(queue[0], RemoteCommand::Next);
    }

    /// Registers the handlers and round-trips Now Playing through
    /// MediaPlayer.framework in this process.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_now_playing_holds_what_flick_sends() {
        let mut session = platform::Session::new(Commands::default(), None).unwrap();
        session.show(&info(12.5, true)).unwrap();
        assert_eq!(session.shown(), (4, 1));
        session.show(&info(12.5, false)).unwrap();
        assert_eq!(session.shown().1, 2);
        session.clear().unwrap();
        assert_eq!(session.shown(), (0, 3));
    }
}
