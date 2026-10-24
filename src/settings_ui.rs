//! The Appearance page: interface style, color scheme, accent, and playback
//! chrome preferences.

use super::{
    chrome_ui::{group, row, section_title},
    *,
};
use crate::{
    shortcuts::{Shortcut, display},
    theme::{
        ACCENTS, ContrastChoice, Fonts, MotionChoice, SUBTITLE_COLORS, SUBTITLE_POSITION,
        SUBTITLE_SCALE, SchemeChoice, StyleChoice, SubtitleBackground, TEXT_SCALE,
    },
};
use gpui::{FontWeight, hsla, linear_color_stop, linear_gradient};

impl Flick {
    pub(super) fn appearance_page(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let appearance = self.store.data.appearance.clone();
        let native = format!("{} detected", Style::native().label());
        let tiles = [
            (StyleChoice::System, "Match System", native.as_str()),
            (StyleChoice::Mac, "macOS", "Toolbar and floating HUD"),
            (StyleChoice::Windows, "Windows 11", "Fluent with Mica"),
            (StyleChoice::Gnome, "GNOME", "libadwaita"),
        ];
        let mut tile_row = h_flex().flex_wrap().items_start().gap_4();
        for (choice, label, detail) in tiles {
            tile_row = tile_row.child(self.style_tile(
                choice,
                label,
                detail.to_owned(),
                appearance.style == choice,
                cx,
            ));
        }
        let scheme = self.segmented(
            "scheme",
            &[
                (SchemeChoice::System, "Auto"),
                (SchemeChoice::Light, "Light"),
                (SchemeChoice::Dark, "Dark"),
            ],
            appearance.scheme,
            cx,
            |this, scheme, window, cx| this.set_appearance(|a| a.scheme = scheme, window, cx),
        );
        let style_accent =
            crate::theme::Theme::new(t.style, t.dark, None, cx.global::<Fonts>()).accent;
        let mut dots = h_flex().gap_1p5();
        for (index, (name, color)) in std::iter::once(("Default", None))
            .chain(ACCENTS.iter().map(|(name, color)| (*name, Some(*color))))
            .enumerate()
        {
            let selected = appearance.accent == color;
            let fill = color.map_or(style_accent, rgb);
            let id = ElementId::from(("accent", index));
            let dot = div()
                .id(id.clone())
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded_full()
                .border_2()
                .border_color(if selected { fill } else { rgba(0x00000000) })
                .cursor_pointer()
                .tooltip(move |_, cx| cx.new(|_| Hint(name.into())).into())
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_appearance(|a| a.accent = color, window, cx)
                }))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .bg(fill)
                        .border_1()
                        .border_color(rgba(0x00000026))
                        .when(color.is_none(), |d| {
                            d.flex()
                                .items_center()
                                .justify_center()
                                .child(div().size(px(6.)).rounded_full().bg(rgb(0xffffff)))
                        }),
                );
            dots = dots.child(self.focusable(dot, &id, Some(name.into()), 13., (0., 0.), cx));
        }
        let autohide = self.switch(
            "autohide",
            appearance.autohide_controls,
            cx,
            |this, window, cx| {
                this.set_appearance(|a| a.autohide_controls = !a.autohide_controls, window, cx)
            },
        );
        let autoplay = self.switch(
            "autoplay-next",
            appearance.autoplay_next,
            cx,
            |this, window, cx| {
                this.set_appearance(|a| a.autoplay_next = !a.autoplay_next, window, cx)
            },
        );
        let percent = |value: f32| format!("{}%", (value * 100.).round());
        let text_size = h_flex()
            .gap_3()
            .child(
                div()
                    .min_w(t.font(44.))
                    .whitespace_nowrap()
                    .text_right()
                    .text_color(t.muted)
                    .child(percent(appearance.text_scale)),
            )
            .child(self.slider(
                "text-scale",
                "Text size",
                appearance.text_scale,
                TEXT_SCALE,
                0.05,
                160.,
                cx,
                |this, value, window, cx| this.set_appearance(|a| a.text_scale = value, window, cx),
            ));
        let contrast = self.segmented(
            "contrast",
            &[
                (ContrastChoice::System, "Auto"),
                (ContrastChoice::Standard, "Standard"),
                (ContrastChoice::High, "High"),
            ],
            appearance.contrast,
            cx,
            |this, contrast, window, cx| this.set_appearance(|a| a.contrast = contrast, window, cx),
        );
        let motion = self.segmented(
            "motion",
            &[
                (MotionChoice::System, "Auto"),
                (MotionChoice::Reduced, "On"),
                (MotionChoice::Full, "Off"),
            ],
            appearance.motion,
            cx,
            |this, motion, window, cx| this.set_appearance(|a| a.motion = motion, window, cx),
        );
        let labels = self.switch(
            "show-labels",
            appearance.show_labels,
            cx,
            |this, window, cx| this.set_appearance(|a| a.show_labels = !a.show_labels, window, cx),
        );
        let subtitles = appearance.subtitles.clone();
        let subtitle_size = h_flex()
            .gap_3()
            .child(
                div()
                    .min_w(t.font(44.))
                    .whitespace_nowrap()
                    .text_right()
                    .text_color(t.muted)
                    .child(percent(subtitles.scale)),
            )
            .child(self.slider(
                "subtitle-scale",
                "Subtitle size",
                subtitles.scale,
                SUBTITLE_SCALE,
                0.05,
                160.,
                cx,
                |this, value, window, cx| {
                    this.set_appearance(|a| a.subtitles.scale = value, window, cx)
                },
            ));
        let mut subtitle_colors = h_flex().gap_1p5();
        for (index, (name, color)) in SUBTITLE_COLORS.into_iter().enumerate() {
            let id = ElementId::from(("subtitle-color", index));
            let dot = div()
                .id(id.clone())
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded_full()
                .border_2()
                .border_color(if subtitles.color == color {
                    t.accent
                } else {
                    rgba(0x00000000)
                })
                .cursor_pointer()
                .tooltip(move |_, cx| cx.new(|_| Hint(name.into())).into())
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_appearance(|a| a.subtitles.color = color, window, cx)
                }))
                .child(
                    div()
                        .size(px(18.))
                        .rounded_full()
                        .bg(rgb(color))
                        .border_1()
                        .border_color(rgba(0x00000040)),
                );
            subtitle_colors = subtitle_colors.child(self.focusable(
                dot,
                &id,
                Some(name.into()),
                13.,
                (0., 0.),
                cx,
            ));
        }
        let subtitle_background = self.segmented(
            "subtitle-background",
            &[
                (SubtitleBackground::Outline, "Outline"),
                (SubtitleBackground::Box, "Box"),
            ],
            subtitles.background,
            cx,
            |this, background, window, cx| {
                this.set_appearance(|a| a.subtitles.background = background, window, cx)
            },
        );
        let raised = SUBTITLE_POSITION.1 - subtitles.position;
        let subtitle_position = h_flex()
            .gap_3()
            .child(
                div()
                    .min_w(t.font(64.))
                    .whitespace_nowrap()
                    .text_right()
                    .text_color(t.muted)
                    .child(if raised == 0 {
                        "Bottom".to_owned()
                    } else {
                        format!("{raised}% up")
                    }),
            )
            .child(self.slider(
                "subtitle-position",
                "Subtitle position",
                raised as f32,
                (0., (SUBTITLE_POSITION.1 - SUBTITLE_POSITION.0) as f32),
                5.,
                160.,
                cx,
                |this, value, window, cx| {
                    let position = SUBTITLE_POSITION.1 - value.round() as u32;
                    this.set_appearance(|a| a.subtitles.position = position, window, cx)
                },
            ));
        v_flex()
            .w_full()
            .max_w(px(760.))
            .child(section_title(&t, "Interface Style"))
            .child(
                div()
                    .mb_2()
                    .text_size(t.font(t.text_size - 1.))
                    .text_color(t.muted)
                    .child("Changes window chrome, controls, typography, and the player for each platform."),
            )
            .child(tile_row)
            .child(div().h(px(24.)))
            .child(section_title(&t, "Look"))
            .child(group(
                &t,
                vec![
                    row(
                        &t,
                        Some("sun"),
                        "Color scheme",
                        Some("Auto follows your system setting".into()),
                        scheme,
                    )
                    .into_any_element(),
                    row(&t, Some("palette"), "Accent color", None, dots).into_any_element(),
                ],
            ))
            .child(div().h(px(24.)))
            .child(section_title(&t, "Accessibility"))
            .child(group(
                &t,
                vec![
                    row(&t, Some("sun"), "Text size", None, text_size).into_any_element(),
                    row(
                        &t,
                        Some("eye-off"),
                        "Contrast",
                        Some("Auto follows your system's increased-contrast setting".into()),
                        contrast,
                    )
                    .into_any_element(),
                    row(
                        &t,
                        Some("monitor"),
                        "Reduce motion",
                        Some("Shows player controls without fades. Auto follows your system".into()),
                        motion,
                    )
                    .into_any_element(),
                    row(
                        &t,
                        Some("info"),
                        "Show button labels",
                        Some("Toolbar buttons show text as well as icons".into()),
                        labels,
                    )
                    .into_any_element(),
                ],
            ))
            .child(div().h(px(24.)))
            .child(section_title(&t, "Playback"))
            .child(group(
                &t,
                vec![
                    row(
                        &t,
                        Some("eye-off"),
                        "Hide controls while playing",
                        Some("After 2.5 seconds without pointer movement".into()),
                        autohide,
                    )
                    .into_any_element(),
                    row(
                        &t,
                        Some("play"),
                        "Play next episode automatically",
                        Some("Shows in your library continue after an 8-second countdown".into()),
                        autoplay,
                    )
                    .into_any_element(),
                ],
            ))
            .child(div().h(px(24.)))
            .child(section_title(&t, "Subtitles"))
            .child(group(
                &t,
                vec![
                    row(&t, Some("captions"), "Size", None, subtitle_size).into_any_element(),
                    row(&t, Some("palette"), "Color", None, subtitle_colors).into_any_element(),
                    row(
                        &t,
                        Some("captions"),
                        "Background",
                        Some("Box draws a dark background behind each line".into()),
                        subtitle_background,
                    )
                    .into_any_element(),
                    row(
                        &t,
                        Some("captions"),
                        "Position",
                        Some("Subtitles still move above visible controls".into()),
                        subtitle_position,
                    )
                    .into_any_element(),
                ],
            ))
    }

    /// Player shortcuts with rebinding, plus the fixed keys.
    pub(super) fn shortcuts_page(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let bindings = &self.store.data.shortcuts;
        let key_pill = |text: String, active: bool| {
            div()
                .flex_none()
                .min_w(px(44.))
                .px_2()
                .py_0p5()
                .rounded(px(t.radius))
                .border_1()
                .border_color(if active { t.accent } else { t.border })
                .bg(t.control)
                .text_center()
                .text_size(t.font(12.))
                .text_color(if active { t.accent } else { t.text })
                .child(text)
        };
        let mut rows = Vec::new();
        for (index, shortcut) in Shortcut::ALL.into_iter().enumerate() {
            let capturing = self.capturing == Some(shortcut);
            let binding = bindings.binding(shortcut);
            let changed = binding != shortcut.default_binding();
            let mut trailing = h_flex()
                .gap_2()
                .child(key_pill(
                    if capturing {
                        "Press a key…".into()
                    } else {
                        display(binding)
                    },
                    capturing,
                ))
                .child(self.button(
                    ("change-shortcut", index),
                    None,
                    if capturing { "Cancel" } else { "Change" },
                    false,
                    cx,
                    move |this, _, cx| {
                        this.capturing = if this.capturing == Some(shortcut) {
                            None
                        } else {
                            Some(shortcut)
                        };
                        cx.notify();
                    },
                ));
            trailing = trailing.child(if changed {
                self.icon_button(
                    ("reset-shortcut", index),
                    "rotate-ccw",
                    format!("Restore {}", display(shortcut.default_binding())),
                    cx,
                    move |this, _, cx| {
                        this.store.data.shortcuts.reset(shortcut);
                        this.persist();
                        cx.notify();
                    },
                )
                .into_any_element()
            } else {
                div().w(px(28.)).into_any_element()
            });
            rows.push(row(&t, None, shortcut.label(), None, trailing).into_any_element());
        }
        let command = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let fixed = [
            ("0 – 9".to_owned(), "Jump to 0–90% of the video"),
            (
                format!("{command}O"),
                "Add videos, or open a video in the player",
            ),
            (format!("{command}L"), "Load subtitles"),
            (format!("{command}K"), "Search"),
            (format!("{command},"), "Appearance"),
            ("Tab / Shift+Tab".to_owned(), "Move between controls"),
            ("Arrow keys".to_owned(), "Move between video cards"),
            ("Enter / Space".to_owned(), "Activate the focused control"),
            ("Escape".to_owned(), "Exit fullscreen or close a menu"),
        ]
        .into_iter()
        .map(|(key, action)| row(&t, None, action, None, key_pill(key, false)).into_any_element())
        .collect();
        v_flex()
            .w_full()
            .max_w(px(760.))
            .child(
                h_flex()
                    .mb_2()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_size(t.font(t.text_size - 1.))
                            .text_color(t.muted)
                            .child("Choose Change, then press the new key. Escape cancels."),
                    )
                    .when(!bindings.is_default(), |d| {
                        d.child(self.button(
                            "reset-shortcuts",
                            Some("rotate-ccw"),
                            "Restore Defaults",
                            false,
                            cx,
                            |this, _, cx| {
                                this.store.data.shortcuts.reset_all();
                                this.capturing = None;
                                this.persist();
                                cx.notify();
                            },
                        ))
                    }),
            )
            .child(section_title(&t, "Player"))
            .child(group(&t, rows))
            .child(div().h(px(24.)))
            .child(section_title(&t, "Always Available"))
            .child(group(&t, fixed))
    }

    fn style_tile(
        &self,
        choice: StyleChoice,
        label: &'static str,
        detail: String,
        selected: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let preview = match choice {
            StyleChoice::System => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(linear_gradient(
                    150.,
                    linear_color_stop(rgb(0x4a4a50), 0.),
                    linear_color_stop(rgb(0x1f1f23), 1.),
                ))
                .child(icon("monitor", 30., rgba(0xffffffd9))),
            StyleChoice::Mac => mini_window(
                linear_gradient(
                    135.,
                    linear_color_stop(rgb(0xf08a6c), 0.),
                    linear_color_stop(rgb(0x7b5ce5), 1.),
                ),
                6.,
                h_flex()
                    .gap(px(3.))
                    .px(px(5.))
                    .h(px(11.))
                    .child(dot(0xff5f57))
                    .child(dot(0xfebc2e))
                    .child(dot(0x28c840)),
            ),
            StyleChoice::Windows => mini_window(
                linear_gradient(
                    160.,
                    linear_color_stop(rgb(0x3a8dff), 0.),
                    linear_color_stop(rgb(0x0b2a6b), 1.),
                ),
                3.,
                h_flex()
                    .justify_end()
                    .gap(px(5.))
                    .px(px(5.))
                    .h(px(11.))
                    .children((0..3).map(|_| div().w(px(5.)).h(px(1.)).bg(rgb(0xdddddd)))),
            ),
            StyleChoice::Gnome => mini_window(
                linear_gradient(
                    135.,
                    linear_color_stop(rgb(0x1a5fb4), 0.),
                    linear_color_stop(rgb(0x613583), 1.),
                ),
                7.,
                h_flex()
                    .justify_end()
                    .px(px(4.))
                    .h(px(13.))
                    .child(div().size(px(7.)).rounded_full().bg(rgb(0x5a5a60))),
            ),
        };
        let id = ElementId::from(SharedString::from(format!("style-{label}")));
        let tile = v_flex()
            .id(id.clone())
            .relative()
            .w(px(152.))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.set_appearance(|a| a.style = choice, window, cx)
            }))
            .child(
                div()
                    .p(px(3.))
                    .rounded(px(13.))
                    .border_2()
                    .border_color(if selected { t.accent } else { rgba(0x00000000) })
                    .child(
                        div()
                            .h(px(84.))
                            .w_full()
                            .rounded(px(9.))
                            .overflow_hidden()
                            .border_1()
                            .border_color(t.border)
                            .child(preview),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .px_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.))
                            .rounded_full()
                            .border_1()
                            .border_color(if selected { t.accent } else { t.muted })
                            .when(selected, |d| {
                                d.bg(t.accent)
                                    .child(div().size(px(6.)).rounded_full().bg(t.on_accent))
                            }),
                    )
                    .child(div().font_weight(FontWeight::MEDIUM).child(label)),
            )
            .child(
                div()
                    .px_1()
                    .text_size(t.font(11.5))
                    .text_color(t.muted)
                    .child(detail),
            );
        self.focusable(tile, &id, None, 13., (0., 0.), cx)
    }
}

fn dot(color: u32) -> Div {
    div().size(px(5.)).rounded_full().bg(rgb(color))
}

/// A tiny window on a wallpaper, used to preview each style.
fn mini_window(wallpaper: gpui::Background, radius: f32, titlebar: Div) -> Div {
    div().relative().size_full().bg(wallpaper).child(
        v_flex()
            .absolute()
            .left(px(18.))
            .right(px(18.))
            .top(px(16.))
            .bottom(px(-8.))
            .rounded(px(radius))
            .overflow_hidden()
            .bg(rgb(0x2b2b2f))
            .shadow_md()
            .child(titlebar)
            .child(
                h_flex()
                    .flex_1()
                    .items_start()
                    .child(div().w(px(32.)).h_full().bg(hsla(0., 0., 1., 0.07)))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(4.))
                            .p(px(6.))
                            .child(div().h(px(14.)).rounded(px(2.)).bg(hsla(0., 0., 1., 0.12)))
                            .child(div().h(px(14.)).rounded(px(2.)).bg(hsla(0., 0., 1., 0.08))),
                    ),
            ),
    )
}
