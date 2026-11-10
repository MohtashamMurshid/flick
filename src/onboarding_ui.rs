//! First-launch onboarding: four screens that pair a library, offer to share
//! this one, choose where downloads go, and confirm the seeding choice.
//! Design: `docs/REMOTE_DOWNLOADS_PLAN.md`. Owned by the onboarding agent.
//!
//! `Flick` holds one [`OnboardingState`] and renders [`Flick::render_onboarding`]
//! while `self.view == View::Onboarding`. [`needs_setup`] decides whether a
//! launch starts here; "Set up Flick again" calls [`Flick::start_onboarding`].

use super::*;

#[derive(Default)]
pub struct OnboardingState {
    /// Zero-based screen, 0 to 3.
    #[allow(dead_code)]
    pub step: usize,
}

/// A launch starts in onboarding until the last screen marked setup done,
/// unless this Flick already has a library or a paired server from before
/// onboarding existed.
pub fn needs_setup(settings: &crate::store::Settings) -> bool {
    !settings.remote.setup_done && settings.library.is_empty() && settings.remote.servers.is_empty()
}

impl Flick {
    /// Switches to the onboarding view at its first step. Called by
    /// "Set up Flick again" in Appearance.
    #[allow(dead_code)]
    pub(crate) fn start_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding = OnboardingState::default();
        self.view = View::Onboarding;
        cx.notify();
    }

    /// Leaves onboarding for the library, marking setup as done.
    pub(crate) fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        self.store.data.remote.setup_done = true;
        self.persist();
        self.view = View::Library;
        cx.notify();
    }

    /// Placeholder until the onboarding agent lands the four screens: a
    /// title and a Skip that marks setup done.
    pub(crate) fn render_onboarding(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = cx.theme().clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .items_center()
            .justify_center()
            .bg(t.window)
            .text_color(t.text)
            .child("Welcome to Flick")
            .child(
                div()
                    .id("onboarding-skip")
                    .px_3()
                    .py_1()
                    .rounded(px(t.radius))
                    .bg(t.selection)
                    .cursor_pointer()
                    .child("Skip setup")
                    .on_click(cx.listener(|this, _, _, cx| this.finish_onboarding(cx))),
            )
            .into_any_element()
    }
}
