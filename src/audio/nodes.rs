//! The `PipeWire` thread: watches the registry and applies routing commands.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use pipewire::spa::utils::dict::DictRef;

use super::Event;
use super::links::{Port, PortDirection, Router};
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
        takes: Vec<(u32, std::path::PathBuf)>,
        rate: u32,
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

    // A detached thread: the loop ends when the process does. There is no
    // clean shutdown to wait for, and blocking the UI on one would be worse.
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

    // 0.10 splits these into Rc and Box flavours; the Rc ones are what the
    // listener closures need to outlive this function.
    let mainloop = pipewire::main_loop::MainLoopRc::new(None)?;
    let context = pipewire::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    // Proxies for sinks this run created. Kept only so they are not dropped
    // mid-creation; with object.linger set, dropping them no longer removes
    // anything.
    let sinks: Rc<RefCell<Vec<pipewire::node::Node>>> = Rc::new(RefCell::new(Vec::new()));
    let sinks_cmd = Rc::clone(&sinks);

    // Our own sinks as the registry sees them, name to global id. Populated
    // for sinks left behind by a previous run as well as ones we make, so
    // either can be removed.
    let owned: Rc<RefCell<HashMap<String, Owned>>> = Rc::new(RefCell::new(HashMap::new()));

    // Bound proxies for every audio node, so their volume can be set. A
    // registry id alone is not enough: params go through a proxy.
    let controls: Rc<RefCell<HashMap<u32, pipewire::node::Node>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let (controls_add, controls_gone, controls_cmd) = (
        Rc::clone(&controls),
        Rc::clone(&controls),
        Rc::clone(&controls),
    );
    let registry_bind = registry.clone();

    // One capture stream per metered node, held for as long as the node is.
    let attached: Rc<RefCell<HashMap<u32, Meter>>> = Rc::new(RefCell::new(HashMap::new()));
    let (attached_add, attached_gone) = (Rc::clone(&attached), Rc::clone(&attached));
    let levels_reg = Levels::clone(levels);
    let core_meter = core.clone();
    let (owned_reg, owned_cmd, owned_gone) =
        (Rc::clone(&owned), Rc::clone(&owned), Rc::clone(&owned));
    let registry_cmd = registry.clone();

    // Shared between the registry listener (which fills it) and the command
    // handler (which routes with it). The loop is single-threaded, so a
    // RefCell is enough and no lock is needed.
    let router = Rc::new(RefCell::new(Router::default()));

    // One per closure, since each is `move`.
    let (added, added_stream, removed) = (tx.clone(), tx.clone(), tx.clone());
    let (router_add, router_remove) = (Rc::clone(&router), Rc::clone(&router));
    let core_ports = core.clone();
    let router_cmd = Rc::clone(&router);
    let core_cmd = core.clone();

    // Held for the lifetime of the loop; dropping it unsubscribes.
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(device) = describe(global) {
                if super::sinks::is_ours(&device.name) {
                    owned_reg.borrow_mut().insert(
                        device.name.clone(),
                        Owned {
                            id: device.id,
                            class: device.class.clone(),
                        },
                    );
                }
                adopt(
                    global,
                    &device,
                    &registry_bind,
                    &controls_add,
                    &core_meter,
                    &attached_add,
                    &levels_reg,
                );
                let _ = added.send(Event::Added(device));
            } else if let Some(stream) = describe_stream(global) {
                adopt_stream(
                    global,
                    &stream,
                    &Adopt {
                        registry: &registry_bind,
                        controls: &controls_add,
                        core: &core_meter,
                        meters: &attached_add,
                        levels: &levels_reg,
                    },
                );
                let _ = added_stream.send(Event::StreamAdded(stream));
            } else if let Some(link) = describe_link(global) {
                let _ = added_stream.send(Event::LinkAdded(link));
            } else if let Some(port) = describe_port(global) {
                // A node is registered before its ports are, so a route asked
                // for the moment a sink appears has nothing to connect yet.
                // Every arriving port is a chance to complete one.
                let mut router = router_add.borrow_mut();
                router.add_port(port);
                router.retry_pending(&core_ports);
            }
        })
        .global_remove(move |id| {
            // The id may be a node or a port; both need clearing, and only
            // one of the two will actually match anything.
            {
                let mut router = router_remove.borrow_mut();
                router.remove_port(id);
                router.forget_node(id);
            }
            owned_gone.borrow_mut().retain(|_, owned| owned.id != id);
            // Dropping the meter stops its capture stream. Without this the
            // stream would outlive the node it was measuring.
            attached_gone.borrow_mut().remove(&id);
            controls_gone.borrow_mut().remove(&id);
            let _ = removed.send(Event::Removed(id));
        })
        .register();

    // Everything the command handler needs, bundled so `run` does not end
    // up threading six clones through a closure inline.
    let context = CommandContext::new(
        core_cmd,
        registry_cmd,
        router_cmd,
        sinks_cmd,
        owned_cmd,
        controls_cmd,
    );
    // Ask the server to tell us when it has finished replaying the registry.
    // Until that arrives, `owned` is incomplete, and creating sinks against
    // it would duplicate whatever a previous run left lingering.
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
            // Only a broken pipe means the server has gone. Every other core
            // error is something one request did wrong - a link to a port
            // that vanished, most often - and those are routine while the
            // graph is still settling.
            //
            // Treating them all as a disconnect tore down the whole
            // connection twice on every startup, which took the FX chains
            // with it and left them linked to nothing.
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
    core: pipewire::core::CoreRc,
    sinks: Rc<RefCell<Vec<pipewire::node::Node>>>,
    owned: Rc<RefCell<HashMap<String, Owned>>>,
    registry: pipewire::registry::RegistryRc,
    controls: Rc<RefCell<HashMap<u32, pipewire::node::Node>>>,
    /// Control writes for nodes that were not bound when they arrived.
    ///
    /// A filter chain that is not running yet has no proxy here, and the
    /// write used to be dropped in silence. Held instead, and retried on
    /// the next write, which is enough: the caller pushes controls
    /// repeatedly, so a chain that comes up gets its levels on the next
    /// pass rather than never.
    pending: Rc<RefCell<Pending>>,
}

