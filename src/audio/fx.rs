//! The shared effect chains: one reverb, one delay.
//!
//! An effect here is not another filter in line. It is a *send*: every strip
//! puts some of itself into a shared effect, and every bus takes some of the
//! result back. That is a topology question before it is a plugin question,
//! and the topology is the whole difficulty.
//!
//! # Where the level lives
//!
//! `PipeWire` links carry no gain, so a send at forty per cent cannot be a
//! quiet link — it has to be a gain node somewhere. The obvious somewhere is
//! beside the strip's own output, and that is where an earlier attempt put
//! it. It does not work: a chain declaring more outputs than its capture
//! side has channels leaves filter-chain to duplicate the graph and guess
//! the channel order, and what it guesses is wrong. The strip's own signal
//! stops arriving on the front pair and the chain passes silence.
//!
//! So the gain lives here instead, on the receiving end. This chain takes
//! two channels per strip and gives each its own `linear`, whose `Mult` is
//! that strip's send knob. Sixteen named inputs against sixteen capture
//! channels, and sixteen named outputs against sixteen playback channels —
//! no duplication anywhere, so no guessing.
//!
//! Two facts this rests on, both established by experiment rather than
//! assumed, because the assumptions here have been wrong twice:
//!
//! - a builtin `linear`'s `Mult` accepts a runtime write, so it can be a
//!   knob;
//! - a builtin `mixer`'s gains do not, so it can only sum. Its gains are
//!   fixed at unity in the config, which does take.
//!
//! # What drives it
//!
//! The playback side is *not* passive, unlike a strip's chain. A strip's
//! chain is pulled by the hardware sink at the end of it; this one is fed by
//! chains that are themselves passive, so with both ends passive nothing
//! ever asked it to run. It stayed suspended, a suspended node has no
//! realised ports, and the returns had nothing to attach to - so the chain
//! ended up with every send arriving and not one link leaving.
//!
//! # The one that has to be got right
//!
//! **A filter-chain drops control writes while it is idle.** Set a send on a
//! chain nothing is linked to and the value is accepted, acknowledged and
//! silently discarded, which looks exactly like a wrong control name — and
//! is most of why the first attempt at this was chased for so long in the
//! wrong direction.
//!
//! Verified both ways on this very graph: idle, `s0L:Mult` set to 0.5 reads
//! back 0.0; with its output linked to a sink so the chain runs, the same
//! write reads back 0.5. So the effect chains must be linked to their buses
//! *before* any send or return level is pushed, and a chain that has fallen
//! idle has to be written again when it comes back.

// Nothing spawns these yet: the graphs are generated and verified live, but
// the mixer does not start them, route them or drive their knobs. Kept whole
// rather than trimmed to what compiles, because the shape is the part that
// took the work and the remaining steps are named in TODO.md.
//
// Unlike the FX sends this replaces, none of it can misbehave in the
// meantime - it builds a string nobody runs.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::io;

use super::eq::{Chain, spawn_config};

/// Strips that can send, and buses that can return. Both are fixed, like
/// everything else about this mixer's shape.
pub const STRIPS: usize = 8;
pub const BUSES: usize = 8;

/// Which effect a chain is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Reverb,
    Delay,
    /// An external FX send. It has no processing of its own: it gathers
    /// each strip's send at that strip's own level and offers the mix as a
    /// node any program can capture. The effect lives in someone else's
    /// software, which is what makes it external.
    External1,
    External2,
    /// An external FX return. The other half of a send: the program that
    /// processed the audio plays back into this chain's input, and it
    /// carries one gain per bus on the way out, which are the External FX
    /// Return knobs. Only the first input pair is used - the rest of the
    /// per-strip inputs exist because it is the same graph as a returning
    /// effect, and building a second shape for it would be two things to
    /// keep in step.
    Return1,
    Return2,
}

impl Kind {
    /// Whether the chain hands its result back to the buses.
    ///
    /// A reverb or a delay does: the FX RETURN knobs are one gain per bus
    /// on its way out. An external send does not - its output is the
    /// product, offered to whatever wants to capture it - so it carries a
    /// single output pair instead of one per bus.
    #[must_use]
    pub fn returns_to_buses(self) -> bool {
        matches!(
            self,
            Self::Reverb | Self::Delay | Self::Return1 | Self::Return2
        )
    }

