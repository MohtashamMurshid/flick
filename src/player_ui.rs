//! Playback view: the video surface with style-specific overlay controls.

use super::{
    chrome_ui::{notice, thumbnail, traffic_light_space},
    *,
};
use gpui::{AnyElement, FontWeight, Rgba, linear_color_stop, linear_gradient};

/// Fraction of the video's duration at `time`, for timeline markers.
fn timeline_fraction(time: f64, duration: f64) -> f32 {
    if duration > 0. {
        (time / duration).clamp(0., 1.) as f32
    } else {
        0.
    }
}

const ON_VIDEO: u32 = 0xffffff;
const ON_VIDEO_HOVER: u32 = 0xffffff26;

/// Height from the window bottom covered by the playback controls.
pub(super) fn controls_height(style: Style) -> f32 {
    match style {
        Style::Mac => 128.,
        Style::Windows => 116.,
        Style::Gnome => 78.,
    }
}

impl Flick {
    pub(super) fn player_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let show = self.controls_shown || self.controls_fading(cx);
        let controls_height = controls_height(t.style);
        let menu = self.menu_panel(cx);
        div()
            .relative()
            .size_full()
            .bg(rgb(0x000000))
            .child(self.video_surface(cx))
            .when(show, |d| {
                d.child(self.fade("titlebar-fade", self.player_titlebar(window, cx), 0., cx))
            })
            .when(show && self.file_loaded, |d| {
                d.child(self.fade("controls-fade", self.player_controls(window, cx), 8., cx))
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    h_flex()
                        .absolute()
                        .top(px(t.titlebar_height.max(44.) + 8.))
                        .left(px(16.))
                        .right(px(16.))
                        .child(
                            notice(&t, true, error)
                                .bg(t.popover)
                                .child(self.icon_button(
                                    "dismiss-error",
                                    "x",
                                    "Dismiss",
                                    cx,
                                    |this, w, cx| this.handle(Control::DismissError, w, cx),
                                )),
                        ),
                )
            })
            .when_some(menu, |d, panel| {
                d.child(
                    div()
                        .absolute()
                        .right(px(16.))
                        .bottom(px(controls_height + 8.))
                        .child(panel),
                )
            })
            .when_some(self.up_next_card(cx), |d, card| {
                d.child(
                    div()
                        .absolute()
                        .right(px(16.))
                        .bottom(px(controls_height + 8.))
                        .child(card),
                )
            })
            .when_some(self.status.as_ref(), |d, (text, _)| {
                let pill = h_flex()
                    .top(px(t.titlebar_height.max(44.) + 12.))
                    .left_0()
                    .right_0()
                    .justify_center()
                    .child(
                        div()
                            .px_3()
                            .py_1p5()
                            .rounded_full()
                            .bg(rgba(0x000000b3))
                            .text_color(rgb(ON_VIDEO))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(text.clone()),
                    );
                if t.reduce_motion {
                    d.child(pill)
                } else {
                    d.child(pill.with_animation(
                        ("status-fade", self.status_count),
                        Animation::new(Duration::from_millis(140)),
                        |pill, delta| pill.opacity(delta),
                    ))
                }
            })
            .into_any_element()
    }

    /// Fades player chrome in or out and, with `slide`, moves it that many
    /// pixels. Reduced motion shows and hides it at once.
    fn fade(
        &self,
        key: &'static str,
        element: impl IntoElement + 'static,
        slide: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        if cx.theme().reduce_motion {
            return element.into_any_element();
        }
        let shown = self.controls_shown;
        div()
            .absolute()
            .inset_0()
            .child(element)
            .with_animation(
                (key, self.controls_changes),
                Animation::new(super::FADE).with_easing(gpui::ease_in_out),
                move |el, delta| {
                    let visible = if shown { delta } else { 1. - delta };
                    let offset = (1. - visible) * slide;
                    el.opacity(visible).top(px(offset)).bottom(px(-offset))
                },
            )
            .into_any_element()
    }

    fn video_surface(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = cx.theme().clone();
        let video_bounds = self.video_bounds.clone();
        div()
            .id("video")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _, _| video_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.focus.focus(window);
                if event.click_count() == 2 {
                    if this.accepts_video_double_click() {
                        window.toggle_fullscreen();
                    }
                } else if this.menu.take().is_some() {
                    cx.notify();
                }
            }))
            .when(self.pip.is_some(), |d| {
                d.child(
                    v_flex()
                        .items_center()
                        .gap_3()
                        .text_color(rgb(ON_VIDEO))
                        .child(icon("picture-in-picture", 40., rgba(0xffffffb3)))
                        .child(
                            div()
                                .text_size(t.font(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Playing in picture in picture"),
                        )
                        .child(self.button(
                            "pip-return",
                            Some("maximize"),
                            "Show Here",
                            true,
                            cx,
                            |this, _, cx| this.close_pip(true, cx),
                        )),
                )
            })
            .when_some(
                self.frame.clone().filter(|_| self.pip.is_none()),
                |d, frame| d.child(img(frame).size_full().object_fit(ObjectFit::Contain)),
            )
            .when(self.frame.is_none() && self.pip.is_none(), |d| {
                let failed = self.error.is_some();
                d.child(
                    v_flex()
                        .items_center()
                        .gap_3()
                        .text_color(rgb(ON_VIDEO))
                        .child(icon(
                            if failed { "circle-alert" } else { "film" },
                            40.,
                            rgba(0xffffffb3),
                        ))
                        .child(
                            div()
                                .text_size(t.font(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(if failed {
                                    "Unable to open video"
                                } else {
                                    "Opening video…"
                                }),
                        )
                        .when(failed, |d| {
                            d.child(self.button(
                                "open-other",
                                Some("folder-open"),
                                "Open Another Video…",
                                true,
                                cx,
                                |this, w, cx| this.handle(Control::Open, w, cx),
                            ))
                        })
                        .when(t.style == Style::Gnome, |d| d.font_weight(FontWeight::BOLD)),
                )
            })
    }

    fn overlay_button(
        &self,
        id: &'static str,
        name: &'static str,
        tooltip: impl Into<SharedString>,
        control: Control,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let size = if cx.theme().style == Style::Windows {
            36.
        } else {
            32.
        };
        self.tinted_icon_button(
            id,
            name,
            tooltip,
            size,
            rgb(ON_VIDEO),
            rgba(ON_VIDEO_HOVER),
            None,
            cx,
            move |this, w, cx| this.handle(control, w, cx),
        )
    }

    /// A labeled overlay button, used when "Show button labels" is on.
    fn overlay_label_button(
        &self,
        id: &'static str,
        name: &'static str,
        label: &'static str,
        control: Control,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let el = h_flex()
            .id(id)
            .relative()
            .flex_none()
            .gap_1p5()
            .h(px(30.))
            .px_2p5()
            .rounded(px(t.radius))
            .bg(rgba(0x00000066))
            .text_color(rgb(ON_VIDEO))
            .cursor_pointer()
            .hover(|s| s.bg(rgba(ON_VIDEO_HOVER)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.handle(control, window, cx);
            }))
            .child(icon(name, 16., rgb(ON_VIDEO)))
            .child(label);
        self.focusable(el, &id.into(), None, t.radius, (0., 0.), cx)
    }

    fn player_titlebar(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let title = self.path.as_deref().map(file_title).unwrap_or_default();
        let labels = self.store.data.appearance.show_labels;
        let back = if labels {
            self.overlay_label_button("player-back", "arrow-left", "Library", Control::Library, cx)
        } else {
            self.overlay_button(
                "player-back",
                "arrow-left",
                "Back to Library",
                Control::Library,
                cx,
            )
        };
        let keys = if labels {
            self.overlay_label_button("player-keys", "keyboard", "Shortcuts", Control::Keys, cx)
        } else {
            self.overlay_button(
                "player-keys",
                "keyboard",
                "Keyboard shortcuts (?)",
                Control::Keys,
                cx,
            )
        };
        let controls = self.window_controls(window, true, cx);
        let bar = div()
            .id("player-titlebar")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(t.titlebar_height.max(44.)))
            .flex()
            .items_center()
            .gap_2()
            .text_color(rgb(ON_VIDEO))
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgba(0x000000b3), 0.),
                linear_color_stop(rgba(0x00000000), 1.),
            ));
        let centered_title = |weight| {
            div()
                .absolute()
                .left(px(120.))
                .right(px(120.))
                .text_center()
                .font_weight(weight)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(title.clone())
        };
        let bar = match t.style {
            Style::Mac => bar
                .px_3()
                .child(div().w(px(traffic_light_space())))
                .when_some(controls, |d, c| d.child(div().mr_2().child(c)))
                .child(centered_title(FontWeight::SEMIBOLD))
                .child(back)
                .child(div().flex_1())
                .child(keys),
            Style::Windows => bar
                .pl(px(8. + traffic_light_space()))
                .child(back)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(t.font(12.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(format!("{title} – Flick")),
                )
                .child(keys)
                .when_some(controls, |d, c| d.child(c)),
            Style::Gnome => bar
                .px_1p5()
                .pl(px(6. + traffic_light_space()))
                .child(back)
                .child(centered_title(FontWeight::BOLD))
                .child(div().flex_1())
                .child(keys)
                .when_some(controls, |d, c| d.child(c)),
        };
        self.titlebar_area(bar, cx)
    }

    fn play_button(&self, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let playing = self.playing();
        let (name, tip) = if playing {
            ("pause", "Pause (Space)")
        } else {
            ("play", "Play (Space)")
        };
        let (size, bg, fg): (f32, Rgba, Rgba) = match t.style {
            Style::Mac => (40., rgba(0x00000000), rgb(ON_VIDEO)),
            Style::Windows => (44., t.accent, t.on_accent),
            Style::Gnome => (38., rgba(0xffffff26), rgb(ON_VIDEO)),
        };
        let el = div()
            .id("play")
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(size))
            .rounded_full()
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| s.bg(rgba(ON_VIDEO_HOVER)))
            .tooltip(move |_, cx| cx.new(|_| Hint(tip.into())).into())
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.handle(Control::Play, window, cx);
            }))
            .child(icon(
                name,
                if t.style == Style::Mac { 26. } else { 20. },
                fg,
            ));
        self.focusable(
            el,
            &"play".into(),
            Some(tip.into()),
            size / 2.,
            (0., 0.),
            cx,
        )
    }

    fn timeline_bar(&self, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let timeline = self.timeline.clone();
        let progress = if self.snapshot.duration > 0. {
            (self.snapshot.position / self.snapshot.duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let fill = if t.style == Style::Mac {
            rgb(ON_VIDEO)
        } else {
            t.accent
        };
        let knob = match t.style {
            Style::Mac => self.scrubbing.then(|| {
                div()
                    .size(px(12.))
                    .rounded_full()
                    .bg(rgb(ON_VIDEO))
                    .into_any_element()
            }),
            Style::Windows => Some(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(18.))
                    .rounded_full()
                    .bg(rgb(0x454545))
                    .child(div().size(px(10.)).rounded_full().bg(t.accent))
                    .into_any_element(),
            ),
            Style::Gnome => Some(
                div()
                    .size(px(14.))
                    .rounded_full()
                    .bg(rgb(ON_VIDEO))
                    .shadow_sm()
                    .into_any_element(),
            ),
        };
        let knob_size = match t.style {
            Style::Mac => 12.,
            Style::Windows => 18.,
            Style::Gnome => 14.,
        };
        let duration = self.snapshot.duration;
        let chapters = self
            .snapshot
            .chapters
            .iter()
            .filter(|chapter| chapter.time > 0. && chapter.time < duration)
            .map(|chapter| {
                div()
                    .absolute()
                    .top(px(-2.))
                    .left(relative(timeline_fraction(chapter.time, duration)))
                    .w(px(2.))
                    .h(px(8.))
                    .rounded_full()
                    .bg(rgba(0xffffffcc))
            })
            .collect::<Vec<_>>();
        let loop_span = self.snapshot.loop_start.map(|start| {
            let a = timeline_fraction(start, duration);
            let b = self
                .snapshot
                .loop_end
                .map_or(a, |end| timeline_fraction(end, duration));
            div()
                .absolute()
                .top(px(-2.))
                .left(relative(a))
                .w(relative((b - a).max(0.)))
                .min_w(px(2.))
                .h(px(8.))
                .rounded(px(2.))
                .border_1()
                .border_color(t.accent)
                .bg(t.accent_tint(0.35))
        });
        div()
            .id("timeline")
            .relative()
            .h(px(20.))
            .flex_1()
            .cursor_pointer()
            .flex()
            .items_center()
            .child(
                canvas(move |bounds, _, _| timeline.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgba(0xffffff40))
                    .child(div().w(relative(progress)).h_full().rounded_full().bg(fill))
                    .children(chapters)
                    .children(loop_span)
                    .when_some(knob, |d, knob| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(2. - knob_size / 2.))
                                .left(relative(progress))
                                .ml(px(-knob_size / 2.))
                                .child(knob),
                        )
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.control_pressed = true;
                    this.scrubbing = true;
                    this.seek_at(e.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.scrubbing {
                    this.seek_at(e.position.x, cx);
                }
                this.hover_timeline(e.position.x, cx);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered {
                    this.hover = None;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.scrubbing = false),
            )
            .when_some(self.timeline_preview(cx), |d, preview| d.child(preview))
    }

    /// The frame and time under the pointer, above the timeline.
    fn timeline_preview(&self, cx: &Context<Self>) -> Option<Div> {
        let (fraction, time) = self.hover?;
        let t = cx.theme();
        let width = 192.;
        let chapter = self
            .snapshot
            .chapters
            .iter()
            .rev()
            .find(|chapter| chapter.time <= time && !chapter.title.is_empty())
            .map(|chapter| chapter.title.clone());
        Some(
            div()
                .absolute()
                .bottom(px(22.))
                .left(relative(fraction))
                .ml(px(-width / 2. - 4.))
                .child(
                    v_flex()
                        .w(px(width + 8.))
                        .p_1()
                        .gap_1()
                        .items_center()
                        .rounded(px(t.radius + 2.))
                        .bg(rgba(0x000000d9))
                        .border_1()
                        .border_color(rgba(0xffffff26))
                        .shadow_lg()
                        .text_color(rgb(ON_VIDEO))
                        .when_some(self.preview_image.clone(), |d, image| {
                            d.child(
                                img(image)
                                    .w(px(width))
                                    .h(px(width * 9. / 16.))
                                    .rounded(px(t.radius))
                                    .object_fit(ObjectFit::Contain),
                            )
                        })
                        .when_some(chapter, |d, chapter| {
                            d.child(
                                div()
                                    .max_w(px(width))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(t.font(11.))
                                    .child(chapter),
                            )
                        })
                        .child(
                            div()
                                .text_size(t.font(11.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format_time(time)),
                        ),
                ),
        )
    }

    fn volume_slider(&self, width: f32, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let volume_bounds = self.volume_bounds.clone();
        let volume = (self.snapshot.volume / 100.).clamp(0., 1.) as f32;
        let fill = if self.snapshot.muted {
            rgba(0xffffff66)
        } else if t.style == Style::Mac {
            rgb(ON_VIDEO)
        } else {
            t.accent
        };
        div()
            .id("volume-slider")
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(20.))
            .cursor_pointer()
            .flex()
            .items_center()
            .tooltip(|_, cx| cx.new(|_| Hint("Volume".into())).into())
            .child(
                canvas(
                    move |bounds, _, _| volume_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .w_full()
                    .h(px(3.))
                    .rounded_full()
                    .bg(rgba(0xffffff40))
                    .child(div().w(relative(volume)).h_full().rounded_full().bg(fill)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.control_pressed = true;
                    this.changing_volume = true;
                    this.volume_at(e.position.x, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if this.changing_volume {
                    this.volume_at(e.position.x, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.changing_volume = false),
            )
    }

    fn speed_button(&self, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let el = div()
            .id("speed")
            .relative()
            .flex_none()
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .rounded(px(t.radius))
            .cursor_pointer()
            .text_size(t.font(12.))
            .text_color(rgb(ON_VIDEO))
            .hover(|s| s.bg(rgba(ON_VIDEO_HOVER)))
            .tooltip(|_, cx| {
                cx.new(|_| Hint("Playback speed · click to cycle".into()))
                    .into()
            })
            .child(format!("{}×", self.snapshot.speed))
            .on_click(cx.listener(|this, _, window, cx| this.handle(Control::Speed, window, cx)));
        self.focusable(
            el,
            &"speed".into(),
            Some("Playback speed".into()),
            t.radius,
            (0., 0.),
            cx,
        )
    }

    fn player_controls(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme();
        let fullscreen = window.is_fullscreen();
        let position = format_time(self.snapshot.position);
        let duration = format_time(self.snapshot.duration);
        let remaining = format!(
            "-{}",
            format_time((self.snapshot.duration - self.snapshot.position).max(0.))
        );
        let time = |text: String| {
            div()
                .flex_none()
                .min_w(px(44.))
                .text_size(t.font(11.5))
                .text_color(rgba(0xffffffc0))
                .child(text)
        };
        let mute = self.overlay_button(
            "mute",
            if self.snapshot.muted {
                "volume-x"
            } else {
                "volume-2"
            },
            if self.snapshot.muted {
                "Unmute (M)"
            } else {
                "Mute (M)"
            },
            Control::Mute,
            cx,
        );
        let back = self.overlay_button(
            "back",
            "rotate-ccw",
            "Back 10 seconds (←)",
            Control::Back,
            cx,
        );
        let forward = self.overlay_button(
            "forward",
            "rotate-cw",
            "Forward 10 seconds (→)",
            Control::Forward,
            cx,
        );
        let tracks = self.overlay_button(
            "tracks",
            "captions",
            "Audio & subtitles",
            Control::Tracks,
            cx,
        );
        let pip = self.overlay_button(
            "pip",
            "picture-in-picture",
            "Picture in picture (P)",
            Control::Pip,
            cx,
        );
        let full = self.overlay_button(
            "fullscreen",
            if fullscreen { "minimize" } else { "maximize" },
            if fullscreen {
                "Exit fullscreen (F)"
            } else {
                "Fullscreen (F)"
            },
            Control::Fullscreen,
            cx,
        );
        let swallow =
            cx.listener(|this, _: &gpui::MouseDownEvent, _, _| this.control_pressed = true);
        match t.style {
            Style::Mac => h_flex()
                .absolute()
                .bottom(px(24.))
                .left_0()
                .right_0()
                .justify_center()
                .child(
                    v_flex()
                        .id("player-hud")
                        .w(relative(0.72))
                        .max_w(px(640.))
                        .px_4()
                        .pt_2()
                        .pb_3()
                        .gap_1()
                        .rounded(px(18.))
                        .bg(rgba(0x1c1c1ec7))
                        .border_1()
                        .border_color(rgba(0xffffff1f))
                        .shadow_lg()
                        .text_color(rgb(ON_VIDEO))
                        .on_mouse_down(MouseButton::Left, swallow)
                        .child(
                            h_flex()
                                .child(
                                    h_flex()
                                        .w(px(164.))
                                        .gap_1()
                                        .child(mute)
                                        .child(self.volume_slider(72., cx)),
                                )
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .justify_center()
                                        .gap_3()
                                        .child(back)
                                        .child(self.play_button(cx))
                                        .child(forward),
                                )
                                .child(
                                    h_flex()
                                        .w(px(164.))
                                        .justify_end()
                                        .gap_1()
                                        .child(self.speed_button(cx))
                                        .child(tracks)
                                        .child(pip)
                                        .child(full),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(time(position))
                                .child(self.timeline_bar(cx))
                                .child(time(remaining).text_right()),
                        ),
                )
                .into_any_element(),
            Style::Windows => {
                let title = self.path.as_deref().map(file_title).unwrap_or_default();
                v_flex()
                    .id("player-bar")
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .px_5()
                    .pt_3()
                    .pb_3()
                    .gap_2()
                    .bg(rgba(0x202020e6))
                    .border_t_1()
                    .border_color(rgba(0xffffff17))
                    .text_color(rgb(ON_VIDEO))
                    .on_mouse_down(MouseButton::Left, swallow)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(time(position))
                            .child(self.timeline_bar(cx))
                            .child(time(duration).text_right()),
                    )
                    .child(
                        h_flex()
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .gap_3()
                                    .child(match self.path.as_deref() {
                                        Some(path) => self.poster(path, &title, 44., 44., 4., cx),
                                        None => thumbnail(&title, 44., 44., 4.),
                                    })
                                    .child(
                                        v_flex()
                                            .min_w(px(0.))
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .child(title),
                                            )
                                            .child(
                                                div()
                                                    .text_size(t.font(12.))
                                                    .text_color(rgba(0xffffffb3))
                                                    .child("Now playing"),
                                            ),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(back)
                                    .child(self.play_button(cx))
                                    .child(forward),
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .justify_end()
                                    .gap_1()
                                    .child(self.speed_button(cx))
                                    .child(tracks)
                                    .child(pip)
                                    .child(mute)
                                    .child(self.volume_slider(88., cx))
                                    .child(full),
                            ),
                    )
                    .into_any_element()
            }
            Style::Gnome => h_flex()
                .id("player-osd")
                .absolute()
                .bottom(px(12.))
                .left(px(12.))
                .right(px(12.))
                .h(px(54.))
                .px_2()
                .gap_2()
                .rounded(px(14.))
                .bg(rgba(0x000000bf))
                .text_color(rgb(ON_VIDEO))
                .on_mouse_down(MouseButton::Left, swallow)
                .child(self.play_button(cx))
                .child(back)
                .child(forward)
                .child(time(position))
                .child(self.timeline_bar(cx))
                .child(time(duration).text_right())
                .child(self.speed_button(cx))
                .child(mute)
                .child(self.volume_slider(72., cx))
                .child(tracks)
                .child(pip)
                .child(full)
                .into_any_element(),
        }
    }

    /// The next episode's countdown, with Play Now and Cancel.
    fn up_next_card(&self, cx: &Context<Self>) -> Option<Div> {
        let (path, started) = self.up_next.as_ref()?;
        let t = cx.theme();
        let left = super::UP_NEXT_DELAY
            .saturating_sub(started.elapsed())
            .as_secs_f32()
            .ceil();
        let next = path.clone();
        Some(
            v_flex()
                .w(px(300.))
                .gap_2()
                .p_3()
                .rounded(px(t.card_radius))
                .bg(t.popover)
                .border_1()
                .border_color(t.border)
                .shadow_lg()
                .text_color(t.text)
                .child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child(format!("Up next in {left:.0} s")),
                )
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(file_title(path)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(self.button(
                            "up-next-cancel",
                            None,
                            "Cancel",
                            false,
                            cx,
                            |this, _, cx| {
                                this.up_next = None;
                                cx.notify();
                            },
                        ))
                        .child(self.button(
                            "up-next-play",
                            Some("play"),
                            "Play Now",
                            true,
                            cx,
                            move |this, w, cx| {
                                this.persist();
                                this.open(next.clone(), w, cx);
                            },
                        )),
                ),
        )
    }

    /// Audio delay, adjustable in 50 ms steps, with a reset.
    fn audio_delay_row(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        let delay = self.snapshot.audio_delay;
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .child(
                div()
                    .flex_1()
                    .child(format!("Audio delay {:+.0} ms", delay * 1000.)),
            )
            .child(self.icon_button(
                "audio-earlier",
                "minus",
                "Audio earlier by 50 ms",
                cx,
                |this, w, cx| this.shortcut(Shortcut::AudioDelayDown, w, cx),
            ))
            .child(self.icon_button(
                "audio-later",
                "plus",
                "Audio later by 50 ms",
                cx,
                |this, w, cx| this.shortcut(Shortcut::AudioDelayUp, w, cx),
            ))
            .when(delay.abs() > 0.0005, |d| {
                d.child(self.icon_button(
                    "audio-delay-reset",
                    "rotate-ccw",
                    "Reset audio delay",
                    cx,
                    |this, _, cx| this.property("audio-delay", "0", cx),
                ))
            })
            .text_color(t.text)
    }

    fn menu_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let t = cx.theme().clone();
        let title = match menu {
            Menu::Tracks => "Audio & subtitles",
            Menu::Keys => "Keyboard shortcuts",
        };
        let option = |id: SharedString, label: String, selected: bool| {
            let id = ElementId::from(id);
            let el = h_flex()
                .id(id.clone())
                .relative()
                .gap_2()
                .p_2()
                .rounded(px(t.radius))
                .cursor_pointer()
                .hover(|s| s.bg(t.selection))
                .child(
                    div()
                        .size(px(16.))
                        .when(selected, |d| d.child(icon("check", 16., t.accent))),
                )
                .child(label);
            self.focusable(el, &id, None, t.radius, (-2., -2.), cx)
        };
        let mut panel = v_flex()
            .id("panel")
            .occlude()
            .p_1p5()
            .w(px(300.))
            .max_h(px(380.))
            .overflow_y_scroll()
            .bg(t.popover)
            .border_1()
            .border_color(t.border)
            .rounded(px(t.card_radius))
            .shadow_lg()
            .text_color(t.text)
            .text_size(t.font(13.))
            .child(
                div()
                    .px_2()
                    .py_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            );
        match menu {
            Menu::Tracks => {
                for (kind, title) in [("audio", "Audio"), ("sub", "Subtitles")] {
                    panel = panel.child(
                        div()
                            .px_2()
                            .pt_2()
                            .pb_1()
                            .text_size(t.font(11.))
                            .text_color(t.muted)
                            .child(title),
                    );
                    for track in self
                        .snapshot
                        .tracks
                        .iter()
                        .filter(|track| track.kind == kind)
                    {
                        let id = track.id.to_string();
                        let property = if kind == "audio" { "aid" } else { "sid" };
                        let label = if !track.title.is_empty() {
                            track.title.clone()
                        } else if !track.language.is_empty() {
                            track.language.clone()
                        } else {
                            format!("Track {}", track.id)
                        };
                        panel = panel.child(
                            option(format!("{property}-{id}").into(), label, track.selected)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.property(property, &id, cx)
                                })),
                        );
                    }
                    if kind == "audio" {
                        panel = panel.child(self.audio_delay_row(cx));
                    }
                    if kind == "sub" {
                        let off = !self
                            .snapshot
                            .tracks
                            .iter()
                            .any(|t| t.kind == "sub" && t.selected);
                        panel = panel.child(option("subs-off".into(), "Off".into(), off).on_click(
                            cx.listener(|this, _, _, cx| this.property("sid", "no", cx)),
                        ));
                    }
                }
                panel = panel.child(
                    div()
                        .mt_2()
                        .pt_2()
                        .px_1()
                        .pb_1()
                        .border_t_1()
                        .border_color(t.border)
                        .child(self.button(
                            "load-subtitle",
                            Some("captions"),
                            "Load Subtitle File…",
                            false,
                            cx,
                            |this, w, cx| this.handle(Control::Subtitle, w, cx),
                        )),
                );
            }
            Menu::Keys => {
                let bindings = &self.store.data.shortcuts;
                let rows = Shortcut::ALL
                    .into_iter()
                    .filter(|shortcut| !bindings.binding(*shortcut).is_empty())
                    .map(|shortcut| {
                        (
                            crate::shortcuts::display(bindings.binding(shortcut)),
                            shortcut.label(),
                        )
                    })
                    .chain([("0 – 9".to_owned(), "Jump to 0–90%")]);
                for (key, action) in rows {
                    panel = panel.child(
                        h_flex()
                            .justify_between()
                            .gap_4()
                            .px_2()
                            .py_1p5()
                            .child(action)
                            .child(div().text_size(t.font(12.)).text_color(t.muted).child(key)),
                    );
                }
                panel = panel.child(
                    div()
                        .px_2()
                        .pt_2()
                        .text_size(t.font(11.))
                        .text_color(t.muted)
                        .child("Change these in Keyboard Shortcuts."),
                );
            }
        }
        Some(panel.into_any_element())
    }
}
