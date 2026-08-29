//! Routing: turning a lit A1-A5 / B1-B3 button into real `PipeWire` links.
//!
//! A route between two nodes is not one link but one link *per channel*, so
//! a stereo route is two links that must be created and destroyed together.
//! This module keeps the bookkeeping for that.
//!
//! Direction is from the graph's point of view, not the mixer's: we always
//! join the source node's **output** ports to the target node's **input**
//! ports. For a virtual strip that means its monitor ports, which is exactly
//! what should be heard downstream.

use std::collections::{HashMap, HashSet};

use pipewire::core::CoreRc;
use pipewire::link::Link;

/// Which way a port faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
    In,
    Out,
}

/// One port of one node.
#[derive(Debug, Clone)]
pub struct Port {
    pub id: u32,
    pub node_id: u32,
    pub direction: PortDirection,
    /// `FL`, `FR`, `MONO`… Ports without a channel are ignored for routing.
    pub channel: String,
    /// Position within the node's ports of that direction, as `PipeWire`
    /// numbers them.
    ///
    /// Needed because a chain that carries sends has several output pairs
    /// and the channel names repeat across them: the first `FL` is the main
    /// output, the third is the reverb send. Only the order tells them
    /// apart.
    pub slot: u32,
}

/// Every port currently known, and every link we created.
#[derive(Default)]
pub struct Router {
    ports: HashMap<u32, Port>,
    /// Links we own, keyed by the route that asked for them. Not every link
    /// in the graph — only ours, so we never tear down someone else's.
    routes: HashMap<Route, Vec<Link>>,
    /// Routes the UI has asked for, whether or not they exist yet.
    ///
    /// A node is registered before its ports are, so a route requested the
    /// instant a sink appears has nothing to connect. Remembering the intent
    /// separately lets [`Self::retry_pending`] complete it once the ports
    /// arrive; the UI sends each button once and cannot retry for us.
    desired: HashSet<Route>,
    /// Nodes to fold down to mono, by which end of a route they sit on.
    ///
    /// Done in the graph rather than with a filter: joining every output to
    /// every input sums both channels into both sides, which is what a mono
    /// button does. It costs four links instead of two and no processing.
    ///
    /// Both ends are needed because the two halves of the mixer are not
    /// symmetric. A strip is the source of its routes, so folding it means
    /// folding what leaves it. A physical bus *is* the hardware output node
    /// and has no outgoing routes at all — everything arrives at it — so
    /// folding it means folding everything that feeds it.
    mono: [HashSet<u32>; 2],
}

/// Which end of a route a node is being folded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Source,
    Target,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("ports", &self.ports.len())
            .field("routes", &self.routes.len())
            .field("desired", &self.desired.len())
            .field("mono", &(self.mono[0].len() + self.mono[1].len()))
            .finish()
    }
}

/// One route: two nodes, and which pair of ports at each end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Route {
    pub source: u32,
    pub target: u32,
    pub tap: Tap,
}

impl Route {
    /// The ordinary case: main output to main input.
    #[must_use]
    pub fn new(source: u32, target: u32) -> Self {
        Self {
            source,
            target,
            tap: Tap::default(),
        }
    }

    /// The same route, using a named pair at one or both ends.
    ///
    /// What the effect chains need: every strip sends into a pair of its
    /// own and every bus takes a pair of its own back, so the pair is the
    /// thing that tells one strip's send from another's.
    #[must_use]
    pub fn with_tap(mut self, tap: Tap) -> Self {
        self.tap = tap;
        self
    }
}

/// Which pair of ports a route uses at each end.
///
/// Zero on both sides is every ordinary route. The others exist because a
/// chain carrying sends has several output pairs and the channel names
/// repeat across them: the first `FL` is the main output, the third is the
/// reverb send, and only the order tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Tap {
    pub source_pair: u32,
    pub target_pair: u32,
}

impl Router {
    pub fn add_port(&mut self, port: Port) {
        self.ports.insert(port.id, port);
    }

    /// Bring every wanted route up to date with the ports known right now.
    ///
    /// Called as each port appears. A node's ports arrive one at a time, so a
    /// route built on the first of them would be half a stereo pair; whenever
    /// more channels could now be joined than the route currently has, it is
    /// rebuilt rather than left partial.
    pub fn retry_pending(&mut self, core: &CoreRc) {
        let stale: Vec<Route> = self
            .desired
            .iter()
            .filter(|route| {
                let have = self.routes.get(route).map_or(0, Vec::len);
                self.pair_count(**route) > have
            })
            .copied()
            .collect();

        for key in stale {
            self.routes.remove(&key);
            let links = self.connect(core, key);
            if !links.is_empty() {
                self.routes.insert(key, links);
            }
        }
    }

