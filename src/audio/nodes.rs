//! The `PipeWire` thread: watches the registry and applies routing commands.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use pipewire::spa::utils::dict::DictRef;

use super::Event;
use super::links::Router;
use super::meters::{self, Meter};

/// Shared peak levels, re-exported so `Backend` can name the type.
pub type Levels = meters::Levels;

/// Which way audio flows through a device, from the mixer's point of view.
///
/// Note this is `PipeWire`'s sense, not Voicemeeter's: a `Sink` is something
/// you play *into*. A Voicemeeter virtual strip is backed by a sink, and a
/// Voicemeeter A-bus is backed by a sink too — the difference is who writes
/// to it, not the node type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A capture device: microphones, monitors.
    Source,
    /// A playback device: speakers, headphones, null sinks.
    Sink,
}

/// Whether a device is a real piece of hardware or something made up in
/// software.
///
/// Decided by `device.api`, which `PipeWire` sets on nodes that have a
/// card behind them - `alsa`, `bluez5` - and leaves off null sinks,
/// loopbacks and the like. Not by the node name: a Bluetooth headset is
/// `bluez_output.*` and is as physical as anything, so a test for an
/// `alsa_` prefix files real hardware under Virtual.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// A card, a USB interface, a Bluetooth headset.
    Physical,
    /// A null sink, a loopback, another mixer's cable.
    ///
    /// The default because it is the safe guess: it is what we assume of a
    /// device we have never had a live look at.
    #[default]
    Virtual,
}

impl Kind {
    /// What `device.api` says about a node, if anything.
    #[must_use]
    pub fn of(api: Option<&str>) -> Self {
        match api {
            Some(api) if !api.is_empty() => Self::Physical,
            _ => Self::Virtual,
        }
    }

    /// How it is written in the settings file, and read back.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Physical => "physical",
            Self::Virtual => "virtual",
        }
    }

    /// The other half of [`Kind::as_str`].
    #[must_use]
    pub fn parse(text: &str) -> Self {
        if text == "physical" {
            Self::Physical
        } else {
            Self::Virtual
        }
    }
}

/// An application playing audio, as the registry reports it.
#[derive(Debug, Clone)]
pub struct Stream {
    pub id: u32,
    /// Human name: the application, falling back to the node's own.
    pub name: String,
    /// The node's own name, which is what a meter has to be told to target.
    /// Not the human one: two copies of a program share a display name.
    pub node_name: String,
}

/// A link between two nodes, which is how a stream is matched to the strip
/// it is playing into.
#[derive(Debug, Clone, Copy)]
pub struct LinkInfo {
    pub id: u32,
    pub output_node: u32,
    pub input_node: u32,
}

/// One audio node as the mixer cares about it.
#[derive(Debug, Clone)]
pub struct Device {
    /// `PipeWire` global id. Not stable across reconnects.
    pub id: u32,
    /// Machine name, e.g. `alsa_output.usb-...analog-stereo`.
    pub name: String,
    /// Human name for display.
    pub description: String,
    pub direction: Direction,
    /// The `media.class` the node was created with.
    pub class: String,
    /// Sample rate and channel count, when the node advertises them. Shown
    /// in the system settings, where the original lists the same per device.
    pub rate: Option<u32>,
    pub channels: Option<u32>,
    /// Whether this is a real device a strip or bus can be assigned to.
    ///
    /// Our own EQ chains are ordinary streams, not devices, but they still
    /// have to be findable by name so the matrix can route through them.
    /// They are carried in the same list and hidden from the pickers rather
    /// than tracked separately, since everything else about them - id
    /// lookup, removal, port bookkeeping - is identical.
    pub assignable: bool,
    /// Hardware, or made up in software. See [`Kind`].
    pub kind: Kind,
}

