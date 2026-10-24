//! Window chrome and controls drawn in the selected platform style.

use super::*;
use gpui::{
    AnyElement, App, CursorStyle, Decorations, FontWeight, HitboxBehavior, Hsla, ResizeEdge, Rgba,
    Tiling, WindowControlArea, anchored, deferred, hsla, linear_color_stop, linear_gradient, point,
    transparent_black,
};

/// Shadow space around client-decorated Linux windows.
const WINDOW_SHADOW: f32 = 10.;

/// A slider's change handler.
type ValueAction = dyn Fn(&mut Flick, f32, &mut Window, &mut Context<Flick>);

/// Space between a control and its keyboard focus ring.
const RING_GAP: f32 = 2.;

/// Apple's minimum comfortable hit target. Small icon buttons keep their
/// visual size and extend their hit area toward it.
const MIN_HIT_TARGET: f32 = 44.;

pub(super) fn h_flex() -> Div {
    div().flex().flex_row().items_center()
}

pub(super) fn v_flex() -> Div {
    div().flex().flex_col()
}

/// Horizontal space kept free for the native macOS traffic lights.
pub(super) fn traffic_light_space() -> f32 {
    if cfg!(target_os = "macos") { 76. } else { 0. }
}

/// Whether Flick draws minimize, maximize, and close itself.
pub(super) fn draws_window_controls(window: &Window) -> bool {
    !cfg!(target_os = "macos")
        && (cfg!(target_os = "windows")
            || matches!(window.window_decorations(), Decorations::Client { .. }))
}

/// Pressed and selected fill for toggles.
pub(super) fn pressed(t: &Theme) -> Rgba {
    match t.style {
        Style::Mac if !t.dark => rgb(0xffffff),
        _ => Rgba {
            a: if t.dark { 0.24 } else { 0.18 },
            ..t.text
        },
    }
}

fn window_action(area: WindowControlArea, window: &mut Window) {
    match area {
        WindowControlArea::Min => window.minimize_window(),
        WindowControlArea::Max => window.zoom_window(),
        WindowControlArea::Close => window.remove_window(),
        WindowControlArea::Drag => {}
    }
}

/// A placeholder thumbnail with a stable color derived from the title.
pub(super) fn thumbnail(title: &str, width: f32, height: f32, radius: f32) -> Div {
    let hash = title
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (hash % 360) as f32 / 360.;
    let shade = ((hash / 360 % 120) as f32 + 180.) % 360. / 360.;
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(width))
        .h(px(height))
        .rounded(px(radius))
        .overflow_hidden()
        .bg(linear_gradient(
            155.,
            linear_color_stop(hsla(hue, 0.55, 0.42, 1.), 0.),
            linear_color_stop(hsla((hue + shade * 0.3) % 1., 0.5, 0.14, 1.), 1.),
        ))
        .child(icon(
            "film",
            (height * 0.28).clamp(14., 32.),
            hsla(0., 0., 1., 0.55),
        ))
}