    /// Whether the chain is switched on and off by a preset.
    ///
    /// Only the two internal effects are. A send and a return are always
    /// live: there is no preset to choose, and their knobs at zero are
    /// what silences them.
    #[must_use]
    pub fn has_preset(self) -> bool {
        matches!(self, Self::Reverb | Self::Delay)
    }

    /// Whether the chain gathers its input from the strips.
    ///
    /// Everything except a return does. A return's input comes from
    /// whichever program processed the send, playing into its input node.
    #[must_use]
    pub fn takes_strip_sends(self) -> bool {
        !matches!(self, Self::Return1 | Self::Return2)
    }

    /// The node name, and the suffix that marks the helper as ours.
    #[must_use]
    pub fn node(self) -> &'static str {
        match self {
            Self::Reverb => "pipemeter_reverb",
            Self::Delay => "pipemeter_delay",
            Self::External1 => "pipemeter_extfx1",
            Self::External2 => "pipemeter_extfx2",
            Self::Return1 => "pipemeter_extret1",
            Self::Return2 => "pipemeter_extret2",
        }
    }

    /// The processing itself, as filter-chain nodes and the links between
    /// them, taking `inL`/`inR` and leaving `outL`/`outR`.
    ///
    /// CAPS Plate for the reverb, since it is part of the standard LADSPA
    /// set and is what the original's reverb sounds nearest to. The delay is
    /// the builtin, which needs no plugin at all.
    fn processing(self) -> (String, String) {
        match self {
            Self::Reverb => (
                "          { type = ladspa name = verb plugin = caps label = Plate \
control = { \"blend\" = 0.25 \"tail\" = 0.5 \"damping\" = 0.25 \"bandwidth\" = 0.75 } }"
                    .to_owned(),
                "          { output = \"sumL:Out\" input = \"verb:in\" }\n\
                 \x20         { output = \"verb:out.l\" input = \"outL:In\" }\n\
                 \x20         { output = \"verb:out.r\" input = \"outR:In\" }"
                    .to_owned(),
            ),
            Self::Delay => (
                "          { type = builtin name = dlyL label = delay \
control = { \"Delay (s)\" = 0.25 \"Feedback\" = 0.3 } }\n\
         \x20         { type = builtin name = dlyR label = delay \
control = { \"Delay (s)\" = 0.25 \"Feedback\" = 0.3 } }"
                    .to_owned(),
                "          { output = \"sumL:Out\" input = \"dlyL:In\" }\n\
                 \x20         { output = \"sumR:Out\" input = \"dlyR:In\" }\n\
                 \x20         { output = \"dlyL:Out\" input = \"outL:In\" }\n\
                 \x20         { output = \"dlyR:Out\" input = \"outR:In\" }"
                    .to_owned(),
            ),
            // Straight through: the mix is the product, so there is nothing
            // between the summing and the output.
            Self::External1 | Self::External2 | Self::Return1 | Self::Return2 => (
                String::new(),
                "          { output = \"sumL:Out\" input = \"outL:In\" }\n\
                 \x20         { output = \"sumR:Out\" input = \"outR:In\" }"
                    .to_owned(),
            ),
        }
    }
}

/// The control that carries strip `index`'s send level, per channel.
#[must_use]
pub fn send_control(index: usize) -> [String; 2] {
    [format!("s{index}L:Mult"), format!("s{index}R:Mult")]
}

/// The control that carries bus `index`'s return level, per channel.
#[must_use]
pub fn return_control(index: usize) -> [String; 2] {
    [format!("r{index}L:Mult"), format!("r{index}R:Mult")]
}

/// Which capture pair strip `index` sends into.
#[must_use]
pub fn send_pair(index: usize) -> u32 {
    index as u32
}

/// Which playback pair bus `index` takes back.
#[must_use]
pub fn return_pair(index: usize) -> u32 {
    index as u32
}

