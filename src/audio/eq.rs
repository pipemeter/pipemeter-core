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

use super::dynamics;
use super::dynamics::{DENOISER_LABEL, DENOISER_PLUGIN, GATE_OPEN_MIN, LIMIT_NODE};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Band centre frequencies, low to high. Chosen to match what the three
/// knobs are captioned: Bass, then an unlabelled middle, then Treble.
const FREQUENCIES: [f32; 3] = [100.0, 1_000.0, 8_000.0];

/// Names of the three filters inside the graph, in the same order. They are
/// also how a band is addressed when setting its gain, as `<name>:Gain`.
pub const BANDS: [&str; 3] = ["bass", "mid", "treble"];

/// The bus EQ's six cells, as a real settings file writes them: peaking
/// filters at these centres with a Q of 3 and no gain.
///
/// Read off `<VoiceMeeterBUSEQ>` rather than chosen. A parametric EQ's
/// bands are the user's to move, so any spread would have *worked* - but
/// a Voicemeeter user opening this dialog should find their own cells
/// where they left them, which means starting where the original starts.
pub const BUS_FREQUENCIES: [f32; 6] = [50.0, 200.0, 800.0, 2_000.0, 8_000.0, 12_000.0];

/// The Q every cell starts at.
pub const BUS_Q: f32 = 3.0;

/// What a bus's own EQ chain is called, before its index. `spawn` adds
/// the `_buseq` suffix, so this must not carry one itself.
pub const BUS_CHAIN_PREFIX: &str = "pipemeter_bus";

/// How many cells a bus EQ has.
pub const BUS_BANDS: usize = BUS_FREQUENCIES.len();

/// What a cell is called inside the graph, and so how its controls are
/// addressed: `cell1:Freq`, `cell1:Q`, `cell1:Gain`.
#[must_use]
pub fn bus_band(cell: usize) -> String {
    format!("cell{}", cell + 1)
}

/// The filter-chain address of one of a cell's three controls.
#[must_use]
pub fn bus_control(cell: usize, control: &str) -> String {
    format!("{}:{control}", bus_band(cell))
}

/// Which graph a chain runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Three biquads: the virtual strips' EQUALIZER.
    Equaliser,
    /// Gate then compressor: the hardware strips' AUDIBILITY pair.
    Dynamics,
    /// Six parametric cells: a bus's MASTER EQ.
    BusEqualiser,
}

/// Stereo pairs on a chain's playback side.
///
/// One. It was three, for the two FX sends that are gone; see `config`.
const OUTPUT_PAIRS: usize = 1;
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
        let text = String::from_utf8_lossy(&cmdline);
        if !text.contains(marker.to_string_lossy().as_ref()) || !text.contains(".conf") {
            continue;
        }
        log::info!("killing a leftover helper, pid {pid}");
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
        Kind::BusEqualiser => "buseq",
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
    // The denoiser usually lives in the user's own directory rather than
    // the system one, and a spawned helper does not inherit a path we
    // never set. Without this the chain cannot find it and refuses to
    // start, taking the strip with it.
    let ladspa_path = dynamics::denoiser_search_paths()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let process = Command::new("pipewire")
        .env("LADSPA_PATH", ladspa_path)
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
        Kind::BusEqualiser => bus_equaliser_graph(),
    };
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
    let nodes = format!("{nodes}\n{}", limiter_node());
    let links = format!("{links}\n{}", link_into_limiter("treble:Out"));
    (nodes, links, "bass:In", "lim:Out")
}

