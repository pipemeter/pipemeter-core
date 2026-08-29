//! The virtual sinks that back the mixer's own strips and buses.
//!
//! Voicemeeter ships fixed virtual devices — VAIO, AUX, VAIO 3 on the input
//! side and B1-B3 on the output side — and the two sides face opposite ways.
//!
//! The input ones are what applications **play into**, so they are ordinary
//! sinks and the desktop lists them as output devices. That is right, and it
//! is what Windows does too: "Voicemeeter Input" is a playback device there.
//!
//! The buses are what applications **record from**. On Windows "Voicemeeter
//! Out B1" is a recording device, and the Linux equivalent is a null sink
//! declared `Audio/Source/Virtual`: it keeps the input ports the mixer routes
//! into, and the desktop lists it among the microphones rather than among the
//! speakers.
//!
//! Making them plain sinks, as they were, left the mix reachable only through
//! the sink's monitor — which works, but which KDE does not show, so the
//! buses looked like six output devices and no way to record any of them.
//!
//! They *do* linger past exit. Other applications and the desktop's own
//! routing point at these devices; tearing them down on quit would silently
//! break every one of those assignments, and KDE would scatter the streams
//! onto whatever it picked next. They are removed only when asked, from the
//! menu.
//!
//! The cost of lingering is that a second run must not create a second set,
//! so creation skips any name already present.

use std::collections::HashSet;

use pipewire::core::CoreRc;
use pipewire::node::Node;

/// A virtual sink to create at startup.
#[derive(Debug)]
pub struct Spec {
    /// `PipeWire` node name, e.g. `pipemeter_main`.
    pub name: &'static str,
    /// Human-readable name shown by other applications.
    pub description: &'static str,
    /// What the desktop should file it under.
    pub class: &'static str,
}

/// Applications play into these, so they belong with the speakers.
pub const CLASS_INPUT: &str = "Audio/Sink";
/// Applications record from these.
///
/// They are plain sinks, and what a capture application reads is the sink's
/// monitor. Declaring them `Audio/Source/Virtual` puts them in the desktop's
/// microphone list, which is where they belong and is what Voicemeeter does,
/// but a node created that way through this factory carries no audio at all:
/// playing into one and recording from it measures digital silence.
///
/// Discoverability is not worth silence, so they stay sinks until the
/// virtual-source path is made to work. See TODO.md.
pub const CLASS_BUS: &str = "Audio/Sink";

/// Input-side virtual sinks: the three virtual strips. Apps select these as
/// their output device, which is why the descriptions read like devices.
pub const VIRTUAL_INPUTS: [Spec; 3] = [
    Spec {
        name: "pipemeter_vaio",
        description: "PipeMeter VAIO",
        class: CLASS_INPUT,
    },
    Spec {
        name: "pipemeter_aux",
        description: "PipeMeter AUX",
        class: CLASS_INPUT,
    },
    Spec {
        name: "pipemeter_vaio3",
        description: "PipeMeter VAIO 3",
        class: CLASS_INPUT,
    },
];

/// Taps on the five hardware outs.
///
/// Voicemeeter sells these as the "VAIO Extension": with it, A1 to A5 also
/// appear as recording devices, so a capture application can take the mix
/// going to a pair of speakers without stealing the speakers. There is no
/// licence to buy here, so they are simply always present.
///
/// Each is fed alongside its hardware out — anything routed to A1 is routed
/// here too — because the hardware device itself has no monitor we can rely
/// on: it may be an ALSA sink whose monitor carries something else, or it
/// may not be assigned at all.
pub const HARDWARE_TAPS: [Spec; 5] = [
    Spec {
        name: "pipemeter_a1",
        description: "PipeMeter Out A1",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_a2",
        description: "PipeMeter Out A2",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_a3",
        description: "PipeMeter Out A3",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_a4",
        description: "PipeMeter Out A4",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_a5",
        description: "PipeMeter Out A5",
        class: CLASS_BUS,
    },
];

/// Output-side virtual devices: buses B1-B3. Declared as sources so the
/// desktop lists them among the microphones, which is where anything wanting
/// to record the mix will look.
pub const VIRTUAL_BUSES: [Spec; 3] = [
    Spec {
        name: "pipemeter_b1",
        description: "PipeMeter B1",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_b2",
        description: "PipeMeter B2",
        class: CLASS_BUS,
    },
    Spec {
        name: "pipemeter_b3",
        description: "PipeMeter B3",
        class: CLASS_BUS,
    },
];

/// Create any virtual sink that is not already present.
///
/// `existing` is the set of node names the registry already knows about, so
/// a second run adopts the previous run's devices instead of duplicating
/// them. Returns proxies for the ones actually created.
pub fn create_missing<S: std::hash::BuildHasher>(
    core: &CoreRc,
    existing: &HashSet<String, S>,
) -> Vec<Node> {
    all_specs()
        .filter(|spec| !existing.contains(spec.name))
        .filter_map(|spec| create(core, spec))
        .collect()
}

/// Every sink this application owns, inputs then buses.
pub fn all_specs() -> impl Iterator<Item = &'static Spec> {
    VIRTUAL_INPUTS
        .iter()
        .chain(HARDWARE_TAPS.iter())
        .chain(VIRTUAL_BUSES.iter())
}

/// The capture tap for a bus, if it has one. Only the five hardware outs do;
/// the B buses are already capture devices in their own right.
#[must_use]
pub fn tap_for(bus: usize) -> Option<&'static str> {
    HARDWARE_TAPS.get(bus).map(|spec| spec.name)
}

/// Create one null sink. Returns `None` if the daemon refuses it — a missing
/// strip is better than refusing to start.
fn create(core: &CoreRc, spec: &Spec) -> Option<Node> {
    let props = pipewire::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => spec.name,
        "node.description" => spec.description,
        "media.class" => spec.class,
        "object.linger" => "1",
        "audio.position" => "FL,FR",
        "node.always-process" => "true",
        "monitor.channel-volumes" => "true",
        "priority.session" => "100",
        "priority.driver" => "100"
    };

    match core.create_object::<Node>("adapter", &props) {
        Ok(node) => Some(node),
        Err(err) => {
            log::warn!("could not create virtual sink {}: {err}", spec.name);
            None
        }
    }
}

/// Whether one of our devices was created with the wrong class.
///
/// A bus from a build that made them plain sinks still works for routing but
/// is filed with the speakers, so it is replaced rather than adopted.
#[must_use]
pub fn wrong_class(name: &str, class: &str) -> bool {
    all_specs().any(|spec| spec.name == name && spec.class != class)
}

/// True if `name` is one of ours. Used to keep our own sinks out of the
/// device lists offered for hardware assignment — you should not be able to
/// route a strip's output back into its own input.
pub const PREFIX: &str = "pipemeter_";

/// Whether a node is one this library created.
#[must_use]
pub fn is_ours(name: &str) -> bool {
    all_specs().any(|s| s.name == name)
}

/// The sink applications should play into when the mixer is holding the
/// system defaults: the first virtual input, which is what Voicemeeter's
/// VAIO is for.
pub const DEFAULT_SINK: &str = "pipemeter_vaio";

/// And the source they should record from: B1, the first virtual bus.
pub const DEFAULT_SOURCE: &str = "pipemeter_b1";

#[cfg(test)]
mod tests {
    use super::{all_specs, is_ours};

    #[test]
    fn recognises_own_sinks() {
        assert!(is_ours("pipemeter_vaio"));
        assert!(is_ours("pipemeter_b3"));
        assert!(!is_ours("alsa_output.usb-something.analog-stereo"));
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = all_specs().map(|s| s.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
    }
}
