//! Reading a `PipeWire` global and deciding what it is to us.
//!
//! Split out of `nodes` because it is the one part with no side effects:
//! given a registry object it answers with a `Device`, a `Stream`, a
//! `LinkInfo`, a `Port` or nothing at all. Everything else in that file
//! changes the graph.

use pipewire::spa::utils::dict::DictRef;

use super::links::{Port, PortDirection};
use super::nodes::{Device, Direction, Kind, LinkInfo, Stream};

/// Turn a registry global into a [`Device`], or `None` if it is not an audio
/// node we care about. Everything `PipeWire` exposes comes through here —
/// devices, ports, links, factories — so the filtering matters.
/// A node's sample rate.
///
/// Most device nodes do not publish `audio.rate`; the rate they are running
/// at is in `clock.rate`, and ALSA nodes carry it as a fraction in
/// `node.rate` such as `1/48000`. Reading only the first left every row of
/// the settings window showing a dash.
pub(super) fn node_rate(props: &DictRef) -> Option<u32> {
    if let Some(rate) = props.get("audio.rate").and_then(|r| r.parse().ok()) {
        return Some(rate);
    }
    if let Some(rate) = props.get("clock.rate").and_then(|r| r.parse().ok()) {
        return Some(rate);
    }
    props
        .get("node.rate")
        .and_then(|r| r.split_once('/'))
        .and_then(|(_, rate)| rate.parse().ok())
}

/// A node's channel count.
///
/// `audio.channels` when it is there, otherwise counted from the channel
/// map in `audio.position`, which reads like `FL,FR`.
pub(super) fn node_channels(props: &DictRef) -> Option<u32> {
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

/// Whether a node has a card behind it.
///
/// `device.api` is the plain answer and is not always to hand: the
/// registry hands out a subset of a node's properties, and that key is not
/// in it. `device.id` is, and means the same thing - a node backed by a
/// Device object is a real one, while a null sink or a loopback has
/// neither key.
///
/// The bound node's `info` is *not* a better source, despite carrying the
/// full properties in `pw-dump`: `PipeWire` only fills them in when they
/// change, so the first info arrives with an empty dictionary. Reading the
/// kind from there classified every device on the machine as virtual by
/// overwriting the right answer with a guess made from nothing.
pub(super) fn node_kind(props: &DictRef) -> Kind {
    if props.get("device.api").is_some_and(|api| !api.is_empty()) {
        return Kind::Physical;
    }
    if props.get("device.id").is_some() {
        return Kind::Physical;
    }
    Kind::Virtual
}

pub(super) fn describe(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<Device> {
    let props = global.props?;
    let class = props.get("media.class")?;
    let ours = props.get("node.name").is_some_and(super::eq::is_chain_node);
    let direction = match class {
        "Audio/Sink" => Direction::Sink,
        "Audio/Source" => Direction::Source,
        "Stream/Input/Audio" if ours => Direction::Sink,
        "Stream/Output/Audio" if ours => Direction::Source,
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
        kind: node_kind(props),
    })
}

/// Turn a registry global into a [`Stream`], or `None` if it is not an
/// application playing audio.
pub(super) fn describe_stream(
    global: &pipewire::registry::GlobalObject<&DictRef>,
) -> Option<Stream> {
    let props = global.props?;
    if props
        .get("node.name")
        .is_some_and(|n| n.contains("pipemeter"))
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
pub(super) fn describe_link(
    global: &pipewire::registry::GlobalObject<&DictRef>,
) -> Option<LinkInfo> {
    let props = global.props?;
    Some(LinkInfo {
        id: global.id,
        output_node: props.get("link.output.node")?.parse().ok()?,
        input_node: props.get("link.input.node")?.parse().ok()?,
    })
}

/// Turn a registry global into a [`Port`], or `None` if it is not one.
pub(super) fn describe_port(global: &pipewire::registry::GlobalObject<&DictRef>) -> Option<Port> {
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
        slot: props
            .get("port.id")
            .and_then(|i| i.parse().ok())
            .unwrap_or(0),
        channel: props.get("audio.channel").unwrap_or_default().to_owned(),
        direction,
    })
}
