//! The libmpv boundary and a portable software presentation baseline.
//!
//! libmpv is loaded at runtime, so building Flick does not need native mpv
//! development headers or a link-time library. Decoding uses the platform's
//! hardware decoder when one is available and copies frames back to memory;
//! scaling and subtitle composition stay on the CPU. GPUI uploads the final
//! BGRA image; this path is functional scaffolding, not a zero-copy GPU path.
//!
//! Keep this object on the UI thread. Advanced render control is deliberately
//! disabled: synchronous client calls and rendering on one thread can otherwise
//! deadlock. As upstream recommends, the desktop moves rendering to its own
//! thread with [`Player::render_in_background`], so scaling a large frame never
//! blocks the interface. Tests and diagnostics can still render synchronously.

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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

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
    pub chapters: Vec<Chapter>,
    /// A–B loop points in seconds, when set.
    pub loop_start: Option<f64>,
    pub loop_end: Option<f64>,
    /// Seconds; positive delays audio relative to video.
    pub audio_delay: f64,
    /// Waiting for stream data, such as torrent pieces.
    pub buffering: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub time: f64,
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
            chapters: Vec::new(),
            loop_start: None,
            loop_end: None,
            audio_delay: 0.0,
            buffering: false,
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
type StreamOpen = unsafe extern "C" fn(*mut c_void, *mut c_char, *mut StreamCallbacks) -> c_int;
type StreamAdd = unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void, StreamOpen) -> c_int;

/// `mpv_stream_cb_info` from libmpv/stream_cb.h.
#[repr(C)]
struct StreamCallbacks {
    cookie: *mut c_void,
    read: Option<unsafe extern "C" fn(*mut c_void, *mut c_char, u64) -> i64>,
    seek: Option<unsafe extern "C" fn(*mut c_void, i64) -> i64>,
    size: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    close: Option<unsafe extern "C" fn(*mut c_void)>,
    cancel: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// libmpv's MPV_ERROR_LOADING_FAILED and MPV_ERROR_UNSUPPORTED.
const MPV_ERROR_LOADING_FAILED: c_int = -13;
const MPV_ERROR_UNSUPPORTED: i64 = -18;

/// A readable, seekable source behind a custom URL scheme, such as a
/// torrent file that is still downloading. Reads may block until verified
/// data is available; libmpv calls them on its demuxer thread.
pub trait MediaStream: Send {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize>;
    fn seek(&mut self, offset: u64) -> std::io::Result<u64>;
    fn size(&self) -> Option<u64>;
    /// Makes a blocked read fail promptly; called from another thread.
    fn canceller(&self) -> Arc<dyn Fn() + Send + Sync>;
}

/// Opens a stream for a URI of a registered protocol.
pub type StreamOpener = dyn Fn(&str) -> std::io::Result<Box<dyn MediaStream>> + Send + Sync;

struct StreamCookie {
    stream: Mutex<Box<dyn MediaStream>>,
    cancel: Arc<dyn Fn() + Send + Sync>,
}

/// Runs a callback body without letting a panic cross into C.
fn guarded<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(fallback)
}

unsafe extern "C" fn stream_open(
    opener: *mut c_void,
    uri: *mut c_char,
    info: *mut StreamCallbacks,
) -> c_int {
    guarded(MPV_ERROR_LOADING_FAILED, || {
        // SAFETY: the opener is the Arc boxed by register_protocol, alive
        // until the core is destroyed; uri and info are valid for this call.
        let opener = unsafe { &*opener.cast::<Arc<StreamOpener>>() };
        let Ok(uri) = unsafe { CStr::from_ptr(uri) }.to_str() else {
            return MPV_ERROR_LOADING_FAILED;
        };
        let stream = match opener(uri) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, uri, "Could not open stream");
                return MPV_ERROR_LOADING_FAILED;
            }
        };
        let cookie = Box::new(StreamCookie {
            cancel: stream.canceller(),
            stream: Mutex::new(stream),
        });
        // SAFETY: libmpv passes a writable info struct.
        let info = unsafe { &mut *info };
        info.cookie = Box::into_raw(cookie).cast();
        info.read = Some(stream_read);
        info.seek = Some(stream_seek);
        info.size = Some(stream_size);
        info.close = Some(stream_close);
        info.cancel = Some(stream_cancel);
        0
    })
}

