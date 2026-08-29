//! Where the mixer keeps its files.
//!
//! In core rather than beside the settings reader because the device list
//! wants it too, and neither should have to ask the other.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The folder everything of ours lives in, under the documents directory.
///
/// The library's name rather than the mixer's: what is on disk is storage
/// rather than something we put a brand on.
pub const DIR: &str = "Pipemeter";

/// An explicit directory, set once from the command line.
static OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Point every file at `dir` instead of the usual place.
///
/// For `--config-dir`, so a test run cannot write over settings somebody
/// is actually using. Takes effect only if nothing has read a path yet,
/// which is why `main` calls it before anything else.
pub fn set_config_dir(dir: PathBuf) {
    if OVERRIDE.set(dir).is_err() {
        log::warn!("the configuration directory was already fixed; ignoring the later one");
    }
}

/// Where our files go: the directory given on the command line, or
/// `<documents>/Pipemeter`.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = OVERRIDE.get() {
        return Some(dir.clone());
    }
    Some(documents_dir()?.join(DIR))
}

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

#[cfg(test)]
mod tests {
    /// Without an override the answer still sits under the documents
    /// directory, so an ordinary run is unaffected by the option existing.
    #[test]
    fn the_default_is_still_under_documents() {
        let dir = super::config_dir().expect("a config dir");
        assert!(dir.ends_with(super::DIR), "{}", dir.display());
    }
}
