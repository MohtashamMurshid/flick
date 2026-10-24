use gpui::{AssetSource, Hsla, SharedString, Styled, Svg, px, svg};
use std::borrow::Cow;

macro_rules! embedded_icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[$(
            (concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg"))),
        )*];
    };
}

embedded_icons!(
    "folder-open",
    "film",
    "history",
    "keyboard",
    "volume-2",
    "volume-x",
    "minus",
    "plus",
    "maximize",
    "minimize",
    "rotate-ccw",
    "rotate-cw",
    "play",
    "pause",
    "check",
    "x",
    "captions",
    "audio-lines",
    "circle-alert",
    "upload",
    "search",
    "library",
    "arrow-left",
    "download",
    "share-2",
    "folder-plus",
    "house",
    "palette",
    "square",
    "copy",
    "monitor",
    "info",
    "sun",
    "eye-off",
    "magnet",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| (*name).into())
            .collect())
    }
}

pub fn icon(name: &'static str, size: f32, color: impl Into<Hsla>) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .text_color(color)
}
