//! Every device the mixer has ever seen.
//!
//! A hardware out assigned to a headset stops existing the moment the headset
//! is unplugged, and a mixer that only knows what is plugged in right now
//! forgets the assignment with it — so the strip reads "Select Input Device"
//! and the routing quietly goes nowhere.
//!
//! So devices are remembered by name. An assignment to one that is not here
//! is kept, shown in red, and starts working again by itself the moment the
//! device comes back. The picker offers the absent ones too, greyed, because
//! you should be able to set up a headset's routing while it is charging.
//!
//! The file is one line per device rather than XML like everything else here.
//! It is a list of three strings with no structure to speak of, and the
//! parser, the writer and their tests would be more code than the feature.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::audio::Direction;

/// A device the mixer knows of, present or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// The `PipeWire` node name, which is what an assignment stores.
    pub name: String,
    /// What to show a person.
    pub description: String,
    pub direction: Direction,
    /// Whether it is in the graph right now.
    pub present: bool,
}

/// Everything seen, by node name.
#[derive(Debug, Default, Clone)]
pub struct Registry {
    /// Sorted, so the picker's order does not shuffle between launches.
    seen: BTreeMap<String, Known>,
}

impl Registry {
    /// Note a device that is in the graph now.
    ///
    /// Returns whether this is the first time it has been seen, which is
    /// what decides if the list needs writing back out.
    pub fn remember(&mut self, name: &str, description: &str, direction: Direction) -> bool {
        if let Some(known) = self.seen.get_mut(name) {
            known.present = true;
            // A description can improve once a device is fully set up, and
            // the better one is the one worth keeping.
            if description.is_empty() || known.description == description {
                return false;
            }
            description.clone_into(&mut known.description);
            return true;
        }
        self.seen.insert(
            name.to_owned(),
            Known {
                name: name.to_owned(),
                description: description.to_owned(),
                direction,
                present: true,
            },
        );
        true
    }

    /// Mark everything absent, before a fresh sweep marks what is back.
    pub fn mark_all_absent(&mut self) {
        for known in self.seen.values_mut() {
            known.present = false;
        }
    }

    /// Whether a device is in the graph right now.
    #[must_use]
    pub fn is_present(&self, name: &str) -> bool {
        self.seen.get(name).is_some_and(|known| known.present)
    }

    /// What to show a person for a node name, present or not.
    #[must_use]
    pub fn description_of(&self, name: &str) -> Option<&str> {
        self.seen.get(name).map(|known| known.description.as_str())
    }

    /// Everything of one direction, present first and absent after.
    ///
    /// Present first because that is what someone is usually reaching for,
    /// and burying a plugged-in headset under six that are not is the sort
    /// of list that makes people give up on a picker.
    #[must_use]
    pub fn of(&self, direction: Direction) -> Vec<&Known> {
        let mut all: Vec<&Known> = self
            .seen
            .values()
            .filter(|known| known.direction == direction)
            .collect();
        all.sort_by_key(|known| (!known.present, known.description.to_lowercase()));
        all
    }

    /// Forget a device outright. Only the menu and the picker do this.
    pub fn forget(&mut self, name: &str) {
        self.seen.remove(name);
    }

    /// Drop everything that is not plugged in right now.
    ///
    /// Returns how many went. Assignments to them are left alone: a strip
    /// still points at the name, so plugging the device back in picks it up
    /// again and remembers it afresh. This tidies the picker, it does not
    /// unwire the mixer.
    pub fn forget_absent(&mut self) -> usize {
        let before = self.seen.len();
        self.seen.retain(|_, known| known.present);
        before - self.seen.len()
    }

    /// Whether nothing is remembered yet, which is how a first run looks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// How many are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }
}

/// Where the list lives.
#[must_use]
pub fn path() -> Option<PathBuf> {
    Some(
        crate::paths::documents_dir()?
            .join("Pipemeeter")
            .join("Pipemeter_Devices.tsv"),
    )
}

/// Where the list used to live, before the library took its own name.
///
/// Read when the current one is missing, so a mixer that has been running
/// for a while does not forget every device it knows the first time it is
/// updated. Never written; the next save moves the list to the new path.
fn former_path() -> Option<PathBuf> {
    Some(
        crate::paths::documents_dir()?
            .join("Pipemeeter")
            .join("Pipemeeter_Devices.tsv"),
    )
}

/// Read the remembered list. Everything read starts out absent: only the
/// graph can say otherwise.
#[must_use]
pub fn load() -> Registry {
    let Some(path) = path() else {
        return Registry::default();
    };
    let text = if let Ok(text) = std::fs::read_to_string(&path) {
        text
    } else {
        let Some(former) = former_path().filter(|p| p.exists()) else {
            return Registry::default();
        };
        let Ok(text) = std::fs::read_to_string(&former) else {
            return Registry::default();
        };
        log::info!("carrying the device list over from {}", former.display());
        text
    };
    let registry = parse(&text);
    log::info!(
        "remembered {} device(s) from {}",
        registry.len(),
        path.display()
    );
    registry
}

