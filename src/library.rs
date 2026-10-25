use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|v| v.to_str()).is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "mp4"
                | "mkv"
                | "mov"
                | "m4v"
                | "avi"
                | "webm"
                | "mpeg"
                | "mpg"
                | "ts"
                | "m2ts"
                | "mts"
                | "wmv"
                | "flv"
                | "ogv"
                | "3gp"
                | "vob"
        )
    })
}

/// Use one existing, canonical file identity for picker and drop imports.
pub fn canonical_video(path: &Path) -> Result<PathBuf> {
    ensure!(is_video(path), "Not a supported video: {}", path.display());
    let path = path.canonicalize().context("Resolve video path")?;
    ensure!(path.is_file(), "Choose a video file");
    Ok(path)
}

/// Explicitly chosen folders only. Skip symlinks to avoid recursion cycles.
pub fn scan_folder(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut videos = Vec::new();
    while let Some(folder) = pending.pop() {
        for entry in
            std::fs::read_dir(&folder).with_context(|| format!("Read {}", folder.display()))?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && is_video(&entry.path()) {
                videos.push(entry.path().canonicalize()?);
            }
        }
    }
    videos.sort();
    Ok(videos)
}

/// An episode identified from a file name, such as `The.Expanse.S01E02.mkv`,
/// `Show 1x02.mkv`, or `Show/Season 1/03 - Title.mkv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    /// The show's title as written in the first episode found.
    pub show: String,
    pub season: u32,
    pub episode: u32,
}

impl Episode {
    /// Groups spellings such as `The.Expanse` and `the expanse`.
    pub fn show_key(&self) -> String {
        show_key(&self.show)
    }

    pub fn label(&self) -> String {
        format!("S{:02}E{:02}", self.season, self.episode)
    }
}

fn show_key(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Recognizes `S01E02` (any case, optional separator), `1x02`, or an
/// episode number inside a `Season N` folder. The show title comes from
/// the text before the episode marker, or else from the show's folder.
pub fn parse_episode(path: &Path) -> Option<Episode> {
    let stem = path.file_stem()?.to_str()?;
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let season_folder = parent.and_then(season_number);
    let show_folder = || {
        let folder = if season_folder.is_some() {
            path.parent()?.parent()?
        } else {
            path.parent()?
        };
        folder.file_name()?.to_str().map(clean_title)
    };
    let (start, season, episode) = marker(stem).or_else(|| {
        let episode = leading_episode_number(stem)?;
        Some((0, season_folder?, episode))
    })?;
    let show = Some(clean_title(&stem[..start]))
        .filter(|title| !title.is_empty())
        .or_else(show_folder)
        .filter(|title| !title.is_empty())?;
    Some(Episode {
        show,
        season,
        episode,
    })
}

/// The byte offset where an `S01E02` or `1x02` marker starts, with its
/// season and episode. Markers must start a word.
fn marker(stem: &str) -> Option<(usize, u32, u32)> {
    let bytes = stem.as_bytes();
    let digits = |from: usize, max: usize| {
        let len = bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        (1..=max)
            .contains(&len)
            .then(|| (stem[from..from + len].parse::<u32>().ok(), from + len))
    };
    for start in 0..bytes.len() {
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let ends_number = |at: usize| bytes.get(at).is_none_or(|b| !b.is_ascii_digit());
        // S01E02, s1e2, S01.E02, S01 E02
        if bytes[start].eq_ignore_ascii_case(&b's')
            && let Some((Some(season), mut at)) = digits(start + 1, 2)
        {
            if matches!(bytes.get(at), Some(b'.' | b'_' | b'-' | b' ')) {
                at += 1;
            }
            if bytes.get(at).is_some_and(|b| b.eq_ignore_ascii_case(&b'e'))
                && let Some((Some(episode), end)) = digits(at + 1, 3)
                && ends_number(end)
            {
                return Some((start, season, episode));
            }
        }
        // 1x02, but not resolutions such as 1920x1080.
        if let Some((Some(season), at)) = digits(start, 2)
            && bytes.get(at).is_some_and(|b| b.eq_ignore_ascii_case(&b'x'))
            && let Some((Some(episode), end)) = digits(at + 1, 3)
            && end - (at + 1) >= 2
            && ends_number(end)
        {
            return Some((start, season, episode));
        }
    }
    None
}

/// `Season 2`, `season_02`, or `S02` as a folder name.
fn season_number(folder: &str) -> Option<u32> {
    let lower = folder.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("season")
        .or_else(|| lower.strip_prefix('s'))?
        .trim_start_matches([' ', '_', '.', '-']);
    (!rest.is_empty() && rest.len() <= 3 && rest.bytes().all(|b| b.is_ascii_digit()))
        .then(|| rest.parse().ok())
        .flatten()
}

/// `E03`, `Episode 3`, `Ep. 3`, or a leading `03` in a season folder.
fn leading_episode_number(stem: &str) -> Option<u32> {
    let lower = stem.trim().to_ascii_lowercase();
    let rest = ["episode", "ep", "e"]
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))
        .unwrap_or(&lower)
        .trim_start_matches([' ', '.', '_', '-']);
    let len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    (1..=3)
        .contains(&len)
        .then(|| rest[..len].parse().ok())
        .flatten()
}