impl Flick {
    /// The focus handle that puts the control with this ID in the Tab order.
    pub(super) fn control_handle(&self, id: &ElementId, cx: &App) -> FocusHandle {
        self.control_focus
            .borrow_mut()
            .entry(id.clone())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    pub(super) fn is_control_handle(&self, handle: &FocusHandle) -> bool {
        self.control_focus.borrow().values().any(|h| h == handle)
    }

    pub(super) fn control_has_focus(&self) -> bool {
        self.focused
            .as_ref()
            .is_some_and(|focused| self.is_control_handle(focused))
    }

    /// Whether the control shows its focus ring and focus tooltip.
    pub(super) fn keyboard_focused(&self, id: &ElementId, cx: &App) -> bool {
        self.keyboard_nav && self.focused.as_ref() == Some(&self.control_handle(id, cx))
    }

    /// Puts a control in the Tab order. GPUI activates focused click targets
    /// with Enter and Space. While navigating by keyboard, the focused control
    /// shows a ring `offset` outside its bounds (negative for hit-area
    /// padding) and its tooltip. Mouse presses keep focus on the window, so
    /// Space keeps controlling playback after clicking a control.
    pub(super) fn focusable(
        &self,
        el: Stateful<Div>,
        id: &ElementId,
        tooltip: Option<SharedString>,
        radius: f32,
        offset: (f32, f32),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let focused = self.keyboard_focused(id, cx);
        let (x, y) = (offset.0 + RING_GAP, offset.1 + RING_GAP);
        el.track_focus(&self.control_handle(id, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    this.control_pressed = true;
                    window.prevent_default();
                    this.focus.focus(window);
                }),
            )
            .when(focused, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(-y))
                        .bottom(px(-y))
                        .left(px(-x))
                        .right(px(-x))
                        .rounded(px(radius + RING_GAP))
                        .border_2()
                        .border_color(t.focus_ring),
                )
            })
            .when_some(tooltip.filter(|_| focused), |d, tooltip| {
                d.child(
                    div().absolute().left_0().top(relative(1.)).child(
                        deferred(
                            anchored()
                                .snap_to_window_with_margin(px(8.))
                                .child(hint_box(t, tooltip).mt(px(4. - offset.1))),
                        )
                        .with_priority(2),
                    ),
                )
            })
    }

    /// A text button. Primary buttons use the accent color.
    pub(super) fn button(
        &self,
        id: impl Into<ElementId>,
        icon_name: Option<&'static str>,
        label: impl Into<SharedString>,
        primary: bool,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let (fg, bg, hover) = if primary {
            (t.on_accent, t.accent, t.accent_tint(0.82))
        } else {
            (t.text, t.control, t.control_hover)
        };
        let height = match t.style {
            Style::Mac => 26.,
            Style::Windows => 32.,
            Style::Gnome => 34.,
        };
        let id = id.into();
        let el = div()
            .id(id.clone())
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .h(px(height))
            .px(px(if t.style == Style::Mac { 10. } else { 14. }))
            .rounded(px(t.radius))
            .bg(bg)
            .text_color(fg)
            .whitespace_nowrap()
            .when(t.style == Style::Gnome, |d| d.font_weight(FontWeight::BOLD))
            .when(t.style == Style::Windows && !primary, |d| {
                d.border_1().border_color(t.border)
            })
            .when(t.style == Style::Mac && !primary, |d| d.shadow_xs())
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.control_pressed = true),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                on_click(this, window, cx);
            }))
            .when_some(icon_name, |d, name| d.child(icon(name, 16., fg)))
            .child(label.into());
        self.focusable(el, &id, None, t.radius, (0., 0.), cx)
    }

    /// A borderless icon button with a tooltip.
    pub(super) fn icon_button(
        &self,
        id: impl Into<ElementId>,
        name: &'static str,
        tooltip: impl Into<SharedString>,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let size = match t.style {
            Style::Mac => 28.,
            Style::Windows => 34.,
            Style::Gnome => 34.,
        };
        self.tinted_icon_button(
            id,
            name,
            tooltip,
            size,
            t.text,
            t.control_hover,
            None,
            cx,
            on_click,
        )
    }

    /// An icon button that shows its pressed state, such as a search toggle.
    pub(super) fn icon_toggle(
        &self,
        id: impl Into<ElementId>,
        name: &'static str,
        tooltip: impl Into<SharedString>,
        on: bool,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let size = match t.style {
            Style::Mac => 28.,
            Style::Windows | Style::Gnome => 34.,
        };
        self.tinted_icon_button(
            id,
            name,
            tooltip,
            size,
            t.text,
            t.control_hover,
            on.then(|| pressed(t).into()),
            cx,
            on_click,
        )
    }

    /// An icon button with a tooltip. Its hit area extends vertically toward
    /// [`MIN_HIT_TARGET`] while the visible button keeps its size. Toolbars
    /// and the player have room above and below their buttons; extending
    /// sideways would overlap neighbors, and GPUI 0.2.2's layout collapses
    /// flex rows whose children have negative horizontal margins.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tinted_icon_button(
        &self,
        id: impl Into<ElementId>,
        name: &'static str,
        tooltip: impl Into<SharedString>,
        size: f32,
        fg: impl Into<Hsla>,
        hover: impl Into<Hsla>,
        fill: Option<Hsla>,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let id = id.into();
        let tooltip = tooltip.into();
        let hover = hover.into();
        let radius = if t.style == Style::Gnome {
            8.
        } else {
            t.radius
        };
        let pad_y = hit_padding(size);
        let group = SharedString::from(format!("icon-button-{id}"));
        let hint = tooltip.clone();
        let el = div()
            .id(id.clone())
            .group(group.clone())
            .relative()
            .flex_none()
            .w(px(size))
            .h(px(size + pad_y * 2.))
            .my(px(-pad_y))
            .py(px(pad_y))
            .cursor_pointer()
            .tooltip(move |_, cx| cx.new(|_| Hint(hint.clone())).into())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.control_pressed = true),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                on_click(this, window, cx);
            }))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius))
                    .when_some(fill, |d, fill| d.bg(fill))
                    .group_hover(group, move |s| s.bg(hover))
                    .child(icon(name, (size * 0.53).round(), fg)),
            );
        self.focusable(el, &id, Some(tooltip), radius, (0., -pad_y), cx)
    }

    /// Makes an element behave like a native title bar: drag to move and
    /// double-click to maximize.
    pub(super) fn titlebar_area(&self, el: Stateful<Div>, cx: &Context<Self>) -> Stateful<Div> {
        el.when(cfg!(target_os = "windows"), |d| {
            d.window_control_area(WindowControlArea::Drag)
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, _| {
                if !std::mem::take(&mut this.control_pressed) {
                    this.should_move = true;
                }
            }),
        )
        .on_mouse_move(cx.listener(|this, _, window, _| {
            if this.should_move {
                this.should_move = false;
                window.start_window_move();
            }
        }))
        .on_click(|event, window, _| {
            if event.click_count() == 2 {
                if cfg!(target_os = "macos") {
                    window.titlebar_double_click();
                } else {
                    window.zoom_window();
                }
            }
        })
    }

    /// Minimize, maximize, and close, drawn in the selected style when the
    /// platform does not draw them.
    pub(super) fn window_controls(
        &self,
        window: &Window,
        overlay: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !draws_window_controls(window) {
            return None;
        }
        let t = cx.theme();
        let fg = if overlay { rgb(0xffffff) } else { t.text };
        let control = |area: WindowControlArea, id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(cfg!(target_os = "windows"), |d| d.window_control_area(area))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.control_pressed = true),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    window_action(area, window);
                })
        };
        Some(match t.style {
            Style::Windows => {
                let height = t.titlebar_height;
                let hover = if overlay {
                    rgba(0xffffff1f)
                } else {
                    t.control_hover
                };
                h_flex()
                    .h(px(height))
                    .child(
                        control(WindowControlArea::Min, "window-minimize")
                            .w(px(46.))
                            .h_full()
                            .hover(move |s| s.bg(hover))
                            .child(icon("minus", 14., fg)),
                    )
                    .child(
                        control(WindowControlArea::Max, "window-maximize")
                            .w(px(46.))
                            .h_full()
                            .hover(move |s| s.bg(hover))
                            .child(icon("square", 12., fg)),
                    )
                    .child(
                        control(WindowControlArea::Close, "window-close")
                            .w(px(46.))
                            .h_full()
                            .hover(|s| s.bg(rgb(0xc42b1c)).text_color(rgb(0xffffff)))
                            .child(icon("x", 14., fg)),
                    )
                    .into_any_element()
            }
            Style::Mac => {
                let light = |area, id, color: u32| {
                    control(area, id)
                        .size(px(12.))
                        .rounded_full()
                        .bg(rgb(color))
                        .border_1()
                        .border_color(rgba(0x00000033))
                };
                h_flex()
                    .gap_2()
                    .child(light(WindowControlArea::Close, "window-close", 0xff5f57))
                    .child(light(WindowControlArea::Min, "window-minimize", 0xfebc2e))
                    .child(light(WindowControlArea::Max, "window-maximize", 0x28c840))
                    .into_any_element()
            }
            Style::Gnome => {
                let (bg, hover) = if overlay {
                    (rgba(0xffffff26), rgba(0xffffff40))
                } else {
                    (t.control, t.control_hover)
                };
                control(WindowControlArea::Close, "window-close")
                    .size(px(24.))
                    .mx_1()
                    .rounded_full()
                    .bg(bg)
                    .hover(move |s| s.bg(hover))
                    .child(icon("x", 14., fg))
                    .into_any_element()
            }
        })
    }

    /// The search field, styled for the current platform.
    pub(super) fn search_field(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = cx.theme();
        let focused = self.search_input.read(cx).focused(window);
        let search_icon = icon("search", 16., t.muted);
        let field = div()
            .id("search-field")
            .flex()
            .items_center()
            .gap_2()
            .min_w(px(0.))
            .px_2p5()
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.control_pressed = true),
            );
        match t.style {
            Style::Mac => field
                .h(px(28.))
                .rounded(px(8.))
                .bg(t.control)
                .when(focused, |d| d.border_2().border_color(t.accent_tint(0.6)))
                .child(search_icon)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .child(self.search_input.clone()),
                ),
            Style::Windows => field
                .h(px(32.))
                .rounded(px(4.))
                .bg(t.control)
                .border_1()
                .border_color(t.border)
                .when(focused, |d| d.border_b_2().border_color(t.accent))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .child(self.search_input.clone()),
                )
                .child(search_icon),
            Style::Gnome => field
                .h(px(34.))
                .rounded(px(8.))
                .bg(t.control)
                .when(focused, |d| d.border_2().border_color(t.accent_tint(0.5)))
                .child(search_icon)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .child(self.search_input.clone()),
                ),
        }
    }

    /// A selectable, copyable value such as a magnet link or a path.
    pub(super) fn read_only_field(&self, input: &Entity<TextInput>, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        div()
            .flex()
            .items_center()
            .min_w(px(0.))
            .h(px(28.))
            .px_2()
            .rounded(px(t.radius))
            .bg(t.control)
            .border_1()
            .border_color(t.border)
            .text_size(t.font(t.text_size - 1.))
            .child(input.clone())
    }

    /// A segmented control: a macOS segmented control, a Windows selector
    /// bar, or GNOME linked toggle buttons.
    pub(super) fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(T, &'static str)],
        selected: T,
        cx: &Context<Self>,
        on_select: impl Fn(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) -> Div {
        let t = cx.theme();
        let on_select = Rc::new(on_select);
        let count = options.len();
        let mut row = h_flex().flex_none();
        row = match t.style {
            Style::Mac => row.p(px(2.)).gap(px(2.)).rounded(px(7.)).bg(t.control),
            Style::Windows => row.gap_1(),
            Style::Gnome => row.gap(px(1.)),
        };
        for (index, (value, label)) in options.iter().copied().enumerate() {
            let on = value == selected;
            let on_select = on_select.clone();
            let item_id = ElementId::from((id, index));
            let item = div()
                .id(item_id.clone())
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .whitespace_nowrap()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.control_pressed = true),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    on_select(this, value, window, cx);
                }));
            let radius = match t.style {
                Style::Mac => 5.,
                Style::Windows => 4.,
                Style::Gnome => 8.,
            };
            let item = self.focusable(item, &item_id, None, radius, (0., 0.), cx);
            row = row.child(match t.style {
                Style::Mac => item
                    .h(px(22.))
                    .px_3()
                    .rounded(px(5.))
                    .text_size(t.font(12.))
                    .when(on, |d| d.bg(pressed(t)).shadow_xs())
                    .child(label),
                Style::Windows => item
                    .relative()
                    .h(px(34.))
                    .px_3()
                    .rounded(px(4.))
                    .text_color(if on { t.text } else { t.muted })
                    .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .hover(|s| s.bg(t.control_hover))
                    .child(label)
                    .when(on, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom(px(2.))
                                .w(px(16.))
                                .h(px(3.))
                                .rounded_full()
                                .bg(t.accent),
                        )
                    }),
                Style::Gnome => item
                    .h(px(34.))
                    .px_3()
                    .font_weight(FontWeight::BOLD)
                    .bg(if on { pressed(t) } else { t.control })
                    .hover(|s| s.bg(t.control_hover))
                    .when(index == 0, |d| d.rounded_l(px(8.)))
                    .when(index + 1 == count, |d| d.rounded_r(px(8.)))
                    .child(label),
            });
        }
        row
    }

    /// An on/off switch in the selected style.
    pub(super) fn switch(
        &self,
        id: &'static str,
        on: bool,
        cx: &Context<Self>,
        on_toggle: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let (width, height, knob) = match t.style {
            Style::Mac => (34., 20., 16.),
            Style::Windows => (40., 20., 12.),
            Style::Gnome => (46., 26., 22.),
        };
        let inset = (height - knob) / 2.;
        let id = ElementId::from(id);
        let (track, knob_color) = match (t.style, on) {
            (_, true) => (
                t.accent,
                if t.style == Style::Windows {
                    t.on_accent
                } else {
                    rgb(0xffffff)
                },
            ),
            (Style::Windows, false) => (rgba(0x00000000), t.muted),
            (_, false) => (t.control_hover, rgb(0xffffff)),
        };
        let el = div()
            .id(id.clone())
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .w(px(width))
            .h(px(height))
            .rounded_full()
            .bg(track)
            .when(t.style == Style::Windows && !on, |d| {
                d.border_1().border_color(t.muted)
            })
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.control_pressed = true),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                on_toggle(this, window, cx);
            }))
            .child(
                div()
                    .ml(px(if on { width - knob - inset } else { inset }))
                    .size(px(knob))
                    .rounded_full()
                    .bg(knob_color)
                    .when(t.style != Style::Windows, |d| d.shadow_sm()),
            );
        self.focusable(el, &id, None, height / 2., (0., 0.), cx)
    }

    /// A horizontal slider stepping through `range` by `step`. Click or drag
    /// the track, or use the arrow, Home, and End keys while it is focused.
    /// `on_change` runs only when the stepped value changes.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn slider(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        value: f32,
        (min, max): (f32, f32),
        step: f32,
        width: f32,
        cx: &Context<Self>,
        on_change: impl Fn(&mut Self, f32, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let bounds = self
            .slider_bounds
            .borrow_mut()
            .entry(id)
            .or_default()
            .clone();
        let measured = bounds.clone();
        let snap = move |v: f32| ((v.clamp(min, max) - min) / step).round() * step + min;
        let fraction = ((value - min) / (max - min)).clamp(0., 1.);
        let at = move |x: Pixels| {
            let b = bounds.get();
            let width = f32::from(b.size.width).max(1.);
            snap(min + (f32::from(x - b.origin.x) / width).clamp(0., 1.) * (max - min))
        };
        // Report only stepped changes, so dragging saves settings sparingly.
        let on_change: Rc<ValueAction> = Rc::new(move |this, next, window, cx| {
            if (next - value).abs() > step / 4. {
                on_change(this, next, window, cx);
            }
        });
        let (down, moved, keyed) = (on_change.clone(), on_change.clone(), on_change);
        let at_move = at.clone();
        let element_id = ElementId::from(id);
        let knob = 16.;
        let el = div()
            .id(element_id.clone())
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(24.))
            .flex()
            .items_center()
            .cursor_pointer()
            .child(
                canvas(move |b, _, _| measured.set(b), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(t.control_hover)
                    .child(
                        div()
                            .w(relative(fraction))
                            .h_full()
                            .rounded_full()
                            .bg(t.accent),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(2. - knob / 2.))
                            .left(relative(fraction))
                            .ml(px(-knob / 2.))
                            .size(px(knob))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .border_1()
                            .border_color(t.border)
                            .shadow_sm(),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                    this.dragging_slider = Some(id);
                    down(this, at(e.position.x), window, cx);
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, e: &gpui::MouseMoveEvent, window, cx| {
                    if this.dragging_slider == Some(id) {
                        moved(this, at_move(e.position.x), window, cx);
                    }
                }),
            )
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                let next = match e.keystroke.key.as_str() {
                    "left" | "down" => value - step,
                    "right" | "up" => value + step,
                    "home" => min,
                    "end" => max,
                    _ => return,
                };
                cx.stop_propagation();
                this.keyboard_nav = true;
                keyed(this, snap(next), window, cx);
            }));
        self.focusable(el, &element_id, Some(label.into()), 12., (2., -2.), cx)
    }

    /// Wraps the window in a shadow and resize border when Linux uses
    /// client-side decorations.
    pub(super) fn window_frame(
        &self,
        content: AnyElement,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let Decorations::Client { tiling } = window.window_decorations() else {
            window.set_client_inset(px(0.));
            return div().size_full().child(content).into_any_element();
        };
        window.set_client_inset(px(WINDOW_SHADOW));
        let radius = px(t.window_radius);
        let border = t.border;
        div()
            .id("window-frame")
            .size_full()
            .bg(transparent_black())
            .when(!tiling.top, |d| d.pt(px(WINDOW_SHADOW)))
            .when(!tiling.bottom, |d| d.pb(px(WINDOW_SHADOW)))
            .when(!tiling.left, |d| d.pl(px(WINDOW_SHADOW)))
            .when(!tiling.right, |d| d.pr(px(WINDOW_SHADOW)))
            .child(
                canvas(
                    |_, window, _| {
                        window.insert_hitbox(
                            Bounds::new(
                                point(px(0.), px(0.)),
                                window.window_bounds().get_bounds().size,
                            ),
                            HitboxBehavior::Normal,
                        )
                    },
                    move |_, hitbox, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        if let Some(edge) = resize_edge(window.mouse_position(), size, tiling) {
                            window.set_cursor_style(
                                match edge {
                                    ResizeEdge::Top | ResizeEdge::Bottom => {
                                        CursorStyle::ResizeUpDown
                                    }
                                    ResizeEdge::Left | ResizeEdge::Right => {
                                        CursorStyle::ResizeLeftRight
                                    }
                                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                        CursorStyle::ResizeUpLeftDownRight
                                    }
                                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                        CursorStyle::ResizeUpRightDownLeft
                                    }
                                },
                                &hitbox,
                            );
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_move(|_, window, _| window.refresh())
            .on_mouse_down(MouseButton::Left, move |event, window, _| {
                let size = window.window_bounds().get_bounds().size;
                if let Some(edge) = resize_edge(event.position, size, tiling) {
                    window.start_window_resize(edge);
                }
            })
            .child(
                div()
                    .size_full()
                    .overflow_hidden()
                    .cursor(CursorStyle::Arrow)
                    .when(!(tiling.top || tiling.left), |d| d.rounded_tl(radius))
                    .when(!(tiling.top || tiling.right), |d| d.rounded_tr(radius))
                    .when(!(tiling.bottom || tiling.left), |d| d.rounded_bl(radius))
                    .when(!(tiling.bottom || tiling.right), |d| d.rounded_br(radius))
                    .when(!tiling.is_tiled(), |d| {
                        d.border_1().border_color(border).shadow_lg()
                    })
                    .on_mouse_move(|_, _, cx| cx.stop_propagation())
                    .child(content),
            )
            .into_any_element()
    }
}