/// A request from the UI thread to the `PipeWire` thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// Route, or unroute, one node into another.
    SetRoute {
        route: super::links::Route,
        enabled: bool,
    },
    /// Create the virtual strips and buses if they do not exist.
    CreateDevices,
    /// Tear them down. Because they linger, dropping our proxies is not
    /// enough — each global has to be destroyed explicitly.
    RemoveDevices,
    /// Remove ours and make them again, without the caller being able to
    /// get the order wrong.
    RecreateDevices,
    /// Set a node's output level, as a linear amplitude. Mute is amplitude
    /// zero rather than a separate flag, so one path covers both.
    SetVolume {
        node: u32,
        amplitude: f32,
        /// Per-channel multipliers on top of `amplitude`, left then right.
        /// Unity on both is no panning.
        balance: (f32, f32),
    },
    /// Mute or unmute a node.
    ///
    /// Its own property rather than a volume of zero: an application muted
    /// from the mixer must come back at the level it had, and Voicemeeter's
    /// mute is likewise independent of the fader.
    SetMute { node: u32, muted: bool },
    /// Start recording a node to a file, or stop the recording.
    ///
    /// Only one at a time: the deck has one transport.
    Record {
        /// One entry per take, each its own node and its own file. Empty
        /// stops whatever is running, which is how the deck's Stop is sent.
        /// Each take's node, by name rather than by id. `TARGET_OBJECT`
        /// resolves a name; handed a registry id it matches nothing and the
        /// stream records a header and no audio at all.
        takes: Vec<(String, std::path::PathBuf)>,
        rate: u32,
        /// How samples are stored on disk.
        depth: super::wav::Depth,
        /// What they are wrapped in.
        container: super::wav::Container,
    },
    /// Start or stop playing an audio file into one or more target buses.
    Play {
        targets: Vec<String>,
        path: Option<std::path::PathBuf>,
        gain_db: f32,
    },
    /// Update playback gain live for the active player.
    SetPlaybackGain {
        gain_db: f32,
    },
    /// Set named controls on a filter-chain node.
    SetControls {
        node: u32,
        controls: Vec<(String, f32)>,
    },
    /// Try again to complete any route whose ports were not there yet.
    RetryRoutes,
    /// Fold a node's routes down to mono, or stop. `end` says whether the
    /// node is the source of those routes or their target.
    SetMono {
        node: u32,
        end: super::links::End,
        mono: bool,
    },
}

/// Start the `PipeWire` thread. Returns the event stream and the command sink.
pub fn spawn() -> (Receiver<Event>, pipewire::channel::Sender<Command>, Levels) {
    let (tx, rx) = channel();
    let (cmd_tx, cmd_rx) = pipewire::channel::channel();
    let levels: Levels = std::sync::Arc::default();
    let levels_thread = std::sync::Arc::clone(&levels);

    thread::Builder::new()
        .name("pipewire".to_owned())
        .spawn(move || {
            if let Err(err) = run(&tx, cmd_rx, &levels_thread) {
                let _ = tx.send(Event::Disconnected(err.to_string()));
            }
        })
        .expect("spawning the PipeWire thread");

    (rx, cmd_tx, levels)
}

/// Connect and run the main loop until it stops.
fn run(
    tx: &Sender<Event>,
    commands: pipewire::channel::Receiver<Command>,
    levels: &Levels,
) -> Result<(), pipewire::Error> {
    pipewire::init();

    let mainloop = pipewire::main_loop::MainLoopRc::new(None)?;
    let context = pipewire::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let sinks: Rc<RefCell<Vec<pipewire::node::Node>>> = Rc::new(RefCell::new(Vec::new()));
    let sinks_cmd = Rc::clone(&sinks);

    let owned: Rc<RefCell<HashMap<String, Owned>>> = Rc::new(RefCell::new(HashMap::new()));

    let controls: Rc<RefCell<HashMap<u32, pipewire::node::Node>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let (controls_add, controls_gone, controls_cmd) = (
        Rc::clone(&controls),
        Rc::clone(&controls),
        Rc::clone(&controls),
    );
    let registry_bind = registry.clone();

    let attached: Rc<RefCell<HashMap<u32, Meter>>> = Rc::new(RefCell::new(HashMap::new()));
    let (attached_add, attached_gone) = (Rc::clone(&attached), Rc::clone(&attached));

    let formats: Formats = Rc::new(RefCell::new(HashMap::new()));
    let (formats_add, formats_gone) = (Rc::clone(&formats), formats);
    let levels_reg = Levels::clone(levels);
    let core_meter = core.clone();
    let (owned_reg, owned_cmd, owned_gone) =
        (Rc::clone(&owned), Rc::clone(&owned), Rc::clone(&owned));
    let registry_cmd = registry.clone();

    let router = Rc::new(RefCell::new(Router::default()));

    let (added, added_stream, removed) = (tx.clone(), tx.clone(), tx.clone());
    let (router_add, router_remove) = (Rc::clone(&router), Rc::clone(&router));
    let core_ports = core.clone();
    let router_cmd = Rc::clone(&router);
    let core_cmd = core.clone();

    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            let handles = Adopt {
                registry: &registry_bind,
                controls: &controls_add,
                core: &core_meter,
                meters: &attached_add,
                levels: &levels_reg,
                formats: &formats_add,
                tx: &added,
            };
            if let Some(device) = super::describe::describe(global) {
                if super::sinks::is_ours(&device.name) {
                    owned_reg.borrow_mut().insert(
                        device.name.clone(),
                        Owned {
                            id: device.id,
                            class: device.class.clone(),
                        },
                    );
                }
                adopt(global, &device, &handles);
                let _ = added.send(Event::Added(device));
            } else if let Some(stream) = super::describe::describe_stream(global) {
                adopt_stream(global, &stream, &handles);
                let _ = added_stream.send(Event::StreamAdded(stream));
            } else if let Some(link) = super::describe::describe_link(global) {
                let _ = added_stream.send(Event::LinkAdded(link));
            } else if let Some(port) = super::describe::describe_port(global) {
                let mut router = router_add.borrow_mut();
                router.add_port(port);
                router.retry_pending(&core_ports);
            }
        })
        .global_remove(move |id| {
            {
                let mut router = router_remove.borrow_mut();
                router.remove_port(id);
                router.forget_node(id);
            }
            owned_gone.borrow_mut().retain(|_, owned| owned.id != id);
            attached_gone.borrow_mut().remove(&id);
            controls_gone.borrow_mut().remove(&id);
            formats_gone.borrow_mut().remove(&id);
            let _ = removed.send(Event::Removed(id));
        })
        .register();

    let context = CommandContext::new(
        core_cmd,
        registry_cmd,
        router_cmd,
        sinks_cmd,
        owned_cmd,
        controls_cmd,
    );
    let _core_listener = watch_core(&core, &mainloop, tx)?;

    let _commands = commands.attach(mainloop.loop_(), move |command| {
        handle(&context, command);
    });

    mainloop.run();
    Ok(())
}

