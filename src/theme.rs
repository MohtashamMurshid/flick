//! Interface styles that follow macOS, Windows 11, and GNOME conventions.
//!
//! A style changes window chrome, controls, colors, typography, and metrics.
//! Pages and playback behavior stay the same in every style.

use gpui::{App, Global, Hsla, Pixels, Rgba, SharedString, WindowAppearance, px, rgb, rgba};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StyleChoice {
    #[default]
    System,
    Mac,
    Windows,
    Gnome,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContrastChoice {
    /// Follow the operating system's increased-contrast setting.
    #[default]
    System,
    Standard,
    High,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleBackground {
    /// mpv's default outline and shadow.
    #[default]
    Outline,
    /// A translucent dark box behind each line.
    Box,
}

/// Subtitle appearance, applied through mpv properties.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SubtitleStyle {
    /// Multiple of mpv's default size (`sub-scale`).
    pub scale: f32,
    /// `0xRRGGBB`.
    pub color: u32,
    pub background: SubtitleBackground,
    /// mpv `sub-pos`: 100 is the bottom of the video, lower is higher.
    pub position: u32,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            scale: 1.,
            color: 0xffffff,
            background: SubtitleBackground::Outline,
            position: 100,
        }
    }
}

/// Subtitle colors offered in Appearance.
pub const SUBTITLE_COLORS: [(&str, u32); 4] = [
    ("White", 0xffffff),
    ("Yellow", 0xffe14d),
    ("Cyan", 0x5ce1ff),
    ("Green", 0x7dff7a),
];

pub const SUBTITLE_SCALE: (f32, f32) = (0.5, 2.);
pub const SUBTITLE_POSITION: (u32, u32) = (50, 100);
pub const TEXT_SCALE: (f32, f32) = (0.85, 1.5);

impl SubtitleStyle {
    fn sanitize(&mut self) {
        self.scale = (finite_clamp(self.scale, SUBTITLE_SCALE, 1.) * 100.).round() / 100.;
        self.color &= 0xffffff;
        self.position = self
            .position
            .clamp(SUBTITLE_POSITION.0, SUBTITLE_POSITION.1);
    }

    /// mpv properties for this style. `sub-border-style` exists from mpv
    /// 0.38; callers ignore it where unsupported, and older libmpv versions
    /// draw `sub-back-color` as the shadow color behind the outline.
    pub fn properties(&self) -> Vec<(&'static str, String)> {
        let boxed = self.background == SubtitleBackground::Box;
        vec![
            ("sub-scale", format!("{:.2}", self.scale)),
            ("sub-color", format!("#{:06X}", self.color)),
            (
                "sub-border-style",
                if boxed {
                    "background-box"
                } else {
                    "outline-and-shadow"
                }
                .into(),
            ),
            (
                "sub-back-color",
                if boxed { "#B3000000" } else { "#00000000" }.into(),
            ),
            ("sub-shadow-offset", if boxed { "4" } else { "0" }.into()),
        ]
    }
}

fn finite_clamp(value: f32, (min, max): (f32, f32), fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Mac,
    Windows,
    Gnome,
}

impl Style {
    pub fn native() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Gnome
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mac => "macOS",
            Self::Windows => "Windows",
            Self::Gnome => "GNOME",
        }
    }
}

impl StyleChoice {
    pub fn resolve(self) -> Style {
        match self {
            Self::System => Style::native(),
            Self::Mac => Style::Mac,
            Self::Windows => Style::Windows,
            Self::Gnome => Style::Gnome,
        }
    }
}

/// Persisted appearance preferences.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Appearance {
    pub style: StyleChoice,
    pub scheme: SchemeChoice,
    /// `0xRRGGBB`, or `None` for the style's own accent.
    pub accent: Option<u32>,
    pub autohide_controls: bool,
    /// Start the next episode of a show when one ends.
    pub autoplay_next: bool,
    /// Multiple of each style's text sizes.
    pub text_scale: f32,
    pub contrast: ContrastChoice,
    /// Show text beside toolbar icons instead of only in tooltips.
    pub show_labels: bool,
    pub subtitles: SubtitleStyle,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            style: StyleChoice::System,
            scheme: SchemeChoice::System,
            accent: None,
            autohide_controls: true,
            autoplay_next: true,
            text_scale: 1.,
            contrast: ContrastChoice::System,
            show_labels: false,
            subtitles: SubtitleStyle::default(),
        }
    }
}

