mod assets;
mod awake;
mod benchmark;
mod input;
mod library_files;
mod live_text;
mod logs;
mod menus;
mod model;
mod notify;
mod now_playing;
mod playback;
mod playback_actor;
mod player_prefs;
mod queue;
mod remote_client;
mod remote_discovery;
mod remote_keep;
mod remote_quality;
mod remote_serve;
mod screenshot;
mod services;
mod shortcuts;
mod shutdown;
mod smoke;
mod source;
mod store;
mod theme;
mod thumbnails;
mod ui;
mod video_adjust;
mod video_frame_ui;
mod window_ext;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, point, px, size,
};
use menus::Quit;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "flick=info".into())
    };
    // Messages go to stderr and, for reporting problems, to a log file.
    match logs::open() {
        Some(file) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::io::stderr.and(std::sync::Mutex::new(file)))
                .with_env_filter(filter())
                .init()
        }
        None => tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter())
            .init(),
    }
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if first.as_deref().is_some_and(|s| s == "--help" || s == "-h") {
        println!(
            "Flick — desktop video player\n\nUsage: flick [VIDEO]\n       flick --smoke-test VIDEO\n       flick --benchmark VIDEO [--seconds N] [--size WxH] [--interval-ms N]\n\nBenchmark defaults: 300 seconds, 3840x2160, 16 ms ticks; local video, no window.\n\nShortcuts: Space play/pause · arrows seek/volume · F fullscreen · M mute\n           Ctrl/Cmd+O open · Ctrl/Cmd+L subtitles · Ctrl/Cmd+, settings · ? shortcuts\n\nRequires libmpv. See README.md for installation."
        );
        return Ok(());
    }
    if first.as_deref().is_some_and(|s| s == "--smoke-test") {
        return smoke::run(&PathBuf::from(args.next().ok_or_else(|| {
            anyhow::anyhow!("Supply a video path after --smoke-test")
        })?));
    }
    if first.as_deref().is_some_and(|s| s == "--benchmark") {
        return benchmark::run(benchmark::Options::parse(args)?);
    }
    let path = first.map(PathBuf::from);
    let app = Application::new().with_assets(assets::Assets);
    // Finder's Open With and dropping files on the Dock icon arrive here.
    app.on_open_urls(ui::queue_open_urls);
    app.run(move |cx: &mut App| {
        input::init(cx);
        shutdown::init(cx);
        // ⌘Q on macOS, Ctrl+Q elsewhere. Clean up services off the UI thread
        // before native quit closes the window and saves playback position.
        cx.on_action(|_: &Quit, cx| shutdown::request(cx));
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        // macOS shows Quit in the app menu, where people look for it, beside
        // the player's own menus.
        menus::init(cx);
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
                            shutdown::request(cx);
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
        match cx.open_window(options, |window, cx| {
            let root = cx.new(|cx| ui::Flick::new(path, window, cx));
            shutdown::set_root(root.downgrade(), cx);
            menus::set_target(root.downgrade(), window.window_handle(), cx);
            root
        }) {
            Ok(_) => cx.activate(true),
            Err(error) => {
                tracing::error!(%error, "Could not open the Flick window");
                shutdown::request(cx);
            }
        }
    });
    services::finish_shutdown();
    Ok(())
}