/// `EPIPE`, which is what the server reports when it has gone away.
///
/// Spelled out rather than pulled from `libc`: one constant is not worth a
/// dependency, and it has had the same value on Linux for decades.
const fn libc_epipe() -> i32 {
    32
}

/// Listen for the two things the core itself tells us: that it has finished
/// replaying the registry, and that it has gone away.
///
/// Quitting the loop on a core error is what makes a dead server visible.
/// Without it the daemon can go away - a package update restarting it is
/// enough - and the thread sits in a loop that never fires again, with the
/// mixer still on screen and connected to nothing.
fn watch_core(
    core: &pipewire::core::CoreRc,
    mainloop: &pipewire::main_loop::MainLoopRc,
    tx: &Sender<Event>,
) -> Result<pipewire::core::Listener, pipewire::Error> {
    let swept = tx.clone();
    let lost = tx.clone();
    let done = std::cell::Cell::new(false);
    let sync_seq = core.sync(0)?;
    let quit = mainloop.clone();

    Ok(core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pipewire::core::PW_ID_CORE && seq == sync_seq && !done.replace(true) {
                let _ = swept.send(Event::Enumerated);
            }
        })
        .error(move |id, _seq, res, message| {
            let fatal = res == -libc_epipe();
            if id == pipewire::core::PW_ID_CORE && fatal {
                let _ = lost.send(Event::Disconnected(format!("{message} ({res})")));
                quit.quit();
            } else {
                log::debug!("core error {res} on {id}: {message}");
            }
        })
        .register())
}

/// The handles the command loop works through.
/// One of our own virtual devices, as the registry sees it.
#[derive(Debug, Clone)]
struct Owned {
    id: u32,
    /// What it was created as. A build that declared the buses differently
    /// leaves them behind, and adopting one of those would give a bus the
    /// desktop still files with the speakers.
    class: String,
}

struct CommandContext {
    router: Rc<RefCell<Router>>,
    /// The recording in progress, if any. Held here because it must outlive
    /// the command that started it and die with the loop.
    /// Every take in progress. More than one when the pre-fader inputs are
    /// armed: the recorder page arms a set of them, not just the one bus.
    recorder: Rc<RefCell<Vec<super::recorder::Recorder>>>,
    /// The playback in progress, if any.
    player: Rc<RefCell<Option<super::player::Player>>>,
    core: pipewire::core::CoreRc,
    sinks: Rc<RefCell<Vec<pipewire::node::Node>>>,
    owned: Rc<RefCell<HashMap<String, Owned>>>,
    registry: pipewire::registry::RegistryRc,
    controls: Rc<RefCell<HashMap<u32, pipewire::node::Node>>>,
    pending: Rc<RefCell<Pending>>,
}