/// Build the chain's configuration.
/// The chain's configuration text.
///
/// Public so the window that names its controls can assert every one of
/// them exists in the graph that is built - a knob naming a control the
/// chain does not have would turn and do nothing.
pub fn config(kind: Kind) -> String {
    let name = kind.node();
    let (effect_nodes, effect_links) = kind.processing();

    let mut nodes = String::new();
    let mut links = String::new();

    // One gain per strip per channel, on the way in. These are the send
    // knobs, and the reason the gain lives here rather than on the strips.
    for strip in 0..STRIPS {
        for side in ["L", "R"] {
            let _ = writeln!(
                nodes,
                "          {{ type = builtin name = s{strip}{side} label = linear \
control = {{ \"Mult\" = 0.0 \"Add\" = 0.0 }} }}"
            );
        }
    }
    // Two mixers to sum them. Their gains are fixed at unity here because a
    // mixer's gains cannot be written at runtime - which is fine, since all
    // this one does is add.
    for side in ["L", "R"] {
        let gains = (1..=STRIPS)
            .map(|i| format!("\"Gain {i}\" = 1.0"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(
            nodes,
            "          {{ type = builtin name = sum{side} label = mixer \
control = {{ {gains} }} }}"
        );
        for strip in 0..STRIPS {
            let _ = writeln!(
                links,
                "          {{ output = \"s{strip}{side}:Out\" input = \"sum{side}:In {}\" }}",
                strip + 1
            );
        }
    }

    nodes.push_str(&effect_nodes);
    nodes.push('\n');
    links.push_str(&effect_links);
    links.push('\n');

    // The effect's own output, then whatever carries it away.
    for side in ["L", "R"] {
        let _ = writeln!(
            nodes,
            "          {{ type = builtin name = out{side} label = copy }}"
        );
        // A returning effect gets one gain per bus, which are the FX RETURN
        // knobs. A send has nowhere to return to: its output is the product,
        // for another program to capture, so it ends at `out`.
        if kind.returns_to_buses() {
            for bus in 0..BUSES {
                let _ = writeln!(
                    nodes,
                    "          {{ type = builtin name = r{bus}{side} label = linear \
control = {{ \"Mult\" = 0.0 \"Add\" = 0.0 }} }}"
                );
                let _ = writeln!(
                    links,
                    "          {{ output = \"out{side}:Out\" input = \"r{bus}{side}:In\" }}"
                );
            }
        }
    }

    wrap(name, &nodes, &links, kind)
}

/// Every input and every output named, one for one against the channel
/// counts. That is what keeps filter-chain from duplicating the graph and
/// guessing an order, which is the mistake that silenced the strips.
fn wrap(name: &str, nodes: &str, links: &str, kind: Kind) -> String {
    let inputs = (0..STRIPS)
        .flat_map(|s| [format!("\"s{s}L:In\""), format!("\"s{s}R:In\"")])
        .collect::<Vec<_>>()
        .join(" ");
    // One pair per bus for a returning effect, a single pair for a send.
    // The count has to match what the graph actually produces: a chain
    // declaring more outputs than it has breaks its channel mapping and
    // passes silence, which is how every strip went quiet once.
    let output_pairs = if kind.returns_to_buses() { BUSES } else { 1 };
    let outputs = if kind.returns_to_buses() {
        (0..BUSES)
            .flat_map(|b| [format!("\"r{b}L:Out\""), format!("\"r{b}R:Out\"")])
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        "\"outL:Out\" \"outR:Out\"".to_owned()
    };

    let positions = |pairs: usize| {
        std::iter::once("FL".to_owned())
            .chain(std::iter::once("FR".to_owned()))
            .chain((0..pairs * 2 - 2).map(|i| format!("AUX{i}")))
            .collect::<Vec<_>>()
            .join(" ")
    };

    format!(
        "context.properties = {{ log.level = 0 }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert }}
context.modules = [
  {{ name = libpipewire-module-rt }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.name = \"{name}\"
      node.description = \"PipeMeter FX\"
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
        inputs  = [ {inputs} ]
        outputs = [ {outputs} ]
      }}
      capture.props  = {{
        node.name = \"input.{name}\"
        audio.channels = {in_ch}
        audio.position = [ {in_pos} ]
        node.passive = true
        node.autoconnect = false
      }}
      playback.props = {{
        node.name = \"output.{name}\"
        audio.channels = {out_ch}
        audio.position = [ {out_pos} ]
        node.autoconnect = false
      }}
    }}
  }}
]
",
        in_ch = STRIPS * 2,
        out_ch = BUSES * 2,
        in_pos = positions(STRIPS),
        out_pos = positions(output_pairs),
    )
}

