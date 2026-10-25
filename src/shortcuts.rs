//! Rebindable player shortcuts.
//!
//! Bindings are stored as GPUI-style keystroke strings such as `space`,
//! `shift-l`, or `ctrl-o`. Following GPUI, Shift is kept only for letters
//! and named keys: `?` is stored as `?`, not `shift-/`.

use gpui::Keystroke;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shortcut {
    PlayPause,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    Mute,
    Fullscreen,
    PictureInPicture,
    FrameBack,
    FrameForward,
    ShuttleBack,
    ShuttlePause,
    ShuttleForward,
    PreviousChapter,
    NextChapter,
    PreviousEpisode,
    NextEpisode,
    LoopAb,
    AudioDelayDown,
    AudioDelayUp,
    ShowShortcuts,
}

impl Shortcut {
    /// Every shortcut, in the order the shortcuts page lists them.
    pub const ALL: [Self; 21] = [
        Self::PlayPause,
        Self::SeekBack,
        Self::SeekForward,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Mute,
        Self::Fullscreen,
        Self::PictureInPicture,
        Self::FrameBack,
        Self::FrameForward,
        Self::ShuttleBack,
        Self::ShuttlePause,
        Self::ShuttleForward,
        Self::PreviousChapter,
        Self::NextChapter,
        Self::PreviousEpisode,
        Self::NextEpisode,
        Self::LoopAb,
        Self::AudioDelayDown,
        Self::AudioDelayUp,
        Self::ShowShortcuts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::PlayPause => "Play / pause",
            Self::SeekBack => "Back 10 seconds",
            Self::SeekForward => "Forward 10 seconds",
            Self::VolumeUp => "Volume up",
            Self::VolumeDown => "Volume down",
            Self::Mute => "Mute",
            Self::Fullscreen => "Fullscreen",
            Self::PictureInPicture => "Picture in picture",
            Self::FrameBack => "Previous frame",
            Self::FrameForward => "Next frame",
            Self::ShuttleBack => "Slower, or back 10 seconds",
            Self::ShuttlePause => "Pause and normal speed",
            Self::ShuttleForward => "Play, then faster",
            Self::PreviousChapter => "Previous chapter",
            Self::NextChapter => "Next chapter",
            Self::PreviousEpisode => "Previous episode",
            Self::NextEpisode => "Next episode",
            Self::LoopAb => "A–B loop: start, end, clear",
            Self::AudioDelayDown => "Audio earlier by 50 ms",
            Self::AudioDelayUp => "Audio later by 50 ms",
            Self::ShowShortcuts => "Show shortcuts",
        }
    }

    pub fn default_binding(self) -> &'static str {
        match self {
            Self::PlayPause => "space",
            Self::SeekBack => "left",
            Self::SeekForward => "right",
            Self::VolumeUp => "up",
            Self::VolumeDown => "down",
            Self::Mute => "m",
            Self::Fullscreen => "f",
            Self::PictureInPicture => "p",
            Self::FrameBack => ",",
            Self::FrameForward => ".",
            Self::ShuttleBack => "j",
            Self::ShuttlePause => "k",
            Self::ShuttleForward => "l",
            Self::PreviousChapter => "pageup",
            Self::NextChapter => "pagedown",
            Self::PreviousEpisode => "shift-p",
            Self::NextEpisode => "shift-n",
            Self::LoopAb => "b",
            Self::AudioDelayDown => "[",
            Self::AudioDelayUp => "]",
            Self::ShowShortcuts => "?",
        }
    }
}

/// Whether Flick handles `binding` itself before looking up shortcuts, so a
/// shortcut bound to it would never run. That covers Tab and Escape with
/// any modifiers, Enter, number keys, and the fixed Ctrl or ⌘ commands:
/// O (open), K (search), comma (settings), L (subtitles), and Q (quit).
fn reserved(binding: &str) -> bool {
    let mut key = binding;
    let mut command = false;
    while let Some((modifier, rest)) = key.split_once('-') {
        match modifier {
            "ctrl" | "cmd" | "super" => command = true,
            "alt" | "shift" => {}
            _ => break,
        }
        key = rest;
    }
    match key {
        "escape" => true,
        "tab" => !command,
        "o" | "k" | "," | "l" | "q" => command,
        _ => binding == "enter" || percent_key(binding).is_some(),
    }
}

/// The user's bindings. Only changes from the defaults are stored, so new
/// shortcuts in later versions get their defaults.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Bindings(BTreeMap<Shortcut, String>);

