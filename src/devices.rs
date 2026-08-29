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
//! The list is not stored here. It goes in the settings file with
//! everything else, under `PipemeterSeenDevices`, because a device history
//! that can drift out of step with the assignments referring to it is
//! worse than no history - and because one settings file is enough. This
//! module keeps the list in memory and leaves the writing to whoever owns
//! that file.

use std::collections::BTreeMap;

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

    /// Everything not plugged in right now, in the order it is stored.
    ///
    /// What the Forget Old Devices window offers: a person cannot decide
    /// what to drop without seeing it, and the count alone never said
    /// which ones were about to go.
    #[must_use]
    pub fn absent(&self) -> Vec<&Known> {
        self.seen.values().filter(|known| !known.present).collect()
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

/// Note a device that is remembered but not in the graph.
///
/// What loading the settings file does. Everything read starts out absent:
/// only the graph can say otherwise.
impl Registry {
    pub fn remember_absent(&mut self, name: &str, description: &str, direction: Direction) {
        self.seen.insert(
            name.to_owned(),
            Known {
                name: name.to_owned(),
                description: description.to_owned(),
                direction,
                present: false,
            },
        );
    }

    /// Everything remembered, in stored order, for writing back out.
    pub fn all(&self) -> impl Iterator<Item = &Known> {
        self.seen.values()
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Registry};

    fn populated() -> Registry {
        let mut registry = Registry::default();
        registry.remember("alsa_output.hdmi", "HDMI Audio", Direction::Sink);
        registry.remember("alsa_input.mic", "Headset Microphone", Direction::Source);
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink);
        registry
    }

    #[test]
    fn a_device_survives_being_unplugged() {
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

    /// What loading the settings file does: names without presence.
    #[test]
    fn a_remembered_device_starts_out_absent() {
        let mut registry = Registry::default();
        registry.remember_absent("alsa_input.mic", "Headset Microphone", Direction::Source);
        assert_eq!(
            registry.description_of("alsa_input.mic"),
            Some("Headset Microphone")
        );
        assert!(!registry.is_present("alsa_input.mic"));
        assert_eq!(registry.of(Direction::Source).len(), 1);
    }

    #[test]
    fn everything_remembered_can_be_listed_for_writing() {
        let registry = populated();
        assert_eq!(registry.all().count(), 3);
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