impl CommandContext {
    fn new(
        core: pipewire::core::CoreRc,
        registry: pipewire::registry::RegistryRc,
        router: Rc<RefCell<Router>>,
        sinks: Rc<RefCell<Vec<pipewire::node::Node>>>,
        owned: Rc<RefCell<HashMap<String, Owned>>>,
        controls: Rc<RefCell<HashMap<u32, pipewire::node::Node>>>,
    ) -> Self {
        Self {
            router,
            recorder: Rc::new(RefCell::new(Vec::new())),
            player: Rc::new(RefCell::new(None)),
            core,
            sinks,
            owned,
            registry,
            controls,
            pending: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// Send control values to a node, holding them if it is not bound yet.
///
/// A filter chain that has not started has no proxy, and a write to it used
/// to vanish. Holding and retrying on the next write is enough in practice:
/// callers push controls repeatedly, so a chain that comes up late gets its
/// levels on the following pass rather than never.
fn write_controls(context: &CommandContext, node: u32, controls: Vec<(String, f32)>) {
    let held: Vec<(u32, Vec<(String, f32)>)> = {
        let mut pending = context.pending.borrow_mut();
        let bound = context.controls.borrow();
        let ready: Vec<u32> = pending
            .keys()
            .copied()
            .filter(|id| bound.contains_key(id))
            .collect();
        ready
            .into_iter()
            .filter_map(|id| pending.remove(&id).map(|values| (id, values)))
            .collect()
    };
    for (id, values) in held {
        if let Some(proxy) = context.controls.borrow().get(&id) {
            log::debug!("applying {} held control(s) to node {id}", values.len());
            set_controls(proxy, &values);
        }
    }

    let bound = context.controls.borrow().get(&node).is_some();
    if bound {
        if let Some(proxy) = context.controls.borrow().get(&node) {
            set_controls(proxy, &controls);
        }
    } else {
        context.pending.borrow_mut().insert(node, controls);
    }
}

/// Info listeners for bound nodes, by node id. Held so the node keeps
/// reporting its format; dropping one unsubscribes.
type Formats = Rc<RefCell<HashMap<u32, pipewire::node::NodeListener>>>;

/// Control writes waiting for their node to be bound, by node id.
type Pending = HashMap<u32, Vec<(String, f32)>>;

/// Apply one command from the UI.
fn handle(context: &CommandContext, command: Command) {
    match command {
        Command::SetRoute { route, enabled } => {
            context
                .router
                .borrow_mut()
                .set_route(&context.core, route, enabled);
        }
        Command::CreateDevices => {
            let stale: Vec<(String, u32)> = context
                .owned
                .borrow()
                .iter()
                .filter(|(name, owned)| super::sinks::wrong_class(name, &owned.class))
                .map(|(name, owned)| (name.clone(), owned.id))
                .collect();
            for (name, id) in stale {
                log::info!("replacing {name}: it was created by an older build");
                let _ = context.registry.destroy_global(id).into_result();
                context.owned.borrow_mut().remove(&name);
            }

            let existing: std::collections::HashSet<String> =
                context.owned.borrow().keys().cloned().collect();
            let mut made = super::sinks::create_missing(&context.core, &existing);
            context.sinks.borrow_mut().append(&mut made);
        }
        Command::RemoveDevices => {
            for owned in context.owned.borrow().values() {
                let _ = context.registry.destroy_global(owned.id).into_result();
            }
            context.sinks.borrow_mut().clear();
        }
        Command::RecreateDevices => {
            for owned in context.owned.borrow().values() {
                let _ = context.registry.destroy_global(owned.id).into_result();
            }
            context.sinks.borrow_mut().clear();
            context.owned.borrow_mut().clear();
            let mut made =
                super::sinks::create_missing(&context.core, &std::collections::HashSet::new());
            context.sinks.borrow_mut().append(&mut made);
        }
        Command::SetMono { node, end, mono } => {
            context
                .router
                .borrow_mut()
                .set_mono(&context.core, node, end, mono);
        }
        Command::RetryRoutes => {
            context.router.borrow_mut().retry_pending(&context.core);
        }
        Command::Record {
            takes,
            rate,
            depth,
            container,
        } => {
            context.recorder.borrow_mut().clear();
            let started: Vec<_> = takes
                .iter()
                .filter_map(|(node, path)| {
                    super::recorder::start(&context.core, node, path, rate, depth, container)
                })
                .collect();
            *context.recorder.borrow_mut() = started;
        }
        Command::Play {
            targets,
            path,
            gain_db,
        } => {
            let _ = context.player.borrow_mut().take();
            if !targets.is_empty()
                && let Some(path) = path
                && let Some(player) =
                    super::player::start(&context.core, &targets, &path, gain_db)
            {
                player.play();
                *context.player.borrow_mut() = Some(player);
            }
        }
        Command::SetPlaybackGain { gain_db } => {
            if let Some(player) = context.player.borrow().as_ref() {
                player.set_gain(gain_db);
            }
        }
        Command::SetControls { node, controls } => write_controls(context, node, controls),
        Command::SetVolume {
            node,
            amplitude,
            balance,
        } => match context.controls.borrow().get(&node) {
            Some(proxy) => {
                log::debug!("set volume {amplitude} balance {balance:?} on node {node}");
                set_volume(proxy, amplitude, balance);
            }
            None => log::debug!("no proxy bound for node {node}"),
        },
        Command::SetMute { node, muted } => match context.controls.borrow().get(&node) {
            Some(proxy) => {
                log::debug!("set mute {muted} on node {node}");
                set_mute(proxy, muted);
            }
            None => log::debug!("no proxy bound for node {node}"),
        },
    }
}

/// Bind a proxy to an application's stream, as [`adopt`] does for a device.
///
/// Without it the per-application slider and its M button have nothing to
/// send Props to: the row moved on screen and nothing happened to the audio.
/// A failure is worth saying out loud for the same reason - there is no way
/// to tell from the outside that a row has gone inert.
/// The handles adopting a node needs, gathered so the callers do not pass
/// five of them positionally.
struct Adopt<'a> {
    registry: &'a pipewire::registry::RegistryRc,
    controls: &'a Rc<RefCell<HashMap<u32, pipewire::node::Node>>>,
    core: &'a pipewire::core::CoreRc,
    meters: &'a Rc<RefCell<HashMap<u32, Meter>>>,
    levels: &'a Levels,
    formats: &'a Formats,
    tx: &'a Sender<Event>,
}

fn adopt_stream(
    global: &pipewire::registry::GlobalObject<&DictRef>,
    stream: &Stream,
    into: &Adopt<'_>,
) {
    let Adopt {
        registry,
        controls,
        core,
        meters: meters_held,
        levels,
        ..
    } = into;
    match registry.bind::<pipewire::node::Node, _>(global) {
        Ok(node) => {
            log::debug!("bound stream {} ({})", stream.id, stream.name);
            controls.borrow_mut().insert(stream.id, node);
        }
        Err(err) => log::warn!(
            "could not bind stream {} ({}): {err} - its row will not control it",
            stream.id,
            stream.name
        ),
    }

    if stream.node_name.is_empty() {
        return;
    }
    let target = super::meters::Target {
        id: stream.id,
        name: stream.node_name.clone(),
        is_sink: false,
    };
    if let Some(meter) = super::meters::attach(core, &target, levels) {
        meters_held.borrow_mut().insert(stream.id, meter);
    }
}

/// Take control of a newly seen audio node: bind a proxy so its volume can
/// be driven, and attach a meter so its level can be read.
///
/// Both are per-node resources held until the node goes away.
fn adopt(global: &pipewire::registry::GlobalObject<&DictRef>, device: &Device, into: &Adopt<'_>) {
    let Adopt {
        registry,
        controls,
        core,
        meters: meters_held,
        levels,
        formats,
        tx,
    } = into;
    let id = device.id;
    if let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) {
        let report = Sender::clone(tx);
        // The rate is not in any property. It lives in the node's Format
        // parameter, which has to be asked for - which is why the picker
        // could say how many channels a device carried and not what rate
        // it ran at.
        let by_param = Sender::clone(tx);
        let listener = node
            .add_listener_local()
            .param(move |_seq, kind, _index, _next, param| {
                if kind != pipewire::spa::param::ParamType::Format {
                    return;
                }
                let Some(pod) = param else { return };
                let mut format = pipewire::spa::param::audio::AudioInfoRaw::new();
                if format.parse(pod).is_err() {
                    return;
                }
                let _ = by_param.send(Event::Format {
                    id,
                    rate: Some(format.rate()).filter(|rate| *rate > 0),
                    channels: Some(format.channels()).filter(|count| *count > 0),
                });
            })
            .info(move |info| {
                let props = info.props();
                let _ = report.send(Event::Format {
                    id,
                    rate: props.and_then(super::describe::node_rate),
                    channels: props.and_then(super::describe::node_channels),
                });
            })
            .register();
        node.subscribe_params(&[pipewire::spa::param::ParamType::Format]);
        controls.borrow_mut().insert(id, node);
        formats.borrow_mut().insert(id, listener);
    }

    if super::eq::is_chain_node(&device.name) {
        return;
    }

    let mut held = meters_held.borrow_mut();
    let target = meters::Target {
        id,
        name: device.name.clone(),
        is_sink: device.class == "Audio/Sink",
    };
    if let std::collections::hash_map::Entry::Vacant(slot) = held.entry(id)
        && let Some(meter) = meters::attach(core, &target, levels)
    {
        slot.insert(meter);
    }
}

/// Push named controls onto a filter-chain node.
///
/// Filter-chain exposes its controls through a single `params` property: a
/// flat struct of alternating name and value, where the name is
/// `<filter>:<control>` as the graph declared it.
fn set_controls(node: &pipewire::node::Node, controls: &[(String, f32)]) {
    use pipewire::spa::pod::{Property, PropertyFlags, Value};

    let mut fields = Vec::with_capacity(controls.len() * 2);
    for (name, value) in controls {
        fields.push(Value::String(name.clone()));
        fields.push(Value::Float(*value));
    }

    let object = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: pipewire::spa::param::ParamType::Props.as_raw(),
        properties: vec![Property {
            key: pipewire::spa::sys::SPA_PROP_params,
            flags: PropertyFlags::empty(),
            value: Value::Struct(fields),
        }],
    };
    send_props(node, &object);
}

