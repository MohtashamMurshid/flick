//! Picture adjustments for the playing video, as in IINA's Video panel:
//! aspect ratio, crop, rotation, flip and mirror, deinterlacing, and a
//! color equalizer. They last until another video opens.
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
//!
//! Worse, the software renderer aborts the process when a file's rotation
//! metadata reaches it, in `mp_image_crop`, because mpv sizes the picture
//! as turned while the frame is not. Every player therefore runs with
//! `video-rotate=no`, and the file's rotation from `demux-rotation` joins
//! the user's turns in the same filter.

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

/// Crops offered, as width-to-height ratios of the picture shown. The first
/// keeps the whole picture.
pub const CROPS: [(&str, Option<f64>); 6] = [
    ("None", None),
    ("16:9", Some(16. / 9.)),
    ("4:3", Some(4. / 3.)),
    ("2.35:1", Some(2.35)),
    ("1:1", Some(1.)),
    ("9:16", Some(9. / 16.)),
];

/// The filter that turns a picture `degrees` clockwise; None for none.
pub fn rotation_filter(degrees: u16) -> Option<&'static str> {
    match degrees % 360 {
        90 => Some("transpose=clock"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=cclock"),
        _ => None,
    }
}

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
    /// An index into [`CROPS`].
    pub crop: usize,
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

    /// The `vf` filter chain for a file whose metadata asks for
    /// `file_rotation` clockwise degrees; empty when nothing needs a filter.
    pub fn filters(&self, file_rotation: u16) -> String {
        let mut filters = Vec::new();
        // Turn first, so crop, flip, and mirror act on the picture as shown.
        if let Some(turn) = rotation_filter(file_rotation + self.rotation) {
            filters.push(turn.to_owned());
        }
        if let Some((_, Some(ratio))) = CROPS.get(self.crop) {
            // The largest centered area of that shape; brackets keep the
            // commas inside FFmpeg's expressions out of mpv's filter list.
            let ratio = number(*ratio);
            filters.push(format!(
                "crop=w=[min(iw,ih*{ratio})]:h=[min(ih,iw/{ratio})]"
            ));
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
    pub fn properties(&self, file_rotation: u16) -> [(&'static str, String); 3] {
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
            ("vf", self.filters(file_rotation)),
        ]
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
        assert_eq!(adjust.filters(0), "");
        assert_eq!(
            adjust.properties(0),
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
            adjust.filters(0),
            "vflip,hflip,eq=brightness=0.05:contrast=2:saturation=1:gamma=0.707"
        );
        adjust.step_color(Color::Hue, -11);
        assert_eq!(adjust.color(Color::Hue), -100);
        assert!(adjust.filters(0).ends_with(",hue=h=-180"));
        for color in Color::ALL {
            let value = adjust.color(color);
            adjust.step_color(color, -value / COLOR_STEP);
        }
        assert_eq!(adjust.filters(0), "vflip,hflip");
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
                (adjust.rotation, adjust.filters(0))
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
    fn the_files_rotation_adds_to_the_users() {
        let mut adjust = VideoAdjust::default();
        assert_eq!(adjust.filters(270), "transpose=cclock");
        adjust.rotate();
        // 270 + 90 is upright again.
        assert_eq!(adjust.filters(270), "");
        adjust.rotate();
        assert_eq!(adjust.filters(270), "transpose=clock");
        assert_eq!(rotation_filter(450), Some("transpose=clock"));
        assert_eq!(rotation_filter(45), None);
    }

    #[test]
    fn crops_follow_the_turn_and_quote_their_expressions() {
        let mut adjust = VideoAdjust {
            crop: 2,
            ..Default::default()
        };
        assert_eq!(
            adjust.filters(90),
            "transpose=clock,crop=w=[min(iw,ih*1.333)]:h=[min(ih,iw/1.333)]"
        );
        adjust.crop = 0;
        assert_eq!(adjust.filters(0), "");
    }

    #[test]
    fn numbers_drop_trailing_zeros() {
        assert_eq!(number(1.0), "1");
        assert_eq!(number(-0.0001), "0");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(-180.), "-180");
    }
}
