//! Where the mixer keeps its files.
//!
//! In core rather than beside the settings reader because the device list
//! wants it too, and neither should have to ask the other.

use std::path::PathBuf;

/// The user's Documents directory, or their home if the desktop has not
/// defined one separately.
#[must_use]
pub fn documents_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DOCUMENTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = home.join(".config").join("user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(config) {
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("XDG_DOCUMENTS_DIR=") {
                let value = value.trim().trim_matches('"');
                return Some(PathBuf::from(
                    value.replace("$HOME", &home.to_string_lossy()),
                ));
            }
        }
    }
    Some(home)
}
