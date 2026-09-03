//! Keeping the virtual buses' monitors at unity.
//!
//! A B bus is a sink and what an application records is its monitor, so
//! the monitor's own volume is the bus's output level - and nothing in
//! the mixer shows it. One of them was found sitting at 51%, which is
//! -17.76 dB, and everything recorded from that bus was quietly that much
//! down while the fader read 0 dB and the meter agreed with the fader.
//!
//! The session manager restores that volume whenever the node reappears,
//! so setting it once is not enough; this is called on the same slow tick
//! as the default-device hold.
//!
//! `pactl` rather than the node's own Props: the source has a volume of
//! its own that the sink's `channelVolumes` does not reach, and `pactl` is
//! the documented way to address it.
//!
//! The name to address is the caller's to give, and it is not always the
//! sink's with `.monitor` on the end. A B bus is declared
//! `Audio/Source/Virtual` so the desktop lists it among the microphones,
//! which means it *is* a source - `pipemeter_b1`, with no monitor of its
//! own. This module used to append `.monitor` itself, so once the buses
//! changed class it spent every tick asking after a source that does not
//! exist, finding nothing, reading nothing as unity, and doing nothing.
//! The drift it was written to prevent was sitting there the whole time:
//! `pipemeter_b1` was found at 51% on the machine this was fixed on.

use std::process::Command;

/// What a monitor should read.
const UNITY: &str = "100%";

/// Every source `pactl` knows about, as one listing.
///
/// Taken once and handed to each call rather than fetched per bus: the
/// listing carries every source on the machine with all its properties,
/// and asking for it three times a tick - once per B bus, forever - was
/// three processes and three parses to answer one question.
///
/// `None` when `pactl` could not be run at all, which is not something to
/// keep retrying every two seconds.
#[must_use]
pub fn source_listing() -> Option<String> {
    let out = Command::new("pactl")
        .args(["list", "sources"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Put a source back to unity if it has drifted.
///
/// `source` is the name as `pactl` lists it: a sink's is its own with
/// `.monitor` appended, while a bus declared `Audio/Source/Virtual` is a
/// source already and has none. Getting that wrong is silent - the name is
/// simply never found, which reads as nothing to do.
///
/// Returns true if it had to act, so the caller can say so once rather
/// than every tick.
pub fn hold_at_unity(listing: &str, source: &str) -> bool {
    if volume_percent(listing, source).is_none_or(|percent| percent == 100) {
        return false;
    }
    let done = Command::new("pactl")
        .args(["set-source-volume", source, UNITY])
        .output()
        .is_ok_and(|out| out.status.success());
    if !done {
        log::debug!("could not set {source} to unity");
    }
    done
}

/// The quietest channel of `source`, as a percentage.
///
/// The quietest rather than the first: `set-source-volume` writes every
/// channel at once, so what matters is whether *any* of them has drifted.
/// Reading only the front-left would leave a monitor whose right channel
/// alone had been pulled down sitting there, reported as fine.
///
/// Its own function so it can be tested without `PipeWire`: `pactl` prints
/// a block per source and the volume line sits a few lines under the name.
#[must_use]
pub fn volume_percent(text: &str, source: &str) -> Option<u32> {
    let at = text.find(&format!("Name: {source}\n"))?;
    let line = text[at..]
        .lines()
        .find(|line| line.trim_start().starts_with("Volume:"))?;
    line.split('/')
        .filter_map(|part| part.trim().strip_suffix('%'))
        .filter_map(|percent| percent.trim().parse::<u32>().ok())
        .min()
}

#[cfg(test)]
mod tests {
    use super::volume_percent;

    const LISTING: &str = "Source #1\n\
\tName: pipemeter_b1.monitor\n\
\tMute: no\n\
\tVolume: front-left: 33153 /  51% / -17.76 dB,   front-right: 33153 /  51% / -17.76 dB\n\
\tBase Volume: 65536 / 100% / 0.00 dB\n\
Source #2\n\
\tName: pipemeter_b2.monitor\n\
\tVolume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB\n";

    #[test]
    fn a_monitors_own_volume_is_read_from_its_block() {
        assert_eq!(volume_percent(LISTING, "pipemeter_b1.monitor"), Some(51));
        assert_eq!(volume_percent(LISTING, "pipemeter_b2.monitor"), Some(100));
    }

    /// The bug: reading the first volume in the listing answers every
    /// question with the first source's.
    #[test]
    fn each_monitor_reads_its_own() {
        assert_ne!(
            volume_percent(LISTING, "pipemeter_b2.monitor"),
            volume_percent(LISTING, "pipemeter_b1.monitor")
        );
    }

    /// A monitor with one channel pulled down is drifted, even if the
    /// other reads unity. Taking the first percentage on the line called
    /// that fine and left it there.
    #[test]
    fn one_quiet_channel_is_enough_to_count_as_drifted() {
        const LOPSIDED: &str = "Source #3\n\
\tName: pipemeter_b3.monitor\n\
\tVolume: front-left: 65536 / 100% / 0.00 dB,   front-right: 33153 /  51% / -17.76 dB\n";
        assert_eq!(volume_percent(LOPSIDED, "pipemeter_b3.monitor"), Some(51));
    }

    #[test]
    fn an_unknown_source_reads_as_nothing() {
        assert_eq!(volume_percent(LISTING, "nothing.monitor"), None);
    }
}