/// Turns `The.Expanse.` or `the_expanse - ` into `The Expanse` / `the expanse`.
fn clean_title(text: &str) -> String {
    text.replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '-' || c == '[' || c == '(' || c.is_whitespace())
        .to_owned()
}

/// A show and its episodes in season and episode order.
#[derive(Clone, Debug, PartialEq)]
pub struct Show {
    pub title: String,
    pub episodes: Vec<(Episode, PathBuf)>,
}

/// Groups recognized episodes by show, keeping each show's first-seen title.
/// Shows are ordered by title; duplicate episode numbers keep library order.
pub fn group_shows<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<Show> {
    let mut shows: Vec<(String, Show)> = Vec::new();
    for path in paths {
        let Some(episode) = parse_episode(path) else {
            continue;
        };
        let key = episode.show_key();
        let index = match shows.iter().position(|(k, _)| *k == key) {
            Some(index) => index,
            None => {
                shows.push((
                    key,
                    Show {
                        title: episode.show.clone(),
                        episodes: Vec::new(),
                    },
                ));
                shows.len() - 1
            }
        };
        shows[index].1.episodes.push((episode, path.to_owned()));
    }
    let mut shows = shows.into_iter().map(|(_, show)| show).collect::<Vec<_>>();
    for show in &mut shows {
        show.episodes.sort_by_key(|(e, _)| (e.season, e.episode));
    }
    shows.sort_by_cached_key(|show| show.title.to_lowercase());
    shows
}

