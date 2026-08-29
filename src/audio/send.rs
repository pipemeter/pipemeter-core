//! One strip's send into one effect, as its own small stereo chain.
//!
//! The effects used to take every strip's send as a pair of channels on
//! one sixteen-channel chain, summed inside the graph. That cannot work
//! here: `filter-chain` processes the first pair of a multi-input graph
//! and silently ignores the rest, so only the first strip was ever heard.
//! See `REASONS.md`.
//!
//! So a send is a chain of its own: two channels, one `linear` node
//! carrying the level, output into the effect's input. That is the shape
//! the equalisers already use and the only one measured to work - a
//! `Mult` of 1.0 passes -8.73 dBFS unchanged, 0.5 gives -14.75, and 0.0
//! is silence.
//!
//! They are created when a level first becomes audible and dropped when
//! it returns to zero, because eight strips into four effects would
//! otherwise be thirty-two helpers running to carry silence.
//!
//! Neither end is passive. A passive node only follows, and a path made
//! entirely of followers - strip chain, send, effect, return, bus - has
//! nothing to schedule it. The return in particular has to *pull* the
//! effect, or the effect sits suspended and the whole path is silent.

use std::io;

use super::eq::{Chain, spawn_config};

/// The control carrying the level, as filter-chain addresses it.
pub const LEVEL: &str = "g:Mult";

/// What a send chain is called: the strip it comes from and the effect it
/// feeds, so a leftover one can be recognised and killed.
#[must_use]
pub fn node_name(strip: usize, effect: &str) -> String {
    format!("pipemeter_send{strip}_{effect}")
}

/// Start a send chain at a level.
///
/// # Errors
///
/// When the helper could not be started or its configuration written.
/// The caller carries on without that send rather than refusing to run.
pub fn spawn(strip: usize, effect: &str, level: f32) -> io::Result<Chain> {
    let name = node_name(strip, effect);
    spawn_config(&name, &config(&name, level))
}

/// The chain: one gain node, stereo in, stereo out, connected to nothing
/// until the router links it.
fn config(name: &str, level: f32) -> String {
    let level = level.clamp(0.0, 1.0);
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
      node.description = \"PipeMeter send\"
      media.name = \"PipeMeter send\"
      filter.graph = {{
        nodes = [
          {{ type = builtin name = g label = linear control = {{ \"Mult\" = {level} \"Add\" = 0.0 }} }}
        ]
        inputs  = [ \"g:In\" ]
        outputs = [ \"g:Out\" ]
      }}
      capture.props  = {{
        node.name = \"input.{name}\"
        audio.channels = 2
        audio.position = [ FL FR ]
        node.passive = false
        node.autoconnect = false
      }}
      playback.props = {{
        node.name = \"output.{name}\"
        audio.channels = 2
        audio.position = [ FL FR ]
        node.passive = false
        node.autoconnect = false
      }}
    }}
  }}
]
"
    )
}

#[cfg(test)]
mod tests {
    use super::{config, node_name};

    #[test]
    fn a_send_is_named_for_its_strip_and_effect() {
        assert_eq!(node_name(5, "reverb"), "pipemeter_send5_reverb");
        assert_ne!(node_name(0, "reverb"), node_name(1, "reverb"));
        assert_ne!(node_name(0, "reverb"), node_name(0, "delay"));
    }

    /// Stereo in, stereo out, one input in the graph - the only shape
    /// measured to carry audio here. A multi-input graph passes its first
    /// pair and drops the rest.
    #[test]
    fn the_graph_has_a_single_stereo_input() {
        let text = config("x", 1.0);
        assert!(text.contains("inputs  = [ \"g:In\" ]"));
        assert!(text.contains("outputs = [ \"g:Out\" ]"));
        assert_eq!(text.matches("audio.channels = 2").count(), 2);
    }

    #[test]
    fn the_level_is_written_into_the_graph_and_bounded() {
        assert!(config("x", 0.5).contains("\"Mult\" = 0.5"));
        assert!(config("x", 2.0).contains("\"Mult\" = 1"));
        assert!(config("x", -1.0).contains("\"Mult\" = 0"));
    }

    #[test]
    #[ignore = "writes a config for the probe script rather than asserting"]
    fn dump_send_for_probe() {
        std::fs::write(
            "/tmp/probe_send.conf",
            super::config("probe_sendchain", 1.0),
        )
        .expect("writes");
    }
}
