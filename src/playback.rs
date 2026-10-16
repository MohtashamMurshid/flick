//! The libmpv boundary and a portable software presentation baseline.
//!
//! libmpv is loaded at runtime, so building Flick does not need native mpv
//! development headers or a link-time library. The renderer intentionally uses
//! software decoding, scaling, and subtitle composition. GPUI uploads the final
//! BGRA image; this path is functional scaffolding, not a zero-copy GPU path.
//!
//! Keep this object on the UI thread. Advanced render control is deliberately
//! disabled: synchronous client calls and rendering on one thread can otherwise
//! deadlock. Even without advanced control, upstream recommends separating the
//! client and renderer threads to avoid stalls. That is part of the later GPU
//! integration work, rather than a performance claim for this baseline.

use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, OsString, c_char, c_int, c_ulong, c_void};
use std::marker::PhantomData;
#[cfg(any(test, target_os = "macos"))]
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct PlaybackSnapshot {
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub tracks: Vec<Track>,
    pub eof: bool,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            paused: false,
            position: 0.0,
            duration: 0.0,
            volume: 100.0,
            muted: false,
            speed: 1.0,
            tracks: Vec::new(),
            eof: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub id: i64,
    /// libmpv track types are `video`, `audio`, and `sub`.
    pub kind: String,
    pub title: String,
    pub language: String,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    FileLoaded,
    EndFile,
    Error(String),
}

/// A top-to-bottom image with tightly packed B, G, R, A bytes in that order.
/// Alpha is always 255. There is no row padding: stride is `width * 4` bytes.
/// This is GPUI RenderImage's BGRA8 format, not the image crate's usual RGBA8.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

// These C declarations follow libmpv/client.h and libmpv/render.h. Function
// pointers are copied from symbols, but Api retains the library until every
// native handle has been destroyed. The only native callback sets an atomic
// redraw flag; it never calls libmpv or accesses UI state.
type Create = unsafe extern "C" fn() -> *mut c_void;
type ApiVersion = unsafe extern "C" fn() -> c_ulong;
type Initialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type SetString = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type Command = unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int;
type GetString = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type Free = unsafe extern "C" fn(*mut c_void);
type WaitEvent = unsafe extern "C" fn(*mut c_void, f64) -> *const MpvEvent;
type ErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type RenderCreate = unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *mut RenderParam) -> c_int;
type Render = unsafe extern "C" fn(*mut c_void, *mut RenderParam) -> c_int;
type RenderUpdate = unsafe extern "C" fn(*mut c_void) -> u64;
type RenderCallback = unsafe extern "C" fn(*mut c_void);
type SetRenderCallback = unsafe extern "C" fn(*mut c_void, Option<RenderCallback>, *mut c_void);

unsafe extern "C" fn request_redraw(context: *mut c_void) {
    // SAFETY: callback storage is boxed before registration, stays at a stable
    // address, and is retained until the callback is unregistered and the
    // renderer is destroyed. Only atomic operations run on native threads.
    let requested = unsafe { &*context.cast::<AtomicBool>() };
    requested.store(true, Ordering::Release);
}

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

// API >= 1.108 guarantees the playlist entry IDs in start/end events.
#[repr(C)]
struct EndFilePrefix {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
}

#[repr(C)]
struct StartFile {
    playlist_entry_id: i64,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

impl RenderParam {
    fn end() -> Self {
        Self {
            kind: 0,
            data: ptr::null_mut(),
        }
    }
}

struct Api {
    create: Create,
    initialize: Initialize,
    set_option: SetString,
    command: Command,
    set_property: SetString,
    get_property: GetString,
    free: Free,
    wait_event: WaitEvent,
    terminate_destroy: Free,
    error_string: ErrorString,
    render_create: RenderCreate,
    render: Render,
    render_update: RenderUpdate,
    set_render_callback: SetRenderCallback,
    render_free: Free,
    _library: Library,
}

impl Api {
    fn load() -> Result<&'static Self> {
        // Native media libraries and their transitive dependencies can retain
        // process-wide registrations or TLS destructors after a player shuts
        // down. Keep one library mapping for the process lifetime rather than
        // dlclosing code that a native shutdown callback could still reference.
        // Player cores, render contexts, and frame buffers remain per-instance
        // resources and are fully released by Player::drop.
        static API: OnceLock<Api> = OnceLock::new();
        if let Some(api) = API.get() {
            return Ok(api);
        }
        let candidate = Self::open()?;
        // A concurrent initializer may win; its mapping then keeps libmpv
        // resident while the losing candidate releases its extra reference.
        let _ = API.set(candidate);
        API.get().context("Could not retain the libmpv library")
    }