/// # Safety
///
/// `cookie` must come from `stream_open` and not yet be closed.
unsafe fn cookie<'a>(cookie: *mut c_void) -> &'a StreamCookie {
    // SAFETY: guaranteed by the caller.
    unsafe { &*cookie.cast::<StreamCookie>() }
}

unsafe extern "C" fn stream_read(cookie_ptr: *mut c_void, buffer: *mut c_char, bytes: u64) -> i64 {
    guarded(-1, || {
        // SAFETY: libmpv passes its cookie and a buffer of `bytes` bytes.
        let cookie = unsafe { cookie(cookie_ptr) };
        let len = usize::try_from(bytes).unwrap_or(usize::MAX).min(1 << 20);
        let buffer = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), len) };
        let mut stream = cookie.stream.lock().unwrap_or_else(|e| e.into_inner());
        match stream.read(buffer) {
            Ok(read) => read as i64,
            Err(error) => {
                tracing::debug!(%error, "Stream read failed");
                -1
            }
        }
    })
}

unsafe extern "C" fn stream_seek(cookie_ptr: *mut c_void, offset: i64) -> i64 {
    guarded(MPV_ERROR_UNSUPPORTED, || {
        // SAFETY: libmpv passes its cookie.
        let cookie = unsafe { cookie(cookie_ptr) };
        let Ok(offset) = u64::try_from(offset) else {
            return MPV_ERROR_UNSUPPORTED;
        };
        let mut stream = cookie.stream.lock().unwrap_or_else(|e| e.into_inner());
        stream
            .seek(offset)
            .ok()
            .and_then(|at| i64::try_from(at).ok())
            .unwrap_or(-1)
    })
}

unsafe extern "C" fn stream_size(cookie_ptr: *mut c_void) -> i64 {
    guarded(MPV_ERROR_UNSUPPORTED, || {
        // SAFETY: libmpv passes its cookie.
        let cookie = unsafe { cookie(cookie_ptr) };
        let stream = cookie.stream.lock().unwrap_or_else(|e| e.into_inner());
        stream
            .size()
            .and_then(|size| i64::try_from(size).ok())
            .unwrap_or(MPV_ERROR_UNSUPPORTED)
    })
}

unsafe extern "C" fn stream_cancel(cookie_ptr: *mut c_void) {
    guarded((), || {
        // SAFETY: libmpv passes its cookie; cancel never takes the stream
        // lock, which a blocked read holds.
        (unsafe { cookie(cookie_ptr) }.cancel)()
    })
}

unsafe extern "C" fn stream_close(cookie_ptr: *mut c_void) {
    guarded((), || {
        // SAFETY: close is libmpv's last call for this cookie.
        drop(unsafe { Box::from_raw(cookie_ptr.cast::<StreamCookie>()) })
    })
}

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
    /// Absent from libmpv builds without the stream callback API.
    stream_add: Option<StreamAdd>,
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
                stream_add: library.get(b"mpv_stream_cb_add_ro\0").ok().map(|f| *f),
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

/// Chapters from mpv's `chapter-list` JSON, sorted by start time.
fn parse_chapters(json: &str) -> Vec<Chapter> {
    let mut chapters = serde_json::from_str::<Vec<Value>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|chapter| {
            let time = chapter["time"]
                .as_f64()
                .filter(|t| t.is_finite() && *t >= 0.)?;
            Some(Chapter {
                title: chapter["title"].as_str().unwrap_or_default().to_owned(),
                time,
            })
        })
        .collect::<Vec<_>>();
    chapters.sort_by(|a, b| a.time.total_cmp(&b.time));
    chapters
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

// mpv requires at least four-byte alignment for BGR0. The fallback buffer
// uses 64-byte pointer and stride alignment, mpv's fast path.
#[derive(Clone)]
#[repr(C, align(64))]
struct RenderBlock([u8; 64]);