    /// How many channel pairings are possible for a route right now.
    fn pair_count(&self, route: Route) -> usize {
        let outs = self.pair_of(route.source, PortDirection::Out, route.tap.source_pair);
        let ins = self.pair_of(route.target, PortDirection::In, route.tap.target_pair);
        if route.tap != Tap::default() {
            return outs.len().min(ins.len());
        }
        outs.iter()
            .map(|out| {
                ins.iter()
                    .filter(|i| self.pairs(route.source, route.target, out, i))
                    .count()
            })
            .sum()
    }

    /// Join one port to one other.
    fn link(core: &CoreRc, out: &Port, input: &Port) -> Option<Link> {
        match core.create_object::<Link>(
            "link-factory",
            &pipewire::properties::properties! {
                "link.output.node" => out.node_id.to_string(),
                "link.output.port" => out.id.to_string(),
                "link.input.node" => input.node_id.to_string(),
                "link.input.port" => input.id.to_string(),
                "object.linger" => "0"
            },
        ) {
            Ok(link) => Some(link),
            Err(err) => {
                log::warn!("link {} -> {} failed: {err}", out.id, input.id);
                None
            }
        }
    }

    /// One stereo pair of a node's ports, counting pairs from zero.
    ///
    /// A chain carrying sends has several pairs and the channel names repeat
    /// across them, so the pair is picked by position and only then matched
    /// by channel within it.
    fn pair_of(&self, node: u32, direction: PortDirection, pair: u32) -> Vec<&Port> {
        let ports = self.ports_of(node, direction);
        if pair == 0 && ports.len() <= 2 {
            return ports;
        }
        let (first, last) = (pair * 2, pair * 2 + 1);
        ports
            .into_iter()
            .filter(|p| p.slot == first || p.slot == last)
            .collect()
    }

    /// Whether one output port should feed one input port.
    ///
    /// Normally same channel to same channel, with a mono source feeding
    /// every input. If either end of the route is folded, every output feeds
    /// every input, which is the fold itself.
    fn pairs(&self, source: u32, target: u32, out: &Port, input: &Port) -> bool {
        self.folded(source, target) || input.channel == out.channel || out.channel == "MONO"
    }

    fn folded(&self, source: u32, target: u32) -> bool {
        self.mono[End::Source as usize].contains(&source)
            || self.mono[End::Target as usize].contains(&target)
    }

    /// Fold a node down to mono, or stop. Rebuilds every route touching it at
    /// the given end, since the fold is in how those links are wired.
    pub fn set_mono(&mut self, core: &CoreRc, node: u32, end: End, mono: bool) {
        let set = &mut self.mono[end as usize];
        let changed = if mono {
            set.insert(node)
        } else {
            set.remove(&node)
        };
        if !changed {
            return;
        }
        let affected: Vec<Route> = self
            .desired
            .iter()
            .filter(|route| match end {
                End::Source => route.source == node,
                End::Target => route.target == node,
            })
            .copied()
            .collect();
        for key in affected {
            self.routes.remove(&key);
            let links = self.connect(core, key);
            if !links.is_empty() {
                self.routes.insert(key, links);
            }
        }
    }

    pub fn remove_port(&mut self, id: u32) {
        self.ports.remove(&id);
    }

    /// Drop any route touching a node that has gone away. Without this the
    /// stale `Link` proxies linger until the route is toggled again.
    pub fn forget_node(&mut self, node_id: u32) {
        let before = self.routes.len();
        self.desired
            .retain(|route| route.source != node_id && route.target != node_id);
        self.routes
            .retain(|route, _| route.source != node_id && route.target != node_id);
        if self.routes.len() != before {
            log::debug!(
                "node {node_id} went away, dropping {} route(s)",
                before - self.routes.len()
            );
        }
    }

    /// Turn a route on or off. Idempotent: asking for a route that already
    /// exists does nothing rather than stacking duplicate links.
    pub fn set_route(&mut self, core: &CoreRc, route: Route, enabled: bool) {
        let key = route;
        if !enabled {
            self.desired.remove(&key);
            self.routes.remove(&key);
            return;
        }
        self.desired.insert(key);
        if self.routes.contains_key(&key) {
            return;
        }

        let links = self.connect(core, route);
        if links.is_empty() {
            log::debug!(
                "route {} -> {} deferred until its ports appear",
                route.source,
                route.target
            );
        } else {
            log::debug!(
                "route {} -> {} made {} link(s)",
                route.source,
                route.target,
                links.len()
            );
            self.routes.insert(key, links);
        }
    }