    fn open() -> Result<Self> {
        let candidates = library_candidates();
        let mut failures = Vec::new();
        for candidate in candidates {
            // SAFETY: loading a native library executes trusted native code.
            // The override is explicitly user-controlled; automatic candidates
            // name the platform's mpv library. Symbols are checked below.
            let library = match unsafe { Library::new(&candidate) } {
                Ok(library) => library,
                Err(error) => {
                    failures.push(format!("{}: {error}", candidate.to_string_lossy()));
                    continue;
                }
            };
            return Self::from_library(library).context(
                "The loaded libmpv is missing required client/render APIs; install a current libmpv build",
            );
        }
        bail!(
            "Could not load libmpv. Install libmpv (Linux: libmpv2; macOS: brew install mpv; Windows: an mpv shared-library build), or set FLICK_MPV_LIBRARY to its full path.\n{}",
            failures.join("\n")
        )
    }

    fn from_library(library: Library) -> Result<Self> {
        // SAFETY: each symbol name is part of the public libmpv C ABI and its
        // signature above matches that ABI. The returned Api owns the library.
        unsafe {
            let version = library.get::<ApiVersion>(b"mpv_client_api_version\0")?();
            let major = version >> 16;
            let minor = version & 0xffff;
            ensure!(
                major == 2 || (major == 1 && minor >= 108),
                "Unsupported libmpv client API {major}.{minor}; Flick requires API 1.108 or 2.x"
            );
            Ok(Self {
                create: *library.get(b"mpv_create\0")?,
                initialize: *library.get(b"mpv_initialize\0")?,
                set_option: *library.get(b"mpv_set_option_string\0")?,
                command: *library.get(b"mpv_command\0")?,
                set_property: *library.get(b"mpv_set_property_string\0")?,
                get_property: *library.get(b"mpv_get_property_string\0")?,
                free: *library.get(b"mpv_free\0")?,
                wait_event: *library.get(b"mpv_wait_event\0")?,
                terminate_destroy: *library.get(b"mpv_terminate_destroy\0")?,
                error_string: *library.get(b"mpv_error_string\0")?,
                render_create: *library.get(b"mpv_render_context_create\0")?,
                render: *library.get(b"mpv_render_context_render\0")?,
                render_update: *library.get(b"mpv_render_context_update\0")?,
                set_render_callback: *library.get(b"mpv_render_context_set_update_callback\0")?,
                render_free: *library.get(b"mpv_render_context_free\0")?,
                _library: library,
            })
        }
    }

    fn check(&self, code: c_int, action: &str) -> Result<()> {
        if code < 0 {
            bail!("{action}: {} (libmpv {code})", self.error(code));
        }
        Ok(())
    }