/// Write the remembered list out.
pub fn save(registry: &Registry) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(&path, render(registry)) {
        log::warn!("could not write {}: {err}", path.display());
    }
}

/// One line per device: direction, node name, description.
fn render(registry: &Registry) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("# direction\tnode name\tdescription\n");
    for known in registry.seen.values() {
        // A tab or a newline in a device name would split the line in two.
        // Neither has ever been seen, but a name comes from the outside.
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let _ = writeln!(
            out,
            "{}\t{}\t{}",
            match known.direction {
                Direction::Sink => "out",
                Direction::Source => "in",
            },
            clean(&known.name),
            clean(&known.description),
        );
    }
    out
}

/// The other half of [`render`].
fn parse(text: &str) -> Registry {
    let mut registry = Registry::default();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(direction), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let description = fields.next().unwrap_or(name);
        let direction = match direction {
            "out" => Direction::Sink,
            "in" => Direction::Source,
            // A line we do not understand is skipped rather than guessed at.
            _ => continue,
        };
        registry.seen.insert(
            name.to_owned(),
            Known {
                name: name.to_owned(),
                description: description.to_owned(),
                direction,
                present: false,
            },
        );
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::{Direction, Registry, parse, render};

    fn populated() -> Registry {
        let mut registry = Registry::default();
        registry.remember("alsa_output.hdmi", "HDMI Audio", Direction::Sink);
        registry.remember("alsa_input.mic", "Headset Microphone", Direction::Source);
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink);
        registry
    }

    #[test]
    fn a_device_survives_being_unplugged() {
        // The whole point: the assignment outlives the device, so the name
        // has to as well.
        let mut registry = populated();
        registry.mark_all_absent();
        assert!(!registry.is_present("bluez.headset"));
        assert_eq!(
            registry.description_of("bluez.headset"),
            Some("WH-1000XM4"),
            "an unplugged device should still have a name",
        );
    }

    #[test]
    fn it_comes_back_present_when_it_comes_back() {
        let mut registry = populated();
        registry.mark_all_absent();
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink);
        assert!(registry.is_present("bluez.headset"));
    }

    #[test]
    fn the_present_ones_are_listed_first() {
        let mut registry = populated();
        registry.mark_all_absent();
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink);
        let sinks = registry.of(Direction::Sink);
        assert_eq!(sinks[0].name, "bluez.headset", "the plugged-in one leads");
        assert!(!sinks[1].present);
    }

    #[test]
    fn directions_do_not_mix() {
        let registry = populated();
        assert_eq!(registry.of(Direction::Source).len(), 1);
        assert_eq!(registry.of(Direction::Sink).len(), 2);
    }

    #[test]
    fn a_better_description_replaces_a_worse_one() {
        let mut registry = Registry::default();
        registry.remember("x", "x", Direction::Sink);
        assert!(registry.remember("x", "Proper Name", Direction::Sink));
        assert_eq!(registry.description_of("x"), Some("Proper Name"));
    }

    #[test]
    fn seeing_the_same_device_twice_is_not_a_change() {
        let mut registry = Registry::default();
        assert!(registry.remember("x", "X", Direction::Sink));
        assert!(!registry.remember("x", "X", Direction::Sink));
    }

    #[test]
    fn the_file_round_trips() {
        let before = populated();
        let after = parse(&render(&before));
        assert_eq!(after.len(), before.len());
        assert_eq!(
            after.description_of("alsa_input.mic"),
            Some("Headset Microphone")
        );
        assert_eq!(after.of(Direction::Source).len(), 1);
        // Nothing read from a file is present until the graph says so.
        assert!(!after.is_present("alsa_input.mic"));
    }

    #[test]
    fn a_tab_in_a_name_cannot_split_the_line() {
        let mut registry = Registry::default();
        registry.remember("odd\tname", "Odd\tDescription", Direction::Sink);
        let after = parse(&render(&registry));
        assert_eq!(after.len(), 1, "the line was split by its own contents");
    }

    #[test]
    fn rubbish_lines_are_skipped_not_guessed_at() {
        let after = parse("# a comment\n\nsideways\tthing\tThing\nout\tgood\tGood\n");
        assert_eq!(after.len(), 1);
        assert_eq!(after.description_of("good"), Some("Good"));
    }

    #[test]
    fn forgetting_the_absent_spares_what_is_plugged_in() {
        let mut registry = populated();
        registry.mark_all_absent();
        registry.remember("alsa_output.hdmi", "HDMI Audio", Direction::Sink);
        assert_eq!(registry.forget_absent(), 2);
        assert_eq!(registry.len(), 1);
        assert!(registry.is_present("alsa_output.hdmi"));
    }

    #[test]
    fn forgetting_the_absent_when_all_are_here_removes_nothing() {
        let mut registry = populated();
        assert_eq!(registry.forget_absent(), 0);
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn forgetting_removes_it() {
        let mut registry = populated();
        registry.forget("bluez.headset");
        assert_eq!(registry.description_of("bluez.headset"), None);
    }
}
