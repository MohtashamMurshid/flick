mod assets;
mod input;
mod model;
mod playback;
mod services;
mod smoke;
mod store;
mod theme;
mod ui;

use gpui::{
    App, AppContext, Application, Bounds, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, point, px, size,
};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flick=info".into()),
        )
        .init();
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if first.as_deref().is_some_and(|s| s == "--help" || s == "-h") {
        println!(
            "Flick — desktop video player\n\nUsage: flick [VIDEO]\n       flick --smoke-test VIDEO\n\nShortcuts: Space play/pause · arrows seek/volume · F fullscreen · M mute\n           Ctrl/Cmd+O open · Ctrl/Cmd+L subtitles · ? shortcuts\n\nRequires libmpv. See README.md for installation."
        );
        return Ok(());
    }
    if first.as_deref().is_some_and(|s| s == "--smoke-test") {
        return smoke::run(&PathBuf::from(args.next().ok_or_else(|| {
            anyhow::anyhow!("Supply a video path after --smoke-test")
        })?));
    }
    let path = first.map(PathBuf::from);
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            input::init(cx);
            if let Err(error) =
                cx.text_system()
                    .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                        "../assets/fonts/Inter.ttf"
                    ))])
            {
                tracing::warn!(%error, "Could not load the bundled Inter font");
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    // GPUI 0.2.2's X11 close callback still holds a platform
                    // RefCell borrow. Quit on the next event-loop turn instead.
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(10))
                            .await;
                        let _ = cx.update(|cx| {
                            if cx.windows().is_empty() {
                                cx.quit();
                            }
                        });
                    })
                    .detach();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(740.)),
                    cx,
                ))),
                window_min_size: Some(size(px(760.), px(500.))),
                // Flick draws its own title bar in the selected platform style.
                // macOS keeps its native traffic lights; Linux uses client-side
                // decorations when the compositor supports them.
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Flick".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(20.), px(20.))),
                }),
                window_decorations: Some(WindowDecorations::Client),
                window_background: if cfg!(target_os = "linux") {
                    WindowBackgroundAppearance::Transparent
                } else {
                    WindowBackgroundAppearance::Opaque
                },
                app_id: Some("flick".into()),
                ..Default::default()
            };
            if let Err(error) = cx.open_window(options, |window, cx| {
                cx.new(|cx| ui::Flick::new(path, window, cx))
            }) {
                tracing::error!(%error, "Could not open the Flick window");
                cx.quit();
            }
            cx.activate(true);
        });
    Ok(())
}
