//! First-launch onboarding: four screens that pair a library, offer to share
//! this one, choose where downloads go, and confirm the seeding choice.
//! Design: `docs/REMOTE_DOWNLOADS_PLAN.md`. Owned by the onboarding agent.
//!
//! `Flick` holds one [`OnboardingState`] and renders [`Flick::render_onboarding`]
//! while `self.view == View::Onboarding`. [`needs_setup`] decides whether a
//! launch starts here; "Set up Flick again" calls [`Flick::start_onboarding`].
//!
//! Every step writes through the same calls its settings page makes, as soon
//! as the user changes something. Skip puts back what the step had when it
//! was entered, so skipping sets nothing. The step logic is in plain
//! functions at the bottom of this file, with tests.

use super::{
    chrome_ui::{group, notice, pressed, row},
    *,
};
use crate::remote_client::{ServerView, Status};
use flick::remote::protocol::Capability;
use gpui::FontWeight;

/// The four screens, in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    /// Where are your videos?
    #[default]
    Source,
    /// Share this library?
    Share,
    /// Where should downloads go?
    Destination,
    /// You're set.
    Done,
}

impl Step {
    pub const ALL: [Step; 4] = [Step::Source, Step::Share, Step::Destination, Step::Done];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    fn after(self) -> Step {
        Self::ALL
            .get(self.index() + 1)
            .copied()
            .unwrap_or(Step::Done)
    }

    fn before(self) -> Step {
        Self::ALL[self.index().saturating_sub(1)]
    }

    fn title(self) -> &'static str {
        match self {
            Step::Source => "Where are your videos?",
            Step::Share => "Share this library?",
            Step::Destination => "Where should downloads go?",
            Step::Done => "You're set.",
        }
    }
}

/// The answer to "Where are your videos?".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    ThisComputer,
    /// On another machine on the tailnet, paired with its string.
    Remote,
}

/// Where the user is in onboarding. `reviewing` is set by a summary row's
/// Change: Next, Skip, and Back then return to the summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub step: Step,
    pub reviewing: bool,
}

impl Nav {
    /// Next and Skip.
    pub fn forward(self) -> Nav {
        Nav {
            step: if self.reviewing {
                Step::Done
            } else {
                self.step.after()
            },
            reviewing: false,
        }
    }

    pub fn back(self) -> Nav {
        Nav {
            step: if self.reviewing {
                Step::Done
            } else {
                self.step.before()
            },
            reviewing: false,
        }
    }

    /// A summary row's Change.
    pub fn change(step: Step) -> Nav {
        Nav {
            step,
            reviewing: step != Step::Done,
        }
    }
}

/// What a step started with, for Skip to put back.
#[derive(Clone, Debug, Default, PartialEq)]
struct Snapshot {
    servers: Vec<String>,
    serve: bool,
    default_destination: String,
    seed_completed: Option<bool>,
}

impl Snapshot {
    fn of(settings: &crate::store::Settings) -> Self {
        Self {
            servers: settings
                .remote
                .servers
                .iter()
                .map(|server| server.id.clone())
                .collect(),
            serve: settings.remote.serve,
            default_destination: settings.remote.default_destination.clone(),
            seed_completed: settings.transfers.seed_completed,
        }
    }
}

#[derive(Default)]
pub struct OnboardingState {
    pub nav: Nav,
    pub source: Source,
    /// The pairing string field, created on first render of step one.
    pairing: Option<Entity<TextInput>>,
    /// Why the last pairing string was refused.
    pairing_error: Option<String>,
    /// The server the last Test connection added or updated.
    tested: Option<String>,
    snapshot: Snapshot,
    /// Keeps keyboard focus inside the card, so Enter and Escape reach it.
    focus: Option<FocusHandle>,
}

/// A launch starts in onboarding until the last screen marked setup done,
/// unless this Flick already has a library or a paired server from before
/// onboarding existed.
pub fn needs_setup(settings: &crate::store::Settings) -> bool {
    !settings.remote.setup_done && settings.library.is_empty() && settings.remote.servers.is_empty()
}

/// "This Mac" on macOS, "This computer" elsewhere.
fn this_computer() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This computer"
    }
}

