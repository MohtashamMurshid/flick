//! Picture adjustments for the playing video, as in IINA's Video panel:
//! aspect ratio, rotation, flip and mirror, deinterlacing, and a color
//! equalizer. They last until another video opens.
//!
//! Flick draws video through libmpv's software renderer
//! (`video/out/libmpv_sw.c`), which has no color equalizer and does not
//! rotate. mpv's GPU output applies `brightness`, `contrast`, and the other
//! equalizer properties itself, and `vo_libmpv` declares `VO_CAP_ROTATE90`,
//! so mpv never inserts its own rotation filter, and `video-rotate` changes
//! nothing on screen here. Rotation, flip, mirror, and the equalizer are
//! therefore FFmpeg filters in mpv's `vf` chain, which work with any output
//! and with copy-back hardware decoding. They cost CPU time on large videos
//! while they are on. The aspect ratio and deinterlacing are mpv properties
//! that need no help from the output.

/// Aspect ratios offered, with their `video-aspect-override` values. The
/// first keeps the video's own.
pub const ASPECTS: [(&str, &str); 6] = [
    ("Default", "-1"),
    ("16:9", "16:9"),
    ("4:3", "4:3"),
    ("16:10", "16:10"),
    ("2.35:1", "2.35"),
    ("1:1", "1"),
];

/// Equalizer steps, in the −100 to 100 range of each color setting.
pub const COLOR_STEP: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Brightness,
    Contrast,
    Saturation,
    Gamma,
    Hue,
}

