//! Player preferences from Settings › Player and Advanced: seeking, volume,
//! the audio equalizer, screenshots, the controls' layout, the queue, and
//! extra mpv options and filters. Each becomes mpv properties; nothing here
//! talks to libmpv directly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Seek steps offered for the arrow keys, in seconds.
pub const SEEK_STEPS: [u32; 4] = [5, 10, 15, 30];
/// Highest volumes offered, in percent; mpv's `volume-max`.
pub const MAX_VOLUMES: [u32; 3] = [100, 150, 200];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayGain {
    #[default]
    Off,
    Track,
    Album,
}

impl ReplayGain {
    pub fn mpv(self) -> &'static str {
        match self {
            Self::Off => "no",
            Self::Track => "track",
            Self::Album => "album",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg,
}

impl ScreenshotFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

/// Which player controls to show, independent of the interface style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlsChoice {
    /// The interface style's own controls.
    #[default]
    Style,
    /// A floating panel, as on macOS.
    Floating,
    /// A bar along the bottom, as on Windows.
    Bar,
    /// One compact row, as on GNOME.
    Compact,
}

/// Equalizer band centers in Hz, an octave apart.
pub const EQ_BANDS: [u32; 10] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
/// Band gain limits and step, in dB.
pub const EQ_LIMIT: i32 = 12;
pub const EQ_STEP: i32 = 2;

/// Built-in equalizer settings, by name.
pub const EQ_PRESETS: [(&str, [i32; 10]); 7] = [
    ("Flat", [0; 10]),
    ("Bass Boost", [6, 6, 4, 2, 0, 0, 0, 0, 0, 0]),
    ("Treble Boost", [0, 0, 0, 0, 0, 0, 2, 4, 6, 6]),
    ("Voice", [-4, -2, 0, 2, 4, 4, 4, 2, 0, -2]),
    ("Loudness", [6, 4, 0, 0, -2, 0, -2, 0, 4, 2]),
    ("Rock", [4, 4, 2, 0, -2, -2, 0, 2, 4, 4]),
    ("Classical", [4, 2, 2, 0, 0, 0, -2, -2, -2, -4]),
];

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PlayerPreferences {
    /// Seconds the arrow keys and the back and forward buttons move.
    pub seek_step: u32,
    /// Seek to the exact frame; otherwise to the nearest keyframe, which is
    /// faster on files with few keyframes.
    pub exact_seek: bool,
    /// mpv's `volume-max`: 100 keeps the volume at or below the file's level.
    pub max_volume: u32,
    pub replaygain: ReplayGain,
    /// macOS: mpv's AVFoundation audio output, which supports Spatial Audio.
    pub spatial_audio: bool,
    /// Gains in dB for [`EQ_BANDS`].
    pub equalizer: [i32; 10],
    pub screenshot_format: ScreenshotFormat,
    /// Draw subtitles into screenshots.
    pub screenshot_subtitles: bool,
    /// Where screenshots go; None is Pictures/Flick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_folder: Option<PathBuf>,
    pub controls: ControlsChoice,
    /// Keep the window above others while a video plays, as IINA can.
    pub float_while_playing: bool,
    /// Queue the other videos in a video's folder, as IINA's playlist does.
    pub queue_folder: bool,
    /// Added to mpv's `vf` after Flick's own picture adjustments.
    pub video_filters: String,
    /// Added to mpv's `af` after the equalizer.
    pub audio_filters: String,
    /// More mpv options as `name=value`, separated by semicolons or lines;
    /// a value in double quotes may contain semicolons.
    pub mpv_options: String,
}

impl Default for PlayerPreferences {
    fn default() -> Self {
        Self {
            seek_step: 10,
            exact_seek: true,
            max_volume: 100,
            replaygain: ReplayGain::Off,
            spatial_audio: false,
            equalizer: [0; 10],
            screenshot_format: ScreenshotFormat::Png,
            screenshot_subtitles: true,
            screenshot_folder: None,
            controls: ControlsChoice::Style,
            float_while_playing: false,
            queue_folder: true,
            video_filters: String::new(),
            audio_filters: String::new(),
            mpv_options: String::new(),
        }
    }
}

