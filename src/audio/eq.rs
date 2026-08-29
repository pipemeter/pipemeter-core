//! The per-strip processing chains.
//!
//! The processing is `PipeWire`'s own `filter-chain` module — three builtin
//! biquads in series — rather than DSP of our own. Two reasons. The filter
//! API is not bound in `pipewire-rs` 0.10, so writing our own node would mean
//! FFI, which this crate forbids; and `PipeWire`'s biquads are already correct,
//! already realtime-safe, and already what every other filter-chain user on
//! the machine is running. A hand-rolled equaliser would be more code in the
//! realtime path for a worse result.
//!
//! Each chain runs in its own `pipewire -c` helper process. That is how a
//! filter-chain is normally instantiated, and it keeps the DSP out of our
//! address space: if a chain dies it takes its own process with it and the
//! mixer keeps running.
//!
//! The chain appears as two nodes, `input.<name>` and `output.<name>`. Audio
//! goes strip sink → chain input, and the chain output is what the routing
//! matrix treats as the strip's source.
//!
//! Two graphs, matching what the original puts on each half of the mixer:
//! virtual strips get the three-band equaliser, hardware strips get the gate
//! and compressor behind their AUDIBILITY knobs.
//!
//! One thing to know before changing any of this: a chain only applies a
//! control change while it is **running**. Set a band on an idle chain and
//! the value is quietly ignored, which looks exactly like a wrong control
//! name.
//!
//! Both ends are pinned to stereo and told not to autoconnect. Left to
//! itself the session manager joins a fresh stream to whatever the default
//! device is, which put three EQs across the headset rather than in the
//! strips they belong to; and a mono chain shares no channel with an FL/FR
//! sink, so nothing would have linked to it anyway. The single-channel graph
//! is duplicated per channel by filter-chain itself.

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Band centre frequencies, low to high. Chosen to match what the three
/// knobs are captioned: Bass, then an unlabelled middle, then Treble.
const FREQUENCIES: [f32; 3] = [100.0, 1_000.0, 8_000.0];

/// Names of the three filters inside the graph, in the same order. They are
/// also how a band is addressed when setting its gain, as `<name>:Gain`.
pub const BANDS: [&str; 3] = ["bass", "mid", "treble"];

/// Which graph a chain runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Three biquads: the virtual strips' EQUALIZER.
    Equaliser,
    /// Gate then compressor: the hardware strips' AUDIBILITY pair.
    Dynamics,
}

/// The controls the AUDIBILITY knobs drive, as filter-chain addresses them.
pub const GATE_CONTROL: &str = "gate:open (dB)";
pub const COMP_CONTROL: &str = "comp:strength";

/// Stereo pairs on a chain's playback side.
///
/// One. It was three, for the two FX sends that are gone; see `config`.
const OUTPUT_PAIRS: usize = 1;

/// Gate threshold for a knob at rest and at full.
///
/// At rest the gate has to be inaudible rather than merely gentle, so the
/// bottom of the range sits below anything the plugin will act on.
const GATE_OPEN_MIN: f32 = -60.0;
const GATE_OPEN_MAX: f32 = -12.0;

/// Turn the Gate knob into the threshold it should open at, in dB.
#[must_use]
pub fn gate_open_db(knob: f32) -> f32 {
    GATE_OPEN_MIN + knob.clamp(0.0, 1.0) * (GATE_OPEN_MAX - GATE_OPEN_MIN)
}

/// The Comp knob is the compressor's strength directly, both being 0..1.
#[must_use]
pub fn comp_strength(knob: f32) -> f32 {
    knob.clamp(0.0, 1.0)
}

/// A running chain.
#[derive(Debug)]
pub struct Chain {
    /// The node the routing matrix should treat as the strip's source.
    pub output: String,
    /// The node the strip's sink feeds.
    pub input: String,
    process: Child,
}

