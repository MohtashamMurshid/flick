//! The play queue, IINA's playlist: the videos that play after this one,
//! in order. Opening a video starts a queue of its show's episodes or the
//! rest of its folder; Play Next and Add to Queue change it, and it can be
//! reordered, shuffled, and repeated.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Queue {
    items: Vec<PathBuf>,
    current: Option<usize>,
    /// The order before shuffling, restored when shuffle is turned off.
    ordered: Vec<PathBuf>,
    shuffle: bool,
    /// Start again from the first video after the last.
    pub repeat: bool,
}

impl Queue {
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn shuffled(&self) -> bool {
        self.shuffle
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.items.iter().any(|item| item == path)
    }

    /// Replaces the queue with `items`, playing `playing`, which is added
    /// first if missing. Shuffle and repeat stay as they were.
    pub fn replace(&mut self, mut items: Vec<PathBuf>, playing: &Path, seed: u64) {
        if !items.iter().any(|item| item == playing) {
            items.insert(0, playing.to_path_buf());
        }
        items.dedup();
        self.ordered = items.clone();
        self.items = items;
        self.select(playing);
        if self.shuffle {
            self.shuffle_rest(seed);
        }
    }

    /// Plays `path` from the queue; false when it is not queued.
    pub fn select(&mut self, path: &Path) -> bool {
        self.current = self.items.iter().position(|item| item == path);
        self.current.is_some()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.ordered.clear();
        self.current = None;
    }

    /// The video after the playing one, wrapping around with repeat.
    pub fn next(&self) -> Option<&PathBuf> {
        let current = self.current?;
        match self.items.get(current + 1) {
            Some(next) => Some(next),
            None if self.repeat && self.items.len() > 1 => self.items.first(),
            None => None,
        }
    }

    /// The video before the playing one, wrapping around with repeat.
    pub fn previous(&self) -> Option<&PathBuf> {
        let current = self.current?;
        match current.checked_sub(1) {
            Some(previous) => self.items.get(previous),
            None if self.repeat && self.items.len() > 1 => self.items.last(),
            None => None,
        }
    }

    /// Queues `path` right after the playing video, moving it if queued.
    pub fn play_next(&mut self, path: &Path) {
        if self
            .current
            .is_some_and(|current| self.items[current] == path)
        {
            return;
        }
        self.remove_path(path);
        let at = self.current.map_or(0, |current| current + 1);
        self.items.insert(at, path.to_path_buf());
        self.ordered.push(path.to_path_buf());
    }

    /// Queues `path` at the end, unless it is already queued.
    pub fn add(&mut self, path: &Path) {
        if !self.contains(path) {
            self.items.push(path.to_path_buf());
            self.ordered.push(path.to_path_buf());
        }
    }

    /// Takes a video that is not playing out of the queue.
    pub fn remove(&mut self, index: usize) {
        if index >= self.items.len() || self.current == Some(index) {
            return;
        }
        let path = self.items.remove(index);
        self.ordered.retain(|item| *item != path);
        if let Some(current) = self.current.filter(|current| *current > index) {
            self.current = Some(current - 1);
        }
    }

    fn remove_path(&mut self, path: &Path) {
        if let Some(index) = self.items.iter().position(|item| item == path) {
            self.remove(index);
        }
    }

    /// Moves a video one place up (`step` −1) or down (1).
    pub fn move_item(&mut self, index: usize, step: isize) {
        let Some(target) = index.checked_add_signed(step) else {
            return;
        };
        if index >= self.items.len() || target >= self.items.len() {
            return;
        }
        self.items.swap(index, target);
        self.current = self.current.map(|current| match current {
            c if c == index => target,
            c if c == target => index,
            c => c,
        });
    }

    /// Shuffles the videos after the playing one, which moves to the front
    /// so all of them still play; turning it off restores their order.
    pub fn set_shuffle(&mut self, on: bool, seed: u64) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        let playing = self.current.map(|current| self.items[current].clone());
        if on {
            self.shuffle_rest(seed);
        } else {
            let items = self.items.clone();
            self.items = self
                .ordered
                .iter()
                .filter(|item| items.contains(item))
                .cloned()
                .collect();
            if let Some(playing) = playing {
                self.select(&playing);
            }
        }
    }

    fn shuffle_rest(&mut self, seed: u64) {
        if let Some(current) = self.current {
            let playing = self.items.remove(current);
            self.items.insert(0, playing);
            self.current = Some(0);
        }
        let start = self.current.map_or(0, |_| 1);
        // Fisher–Yates with xorshift; a shuffle needs no stronger random.
        let mut state = seed | 1;
        for i in (start + 1..self.items.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = start + (state % (i - start + 1) as u64) as usize;
            self.items.swap(i, j);
        }
    }
}