impl Bindings {
    pub fn binding(&self, shortcut: Shortcut) -> &str {
        self.0
            .get(&shortcut)
            .map_or(shortcut.default_binding(), String::as_str)
    }

    /// The shortcut bound to a pressed key, if any. An empty binding is unset.
    pub fn lookup(&self, keystroke: &str) -> Option<Shortcut> {
        Shortcut::ALL
            .into_iter()
            .find(|shortcut| !keystroke.is_empty() && self.binding(*shortcut) == keystroke)
    }

    /// Binds `keystroke` to `shortcut`. A shortcut that already used the key
    /// is left unbound rather than silently sharing it; it is returned so the
    /// interface can say so. Number keys and reserved keys are refused.
    pub fn assign(
        &mut self,
        shortcut: Shortcut,
        keystroke: &str,
    ) -> Result<Option<Shortcut>, String> {
        if reserved(keystroke) {
            return Err(format!(
                "{} is reserved. Choose another key.",
                display(keystroke)
            ));
        }
        let displaced = self.lookup(keystroke).filter(|other| *other != shortcut);
        if let Some(other) = displaced {
            self.set(other, "");
        }
        self.set(shortcut, keystroke);
        Ok(displaced)
    }

    pub fn reset(&mut self, shortcut: Shortcut) {
        // Restoring a default must not leave two shortcuts on one key.
        if let Some(other) = self
            .lookup(shortcut.default_binding())
            .filter(|other| *other != shortcut)
        {
            self.set(other, "");
        }
        self.0.remove(&shortcut);
    }

    pub fn reset_all(&mut self) {
        self.0.clear();
    }

    pub fn is_default(&self) -> bool {
        self.0.is_empty()
    }

    fn set(&mut self, shortcut: Shortcut, keystroke: &str) {
        if keystroke == shortcut.default_binding() {
            self.0.remove(&shortcut);
        } else {
            self.0.insert(shortcut, keystroke.to_owned());
        }
    }

    /// Unsets duplicated bindings from hand-edited settings. Shortcuts on
    /// their default keys keep them; a changed binding that collides loses.
    pub fn sanitize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let (defaults, changed): (Vec<_>, Vec<_>) = Shortcut::ALL
            .into_iter()
            .partition(|shortcut| !self.0.contains_key(shortcut));
        for shortcut in defaults.into_iter().chain(changed) {
            let binding = self.binding(shortcut).to_owned();
            if !binding.is_empty() && (reserved(&binding) || !seen.insert(binding)) {
                self.0.insert(shortcut, String::new());
            }
        }
    }
}

/// A keystroke in binding form: modifiers then key, joined by `-`.
pub fn normalize(keystroke: &Keystroke) -> String {
    let key = keystroke.key.as_str();
    let modifiers = keystroke.modifiers;
    // Single symbols and digits already reflect Shift, as in GPUI.
    let symbol = key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase();
    let mut parts = Vec::new();
    if modifiers.control {
        parts.push("ctrl");
    }
    if modifiers.alt {
        parts.push("alt");
    }
    if modifiers.platform {
        parts.push(if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "super"
        });
    }
    if modifiers.shift && !symbol {
        parts.push("shift");
    }
    parts.push(key);
    parts.join("-")
}

/// The platform's command key with `key`: `⌘K` on macOS, `Ctrl+K` elsewhere.
pub fn command(key: &str) -> String {
    display(&format!(
        "{}-{key}",
        if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        }
    ))
}