#[derive(Default)]
struct RenderBuffer {
    width: u32,
    height: u32,
    stride: usize,
    /// Allocated only for [`Target::Aligned`] frames.
    blocks: Vec<RenderBlock>,
}

/// Where libmpv writes a frame before GPUI receives it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    /// Straight into the tightly packed frame handed to GPUI: no extra copy.
    Direct,
    /// Into a persistent 64-byte-aligned buffer, then copied row by row.
    Aligned,
}

impl Target {
    /// GPUI needs tightly packed rows and takes ownership of each frame, so
    /// libmpv renders straight into the frame whenever the allocation meets
    /// BGR0's four-byte pixel alignment. On Linux this beat the aligned copy
    /// at every measured size, even without 64-byte rows or pointers (see
    /// docs/VALIDATION.md). Rust only guarantees one-byte alignment for a
    /// byte vector, so the aligned buffer remains the fallback.
    fn choose(pointer: usize) -> Self {
        if pointer.is_multiple_of(4) {
            Self::Direct
        } else {
            Self::Aligned
        }
    }
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
    /// Set by [`Player::render_in_background`]; owns the render context.
    render_thread: RefCell<Option<RenderThread>>,
    /// Openers for registered protocols. libmpv keeps protocols until the
    /// core is destroyed, and fields drop after `Drop::drop` destroys it.
    /// Each is boxed because libmpv holds a thin pointer to the Arc, which
    /// must not move when the vector grows.
    #[allow(clippy::vec_box)]
    protocols: RefCell<Vec<Box<Arc<StreamOpener>>>>,
    api: &'static Api,
    _main_thread: PhantomData<Rc<()>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        Self::create(&[])
    }

    /// A silent player for poster frames and previews: no audio output,
    /// audio decoding, or subtitles.
    pub fn silent() -> Result<Self> {
        Self::create(&[
            ("ao", "null"),
            ("aid", "no"),
            ("sid", "no"),
            ("mute", "yes"),
        ])
    }

    fn create(extra: &[(&str, &str)]) -> Result<Self> {
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
            render_thread: RefCell::new(None),
            protocols: RefCell::new(Vec::new()),
            api,
            _main_thread: PhantomData,
        };
        // FLICK_HWDEC=no compares against pure software decoding.
        let hwdec = std::env::var("FLICK_HWDEC").unwrap_or_else(|_| "auto-copy".into());
        // Any failure after creating the handle drops player and releases it.
        for (name, value) in [
            ("config", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("vo", "libmpv"),
            // Copy-back decoding keeps the software renderer's frame access.
            ("hwdec", hwdec.as_str()),
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
        for (name, value) in extra {
            player.option(name, value)?;
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
        if let Some(thread) = self.render_thread.borrow().as_ref() {
            thread.shared.generation.fetch_add(1, Ordering::AcqRel);
            thread.shared.update(|state| {
                state.target = (0, 0);
                false
            });
        }
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
        // ab-loop-a/b read "no" when unset.
        let point = |name| {
            self.property(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite())
        };
        PlaybackSnapshot {
            paused: flag("pause"),
            position: number("time-pos", 0.0).max(0.0),
            duration: number("duration", 0.0).max(0.0),
            volume: number("volume", 100.0),
            muted: flag("mute"),
            speed: number("speed", 1.0),
            tracks,
            eof: flag("eof-reached"),
            chapters: self
                .property("chapter-list")
                .map(|json| parse_chapters(&json))
                .unwrap_or_default(),
            loop_start: point("ab-loop-a"),
            loop_end: point("ab-loop-b"),
            audio_delay: number("audio-delay", 0.0),
            buffering: flag("paused-for-cache"),
        }
    }

    /// The video's display size after aspect correction, once known.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        let dimension = |name| {
            self.property(name)
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0)
        };
        Some((dimension("video-params/dw")?, dimension("video-params/dh")?))
    }

    /// The video codec's short name, such as `h264`.
    pub fn video_codec(&self) -> Option<String> {
        self.property("video-codec-name")
            .or_else(|| self.property("current-tracks/video/codec"))
            .filter(|value| !value.is_empty())
    }

    /// Frames mpv dropped because presentation fell behind.
    pub fn dropped_frames(&self) -> Option<u64> {
        self.property("frame-drop-count")?.parse().ok()
    }

    /// Read a libmpv diagnostic property without changing playback state.
    /// Unavailable properties stay `None`, rather than being reported as zero.
    pub fn diagnostic_property(&self, name: &str) -> Option<String> {
        self.property(name)
    }

    /// The active hardware decoder, or `None` while decoding in software.
    pub fn hardware_decoder(&self) -> Option<String> {
        self.property("hwdec-current")
            .filter(|value| !value.is_empty() && value != "no")
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
    /// After [`Player::render_in_background`], frames come from the render
    /// thread instead and this returns `None`.
    pub fn render(&self, width: u32, height: u32) -> Result<Option<VideoFrame>> {
        self.render_to(width, height, None)
    }

    fn render_to(
        &self,
        width: u32,
        height: u32,
        force: Option<Target>,
    ) -> Result<Option<VideoFrame>> {
        if width == 0 || height == 0 || !self.file_loaded.get() || self.renderer.is_null() {
            return Ok(None);
        }
        // Keep callback notifications even when libmpv's 200ms presentation
        // timeout has already promoted next_frame to cur_frame. In that case
        // update() returns no pending frame, but cur_frame still needs drawing.
        // Relying only on update() freezes paused seek previews at the old image.
        let requested = self.redraw_requested.swap(false, Ordering::AcqRel);
        // SAFETY: the renderer is live and only used on this Player's thread.
        unsafe {
            render_frame(
                self.api,
                self.renderer,
                &mut self.buffer.borrow_mut(),
                requested,
                (width, height),
                force,
            )
        }
    }

    /// Moves rendering to a dedicated thread, so software scaling never
    /// blocks the caller. libmpv's update callback wakes the thread, which
    /// renders at the size set by [`Player::set_render_target`] and sends
    /// each frame with the load generation it belongs to. Upstream
    /// recommends separating client calls from rendering this way.
    pub fn render_in_background(&mut self) -> Result<RenderedFrames> {
        ensure!(
            !self.renderer.is_null(),
            "Rendering already moved to a thread"
        );
        let shared = Arc::new(RenderShared::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let renderer = SendPointer(self.renderer);
        let api = self.api;
        let thread_shared = shared.clone();
        let join = thread::Builder::new()
            .name("flick-render".into())
            .spawn(move || render_thread(api, renderer, thread_shared, sender))
            .context("Start the video render thread")?;
        // The thread owns and frees the render context from here on.
        self.renderer = ptr::null_mut();
        *self.render_thread.borrow_mut() = Some(RenderThread {
            shared,
            join: Some(join),
        });
        Ok(receiver)
    }

    /// The physical size the render thread draws at; zero stops drawing,
    /// for example while the library is shown.
    pub fn set_render_target(&self, width: u32, height: u32) {
        let target = if self.file_loaded.get() {
            (width, height)
        } else {
            (0, 0)
        };
        if let Some(thread) = self.render_thread.borrow().as_ref() {
            thread.shared.update(|state| {
                let changed = state.target != target;
                state.target = target;
                changed
            });
        }
    }

    /// Serves `protocol://…` URIs from streams made by `opener`, so they can
    /// be loaded like files.
    pub fn register_protocol(&self, protocol: &str, opener: Arc<StreamOpener>) -> Result<()> {
        let add = self
            .api
            .stream_add
            .context("This libmpv build cannot play from Flick streams")?;
        let name = CString::new(protocol).context("Protocol name contains a NUL byte")?;
        let opener = Box::new(opener);
        let user_data = (&*opener as *const Arc<StreamOpener>).cast_mut().cast();
        // SAFETY: the handle is live; the boxed opener keeps a stable address
        // and is kept until after the core is destroyed.
        self.api.check(
            unsafe { add(self.handle.as_ptr(), name.as_ptr(), user_data, stream_open) },
            &format!("Register {protocol} streams"),
        )?;
        self.protocols.borrow_mut().push(opener);
        Ok(())
    }

    /// The generation of frames for the file most recently loaded.
    pub fn frame_generation(&self) -> u64 {
        self.render_thread
            .borrow()
            .as_ref()
            .map_or(0, |thread| thread.shared.generation.load(Ordering::Acquire))
    }
}

/// Frames from the render thread: the load generation and the frame, or an
/// error message.
pub type RenderedFrames = tokio::sync::mpsc::UnboundedReceiver<(u64, Result<VideoFrame, String>)>;

/// A raw render context handed to the render thread, which becomes its only
/// user.
struct SendPointer(*mut c_void);

// SAFETY: libmpv render contexts may be used from any one thread at a time;
// after the handoff only the render thread touches this pointer.
unsafe impl Send for SendPointer {}

#[derive(Default)]
struct RenderState {
    target: (u32, u32),
    redraw: bool,
    stop: bool,
}

#[derive(Default)]
struct RenderShared {
    state: Mutex<RenderState>,
    wake: Condvar,
    /// Incremented by each load, so frames from a replaced file are ignored.
    generation: AtomicU64,
}

impl RenderShared {
    /// Changes the state and wakes the render thread when `change` says so.
    fn update(&self, change: impl FnOnce(&mut RenderState) -> bool) {
        let wake = match self.state.lock() {
            Ok(mut state) => change(&mut state),
            Err(poisoned) => change(&mut poisoned.into_inner()),
        };
        if wake {
            self.wake.notify_one();
        }
    }
}

struct RenderThread {
    shared: Arc<RenderShared>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        self.shared.update(|state| {
            state.stop = true;
            true
        });
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

unsafe extern "C" fn wake_render_thread(context: *mut c_void) {
    // SAFETY: the context is the render thread's Arc<RenderShared>, which
    // the thread keeps alive until it unregisters this callback. A brief
    // lock is the only work done on libmpv's thread.
    let shared = unsafe { &*context.cast::<RenderShared>() };
    shared.update(|state| {
        state.redraw = true;
        true
    });
}

fn render_thread(
    api: &'static Api,
    renderer: SendPointer,
    shared: Arc<RenderShared>,
    frames: tokio::sync::mpsc::UnboundedSender<(u64, Result<VideoFrame, String>)>,
) {
    let renderer = renderer.0;
    // SAFETY: the renderer is live, and the context outlives registration.
    unsafe {
        (api.set_render_callback)(
            renderer,
            Some(wake_render_thread),
            Arc::as_ptr(&shared).cast_mut().cast(),
        );
    }
    let mut buffer = RenderBuffer::default();
    loop {
        let (target, requested) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stop {
                    break;
                }
                let visible = state.target.0 > 0 && state.target.1 > 0;
                let resized = state.target != (buffer.width, buffer.height);
                if visible && (state.redraw || resized) {
                    break;
                }
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.stop {
                break;
            }
            (state.target, std::mem::take(&mut state.redraw))
        };
        let generation = shared.generation.load(Ordering::Acquire);
        // SAFETY: this thread is the render context's only user.
        let result = unsafe { render_frame(api, renderer, &mut buffer, requested, target, None) };
        let message = match result {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => continue,
            Err(error) => Err(format!("{error:#}")),
        };
        if frames.send((generation, message)).is_err() {
            break;
        }
    }
    // SAFETY: unregister before freeing; the core is destroyed only after
    // this thread is joined.
    unsafe {
        (api.set_render_callback)(renderer, None, ptr::null_mut());
        (api.render_free)(renderer);
    }
}

/// Renders the current frame into a new packed BGRA frame when libmpv has a
/// new frame, the size changed, or a redraw was requested.
///
/// # Safety
///
/// `renderer` must be a live render context used by no other thread.
unsafe fn render_frame(
    api: &Api,
    renderer: *mut c_void,
    buffer: &mut RenderBuffer,
    requested: bool,
    (width, height): (u32, u32),
    force: Option<Target>,
) -> Result<Option<VideoFrame>> {
    ensure!(
        c_int::try_from(width).is_ok() && c_int::try_from(height).is_ok(),
        "The requested video dimensions cannot be represented by libmpv"
    );
    let resized = buffer.width != width || buffer.height != height;
    // SAFETY: guaranteed by the caller.
    let updates = unsafe { (api.render_update)(renderer) };
    if updates & 1 == 0 && !resized && !requested {
        return Ok(None);
    }
    if resized {
        tracing::debug!(width, height, "Video presentation resized");
    }
    buffer.width = width;
    buffer.height = height;
    let row_bytes = (width as usize)
        .checked_mul(4)
        .context("Video row size exceeds this platform's address space")?;
    let byte_count = row_bytes
        .checked_mul(height as usize)
        .context("Video frame size overflow")?;
    let mut pixels = Vec::<u8>::new();
    pixels
        .try_reserve_exact(byte_count)
        .context("Allocate video frame at display resolution")?;
    match force.unwrap_or_else(|| Target::choose(pixels.as_ptr().addr())) {
        Target::Direct => {
            // SAFETY: the reserved capacity holds row_bytes * height bytes
            // and outlives the call. Pointer and stride are at least
            // four-byte aligned (BGR0's pixel size).
            unsafe { render_into(api, renderer, pixels.as_mut_ptr(), row_bytes, width, height)? };
            // SAFETY: on success libmpv overwrites every byte from the
            // pointer to pointer + stride * height (libmpv render.h).
            unsafe { pixels.set_len(byte_count) };
            buffer.blocks = Vec::new();
        }
        Target::Aligned => {
            let stride = row_bytes
                .checked_add(63)
                .map(|bytes| bytes / 64 * 64)
                .context("Video row size exceeds this platform's address space")?;
            let blocks = stride
                .checked_mul(height as usize)
                .context("Video surface exceeds this platform's address space")?
                / 64;
            if buffer.stride != stride || buffer.blocks.len() != blocks {
                let additional = blocks.saturating_sub(buffer.blocks.len());
                buffer
                    .blocks
                    .try_reserve_exact(additional)
                    .context("Allocate video presentation surface")?;
                buffer.blocks.resize(blocks, RenderBlock([0; 64]));
                buffer.stride = stride;
            }
            // SAFETY: the block vector has 64-byte alignment and
            // stride * height writable bytes, including row padding.
            unsafe {
                render_into(
                    api,
                    renderer,
                    buffer.blocks.as_mut_ptr().cast(),
                    stride,
                    width,
                    height,
                )?
            };
            // SAFETY: RenderBlock has no padding, contains initialized u8
            // values, and the vector covers exactly stride * height bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(buffer.blocks.as_ptr().cast::<u8>(), blocks * 64)
            };
            for row in bytes.chunks_exact(stride) {
                pixels.extend_from_slice(&row[..row_bytes]);
            }
        }
    }
    // BGR0 leaves the fourth byte undefined, and every video frame is
    // opaque, including black letterbox bars. Set alpha rather than
    // exposing decoder/format-specific alpha semantics.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    Ok(Some(VideoFrame {
        width,
        height,
        pixels,
    }))
}

