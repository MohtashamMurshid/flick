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

/// A time typed as `m:ss` or `h:mm:ss`, such as `12:34` or `1:02:03`, in
/// seconds. Plain numbers are not times, so searches for them still work.
pub fn parse_time(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut seconds = 0u64;
    for (index, part) in parts.iter().enumerate() {
        let leading = index == 0;
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (!leading && part.len() != 2)
            || part.len() > 6
        {
            return None;
        }
        let value: u64 = part.parse().ok()?;
        if !leading && value >= 60 {
            return None;
        }
        seconds = seconds * 60 + value;
    }
    Some(seconds as f64)
}

/// Pixels of scrolling per percent of volume or second of seeking.
pub const SCROLL_UNIT: f32 = 5.;
/// Pixels counted for one line, or mouse-wheel notch: five units.
pub const SCROLL_LINE: f32 = 25.;

/// Whole units in `pending + delta` pixels, and the pixels left over for
/// the next event, so slow trackpad scrolling still adds up.
pub fn scroll_units(pending: f32, delta: f32) -> (i32, f32) {
    let total = pending + delta;
    if !total.is_finite() {
        return (0, 0.);
    }
    let units = (total / SCROLL_UNIT).trunc();
    (units as i32, total - units * SCROLL_UNIT)
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
    fn typed_times_need_a_colon_and_valid_fields() {
        assert_eq!(parse_time("12:34"), Some(754.));
        assert_eq!(parse_time(" 0:05 "), Some(5.));
        assert_eq!(parse_time("1:02:03"), Some(3723.));
        assert_eq!(parse_time("90:00"), Some(5400.));
        for text in [
            "45", "1:2", "1:60", "1:02:60", ":30", "1:", "a:30", "1:2:3:4", "-1:00", "1:3x", "",
        ] {
            assert_eq!(parse_time(text), None, "{text}");
        }
    }

    #[test]
    fn scrolling_adds_up_whole_units_and_keeps_the_rest() {
        assert_eq!(scroll_units(0., 12.), (2, 2.));
        assert_eq!(scroll_units(2., 3.), (1, 0.));
        assert_eq!(scroll_units(2., -4.), (0, -2.));
        assert_eq!(scroll_units(-2., -9.), (-2, -1.));
        assert_eq!(scroll_units(0., SCROLL_LINE), (5, 0.));
        assert_eq!(scroll_units(0., f32::NAN), (0, 0.));
    }

    #[test]
    fn title_uses_file_name_with_a_readable_fallback() {
        assert_eq!(file_title(Path::new("movies/Arrival.mkv")), "Arrival.mkv");
        assert_eq!(file_title(Path::new("")), "Untitled video");
    }
}
