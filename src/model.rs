use std::path::Path;

/// Format playback time, omitting hours until needed and ignoring fractions.
pub fn format_time(seconds: f64) -> String {
    let seconds = non_negative_finite(seconds).floor() as u64;
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Clamp seeks to the media bounds when duration is known.
pub fn seek_target(current: f64, delta: f64, duration: f64) -> f64 {
    let current = non_negative_finite(current);
    let delta = if delta.is_finite() { delta } else { 0.0 };
    let target = (current + delta).max(0.0);
    if duration.is_finite() && duration > 0.0 {
        target.min(duration)
    } else if target.is_finite() {
        target
    } else {
        current
    }
}

pub fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let title = path.to_string_lossy();
            if title.is_empty() {
                "Untitled video".to_owned()
            } else {
                title.into_owned()
            }
        })
}

pub(crate) fn non_negative_finite(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting_handles_boundaries_and_invalid_input() {
        for (seconds, expected) in [
            (0.0, "0:00"),
            (59.99, "0:59"),
            (60.0, "1:00"),
            (3599.9, "59:59"),
            (3600.0, "1:00:00"),
            (3661.0, "1:01:01"),
            (360_000.0, "100:00:00"),
            (-1.0, "0:00"),
            (f64::NAN, "0:00"),
            (f64::INFINITY, "0:00"),
        ] {
            assert_eq!(format_time(seconds), expected);
        }
    }

    #[test]
    fn seeks_stay_in_bounds_and_handle_unknown_duration() {
        assert_eq!(seek_target(5.0, -10.0, 100.0), 0.0);
        assert_eq!(seek_target(95.0, 10.0, 100.0), 100.0);
        assert_eq!(seek_target(25.0, 10.0, 100.0), 35.0);
        assert_eq!(seek_target(25.0, 10.0, 0.0), 35.0);
        assert_eq!(seek_target(f64::NAN, 10.0, 100.0), 10.0);
        assert_eq!(seek_target(25.0, f64::INFINITY, 100.0), 25.0);
        assert_eq!(seek_target(f64::MAX, f64::MAX, 100.0), 100.0);
        assert!(seek_target(f64::MAX, f64::MAX, f64::NAN).is_finite());
    }

    #[test]
    fn title_uses_file_name_with_a_readable_fallback() {
        assert_eq!(file_title(Path::new("movies/Arrival.mkv")), "Arrival.mkv");
        assert_eq!(file_title(Path::new("")), "Untitled video");
    }
}