impl Color {
    pub const ALL: [Self; 5] = [
        Self::Brightness,
        Self::Contrast,
        Self::Saturation,
        Self::Gamma,
        Self::Hue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Brightness => "Brightness",
            Self::Contrast => "Contrast",
            Self::Saturation => "Saturation",
            Self::Gamma => "Gamma",
            Self::Hue => "Hue",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoAdjust {
    /// An index into [`ASPECTS`].
    pub aspect: usize,
    /// Clockwise degrees: 0, 90, 180, or 270.
    pub rotation: u16,
    /// Upside down.
    pub flip: bool,
    /// Left to right.
    pub mirror: bool,
    pub deinterlace: bool,
    /// Each from −100 to 100; 0 leaves the picture unchanged.
    brightness: i32,
    contrast: i32,
    saturation: i32,
    gamma: i32,
    hue: i32,
}

impl VideoAdjust {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn color(&self, color: Color) -> i32 {
        match color {
            Color::Brightness => self.brightness,
            Color::Contrast => self.contrast,
            Color::Saturation => self.saturation,
            Color::Gamma => self.gamma,
            Color::Hue => self.hue,
        }
    }

    /// Moves a color setting by `steps` of [`COLOR_STEP`], within −100 to 100.
    pub fn step_color(&mut self, color: Color, steps: i32) {
        let value = (self.color(color) + steps * COLOR_STEP).clamp(-100, 100);
        *match color {
            Color::Brightness => &mut self.brightness,
            Color::Contrast => &mut self.contrast,
            Color::Saturation => &mut self.saturation,
            Color::Gamma => &mut self.gamma,
            Color::Hue => &mut self.hue,
        } = value;
    }

    /// A quarter turn clockwise.
    pub fn rotate(&mut self) {
        self.rotation = (self.rotation + 90) % 360;
    }

    /// The `vf` filter chain; empty when nothing needs a filter.
    pub fn filters(&self) -> String {
        let mut filters = Vec::new();
        // Turn first, so flip and mirror act on the picture as it is shown.
        match self.rotation {
            90 => filters.push("transpose=clock".to_owned()),
            180 => filters.push("hflip,vflip".to_owned()),
            270 => filters.push("transpose=cclock".to_owned()),
            _ => {}
        }
        if self.flip {
            filters.push("vflip".to_owned());
        }
        if self.mirror {
            filters.push("hflip".to_owned());
        }
        let (b, c, s, g) = (self.brightness, self.contrast, self.saturation, self.gamma);
        if b != 0 || c != 0 || s != 0 || g != 0 {
            // FFmpeg's eq: brightness −1 to 1 around 0, contrast and
            // saturation around 1, gamma 0.1 to 10 around 1. Half its
            // brightness range, doubling of contrast and saturation, and a
            // factor of two in gamma keep each end of the range usable.
            filters.push(format!(
                "eq=brightness={}:contrast={}:saturation={}:gamma={}",
                number(f64::from(b) / 200.),
                number(1. + f64::from(c) / 100.),
                number(1. + f64::from(s) / 100.),
                number(2f64.powf(f64::from(g) / 100.)),
            ));
        }
        if self.hue != 0 {
            // ±100 turns the hue up to half the color wheel either way.
            filters.push(format!("hue=h={}", number(f64::from(self.hue) * 1.8)));
        }
        filters.join(",")
    }

    /// The mpv properties that carry these adjustments, with their values.
    pub fn properties(&self) -> [(&'static str, String); 3] {
        [
            (
                "video-aspect-override",
                ASPECTS
                    .get(self.aspect)
                    .map_or(ASPECTS[0].1, |(_, value)| value)
                    .to_owned(),
            ),
            (
                "deinterlace",
                if self.deinterlace { "yes" } else { "no" }.to_owned(),
            ),
            ("vf", self.filters()),
        ]
    }

    /// The properties whose values differ from `before`, to send to mpv.
    pub fn changes_from(&self, before: &Self) -> Vec<(&'static str, String)> {
        let old = before.properties();
        self.properties()
            .into_iter()
            .zip(old)
            .filter(|(new, old)| new.1 != old.1)
            .map(|(new, _)| new)
            .collect()
    }
}

/// A filter argument with at most three decimals and no trailing zeros.
fn number(value: f64) -> String {
    let text = format!("{value:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0" } else { text }.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_adjustments_need_no_filters_and_restore_mpv_defaults() {
        let adjust = VideoAdjust::default();
        assert!(adjust.is_default());
        assert_eq!(adjust.filters(), "");
        assert_eq!(
            adjust.properties(),
            [
                ("video-aspect-override", "-1".to_owned()),
                ("deinterlace", "no".to_owned()),
                ("vf", String::new()),
            ]
        );
    }

    #[test]
    fn colors_step_within_their_range_and_build_one_filter_chain() {
        let mut adjust = VideoAdjust::default();
        adjust.step_color(Color::Brightness, 1);
        adjust.step_color(Color::Contrast, 25);
        adjust.step_color(Color::Gamma, -5);
        adjust.flip = true;
        adjust.mirror = true;
        assert_eq!(adjust.color(Color::Contrast), 100);
        assert_eq!(
            adjust.filters(),
            "vflip,hflip,eq=brightness=0.05:contrast=2:saturation=1:gamma=0.707"
        );
        adjust.step_color(Color::Hue, -11);
        assert_eq!(adjust.color(Color::Hue), -100);
        assert!(adjust.filters().ends_with(",hue=h=-180"));
        for color in Color::ALL {
            let value = adjust.color(color);
            adjust.step_color(color, -value / COLOR_STEP);
        }
        assert_eq!(adjust.filters(), "vflip,hflip");
    }

    #[test]
    fn rotation_turns_a_quarter_at_a_time_before_flipping() {
        let mut adjust = VideoAdjust {
            mirror: true,
            ..Default::default()
        };
        let turns: Vec<_> = (0..4)
            .map(|_| {
                adjust.rotate();
                (adjust.rotation, adjust.filters())
            })
            .collect();
        assert_eq!(
            turns,
            [
                (90, "transpose=clock,hflip".to_owned()),
                (180, "hflip,vflip,hflip".to_owned()),
                (270, "transpose=cclock,hflip".to_owned()),
                (0, "hflip".to_owned()),
            ]
        );
    }

    #[test]
    fn only_changed_properties_are_sent() {
        let before = VideoAdjust::default();
        let mut after = before.clone();
        after.aspect = 2;
        after.deinterlace = true;
        assert_eq!(
            after.changes_from(&before),
            [
                ("video-aspect-override", "4:3".to_owned()),
                ("deinterlace", "yes".to_owned()),
            ]
        );
        assert!(after.changes_from(&after).is_empty());
        // Resetting sends each changed property back to its default.
        assert_eq!(VideoAdjust::default().changes_from(&after).len(), 2);
    }

    #[test]
    fn numbers_drop_trailing_zeros() {
        assert_eq!(number(1.0), "1");
        assert_eq!(number(-0.0001), "0");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(-180.), "-180");
    }
}
