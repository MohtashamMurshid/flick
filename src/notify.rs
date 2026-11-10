//! Desktop notifications through the platform's own tools: `osascript` on
//! macOS, `notify-send` on Linux, nothing on Windows for now. Owned by the
//! desktop-modules agent; callers pass plain text only (no paths, no
//! tokens), and failures are logged, never shown.

#![allow(dead_code)]

/// Longest title shown, in characters.
const TITLE_LIMIT: usize = 80;
/// Longest body shown, in characters.
const BODY_LIMIT: usize = 240;

/// Shows a notification with a title and a body. Returns at once; the
/// command runs in the background and is never waited on.
pub fn notify(title: &str, body: &str) {
    let Some((program, args)) = command(title, body) else {
        return;
    };
    let spawned = std::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Reaped off the caller's thread so it never leaves a zombie.
            let reaper = std::thread::Builder::new()
                .name("flick-notify".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            if let Err(error) = reaper {
                tracing::debug!(%error, "Notification left unreaped");
            }
        }
        Err(error) => tracing::debug!(program, %error, "Notification not shown"),
    }
}

/// The program and arguments for this platform; None where there is none.
#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<(&'static str, Vec<String>)> {
    Some(("osascript", vec!["-e".into(), apple_script(title, body)]))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(title: &str, body: &str) -> Option<(&'static str, Vec<String>)> {
    // Arguments go straight to the program with no shell, so only length
    // and control characters need care. Missing `notify-send` fails the
    // spawn, which is logged.
    Some((
        "notify-send",
        vec![
            "--app-name=Flick".into(),
            "--".into(),
            plain(title, TITLE_LIMIT),
            plain(body, BODY_LIMIT),
        ],
    ))
}

#[cfg(not(unix))]
fn command(_title: &str, _body: &str) -> Option<(&'static str, Vec<String>)> {
    None
}

/// `display notification "<body>" with title "<title>"`, both escaped.
fn apple_script(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape_apple_script(&plain(body, BODY_LIMIT)),
        escape_apple_script(&plain(title, TITLE_LIMIT)),
    )
}

/// The inside of an AppleScript string literal: backslashes and double
/// quotes escaped. Run on text that has already been through [`plain`].
fn escape_apple_script(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            c => escaped.push(c),
        }
    }
    escaped
}

/// One line of at most `limit` characters: control characters, including
/// newlines, become spaces, runs of spaces collapse, and a cut ends in `…`.
fn plain(text: &str, limit: usize) -> String {
    let mut line = String::with_capacity(text.len().min(limit * 4));
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' && (line.is_empty() || line.ends_with(' ')) {
            continue;
        }
        line.push(c);
    }
    let line = line.trim_end();
    if line.chars().count() <= limit {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(limit.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(escape_apple_script(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_apple_script(r"a\b"), r"a\\b");
        // A backslash before a quote cannot end the string early.
        assert_eq!(escape_apple_script(r#"\""#), r#"\\\""#);
        assert_eq!(escape_apple_script("plain ‘text’ ✓"), "plain ‘text’ ✓");
    }

    #[test]
    fn the_script_keeps_text_inside_its_strings() {
        let script = apple_script(r#"Done" with title "pwned"#, "Line one\nLine \"two\"\\");
        assert_eq!(
            script,
            r#"display notification "Line one Line \"two\"\\" with title "Done\" with title \"pwned""#
        );
    }

    #[test]
    fn text_is_one_line_and_capped() {
        assert_eq!(plain("  a\n\n b\t\rc  ", 80), "a b c");
        assert_eq!(plain("", 80), "");
        let long = "word ".repeat(100);
        let cut = plain(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with("word…"), "{cut:?}");
        let wide = "é".repeat(300);
        assert_eq!(plain(&wide, BODY_LIMIT).chars().count(), BODY_LIMIT);
        assert_eq!(plain("exactly", 7), "exactly");
    }

    #[test]
    fn the_cap_applies_before_escaping() {
        let script = apple_script(&"\"".repeat(200), "body");
        // 79 quotes and an ellipsis, each quote escaped.
        assert!(
            script.ends_with(&format!("\"{}…\"", "\\\"".repeat(79))),
            "{script}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_runs_osascript_with_one_script_argument() {
        let (program, args) = command("Flick", "Copy finished").unwrap();
        assert_eq!(program, "osascript");
        assert_eq!(
            args,
            vec![
                "-e".to_owned(),
                r#"display notification "Copy finished" with title "Flick""#.to_owned()
            ]
        );
    }
}