impl Flick {
    /// Switches to the onboarding view at its first step. Called by
    /// "Set up Flick again" in Settings. Clears `setup_done`, so quitting
    /// halfway comes back here on an empty library.
    #[allow(dead_code)]
    pub(crate) fn start_onboarding(&mut self, cx: &mut Context<Self>) {
        let focus = self.onboarding.focus.take();
        self.onboarding = OnboardingState {
            focus,
            ..Default::default()
        };
        // Already paired: step one starts on that server and its status.
        if let Some(first) = self.store.data.remote.servers.first() {
            self.onboarding.source = Source::Remote;
            self.onboarding.tested = Some(first.id.clone());
            self.remote
                .refresh_server(&self.store.data, &first.id.clone());
        }
        self.onboarding.snapshot = Snapshot::of(&self.store.data);
        self.store.data.remote.setup_done = false;
        self.persist();
        self.view = View::Onboarding;
        cx.notify();
    }

    /// Leaves onboarding for the library, marking setup as done.
    pub(crate) fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        self.store.data.remote.setup_done = true;
        self.persist();
        let focus = self.onboarding.focus.take();
        // Drops the pasted token from the UI.
        self.onboarding = OnboardingState {
            focus,
            ..Default::default()
        };
        self.view = View::Library;
        cx.notify();
    }

    /// Open Library: finishes, returns focus to the window, and shows Home.
    fn open_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_onboarding(cx);
        self.focus.focus(window);
        self.show_page(Page::Home, window, cx);
    }

    fn go(&mut self, nav: Nav, cx: &mut Context<Self>) {
        self.onboarding.nav = nav;
        self.onboarding.snapshot = Snapshot::of(&self.store.data);
        cx.notify();
    }

    fn onboarding_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nav = self.onboarding.nav;
        if !self.step_valid(nav.step) {
            return;
        }
        match nav.step {
            Step::Done => return self.open_library(window, cx),
            // "On this Mac" keeps no server that Test connection just added.
            Step::Source if self.onboarding.source == Source::ThisComputer => {
                self.forget_tested_server()
            }
            _ => {}
        }
        self.go(nav.forward(), cx);
    }

    /// Skip: puts back what this step started with, then moves on.
    fn onboarding_skip(&mut self, cx: &mut Context<Self>) {
        let nav = self.onboarding.nav;
        let snapshot = self.onboarding.snapshot.clone();
        match nav.step {
            Step::Source => {
                self.forget_tested_server();
                self.onboarding.pairing_error = None;
            }
            Step::Share => {
                if self.store.data.remote.serve != snapshot.serve {
                    self.set_serving(snapshot.serve, cx);
                }
            }
            Step::Destination => {
                self.store.data.remote.default_destination = snapshot.default_destination;
                self.persist();
                if self.store.data.transfers.seed_completed != snapshot.seed_completed {
                    self.store.data.transfers.seed_completed = snapshot.seed_completed;
                    self.update_transfer_preferences(cx);
                }
            }
            Step::Done => return,
        }
        self.go(nav.forward(), cx);
    }

    /// Unpairs the server Test connection added during this step, if it was
    /// not paired before the step began.
    fn forget_tested_server(&mut self) {
        if let Some(id) = self.onboarding.tested.take() {
            if self.onboarding.snapshot.servers.contains(&id) {
                self.onboarding.tested = Some(id);
            } else {
                self.remote.remove_server(&mut self.store.data, &id);
                self.persist();
            }
        }
    }

    fn tested_view(&self) -> Option<ServerView> {
        let id = self.onboarding.tested.as_ref()?;
        self.remote
            .servers(&self.store.data)
            .into_iter()
            .find(|server| &server.id == id)
    }

    fn step_valid(&self, step: Step) -> bool {
        match step {
            Step::Source => source_valid(
                self.onboarding.source,
                self.tested_view().as_ref().map(|server| &server.status),
            ),
            Step::Share | Step::Done => true,
            Step::Destination => destination_valid(
                &destinations(&self.remote.servers(&self.store.data)),
                &self.store.data.remote.default_destination,
                self.store.data.transfers.seed_completed,
            ),
        }
    }

    /// Adds the pasted server (which also asks it for its listing), or
    /// explains why the string was refused.
    fn test_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.onboarding.pairing.clone() else {
            return;
        };
        let text = field.read(cx).value().trim().to_owned();
        match self.remote.add_pairing(&mut self.store.data, &text) {
            Ok(id) => {
                // A different server from an earlier test in this step.
                if self.onboarding.tested.as_ref() != Some(&id) {
                    self.forget_tested_server();
                }
                self.onboarding.tested = Some(id);
                self.onboarding.pairing_error = None;
                self.persist();
            }
            Err(error) => self.onboarding.pairing_error = Some(error),
        }
        cx.notify();
    }

    fn pairing_field(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(field) = &self.onboarding.pairing {
            return field.clone();
        }
        let field = cx.new(|cx| TextInput::with_placeholder(cx, "flick-remote://…"));
        cx.subscribe(&field, |this, _, event, cx| {
            match event {
                InputEvent::Changed => this.onboarding.pairing_error = None,
                InputEvent::Submit => this.test_pairing(cx),
            }
            cx.notify();
        })
        .detach();
        self.onboarding.pairing = Some(field.clone());
        field
    }

    fn onboarding_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.platform || modifiers.alt {
            return;
        }
        // A focused control activates itself; the field handles its own keys.
        let on_control = window
            .focused(cx)
            .is_some_and(|focused| self.is_control_handle(&focused));
        let in_field = self
            .onboarding
            .pairing
            .as_ref()
            .is_some_and(|field| field.read(cx).focused(window));
        match keystroke.key.as_str() {
            "enter" if !on_control && !in_field => {
                cx.stop_propagation();
                self.onboarding_next(window, cx);
            }
            "escape" if !in_field => {
                cx.stop_propagation();
                self.onboarding_skip(cx);
            }
            "tab" | "enter" | "escape" => {}
            _ if !on_control => {
                // The window's key handler would read a player shortcut
                // here, such as F for full screen or a digit for a seek, and
                // a handled key never reaches the text field. Keep those keys
                // from it, typing them into the pairing field ourselves.
                let binding = crate::shortcuts::normalize(keystroke);
                let shortcut = crate::shortcuts::percent_key(&binding).is_some()
                    || self.store.data.shortcuts.lookup(&binding).is_some();
                if !shortcut {
                    return;
                }
                cx.stop_propagation();
                if in_field
                    && let Some(text) = keystroke.key_char.clone()
                    && let Some(field) = self.onboarding.pairing.clone()
                {
                    field.update(cx, |field, cx| {
                        gpui::EntityInputHandler::replace_text_in_range(
                            field, None, &text, window, cx,
                        )
                    });
                }
            }
            _ => {}
        }
    }

    pub(crate) fn render_onboarding(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = cx.theme().clone();
        let focus = self
            .onboarding
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        // Clicks put focus back on the window; take it so keys reach the card.
        let focused = window.focused(cx);
        if focused.is_none() || focused.as_ref() == Some(&self.focus) {
            let focus = focus.clone();
            cx.defer_in(window, move |_, window, _| focus.focus(window));
        }
        let step = self.onboarding.nav.step;
        let body = match step {
            Step::Source => self.source_step(cx),
            Step::Share => self.share_step(cx),
            Step::Destination => self.destination_step(cx),
            Step::Done => self.done_step(cx),
        };
        let card = v_flex()
            .w(px(560.))
            .max_w_full()
            .gap_4()
            .p(px(match t.style {
                Style::Mac => 24.,
                Style::Windows => 28.,
                Style::Gnome => 30.,
            }))
            .rounded(px(t.card_radius.max(10.)))
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .shadow_md()
            .child(step_dots(&t, step))
            .child(
                div()
                    .text_size(t.font(match t.style {
                        Style::Gnome => 24.,
                        _ => 22.,
                    }))
                    .font_weight(FontWeight::BOLD)
                    .child(step.title()),
            )
            .child(body)
            .child(self.onboarding_actions(cx));
        let titlebar = self.titlebar_area(
            div()
                .id("onboarding-titlebar")
                .flex()
                .flex_none()
                .items_center()
                .justify_end()
                .w_full()
                .h(px(t.titlebar_height))
                .children(self.window_controls(window, false, cx)),
            cx,
        );
        v_flex()
            .id("onboarding")
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::onboarding_key))
            .size_full()
            .bg(t.window)
            .text_color(t.text)
            .child(titlebar)
            .child(
                div()
                    .id("onboarding-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .flex_grow()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .px_4()
                            .pb(px(t.titlebar_height))
                            .child(card),
                    ),
            )
            .into_any_element()
    }

    /// Back, Skip, and Next (Open Library on the last step).
    fn onboarding_actions(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        let nav = self.onboarding.nav;
        let pill = |b: Stateful<Div>| {
            if t.style == Style::Gnome {
                b.rounded_full().px_5()
            } else {
                b
            }
        };
        let mut actions = h_flex().mt_2().gap_2();
        if nav.step != Step::Source {
            actions = actions.child(pill(self.button(
                "onboarding-back",
                Some("arrow-left"),
                "Back",
                false,
                cx,
                |this, _, cx| {
                    let back = this.onboarding.nav.back();
                    this.go(back, cx);
                },
            )));
        }
        actions = actions.child(div().flex_1());
        if nav.step == Step::Done {
            return actions.child(pill(self.button(
                "onboarding-open",
                Some("library"),
                "Open Library",
                true,
                cx,
                |this, window, cx| this.open_library(window, cx),
            )));
        }
        actions = actions.child(pill(self.button(
            "onboarding-skip",
            None,
            "Skip",
            false,
            cx,
            |this, _, cx| this.onboarding_skip(cx),
        )));
        let label = if nav.reviewing { "Done" } else { "Next" };
        actions.child(if self.step_valid(nav.step) {
            pill(self.button(
                "onboarding-next",
                None,
                label,
                true,
                cx,
                |this, window, cx| this.onboarding_next(window, cx),
            ))
            .into_any_element()
        } else {
            disabled_button(t, label).into_any_element()
        })
    }

    /// A selectable option with a title and detail, like a radio row.
    #[allow(clippy::too_many_arguments)]
    fn choice(
        &self,
        id: &'static str,
        icon_name: &'static str,
        title: &'static str,
        detail: &'static str,
        selected: bool,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let t = cx.theme();
        let id = ElementId::from(id);
        let radius = match t.style {
            Style::Mac => 8.,
            Style::Windows => 6.,
            Style::Gnome => 12.,
        };
        let el = div()
            .id(id.clone())
            .relative()
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .px_3()
            .py(px(10.))
            .rounded(px(radius))
            .border_1()
            .border_color(if selected { t.accent } else { t.border })
            .bg(if selected {
                t.accent_tint(0.12)
            } else {
                t.control
            })
            .cursor_pointer()
            .hover(|s| s.bg(t.control_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                on_click(this, window, cx);
            }))
            .child(icon(
                icon_name,
                20.,
                if selected { t.accent } else { t.muted },
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap_0p5()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(
                        div()
                            .text_size(t.font(t.text_size - 1.5))
                            .text_color(t.muted)
                            .child(detail),
                    ),
            )
            .when(selected, |d| d.child(icon("check", 16., t.accent)));
        self.focusable(el, &id, None, radius, (0., 0.), cx)
    }

    fn source_step(&mut self, cx: &mut Context<Self>) -> Div {
        let source = self.onboarding.source;
        let remote = source == Source::Remote;
        let field = remote.then(|| self.pairing_field(cx));
        let t = cx.theme().clone();
        let mut body = v_flex()
            .gap_2()
            .child(
                div()
                    .text_color(t.muted)
                    .child("Flick plays videos on this computer, or from another Flick on your Tailscale network."),
            )
            .child(self.choice(
                "onboarding-source-local",
                "monitor",
                if cfg!(target_os = "macos") {
                    "On this Mac"
                } else {
                    "On this computer"
                },
                "Add folders or videos once you are in. You can download and share here too.",
                !remote,
                cx,
                |this, _, cx| {
                    this.onboarding.source = Source::ThisComputer;
                    cx.notify();
                },
            ))
            .child(self.choice(
                "onboarding-source-remote",
                "library",
                "On another machine on my Tailscale network",
                "Paste the pairing string shown in that Flick's Settings → Remote Libraries.",
                remote,
                cx,
                |this, window, cx| {
                    this.onboarding.source = Source::Remote;
                    let field = this.pairing_field(cx);
                    field.focus_handle(cx).focus(window);
                    cx.notify();
                },
            ));
        if let Some(field) = field {
            body = body.child(
                h_flex()
                    .mt_2()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.text_field(&field, cx)),
                    )
                    .child(self.button(
                        "onboarding-test",
                        Some("rotate-cw"),
                        "Test Connection",
                        false,
                        cx,
                        |this, _, cx| this.test_pairing(cx),
                    )),
            );
            if let Some(error) = self.onboarding.pairing_error.clone() {
                body = body.child(notice(&t, true, error));
            } else if let Some(server) = self.tested_view() {
                let online = server.status.is_online();
                body = body.child(
                    h_flex()
                        .gap_2()
                        .text_size(t.font(t.text_size - 1.))
                        .text_color(if online { t.text } else { t.muted })
                        .child(icon(
                            if online { "check" } else { "info" },
                            16.,
                            if online { t.accent } else { t.muted },
                        ))
                        .child(format!(
                            "{} at {} · {}",
                            server.name,
                            server.address,
                            server.summary()
                        )),
                );
            }
        }
        body
    }

    fn share_step(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let (rows, error) = self.serve_rows(cx);
        v_flex()
            .gap_2()
            .child(div().text_color(t.muted).child(
                "Let your other Flicks play this library over Tailscale. They pair with the string shown here once sharing starts.",
            ))
            .child(group(&t, rows))
            .when_some(error, |d, error| d.child(notice(&t, true, error)))
    }

    fn destination_step(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let list = destinations(&self.remote.servers(&self.store.data));
        let chosen = self.store.data.remote.default_destination.clone();
        let detail = list
            .iter()
            .find(|destination| destination.id == chosen && destination.unavailable.is_none())
            .map(destination_detail)
            .unwrap_or_else(|| "Choose where new downloads are saved.".into());
        let seed = self.store.data.transfers.seed_completed;
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_color(t.muted)
                    .child("New torrents and link downloads go here unless you choose otherwise when you start one."),
            )
            .child(h_flex().child(self.destination_picker(&list, &chosen, cx)))
            .child(
                div()
                    .text_size(t.font(t.text_size - 1.5))
                    .text_color(t.muted)
                    .child(detail),
            )
            .child(
                div()
                    .mt_3()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Keep sharing completed downloads?"),
            )
            .child(
                div()
                    .text_size(t.font(t.text_size - 1.5))
                    .text_color(t.muted)
                    .child("Sharing uploads to other peers while Flick is open. Until you choose, completed downloads pause. Files you explicitly share keep sharing."),
            )
            .child(h_flex().child(self.segmented(
                "onboarding-seed",
                &[
                    (Some(true), "Keep sharing completed downloads"),
                    (Some(false), "Stop sharing"),
                ],
                seed,
                cx,
                |this, choice, _, cx| {
                    this.store.data.transfers.seed_completed = choice;
                    this.update_transfer_preferences(cx);
                    cx.notify();
                },
            )))
    }

    /// The Save to picker: a segmented control in the selected style, with
    /// machines that cannot take downloads greyed out and not focusable.
    fn destination_picker(&self, list: &[Destination], chosen: &str, cx: &Context<Self>) -> Div {
        let t = cx.theme();
        let count = list.len();
        let mut picker = h_flex().flex_wrap();
        picker = match t.style {
            Style::Mac => picker.p(px(2.)).gap(px(2.)).rounded(px(7.)).bg(t.control),
            Style::Windows => picker.gap_1(),
            Style::Gnome => picker.gap(px(1.)),
        };
        for (index, destination) in list.iter().enumerate() {
            let on = destination.id == chosen && destination.unavailable.is_none();
            let item_id = ElementId::from(("onboarding-destination", index));
            let mut item = div()
                .id(item_id.clone())
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .gap_1p5()
                .whitespace_nowrap()
                .child(destination.name.clone());
            let radius = match t.style {
                Style::Mac => 5.,
                Style::Windows => 4.,
                Style::Gnome => 8.,
            };
            if let Some(reason) = &destination.unavailable {
                item = item.text_color(t.muted).opacity(0.6).child(
                    div()
                        .text_size(t.font(t.text_size - 2.))
                        .child(format!("· {reason}")),
                );
            } else {
                let id = destination.id.clone();
                item = item
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.control_pressed = true),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.data.remote.default_destination = id.clone();
                        this.persist();
                        cx.notify();
                    }));
                item = self.focusable(item, &item_id, None, radius, (0., 0.), cx);
            }
            picker = picker.child(match t.style {
                Style::Mac => item
                    .h(px(22.))
                    .px_3()
                    .rounded(px(5.))
                    .text_size(t.font(12.))
                    .when(on, |d| d.bg(pressed(t)).shadow_xs()),
                Style::Windows => item
                    .h(px(34.))
                    .px_3()
                    .rounded(px(4.))
                    .when(destination.unavailable.is_none(), |d| {
                        d.text_color(if on { t.text } else { t.muted })
                            .hover(|s| s.bg(t.control_hover))
                    })
                    .when(on, |d| {
                        d.font_weight(FontWeight::SEMIBOLD).child(
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
                    .when(destination.unavailable.is_none(), |d| {
                        d.hover(|s| s.bg(t.control_hover))
                    })
                    .when(index == 0, |d| d.rounded_l(px(8.)))
                    .when(index + 1 == count, |d| d.rounded_r(px(8.))),
            });
        }
        picker
    }

    fn done_step(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let tested = match self.onboarding.source {
            Source::Remote => self.tested_view(),
            Source::ThisComputer => None,
        };
        let list = destinations(&self.remote.servers(&self.store.data));
        let sharing = match self.serve.status() {
            crate::remote_serve::ServeStatus::Running { listen, .. } => {
                Some(format!("On · Sharing on {listen}"))
            }
            crate::remote_serve::ServeStatus::Failed(message) => Some(format!("On · {message}")),
            _ => None,
        };
        let rows = summary(&self.store.data, tested.as_ref(), sharing, &list)
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let step = line.step;
                row(
                    &t,
                    Some(line.icon),
                    line.title,
                    Some(line.value.into()),
                    self.button(
                        ("onboarding-change", index),
                        None,
                        "Change",
                        false,
                        cx,
                        move |this, _, cx| this.go(Nav::change(step), cx),
                    ),
                )
                .into_any_element()
            })
            .collect();
        v_flex()
            .gap_2()
            .child(div().text_color(t.muted).child(
                "Everything here is also in Settings, and you can run this setup again from there.",
            ))
            .child(group(&t, rows))
    }
}