/// # Safety
///
/// `renderer` must be a live render context used by no other thread, and
/// `pixels` must be valid for writes of `stride * height` bytes and, like
/// `stride`, aligned to at least four bytes.
unsafe fn render_into(
    api: &Api,
    renderer: *mut c_void,
    pixels: *mut u8,
    stride: usize,
    width: u32,
    height: u32,
) -> Result<()> {
    let mut dimensions = [width as c_int, height as c_int];
    let mut stride = stride; // MPV_RENDER_PARAM_SW_STRIDE is size_t*.
    let mut block_for_time: c_int = 0;
    let mut params = [
        RenderParam {
            kind: 17, // SW_SIZE: int[2]
            data: dimensions.as_mut_ptr().cast(),
        },
        RenderParam {
            kind: 18, // SW_FORMAT: char*
            data: c"bgr0".as_ptr().cast_mut().cast(),
        },
        RenderParam {
            kind: 19, // SW_STRIDE: size_t*
            data: (&mut stride as *mut usize).cast(),
        },
        RenderParam {
            kind: 20, // SW_POINTER: pointer to writable pixels.
            data: pixels.cast(),
        },
        RenderParam {
            kind: 12, // BLOCK_FOR_TARGET_TIME: int*, don't sleep on UI.
            data: (&mut block_for_time as *mut c_int).cast(),
        },
        RenderParam::end(),
    ];
    // SAFETY: all params outlive this call; the caller guarantees the
    // target memory, alignment, and exclusive use of the renderer.
    api.check(
        unsafe { (api.render)(renderer, params.as_mut_ptr()) },
        "Render video frame",
    )
}

