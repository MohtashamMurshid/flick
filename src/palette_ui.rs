//! The command center. ⌘K or Ctrl+K opens one large search field over any
//! view. It finds library videos, searches online, downloads a pasted
//! YouTube, X, web, or magnet link, and runs commands. Arrow keys choose,
//! Enter runs the choice, and Escape or a click outside closes it.

use super::{
    library_ui::{is_torrent_link, result_detail},
    *,
};
use gpui::{AnyElement, FontWeight, ScrollHandle, deferred};

/// Library videos listed for typed text, before the online and command rows.
const LIBRARY_MATCHES: usize = 6;
/// Videos listed under Continue Watching before anything is typed.
const RECENT_VIDEOS: usize = 4;

pub(super) struct Palette {
    selected: usize,
    /// The text last searched online from here. Its results are listed while
    /// the field still holds it.
    online: Option<String>,
    scroll: ScrollHandle,
    /// Bring the selection into view on the next render, after arrow keys.
    reveal: Cell<bool>,
}

#[derive(Clone)]
enum Action {
    Play(PathBuf),
    Player,
    Link,
    SearchOnline,
    Result(usize),
    Page(Page),
    Control(Control),
    Quit,
}

struct Item {
    section: &'static str,
    icon: &'static str,
    title: SharedString,
    detail: Option<SharedString>,
    /// A keyboard shortcut for the same thing, shown on the right.
    keys: Option<SharedString>,
    /// What Enter does, shown in the footer.
    verb: &'static str,
    action: Action,
}