    fn error(&self, code: c_int) -> String {
        // SAFETY: mpv_error_string returns a static NUL-terminated string.
        let message = unsafe { (self.error_string)(code) };
        if message.is_null() {
            return format!("Unknown libmpv error {code}");
        }
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

fn library_candidates() -> Vec<OsString> {
    if let Some(path) = std::env::var_os("FLICK_MPV_LIBRARY") {
        return vec![path];
    }
    #[cfg(target_os = "linux")]
    let names = ["libmpv.so.2", "libmpv.so.1", "libmpv.so"];
    #[cfg(target_os = "macos")]
    let names = ["libmpv.2.dylib", "libmpv.1.dylib", "libmpv.dylib"];
    #[cfg(target_os = "windows")]
    let names = ["mpv-2.dll", "libmpv-2.dll", "mpv-1.dll"];
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let names = ["libmpv.so"];

    let mut candidates: Vec<OsString> = names.iter().map(OsString::from).collect();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        for name in names {
            candidates.push(directory.join(name).into_os_string());
            #[cfg(target_os = "macos")]
            candidates.push(directory.join("../Frameworks").join(name).into_os_string());
        }
    }
    #[cfg(target_os = "macos")]
    for directory in ["/opt/homebrew/lib", "/usr/local/lib"] {
        for name in names {
            candidates.push(PathBuf::from(directory).join(name).into_os_string());
        }
    }
    candidates
}

// mpv requires at least four-byte alignment for BGRA; 64-byte alignment for
// both pointer and stride also avoids unnecessary conversion fallback copies.
#[derive(Clone)]
#[repr(C, align(64))]
struct RenderBlock([u8; 64]);

#[derive(Default)]
struct RenderBuffer {
    width: u32,
    height: u32,
    stride: usize,
    blocks: Vec<RenderBlock>,
}

/// A single-thread-bound libmpv instance. It is intentionally neither Send nor
/// Sync; callers do not need to uphold any hidden native threading contract.
pub struct Player {
    handle: NonNull<c_void>,
    renderer: *mut c_void,
    buffer: RefCell<RenderBuffer>,
    requested_entry: Cell<Option<i64>>,
    event_entry: Cell<Option<i64>>,
    file_loaded: Cell<bool>,
    redraw_requested: Box<AtomicBool>,
    api: &'static Api,
    _main_thread: PhantomData<Rc<()>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let api = Api::load()?;
        // SAFETY: the checked function pointer belongs to the loaded library.
        let handle = NonNull::new(unsafe { (api.create)() })
            .context("libmpv could not create a playback instance")?;
        let mut player = Self {
            handle,
            renderer: ptr::null_mut(),
            buffer: RefCell::new(RenderBuffer::default()),
            requested_entry: Cell::new(None),
            event_entry: Cell::new(None),
            file_loaded: Cell::new(false),
            redraw_requested: Box::new(AtomicBool::new(false)),
            api,
            _main_thread: PhantomData,
        };
        // Any failure after creating the handle drops player and releases it.
        for (name, value) in [
            ("config", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("vo", "libmpv"),
            ("hwdec", "no"),
            ("vd-lavc-dr", "no"),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("osd-level", "0"),
            ("video-timing-offset", "0"),
        ] {
            player.option(name, value)?;
        }
        // Normal builds use the platform audio device; smoke tests may use the
        // timed null audio device without changing production defaults.
        if std::env::var("FLICK_AUDIO").as_deref() == Ok("null") {
            player.option("ao", "null")?;
        }
        // SAFETY: the handle is live, and all initialization options are set.
        player.api.check(
            unsafe { (player.api.initialize)(player.handle.as_ptr()) },
            "Initialize playback",
        )?;
        let mut parameters = [
            RenderParam {
                kind: 1, // MPV_RENDER_PARAM_API_TYPE: char*
                data: c"sw".as_ptr().cast_mut().cast(),
            },
            RenderParam::end(),
        ];
        // SAFETY: output pointer and terminated params are valid for this call.
        let code = unsafe {
            (player.api.render_create)(
                &mut player.renderer,
                player.handle.as_ptr(),
                parameters.as_mut_ptr(),
            )
        };
        player.api.check(code, "Create software video renderer")?;
        ensure!(
            !player.renderer.is_null(),
            "libmpv returned no video renderer"
        );
        // SAFETY: the boxed atomic has a stable address across Player moves.
        // Drop unregisters this callback before releasing its storage.
        unsafe {
            (player.api.set_render_callback)(
                player.renderer,
                Some(request_redraw),
                (&*player.redraw_requested as *const AtomicBool)
                    .cast_mut()
                    .cast(),
            );
        }
        Ok(player)
    }

    fn option(&self, name: &str, value: &str) -> Result<()> {
        let action = format!("Configure {name}={value}");
        let name = CString::new(name).context("Option name contains a NUL byte")?;
        let value = CString::new(value).context("Option value contains a NUL byte")?;
        // SAFETY: both C strings live through the call and the handle is live.
        self.api.check(
            unsafe { (self.api.set_option)(self.handle.as_ptr(), name.as_ptr(), value.as_ptr()) },
            &action,
        )
    }

    /// Run a native argument array, never a shell command or an mpv command
    /// string. Paths containing spaces therefore stay one argument.
    pub fn command(&self, args: &[&str]) -> Result<()> {
        ensure!(!args.is_empty(), "Playback command needs a name");
        let strings: Vec<CString> = args
            .iter()
            .map(|arg| CString::new(*arg).context("Playback argument contains a NUL byte"))
            .collect::<Result<_>>()?;
        let mut pointers: Vec<*const c_char> = strings.iter().map(|arg| arg.as_ptr()).collect();
        pointers.push(ptr::null());
        // SAFETY: all strings and the NUL-terminated pointer array outlive the
        // synchronous call. libmpv copies arguments it needs to retain.
        self.api.check(
            unsafe { (self.api.command)(self.handle.as_ptr(), pointers.as_ptr()) },
            args[0],
        )
    }

