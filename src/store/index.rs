use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use super::{Settings, Sort, TorrentIdentity, eligible_resume};
use flick::library::{Show, group_shows};

type EpisodeNeighbors = HashMap<PathBuf, (Option<PathBuf>, Option<PathBuf>)>;

/// Cloning settings for persistence never clones derived indexes or show trees.
#[derive(Debug, Default)]
pub(super) struct Cache {
    pub revision: u64,
    pub index: OnceLock<Index>,
    pub orders: OnceLock<[Vec<usize>; 3]>,
    pub shows: OnceLock<Vec<Show>>,
    pub adjacent: OnceLock<EpisodeNeighbors>,
}

impl Clone for Cache {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision,
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Index {
    pub library: HashMap<PathBuf, usize>,
    pub recent: HashMap<PathBuf, usize>,
    pub recent_order: HashMap<PathBuf, usize>,
    pub finished: HashSet<PathBuf>,
    pub torrent: HashMap<TorrentIdentity, usize>,
    pub removed: HashSet<PathBuf>,
    pub removed_torrents: HashSet<TorrentIdentity>,
    pub titles: Vec<String>,
    pub recent_titles: Vec<String>,
}

impl Index {
    pub fn new(settings: &Settings) -> Self {
        let mut index = Self::default();
        for (i, video) in settings.library.iter().enumerate() {
            index.library.entry(video.path.clone()).or_insert(i);
            index.titles.push(video.title.to_lowercase());
        }
        for (i, file) in settings.recent.iter().enumerate() {
            index.recent.entry(file.path.clone()).or_insert(i);
            // Legacy watched sorting used HashMap::collect (last duplicate wins).
            index.recent_order.insert(file.path.clone(), i);
            if file.finished {
                index.finished.insert(file.path.clone());
            }
            index.recent_titles.push(file.title.to_lowercase());
        }
        for (i, saved) in settings.torrent_history.iter().enumerate() {
            index.torrent.entry(saved.identity.clone()).or_insert(i);
        }
        for removed in &settings.removed_videos {
            index.removed.insert(removed.path.clone());
            if removed.block_torrent
                && let Some(identity) = &removed.torrent
            {
                index.removed_torrents.insert(identity.clone());
            }
        }
        for saved in &settings.torrent_history {
            if index.removed_torrents.contains(&saved.identity) {
                index.removed.insert(saved.path.clone());
            }
        }
        index
    }

    pub fn is_removed(&self, settings: &Settings, path: &Path) -> bool {
        self.removed.contains(path)
            || self
                .library
                .get(path)
                .and_then(|i| settings.library[*i].torrent.as_ref())
                .is_some_and(|identity| self.removed_torrents.contains(identity))
    }

    pub fn resume(&self, settings: &Settings, path: &Path) -> f64 {
        if self.is_removed(settings, path) {
            return 0.0;
        }
        if let Some(saved) = self
            .library
            .get(path)
            .and_then(|i| settings.library[*i].torrent.as_ref())
            .and_then(|identity| self.torrent.get(identity))
            .map(|i| &settings.torrent_history[*i])
        {
            return eligible_resume(saved.position, saved.duration);
        }
        self.recent.get(path).map_or(0.0, |i| {
            let file = &settings.recent[*i];
            eligible_resume(file.position, file.duration)
        })
    }

    pub fn orders(&self, settings: &Settings) -> [Vec<usize>; 3] {
        let added = (0..settings.library.len())
            .rev()
            .filter(|i| !self.is_removed(settings, &settings.library[*i].path))
            .collect::<Vec<_>>();
        let mut title = added.clone();
        title.sort_by(|a, b| self.titles[*a].cmp(&self.titles[*b]));
        let mut watched = added.clone();
        watched.sort_by_key(|i| {
            self.recent_order
                .get(&settings.library[*i].path)
                .copied()
                .unwrap_or(usize::MAX)
        });
        [added, title, watched]
    }
}

impl Settings {
    pub(super) fn index(&self) -> &Index {
        self.cache.index.get_or_init(|| Index::new(self))
    }

    /// Call after direct public-vector or entry edits; methods do this automatically.
    /// No filesystem checks are performed. Scalar preference edits need no invalidation.
    pub fn invalidate(&mut self) {
        self.cache = Cache {
            revision: self.cache.revision.wrapping_add(1),
            ..Default::default()
        };
    }

    /// Call after changing only poster/duration/dimensions on existing library
    /// entries. Their paths, titles, order and identities must be unchanged.
    /// Keep expensive path/title/show derivations while refreshing UI revision.
    pub fn invalidate_metadata(&mut self) {
        self.cache.revision = self.cache.revision.wrapping_add(1);
    }

    pub(super) fn invalidate_history(&mut self) {
        self.cache.revision = self.cache.revision.wrapping_add(1);
        self.cache.index.take();
        self.cache.orders.take();
    }

    pub fn revision(&self) -> u64 {
        self.cache.revision
    }

    pub fn video(&self, path: &Path) -> Option<&super::LibraryVideo> {
        (!self.is_removed(path))
            .then(|| self.index().library.get(path).map(|i| &self.library[*i]))
            .flatten()
    }

    pub fn recent_file(&self, path: &Path) -> Option<&super::RecentFile> {
        (!self.is_removed(path))
            .then(|| self.index().recent.get(path).map(|i| &self.recent[*i]))
            .flatten()
    }

    pub fn normalized_title(&self, path: &Path) -> Option<&str> {
        self.index()
            .library
            .get(path)
            .map(|i| self.index().titles[*i].as_str())
    }

    pub fn normalized_recent_title(&self, path: &Path) -> Option<&str> {
        self.index()
            .recent
            .get(path)
            .map(|i| self.index().recent_titles[*i].as_str())
    }

    pub fn shows(&self) -> &[Show] {
        self.cache.shows.get_or_init(|| {
            group_shows(
                self.library
                    .iter()
                    .filter(|v| !self.is_removed(&v.path))
                    .map(|v| v.path.as_path()),
            )
        })
    }

    /// Uses cached grouping and adjacency. Duplicate episode copies are skipped.
    pub fn adjacent_episode(&self, current: &Path, step: isize) -> Option<PathBuf> {
        if step != -1 && step != 1 {
            return None;
        }
        let adjacent = self.cache.adjacent.get_or_init(|| {
            let mut result = HashMap::new();
            for show in self.shows() {
                let mut start = 0;
                while start < show.episodes.len() {
                    let number = |i: usize| {
                        let e = &show.episodes[i].0;
                        (e.season, e.episode)
                    };
                    let mut end = start + 1;
                    while end < show.episodes.len() && number(end) == number(start) {
                        end += 1;
                    }
                    let before = start.checked_sub(1).map(|i| show.episodes[i].1.clone());
                    let after = show.episodes.get(end).map(|(_, path)| path.clone());
                    for (_, path) in &show.episodes[start..end] {
                        result
                            .entry(path.clone())
                            .or_insert_with(|| (before.clone(), after.clone()));
                    }
                    start = end;
                }
            }
            result
        });
        adjacent.get(current).and_then(|(before, after)| {
            if step < 0 {
                before.clone()
            } else {
                after.clone()
            }
        })
    }

    pub(super) fn sorted_indices(&self, sort: Sort) -> &[usize] {
        let orders = self.cache.orders.get_or_init(|| self.index().orders(self));
        &orders[match sort {
            Sort::Added => 0,
            Sort::Title => 1,
            Sort::Watched => 2,
        }]
    }
}
