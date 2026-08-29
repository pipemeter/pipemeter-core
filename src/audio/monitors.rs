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
//! `pactl` rather than the node's own Props: the monitor is a source in
//! its own right, with a volume the sink's `channelVolumes` does not
//! reach, and `pactl` is the documented way to address it.

use std::process::Command;

/// What a monitor should read.
const UNITY: &str = "100%";

/// Put a monitor back to unity if it has drifted.
///
/// Returns true if it had to act, so the caller can say so once rather
/// than every tick.
pub fn hold_at_unity(sink: &str) -> bool {
    let source = format!("{sink}.monitor");
    if reads_unity(&source) {
        return false;
    }
    let done = Command::new("pactl")
        .args(["set-source-volume", &source, UNITY])
        .output()
        .is_ok_and(|out| out.status.success());
    if !done {
        log::debug!("could not set {source} to unity");
    }
    done
}

/// Whether the monitor is already at unity, to avoid writing every tick.
fn reads_unity(source: &str) -> bool {
    let Ok(out) = Command::new("pactl").args(["list", "sources"]).output() else {
        return true;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    volume_percent(&text, source).is_none_or(|percent| percent == 100)
}

/// The first volume percentage reported for `source`.
///
/// Its own function so it can be tested without `PipeWire`: `pactl`
/// prints a block per source and the volume line sits a few lines under
/// the name.
#[must_use]
pub fn volume_percent(text: &str, source: &str) -> Option<u32> {
    let at = text.find(&format!("Name: {source}\n"))?;
    let line = text[at..]
        .lines()
        .find(|line| line.trim_start().starts_with("Volume:"))?;
    let percent = line.split('/').nth(1)?.trim().trim_end_matches('%');
    percent.trim().parse().ok()
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

    #[test]
    fn an_unknown_source_reads_as_nothing() {
        assert_eq!(volume_percent(LISTING, "nothing.monitor"), None);
    }
}