impl Chain {
    /// Whether the helper has exited. Checked rather than waited on: this is
    /// called from the UI thread once a frame and must not block.
    pub fn has_died(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(Some(_)) | Err(_))
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // The helper is ours; leaving it behind would leave an orphan node in
        // the graph with nothing driving it.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Whether a node name is one of our chains' endpoints.
///
/// Getting this wrong does not fail loudly: the node simply never becomes
/// routable, and whatever depended on it goes quiet. It has been wrong
/// twice: once when a suffix here fell out of step with the names [`spawn`]
/// builds, and again when the effect chains arrived with names ending in
/// neither `_eq` nor `_fx` and silently could not be wired to anything.
///
/// So it asks the question it means: is this one of ours?
#[must_use]
pub fn is_chain_node(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("input.")
        .or_else(|| name.strip_prefix("output."))
    else {
        return false;
    };
    rest.ends_with("_eq") || rest.ends_with("_fx") || rest.starts_with("pipemeter_")
}

/// Kill any helper left over from a previous run.
///
/// [`Chain`]'s `Drop` only runs when the mixer exits cleanly. A crash, or a
/// plain `kill`, leaves the helpers orphaned - and since they hold their node
/// names, the next launch would spawn a second set beside them and route
/// through whichever it happened to find.
///
/// Found by reading `/proc` for a command line naming one of our configs.
/// There is no pid file to go stale, and nothing here can match a process
/// that is not one of ours.
pub fn kill_leftovers() {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    let marker = config_dir();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        // Arguments are NUL-separated, so this is a plain substring search.
        //
        // The directory is the whole test. It used to also require an _eq or
        // _fx suffix, which quietly stopped covering anything once the
        // effect chains arrived with names of their own - and a helper this
        // does not match is one that outlives the mixer holding its node
        // names, which is the exact fault this function exists for.
        let text = String::from_utf8_lossy(&cmdline);
        if !text.contains(marker.to_string_lossy().as_ref()) || !text.contains(".conf") {
            continue;
        }
        log::info!("killing a leftover helper, pid {pid}");
        // `kill` rather than a signal of our own: these are our processes and
        // the point is that they are already unsupervised.
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
}

/// Start a chain for the strip backed by `sink`, named after it.
///
/// # Errors
///
/// When the helper process could not be started or its configuration
/// could not be written. The mixer carries on without that strip's
/// chain rather than refusing to run.
pub fn spawn(sink: &str, kind: Kind) -> io::Result<Chain> {
    let suffix = match kind {
        Kind::Equaliser => "eq",
        Kind::Dynamics => "fx",
    };
    let name = format!("{sink}_{suffix}");
    spawn_config(&name, &config(&name, kind))
}

/// Start a helper for an already-built configuration.
///
/// Shared with the effect chains, which build a very different graph but
/// need exactly the same plumbing around it: write the file, run a
/// `PipeWire` of its own, and hand back the two node names it will create.
///
/// # Errors
///
/// Fails if the configuration cannot be written or the helper cannot start.
pub fn spawn_config(name: &str, config: &str) -> io::Result<Chain> {
    let path = config_path(name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, config)?;
    let process = Command::new("pipewire")
        .arg("-c")
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(Chain {
        output: format!("output.{name}"),
        input: format!("input.{name}"),
        process,
    })
}

/// Where a chain's generated config lives. The runtime directory, since it
/// is regenerated every launch and means nothing between them.
fn config_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join("pipemeter")
}

fn config_path(name: &str) -> PathBuf {
    config_dir().join(format!("{name}.conf"))
}

/// The config for one chain.
///
/// `node.passive` on both ends keeps the chain from running when nothing is
/// connected to it, so an unused strip costs nothing.
fn config(name: &str, kind: Kind) -> String {
    let (nodes, links, input, output) = match kind {
        Kind::Equaliser => equaliser_graph(),
        Kind::Dynamics => dynamics_graph(),
    };
    // Every strip carries its two FX sends beside its own output: a gain
    // node each, fed from the same place the strip's output comes from.
    // `PipeWire` links carry no gain of their own, so this is the only place
    // a send level can live.
    // One output, and only one.
    //
    // This graph used to declare two more - a gain node each for the reverb
    // and delay sends - against a six-channel playback side. Nothing ever
    // drove them, and they did not merely sit idle: with three graph outputs
    // replicated across two capture channels, the strip's own output stopped
    // arriving on the playback node's front pair, and every chain passed
    // silence. Audio went into the equaliser and none came out.
    //
    // The sends have to come back for the internal FX to work, but not like
    // this. See the FX section of TODO.md.
    wrap(name, &nodes, &links, input, &format!("\"{output}\""))
}