/// Start an effect chain.
///
/// # Errors
///
/// Fails if the configuration cannot be written or the helper cannot start.
pub fn spawn(kind: Kind) -> io::Result<Chain> {
    spawn_config(kind.node(), &config(kind))
}

#[cfg(test)]
mod tests {
    use super::{BUSES, Kind, STRIPS, config, return_control, send_control};

    /// A send declares one output pair, not one per bus. A chain whose
    /// outputs outnumber its captures breaks its channel mapping and
    /// passes silence, which is how every strip went quiet once.
    #[test]
    fn a_send_carries_a_single_output_pair() {
        let send = config(Kind::External1);
        assert!(send.contains("\"outL:Out\" \"outR:Out\""), "{send}");
        assert!(!send.contains("r0L:Out"), "a send has no per-bus returns");
        // And the returning effects still do.
        assert!(config(Kind::Reverb).contains("r0L:Out"));
    }

    #[test]
    #[ignore = "writes a config for the probe script rather than asserting"]
    fn dump_for_probe() {
        std::fs::write("/tmp/fx_reverb.conf", config(Kind::Reverb)).expect("writes");
        std::fs::write("/tmp/fx_delay.conf", config(Kind::Delay)).expect("writes");
        std::fs::write("/tmp/fx_extfx1.conf", config(Kind::External1)).expect("writes");
    }

    #[test]
    fn every_input_and_output_is_named_one_for_one() {
        // The whole point. A chain whose named ports do not match its
        // channel counts leaves filter-chain duplicating the graph and
        // guessing an order, and what it guesses is wrong - that is what
        // silenced every strip the first time this was attempted.
        let text = config(Kind::Reverb);
        let inputs = text
            .lines()
            .find(|l| l.trim_start().starts_with("inputs "))
            .expect("declares inputs");
        let outputs = text
            .lines()
            .find(|l| l.trim_start().starts_with("outputs "))
            .expect("declares outputs");
        assert_eq!(inputs.matches(":In").count(), STRIPS * 2);
        assert_eq!(outputs.matches(":Out").count(), BUSES * 2);
        assert!(text.contains(&format!("audio.channels = {}", STRIPS * 2)));
        assert!(text.contains(&format!("audio.channels = {}", BUSES * 2)));
    }

    #[test]
    fn every_send_and_return_has_a_gain_of_its_own() {
        let text = config(Kind::Reverb);
        for strip in 0..STRIPS {
            for control in send_control(strip) {
                let node = control.split(':').next().unwrap();
                assert!(
                    text.contains(&format!("name = {node} label = linear")),
                    "{node} is missing its gain",
                );
            }
        }
        for bus in 0..BUSES {
            for control in return_control(bus) {
                let node = control.split(':').next().unwrap();
                assert!(
                    text.contains(&format!("name = {node} label = linear")),
                    "{node} is missing its gain",
                );
            }
        }
    }

    #[test]
    fn everything_starts_silent() {
        // A chain that came up sending would put every strip into the reverb
        // the moment the mixer started.
        let text = config(Kind::Reverb);
        assert_eq!(
            text.matches("\"Mult\" = 0.0").count(),
            (STRIPS + BUSES) * 2,
            "some gain does not start at zero",
        );
    }

    #[test]
    fn the_summing_mixers_are_fixed_at_unity() {
        // They can only be set once, in the config, so this is the only
        // chance to get them right - and all they do is add.
        let text = config(Kind::Reverb);
        assert_eq!(text.matches("\"Gain 1\" = 1.0").count(), 2);
        assert_eq!(text.matches(&format!("\"Gain {STRIPS}\" = 1.0")).count(), 2);
    }

    #[test]
    fn both_effects_reach_the_same_output_nodes() {
        // The two chains differ only in what sits between the sum and the
        // returns; everything either side is shared.
        for kind in [Kind::Reverb, Kind::Delay] {
            let text = config(kind);
            assert!(text.contains("input = \"outL:In\""), "{kind:?}");
            assert!(text.contains("input = \"outR:In\""), "{kind:?}");
            assert!(text.contains("\"sumL:Out\""), "{kind:?}");
        }
    }

    #[test]
    fn the_two_chains_have_different_names() {
        assert_ne!(Kind::Reverb.node(), Kind::Delay.node());
        assert!(config(Kind::Delay).contains("pipemeter_delay"));
    }
}