/// One dot per step; the current one is longer and in the accent color.
fn step_dots(t: &Theme, step: Step) -> Div {
    let current = step.index();
    let mut dots = h_flex().gap_1p5();
    for index in 0..Step::ALL.len() {
        dots = dots.child(
            div()
                .h(px(6.))
                .w(px(if index == current { 18. } else { 6. }))
                .rounded_full()
                .bg(if index <= current {
                    t.accent
                } else {
                    t.control_hover
                }),
        );
    }
    dots
}

/// Next while its step is not answered yet: dimmed and not focusable.
fn disabled_button(t: &Theme, label: &'static str) -> Div {
    let height = match t.style {
        Style::Mac => 26.,
        Style::Windows => 32.,
        Style::Gnome => 34.,
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .h(px(height))
        .px(px(if t.style == Style::Mac { 10. } else { 14. }))
        .when(t.style == Style::Gnome, |d| {
            d.rounded_full().px_5().font_weight(FontWeight::BOLD)
        })
        .when(t.style != Style::Gnome, |d| d.rounded(px(t.radius)))
        .bg(t.control)
        .text_color(t.muted)
        .opacity(0.6)
        .child(label)
}

/// Whether step one can go on: this computer always, another machine once
/// its connection test reached it.
pub fn source_valid(source: Source, tested: Option<&Status>) -> bool {
    match source {
        Source::ThisComputer => true,
        Source::Remote => tested.is_some_and(Status::is_online),
    }
}

/// A machine in the Save to picker. `id` is empty for this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct Destination {
    pub id: String,
    pub name: String,
    /// Why it cannot be chosen, shown greyed beside its name.
    pub unavailable: Option<String>,
    pub free_bytes: Option<u64>,
    pub folder: Option<String>,
}