impl CommandContext {
    /// The two fields nobody else shares - the takes in progress and the
    /// held control writes - are made here rather than passed in, which is
    /// also what keeps this to six arguments instead of eight.
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
    // Anything held from an earlier write goes out first, so the
    // order the caller sent them in survives.
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
        // Replaced rather than appended: a later write for the same
        // node is the newer truth, and queueing both would push a
        // stale level out first.
        context.pending.borrow_mut().insert(node, controls);
    }
}

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
            // Anything left behind with the wrong class is from an older
            // build and has to go, or it would be adopted and the bus would
            // stay filed as a speaker.
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

            // Adopt whatever else a previous run left behind rather than
            // making a second set beside it.
            // Named, because `create_missing` is generic over the hasher
            // and cannot infer which one an anonymous collect meant.
            let existing: std::collections::HashSet<String> =
                context.owned.borrow().keys().cloned().collect();
            let mut made = super::sinks::create_missing(&context.core, &existing);
            context.sinks.borrow_mut().append(&mut made);
        }
        Command::RemoveDevices => {
            for owned in context.owned.borrow().values() {
                let _ = context.registry.destroy_global(owned.id).into_result();
            }
            // The registry's global_remove clears the map; dropping our
            // proxies here just releases them.
            context.sinks.borrow_mut().clear();
        }
        Command::RecreateDevices => {
            // Both halves on the thread that owns them, which is what makes
            // this safe where sending the two commands in sequence is not:
            // neither has happened by the time its call returns.
            for owned in context.owned.borrow().values() {
                let _ = context.registry.destroy_global(owned.id).into_result();
            }
            context.sinks.borrow_mut().clear();
            // Cleared here rather than waiting for `global_remove`, which
            // arrives later. `create_missing` skips any name it finds in
            // this map, so leaving the old ones in would create nothing at
            // all and the devices would simply disappear.
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
        Command::Record { takes, rate } => {
            // Dropping the old ones first finalises their files, so starting
            // a new take cannot leave the last one truncated.
            context.recorder.borrow_mut().clear();
            let started: Vec<_> = takes
                .iter()
                .filter_map(|(node, path)| super::recorder::start(&context.core, *node, path, rate))
                .collect();
            *context.recorder.borrow_mut() = started;
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

    // And a meter, so the application's row can show a level.
    //
    // Targeted by node name and *not* as a sink capture: an application is a
    // source of audio, and there is no monitor on it to ask for. Whether the
    // session manager will link a capture stream to another stream's output
    // is not something PipeWire promises, so this is allowed to come to
    // nothing - the row then shows no movement, which is what it did before.
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
#[allow(clippy::too_many_arguments)]
fn adopt(
    global: &pipewire::registry::GlobalObject<&DictRef>,
    device: &Device,
    registry: &pipewire::registry::RegistryRc,
    controls: &Rc<RefCell<HashMap<u32, pipewire::node::Node>>>,
    core: &pipewire::core::CoreRc,
    meters_held: &Rc<RefCell<HashMap<u32, Meter>>>,
    levels: &Levels,
) {
    let id = device.id;
    if let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) {
        controls.borrow_mut().insert(id, node);
    }

    // Our own filter chains are plumbing, not strips: nothing shows their
    // level, and they are streams rather than devices so a capture stream
    // cannot target them anyway. Left in, each one attached to the default
    // device instead and its readings were added to whatever was there -
    // which is how the microphone's meter came to carry six other nodes.
    if super::eq::is_chain_node(&device.name) {
        return;
    }

    // Metering every node we surface costs a stream each, but the
    // alternative is guessing which ones a strip will end up pointed at.
    let mut held = meters_held.borrow_mut();
    let target = meters::Target {
        id,
        name: device.name.clone(),
        // Every device we make is a null sink underneath, whatever class it
        // is declared as, so the mix is on its monitor. Reading a bus's
        // plain source output gives silence - measured - which is why the B
        // meters sat still while the buses carried audio.
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

/// Turn a registry global into a [`Device`], or `None` if it is not an audio
/// node we care about. Everything `PipeWire` exposes comes through here —
/// devices, ports, links, factories — so the filtering matters.
/// A node's sample rate.
///
/// Most device nodes do not publish `audio.rate`; the rate they are running
/// at is in `clock.rate`, and ALSA nodes carry it as a fraction in
/// `node.rate` such as `1/48000`. Reading only the first left every row of
/// the settings window showing a dash.
fn node_rate(props: &DictRef) -> Option<u32> {
    if let Some(rate) = props.get("audio.rate").and_then(|r| r.parse().ok()) {
        return Some(rate);
    }
    if let Some(rate) = props.get("clock.rate").and_then(|r| r.parse().ok()) {
        return Some(rate);
    }
    // `1/48000`, where the denominator is the rate.
    props
        .get("node.rate")
        .and_then(|r| r.split_once('/'))
        .and_then(|(_, rate)| rate.parse().ok())
}

/// A node's channel count.
///
/// `audio.channels` when it is there, otherwise counted from the channel
/// map in `audio.position`, which reads like `FL,FR`.
fn node_channels(props: &DictRef) -> Option<u32> {
    if let Some(channels) = props.get("audio.channels").and_then(|c| c.parse().ok()) {
        return Some(channels);
    }
    let position = props.get("audio.position")?;
    let count = position
        .split(',')
        .filter(|part| !part.trim().is_empty())
        .count();
    u32::try_from(count).ok().filter(|count| *count > 0)
}

fn describe(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<Device> {
    let props = global.props?;
    let class = props.get("media.class")?;
    // A chain's input is a stream that consumes, so it is a sink as far as
    // routing is concerned, and its output is a source.
    let ours = props.get("node.name").is_some_and(super::eq::is_chain_node);
    let direction = match class {
        "Audio/Sink" => Direction::Sink,
        "Audio/Source" => Direction::Source,
        // Our buses are sinks too, so they need no arm of their own. If
        // CLASS_BUS ever becomes a source class again, one goes here: the
        // mixer routes *into* a bus whatever the desktop calls it.
        "Stream/Input/Audio" if ours => Direction::Sink,
        "Stream/Output/Audio" if ours => Direction::Source,
        // Audio/Duplex and the various Stream classes are not devices the
        // mixer assigns to a strip, so they are skipped.
        _ => return None,
    };

    let name = props.get("node.name")?.to_owned();
    let description = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .unwrap_or(&name)
        .to_owned();

    Some(Device {
        id: global.id,
        name,
        description,
        direction,
        class: class.to_owned(),
        rate: node_rate(props),
        channels: node_channels(props),
        assignable: !ours,
    })
}

/// Turn a registry global into a [`Stream`], or `None` if it is not an
/// application playing audio.
fn describe_stream(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<Stream> {
    let props = global.props?;
    // Only playback streams: a capture stream is an application listening,
    // not one feeding a strip.
    if props
        .get("node.name")
        .is_some_and(|n| n.contains("pipemeeter"))
    {
        return None;
    }
    if props.get("media.class")? != "Stream/Output/Audio" {
        return None;
    }
    let name = props
        .get("application.name")
        .or_else(|| props.get("node.description"))
        .or_else(|| props.get("node.name"))?
        .to_owned();
    Some(Stream {
        id: global.id,
        name,
        node_name: props.get("node.name").unwrap_or_default().to_owned(),
    })
}

/// Turn a registry global into a [`LinkInfo`], or `None` if it is not a link.
fn describe_link(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<LinkInfo> {
    let props = global.props?;
    Some(LinkInfo {
        id: global.id,
        output_node: props.get("link.output.node")?.parse().ok()?,
        input_node: props.get("link.input.node")?.parse().ok()?,
    })
}

/// Turn a registry global into a [`Port`], or `None` if it is not one.
fn describe_port(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<Port> {
    let props = global.props?;
    let direction = match props.get("port.direction")? {
        "in" => PortDirection::In,
        "out" => PortDirection::Out,
        _ => return None,
    };
    let node_id = props.get("node.id")?.parse().ok()?;

    Some(Port {
        id: global.id,
        node_id,
        // port.id numbers the ports of one direction on one node, which is
        // exactly the ordering a send needs to pick a pair out of.
        slot: props
            .get("port.id")
            .and_then(|i| i.parse().ok())
            .unwrap_or(0),
        // Ports with no channel (MIDI, control) are filtered out when
        // routing rather than here, so the map stays a faithful mirror.
        channel: props.get("audio.channel").unwrap_or_default().to_owned(),
        direction,
    })
}