/// Push a linear amplitude onto a node as its channel volumes.
///
/// Stereo is assumed: every node the mixer surfaces is two-channel, and
/// sending two values to a mono node is harmless.
fn set_volume(node: &pipewire::node::Node, amplitude: f32, balance: (f32, f32)) {
    use pipewire::spa::pod::{Property, PropertyFlags, Value, ValueArray};

    let object = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: pipewire::spa::param::ParamType::Props.as_raw(),
        properties: vec![Property {
            key: pipewire::spa::sys::SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vec![
                amplitude * balance.0,
                amplitude * balance.1,
            ])),
        }],
    };

    send_props(node, &object);
}

/// Mute a node through its own property, leaving its volume alone.
fn set_mute(node: &pipewire::node::Node, muted: bool) {
    use pipewire::spa::pod::{Property, PropertyFlags, Value};

    let object = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: pipewire::spa::param::ParamType::Props.as_raw(),
        properties: vec![Property {
            key: pipewire::spa::sys::SPA_PROP_mute,
            flags: PropertyFlags::empty(),
            value: Value::Bool(muted),
        }],
    };

    send_props(node, &object);
}

/// Serialise a Props object and send it to a node.
fn send_props(node: &pipewire::node::Node, object: &pipewire::spa::pod::Object) {
    let value = pipewire::spa::pod::Value::Object(object.clone());
    let Ok((cursor, _)) = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &value,
    ) else {
        return;
    };
    let bytes = cursor.into_inner();
    if let Some(pod) = pipewire::spa::pod::Pod::from_bytes(&bytes) {
        node.set_param(pipewire::spa::param::ParamType::Props, 0, pod);
    }
}