/// This computer, then every paired server. A server can be chosen only
/// when its listing advertises `Downloads`; before a listing arrives it
/// shows its status instead.
pub fn destinations(servers: &[ServerView]) -> Vec<Destination> {
    let mut list = vec![Destination {
        id: String::new(),
        name: this_computer().into(),
        unavailable: None,
        free_bytes: None,
        folder: None,
    }];
    for server in servers {
        let (unavailable, storage) = match &server.listing {
            Some(listing) if listing.capabilities.contains(&Capability::Downloads) => {
                (None, listing.storage.clone())
            }
            Some(_) => (Some("Doesn't accept downloads".to_owned()), None),
            None => (Some(server.status.label()), None),
        };
        list.push(Destination {
            id: server.id.clone(),
            name: server.name.clone(),
            unavailable,
            free_bytes: storage.as_ref().map(|storage| storage.free_bytes),
            folder: storage
                .map(|storage| storage.download_folder_name)
                .filter(|name| !name.is_empty()),
        });
    }
    list
}

fn destination_detail(destination: &Destination) -> String {
    if destination.id.is_empty() {
        return if cfg!(target_os = "macos") {
            "Downloads are saved on this Mac.".into()
        } else {
            "Downloads are saved on this computer.".into()
        };
    }
    let mut detail = match &destination.folder {
        Some(folder) => format!("Saved in {folder} on {}", destination.name),
        None => format!("Saved on {}", destination.name),
    };
    if let Some(free) = destination.free_bytes {
        detail.push_str(&format!(" · {} free", flick::library::format_bytes(free)));
    }
    detail
}

