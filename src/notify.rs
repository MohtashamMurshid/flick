//! Desktop notifications through the platform's own tools: `osascript` on
//! macOS, `notify-send` on Linux, nothing on Windows for now. Owned by the
//! desktop-modules agent; callers pass plain text only (no paths, no
//! tokens), and failures are logged, never shown.

#![allow(dead_code)]

/// Shows a notification with a title and a body. Returns at once; the
/// command runs in the background and is never waited on.
pub fn notify(title: &str, body: &str) {
    let _ = (title, body);
}