impl Drop for Player {
    fn drop(&mut self) {
        // The render thread frees its context; join it before the core goes.
        self.render_thread.get_mut().take();
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

    #[test]
    fn chapter_lists_parse_sort_and_skip_invalid_entries() {
        let chapters = parse_chapters(
            r#"[{"title":"Two","time":60.5},{"time":0},{"title":"Bad","time":-1},{"title":"x"}]"#,
        );
        assert_eq!(
            chapters,
            vec![
                Chapter {
                    title: String::new(),
                    time: 0.
                },
                Chapter {
                    title: "Two".into(),
                    time: 60.5
                },
            ]
        );
        assert!(parse_chapters("not json").is_empty());
    }

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

    /// Exercise native output allocation above both former presentation limits.
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_presentation_has_no_fixed_resolution_cap() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")?;
        let player = Player::new()?;
        player.load(&clip)?;
        wait_for(&player, |p| {
            p.file_loaded.get() && p.snapshot().duration > 0.
        })?;
        player.set_property("pause", "yes")?;
        for (width, height) in [(10_000, 2), (7680, 4320)] {
            let frame = player
                .render(width, height)?
                .context("Missing resized video frame")?;
            assert_eq!((frame.width, frame.height), (width, height));
            assert_eq!(frame.pixels.len(), width as usize * height as usize * 4);
        }
        Ok(())
    }

