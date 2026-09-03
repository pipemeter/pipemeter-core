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
    if let Ok(text) = std::fs::read_to_string(config)
        && let Some(dir) = documents_from(&text, &home.to_string_lossy())
    {
        return Some(dir);
    }
    Some(home)
}

/// Read `XDG_DOCUMENTS_DIR` out of a `user-dirs.dirs`.
///
/// Its own function so the quoting and the `$HOME` substitution can be
/// tested without a home directory to read - the same reason
/// `defaults::parse_name` and `monitors::volume_percent` are split out.
/// The file is shell syntax: the value is quoted, `$HOME` is literal, and
/// a commented-out line has to stay commented out.
#[must_use]
pub fn documents_from(text: &str, home: &str) -> Option<PathBuf> {
    let value = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("XDG_DOCUMENTS_DIR="))?
        .trim()
        .trim_matches('"');
    (!value.is_empty()).then(|| PathBuf::from(value.replace("$HOME", home)))
}

#[cfg(test)]
mod tests {
    use super::documents_from;

    /// What a desktop actually writes, quotes and `$HOME` and all.
    const DIRS: &str = "# This file is written by xdg-user-dirs-update\n\
XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
XDG_DOCUMENTS_DIR=\"$HOME/Documents\"\n\
XDG_MUSIC_DIR=\"$HOME/Music\"\n";

    #[test]
    fn the_documents_line_is_read_with_its_quotes_and_home() {
        assert_eq!(
            documents_from(DIRS, "/home/someone"),
            Some(std::path::PathBuf::from("/home/someone/Documents"))
        );
    }

    /// A desktop that points documents somewhere absolute, without `$HOME`.
    #[test]
    fn an_absolute_path_is_taken_as_it_stands() {
        assert_eq!(
            documents_from("XDG_DOCUMENTS_DIR=\"/mnt/work/docs\"\n", "/home/someone"),
            Some(std::path::PathBuf::from("/mnt/work/docs"))
        );
    }

    /// No line, an empty value, or one that has been commented out all
    /// mean "not configured" - and the caller falls back to the home
    /// directory rather than to an empty path.
    #[test]
    fn nothing_configured_reads_as_nothing() {
        assert_eq!(
            documents_from("XDG_MUSIC_DIR=\"$HOME/Music\"\n", "/h"),
            None
        );
        assert_eq!(documents_from("XDG_DOCUMENTS_DIR=\"\"\n", "/h"), None);
        assert_eq!(
            documents_from("#XDG_DOCUMENTS_DIR=\"$HOME/D\"\n", "/h"),
            None
        );
    }

    /// Without an override the answer still sits under the documents
    /// directory, so an ordinary run is unaffected by the option existing.
    #[test]
    fn the_default_is_still_under_documents() {
        let dir = super::config_dir().expect("a config dir");
        assert!(dir.ends_with(super::DIR), "{}", dir.display());
    }
}