#[cfg(test)]
mod kind_tests {
    use super::Kind;

    /// `device.api` is what `PipeWire` puts on a node with a card behind
    /// it, and leaves off everything else.
    #[test]
    fn a_card_makes_it_physical() {
        assert_eq!(Kind::of(Some("alsa")), Kind::Physical);
        assert_eq!(Kind::of(Some("bluez5")), Kind::Physical);
    }

    #[test]
    fn a_null_sink_has_no_api_and_is_virtual() {
        assert_eq!(Kind::of(None), Kind::Virtual);
        assert_eq!(Kind::of(Some("")), Kind::Virtual);
    }

    /// The bug this replaces: a Bluetooth headset is not called `alsa_`
    /// and is not virtual.
    #[test]
    fn a_bluetooth_headset_is_not_virtual() {
        assert_eq!(Kind::of(Some("bluez5")), Kind::Physical);
    }

    #[test]
    fn the_spelling_round_trips() {
        for kind in [Kind::Physical, Kind::Virtual] {
            assert_eq!(Kind::parse(kind.as_str()), kind);
        }
    }

    /// Anything unrecognised reads as virtual, which is the safe guess.
    #[test]
    fn an_unknown_spelling_is_virtual() {
        assert_eq!(Kind::parse("wat"), Kind::Virtual);
    }
}
