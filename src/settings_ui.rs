//! The Appearance page: interface style, color scheme, accent, and playback
//! chrome preferences.

use super::{
    chrome_ui::{group, row, section_title},
    *,
};
use crate::theme::{ACCENTS, Fonts, SchemeChoice, StyleChoice};
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
            dots = dots.child(
                div()
                    .id(("accent", index))
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
                    ),
            );
        }
        let autohide = self.switch(
            "autohide",
            appearance.autohide_controls,
            cx,
            |this, window, cx| {
                this.set_appearance(|a| a.autohide_controls = !a.autohide_controls, window, cx)
            },
        );
        v_flex()
            .w_full()
            .max_w(px(760.))
            .child(section_title(&t, "Interface Style"))
            .child(
                div()
                    .mb_2()
                    .text_size(px(t.text_size - 1.))
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
                ],
            ))
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
        v_flex()
            .id(SharedString::from(format!("style-{label}")))
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
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(detail),
            )
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