/// Whether step three can go on: the default is a machine that can take
/// downloads, and the seeding question has an answer.
pub fn destination_valid(list: &[Destination], chosen: &str, seed: Option<bool>) -> bool {
    seed.is_some()
        && list
            .iter()
            .any(|destination| destination.id == chosen && destination.unavailable.is_none())
}

/// A line on the last screen. Change jumps to `step`.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryLine {
    pub step: Step,
    pub icon: &'static str,
    pub title: &'static str,
    pub value: String,
}

/// What onboarding set, one line per question. `tested` is the server step
/// one paired when the videos are on another machine; `sharing` is the
/// running server's status line when sharing is on.
pub fn summary(
    settings: &crate::store::Settings,
    tested: Option<&ServerView>,
    sharing: Option<String>,
    destinations: &[Destination],
) -> Vec<SummaryLine> {
    let library = match tested {
        Some(server) => format!("{} · {}", server.name, server.summary()),
        None => match settings.library.len() {
            0 => this_computer().to_owned(),
            1 => format!("{} · 1 video", this_computer()),
            count => format!("{} · {count} videos", this_computer()),
        },
    };
    let sharing = if settings.remote.serve {
        sharing.unwrap_or_else(|| "On · Starting…".into())
    } else {
        "Off".into()
    };
    let chosen = &settings.remote.default_destination;
    let downloads = destinations
        .iter()
        .find(|destination| &destination.id == chosen)
        .map(|destination| destination.name.clone())
        .unwrap_or_else(|| this_computer().into());
    let seeding = match settings.transfers.seed_completed {
        Some(true) => "Keep sharing completed downloads",
        Some(false) => "Stop sharing when complete",
        None => "Not chosen · completed downloads pause",
    };
    vec![
        SummaryLine {
            step: Step::Source,
            icon: "library",
            title: "Videos",
            value: library,
        },
        SummaryLine {
            step: Step::Share,
            icon: "share-2",
            title: "Share this library",
            value: sharing,
        },
        SummaryLine {
            step: Step::Destination,
            icon: "download",
            title: "Downloads go to",
            value: downloads,
        },
        SummaryLine {
            step: Step::Destination,
            icon: "share-2",
            title: "After downloading",
            value: seeding.into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use flick::remote::protocol::{Library, Storage, Video};
    use std::sync::Arc;

    fn server(id: &str, status: Status, listing: Option<Library>) -> ServerView {
        ServerView {
            id: id.into(),
            name: format!("Server {id}"),
            address: "100.64.0.1:47117".into(),
            status,
            listing: listing.map(Arc::new),
            quality: Quality::Original,
        }
    }

    fn listing(capabilities: Vec<Capability>, videos: usize) -> Library {
        Library {
            capabilities,
            storage: Some(Storage {
                free_bytes: 2048,
                download_folder_name: "Downloads".into(),
            }),
            videos: vec![Video::default(); videos],
            ..Default::default()
        }
    }

    #[test]
    fn next_and_skip_walk_the_steps_in_order() {
        let mut nav = Nav::default();
        let mut seen = vec![nav.step];
        for _ in 0..5 {
            nav = nav.forward();
            seen.push(nav.step);
        }
        assert_eq!(
            seen,
            [
                Step::Source,
                Step::Share,
                Step::Destination,
                Step::Done,
                Step::Done,
                Step::Done
            ]
        );
        assert_eq!(nav.back().step, Step::Destination);
        assert_eq!(Nav::default().back().step, Step::Source);
    }

    #[test]
    fn change_from_the_summary_returns_to_it() {
        let nav = Nav::change(Step::Share);
        assert_eq!(nav.step, Step::Share);
        assert!(nav.reviewing);
        assert_eq!(nav.forward(), Nav::change(Step::Done));
        assert_eq!(nav.back().step, Step::Done);
        assert!(!Nav::change(Step::Done).reviewing);
        // Once back on the summary, the next Change starts fresh.
        assert!(!nav.forward().reviewing);
    }

    #[test]
    fn step_one_needs_an_online_server_for_another_machine() {
        assert!(source_valid(Source::ThisComputer, None));
        assert!(source_valid(Source::ThisComputer, Some(&Status::Offline)));
        assert!(!source_valid(Source::Remote, None));
        assert!(!source_valid(Source::Remote, Some(&Status::Connecting)));
        assert!(!source_valid(Source::Remote, Some(&Status::WrongToken)));
        assert!(!source_valid(Source::Remote, Some(&Status::Offline)));
        assert!(source_valid(Source::Remote, Some(&Status::Online)));
    }

    #[test]
    fn only_servers_that_accept_downloads_can_be_chosen() {
        let servers = [
            server(
                "a",
                Status::Online,
                Some(listing(vec![Capability::Downloads], 0)),
            ),
            server(
                "b",
                Status::Online,
                Some(listing(vec![Capability::Links, Capability::Shares], 0)),
            ),
            server("c", Status::Offline, None),
        ];
        let list = destinations(&servers);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].id, "");
        assert_eq!(list[0].name, this_computer());
        assert_eq!(list[0].unavailable, None);
        assert_eq!(list[1].unavailable, None);
        assert_eq!(list[1].free_bytes, Some(2048));
        assert_eq!(list[1].folder.as_deref(), Some("Downloads"));
        assert_eq!(
            list[2].unavailable.as_deref(),
            Some("Doesn't accept downloads")
        );
        assert_eq!(list[2].free_bytes, None);
        assert_eq!(list[3].unavailable.as_deref(), Some("Offline"));
        assert_eq!(destinations(&[]).len(), 1);
    }

    #[test]
    fn step_three_needs_an_available_machine_and_a_seeding_answer() {
        let list = destinations(&[
            server(
                "a",
                Status::Online,
                Some(listing(vec![Capability::Downloads], 0)),
            ),
            server("b", Status::Online, Some(listing(Vec::new(), 0))),
        ]);
        assert!(destination_valid(&list, "", Some(true)));
        assert!(destination_valid(&list, "a", Some(false)));
        assert!(!destination_valid(&list, "", None));
        assert!(!destination_valid(&list, "b", Some(true)));
        assert!(!destination_valid(&list, "forgotten", Some(true)));
    }

    #[test]
    fn destination_details_name_the_folder_and_free_space() {
        let list = destinations(&[server(
            "a",
            Status::Online,
            Some(listing(vec![Capability::Downloads], 0)),
        )]);
        let detail = destination_detail(&list[1]);
        assert!(
            detail.starts_with("Saved in Downloads on Server a · "),
            "{detail}"
        );
        assert!(detail.ends_with(" free"), "{detail}");
        assert!(destination_detail(&list[0]).starts_with("Downloads are saved on this "));
    }

    #[test]
    fn summary_lines_follow_the_settings() {
        let mut settings = crate::store::Settings::default();
        let list = destinations(&[server(
            "a",
            Status::Online,
            Some(listing(vec![Capability::Downloads], 2)),
        )]);
        let values = |settings: &crate::store::Settings,
                      tested: Option<&ServerView>,
                      sharing: Option<String>| {
            summary(settings, tested, sharing, &list)
                .into_iter()
                .map(|line| (line.step, line.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values(&settings, None, None),
            [
                (Step::Source, this_computer().to_owned()),
                (Step::Share, "Off".to_owned()),
                (Step::Destination, this_computer().to_owned()),
                (
                    Step::Destination,
                    "Not chosen · completed downloads pause".to_owned()
                ),
            ]
        );
        settings.remote.serve = true;
        settings.remote.default_destination = "a".into();
        settings.transfers.seed_completed = Some(false);
        let tested = server(
            "a",
            Status::Online,
            Some(listing(vec![Capability::Downloads], 2)),
        );
        assert_eq!(
            values(&settings, Some(&tested), None),
            [
                (Step::Source, "Server a · Online · 2 videos".to_owned()),
                (Step::Share, "On · Starting…".to_owned()),
                (Step::Destination, "Server a".to_owned()),
                (Step::Destination, "Stop sharing when complete".to_owned()),
            ]
        );
        let running = values(
            &settings,
            None,
            Some("On · Sharing on 100.64.0.2:47117".into()),
        );
        assert_eq!(running[1].1, "On · Sharing on 100.64.0.2:47117");
        settings.transfers.seed_completed = Some(true);
        assert_eq!(
            values(&settings, None, None)[3].1,
            "Keep sharing completed downloads"
        );
    }

    #[test]
    fn a_snapshot_records_what_skip_puts_back() {
        let mut settings = crate::store::Settings::default();
        settings
            .remote
            .servers
            .push(crate::store::remote::RemoteServer {
                id: "a".into(),
                ..Default::default()
            });
        settings.remote.serve = true;
        settings.remote.default_destination = "a".into();
        settings.transfers.seed_completed = Some(true);
        assert_eq!(
            Snapshot::of(&settings),
            Snapshot {
                servers: vec!["a".into()],
                serve: true,
                default_destination: "a".into(),
                seed_completed: Some(true),
            }
        );
    }

    #[test]
    fn setup_is_needed_only_for_a_new_empty_flick() {
        let mut settings = crate::store::Settings::default();
        assert!(needs_setup(&settings));
        settings.remote.setup_done = true;
        assert!(!needs_setup(&settings));
        settings.remote.setup_done = false;
        settings.remote.servers.push(Default::default());
        assert!(!needs_setup(&settings));
    }
}