/// Three biquads in series, low shelf to high shelf.
fn equaliser_graph() -> (String, String, &'static str, &'static str) {
    let nodes = BANDS
        .iter()
        .zip(FREQUENCIES)
        .map(|(band, freq)| {
            let label = match *band {
                "bass" => "bq_lowshelf",
                "treble" => "bq_highshelf",
                _ => "bq_peaking",
            };
            format!(
                "          {{ type = builtin name = {band} label = {label} \
                 control = {{ \"Freq\" = {freq} \"Q\" = 1.0 \"Gain\" = 0.0 }} }}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let links = "          { output = \"bass:Out\" input = \"mid:In\" }\n\
                 \x20         { output = \"mid:Out\"  input = \"treble:In\" }"
        .to_owned();
    (nodes, links, "bass:In", "treble:Out")
}

/// Gate into compressor. Both are CAPS plugins, which are a standard part of
/// a desktop's LADSPA set; a machine without them gets a chain that fails to
/// start and a strip that carries on without dynamics.
fn dynamics_graph() -> (String, String, &'static str, &'static str) {
    let nodes = format!(
        "          {{ type = ladspa name = gate plugin = caps label = Noisegate \
control = {{ \"open (dB)\" = {GATE_OPEN_MIN} \"attack (ms)\" = 0.0 \"close (dB)\" = -80.0 }} }}\n\
         \x20         {{ type = ladspa name = comp plugin = caps label = Compress \
control = {{ \"strength\" = 0.0 \"threshold\" = 0.5 \"attack\" = 0.75 \"release\" = 0.5 \"gain (dB)\" = 0.0 }} }}"
    );
    let links = "          { output = \"gate:out\" input = \"comp:in\" }".to_owned();
    (nodes, links, "gate:in", "comp:out")
}

/// The boilerplate every chain shares.
fn wrap(name: &str, nodes: &str, links: &str, input: &str, output: &str) -> String {
    // The playback side carries one stereo pair per graph output. The extra
    // pairs have no meaningful speaker position, so they take AUX slots:
    // what matters is that they are distinct and in order.
    let channels = OUTPUT_PAIRS * 2;
    let positions = std::iter::once("FL".to_owned())
        .chain(std::iter::once("FR".to_owned()))
        .chain((0..(channels - 2)).map(|i| format!("AUX{i}")))
        .collect::<Vec<_>>()
        .join(" ");
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
      node.description = \"PipeMeter EQ\"
      media.name = \"PipeMeter EQ\"
      filter.graph = {{
        nodes = [
{nodes}
        ]
        links = [
{links}
        ]
        inputs  = [ \"{input}\" ]
        outputs = [ {output} ]
      }}
      capture.props  = {{
        node.name = \"input.{name}\"
        audio.channels = 2
        audio.position = [ FL FR ]
        node.passive = true
        node.autoconnect = false
      }}
      playback.props = {{
        node.name = \"output.{name}\"
        audio.channels = {channels}
        audio.position = [ {positions} ]
        node.passive = true
        node.autoconnect = false
      }}
    }}
  }}
]
"
    )
}

/// Turn a knob position into the gain that band should apply.
///
/// Knobs store 0..1 with 0.5 as flat; the EQ takes decibels over the same
/// ±12 dB range the captions promise.
#[must_use]
pub fn gain_db(knob: f32) -> f32 {
    (knob.clamp(0.0, 1.0) - 0.5) * 24.0
}

#[cfg(test)]
mod tests {
    use super::{BANDS, Kind, comp_strength, config, gain_db, gate_open_db};

