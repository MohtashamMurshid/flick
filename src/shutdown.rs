//! Keep ordinary quits asynchronous, with a final barrier for native OS exits.
use gpui::{App, Global, Task, WeakEntity};

use crate::{services, ui::Flick};

#[derive(Default)]
struct QuitCoordinator {
    root: Option<WeakEntity<Flick>>,
    task: Option<Task<()>>,
    native_quit: bool,
}

impl Global for QuitCoordinator {}

pub fn init(cx: &mut App) {
    cx.set_global(QuitCoordinator::default());
    cx.on_app_quit(|cx| {
        cx.global_mut::<QuitCoordinator>().native_quit = true;
        prepare(cx);
        // GPUI 0.2.2 constructs this future BEFORE clearing windows, then polls
        // it AFTER windows.clear()/flush_effects() release Flick and its store.
        // NSApp termination can bypass our Quit action and never return from
        // app.run(). Its 100 ms future budget cannot safely own worker cleanup.
        // Deliberately complete this synchronous join in the first poll: GPUI
        // cannot interrupt a poll. The UI is already gone; native exit may wait
        // for the existing HTTP cleanup/runtime deadlines instead of losing
        // partial-file cleanup or leaving session ownership unfinished.
        async { services::finish_shutdown() }
    })
    .detach();
}

pub fn set_root(root: WeakEntity<Flick>, cx: &mut App) {
    cx.global_mut::<QuitCoordinator>().root = Some(root);
}

fn prepare(cx: &mut App) {
    let root = cx.global::<QuitCoordinator>().root.clone();
    if let Some(root) = root.and_then(|root| root.upgrade()) {
        root.update(cx, |root, cx| {
            root.prepare_quit();
            cx.notify();
        });
    }
    // If the last window already released Flick, on_release signalled and
    // registered its worker. Keeping only a weak entity permits that release.
}

pub fn request(cx: &mut App) {
    let coordinator = cx.global::<QuitCoordinator>();
    if coordinator.native_quit || coordinator.task.is_some() {
        return;
    }
    prepare(cx);
    let cleanup = cx.background_executor().spawn(async {
        services::finish_shutdown();
    });
    let task = cx.spawn(async move |cx| {
        cleanup.await;
        let _ = cx.update(|cx| {
            if !cx.global::<QuitCoordinator>().native_quit {
                cx.quit();
            }
        });
    });
    // Own the task for the app lifetime and coalesce menu/shortcut/last-window
    // requests. Native termination's barrier remains safe if this is cancelled.
    cx.global_mut::<QuitCoordinator>().task = Some(task);
}
