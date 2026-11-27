//! Where screenshots of the playing video go, and what they are called.
//!
//! libmpv writes the file itself with `screenshot-to-file`, at the video's
//! own resolution, choosing the format from the file's extension.

use std::path::{Path, PathBuf};

/// `Pictures/Flick`, or `Flick Screenshots` in the home folder on a system
/// without a Pictures folder.
pub fn folder() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(match dirs.picture_dir() {
        Some(pictures) => pictures.join("Flick"),
        None => dirs.home_dir().join("Flick Screenshots"),
    })
}

/// A file in `folder` for a screenshot of `title` at `seconds`, such as
/// `Sintel at 12.34.png`, numbered when that name is taken.
pub fn path(
    folder: &Path,
    title: &str,
    seconds: f64,
    extension: &str,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let stem = format!("{} at {}", safe_title(title), timestamp(seconds));
    let mut candidate = folder.join(format!("{stem}.{extension}"));
    let mut number = 2;
    while exists(&candidate) {
        candidate = folder.join(format!("{stem} {number}.{extension}"));
        number += 1;
    }
    candidate
}

/// Whether `bytes` are a whole PNG file: libmpv writes the file while Flick
/// waits to copy it, and its last chunk is IEND.
pub fn complete_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.ends_with(&[0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82])
}

/// The title without characters that file systems refuse or that would
/// make a path, at most 80 characters.
fn safe_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                ' '
            } else {
                c
            }
        })
        .take(80)
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    // A leading dot would hide the file on macOS and Linux.
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "Flick".to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// `12.34`, or `1.02.03` past an hour; colons are not allowed in file names.
fn timestamp(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}.{minutes:02}.{seconds:02}")
    } else {
        format!("{minutes}.{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_title_and_time() {
        let folder = Path::new("/shots");
        assert_eq!(
            path(folder, "Sintel", 754.6, "png", |_| false),
            folder.join("Sintel at 12.34.png")
        );
        assert_eq!(
            path(folder, "Film", 3723., "png", |_| false),
            folder.join("Film at 1.02.03.png")
        );
        assert_eq!(
            path(folder, "Film", f64::NAN, "png", |_| false),
            folder.join("Film at 0.00.png")
        );
    }

    #[test]
    fn unsafe_characters_and_empty_titles_are_replaced() {
        let folder = Path::new("/shots");
        assert_eq!(
            path(folder, "../a/b: c?\n", 5., "png", |_| false),
            folder.join("a b c at 0.05.png")
        );
        assert_eq!(
            path(folder, "  ", 5., "png", |_| false),
            folder.join("Flick at 0.05.png")
        );
        let long = "x".repeat(200);
        let name = path(folder, &long, 5., "png", |_| false);
        assert_eq!(
            name.file_name().unwrap().to_string_lossy().len(),
            80 + " at 0.05.png".len()
        );
    }

    #[test]
    fn whole_pngs_end_with_iend() {
        let mut png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        assert!(!complete_png(&png));
        png.extend([0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]);
        assert!(complete_png(&png));
        assert!(!complete_png(b"IEND"));
        assert_eq!(
            path(Path::new("/s"), "Film", 5., "jpg", |_| false),
            Path::new("/s").join("Film at 0.05.jpg")
        );
    }

    #[test]
    fn taken_names_are_numbered() {
        let folder = Path::new("/shots");
        let taken = [
            folder.join("Film at 0.05.png"),
            folder.join("Film at 0.05 2.png"),
        ];
        assert_eq!(
            path(folder, "Film", 5., "png", |p| taken.iter().any(|t| t == p)),
            folder.join("Film at 0.05 3.png")
        );
    }
}
