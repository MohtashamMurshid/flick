//! Log files for reporting problems: Flick's own messages in `flick.log`
//! and libmpv's in `mpv.log`, both in a `logs` folder beside Flick's other
//! data. Each launch starts them again and keeps the previous launch's
//! Flick log as `flick.previous.log`.

use std::fs::File;
use std::path::PathBuf;
use std::sync::OnceLock;

static FOLDER: OnceLock<PathBuf> = OnceLock::new();

/// `FLICK_DATA_DIR/logs`, or `logs` in the platform's data folder.
fn default_folder() -> Option<PathBuf> {
    let base = match std::env::var_os("FLICK_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => directories::ProjectDirs::from("app", "Flick", "Flick")?
            .data_local_dir()
            .to_path_buf(),
    };
    Some(base.join("logs"))
}

/// Opens this launch's Flick log, moving the last one aside. None when the
/// folder cannot be made, in which case messages go only to stderr.
pub fn open() -> Option<File> {
    let folder = default_folder()?;
    std::fs::create_dir_all(&folder).ok()?;
    let log = folder.join("flick.log");
    let _ = std::fs::rename(&log, folder.join("flick.previous.log"));
    let file = File::create(&log).ok()?;
    let _ = FOLDER.set(folder);
    Some(file)
}

/// The folder this launch logs to, once [`open`] succeeded.
pub fn folder() -> Option<&'static PathBuf> {
    FOLDER.get()
}

/// Where libmpv writes its log, for the playing video's player only.
pub fn mpv_log() -> Option<PathBuf> {
    folder().map(|folder| folder.join("mpv.log"))
}
