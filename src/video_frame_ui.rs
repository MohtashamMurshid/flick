//! A frame invalidates only this small view, independently of library/chrome.
use gpui::{Context, IntoElement, ObjectFit, Render, RenderImage, Window, div, img, prelude::*};
use std::sync::Arc;

#[derive(Default)]
pub struct VideoFrameView {
    pub frame: Option<Arc<RenderImage>>,
}

impl Render for VideoFrameView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        tracing::trace!("Video surface rendered");
        div().size_full().children(
            self.frame
                .clone()
                .map(|frame| img(frame).size_full().object_fit(ObjectFit::Contain)),
        )
    }
}