/// A grouped list: a macOS inset group, Windows settings cards, or a GNOME
/// boxed list.
pub(super) fn group(t: &Theme, rows: Vec<AnyElement>) -> Div {
    let mut list = v_flex().w_full();
    list = match t.style {
        Style::Mac => list
            .rounded(px(10.))
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .overflow_hidden(),
        Style::Windows => list.gap_1(),
        Style::Gnome => list
            .rounded(px(12.))
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .shadow_xs()
            .overflow_hidden(),
    };
    for (index, row) in rows.into_iter().enumerate() {
        let cell = div().w_full();
        list = list.child(match t.style {
            Style::Windows => cell
                .rounded(px(6.))
                .bg(t.surface)
                .border_1()
                .border_color(t.border)
                .child(row),
            _ => cell
                .when(index > 0, |d| d.border_t_1().border_color(t.border))
                .child(row),
        });
    }
    list
}

/// A row in a grouped list. Windows settings cards also show an icon.
pub(super) fn row(
    t: &Theme,
    icon_name: Option<&'static str>,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    trailing: impl IntoElement,
) -> Div {
    let (x, y, min_height) = match t.style {
        Style::Mac => (12., 9., 44.),
        Style::Windows => (16., 12., 64.),
        Style::Gnome => (14., 10., 56.),
    };
    h_flex()
        .w_full()
        .gap_3()
        .px(px(x))
        .py(px(y))
        .min_h(px(min_height))
        .when_some(
            icon_name.filter(|_| t.style == Style::Windows),
            |d, name| d.child(icon(name, 20., t.text)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .gap_0p5()
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(title.into()),
                )
                .when_some(detail, |d, detail| {
                    d.child(
                        div()
                            .text_size(t.font(t.text_size - 1.5))
                            .text_color(t.muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(detail),
                    )
                }),
        )
        .child(trailing)
}

/// A group heading above a grouped list or grid.
pub(super) fn section_title(t: &Theme, text: impl Into<SharedString>) -> Div {
    let heading = div().mb_2();
    match t.style {
        Style::Mac => heading
            .px_1()
            .text_size(t.font(13.))
            .font_weight(FontWeight::SEMIBOLD),
        Style::Windows => heading
            .text_size(t.font(14.))
            .font_weight(FontWeight::SEMIBOLD),
        Style::Gnome => heading.text_size(t.font(15.)).font_weight(FontWeight::BOLD),
    }
    .child(text.into())
}

/// An inline message: an InfoBar on Windows, a banner elsewhere.
pub(super) fn notice(t: &Theme, error: bool, text: impl Into<SharedString>) -> Div {
    let (tint, accent) = if error {
        (
            Rgba {
                a: 0.14,
                ..t.danger
            },
            t.danger,
        )
    } else {
        (t.accent_tint(0.12), t.accent)
    };
    h_flex()
        .w_full()
        .gap_3()
        .px_3()
        .py_2()
        .rounded(px(match t.style {
            Style::Mac => 8.,
            Style::Windows => 6.,
            Style::Gnome => 12.,
        }))
        .bg(tint)
        .border_1()
        .border_color(t.border)
        .child(icon(
            if error { "circle-alert" } else { "info" },
            16.,
            accent,
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(t.font(t.text_size - 1.))
                .child(text.into()),
        )
}

/// Vertical hit-area padding that brings an icon button of `size` to
/// [`MIN_HIT_TARGET`].
fn hit_padding(size: f32) -> f32 {
    ((MIN_HIT_TARGET - size) / 2.).max(0.)
}

fn resize_edge(
    position: gpui::Point<Pixels>,
    size: gpui::Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let shadow = px(WINDOW_SHADOW);
    let corner = px(WINDOW_SHADOW * 1.5);
    let left = position.x < shadow && !tiling.left;
    let right = position.x > size.width - shadow && !tiling.right;
    let top = position.y < shadow && !tiling.top;
    let bottom = position.y > size.height - shadow && !tiling.bottom;
    let near_left = position.x < corner;
    let near_right = position.x > size.width - corner;
    let near_top = position.y < corner;
    let near_bottom = position.y > size.height - corner;
    Some(match () {
        _ if (top && near_left) || (left && near_top) => ResizeEdge::TopLeft,
        _ if (top && near_right) || (right && near_top) => ResizeEdge::TopRight,
        _ if (bottom && near_left) || (left && near_bottom) => ResizeEdge::BottomLeft,
        _ if (bottom && near_right) || (right && near_bottom) => ResizeEdge::BottomRight,
        _ if top => ResizeEdge::Top,
        _ if bottom => ResizeEdge::Bottom,
        _ if left => ResizeEdge::Left,
        _ if right => ResizeEdge::Right,
        _ => return None,
    })
}
