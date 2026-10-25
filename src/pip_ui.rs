//! Picture in picture: the playing video in a small window that stays above
//! other windows, so the library stays usable while playback continues.
//!
//! GPUI 0.2.2 cannot change a window's level after creation, so this is a
//! separate pop-up window: a floating panel on macOS, a tool window on
//! Windows, and a notification-type window on X11, which window managers
//! keep above others. Wayland has no always-on-top request; there it is an
//! ordinary small window. The main view keeps the player; this window draws
//! its frames and asks it to render at this window's size.

use super::*;
use gpui::{
    App, WeakEntity, WindowBounds, WindowDecorations, WindowHandle, WindowKind, WindowOptions,
    point, size,
};

const PIP_SIZE: (f32, f32) = (480., 270.);
const PIP_MARGIN: f32 = 24.;

pub(super) struct PipView {
    flick: WeakEntity<Flick>,
    focus: FocusHandle,
    hovered: bool,
}

impl Flick {
    /// Opens picture in picture, or returns to the player if it is open.
    pub(super) fn toggle_pip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pip.is_some() {
            self.close_pip(true, cx);
            return;
        }
        if self.path.is_none() || !self.file_loaded {
            return;
        }
        let display = window.display(cx);
        let area = display
            .as_ref()
            .map(|display| display.bounds())
            .unwrap_or_else(|| window.bounds());
        let (width, height) = PIP_SIZE;
        let origin = point(
            area.right() - px(width + PIP_MARGIN),
            area.bottom() - px(height + PIP_MARGIN * 2.),
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                size(px(width), px(height)),
            ))),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: false,
            is_movable: true,
            is_resizable: true,
            is_minimizable: false,
            display_id: display.as_ref().map(|display| display.id()),
            window_min_size: Some(size(px(240.), px(135.))),
            window_decorations: Some(WindowDecorations::Client),
            app_id: Some("flick".into()),
            ..Default::default()
        };
        let flick = cx.entity().downgrade();
        // The window's first render reads this view, so open it after this
        // update finishes.
        App::defer(cx, move |cx| {
            let view = flick.clone();
            let opened = cx.open_window(options, |window, cx| {
                cx.new(|cx| PipView::new(view, window, cx))
            });
            let _ = flick.update(cx, |this, cx| {
                match opened {
                    Ok(handle) => {
                        this.pip = Some(handle);
                        this.show_status("Playing in picture in picture");
                    }
                    Err(error) => {
                        this.error = Some(format!("Picture in picture could not open. {error:#}"));
                    }
                }
                cx.notify();
            });
        });
    }

    /// Closes the window. With `restore`, the main window shows the player.
    pub(super) fn close_pip(&mut self, restore: bool, cx: &mut Context<Self>) {
        let Some(handle) = self.pip.take() else {
            return;
        };
        // Frames shared with the window leave its sprite atlas with it.
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        if restore && self.path.is_some() {
            self.view = View::Player;
            if let Some(main) = self.main_window {
                let _ = main.update(cx, |_, window, _| window.activate_window());
            }
        }
        cx.notify();
    }

    /// Releases a replaced frame from the picture-in-picture window too.
    pub(super) fn drop_pip_image(&self, image: &Arc<RenderImage>, cx: &mut App) {
        if let Some(handle) = self.pip {
            let image = image.clone();
            let _ = handle.update(cx, |_, window, _| window.drop_image(image));
        }
    }

    /// The physical size frames render at while picture in picture is open.
    pub(super) fn pip_target(&self, cx: &mut App) -> Option<(u32, u32)> {
        let handle = self.pip?;
        let (bounds, scale) = handle
            .update(cx, |_, window, _| {
                (window.viewport_size(), window.scale_factor())
            })
            .ok()?;
        Some(flick::presentation::target_size(
            f32::from(bounds.width),
            f32::from(bounds.height),
            scale,
        ))
    }
}

impl PipView {
    fn new(flick: WeakEntity<Flick>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if let Some(flick) = flick.upgrade() {
            // Redraw whenever the player publishes a frame or state.
            cx.observe(&flick, |_, _, cx| cx.notify()).detach();
        }
        window.set_window_title("Picture in Picture — Flick");
        let focus = cx.focus_handle();
        focus.focus(window);
        Self {
            flick,
            focus,
            hovered: false,
        }
    }

    fn flick(&self, cx: &mut App, f: impl FnOnce(&mut Flick, &mut Context<Flick>)) {
        if let Some(flick) = self.flick.upgrade() {
            flick.update(cx, f);
        }
    }
}

impl Render for PipView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (frame, playing, title) = self
            .flick
            .upgrade()
            .map(|flick| {
                let flick = flick.read(cx);
                (
                    flick.frame.clone(),
                    flick.playing(),
                    flick.path.as_deref().map(file_title).unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        let t = cx.theme().clone();
        let control = |id: &'static str, name: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(36.))
                .rounded_full()
                .bg(rgba(0x00000099))
                .hover(|s| s.bg(rgba(0x000000cc)))
                .cursor_pointer()
                .tooltip(move |_, cx| cx.new(|_| Hint(label.into())).into())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(icon(name, 18., rgb(0xffffff)))
        };
        div()
            .id("pip")
            .relative()
            .size_full()
            .bg(rgb(0x000000))
            .font_family(t.font.clone())
            .text_color(rgb(0xffffff))
            .track_focus(&self.focus)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hovered = *hovered;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "space" => this.flick(cx, |flick, cx| {
                        flick.command(&["cycle", "pause"], cx);
                    }),
                    "escape" => this.flick(cx, |flick, cx| flick.close_pip(true, cx)),
                    _ => return,
                }
                let _ = window;
                cx.stop_propagation();
            }))
            .when_some(frame, |d, frame| {
                d.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            })
            .when(self.hovered, |d| {
                d.child(
                    v_flex()
                        .absolute()
                        .inset_0()
                        .justify_between()
                        .p_2()
                        .bg(rgba(0x00000040))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_size(t.font(12.))
                                        .child(title),
                                )
                                .child(
                                    control("pip-return", "maximize", "Back to Flick (Esc)")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.flick(cx, |flick, cx| flick.close_pip(true, cx))
                                        })),
                                ),
                        )
                        .child(
                            h_flex().justify_center().child(
                                control(
                                    "pip-play",
                                    if playing { "pause" } else { "play" },
                                    if playing {
                                        "Pause (Space)"
                                    } else {
                                        "Play (Space)"
                                    },
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.flick(cx, |flick, cx| {
                                            flick.command(&["cycle", "pause"], cx);
                                        })
                                    },
                                )),
                            ),
                        ),
                )
            })
    }
}

impl Focusable for PipView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub(super) type PipHandle = WindowHandle<PipView>;
