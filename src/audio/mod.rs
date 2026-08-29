//! `PipeWire` backend.
//!
//! `PipeWire`'s objects are not `Send`, and its main loop blocks, so the whole
//! connection lives on its own thread and talks to the UI over a channel.
//! The UI never touches a `PipeWire` object directly — it drains [`Event`]s
//! once per frame and reads the resulting snapshot.

pub mod eq;
pub mod fx;
mod links;
mod meters;
mod nodes;
mod recorder;
pub mod sinks;
pub mod wav;

use std::sync::mpsc::{Receiver, TryRecvError};

pub use links::End;
// Re-exported for the FX sends, which route a specific pair rather than a
// node's main output. Nothing calls it yet; see the FX section of TODO.md.
pub use links::{Route, Tap};
pub use nodes::{Command, Device, Direction, LinkInfo, Stream};

/// Fader dB to the linear amplitude `PipeWire` wants.
///
/// Muting is amplitude zero rather than a separate flag, so one path covers
/// both and a mute cannot be left behind by a fader move.
fn amplitude_of(gain_db: f32, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    // 10^(dB/20) is the standard amplitude conversion; unity lands on 1.0.
    10.0_f32.powf(gain_db / 20.0)
}

/// Split a pan position into a gain for each channel.
///
/// Centre leaves both at unity and each extreme silences the far side, with a
/// straight taper between. Not a constant-power law: Voicemeeter's pan holds
/// the near channel at unity rather than lifting it by 3 dB in the middle,
/// and matching what the user hears on Windows matters more here than the
/// textbook curve.
///
/// This is the left-right half of the Front/Rear pad. The front-rear axis
/// needs more than two channels to mean anything and is not applied yet;
/// see TODO.md.
fn pan_gains(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
}

#[cfg(test)]
mod pan_tests {
    use super::pan_gains;

    #[test]
    fn centre_leaves_both_channels_alone() {
        assert_eq!(pan_gains(0.0), (1.0, 1.0));
    }

    #[test]
    fn pan_holds_the_near_channel_at_unity() {
        // Not a constant-power law: hard left is unity on the left and
        // silence on the right, which is what the original does.
        assert_eq!(pan_gains(-1.0), (1.0, 0.0));
        assert_eq!(pan_gains(1.0), (0.0, 1.0));
    }

    #[test]
    fn halfway_left_halves_the_right_channel() {
        let (left, right) = pan_gains(-0.5);
        assert!((left - 1.0).abs() < 1e-6);
        assert!((right - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_pan_past_the_ends_is_clamped_not_inverted() {
        assert_eq!(pan_gains(-4.0), pan_gains(-1.0));
        assert_eq!(pan_gains(4.0), pan_gains(1.0));
    }
}

/// A rate for the log, or a dash where the node did not say.
///
/// A dash rather than a zero: plenty of nodes never publish a rate, and
/// "0 Hz" reads like a fault where "-" reads like a silence.
fn rate_of(rate: Option<u32>) -> String {
    rate.map_or_else(|| "-".to_owned(), |rate| format!("{rate} Hz"))
}

/// As [`rate_of`], for the channel count.
fn channels_of(channels: Option<u32>) -> String {
    channels.map_or_else(|| "-".to_owned(), |n| format!("{n} ch"))
}

/// A change reported by the `PipeWire` thread.
#[derive(Debug, Clone)]
pub enum Event {
    Added(Device),
    StreamAdded(Stream),
    LinkAdded(LinkInfo),
    Removed(u32),
    /// A node's format, once its proxy has reported it.
    ///
    /// Its own event because the registry announcement carries neither the
    /// channel count nor the rate: those are on the node's info, which
    /// arrives only after binding. Without it every device in the settings
    /// window lists a dash where its rate and channels should be.
    Format {
        id: u32,
        rate: Option<u32>,
        channels: Option<u32>,
    },
    /// The first registry sweep has finished, so everything that already
    /// existed has been reported. Nothing may be created before this: a
    /// sink made while the sweep is still running cannot see the lingering
    /// one it should have adopted, and makes a duplicate beside it.
    Enumerated,
    /// The connection dropped. The UI keeps its last snapshot and greys out.
    Disconnected(String),
}

/// UI-side handle to the backend.
pub struct Backend {
    rx: Receiver<Event>,
    commands: pipewire::channel::Sender<Command>,
    devices: Vec<Device>,
    /// Peak levels per node, written by the metering threads.
    levels: nodes::Levels,
    /// Applications currently playing, and the links that say where into.
    streams: Vec<Stream>,
    links: Vec<LinkInfo>,
    connected: bool,
    /// Whether the initial registry sweep has completed.
    enumerated: bool,
    error: Option<String>,
}

// Hand-written because the command sender has no Debug of its own, and the
// device list is more useful as a count than in full.
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("devices", &self.devices.len())
            .field("connected", &self.connected)
            .field("enumerated", &self.enumerated)
            .field("error", &self.error)
            .field("commands", &"<pipewire channel>")
            .field("rx", &self.rx)
            .field("streams", &self.streams.len())
            .field("links", &self.links.len())
            .field("levels", &"<shared>")
            .finish()
    }
}

impl Backend {
    /// Start the `PipeWire` thread. This never fails from the caller's point of
    /// view: if the connection cannot be made, the failure arrives as a
    /// [`Event::Disconnected`] and the UI runs with no devices.
    pub fn spawn() -> Self {
        let (rx, commands, levels) = nodes::spawn();
        Self {
            rx,
            commands,
            levels,
            devices: Vec::new(),
            streams: Vec::new(),
            links: Vec::new(),
            connected: true,
            enumerated: false,
            error: None,
        }
    }