    pub fn load(&self, path: &str) -> Result<()> {
        // Forget already queued events for the previous file. IDs below also
        // filter events emitted concurrently while the new command executes.
        self.poll_events();
        self.command(&["loadfile", path, "replace"])?;
        let entry = self
            .property("playlist/0/id")
            .and_then(|value| value.parse().ok())
            .context("libmpv did not identify the requested playlist entry")?;
        self.requested_entry.set(Some(entry));
        self.file_loaded.set(false);
        // A previous file may have been paused or held at EOF.
        self.set_property("pause", "no")
    }

    pub fn set_property(&self, name: &str, value: &str) -> Result<()> {
        let property = CString::new(name).context("Property name contains a NUL byte")?;
        let value = CString::new(value).context("Property value contains a NUL byte")?;
        // SAFETY: C strings and initialized native handle live through the call.
        self.api.check(
            unsafe {
                (self.api.set_property)(self.handle.as_ptr(), property.as_ptr(), value.as_ptr())
            },
            &format!("Set {name}"),
        )
    }

    fn property(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // SAFETY: native return value is either NULL or an allocated C string.
        let value = unsafe { (self.api.get_property)(self.handle.as_ptr(), name.as_ptr()) };
        if value.is_null() {
            return None;
        }
        let owned = unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: mpv_free owns this allocation; no reference escapes above.
        unsafe { (self.api.free)(value.cast()) };
        Some(owned)
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        let number = |name, fallback| {
            self.property(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite())
                .unwrap_or(fallback)
        };
        let flag = |name| self.property(name).as_deref() == Some("yes");
        let tracks = self
            .property("track-list")
            .and_then(|json| serde_json::from_str::<Vec<Value>>(&json).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|track| Track {
                id: track["id"].as_i64().unwrap_or_default(),
                kind: track["type"].as_str().unwrap_or_default().to_owned(),
                title: track["title"].as_str().unwrap_or_default().to_owned(),
                language: track["lang"].as_str().unwrap_or_default().to_owned(),
                selected: track["selected"].as_bool().unwrap_or(false),
            })
            .collect();
        PlaybackSnapshot {
            paused: flag("pause"),
            position: number("time-pos", 0.0).max(0.0),
            duration: number("duration", 0.0).max(0.0),
            volume: number("volume", 100.0),
            muted: flag("mute"),
            speed: number("speed", 1.0),
            tracks,
            eof: flag("eof-reached"),
        }
    }

    /// Drain queued events with a zero timeout; never wait for the next event.
    pub fn poll_events(&self) -> Vec<PlayerEvent> {
        let mut events = Vec::new();
        // Bound work per UI tick, even if a malfunctioning input floods events.
        for _ in 0..256 {
            // SAFETY: the pointer is valid until the next wait_event call. All
            // event data is copied before the loop advances.
            let event = unsafe { (self.api.wait_event)(self.handle.as_ptr(), 0.0) };
            let Some(event) = (unsafe { event.as_ref() }) else {
                break;
            };
            match event.event_id {
                0 => break, // MPV_EVENT_NONE
                6 => {
                    // START_FILE identifies subsequent FILE_LOADED events,
                    // which themselves carry no file or request identifier.
                    if let Some(start) = unsafe { event.data.cast::<StartFile>().as_ref() } {
                        self.event_entry.set(Some(start.playlist_entry_id));
                    }
                }
                7 => {
                    // MPV_EVENT_END_FILE, with a stable reason/error prefix.
                    if let Some(end) = unsafe { event.data.cast::<EndFilePrefix>().as_ref() } {
                        if self.requested_entry.get().is_some()
                            && self.requested_entry.get() != Some(end.playlist_entry_id)
                        {
                            continue;
                        }
                        self.file_loaded.set(false);
                        if end.reason == 4 {
                            // MPV_END_FILE_REASON_ERROR
                            events.push(PlayerEvent::Error(format!(
                                "Playback failed: {}",
                                self.api.error(end.error)
                            )));
                        }
                    }
                    events.push(PlayerEvent::EndFile);
                }
                8 if self.event_entry.get() == self.requested_entry.get() => {
                    self.file_loaded.set(true);
                    events.push(PlayerEvent::FileLoaded);
                }
                24 => events.push(PlayerEvent::Error(
                    "Playback event queue overflowed; some updates were lost".into(),
                )),
                _ if event.error < 0 => {
                    events.push(PlayerEvent::Error(self.api.error(event.error)));
                }
                _ => {}
            }
        }
        events
    }