    /// A registered protocol plays like a file, reading through callbacks.
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_custom_protocol_streams_play() -> Result<()> {
        struct FileStream {
            file: std::fs::File,
            size: u64,
            reads: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl MediaStream for FileStream {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                use std::io::Read;
                self.reads.fetch_add(1, Ordering::Relaxed);
                self.file.read(buffer)
            }
            fn seek(&mut self, offset: u64) -> std::io::Result<u64> {
                use std::io::Seek;
                self.file.seek(std::io::SeekFrom::Start(offset))
            }
            fn size(&self) -> Option<u64> {
                Some(self.size)
            }
            fn canceller(&self) -> Arc<dyn Fn() + Send + Sync> {
                Arc::new(|| {})
            }
        }
        let clip = PathBuf::from(std::env::var("FLICK_TEST_VIDEO")?);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let opened = reads.clone();
        let player = Player::new()?;
        player.register_protocol(
            "flicktest",
            Arc::new(move |uri: &str| {
                if uri != "flicktest://clip" {
                    return Err(std::io::Error::other("unknown stream"));
                }
                let file = std::fs::File::open(&clip)?;
                let size = file.metadata()?.len();
                Ok(Box::new(FileStream {
                    file,
                    size,
                    reads: opened.clone(),
                }) as Box<dyn MediaStream>)
            }),
        )?;
        player.load("flicktest://clip")?;
        wait_for(&player, |p| {
            p.file_loaded.get() && p.snapshot().position > 0.5
        })?;
        assert!(reads.load(Ordering::Relaxed) > 0);
        player.command(&["seek", "8", "absolute+exact"])?;
        wait_for(&player, |p| p.snapshot().position >= 8.)?;
        // An unknown stream fails like a missing file.
        player.load("flicktest://missing")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if player
                .poll_events()
                .iter()
                .any(|event| matches!(event, PlayerEvent::Error(_)))
            {
                break;
            }
            ensure!(Instant::now() < deadline, "Unknown stream did not fail");
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// The render thread delivers frames at the target size, stops at a zero
    /// target, tags frames with the load generation, and shuts down while
    /// playing.
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_render_thread_delivers_tagged_frames() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")?;
        let mut player = Player::new()?;
        let mut frames = player.render_in_background()?;
        assert!(
            player.render(320, 180)?.is_none(),
            "Synchronous rendering is off"
        );
        fn wait(
            player: &Player,
            frames: &mut RenderedFrames,
            size: (u32, u32),
            generation: u64,
        ) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                for event in player.poll_events() {
                    if let PlayerEvent::Error(error) = event {
                        bail!("{error}");
                    }
                }
                player.set_render_target(size.0, size.1);
                while let Ok((tag, frame)) = frames.try_recv() {
                    let frame = frame.map_err(anyhow::Error::msg)?;
                    if tag == generation && (frame.width, frame.height) == size {
                        assert_eq!(frame.pixels.len(), size.0 as usize * size.1 as usize * 4);
                        return Ok(());
                    }
                }
                ensure!(Instant::now() < deadline, "No frame from the render thread");
                thread::sleep(Duration::from_millis(10));
            }
        }
        player.load(&clip)?;
        let first = player.frame_generation();
        wait(&player, &mut frames, (321, 181), first)?;
        // A zero target stops rendering while playback continues.
        player.set_render_target(0, 0);
        thread::sleep(Duration::from_millis(300));
        while frames.try_recv().is_ok() {}
        thread::sleep(Duration::from_millis(300));
        assert!(frames.try_recv().is_err(), "Rendered with a zero target");
        player.load(&clip)?;
        let second = player.frame_generation();
        assert!(second > first);
        wait(&player, &mut frames, (640, 360), second)?;
        // A resize re-renders even when paused.
        player.set_property("pause", "yes")?;
        wait(&player, &mut frames, (480, 270), second)?;
        player.set_property("pause", "no")?;
        drop(player);
        Ok(())
    }

    /// Both presentation targets must produce identical pixels; timings show
    /// what the direct target saves. Run with --nocapture and a release build
    /// for meaningful numbers.
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_presentation_targets_match_and_report_cost() -> Result<()> {
        let clip = std::env::var("FLICK_TEST_VIDEO")?;
        let player = Player::new()?;
        player.load(&clip)?;
        wait_for(&player, |p| {
            p.file_loaded.get() && p.snapshot().position > 0.2
        })?;
        player.set_property("pause", "yes")?;
        // Let the pause settle so every render below shows the same frame.
        for _ in 0..4 {
            player.poll_events();
            player.render(323, 183)?;
            thread::sleep(Duration::from_millis(150));
        }
        // 1917 px rows are not a multiple of 64 bytes.
        for (width, height) in [(1920, 1080), (1917, 1080), (3840, 2160)] {
            let mut frames = Vec::new();
            for target in [Target::Aligned, Target::Direct] {
                let rounds = 20;
                let mut elapsed = Duration::ZERO;
                let mut last = None;
                for _ in 0..rounds {
                    player.redraw_requested.store(true, Ordering::Release);
                    let started = Instant::now();
                    let frame = player
                        .render_to(width, height, Some(target))?
                        .context("Requested redraw produced no frame")?;
                    elapsed += started.elapsed();
                    last = Some(frame);
                }
                let frame = last.context("No frame rendered")?;
                assert_eq!(frame.pixels.len(), width as usize * height as usize * 4);
                assert!(frame.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
                println!(
                    "{width}x{height} {target:?}: {:.2} ms/frame",
                    elapsed.as_secs_f64() * 1000. / rounds as f64
                );
                frames.push(frame.pixels);
            }
            assert!(
                frames[0] == frames[1],
                "Direct and aligned targets rendered different pixels at {width}x{height}"
            );
        }
        println!("hardware decoder: {:?}", player.hardware_decoder());
        Ok(())
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