    /// How many audio devices the backend currently knows about.
    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.iter().filter(|d| d.assignable).count()
    }

    /// Whether the backend still has a live connection.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// Throw away the dead connection and start a new one.
    ///
    /// Everything cached here described the old graph: node ids are not
    /// stable across a reconnect, so keeping any of it would leave the mixer
    /// routing to numbers that now mean something else, or nothing.
    pub fn reconnect(&mut self) {
        let (rx, commands, levels) = nodes::spawn();
        self.rx = rx;
        self.commands = commands;
        self.levels = levels;
        self.devices.clear();
        self.streams.clear();
        self.links.clear();
        self.connected = true;
        self.enumerated = false;
        self.error = None;
    }

    /// Drain pending events. Call once per frame.
    pub fn poll(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Event::Added(dev)) => {
                    // Re-announcements replace rather than duplicate.
                    if let Some(slot) = self.devices.iter_mut().find(|d| d.id == dev.id) {
                        *slot = dev;
                    } else {
                        log::info!(
                            "device appeared: {} ({}) id {} {} {} {}",
                            dev.description,
                            dev.name,
                            dev.id,
                            dev.class,
                            rate_of(dev.rate),
                            channels_of(dev.channels),
                        );
                        self.devices.push(dev);
                    }
                }
                Ok(Event::StreamAdded(stream)) => {
                    if let Some(slot) = self.streams.iter_mut().find(|s| s.id == stream.id) {
                        *slot = stream;
                    } else {
                        self.streams.push(stream);
                    }
                }
                Ok(Event::LinkAdded(link)) => {
                    if !self.links.iter().any(|l| l.id == link.id) {
                        self.links.push(link);
                    }
                }
                Ok(Event::Format { id, rate, channels }) => {
                    // Filled in rather than replacing the device: the
                    // registry told us everything else about it already.
                    if let Some(device) = self.devices.iter_mut().find(|d| d.id == id) {
                        if rate.is_some() {
                            device.rate = rate;
                        }
                        if channels.is_some() {
                            device.channels = channels;
                        }
                    }
                }
                Ok(Event::Removed(id)) => {
                    // The id could be any of the three; only one will match.
                    if let Some(gone) = self.devices.iter().find(|d| d.id == id) {
                        log::info!("device went away: {} ({})", gone.description, gone.name);
                    }
                    self.devices.retain(|d| d.id != id);
                    self.streams.retain(|s| s.id != id);
                    self.links.retain(|l| l.id != id);
                }
                Ok(Event::Enumerated) => {
                    self.enumerated = true;
                    self.dump_graph();
                }
                Ok(Event::Disconnected(err)) => {
                    self.connected = false;
                    self.error = Some(err);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.connected = false;
                    break;
                }
            }
        }
        self.devices
            .sort_by(|a, b| a.description.cmp(&b.description));
    }

    /// Every known device in one direction.
    pub fn devices(&self, direction: Direction) -> impl Iterator<Item = &Device> {
        self.devices
            .iter()
            .filter(move |d| d.direction == direction)
    }

    /// Devices a strip or bus may be assigned to: real hardware only.
    ///
    /// Our own virtual sinks are excluded. They are already strips in their
    /// own right, and offering one as a hardware out invites routing a strip
    /// back into itself.
    pub fn assignable(&self, direction: Direction) -> impl Iterator<Item = &Device> {
        self.devices(direction)
            .filter(|d| d.assignable && !sinks::is_ours(&d.name))
    }

    /// Ask the backend to route, or unroute, one node into another.
    ///
    /// Fire-and-forget: the reply arrives as registry events, if at all. A
    /// send that fails means the `PipeWire` thread is gone, which the next
    /// poll reports as a disconnect.
    pub fn set_route(&self, source: u32, target: u32, enabled: bool) {
        self.set_tapped_route(links::Route::new(source, target), enabled);
    }

    /// Route a specific pair of ports rather than a node's main output.
    ///
    /// What sends need: a strip's chain carries its main output and its FX
    /// sends on the same node, told apart only by which pair they are.
    pub fn set_tapped_route(&self, route: links::Route, enabled: bool) {
        let _ = self.commands.send(Command::SetRoute { route, enabled });
    }

    /// Write out the whole graph as the mixer sees it.
    ///
    /// Called once, the moment `PipeWire` has finished saying what already
    /// exists. A report that begins "it does not work" can then be answered
    /// without asking what was plugged in at the time.
    fn dump_graph(&self) {
        log::info!("--- graph at startup: {} devices ---", self.devices.len());
        let mut devices: Vec<&Device> = self.devices.iter().collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name));
        for device in devices {
            log::info!(
                "  {:6} {:>5}  {:>9} {:>5}  {}  [{}]",
                match device.direction {
                    Direction::Sink => "sink",
                    Direction::Source => "source",
                },
                device.id,
                rate_of(device.rate),
                channels_of(device.channels),
                device.name,
                device.description,
            );
        }
        log::info!(
            "--- {} application streams, {} links ---",
            self.streams.len(),
            self.links.len()
        );
        for stream in &self.streams {
            log::info!("  stream {:>5}  {}", stream.id, stream.name);
        }
        for link in &self.links {
            let name = |id: u32| {
                self.devices
                    .iter()
                    .find(|d| d.id == id)
                    .map_or_else(|| id.to_string(), |d| d.name.clone())
            };
            log::info!(
                "  link  {} -> {}",
                name(link.output_node),
                name(link.input_node)
            );
        }
    }

    /// The live id of a node by its `PipeWire` name, if it is present.
    /// Ids are not stable across restarts, so routing always goes through
    /// this rather than caching one.
    /// How many links leave a node right now.
    ///
    /// Asked rather than assumed: a route is a *request*, and one whose
    /// ports have not appeared is deferred inside the router. Code that
    /// treats "I asked" as "it happened" reports success and leaves a chain
    /// wired to nothing, which is exactly what the effect chains did.
    /// How many links arrive at a node.
    #[must_use]
    pub fn links_into(&self, node: u32) -> usize {
        self.links.iter().filter(|l| l.input_node == node).count()
    }

    pub fn links_from(&self, node: u32) -> usize {
        self.links.iter().filter(|l| l.output_node == node).count()
    }

    pub fn id_of(&self, name: &str) -> Option<u32> {
        self.devices.iter().find(|d| d.name == name).map(|d| d.id)
    }

    /// Applications currently playing into the named sink.
    ///
    /// Resolved through the link graph rather than any "target" property:
    /// what a stream *asked* for and what it is actually joined to can
    /// differ, and the links are the truth.
    pub fn apps_playing_into(&self, sink_name: &str) -> Vec<&Stream> {
        let Some(sink) = self.id_of(sink_name) else {
            return Vec::new();
        };
        let mut found: Vec<&Stream> = self
            .streams
            .iter()
            .filter(|stream| {
                self.links
                    .iter()
                    .any(|l| l.output_node == stream.id && l.input_node == sink)
            })
            .collect();
        // A stereo stream has a link per channel, so the same application
        // matches more than once.
        found.dedup_by(|a, b| a.id == b.id);
        found
    }

    /// Peak level of a node's two channels, or silence if it is not metered.
    pub fn level_of(&self, node_name: &str) -> (f32, f32) {
        let Some(id) = self.id_of(node_name) else {
            return (0.0, 0.0);
        };
        self.level_of_id(id)
    }

    /// Peak level of a node by its id.
    ///
    /// Applications are streams, not devices, so they are not in the table
    /// `id_of` searches - their id is the one the row already carries.
    #[must_use]
    pub fn level_of_id(&self, id: u32) -> (f32, f32) {
        self.levels
            .lock()
            .ok()
            .and_then(|map| map.get(&id).copied())
            .unwrap_or((0.0, 0.0))
    }

    /// Set a node's level from a fader position in dB.
    ///
    /// `muted` wins over the fader, so a muted strip is silent wherever its
    /// fader happens to sit.
    /// Returns whether the node was known well enough to send anything. The
    /// caller uses that to decide whether to remember the value as applied:
    /// nodes appear asynchronously, so an early call can find nothing and
    /// must be retried rather than recorded as done.
    /// Whether the backend has finished reporting what already existed.
    #[must_use]
    pub fn enumerated(&self) -> bool {
        self.enumerated
    }

    /// `pan` is -1.0 hard left to +1.0 hard right, 0.0 centred.
    pub fn set_gain(&self, node_name: &str, gain_db: f32, muted: bool, pan: f32) -> bool {
        let Some(node) = self.id_of(node_name) else {
            // Trace, not debug. A strip assigned to something unplugged
            // fails this every frame, for as long as it stays unplugged,
            // and it is not news - the mixer draws that strip in red and
            // will pick the device up by itself when it returns.
            log::trace!("set_gain: no node named {node_name}");
            return false;
        };
        log::debug!("set_gain {node_name} (id {node}) -> {gain_db} dB, muted {muted}, pan {pan}");
        let amplitude = amplitude_of(gain_db, muted);
        let (left, right) = pan_gains(pan);
        self.commands
            .send(Command::SetVolume {
                node,
                amplitude,
                balance: (left, right),
            })
            .is_ok()
    }

    /// Set an application stream's own volume and mute.
    ///
    /// No panning: an application row has a level and a mute and nothing else,
    /// the same as on the original.
    ///
    /// By node id rather than by name: the row already carries the id the
    /// registry gave it, and two copies of the same program are two streams
    /// sharing one name.
    pub fn set_app(&self, node: u32, volume: f32, muted: bool) {
        let _ = self.commands.send(Command::SetVolume {
            node,
            amplitude: volume,
            balance: (1.0, 1.0),
        });
        let _ = self.commands.send(Command::SetMute { node, muted });
    }

    /// Fold a node's outgoing routes down to mono.
    ///
    /// Returns whether the node was known, on the same terms as
    /// [`Self::set_gain`]: an unknown node must be retried, not recorded.
    pub fn set_mono(&self, node_name: &str, end: End, mono: bool) -> bool {
        let Some(node) = self.id_of(node_name) else {
            return false;
        };
        self.commands
            .send(Command::SetMono { node, end, mono })
            .is_ok()
    }

    /// Nudge the backend to complete any route still waiting on ports.
    ///
    /// Routes are normally completed as ports arrive, but a route asked for
    /// in the gap between a node appearing and its ports appearing has no
    /// later event to ride: if every port is already there, nothing more will
    /// arrive to trigger the retry. This is the periodic backstop.
    pub fn retry_routes(&self) {
        let _ = self.commands.send(Command::RetryRoutes);
    }

    /// Start recording a node to a file. `None` stops whatever is running.
    pub fn record(&self, takes: &[(String, std::path::PathBuf)], rate: u32) -> bool {
        let mut resolved = Vec::with_capacity(takes.len());
        for (name, path) in takes {
            // Asked to record something that is not there: say so rather
            // than silently starting the rest and calling it a take.
            let Some(id) = self.id_of(name) else {
                log::warn!("cannot record {name}: it is not in the graph");
                return false;
            };
            resolved.push((id, path.clone()));
        }
        self.commands
            .send(Command::Record {
                takes: resolved,
                rate,
            })
            .is_ok()
    }

    /// Set named controls on a chain node.
    ///
    /// Names are `<filter>:<control>` as the graph declared them. Note that a
    /// chain only applies these while it is running; an idle one accepts the
    /// message and ignores it.
    pub fn set_controls(&self, node_name: &str, controls: &[(String, f32)]) -> bool {
        let Some(node) = self.id_of(node_name) else {
            return false;
        };
        self.commands
            .send(Command::SetControls {
                node,
                controls: controls.to_vec(),
            })
            .is_ok()
    }

    /// Remove our virtual devices and make them again.
    ///
    /// Use this rather than [`Backend::remove_devices`] followed by
    /// [`Backend::create_devices`]. Both of those are requests to the
    /// `PipeWire` thread rather than things that have happened when the
    /// call returns, so sending them in sequence is a race, and it is lost
    /// the obvious way: the devices go and do not come back. This does the
    /// pair on the thread that owns them, where the order holds.
    pub fn recreate_devices(&self) {
        let _ = self.commands.send(Command::RecreateDevices);
    }

    /// Ask the backend to create the virtual strips and buses.
    pub fn create_devices(&self) {
        let _ = self.commands.send(Command::CreateDevices);
    }

    /// Ask the backend to tear them down again.
    pub fn remove_devices(&self) {
        let _ = self.commands.send(Command::RemoveDevices);
    }
}

#[cfg(test)]
mod tests {
    use super::amplitude_of;

    #[test]
    fn unity_is_amplitude_one() {
        assert!((amplitude_of(0.0, false) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn minus_six_db_is_about_half() {
        let half = amplitude_of(-6.0, false);
        assert!((half - 0.501).abs() < 0.01, "was {half}");
    }

    #[test]
    fn boosting_goes_above_one() {
        assert!(amplitude_of(12.0, false) > 3.9);
    }

    #[test]
    fn mute_wins_over_the_fader() {
        assert!(amplitude_of(12.0, true).abs() < f32::EPSILON);
        assert!(amplitude_of(-60.0, true).abs() < f32::EPSILON);
    }
}