    #[test]
    fn both_chain_kinds_are_recognised_at_both_ends() {
        assert!(super::is_chain_node("input.pipemeter_vaio_eq"));
        assert!(super::is_chain_node("output.pipemeter_vaio_eq"));
        assert!(super::is_chain_node("input.some_device_fx"));
        assert!(super::is_chain_node("output.some_device_fx"));
    }

    #[test]
    fn nothing_else_is_mistaken_for_a_chain() {
        // The meter streams in particular: they are ours, and routing one
        // would put a capture stream where a strip should be.
        assert!(!super::is_chain_node("pipemeter_meter"));
        assert!(!super::is_chain_node("pipemeter_vaio"));
        assert!(!super::is_chain_node("alsa_output.something"));
    }

    #[test]
    fn a_centred_knob_is_flat() {
        assert!(gain_db(0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn the_knob_ends_are_the_full_range() {
        assert!((gain_db(1.0) - 12.0).abs() < 1e-5);
        assert!((gain_db(0.0) + 12.0).abs() < 1e-5);
        // Beyond the ends is clamped rather than extrapolated.
        assert!((gain_db(4.0) - 12.0).abs() < 1e-5);
    }

    #[test]
    fn the_config_names_every_band_and_the_chain() {
        let text = config("pipemeter_vaio_eq", Kind::Equaliser);
        assert!(text.contains("node.name = \"pipemeter_vaio_eq\""));
        for band in BANDS {
            assert!(text.contains(&format!("name = {band}")), "missing {band}");
        }
        assert!(text.contains("bq_lowshelf"));
        assert!(text.contains("bq_peaking"));
        assert!(text.contains("bq_highshelf"));
    }

    #[test]
    fn a_chain_declares_exactly_one_output() {
        // The bug this replaced: three graph outputs against a six-channel
        // playback side left the strip's own signal off the front pair, and
        // every chain passed silence with its input plainly carrying audio.
        for kind in [Kind::Equaliser, Kind::Dynamics] {
            let text = config("x", kind);
            let outputs = text
                .lines()
                .find(|l| l.trim_start().starts_with("outputs ="))
                .expect("the config declares its outputs");
            assert_eq!(
                outputs.matches(':').count(),
                1,
                "more than one output: {outputs}",
            );
        }
    }

    #[test]
    fn the_playback_side_is_one_stereo_pair() {
        let text = config("x", Kind::Equaliser);
        assert!(text.contains("audio.channels = 2"), "{text}");
        assert!(text.contains("audio.position = [ FL FR ]"), "{text}");
    }

    #[test]
    fn a_gate_knob_at_rest_sits_below_anything_audible() {
        assert!(gate_open_db(0.0) <= -60.0);
        // And opens up as it is turned, without ever gating everything.
        assert!(gate_open_db(1.0) < 0.0);
        assert!(gate_open_db(1.0) > gate_open_db(0.0));
    }

    #[test]
    fn the_comp_knob_is_the_strength_directly() {
        assert!((comp_strength(0.4) - 0.4).abs() < f32::EPSILON);
        assert!((comp_strength(-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn the_dynamics_graph_runs_the_gate_into_the_compressor() {
        let text = config("x", Kind::Dynamics);
        assert!(text.contains("label = Noisegate"));
        assert!(text.contains("label = Compress"));
        assert!(text.contains("output = \"gate:out\" input = \"comp:in\""));
        assert!(text.contains("inputs  = [ \"gate:in\" ]"));
        assert!(text.contains("outputs = [ \"comp:out\" ]"), "{text}");
        // The two halves must not share a graph.
        assert!(!text.contains("bq_lowshelf"));
    }

    #[test]
    fn the_bands_are_wired_in_series_from_input_to_output() {
        let text = config("x", Kind::Equaliser);
        assert!(text.contains("inputs  = [ \"bass:In\" ]"));
        // The strip's own output first, then its two sends.
        assert!(text.contains("outputs = [ \"treble:Out\" ]"), "{text}");
        assert!(text.contains("output = \"bass:Out\" input = \"mid:In\""));
    }
}