/// The episode after (or before, with `step` -1) `current` within its show.
/// Other copies of the current episode, such as a 720p and a 1080p file, are
/// skipped.
pub fn adjacent_episode<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    current: &Path,
    step: isize,
) -> Option<PathBuf> {
    let key = parse_episode(current)?.show_key();
    let show = group_shows(paths).into_iter().find(|show| {
        show.episodes
            .first()
            .is_some_and(|(e, _)| e.show_key() == key)
    })?;
    let mut index = show.episodes.iter().position(|(_, p)| p == current)?;
    let number = |e: &Episode| (e.season, e.episode);
    let here = number(&show.episodes[index].0);
    loop {
        index = index.checked_add_signed(step)?;
        let (episode, path) = show.episodes.get(index)?;
        if number(episode) != here {
            return Some(path.clone());
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", value / (1024_f64.powi(3)))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", value / (1024_f64.powi(2)))
    } else {
        format!("{:.0} KB", value / 1024.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_import_resolves_aliases_and_rejects_non_videos_or_missing_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::create_dir(root.path().join("folder.mp4")).unwrap();
        let file = root.path().join("film.MKV");
        std::fs::write(&file, []).unwrap();
        std::fs::write(root.path().join("notes.txt"), []).unwrap();
        assert_eq!(
            canonical_video(&file).unwrap(),
            canonical_video(&root.path().join("sub/../film.MKV")).unwrap()
        );
        for invalid in ["notes.txt", "missing.mp4", "folder.mp4"] {
            assert!(canonical_video(&root.path().join(invalid)).is_err());
        }
    }

    fn episode(path: &str) -> Option<(String, u32, u32)> {
        parse_episode(Path::new(path)).map(|e| (e.show, e.season, e.episode))
    }

    #[test]
    fn episode_names_are_recognized() {
        let some = |show: &str, s, e| Some((show.to_owned(), s, e));
        assert_eq!(
            episode("/tv/The.Expanse.S01E02.1080p.mkv"),
            some("The Expanse", 1, 2)
        );
        assert_eq!(
            episode("/tv/the_expanse s1e10 - Title.mp4"),
            some("the expanse", 1, 10)
        );
        assert_eq!(episode("/tv/Show S02.E03.mkv"), some("Show", 2, 3));
        assert_eq!(episode("/tv/Show - 3x07 - Name.mkv"), some("Show", 3, 7));
        assert_eq!(
            episode("/tv/Dark/Season 2/03 - Lost.mkv"),
            some("Dark", 2, 3)
        );
        assert_eq!(episode("/tv/Dark/S01/Episode 4.mkv"), some("Dark", 1, 4));
        assert_eq!(episode("/tv/Dark/S01E05.mkv"), some("Dark", 1, 5));
        assert_eq!(episode("/tv/Dark/Season 1/S01E06.mkv"), some("Dark", 1, 6));
        // Resolutions, years, and plain films are not episodes.
        assert_eq!(episode("/films/Arrival.1920x1080.mkv"), None);
        assert_eq!(episode("/films/Blade Runner 2049.mkv"), None);
        assert_eq!(episode("/films/Sintel.mp4"), None);
        assert_eq!(episode("/films/Mess1x02.mkv"), None);
        assert_eq!(episode("/films/S01E02"), some("films", 1, 2));
    }

    #[test]
    fn shows_group_spellings_and_order_episodes() {
        let paths = [
            "/tv/The.Expanse.S01E03.mkv",
            "/tv/Dark/Season 1/02.mkv",
            "/tv/the expanse S01E01.mkv",
            "/tv/The Expanse S02E01.mkv",
            "/tv/Dark/Season 1/01.mkv",
            "/films/Sintel.mp4",
        ]
        .map(PathBuf::from);
        let shows = group_shows(paths.iter().map(PathBuf::as_path));
        assert_eq!(shows.len(), 2);
        assert_eq!(shows[0].title, "Dark");
        assert_eq!(shows[1].title, "The Expanse");
        let order = shows[1]
            .episodes
            .iter()
            .map(|(e, _)| e.label())
            .collect::<Vec<_>>();
        assert_eq!(order, ["S01E01", "S01E03", "S02E01"]);
        let next = |current: &str, step| {
            adjacent_episode(paths.iter().map(PathBuf::as_path), Path::new(current), step)
        };
        assert_eq!(
            next("/tv/The.Expanse.S01E03.mkv", 1),
            Some(PathBuf::from("/tv/The Expanse S02E01.mkv"))
        );
        assert_eq!(
            next("/tv/The.Expanse.S01E03.mkv", -1),
            Some(PathBuf::from("/tv/the expanse S01E01.mkv"))
        );
        assert_eq!(next("/tv/The Expanse S02E01.mkv", 1), None);
        assert_eq!(next("/tv/the expanse S01E01.mkv", -1), None);
        assert_eq!(next("/films/Sintel.mp4", 1), None);

        // Two copies of one episode step past each other, in both directions.
        let copies = [
            "/tv/Show.S01E01.mkv",
            "/tv/Show.S01E02.720p.mkv",
            "/tv/Show.S01E02.1080p.mkv",
            "/tv/Show.S01E03.mkv",
        ]
        .map(PathBuf::from);
        let next = |current: &str, step| {
            adjacent_episode(
                copies.iter().map(PathBuf::as_path),
                Path::new(current),
                step,
            )
        };
        for copy in ["/tv/Show.S01E02.720p.mkv", "/tv/Show.S01E02.1080p.mkv"] {
            assert_eq!(next(copy, 1), Some(PathBuf::from("/tv/Show.S01E03.mkv")));
            assert_eq!(next(copy, -1), Some(PathBuf::from("/tv/Show.S01E01.mkv")));
        }
        let previous = next("/tv/Show.S01E03.mkv", -1).and_then(|p| parse_episode(&p));
        assert_eq!(previous.map(|e| e.label()).as_deref(), Some("S01E02"));
    }

    #[test]
    fn explicit_folder_scan_finds_nested_videos_but_not_other_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        for path in ["film.MKV", "sub/other.mp4", "sub/notes.txt"] {
            std::fs::write(root.path().join(path), []).unwrap();
        }
        let videos = scan_folder(root.path()).unwrap();
        assert_eq!(videos.len(), 2);
        assert!(videos.iter().all(|p| p.is_absolute() && is_video(p)));
    }
}
