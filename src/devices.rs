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

use crate::audio::{Direction, Kind};

/// Seconds since the Unix epoch, UTC.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}


/// A device the mixer knows of, present or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// The `PipeWire` node name, which is what an assignment stores.
    pub name: String,
    /// What to show a person.
    pub description: String,
    pub direction: Direction,
    /// Hardware, or made up in software.
    ///
    /// Remembered rather than worked out on demand, because it cannot be
    /// worked out on demand: an unplugged device is not in the graph to
    /// ask. Guessing it from the node name is what this replaces, and the
    /// guess was wrong for exactly the devices that come and go.
    pub kind: Kind,
    /// When it was last in the graph, as seconds since the Unix epoch,
    /// UTC. `None` for a device remembered from before this was recorded.
    ///
    /// What lets the interface say "last seen 2 weeks ago" about something
    /// unplugged, which is the difference between a headset charging and
    /// one sold last year.
    pub last_seen: Option<u64>,
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
    pub fn remember(
        &mut self,
        name: &str,
        description: &str,
        direction: Direction,
        kind: Kind,
    ) -> bool {
        let at = now();
        if let Some(known) = self.seen.get_mut(name) {
            known.present = true;
            // A live look settles the kind, whatever was assumed of it
            // while it was away.
            let rekinded = known.kind != kind;
            known.kind = kind;
            known.last_seen = Some(at);
            if description.is_empty() || known.description == description {
                return rekinded;
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
                kind,
                last_seen: Some(at),
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
    pub fn remember_absent(
        &mut self,
        name: &str,
        description: &str,
        direction: Direction,
        kind: Kind,
        last_seen: Option<u64>,
    ) {
        self.seen.insert(
            name.to_owned(),
            Known {
                name: name.to_owned(),
                description: description.to_owned(),
                direction,
                kind,
                last_seen,
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
    use super::{Direction, Kind, Registry};

    fn populated() -> Registry {
        let mut registry = Registry::default();
        registry.remember("alsa_output.hdmi", "HDMI Audio", Direction::Sink, Kind::Physical);
        registry.remember(
            "alsa_input.mic",
            "Headset Microphone",
            Direction::Source,
            Kind::Physical,
        );
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink, Kind::Physical);
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
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink, Kind::Physical);
        assert!(registry.is_present("bluez.headset"));
    }

    #[test]
    fn the_present_ones_are_listed_first() {
        let mut registry = populated();
        registry.mark_all_absent();
        registry.remember("bluez.headset", "WH-1000XM4", Direction::Sink, Kind::Physical);
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
        registry.remember("x", "x", Direction::Sink, Kind::Virtual);
        assert!(registry.remember("x", "Proper Name", Direction::Sink, Kind::Virtual));
        assert_eq!(registry.description_of("x"), Some("Proper Name"));
    }

    #[test]
    fn seeing_the_same_device_twice_is_not_a_change() {
        let mut registry = Registry::default();
        assert!(registry.remember("x", "X", Direction::Sink, Kind::Virtual));
        assert!(!registry.remember("x", "X", Direction::Sink, Kind::Virtual));
    }

    /// What loading the settings file does: names without presence.
    #[test]
    fn a_remembered_device_starts_out_absent() {
        let mut registry = Registry::default();
        registry.remember_absent(
            "alsa_input.mic",
            "Headset Microphone",
            Direction::Source,
            Kind::Physical,
            None,
        );
        assert_eq!(
            registry.description_of("alsa_input.mic"),
            Some("Headset Microphone")
        );
        assert!(!registry.is_present("alsa_input.mic"));
        assert_eq!(registry.of(Direction::Source).len(), 1);
    }

    /// The point of remembering the kind: a device that is away cannot be
    /// asked what it is, and the name is not the answer.
    #[test]
    fn an_absent_device_keeps_the_kind_it_was_stored_with() {
        let mut registry = Registry::default();
        registry.remember_absent(
            "bluez_output.AC80",
            "WH-1000XM4",
            Direction::Sink,
            Kind::Physical,
            Some(1_700_000_000),
        );
        assert_eq!(registry.of(Direction::Sink)[0].kind, Kind::Physical);
    }

    /// Seeing it live settles the kind, whatever was assumed while it was
    /// away - and counts as a change, so the list gets written back.
    #[test]
    fn a_live_look_corrects_a_stored_kind() {
        let mut registry = Registry::default();
        registry.remember_absent("x", "X", Direction::Sink, Kind::Virtual, None);
        assert!(registry.remember("x", "X", Direction::Sink, Kind::Physical));
        assert_eq!(registry.of(Direction::Sink)[0].kind, Kind::Physical);
        assert!(!registry.remember("x", "X", Direction::Sink, Kind::Physical));
    }

    /// A stored timestamp survives being read back, and seeing the device
    /// live moves it on.
    #[test]
    fn a_sighting_is_stamped() {
        let mut registry = Registry::default();
        registry.remember_absent("x", "X", Direction::Sink, Kind::Virtual, Some(1_700_000_000));
        assert_eq!(registry.of(Direction::Sink)[0].last_seen, Some(1_700_000_000));
        registry.remember("x", "X", Direction::Sink, Kind::Virtual);
        let moved = registry.of(Direction::Sink)[0].last_seen.expect("stamped");
        assert!(moved > 1_700_000_000, "a live sighting should be recent");
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
        registry.remember("alsa_output.hdmi", "HDMI Audio", Direction::Sink, Kind::Physical);
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