impl PlayerPreferences {
    /// Keeps hand-edited values within what the interface offers.
    pub fn sanitize(&mut self) {
        if !SEEK_STEPS.contains(&self.seek_step) {
            self.seek_step = 10;
        }
        // The nearest choice Settings offers.
        self.max_volume = MAX_VOLUMES
            .into_iter()
            .min_by_key(|choice| choice.abs_diff(self.max_volume))
            .unwrap_or(100);
        for gain in &mut self.equalizer {
            *gain = (*gain).clamp(-EQ_LIMIT, EQ_LIMIT);
        }
    }

    /// The seek flags for the arrow keys.
    pub fn seek_flags(&self) -> &'static str {
        if self.exact_seek {
            "absolute+exact"
        } else {
            "absolute+keyframes"
        }
    }

    /// The named preset these gains match, if any.
    pub fn equalizer_preset(&self) -> Option<&'static str> {
        EQ_PRESETS
            .iter()
            .find(|(_, gains)| *gains == self.equalizer)
            .map(|(name, _)| *name)
    }

    /// Moves one band by `steps` of [`EQ_STEP`], within ±[`EQ_LIMIT`].
    pub fn step_band(&mut self, band: usize, steps: i32) {
        if let Some(gain) = self.equalizer.get_mut(band) {
            *gain = (*gain + steps * EQ_STEP).clamp(-EQ_LIMIT, EQ_LIMIT);
        }
    }

    /// mpv's `af`: one FFmpeg `equalizer` per band that is not flat, then the
    /// user's own filters. Boosted bands lower the overall level by the
    /// largest boost, so the equalizer cannot clip.
    pub fn audio_filter_chain(&self) -> String {
        let mut filters = Vec::new();
        let boost = self.equalizer.iter().copied().max().unwrap_or(0).max(0);
        if boost > 0 {
            filters.push(format!("volume=-{boost}dB"));
        }
        for (frequency, gain) in EQ_BANDS.iter().zip(self.equalizer) {
            if gain != 0 {
                filters.push(format!("equalizer=f={frequency}:t=o:w=1:g={gain}"));
            }
        }
        let custom = self.audio_filters.trim();
        if !custom.is_empty() {
            filters.push(custom.to_owned());
        }
        filters.join(",")
    }

    /// Flick's picture filters followed by the user's own.
    pub fn video_filter_chain(&self, picture: &str) -> String {
        [picture, self.video_filters.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// mpv properties for these preferences, applied at startup and when
    /// they change. The filter chains are set separately with the picture
    /// adjustments.
    pub fn properties(&self) -> Vec<(&'static str, String)> {
        vec![
            ("volume-max", self.max_volume.to_string()),
            ("replaygain", self.replaygain.mpv().to_owned()),
        ]
    }

    /// mpv's `ao` for Spatial Audio on macOS, falling back to Core Audio
    /// where this libmpv lacks AVFoundation output. None leaves mpv's own
    /// choice, or `FLICK_AUDIO=null`, alone.
    pub fn audio_output(&self) -> Option<&'static str> {
        (cfg!(target_os = "macos") && self.spatial_audio).then_some("avfoundation,coreaudio")
    }

    /// The extra mpv options as `(name, value)` pairs. An entry with no `=`
    /// turns a flag on; blank entries and `#` comments are skipped. Entries
    /// whose name is not a plain option name are returned as errors.
    pub fn extra_options(&self) -> (Vec<(String, String)>, Vec<String>) {
        let mut options = Vec::new();
        let mut errors = Vec::new();
        for line in split_options(&self.mpv_options) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("--").unwrap_or(line);
            let (name, value) = line.split_once('=').unwrap_or((line, "yes"));
            let name = name.trim();
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | '_'))
            {
                errors.push(line.to_owned());
            } else {
                options.push((name.to_owned(), value.to_owned()));
            }
        }
        (options, errors)
    }
}

