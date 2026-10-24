//! Playback view: the video surface with style-specific overlay controls.

use super::{
    chrome_ui::{notice, thumbnail, traffic_light_space},
    *,
};
use gpui::{AnyElement, FontWeight, Rgba, linear_color_stop, linear_gradient};

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
        let show = self.controls_shown;
        let controls_height = controls_height(t.style);
        let menu = self.menu_panel(cx);
        div()
            .relative()
            .size_full()
            .bg(rgb(0x000000))
            .child(self.video_surface(cx))
            .when(show, |d| d.child(self.player_titlebar(window, cx)))
            .when(show && self.file_loaded, |d| {
                d.child(self.player_controls(window, cx))
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
            .when_some(self.frame.clone(), |d, frame| {
                d.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            })
            .when(self.frame.is_none(), |d| {
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
                                .text_size(px(20.))
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
            cx,
            move |this, w, cx| this.handle(control, w, cx),
        )
    }

    fn player_titlebar(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let title = self.path.as_deref().map(file_title).unwrap_or_default();
        let back = self.overlay_button(
            "player-back",
            "arrow-left",
            "Back to Library",
            Control::Library,
            cx,
        );
        let keys = self.overlay_button(
            "player-keys",
            "keyboard",
            "Keyboard shortcuts (?)",
            Control::Keys,
            cx,
        );
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
                        .text_size(px(12.))
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
        div()
            .id("play")
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
            ))
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
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.scrubbing = false),
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
        div()
            .id("speed")
            .flex_none()
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .rounded(px(cx.theme().radius))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(ON_VIDEO))
            .hover(|s| s.bg(rgba(ON_VIDEO_HOVER)))
            .tooltip(|_, cx| {
                cx.new(|_| Hint("Playback speed · click to cycle".into()))
                    .into()
            })
            .child(format!("{}×", self.snapshot.speed))
            .on_click(cx.listener(|this, _, window, cx| this.handle(Control::Speed, window, cx)))
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
                .text_size(px(11.5))
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
                        .max_w(px(580.))
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
                                        .w(px(130.))
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
                                        .w(px(130.))
                                        .justify_end()
                                        .gap_1()
                                        .child(self.speed_button(cx))
                                        .child(tracks)
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
                                    .child(thumbnail(&title, 44., 44., 4.))
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
                                                    .text_size(px(12.))
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
                .child(full)
                .into_any_element(),
        }
    }

    fn menu_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let t = cx.theme().clone();
        let title = match menu {
            Menu::Tracks => "Audio & subtitles",
            Menu::Keys => "Keyboard shortcuts",
        };
        let option = |id: SharedString, label: String, selected: bool| {
            h_flex()
                .id(id)
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
                .child(label)
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
            .text_size(px(13.))
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
                            .text_size(px(11.))
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
                let command = if cfg!(target_os = "macos") {
                    "⌘"
                } else {
                    "Ctrl+"
                };
                for (key, action) in [
                    ("Space".to_owned(), "Play / pause"),
                    ("←  →".to_owned(), "Seek 10 seconds"),
                    ("↑  ↓".to_owned(), "Change volume"),
                    ("F / Esc".to_owned(), "Fullscreen / exit"),
                    ("M".to_owned(), "Mute"),
                    (format!("{command}O"), "Open video"),
                    (format!("{command}L"), "Load subtitles"),
                    (format!("{command}K"), "Search"),
                    (format!("{command},"), "Appearance"),
                ] {
                    panel = panel.child(
                        h_flex()
                            .justify_between()
                            .gap_4()
                            .px_2()
                            .py_1p5()
                            .child(action)
                            .child(div().text_size(px(12.)).text_color(t.muted).child(key)),
                    );
                }
            }
        }
        Some(panel.into_any_element())
    }
}