/// A readable form of a binding, such as `Shift+L`, `←`, or `⌘O`.
pub fn display(binding: &str) -> String {
    if binding.is_empty() {
        return "Not set".into();
    }
    let mut parts = binding.split('-').collect::<Vec<_>>();
    // A binding of "-" itself splits into empty parts.
    if binding.ends_with("--") || binding == "-" {
        parts.retain(|part| !part.is_empty());
        parts.push("-");
    }
    let key = parts.pop().unwrap_or_default();
    let key = match key {
        "space" => "Space".to_owned(),
        "left" => "←".to_owned(),
        "right" => "→".to_owned(),
        "up" => "↑".to_owned(),
        "down" => "↓".to_owned(),
        "pageup" => "Page Up".to_owned(),
        "pagedown" => "Page Down".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "backspace" => "Backspace".to_owned(),
        "delete" => "Delete".to_owned(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    };
    let mut text = String::new();
    for modifier in parts {
        text.push_str(match modifier {
            "cmd" => "⌘",
            "ctrl" => "Ctrl+",
            "alt" if cfg!(target_os = "macos") => "⌥",
            "alt" => "Alt+",
            "super" => "Super+",
            "shift" if cfg!(target_os = "macos") => "⇧",
            "shift" => "Shift+",
            other => other,
        });
    }
    text + &key
}

/// Number keys seek to 0–90 percent of the video.
pub fn percent_key(keystroke: &str) -> Option<f64> {
    let digit = keystroke.parse::<u8>().ok().filter(|digit| *digit <= 9)?;
    Some(f64::from(digit) / 10.)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    #[test]
    fn defaults_are_unique_and_round_trip() {
        let bindings = Bindings::default();
        let mut seen = std::collections::HashSet::new();
        for shortcut in Shortcut::ALL {
            assert!(seen.insert(bindings.binding(shortcut)), "{shortcut:?}");
            assert!(!reserved(shortcut.default_binding()), "{shortcut:?}");
            assert_eq!(bindings.lookup(shortcut.default_binding()), Some(shortcut));
        }
        let json = serde_json::to_string(&bindings).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn rebinding_moves_a_key_and_unbinds_the_previous_owner() {
        let mut bindings = Bindings::default();
        assert_eq!(
            bindings.assign(Shortcut::LoopAb, "m"),
            Ok(Some(Shortcut::Mute))
        );
        assert_eq!(bindings.lookup("m"), Some(Shortcut::LoopAb));
        assert_eq!(bindings.binding(Shortcut::Mute), "");
        assert_eq!(bindings.lookup("b"), None);
        // An unset shortcut never matches an empty keystroke.
        assert_eq!(bindings.lookup(""), None);
        let json = serde_json::to_string(&bindings).unwrap();
        let loaded: Bindings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, bindings);
        // Resetting Loop restores "b"; Mute stays unset until reset too.
        bindings.reset(Shortcut::LoopAb);
        assert_eq!(bindings.lookup("b"), Some(Shortcut::LoopAb));
        bindings.reset(Shortcut::Mute);
        assert_eq!(bindings, Bindings::default());
    }

    #[test]
    fn reserved_and_number_keys_cannot_be_bound() {
        let mut bindings = Bindings::default();
        for key in [
            "tab",
            "alt-tab",
            "shift-tab",
            "escape",
            "shift-escape",
            "enter",
            "5",
            "ctrl-o",
            "cmd-o",
            "ctrl-shift-k",
            "super-,",
            "cmd-l",
            "cmd-q",
            "ctrl-q",
        ] {
            assert!(bindings.assign(Shortcut::Mute, key).is_err(), "{key}");
        }
        for key in [
            "shift-m",
            "ctrl-tab",
            "alt-enter",
            "ctrl-5",
            "ctrl--",
            "-",
            "ctrl-f",
        ] {
            assert!(bindings.assign(Shortcut::Mute, key).is_ok(), "{key}");
            assert_eq!(bindings.lookup(key), Some(Shortcut::Mute));
        }
        // A reserved key in hand-edited settings is unset.
        let mut edited: Bindings = serde_json::from_str(r#"{"mute":"ctrl-o"}"#).unwrap();
        edited.sanitize();
        assert_eq!(edited.binding(Shortcut::Mute), "");
    }

    #[test]
    fn hand_edited_duplicates_are_unset() {
        let mut bindings: Bindings = serde_json::from_str(r#"{"mute":"f"}"#).unwrap();
        bindings.sanitize();
        assert_eq!(bindings.binding(Shortcut::Mute), "");
        assert_eq!(bindings.lookup("f"), Some(Shortcut::Fullscreen));
        assert!(serde_json::from_str::<Bindings>(r#"{"unknown":"x"}"#).is_err());
    }

    #[test]
    fn keystrokes_normalize_like_gpui() {
        assert_eq!(normalize(&keystroke("space")), "space");
        assert_eq!(normalize(&keystroke("shift-l")), "shift-l");
        assert_eq!(normalize(&keystroke("ctrl-o")), "ctrl-o");
        let mut question = keystroke("?");
        question.modifiers.shift = true;
        assert_eq!(normalize(&question), "?");
        assert_eq!(
            display("shift-l"),
            if cfg!(target_os = "macos") {
                "⇧L"
            } else {
                "Shift+L"
            }
        );
        assert_eq!(display("left"), "←");
        assert_eq!(display("pagedown"), "Page Down");
        assert_eq!(display(""), "Not set");
        assert_eq!(display("-"), "-");
    }

    #[test]
    fn number_keys_seek_by_tenths() {
        assert_eq!(percent_key("0"), Some(0.));
        assert_eq!(percent_key("7"), Some(0.7));
        assert_eq!(percent_key("10"), None);
        assert_eq!(percent_key("a"), None);
    }
}