/// Compares file names as people read them: `Part 2` before `Part 10`,
/// ignoring case.
pub fn natural_order(a: &Path, b: &Path) -> Ordering {
    let key = |path: &Path| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut parts = Vec::new();
        let mut chars = name.chars().peekable();
        while let Some(&c) = chars.peek() {
            let digit = c.is_ascii_digit();
            let mut part = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() != digit {
                    break;
                }
                part.push(c);
                chars.next();
            }
            parts.push(if digit {
                // Leading zeros and length decide numbers before text.
                let trimmed = part.trim_start_matches('0');
                (0, format!("{:0>20}", trimmed))
            } else {
                (1, part)
            });
        }
        parts
    };
    key(a).cmp(&key(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn next_and_previous_follow_the_order_and_wrap_with_repeat() {
        let mut queue = Queue::default();
        queue.replace(paths(&["a", "b", "c"]), Path::new("b"), 1);
        assert_eq!(queue.next(), Some(&PathBuf::from("c")));
        assert_eq!(queue.previous(), Some(&PathBuf::from("a")));
        queue.select(Path::new("c"));
        assert_eq!(queue.next(), None);
        queue.repeat = true;
        assert_eq!(queue.next(), Some(&PathBuf::from("a")));
        queue.select(Path::new("a"));
        assert_eq!(queue.previous(), Some(&PathBuf::from("c")));
        // A missing video starts the queue.
        queue.replace(paths(&["x"]), Path::new("new"), 1);
        assert_eq!(queue.items(), paths(&["new", "x"]));
        assert_eq!(queue.current(), Some(0));
    }

    #[test]
    fn play_next_add_remove_and_move() {
        let mut queue = Queue::default();
        queue.replace(paths(&["a", "b", "c"]), Path::new("a"), 1);
        queue.play_next(Path::new("c"));
        assert_eq!(queue.items(), paths(&["a", "c", "b"]));
        queue.add(Path::new("d"));
        queue.add(Path::new("d"));
        assert_eq!(queue.items(), paths(&["a", "c", "b", "d"]));
        queue.remove(0);
        assert_eq!(queue.items().len(), 4, "the playing video stays");
        queue.remove(1);
        assert_eq!(queue.items(), paths(&["a", "b", "d"]));
        queue.move_item(0, 1);
        assert_eq!(queue.items(), paths(&["b", "a", "d"]));
        assert_eq!(queue.current(), Some(1));
        queue.move_item(2, 1);
        queue.move_item(0, -1);
        assert_eq!(queue.items(), paths(&["b", "a", "d"]));
        queue.play_next(Path::new("a"));
        assert_eq!(queue.items(), paths(&["b", "a", "d"]));
    }

    #[test]
    fn shuffle_keeps_every_video_and_restores_the_order() {
        let names: Vec<String> = (0..20).map(|n| format!("v{n:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut queue = Queue::default();
        queue.replace(paths(&names), Path::new("v05"), 7);
        queue.set_shuffle(true, 12345);
        assert_eq!(queue.items()[0], PathBuf::from("v05"));
        assert_eq!(queue.current(), Some(0));
        let mut sorted = queue.items().to_vec();
        sorted.sort();
        assert_eq!(sorted, paths(&names));
        assert_ne!(queue.items(), paths(&names));
        queue.set_shuffle(false, 0);
        assert_eq!(queue.items(), paths(&names));
        assert_eq!(queue.current(), Some(5));
    }

    #[test]
    fn names_sort_as_people_read_them() {
        let mut names = paths(&[
            "Part 10.mkv",
            "part 2.mkv",
            "Part 1.mkv",
            "Extra.mkv",
            "Part 02b.mkv",
        ]);
        names.sort_by(|a, b| natural_order(a, b));
        assert_eq!(
            names,
            paths(&[
                "Extra.mkv",
                "Part 1.mkv",
                "part 2.mkv",
                "Part 02b.mkv",
                "Part 10.mkv"
            ])
        );
    }
}
