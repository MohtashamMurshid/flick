//! Keeps the display awake while a video plays.
//!
//! mpv's own windows hold off the screensaver and display sleep
//! (`stop-screensaver`), but `vo=libmpv` has no window, and libmpv leaves
//! this to the application. Without it the display dims and sleeps in the
//! middle of a film. Flick holds the platform's assertion while a video is
//! playing, as IINA does, and releases it on pause, at the end, or when
//! the player closes.
//!
//! macOS uses an IOKit power assertion, shown by `pmset -g assertions`.
//! Windows sets the UI thread's execution state, so [`DisplayAwake`] must
//! stay on the thread that created it. Linux has no implementation yet:
//! its screensaver inhibitors are D-Bus calls that last only while the
//! connection that made them stays open.

/// What the assertion is called where the system lists it.
const REASON: &str = "Flick is playing a video";

#[derive(Default)]
pub struct DisplayAwake {
    held: Option<platform::Hold>,
    /// Set after the first failure, so an unsupported platform is logged once.
    failed: bool,
}

impl DisplayAwake {
    /// Holds the display awake while `playing`, and releases it otherwise.
    /// Cheap to call on every tick: it acts only when `playing` changes.
    pub fn set(&mut self, playing: bool) {
        if !playing {
            self.held = None;
            return;
        }
        if self.held.is_some() || self.failed {
            return;
        }
        match platform::Hold::acquire(REASON) {
            Ok(hold) => self.held = Some(hold),
            Err(error) => {
                self.failed = true;
                tracing::info!(%error, "Could not keep the display awake during playback");
            }
        }
    }

    #[cfg(test)]
    fn holding(&self) -> bool {
        self.held.is_some()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, ensure};
    use std::ffi::{CString, c_char, c_void};

    type CfStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> CfStringRef;
        fn CFRelease(value: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CfStringRef,
            level: u32,
            name: CfStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// An owned CFString, released on drop.
    struct CfString(CfStringRef);

    impl CfString {
        fn new(text: &str) -> Result<Self> {
            let text = CString::new(text)?;
            // SAFETY: a valid C string; a null allocator means the default.
            let string =
                unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
            ensure!(!string.is_null(), "Could not create a CFString");
            Ok(Self(string))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: created above and released exactly once.
            unsafe { CFRelease(self.0) };
        }
    }

    pub struct Hold(u32);

    impl Hold {
        pub fn acquire(reason: &str) -> Result<Self> {
            let kind = CfString::new("PreventUserIdleDisplaySleep")?;
            let name = CfString::new(reason)?;
            let mut id = 0;
            // SAFETY: both strings live through the call; id is writable.
            let code = unsafe { IOPMAssertionCreateWithName(kind.0, LEVEL_ON, name.0, &mut id) };
            ensure!(code == 0, "IOKit refused the assertion (error {code:#x})");
            Ok(Self(id))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: an assertion this process created and releases once.
            unsafe { IOPMAssertionRelease(self.0) };
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::{Result, ensure};
    use std::marker::PhantomData;

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    /// The calling thread's execution state, so not `Send`.
    pub struct Hold(PhantomData<*const ()>);

    impl Hold {
        pub fn acquire(_reason: &str) -> Result<Self> {
            // SAFETY: plain flags; the state belongs to this thread.
            let previous = unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)
            };
            ensure!(previous != 0, "Windows refused to keep the display on");
            Ok(Self(PhantomData))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: clears what acquire set, on the same thread.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    pub struct Hold;

    impl Hold {
        pub fn acquire(_reason: &str) -> anyhow::Result<Self> {
            anyhow::bail!("not supported on this platform yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn listed(reason: &str) -> bool {
        let output = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        String::from_utf8_lossy(&output.stdout).contains(reason)
    }

    /// The assertion appears in `pmset -g assertions` while held and goes
    /// away on release. powerd may take a moment to update its list.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_lists_the_assertion_only_while_held() {
        let reason = format!("Flick awake test {}", std::process::id());
        let until = |want: bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while listed(&reason) != want && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            listed(&reason) == want
        };
        let hold = platform::Hold::acquire(&reason).unwrap();
        assert!(until(true), "assertion not listed while held");
        drop(hold);
        assert!(until(false), "assertion still listed after release");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn holds_only_while_playing() {
        let mut awake = DisplayAwake::default();
        awake.set(true);
        assert!(awake.holding());
        awake.set(true);
        assert!(awake.holding());
        awake.set(false);
        assert!(!awake.holding());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unsupported_platforms_try_once() {
        let mut awake = DisplayAwake::default();
        awake.set(true);
        assert!(!awake.holding());
        assert!(awake.failed);
    }
}