impl Flick {
    /// Opens the command center, or closes it if it is open.
    pub(super) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.menu = None;
        self.card_menu = None;
        self.palette = Some(Palette {
            selected: 0,
            online: None,
            scroll: ScrollHandle::new(),
            reveal: Cell::new(false),
        });
        self.palette_input
            .update(cx, |input, cx| input.set_value("", cx));
        self.palette_input.focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.focus.focus(window);
            cx.notify();
        }
    }

    /// The field changed: start again at the first row.
    pub(super) fn palette_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(palette) = &mut self.palette {
            palette.selected = 0;
            palette.reveal.set(true);
        }
        cx.notify();
    }

    /// Runs the selected row, as Enter does.
    pub(super) fn palette_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.palette.as_ref().map(|palette| palette.selected) {
            self.run_palette(index, window, cx);
        }
    }

    fn palette_query(&self, cx: &App) -> String {
        self.palette_input.read(cx).value().trim().to_owned()
    }

    fn palette_items(&self, cx: &App) -> Vec<Item> {
        let query = self.palette_query(cx);
        let mut items = Vec::new();
        // A link is something to download, not words to look for.
        if is_torrent_link(&query) {
            items.push(Item {
                section: "Download",
                icon: "magnet",
                title: "Download this torrent".into(),
                detail: Some("Magnet link · See its files before downloading".into()),
                keys: None,
                verb: "Download",
                action: Action::Link,
            });
            return items;
        }
        if let Some(link) = flick::fetch::web_link(&query) {
            let (title, detail) = match link {
                Ok(link @ flick::fetch::WebLink::Archive(_)) => (
                    format!("Download from {}", link.site()),
                    "Choose files first".to_owned(),
                ),
                Ok(link) => (format!("Download video from {}", link.site()), query),
                Err(error) => (
                    "This link can't be downloaded".to_owned(),
                    format!("{error:#}"),
                ),
            };
            items.push(Item {
                section: "Download",
                icon: "download",
                title: title.into(),
                detail: Some(detail.into()),
                keys: None,
                verb: "Download",
                action: Action::Link,
            });
            return items;
        }
        let words = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let matches = |text: &str| {
            let text = text.to_lowercase();
            words.iter().all(|word| text.contains(word))
        };
        let (section, videos) = if query.is_empty() {
            (
                "Continue Watching",
                self.store
                    .data
                    .continue_watching()
                    .take(RECENT_VIDEOS)
                    .map(|file| (file.path.clone(), file.title.clone()))
                    .collect::<Vec<_>>(),
            )
        } else {
            (
                "Library",
                self.store
                    .data
                    .library
                    .iter()
                    .rev()
                    .filter(|video| matches(&video.title))
                    .take(LIBRARY_MATCHES)
                    .map(|video| (video.path.clone(), video.title.clone()))
                    .collect(),
            )
        };
        for (path, title) in videos {
            let (_, detail) = self.video_detail(&path);
            items.push(Item {
                section,
                icon: "play",
                title: title.into(),
                detail: Some(detail.into()),
                keys: None,
                verb: "Play",
                action: Action::Play(path),
            });
        }
        if query.chars().count() >= 3 {
            let searched = self.palette.as_ref().and_then(|p| p.online.as_deref()) == Some(&query);
            match self.search_results.as_ref().filter(|_| searched) {
                Some(results) => {
                    for (index, result) in results.items.iter().enumerate() {
                        items.push(Item {
                            section: "Online",
                            icon: if result.archive_id.is_some() {
                                "download"
                            } else {
                                "magnet"
                            },
                            title: result.name.clone().into(),
                            detail: Some(result_detail(result).into()),
                            keys: None,
                            verb: "Download",
                            action: Action::Result(index),
                        });
                    }
                }
                None if searched && self.searching => {}
                None => items.push(Item {
                    section: "Online",
                    icon: "search",
                    title: format!("Search online for “{query}”").into(),
                    detail: Some("Find videos to download".into()),
                    keys: None,
                    verb: "Search",
                    action: Action::SearchOnline,
                }),
            }
        }
        items.extend(
            self.palette_commands()
                .into_iter()
                .filter(|item| matches(&item.title)),
        );
        items
    }

    fn palette_commands(&self) -> Vec<Item> {
        let command = |icon, title: &'static str, keys: Option<String>, action| Item {
            section: "Commands",
            icon,
            title: title.into(),
            detail: None,
            keys: keys.map(Into::into),
            verb: "Run",
            action,
        };
        let key = |key: &str| Some(crate::shortcuts::command(key));
        let shortcut = |shortcut: Shortcut| {
            let binding = self.store.data.shortcuts.binding(shortcut);
            (!binding.is_empty()).then(|| crate::shortcuts::display(binding))
        };
        let mut items = Vec::new();
        if self.view == View::Library && self.path.is_some() {
            items.push(command("play", "Return to Player", None, Action::Player));
        }
        if self.view == View::Player && self.file_loaded {
            items.extend([
                command(
                    "maximize",
                    "Toggle Fullscreen",
                    shortcut(Shortcut::Fullscreen),
                    Action::Control(Control::Fullscreen),
                ),
                command(
                    "picture-in-picture",
                    "Picture in Picture",
                    shortcut(Shortcut::PictureInPicture),
                    Action::Control(Control::Pip),
                ),
                command(
                    "captions",
                    "Load Subtitle File…",
                    key("l"),
                    Action::Control(Control::Subtitle),
                ),
            ]);
        }
        items.extend([
            command(
                "folder-open",
                "Open Video…",
                None,
                Action::Control(Control::Open),
            ),
            command(
                "plus",
                "Add Videos to Library…",
                (self.view == View::Library).then(|| crate::shortcuts::command("o")),
                Action::Control(Control::AddVideos),
            ),
            command(
                "folder-plus",
                "Add Folder to Library…",
                None,
                Action::Control(Control::AddFolder),
            ),
            command(
                "magnet",
                "Paste Link to Download",
                None,
                Action::Control(Control::PasteMagnet),
            ),
            command(
                "share-2",
                "Share a File or Folder…",
                None,
                Action::Control(Control::Share),
            ),
            command("house", "Go to Home", None, Action::Page(Page::Home)),
            command("film", "Go to All Videos", None, Action::Page(Page::Videos)),
            command("library", "Go to Shows", None, Action::Page(Page::Shows)),
            command(
                "history",
                "Go to History",
                None,
                Action::Page(Page::History),
            ),
            command(
                "download",
                "Go to Downloads",
                None,
                Action::Page(Page::Downloads),
            ),
            command(
                "palette",
                "Appearance",
                key(","),
                Action::Page(Page::Appearance),
            ),
            command(
                "keyboard",
                "Keyboard Shortcuts",
                shortcut(Shortcut::ShowShortcuts),
                Action::Page(Page::Shortcuts),
            ),
            command("x", "Quit Flick", key("q"), Action::Quit),
        ]);
        items
    }

    fn run_palette(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.palette_items(cx).into_iter().nth(index) else {
            return;
        };
        let query = self.palette_query(cx);
        if let Action::SearchOnline = item.action {
            // Results are listed here, and on Home once this closes.
            if let Some(palette) = &mut self.palette {
                palette.online = Some(query.clone());
            }
            self.search_input
                .update(cx, |input, cx| input.set_value(query.clone(), cx));
            self.query = query;
            self.search(cx);
            return;
        }
        self.close_palette(window, cx);
        match item.action {
            Action::Play(path) => self.open(path, window, cx),
            Action::Player => {
                if self.path.is_some() {
                    self.entered_player = Instant::now();
                    self.view = View::Player;
                }
            }
            Action::Link if is_torrent_link(&query) => {
                self.resolve(flick::torrent::TorrentInput::Magnet(query), cx)
            }
            Action::Link => {
                self.search_link(&query, cx);
            }
            Action::Result(index) => {
                if let Some(result) = self
                    .search_results
                    .as_ref()
                    .and_then(|results| results.items.get(index))
                    .cloned()
                {
                    self.download_result(&result, cx);
                }
            }
            Action::Page(page) => self.show_page(page, window, cx),
            Action::Control(control) => self.handle(control, window, cx),
            Action::Quit => cx.quit(),
            Action::SearchOnline => {}
        }
        cx.notify();
    }

    fn move_palette_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.palette_items(cx).len();
        if let Some(palette) = &mut self.palette
            && count > 0
        {
            palette.selected =
                (palette.selected as isize + step).rem_euclid(count as isize) as usize;
            palette.reveal.set(true);
            cx.notify();
        }
    }

    pub(super) fn palette_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let palette = self.palette.as_ref()?;
        let t = cx.theme().clone();
        let query = self.palette_query(cx);
        let items = self.palette_items(cx);
        let selected = palette.selected.min(items.len().saturating_sub(1));
        let viewport = window.viewport_size();
        let width = (f32::from(viewport.width) - 48.).clamp(320., 680.);
        let list_height = (f32::from(viewport.height) * 0.55).clamp(140., 480.);

        let heading = |text: &'static str| {
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_size(t.font(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.muted)
                .child(text)
        };
        let keycap = |text: SharedString| {
            div()
                .flex_none()
                .px_1p5()
                .rounded(px(4.))
                .border_1()
                .border_color(t.border)
                .text_size(t.font(11.))
                .text_color(t.muted)
                .child(text)
        };

        let mut list = v_flex()
            .id("palette-list")
            .track_scroll(&palette.scroll)
            .overflow_y_scroll()
            .max_h(px(list_height))
            .px_2()
            .py_1p5();
        let mut child = 0;
        let mut section = None;
        for (index, item) in items.iter().enumerate() {
            if section != Some(item.section) {
                section = Some(item.section);
                list = list.child(heading(item.section));
                child += 1;
            }
            if index == selected && palette.reveal.take() {
                palette.scroll.scroll_to_item(child);
            }
            let leading = match &item.action {
                Action::Play(path) => self.poster(path, &item.title, 48., 27., 4., cx),
                _ => div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(6.))
                    .bg(t.control)
                    .child(icon(item.icon, 16., t.text)),
            };
            let row = h_flex()
                .id(("palette-item", index))
                .gap_3()
                .px_2()
                .min_h(px(44.))
                .py_1()
                .rounded(px(8.))
                .cursor_pointer()
                .when(index == selected, |d| d.bg(t.selection))
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(palette) = &mut this.palette
                        && palette.selected != index
                    {
                        palette.selected = index;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.run_palette(index, window, cx)
                }))
                .child(leading)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(item.title.clone()),
                        )
                        .when_some(item.detail.clone(), |d, detail| {
                            d.child(
                                div()
                                    .text_size(t.font(12.))
                                    .text_color(t.muted)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(detail),
                            )
                        }),
                )
                .children(item.keys.clone().map(keycap));
            list = list.child(row);
            child += 1;
        }
        if self.searching && palette.online.as_deref() == Some(query.as_str()) {
            list = list.child(
                div()
                    .px_2()
                    .py_2()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child("Searching online…"),
            );
        }
        if items.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_4()
                    .text_color(t.muted)
                    .child("No matches. Paste a YouTube, X, or magnet link to download it."),
            );
        }

        let field = h_flex()
            .gap_3()
            .px_4()
            .h(px(56.))
            .border_b_1()
            .border_color(t.border)
            .child(icon("search", 20., t.muted))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(self.palette_input.clone()),
            )
            .child(keycap("esc".into()));
        let verb = items.get(selected).map_or("", |item| item.verb);
        let footer = h_flex()
            .gap_3()
            .px_4()
            .h(px(36.))
            .border_t_1()
            .border_color(t.border)
            .text_size(t.font(12.))
            .text_color(t.muted)
            .child(div().flex_1().child("Flick"))
            .child("↑↓ Choose")
            .when(!verb.is_empty(), |d| {
                d.child(h_flex().gap_1().child(verb).child(keycap("↵".into())))
            });
        let panel = v_flex()
            .id("palette")
            .occlude()
            .w(px(width))
            .bg(t.popover)
            .border_1()
            .border_color(t.border)
            .rounded(px(14.))
            .shadow_lg()
            .overflow_hidden()
            .text_color(t.text)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let shift = event.keystroke.modifiers.shift;
                match event.keystroke.key.as_str() {
                    "down" => this.move_palette_selection(1, cx),
                    "up" => this.move_palette_selection(-1, cx),
                    "tab" => this.move_palette_selection(if shift { -1 } else { 1 }, cx),
                    "escape" => this.close_palette(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(field)
            .child(list)
            .child(footer);
        Some(
            deferred(
                div()
                    .id("palette-backdrop")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(f32::from(viewport.height) * 0.12))
                    .bg(rgba(0x00000059))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                    )
                    .child(panel),
            )
            .with_priority(4)
            .into_any_element(),
        )
    }
}