/// Six peaking biquads in series.
///
/// All three controls of every cell are writable at runtime, which is what
/// makes it parametric rather than six fixed bands: `bq_peaking` takes
/// Freq, Q and Gain, and filter-chain will accept all of them while the
/// chain is running.
fn bus_equaliser_graph() -> (String, String, &'static str, &'static str) {
    let nodes = BUS_FREQUENCIES
        .iter()
        .enumerate()
        .map(|(cell, freq)| {
            format!(
                "          {{ type = builtin name = {} label = bq_peaking \
                 control = {{ \"Freq\" = {freq} \"Q\" = {BUS_Q} \"Gain\" = 0.0 }} }}",
                bus_band(cell)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let links = (0..BUS_BANDS - 1)
        .map(|cell| {
            format!(
                "          {{ output = \"{}:Out\" input = \"{}:In\" }}",
                bus_band(cell),
                bus_band(cell + 1)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    (nodes, links, "cell1:In", "cell6:Out")
}

/// Gate into compressor. Both are CAPS plugins, which are a standard part of
/// a desktop's LADSPA set; a machine without them gets a chain that fails to
/// start and a strip that carries on without dynamics.
fn dynamics_graph() -> (String, String, &'static str, &'static str) {
    // A copy feeds both the gate and a mixer, and the mixer blends the
    // ungated signal back in - which is how the gate gets a floor, since
    // caps Noisegate shuts fully or not at all. Borrowed from
    // onjoakimsmind/pipemeeter, which is MIT.
    let close = dynamics::gate_close_db(GATE_OPEN_MIN);
    let (dry, wet) = dynamics::gate_blend(-80.0);
    let nodes = format!(
        "          {{ type = builtin name = gcopy label = copy }}\n\
         \x20         {{ type = ladspa name = gate plugin = caps label = Noisegate \
control = {{ \"open (dB)\" = {GATE_OPEN_MIN} \"attack (ms)\" = 0.0 \"close (dB)\" = {close} }} }}\n\
         \x20         {{ type = builtin name = gmix label = mixer \
control = {{ \"Gain 1\" = {dry} \"Gain 2\" = {wet} }} }}\n\
         \x20         {{ type = ladspa name = comp plugin = caps label = Compress \
control = {{ \"mode\" = 0 \"strength\" = 0.0 \"threshold\" = 0.5 \"attack\" = 0.75 \"release\" = 0.5 \"gain (dB)\" = 0.0 }} }}"
    );
    let mut links = "          { output = \"gcopy:Out\" input = \"gate:in\" }\n\
         \x20         { output = \"gcopy:Out\" input = \"gmix:In 1\" }\n\
         \x20         { output = \"gate:out\" input = \"gmix:In 2\" }\n\
         \x20         { output = \"gmix:Out\" input = \"comp:in\" }"
        .to_owned();

    // The denoiser sits at the head, before the gate, and only if one is
    // installed - a chain naming a plugin that is not there does not
    // start, and would take the strip with it.
    let mut nodes = nodes;
    let mut input = "gcopy:In";
    if dynamics::denoiser_available() {
        let (dry, wet) = dynamics::denoiser_blend(0.0);
        nodes = format!(
            "          {{ type = builtin name = dcopy label = copy }}\n\
             \x20         {{ type = ladspa name = dn plugin = {DENOISER_PLUGIN} label = {DENOISER_LABEL} }}\n\
             \x20         {{ type = builtin name = dmix label = mixer \
control = {{ \"Gain 1\" = {dry} \"Gain 2\" = {wet} }} }}\n{nodes}"
        );
        links = format!(
            "          {{ output = \"dcopy:Out\" input = \"dn:Input\" }}\n\
             \x20         {{ output = \"dcopy:Out\" input = \"dmix:In 1\" }}\n\
             \x20         {{ output = \"dn:Output\" input = \"dmix:In 2\" }}\n\
             \x20         {{ output = \"dmix:Out\" input = \"gcopy:In\" }}\n{links}"
        );
        input = "dcopy:In";
    }
    let nodes = format!("{nodes}\n{}", limiter_node());
    let links = format!("{links}\n{}", link_into_limiter("comp:out"));
    (nodes, links, input, "lim:Out")
}

/// The limiter that ends every strip graph, resting wide open.
fn limiter_node() -> String {
    let open = dynamics::limit_amplitude(crate::model::LIMIT_OFF);
    format!(
        "          {{ type = builtin name = {LIMIT_NODE} label = clamp control = {{ \"Min\" = {} \"Max\" = {open} }} }}",
        -open
    )
}

/// Attach the last stage of a graph to the limiter.
fn link_into_limiter(from: &str) -> String {
    format!("          {{ output = \"{from}\" input = \"{LIMIT_NODE}:In\" }}")
}

/// The boilerplate every chain shares.
fn wrap(name: &str, nodes: &str, links: &str, input: &str, output: &str) -> String {
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
    use super::{BANDS, BUS_BANDS, BUS_FREQUENCIES, Kind, config, gain_db};

    #[test]
    fn both_chain_kinds_are_recognised_at_both_ends() {
        assert!(super::is_chain_node("input.pipemeter_vaio_eq"));
        assert!(super::is_chain_node("output.pipemeter_vaio_eq"));
        assert!(super::is_chain_node("input.some_device_fx"));
        assert!(super::is_chain_node("output.some_device_fx"));
    }

    #[test]
    fn nothing_else_is_mistaken_for_a_chain() {
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
        assert!(crate::audio::dynamics::gate_open_db(0.0) <= -60.0);
        assert!(crate::audio::dynamics::gate_open_db(1.0) < 0.0);
        assert!(
            crate::audio::dynamics::gate_open_db(1.0) > crate::audio::dynamics::gate_open_db(0.0)
        );
    }

    #[test]
    fn the_comp_knob_is_the_strength_directly() {
        assert!((crate::audio::dynamics::comp_strength(0.4) - 0.4).abs() < f32::EPSILON);
        assert!((crate::audio::dynamics::comp_strength(-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn the_dynamics_graph_runs_the_gate_into_the_compressor() {
        let text = config("x", Kind::Dynamics);
        assert!(text.contains("label = Noisegate"));
        assert!(text.contains("label = Compress"));
        assert!(text.contains("output = \"gmix:Out\" input = \"comp:in\""));
        // The denoiser, when installed, sits ahead of the gate and
        // becomes the graph's input instead.
        let head = if crate::audio::dynamics::denoiser_available() {
            "dcopy:In"
        } else {
            "gcopy:In"
        };
        assert!(
            text.contains(&format!("inputs  = [ \"{head}\" ]")),
            "{text}"
        );
        assert!(text.contains("outputs = [ \"lim:Out\" ]"), "{text}");
        assert!(!text.contains("bq_lowshelf"));
    }

    #[test]
    fn the_bands_are_wired_in_series_from_input_to_output() {
        let text = config("x", Kind::Equaliser);
        assert!(text.contains("inputs  = [ \"bass:In\" ]"));
        assert!(text.contains("outputs = [ \"lim:Out\" ]"), "{text}");
        assert!(text.contains("output = \"bass:Out\" input = \"mid:In\""));
    }

    #[test]
    fn the_bus_eq_carries_every_cell_the_file_does() {
        let text = config("bus", Kind::BusEqualiser);
        for (cell, freq) in BUS_FREQUENCIES.iter().enumerate() {
            let name = super::bus_band(cell);
            assert!(
                text.contains(&format!("name = {name} label = bq_peaking")),
                "cell {name} missing from the graph"
            );
            assert!(text.contains(&format!("\"Freq\" = {freq}")));
        }
    }

    #[test]
    fn the_bus_eq_cells_run_in_series() {
        let text = config("bus", Kind::BusEqualiser);
        for cell in 0..BUS_BANDS - 1 {
            assert!(text.contains(&format!(
                "output = \"{}:Out\" input = \"{}:In\"",
                super::bus_band(cell),
                super::bus_band(cell + 1)
            )));
        }
    }

    /// A chain declaring more outputs than its capture channels breaks its
    /// channel mapping and passes silence, which cost an afternoon once.
    #[test]
    fn the_bus_eq_is_stereo_in_and_stereo_out() {
        let text = config("bus", Kind::BusEqualiser);
        assert_eq!(text.matches("audio.channels = 2").count(), 2);
    }

    #[test]
    fn a_bus_eq_control_is_addressed_by_cell() {
        assert_eq!(super::bus_control(0, "Gain"), "cell1:Gain");
        assert_eq!(super::bus_control(5, "Freq"), "cell6:Freq");
    }

    #[test]
    #[ignore = "writes a config for the probe script rather than asserting"]
    fn dump_bus_eq_for_probe() {
        std::fs::write(
            "/tmp/bus_eq.conf",
            config("probe_buseq", Kind::BusEqualiser),
        )
        .expect("writes");
    }

    /// At rest it has to be the measured no-op, not merely close to one.
    #[test]
    fn the_resting_limiter_is_the_transparent_threshold() {
        assert!(crate::audio::dynamics::limit_amplitude(crate::model::LIMIT_OFF) > 3.9);
        assert!((crate::audio::dynamics::limit_amplitude(0.0) - 1.0).abs() < 1e-5);
        assert!((crate::audio::dynamics::limit_amplitude(-6.02) - 0.5).abs() < 0.001);
        assert!((crate::audio::dynamics::limit_amplitude(-12.04) - 0.25).abs() < 0.001);
    }

    /// Every point the staircase actually measured, to a tenth of the
    /// control. If this drifts, the mapping stopped matching the plugin.
    #[test]
    fn the_threshold_matches_the_measured_onsets() {
        for (db, expected) in [
            (-6.0, 0.80),
            (-9.0, 0.74),
            (-12.0, 0.68),
            (-21.0, 0.50),
            (-30.0, 0.32_f32.max(0.35)),
        ] {
            let got = crate::audio::dynamics::comp_threshold(db);
            assert!(
                (got - expected).abs() < 0.03,
                "{db} dB gave {got}, expected about {expected}"
            );
        }
    }

    /// Wide open has to be genuinely transparent: 0.85 and above left the
    /// whole staircase untouched.
    #[test]
    fn a_high_threshold_never_acts() {
        assert!(crate::audio::dynamics::comp_threshold(0.0) >= 0.9);
        assert!(crate::audio::dynamics::comp_threshold(12.0) <= 1.0);
    }

    /// The knob and the dialog have to agree, or moving one leaves the
    /// other showing something that is no longer true.
    #[test]
    fn the_gate_knob_and_its_decibels_round_trip() {
        for knob in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let back = crate::audio::dynamics::gate_knob_from_db(
                crate::audio::dynamics::gate_open_db(knob),
            );
            assert!((back - knob).abs() < 1e-5, "{knob} came back as {back}");
        }
    }

    /// The close threshold is what actually gates. It was pinned at -80,
    /// which nothing reaches, so the gate never shut.
    #[test]
    fn the_gate_closes_below_where_it_opens() {
        assert!(crate::audio::dynamics::gate_close_db(-30.0) < -30.0);
        assert!(
            crate::audio::dynamics::gate_close_db(-78.0) >= -80.0,
            "clamped to the port's range"
        );
        let text = config("x", Kind::Dynamics);
        assert!(!text.contains("\"close (dB)\" = -80"), "{text}");
    }

    /// Damping is a floor: fully damped passes only the gated signal,
    /// undamped passes the dry one and nothing else.
    #[test]
    fn damping_blends_the_dry_signal_back() {
        let (dry, wet) = crate::audio::dynamics::gate_blend(-80.0);
        assert!(dry < 0.001 && (wet - 1.0).abs() < 0.001);
        let (dry, wet) = crate::audio::dynamics::gate_blend(0.0);
        assert!((dry - 1.0).abs() < 1e-6 && wet.abs() < 1e-6);
        let (dry, _) = crate::audio::dynamics::gate_blend(-6.0);
        assert!((dry - 0.501).abs() < 0.01);
    }

    /// The knob is an amount, because the plugin has no depth control of
    /// its own: at rest it is entirely dry, so an uninstalled or unwanted
    /// denoiser changes nothing.
    #[test]
    fn the_denoiser_knob_blends_from_dry_to_wet() {
        assert_eq!(crate::audio::dynamics::denoiser_blend(0.0), (1.0, 0.0));
        assert_eq!(crate::audio::dynamics::denoiser_blend(1.0), (0.0, 1.0));
        let (dry, wet) = crate::audio::dynamics::denoiser_blend(0.25);
        assert!((dry - 0.75).abs() < 1e-6 && (wet - 0.25).abs() < 1e-6);
    }

    /// The graph only names the plugin when it is actually installed;
    /// naming a missing one stops the chain starting at all.
    #[test]
    fn the_graph_mentions_the_denoiser_only_when_it_is_there() {
        let text = config("x", Kind::Dynamics);
        assert_eq!(
            text.contains(super::DENOISER_LABEL),
            crate::audio::dynamics::denoiser_available()
        );
    }

    /// caps Compress defaults to a mode that attenuates even when it is
    /// asked to do nothing. Leaving the mode unset cost 2.87 dB on every
    /// hardware strip.
    #[test]
    fn the_compressor_runs_in_the_transparent_mode() {
        let text = config("x", Kind::Dynamics);
        assert!(text.contains("\"mode\" = 0"), "{text}");
    }

    #[test]
    #[ignore = "writes a config for the probe script rather than asserting"]
    fn dump_eq_for_probe() {
        std::fs::write(
            "/tmp/probe_eq.conf",
            config("probe_eqchain", Kind::Equaliser),
        )
        .expect("writes");
        std::fs::write(
            "/tmp/probe_dyn.conf",
            config("probe_dynchain", Kind::Dynamics),
        )
        .expect("writes");
    }
}