    /// Render only when a new frame or changed target size needs presentation.
    /// libmpv performs aspect-preserving scaling and letterboxing itself.
    pub fn render(&self, width: u32, height: u32) -> Result<Option<VideoFrame>> {
        if width == 0 || height == 0 || !self.file_loaded.get() {
            return Ok(None);
        }
        ensure!(
            width <= 8192 && height <= 8192,
            "Video presentation target exceeds 8192 pixels on one side"
        );
        let mut buffer = self.buffer.borrow_mut();
        let resized = buffer.width != width || buffer.height != height;
        // SAFETY: this method is only called on the Player's single thread.
        let updates = unsafe { (self.api.render_update)(self.renderer) };
        // Keep callback notifications even when libmpv's 200ms presentation
        // timeout has already promoted next_frame to cur_frame. In that case
        // update() returns no pending frame, but cur_frame still needs drawing.
        // Relying only on update() freezes paused seek previews at the old image.
        let requested = self.redraw_requested.swap(false, Ordering::AcqRel);
        if updates & 1 == 0 && !resized && !requested {
            return Ok(None);
        }
        if resized {
            let stride = (width as usize * 4).next_multiple_of(64);
            let blocks = stride * height as usize / 64;
            buffer.blocks.clear();
            buffer
                .blocks
                .try_reserve_exact(blocks)
                .context("Allocate video presentation surface")?;
            buffer.blocks.resize(blocks, RenderBlock([0; 64]));
            buffer.width = width;
            buffer.height = height;
            buffer.stride = stride;
        }
        let mut dimensions = [width as c_int, height as c_int];
        let mut stride = buffer.stride; // MPV_RENDER_PARAM_SW_STRIDE is size_t*.
        let mut block_for_time: c_int = 0;
        let mut params = [
            RenderParam {
                kind: 17, // SW_SIZE: int[2]
                data: dimensions.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: 18, // SW_FORMAT: char*
                data: c"bgra".as_ptr().cast_mut().cast(),
            },
            RenderParam {
                kind: 19, // SW_STRIDE: size_t*
                data: (&mut stride as *mut usize).cast(),
            },
            RenderParam {
                kind: 20, // SW_POINTER: pointer to writable aligned pixels.
                data: buffer.blocks.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: 12, // BLOCK_FOR_TARGET_TIME: int*, don't sleep on UI.
                data: (&mut block_for_time as *mut c_int).cast(),
            },
            RenderParam::end(),
        ];
        // SAFETY: all params outlive this call. The block vector has 64-byte
        // alignment and stride * height writable bytes, including row padding.
        self.api.check(
            unsafe { (self.api.render)(self.renderer, params.as_mut_ptr()) },
            "Render video frame",
        )?;
        let byte_count = buffer.stride * height as usize;
        // SAFETY: RenderBlock has no padding, contains initialized u8 values,
        // and the contiguous vector covers exactly byte_count bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.blocks.as_ptr().cast::<u8>(), byte_count) };
        let row_bytes = width as usize * 4;
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for row in bytes.chunks_exact(buffer.stride) {
            pixels.extend_from_slice(&row[..row_bytes]);
        }
        // Every video frame is opaque, including black letterbox bars. Force
        // alpha rather than exposing decoder/format-specific alpha semantics.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Ok(Some(VideoFrame {
            width,
            height,
            pixels,
        }))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: the renderer must be freed before the core; the library is
        // still owned by self.api and outlives both operations, even when
        // initialization only partially succeeded. No callbacks retain self.
        unsafe {
            if !self.renderer.is_null() {
                (self.api.set_render_callback)(self.renderer, None, ptr::null_mut());
                (self.api.render_free)(self.renderer);
            }
            (self.api.terminate_destroy)(self.handle.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn wait_for(player: &Player, mut condition: impl FnMut(&Player) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            for event in player.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!("{error}");
                }
            }
            player.render(323, 183)?;
            if condition(player) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Native playback condition timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct SubtitleFixture(PathBuf);

    impl SubtitleFixture {
        fn new() -> Result<Self> {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir()
                .join(format!("flick-playback-{}-{stamp}.srt", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(b"1\n00:00:00,000 --> 00:00:03,000\nFlick subtitle integration test\n")?;
            Ok(Self(path))
        }
    }

    impl Drop for SubtitleFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A timeline update alone does not prove that paused seeking displayed
    /// the target frame. This exercises the UI's deliberately slower paused
    /// polling with the generated moving test pattern and compares image bytes.
    #[test]
    #[ignore = "requires native libmpv and a moving-pattern FLICK_TEST_VIDEO"]
    fn real_paused_seek_updates_frame_content() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")?;
        let player = Player::new()?;
        player.load(&clip)?;
        let mut last_pixels = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            for event in player.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!("{error}");
                }
            }
            if let Some(frame) = player.render(640, 360)? {
                last_pixels = frame.pixels;
            }
            let state = player.snapshot();
            if state.position >= 0.2 && !last_pixels.is_empty() {
                break;
            }
            ensure!(Instant::now() < deadline, "No initial test image");
            thread::sleep(Duration::from_millis(16));
        }
        player.set_property("pause", "yes")?;
        // Consume any last playback/pause redraws, so a change after seeking
        // cannot be mistaken for a frame already pending before the seek.
        for _ in 0..4 {
            player.poll_events();
            if let Some(frame) = player.render(640, 360)? {
                last_pixels = frame.pixels;
            }
            thread::sleep(Duration::from_millis(150));
        }
        let before = last_pixels;
        let target = player.snapshot().duration * 0.75;
        ensure!(target > 1.0, "Use a test clip at least 3 seconds long");
        player.command(&["seek", &target.to_string(), "absolute+exact"])?;
        // Model a temporarily busy UI thread. libmpv's 200ms flip timeout may
        // move the decoded target into cur_frame before the next render poll.
        // The callback notification must survive that transition.
        thread::sleep(Duration::from_millis(600));
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut changed_image = false;
        loop {
            player.poll_events();
            if let Some(frame) = player.render(640, 360)? {
                assert_eq!(frame.pixels.len(), before.len());
                changed_image |= frame.pixels != before;
            }
            let state = player.snapshot();
            assert!(state.paused, "Seeking must keep the user paused");
            if changed_image && (state.position - target).abs() < 0.15 {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Paused seek updated the timeline but never displayed a changed video image"
            );
            thread::sleep(Duration::from_millis(150));
        }
        Ok(())
    }

    /// Covers native event correlation, repeated core/render destruction,
    /// external subtitles, audio selection, and replay after keep-open EOF.
    #[test]
    #[ignore = "requires native libmpv and >=3s H.264/AAC FLICK_TEST_VIDEO"]
    fn real_lifecycle_replacement_subtitles_and_eof() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")?;
        ensure!(PathBuf::from(&clip).is_file(), "Test clip does not exist");
        let subtitle = SubtitleFixture::new()?;
        for _ in 0..4 {
            let player = Player::new()?;
            for _ in 0..3 {
                player.load(&clip)?;
                // Allow the old file's native FILE_LOADED event to queue, then
                // replace it before the application consumes that event.
                thread::sleep(Duration::from_millis(60));
                let old_entry = player.requested_entry.get();
                player.load(&clip)?;
                assert_ne!(player.requested_entry.get(), old_entry);
                assert!(player.render(323, 183)?.is_none());
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut loaded_events = 0;
                loop {
                    for event in player.poll_events() {
                        match event {
                            PlayerEvent::FileLoaded => {
                                loaded_events += 1;
                                assert!(player.snapshot().duration > 0.0);
                                // This failed with -12 when FILE_LOADED from
                                // the replaced file consumed the resume seek.
                                player.command(&["seek", "0.5", "absolute+exact"])?;
                            }
                            PlayerEvent::Error(error) => bail!("{error}"),
                            PlayerEvent::EndFile => bail!("Replaced file's end event leaked"),
                        }
                    }
                    if player.render(323, 183)?.is_some() && player.snapshot().position >= 0.45 {
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "Replacement did not render and resume"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(loaded_events, 1, "Only the requested file should load");
            }
            let audio = player
                .snapshot()
                .tracks
                .into_iter()
                .find(|track| track.kind == "audio")
                .context("Test clip must have an audio track")?;
            player.set_property("aid", &audio.id.to_string())?;
            wait_for(&player, |player| {
                player
                    .snapshot()
                    .tracks
                    .iter()
                    .any(|track| track.kind == "audio" && track.id == audio.id && track.selected)
            })?;
            player.command(&[
                "sub-add",
                subtitle
                    .0
                    .to_str()
                    .context("Temporary subtitle path must be UTF-8")?,
                "select",
            ])?;
            wait_for(&player, |player| {
                player
                    .snapshot()
                    .tracks
                    .iter()
                    .any(|track| track.kind == "sub" && track.selected)
            })?;
            player.set_property("sid", "no")?;
            assert!(
                !player
                    .snapshot()
                    .tracks
                    .iter()
                    .any(|track| track.kind == "sub" && track.selected)
            );

            let duration = player.snapshot().duration;
            player.set_property("speed", "4")?;
            player.command(&["seek", &(duration - 0.3).to_string(), "absolute+exact"])?;
            player.set_property("pause", "no")?;
            wait_for(&player, |player| player.snapshot().eof)?;
            assert!(
                player.snapshot().duration > 0.0,
                "keep-open should retain the loaded file at EOF"
            );
            player.command(&["seek", "0", "absolute+exact"])?;
            player.set_property("pause", "no")?;
            wait_for(&player, |player| {
                let state = player.snapshot();
                !state.eof && !state.paused && state.position < duration * 0.5
            })?;
            // Drop while playing to exercise native render/core shutdown.
            drop(player);
        }
        Ok(())
    }

    /// Requires an installed libmpv and a generated >= 3s local video clip.
    /// Run with FLICK_AUDIO=null FLICK_TEST_VIDEO=/path/clip.mp4 cargo test
    /// playback::tests::real_playback -- --ignored --nocapture
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_playback() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")
            .context("Set FLICK_TEST_VIDEO to a generated test clip")?;
        ensure!(PathBuf::from(&clip).is_file(), "Test clip does not exist");
        let player = Player::new()?;
        assert!(player.command(&["flick-invalid-command"]).is_err());
        assert!(player.command(&[]).is_err());
        assert!(player.set_property("pause", "yes\0no").is_err());
        player.load(&clip)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut loaded = false;
        let mut rendered = false;
        loop {
            for event in player.poll_events() {
                match event {
                    PlayerEvent::FileLoaded => loaded = true,
                    PlayerEvent::Error(error) => bail!("{error}"),
                    PlayerEvent::EndFile => {}
                }
            }
            if let Some(frame) = player.render(321, 181)? {
                assert_eq!((frame.width, frame.height), (321, 181));
                assert_eq!(frame.pixels.len(), 321 * 181 * 4);
                assert!(
                    frame
                        .pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| pixel[3] == 255)
                );
                // A generated test pattern must render actual image content,
                // rather than merely yielding a correctly sized black buffer.
                rendered |= frame
                    .pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[..3].iter().any(|channel| *channel > 32));
            }
            let snapshot = player.snapshot();
            if loaded && rendered && snapshot.position > 0.1 {
                assert!(snapshot.duration > 0.5);
                assert!(snapshot.tracks.iter().any(|track| track.kind == "video"));
                break;
            }
            ensure!(Instant::now() < deadline, "Timed out waiting for playback");
            thread::sleep(Duration::from_millis(10));
        }
        player.set_property("pause", "yes")?;
        assert!(player.snapshot().paused);
        player.set_property("volume", "37")?;
        player.set_property("speed", "1.5")?;
        let snapshot = player.snapshot();
        assert!((snapshot.volume - 37.0).abs() < 0.1);
        assert!((snapshot.speed - 1.5).abs() < 0.01);
        player.command(&["seek", "0.5", "absolute+exact"])?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            player.poll_events();
            player.render(320, 180)?;
            if (player.snapshot().position - 0.5).abs() < 0.15 {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Seek did not reach the requested position"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let missing = format!("{clip}.flick-missing");
        ensure!(
            !PathBuf::from(&missing).exists(),
            "Missing-file fixture unexpectedly exists"
        );
        player.load(&missing)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reported_error = false;
        let mut ended = false;
        while !reported_error || !ended {
            for event in player.poll_events() {
                match event {
                    PlayerEvent::Error(_) => reported_error = true,
                    PlayerEvent::EndFile => ended = true,
                    PlayerEvent::FileLoaded => {}
                }
            }
            player.render(320, 180)?;
            ensure!(
                Instant::now() < deadline,
                "Missing file did not report a playback error and end event"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