/// Splits at line breaks, and at semicolons outside double quotes and
/// brackets, so `af=lavfi=[a;b]` and `sub-file-paths="a;b"` stay whole.
fn split_options(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut depth, mut quoted) = (0, 0usize, false);
    for (index, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth = depth.saturating_sub(1),
            '\n' => {
                parts.push(&text[start..index]);
                start = index + 1;
                (depth, quoted) = (0, false);
            }
            ';' if !quoted && depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_flicks_earlier_behavior() {
        let preferences = PlayerPreferences::default();
        assert_eq!(preferences.seek_flags(), "absolute+exact");
        assert_eq!(preferences.audio_filter_chain(), "");
        assert_eq!(preferences.video_filter_chain(""), "");
        assert_eq!(preferences.equalizer_preset(), Some("Flat"));
        let json = serde_json::to_string(&preferences).unwrap();
        let loaded: PlayerPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, preferences);
        // Settings saved before these existed load as defaults.
        let old: PlayerPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(old, preferences);
    }

    #[test]
    fn hand_edited_values_are_brought_into_range() {
        let mut preferences: PlayerPreferences = serde_json::from_str(
            r#"{"seek_step":7,"max_volume":900,"equalizer":[40,0,0,0,0,0,0,0,0,-40]}"#,
        )
        .unwrap();
        preferences.sanitize();
        assert_eq!(preferences.seek_step, 10);
        assert_eq!(preferences.max_volume, 200);
        let mut between: PlayerPreferences = serde_json::from_str(r#"{"max_volume":120}"#).unwrap();
        between.sanitize();
        assert_eq!(between.max_volume, 100);
        assert_eq!(preferences.equalizer[0], EQ_LIMIT);
        assert_eq!(preferences.equalizer[9], -EQ_LIMIT);
    }

    #[test]
    fn the_equalizer_leaves_flat_bands_out_and_makes_room_for_boosts() {
        let mut preferences = PlayerPreferences::default();
        preferences.step_band(0, 3);
        preferences.step_band(9, -1);
        preferences.step_band(42, 1);
        preferences.audio_filters = " loudnorm ".into();
        assert_eq!(
            preferences.audio_filter_chain(),
            "volume=-6dB,equalizer=f=31:t=o:w=1:g=6,equalizer=f=16000:t=o:w=1:g=-2,loudnorm"
        );
        preferences.step_band(0, 10);
        assert_eq!(preferences.equalizer[0], EQ_LIMIT);
        preferences.equalizer = EQ_PRESETS[1].1;
        assert_eq!(preferences.equalizer_preset(), Some("Bass Boost"));
    }

    #[test]
    fn video_filters_follow_the_picture_adjustments() {
        let mut preferences = PlayerPreferences::default();
        assert_eq!(preferences.video_filter_chain("hflip"), "hflip");
        preferences.video_filters = "gradfun".into();
        assert_eq!(preferences.video_filter_chain("hflip"), "hflip,gradfun");
        assert_eq!(preferences.video_filter_chain(""), "gradfun");
    }

    #[test]
    fn extra_options_parse_one_a_line() {
        let preferences = PlayerPreferences {
            mpv_options: "# comment\n--sub-font=Avenir Next;\ninterpolation; bad name=1;=x\n"
                .into(),
            ..Default::default()
        };
        let (options, errors) = preferences.extra_options();
        assert_eq!(
            options,
            [
                ("sub-font".to_owned(), "Avenir Next".to_owned()),
                ("interpolation".to_owned(), "yes".to_owned()),
            ]
        );
        assert_eq!(errors, ["bad name=1", "=x"]);
        let quoted = PlayerPreferences {
            mpv_options: r#"sub-file-paths="subs;Subs"; af=lavfi=[aformat=s16;anull]; pause"#
                .into(),
            ..Default::default()
        };
        assert_eq!(
            quoted.extra_options().0,
            [
                ("sub-file-paths".to_owned(), "subs;Subs".to_owned()),
                ("af".to_owned(), "lavfi=[aformat=s16;anull]".to_owned()),
                ("pause".to_owned(), "yes".to_owned()),
            ]
        );
    }
}
