//! Interface styles that follow macOS, Windows 11, and GNOME conventions.
//!
//! A style changes window chrome, controls, colors, typography, and metrics.
//! Pages and playback behavior stay the same in every style.

use gpui::{App, Global, Rgba, SharedString, WindowAppearance, rgb, rgba};
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
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            style: StyleChoice::System,
            scheme: SchemeChoice::System,
            accent: None,
            autohide_controls: true,
        }
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
    pub text_size: f32,
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
    pub accent: Rgba,
    pub on_accent: Rgba,
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
    let theme = Theme::new(
        appearance.style.resolve(),
        dark,
        appearance.accent,
        cx.global::<Fonts>(),
    );
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
        theme
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
            danger: rgb(if dark { 0xff6b6b } else { 0xc42b1c }),
            radius,
            card_radius,
            window_radius,
            titlebar_height,
            sidebar_width,
        }
    }

    /// The accent at a reduced opacity, for tinted backgrounds.
    pub fn accent_tint(&self, alpha: f32) -> Rgba {
        Rgba {
            a: alpha,
            ..self.accent
        }
    }
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
        };
        let json = serde_json::to_string(&appearance).unwrap();
        assert!(json.contains("\"gnome\""));
        assert_eq!(
            serde_json::from_str::<Appearance>(&json).unwrap(),
            appearance
        );
    }

    #[test]
    fn light_accents_use_dark_text() {
        assert!(luminance(0xd9a400) > 0.55);
        assert!(luminance(0x2f7cf6) < 0.55);
    }
}