impl Appearance {
    pub fn sanitize(&mut self) {
        // Sliders step in hundredths; drop accumulated float noise.
        self.text_scale = (finite_clamp(self.text_scale, TEXT_SCALE, 1.) * 100.).round() / 100.;
        self.accent = self.accent.map(|accent| accent & 0xffffff);
        self.subtitles.sanitize();
    }
}

/// Accessibility settings read from the operating system. GPUI 0.2.2 does
/// not expose them, so Flick reads each platform's setting itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemPreferences {
    pub high_contrast: bool,
}

impl Global for SystemPreferences {}

impl SystemPreferences {
    /// Reads the platform setting with a bounded wait. Unknown means off.
    pub fn detect() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(Self {
                high_contrast: system_high_contrast(),
            });
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_default()
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn system_high_contrast() -> bool {
    if cfg!(target_os = "macos") {
        command_output(
            "defaults",
            &["read", "com.apple.universalaccess", "increaseContrast"],
        )
        .is_some_and(|value| value == "1")
    } else if cfg!(target_os = "windows") {
        // HCF_HIGHCONTRASTON is bit 0 of the HighContrast flags.
        command_output(
            "reg",
            &[
                "query",
                r"HKCU\Control Panel\Accessibility\HighContrast",
                "/v",
                "Flags",
            ],
        )
        .and_then(|text| {
            let value = text.split_whitespace().last()?.to_owned();
            value
                .trim()
                .parse::<u32>()
                .ok()
                .or_else(|| u32::from_str_radix(value.trim_start_matches("0x"), 16).ok())
        })
        .is_some_and(|flags| flags & 1 == 1)
    } else {
        command_output(
            "gsettings",
            &["get", "org.gnome.desktop.a11y.interface", "high-contrast"],
        )
        .is_some_and(|value| value == "true")
    }
}

/// Accent colors offered in Appearance, in display order.
pub const ACCENTS: [(&str, u32); 8] = [
    ("Blue", 0x2f7cf6),
    ("Purple", 0x9a5cf0),
    ("Pink", 0xe2508f),
    ("Red", 0xe5484d),
    ("Orange", 0xf0802b),
    ("Yellow", 0xd9a400),
    ("Green", 0x3fa34d),
    ("Graphite", 0x8a8a8f),
];

/// Interface fonts available on this machine for each style.
pub struct Fonts {
    mac: SharedString,
    windows: SharedString,
    gnome: SharedString,
}

impl Global for Fonts {}

impl Fonts {
    /// Native styles use the platform UI font. Other styles use the closest
    /// installed family and fall back to the bundled Inter.
    pub fn detect(cx: &App) -> Self {
        let names = cx
            .text_system()
            .all_font_names()
            .into_iter()
            .collect::<HashSet<_>>();
        let pick = |native: bool, families: &[&str]| -> SharedString {
            if native {
                return ".SystemUIFont".into();
            }
            families
                .iter()
                .find(|family| names.contains(**family))
                .map_or_else(|| "Inter".into(), |family| (*family).to_owned().into())
        };
        Self {
            mac: pick(cfg!(target_os = "macos"), &["SF Pro Text", "SF Pro"]),
            windows: pick(
                cfg!(target_os = "windows"),
                &["Segoe UI Variable Text", "Segoe UI"],
            ),
            gnome: pick(false, &["Adwaita Sans", "Cantarell"]),
        }
    }
}

#[derive(Clone)]
pub struct Theme {
    pub style: Style,
    pub dark: bool,
    pub font: SharedString,
    /// Body text size before [`Theme::font`] applies the text scale.
    pub text_size: f32,
    /// The user's text size, as a multiple of each style's sizes.
    pub text_scale: f32,
    pub high_contrast: bool,
    /// Page content background.
    pub window: Rgba,
    /// Title bar and Windows Mica backdrop.
    pub chrome: Rgba,
    pub sidebar: Rgba,
    /// Cards and grouped lists.
    pub surface: Rgba,
    /// Menus, popovers, and tooltips.
    pub popover: Rgba,
    pub control: Rgba,
    pub control_hover: Rgba,
    pub selection: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    /// Fills behind [`Theme::on_accent`] labels, selection marks, and bars.
    pub accent: Rgba,
    pub on_accent: Rgba,
    /// The accent used for text, such as links.
    pub accent_text: Rgba,
    /// The keyboard focus ring.
    pub focus_ring: Rgba,
    pub danger: Rgba,
    pub radius: f32,
    pub card_radius: f32,
    pub window_radius: f32,
    pub titlebar_height: f32,
    pub sidebar_width: f32,
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Resolves preferences against the system appearance and installs the theme.
pub fn apply(cx: &mut App, appearance: &Appearance) {
    if !cx.has_global::<Fonts>() {
        let fonts = Fonts::detect(cx);
        cx.set_global(fonts);
    }
    let dark = match appearance.scheme {
        SchemeChoice::Light => false,
        SchemeChoice::Dark => true,
        SchemeChoice::System => matches!(
            cx.window_appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    let mut theme = Theme::new(
        appearance.style.resolve(),
        dark,
        appearance.accent,
        cx.global::<Fonts>(),
    );
    theme.text_scale = appearance.text_scale;
    // Labels in the sidebar keep fitting at larger text sizes.
    theme.sidebar_width *= appearance.text_scale.max(1.);
    let high = match appearance.contrast {
        ContrastChoice::High => true,
        ContrastChoice::Standard => false,
        ContrastChoice::System => cx
            .try_global::<SystemPreferences>()
            .is_some_and(|prefs| prefs.high_contrast),
    };
    if high {
        theme.raise_contrast();
    }
    cx.set_global(theme);
}

impl Theme {
    pub fn new(style: Style, dark: bool, accent: Option<u32>, fonts: &Fonts) -> Self {
        let mut theme = match (style, dark) {
            (Style::Mac, true) => Self {
                window: rgb(0x1e1e20),
                chrome: rgb(0x1e1e20),
                sidebar: rgb(0x2b2a30),
                surface: rgba(0xffffff0d),
                popover: rgb(0x2c2c2e),
                control: rgba(0xffffff1a),
                control_hover: rgba(0xffffff26),
                selection: rgba(0xffffff1a),
                border: rgba(0xffffff17),
                text: rgb(0xf5f5f7),
                muted: rgb(0x98989f),
                accent: rgb(0x0a84ff),
                on_accent: rgb(0xffffff),
                ..Self::base(style, dark)
            },
            (Style::Mac, false) => Self {
                window: rgb(0xffffff),
                chrome: rgb(0xffffff),
                sidebar: rgb(0xeeedf2),
                surface: rgba(0x00000008),
                popover: rgb(0xf6f6f6),
                control: rgba(0x0000000d),
                control_hover: rgba(0x00000017),
                selection: rgba(0x00000012),
                border: rgba(0x00000017),
                text: rgb(0x1d1d1f),
                muted: rgb(0x6e6e73),
                accent: rgb(0x007aff),
                on_accent: rgb(0xffffff),
                ..Self::base(style, dark)
            },
            (Style::Windows, true) => Self {
                window: rgb(0x272727),
                chrome: rgb(0x202020),
                sidebar: rgb(0x202020),
                surface: rgba(0xffffff0d),
                popover: rgb(0x2c2c2c),
                control: rgba(0xffffff0f),
                control_hover: rgba(0xffffff17),
                selection: rgba(0xffffff0f),
                border: rgba(0xffffff12),
                text: rgb(0xffffff),
                muted: rgb(0xc5c5c5),
                accent: rgb(0x4cc2ff),
                on_accent: rgb(0x000000),
                ..Self::base(style, dark)
            },
            (Style::Windows, false) => Self {
                window: rgb(0xf9f9f9),
                chrome: rgb(0xeef0f3),
                sidebar: rgb(0xeef0f3),
                surface: rgb(0xffffff),
                popover: rgb(0xf9f9f9),
                control: rgba(0xffffffb3),
                control_hover: rgba(0x0000000a),
                selection: rgba(0x0000000a),
                border: rgba(0x00000014),
                text: rgb(0x1b1b1b),
                muted: rgb(0x5d5d5d),
                accent: rgb(0x005fb8),
                on_accent: rgb(0xffffff),
                ..Self::base(style, dark)
            },
            (Style::Gnome, true) => Self {
                window: rgb(0x222226),
                chrome: rgb(0x222226),
                sidebar: rgb(0x2e2e32),
                surface: rgba(0xffffff14),
                popover: rgb(0x36363a),
                control: rgba(0xffffff1a),
                control_hover: rgba(0xffffff26),
                selection: rgba(0xffffff1a),
                border: rgba(0xffffff14),
                text: rgb(0xffffff),
                muted: rgba(0xffffff8c),
                accent: rgb(0x3584e4),
                on_accent: rgb(0xffffff),
                ..Self::base(style, dark)
            },
            (Style::Gnome, false) => Self {
                window: rgb(0xfafafb),
                chrome: rgb(0xfafafb),
                sidebar: rgb(0xebebed),
                surface: rgb(0xffffff),
                popover: rgb(0xffffff),
                control: rgba(0x00000614),
                control_hover: rgba(0x00000621),
                selection: rgba(0x0000061a),
                border: rgba(0x00000614),
                text: rgba(0x000006cc),
                muted: rgba(0x00000680),
                accent: rgb(0x3584e4),
                on_accent: rgb(0xffffff),
                ..Self::base(style, dark)
            },
        };
        theme.font = match style {
            Style::Mac => fonts.mac.clone(),
            Style::Windows => fonts.windows.clone(),
            Style::Gnome => fonts.gnome.clone(),
        };
        if let Some(accent) = accent {
            theme.accent = rgb(accent);
            theme.on_accent = rgb(if luminance(accent) > 0.55 {
                0x000000
            } else {
                0xffffff
            });
        }
        theme.meet_contrast(TEXT_CONTRAST);
        theme
    }

    /// Adjusts lightness, keeping each hue, until text reaches `text` contrast
    /// on every page background, accent labels reach it on accent fills, and
    /// non-text accents and borders of controls reach WCAG's 3:1.
    fn meet_contrast(&mut self, text: f32) {
        let backgrounds = self.backgrounds();
        self.text = ensure_contrast(self.text, &backgrounds, text);
        self.muted = ensure_contrast(self.muted, &backgrounds, text);
        self.accent_text = ensure_contrast(self.accent, &backgrounds, text);
        // Accent fills carry labels and also mark progress and selection, so
        // they need contrast with both. Flip the label color if one fill
        // cannot satisfy both with the preferred label.
        let other = if relative_luminance(self.on_accent) > 0.5 {
            rgb(0x000000)
        } else {
            rgb(0xffffff)
        };
        let fill_for = |label: Rgba| {
            let fill = ensure_contrast(self.accent, &[label], text);
            let fill = ensure_contrast(fill, &backgrounds, GRAPHIC_CONTRAST);
            (contrast_ratio(label, fill) >= text).then_some((fill, label))
        };
        if let Some((fill, label)) = fill_for(self.on_accent).or_else(|| fill_for(other)) {
            self.accent = fill;
            self.on_accent = label;
        } else {
            self.accent = ensure_contrast(self.accent, &[self.on_accent], text);
        }
        // Fluent draws a high-contrast primary-color focus visual; macOS and
        // libadwaita tint it with the accent.
        self.focus_ring = match self.style {
            Style::Windows => self.text,
            Style::Mac | Style::Gnome => {
                ensure_contrast(self.accent, &backgrounds, GRAPHIC_CONTRAST)
            }
        };
    }

    /// Opaque page backgrounds that text and controls sit on.
    pub fn backgrounds(&self) -> [Rgba; 4] {
        [
            self.window,
            self.sidebar,
            composite(self.surface, self.window),
            self.popover,
        ]
    }

    /// Raises text and borders to WCAG AAA text contrast and solid outlines.
    pub fn raise_contrast(&mut self) {
        self.high_contrast = true;
        self.text = if self.dark {
            rgb(0xffffff)
        } else {
            rgb(0x000000)
        };
        self.border = Rgba {
            a: 0.6,
            ..self.text
        };
        self.selection = self.accent_tint(0.35);
        self.control_hover = Rgba {
            a: 0.22,
            ..self.text
        };
        self.meet_contrast(HIGH_TEXT_CONTRAST);
    }

    fn base(style: Style, dark: bool) -> Self {
        let (radius, card_radius, window_radius, titlebar_height, sidebar_width) = match style {
            Style::Mac => (6., 9., 12., 52., 208.),
            Style::Windows => (4., 8., 8., 40., 232.),
            Style::Gnome => (6., 12., 12., 46., 220.),
        };
        Self {
            style,
            dark,
            font: "Inter".into(),
            text_size: if style == Style::Mac { 13. } else { 14. },
            text_scale: 1.,
            high_contrast: false,
            window: rgb(0),
            chrome: rgb(0),
            sidebar: rgb(0),
            surface: rgb(0),
            popover: rgb(0),
            control: rgb(0),
            control_hover: rgb(0),
            selection: rgb(0),
            border: rgb(0),
            text: rgb(0),
            muted: rgb(0),
            accent: rgb(0),
            on_accent: rgb(0),
            accent_text: rgb(0),
            focus_ring: rgb(0),
            danger: rgb(if dark { 0xff6b6b } else { 0xc42b1c }),
            radius,
            card_radius,
            window_radius,
            titlebar_height,
            sidebar_width,
        }
    }

    /// A text size from the style's type ramp, scaled to the user's choice.
    pub fn font(&self, size: f32) -> Pixels {
        px((size * self.text_scale * 2.).round() / 2.)
    }

    /// The accent at a reduced opacity, for tinted backgrounds.
    pub fn accent_tint(&self, alpha: f32) -> Rgba {
        Rgba {
            a: alpha,
            ..self.accent
        }
    }
}

/// WCAG AA contrast for normal-size text.
pub const TEXT_CONTRAST: f32 = 4.5;
/// WCAG AAA contrast, used by the high-contrast option.
pub const HIGH_TEXT_CONTRAST: f32 = 7.;
/// WCAG contrast for focus indicators and other meaningful graphics.
pub const GRAPHIC_CONTRAST: f32 = 3.;

/// `fg` drawn over an opaque `bg`.
pub fn composite(fg: Rgba, bg: Rgba) -> Rgba {
    let mix = |f: f32, b: f32| f * fg.a + b * (1. - fg.a);
    Rgba {
        r: mix(fg.r, bg.r),
        g: mix(fg.g, bg.g),
        b: mix(fg.b, bg.b),
        a: 1.,
    }
}

/// WCAG 2 relative luminance of an opaque color.
fn relative_luminance(color: Rgba) -> f32 {
    let channel = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG 2 contrast ratio of `fg`, composited over opaque `bg`, against `bg`.
pub fn contrast_ratio(fg: Rgba, bg: Rgba) -> f32 {
    let (a, b) = (
        relative_luminance(composite(fg, bg)),
        relative_luminance(bg),
    );
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `color` unchanged if it reaches `target` against every background, or
/// else made opaque and moved away from the backgrounds' lightness until it
/// does. Hue and saturation are kept.
fn ensure_contrast(color: Rgba, backgrounds: &[Rgba], target: f32) -> Rgba {
    let passes = |c: Rgba| {
        backgrounds
            .iter()
            .all(|bg| contrast_ratio(c, *bg) >= target)
    };
    if passes(color) {
        return color;
    }
    let Some(first) = backgrounds.first() else {
        return color;
    };
    let dark_backgrounds = backgrounds
        .iter()
        .map(|bg| relative_luminance(*bg))
        .sum::<f32>()
        / backgrounds.len() as f32
        <= 0.18;
    let mut hsla = Hsla::from(composite(color, *first));
    for _ in 0..100 {
        hsla.l = if dark_backgrounds {
            (hsla.l + 0.01).min(1.)
        } else {
            (hsla.l - 0.01).max(0.)
        };
        let candidate = Rgba::from(hsla);
        if passes(candidate) || hsla.l <= 0. || hsla.l >= 1. {
            return candidate;
        }
    }
    Rgba::from(hsla)
}

fn luminance(color: u32) -> f32 {
    let channel = |shift: u32| ((color >> shift) & 0xff) as f32 / 255.;
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_defaults_follow_the_system() {
        let appearance: Appearance = serde_json::from_str("{}").unwrap();
        assert_eq!(appearance, Appearance::default());
        assert_eq!(appearance.style.resolve(), Style::native());
    }

    #[test]
    fn appearance_round_trips() {
        let appearance = Appearance {
            style: StyleChoice::Gnome,
            scheme: SchemeChoice::Light,
            accent: Some(0x3fa34d),
            autohide_controls: false,
            autoplay_next: false,
            text_scale: 1.25,
            contrast: ContrastChoice::High,
            show_labels: true,
            subtitles: SubtitleStyle {
                scale: 1.5,
                color: 0xffe14d,
                background: SubtitleBackground::Box,
                position: 90,
            },
        };
        let json = serde_json::to_string(&appearance).unwrap();
        assert!(json.contains("\"gnome\""));
        assert_eq!(
            serde_json::from_str::<Appearance>(&json).unwrap(),
            appearance
        );
    }

    /// Every style, scheme, and accent meets WCAG AA for text, and 3:1 for
    /// the focus ring, in both normal and high-contrast modes.
    #[test]
    fn palettes_meet_wcag_aa() {
        let fonts = Fonts {
            mac: "Inter".into(),
            windows: "Inter".into(),
            gnome: "Inter".into(),
        };
        let accents = std::iter::once(None).chain(ACCENTS.iter().map(|(_, c)| Some(*c)));
        for style in [Style::Mac, Style::Windows, Style::Gnome] {
            for dark in [false, true] {
                for accent in accents.clone() {
                    for high in [false, true] {
                        let mut t = Theme::new(style, dark, accent, &fonts);
                        let target = if high {
                            t.raise_contrast();
                            HIGH_TEXT_CONTRAST
                        } else {
                            TEXT_CONTRAST
                        };
                        let case = format!("{style:?} dark={dark} accent={accent:x?} high={high}");
                        for bg in t.backgrounds() {
                            for (name, fg) in [
                                ("text", t.text),
                                ("muted", t.muted),
                                ("link", t.accent_text),
                            ] {
                                let ratio = contrast_ratio(fg, bg);
                                assert!(ratio >= target, "{case}: {name} {ratio:.2}");
                            }
                            let ring = contrast_ratio(t.focus_ring, bg);
                            assert!(ring >= GRAPHIC_CONTRAST, "{case}: ring {ring:.2}");
                            let fill = contrast_ratio(t.accent, bg);
                            assert!(fill >= GRAPHIC_CONTRAST, "{case}: accent fill {fill:.2}");
                        }
                        let label = contrast_ratio(t.on_accent, t.accent);
                        assert!(label >= target, "{case}: accent label {label:.2}");
                    }
                }
            }
        }
    }

    #[test]
    fn contrast_adjustment_keeps_passing_colors() {
        let white = rgb(0xffffff);
        assert!((contrast_ratio(rgb(0x000000), white) - 21.).abs() < 0.01);
        assert_eq!(ensure_contrast(rgb(0x595959), &[white], 4.5), rgb(0x595959));
        let fixed = ensure_contrast(rgb(0x9a9a9a), &[white], 4.5);
        assert!(contrast_ratio(fixed, white) >= 4.5);
        assert!(fixed.r < 0.6);
    }

    #[test]
    fn appearance_values_are_clamped() {
        let mut appearance: Appearance = serde_json::from_str(
            r#"{"text_scale":9,"accent":4294967295,"subtitles":{"scale":-1,"color":4278190335,"position":3}}"#,
        )
        .unwrap();
        appearance.sanitize();
        assert_eq!(appearance.text_scale, TEXT_SCALE.1);
        assert_eq!(appearance.accent, Some(0xffffff));
        assert_eq!(appearance.subtitles.scale, SUBTITLE_SCALE.0);
        assert_eq!(appearance.subtitles.color, 0x0000ff);
        assert_eq!(appearance.subtitles.position, SUBTITLE_POSITION.0);
        let mut nan = Appearance {
            text_scale: f32::NAN,
            ..Appearance::default()
        };
        nan.sanitize();
        assert_eq!(nan.text_scale, 1.);
    }

    #[test]
    fn subtitle_styles_map_to_mpv_properties() {
        let style = SubtitleStyle {
            scale: 1.5,
            color: 0xffe14d,
            background: SubtitleBackground::Box,
            position: 90,
        };
        let properties = style.properties();
        assert!(properties.contains(&("sub-scale", "1.50".into())));
        assert!(properties.contains(&("sub-color", "#FFE14D".into())));
        assert!(properties.contains(&("sub-border-style", "background-box".into())));
        assert!(properties.contains(&("sub-back-color", "#B3000000".into())));
    }

    #[test]
    fn text_scale_rounds_to_half_pixels() {
        let fonts = Fonts {
            mac: "Inter".into(),
            windows: "Inter".into(),
            gnome: "Inter".into(),
        };
        let mut theme = Theme::new(Style::Mac, false, None, &fonts);
        assert_eq!(theme.font(13.), px(13.));
        theme.text_scale = 1.25;
        assert_eq!(theme.font(13.), px(16.5));
    }

    #[test]
    fn light_accents_use_dark_text() {
        assert!(luminance(0xd9a400) > 0.55);
        assert!(luminance(0x2f7cf6) < 0.55);
    }
}