    /// Pair up the source's outputs with the target's inputs by channel.
    fn connect(&self, core: &CoreRc, route: Route) -> Vec<Link> {
        let outs = self.pair_of(route.source, PortDirection::Out, route.tap.source_pair);
        let ins = self.pair_of(route.target, PortDirection::In, route.tap.target_pair);

        if route.tap != Tap::default() {
            return outs
                .iter()
                .zip(&ins)
                .filter_map(|(out, input)| Self::link(core, out, input))
                .collect();
        }

        let mut links = Vec::new();
        for out in &outs {
            let matching = ins
                .iter()
                .filter(|i| self.pairs(route.source, route.target, out, i));
            for input in matching {
                match core.create_object::<Link>(
                    "link-factory",
                    &pipewire::properties::properties! {
                        "link.output.node" => out.node_id.to_string(),
                        "link.output.port" => out.id.to_string(),
                        "link.input.node" => input.node_id.to_string(),
                        "link.input.port" => input.id.to_string(),
                        "object.linger" => "0"
                    },
                ) {
                    Ok(link) => links.push(link),
                    Err(err) => log::warn!("link {} -> {} failed: {err}", out.id, input.id),
                }
            }
        }
        links
    }

    fn ports_of(&self, node_id: u32, direction: PortDirection) -> Vec<&Port> {
        let mut ports: Vec<&Port> = self
            .ports
            .values()
            .filter(|p| p.node_id == node_id && p.direction == direction && !p.channel.is_empty())
            .collect();
        ports.sort_by(|a, b| a.channel.cmp(&b.channel));
        ports
    }
}

#[cfg(test)]
mod tests {
    use super::{Port, PortDirection, Router};

    fn port(id: u32, node_id: u32, direction: PortDirection, channel: &str) -> Port {
        at(id, node_id, direction, channel, 0)
    }

    fn at(id: u32, node_id: u32, direction: PortDirection, channel: &str, slot: u32) -> Port {
        Port {
            id,
            node_id,
            direction,
            channel: channel.to_owned(),
            slot,
        }
    }

    #[test]
    fn a_single_pair_is_taken_whatever_it_is_numbered() {
        let mut r = Router::default();
        r.add_port(at(1, 10, PortDirection::Out, "FL", 7));
        r.add_port(at(2, 10, PortDirection::Out, "FR", 8));
        assert_eq!(r.pair_of(10, PortDirection::Out, 0).len(), 2);
    }

    #[test]
    fn a_send_is_told_from_the_main_output_by_its_pair() {
        let mut r = Router::default();
        for pair in 0..3u32 {
            r.add_port(at(pair * 2 + 1, 10, PortDirection::Out, "FL", pair * 2));
            r.add_port(at(pair * 2 + 2, 10, PortDirection::Out, "FR", pair * 2 + 1));
        }

        let main = r.pair_of(10, PortDirection::Out, 0);
        let second = r.pair_of(10, PortDirection::Out, 1);
        assert_eq!(main.len(), 2);
        assert_eq!(second.len(), 2);
        assert_ne!(main[0].id, second[0].id);
        assert_eq!(main[0].channel, second[0].channel);
    }

    #[test]
    fn ports_are_grouped_by_node_and_direction() {
        let mut r = Router::default();
        r.add_port(port(1, 10, PortDirection::Out, "FL"));
        r.add_port(port(2, 10, PortDirection::Out, "FR"));
        r.add_port(port(3, 10, PortDirection::In, "FL"));
        r.add_port(port(4, 20, PortDirection::Out, "FL"));

        assert_eq!(r.ports_of(10, PortDirection::Out).len(), 2);
        assert_eq!(r.ports_of(10, PortDirection::In).len(), 1);
        assert_eq!(r.ports_of(20, PortDirection::Out).len(), 1);
    }

    #[test]
    fn channelless_ports_are_not_routable() {
        let mut r = Router::default();
        r.add_port(port(1, 10, PortDirection::Out, ""));
        assert!(r.ports_of(10, PortDirection::Out).is_empty());
    }

    #[test]
    fn removing_a_port_forgets_it() {
        let mut r = Router::default();
        r.add_port(port(1, 10, PortDirection::Out, "FL"));
        r.remove_port(1);
        assert!(r.ports_of(10, PortDirection::Out).is_empty());
    }

    #[test]
    fn ports_sort_by_channel() {
        let mut r = Router::default();
        r.add_port(port(1, 10, PortDirection::Out, "FR"));
        r.add_port(port(2, 10, PortDirection::Out, "FL"));
        let ports = r.ports_of(10, PortDirection::Out);
        assert_eq!(ports[0].channel, "FL");
        assert_eq!(ports[1].channel, "FR");
    }
}
