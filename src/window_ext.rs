//! Window features GPUI 0.2.2 lacks: keeping the window above others, as
//! IINA's Float on Top does. macOS sets the window level and Windows the
//! topmost flag through the native handle; Linux window managers offer no
//! portable way to ask, so it reports that.

use anyhow::{Result, bail};
use gpui::Window;
use raw_window_handle::HasWindowHandle;

/// The window's `HWND` on Windows, for the media controls; None elsewhere.
pub fn win32_handle(window: &Window) -> Option<isize> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        #[cfg(target_os = "windows")]
        raw_window_handle::RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Keeps `window` above other windows, or lets it go back among them.
pub fn set_floating(window: &Window, on: bool) -> Result<()> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("The window has no native handle: {error:?}"))?;
    match handle.as_raw() {
        #[cfg(target_os = "macos")]
        raw_window_handle::RawWindowHandle::AppKit(handle) => {
            macos::set_floating(handle.ns_view.as_ptr(), on)
        }
        #[cfg(target_os = "windows")]
        raw_window_handle::RawWindowHandle::Win32(handle) => {
            windows::set_topmost(handle.hwnd.get(), on)
        }
        _ => {
            let _ = on;
            bail!("This system does not let Flick keep its window on top.")
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Result, ensure};
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use std::ffi::c_void;

    /// `NSFloatingWindowLevel` and `NSNormalWindowLevel`.
    const FLOATING: isize = 3;
    const NORMAL: isize = 0;

    pub fn set_floating(view: *mut c_void, on: bool) -> Result<()> {
        let view = view.cast::<AnyObject>();
        // SAFETY: GPUI's live NSView for this window, messaged on the main
        // thread; `window` and `setLevel:` are standard NSView/NSWindow API.
        unsafe {
            let window: *mut AnyObject = msg_send![view, window];
            ensure!(!window.is_null(), "The video window is not open");
            let _: () = msg_send![window, setLevel: if on { FLOATING } else { NORMAL }];
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use anyhow::Result;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };

    pub fn set_topmost(hwnd: isize, on: bool) -> Result<()> {
        // SAFETY: GPUI's live window handle; only the z-order changes.
        unsafe {
            SetWindowPos(
                HWND(hwnd as *mut _),
                Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
        }
        Ok(())
    }
}

/// The window size, in points, that shows a `width`×`height` video at
/// `factor` of its own size on a display with `scale` pixels per point,
/// shrunk to fit within `room` and never smaller than `minimum`. A factor
/// of 0 fits the video to `room`.
pub fn window_size(
    (width, height): (u32, u32),
    factor: f32,
    scale: f32,
    room: (f32, f32),
    minimum: (f32, f32),
) -> (f32, f32) {
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    let (mut w, mut h) = if factor > 0. {
        (
            width * factor / scale.max(1.),
            height * factor / scale.max(1.),
        )
    } else {
        (f32::INFINITY, f32::INFINITY)
    };
    let fit = (room.0 / w).min(room.1 / h);
    if fit < 1. || factor <= 0. {
        let fit = (room.0 / width).min(room.1 / height);
        (w, h) = (width * fit, height * fit);
    }
    (w.max(minimum.0).round(), h.max(minimum.1).round())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_follow_the_video_and_fit_the_screen() {
        let room = (1400., 800.);
        let minimum = (760., 500.);
        // A 1080p video at half size on a Retina display.
        assert_eq!(
            window_size((1920, 1080), 0.5, 2., room, minimum),
            (760., 500.)
        );
        assert_eq!(
            window_size((1920, 1080), 1., 2., room, minimum),
            (960., 540.)
        );
        // Double would be too big, so it fits.
        assert_eq!(
            window_size((1920, 1080), 2., 2., room, minimum),
            (1400., 788.)
        );
        assert_eq!(
            window_size((1920, 1080), 0., 1., room, minimum),
            (1400., 788.)
        );
        // A portrait video keeps its shape.
        assert_eq!(
            window_size((720, 1280), 0., 1., room, minimum),
            (760., 800.)
        );
    }
}
